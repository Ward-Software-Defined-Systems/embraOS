//! `/ws/tools`: the tool-call feed to a browser. Read-only: the socket gets
//! the backlog first (and `offline` while the brain is away), then every
//! call the feed publishes; what the browser sends is read and dropped. No
//! arbiter role: an observer sees the same list as the writer, as it sees
//! the same terminal.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};

use crate::state::AppState;

pub async fn ws_tools(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    crate::ws::bounded(ws).on_upgrade(move |socket| handle_tools_socket(socket, st))
}

async fn handle_tools_socket(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let (first, mut feed) = st.tools.subscribe();
    for msg in first {
        if sender.send(Message::Text(msg.into())).await.is_err() {
            return;
        }
    }
    loop {
        tokio::select! {
            msg = feed.recv() => match msg {
                Ok(json) => {
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                // A slow socket skips what it missed; a reload brings the
                // backlog.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            },
            inbound = receiver.next() => match inbound {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                // Nothing is accepted from the browser on this socket.
                Some(Ok(_)) => {}
            },
        }
    }
}
