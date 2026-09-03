//! Multi-client write arbitration: single-writer + read-only observers +
//! explicit takeover.
//!
//! The brain is single-conversation and there is exactly one shared PTY,
//! so the only thing to police is *who, among the connected browsers, may
//! type*. The first connection is the writer; later connections are
//! observers that still see all output live. Only the writer's
//! input/key/resize frames reach the PTY — this is enforced **here**
//! (server-authoritative); the UI role is advisory. An explicit, confirmed
//! takeover transfers the token; writer disconnect frees it (no idle
//! auto-handoff in v1).

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use tokio::sync::mpsc;

pub type ClientId = u64;

struct ClientHandle {
    /// Pre-serialized JSON control frames pushed to this client's WS task.
    tx: mpsc::UnboundedSender<String>,
}

struct Inner {
    writer: Option<ClientId>,
    clients: BTreeMap<ClientId, ClientHandle>,
}

#[derive(Clone)]
pub struct Arbiter {
    inner: Arc<Mutex<Inner>>,
    next_id: Arc<AtomicU64>,
}

impl Arbiter {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                writer: None,
                clients: BTreeMap::new(),
            })),
            next_id: Arc::new(AtomicU64::new(1)),
        }
    }

    /// Register a new connection. The first client becomes the writer;
    /// the rest are observers. Returns the assigned id and the receiver
    /// the WS task drains for `{"t":"role",...}` frames.
    pub fn connect(&self) -> (ClientId, mpsc::UnboundedReceiver<String>) {
        let id = self.next_id.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = mpsc::unbounded_channel::<String>();

        let mut inner = self.inner.lock().unwrap();
        if inner.writer.is_none() {
            inner.writer = Some(id);
        }
        inner.clients.insert(id, ClientHandle { tx });
        broadcast_roles(&inner);
        (id, rx)
    }

    pub fn is_writer(&self, id: ClientId) -> bool {
        self.inner.lock().unwrap().writer == Some(id)
    }

    /// Explicit takeover: the requesting client becomes the writer and the
    /// previous writer is demoted to observer. All clients are notified.
    pub fn takeover(&self, id: ClientId) {
        let mut inner = self.inner.lock().unwrap();
        if inner.clients.contains_key(&id) {
            inner.writer = Some(id);
            broadcast_roles(&inner);
        }
    }

    /// Drop a connection. If it held the writer token, the token is freed
    /// (a remaining observer must explicitly take control — v1 policy).
    pub fn disconnect(&self, id: ClientId) {
        let mut inner = self.inner.lock().unwrap();
        inner.clients.remove(&id);
        if inner.writer == Some(id) {
            inner.writer = None;
        }
        broadcast_roles(&inner);
    }
}

impl Default for Arbiter {
    fn default() -> Self {
        Self::new()
    }
}

/// Push a per-client `{"t":"role",...}` frame to every connection so each
/// UI reflects its own role and who currently holds the token.
fn broadcast_roles(inner: &Inner) {
    let owner = inner
        .writer
        .map(|w| w.to_string())
        .unwrap_or_else(|| "none".to_string());
    for (cid, handle) in inner.clients.iter() {
        let role = if inner.writer == Some(*cid) {
            "writer"
        } else {
            "observer"
        };
        let frame = format!(
            r#"{{"t":"role","role":"{}","owner":"{}"}}"#,
            role, owner
        );
        // Dropped receiver just means that client is tearing down.
        let _ = handle.tx.send(frame);
    }
}

#[cfg(test)]
mod tests {
    //! The write-arbitration contract: first connection = writer, later
    //! ones = observers, explicit takeover, writer disconnect frees the
    //! token with NO auto-handoff. Every state change broadcasts a role
    //! frame to EVERY client, so assertions drain to the LAST frame.
    use super::*;
    use tokio::sync::mpsc::error::TryRecvError;

    fn last_frame(rx: &mut mpsc::UnboundedReceiver<String>) -> Option<String> {
        let mut last = None;
        while let Ok(f) = rx.try_recv() {
            last = Some(f);
        }
        last
    }

    /// `(role, owner)` of a role frame, asserting the `t` tag.
    fn role_of(frame: &str) -> (String, String) {
        let v: serde_json::Value = serde_json::from_str(frame).expect("role frame is JSON");
        assert_eq!(v["t"], "role");
        (
            v["role"].as_str().expect("role").to_string(),
            v["owner"].as_str().expect("owner").to_string(),
        )
    }

    #[test]
    fn first_connect_is_writer_second_is_observer() {
        let arb = Arbiter::new();
        let (a, mut rx_a) = arb.connect();
        let (b, mut rx_b) = arb.connect();
        assert!(arb.is_writer(a));
        assert!(!arb.is_writer(b));
        assert_eq!(role_of(&last_frame(&mut rx_a).unwrap()), ("writer".into(), a.to_string()));
        assert_eq!(role_of(&last_frame(&mut rx_b).unwrap()), ("observer".into(), a.to_string()));
    }

    #[test]
    fn role_frame_json_shape() {
        let arb = Arbiter::new();
        let (a, mut rx_a) = arb.connect();
        let frame = last_frame(&mut rx_a).unwrap();
        let v: serde_json::Value = serde_json::from_str(&frame).unwrap();
        let obj = v.as_object().expect("object");
        let mut keys: Vec<&str> = obj.keys().map(String::as_str).collect();
        keys.sort_unstable();
        assert_eq!(keys, ["owner", "role", "t"]);
        assert_eq!(v["t"], "role");
        assert_eq!(v["role"], "writer");
        assert_eq!(v["owner"], a.to_string());
    }

    #[test]
    fn writer_disconnect_frees_token_no_auto_handoff() {
        let arb = Arbiter::new();
        let (a, _rx_a) = arb.connect();
        let (b, mut rx_b) = arb.connect();
        arb.disconnect(a);
        // v1 policy: the remaining observer must take control explicitly.
        assert!(!arb.is_writer(b));
        assert_eq!(role_of(&last_frame(&mut rx_b).unwrap()), ("observer".into(), "none".into()));
        // A fresh connection finds the token free and becomes the writer.
        let (c, mut rx_c) = arb.connect();
        assert!(arb.is_writer(c));
        assert_eq!(role_of(&last_frame(&mut rx_c).unwrap()), ("writer".into(), c.to_string()));
        assert_eq!(role_of(&last_frame(&mut rx_b).unwrap()), ("observer".into(), c.to_string()));
    }

    #[test]
    fn takeover_demotes_previous_writer() {
        let arb = Arbiter::new();
        let (a, mut rx_a) = arb.connect();
        let (b, mut rx_b) = arb.connect();
        arb.takeover(b);
        assert!(!arb.is_writer(a));
        assert!(arb.is_writer(b));
        assert_eq!(role_of(&last_frame(&mut rx_a).unwrap()), ("observer".into(), b.to_string()));
        assert_eq!(role_of(&last_frame(&mut rx_b).unwrap()), ("writer".into(), b.to_string()));
    }

    #[test]
    fn takeover_by_unknown_id_is_noop() {
        let arb = Arbiter::new();
        let (a, mut rx_a) = arb.connect();
        let _ = last_frame(&mut rx_a);
        arb.takeover(a + 1000);
        assert!(arb.is_writer(a));
        assert!(matches!(rx_a.try_recv(), Err(TryRecvError::Empty)));
    }

    #[test]
    fn observer_disconnect_keeps_writer() {
        let arb = Arbiter::new();
        let (a, mut rx_a) = arb.connect();
        let (b, _rx_b) = arb.connect();
        arb.disconnect(b);
        assert!(arb.is_writer(a));
        assert_eq!(role_of(&last_frame(&mut rx_a).unwrap()), ("writer".into(), a.to_string()));
    }

    #[test]
    fn dropped_receiver_does_not_panic() {
        let arb = Arbiter::new();
        let (a, rx_a) = arb.connect();
        drop(rx_a); // the WS task tore down without disconnecting yet
        let (b, mut rx_b) = arb.connect();
        assert!(arb.is_writer(a));
        assert_eq!(role_of(&last_frame(&mut rx_b).unwrap()), ("observer".into(), a.to_string()));
        arb.disconnect(a);
        assert!(!arb.is_writer(b));
    }
}
