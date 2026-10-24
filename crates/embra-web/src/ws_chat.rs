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
    crate::ws::bounded(ws).on_upgrade(move |socket| handle_chat_socket(socket, st))
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
    // Every hop that receives brain payloads takes the shared limit
    // (`media.rs` does the same): a `ToolExecution` carries a result of up
    // to 2 MiB and an uncapped input, and tonic's default is 4 MiB.
    let mut client = EmbraApiClient::new(channel)
        .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES);

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
                    let frame = match translate_apid_payload(&apid_resp.payload) {
                        Ok(Some(frame)) => frame,
                        Ok(None) => continue,
                        Err(error_frame) => {
                            let _ = sender.send(Message::Text(error_frame.into())).await;
                            continue;
                        }
                    };
                    if sender.send(Message::Text(frame.into())).await.is_err() {
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

/// One apid frame as the browser's text frame: the opaque payload decoded
/// as the brain's response, translated (`brain_to_server_msg`) and
/// serialized. `Ok(None)` is a response with no variant, nothing to send;
/// `Err` is the error frame to send in its place.
fn translate_apid_payload(payload: &[u8]) -> Result<Option<String>, String> {
    let brain_resp = brain::ConversationResponse::decode(payload)
        .map_err(|e| json_err(&format!("decode brain response: {e}")))?;
    let Some(srv_msg) = brain_to_server_msg(brain_resp) else {
        return Ok(None);
    };
    serde_json::to_string(&srv_msg)
        .map(Some)
        .map_err(|e| json_err(&format!("encode server msg: {e}")))
}

/// Serialize a `ServerMsg::Error` so we can ship a structured failure
/// to the browser before closing. Falls back to a hand-built JSON
/// string on the (extremely unlikely) chance the serializer itself
/// fails.
fn json_err(message: &str) -> String {
    serde_json::to_string(&ServerMsg::Error {
        message: message.to_string(),
    })
    .unwrap_or_else(|_| r#"{"t":"error","message":"json_err fallback"}"#.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Value, json};

    /// A failure of this bridge reaches the browser as the error frame the
    /// UI shows, with the message as given (#54).
    #[test]
    fn a_bridge_error_is_the_error_frame() {
        let v: Value = serde_json::from_str(&json_err("converse open failed: x")).unwrap();
        assert_eq!(v, json!({"t": "error", "message": "converse open failed: x"}));
    }

    /// What apid hands over goes to the browser as the frame the brain
    /// meant; bytes that are not a brain response become the error frame,
    /// never a dropped connection; a response with no variant sends
    /// nothing (#54).
    #[test]
    fn an_apid_payload_becomes_the_frame_the_brain_meant_or_an_error_frame() {
        let token = brain::ConversationResponse {
            response_type: Some(brain::conversation_response::ResponseType::Token(brain::StreamToken {
                text: "hel".into(),
            })),
        };
        let frame = translate_apid_payload(&token.encode_to_vec()).unwrap().expect("a frame");
        assert_eq!(serde_json::from_str::<Value>(&frame).unwrap(), json!({"t": "token", "text": "hel"}));

        let empty = brain::ConversationResponse { response_type: None };
        assert_eq!(translate_apid_payload(&empty.encode_to_vec()).unwrap(), None);

        let error_frame = translate_apid_payload(&[0xff, 0xff, 0xff, 0xff, 0xff]).unwrap_err();
        let v: Value = serde_json::from_str(&error_frame).unwrap();
        assert_eq!(v["t"], "error");
        assert!(v["message"].as_str().unwrap().starts_with("decode brain response: "), "{v}");
    }
}
