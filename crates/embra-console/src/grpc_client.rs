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
    ToolExecution { name: String, input: String, result: String, success: bool },
    ThinkingState { is_thinking: bool, name: String },
    ModeTransition { from_mode: i32, to_mode: i32, message: String },
    SetupPrompt { field_type: String, prompt: String, options: Vec<String>, default_value: String },
}

impl BrainClient {
    pub async fn connect(addr: &str) -> anyhow::Result<Self> {
        let channel = Channel::from_shared(addr.to_string())?
            .connect()
            .await?;
        info!("Connected to embra-apid at {}", addr);
        Ok(Self {
            client: EmbraApiClient::new(channel),
        })
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
                                            name: t.tool_name,
                                            input: t.input,
                                            result: t.result,
                                            success: t.success,
                                        }
                                    }
                                    brain::conversation_response::ResponseType::Thinking(t) => {
                                        ConsoleEvent::ThinkingState {
                                            is_thinking: t.is_thinking,
                                            name: t.name,
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
}
