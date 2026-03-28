//! gRPC service implementation for embra-brain.
//!
//! Bridges the Phase 0 Brain + tools + sessions into a gRPC streaming interface.

use crate::brain;
use crate::tools;
use crate::sessions::SessionManager;
use crate::proactive::ProactiveEngine;
use crate::db::client::WardsonClient;
use crate::config::BrainConfig;

use embra_common::proto::brain::brain_service_server::BrainService;
use embra_common::proto::brain::*;
use embra_common::proto::common;

use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::{mpsc, RwLock};
use tokio_stream::{Stream, StreamExt, wrappers::ReceiverStream};
use tonic::{Request, Response, Status, Streaming};
use tracing::{info, debug, error, warn};

pub struct BrainGrpcService {
    db: Arc<WardsonClient>,
    session_manager: Arc<RwLock<SessionManager>>,
    proactive: Arc<ProactiveEngine>,
    _config: BrainConfig,
    start_time: std::time::Instant,
}

impl BrainGrpcService {
    pub async fn new(db: WardsonClient, config: BrainConfig) -> anyhow::Result<Self> {
        let db = Arc::new(db);
        let session_manager = Arc::new(RwLock::new(
            SessionManager::new(db.clone()).await?
        ));
        let proactive = Arc::new(ProactiveEngine::new(db.clone()));

        // Start proactive engine background tasks
        proactive.start().await;

        Ok(Self {
            db,
            session_manager,
            proactive,
            _config: config,
            start_time: std::time::Instant::now(),
        })
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

        // Create output channel — this replaces the Phase 0 TUI channel
        let (tx, rx) = mpsc::channel::<Result<ConversationResponse, Status>>(100);

        let db = self.db.clone();
        let session_mgr = self.session_manager.clone();
        let proactive = self.proactive.clone();

        // Spawn a task to process incoming messages
        tokio::spawn(async move {
            // Subscribe to proactive notifications
            let mut proactive_rx = proactive.subscribe();

            loop {
                tokio::select! {
                    // Handle incoming client messages
                    msg = incoming.next() => {
                        match msg {
                            Some(Ok(req)) => {
                                if let Err(e) = handle_request(
                                    req, &tx, &db, &session_mgr
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
                    // Forward proactive notifications
                    notification = proactive_rx.recv() => {
                        if let Ok(notif) = notification {
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::System(
                                    SystemMessage {
                                        content: notif,
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
                state: s.state,
                turn_count: s.turn_count,
                created_at: Some(common::Timestamp { iso8601: s.created_at }),
                last_active: Some(common::Timestamp { iso8601: s.last_active }),
                has_summary: s.has_summary,
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
                state: session.state,
                turn_count: 0,
                created_at: Some(common::Timestamp { iso8601: session.created_at }),
                last_active: Some(common::Timestamp { iso8601: session.last_active }),
                has_summary: false,
            }),
        }))
    }

    async fn switch_session(&self, request: Request<SwitchSessionRequest>) -> Result<Response<SwitchSessionResponse>, Status> {
        let name = request.into_inner().name;
        let mut mgr = self.session_manager.write().await;
        let session = mgr.switch(&name).await
            .map_err(|e| Status::internal(format!("{}", e)))?;
        Ok(Response::new(SwitchSessionResponse {
            session: Some(SessionInfo {
                name: session.name, state: session.state, turn_count: session.turn_count,
                created_at: Some(common::Timestamp { iso8601: session.created_at }),
                last_active: Some(common::Timestamp { iso8601: session.last_active }),
                has_summary: session.has_summary,
            }),
            reconnection_briefing: String::new(),
        }))
    }

    async fn close_session(&self, _req: Request<CloseSessionRequest>) -> Result<Response<CloseSessionResponse>, Status> {
        let mut mgr = self.session_manager.write().await;
        let result = mgr.close_current().await
            .map_err(|e| Status::internal(format!("{}", e)))?;
        Ok(Response::new(CloseSessionResponse {
            closed_session: result.closed,
            switched_to: result.switched_to,
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
            wardsondb_status: "healthy".to_string(),
            services: std::collections::HashMap::new(),
        }))
    }

    async fn get_soul_document(&self, _req: Request<GetSoulDocumentRequest>) -> Result<Response<GetSoulDocumentResponse>, Status> {
        Err(Status::unimplemented("GetSoulDocument not yet implemented"))
    }

    async fn get_identity(&self, _req: Request<GetIdentityRequest>) -> Result<Response<GetIdentityResponse>, Status> {
        Err(Status::unimplemented("GetIdentity not yet implemented"))
    }

    async fn get_mode(&self, _req: Request<GetModeRequest>) -> Result<Response<GetModeResponse>, Status> {
        Ok(Response::new(GetModeResponse {
            mode: OperatingMode::Operational as i32,
            soul_status: "sealed".to_string(),
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
    db: &Arc<WardsonClient>,
    session_mgr: &Arc<RwLock<SessionManager>>,
) -> anyhow::Result<()> {
    match req.request_type {
        Some(conversation_request::RequestType::UserMessage(msg)) => {
            // Send thinking indicator
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::Thinking(
                    ThinkingState { is_thinking: true, name: "Embra".to_string() }
                )),
            })).await;

            // Create a channel for the Brain's SSE tokens
            let (brain_tx, mut brain_rx) = mpsc::channel(100);

            // Spawn Brain call
            let brain_handle = {
                let _db = db.clone();
                let _session_mgr = session_mgr.clone();
                let _input = msg.content.clone();
                tokio::spawn(async move {
                    // TODO: Wire up Phase 0 brain module here
                    brain_tx.send(brain::StreamEvent::Done(
                        "embra-brain Phase 1 stub ��� Brain not yet wired to Anthropic API".to_string()
                    )).await.ok();
                })
            };

            // Stream Brain tokens to gRPC
            while let Some(event) = brain_rx.recv().await {
                match event {
                    brain::StreamEvent::Token(text) => {
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Thinking(
                                ThinkingState { is_thinking: false, name: String::new() }
                            )),
                        })).await;

                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Token(
                                StreamToken { text }
                            )),
                        })).await;
                    }
                    brain::StreamEvent::Done(full_response) => {
                        let _ = tx.send(Ok(ConversationResponse {
                            response_type: Some(conversation_response::ResponseType::Done(
                                StreamDone { full_response: full_response.clone() }
                            )),
                        })).await;

                        // Check for tool tags in the response
                        let tool_tags = tools::extract_tool_tags(&full_response);
                        for tag in tool_tags {
                            let result = tools::dispatch(&tag, db, session_mgr).await;
                            let _ = tx.send(Ok(ConversationResponse {
                                response_type: Some(conversation_response::ResponseType::Tool(
                                    ToolExecution {
                                        tool_name: tag.name.clone(),
                                        input: tag.input.clone(),
                                        result: result.output.clone(),
                                        success: result.success,
                                    }
                                )),
                            })).await;
                        }
                    }
                    brain::StreamEvent::Error(err) => {
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

            brain_handle.await?;
            Ok(())
        }

        Some(conversation_request::RequestType::SlashCommand(cmd)) => {
            let output = handle_slash_command(&cmd.command, &cmd.args).await;
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

        Some(conversation_request::RequestType::SessionAttach(_attach)) => {
            let _ = tx.send(Ok(ConversationResponse {
                response_type: Some(conversation_response::ResponseType::System(
                    SystemMessage {
                        content: "Session attached".to_string(),
                        msg_type: SystemMessageType::Reconnection as i32,
                    }
                )),
            })).await;
            Ok(())
        }

        None => Ok(()),
    }
}

async fn handle_slash_command(command: &str, _args: &str) -> String {
    match command {
        "/help" => "Available commands: /sessions, /switch <n>, /new <n>, /close, /status, /soul, /identity, /mode, /help".to_string(),
        "/status" => "TODO: system_status".to_string(),
        _ => format!("Unknown command: {}. Type /help for available commands.", command),
    }
}
