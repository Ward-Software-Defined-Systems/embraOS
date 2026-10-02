//! `/ws/activity`: the activity feed to a browser. Read-only: the socket
//! gets the latest snapshot (or `offline`) first, then every message the
//! feed publishes; what the browser sends is read and dropped. No arbiter
//! role: an observer sees the same picture as the writer.

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::Response;
use futures_util::{SinkExt, StreamExt};

use crate::state::AppState;

pub async fn ws_activity(ws: WebSocketUpgrade, State(st): State<AppState>) -> Response {
    ws.on_upgrade(move |socket| handle_activity_socket(socket, st))
}

async fn handle_activity_socket(socket: WebSocket, st: AppState) {
    let (mut sender, mut receiver) = socket.split();
    let (first, mut feed) = st.activity.subscribe();
    if sender.send(Message::Text(first.into())).await.is_err() {
        return;
    }
    loop {
        tokio::select! {
            msg = feed.recv() => match msg {
                Ok(json) => {
                    if sender.send(Message::Text(json.into())).await.is_err() {
                        break;
                    }
                }
                // A slow socket skips what it missed; the next snapshot
                // resyncs it.
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
