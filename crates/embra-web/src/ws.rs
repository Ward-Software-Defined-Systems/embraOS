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
//!
//! Every attach also requests a full console repaint (`PtyBridge::repaint`,
//! see the contract in `pty_bridge.rs`): a fresh xterm has no screen and
//! the TUI only emits diffs, so a new tab would otherwise stay blank until
//! a real window resize.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};
use serde::Deserialize;

use crate::state::AppState;

/// Sent to every browser as it attaches, before any console output. The
/// console enables bracketed paste once, at its start
/// (`embra-console/src/terminal/mod.rs`), and that is before any browser is
/// listening: a subscriber sees only what comes after it. xterm.js wraps a
/// clipboard paste in `ESC[200~ … ESC[201~` only once it has seen this mode
/// set; without it a multi-line paste reaches the console as lines and
/// Enters, one message per line. The console keeps the mode on for its
/// life on the web PTY, so every new terminal is told so.
pub(crate) const ATTACH_PREAMBLE: &[u8] = b"\x1b[?2004h";

#[derive(Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ClientControl {
    /// `xpixel`/`ypixel` (media wave): the screen's pixel size, so the
    /// PTY winsize carries real cell geometry for the console's in-TUI
    /// image pane (TIOCGWINSZ ws_xpixel/ws_ypixel). Default 0 = unknown.
    Resize {
        cols: u16,
        rows: u16,
        #[serde(default)]
        xpixel: u16,
        #[serde(default)]
        ypixel: u16,
    },
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
    // Fresh-attach repaint — for EVERY role: observers never send resize
    // frames (the client gates them on `writable`, and this handler drops
    // non-writer resizes below), and a writer's same-size resize is a
    // kernel no-op anyway. Must stay AFTER `subscribe()`: a broadcast
    // receiver only sees bytes sent after it was created, and the repaint
    // is exactly those bytes. If the new tab's grid differs from the PTY's,
    // this frame arrives at the old size and the browser's own resize
    // repaints once more at the right one.
    st.bridge.repaint();

    // To-client: one task owns the WS sink, multiplexing PTY output
    // (binary, all roles) and arbiter role frames (text).
    let mut to_client = tokio::spawn(async move {
        // The terminal mode the console set before this browser was
        // listening, ahead of the repaint bytes (see ATTACH_PREAMBLE).
        if sender
            .send(Message::Binary(axum::body::Bytes::from_static(ATTACH_PREAMBLE)))
            .await
            .is_err()
        {
            return;
        }
        loop {
            tokio::select! {
                out = output.recv() => match out {
                    Ok(bytes) => {
                        if sender.send(Message::Binary(bytes)).await.is_err() {
                            break;
                        }
                    }
                    // Bytes were lost to a slow socket and the gap stays.
                    // Not the place for `bridge.repaint()`: a repaint is more
                    // bytes, and a client that lags would ask for one after
                    // the other. It needs a rate limit first (see the
                    // channel's comment in `pty_bridge.rs`).
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
                        ClientControl::Resize { cols, rows, xpixel, ypixel } => {
                            if arbiter.is_writer(id) {
                                bridge.resize(cols, rows, xpixel, ypixel);
                            }
                        }
                        ClientControl::Input { data } => {
                            if arbiter.is_writer(id) {
                                bridge.write_input(data.into_bytes());
                            }
                        }
                        ClientControl::Key { code } => {
                            if arbiter.is_writer(id)
                                && let Some(seq) = key_to_bytes(&code)
                            {
                                bridge.write_input(seq.to_vec());
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
