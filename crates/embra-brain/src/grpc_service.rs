//! gRPC service implementation for embra-brain.
//!
//! Bridges Phase 0 Brain + tools + sessions into a gRPC streaming interface.

use crate::brain::{Brain, Message, StreamEvent};
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
use tracing::{info, debug, error, warn};

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
            let config_name = config::load_config(db).await
                .map(|c| c.name)
                .unwrap_or_else(|_| "Embra".to_string());

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

            // Add current message to history for the Brain call
            let mut messages = history.clone();
            messages.push(Message::user(&msg.content));

            // Send thinking indicator
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Thinking(
                    ThinkingState { is_thinking: true, name: config_name.clone() }
                )),
            })).await;

            // Call Brain streaming
            debug!("Calling Anthropic API with {} messages", messages.len());
            let mut brain_rx = brain.send_message_streaming(&messages).await
                .map_err(|e| anyhow::anyhow!("Brain call failed: {}", e))?;

            // Stream Brain tokens to gRPC
            let mut full_response = String::new();
            let mut first_token = true;
            while let Some(event) = brain_rx.recv().await {
                match event {
                    StreamEvent::Token(text) => {
                        if first_token {
                            // Clear thinking on first token
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
                        full_response = full;
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Done(
                                StreamDone { full_response: full_response.clone() }
                            )),
                        })).await;
                    }
                    StreamEvent::Error(err) => {
                        error!("Brain stream error: {}", err);
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::System(
                                SystemMessage {
                                    content: format!("Brain error: {}", err),
                                    msg_type: SystemMessageType::Error as i32,
                                }
                            )),
                        })).await;
                    }
                }
            }

            // Bounded tool feedback loop — extract tags, dispatch, feed results back,
            // then re-call Brain if tools produced output. Repeat until no more tools
            // or MAX_TOOL_ITERATIONS is reached.
            const MAX_TOOL_ITERATIONS: usize = 10;

            let mut current_response = full_response;
            for iteration in 0..MAX_TOOL_ITERATIONS {
                let tags = tools::extract_tool_tags(&current_response);
                if tags.is_empty() {
                    break;
                }

                // Dispatch tools and stream ToolExecution events
                let mut tool_results = String::new();
                for tag in &tags {
                    if let Some(result) = tools::dispatch(tag, db, config_tz, &session_name).await {
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Tool(
                                ToolExecution {
                                    tool_name: tag.clone(),
                                    input: String::new(),
                                    result: result.clone(),
                                    success: true,
                                }
                            )),
                        })).await;
                        tool_results.push_str(&result);
                        tool_results.push('\n');
                    }
                }

                if tool_results.is_empty() {
                    break;
                }

                // Feed tool results back to Brain for continuation
                let feedback = format!("[SYSTEM] Tool results:\n{}", tool_results);
                messages.push(Message::assistant(&current_response));
                messages.push(Message::user(&feedback));

                let mut brain_rx_cont = brain.send_message_streaming(&messages).await
                    .map_err(|e| anyhow::anyhow!("Brain continuation failed (iteration {}): {}", iteration + 1, e))?;

                let mut continuation = String::new();
                while let Some(event) = brain_rx_cont.recv().await {
                    match event {
                        StreamEvent::Token(text) => {
                            continuation.push_str(&text);
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Token(
                                    StreamToken { text }
                                )),
                            })).await;
                        }
                        StreamEvent::Done(full) => {
                            continuation = full;
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Done(
                                    StreamDone { full_response: continuation.clone() }
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
                            break;
                        }
                    }
                }

                current_response = continuation;
            }

            // Save complete conversation to session history
            {
                let mgr = session_mgr.read().await;
                // Save the original user message
                let _ = mgr.append_message(&session_name, &Message::user(&msg.content)).await;
                // Save all assistant + tool feedback turns from the messages vec
                // (messages started as history + user msg; assistant/feedback pairs were appended in the loop)
                let original_len = history.len() + 1; // history + user message
                for m in &messages[original_len..] {
                    let _ = mgr.append_message(&session_name, m).await;
                }
                // Save the final response
                let _ = mgr.append_message(&session_name, &Message::assistant(&current_response)).await;
            }

            Ok(())
        }

        Some(conversation_request::RequestType::SlashCommand(cmd)) => {
            handle_slash_command(&cmd.command, &cmd.args, tx, db, session_mgr, config_tz).await;
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

            // Send ModeTransition so console knows the correct mode and timezone
            let is_sealed = learning::is_soul_sealed(&**db).await.unwrap_or(false);
            let mode = if is_sealed { OperatingMode::Operational } else { OperatingMode::Learning };
            let tz = config::load_config(&**db).await.map(|c| c.timezone).unwrap_or_else(|_| config_tz.to_string());
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::ModeChange(
                    ModeTransition {
                        from_mode: OperatingMode::Unspecified as i32,
                        to_mode: mode as i32,
                        message: if is_sealed {
                            format!("Operational — Session: {} — TZ: {}", session_name, tz)
                        } else {
                            format!("Learning Mode — TZ: {}", tz)
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
) {
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
    let send_session_update = move |tx: &mpsc::Sender<Result<ConversationResponse, Status>>, session_name: &str| {
        let tx = tx.clone();
        let name = session_name.to_string();
        let tz = tz.clone();
        async move {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::ModeChange(
                    ModeTransition {
                        from_mode: OperatingMode::Operational as i32,
                        to_mode: OperatingMode::Operational as i32,
                        message: format!("Operational — Session: {} — TZ: {}", name, tz),
                    }
                )),
            })).await;
        }
    };

    match command {
        "/help" => {
            send_msg(tx, "Available commands:\n  /sessions, /switch <name>, /new <name>, /close\n  /status, /soul, /identity, /mode\n  /github-token <token>    Set GitHub token\n  /ssh-keygen              Generate SSH key pair\n  /ssh-copy-id <user@host> Copy SSH key to host\n  /git-setup <name> | <email>  Set git user config\n  /help".to_string()).await;
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
                            return;
                        }
                    }
                    Err(_) => {
                        send_msg(tx, "No system config found. Run config wizard first.".to_string()).await;
                        return;
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
            let key_path = "/root/.ssh/id_ed25519";
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
                return;
            }

            // Ensure .ssh directory exists with correct permissions
            let ssh_dir = "/root/.ssh";
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
                return;
            }

            let pub_path = "/root/.ssh/id_ed25519.pub";
            if !std::path::Path::new(pub_path).exists() {
                send_msg(tx, "No SSH key found. Run /ssh-keygen first.".to_string()).await;
                return;
            }

            let target = args.trim();
            let host = target.rsplit('@').next().unwrap_or(target);
            if !tools::security::is_private_address(host) {
                send_msg(tx, format!(
                    "Denied: '{}' is not a private address. SSH is restricted to RFC 1918 ranges.",
                    host
                )).await;
                return;
            }

            match tokio::time::timeout(
                std::time::Duration::from_secs(30),
                tokio::process::Command::new("ssh-copy-id")
                    .args(["-i", pub_path, "-o", "StrictHostKeyChecking=accept-new", target])
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
                    return;
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
                message: format!("Learning Mode — Phase: {}", learning::phase_label(&state.phase)),
            }
        )),
    })).await;

    loop {
        if state.phase == learning::LearningPhase::Complete {
            break;
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

        // Add to conversation history (without marker)
        state.conversation_history.push(Message::assistant(&clean_response));

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
                            message: "Soul sealed! Entering Operational mode.".to_string(),
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
                            state.conversation_history.push(Message::assistant(&clean_response));

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
                                                message: "Soul sealed! Entering Operational mode.".to_string(),
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
