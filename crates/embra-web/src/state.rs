//! Shared axum application state.

use std::sync::{Arc, Mutex};

use crate::activity_feed::ActivityFeed;
use crate::arbiter::Arbiter;
use crate::metrics::CpuSnapshot;
use crate::pty_bridge::PtyBridge;
use crate::tool_feed::ToolFeed;

#[derive(Clone)]
pub struct AppState {
    pub bridge: PtyBridge,
    pub arbiter: Arbiter,
    /// Previous /proc/stat snapshot for CPU% delta. `None` until first poll
    /// — handler returns `cpu_pct: null` then. `std::sync::Mutex` is safe:
    /// the critical section is parse+swap, never spans `.await`.
    pub cpu_snap: Arc<Mutex<Option<CpuSnapshot>>>,
    /// Apid gRPC endpoint (e.g. `http://127.0.0.1:50000`). Each
    /// `/ws/chat` connection opens its own tonic Channel against this
    /// address — see `ws_chat::handle_chat_socket`.
    ///
    /// apid is embra-web's only door to the brain: chat, sessions, stop and
    /// media all go through it. The status pills look at other services
    /// directly, to see whether they are up, and ask nothing of them.
    pub apid_addr: String,
    /// The brain's activity feed, subscribed once through apid and fanned
    /// out to every `/ws/activity` socket (`activity_feed.rs`).
    pub activity: ActivityFeed,
    /// The brain's tool-call feed, subscribed once through apid, kept as a
    /// bounded backlog and fanned out to every `/ws/tools` socket
    /// (`tool_feed.rs`).
    pub tools: ToolFeed,
    /// This boot's access token (`access.rs`): every request on `:3345`
    /// carries it, in the query once and in a cookie after.
    pub token: Arc<str>,
}

impl AppState {
    /// A state with no console, no feed subscriptions and an apid nothing
    /// listens on (`127.0.0.1:1`), for a handler test. The PTY bridge's
    /// input queue comes with it, so a test can read what reached the PTY.
    #[cfg(test)]
    pub(crate) fn stub() -> (Self, tokio::sync::mpsc::Receiver<Vec<u8>>) {
        let (bridge, input_rx) = PtyBridge::stub();
        let state = AppState {
            bridge,
            arbiter: Arbiter::new(),
            cpu_snap: Arc::new(Mutex::new(None)),
            apid_addr: "http://127.0.0.1:1".to_string(),
            activity: ActivityFeed::new(),
            tools: ToolFeed::new(),
            token: Arc::from("0123456789abcdef0123456789abcdef"),
        };
        (state, input_rx)
    }
}
