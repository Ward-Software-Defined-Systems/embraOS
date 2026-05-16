//! Shared axum application state.

use crate::arbiter::Arbiter;
use crate::pty_bridge::PtyBridge;

#[derive(Clone)]
pub struct AppState {
    pub bridge: PtyBridge,
    pub arbiter: Arbiter,
}
