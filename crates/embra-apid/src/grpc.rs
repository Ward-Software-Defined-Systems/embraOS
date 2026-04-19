//! gRPC proxy implementation for embra-apid.
//!
//! Proxies all RPCs to the appropriate backend service.
//! The Converse RPC is bidirectional streaming — forwarded to embra-brain.

use crate::proxy::BackendConnections;
use embra_common::proto::apid::embra_api_server::EmbraApi;
use embra_common::proto::apid::*;
use embra_common::proto::common;

use prost::Message;
use std::pin::Pin;
use tokio_stream::{Stream, StreamExt};
use tonic::{Request, Response, Status, Streaming};
use tracing::debug;

pub struct EmbraApiImpl {
    backends: BackendConnections,
    start_time: std::time::Instant,
}

impl EmbraApiImpl {
    pub fn new(backends: BackendConnections) -> Self {
        Self {
            backends,
            start_time: std::time::Instant::now(),
        }
    }
}

#[tonic::async_trait]
impl EmbraApi for EmbraApiImpl {
    type ConverseStream = Pin<Box<dyn Stream<Item = Result<ConversationResponse, Status>> + Send>>;

    async fn converse(
        &self,
        request: Request<Streaming<ConversationRequest>>,
    ) -> Result<Response<Self::ConverseStream>, Status> {
        debug!("Converse stream opened");

        let mut brain = self.backends.brain_client().await?;
        let incoming = request.into_inner();

        // Map apid ConversationRequest → brain ConversationRequest
        // The brain client's converse() expects Stream<Item = brain::ConversationRequest>
        let brain_stream = incoming.filter_map(|msg| {
            match msg {
                Ok(req) => {
                    let brain_req = embra_common::proto::brain::ConversationRequest {
                        request_type: req.request_type.map(|rt| match rt {
                            conversation_request::RequestType::UserMessage(um) => {
                                embra_common::proto::brain::conversation_request::RequestType::UserMessage(
                                    embra_common::proto::brain::UserMessage {
                                        content: um.content,
                                        timestamp: None,
                                    }
                                )
                            }
                            conversation_request::RequestType::SlashCommand(sc) => {
                                embra_common::proto::brain::conversation_request::RequestType::SlashCommand(
                                    embra_common::proto::brain::SlashCommand {
                                        command: sc.command,
                                        args: sc.args,
                                    }
                                )
                            }
                            conversation_request::RequestType::SessionAttach(sa) => {
                                embra_common::proto::brain::conversation_request::RequestType::SessionAttach(
                                    embra_common::proto::brain::SessionAttach {
                                        session_name: sa.session_name,
                                    }
                                )
                            }
                        }),
                    };
                    Some(brain_req)
                }
                Err(_) => None,
            }
        });

        // Forward to brain and stream back responses
        let response = brain.converse(brain_stream).await?;
        let brain_response_stream = response.into_inner();

        // Map brain ConversationResponse → apid ConversationResponse
        let output_stream = brain_response_stream.map(|msg| {
            match msg {
                Ok(brain_resp) => {
                    // Serialize brain response as pass-through payload
                    let payload = brain_resp.encode_to_vec();
                    Ok(ConversationResponse { payload })
                }
                Err(e) => Err(e),
            }
        });

        Ok(Response::new(Box::pin(output_stream)))
    }

    // --- Session proxies ---

    async fn list_sessions(&self, _request: Request<ListSessionsRequest>) -> Result<Response<ListSessionsResponse>, Status> {
        let mut brain = self.backends.brain_client().await?;
        let resp = brain.list_sessions(embra_common::proto::brain::ListSessionsRequest {}).await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(ListSessionsResponse { payload }))
    }

    async fn create_session(&self, request: Request<CreateSessionRequest>) -> Result<Response<CreateSessionResponse>, Status> {
        let req = request.into_inner();
        let mut brain = self.backends.brain_client().await?;
        let resp = brain.create_session(embra_common::proto::brain::CreateSessionRequest { name: req.name }).await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(CreateSessionResponse { payload }))
    }

    async fn switch_session(&self, request: Request<SwitchSessionRequest>) -> Result<Response<SwitchSessionResponse>, Status> {
        let req = request.into_inner();
        let mut brain = self.backends.brain_client().await?;
        let resp = brain.switch_session(embra_common::proto::brain::SwitchSessionRequest { name: req.name }).await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(SwitchSessionResponse { payload }))
    }

    async fn close_session(&self, _request: Request<CloseSessionRequest>) -> Result<Response<CloseSessionResponse>, Status> {
        let mut brain = self.backends.brain_client().await?;
        let resp = brain.close_session(embra_common::proto::brain::CloseSessionRequest {}).await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(CloseSessionResponse { payload }))
    }

    async fn get_expression(&self, _request: Request<GetExpressionRequest>) -> Result<Response<ExpressionState>, Status> {
        let mut brain = self.backends.brain_client().await?;
        let resp = brain
            .get_expression(embra_common::proto::brain::GetExpressionRequest {})
            .await?;
        let inner = resp.into_inner();
        Ok(Response::new(ExpressionState {
            content: inner.content,
            version: inner.version,
            updated_at: inner.updated_at,
        }))
    }

    // --- Trust proxies ---

    async fn verify_soul(&self, request: Request<VerifySoulRequest>) -> Result<Response<VerifySoulResponse>, Status> {
        let req = request.into_inner();
        let mut trust = self.backends.trust_client().await?;
        let resp = trust.verify_soul(embra_common::proto::trust::VerifySoulRequest {
            expected_hash: req.expected_hash,
        }).await?;
        let inner = resp.into_inner();
        Ok(Response::new(VerifySoulResponse {
            valid: inner.valid,
            error: inner.error,
        }))
    }

    async fn get_soul_status(&self, _request: Request<GetSoulStatusRequest>) -> Result<Response<GetSoulStatusResponse>, Status> {
        let mut trust = self.backends.trust_client().await?;
        let resp = trust.get_soul_status(embra_common::proto::trust::GetSoulStatusRequest {}).await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(GetSoulStatusResponse { payload }))
    }

    // --- System management ---

    async fn system_health(&self, _request: Request<SystemHealthRequest>) -> Result<Response<SystemHealthResponse>, Status> {
        Ok(Response::new(SystemHealthResponse {
            overall: common::HealthStatus::Healthy as i32,
            services: vec![], // TODO: populate in sub-sprint
        }))
    }

    async fn list_services(&self, _request: Request<ListServicesRequest>) -> Result<Response<ListServicesResponse>, Status> {
        // TODO: embrad should expose service state; for now return static list
        Ok(Response::new(ListServicesResponse {
            services: vec![
                ServiceInfo { name: "wardsondb".into(), state: "running".into(), pid: 0, uptime_seconds: 0, health_endpoint: "http://127.0.0.1:8090/_health".into() },
                ServiceInfo { name: "embra-trustd".into(), state: "running".into(), pid: 0, uptime_seconds: 0, health_endpoint: "grpc://127.0.0.1:50001".into() },
                ServiceInfo { name: "embra-brain".into(), state: "running".into(), pid: 0, uptime_seconds: 0, health_endpoint: "grpc://127.0.0.1:50002".into() },
            ],
        }))
    }

    async fn get_version(&self, _request: Request<GetVersionRequest>) -> Result<Response<GetVersionResponse>, Status> {
        Ok(Response::new(GetVersionResponse {
            embraos_version: "0.2.0-phase1".to_string(),
            embrad_version: "0.2.0-phase1".to_string(),
            wardsondb_version: "0.1.0".to_string(),
            kernel_version: String::new(),
        }))
    }

    async fn health_check(&self, _request: Request<common::HealthCheckRequest>) -> Result<Response<common::HealthCheckResponse>, Status> {
        Ok(Response::new(common::HealthCheckResponse {
            status: common::HealthStatus::Healthy as i32,
            service_name: "embra-apid".to_string(),
            version: env!("CARGO_PKG_VERSION").to_string(),
            uptime_seconds: self.start_time.elapsed().as_secs(),
            details: std::collections::HashMap::new(),
        }))
    }
}
