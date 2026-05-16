//! `/ws/terminal` WebSocket handler.
//!
//! - server→client: PTY output as binary frames (to *every* connection,
//!   regardless of role) + `{"t":"role",...}` text frames from the arbiter.
//! - client→server: binary frames = raw keystrokes; text control frames
//!   `resize` / `input` / `key` / `takeover`.
//!
//! Write-arbitration is enforced **here**: input/key/resize are dropped
//! unless the connection currently holds the writer token. `takeover` is
//! allowed from any connection (it's the explicit handoff request).

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::state::AppState;

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ClientControl {
    Resize { cols: u16, rows: u16 },
    Input { data: String },
    Key { code: String },
    Takeover,
}

/// Map a logical key name to the byte sequence xterm would send, so chrome
/// palette/wizard controls can drive the in-TUI `Selector` etc.
fn key_to_bytes(code: &str) -> Option<&'static [u8]> {
    Some(match code {
        "Up" => b"\x1b[A",
        "Down" => b"\x1b[B",
        "Right" => b"\x1b[C",
        "Left" => b"\x1b[D",
        "Enter" => b"\r",
        "Tab" => b"\t",
        "Backspace" => b"\x7f",
        "Escape" => b"\x1b",
        "PageUp" => b"\x1b[5~",
        "PageDown" => b"\x1b[6~",
        "Home" => b"\x1b[H",
        "End" => b"\x1b[F",
        _ => return None,
    })
}

pub async fn ws_terminal(
    ws: WebSocketUpgrade,
    State(st): State<AppState>,
) -> Response {
    ws.on_upgrade(move |socket| handle_socket(socket, st))
}

async fn handle_socket(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let mut output = st.bridge.subscribe();
    let (id, mut ctrl_rx) = st.arbiter.connect();

    // To-client: one task owns the WS sink, multiplexing PTY output
    // (binary, all roles) and arbiter role frames (text).
    let mut to_client = tokio::spawn(async move {
        loop {
            tokio::select! {
                out = output.recv() => match out {
                    Ok(bytes) => {
                        if sender.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                },
                ctrl = ctrl_rx.recv() => match ctrl {
                    Some(json) => {
                        if sender.send(Message::Text(json.into())).await.is_err() {
                            break;
                        }
                    }
                    None => break,
                },
            }
        }
    });

    // From-client: writer-gated input; takeover from anyone.
    let arbiter = st.arbiter.clone();
    let bridge = st.bridge.clone();
    let mut from_client = tokio::spawn(async move {
        while let Some(Ok(msg)) = receiver.next().await {
            match msg {
                Message::Binary(b) => {
                    if arbiter.is_writer(id) {
                        bridge.write_input(b.to_vec());
                    }
                }
                Message::Text(t) => {
                    let Ok(ctrl) = serde_json::from_str::<ClientControl>(&t) else {
                        continue;
                    };
                    match ctrl {
                        ClientControl::Takeover => arbiter.takeover(id),
                        ClientControl::Resize { cols, rows } => {
                            if arbiter.is_writer(id) {
                                bridge.resize(cols, rows);
                            }
                        }
                        ClientControl::Input { data } => {
                            if arbiter.is_writer(id) {
                                bridge.write_input(data.into_bytes());
                            }
                        }
                        ClientControl::Key { code } => {
                            if arbiter.is_writer(id) {
                                if let Some(seq) = key_to_bytes(&code) {
                                    bridge.write_input(seq.to_vec());
                                }
                            }
                        }
                    }
                }
                Message::Close(_) => break,
                _ => {}
            }
        }
    });

    tokio::select! {
        _ = &mut to_client => from_client.abort(),
        _ = &mut from_client => to_client.abort(),
    }
    st.arbiter.disconnect(id);
}
