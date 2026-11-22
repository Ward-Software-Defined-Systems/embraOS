//! The jobs on a native model's file (experimental): an install (a download
//! and its check) and a removal. Each is a run of the OS's updater
//! (`embra-update model-install`, `model-remove <id>`), which does the work,
//! holds the models' lock and says how far it is, a line every five per
//! cent.
//!
//! One job at a time, kept in one place for the whole brain
//! ([`Jobs::shared`]). The console that started a job is sent its lines; any
//! other console can ask what runs and how far it is, or follow it too; and
//! a job goes on when the console that started it leaves. A stop ends the
//! updater: what a download had is kept, and the next one goes on from it.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::process::Stdio;
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};

use embra_boot::native_model::NativeModel;
use tokio::io::{AsyncBufReadExt, AsyncReadExt};
use tokio::sync::{Notify, broadcast};

use super::Env;

/// The lines of a job that are kept while it runs: its latest is how far it
/// is, and the last of a job that had nothing to do is what it found.
const TAIL: usize = 8;
/// The lines a console that follows a job may be behind by. A job says
/// about thirty.
const FEED: usize = 256;

/// What a job does to a model's file.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum JobKind {
    /// Download a model's file and check it; a file that is there is
    /// checked, and a download that stopped goes on.
    Install,
    Remove,
}

impl JobKind {
    /// The updater's command for it.
    pub(crate) fn command(self) -> &'static str {
        match self {
            JobKind::Install => "model-install",
            JobKind::Remove => "model-remove",
        }
    }

    /// What the operator calls it.
    pub(crate) fn noun(self) -> &'static str {
        match self {
            JobKind::Install => "download",
            JobKind::Remove => "removal",
        }
    }
}

/// How a job ended, by the updater's exit.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum JobEnd {
    /// The updater did it (exit 0).
    Done,
    /// There was nothing to do (exit 3): its last line.
    Nothing(String),
    /// A precondition stopped it and nothing was changed (exit 2): its
    /// reason.
    Refused(String),
    /// It failed on the way (any other exit, a signal, or an updater that
    /// could not be run): the reason.
    Failed(String),
    /// The operator stopped it.
    Stopped,
}

/// A job that runs.
#[derive(Clone, Debug)]
pub(crate) struct Running {
    pub(crate) kind: JobKind,
    pub(crate) model: &'static NativeModel,
    /// The updater's latest line: how far the job is.
    pub(crate) last_line: Option<String>,
}

/// The job that ran last.
#[derive(Clone, Debug)]
pub(crate) struct Finished {
    pub(crate) kind: JobKind,
    pub(crate) model: &'static NativeModel,
    pub(crate) end: JobEnd,
}

/// What a console that follows a job is sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum JobEvent {
    Line(String),
    Ended(JobEnd),
}

/// The job that runs, with what ends it and what follows it.
struct Slot {
    running: Running,
    stop: Arc<Notify>,
    #[cfg_attr(not(test), expect(dead_code, reason = "read by `follow`, which the wizard calls (its own commit)"))]
    feed: broadcast::Sender<JobEvent>,
}

#[derive(Default)]
struct Inner {
    slot: Option<Slot>,
    tail: VecDeque<String>,
    last: Option<Finished>,
}

/// The models' jobs of one brain.
#[derive(Clone, Default)]
pub(crate) struct Jobs(Arc<Mutex<Inner>>);

impl Jobs {
    /// The brain's own: every console asks the same one.
    pub(crate) fn shared() -> &'static Jobs {
        static SHARED: OnceLock<Jobs> = OnceLock::new();
        SHARED.get_or_init(Jobs::default)
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Start a job and follow it. `Err` is the job that runs already: one
    /// at a time.
    pub(crate) fn start(&self, env: &Env, kind: JobKind, model: &'static NativeModel) -> Result<broadcast::Receiver<JobEvent>, Running> {
        let (feed, events) = broadcast::channel(FEED);
        let stop = Arc::new(Notify::new());
        {
            let mut inner = self.lock();
            if let Some(slot) = &inner.slot {
                return Err(slot.running.clone());
            }
            inner.tail.clear();
            let running = Running { kind, model, last_line: None };
            inner.slot = Some(Slot { running, stop: stop.clone(), feed: feed.clone() });
        }
        let (jobs, updater) = (self.clone(), env.updater.clone());
        tokio::spawn(async move {
            let end = run(&jobs, &updater, kind, model, &stop, &feed).await;
            // The record first: who hears of the end and asks finds no job.
            jobs.ended(kind, model, end.clone());
            let _ = feed.send(JobEvent::Ended(end));
        });
        Ok(events)
    }

    /// The job that runs, with its latest line.
    pub(crate) fn running(&self) -> Option<Running> {
        self.lock().slot.as_ref().map(|slot| slot.running.clone())
    }

    /// The job that ran last, and how it ended.
    pub(crate) fn last(&self) -> Option<Finished> {
        self.lock().last.clone()
    }

    /// Follow the job that runs, from its next line on.
    #[cfg_attr(not(test), expect(dead_code, reason = "the wizard follows a download that runs when a console comes back to it (its own commit)"))]
    pub(crate) fn follow(&self) -> Option<broadcast::Receiver<JobEvent>> {
        self.lock().slot.as_ref().map(|slot| slot.feed.subscribe())
    }

    /// Ask the job that runs to end; `None` when none runs. Its end
    /// (`Stopped`) comes to those who follow it.
    pub(crate) fn stop(&self) -> Option<Running> {
        self.lock().slot.as_ref().map(|slot| {
            slot.stop.notify_one();
            slot.running.clone()
        })
    }

    fn said(&self, line: &str) {
        let mut inner = self.lock();
        if let Some(slot) = &mut inner.slot {
            slot.running.last_line = Some(line.to_string());
        }
        if inner.tail.len() == TAIL {
            inner.tail.pop_front();
        }
        inner.tail.push_back(line.to_string());
    }

    fn latest(&self) -> Option<String> {
        self.lock().tail.back().cloned()
    }

    fn ended(&self, kind: JobKind, model: &'static NativeModel, end: JobEnd) {
        let mut inner = self.lock();
        inner.slot = None;
        inner.last = Some(Finished { kind, model, end });
    }
}

/// How long the updater is given to list the models.
const LIST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(60);

/// The updater's own list: a line a model of the catalogue with how its
/// file stands on this machine, then DATA's free space (`model-list`).
pub(crate) async fn list(env: &Env) -> Result<String, String> {
    let Some((program, leading)) = env.updater.split_first() else {
        return Err("no updater is named".to_string());
    };
    let mut cmd = crate::tools::subprocess(program);
    cmd.args(leading).arg("model-list").stdin(Stdio::null());
    let out = tokio::time::timeout(LIST_TIMEOUT, cmd.output())
        .await
        .map_err(|_| format!("the updater did not answer within {} s", LIST_TIMEOUT.as_secs()))?
        .map_err(|e| format!("the updater could not be run ({e})"))?;
    if out.status.success() {
        return Ok(String::from_utf8_lossy(&out.stdout).trim().to_string());
    }
    let errors = String::from_utf8_lossy(&out.stderr);
    Err(errors.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim().to_string())
}

/// Run the updater for one job, to its end or to a stop.
async fn run(jobs: &Jobs, updater: &[OsString], kind: JobKind, model: &'static NativeModel, stop: &Notify, feed: &broadcast::Sender<JobEvent>) -> JobEnd {
    let Some((program, leading)) = updater.split_first() else {
        return JobEnd::Failed("no updater is named".to_string());
    };
    let mut cmd = crate::tools::subprocess(program);
    cmd.args(leading).args([kind.command(), model.id]).stdin(Stdio::null()).stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = match cmd.spawn() {
        Ok(child) => child,
        Err(e) => return JobEnd::Failed(format!("the updater could not be started: {e}")),
    };
    let (stdout, stderr) = (child.stdout.take(), child.stderr.take());
    let lines = async {
        if let Some(out) = stdout {
            let mut lines = tokio::io::BufReader::new(out).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let line = line.trim();
                if !line.is_empty() {
                    jobs.said(line);
                    // Nobody may be following: the line is kept all the same.
                    let _ = feed.send(JobEvent::Line(line.to_string()));
                }
            }
        }
    };
    let errors = async {
        let mut text = String::new();
        if let Some(mut err) = stderr {
            let _ = err.read_to_string(&mut text).await;
        }
        text
    };
    let said = async { tokio::join!(lines, errors).1 };
    tokio::pin!(said);
    let errors = tokio::select! {
        errors = &mut said => errors,
        () = stop.notified() => {
            // The updater is ended and waited for. It needs no time to tidy:
            // the kernel frees its lock, and what a download had stays for
            // the next one.
            let _ = child.kill().await;
            return JobEnd::Stopped;
        }
    };
    let reason = errors.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim().to_string();
    match child.wait().await {
        Ok(status) => match status.code() {
            Some(0) => JobEnd::Done,
            Some(3) => JobEnd::Nothing(jobs.latest().unwrap_or_else(|| "nothing to do".to_string())),
            Some(2) => JobEnd::Refused(reason),
            Some(_) => JobEnd::Failed(reason),
            None => JobEnd::Failed(format!("the updater was ended by a signal ({reason})")),
        },
        Err(e) => JobEnd::Failed(format!("the updater could not be waited for: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::test_support::machine;
    use super::*;
    use embra_boot::native_model::by_id;
    use std::path::Path;
    use std::time::Duration;

    fn four() -> &'static NativeModel {
        by_id("qwen3.5-4b").unwrap()
    }

    /// A stand-in for the updater: a shell script, run as `/bin/sh <script>`
    /// so that no test executes a file it has just written.
    fn stand_in(root: &Path, body: &str) -> Vec<OsString> {
        let script = root.join("updater.sh");
        std::fs::write(&script, format!("{body}\n")).unwrap();
        vec![OsString::from("/bin/sh"), script.into_os_string()]
    }

    /// The next thing a job says. Two minutes is a deadline no loaded
    /// machine reaches when the job works: without it, a job that never
    /// spoke would hold the suite.
    async fn next(events: &mut broadcast::Receiver<JobEvent>) -> JobEvent {
        tokio::time::timeout(Duration::from_secs(120), events.recv())
            .await
            .expect("the job says something")
            .expect("the feed is open")
    }

    async fn end_of(events: &mut broadcast::Receiver<JobEvent>) -> JobEnd {
        loop {
            if let JobEvent::Ended(end) = next(events).await {
                return end;
            }
        }
    }

    /// The updater is run with the job's command and the model's id; its
    /// lines reach the console that started it and any that follows later;
    /// a console that only asks sees how far the job is; a second job is
    /// refused while one runs; and the end is kept for who asks after it.
    #[tokio::test]
    async fn a_job_runs_the_updater_and_every_console_can_ask_how_far_it_is() {
        let (root, mut env) = machine("job-progress");
        let (args, gate) = (root.join("args"), root.join("go-on"));
        env.updater = stand_in(
            &root,
            &format!(
                "echo \"$1 $2\" > '{}'\n\
                 echo 'qwen3.5-4b: downloading Qwen3.5-4B-Q4_K_M.gguf (2613 MB) from https://models.example/'\n\
                 echo 'qwen3.5-4b: received 131 of 2613 MB (5%)'\n\
                 until [ -e '{}' ]; do sleep 0.02; done\n\
                 echo 'qwen3.5-4b: installed and verified (2613 MB, sha256 00fe7986ff5f6b46…)'",
                args.display(),
                gate.display()
            ),
        );
        let jobs = Jobs::default();
        assert!(jobs.running().is_none() && jobs.last().is_none() && jobs.follow().is_none() && jobs.stop().is_none());

        let mut first = jobs.start(&env, JobKind::Install, four()).expect("no job runs");
        assert!(matches!(next(&mut first).await, JobEvent::Line(l) if l.starts_with("qwen3.5-4b: downloading ")));
        assert_eq!(next(&mut first).await, JobEvent::Line("qwen3.5-4b: received 131 of 2613 MB (5%)".into()));

        // Another console asks: what runs, and how far it is.
        let seen = jobs.running().expect("the job runs");
        assert_eq!((seen.kind, seen.model.id, seen.last_line.as_deref()), (JobKind::Install, "qwen3.5-4b", Some("qwen3.5-4b: received 131 of 2613 MB (5%)")));
        // It follows the job from here on.
        let mut second = jobs.follow().expect("a job to follow");
        // One job at a time: a second one is refused with the one that runs.
        let refused = jobs.start(&env, JobKind::Remove, by_id("qwen3.5-9b").unwrap()).expect_err("a job runs");
        assert_eq!((refused.kind, refused.model.id), (JobKind::Install, "qwen3.5-4b"));

        std::fs::write(&gate, "").unwrap();
        for events in [&mut first, &mut second] {
            assert!(matches!(next(events).await, JobEvent::Line(l) if l.starts_with("qwen3.5-4b: installed and verified ")));
            assert_eq!(next(events).await, JobEvent::Ended(JobEnd::Done));
        }
        assert!(jobs.running().is_none() && jobs.follow().is_none(), "the job is over for who hears of its end");
        let last = jobs.last().expect("the job that ran");
        assert_eq!((last.kind, last.model.id, &last.end), (JobKind::Install, "qwen3.5-4b", &JobEnd::Done));
        assert_eq!(std::fs::read_to_string(&args).unwrap().trim(), "model-install qwen3.5-4b");

        // And the next job starts: a removal, by its own command.
        let mut removal = jobs.start(&env, JobKind::Remove, four()).expect("the job before it ended");
        assert_eq!(end_of(&mut removal).await, JobEnd::Done);
        assert_eq!(std::fs::read_to_string(&args).unwrap().trim(), "model-remove qwen3.5-4b");
        assert_eq!(jobs.last().unwrap().kind, JobKind::Remove);
    }

    /// A job goes on when nobody follows it: the console that started it
    /// left. Whoever asks later finds how it ended.
    #[tokio::test]
    async fn a_job_goes_on_when_the_console_that_started_it_leaves() {
        let (root, mut env) = machine("job-left");
        let gate = root.join("go-on");
        env.updater = stand_in(
            &root,
            &format!("echo 'qwen3.5-4b: received 131 of 2613 MB (5%)'\nuntil [ -e '{}' ]; do sleep 0.02; done\necho 'qwen3.5-4b: installed and verified'", gate.display()),
        );
        let jobs = Jobs::default();
        let mut events = jobs.start(&env, JobKind::Install, four()).unwrap();
        assert!(matches!(next(&mut events).await, JobEvent::Line(_)));
        drop(events);
        std::fs::write(&gate, "").unwrap();
        // Nobody followed it to its end. A console that comes now follows it
        // from here, or finds it over already; either way its end is kept.
        if let Some(mut events) = jobs.follow() {
            assert_eq!(end_of(&mut events).await, JobEnd::Done);
        }
        let last = jobs.last().expect("the job ended");
        assert_eq!(last.end, JobEnd::Done);
    }

    /// A stop ends the updater itself, not only the wait for it.
    #[tokio::test]
    async fn a_stopped_job_ends_the_updater_and_says_that_it_was_stopped() {
        let (root, mut env) = machine("job-stop");
        let pid_file = root.join("pid");
        env.updater = stand_in(&root, &format!("echo $$ > '{}'\necho 'qwen3.5-4b: received 131 of 2613 MB (5%)'\nwhile :; do sleep 0.02; done", pid_file.display()));
        let jobs = Jobs::default();
        let mut events = jobs.start(&env, JobKind::Install, four()).unwrap();
        assert!(matches!(next(&mut events).await, JobEvent::Line(_)));
        let pid: u32 = std::fs::read_to_string(&pid_file).unwrap().trim().parse().unwrap();
        assert!(Path::new(&format!("/proc/{pid}")).exists(), "the stand-in runs");

        let stopped = jobs.stop().expect("a job to stop");
        assert_eq!((stopped.kind, stopped.model.id), (JobKind::Install, "qwen3.5-4b"));
        assert_eq!(end_of(&mut events).await, JobEnd::Stopped);
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "the updater was ended and waited for");
        assert_eq!(jobs.last().unwrap().end, JobEnd::Stopped);
        assert!(jobs.running().is_none() && jobs.stop().is_none());
    }

    #[tokio::test]
    async fn a_job_ends_as_the_updater_exits() {
        let (root, mut env) = machine("job-ends");
        let jobs = Jobs::default();
        for (body, expected) in [
            ("echo 'qwen3.5-4b: removed, 2613 MB freed'", JobEnd::Done),
            (
                "echo 'refused: another model job is running (an install, an import, a check or a removal); it has to end first' >&2; exit 2",
                JobEnd::Refused("refused: another model job is running (an install, an import, a check or a removal); it has to end first".into()),
            ),
            ("echo 'qwen3.5-4b: checking'; echo 'qwen3.5-4b: nothing to do'; exit 3", JobEnd::Nothing("qwen3.5-4b: nothing to do".into())),
            ("exit 3", JobEnd::Nothing("nothing to do".into())),
            (
                "echo 'a line before the reason' >&2; echo 'the download has the size of the file and its sha256 is another; nothing of it is kept' >&2; echo >&2; exit 1",
                JobEnd::Failed("the download has the size of the file and its sha256 is another; nothing of it is kept".into()),
            ),
            ("exit 1", JobEnd::Failed("no reason given".into())),
            ("kill -9 $$", JobEnd::Failed("the updater was ended by a signal (no reason given)".into())),
        ] {
            env.updater = stand_in(&root, body);
            let mut events = jobs.start(&env, JobKind::Remove, four()).expect("the job before it ended");
            assert_eq!(end_of(&mut events).await, expected, "{body}");
        }
        // No updater on this machine: said, and the next job can start.
        env.updater = vec![OsString::from(root.join("no-such-updater"))];
        let mut events = jobs.start(&env, JobKind::Install, four()).unwrap();
        let JobEnd::Failed(why) = end_of(&mut events).await else { panic!("an updater that is not there") };
        assert!(why.starts_with("the updater could not be started: "), "{why}");
        env.updater = Vec::new();
        let mut events = jobs.start(&env, JobKind::Install, four()).unwrap();
        assert_eq!(end_of(&mut events).await, JobEnd::Failed("no updater is named".into()));
    }

    #[tokio::test]
    async fn the_list_is_the_updaters_own() {
        let (root, mut env) = machine("job-list");
        env.updater = stand_in(&root, "echo \"asked: $1\"\necho 'qwen3.5-4b  Qwen3.5 4B (Q4_K_M, 2613 MB, Apache-2.0): installed'\necho 'DATA has 1840 MB free'");
        assert_eq!(
            list(&env).await.as_deref(),
            Ok("asked: model-list\nqwen3.5-4b  Qwen3.5 4B (Q4_K_M, 2613 MB, Apache-2.0): installed\nDATA has 1840 MB free")
        );
        env.updater = stand_in(&root, "echo 'DATA could not be read' >&2; exit 1");
        assert_eq!(list(&env).await, Err("DATA could not be read".to_string()));
        env.updater = vec![OsString::from(root.join("no-such-updater"))];
        assert!(list(&env).await.unwrap_err().starts_with("the updater could not be run ("));
        env.updater = Vec::new();
        assert_eq!(list(&env).await, Err("no updater is named".to_string()));
    }

    #[test]
    fn a_job_names_the_updaters_command_and_the_operators_word() {
        let named: Vec<_> = [JobKind::Install, JobKind::Remove].into_iter().map(|kind| (kind.command(), kind.noun())).collect();
        assert_eq!(named, [("model-install", "download"), ("model-remove", "removal")]);
        // The brain's shared registry is one.
        assert!(std::ptr::eq(Jobs::shared(), Jobs::shared()));
    }
}
