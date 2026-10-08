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
        //
        // Field by field, by hand. A field added to the proto and not copied
        // here is dropped without an error; `timestamp` is not forwarded.
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
                                        attachment_ids: um.attachment_ids,
                                        file_paths: um.file_paths,
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
        #[expect(
            clippy::result_large_err,
            reason = "the stream's item type is Result<_, tonic::Status>, fixed by the service trait"
        )]
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

    // Out-of-band operator interrupt — unary so it bypasses the parked
    // Converse stream (tonic runs it as its own task).
    async fn stop_turn(&self, _request: Request<StopTurnRequest>) -> Result<Response<StopTurnResponse>, Status> {
        let mut brain = self.backends.brain_client().await?;
        let resp = brain.stop_turn(embra_common::proto::brain::StopTurnRequest {}).await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(StopTurnResponse { payload }))
    }

    // --- Activity feed pass-through ---

    type WatchActivityStream = Pin<Box<dyn Stream<Item = Result<ActivityFrame, Status>> + Send>>;

    /// The brain's activity feed, frame by frame, as an opaque payload like
    /// `Converse`. embra-web holds the one production subscription.
    async fn watch_activity(
        &self,
        _request: Request<WatchActivityRequest>,
    ) -> Result<Response<Self::WatchActivityStream>, Status> {
        let mut brain = self.backends.brain_client().await?;
        let frames = brain
            .watch_activity(embra_common::proto::brain::WatchActivityRequest {})
            .await?
            .into_inner();
        #[expect(
            clippy::result_large_err,
            reason = "the stream's item type is Result<_, tonic::Status>, fixed by the service trait"
        )]
        let output = frames.map(|frame| frame.map(|f| ActivityFrame { payload: f.encode_to_vec() }));
        Ok(Response::new(Box::pin(output)))
    }

    // --- Media store pass-through ---

    async fn put_media(&self, request: Request<PutMediaRequest>) -> Result<Response<PutMediaResponse>, Status> {
        let req = request.into_inner();
        let mut brain = self.backends.brain_client().await?;
        let resp = brain
            .put_media(embra_common::proto::brain::PutMediaRequest {
                session_name: req.session_name,
                name: req.name,
                media_type_hint: req.media_type_hint,
                data: req.data,
            })
            .await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(PutMediaResponse { payload }))
    }

    async fn get_media(&self, request: Request<GetMediaRequest>) -> Result<Response<GetMediaResponse>, Status> {
        let req = request.into_inner();
        let mut brain = self.backends.brain_client().await?;
        let resp = brain
            .get_media(embra_common::proto::brain::GetMediaRequest { id: req.id })
            .await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(GetMediaResponse { payload }))
    }

    async fn get_file(&self, request: Request<GetFileRequest>) -> Result<Response<GetFileResponse>, Status> {
        let req = request.into_inner();
        let mut brain = self.backends.brain_client().await?;
        let resp = brain
            .get_file(embra_common::proto::brain::GetFileRequest { path: req.path })
            .await?;
        let payload = resp.into_inner().encode_to_vec();
        Ok(Response::new(GetFileResponse { payload }))
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
            embraos_version: env!("CARGO_PKG_VERSION").to_string(),
            embrad_version: env!("CARGO_PKG_VERSION").to_string(),
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

#[cfg(test)]
mod tests {
    use super::*;

    /// Nothing listens on port 1: the watch fails like every other proxied
    /// call, with the brain's unavailability, instead of hanging.
    #[tokio::test]
    async fn an_unreachable_brain_fails_the_watch_with_unavailable() {
        let backends = BackendConnections::new(
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:1".to_string(),
        );
        let api = EmbraApiImpl::new(backends);
        let err = api
            .watch_activity(Request::new(WatchActivityRequest {}))
            .await
            .err()
            .expect("no brain, no stream");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }
}
