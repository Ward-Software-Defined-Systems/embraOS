//! gRPC client for talking to embra-brain via embra-apid.

use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::apid::*;
use embra_common::proto::brain;

use prost::Message;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;
use tracing::{info, error};

pub struct BrainClient {
    client: EmbraApiClient<Channel>,
}

/// Events that the TUI consumes (replaces Phase 0's StreamEvent + proactive notifications)
#[derive(Debug)]
pub enum ConsoleEvent {
    Token(String),
    ResponseDone(String),
    SystemMessage { content: String, msg_type: String },
    ToolExecution {
        tool_use_id: String,
        name: String,
        input_json: String,
        result: String,
        is_error: bool,
    },
    ThinkingState { is_thinking: bool, name: String, current_tool: Option<String> },
    ModeTransition { from_mode: i32, to_mode: i32, message: String },
    SetupPrompt { field_type: String, prompt: String, options: Vec<String>, default_value: String },
    /// Live reasoning/CoT shard from the brain. Routed exclusively to
    /// the expression panel surface; never appended to the response
    /// buffer or persisted. Cleared on user submit, ResponseDone,
    /// SystemMessage::Error, and ModeTransition.
    ReasoningDelta(String),
    /// An image became visible: operator-attached (`/attach`), produced
    /// or viewed by a tool, or replayed from history on attach
    /// (`replay == true`). The frame carries only the reference — bytes
    /// come from `BrainClient::get_media` when the pane renders.
    Media(brain::MediaRef),
}

impl BrainClient {
    pub async fn connect(addr: &str) -> anyhow::Result<Self> {
        let channel = Channel::from_shared(addr.to_string())?
            .connect()
            .await?;
        info!("Connected to embra-apid at {}", addr);
        Ok(Self {
            // GetMedia responses carry stored image bytes (≤ 12 MiB);
            // tonic's 4 MiB default would reject them at this hop.
            client: EmbraApiClient::new(channel)
                .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES),
        })
    }

    /// A detached handle for background fetches (tonic clients are
    /// cheap clones over the shared channel), so the main loop never
    /// holds `&mut self` across a download + decode.
    pub fn media_client(&self) -> EmbraApiClient<Channel> {
        self.client.clone()
    }

    /// Open a bidirectional conversation stream.
    /// Returns (sender for user input, receiver for brain events).
    pub async fn open_conversation(
        &mut self,
        session_name: &str,
    ) -> anyhow::Result<(
        mpsc::Sender<ConversationRequest>,
        mpsc::Receiver<ConsoleEvent>,
    )> {
        let (in_tx, in_rx) = mpsc::channel::<ConversationRequest>(32);
        let (out_tx, out_rx) = mpsc::channel::<ConsoleEvent>(100);

        // Send session attach as first message
        let _ = in_tx.send(ConversationRequest {
            request_type: Some(conversation_request::RequestType::SessionAttach(
                SessionAttach { session_name: session_name.to_string() }
            )),
        }).await;

        // Open the bidirectional stream
        let in_stream = ReceiverStream::new(in_rx);
        let response = self.client.converse(in_stream).await?;
        let mut resp_stream = response.into_inner();

        // Spawn task to read responses and convert to ConsoleEvents
        tokio::spawn(async move {
            loop {
                match resp_stream.message().await {
                    Ok(Some(resp)) => {
                        // Deserialize the pass-through payload into brain::ConversationResponse
                        if let Ok(brain_resp) = brain::ConversationResponse::decode(
                            resp.payload.as_slice()
                        ) {
                            if let Some(rt) = brain_resp.response_type {
                                let event = match rt {
                                    brain::conversation_response::ResponseType::Token(t) => {
                                        ConsoleEvent::Token(t.text)
                                    }
                                    brain::conversation_response::ResponseType::Done(d) => {
                                        ConsoleEvent::ResponseDone(d.full_response)
                                    }
                                    brain::conversation_response::ResponseType::System(s) => {
                                        ConsoleEvent::SystemMessage {
                                            content: s.content,
                                            msg_type: format!("{}", s.msg_type),
                                        }
                                    }
                                    brain::conversation_response::ResponseType::Tool(t) => {
                                        ConsoleEvent::ToolExecution {
                                            tool_use_id: t.tool_use_id,
                                            name: t.tool_name,
                                            input_json: t.input_json,
                                            result: t.result,
                                            is_error: t.is_error,
                                        }
                                    }
                                    brain::conversation_response::ResponseType::Thinking(t) => {
                                        ConsoleEvent::ThinkingState {
                                            is_thinking: t.is_thinking,
                                            name: t.name,
                                            current_tool: if t.current_tool.is_empty() {
                                                None
                                            } else {
                                                Some(t.current_tool)
                                            },
                                        }
                                    }
                                    brain::conversation_response::ResponseType::ModeChange(m) => {
                                        ConsoleEvent::ModeTransition {
                                            from_mode: m.from_mode,
                                            to_mode: m.to_mode,
                                            message: m.message,
                                        }
                                    }
                                    brain::conversation_response::ResponseType::Setup(s) => {
                                        ConsoleEvent::SetupPrompt {
                                            field_type: match s.field_type {
                                                1 => "text".to_string(),
                                                2 => "selector".to_string(),
                                                3 => "confirm".to_string(),
                                                _ => "text".to_string(),
                                            },
                                            prompt: s.prompt,
                                            options: s.options,
                                            default_value: s.default_value,
                                        }
                                    }
                                    brain::conversation_response::ResponseType::ReasoningDelta(r) => {
                                        ConsoleEvent::ReasoningDelta(r.text)
                                    }
                                    brain::conversation_response::ResponseType::Media(m) => {
                                        ConsoleEvent::Media(m)
                                    }
                                };
                                if out_tx.send(event).await.is_err() {
                                    break; // Console closed
                                }
                            }
                        }
                    }
                    Ok(None) => break, // Stream ended
                    Err(e) => {
                        error!("Stream error: {}", e);
                        break;
                    }
                }
            }
        });

        Ok((in_tx, out_rx))
    }

    /// Fetch the current expression panel state.
    /// Returns (content, version) — updated_at is intentionally dropped
    /// because the console only uses version for cache-change detection.
    pub async fn get_expression(&mut self) -> anyhow::Result<(String, u64)> {
        let resp = self.client.get_expression(GetExpressionRequest {}).await?;
        let inner = resp.into_inner();
        Ok((inner.content, inner.version))
    }

    /// Out-of-band operator interrupt for a stuck in-flight turn. Unary on
    /// the same multiplexed channel as the Converse stream (the
    /// get_expression precedent) — it reaches the brain even while the
    /// stream is parked inside a turn. Returns whether a turn was actually
    /// in flight.
    pub async fn stop_turn(&mut self) -> anyhow::Result<bool> {
        let resp = self.client.stop_turn(StopTurnRequest {}).await?;
        let payload = resp.into_inner().payload;
        let inner = embra_common::proto::brain::StopTurnResponse::decode(&payload[..])?;
        Ok(inner.was_in_turn)
    }
}

/// Unary `GetMedia` through apid, decoded from the opaque payload.
pub async fn fetch_media(
    mut client: EmbraApiClient<Channel>,
    id: &str,
) -> anyhow::Result<(brain::MediaRef, Vec<u8>)> {
    let resp = client
        .get_media(GetMediaRequest { id: id.to_string() })
        .await?
        .into_inner();
    let decoded = brain::GetMediaResponse::decode(resp.payload.as_slice())?;
    let meta = decoded
        .media
        .ok_or_else(|| anyhow::anyhow!("GetMedia response without meta"))?;
    Ok((meta, decoded.data))
}
