//! The one subscription to the brain's tool-call feed, through apid, fanned
//! out to every `/ws/tools` socket. It lives for the process: it reconnects
//! with a backoff, says `offline` once per outage and `online` on each
//! return, and keeps a bounded backlog of the calls it saw so a reloaded
//! tab starts with the list, not a blank. The shape is `activity_feed.rs`'s.

use std::collections::VecDeque;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use embra_common::proto::apid::WatchToolCallsRequest;
use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::brain;
use prost::Message as ProstMessage;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

use crate::activity_feed::{BACKOFF_BASE, next_backoff};
use crate::tool_bridge::{ToolCall, ToolMsg, record_to_call};

/// Messages a slow socket may fall behind by. A lagging socket skips what
/// it missed; a reload brings the backlog.
pub const FEED_CAPACITY: usize = 256;
/// The backlog's bounds: calls, and the bytes of their inputs and results
/// together. The oldest call goes first when either is passed.
pub const BACKLOG_ENTRIES: usize = 200;
pub const BACKLOG_BYTES: usize = 4 * 1024 * 1024;

#[derive(Clone)]
pub struct ToolFeed {
    tx: broadcast::Sender<String>,
    backlog: Arc<RwLock<Backlog>>,
}

struct Backlog {
    calls: VecDeque<ToolCall>,
    bytes: usize,
    online: bool,
    max_entries: usize,
    max_bytes: usize,
}

impl Backlog {
    /// Keep the call; the oldest go first past either bound. The newest
    /// call always stays, even alone over the byte bound.
    fn push(&mut self, call: ToolCall) {
        self.bytes += call.bytes();
        self.calls.push_back(call);
        while self.calls.len() > 1
            && (self.calls.len() > self.max_entries || self.bytes > self.max_bytes)
        {
            if let Some(old) = self.calls.pop_front() {
                self.bytes -= old.bytes();
            }
        }
    }
}

impl ToolFeed {
    /// Start the subscription task against apid and hand out the fan-out.
    pub fn spawn(apid_addr: String) -> Self {
        let feed = Self::new();
        let runner = feed.clone();
        tokio::spawn(async move { runner.run_forever(apid_addr).await });
        feed
    }

    pub fn new() -> Self {
        Self::with_bounds(BACKLOG_ENTRIES, BACKLOG_BYTES)
    }

    /// A feed with its own backlog bounds; the tests use small ones.
    pub(crate) fn with_bounds(max_entries: usize, max_bytes: usize) -> Self {
        let (tx, _idle) = broadcast::channel(FEED_CAPACITY);
        Self {
            tx,
            backlog: Arc::new(RwLock::new(Backlog {
                calls: VecDeque::new(),
                bytes: 0,
                online: false,
                max_entries,
                max_bytes,
            })),
        }
    }

    /// The first messages for a new socket — the backlog, then the feed's
    /// state (`online`, or `offline` while the brain is away) — and the
    /// receiver for the rest. The receiver is taken before the backlog is
    /// read, so nothing published between the two is missed; the browser
    /// keys the calls, so one that lands in both is shown once.
    pub fn subscribe(&self) -> (Vec<String>, broadcast::Receiver<String>) {
        let rx = self.tx.subscribe();
        let mut first = Vec::with_capacity(2);
        if let Ok(b) = self.backlog.read() {
            first.push(to_json(&ToolMsg::Backlog { calls: b.calls.iter().cloned().collect() }));
            first.push(to_json(if b.online { &ToolMsg::Online } else { &ToolMsg::Offline }));
        }
        (first, rx)
    }

    /// Keep one call and hand it to every socket.
    pub fn publish_call(&self, call: ToolCall) {
        let json = to_json(&ToolMsg::Call(Box::new(call.clone())));
        if let Ok(mut b) = self.backlog.write() {
            b.push(call);
        }
        let _ = self.tx.send(json);
    }

    /// The feed to apid opened: every socket hears it.
    pub(crate) fn mark_online(&self) {
        if let Ok(mut b) = self.backlog.write() {
            b.online = true;
        }
        let _ = self.tx.send(to_json(&ToolMsg::Online));
    }

    /// After a connection ended: `offline` goes out once, when the
    /// connection had opened, and the next wait is chosen.
    pub fn after_run(&self, delivered: bool, backoff: Duration) -> Duration {
        if delivered {
            if let Ok(mut b) = self.backlog.write() {
                b.online = false;
            }
            let _ = self.tx.send(to_json(&ToolMsg::Offline));
            BACKOFF_BASE
        } else {
            next_backoff(backoff)
        }
    }

    async fn run_forever(self, apid_addr: String) {
        let mut backoff = BACKOFF_BASE;
        loop {
            let delivered = self.run_once(&apid_addr).await;
            backoff = self.after_run(delivered, backoff);
            tokio::time::sleep(backoff).await;
        }
    }

    /// One connection, until its stream ends. `true` when the RPC opened:
    /// the stream can stay silent for hours between calls.
    async fn run_once(&self, apid_addr: &str) -> bool {
        let endpoint = match Channel::from_shared(apid_addr.to_string()) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "tool feed: invalid apid endpoint");
                return false;
            }
        };
        // Every hop that receives brain payloads takes the shared limit;
        // a record is at most two 64 KiB sides, but the rule is uniform.
        let mut client = EmbraApiClient::new(endpoint.connect_lazy())
            .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES);
        let mut records = match client.watch_tool_calls(WatchToolCallsRequest {}).await {
            Ok(r) => r.into_inner(),
            Err(e) => {
                tracing::debug!(error = %e, "tool feed: watch not open");
                return false;
            }
        };
        tracing::info!("tool feed connected");
        self.mark_online();
        while let Some(item) = records.next().await {
            let frame = match item {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "tool feed ended");
                    break;
                }
            };
            let Ok(decoded) = brain::ToolCallRecord::decode(frame.payload.as_slice()) else {
                continue;
            };
            self.publish_call(record_to_call(decoded));
        }
        true
    }
}

impl Default for ToolFeed {
    fn default() -> Self {
        Self::new()
    }
}

fn to_json(msg: &ToolMsg) -> String {
    serde_json::to_string(msg).expect("the tool messages serialize")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(seq: u64, result: &str) -> ToolCall {
        ToolCall {
            seq,
            name: "file_read".into(),
            result: result.into(),
            ..Default::default()
        }
    }

    #[test]
    fn the_backlog_is_bounded_by_entries_and_bytes() {
        let feed = ToolFeed::with_bounds(3, 100);
        for seq in 1..=4 {
            feed.publish_call(call(seq, "ok"));
        }
        {
            let b = feed.backlog.read().unwrap();
            let kept: Vec<u64> = b.calls.iter().map(|c| c.seq).collect();
            assert_eq!(kept, vec![2, 3, 4], "the oldest goes first past the entry bound");
        }
        feed.publish_call(call(5, &"x".repeat(99)));
        {
            let b = feed.backlog.read().unwrap();
            let kept: Vec<u64> = b.calls.iter().map(|c| c.seq).collect();
            assert_eq!(kept, vec![5], "the byte bound evicts from the front until it holds");
            assert_eq!(b.bytes, 99);
        }
        feed.publish_call(call(6, &"y".repeat(120)));
        {
            let b = feed.backlog.read().unwrap();
            let kept: Vec<u64> = b.calls.iter().map(|c| c.seq).collect();
            assert_eq!(kept, vec![6], "a call over the bound alone is still kept, as the newest");
        }
    }

    #[test]
    fn a_new_socket_gets_the_backlog_first_and_offline_while_the_brain_is_away() {
        let feed = ToolFeed::new();
        let (first, mut early) = feed.subscribe();
        assert_eq!(first, vec![r#"{"t":"backlog","calls":[]}"#.to_string(), r#"{"t":"offline"}"#.to_string()]);

        feed.mark_online();
        feed.publish_call(call(1, "ok"));
        let (first, _late) = feed.subscribe();
        assert_eq!(first.len(), 2, "online: the backlog, then the state");
        assert!(first[0].starts_with(r#"{"t":"backlog","calls":[{"seq":1,"#), "{first:?}");
        assert_eq!(first[1], r#"{"t":"online"}"#, "a fresh tab must not read offline while the feed is up");
        assert_eq!(early.try_recv().unwrap(), r#"{"t":"online"}"#);
        assert!(early.try_recv().unwrap().starts_with(r#"{"t":"call","seq":1,"#));
    }

    #[test]
    fn an_offline_tool_feed_is_announced_once_until_it_returns() {
        let feed = ToolFeed::new();
        feed.mark_online();
        let (_first, mut rx) = feed.subscribe();

        assert_eq!(feed.after_run(true, Duration::from_secs(8)), BACKOFF_BASE, "a run that opened resets the wait");
        assert_eq!(rx.try_recv().unwrap(), r#"{"t":"offline"}"#);
        assert_eq!(feed.after_run(false, BACKOFF_BASE), Duration::from_secs(2), "a failed reconnect doubles it");
        assert!(rx.try_recv().is_err(), "no second offline while it stays away");
        assert_eq!(feed.subscribe().0[1], r#"{"t":"offline"}"#, "a new socket is told at once");
    }
}
