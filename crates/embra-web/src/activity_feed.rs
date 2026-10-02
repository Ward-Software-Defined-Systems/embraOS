//! The one subscription to the brain's activity feed, through apid, fanned
//! out to every `/ws/activity` socket. It lives for the process: it
//! reconnects with a backoff, says `offline` once per outage, and keeps the
//! latest snapshot so a new socket starts with the picture, not a blank.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use embra_common::proto::apid::WatchActivityRequest;
use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::brain;
use prost::Message as ProstMessage;
use tokio::sync::broadcast;
use tokio_stream::StreamExt;
use tonic::transport::Channel;

use crate::activity_bridge::{ActivityMsg, frame_to_msg};

/// Messages a slow socket may fall behind by. A lagging socket skips what
/// it missed; the next snapshot resyncs it.
pub const FEED_CAPACITY: usize = 256;
/// The wait before the first reconnect; it doubles up to `BACKOFF_MAX` and
/// resets after a connection that delivered a snapshot.
pub const BACKOFF_BASE: Duration = Duration::from_secs(1);
pub const BACKOFF_MAX: Duration = Duration::from_secs(10);

#[derive(Clone)]
pub struct ActivityFeed {
    tx: broadcast::Sender<String>,
    /// The latest snapshot, or `offline`; what a new socket gets first.
    latest: Arc<RwLock<Option<String>>>,
}

impl ActivityFeed {
    /// Start the subscription task against apid and hand out the fan-out.
    pub fn spawn(apid_addr: String) -> Self {
        let feed = Self::new();
        let runner = feed.clone();
        tokio::spawn(async move { runner.run_forever(apid_addr).await });
        feed
    }

    pub fn new() -> Self {
        let (tx, _idle) = broadcast::channel(FEED_CAPACITY);
        Self { tx, latest: Arc::new(RwLock::new(None)) }
    }

    /// The first message for a new socket and the receiver for the rest.
    /// The receiver is taken before the latest message is read, so nothing
    /// published between the two is missed.
    pub fn subscribe(&self) -> (String, broadcast::Receiver<String>) {
        let rx = self.tx.subscribe();
        let first = self
            .latest
            .read()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_else(offline_json);
        (first, rx)
    }

    /// Publish one message. A snapshot or `offline` becomes the latest.
    pub fn publish(&self, msg: &ActivityMsg) {
        let Ok(json) = serde_json::to_string(msg) else {
            return;
        };
        if matches!(msg, ActivityMsg::Snapshot(_) | ActivityMsg::Offline)
            && let Ok(mut latest) = self.latest.write()
        {
            *latest = Some(json.clone());
        }
        let _ = self.tx.send(json);
    }

    /// After a connection ended: `offline` goes out once, when the
    /// connection had delivered a snapshot, and the next wait is chosen.
    pub fn after_run(&self, delivered: bool, backoff: Duration) -> Duration {
        if delivered {
            self.publish(&ActivityMsg::Offline);
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

    /// One connection, until its stream ends. `true` when it delivered a
    /// snapshot.
    async fn run_once(&self, apid_addr: &str) -> bool {
        let endpoint = match Channel::from_shared(apid_addr.to_string()) {
            Ok(e) => e,
            Err(e) => {
                tracing::error!(error = %e, "activity feed: invalid apid endpoint");
                return false;
            }
        };
        let mut client = EmbraApiClient::new(endpoint.connect_lazy());
        let mut frames = match client.watch_activity(WatchActivityRequest {}).await {
            Ok(r) => r.into_inner(),
            Err(e) => {
                tracing::debug!(error = %e, "activity feed: watch not open");
                return false;
            }
        };
        let mut delivered = false;
        while let Some(item) = frames.next().await {
            let frame = match item {
                Ok(f) => f,
                Err(e) => {
                    tracing::warn!(error = %e, "activity feed ended");
                    break;
                }
            };
            let Ok(decoded) = brain::ActivityFrame::decode(frame.payload.as_slice()) else {
                continue;
            };
            if let Some(msg) = frame_to_msg(decoded) {
                if !delivered && matches!(msg, ActivityMsg::Snapshot(_)) {
                    delivered = true;
                    tracing::info!("activity feed connected");
                }
                self.publish(&msg);
            }
        }
        delivered
    }
}

impl Default for ActivityFeed {
    fn default() -> Self {
        Self::new()
    }
}

pub fn next_backoff(current: Duration) -> Duration {
    (current * 2).min(BACKOFF_MAX)
}

fn offline_json() -> String {
    serde_json::to_string(&ActivityMsg::Offline).expect("a unit variant serializes")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::activity_bridge::Snapshot;

    #[test]
    fn the_backoff_doubles_from_one_second_and_caps_at_ten() {
        let mut b = BACKOFF_BASE;
        let mut seen = Vec::new();
        for _ in 0..5 {
            b = next_backoff(b);
            seen.push(b.as_secs());
        }
        assert_eq!(seen, vec![2, 4, 8, 10, 10]);
    }

    #[test]
    fn a_new_subscriber_gets_the_latest_snapshot_or_offline_first() {
        let feed = ActivityFeed::new();
        let (first, mut early) = feed.subscribe();
        assert_eq!(first, r#"{"t":"offline"}"#, "nothing yet means offline");

        feed.publish(&ActivityMsg::Snapshot(Snapshot { model: "m".into(), ..Default::default() }));
        feed.publish(&ActivityMsg::Tick(Default::default()));
        let (first, _late) = feed.subscribe();
        assert!(first.starts_with(r#"{"t":"snapshot""#), "the latest snapshot, not the tick: {first}");
        assert!(early.try_recv().unwrap().starts_with(r#"{"t":"snapshot""#));
        assert!(early.try_recv().unwrap().starts_with(r#"{"t":"tick""#));
    }

    #[test]
    fn an_offline_feed_is_announced_once_until_it_returns() {
        let feed = ActivityFeed::new();
        feed.publish(&ActivityMsg::Snapshot(Default::default()));
        let (_first, mut rx) = feed.subscribe();

        assert_eq!(feed.after_run(true, Duration::from_secs(8)), BACKOFF_BASE, "a delivered run resets the wait");
        assert_eq!(rx.try_recv().unwrap(), r#"{"t":"offline"}"#);
        assert_eq!(feed.after_run(false, BACKOFF_BASE), Duration::from_secs(2), "a failed reconnect doubles it");
        assert!(rx.try_recv().is_err(), "no second offline while it stays away");
        assert_eq!(feed.subscribe().0, r#"{"t":"offline"}"#, "a new socket is told at once");
    }
}
