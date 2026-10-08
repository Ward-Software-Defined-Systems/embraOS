use std::time::Duration;

use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use crate::config;
use crate::db::WardsonDbClient;
use crate::provider::health::{self, ProviderProbe};

const NORMAL_CHECK_INTERVAL: Duration = Duration::from_secs(300); // 5 minutes
/// Most reminders handed over by one 15-second check.
const REMINDER_BATCH_MAX: usize = 16;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Priority {
    Critical,
    Normal,
    Low,
}

impl Priority {
    pub fn label(&self) -> &'static str {
        match self {
            Priority::Critical => "CRITICAL",
            Priority::Normal => "NOTICE",
            Priority::Low => "INFO",
        }
    }
}

/// What a reminder or a cron job set with `act=true` asks for when it
/// fires: a turn of the intelligence's own, run by the Converse stream
/// that holds the notification receiver (`grpc_service`), in its active
/// session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ActTrigger {
    pub kind: TriggerKind,
    /// What fired: the reminder's message, or the job's command as shown.
    pub headline: String,
    /// What the turn carries: the reminder's message, or the run's result
    /// (already cut at `CRON_RESULT_MAX`).
    pub body: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TriggerKind {
    Reminder,
    Cron,
}

/// What the proactive loops hand to the Converse stream. It carries no
/// id, timestamp or delivery flag: a notification is shown once, when the
/// stream drains the channel, and nothing looks it up afterwards. What
/// must not repeat is decided before it is built (`health::transition_events`).
/// `act` is set when the reminder or job that fired asked for a turn.
#[derive(Debug, Clone)]
pub struct Notification {
    pub priority: Priority,
    pub message: String,
    pub act: Option<ActTrigger>,
}

impl Notification {
    pub fn new(priority: Priority, message: impl Into<String>) -> Self {
        Self {
            priority,
            message: message.into(),
            act: None,
        }
    }

    pub fn with_act(mut self, act: ActTrigger) -> Self {
        self.act = Some(act);
        self
    }

    pub fn priority_label(&self) -> &str {
        self.priority.label()
    }
}

/// The notification for a reminder that fired: the text the console shows,
/// and the trigger when the reminder asked for a turn.
pub(crate) fn reminder_notification(fired: &crate::tools::FiredReminder) -> Notification {
    let notification = Notification::new(Priority::Normal, fired.text.clone());
    if fired.act {
        notification.with_act(ActTrigger {
            kind: TriggerKind::Reminder,
            headline: fired.message.clone(),
            body: fired.message.clone(),
        })
    } else {
        notification
    }
}

/// The notification for a cron run: the report the console shows, and the
/// trigger with the run's result when the job asked for a turn.
pub(crate) fn cron_notification(fired: &crate::tools::cron::CronFired) -> Notification {
    let notification = Notification::new(Priority::Normal, fired.text.clone());
    if fired.act {
        notification.with_act(ActTrigger {
            kind: TriggerKind::Cron,
            headline: fired.display.clone(),
            body: fired.result.clone(),
        })
    } else {
        notification
    }
}

pub fn start_proactive_engine(
    db: &WardsonDbClient,
    config_tz: &str,
    boot_api_key: &str,
) -> mpsc::Receiver<Notification> {
    let (tx, rx) = mpsc::channel(64);
    let db = db.clone();
    let config_tz = config_tz.to_string();

    // Normal health checks every 5 minutes: WardSONDB + system memory,
    // then the active LLM provider's endpoint (the first run, ~30 s after
    // boot, is the model check the config load never did). A `/provider`
    // switch or a finished setup flow re-runs the tick immediately.
    let tx_health = tx.clone();
    let db_health = db.clone();
    let boot_key = boot_api_key.to_string();
    tokio::spawn(async move {
        // Initial delay to let the system stabilize
        tokio::time::sleep(Duration::from_secs(30)).await;

        let mut previous: Option<ProviderProbe> = None;
        loop {
            run_health_checks(&db_health, &tx_health).await;
            previous = run_provider_probe(&db_health, &boot_key, previous, &tx_health).await;
            tokio::select! {
                _ = tokio::time::sleep(NORMAL_CHECK_INTERVAL) => {}
                _ = health::probe_requested() => {}
            }
        }
    });

    // Reminder checks every 15 seconds
    let tx_reminders = tx.clone();
    let db_reminders = db.clone();
    tokio::spawn(async move {
        // Initial delay
        tokio::time::sleep(Duration::from_secs(10)).await;

        loop {
            deliver_due_reminders(&db_reminders, &tx_reminders).await;
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });

    // Cron checks every 15 seconds. The report of a run is handed over
    // without waiting: this loop is what RUNS the jobs, and waiting for room
    // in the channel would stop every job behind the one being reported.
    let tx_cron = tx;
    let db_cron = db.clone();
    let config_tz_cron = config_tz;
    tokio::spawn(async move {
        // Initial delay
        tokio::time::sleep(Duration::from_secs(15)).await;

        loop {
            let fired = crate::tools::cron::check_crons(&db_cron, &config_tz_cron).await;
            for run in fired {
                push_notification(&tx_cron, cron_notification(&run));
            }
            tokio::time::sleep(Duration::from_secs(15)).await;
        }
    });

    rx
}

/// Non-blocking hand-off to the Converse stream. The channel is bounded
/// (64) and drained only while an operational stream holds the receiver;
/// a headless boot fills it, and an awaiting `send` would park this loop
/// — and every check after it — forever. Dropping a notification nobody
/// is listening to is the right trade. What is dropped is logged with its
/// text, so a cron report that found no room can still be read back
/// (`system_logs`).
fn push_notification(tx: &mpsc::Sender<Notification>, notification: Notification) {
    use mpsc::error::TrySendError;
    let (why, lost) = match tx.try_send(notification) {
        Ok(()) => return,
        Err(TrySendError::Full(n)) => ("channel full", n),
        Err(TrySendError::Closed(n)) => ("channel closed", n),
    };
    warn!(
        priority = lost.priority_label(),
        "proactive notification dropped ({why}): {}",
        lost.message
    );
}

/// Room for up to `max` notifications, reserved — or `None` when the
/// channel has none. A reserved slot cannot be taken by another loop, and
/// one that goes unused is returned when its permit drops.
fn reserve_slots(
    tx: &mpsc::Sender<Notification>,
    max: usize,
) -> Option<mpsc::PermitIterator<'_, Notification>> {
    let room = tx.capacity().min(max);
    if room == 0 {
        return None;
    }
    tx.try_reserve_many(room).ok()
}

/// Hand over the reminders that are due. Unlike every other notification a
/// reminder is never dropped: firing consumes it, so it fires only into a
/// slot reserved for it. With the channel full it stays in the store, and
/// the operator gets it late instead of not at all.
async fn deliver_due_reminders(db: &WardsonDbClient, tx: &mpsc::Sender<Notification>) {
    let Some(slots) = reserve_slots(tx, REMINDER_BATCH_MAX) else {
        debug!("proactive channel full — due reminders wait in the store");
        return;
    };
    let fired = crate::tools::check_reminders(db, slots.len()).await;
    for (slot, reminder) in slots.zip(fired) {
        slot.send(reminder_notification(&reminder));
    }
}

/// One provider probe: resolve the target from persisted config the way a
/// turn would, run it, publish it for `/status`, and tell the operator
/// about CHANGES (`health::transition_events`).
async fn run_provider_probe(
    db: &WardsonDbClient,
    boot_key: &str,
    previous: Option<ProviderProbe>,
    tx: &mpsc::Sender<Notification>,
) -> Option<ProviderProbe> {
    // A pre-wizard boot has no config yet → nothing to probe.
    let target = config::load_config(db)
        .await
        .ok()
        .and_then(|cfg| crate::grpc_service::provider_probe_target(&cfg, boot_key));
    let probe = health::probe(target.as_ref()).await;
    if probe.configured {
        if probe.healthy() {
            info!(
                target: "provider::health",
                kind = %probe.kind,
                model = %probe.model,
                endpoint = %probe.endpoint,
                latency_ms = ?probe.latency_ms,
                "provider endpoint ok"
            );
        } else {
            warn!(
                target: "provider::health",
                kind = %probe.kind,
                model = %probe.model,
                endpoint = %probe.endpoint,
                reachable = probe.reachable,
                model_present = ?probe.model_present,
                key_state = ?probe.key_state,
                error = ?probe.error,
                "provider endpoint check failed"
            );
        }
    }
    for notification in health::transition_events(previous.as_ref(), &probe) {
        push_notification(tx, notification);
    }
    health::record(probe.clone());
    Some(probe)
}

async fn run_health_checks(db: &WardsonDbClient, tx: &mpsc::Sender<Notification>) {
    // Check WardSONDB health with degradation awareness
    match db.health_detailed().await {
        Ok(detail) if detail.up => {
            if detail.status == "degraded" {
                warn!("WardSONDB storage engine degraded");
                let warning_msg = detail
                    .warning
                    .unwrap_or_else(|| "Storage engine degraded".into());
                push_notification(
                    tx,
                    Notification::new(
                        Priority::Critical,
                        format!(
                        "CRITICAL: WardSONDB storage engine is degraded — {}. Memory and session writes may be failing silently.",
                        warning_msg
                        ),
                    ),
                );
            }
            if detail.write_pressure.as_deref() == Some("high") {
                push_notification(
                    tx,
                    Notification::new(
                        Priority::Normal,
                        "WardSONDB write pressure is high — compaction in progress, non-essential queries may be slow.",
                    ),
                );
            }
        }
        Ok(_) => {
            warn!("WardSONDB health check failed");
            push_notification(
                tx,
                Notification::new(
                    Priority::Critical,
                    "WardSONDB is not responding. Data persistence may be affected.",
                ),
            );
        }
        Err(e) => {
            error!("WardSONDB health check error: {}", e);
            push_notification(
                tx,
                Notification::new(
                    Priority::Critical,
                    format!("WardSONDB health check error: {}", e),
                ),
            );
        }
    }

    // Check system memory
    if let Some(usage) = get_memory_usage_mb()
        && usage > 512
    {
        push_notification(
            tx,
            Notification::new(
                Priority::Normal,
                format!("High memory usage: {}MB", usage),
            ),
        );
    }
}

fn get_memory_usage_mb() -> Option<u64> {
    // Read from /proc/self/status on Linux
    if let Ok(status) = std::fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if line.starts_with("VmRSS:") {
                let parts: Vec<&str> = line.split_whitespace().collect();
                if let Some(kb) = parts.get(1).and_then(|v| v.parse::<u64>().ok()) {
                    return Some(kb / 1024);
                }
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn note(text: &str) -> Notification {
        Notification::new(Priority::Normal, text)
    }

    #[test]
    fn a_full_channel_drops_the_notification_and_returns() {
        // Not a #[tokio::test]: push_notification must not need a runtime
        // to get out of the way.
        let (tx, mut rx) = mpsc::channel(2);
        push_notification(&tx, note("one"));
        push_notification(&tx, note("two"));
        push_notification(&tx, note("three")); // no room: dropped, not awaited
        assert_eq!(rx.try_recv().unwrap().message, "one");
        assert_eq!(rx.try_recv().unwrap().message, "two");
        assert!(rx.try_recv().is_err());
        // A closed channel is the same non-event.
        drop(rx);
        push_notification(&tx, note("four"));
    }

    #[test]
    fn slots_are_reserved_up_to_what_is_free() {
        let (tx, mut rx) = mpsc::channel(4);
        push_notification(&tx, note("health"));

        let slots = reserve_slots(&tx, 16).expect("three slots are free");
        assert_eq!(slots.len(), 3);
        // Reserved means taken: nobody else gets them meanwhile.
        assert_eq!(tx.capacity(), 0);
        push_notification(&tx, note("cron report")); // dropped

        // Two reminders fire into three slots; the third slot goes back.
        for (slot, text) in slots.zip(["first", "second"]) {
            slot.send(note(text));
        }
        assert_eq!(tx.capacity(), 1);
        let got: Vec<String> = std::iter::from_fn(|| rx.try_recv().ok())
            .map(|n| n.message)
            .collect();
        assert_eq!(got, ["health", "first", "second"]);
    }

    #[test]
    fn a_reminder_with_act_carries_its_trigger_and_one_without_carries_none() {
        let plain = reminder_notification(&crate::tools::FiredReminder {
            text: "Reminder: check the build".into(),
            message: "check the build".into(),
            act: false,
        });
        assert_eq!(plain.message, "Reminder: check the build");
        assert!(plain.act.is_none());
        let acting = reminder_notification(&crate::tools::FiredReminder {
            text: "Reminder: check the build".into(),
            message: "check the build".into(),
            act: true,
        });
        assert_eq!(acting.message, "Reminder: check the build");
        assert_eq!(
            acting.act,
            Some(ActTrigger {
                kind: TriggerKind::Reminder,
                headline: "check the build".into(),
                body: "check the build".into(),
            })
        );
    }

    #[test]
    fn a_cron_fire_with_act_carries_its_result() {
        let run = crate::tools::cron::CronFired::new("system_status", "uptime 3h", true, "");
        let n = cron_notification(&run);
        assert_eq!(n.message, "embraCRON [system_status]: uptime 3h");
        assert_eq!(
            n.act,
            Some(ActTrigger {
                kind: TriggerKind::Cron,
                headline: "system_status".into(),
                body: "uptime 3h".into(),
            })
        );
        let quiet = crate::tools::cron::CronFired::new("system_status", "uptime 3h", false, " (note)");
        assert_eq!(quiet.text, "embraCRON [system_status]: uptime 3h (note)");
        assert!(cron_notification(&quiet).act.is_none());
    }

    #[test]
    fn no_room_means_no_slots_and_nothing_to_fire() {
        let (tx, _rx) = mpsc::channel(1);
        push_notification(&tx, note("fills it"));
        assert!(reserve_slots(&tx, 16).is_none());
        // The cap is honored when there is plenty of room.
        let (tx, _rx) = mpsc::channel(64);
        assert_eq!(reserve_slots(&tx, REMINDER_BATCH_MAX).unwrap().len(), REMINDER_BATCH_MAX);
        assert!(reserve_slots(&tx, 0).is_none());
    }
}
