//! gRPC proxy implementation for embra-apid.
//!
//! Proxies all RPCs to the appropriate backend service.
//! The Converse RPC is bidirectional streaming — forwarded to embra-brain.

use crate::proxy::BackendConnections;
use embra_common::proto::apid::embra_api_server::EmbraApi;
use embra_common::proto::apid::*;
use embra_common::proto::brain;
use embra_common::proto::brain::brain_service_client::BrainServiceClient;
use embra_common::proto::common;
use embra_common::proto::trust::trust_service_client::TrustServiceClient;

use prost::Message;
use std::future::Future;
use std::pin::Pin;
use tokio_stream::{Stream, StreamExt};
use tonic::transport::Channel;
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

    /// One shape for the unary pass-throughs to the brain (#41): its
    /// client, the call, and the reply encoded as the opaque payload the
    /// apid proto carries. The bytes are the brain's own message, so a
    /// field cannot be dropped on the way; `Converse` keeps the one
    /// hand-written mapping (`map_conversation_request`).
    async fn proxy_brain<Req, Reply, F, Fut, Out>(
        &self,
        req: Req,
        call: F,
        wrap: fn(Vec<u8>) -> Out,
    ) -> Result<Response<Out>, Status>
    where
        Reply: Message,
        F: FnOnce(BrainServiceClient<Channel>, Req) -> Fut,
        Fut: Future<Output = Result<Response<Reply>, Status>>,
    {
        let brain = self.backends.brain_client().await?;
        let reply = call(brain, req).await?;
        Ok(Response::new(wrap(reply.into_inner().encode_to_vec())))
    }

    /// The same shape towards trustd.
    async fn proxy_trust<Req, Reply, F, Fut, Out>(
        &self,
        req: Req,
        call: F,
        wrap: fn(Vec<u8>) -> Out,
    ) -> Result<Response<Out>, Status>
    where
        Reply: Message,
        F: FnOnce(TrustServiceClient<Channel>, Req) -> Fut,
        Fut: Future<Output = Result<Response<Reply>, Status>>,
    {
        let trust = self.backends.trust_client().await?;
        let reply = call(trust, req).await?;
        Ok(Response::new(wrap(reply.into_inner().encode_to_vec())))
    }
}

/// The apid request as the brain's (#55 item 4): field by field, by hand.
/// A field added to the proto and not copied here is dropped without an
/// error, and `timestamp` is not forwarded (the brain stamps its own).
/// The two protos number their fields differently, so this is a mapping
/// of names, never a re-encode of bytes; `request_mapping_tests` lists
/// every field that must survive.
pub(crate) fn map_conversation_request(req: ConversationRequest) -> brain::ConversationRequest {
    brain::ConversationRequest {
        request_type: req.request_type.map(|rt| match rt {
            conversation_request::RequestType::UserMessage(um) => {
                brain::conversation_request::RequestType::UserMessage(brain::UserMessage {
                    content: um.content,
                    timestamp: None,
                    attachment_ids: um.attachment_ids,
                    file_paths: um.file_paths,
                })
            }
            conversation_request::RequestType::SlashCommand(sc) => {
                brain::conversation_request::RequestType::SlashCommand(brain::SlashCommand {
                    command: sc.command,
                    args: sc.args,
                })
            }
            conversation_request::RequestType::SessionAttach(sa) => {
                brain::conversation_request::RequestType::SessionAttach(brain::SessionAttach {
                    session_name: sa.session_name,
                })
            }
        }),
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

        // Map apid ConversationRequest → brain ConversationRequest: the
        // brain client's converse() expects Stream<Item = brain::ConversationRequest>.
        // A request the stream failed to deliver is dropped.
        let brain_stream = incoming.filter_map(|msg| msg.ok().map(map_conversation_request));

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
        self.proxy_brain(
            brain::ListSessionsRequest {},
            |mut c, r| async move { c.list_sessions(r).await },
            |payload| ListSessionsResponse { payload },
        )
        .await
    }

    // Out-of-band operator interrupt — unary so it bypasses the parked
    // Converse stream (tonic runs it as its own task).
    async fn stop_turn(&self, _request: Request<StopTurnRequest>) -> Result<Response<StopTurnResponse>, Status> {
        self.proxy_brain(
            brain::StopTurnRequest {},
            |mut c, r| async move { c.stop_turn(r).await },
            |payload| StopTurnResponse { payload },
        )
        .await
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
        let output = frames.map(|frame| frame.map(|f| ActivityFrame { payload: f.encode_to_vec() }));
        Ok(Response::new(Box::pin(output)))
    }

    // --- Tool-call feed pass-through ---

    type WatchToolCallsStream = Pin<Box<dyn Stream<Item = Result<ToolCallRecord, Status>> + Send>>;

    /// The brain's tool-call feed, record by record, as an opaque payload
    /// like `Converse`. embra-web holds the one production subscription.
    async fn watch_tool_calls(
        &self,
        _request: Request<WatchToolCallsRequest>,
    ) -> Result<Response<Self::WatchToolCallsStream>, Status> {
        let mut brain = self.backends.brain_client().await?;
        let records = brain
            .watch_tool_calls(embra_common::proto::brain::WatchToolCallsRequest {})
            .await?
            .into_inner();
        let output = records.map(|rec| rec.map(|r| ToolCallRecord { payload: r.encode_to_vec() }));
        Ok(Response::new(Box::pin(output)))
    }

    /// The record read back, unary pass-through: the request and the
    /// response are the brain's messages, serialized.
    async fn list_tool_calls(
        &self,
        request: Request<ListToolCallsRequest>,
    ) -> Result<Response<ListToolCallsResponse>, Status> {
        let inner = brain::ListToolCallsRequest::decode(request.into_inner().payload.as_slice())
            .map_err(|e| Status::invalid_argument(format!("ListToolCallsRequest payload: {e}")))?;
        self.proxy_brain(
            inner,
            |mut c, r| async move { c.list_tool_calls(r).await },
            |payload| ListToolCallsResponse { payload },
        )
        .await
    }

    // --- Media store pass-through ---

    async fn put_media(&self, request: Request<PutMediaRequest>) -> Result<Response<PutMediaResponse>, Status> {
        let req = request.into_inner();
        self.proxy_brain(
            brain::PutMediaRequest {
                session_name: req.session_name,
                name: req.name,
                media_type_hint: req.media_type_hint,
                data: req.data,
            },
            |mut c, r| async move { c.put_media(r).await },
            |payload| PutMediaResponse { payload },
        )
        .await
    }

    async fn get_media(&self, request: Request<GetMediaRequest>) -> Result<Response<GetMediaResponse>, Status> {
        let req = request.into_inner();
        self.proxy_brain(
            brain::GetMediaRequest { id: req.id },
            |mut c, r| async move { c.get_media(r).await },
            |payload| GetMediaResponse { payload },
        )
        .await
    }

    async fn get_file(&self, request: Request<GetFileRequest>) -> Result<Response<GetFileResponse>, Status> {
        let req = request.into_inner();
        self.proxy_brain(
            brain::GetFileRequest { path: req.path },
            |mut c, r| async move { c.get_file(r).await },
            |payload| GetFileResponse { payload },
        )
        .await
    }

    async fn create_session(&self, request: Request<CreateSessionRequest>) -> Result<Response<CreateSessionResponse>, Status> {
        let req = request.into_inner();
        self.proxy_brain(
            brain::CreateSessionRequest { name: req.name },
            |mut c, r| async move { c.create_session(r).await },
            |payload| CreateSessionResponse { payload },
        )
        .await
    }

    async fn switch_session(&self, request: Request<SwitchSessionRequest>) -> Result<Response<SwitchSessionResponse>, Status> {
        let req = request.into_inner();
        self.proxy_brain(
            brain::SwitchSessionRequest { name: req.name },
            |mut c, r| async move { c.switch_session(r).await },
            |payload| SwitchSessionResponse { payload },
        )
        .await
    }

    async fn close_session(&self, _request: Request<CloseSessionRequest>) -> Result<Response<CloseSessionResponse>, Status> {
        self.proxy_brain(
            brain::CloseSessionRequest {},
            |mut c, r| async move { c.close_session(r).await },
            |payload| CloseSessionResponse { payload },
        )
        .await
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
        self.proxy_trust(
            embra_common::proto::trust::GetSoulStatusRequest {},
            |mut c, r| async move { c.get_soul_status(r).await },
            |payload| GetSoulStatusResponse { payload },
        )
        .await
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

    #[tokio::test]
    async fn an_unreachable_brain_fails_the_tool_call_watch_with_unavailable() {
        let backends = BackendConnections::new(
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:1".to_string(),
        );
        let api = EmbraApiImpl::new(backends);
        let err = api
            .watch_tool_calls(Request::new(WatchToolCallsRequest {}))
            .await
            .err()
            .expect("no brain, no stream");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }

    #[tokio::test]
    async fn an_unreachable_brain_fails_the_tool_call_list_with_unavailable() {
        let backends = BackendConnections::new(
            "http://127.0.0.1:1".to_string(),
            "http://127.0.0.1:1".to_string(),
        );
        let api = EmbraApiImpl::new(backends);
        let payload = embra_common::proto::brain::ListToolCallsRequest {
            limit: 5,
            before: String::new(),
            session: String::new(),
        }
        .encode_to_vec();
        let err = api
            .list_tool_calls(Request::new(ListToolCallsRequest { payload }))
            .await
            .expect_err("no brain, no page");
        assert_eq!(err.code(), tonic::Code::Unavailable);
    }
}

#[cfg(test)]
mod request_mapping_tests {
    use super::*;
    use embra_common::proto::brain;

    fn mapped(rt: conversation_request::RequestType) -> brain::conversation_request::RequestType {
        map_conversation_request(ConversationRequest { request_type: Some(rt) })
            .request_type
            .expect("the variant survives")
    }

    /// Every field of a user message reaches the brain's message but the
    /// timestamp, which the brain stamps itself (#55 item 4). The two protos
    /// number their fields differently, so the mapping is by name, never a
    /// re-encode of bytes.
    #[test]
    fn a_user_message_maps_its_content_attachments_and_files_and_drops_the_timestamp() {
        let um = UserMessage {
            content: "hello".into(),
            attachment_ids: vec!["att-1".into(), "att-2".into()],
            file_paths: vec!["/embra/workspace/a.txt".into()],
        };
        match mapped(conversation_request::RequestType::UserMessage(um)) {
            brain::conversation_request::RequestType::UserMessage(m) => {
                assert_eq!(m.content, "hello");
                assert_eq!(m.attachment_ids, vec!["att-1".to_string(), "att-2".to_string()]);
                assert_eq!(m.file_paths, vec!["/embra/workspace/a.txt".to_string()]);
                assert_eq!(m.timestamp, None, "the brain stamps its own");
            }
            other => panic!("not a user message: {other:?}"),
        }
    }

    #[test]
    fn a_slash_command_maps_its_command_and_args() {
        let sc = SlashCommand { command: "switch".into(), args: "main --force".into() };
        match mapped(conversation_request::RequestType::SlashCommand(sc)) {
            brain::conversation_request::RequestType::SlashCommand(c) => {
                assert_eq!((c.command.as_str(), c.args.as_str()), ("switch", "main --force"));
            }
            other => panic!("not a slash command: {other:?}"),
        }
    }

    #[test]
    fn a_session_attach_maps_its_session_name_and_an_empty_request_stays_empty() {
        let sa = SessionAttach { session_name: "research".into() };
        match mapped(conversation_request::RequestType::SessionAttach(sa)) {
            brain::conversation_request::RequestType::SessionAttach(a) => assert_eq!(a.session_name, "research"),
            other => panic!("not an attach: {other:?}"),
        }
        assert!(map_conversation_request(ConversationRequest { request_type: None }).request_type.is_none());
    }
}
