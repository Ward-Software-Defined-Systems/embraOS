//! gRPC service implementation for embra-brain.
//!
//! Bridges Phase 0 Brain + tools + sessions into a gRPC streaming interface.

use crate::brain::Message;
use crate::config;
use crate::db::WardsonDbClient;
use crate::learning;
use crate::proactive::Notification;
use crate::provider::anthropic::AnthropicProvider;
use crate::provider::gemini::GeminiProvider;
use crate::provider::ir::{ApiMessage, AssistantTurn, Block, EarlyStopReason, TurnOutcome};
use crate::provider::{LlmProvider, ProviderKind, StreamEvent, SystemPromptBundle, ToolManifest};
use crate::sessions::SessionManager;
use crate::tools;

use embra_common::proto::brain::brain_service_server::BrainService;
use embra_common::proto::brain::*;
use embra_common::proto::common;

use futures::stream::BoxStream;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use tokio::sync::{mpsc, Mutex, RwLock};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, error, info, warn};

pub struct BrainGrpcService {
    db: Arc<WardsonDbClient>,
    session_manager: Arc<RwLock<SessionManager>>,
    config_tz: String,
    api_key: String,
    proactive_rx: Arc<Mutex<mpsc::Receiver<Notification>>>,
    start_time: std::time::Instant,
    /// True while a tool-iteration loop is in flight. `/provider`
    /// switches block on this and queue via `pending_provider`. Wired
    /// into the loop driver in Stage 2; idle in this stage.
    in_turn: Arc<AtomicBool>,
    /// Operator's queued provider switch. Drained between turns by the
    /// loop driver after `in_turn` clears. Stage 8 populates it.
    pending_provider: Arc<Mutex<Option<ProviderKind>>>,
    /// Set by `/provider --setup [<kind>]` to indicate that the next
    /// user message should be treated as a candidate API key for the
    /// given provider rather than a regular conversation turn. The
    /// UserMessage handler intercepts and clears this before the
    /// loop driver runs.
    pending_key_setup: Arc<Mutex<Option<ProviderKind>>>,
}

impl BrainGrpcService {
    pub fn new(
        db: WardsonDbClient,
        config_tz: String,
        api_key: String,
        proactive_rx: mpsc::Receiver<Notification>,
    ) -> Self {
        let db = Arc::new(db);
        let session_manager = Arc::new(RwLock::new(
            SessionManager::new((*db).clone())
        ));

        Self {
            db,
            session_manager,
            config_tz,
            api_key,
            proactive_rx: Arc::new(Mutex::new(proactive_rx)),
            start_time: std::time::Instant::now(),
            in_turn: Arc::new(AtomicBool::new(false)),
            pending_provider: Arc::new(Mutex::new(None)),
            pending_key_setup: Arc::new(Mutex::new(None)),
        }
    }
}

#[tonic::async_trait]
impl BrainService for BrainGrpcService {
    type ConverseStream = Pin<Box<dyn Stream<Item = Result<ConversationResponse, Status>> + Send>>;

    async fn converse(
        &self,
        request: Request<Streaming<ConversationRequest>>,
    ) -> Result<Response<Self::ConverseStream>, Status> {
        let mut incoming = request.into_inner();

        let (tx, rx) = mpsc::channel::<Result<ConversationResponse, Status>>(100);

        let db = self.db.clone();
        let session_mgr = self.session_manager.clone();
        let config_tz = self.config_tz.clone();
        let api_key = self.api_key.clone();
        let proactive_rx = self.proactive_rx.clone();
        let in_turn = self.in_turn.clone();
        let pending_provider = self.pending_provider.clone();
        let pending_key_setup = self.pending_key_setup.clone();

        tokio::spawn(async move {
            let mut config_tz = config_tz;
            let mut api_key = api_key;

            // Check for first-run: no config in WardSONDB
            let is_first_run = config::load_config(&db).await.is_err();

            if is_first_run {
                info!("First run detected — starting config wizard via gRPC");
                let (wizard_tx, mut wizard_rx) = mpsc::channel::<String>(1);

                // Spawn wizard task
                let wizard_db = db.clone();
                let wizard_gRPC_tx = tx.clone();
                let mut wizard_handle = tokio::spawn(async move {
                    config::run_config_wizard_grpc(&wizard_gRPC_tx, &mut wizard_rx, &wizard_db).await
                });

                // Feed user responses to wizard until it completes
                loop {
                    tokio::select! {
                        msg = incoming.next() => {
                            match msg {
                                Some(Ok(req)) => {
                                    if let Some(conversation_request::RequestType::UserMessage(um)) = req.request_type {
                                        // Forward user input to wizard
                                        if wizard_tx.send(um.content).await.is_err() {
                                            break; // Wizard closed its receiver
                                        }
                                    }
                                    // Ignore non-UserMessage during wizard (SlashCommand, SessionAttach)
                                }
                                Some(Err(e)) => { warn!("Stream error during wizard: {}", e); break; }
                                None => { debug!("Client disconnected during wizard"); break; }
                            }
                        }
                        result = &mut wizard_handle => {
                            match result {
                                Ok(Ok(new_config)) => {
                                    info!("Config wizard completed: name={}", new_config.name);
                                    api_key = new_config.api_key;
                                    config_tz = new_config.timezone;
                                }
                                Ok(Err(e)) => {
                                    error!("Config wizard failed: {}", e);
                                }
                                Err(e) => {
                                    error!("Config wizard task panicked: {}", e);
                                }
                            }
                            break;
                        }
                    }
                }
            }

            // Stage 2: Learning Mode (if soul not sealed)
            let soul_sealed = learning::is_soul_sealed(&**&db).await.unwrap_or(false);
            if !soul_sealed {
                info!("Soul not sealed — entering Learning Mode");
                let loaded_config = config::load_config(&**&db).await.unwrap_or_else(|_| config::SystemConfig {
                    name: "Embra".to_string(),
                    api_key: api_key.clone(),
                    timezone: config_tz.clone(),
                    deployment_mode: "phase1".into(),
                    created_at: String::new(),
                    version: env!("CARGO_PKG_VERSION").into(),
                    github_token: None,
                    kg_temporal_window_secs: 1800,
                    kg_max_traversal_depth: 3,
                    kg_traversal_depth_ceiling: 5,
                    kg_edge_candidate_limit: 50,
                    api_provider: "anthropic".to_string(),
                    gemini_model: None,
                    anthropic_api_key: None,
                    gemini_api_key: None,
                });
                match run_learning_loop(&tx, &mut incoming, &db, &loaded_config, &api_key).await {
                    Ok(()) => {
                        info!("Learning Mode complete — transitioning to Operational");
                        // Reload config in case it was updated
                        if let Ok(cfg) = config::load_config(&**&db).await {
                            api_key = cfg.api_key;
                            config_tz = cfg.timezone;
                        }
                        // Ensure default session exists and is active
                        {
                            let mut mgr = session_mgr.write().await;
                            if !mgr.session_exists("main").await.unwrap_or(false) {
                                let _ = mgr.create("main").await;
                            }
                            mgr.active_session = Some("main".to_string());
                            info!("Session 'main' activated for Operational mode");
                        }
                    }
                    Err(e) => {
                        error!("Learning Mode failed: {}", e);
                    }
                }
            }

            // Try to get proactive notifications (only one Converse stream gets them)
            let mut proactive = proactive_rx.try_lock().ok();

            // Stage 3: Main conversation loop (Operational mode)
            loop {
                tokio::select! {
                    msg = incoming.next() => {
                        match msg {
                            Some(Ok(req)) => {
                                if let Err(e) = handle_request(
                                    req, &tx, &db, &session_mgr, &config_tz, &api_key, &in_turn, &pending_provider, &pending_key_setup
                                ).await {
                                    error!("Error handling request: {}", e);
                                    let _ = tx.send(Ok(ConversationResponse {
                                        response_type: Some(conversation_response::ResponseType::System(
                                            SystemMessage {
                                                content: format!("Error: {}", e),
                                                msg_type: SystemMessageType::Error as i32,
                                            }
                                        )),
                                    })).await;
                                }
                            }
                            Some(Err(e)) => {
                                warn!("Stream error: {}", e);
                                break;
                            }
                            None => {
                                debug!("Client stream ended");
                                break;
                            }
                        }
                    }
                    // Forward proactive notifications if we hold the lock
                    notif = async {
                        if let Some(ref mut rx) = proactive {
                            rx.recv().await
                        } else {
                            std::future::pending().await
                        }
                    } => {
                        if let Some(notif) = notif {
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(
                                    SystemMessage {
                                        content: format!("[{}] {}", notif.priority_label(), notif.message),
                                        msg_type: SystemMessageType::Notification as i32,
                                    }
                                )),
                            })).await;
                        }
                    }
                }
            }
        });

        let output_stream = ReceiverStream::new(rx);
        Ok(Response::new(Box::pin(output_stream)))
    }

    // --- Session RPCs ---

    async fn list_sessions(&self, _req: Request<ListSessionsRequest>) -> Result<Response<ListSessionsResponse>, Status> {
        let mgr = self.session_manager.read().await;
        let sessions = mgr.list().await
            .map_err(|e| Status::internal(format!("Failed to list sessions: {}", e)))?;

        Ok(Response::new(ListSessionsResponse {
            sessions: sessions.into_iter().map(|s| SessionInfo {
                name: s.name,
                state: format!("{:?}", s.state),
                turn_count: 0, // SessionMeta doesn't track turn count directly
                created_at: Some(common::Timestamp { iso8601: s.created_at.to_rfc3339() }),
                last_active: Some(common::Timestamp { iso8601: s.last_active.to_rfc3339() }),
                has_summary: false,
            }).collect(),
        }))
    }

    async fn create_session(&self, request: Request<CreateSessionRequest>) -> Result<Response<CreateSessionResponse>, Status> {
        let name = request.into_inner().name;
        let mut mgr = self.session_manager.write().await;
        let session = mgr.create(&name).await
            .map_err(|e| Status::internal(format!("Failed to create session: {}", e)))?;

        Ok(Response::new(CreateSessionResponse {
            session: Some(SessionInfo {
                name: session.name,
                state: format!("{:?}", session.state),
                turn_count: 0,
                created_at: Some(common::Timestamp { iso8601: session.created_at.to_rfc3339() }),
                last_active: Some(common::Timestamp { iso8601: session.last_active.to_rfc3339() }),
                has_summary: false,
            }),
        }))
    }

    async fn switch_session(&self, request: Request<SwitchSessionRequest>) -> Result<Response<SwitchSessionResponse>, Status> {
        let name = request.into_inner().name;
        let mut mgr = self.session_manager.write().await;

        // Detach current session if any
        if let Some(ref current) = mgr.active_session.clone() {
            let _ = mgr.detach(current).await;
        }

        let history = mgr.reattach(&name).await
            .map_err(|e| Status::internal(format!("{}", e)))?;

        Ok(Response::new(SwitchSessionResponse {
            session: Some(SessionInfo {
                name: name.clone(),
                state: "Active".to_string(),
                turn_count: history.len() as u32,
                created_at: None,
                last_active: None,
                has_summary: false,
            }),
            reconnection_briefing: String::new(),
        }))
    }

    async fn close_session(&self, _req: Request<CloseSessionRequest>) -> Result<Response<CloseSessionResponse>, Status> {
        let mut mgr = self.session_manager.write().await;
        let closed = mgr.active_session.clone().unwrap_or_default();
        if !closed.is_empty() {
            let _ = mgr.close(&closed).await;
        }
        Ok(Response::new(CloseSessionResponse {
            closed_session: closed,
            switched_to: String::new(),
        }))
    }

    async fn get_session_meta(&self, _request: Request<GetSessionMetaRequest>) -> Result<Response<GetSessionMetaResponse>, Status> {
        Err(Status::unimplemented("GetSessionMeta not yet implemented"))
    }

    async fn get_system_status(&self, _req: Request<GetSystemStatusRequest>) -> Result<Response<GetSystemStatusResponse>, Status> {
        Ok(Response::new(GetSystemStatusResponse {
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            soul: None,
            wardsondb_status: if self.db.health().await.unwrap_or(false) { "healthy" } else { "unhealthy" }.to_string(),
            services: std::collections::HashMap::new(),
        }))
    }

    async fn get_soul_document(&self, _req: Request<GetSoulDocumentRequest>) -> Result<Response<GetSoulDocumentResponse>, Status> {
        match learning::load_soul(&**&self.db).await {
            Ok(Some(soul)) => {
                let hash = learning::compute_soul_hash(&soul).unwrap_or_default();
                Ok(Response::new(GetSoulDocumentResponse {
                    soul_json: serde_json::to_string_pretty(&soul).unwrap_or_default(),
                    sha256: hash,
                    sealed: true,
                }))
            }
            Ok(None) => Ok(Response::new(GetSoulDocumentResponse {
                soul_json: String::new(),
                sha256: String::new(),
                sealed: false,
            })),
            Err(e) => Err(Status::internal(format!("Failed to load soul: {}", e))),
        }
    }

    async fn get_identity(&self, _req: Request<GetIdentityRequest>) -> Result<Response<GetIdentityResponse>, Status> {
        match self.db.read("memory.identity", "identity").await {
            Ok(doc) => Ok(Response::new(GetIdentityResponse {
                identity_json: serde_json::to_string_pretty(&doc).unwrap_or_default(),
            })),
            Err(_) => Ok(Response::new(GetIdentityResponse {
                identity_json: String::new(),
            })),
        }
    }

    async fn get_expression(&self, _req: Request<GetExpressionRequest>) -> Result<Response<ExpressionState>, Status> {
        match self.db.read("ui", "expression").await {
            Ok(doc) => Ok(Response::new(ExpressionState {
                content: doc.get("content").and_then(|v| v.as_str()).unwrap_or("").to_string(),
                version: doc.get("version").and_then(|v| v.as_u64()).unwrap_or(0),
                updated_at: doc.get("updated_at").and_then(|v| v.as_str()).unwrap_or("").to_string(),
            })),
            Err(_) => Ok(Response::new(ExpressionState {
                content: String::new(),
                version: 0,
                updated_at: String::new(),
            })),
        }
    }

    async fn get_mode(&self, _req: Request<GetModeRequest>) -> Result<Response<GetModeResponse>, Status> {
        let is_sealed = learning::is_soul_sealed(&**&self.db).await.unwrap_or(false);
        let mode = if is_sealed {
            OperatingMode::Operational
        } else {
            OperatingMode::Learning
        };
        Ok(Response::new(GetModeResponse {
            mode: mode as i32,
            soul_status: if is_sealed { "sealed" } else { "unsealed" }.to_string(),
        }))
    }

    async fn health_check(&self, _req: Request<common::HealthCheckRequest>) -> Result<Response<common::HealthCheckResponse>, Status> {
        Ok(Response::new(common::HealthCheckResponse {
            status: common::HealthStatus::Healthy as i32,
            service_name: "embra-brain".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            details: std::collections::HashMap::new(),
        }))
    }
}

/// Handle a single incoming conversation request.
async fn handle_request(
    req: ConversationRequest,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    config_tz: &str,
    api_key: &str,
    in_turn: &Arc<AtomicBool>,
    pending_provider: &Arc<Mutex<Option<ProviderKind>>>,
    pending_key_setup: &Arc<Mutex<Option<ProviderKind>>>,
) -> anyhow::Result<()> {
    let self_in_turn = in_turn.clone();
    match req.request_type {
        Some(conversation_request::RequestType::UserMessage(msg)) => {
            // D2: intercept the user message as a candidate API key
            // when /provider --setup queued a target. Validates,
            // persists per-provider key + STATE file, clears flag.
            // On invalid: keeps flag set so the operator can retry
            // with the next message.
            let pending_target = pending_key_setup.lock().await.clone();
            if let Some(target) = pending_target {
                let candidate = msg.content.trim().to_string();
                handle_pending_key_setup(target, candidate, tx, db, pending_key_setup).await;
                return Ok(());
            }

            // Get active session name
            let session_name = {
                let mgr = session_mgr.read().await;
                mgr.active_session.clone().unwrap_or_else(|| "default".to_string())
            };

            // Load config — api_key comes from --api-key flag or env, not WardSONDB
            let loaded_config = config::load_config(db).await.unwrap_or_else(|_| config::SystemConfig {
                name: "Embra".to_string(),
                api_key: api_key.to_string(),
                timezone: config_tz.to_string(),
                deployment_mode: "phase1".into(),
                created_at: String::new(),
                version: env!("CARGO_PKG_VERSION").into(),
                github_token: None,
                kg_temporal_window_secs: 1800,
                kg_max_traversal_depth: 3,
                kg_traversal_depth_ceiling: 5,
                kg_edge_candidate_limit: 50,
                api_provider: "anthropic".to_string(),
                gemini_model: None,
                anthropic_api_key: None,
                gemini_api_key: None,
            });
            let config_name = loaded_config.name.clone();

            if api_key.is_empty() {
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: "No Anthropic API key configured. Set ANTHROPIC_API_KEY or run config wizard.".to_string(),
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
                return Ok(());
            }

            // Build system prompt
            let system_prompt = if learning::is_soul_sealed(&**db).await.unwrap_or(false) {
                // Operational mode — build full system prompt
                let soul = learning::load_soul(&**db).await.ok().flatten()
                    .map(|s| serde_json::to_string_pretty(&s).unwrap_or_default())
                    .unwrap_or_default();
                let user_profile = db.read("memory.user", "user").await.ok()
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                    .unwrap_or_default();
                let identity = db.read("memory.identity", "identity").await.ok()
                    .map(|v| serde_json::to_string_pretty(&v).unwrap_or_default())
                    .unwrap_or_default();
                let session_context = format!("Session: {}, Timezone: {}", session_name, config_tz);
                crate::brain::operational_mode(
                    &config_name, &soul, &identity, &user_profile, &session_context,
                )
            } else {
                // Learning mode — use phase-specific prompt
                "You are in Learning Mode. The system prompt will be set once the soul is defined.".to_string()
            };

            // Construct the right provider per-turn based on the
            // persisted config. Stage 8's /provider switch updates
            // config.system.api_provider AND mirrors the target's
            // per-provider key into loaded_config.api_key — so we
            // resolve the active key from the persisted config
            // rather than the brain-startup parameter (which holds
            // whatever key was on STATE at boot, NOT the post-swap
            // key). key_for falls back to legacy api_key when the
            // active provider matches, which is the post-swap case.
            let provider_kind = ProviderKind::from_str(&loaded_config.api_provider)
                .unwrap_or(ProviderKind::Anthropic);
            let active_key = loaded_config
                .key_for(provider_kind)
                .map(str::to_string)
                .unwrap_or_else(|| api_key.to_string());
            let provider: Arc<dyn LlmProvider> = match provider_kind {
                ProviderKind::Gemini => {
                    let model_id = resolve_gemini_model_id(&loaded_config);
                    info!(
                        target: "gemini::diag",
                        model = %model_id,
                        session = %session_name,
                        "gemini turn starting"
                    );
                    Arc::new(
                        GeminiProvider::with_model(active_key, model_id)
                            .with_cache(db.clone()),
                    )
                }
                ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(active_key)),
            };
            let descriptors: Vec<&'static tools::registry::ToolDescriptor> =
                tools::registry::all_descriptors().collect();
            let tool_manifest: ToolManifest = provider.build_tool_manifest(&descriptors);
            let system_bundle = SystemPromptBundle {
                fingerprint: prompt_fingerprint(&system_prompt),
                text: system_prompt,
                session_name: session_name.clone(),
            };

            // Set the in-flight flag so /provider commands queue
            // instead of swapping mid-turn. Cleared in a drop guard so
            // the flag never sticks if this future is cancelled.
            self_in_turn.store(true, Ordering::SeqCst);
            let in_turn_guard = InTurnGuard(self_in_turn.clone());

            // Load session history
            let history = {
                let mgr = session_mgr.read().await;
                mgr.load_history(&session_name).await.unwrap_or_default()
            };

            // Auto-KG-enrichment: wrap the user message in a <retrieved_context>
            // block when the knowledge graph has relevant prior knowledge. The
            // system prompt is left untouched so Anthropic prompt caching stays
            // warm. History persistence below saves `msg.content` (raw), so the
            // wrapper never leaks into subsequent turns.
            let enriched = crate::knowledge::enrichment::build_turn_context(
                db.as_ref(),
                &msg.content,
                &session_name,
                &loaded_config,
            )
            .await;

            // Build neutral-IR message history. Legacy persistence shape
            // (role+String) maps to text-only `Block::Text`; thinking
            // signatures were never persisted so cross-turn replay is
            // out of scope. Within a single turn, the loop pushes the
            // assistant turn's full content (including any
            // ProviderOpaque thinking blocks) verbatim between
            // iterations — the API requires this.
            let mut api_messages: Vec<ApiMessage> = history
                .iter()
                .map(legacy_message_to_api)
                .collect();
            api_messages.push(ApiMessage::user_text(&enriched));

            // Send thinking indicator.
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Thinking(
                    ThinkingState { is_thinking: true, name: config_name.clone() }
                )),
            })).await;

            // Native tool-use loop driven by TurnOutcome.
            const MAX_TOOL_ITERATIONS: usize = 10;
            let mut tool_iter: usize = 0;
            let mut last_response_text = String::new();

            // Per-request turn trace. Interior mutability via Arc<Mutex>
            // avoids &mut propagation through the `fn` ToolDescriptor
            // handler signature. `turn_index` is the *logical* turn number
            // (history is per-role-message; one user + one assistant = one
            // turn, so `len() / 2` names the turn the model is now starting).
            // The `turn_trace` tool's read path computes `target = ctx.turn_index
            // - back` and matches `back=1` to the immediately prior turn's
            // persisted entries — that arithmetic only holds under logical
            // (not message-count) units.
            let trace_handle: embra_tools_core::TurnTraceHandle =
                embra_tools_core::new_turn_trace_handle();
            let turn_index: usize = history.len() / 2;

            let first_stream = provider
                .stream_turn(&api_messages, &system_bundle, &tool_manifest)
                .await
                .map_err(|e| anyhow::anyhow!("Brain call failed: {}", e))?;
            let Some(mut current_turn) =
                collect_response(first_stream, &tx, &config_name).await?
            else {
                // Stream closed without Complete — treat as error and save nothing.
                drop(in_turn_guard);
                return Ok(());
            };
            // Track text from the most recent turn for persistence fallback.
            last_response_text = turn_text(&current_turn);
            api_messages.push(ApiMessage::assistant_blocks(current_turn.content.clone()));

            loop {
                match current_turn.outcome {
                    TurnOutcome::EndTurn
                    | TurnOutcome::MaxTokens
                    | TurnOutcome::EarlyStop(_) => {
                        // #32 defense: detect empty-text terminal turns
                        // after side-effectful work and emit a diagnostic
                        // token so the user/model don't desync. The model
                        // legitimately ends turn silently on pure-read
                        // flows, so the guard requires at least one
                        // side-effectful success.
                        if !current_turn.has_text() && !current_turn.has_tool_call() {
                            let side_effectful_count = trace_handle
                                .lock()
                                .map(|g| {
                                    g.iter()
                                        .filter(|e| !e.is_error)
                                        .filter(|e| {
                                            tools::registry::REGISTRY
                                                .get(e.tool_name.as_str())
                                                .map(|d| d.is_side_effectful)
                                                .unwrap_or(false)
                                        })
                                        .count()
                                })
                                .unwrap_or(0);
                            if side_effectful_count > 0 {
                                let diagnostic = format!(
                                    "(model ended turn silently after {} side-effectful tool call{}; see `turn_trace` for details)",
                                    side_effectful_count,
                                    if side_effectful_count == 1 { "" } else { "s" },
                                );
                                let _ = tx
                                    .send(Ok(ConversationResponse {
                                        response_type: Some(
                                            conversation_response::ResponseType::Token(
                                                StreamToken {
                                                    text: diagnostic.clone(),
                                                },
                                            ),
                                        ),
                                    }))
                                    .await;
                                last_response_text = diagnostic;
                                warn!(
                                    target: "dispatch",
                                    session = %session_name,
                                    side_effectful_count,
                                    "empty-text terminal turn after side-effectful tools"
                                );
                            }
                        }
                        break;
                    }

                    TurnOutcome::Pause => {
                        warn!(
                            target: "dispatch",
                            session = %session_name,
                            "stop_reason=pause_turn; resuming conversation"
                        );
                        let stream = provider
                            .stream_turn(&api_messages, &system_bundle, &tool_manifest)
                            .await
                            .map_err(|e| anyhow::anyhow!("Brain pause-resume failed: {}", e))?;
                        let Some(resp) = collect_response(stream, &tx, &config_name).await? else {
                            break;
                        };
                        current_turn = resp;
                        last_response_text = turn_text(&current_turn);
                        api_messages.push(ApiMessage::assistant_blocks(
                            current_turn.content.clone(),
                        ));
                        continue;
                    }

                    TurnOutcome::ToolUse => {
                        if tool_iter >= MAX_TOOL_ITERATIONS {
                            warn!(
                                target: "dispatch",
                                session = %session_name,
                                "tool iteration cap hit ({MAX_TOOL_ITERATIONS})"
                            );
                            break;
                        }
                        tool_iter += 1;

                        let mut result_blocks: Vec<Block> = Vec::new();
                        for block in &current_turn.content {
                            let Block::ToolCall { id, name, args, .. } = block else {
                                continue;
                            };
                            let started = std::time::Instant::now();
                            let started_at_rfc = chrono::Utc::now().to_rfc3339();
                            info!(
                                target: "dispatch",
                                session = %session_name,
                                tool = %name,
                                tool_use_id = %id,
                                "dispatch:start"
                            );
                            let ctx = tools::registry::DispatchContext {
                                db,
                                config: &loaded_config,
                                session_name: &session_name,
                                config_tz: &loaded_config.timezone,
                                trace: &trace_handle,
                                turn_index,
                            };
                            let outcome = tools::registry::dispatch(
                                name,
                                args.clone(),
                                ctx,
                            )
                            .await;
                            let elapsed_ms = started.elapsed().as_millis() as u64;
                            let (content, is_error) = match outcome {
                                Ok(s) => (s, false),
                                Err(embra_tools_core::DispatchError::Unknown(n)) => (
                                    format!(
                                        "Unknown tool: '{}'. Check the tool manifest for the correct name.",
                                        n
                                    ),
                                    true,
                                ),
                                Err(embra_tools_core::DispatchError::BadInput {
                                    tool,
                                    source,
                                }) => (
                                    format!(
                                        "Input schema mismatch for '{}': {}",
                                        tool, source
                                    ),
                                    true,
                                ),
                                Err(embra_tools_core::DispatchError::Handler(msg)) => (msg, true),
                            };
                            info!(
                                target: "dispatch",
                                session = %session_name,
                                tool = %name,
                                tool_use_id = %id,
                                elapsed_ms,
                                is_error,
                                "dispatch:end"
                            );

                            // Record in the in-memory turn trace and persist
                            // asynchronously to tools.turn_trace so cross-turn
                            // introspection is available to the model.
                            let input_json =
                                serde_json::to_string(args).unwrap_or_default();
                            let input_preview = preview_str(&input_json, 200);
                            let result_preview = preview_str(&content, 200);
                            let entry = embra_tools_core::TraceEntry {
                                tool_name: name.clone(),
                                tool_use_id: id.clone(),
                                input_preview: input_preview.clone(),
                                started_at: started_at_rfc.clone(),
                                elapsed_ms,
                                is_error,
                                result_preview: result_preview.clone(),
                            };
                            if let Ok(mut guard) = trace_handle.lock() {
                                guard.push_back(entry);
                            }
                            let persist_doc = serde_json::json!({
                                "session": session_name.to_string(),
                                "turn_index": turn_index,
                                "tool_use_id": id.clone(),
                                "tool_name": name.clone(),
                                "input_preview": input_preview,
                                "started_at": started_at_rfc,
                                "elapsed_ms": elapsed_ms,
                                "is_error": is_error,
                                "result_preview": result_preview,
                            });
                            let db_clone = db.clone();
                            tokio::spawn(async move {
                                let _ = db_clone.write("tools.turn_trace", &persist_doc).await;
                            });

                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Tool(
                                    ToolExecution {
                                        tool_use_id: id.clone(),
                                        tool_name: name.clone(),
                                        input_json,
                                        result: content.clone(),
                                        is_error,
                                    }
                                )),
                            })).await;
                            result_blocks.push(Block::ToolResult {
                                call_id: id.clone(),
                                content,
                                is_error,
                            });
                        }

                        if result_blocks.is_empty() {
                            // outcome claimed ToolUse but no ToolCall blocks
                            // were present — treat as a terminal state and exit.
                            break;
                        }

                        api_messages.push(ApiMessage::user_tool_results(result_blocks));
                        let stream = provider
                            .stream_turn(&api_messages, &system_bundle, &tool_manifest)
                            .await
                            .map_err(|e| {
                                anyhow::anyhow!(
                                    "Brain continuation failed (iter {tool_iter}): {e}"
                                )
                            })?;
                        let Some(resp) = collect_response(stream, &tx, &config_name).await? else {
                            break;
                        };
                        current_turn = resp;
                        last_response_text = turn_text(&current_turn);
                        api_messages.push(ApiMessage::assistant_blocks(
                            current_turn.content.clone(),
                        ));
                    }
                }
            }
            // Per-turn telemetry — surfaces the customtools-needed
            // pattern (spec D8). Operators can grep journalctl for
            // turns where Gemini emitted text but zero tool calls
            // and consider swapping to gemini-3.1-pro-preview-customtools.
            if provider_kind == ProviderKind::Gemini {
                let tool_call_count = trace_handle
                    .lock()
                    .map(|g| g.len())
                    .unwrap_or(0);
                info!(
                    target: "gemini::diag",
                    session = %session_name,
                    text_chars = last_response_text.len(),
                    tool_calls = tool_call_count,
                    iters = tool_iter,
                    "gemini turn complete"
                );
            }

            drop(in_turn_guard);

            // Drain any /provider switch the operator queued
            // mid-loop. Performed after in_turn clears and before
            // we save history so the next user-message handler
            // observes the new provider state.
            {
                let mut guard = pending_provider.lock().await;
                if let Some(target) = guard.take() {
                    perform_provider_swap(target, db, session_mgr, &tx).await;
                }
            }

            // Save conversation to session history.
            //
            // Persistence still uses the legacy Message { role, content: String }
            // shape — Stage 8 introduces format_version-aware typed-block
            // storage. We store the raw user message (pre-enrichment) and the
            // concatenated text of the final assistant response. Thinking
            // signatures and tool_use/tool_result blocks are NOT persisted in
            // Stage 5; the model doesn't require cross-turn thinking
            // preservation, and tool results are dropped silently (they reconstruct
            // on re-runs if the user asks again).
            {
                let mgr = session_mgr.read().await;
                let append_outcome = mgr
                    .append_message(&session_name, &Message::user(&msg.content))
                    .await;
                if let Err(crate::sessions::SessionError::LegacyReadOnly(ref sess)) =
                    append_outcome
                {
                    warn!(
                        target: "sessions",
                        session = %sess,
                        "legacy session rejected append; surfacing to client"
                    );
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!(
                                    "Session '{}' is legacy (pre-native-tools) and is read-only. Create a new session to continue.",
                                    sess
                                ),
                                msg_type: SystemMessageType::Error as i32,
                            }
                        )),
                    })).await;
                    return Ok(());
                }
                let final_text = if last_response_text.trim().is_empty() {
                    "(no response)".to_string()
                } else {
                    last_response_text.clone()
                };
                let _ = mgr
                    .append_message(&session_name, &Message::assistant(&final_text))
                    .await;
            }

            Ok(())
        }

        Some(conversation_request::RequestType::SlashCommand(cmd)) => {
            if let Some(synthetic_prompt) = handle_slash_command(
                &cmd.command, &cmd.args, tx, db, session_mgr, config_tz, api_key, in_turn, pending_provider, pending_key_setup
            ).await {
                // Slash command requested a synthetic user turn — feed it through the Brain.
                let synthetic = ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content: synthetic_prompt, timestamp: None }
                    )),
                };
                Box::pin(handle_request(
                    synthetic, tx, db, session_mgr, config_tz, api_key, in_turn, pending_provider, pending_key_setup
                )).await?;
            }
            Ok(())
        }

        Some(conversation_request::RequestType::SessionAttach(attach)) => {
            let mut mgr = session_mgr.write().await;

            // Determine session: explicit name > most recent active > "main"
            let session_name = if !attach.session_name.is_empty() {
                attach.session_name.clone()
            } else if let Some(ref active) = mgr.active_session {
                active.clone()
            } else if let Ok(Some(recent)) = mgr.get_most_recent_active().await {
                recent.name.clone()
            } else {
                "main".to_string()
            };

            // Ensure session exists
            let new_session = !mgr.session_exists(&session_name).await.unwrap_or(false);
            if new_session {
                let _ = mgr.create(&session_name).await;
            }
            drop(mgr);

            // Cross-provider session-resume hard-block (locked
            // decision #3). For an existing session whose meta
            // recorded a different provider than the active
            // process-level provider, refuse the attach with a
            // clear error rather than silently corrupting state on
            // the first turn.
            if !new_session {
                let session_provider = read_session_provider(db, &session_name).await;
                let active_provider = config::load_config(&**db)
                    .await
                    .map(|c| c.api_provider)
                    .unwrap_or_else(|_| "anthropic".to_string());
                if !providers_compatible(&session_provider, &active_provider) {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!(
                                        "Session '{}' was recorded under {}. Use /provider {} to continue, or /new <name> to start a fresh session.",
                                        session_name, session_provider, session_provider
                                    ),
                                    msg_type: SystemMessageType::Error as i32,
                                },
                            )),
                        }))
                        .await;
                    return Ok(());
                }
            }

            // Mark active only after the cross-provider check passes.
            session_mgr.write().await.active_session = Some(session_name.clone());

            // Load and send session history so console displays prior conversation
            {
                let mgr = session_mgr.read().await;
                if let Ok(history) = mgr.load_history(&session_name).await {
                    for msg in &history {
                        let role_display = if msg.role == "user" { "user" } else { "assistant" };
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!("[{}] {}", role_display, msg.content),
                                    msg_type: SystemMessageType::Reconnection as i32,
                                }
                            )),
                        })).await;
                    }
                }
            }

            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: format!("Session '{}' attached ({} messages restored)", session_name,
                            session_mgr.read().await.load_history(&session_name).await.map(|h| h.len()).unwrap_or(0)),
                        msg_type: SystemMessageType::Reconnection as i32,
                    }
                )),
            })).await;

            // Send ModeTransition so console knows the correct mode, timezone, and name
            let is_sealed = learning::is_soul_sealed(&**db).await.unwrap_or(false);
            let mode = if is_sealed { OperatingMode::Operational } else { OperatingMode::Learning };
            let cfg = config::load_config(&**db).await.ok();
            let tz = cfg.as_ref().map(|c| c.timezone.clone()).unwrap_or_else(|| config_tz.to_string());
            let name = cfg.as_ref().map(|c| c.name.clone()).unwrap_or_else(|| "Embra".to_string());
            let active_provider = cfg.as_ref().map(|c| c.api_provider.clone()).unwrap_or_else(|| "anthropic".to_string());
            let model = display_model_for(&active_provider);
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::ModeChange(
                    ModeTransition {
                        from_mode: OperatingMode::Unspecified as i32,
                        to_mode: mode as i32,
                        message: if is_sealed {
                            format!(
                                "Operational — Name: {} — Session: {} — TZ: {} — Brain: {}",
                                name, session_name, tz, model
                            )
                        } else {
                            format!("Learning Mode — Name: {} — TZ: {} — Brain: {}", name, tz, model)
                        },
                    }
                )),
            })).await;

            Ok(())
        }

        None => Ok(()),
    }
}

async fn handle_slash_command(
    command: &str,
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    config_tz: &str,
    _api_key: &str,
    in_turn: &Arc<AtomicBool>,
    pending_provider: &Arc<Mutex<Option<ProviderKind>>>,
    pending_key_setup: &Arc<Mutex<Option<ProviderKind>>>,
) -> Option<String> {
    // Helper to send a system message
    let send_msg = |tx: &mpsc::Sender<Result<ConversationResponse, Status>>, content: String| {
        let tx = tx.clone();
        async move {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage { content, msg_type: SystemMessageType::Info as i32 }
                )),
            })).await;
        }
    };

    // Helper to send a ModeTransition with updated session name.
    // Sprint 4: includes the active model in the message so the
    // console status bar can render it without a separate event.
    let tz = config_tz.to_string();
    let cfg_loaded = config::load_config(&**db).await.ok();
    let config_name = cfg_loaded
        .as_ref()
        .map(|c| c.name.clone())
        .unwrap_or_else(|| "Embra".to_string());
    let active_provider = cfg_loaded
        .as_ref()
        .map(|c| c.api_provider.clone())
        .unwrap_or_else(|| "anthropic".to_string());
    let model = display_model_for(&active_provider).to_string();
    let send_session_update = {
        let config_name = config_name.clone();
        move |tx: &mpsc::Sender<Result<ConversationResponse, Status>>, session_name: &str| {
            let tx = tx.clone();
            let session = session_name.to_string();
            let tz = tz.clone();
            let config_name = config_name.clone();
            let model = model.clone();
            async move {
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::ModeChange(
                        ModeTransition {
                            from_mode: OperatingMode::Operational as i32,
                            to_mode: OperatingMode::Operational as i32,
                            message: format!(
                                "Operational — Name: {} — Session: {} — TZ: {} — Brain: {}",
                                config_name, session, tz, model
                            ),
                        }
                    )),
                })).await;
            }
        }
    };

    match command {
        "/help" => {
            send_msg(tx, "Available commands:\n  /sessions, /switch <name>, /new <name>, /close\n  /status, /soul, /identity, /mode\n  /provider                          Show active provider, model, session\n  /provider <anthropic|gemini>       Switch provider for future turns\n  /provider --setup [<kind>]         Add an alternate provider's API key (multi-turn)\n  /github-token <token>              Set GitHub token\n  /ssh-keygen                        Generate SSH key pair\n  /ssh-copy-id <user@host>           Copy SSH key to host\n  /git-setup <name> | <email>        Set git user config\n  /feedback-loop                     (EXPERIMENTAL) trigger Phase 3 feedback-loop protocol\n  /help".to_string()).await;
        }
        "/feedback-loop" => {
            send_msg(tx, "\u{26A0} EXPERIMENTAL: Phase 3 Continuity Engine preview (manual trigger)\nInitiating feedback loop per feedback-loop-spec-v2.md.\nThe Brain will now begin Step 1.1 (Gather \u{2192} Introspect).\nThis is a multi-turn protocol \u{2014} expect 5+ tool invocations.".to_string()).await;
            return Some(build_feedback_loop_prompt());
        }
        "/provider" => {
            handle_provider_command(args, tx, db, session_mgr, in_turn, pending_provider, pending_key_setup).await;
        }
        "/status" => {
            let status = tools::system_status(db).await;
            send_msg(tx, serde_json::to_string_pretty(&status).unwrap_or_else(|_| "Failed to get status".to_string())).await;
        }
        "/sessions" => {
            let mgr = session_mgr.read().await;
            let is_sealed = learning::is_soul_sealed(&**db).await.unwrap_or(false);
            let output = match mgr.list().await {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        "No sessions.".to_string()
                    } else {
                        sessions.iter().map(|s| {
                            let indicator = if s.name == "learning" {
                                if is_sealed { " [sealed]" } else { " [learning]" }
                            } else {
                                ""
                            };
                            format!("  {}{} [{:?}] last active: {}", s.name, indicator, s.state, s.last_active.format("%Y-%m-%d %H:%M"))
                        }).collect::<Vec<_>>().join("\n")
                    }
                }
                Err(e) => format!("Error listing sessions: {}", e),
            };
            send_msg(tx, output).await;
        }
        "/new" => {
            if args.is_empty() {
                send_msg(tx, "Usage: /new <session-name>".to_string()).await;
            } else {
                let mut mgr = session_mgr.write().await;
                match mgr.create(args).await {
                    Ok(s) => {
                        mgr.active_session = Some(s.name.clone());
                        let name = s.name.clone();
                        drop(mgr);
                        send_msg(tx, format!("Created and switched to session '{}'", name)).await;
                        send_session_update(tx, &name).await;
                    }
                    Err(e) => { send_msg(tx, format!("Error creating session: {}", e)).await; }
                }
            }
        }
        "/switch" => {
            if args.is_empty() {
                send_msg(tx, "Usage: /switch <session-name>".to_string()).await;
            } else if args == "learning" {
                send_msg(tx, "Learning session is read-only. Use /soul to view the sealed soul document.".to_string()).await;
            } else {
                let mut mgr = session_mgr.write().await;
                if mgr.session_exists(args).await.unwrap_or(false) {
                    let _ = mgr.reattach(args).await;
                    mgr.active_session = Some(args.to_string());
                    // Load and send session history
                    if let Ok(history) = mgr.load_history(args).await {
                        drop(mgr);
                        for msg in &history {
                            let role_display = if msg.role == "user" { "user" } else { "assistant" };
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(
                                    SystemMessage {
                                        content: format!("[{}] {}", role_display, msg.content),
                                        msg_type: SystemMessageType::Reconnection as i32,
                                    }
                                )),
                            })).await;
                        }
                        send_msg(tx, format!("Switched to session '{}' ({} messages)", args, history.len())).await;
                    } else {
                        drop(mgr);
                        send_msg(tx, format!("Switched to session '{}'", args)).await;
                    }
                    send_session_update(tx, args).await;
                } else {
                    send_msg(tx, format!("Session '{}' does not exist", args)).await;
                }
            }
        }
        "/close" => {
            let mut mgr = session_mgr.write().await;
            if let Some(ref name) = mgr.active_session.clone() {
                let _ = mgr.close(name).await;
                mgr.active_session = None;
                send_msg(tx, format!("Closed session '{}'", name)).await;
            } else {
                send_msg(tx, "No active session".to_string()).await;
            }
        }
        "/soul" => {
            let output = match learning::load_soul(&**db).await {
                Ok(Some(soul)) => serde_json::to_string_pretty(&soul).unwrap_or_default(),
                Ok(None) => "No soul sealed yet.".to_string(),
                Err(e) => format!("Error loading soul: {}", e),
            };
            send_msg(tx, output).await;
        }
        "/identity" => {
            let output = match db.read("memory.identity", "identity").await {
                Ok(doc) => serde_json::to_string_pretty(&doc).unwrap_or_default(),
                Err(_) => "No identity document found.".to_string(),
            };
            send_msg(tx, output).await;
        }
        "/mode" => {
            let sealed = learning::is_soul_sealed(&**db).await.unwrap_or(false);
            send_msg(tx, if sealed { "Operational (soul sealed)".to_string() } else { "Learning (soul not sealed)".to_string() }).await;
        }
        "/github-token" => {
            if args.is_empty() {
                let has_token = tools::engineering::resolve_github_token(&**db).await.is_some();
                if has_token {
                    send_msg(tx, "GitHub token is configured. Use /github-token <token> to update it.".to_string()).await;
                } else {
                    send_msg(tx, "Usage: /github-token <your-github-token>\nSets GITHUB_TOKEN for GitHub API access (issues, PRs, clone).".to_string()).await;
                }
            } else {
                let token = args.trim().to_string();
                if !token.starts_with("ghp_") && !token.starts_with("gho_") && !token.starts_with("github_pat_") {
                    send_msg(tx, "Warning: token doesn't look like a GitHub token (expected ghp_/gho_/github_pat_ prefix). Saving anyway.".to_string()).await;
                }
                // Save to WardSONDB config.system
                match config::load_config(&**db).await {
                    Ok(mut cfg) => {
                        cfg.github_token = Some(token.clone());
                        if let Err(e) = config::save_config(&**db, &cfg).await {
                            send_msg(tx, format!("Failed to save token: {}", e)).await;
                            return None;
                        }
                    }
                    Err(_) => {
                        send_msg(tx, "No system config found. Run config wizard first.".to_string()).await;
                        return None;
                    }
                }
                // Persist to STATE partition for boot propagation
                let path = "/embra/state/github_token";
                if let Some(parent) = std::path::Path::new(path).parent() {
                    let _ = std::fs::create_dir_all(parent);
                }
                let _ = std::fs::write(path, &token);
                // Set env var for immediate availability
                // SAFETY: single-threaded access to env at this point in the slash command handler
                unsafe { std::env::set_var("GITHUB_TOKEN", &token); }
                send_msg(tx, "GitHub token saved. GitHub tools (gh_issues, gh_prs, git_clone, etc.) are now active.".to_string()).await;
            }
        }
        "/ssh-keygen" => {
            let key_path = "/embra/workspace/.ssh/id_ed25519";
            let pub_path = format!("{}.pub", key_path);

            if std::path::Path::new(key_path).exists() {
                match std::fs::read_to_string(&pub_path) {
                    Ok(pubkey) => {
                        send_msg(tx, format!(
                            "SSH key already exists. Public key:\n\n{}\n\nAdd this to ~/.ssh/authorized_keys on your target hosts.",
                            pubkey.trim()
                        )).await;
                    }
                    Err(_) => {
                        send_msg(tx, "SSH private key exists but public key is missing.".to_string()).await;
                    }
                }
                return None;
            }

            // Ensure .ssh directory exists with correct permissions
            let ssh_dir = "/embra/workspace/.ssh";
            let _ = std::fs::create_dir_all(ssh_dir);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                let _ = std::fs::set_permissions(ssh_dir, std::fs::Permissions::from_mode(0o700));
            }

            match tokio::process::Command::new("ssh-keygen")
                .args(["-t", "ed25519", "-f", key_path, "-N", "", "-C", "embra@embraos"])
                .output()
                .await
            {
                Ok(out) if out.status.success() => {
                    match std::fs::read_to_string(&pub_path) {
                        Ok(pubkey) => {
                            send_msg(tx, format!(
                                "SSH key generated. Public key:\n\n{}\n\nAdd this to ~/.ssh/authorized_keys on your target hosts.",
                                pubkey.trim()
                            )).await;
                        }
                        Err(e) => {
                            send_msg(tx, format!("Key generated but could not read public key: {}", e)).await;
                        }
                    }
                }
                Ok(out) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    send_msg(tx, format!("ssh-keygen failed: {}", stderr.trim())).await;
                }
                Err(e) => {
                    send_msg(tx, format!("Failed to run ssh-keygen: {}", e)).await;
                }
            }
        }
        "/ssh-copy-id" => {
            if args.is_empty() {
                send_msg(tx, "Usage: /ssh-copy-id <user@host>\nCopies SSH public key to remote host (RFC 1918 only).".to_string()).await;
                return None;
            }

            let pub_path = "/embra/workspace/.ssh/id_ed25519.pub";
            if !std::path::Path::new(pub_path).exists() {
                send_msg(tx, "No SSH key found. Run /ssh-keygen first.".to_string()).await;
                return None;
            }

            let target = args.trim();
            let host = target.rsplit('@').next().unwrap_or(target);
            if !tools::security::is_private_address(host) {
                send_msg(tx, format!(
                    "Denied: '{}' is not a private address. SSH is restricted to RFC 1918 ranges.",
                    host
                )).await;
                return None;
            }

            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::process::Command::new("ssh-copy-id")
                    .args([
                        "-i", pub_path,
                        "-o", "StrictHostKeyChecking=accept-new",
                        "-o", &format!("UserKnownHostsFile=/embra/workspace/.ssh/known_hosts"),
                        target,
                    ])
                    .output(),
            ).await {
                Ok(Ok(out)) if out.status.success() => {
                    send_msg(tx, format!("SSH public key copied to {}.", target)).await;
                }
                Ok(Ok(out)) => {
                    let stderr = String::from_utf8_lossy(&out.stderr);
                    send_msg(tx, format!(
                        "ssh-copy-id failed (password auth may be required or key already present):\n{}",
                        stderr.trim()
                    )).await;
                }
                Ok(Err(e)) => {
                    send_msg(tx, format!("Failed to run ssh-copy-id: {}", e)).await;
                }
                Err(_) => {
                    send_msg(tx, "ssh-copy-id timed out after 30 seconds.".to_string()).await;
                }
            }
        }
        "/git-setup" => {
            if args.is_empty() {
                let name_out = tokio::process::Command::new("git")
                    .args(["config", "--global", "user.name"]).output().await
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                let email_out = tokio::process::Command::new("git")
                    .args(["config", "--global", "user.email"]).output().await
                    .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
                    .unwrap_or_default();
                send_msg(tx, format!(
                    "Git config:\n  user.name:  {}\n  user.email: {}\n\nUsage: /git-setup <name> | <email>",
                    if name_out.is_empty() { "(not set)".to_string() } else { name_out },
                    if email_out.is_empty() { "(not set)".to_string() } else { email_out },
                )).await;
            } else {
                let parts: Vec<&str> = args.splitn(2, " | ").collect();
                if parts.len() < 2 {
                    send_msg(tx, "Usage: /git-setup <name> | <email>".to_string()).await;
                    return None;
                }
                let name = parts[0].trim();
                let email = parts[1].trim();
                let _ = tokio::process::Command::new("git")
                    .args(["config", "--global", "user.name", name]).output().await;
                let _ = tokio::process::Command::new("git")
                    .args(["config", "--global", "user.email", email]).output().await;
                send_msg(tx, format!("Git config updated: user.name='{}', user.email='{}'", name, email)).await;
            }
        }
        _ => {
            send_msg(tx, format!("Unknown command: {}. Type /help for available commands.", command)).await;
        }
    }
    None
}

/// `/provider` command surface: status, switch, and (as deferred via
/// spec D2) a deny-then-instruct path when no key is recorded for the
/// alternate provider. Idle switches apply immediately; in-turn
/// switches queue via `pending_provider` and drain after the loop.
async fn handle_provider_command(
    args: &str,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    in_turn: &Arc<AtomicBool>,
    pending_provider: &Arc<Mutex<Option<ProviderKind>>>,
    pending_key_setup: &Arc<Mutex<Option<ProviderKind>>>,
) {
    let send_msg = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content,
                            msg_type: kind as i32,
                        },
                    )),
                }))
                .await;
        }
    };

    let action = args.trim();

    // /provider --setup [<kind>] — multi-turn key add. Sets the
    // pending_key_setup flag and prompts; the operator's next user
    // message is intercepted in handle_request as the candidate key.
    if let Some(rest) = action.strip_prefix("--setup") {
        let cfg = match config::load_config(&**db).await {
            Ok(c) => c,
            Err(_) => {
                send_msg(
                    "No system config found — run setup wizard first.".to_string(),
                    SystemMessageType::Error,
                )
                .await;
                return;
            }
        };
        let explicit = rest.trim();
        let target = if explicit.is_empty() {
            // Auto-target the provider whose per-provider key is
            // missing. If both are set, require explicit form.
            let anth_present = cfg.key_for(ProviderKind::Anthropic).is_some();
            let gem_present = cfg.key_for(ProviderKind::Gemini).is_some();
            match (anth_present, gem_present) {
                (true, false) => ProviderKind::Gemini,
                (false, true) => ProviderKind::Anthropic,
                (true, true) => {
                    send_msg(
                        "Both providers already have keys. Use /provider --setup <anthropic|gemini> to replace one.".to_string(),
                        SystemMessageType::Error,
                    ).await;
                    return;
                }
                (false, false) => {
                    // Pre-wizard / cleared state.
                    send_msg(
                        "No keys recorded yet. Re-run setup wizard first.".to_string(),
                        SystemMessageType::Error,
                    )
                    .await;
                    return;
                }
            }
        } else {
            match ProviderKind::from_str(explicit) {
                Some(k) => k,
                None => {
                    send_msg(
                        format!(
                            "Unknown provider '{}'. Use 'anthropic' or 'gemini'.",
                            explicit
                        ),
                        SystemMessageType::Error,
                    )
                    .await;
                    return;
                }
            }
        };
        // Set the flag (replacing any prior pending setup).
        *pending_key_setup.lock().await = Some(target);
        // Prompt for the key. Console renders as a system message;
        // the next user input will be intercepted.
        let prompt_text = match target {
            ProviderKind::Anthropic => "Enter your Anthropic API key (next message):",
            ProviderKind::Gemini => "Enter your Gemini API key (next message):",
        };
        let _ = tx
            .send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Setup(
                    SetupPrompt {
                        field_type: SetupFieldType::Text as i32,
                        prompt: prompt_text.to_string(),
                        options: vec![],
                        default_value: String::new(),
                    },
                )),
            }))
            .await;
        return;
    }

    match action {
        "" | "status" => {
            let cfg = config::load_config(&**db).await.ok();
            let provider = cfg
                .as_ref()
                .map(|c| c.api_provider.clone())
                .unwrap_or_else(|| "anthropic".to_string());
            let model = display_model_for(&provider);
            let session = session_mgr
                .read()
                .await
                .active_session
                .clone()
                .unwrap_or_else(|| "<none>".to_string());
            send_msg(
                format!(
                    "Active provider: {}. Model: {}. Session: {}.",
                    provider, model, session
                ),
                SystemMessageType::Info,
            )
            .await;
        }
        "anthropic" | "gemini" => {
            let target = ProviderKind::from_str(action).unwrap();
            // Pre-check: do we have a key for the target? Currently
            // config.system.api_key is single-key, so switching to a
            // provider whose key was never wizard-validated would
            // fail at the next turn. Surface that early.
            let cfg = match config::load_config(&**db).await {
                Ok(c) => c,
                Err(_) => {
                    send_msg(
                        "No system config found — re-run setup wizard first.".to_string(),
                        SystemMessageType::Error,
                    )
                    .await;
                    return;
                }
            };
            if cfg.api_provider == target.as_str() {
                send_msg(
                    format!("Already using {}. No change.", target.as_str()),
                    SystemMessageType::Info,
                )
                .await;
                return;
            }
            // Per-provider key check (Sprint 4 D2). Replaces the
            // pre-D2 prefix heuristic with an actual presence check
            // against the per-provider field. Uses cfg.key_for(target)
            // which falls back to the legacy api_key when the active
            // provider matches.
            if cfg.key_for(target).is_none() {
                send_msg(
                    format!(
                        "No API key recorded for {}. Run /provider --setup {} to add one.",
                        target.as_str(),
                        target.as_str()
                    ),
                    SystemMessageType::Error,
                )
                .await;
                return;
            }

            if in_turn.load(Ordering::SeqCst) {
                let mut guard = pending_provider.lock().await;
                let prev = guard.replace(target);
                let body = match prev {
                    Some(p) if p != target => format!(
                        "Switch queued — replacing previously queued switch to {}.",
                        p.as_str()
                    ),
                    _ => "Switch queued. Will apply after current turn completes.".to_string(),
                };
                send_msg(body, SystemMessageType::Info).await;
                return;
            }
            perform_provider_swap(target, db, session_mgr, tx).await;
        }
        _ => {
            send_msg(
                format!("Unknown provider '{}'. Use 'anthropic' or 'gemini'.", action),
                SystemMessageType::Error,
            )
            .await;
        }
    }
}

/// Apply a provider switch. Runs after `in_turn` has cleared (either
/// because the loop just ended or because the command arrived idle).
/// Updates config.system.api_provider, the active session's
/// meta.provider/meta.model, and /embra/state/api_provider.
async fn perform_provider_swap(
    target: ProviderKind,
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
) {
    // 1. Update config doc — set api_provider AND mirror the
    //    target's per-provider key into the legacy api_key field so
    //    existing read paths see the right value. Also mirror to
    //    /embra/state/api_key so the supervisor's existing read path
    //    keeps working until a future change teaches embrad about
    //    per-provider STATE.
    if let Ok(mut cfg) = config::load_config(&**db).await {
        let target_key = match cfg.key_for(target) {
            Some(k) => k.to_string(),
            None => {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!(
                                    "Cannot switch — no API key recorded for {}.",
                                    target.as_str()
                                ),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
                return;
            }
        };
        cfg.api_provider = target.as_str().to_string();
        cfg.api_key = target_key.clone();
        if let Err(e) = config::save_config(&**db, &cfg).await {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Failed to persist provider switch: {}", e),
                            msg_type: SystemMessageType::Error as i32,
                        },
                    )),
                }))
                .await;
            return;
        }
        let _ = std::fs::write("/embra/state/api_key", &target_key);
    }

    // 2. Update active session's meta.provider / meta.model so
    //    cross-session resume sees the right provider.
    let active = session_mgr.read().await.active_session.clone();
    if let Some(name) = active {
        let collection = format!("sessions.{}.meta", name);
        if let Ok(meta_doc) = db.read(&collection, &name).await {
            let mut doc = meta_doc;
            if let Some(obj) = doc.as_object_mut() {
                obj.insert(
                    "provider".into(),
                    serde_json::Value::String(target.as_str().to_string()),
                );
                obj.insert(
                    "model".into(),
                    serde_json::Value::String(display_model_for(target.as_str()).to_string()),
                );
            }
            let _ = db.update(&collection, &name, &doc).await;
        }
    }

    // 3. Persist to STATE so embrad picks the right provider on the
    //    next boot.
    let _ = std::fs::write("/embra/state/api_provider", target.as_str());

    let _ = tx
        .send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::System(
                SystemMessage {
                    content: format!(
                        "Provider switched to {}. Next turn will use {}.",
                        target.as_str(),
                        display_model_for(target.as_str())
                    ),
                    msg_type: SystemMessageType::Info as i32,
                },
            )),
        }))
        .await;
}

fn display_model_for(provider: &str) -> &'static str {
    match provider {
        "gemini" => "gemini-3.1-pro",
        _ => "opus-4.7",
    }
}

/// Validate + persist a candidate API key submitted via the
/// `/provider --setup` multi-turn flow. On valid, writes the per-
/// provider config field and STATE file, clears the pending flag,
/// and acknowledges. On invalid, leaves the flag set so the next
/// message is re-intercepted (the operator can retype without
/// re-running --setup).
async fn handle_pending_key_setup(
    target: ProviderKind,
    candidate: String,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    db: &Arc<WardsonDbClient>,
    pending_key_setup: &Arc<Mutex<Option<ProviderKind>>>,
) {
    let send_msg = |content: String, kind: SystemMessageType| {
        let tx = tx.clone();
        async move {
            let _ = tx
                .send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content,
                            msg_type: kind as i32,
                        },
                    )),
                }))
                .await;
        }
    };

    if candidate.is_empty() {
        send_msg(
            format!(
                "Empty key — type the {} API key on the next message, or use a non-/provider command to abort setup.",
                target.as_str()
            ),
            SystemMessageType::Error,
        )
        .await;
        return;
    }

    // Validate via the same probe path the wizard uses.
    if let Err(msg) = config::check_api_key(target, &candidate).await {
        send_msg(
            format!(
                "{} Try again on the next message, or use a non-/provider command to abort.",
                msg
            ),
            SystemMessageType::Error,
        )
        .await;
        return;
    }

    // Persist into config.
    let mut cfg = match config::load_config(&**db).await {
        Ok(c) => c,
        Err(_) => {
            send_msg(
                "No system config found — re-run setup wizard first.".to_string(),
                SystemMessageType::Error,
            )
            .await;
            // Clear flag — config is broken, bailing entirely.
            *pending_key_setup.lock().await = None;
            return;
        }
    };
    match target {
        ProviderKind::Anthropic => cfg.anthropic_api_key = Some(candidate.clone()),
        ProviderKind::Gemini => cfg.gemini_api_key = Some(candidate.clone()),
    }
    if let Err(e) = config::save_config(&**db, &cfg).await {
        send_msg(
            format!("Failed to persist key: {}", e),
            SystemMessageType::Error,
        )
        .await;
        return;
    }

    // Persist to per-provider STATE file.
    let state_path = match target {
        ProviderKind::Anthropic => "/embra/state/api_key_anthropic",
        ProviderKind::Gemini => "/embra/state/api_key_gemini",
    };
    if let Err(e) = std::fs::write(state_path, &candidate) {
        warn!(
            target: "config",
            "Could not write per-provider key to {}: {}",
            state_path,
            e
        );
    }

    // Clear flag and acknowledge.
    *pending_key_setup.lock().await = None;
    send_msg(
        format!(
            "Key for {} recorded. Use /provider {} to switch, or stay on the current provider.",
            target.as_str(),
            target.as_str()
        ),
        SystemMessageType::Info,
    )
    .await;
}

/// Pick the Gemini model id, honoring (in priority order):
/// 1. `EMBRA_GEMINI_MODEL` env var (set by --api-provider startup or
///    by the operator before brain restart).
/// 2. `config.system.gemini_model` (persistent, set via wizard or
///    future `/provider --gemini-model` command).
/// 3. The provider crate's default constant (`gemini-3.1-pro-preview`).
///
/// Used by both the operational and learning paths to bias toward
/// `gemini-3.1-pro-preview-customtools` when standard Gemini ignores
/// custom tools (spec D8). Reads env once via the wrapper; the
/// pure inner is unit-tested directly to avoid env-mutation races
/// across the test suite.
fn resolve_gemini_model_id(cfg: &config::SystemConfig) -> String {
    let env_override = std::env::var("EMBRA_GEMINI_MODEL").ok();
    resolve_gemini_model_id_inner(env_override.as_deref(), cfg.gemini_model.as_deref())
}

fn resolve_gemini_model_id_inner(env: Option<&str>, cfg_field: Option<&str>) -> String {
    if let Some(s) = env {
        let trimmed = s.trim();
        if !trimmed.is_empty() {
            return trimmed.to_string();
        }
    }
    if let Some(s) = cfg_field {
        if !s.is_empty() {
            return s.to_string();
        }
    }
    "gemini-3.1-pro-preview".to_string()
}

/// Read `meta.provider` for a session, defaulting to `"anthropic"`
/// for pre-v9 docs that don't carry the field. Stage 9's migration
/// stamps the field on every session meta; until then this default
/// preserves backward compatibility.
async fn read_session_provider(db: &Arc<WardsonDbClient>, session_name: &str) -> String {
    let collection = format!("sessions.{}.meta", session_name);
    match db.read(&collection, session_name).await {
        Ok(meta_doc) => meta_doc
            .get("provider")
            .and_then(|v| v.as_str())
            .unwrap_or("anthropic")
            .to_string(),
        Err(_) => "anthropic".to_string(),
    }
}

/// Two providers are compatible if they're the same string or one
/// side is empty. Treats blank/missing as "no constraint" so attach
/// of a freshly-created session against any active provider works.
fn providers_compatible(a: &str, b: &str) -> bool {
    a.is_empty() || b.is_empty() || a == b
}

/// Embedded feedback-loop protocol spec (Phase 3 preview, v2).
/// Read-only: baked into the binary at compile time.
const FEEDBACK_LOOP_SPEC_V2: &str = include_str!("brain/feedback_loop_spec_v2.md");

/// Build the synthetic user message that kicks off the feedback-loop protocol.
/// Fed through `handle_request` as if the user had typed it.
fn build_feedback_loop_prompt() -> String {
    format!(
        "MANUAL FEEDBACK LOOP TRIGGER \u{2014} EXPERIMENTAL (Phase 3 Continuity Engine preview)\n\
\n\
Will has invoked /feedback-loop. You are to initiate the feedback loop\n\
self-evaluation protocol using the spec below. Work through the protocol\n\
using the tools you already have. Follow the spec's governance boundary:\n\
S0/S1 actions auto-execute; S2/S3 actions are presented to Will for approval.\n\
\n\
Begin with Step 1.1 (Introspect: Load Evaluation Criteria). Work through\n\
the protocol sequentially. Pause at the Step 4.2 governance boundary to\n\
present S2/S3 findings for approval.\n\
\n\
=== FEEDBACK LOOP SPEC v2.0 (read-only, embedded in binary) ===\n\
\n\
{}\n\
\n\
=== END SPEC ===\n\
\n\
Acknowledge you're initiating the protocol, then execute Step 1.1.",
        FEEDBACK_LOOP_SPEC_V2
    )
}

/// Drive the 6-phase Learning Mode over gRPC.
/// Creates LearningState, runs each phase with Brain calls, detects [PHASE_COMPLETE],
/// persists extracted documents, and transitions to Operational when complete.
async fn run_learning_loop(
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    incoming: &mut Streaming<ConversationRequest>,
    db: &Arc<WardsonDbClient>,
    config: &config::SystemConfig,
    api_key: &str,
) -> anyhow::Result<()> {
    let mut state = learning::LearningState::new();

    // Construct the right provider once per learning session.
    // Learning never invokes tools; pass an empty tool manifest so
    // the request body omits `tools` / `tool_choice` (the provider
    // handles that gating). Provider selection comes from the
    // persisted config (or its anthropic default for first-run).
    let provider_kind = ProviderKind::from_str(&config.api_provider)
        .unwrap_or(ProviderKind::Anthropic);
    let active_key = config
        .key_for(provider_kind)
        .map(str::to_string)
        .unwrap_or_else(|| api_key.to_string());
    let provider: Arc<dyn LlmProvider> = match provider_kind {
        ProviderKind::Gemini => {
            let model_id = resolve_gemini_model_id(config);
            info!(
                target: "gemini::diag",
                model = %model_id,
                "gemini learning turn starting"
            );
            Arc::new(
                GeminiProvider::with_model(active_key, model_id).with_cache(db.clone()),
            )
        }
        ProviderKind::Anthropic => Arc::new(AnthropicProvider::new(active_key)),
    };
    let empty_manifest: ToolManifest = provider.build_tool_manifest(&[]);

    // Resume support: load any previously persisted documents
    if let Ok(profile) = db.read("memory.user", "user").await {
        state.user_profile = Some(profile);
        state.phase = learning::LearningPhase::IdentityFormation;
    }
    if let Ok(identity) = db.read("memory.identity", "identity").await {
        state.identity = Some(identity);
        state.phase = learning::LearningPhase::SoulDefinition;
    }
    // If soul is sealed, we shouldn't be here — but check anyway
    if learning::is_soul_sealed(&**db).await.unwrap_or(false) {
        return Ok(());
    }

    // Send mode transition
    let _ = tx.send(Ok(ConversationResponse {
        response_type: Some(conversation_response::ResponseType::ModeChange(
            ModeTransition {
                from_mode: OperatingMode::Setup as i32,
                to_mode: OperatingMode::Learning as i32,
                message: format!("Learning Mode — Name: {} — Phase: {} — TZ: {}", config.name, learning::phase_label(&state.phase), config.timezone),
            }
        )),
    })).await;

    loop {
        if state.phase == learning::LearningPhase::Complete {
            break;
        }

        // Phase 4 is non-interactive: render a deterministic tool summary,
        // persist an "all_enabled" registry doc, and auto-advance to Confirmation
        // without consulting the Brain.
        if state.phase == learning::LearningPhase::InitialToolset {
            let summary = learning::tool_summary_message(&config.name);
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: summary,
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;

            if let Err(e) = learning::handle_phase_complete(&mut state, &**db, config).await {
                error!("Phase 4 auto-advance failed: {}", e);
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Error processing phase: {}", e),
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
            }

            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: format!("Phase complete — advancing to: {}", learning::phase_label(&state.phase)),
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;
            continue;
        }

        // Build system prompt for current phase
        let system_prompt = learning::system_prompt_for_phase(&state, config);
        let system_bundle = SystemPromptBundle {
            fingerprint: prompt_fingerprint(&system_prompt),
            text: system_prompt,
            session_name: "learning".to_string(),
        };

        // Send phase kickoff as first message
        let kickoff = learning::phase_kickoff(&state.phase);
        if !kickoff.is_empty() {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: kickoff.clone(),
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;
        }

        // Add kickoff as user message to conversation history and call Brain
        state.conversation_history.push(Message::user(&kickoff));

        // Send thinking indicator
        let _ = tx.send(Ok(ConversationResponse {
            response_type: Some(conversation_response::ResponseType::Thinking(
                ThinkingState { is_thinking: true, name: config.name.clone() }
            )),
        })).await;

        // Call Brain with conversation history (text-only neutral IR).
        let messages: Vec<ApiMessage> = state
            .conversation_history
            .iter()
            .map(legacy_message_to_api)
            .collect();
        let mut brain_rx = provider
            .stream_turn(&messages, &system_bundle, &empty_manifest)
            .await
            .map_err(|e| anyhow::anyhow!("Brain call failed in learning: {}", e))?;

        let full_response = stream_brain_to_grpc(&mut brain_rx, tx, &config.name).await;

        // Check for [PHASE_COMPLETE]
        let phase_complete = full_response.contains("[PHASE_COMPLETE]");
        let clean_response = full_response.replace("[PHASE_COMPLETE]", "").trim().to_string();

        // Add to conversation history (without marker).
        // Opus 4.7 sometimes emits only the marker with no prose; never push
        // an empty assistant message — Anthropic rejects empty text blocks.
        let history_entry = if clean_response.is_empty() {
            "(phase complete)".to_string()
        } else {
            clean_response.clone()
        };
        state.conversation_history.push(Message::assistant(&history_entry));

        if phase_complete {
            // Persist extracted documents and advance phase
            if let Err(e) = learning::handle_phase_complete(&mut state, &**db, config).await {
                error!("Phase complete handling failed: {}", e);
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Error processing phase: {}", e),
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
            }

            if state.phase == learning::LearningPhase::Complete {
                // Save learning conversation history
                let _ = save_learning_history(db, &state.conversation_history).await;

                // Transition to Operational
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::ModeChange(
                        ModeTransition {
                            from_mode: OperatingMode::Learning as i32,
                            to_mode: OperatingMode::Operational as i32,
                            message: format!("Soul sealed — Name: {} — TZ: {}", config.name, config.timezone),
                        }
                    )),
                })).await;
                break;
            } else {
                // Notify phase change
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Phase complete — advancing to: {}", learning::phase_label(&state.phase)),
                            msg_type: SystemMessageType::Info as i32,
                        }
                    )),
                })).await;
            }
        } else {
            // No [PHASE_COMPLETE] — wait for user input and continue conversation
            loop {
                match incoming.next().await {
                    Some(Ok(req)) => {
                        if let Some(conversation_request::RequestType::UserMessage(um)) = req.request_type {
                            state.conversation_history.push(Message::user(&um.content));

                            // Rebuild system prompt (may include newly extracted docs)
                            let system_prompt = learning::system_prompt_for_phase(&state, config);
                            let system_bundle = SystemPromptBundle {
                                fingerprint: prompt_fingerprint(&system_prompt),
                                text: system_prompt,
                                session_name: "learning".to_string(),
                            };

                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Thinking(
                                    ThinkingState { is_thinking: true, name: config.name.clone() }
                                )),
                            })).await;

                            let messages: Vec<ApiMessage> = state
                                .conversation_history
                                .iter()
                                .map(legacy_message_to_api)
                                .collect();
                            let mut brain_rx = provider
                                .stream_turn(&messages, &system_bundle, &empty_manifest)
                                .await
                                .map_err(|e| anyhow::anyhow!("Brain call failed: {}", e))?;

                            let full_response = stream_brain_to_grpc(&mut brain_rx, tx, &config.name).await;

                            let phase_complete = full_response.contains("[PHASE_COMPLETE]");
                            let clean_response = full_response.replace("[PHASE_COMPLETE]", "").trim().to_string();
                            let history_entry = if clean_response.is_empty() {
                                "(phase complete)".to_string()
                            } else {
                                clean_response.clone()
                            };
                            state.conversation_history.push(Message::assistant(&history_entry));

                            if phase_complete {
                                if let Err(e) = learning::handle_phase_complete(&mut state, &**db, config).await {
                                    error!("Phase complete handling failed: {}", e);
                                }

                                if state.phase == learning::LearningPhase::Complete {
                                    let _ = save_learning_history(db, &state.conversation_history).await;
                                    let _ = tx.send(Ok(ConversationResponse {
                                        response_type: Some(conversation_response::ResponseType::ModeChange(
                                            ModeTransition {
                                                from_mode: OperatingMode::Learning as i32,
                                                to_mode: OperatingMode::Operational as i32,
                                                message: format!("Soul sealed — Name: {} — TZ: {}", config.name, config.timezone),
                                            }
                                        )),
                                    })).await;
                                    return Ok(());
                                } else {
                                    let _ = tx.send(Ok(ConversationResponse {
                                        response_type: Some(conversation_response::ResponseType::System(
                                            SystemMessage {
                                                content: format!("Phase complete — advancing to: {}", learning::phase_label(&state.phase)),
                                                msg_type: SystemMessageType::Info as i32,
                                            }
                                        )),
                                    })).await;
                                }
                                break; // Back to outer loop for next phase kickoff
                            }
                            // No phase complete — continue waiting for user input
                        }
                        // Ignore non-UserMessage during learning
                    }
                    Some(Err(e)) => { warn!("Stream error during learning: {}", e); return Err(e.into()); }
                    None => { debug!("Client disconnected during learning"); return Ok(()); }
                }
            }
        }
    }

    Ok(())
}

/// Stream a provider response to gRPC for the learning loop, return
/// the full response text (concatenation of all `TextDelta`s).
async fn stream_brain_to_grpc(
    brain_rx: &mut BoxStream<'static, StreamEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    _name: &str,
) -> String {
    let mut full_response = String::new();
    let mut first_token = true;

    while let Some(event) = brain_rx.next().await {
        match event {
            StreamEvent::TextDelta(text) => {
                if first_token {
                    let _ = tx.send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Thinking(
                            ThinkingState { is_thinking: false, name: String::new() }
                        )),
                    })).await;
                    first_token = false;
                }
                full_response.push_str(&text);
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Token(
                        StreamToken { text }
                    )),
                })).await;
            }
            StreamEvent::Error(err) => {
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::System(
                        SystemMessage {
                            content: format!("Brain error: {}", err),
                            msg_type: SystemMessageType::Error as i32,
                        }
                    )),
                })).await;
            }
            StreamEvent::Complete(turn) => {
                // Use the assembled turn for the Done payload — this is
                // the equivalent of the pre-refactor `Done(full)` event.
                let full = if full_response.is_empty() {
                    turn_text(&turn)
                } else {
                    full_response.clone()
                };
                let clean = full.replace("[PHASE_COMPLETE]", "").trim().to_string();
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Done(
                        StreamDone { full_response: clean }
                    )),
                })).await;
                if full_response.is_empty() {
                    full_response = full;
                }
            }
            StreamEvent::BlockComplete | StreamEvent::ToolArgsDelta { .. } => {}
        }
    }

    full_response
}

/// Save learning conversation history to WardSONDB
async fn save_learning_history(db: &Arc<WardsonDbClient>, history: &[Message]) -> anyhow::Result<()> {
    let collection = "sessions.learning.history";
    if !db.collection_exists(collection).await? {
        db.create_collection(collection).await?;
    }
    let now = chrono::Utc::now().to_rfc3339();
    let doc = serde_json::json!({
        "_id": "learning",
        "turns": history,
        "completed_at": &now,
    });
    match db.write(collection, &doc).await {
        Ok(_) => {}
        Err(_) => { let _ = db.update(collection, "learning", &doc).await; }
    }

    // Create meta entry so learning session appears in /sessions listing
    let meta_collection = "sessions.learning.meta";
    if !db.collection_exists(meta_collection).await? {
        db.create_collection(meta_collection).await?;
    }
    let meta = serde_json::json!({
        "_id": "learning",
        "name": "learning",
        "state": "Closed",
        "created_at": &now,
        "last_active": &now,
    });
    match db.write(meta_collection, &meta).await {
        Ok(_) => Ok(()),
        Err(_) => db.update(meta_collection, "learning", &meta).await.map_err(|e| e.into()),
    }
}

// ── NATIVE-TOOLS-01 helpers ──

/// Drop guard that clears the per-service `in_turn` flag when the
/// future scope ends — including cancellation paths. Without this, a
/// dropped client connection mid-loop would leave `/provider` queued
/// forever.
struct InTurnGuard(Arc<AtomicBool>);

impl Drop for InTurnGuard {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// SHA-256 fingerprint over the system prompt text, truncated to 16
/// hex chars. Used by Gemini's context-cache manager (Stage 6) to
/// detect staleness; harmless for Anthropic (the fingerprint is just
/// computed, never inspected).
fn prompt_fingerprint(text: &str) -> String {
    use sha2::{Digest, Sha256};
    let mut h = Sha256::new();
    h.update(text.as_bytes());
    let digest = h.finalize();
    hex::encode(&digest[..8])
}

/// Convert a legacy on-disk `Message` (role + String content) to the
/// neutral `ApiMessage` IR. Thinking signatures were never persisted,
/// so every historical turn becomes a text-only block. Migration-era
/// shim; future schema bump introduces typed-block persistence.
fn legacy_message_to_api(m: &Message) -> ApiMessage {
    let block = Block::Text(m.content.clone());
    match m.role.as_str() {
        "user" => ApiMessage::User {
            content: vec![block],
        },
        _ => ApiMessage::Assistant {
            content: vec![block],
        },
    }
}

/// Truncate `s` to at most `max` bytes on a UTF-8 char boundary, appending an
/// ellipsis when truncation occurred. Used to bound trace-entry previews.
fn preview_str(s: &str, max: usize) -> String {
    if s.len() <= max {
        return s.to_string();
    }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &s[..end])
}

/// Extract the plain-text portion of an assistant turn for session
/// persistence. Concatenates all `Text` blocks; thinking signatures and
/// tool calls are dropped.
fn turn_text(turn: &AssistantTurn) -> String {
    turn.content
        .iter()
        .filter_map(|b| match b {
            Block::Text(t) => Some(t.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Drive a provider stream, forwarding `TextDelta` events to the gRPC
/// UX channel (`tx`) and returning the final neutral `AssistantTurn`
/// when the stream completes. Synthesizes a gRPC `Done` from the
/// terminal `Complete(turn)` event so the TUI's typing animation
/// behavior matches pre-refactor.
///
/// Returns `Ok(None)` when the stream ended without a `Complete` event
/// (e.g. connection dropped or fatal error mid-stream).
async fn collect_response(
    mut brain_rx: BoxStream<'static, StreamEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    config_name: &str,
) -> anyhow::Result<Option<AssistantTurn>> {
    let mut first_token = true;
    let mut full_turn: Option<AssistantTurn> = None;
    let mut accum_text = String::new();

    while let Some(event) = brain_rx.next().await {
        match event {
            StreamEvent::TextDelta(text) => {
                if first_token {
                    let _ = tx
                        .send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Thinking(
                                ThinkingState {
                                    is_thinking: false,
                                    name: String::new(),
                                },
                            )),
                        }))
                        .await;
                    first_token = false;
                }
                accum_text.push_str(&text);
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Token(
                            StreamToken { text },
                        )),
                    }))
                    .await;
            }
            StreamEvent::Error(err) => {
                error!(
                    target: "dispatch",
                    config = %config_name,
                    "Brain stream error: {err}"
                );
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::System(
                            SystemMessage {
                                content: format!("Brain error: {err}"),
                                msg_type: SystemMessageType::Error as i32,
                            },
                        )),
                    }))
                    .await;
            }
            StreamEvent::BlockComplete => {}
            StreamEvent::ToolArgsDelta { .. } => {}
            StreamEvent::Complete(turn) => {
                // Synthesize a gRPC Done from the assembled turn so the
                // TUI gets a terminal "full text" frame for its
                // streaming animation, matching pre-refactor behavior.
                let full_text = if accum_text.is_empty() {
                    turn_text(&turn)
                } else {
                    accum_text.clone()
                };
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Done(
                            StreamDone {
                                full_response: full_text,
                            },
                        )),
                    }))
                    .await;
                full_turn = Some(turn);
            }
        }
    }

    Ok(full_turn)
}

#[cfg(test)]
mod native_loop_tests {
    use super::*;

    #[test]
    fn legacy_user_message_converts_to_user_text_block() {
        let m = Message::user("hello");
        let api = legacy_message_to_api(&m);
        match api {
            ApiMessage::User { content } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    Block::Text(text) => assert_eq!(text, "hello"),
                    other => panic!("expected Text, got {other:?}"),
                }
            }
            other => panic!("expected User, got {other:?}"),
        }
    }

    #[test]
    fn legacy_assistant_message_converts_to_assistant_text_block() {
        let m = Message::assistant("response");
        let api = legacy_message_to_api(&m);
        assert!(matches!(api, ApiMessage::Assistant { .. }));
    }

    #[test]
    fn turn_text_concatenates_text_blocks_skipping_thinking_and_tool_call() {
        let turn = AssistantTurn {
            content: vec![
                Block::ProviderOpaque(serde_json::json!({
                    "type": "thinking",
                    "thinking": "",
                    "signature": "sig"
                })),
                Block::Text("Hello".into()),
                Block::ToolCall {
                    id: "t1".into(),
                    name: "time".into(),
                    args: serde_json::json!({}),
                    provider_opaque: None,
                },
                Block::Text(", world".into()),
            ],
            outcome: TurnOutcome::EndTurn,
            usage: None,
        };
        assert_eq!(turn_text(&turn), "Hello, world");
    }

    #[test]
    fn display_model_maps_known_providers() {
        assert_eq!(display_model_for("anthropic"), "opus-4.7");
        assert_eq!(display_model_for("gemini"), "gemini-3.1-pro");
        // Unknown defaults to anthropic display (defensive — never
        // shows an empty model name).
        assert_eq!(display_model_for("unknown"), "opus-4.7");
    }

    #[test]
    fn resolve_gemini_model_falls_back_to_default() {
        assert_eq!(
            resolve_gemini_model_id_inner(None, None),
            "gemini-3.1-pro-preview"
        );
    }

    #[test]
    fn resolve_gemini_model_uses_config_field_when_no_env() {
        assert_eq!(
            resolve_gemini_model_id_inner(None, Some("gemini-3.1-pro-preview-customtools")),
            "gemini-3.1-pro-preview-customtools"
        );
    }

    #[test]
    fn resolve_gemini_model_env_overrides_config() {
        assert_eq!(
            resolve_gemini_model_id_inner(Some("env-override-id"), Some("config-id")),
            "env-override-id"
        );
    }

    #[test]
    fn resolve_gemini_model_empty_strings_skip_to_next_layer() {
        // Whitespace-only env value falls through to config; empty
        // config field falls through to the default.
        assert_eq!(
            resolve_gemini_model_id_inner(Some("   "), Some("from-config")),
            "from-config"
        );
        assert_eq!(
            resolve_gemini_model_id_inner(Some(""), Some("")),
            "gemini-3.1-pro-preview"
        );
    }

    #[test]
    fn providers_compatible_handles_empty_and_match() {
        assert!(providers_compatible("anthropic", "anthropic"));
        assert!(providers_compatible("gemini", "gemini"));
        assert!(!providers_compatible("anthropic", "gemini"));
        assert!(!providers_compatible("gemini", "anthropic"));
        // Either side empty (e.g. brand-new session before v9 migration)
        // is permissive — no false-positive blocks.
        assert!(providers_compatible("", "anthropic"));
        assert!(providers_compatible("gemini", ""));
        assert!(providers_compatible("", ""));
    }

    #[test]
    fn turn_text_empty_without_text_blocks() {
        let turn = AssistantTurn {
            content: vec![Block::ProviderOpaque(serde_json::json!({
                "type": "thinking",
                "thinking": "",
                "signature": "sig"
            }))],
            outcome: TurnOutcome::ToolUse,
            usage: None,
        };
        assert_eq!(turn_text(&turn), "");
    }
}
