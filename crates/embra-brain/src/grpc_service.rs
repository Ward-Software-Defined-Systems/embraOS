//! gRPC service implementation for embra-brain.
//!
//! Bridges Phase 0 Brain + tools + sessions into a gRPC streaming interface.

use crate::brain::{ApiMessage, AssistantResponse, Brain, Message, MessageBlock, StopReason, StreamEvent};
use crate::config;
use crate::db::WardsonDbClient;
use crate::learning;
use crate::proactive::Notification;
use crate::sessions::SessionManager;
use crate::tools;

use embra_common::proto::brain::brain_service_server::BrainService;
use embra_common::proto::brain::*;
use embra_common::proto::common;

use std::pin::Pin;
use std::sync::Arc;
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
                                    req, &tx, &db, &session_mgr, &config_tz, &api_key
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
            version: "0.2.0-phase1".to_string(),
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
) -> anyhow::Result<()> {
    match req.request_type {
        Some(conversation_request::RequestType::UserMessage(msg)) => {
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

            // Create Brain and send message
            let brain = Brain::new(api_key.to_string(), system_prompt);

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

            // Build typed-message conversation for the native tool-use loop.
            // Legacy history (role+String content) maps to text-only ApiMessage
            // blocks; thinking signatures were never persisted, so cross-turn
            // preservation is out of scope. Within a single turn's loop the
            // assistant response (thinking blocks included) is pushed back
            // verbatim between iterations — the API requires this.
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

            // Native tool-use loop driven by stop_reason.
            const MAX_TOOL_ITERATIONS: usize = 10;
            let mut tool_iter: usize = 0;
            let mut last_response_text = String::new();

            let first_rx = brain
                .send_message_streaming_with_tools(&api_messages)
                .await
                .map_err(|e| anyhow::anyhow!("Brain call failed: {}", e))?;
            let Some(mut current_response) =
                collect_response(first_rx, &tx, &config_name).await?
            else {
                // Stream closed without Complete — treat as error and save nothing.
                return Ok(());
            };
            // Track text from the most recent response for persistence fallback.
            last_response_text = response_text(&current_response);
            api_messages.push(ApiMessage::assistant_blocks(current_response.content.clone()));

            loop {
                match current_response.stop_reason {
                    StopReason::EndTurn
                    | StopReason::MaxTokens
                    | StopReason::StopSequence
                    | StopReason::Refusal => break,

                    StopReason::PauseTurn => {
                        warn!(
                            target: "dispatch",
                            session = %session_name,
                            "stop_reason=pause_turn; resuming conversation"
                        );
                        let rx = brain
                            .send_message_streaming_with_tools(&api_messages)
                            .await
                            .map_err(|e| anyhow::anyhow!("Brain pause-resume failed: {}", e))?;
                        let Some(resp) = collect_response(rx, &tx, &config_name).await? else {
                            break;
                        };
                        current_response = resp;
                        last_response_text = response_text(&current_response);
                        api_messages.push(ApiMessage::assistant_blocks(
                            current_response.content.clone(),
                        ));
                        continue;
                    }

                    StopReason::ToolUse => {
                        if tool_iter >= MAX_TOOL_ITERATIONS {
                            warn!(
                                target: "dispatch",
                                session = %session_name,
                                "tool iteration cap hit ({MAX_TOOL_ITERATIONS})"
                            );
                            break;
                        }
                        tool_iter += 1;

                        let mut result_blocks: Vec<MessageBlock> = Vec::new();
                        for block in &current_response.content {
                            let MessageBlock::ToolUse { id, name, input } = block else {
                                continue;
                            };
                            let started = std::time::Instant::now();
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
                            };
                            let outcome = tools::registry::dispatch(
                                name,
                                input.clone(),
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
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Tool(
                                    ToolExecution {
                                        tool_use_id: id.clone(),
                                        tool_name: name.clone(),
                                        input_json: serde_json::to_string(input).unwrap_or_default(),
                                        result: content.clone(),
                                        is_error,
                                    }
                                )),
                            })).await;
                            result_blocks.push(MessageBlock::ToolResult {
                                tool_use_id: id.clone(),
                                content,
                                is_error,
                            });
                        }

                        if result_blocks.is_empty() {
                            // stop_reason claimed tool_use but no ToolUse blocks
                            // were present — treat as a terminal state and exit.
                            break;
                        }

                        api_messages.push(ApiMessage::user_tool_results(result_blocks));
                        let rx = brain
                            .send_message_streaming_with_tools(&api_messages)
                            .await
                            .map_err(|e| {
                                anyhow::anyhow!(
                                    "Brain continuation failed (iter {tool_iter}): {e}"
                                )
                            })?;
                        let Some(resp) = collect_response(rx, &tx, &config_name).await? else {
                            break;
                        };
                        current_response = resp;
                        last_response_text = response_text(&current_response);
                        api_messages.push(ApiMessage::assistant_blocks(
                            current_response.content.clone(),
                        ));
                    }
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
                let _ = mgr
                    .append_message(&session_name, &Message::user(&msg.content))
                    .await;
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
                &cmd.command, &cmd.args, tx, db, session_mgr, config_tz, api_key
            ).await {
                // Slash command requested a synthetic user turn — feed it through the Brain.
                let synthetic = ConversationRequest {
                    request_type: Some(conversation_request::RequestType::UserMessage(
                        UserMessage { content: synthetic_prompt, timestamp: None }
                    )),
                };
                Box::pin(handle_request(
                    synthetic, tx, db, session_mgr, config_tz, api_key
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
            if !mgr.session_exists(&session_name).await.unwrap_or(false) {
                let _ = mgr.create(&session_name).await;
            }
            mgr.active_session = Some(session_name.clone());
            drop(mgr);

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
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::ModeChange(
                    ModeTransition {
                        from_mode: OperatingMode::Unspecified as i32,
                        to_mode: mode as i32,
                        message: if is_sealed {
                            format!("Operational — Name: {} — Session: {} — TZ: {}", name, session_name, tz)
                        } else {
                            format!("Learning Mode — Name: {} — TZ: {}", name, tz)
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

    // Helper to send a ModeTransition with updated session name
    let tz = config_tz.to_string();
    let config_name = config::load_config(&**db).await
        .map(|c| c.name).unwrap_or_else(|_| "Embra".to_string());
    let send_session_update = {
        let config_name = config_name.clone();
        move |tx: &mpsc::Sender<Result<ConversationResponse, Status>>, session_name: &str| {
            let tx = tx.clone();
            let session = session_name.to_string();
            let tz = tz.clone();
            let config_name = config_name.clone();
            async move {
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::ModeChange(
                        ModeTransition {
                            from_mode: OperatingMode::Operational as i32,
                            to_mode: OperatingMode::Operational as i32,
                            message: format!("Operational — Name: {} — Session: {} — TZ: {}", config_name, session, tz),
                        }
                    )),
                })).await;
            }
        }
    };

    match command {
        "/help" => {
            send_msg(tx, "Available commands:\n  /sessions, /switch <name>, /new <name>, /close\n  /status, /soul, /identity, /mode\n  /github-token <token>    Set GitHub token\n  /ssh-keygen              Generate SSH key pair\n  /ssh-copy-id <user@host> Copy SSH key to host\n  /git-setup <name> | <email>  Set git user config\n  /feedback-loop           (EXPERIMENTAL) trigger Phase 3 feedback-loop protocol\n  /help".to_string()).await;
        }
        "/feedback-loop" => {
            send_msg(tx, "\u{26A0} EXPERIMENTAL: Phase 3 Continuity Engine preview (manual trigger)\nInitiating feedback loop per feedback-loop-spec-v2.md.\nThe Brain will now begin Step 1.1 (Gather \u{2192} Introspect).\nThis is a multi-turn protocol \u{2014} expect 5+ tool invocations.".to_string()).await;
            return Some(build_feedback_loop_prompt());
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
        let brain = Brain::new(api_key.to_string(), system_prompt);

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

        // Call Brain with conversation history
        let mut brain_rx = brain.send_message_streaming(&state.conversation_history).await
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
                            let brain = Brain::new(api_key.to_string(), system_prompt);

                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Thinking(
                                    ThinkingState { is_thinking: true, name: config.name.clone() }
                                )),
                            })).await;

                            let mut brain_rx = brain.send_message_streaming(&state.conversation_history).await
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

/// Stream Brain response tokens to gRPC, return the full response text.
async fn stream_brain_to_grpc(
    brain_rx: &mut mpsc::Receiver<StreamEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    name: &str,
) -> String {
    let mut full_response = String::new();
    let mut first_token = true;

    while let Some(event) = brain_rx.recv().await {
        match event {
            StreamEvent::Token(text) => {
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
            StreamEvent::Done(full) => {
                full_response = full.clone();
                // Strip [PHASE_COMPLETE] from the Done message sent to client
                let clean = full.replace("[PHASE_COMPLETE]", "").trim().to_string();
                let _ = tx.send(Ok(ConversationResponse {
                    response_type: Some(conversation_response::ResponseType::Done(
                        StreamDone { full_response: clean }
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
            // Native tool-use events — consumed by Stage 5's loop driver.
            StreamEvent::BlockComplete { .. } | StreamEvent::Complete { .. } => {}
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

/// Convert a legacy on-disk `Message` (role + String content) to a typed
/// `ApiMessage`. Used to build the conversation history for the native
/// tool-use loop. Thinking signatures were never persisted, so every
/// historical turn becomes a text-only block. This is the migration-era
/// shim; Stage 8 introduces typed-block persistence and deprecates this
/// conversion path for post-v7 sessions.
fn legacy_message_to_api(m: &Message) -> ApiMessage {
    let block = MessageBlock::Text {
        text: m.content.clone(),
    };
    match m.role.as_str() {
        "user" => ApiMessage::User {
            content: vec![block],
        },
        _ => ApiMessage::Assistant {
            content: vec![block],
        },
    }
}

/// Extract the plain-text portion of an assistant response for session
/// persistence. Concatenates all `Text` blocks; thinking signatures and
/// tool_use blocks are dropped.
fn response_text(response: &AssistantResponse) -> String {
    response
        .content
        .iter()
        .filter_map(|b| match b {
            MessageBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

/// Drive a Brain SSE stream, forwarding `Token` events to the gRPC UX
/// channel (`tx`) and returning the final typed `AssistantResponse` when
/// the stream completes. Also clears the thinking indicator on first token
/// and forwards Done frames to the console as before.
///
/// Returns `Ok(None)` when the stream ended without a `Complete` event
/// (e.g. connection dropped or fatal error mid-stream).
async fn collect_response(
    mut brain_rx: mpsc::Receiver<StreamEvent>,
    tx: &mpsc::Sender<Result<ConversationResponse, Status>>,
    config_name: &str,
) -> anyhow::Result<Option<AssistantResponse>> {
    let mut first_token = true;
    let mut full_response: Option<AssistantResponse> = None;

    while let Some(event) = brain_rx.recv().await {
        match event {
            StreamEvent::Token(text) => {
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
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Token(
                            StreamToken { text },
                        )),
                    }))
                    .await;
            }
            StreamEvent::Done(full) => {
                let _ = tx
                    .send(Ok(ConversationResponse {
                        response_type: Some(conversation_response::ResponseType::Done(
                            StreamDone { full_response: full },
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
            StreamEvent::BlockComplete { .. } => {}
            StreamEvent::Complete { response } => {
                full_response = Some(response);
            }
        }
    }

    Ok(full_response)
}

#[cfg(test)]
mod native_loop_tests {
    use super::*;
    use crate::brain::{AssistantResponse, MessageBlock, StopReason};

    #[test]
    fn legacy_user_message_converts_to_user_text_block() {
        let m = Message::user("hello");
        let api = legacy_message_to_api(&m);
        match api {
            ApiMessage::User { content } => {
                assert_eq!(content.len(), 1);
                match &content[0] {
                    MessageBlock::Text { text } => assert_eq!(text, "hello"),
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
    fn response_text_concatenates_text_blocks_skipping_thinking_and_tool_use() {
        let resp = AssistantResponse {
            id: None,
            content: vec![
                MessageBlock::Thinking {
                    thinking: String::new(),
                    signature: "sig".into(),
                },
                MessageBlock::Text {
                    text: "Hello".into(),
                },
                MessageBlock::ToolUse {
                    id: "t1".into(),
                    name: "time".into(),
                    input: serde_json::json!({}),
                },
                MessageBlock::Text {
                    text: ", world".into(),
                },
            ],
            stop_reason: StopReason::EndTurn,
            stop_sequence: None,
        };
        assert_eq!(response_text(&resp), "Hello, world");
    }

    #[test]
    fn response_text_empty_without_text_blocks() {
        let resp = AssistantResponse {
            id: None,
            content: vec![MessageBlock::Thinking {
                thinking: String::new(),
                signature: "sig".into(),
            }],
            stop_reason: StopReason::ToolUse,
            stop_sequence: None,
        };
        assert_eq!(response_text(&resp), "");
    }
}
