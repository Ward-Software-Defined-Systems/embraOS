//! Maintenance boots and the jobs a session schedules for the next boot.
//!
//! A GRUB menu entry puts `embra.mode=update|backup|restore` on the kernel
//! line: embrad runs one job of embra-update on the console instead of the
//! services, then reboots back to the menu. A session schedules a snapshot
//! or a restore for the next boot through the marker `/embra/state/next-boot`
//! (`/backup create`, `/backup restore`): read and removed before it is
//! acted on, so a job that fails never loops; a snapshot runs before the
//! services and the boot continues, a restore runs and reboots. A menu
//! entry wins over the marker and leaves it for the following boot.
//!
//! Everything here runs before the tokio runtime, where stdio is still the
//! kernel console and no service exists.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};

use embra_boot::BootMode;
use tracing::{error, info, warn};

pub const UPDATER: &str = "/usr/bin/embra-update";
pub const MARKER_PATH: &str = "/embra/state/next-boot";
/// How long the console shows a failure before the reboot to the menu.
const PAUSE: std::time::Duration = std::time::Duration::from_secs(15);

/// What a session scheduled for the next boot.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Scheduled {
    Snapshot { label: String },
    Restore { name: String },
}

/// What this boot runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// The services, nothing before them.
    Services,
    /// The scheduled snapshot, then the services.
    SnapshotThenServices { label: String },
    /// The scheduled restore, then a reboot.
    RestoreThenReboot { name: String },
    /// A menu entry's job, then a reboot.
    Maintenance(BootMode),
}

/// The marker's text: `mode=snapshot` with `label=`, or `mode=restore`
/// with `name=`; anything else is nothing.
pub fn parse_marker(text: &str) -> Option<Scheduled> {
    let mut mode = None;
    let mut label = None;
    let mut name = None;
    for line in text.lines() {
        if let Some((k, v)) = line.split_once('=') {
            match k.trim() {
                "mode" => mode = Some(v.trim().to_string()),
                "label" => label = Some(v.trim().to_string()),
                "name" => name = Some(v.trim().to_string()),
                _ => {}
            }
        }
    }
    match mode.as_deref() {
        Some("snapshot") => Some(Scheduled::Snapshot { label: label.filter(|l| !l.is_empty()).unwrap_or_else(|| "scheduled".into()) }),
        Some("restore") => name.filter(|n| !n.is_empty()).map(|name| Scheduled::Restore { name }),
        _ => None,
    }
}

/// Read the marker and remove it, so it is acted on once whatever the
/// outcome. `None` without one (a development host has none).
pub fn take_marker() -> Option<Scheduled> {
    take_marker_at(Path::new(MARKER_PATH))
}

pub fn take_marker_at(path: &Path) -> Option<Scheduled> {
    let text = std::fs::read_to_string(path).ok()?;
    if let Err(e) = std::fs::remove_file(path) {
        error!("the next-boot marker {} could not be removed ({e}); it is not acted on", path.display());
        return None;
    }
    let job = parse_marker(&text);
    if job.is_none() {
        warn!("the next-boot marker {} named no job: {text:?}", path.display());
    }
    job
}

/// Pure: the kernel line's mode against what a session scheduled. A menu
/// entry wins; the caller leaves the marker in place then.
pub fn resolve(mode: BootMode, scheduled: Option<Scheduled>) -> Decision {
    match (mode, scheduled) {
        (BootMode::Normal, None) => Decision::Services,
        (BootMode::Normal, Some(Scheduled::Snapshot { label })) => Decision::SnapshotThenServices { label },
        (BootMode::Normal, Some(Scheduled::Restore { name })) => Decision::RestoreThenReboot { name },
        (other, _) => Decision::Maintenance(other),
    }
}

/// What a job of the updater came to: its exit code and its last stderr line.
struct Ran {
    code: Option<i32>,
    reason: String,
}

/// Run the updater on the console: stdout and stdin are the console (the
/// progress lines, the restore prompt), stderr is read for the reason.
fn run_updater(args: &[&str]) -> Ran {
    let child = Command::new(UPDATER).args(args).stdin(Stdio::inherit()).stdout(Stdio::inherit()).stderr(Stdio::piped()).spawn();
    let mut child = match child {
        Ok(c) => c,
        Err(e) => return Ran { code: None, reason: format!("{UPDATER} could not be started: {e}") },
    };
    let mut stderr = String::new();
    if let Some(mut pipe) = child.stderr.take() {
        let _ = pipe.read_to_string(&mut stderr);
    }
    let status = child.wait();
    let reason = stderr.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("").trim().to_string();
    match status {
        Ok(s) => Ran { code: s.code(), reason },
        Err(e) => Ran { code: None, reason: format!("{UPDATER} could not be waited for: {e}") },
    }
}

fn say(line: &str) {
    eprintln!("[embrad] {line}");
}

/// A menu entry's job. Every exit path reboots (the caller reboots): the
/// menu is where the operator returns.
pub fn run_maintenance(mode: BootMode) {
    say(&format!("maintenance boot: {}", mode.name()));
    let ran = match mode {
        BootMode::Update => run_updater(&["apply", "--console"]),
        BootMode::Backup => run_updater(&["snapshot", "--label", "manual", "--taken-by", "manual"]),
        BootMode::Restore => run_updater(&["restore", "--console"]),
        BootMode::Normal => return,
    };
    match ran.code {
        Some(0) => {
            say(&format!("{} done; rebooting", mode.name()));
            info!("maintenance {} done", mode.name());
        }
        Some(3) => say(&format!("{}: nothing to do; rebooting to the menu", mode.name())),
        Some(2) => {
            say(&format!("{} refused: {}", mode.name(), ran.reason));
            say(&format!("rebooting to the menu in {} s", PAUSE.as_secs()));
            std::thread::sleep(PAUSE);
        }
        code => {
            say(&format!("{} failed ({}): {}", mode.name(), code.map(|c| c.to_string()).unwrap_or_else(|| "no exit code".into()), ran.reason));
            error!("maintenance {} failed: {}", mode.name(), ran.reason);
            say(&format!("rebooting to the menu in {} s", PAUSE.as_secs()));
            std::thread::sleep(PAUSE);
        }
    }
}

/// The snapshot a trial boot takes before the services (`embra.trial=1`
/// from GRUB: a staged release's first boots), so a copy of STATE and DATA
/// exists before the new brain migrates the knowledge graph. Idempotent
/// across the tries (`--once` on the label). A snapshot that cannot be
/// taken is the reason the caller halts on: the try is counted and GRUB
/// falls back.
pub fn run_trial_snapshot(version: &str) -> Result<(), String> {
    let label = format!("pre-v{version}");
    say(&format!("trial boot of v{version}: taking the snapshot \"{label}\" of STATE and DATA before the knowledge graph migrates"));
    let ran = run_updater(&["snapshot", "--label", &label, "--taken-by", "pending-slot", "--once"]);
    match ran.code {
        Some(0) => {
            info!("trial snapshot {label} done");
            Ok(())
        }
        Some(3) => {
            info!("trial snapshot {label} exists from an earlier try");
            Ok(())
        }
        _ => Err(ran.reason),
    }
}

/// A scheduled snapshot, before the services; the boot continues whatever
/// the outcome, which is said on the console and in the log.
pub fn run_scheduled_snapshot(label: &str) {
    say(&format!("scheduled snapshot \"{label}\" before the services start"));
    let ran = run_updater(&["snapshot", "--label", label, "--taken-by", "scheduled"]);
    match ran.code {
        Some(0) => info!("scheduled snapshot {label} done"),
        Some(3) => info!("scheduled snapshot {label}: nothing to do"),
        _ => {
            say(&format!("scheduled snapshot \"{label}\" did not complete: {}", ran.reason));
            error!("scheduled snapshot {label} failed: {}", ran.reason);
        }
    }
}

/// A scheduled restore; the caller reboots after it.
pub fn run_scheduled_restore(name: &str) {
    say(&format!("scheduled restore of \"{name}\" before the services start"));
    let ran = run_updater(&["restore", name, "--yes"]);
    match ran.code {
        Some(0) => say(&format!("restore of \"{name}\" done; rebooting")),
        _ => {
            say(&format!("restore of \"{name}\" did not complete: {}", ran.reason));
            error!("scheduled restore {name} failed: {}", ran.reason);
            say(&format!("rebooting in {} s", PAUSE.as_secs()));
            std::thread::sleep(PAUSE);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_menu_entry_on_the_cmdline_wins_over_the_marker() {
        let snap = Some(Scheduled::Snapshot { label: "nightly".into() });
        assert_eq!(resolve(BootMode::Update, snap.clone()), Decision::Maintenance(BootMode::Update));
        assert_eq!(resolve(BootMode::Restore, None), Decision::Maintenance(BootMode::Restore));
        assert_eq!(resolve(BootMode::Normal, snap), Decision::SnapshotThenServices { label: "nightly".into() });
        assert_eq!(resolve(BootMode::Normal, Some(Scheduled::Restore { name: "x".into() })), Decision::RestoreThenReboot { name: "x".into() });
        assert_eq!(resolve(BootMode::Normal, None), Decision::Services);
    }

    #[test]
    fn a_marker_is_consumed_before_it_is_acted_on() {
        let path = std::env::temp_dir().join(format!("embrad-next-boot-{}", std::process::id()));
        std::fs::write(&path, "mode=snapshot\nlabel=nightly\n").unwrap();
        assert_eq!(take_marker_at(&path), Some(Scheduled::Snapshot { label: "nightly".into() }));
        assert!(!path.exists(), "the marker is gone before the job runs");
        assert_eq!(take_marker_at(&path), None);
        std::fs::write(&path, "mode=restore\nname=2026-11-05_101200_manual\n").unwrap();
        assert_eq!(take_marker_at(&path), Some(Scheduled::Restore { name: "2026-11-05_101200_manual".into() }));
        assert!(!path.exists());
    }

    #[test]
    fn a_marker_names_its_job_or_nothing() {
        assert_eq!(parse_marker("mode=snapshot\n"), Some(Scheduled::Snapshot { label: "scheduled".into() }));
        assert_eq!(parse_marker("mode=snapshot\nlabel=pre-v1\n"), Some(Scheduled::Snapshot { label: "pre-v1".into() }));
        assert_eq!(parse_marker("mode=restore\n"), None, "a restore needs a name");
        assert_eq!(parse_marker("mode=dance\n"), None);
        assert_eq!(parse_marker(""), None);
    }
}
