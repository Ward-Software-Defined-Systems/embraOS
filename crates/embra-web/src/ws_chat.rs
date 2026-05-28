//! `/ws/chat` WebSocket handler — JSON↔gRPC bridge for the mobile chat
//! UI.
//!
//! Each connection opens its own tonic `Channel` to apid and a single
//! bidirectional `Converse` stream. Two tasks then pump in parallel:
//!
//! - **to_client:** each `apid::ConversationResponse`'s opaque `bytes
//!   payload` is decoded into a `brain::ConversationResponse`, translated
//!   to a [`ServerMsg`] (see `chat_bridge`), serialized as JSON, and
//!   shipped as a WS text frame.
//! - **from_client:** each WS text frame is deserialized into a
//!   [`ClientMsg`], converted to an `apid::ConversationRequest`, and
//!   pushed into the request stream that backs `Converse`.
//!
//! Binary frames are ignored (chat is text-only). Decode/encode errors
//! are reported back to the browser as a `ServerMsg::Error` and the
//! affected message is skipped — only stream-level failures terminate
//! the connection.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use embra_common::proto::apid::ConversationRequest;
use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::brain;
use futures_util::{SinkExt, StreamExt};
use prost::Message as ProstMessage;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Channel;

use crate::chat_bridge::{ClientMsg, ServerMsg, brain_to_server_msg};
use crate::state::AppState;

pub async fn ws_chat(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_chat_socket(socket, st))
}

async fn handle_chat_socket(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();

    // Parse the apid address into a tonic Endpoint up-front. Address
    // shape is validated by the operator at boot, so a failure here
    // means a misconfigured embrad — surface it as a single error frame.
    let endpoint = match Channel::from_shared(st.apid_addr.clone()) {
        Ok(e) => e,
        Err(e) => {
            let _ = sender
                .send(Message::Text(
                    json_err(&format!("invalid apid endpoint: {e}")).into(),
                ))
                .await;
            return;
        }
    };
    // `connect_lazy` defers the TCP handshake to the first RPC — fine
    // here because the very next call is `converse`.
    let channel = endpoint.connect_lazy();
    let mut client = EmbraApiClient::new(channel);

    // Bridge the WS receiver into the request stream via an mpsc — same
    // pattern as embra-console's `grpc_client::open_conversation`.
    let (req_tx, req_rx) = mpsc::channel::<ConversationRequest>(32);
    let req_stream = ReceiverStream::new(req_rx);

    let mut resp_stream = match client.converse(req_stream).await {
        Ok(r) => r.into_inner(),
        Err(e) => {
            let _ = sender
                .send(Message::Text(
                    json_err(&format!("converse open failed: {e}")).into(),
                ))
                .await;
            return;
        }
    };

    // gRPC → WS.
    let mut to_client = tokio::spawn(async move {
        loop {
            match resp_stream.message().await {
                Ok(Some(apid_resp)) => {
                    let brain_resp =
                        match brain::ConversationResponse::decode(apid_resp.payload.as_slice()) {
                            Ok(r) => r,
                            Err(e) => {
                                let _ = sender
                                    .send(Message::Text(
                                        json_err(&format!("decode brain response: {e}")).into(),
                                    ))
                                    .await;
                                continue;
                            }
                        };
                    let Some(srv_msg) = brain_to_server_msg(brain_resp) else {
                        continue;
                    };
                    let json = match serde_json::to_string(&srv_msg) {
                        Ok(j) => j,
                        Err(e) => {
                            let _ = sender
                                .send(Message::Text(
                                    json_err(&format!("encode server msg: {e}")).into(),
                                ))
                                .await;
                            continue;
                        }
                    };
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                Ok(None) => break, // stream ended cleanly
                Err(e) => {
                    let _ = sender
                        .send(Message::Text(
                            json_err(&format!("stream error: {e}")).into(),
                        ))
                        .await;
                    break;
                }
            }
        }
    });

    // WS → gRPC. Drops `req_tx` on exit, which closes the request
    // stream and lets the brain see EOF on its side.
    let mut from_client = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Text(t) => {
                    let Ok(client_msg) = serde_json::from_str::<ClientMsg>(&t) else {
                        // Malformed JSON from the browser — drop the
                        // frame silently. The frontend should never
                        // produce these in practice.
                        continue;
                    };
                    if req_tx.send(client_msg.into_proto()).await.is_err() {
                        break; // request stream closed
                    }
                }
                Message::Close(_) => break,
                _ => {} // binary frames ignored on the chat channel
            }
        }
    });

    // Either direction terminating tears down the other — symmetric
    // shutdown.
    tokio::select! {
        _ = &mut to_client => from_client.abort(),
        _ = &mut from_client => to_client.abort(),
    }
}

/// Serialize a `ServerMsg::Error` so we can ship a structured failure
/// to the browser before closing. Falls back to a hand-built JSON
/// string on the (extremely unlikely) chance the serializer itself
/// fails.
fn json_err(message: &str) -> String {
    serde_json::to_string(&ServerMsg::Error {
        message: message.to_string(),
    })
    .unwrap_or_else(|_| format!(r#"{{"t":"error","message":"json_err fallback"}}"#))
}
