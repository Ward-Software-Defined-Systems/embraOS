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
