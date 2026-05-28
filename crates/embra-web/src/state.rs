//! Shared axum application state.

use std::sync::{Arc, Mutex};

use crate::arbiter::Arbiter;
use crate::metrics::CpuSnapshot;
use crate::pty_bridge::PtyBridge;

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
    pub apid_addr: String,
}
