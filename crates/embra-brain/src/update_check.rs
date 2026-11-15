//! The OS's update check as the brain keeps it. One question, asked of the
//! updater and of nobody else: `embra-update check --json`, which fetches
//! the release manifest at the update source, verifies its signature and
//! compares it with the release this machine runs. The brain asks a few
//! minutes after it starts and once a day while the automatic check is on
//! (`/update auto`), and whenever the `update_check` tool is called. It
//! keeps the last answer for `system_status` and `/status`; when a newer
//! release is first seen it tells the operator once, and one line rides the
//! next turn of each session and a resume briefing.
//!
//! Nothing is downloaded or staged here: a release is applied by
//! `/update apply`, the operator's command, which no tool reaches. The
//! updater's command line is fixed (`CHECK_ARGS`): no word of the model's
//! is on it, and the source it asks is the operator's own or the default.
//! Nothing is kept across a restart of the brain: after a boot the answer
//! is that boot's, and a release still ahead is seen, and said, once more.

use std::path::Path;
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant};

use chrono::{DateTime, SecondsFormat, Utc};
use serde::Serialize;
use tokio::sync::Notify;
use tracing::{info, warn};

use crate::grpc_service::{run_updater, update_running, UPDATER};

/// What the brain asks of the updater: this, always, and nothing after it.
pub(crate) const CHECK_ARGS: [&str; 2] = ["check", "--json"];

/// The operator's switch for the automatic check (`/update auto off`): a
/// file on STATE that holds `off`. Without it the check is on.
pub(crate) const AUTO_FILE: &str = "/embra/state/update-auto-check";

/// The first automatic check after the brain starts: the network is up by
/// then and the boot's own work is done.
pub(crate) const FIRST_CHECK_AFTER: Duration = Duration::from_secs(180);
/// How often the loop looks whether a check is due.
pub(crate) const TICK: Duration = Duration::from_secs(300);
/// A check that reached a verdict stands for a day.
const CHECK_EVERY: Duration = Duration::from_secs(24 * 3600);
/// One that failed (no network yet, the source away) is tried again sooner.
const RETRY_AFTER: Duration = Duration::from_secs(3600);
/// The tool asks the source again only for an answer older than this.
const FRESH_FOR: Duration = Duration::from_secs(60);

/// The updater's verdict on the published release against the running one
/// (`embra-update`'s `manifest::Verdict`, by the words `check --json` prints).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Verdict {
    Newer,
    SameBuild,
    SameVersionOtherBuild,
    Older,
    Mismatch,
}

impl Verdict {
    /// The words of the updater's object, one per verdict.
    const WORDS: [(&'static str, Verdict); 5] = [
        ("newer", Verdict::Newer),
        ("same_build", Verdict::SameBuild),
        ("same_version_other_build", Verdict::SameVersionOtherBuild),
        ("older", Verdict::Older),
        ("mismatch", Verdict::Mismatch),
    ];

    fn of(word: &str) -> Option<Verdict> {
        Self::WORDS.iter().find(|(w, _)| *w == word).map(|(_, v)| *v)
    }
}

/// The release the source publishes, as its verified manifest names it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Published {
    pub version: String,
    pub build_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub built: Option<String>,
    /// The release notes' address, when the manifest carries one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notes: Option<String>,
}

/// What one check established: there only for a manifest whose signature
/// the updater verified.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub verdict: Verdict,
    /// Why the release is not this machine's (`Verdict::Mismatch`).
    pub reason: Option<String>,
    pub running_version: String,
    pub running_build: String,
    pub available: Published,
    /// The source asked, without any credentials it carries.
    pub source: String,
}

/// A text without the credentials a URL in it carries before its host
/// (`https://user:secret@host/…`). The updater shows its source this way in
/// its object; the reason of a failed check is the updater's own sentence
/// and its HTTP client's, which name the URL as it was asked. What is kept
/// here reaches the model (`system_status`, the tool) and the brain's log,
/// which the model reads too.
pub(crate) fn without_credentials(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find("://") {
        let (head, tail) = rest.split_at(at + 3);
        out.push_str(head);
        // The authority ends at the first character a host cannot hold.
        let end = tail.find(|c: char| c == '/' || c == '?' || c == '#' || c == ')' || c == '"' || c == '\'' || c.is_whitespace()).unwrap_or(tail.len());
        let authority = &tail[..end];
        out.push_str(authority.rfind('@').map_or(authority, |i| &authority[i + 1..]));
        rest = &tail[end..];
    }
    out.push_str(rest);
    out
}

/// Read a run of `check --json`: the object on standard output is the
/// answer, whatever the exit code beside it (0 a newer release, 3 nothing
/// to do, 2 a release for another machine). A run without the object is a
/// failed check, and its reason is the updater's last stderr line.
pub(crate) fn read_run(code: Option<i32>, stdout: &str, reason: &str) -> Result<Answer, String> {
    let object = stdout
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with('{'))
        .and_then(|line| serde_json::from_str::<serde_json::Value>(line).ok());
    let Some(object) = object else {
        return Err(match (reason, code) {
            ("no reason given", Some(c)) => format!("the updater gave no answer and no reason (exit code {c})"),
            ("no reason given", None) => "the updater gave no answer and no reason".to_string(),
            (said, _) => without_credentials(said),
        });
    };
    let text = |pointer: &str| object.pointer(pointer).and_then(|v| v.as_str()).filter(|s| !s.is_empty()).map(str::to_string);
    let word = text("/verdict").ok_or("the updater's answer names no verdict")?;
    let verdict = Verdict::of(&word).ok_or_else(|| format!("the updater's answer has a verdict this brain does not know: {word}"))?;
    let version = text("/available/version").ok_or("the updater's answer names no published version")?;
    Ok(Answer {
        verdict,
        reason: text("/reason"),
        running_version: text("/running/version").unwrap_or_default(),
        running_build: text("/running/build_id").unwrap_or_default(),
        available: Published {
            version,
            build_id: text("/available/build_id").unwrap_or_default(),
            built: text("/available/built"),
            notes: text("/available/notes_url"),
        },
        source: without_credentials(&text("/source").unwrap_or_default()),
    })
}

/// A newer release as this boot first saw it.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Sighting {
    version: String,
    build_id: String,
    running: String,
    at: DateTime<Utc>,
    /// Whether the operator's notice was handed over.
    noticed: bool,
}

impl Sighting {
    /// The operator's notice, shown once in the console.
    fn notice(&self) -> String {
        format!(
            "embraOS {} is published; this machine runs {}. /update shows it, and /update apply stages it into the inactive slot.",
            self.version, self.running
        )
    }

    /// The line a turn or a briefing carries for the intelligence.
    fn line(&self) -> String {
        format!(
            "embraOS {} is published; this machine runs {}. Only the operator applies it (/update apply).",
            self.version, self.running
        )
    }
}

/// What the brain holds of its checks, this boot.
#[derive(Debug, Clone, Default)]
struct State {
    /// The newest check that reached a verdict, and when.
    answer: Option<(DateTime<Utc>, Answer)>,
    /// The newest check, when it failed: when, and why. A check that
    /// reaches a verdict clears it; the answer before it stays meanwhile.
    failure: Option<(DateTime<Utc>, String)>,
    sighting: Option<Sighting>,
    /// The last check on the monotonic clock, and whether it reached a
    /// verdict: what `due` and the tool's freshness go by.
    attempted: Option<(Instant, bool)>,
}

impl State {
    /// Keep one check's result. A newer release is a sighting when it is
    /// the first of this boot or another release than the one seen; a
    /// verdict that is not `Newer` ends the sighting (the machine caught
    /// up, or the release is gone).
    fn keep(&mut self, now: DateTime<Utc>, at: Instant, result: Result<Answer, String>) {
        self.attempted = Some((at, result.is_ok()));
        match result {
            Ok(answer) => {
                self.failure = None;
                if answer.verdict == Verdict::Newer {
                    let same = self.sighting.as_ref().is_some_and(|s| s.build_id == answer.available.build_id && s.version == answer.available.version);
                    if !same {
                        self.sighting = Some(Sighting {
                            version: answer.available.version.clone(),
                            build_id: answer.available.build_id.clone(),
                            running: answer.running_version.clone(),
                            at: now,
                            noticed: false,
                        });
                    }
                } else {
                    self.sighting = None;
                }
                self.answer = Some((now, answer));
            }
            Err(reason) => self.failure = Some((now, reason)),
        }
    }

    /// The operator's notice for a sighting nobody was told of yet; taken once.
    fn take_notice(&mut self) -> Option<String> {
        let sighting = self.sighting.as_mut().filter(|s| !s.noticed)?;
        sighting.noticed = true;
        Some(sighting.notice())
    }

    /// The line for a turn or a briefing whose cutoff is `since`: there
    /// when the newer release was first seen after it.
    fn seen_since(&self, since: DateTime<Utc>) -> Option<String> {
        self.sighting.as_ref().filter(|s| s.at > since).map(Sighting::line)
    }

    fn block(&self, automatic_check: bool) -> UpdateStatus {
        let stamp = |t: &DateTime<Utc>| t.to_rfc3339_opts(SecondsFormat::Secs, true);
        UpdateStatus {
            automatic_check,
            checked_at: self.answer.as_ref().map(|(at, _)| stamp(at)),
            verdict: self.answer.as_ref().map(|(_, a)| a.verdict),
            available: self.answer.as_ref().map(|(_, a)| a.available.clone()),
            source: self.answer.as_ref().map(|(_, a)| a.source.clone()).filter(|s| !s.is_empty()),
            last_failure: self.failure.as_ref().map(|(at, reason)| Failure { at: stamp(at), reason: reason.clone() }),
            summary: summary(automatic_check, self.answer.as_ref().map(|(_, a)| a), self.failure.as_ref().map(|(_, r)| r.as_str())),
        }
    }
}

/// The check as `system_status`, `/status` and the `update_check` tool
/// report it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct UpdateStatus {
    /// Whether the brain asks by itself, a few minutes after boot and once
    /// a day (`/update auto`).
    pub automatic_check: bool,
    /// When the verdict below was reached; absent before the first check
    /// of this boot that reached one.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checked_at: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verdict: Option<Verdict>,
    /// The release the source publishes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub available: Option<Published>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// The newest check, when it failed; the verdict above is then the
    /// check's before it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_failure: Option<Failure>,
    pub summary: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub(crate) struct Failure {
    pub at: String,
    pub reason: String,
}

/// The block in one or two sentences.
fn summary(automatic_check: bool, answer: Option<&Answer>, failure: Option<&str>) -> String {
    let mut text = match answer {
        Some(a) => match a.verdict {
            Verdict::Newer => format!(
                "embraOS {} is published and this machine runs {}. The operator stages it with /update apply and boots it with /reboot now; nothing is downloaded before that.",
                a.available.version, a.running_version
            ),
            Verdict::SameBuild => format!("This machine runs the published release, {}.", a.available.version),
            Verdict::SameVersionOtherBuild => format!(
                "The published release is {}, the version this machine runs, in another build ({}); nothing newer is published.",
                a.available.version, a.available.build_id
            ),
            Verdict::Older => format!("The published release, {}, is older than the {} this machine runs.", a.available.version, a.running_version),
            Verdict::Mismatch => format!(
                "The release at the update source is not for this machine: {}.",
                a.reason.as_deref().unwrap_or("the updater gave no reason")
            ),
        },
        None => match failure {
            Some(reason) => format!("No check has reached a verdict this boot: {reason}."),
            None if automatic_check => "No check has run yet this boot; the first runs a few minutes after boot.".to_string(),
            None => "No check has run this boot.".to_string(),
        },
    };
    if let (Some(_), Some(reason)) = (answer, failure) {
        text.push_str(&format!(" The newest check reached no verdict: {reason}."));
    }
    if !automatic_check {
        text.push_str(" The automatic check is off (/update auto on turns it on).");
    }
    text
}

/// Whether a check is due: none ran yet, or the last is a day old, or it
/// failed an hour ago.
fn due(now: Instant, attempted: Option<(Instant, bool)>) -> bool {
    match attempted {
        None => true,
        Some((at, reached_a_verdict)) => now.saturating_duration_since(at) >= if reached_a_verdict { CHECK_EVERY } else { RETRY_AFTER },
    }
}

/// Whether the last check is young enough for the tool to answer from it.
fn fresh(now: Instant, attempted: Option<(Instant, bool)>) -> bool {
    attempted.is_some_and(|(at, _)| now.saturating_duration_since(at) < FRESH_FOR)
}

fn automatic_in(file: &Path) -> bool {
    !matches!(std::fs::read_to_string(file), Ok(text) if text.trim() == "off")
}

/// Whether the automatic check is on: off only while the operator's file
/// says so.
pub(crate) fn automatic() -> bool {
    automatic_in(Path::new(AUTO_FILE))
}

/// Turn the automatic check on (the file removed) or off (the file written
/// whole: a temporary file beside it, then a rename).
pub(crate) async fn set_automatic(on: bool) -> Result<(), String> {
    set_automatic_in(Path::new(AUTO_FILE), on).await
}

async fn set_automatic_in(file: &Path, on: bool) -> Result<(), String> {
    if on {
        return match tokio::fs::remove_file(file).await {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{} could not be removed: {e}", file.display())),
        };
    }
    let tmp = file.with_extension("tmp");
    tokio::fs::write(&tmp, "off\n").await.map_err(|e| format!("{} could not be written: {e}", tmp.display()))?;
    tokio::fs::rename(&tmp, file).await.map_err(|e| format!("{} could not be placed: {e}", file.display()))
}

static STATE: RwLock<State> = RwLock::new(State { answer: None, failure: None, sighting: None, attempted: None });
/// One check at a time: the loop's and the tool's never ask the source twice at once.
static RUNNING: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
static WAKE: OnceLock<Notify> = OnceLock::new();

fn wake() -> &'static Notify {
    WAKE.get_or_init(Notify::new)
}

/// Resolves when the loop has something to do before its next tick: a
/// notice to hand over, or a check the operator asked for.
pub(crate) async fn woken() {
    wake().notified().await;
}

/// The last check is forgotten as a check and the loop woken: with the
/// automatic check on, the next one runs now (`/update auto on`, and after
/// the operator's own `/update`, so that the kept answer is the one the
/// operator just saw).
pub(crate) fn check_soon() {
    if let Ok(mut state) = STATE.write() {
        state.attempted = None;
    }
    wake().notify_one();
}

/// Whether this machine has the updater (a development host has none, and
/// reports no block and runs no check).
pub(crate) fn updater_present() -> bool {
    Path::new(UPDATER).exists()
}

/// Run the updater's check and keep its result. The caller holds `RUNNING`.
async fn check_locked() {
    let result = match run_updater(&CHECK_ARGS).await {
        Ok(run) => read_run(run.code, &run.stdout, &run.reason()),
        Err(e) => Err(without_credentials(&e)),
    };
    match &result {
        Ok(answer) => info!(
            target: "update_check",
            verdict = ?answer.verdict,
            published = %answer.available.version,
            build = %answer.available.build_id,
            running = %answer.running_version,
            "update check"
        ),
        Err(reason) => warn!(target: "update_check", "the update check reached no verdict: {reason}"),
    }
    if let Ok(mut state) = STATE.write() {
        state.keep(Utc::now(), Instant::now(), result);
    }
}

/// One tick of the loop: the check, when the automatic check is on, one is
/// due, the updater is there and no release is being staged.
pub(crate) async fn tick() {
    if !updater_present() || !automatic() || update_running() {
        return;
    }
    let _one = RUNNING.lock().await;
    let attempted = STATE.read().ok().and_then(|s| s.attempted);
    if due(Instant::now(), attempted) {
        check_locked().await;
    }
}

/// The `update_check` tool: the check now, unless one ran within the last
/// minute, then the block. The automatic check's switch does not bind it:
/// the intelligence was asked.
pub(crate) async fn check_for_tool() -> UpdateStatus {
    {
        let _one = RUNNING.lock().await;
        let attempted = STATE.read().ok().and_then(|s| s.attempted);
        if !fresh(Instant::now(), attempted) {
            check_locked().await;
        }
    }
    // The loop hands over the notice of a release this check saw first.
    wake().notify_one();
    status()
}

/// The block as it stands.
pub(crate) fn status() -> UpdateStatus {
    let automatic_check = automatic();
    match STATE.read() {
        Ok(state) => state.block(automatic_check),
        Err(_) => State::default().block(automatic_check),
    }
}

/// The block for `system_status`: there on a machine that has the updater.
pub(crate) fn status_for_report() -> Option<UpdateStatus> {
    updater_present().then(status)
}

/// The operator's notice for a newer release nobody was told of yet.
pub(crate) fn take_notice() -> Option<String> {
    STATE.write().ok().and_then(|mut state| state.take_notice())
}

/// The line a turn or a briefing carries when a newer release was first
/// seen after `since`.
pub(crate) fn seen_since(since: DateTime<Utc>) -> Option<String> {
    STATE.read().ok().and_then(|state| state.seen_since(since))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn object(verdict: &str, running: &str, published: &str, build: &str) -> String {
        serde_json::json!({
            "verdict": verdict,
            "reason": serde_json::Value::Null,
            "running": {"version": running, "build_id": "run0", "arch": "x86_64", "engine": "fjall"},
            "available": {"version": published, "build_id": build, "built": "2026-11-20T10:00:00Z", "notes_url": "https://example/notes"},
            "source": "https://github.com/Ward-Software-Defined-Systems/embraOS/releases/latest/download",
        })
        .to_string()
    }

    fn newer(published: &str, build: &str) -> Answer {
        read_run(Some(0), &object("newer", "0.24.0-phase2", published, build), "no reason given").unwrap()
    }

    fn t(text: &str) -> DateTime<Utc> {
        DateTime::parse_from_rfc3339(text).unwrap().with_timezone(&Utc)
    }

    #[test]
    fn the_updaters_object_is_read_for_every_verdict_and_a_run_without_one_is_a_failed_check() {
        let answer = newer("0.25.0-phase2", "bbb");
        assert_eq!(answer.verdict, Verdict::Newer);
        assert_eq!((answer.running_version.as_str(), answer.running_build.as_str()), ("0.24.0-phase2", "run0"));
        assert_eq!(
            answer.available,
            Published { version: "0.25.0-phase2".into(), build_id: "bbb".into(), built: Some("2026-11-20T10:00:00Z".into()), notes: Some("https://example/notes".into()) }
        );
        assert!(answer.source.ends_with("/releases/latest/download"));
        for (word, code, verdict) in [
            ("same_build", 3, Verdict::SameBuild),
            ("same_version_other_build", 3, Verdict::SameVersionOtherBuild),
            ("older", 3, Verdict::Older),
            ("mismatch", 2, Verdict::Mismatch),
        ] {
            assert_eq!(read_run(Some(code), &object(word, "0.24.0-phase2", "0.24.0-phase2", "bbb"), "refused: x").unwrap().verdict, verdict, "{word}");
        }
        // A release for another machine carries its reason.
        let other = object("mismatch", "0.24.0-phase2", "0.25.0-phase2", "bbb").replace("\"reason\":null", "\"reason\":\"the manifest is for aarch64, this machine is x86_64\"");
        assert_eq!(read_run(Some(2), &other, "refused: x").unwrap().reason.as_deref(), Some("the manifest is for aarch64, this machine is x86_64"));

        // No object: the updater's reason is the check's.
        let failed = read_run(Some(2), "", "update check failed: error sending request; no manifest for x86_64-fjall is published at this source");
        assert_eq!(failed.unwrap_err(), "update check failed: error sending request; no manifest for x86_64-fjall is published at this source");
        assert_eq!(read_run(Some(1), "source: x\n", "no reason given").unwrap_err(), "the updater gave no answer and no reason (exit code 1)");
        assert!(read_run(Some(0), "{\"verdict\":\"sideways\",\"available\":{\"version\":\"1.0.0\"}}", "x").unwrap_err().contains("does not know: sideways"));
        assert!(read_run(Some(0), "{\"verdict\":\"newer\"}", "x").unwrap_err().contains("no published version"));
        assert!(read_run(Some(0), "{\"available\":{\"version\":\"1.0.0\"}}", "x").unwrap_err().contains("no verdict"));
    }

    /// The words and the fields are the updater's: `check_json` in
    /// `embra-update/src/update.rs` writes each of them, and its command
    /// line takes the flag this brain passes.
    #[test]
    fn the_object_read_here_is_the_one_the_updater_writes() {
        let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../embra-update/src");
        let update = std::fs::read_to_string(dir.join("update.rs")).expect("the updater's check");
        for (word, _) in Verdict::WORDS {
            assert!(update.contains(&format!("(\"{word}\", ")), "the updater has no verdict word {word}");
        }
        for key in ["\"verdict\": verdict", "\"reason\": reason", "\"running\": {", "\"available\": {", "\"version\": checked.manifest.version", "\"build_id\": checked.manifest.build_id", "\"built\": checked.manifest.built", "\"notes_url\": checked.manifest.notes_url", "\"version\": checked.release.version", "\"build_id\": checked.release.build_id", "\"source\": checked.source.shown()"] {
            assert!(update.contains(key), "the updater's object has no {key}");
        }
        let cli = std::fs::read_to_string(dir.join("cli.rs")).expect("the updater's command line");
        assert_eq!(CHECK_ARGS, ["check", "--json"]);
        assert!(cli.contains("\"--json\" => json = true") && cli.contains("\"check\" => Ok(Command::Check { source, tag, json })"), "check --json is not the updater's any more");
    }

    #[test]
    fn a_check_is_due_at_once_a_day_after_an_answer_and_an_hour_after_a_failure() {
        let start = Instant::now();
        let later = |secs: u64| start + Duration::from_secs(secs);
        assert!(due(start, None), "no check yet");
        assert!(!due(later(3600), Some((start, true))), "an answer stands for a day");
        assert!(!due(later(24 * 3600 - 1), Some((start, true))));
        assert!(due(later(24 * 3600), Some((start, true))));
        assert!(!due(later(3599), Some((start, false))), "a failure waits an hour");
        assert!(due(later(3600), Some((start, false))));
        // The tool answers from a check under a minute old and asks again after.
        assert!(!fresh(start, None));
        assert!(fresh(later(59), Some((start, false))));
        assert!(!fresh(later(60), Some((start, true))));
    }

    #[test]
    fn a_newer_release_is_seen_once_and_another_release_is_seen_anew() {
        let mut state = State::default();
        let start = Instant::now();
        state.keep(t("2026-11-20T10:00:00Z"), start, Ok(newer("0.25.0-phase2", "bbb")));
        let first = state.sighting.clone().unwrap();
        assert_eq!((first.version.as_str(), first.at, first.noticed), ("0.25.0-phase2", t("2026-11-20T10:00:00Z"), false));
        // The operator is told once.
        let notice = state.take_notice().unwrap();
        assert!(notice.contains("embraOS 0.25.0-phase2 is published") && notice.contains("runs 0.24.0-phase2") && notice.contains("/update apply"), "{notice}");
        assert_eq!(state.take_notice(), None);
        // The same release a day later is no new sighting.
        state.keep(t("2026-11-21T10:00:00Z"), start, Ok(newer("0.25.0-phase2", "bbb")));
        assert_eq!(state.sighting.as_ref().unwrap().at, t("2026-11-20T10:00:00Z"));
        assert_eq!(state.take_notice(), None);
        // A failed check changes nothing of it.
        state.keep(t("2026-11-21T11:00:00Z"), start, Err("the source is away".into()));
        assert!(state.sighting.is_some() && state.failure.is_some());
        // Another release is seen anew, and said.
        state.keep(t("2026-11-22T10:00:00Z"), start, Ok(newer("0.26.0-phase2", "ccc")));
        assert_eq!(state.sighting.as_ref().unwrap().at, t("2026-11-22T10:00:00Z"));
        assert!(state.failure.is_none(), "a verdict clears the failure");
        assert!(state.take_notice().unwrap().contains("0.26.0-phase2"));
        // The machine caught up: nothing is ahead any more.
        let same = read_run(Some(3), &object("same_build", "0.26.0-phase2", "0.26.0-phase2", "ccc"), "x").unwrap();
        state.keep(t("2026-11-23T10:00:00Z"), start, Ok(same));
        assert_eq!(state.sighting, None);
        assert_eq!(state.take_notice(), None);
    }

    #[test]
    fn a_release_seen_after_the_cutoff_is_one_line_and_one_seen_before_it_is_none() {
        let mut state = State::default();
        assert_eq!(state.seen_since(t("2026-11-01T00:00:00Z")), None);
        state.keep(t("2026-11-20T10:00:00Z"), Instant::now(), Ok(newer("0.25.0-phase2", "bbb")));
        assert_eq!(
            state.seen_since(t("2026-11-20T09:59:59Z")).as_deref(),
            Some("embraOS 0.25.0-phase2 is published; this machine runs 0.24.0-phase2. Only the operator applies it (/update apply).")
        );
        assert_eq!(state.seen_since(t("2026-11-20T10:00:00Z")), None, "a turn that started at the sighting has it already");
        assert_eq!(state.seen_since(t("2026-11-20T12:00:00Z")), None);
    }

    #[test]
    fn the_block_says_the_verdict_the_release_and_when_and_keeps_the_last_answer_through_a_failure() {
        let mut state = State::default();
        let none = serde_json::to_value(state.block(true)).unwrap();
        assert_eq!(none, serde_json::json!({"automatic_check": true, "summary": "No check has run yet this boot; the first runs a few minutes after boot."}));
        assert_eq!(state.block(false).summary, "No check has run this boot. The automatic check is off (/update auto on turns it on).");

        state.keep(t("2026-11-20T10:00:00Z"), Instant::now(), Err("update check failed: no route to host".into()));
        let failed = state.block(true);
        assert_eq!((failed.verdict, failed.checked_at.clone()), (None, None));
        assert_eq!(failed.last_failure, Some(Failure { at: "2026-11-20T10:00:00Z".into(), reason: "update check failed: no route to host".into() }));
        assert_eq!(failed.summary, "No check has reached a verdict this boot: update check failed: no route to host.");

        state.keep(t("2026-11-20T11:00:00Z"), Instant::now(), Ok(newer("0.25.0-phase2", "bbb")));
        let block = serde_json::to_value(state.block(true)).unwrap();
        assert_eq!(block["checked_at"], "2026-11-20T11:00:00Z");
        assert_eq!(block["verdict"], "newer");
        assert_eq!(block["available"], serde_json::json!({"version": "0.25.0-phase2", "build_id": "bbb", "built": "2026-11-20T10:00:00Z", "notes": "https://example/notes"}));
        assert!(block["source"].as_str().unwrap().starts_with("https://github.com/"));
        assert!(block.get("last_failure").is_none(), "a verdict clears the failure");
        assert_eq!(
            block["summary"],
            "embraOS 0.25.0-phase2 is published and this machine runs 0.24.0-phase2. The operator stages it with /update apply and boots it with /reboot now; nothing is downloaded before that."
        );

        // The next check fails: the verdict of the check before it stays, and the failure is said.
        state.keep(t("2026-11-21T11:00:00Z"), Instant::now(), Err("the source is away".into()));
        let after = state.block(false);
        assert_eq!((after.verdict, after.checked_at.as_deref()), (Some(Verdict::Newer), Some("2026-11-20T11:00:00Z")));
        assert_eq!(after.last_failure.as_ref().map(|f| f.at.as_str()), Some("2026-11-21T11:00:00Z"));
        assert!(after.summary.ends_with("The newest check reached no verdict: the source is away. The automatic check is off (/update auto on turns it on)."), "{}", after.summary);
    }

    /// An operator's own source may carry credentials before its host. The
    /// updater's reason for a failed check names the URL it asked, twice
    /// over (its own sentence and its HTTP client's): none of it is kept.
    #[test]
    fn a_failed_checks_reason_carries_no_credentials_of_the_source() {
        let said = "update check failed: GET https://deploy:s3cret@mirror.example:8443/embraos/embraos-x86_64-fjall.manifest.json: error sending request for url (https://deploy:s3cret@mirror.example:8443/embraos/embraos-x86_64-fjall.manifest.json): client error (Connect); no manifest for x86_64-fjall is published at this source";
        let reason = read_run(Some(2), "", said).unwrap_err();
        assert!(!reason.contains("s3cret") && !reason.contains("deploy"), "{reason}");
        assert_eq!(reason, said.replace("deploy:s3cret@", ""));
        // What it leaves alone: a URL without credentials, an at sign after the host, text without a URL.
        for same in ["GET https://github.com/a/b: 404", "https://mirror.example/a@b/c?d=e@f", "no route to host", "", "a://"] {
            assert_eq!(without_credentials(same), same);
        }
        assert_eq!(without_credentials("http://token@10.0.0.5:8080/u and (https://u:p@h.example)"), "http://10.0.0.5:8080/u and (https://h.example)");
        // The source in the object is scrubbed again here, whatever the updater did.
        let object = object("newer", "0.24.0-phase2", "0.25.0-phase2", "bbb").replace("https://github.com/", "https://u:p@github.com/");
        assert_eq!(read_run(Some(0), &object, "x").unwrap().source, "https://github.com/Ward-Software-Defined-Systems/embraOS/releases/latest/download");
    }

    #[test]
    fn each_verdict_has_its_sentence() {
        let said = |word: &str, code: i32, running: &str, published: &str| {
            let answer = read_run(Some(code), &object(word, running, published, "bbb"), "x").unwrap();
            summary(true, Some(&answer), None)
        };
        assert_eq!(said("same_build", 3, "0.25.0-phase2", "0.25.0-phase2"), "This machine runs the published release, 0.25.0-phase2.");
        assert_eq!(
            said("same_version_other_build", 3, "0.25.0-phase2", "0.25.0-phase2"),
            "The published release is 0.25.0-phase2, the version this machine runs, in another build (bbb); nothing newer is published."
        );
        assert_eq!(said("older", 3, "0.26.0-phase2", "0.25.0-phase2"), "The published release, 0.25.0-phase2, is older than the 0.26.0-phase2 this machine runs.");
        let other = object("mismatch", "0.24.0-phase2", "0.25.0-phase2", "bbb").replace("\"reason\":null", "\"reason\":\"the manifest is for aarch64, this machine is x86_64\"");
        assert_eq!(
            summary(true, Some(&read_run(Some(2), &other, "x").unwrap()), None),
            "The release at the update source is not for this machine: the manifest is for aarch64, this machine is x86_64."
        );
    }

    #[tokio::test]
    async fn the_automatic_check_is_on_unless_the_operators_file_says_off() {
        let dir = std::env::temp_dir().join(format!("embra-update-auto-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("update-auto-check");
        assert!(automatic_in(&file), "no file: on");
        set_automatic_in(&file, false).await.unwrap();
        assert_eq!(std::fs::read_to_string(&file).unwrap(), "off\n");
        assert!(!automatic_in(&file));
        assert!(!dir.join("update-auto-check.tmp").exists(), "the temporary file is gone");
        set_automatic_in(&file, true).await.unwrap();
        assert!(!file.exists() && automatic_in(&file));
        set_automatic_in(&file, true).await.unwrap(); // on twice is on
        // Any other content is not the switch.
        std::fs::write(&file, "maybe\n").unwrap();
        assert!(automatic_in(&file));
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
