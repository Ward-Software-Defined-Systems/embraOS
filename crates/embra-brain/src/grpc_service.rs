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
            // Try to get proactive notifications (only one Converse stream gets them)
            let mut proactive = proactive_rx.try_lock().ok();

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
                            // No lock, just pend forever
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
            eprintln!("[embra-brain] Calling Anthropic API with {} messages", messages.len());
            let mut brain_rx = match brain.send_message_streaming(&messages).await {
                Ok(rx) => {
                    eprintln!("[embra-brain] Brain streaming started");
                    rx
                }
                Err(e) => {
                    eprintln!("[embra-brain] Brain call FAILED: {}", e);
                    return Err(anyhow::anyhow!("Brain call failed: {}", e));
                }
            };

            // Stream Brain tokens to gRPC
            let mut full_response = String::new();
            let mut first_token = true;
            eprintln!("[embra-brain] Waiting for Brain stream events...");
            while let Some(event) = brain_rx.recv().await {
                match event {
                    StreamEvent::Token(text) => {
                        if first_token {
                            eprintln!("[embra-brain] First token received");
                        }
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
                        eprintln!("[embra-brain] StreamEvent::Error: {}", err);
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

            // Tool feedback loop — extract tags, dispatch, feed results back
            let mut tool_results = String::new();
            let tags = tools::extract_tool_tags(&full_response);
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

            // Save to session history
            {
                let mgr = session_mgr.read().await;
                let _ = mgr.append_message(&session_name, &Message::user(&msg.content)).await;
                let _ = mgr.append_message(&session_name, &Message::assistant(&full_response)).await;
            }

            // If tools produced results, feed back to Brain for continuation
            if !tool_results.is_empty() {
                let feedback = format!("[SYSTEM] Tool results:\n{}", tool_results);
                messages.push(Message::assistant(&full_response));
                messages.push(Message::user(&feedback));

                let mut brain_rx2 = brain.send_message_streaming(&messages).await
                    .map_err(|e| anyhow::anyhow!("Brain continuation failed: {}", e))?;

                let mut continuation = String::new();
                while let Some(event) = brain_rx2.recv().await {
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
                        }
                    }
                }

                // Save continuation
                {
                    let mgr = session_mgr.read().await;
                    let _ = mgr.append_message(&session_name, &Message::assistant(&continuation)).await;
                }
            }

            Ok(())
        }

        Some(conversation_request::RequestType::SlashCommand(cmd)) => {
            let output = handle_slash_command(&cmd.command, &cmd.args, db, session_mgr, config_tz).await;
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: output,
                        msg_type: SystemMessageType::Info as i32,
                    }
                )),
            })).await;
            Ok(())
        }

        Some(conversation_request::RequestType::SessionAttach(attach)) => {
            let session_name = if attach.session_name.is_empty() {
                "default".to_string()
            } else {
                attach.session_name.clone()
            };

            // Ensure session exists
            let mut mgr = session_mgr.write().await;
            if !mgr.session_exists(&session_name).await.unwrap_or(false) {
                let _ = mgr.create(&session_name).await;
            }
            mgr.active_session = Some(session_name.clone());

            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: format!("Session '{}' attached", session_name),
                        msg_type: SystemMessageType::Reconnection as i32,
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
    db: &Arc<WardsonDbClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
    config_tz: &str,
) -> String {
    match command {
        "/help" => "Available commands: /sessions, /switch <name>, /new <name>, /close, /status, /soul, /identity, /mode, /help".to_string(),
        "/status" => {
            let status = tools::system_status(db).await;
            serde_json::to_string_pretty(&status).unwrap_or_else(|_| "Failed to get status".to_string())
        }
        "/sessions" => {
            let mgr = session_mgr.read().await;
            match mgr.list().await {
                Ok(sessions) => {
                    if sessions.is_empty() {
                        "No sessions.".to_string()
                    } else {
                        sessions.iter().map(|s| {
                            format!("  {} [{:?}] last active: {}", s.name, s.state, s.last_active.format("%Y-%m-%d %H:%M"))
                        }).collect::<Vec<_>>().join("\n")
                    }
                }
                Err(e) => format!("Error listing sessions: {}", e),
            }
        }
        "/new" => {
            if args.is_empty() {
                "Usage: /new <session-name>".to_string()
            } else {
                let mut mgr = session_mgr.write().await;
                match mgr.create(args).await {
                    Ok(s) => {
                        mgr.active_session = Some(s.name.clone());
                        format!("Created and switched to session '{}'", s.name)
                    }
                    Err(e) => format!("Error creating session: {}", e),
                }
            }
        }
        "/switch" => {
            if args.is_empty() {
                "Usage: /switch <session-name>".to_string()
            } else {
                let mut mgr = session_mgr.write().await;
                if mgr.session_exists(args).await.unwrap_or(false) {
                    let _ = mgr.reattach(args).await;
                    mgr.active_session = Some(args.to_string());
                    format!("Switched to session '{}'", args)
                } else {
                    format!("Session '{}' does not exist", args)
                }
            }
        }
        "/close" => {
            let mut mgr = session_mgr.write().await;
            if let Some(ref name) = mgr.active_session.clone() {
                let _ = mgr.close(name).await;
                mgr.active_session = None;
                format!("Closed session '{}'", name)
            } else {
                "No active session".to_string()
            }
        }
        "/soul" => {
            match learning::load_soul(&**db).await {
                Ok(Some(soul)) => serde_json::to_string_pretty(&soul).unwrap_or_default(),
                Ok(None) => "No soul sealed yet.".to_string(),
                Err(e) => format!("Error loading soul: {}", e),
            }
        }
        "/identity" => {
            match db.read("memory.identity", "identity").await {
                Ok(doc) => serde_json::to_string_pretty(&doc).unwrap_or_default(),
                Err(_) => "No identity document found.".to_string(),
            }
        }
        "/mode" => {
            let sealed = learning::is_soul_sealed(&**db).await.unwrap_or(false);
            if sealed { "Operational (soul sealed)".to_string() } else { "Learning (soul not sealed)".to_string() }
        }
        _ => format!("Unknown command: {}. Type /help for available commands.", command),
    }
}
