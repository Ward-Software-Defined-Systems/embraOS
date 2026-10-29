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
/// How long a fresh connection may run without one decodable snapshot
/// before it counts as failed (#62). apid sees a healthy stream while every
/// frame is dropped here, so only this deadline fires the reconnect. A
/// live feed delivers its first snapshot within seconds of connecting.
pub const FIRST_SNAPSHOT_WINDOW: Duration = Duration::from_secs(60);

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
        // The shared limit, like every hop that receives brain payloads;
        // a frame here is small, the rule is uniform.
        let mut client = EmbraApiClient::new(endpoint.connect_lazy())
            .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES);
        let frames = match client.watch_activity(WatchActivityRequest {}).await {
            Ok(r) => r.into_inner(),
            Err(e) => {
                tracing::debug!(error = %e, "activity feed: watch not open");
                return false;
            }
        };
        // The error is only logged, so it travels as text.
        self.consume(frames.map(|item| item.map(|f| f.payload).map_err(|e| e.to_string())), FIRST_SNAPSHOT_WINDOW).await
    }

    /// The frame loop of one connection. `true` when it delivered a
    /// snapshot. A connection that has produced no decodable snapshot by the
    /// end of `window` returns `false` (#62): without the deadline such a run
    /// would spin forever on dropped frames, never backing off or
    /// reconnecting. The deadline is checked while waiting for a frame and
    /// again after each one, so a firehose of undecodable frames cannot
    /// outrun it.
    async fn consume<S>(&self, mut frames: S, window: Duration) -> bool
    where
        S: tokio_stream::Stream<Item = Result<Vec<u8>, String>> + Unpin,
    {
        let deadline = tokio::time::Instant::now() + window;
        let mut delivered = false;
        let mut undecodable_said = false;
        loop {
            let next = if delivered {
                frames.next().await
            } else {
                match tokio::time::timeout_at(deadline, frames.next()).await {
                    Ok(next) => next,
                    Err(_) => {
                        tracing::warn!("activity feed: no decodable snapshot within {window:?}; reconnecting");
                        return false;
                    }
                }
            };
            let Some(item) = next else { break };
            let payload = match item {
                Ok(p) => p,
                Err(e) => {
                    tracing::warn!(error = %e, "activity feed ended");
                    break;
                }
            };
            match brain::ActivityFrame::decode(payload.as_slice()) {
                Ok(decoded) => {
                    if let Some(msg) = frame_to_msg(decoded) {
                        if !delivered && matches!(msg, ActivityMsg::Snapshot(_)) {
                            delivered = true;
                            tracing::info!("activity feed connected");
                        }
                        self.publish(&msg);
                    }
                }
                Err(e) => {
                    // Said once per connection: a run whose every frame fails
                    // to decode would otherwise spin without a word (#62).
                    if !undecodable_said {
                        undecodable_said = true;
                        tracing::warn!(error = %e, "activity feed: a frame did not decode; such frames are dropped");
                    }
                }
            }
            if !delivered && tokio::time::Instant::now() >= deadline {
                tracing::warn!("activity feed: no decodable snapshot within {window:?}; reconnecting");
                return false;
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

#[cfg(test)]
mod consume_tests {
    use super::ActivityFeed;
    use embra_common::proto::brain;
    use prost::Message as _;
    use std::time::Duration;
    use tokio_stream::wrappers::ReceiverStream;

    fn snapshot_frame() -> Vec<u8> {
        brain::ActivityFrame {
            frame: Some(brain::activity_frame::Frame::Snapshot(brain::ActivitySnapshot::default())),
        }
        .encode_to_vec()
    }

    /// #62: a connection that keeps sending frames none of which decode is
    /// given up at the window, so the caller backs off and reconnects
    /// instead of spinning; one that delivers a snapshot is a success.
    #[tokio::test]
    async fn a_connection_with_no_decodable_snapshot_in_the_window_is_a_failed_run() {
        let feed = ActivityFeed::default();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, String>>(8);
        let firehose = tokio::spawn(async move {
            loop {
                if tx.send(Ok(vec![0xff, 0xfe, 0xfd])).await.is_err() {
                    break;
                }
                tokio::time::sleep(Duration::from_millis(2)).await;
            }
        });
        let started = std::time::Instant::now();
        let delivered = feed.consume(ReceiverStream::new(rx), Duration::from_millis(80)).await;
        firehose.abort();
        assert!(!delivered, "a run with no decodable snapshot is a failed run");
        assert!(started.elapsed() < Duration::from_secs(5), "it ended at the window, not with the stream");

        let feed = ActivityFeed::default();
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, String>>(8);
        tx.send(Ok(snapshot_frame())).await.unwrap();
        drop(tx);
        assert!(feed.consume(ReceiverStream::new(rx), Duration::from_millis(80)).await);
    }
}
