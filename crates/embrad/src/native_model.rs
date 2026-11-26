//! The keeper of the model server (native models, experimental).
//!
//! One `llama-server` runs while the settings on STATE name a model
//! (`embra_boot::native_model`): on loopback alone, with the model's file and
//! the window the settings give it. The brain writes the settings (`/native
//! use`, `/native off`); the keeper follows them at every tick of the
//! reconcile loop and writes what it sees to the status under `/run`, which
//! the brain reads.
//!
//! The model server is no service of the supervisor's. A service is started
//! once and awaited until it answers, and the loop stands still meanwhile; a
//! model's load takes as long as its file is large and its disk is slow, and
//! a model server that ends must never be a reason to stop the machine. So
//! the keeper never waits for it: one tick starts the server, a later tick
//! finds it answering. A load is never ended for being slow.
//!
//! - A start needs the model's file verified (`native_model::installed`: the
//!   record the updater wrote still describes the file). Nothing is hashed
//!   here.
//! - The model's image file (its projector) is given to the server when it
//!   is verified the same way: the model then reads images, and the status
//!   says so. Without it the model is started all the same and reads text.
//!   Whether the file is there is part of what the keeper follows: when it
//!   arrives, or goes, the server is ended and started again, as for other
//!   settings.
//! - A server that ends before it answered is started again at the next
//!   tick, [`UNREADY_EXITS_MAX`] times; after that nothing more is tried
//!   until the settings change. One that ends after it answered (the kernel
//!   ended it for memory, a crash) is started again, and the count begins
//!   anew.
//! - The kernel is told to end the model server first when memory runs out
//!   (`oom_score_adj`): the database and the brain stay.
//! - `embra.native=0` on the kernel line starts none in this boot.
//! - Which CPU backend the server's loader takes on this processor is asked
//!   of the server's own program, when a server is first started in a boot
//!   (`native::BACKEND_QUESTION`): a second, short run that loads no model.
//!   It is not waited for either; a later tick reads its answer, and the
//!   status carries the name from then on. The server itself does not say
//!   it at the verbosity it runs at.
//!
//! The server gets no environment (the brain's holds the provider keys) and
//! no way to load anything but the model's own files; the question gets no
//! environment and names no file.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use embra_boot::native_model::{self as native, Installed, Part, ServiceState, Status, Wanted};
use tokio::process::{Child, Command};
use tracing::{info, warn};

/// How often a server may end before it answered, for one state of the
/// settings.
pub(crate) const UNREADY_EXITS_MAX: u32 = 3;
/// The prompt the server reads in one step. A stop (the brain closes the
/// request) is looked at between steps: with llama.cpp's own 2,048 a stop
/// took 14 s to be noticed while a long prompt was read on the build
/// server, with 512 it took 5 s, at the same reading speed.
const BATCH: u32 = 512;
/// How long a probe waits for the server's answer. The server answers its
/// health at once, loading or not; a probe that gets none is "not yet".
const PROBE_TIMEOUT: Duration = Duration::from_millis(1500);
/// The log is cut back to its last [`LOG_KEEP`] bytes when it passes this:
/// it is on the ephemeral tmpfs, and the server writes lines for every
/// request.
const LOG_MAX: u64 = 8 * 1024 * 1024;
const LOG_KEEP: u64 = 1024 * 1024;
/// How much of the log's end is read for the line an exit is explained by.
const LOG_LAST_LINE_WINDOW: u64 = 4096;
/// The kernel ends the process with the highest score first.
const OOM_SCORE_ADJ: &str = "1000";
/// How many ticks the program's answer to the backend's question is looked
/// for before the question is ended. The program answers in a fraction of a
/// second (20 ms once its files were read, about a second before); one that
/// does not is not looked at for the life of the boot.
pub(crate) const BACKEND_TICKS_MAX: u32 = 24;
/// How much of the answer is read: three lines, the loader's among them.
const BACKEND_ANSWER_MAX: u64 = 64 * 1024;

/// Where the keeper reads and writes; a test passes its own.
#[derive(Clone, Debug)]
pub(crate) struct Paths {
    pub settings: PathBuf,
    pub status: PathBuf,
    pub models: PathBuf,
    pub log: PathBuf,
    pub server: PathBuf,
    /// Where the program's answer to the backend's question is written,
    /// until it is read.
    pub backend_answer: PathBuf,
    /// Where the server listens and the keeper asks.
    pub host: String,
    pub port: u16,
}

impl Paths {
    pub(crate) fn os() -> Self {
        Paths {
            settings: PathBuf::from(native::SETTINGS_PATH),
            status: PathBuf::from(native::STATUS_PATH),
            models: PathBuf::from(native::MODELS_DIR),
            log: PathBuf::from(native::LOG_PATH),
            server: PathBuf::from(native::SERVER_PATH),
            // Beside the status, on the same tmpfs.
            backend_answer: PathBuf::from(native::STATUS_PATH).with_file_name("native-model-backend-answer"),
            host: native::HOST.to_string(),
            port: native::PORT,
        }
    }
}

/// What the settings said at a tick.
#[derive(Clone, Debug, PartialEq, Eq)]
enum Followed {
    /// The record could not be taken: the reason.
    Refused(String),
    /// No model is asked for.
    Nothing,
    /// A model and its window, and whether its image file is there to be
    /// given to the server.
    Model(Wanted, bool),
}

struct Server {
    child: Child,
    wanted: Wanted,
    /// It was started with the model's image file.
    images: bool,
    /// It answered its health once.
    ready: bool,
}

/// Which CPU backend the server's loader takes on this processor, as the
/// server's own program says it. A processor does not change while the
/// machine runs: once named, it is not asked again in this boot.
enum Backend {
    /// Nobody asked yet: the tick that finds a server running asks.
    Unasked,
    /// The question runs: the program, and the ticks it was looked at since.
    Asked(Child, u32),
    /// The program's answer (`haswell`, `armv8.2_2`).
    Named(String),
    /// The program named none, or could not be asked. The next start of a
    /// server asks once more.
    Unnamed,
}

pub(crate) struct Keeper {
    enabled: bool,
    paths: Paths,
    /// The settings as the last tick read them.
    followed: Option<Followed>,
    server: Option<Server>,
    /// Exits since the settings changed, for the status.
    exits: u32,
    /// Exits since the settings changed or a server answered.
    unready_exits: u32,
    /// How the last run ended, or why a start failed; empty when a server
    /// answered since, and when the settings changed.
    last_end: String,
    /// The same in a few words, for the sentence of a server that is left
    /// alone, which says the rest itself.
    last_how: String,
    /// What was last written, so that a tick with nothing new writes nothing.
    published: Option<Status>,
    backend: Backend,
    /// How many ticks the program's answer is looked for
    /// ([`BACKEND_TICKS_MAX`]; a test sets its own).
    backend_ticks_max: u32,
}

/// The server's arguments: one file, one name, one window, one slot, on
/// loopback, without the web page.
pub(crate) fn server_args(wanted: &Wanted, paths: &Paths, images: bool) -> Vec<String> {
    [
        "--host",
        paths.host.as_str(),
        "--port",
        &paths.port.to_string(),
        "-m",
        &wanted.model.file_path(&paths.models).to_string_lossy(),
        "-a",
        wanted.model.id,
        "-c",
        &wanted.context.to_string(),
        "-np",
        "1",
        "-b",
        &BATCH.to_string(),
        "--ctx-checkpoints",
        &native::CHECKPOINTS_MAX.to_string(),
        "--cache-ram",
        // What it may keep of earlier prompts: a function of the window,
        // which the brain reads too when it says what a model takes.
        &wanted.model.cache_mib(wanted.context).to_string(),
        // The build has no web page; the switch says so to the server too.
        // `--no-webui` is the name llama.cpp 1.0.0 calls deprecated.
        "--no-ui",
    ]
    .iter()
    .map(|s| s.to_string())
    .chain(if images { image_args(wanted, paths) } else { Vec::new() })
    .collect()
}

/// What the server is told of the model's image file: the file, and how
/// many tokens it makes of an image, the fewest and the most alike
/// (`NativeModel::image_tokens`).
fn image_args(wanted: &Wanted, paths: &Paths) -> Vec<String> {
    let tokens = wanted.model.image_tokens.to_string();
    vec![
        "--mmproj".to_string(),
        wanted.model.path_of(Part::Images, &paths.models).to_string_lossy().into_owned(),
        "--image-min-tokens".to_string(),
        tokens.clone(),
        "--image-max-tokens".to_string(),
        tokens,
    ]
}

/// How a run ended, in words: the exit code, or the signal with what it
/// usually means here.
fn ended_words(status: std::process::ExitStatus) -> String {
    use std::os::unix::process::ExitStatusExt;
    match (status.code(), status.signal()) {
        (Some(code), _) => format!("exit code {code}"),
        (None, Some(9)) => "signal 9: killed, by the kernel when memory ran out or by hand".to_string(),
        (None, Some(4)) => "signal 4: an instruction this processor does not have".to_string(),
        (None, Some(signal)) => format!("signal {signal}"),
        (None, None) => "no status".to_string(),
    }
}

/// The last line of the log that says something, for an exit's reason.
fn last_log_line(log: &Path) -> Option<String> {
    use std::io::{Read, Seek, SeekFrom};
    let mut file = std::fs::File::open(log).ok()?;
    let len = file.metadata().ok()?.len();
    file.seek(SeekFrom::Start(len.saturating_sub(LOG_LAST_LINE_WINDOW))).ok()?;
    let mut tail = Vec::new();
    file.read_to_end(&mut tail).ok()?;
    String::from_utf8_lossy(&tail).lines().rev().map(str::trim).find(|line| !line.is_empty()).map(str::to_string)
}

/// Cut the log back to its end when it passed [`LOG_MAX`]. The server
/// appends, so what it writes next follows what is kept.
fn trim_log(log: &Path) {
    use std::io::{Read, Seek, SeekFrom, Write};
    let Ok(len) = std::fs::metadata(log).map(|m| m.len()) else { return };
    if len <= LOG_MAX {
        return;
    }
    let Ok(mut file) = std::fs::OpenOptions::new().read(true).append(true).open(log) else { return };
    let mut kept = Vec::new();
    if file.seek(SeekFrom::Start(len - LOG_KEEP)).is_err() || file.read_to_end(&mut kept).is_err() {
        return;
    }
    if file.set_len(0).is_ok() {
        let _ = writeln!(file, "[embrad] the log passed {} MiB; its last {} MiB follows", LOG_MAX >> 20, LOG_KEEP >> 20);
        let _ = file.write_all(&kept);
    }
}

/// The start of a small file as text: the program's answer. Empty when it
/// cannot be read.
fn read_start(path: &Path, max: u64) -> String {
    use std::io::Read;
    let mut start = Vec::new();
    if let Ok(file) = std::fs::File::open(path) {
        let _ = file.take(max).read_to_end(&mut start);
    }
    String::from_utf8_lossy(&start).into_owned()
}

/// The status to write, or `None` when it is what was written last. `since`
/// says when the state began: it moves when anything of the state does, and
/// stays when the one thing new is the backend's name, which arrives a tick
/// after the server it is about and is no new state.
fn to_publish(last: Option<&Status>, mut status: Status, now: u64) -> Option<Status> {
    let Some(last) = last else {
        status.since = now;
        return Some(status);
    };
    status.since = last.since;
    if *last == status {
        return None;
    }
    let but_for_the_backend = Status { backend: last.backend.clone(), ..status.clone() };
    if but_for_the_backend != *last {
        status.since = now;
    }
    Some(status)
}

/// One question to the server's health: whether it answers 200. Loading, it
/// answers 503; not listening yet, the connection is refused.
async fn answers(host: &str, port: u16) -> bool {
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    let ask = async {
        let mut stream = tokio::net::TcpStream::connect((host, port)).await.ok()?;
        let request = format!("GET /health HTTP/1.0\r\nHost: {host}:{port}\r\n\r\n");
        stream.write_all(request.as_bytes()).await.ok()?;
        let mut buf = vec![0u8; 1024];
        let n = stream.read(&mut buf).await.ok()?;
        Some(crate::supervisor::is_http_200(&String::from_utf8_lossy(&buf[..n])))
    };
    matches!(tokio::time::timeout(PROBE_TIMEOUT, ask).await, Ok(Some(true)))
}

impl Keeper {
    /// The keeper of this machine; `enabled` is the kernel line's word
    /// (`Cmdline::native`).
    pub(crate) fn new(enabled: bool) -> Self {
        Self::with_paths(enabled, Paths::os())
    }

    pub(crate) fn with_paths(enabled: bool, paths: Paths) -> Self {
        Keeper {
            enabled,
            paths,
            followed: None,
            server: None,
            exits: 0,
            unready_exits: 0,
            last_end: String::new(),
            last_how: String::new(),
            published: None,
            backend: Backend::Unasked,
            backend_ticks_max: BACKEND_TICKS_MAX,
        }
    }

    /// The server's process.
    pub(crate) fn pid(&self) -> Option<u32> {
        self.server.as_ref().and_then(|s| s.child.id())
    }

    /// The keeper's processes, for the reaper to leave alone: the server,
    /// and the program while it is asked for its backend.
    pub(crate) fn pids(&self) -> Vec<u32> {
        let asked = match &self.backend {
            Backend::Asked(program, _) => program.id(),
            Backend::Unasked | Backend::Named(_) | Backend::Unnamed => None,
        };
        self.pid().into_iter().chain(asked).collect()
    }

    /// One look at the settings and at the server. Never waits for a model
    /// to load; ending a server takes its grace at most.
    pub(crate) async fn tick(&mut self) {
        if !self.enabled {
            self.publish(ServiceState::Disabled, None, "native models are switched off on the kernel line (embra.native=0)".to_string());
            return;
        }
        // Whatever the settings say by now: a program that was asked is
        // looked at until it has answered, and collected.
        self.look_at_the_answer().await;
        let followed = match native::read_settings(&self.paths.settings) {
            Err(reason) => Followed::Refused(reason),
            Ok(None) => Followed::Nothing,
            Ok(Some(wanted)) => Followed::Model(wanted, native::installed_part(wanted.model, Part::Images, &self.paths.models) == Installed::Verified),
        };
        if self.followed.as_ref() != Some(&followed) {
            // Other settings: what ran for the ones before ends, and every
            // count begins again.
            self.stop().await;
            (self.exits, self.unready_exits) = (0, 0);
            self.last_end.clear();
            self.last_how.clear();
            self.followed = Some(followed.clone());
        }
        let wanted = match followed {
            Followed::Refused(reason) => {
                self.publish(ServiceState::Unavailable, None, format!("the settings were not taken ({reason})"));
                return;
            }
            Followed::Nothing => {
                self.publish(ServiceState::Off, None, String::new());
                return;
            }
            Followed::Model(wanted, images) => (wanted, images),
        };
        let (wanted, images) = wanted;
        self.look_at_the_server().await;
        // Why no server can be started for these settings now, when one cannot.
        let mut waits_for = None;
        if self.server.is_none() && self.unready_exits < UNREADY_EXITS_MAX {
            match self.file_of(&wanted) {
                Ok(()) => self.start(wanted, images),
                Err(why) => waits_for = Some(why),
            }
        }
        if self.server.is_some() && matches!(self.backend, Backend::Unasked) {
            self.ask_for_the_backend();
        }
        trim_log(&self.paths.log);
        let (state, reason) = match (&self.server, waits_for) {
            (Some(server), _) if server.ready => (ServiceState::Ready, String::new()),
            // A start after an exit says how the run before ended.
            (Some(_), _) => (ServiceState::Loading, self.last_end.clone()),
            (None, Some(why)) => (ServiceState::Unavailable, why),
            (None, None) if self.unready_exits >= UNREADY_EXITS_MAX => (
                ServiceState::Halted,
                format!(
                    "the model server ended {} times before it answered ({}); nothing more is tried until the settings change",
                    self.unready_exits, self.last_how
                ),
            ),
            // A start that failed, below the count.
            (None, None) => (ServiceState::Unavailable, self.last_end.clone()),
        };
        self.publish(state, Some(wanted), reason);
    }

    /// Whether the model's file can be started from: verified, as the record
    /// beside it says. The error is the sentence the status carries: what is
    /// wrong with the file, or that a job is at it (`native::job_running`).
    ///
    /// Before the file, the model's terms: a model that comes with a policy
    /// on its use is loaded only where the operator accepted it
    /// (`native::terms_of`: the brain records it, beside the settings).
    /// A STATE that was restored from before the acceptance has the files
    /// on DATA and no record: the server is not started, and the status
    /// says the two commands. An acceptance is found at the next tick.
    fn file_of(&self, wanted: &Wanted) -> Result<(), String> {
        let id = wanted.model.id;
        if !native::terms_settled(wanted.model, &native::terms_dir(&self.paths.settings)) {
            return Err(native::terms_open_words(wanted.model).unwrap_or_else(|| format!("the terms of {id} are not accepted on this machine")));
        }
        let at_work = native::job_running(&self.paths.models);
        match native::installed(wanted.model, &self.paths.models) {
            Installed::Verified => Ok(()),
            Installed::No | Installed::Partial(_) if at_work => Err(format!("{id} is being installed")),
            Installed::Unverified(_) if at_work => Err(format!("{id} is being checked")),
            Installed::No => Err(format!("{id} is not installed")),
            Installed::Partial(_) => Err(format!("{id} is not installed: its download stopped")),
            // With the way on: a turn, `/native` and the provider pill
            // carry this sentence.
            Installed::Unverified(why) => Err(format!("{id} cannot be used: {why}; /native install {id} checks it again and replaces a file that is not this release's")),
        }
    }

    /// Whether the server ended, and whether one that loads answers by now.
    async fn look_at_the_server(&mut self) {
        let Some(server) = &mut self.server else { return };
        match server.child.try_wait() {
            Ok(Some(status)) => {
                use std::os::unix::process::ExitStatusExt;
                let mut how = ended_words(status);
                // What it said last explains an error and a crash. Killed, it
                // said nothing about it: its last line is an ordinary one.
                if status.signal() != Some(libc::SIGKILL)
                    && let Some(line) = last_log_line(&self.paths.log)
                {
                    how.push_str(&format!("; its last line: {line}"));
                }
                let reason = format!("the model server ended ({how})");
                warn!("native model: {reason}");
                self.last_how = how;
                self.exits += 1;
                if !server.ready {
                    self.unready_exits += 1;
                }
                self.last_end = reason;
                self.server = None;
            }
            Ok(None) => {
                if !server.ready && answers(&self.paths.host, self.paths.port).await {
                    server.ready = true;
                    self.unready_exits = 0;
                    self.last_end.clear();
                    self.last_how.clear();
                    info!("native model: {} answers on {}:{}", server.wanted.model.id, self.paths.host, self.paths.port);
                }
            }
            Err(e) => warn!("native model: the model server's state could not be read: {e}"),
        }
    }

    /// Ask the server's program which backend its loader takes on this
    /// processor ([`native::BACKEND_QUESTION`]): a run of its own, without
    /// an environment, its two streams into one small file. Not waited for.
    fn ask_for_the_backend(&mut self) {
        if let Some(dir) = self.paths.backend_answer.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        let answer = std::fs::File::create(&self.paths.backend_answer).and_then(|f| Ok((f.try_clone()?, f)));
        let asked = answer.and_then(|(out, err)| {
            Command::new(&self.paths.server)
                .args(native::BACKEND_QUESTION)
                .env_clear()
                .stdin(Stdio::null())
                .stdout(Stdio::from(out))
                .stderr(Stdio::from(err))
                .kill_on_drop(true)
                .spawn()
        });
        self.backend = match asked {
            Ok(program) => Backend::Asked(program, 0),
            Err(e) => {
                warn!("native model: {} could not be asked which backend it takes on this processor ({e})", self.paths.server.display());
                let _ = std::fs::remove_file(&self.paths.backend_answer);
                Backend::Unnamed
            }
        };
    }

    /// Whether the program that was asked has answered, and what. One that
    /// has not ended after [`BACKEND_TICKS_MAX`] ticks is ended.
    async fn look_at_the_answer(&mut self) {
        let limit = self.backend_ticks_max;
        let Backend::Asked(program, ticks) = &mut self.backend else { return };
        let how = match program.try_wait() {
            Ok(Some(status)) => ended_words(status),
            Ok(None) if ticks.saturating_add(1) < limit => {
                *ticks += 1;
                return;
            }
            Ok(None) => {
                // Killed and collected here: it is this process's child.
                let _ = program.kill().await;
                format!("it had not ended after {limit} looks and was ended")
            }
            Err(e) => format!("its state could not be read: {e}"),
        };
        let answer = read_start(&self.paths.backend_answer, BACKEND_ANSWER_MAX);
        let _ = std::fs::remove_file(&self.paths.backend_answer);
        self.backend = match native::backend_of_answer(&answer) {
            Some(name) => {
                info!(
                    "native model: on this processor the model server takes the {name} backend ({}/{}{name}{})",
                    native::BACKEND_DIR,
                    native::BACKEND_FILE_PREFIX,
                    native::BACKEND_FILE_SUFFIX
                );
                Backend::Named(name.to_string())
            }
            None => {
                let said = answer.lines().map(str::trim).find(|line| !line.is_empty()).unwrap_or("nothing");
                warn!(
                    "native model: {} named no backend when asked ({how}; it said: {}); the status carries none",
                    self.paths.server.display(),
                    native::one_line(said)
                );
                Backend::Unnamed
            }
        };
    }

    /// Start the server for `wanted`, with the model's image file when
    /// `images` says it is there. A start that fails counts as a run that
    /// ended before it answered.
    fn start(&mut self, wanted: Wanted, images: bool) {
        let log = std::fs::OpenOptions::new().create(true).append(true).open(&self.paths.log);
        let (out, err) = match log.and_then(|f| Ok((f.try_clone()?, f))) {
            Ok((a, b)) => (Stdio::from(a), Stdio::from(b)),
            Err(_) => (Stdio::null(), Stdio::null()),
        };
        let mut command = Command::new(&self.paths.server);
        command
            .args(server_args(&wanted, &self.paths, images))
            .env_clear()
            .stdin(Stdio::null())
            .stdout(out)
            .stderr(err)
            .kill_on_drop(true);
        match command.spawn() {
            Ok(child) => {
                if let Some(pid) = child.id()
                    && let Err(e) = std::fs::write(format!("/proc/{pid}/oom_score_adj"), OOM_SCORE_ADJ)
                {
                    warn!("native model: the kernel was not told to end the model server first ({e})");
                }
                info!(
                    "native model: started {} for {} with a window of {} tokens, {} (pid {:?})",
                    self.paths.server.display(),
                    wanted.model.id,
                    wanted.context,
                    if images { "with its image file" } else { "to read text alone: its image file is not installed" },
                    child.id()
                );
                self.server = Some(Server { child, wanted, images, ready: false });
                // A program that named no backend is asked once more with
                // every start: by the tick this start belongs to.
                if matches!(self.backend, Backend::Unnamed) {
                    self.backend = Backend::Unasked;
                }
            }
            Err(e) => {
                self.last_how = format!("it could not be started, {}: {e}", self.paths.server.display());
                self.last_end = format!("the model server could not be started ({}: {e})", self.paths.server.display());
                warn!("native model: {}", self.last_end);
                self.exits += 1;
                self.unready_exits += 1;
            }
        }
    }

    /// End the server, if one runs: SIGTERM, then the grace every service
    /// gets.
    pub(crate) async fn stop(&mut self) {
        if let Some(mut server) = self.server.take() {
            info!("native model: stopping the model server ({})", server.wanted.model.id);
            crate::supervisor::stop_one(&mut server.child, "the model server", crate::supervisor::STOP_GRACE).await;
        }
    }

    /// Write the status when it is not what was written last
    /// ([`to_publish`]).
    fn publish(&mut self, state: ServiceState, wanted: Option<Wanted>, reason: String) {
        let mut status = Status::of(state, 0);
        status.model = wanted.map(|w| w.model);
        status.context = wanted.map(|w| w.context);
        status.pid = if state.runs() { self.pid() } else { None };
        status.images = state.runs() && self.server.as_ref().is_some_and(|server| server.images);
        // The backend is said of a server that runs, once the program named it.
        status.backend = match &self.backend {
            Backend::Named(name) if state.runs() => Some(name.clone()),
            Backend::Named(_) | Backend::Unasked | Backend::Asked(..) | Backend::Unnamed => None,
        };
        status.exits = self.exits;
        status.reason = native::one_line(&reason);
        let Some(status) = to_publish(self.published.as_ref(), status, native::uptime_secs()) else { return };
        if let Err(e) = native::write_status(&self.paths.status, &status) {
            warn!("native model: the status could not be written to {} ({e})", self.paths.status.display());
        }
        self.published = Some(status);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// A scratch directory with a model installed and verified, a stand-in
    /// server, and the keeper's paths in it. The stand-in writes its
    /// arguments and its environment down, then does what `behaviour` says.
    struct Rig {
        dir: PathBuf,
        paths: Paths,
    }

    const MODEL: &native::NativeModel = &native::CATALOGUE[0];

    /// What the image's own build answers on a processor with AVX2: the
    /// list's two lines on one stream, the loader's line on the other.
    const NAMES_HASWELL: &str = "echo 'Available devices:'\necho '0.00.427.675 I load_backend: loaded CPU backend from /usr/lib/llama-cpp/libggml-cpu-haswell.so' >&2\necho '  (none)'";

    impl Rig {
        fn new(tag: &str, behaviour: &str) -> Rig {
            let dir = std::env::temp_dir().join(format!("embrad-native-{tag}-{}", std::process::id()));
            let _ = std::fs::remove_dir_all(&dir);
            std::fs::create_dir_all(&dir).unwrap();
            // A port nobody listens on until a test does.
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let port = listener.local_addr().unwrap().port();
            drop(listener);
            let paths = Paths {
                settings: dir.join("state/native-model/settings"),
                status: dir.join("run/native-model-status"),
                models: dir.join("models"),
                log: dir.join("native-model.log"),
                server: dir.join("llama-server"),
                backend_answer: dir.join("run/native-model-backend-answer"),
                host: "127.0.0.1".to_string(),
                port,
            };
            let rig = Rig { dir, paths };
            rig.install();
            rig.server_does(behaviour);
            rig
        }

        /// The model's image file with its record, the same way.
        fn install_images(&self) {
            let path = MODEL.path_of(Part::Images, &self.paths.models);
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(MODEL.images.bytes).unwrap();
            let record = native::Verified::of_part(MODEL, Part::Images, &file.metadata().unwrap());
            native::write_verified(&MODEL.verified_of(Part::Images, &self.paths.models), &record).unwrap();
            assert_eq!(native::installed_part(MODEL, Part::Images, &self.paths.models), Installed::Verified);
        }

        /// The model's file with the record that vouches for it. The file
        /// has the catalogue's size and no bytes on disk (a hole): a start
        /// compares the size and the time and reads nothing.
        fn install(&self) {
            self.install_model(MODEL);
        }

        /// The same for any model of the catalogue.
        fn install_model(&self, model: &'static native::NativeModel) {
            std::fs::create_dir_all(model.dir(&self.paths.models)).unwrap();
            let path = model.file_path(&self.paths.models);
            let file = std::fs::File::create(&path).unwrap();
            file.set_len(model.bytes).unwrap();
            let record = native::Verified::of(model, &file.metadata().unwrap());
            native::write_verified(&model.verified_path(&self.paths.models), &record).unwrap();
            assert_eq!(native::installed(model, &self.paths.models), Installed::Verified);
        }

        fn server_does(&self, behaviour: &str) {
            self.program_does(behaviour, NAMES_HASWELL);
        }

        /// The stand-in program. Asked for its backend
        /// ([`native::BACKEND_QUESTION`]) it writes the question and its
        /// environment down and does what `asked` says; started as a server
        /// it writes its arguments and its environment down and does what
        /// `behaviour` says. The two leave their marks in files of their
        /// own: a question is no start.
        fn program_does(&self, behaviour: &str, asked: &str) {
            let script = format!(
                "#!/bin/sh\ncase \" $* \" in *' --list-devices '*)\nprintf '%s\\n' \"$@\" > '{dir}/question'\nenv > '{dir}/question-env'\necho asked >> '{dir}/questions'\n{asked}\nexit 0 ;;\nesac\nprintf '%s\\n' \"$@\" > '{dir}/args'\nenv > '{dir}/env'\necho started >> '{dir}/starts'\n{behaviour}\n",
                dir = self.dir.display()
            );
            std::fs::write(&self.paths.server, script).unwrap();
            std::fs::set_permissions(&self.paths.server, std::fs::Permissions::from_mode(0o755)).unwrap();
        }

        /// A keeper that looks for the program's answer as long as a test
        /// goes on: the tests that are not about that limit.
        fn patient_keeper(&self) -> Keeper {
            let mut keeper = self.keeper();
            keeper.backend_ticks_max = u32::MAX;
            keeper
        }

        fn questions(&self) -> usize {
            std::fs::read_to_string(self.dir.join("questions")).map(|s| s.lines().count()).unwrap_or(0)
        }

        /// Tick until the program that was asked has ended and the keeper
        /// has read its answer: its run is not the keeper's to await.
        async fn answered(&self, keeper: &mut Keeper) {
            for _ in 0..5_000 {
                keeper.tick().await;
                if !matches!(keeper.backend, Backend::Asked(..) | Backend::Unasked) {
                    return;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("the program that was asked for its backend never answered");
        }

        fn want(&self, wanted: Option<&Wanted>) {
            native::write_settings(&self.paths.settings, wanted).unwrap();
        }

        fn keeper(&self) -> Keeper {
            Keeper::with_paths(true, self.paths.clone())
        }

        fn status(&self) -> Status {
            native::read_status(&self.paths.status).unwrap().expect("a status was written")
        }

        fn starts(&self) -> usize {
            std::fs::read_to_string(self.dir.join("starts")).map(|s| s.lines().count()).unwrap_or(0)
        }

        /// Wait until the stand-in has written its files: its start is not
        /// the keeper's to await.
        async fn started(&self, count: usize) {
            for _ in 0..2_000 {
                if self.starts() >= count && self.dir.join("env").exists() {
                    return;
                }
                tokio::task::yield_now().await;
                std::thread::sleep(Duration::from_millis(1));
            }
            panic!("the stand-in server did not start {count} times");
        }

        /// A health endpoint on the rig's port: it answers every request
        /// with the status line it was last given.
        fn health(&self, status_line: &'static str) -> Health {
            use std::io::{Read, Write};
            let listener = std::net::TcpListener::bind(("127.0.0.1", self.paths.port)).unwrap();
            let line = std::sync::Arc::new(std::sync::Mutex::new(status_line));
            let answering = line.clone();
            std::thread::spawn(move || {
                for stream in listener.incoming() {
                    let Ok(mut stream) = stream else { return };
                    let mut request = [0u8; 512];
                    let _ = stream.read(&mut request);
                    let status_line = *answering.lock().unwrap();
                    let _ = stream.write_all(format!("{status_line}\r\ncontent-length: 2\r\n\r\n{{}}").as_bytes());
                }
            });
            Health(line)
        }
    }

    struct Health(std::sync::Arc<std::sync::Mutex<&'static str>>);

    impl Health {
        fn answers(&self, status_line: &'static str) {
            *self.0.lock().unwrap() = status_line;
        }
    }

    impl Drop for Rig {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.dir);
        }
    }

    #[test]
    fn the_server_is_told_to_listen_on_loopback_and_to_load_one_file() {
        let paths = Paths::os();
        assert_eq!((paths.host.as_str(), paths.port), ("127.0.0.1", 8091), "loopback alone, the contract's port");
        let args = server_args(&Wanted::new(MODEL), &paths, false);
        assert_eq!(
            args,
            [
                "--host", "127.0.0.1", "--port", "8091",
                "-m", "/embra/data/models/qwen3.5-4b/Qwen3.5-4B-Q4_K_M.gguf",
                "-a", "qwen3.5-4b",
                "-c", "65536",
                "-np", "1",
                "-b", "512",
                "--ctx-checkpoints", "32",
                "--cache-ram", "3796",
                "--no-ui",
            ]
        );
        // With the model's image file: the same, then the file and the
        // tokens of an image, the fewest and the most alike.
        let seeing = server_args(&Wanted::new(MODEL), &paths, true);
        assert_eq!(seeing[..args.len()], args[..]);
        assert_eq!(
            seeing[args.len()..],
            ["--mmproj", "/embra/data/models/qwen3.5-4b/mmproj-Qwen3.5-4B-F16.gguf", "--image-min-tokens", "1024", "--image-max-tokens", "1024"]
        );
        // No flag that reaches outside the machine or loads a file that is
        // not the model's own: an image reaches the server in the request
        // (the brain sends its bytes), never by a path or an address.
        for flag in [
            "--hf-repo", "-hf", "--model-url", "-mu", "--mmproj-url", "-mmu", "--media-path", "--media-allow-private-urls",
            "--api-key", "--ssl-key-file", "--models-dir", "--webui", "--ui", "--no-webui",
        ] {
            assert!(!args.iter().any(|a| a == flag) && !seeing.iter().any(|a| a == flag), "{flag}");
        }
        // The prompt cache follows the window (`native::cache_mib`: one
        // prompt that fills it, with every checkpoint it can have), and the
        // checkpoints are the number the limit counts with.
        let small = server_args(&Wanted { model: MODEL, context: 4_096 }, &paths, false);
        assert_eq!(small[small.iter().position(|a| a == "--cache-ram").unwrap() + 1], MODEL.cache_mib(4_096).to_string());
        assert_eq!(small[small.iter().position(|a| a == "--cache-ram").unwrap() + 1], "1816");
        assert_eq!(small[small.iter().position(|a| a == "--ctx-checkpoints").unwrap() + 1], native::CHECKPOINTS_MAX.to_string());
    }

    #[test]
    fn an_exit_is_said_in_words() {
        use std::os::unix::process::ExitStatusExt;
        assert_eq!(ended_words(std::process::ExitStatus::from_raw(1 << 8)), "exit code 1");
        assert_eq!(ended_words(std::process::ExitStatus::from_raw(0)), "exit code 0");
        assert!(ended_words(std::process::ExitStatus::from_raw(9)).starts_with("signal 9: killed"));
        assert!(ended_words(std::process::ExitStatus::from_raw(4)).contains("this processor does not have"));
        assert_eq!(ended_words(std::process::ExitStatus::from_raw(11)), "signal 11");
    }

    #[tokio::test]
    async fn no_model_asked_for_is_off_and_the_kernel_lines_switch_is_disabled() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("off", "exec sleep 30");
        let mut keeper = rig.keeper();
        keeper.tick().await;
        assert_eq!(rig.status().state, ServiceState::Off, "no settings file");
        rig.want(None);
        keeper.tick().await;
        assert_eq!((rig.status().state, rig.starts()), (ServiceState::Off, 0), "a record that names no model");

        // Switched off on the kernel line: the settings are not even read.
        rig.want(Some(&Wanted::new(MODEL)));
        let mut disabled = Keeper::with_paths(false, rig.paths.clone());
        disabled.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.model), (ServiceState::Disabled, None));
        assert!(status.reason.contains("embra.native=0"), "{}", status.reason);
        assert_eq!(rig.starts(), 0);

        // Settings that are no record start nothing and say why.
        std::fs::write(&rig.paths.settings, "model=qwen3.5-4b\nthreads=8\n").unwrap();
        keeper.tick().await;
        let status = rig.status();
        assert_eq!(status.state, ServiceState::Unavailable);
        assert!(status.reason.contains("the settings were not taken") && status.reason.contains("threads=8"), "{}", status.reason);
        assert_eq!(rig.starts(), 0);
    }

    #[tokio::test]
    async fn a_model_asked_for_is_started_without_an_environment_and_found_answering_later() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("ready", "exec sleep 30");
        rig.want(Some(&Wanted { model: MODEL, context: 32_768 }));
        // A secret in this process's environment, as the brain's has.
        // SAFETY: the tests that spawn children hold one lock; no other thread reads the environment meanwhile.
        unsafe { std::env::set_var("EMBRA_NATIVE_TEST_SECRET", "sk-never") };
        let mut keeper = rig.patient_keeper();
        keeper.tick().await;
        rig.started(1).await;
        // The question is asked in the same tick, with the same nothing.
        for _ in 0..5_000 {
            if rig.questions() >= 1 && rig.dir.join("question-env").exists() {
                break;
            }
            std::thread::sleep(Duration::from_millis(1));
        }
        unsafe { std::env::remove_var("EMBRA_NATIVE_TEST_SECRET") };
        let status = rig.status();
        assert_eq!((status.state, status.model, status.context), (ServiceState::Loading, Some(MODEL), Some(32_768)));
        assert_eq!(status.pid, keeper.pid());
        assert!(status.pid.is_some() && status.exits == 0 && status.reason.is_empty());

        let args = std::fs::read_to_string(rig.dir.join("args")).unwrap();
        let args: Vec<&str> = args.lines().collect();
        assert_eq!(args, server_args(&Wanted { model: MODEL, context: 32_768 }, &rig.paths, false));
        assert!(!rig.status().images, "no image file is installed: the model reads text");
        let env = std::fs::read_to_string(rig.dir.join("env")).unwrap();
        assert!(!env.contains("EMBRA_NATIVE_TEST_SECRET") && !env.contains("sk-never"), "{env}");
        assert!(!env.lines().any(|l| l.starts_with("HOME=") || l.starts_with("PATH=/")), "nothing is handed on: {env}");
        let asked_env = std::fs::read_to_string(rig.dir.join("question-env")).unwrap();
        assert!(!asked_env.contains("sk-never") && !asked_env.lines().any(|l| l.starts_with("HOME=") || l.starts_with("PATH=/")), "{asked_env}");

        // The program's answer arrives a tick or more after the start: the
        // status names the backend then, and the state it is about began
        // when it began.
        let began = status.since;
        rig.answered(&mut keeper).await;
        let status = rig.status();
        assert_eq!((status.state, status.backend.as_deref(), status.since), (ServiceState::Loading, Some("haswell"), began));

        // Not answering yet: nothing changes, and nothing is written again.
        let written = std::fs::metadata(&rig.paths.status).unwrap().modified().unwrap();
        keeper.tick().await;
        assert_eq!(rig.status().state, ServiceState::Loading);
        assert_eq!(std::fs::metadata(&rig.paths.status).unwrap().modified().unwrap(), written);

        // Loading, the server answers 503: still loading.
        let health = rig.health("HTTP/1.1 503 Service Unavailable");
        keeper.tick().await;
        assert_eq!(rig.status().state, ServiceState::Loading);

        health.answers("HTTP/1.1 200 OK");
        keeper.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.pid), (ServiceState::Ready, keeper.pid()));
        assert_eq!(rig.starts(), 1, "one start");

        // No model asked for any more: the server ends.
        let pid = keeper.pid().unwrap();
        rig.want(None);
        keeper.tick().await;
        assert_eq!((rig.status().state, keeper.pid()), (ServiceState::Off, None));
        assert!(!Path::new(&format!("/proc/{pid}")).exists(), "the server's process is gone");
    }

    /// A model whose image file is installed is started with it, and the
    /// status says that it reads images. An image file that arrives while
    /// the model runs without one ends that server and starts another with
    /// the file, as other settings do; one that goes does the same.
    #[tokio::test]
    async fn the_image_file_is_given_to_the_server_when_it_is_there_and_its_coming_starts_the_server_again() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("images", "exec sleep 30");
        let wanted = Wanted::new(MODEL);
        rig.want(Some(&wanted));
        let mut keeper = rig.keeper();
        keeper.tick().await;
        rig.started(1).await;
        let read_args = || -> Vec<String> { std::fs::read_to_string(rig.dir.join("args")).unwrap().lines().map(str::to_string).collect() };
        assert_eq!(read_args(), server_args(&wanted, &rig.paths, false));
        assert!(!rig.status().images);
        let first = keeper.pid().unwrap();

        // Nothing new: nothing is started again.
        keeper.tick().await;
        assert_eq!((rig.starts(), keeper.pid()), (1, Some(first)));

        // The image file arrives (an install brought it): the next tick ends
        // the server that reads text, and starts one with the file.
        rig.install_images();
        keeper.tick().await;
        rig.started(2).await;
        assert_ne!(keeper.pid(), Some(first));
        assert!(!Path::new(&format!("/proc/{first}")).exists(), "the server without the file is gone");
        let args = read_args();
        assert_eq!(args, server_args(&wanted, &rig.paths, true));
        assert!(args.windows(2).any(|pair| pair[0] == "--mmproj" && pair[1].ends_with("/qwen3.5-4b/mmproj-Qwen3.5-4B-F16.gguf")), "{args:?}");
        let status = rig.status();
        assert!(status.images && status.exits == 0, "a start for other settings is no exit: {status:?}");
        assert_eq!(native::read_status(&rig.paths.status).unwrap().unwrap().to_text().lines().filter(|line| *line == "images=yes").count(), 1);

        // The file goes (a check found it changed and removed it): text again.
        let second = keeper.pid().unwrap();
        std::fs::remove_file(MODEL.path_of(Part::Images, &rig.paths.models)).unwrap();
        keeper.tick().await;
        rig.started(3).await;
        assert_ne!(keeper.pid(), Some(second));
        assert_eq!(read_args(), server_args(&wanted, &rig.paths, false));
        assert!(!rig.status().images);
        keeper.stop().await;
    }

    #[tokio::test]
    async fn a_server_that_ends_before_it_answers_is_tried_three_times_and_then_left() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("budget", "echo 'error: failed to load model' >&2\nexit 1");
        rig.want(Some(&Wanted::new(MODEL)));
        let mut keeper = rig.keeper();
        // A tick starts it; the next finds it ended and starts it again.
        for start in 1..=3 {
            keeper.tick().await;
            rig.started(start).await;
            // The stand-in has ended by the time its last line is in the log.
            for _ in 0..2_000 {
                if keeper.server.as_mut().is_some_and(|s| s.child.try_wait().ok().flatten().is_some()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        keeper.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.exits, status.pid), (ServiceState::Halted, 3, None));
        assert_eq!(
            status.reason,
            "the model server ended 3 times before it answered (exit code 1; its last line: error: failed to load model); nothing more is tried until the settings change"
        );
        for _ in 0..5 {
            keeper.tick().await;
        }
        assert_eq!(rig.starts(), 3, "no fourth start");

        // Other settings: the count begins again.
        rig.server_does("exec sleep 30");
        rig.want(Some(&Wanted { model: MODEL, context: 8_192 }));
        keeper.tick().await;
        rig.started(4).await;
        let status = rig.status();
        assert_eq!((status.state, status.exits, status.context), (ServiceState::Loading, 0, Some(8_192)));
        keeper.stop().await;
    }

    #[tokio::test]
    async fn changed_settings_end_the_server_and_start_one_for_the_new_ones() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("changed", "exec sleep 30");
        rig.want(Some(&Wanted { model: MODEL, context: 8_192 }));
        let mut keeper = rig.keeper();
        keeper.tick().await;
        rig.started(1).await;
        let first = keeper.pid().unwrap();

        rig.want(Some(&Wanted { model: MODEL, context: 16_384 }));
        keeper.tick().await;
        rig.started(2).await;
        let second = keeper.pid().unwrap();
        assert_ne!(first, second);
        assert!(!Path::new(&format!("/proc/{first}")).exists(), "the server of the settings before is gone");
        let args = std::fs::read_to_string(rig.dir.join("args")).unwrap();
        assert!(args.contains("\n16384\n"), "{args}");
        assert_eq!((rig.status().state, rig.status().context), (ServiceState::Loading, Some(16_384)));
        keeper.stop().await;
        assert_eq!(keeper.pid(), None);
    }

    #[tokio::test]
    async fn a_file_nobody_vouches_for_is_not_started() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("unverified", "exec sleep 30");
        rig.want(Some(&Wanted::new(MODEL)));
        let mut keeper = rig.keeper();

        // Changed since it was checked: another size than the record's.
        std::fs::write(MODEL.file_path(&rig.paths.models), b"GGUF and more").unwrap();
        keeper.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.model), (ServiceState::Unavailable, Some(MODEL)));
        assert_eq!(
            status.reason,
            "qwen3.5-4b cannot be used: the file changed since it was checked; /native install qwen3.5-4b checks it again and replaces a file that is not this release's"
        );

        // Never checked.
        std::fs::remove_file(MODEL.verified_path(&rig.paths.models)).unwrap();
        keeper.tick().await;
        assert_eq!(
            rig.status().reason,
            "qwen3.5-4b cannot be used: the file was never checked; /native install qwen3.5-4b checks it again and replaces a file that is not this release's"
        );

        // A stopped download, and nothing at all.
        std::fs::rename(MODEL.file_path(&rig.paths.models), MODEL.partial_path(&rig.paths.models)).unwrap();
        keeper.tick().await;
        assert_eq!(rig.status().reason, "qwen3.5-4b is not installed: its download stopped");
        std::fs::remove_dir_all(MODEL.dir(&rig.paths.models)).unwrap();
        keeper.tick().await;
        assert_eq!(rig.status().reason, "qwen3.5-4b is not installed");
        assert_eq!(rig.starts(), 0, "nothing was started");

        // A job at the models (an install, an import): the status says so, whatever is there so far.
        let job = native::job_lock(&rig.paths.models).unwrap().expect("the lock is free");
        keeper.tick().await;
        assert_eq!(rig.status().reason, "qwen3.5-4b is being installed");
        std::fs::create_dir_all(MODEL.dir(&rig.paths.models)).unwrap();
        std::fs::write(MODEL.partial_path(&rig.paths.models), b"GG").unwrap();
        keeper.tick().await;
        assert_eq!(rig.status().reason, "qwen3.5-4b is being installed");
        std::fs::rename(MODEL.partial_path(&rig.paths.models), MODEL.file_path(&rig.paths.models)).unwrap();
        keeper.tick().await;
        assert_eq!(rig.status().reason, "qwen3.5-4b is being checked");
        drop(job);
        assert_eq!(rig.starts(), 0, "nothing was started");

        // Installed and verified while the settings stand: the next tick starts it,
        // and what was wrong before is no reason of the run that loads.
        rig.install();
        keeper.tick().await;
        rig.started(1).await;
        let status = rig.status();
        assert_eq!((status.state, status.reason.as_str()), (ServiceState::Loading, ""));
        keeper.stop().await;
    }

    /// A model that comes with a policy on its use is not loaded where the
    /// operator has not accepted it, whatever stands on DATA: a STATE from
    /// before the acceptance, or a record of another date of the policy.
    /// The status carries the two commands, and the acceptance is found at
    /// the next tick with no change of the settings.
    #[tokio::test]
    async fn a_model_whose_terms_are_not_accepted_is_not_started() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("terms", "exec sleep 30");
        let inkling = native::by_id("inkling-2-mini").unwrap();
        rig.install_model(inkling);
        rig.want(Some(&Wanted::new(inkling)));
        let mut keeper = rig.keeper();
        let open = "inkling-2-mini comes with its maker's Model Acceptable Use Policy, which is not accepted on this machine: /native terms inkling-2-mini shows it, and /native accept inkling-2-mini accepts it";

        keeper.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.model, status.reason.as_str()), (ServiceState::Unavailable, Some(inkling), open));
        // An acceptance of the policy at another date is not one of this release's reading.
        let terms = native::terms_dir(&rig.paths.settings);
        std::fs::create_dir_all(&terms).unwrap();
        std::fs::write(native::terms_path(&terms, inkling), "policy=Model Acceptable Use Policy\nreviewed=March 1, 2026\nat=5\nby=console\n").unwrap();
        keeper.tick().await;
        assert_eq!(rig.status().reason, open);
        assert_eq!(rig.starts(), 0, "nothing was started");

        // Accepted: the next tick starts the server, and the settings were not touched.
        native::accept_terms(inkling, &terms, 1_764_183_000, "console").unwrap().unwrap();
        keeper.tick().await;
        rig.started(1).await;
        let status = rig.status();
        assert_eq!((status.state, status.model, status.reason.as_str()), (ServiceState::Loading, Some(inkling), ""));
        keeper.stop().await;

        // A model with its licence alone never meets the question.
        rig.want(Some(&Wanted::new(MODEL)));
        keeper.tick().await;
        rig.started(2).await;
        assert_eq!(rig.status().state, ServiceState::Loading);
        keeper.stop().await;
    }

    #[tokio::test]
    async fn a_server_that_cannot_be_started_counts_as_one_that_ended() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("nobinary", "exec sleep 30");
        std::fs::remove_file(&rig.paths.server).unwrap();
        rig.want(Some(&Wanted::new(MODEL)));
        let mut keeper = rig.keeper();
        keeper.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.exits), (ServiceState::Unavailable, 1));
        assert!(status.reason.starts_with("the model server could not be started ("), "{}", status.reason);
        keeper.tick().await;
        keeper.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.exits), (ServiceState::Halted, 3));
        assert!(status.reason.starts_with("the model server ended 3 times before it answered (it could not be started, "), "{}", status.reason);
        assert!(status.reason.ends_with("); nothing more is tried until the settings change"), "{}", status.reason);
    }

    #[tokio::test]
    async fn a_server_that_ends_after_it_answered_is_started_again_and_the_count_begins_anew() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("again", "exec sleep 30");
        rig.want(Some(&Wanted::new(MODEL)));
        let mut keeper = rig.keeper();
        let _ready = rig.health("HTTP/1.1 200 OK");
        for round in 1..=4u32 {
            keeper.tick().await;
            rig.started(round as usize).await;
            keeper.tick().await;
            assert_eq!(rig.status().state, ServiceState::Ready, "round {round}");
            // The kernel ends it, as it does when memory runs out.
            let pid = keeper.pid().unwrap();
            nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid as i32), nix::sys::signal::Signal::SIGKILL).unwrap();
            for _ in 0..2_000 {
                if keeper.server.as_mut().is_some_and(|s| s.child.try_wait().ok().flatten().is_some()) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
        }
        keeper.tick().await;
        rig.started(5).await;
        let status = rig.status();
        assert_eq!((status.state, status.exits), (ServiceState::Loading, 4), "four exits, each after an answer: never left");
        assert_eq!(status.reason, "the model server ended (signal 9: killed, by the kernel when memory ran out or by hand)", "no last line of one that was killed");
        keeper.stop().await;
    }

    /// The program is asked once in a boot, by the fixed question; the
    /// status names the backend while a server runs, across starts for
    /// other settings, and names none when no server runs.
    #[tokio::test]
    async fn the_backend_is_asked_of_the_program_once_and_the_status_names_it_while_a_server_runs() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("backend", "exec sleep 30");
        rig.want(Some(&Wanted::new(MODEL)));
        let mut keeper = rig.patient_keeper();
        assert_eq!(keeper.pids(), Vec::<u32>::new());
        keeper.tick().await;
        rig.started(1).await;
        // Both are this process's children, and the reaper leaves both alone.
        let server = keeper.pid().unwrap();
        let pids = keeper.pids();
        assert!(pids.len() == 2 && pids[0] == server && pids[1] != server, "the server and the program that is asked: {pids:?}");
        assert_eq!(rig.status().backend, None, "nobody has answered yet");

        rig.answered(&mut keeper).await;
        assert_eq!(rig.status().backend.as_deref(), Some("haswell"));
        assert_eq!(keeper.pids(), vec![server], "the program that answered is collected");
        let question = std::fs::read_to_string(rig.dir.join("question")).unwrap();
        assert_eq!(question.lines().collect::<Vec<_>>(), native::BACKEND_QUESTION);
        assert!(!rig.paths.backend_answer.exists(), "the answer is read and removed");
        // A question is no start: the server's own marks are the server's.
        let args = std::fs::read_to_string(rig.dir.join("args")).unwrap();
        assert_eq!(args.lines().collect::<Vec<_>>(), server_args(&Wanted::new(MODEL), &rig.paths, false));
        assert_eq!((rig.starts(), rig.questions()), (1, 1));

        // Other settings: another server, and the same processor. Nobody is
        // asked again, and the first status of the new server names it.
        rig.want(Some(&Wanted { model: MODEL, context: 8_192 }));
        keeper.tick().await;
        rig.started(2).await;
        let status = rig.status();
        assert_eq!((status.state, status.context, status.backend.as_deref()), (ServiceState::Loading, Some(8_192), Some("haswell")));
        assert_eq!(status.to_text().lines().filter(|line| *line == "backend=haswell").count(), 1);
        keeper.tick().await;
        assert_eq!(rig.questions(), 1);

        // No server, no backend to say anything of.
        rig.want(None);
        keeper.tick().await;
        let status = rig.status();
        assert_eq!((status.state, status.backend.as_deref()), (ServiceState::Off, None));
        assert!(!status.to_text().contains("backend="));
        assert_eq!(rig.questions(), 1);
    }

    /// A program that names no backend leaves the status without one, and
    /// is asked once more at the next start of a server.
    #[tokio::test]
    async fn a_program_that_names_no_backend_is_asked_again_at_the_next_start() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("unnamed", "exec sleep 30");
        rig.program_does("exec sleep 30", "echo 'Available devices:'\necho '  (none)'");
        rig.want(Some(&Wanted::new(MODEL)));
        let mut keeper = rig.patient_keeper();
        keeper.tick().await;
        rig.started(1).await;
        rig.answered(&mut keeper).await;
        assert!(matches!(keeper.backend, Backend::Unnamed));
        assert_eq!((rig.status().state, rig.status().backend), (ServiceState::Loading, None));
        assert!(!rig.paths.backend_answer.exists());
        // The same server goes on: nobody is asked at every tick.
        for _ in 0..3 {
            keeper.tick().await;
        }
        assert_eq!(rig.questions(), 1);

        // A start for other settings asks once more, and this time the
        // program names one.
        rig.server_does("exec sleep 30");
        rig.want(Some(&Wanted { model: MODEL, context: 8_192 }));
        keeper.tick().await;
        rig.started(2).await;
        rig.answered(&mut keeper).await;
        assert_eq!((rig.questions(), rig.status().backend.as_deref()), (2, Some("haswell")));
        keeper.stop().await;
    }

    /// A program that does not end is looked at a few times and then ended:
    /// the reaper is not kept from its sweep by a child nobody collects.
    #[tokio::test]
    async fn a_question_that_does_not_end_is_ended_after_its_looks() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("stuck", "exec sleep 30");
        rig.program_does("exec sleep 30", "exec sleep 30");
        rig.want(Some(&Wanted::new(MODEL)));
        let mut keeper = rig.keeper();
        keeper.backend_ticks_max = 3;
        keeper.tick().await;
        rig.started(1).await;
        let server = keeper.pid().unwrap();
        let asked = keeper.pids().into_iter().find(|pid| *pid != server).expect("the program is asked");
        // Two looks leave it; the third ends it.
        keeper.tick().await;
        keeper.tick().await;
        assert!(matches!(keeper.backend, Backend::Asked(_, 2)));
        assert!(Path::new(&format!("/proc/{asked}")).exists());
        keeper.tick().await;
        assert!(matches!(keeper.backend, Backend::Unnamed));
        assert!(!Path::new(&format!("/proc/{asked}")).exists(), "the program that was asked is gone");
        assert_eq!(keeper.pids(), vec![server]);
        assert_eq!((rig.status().state, rig.status().backend), (ServiceState::Loading, None));
        assert!(!rig.paths.backend_answer.exists());
        keeper.stop().await;
    }

    /// With native models switched off nobody is asked, and a machine that
    /// keeps no model loaded asks nobody either.
    #[tokio::test]
    async fn nobody_is_asked_while_no_server_is_started() {
        let _children = crate::supervisor::test_support::CHILDREN.lock().await;
        let rig = Rig::new("unasked", "exec sleep 30");
        let mut keeper = rig.keeper();
        rig.want(None);
        for _ in 0..3 {
            keeper.tick().await;
        }
        rig.want(Some(&Wanted::new(MODEL)));
        let mut disabled = Keeper::with_paths(false, rig.paths.clone());
        for _ in 0..3 {
            disabled.tick().await;
        }
        assert_eq!((rig.starts(), rig.questions()), (0, 0));
        assert!(matches!(keeper.backend, Backend::Unasked) && matches!(disabled.backend, Backend::Unasked));
    }

    /// `since` is when the state began: it moves with the state, and stays
    /// when the backend's name is all that is new.
    #[test]
    fn the_backends_name_arriving_is_no_new_state() {
        let mut loading = Status::of(ServiceState::Loading, 0);
        (loading.model, loading.context, loading.pid) = (Some(MODEL), Some(65_536), Some(412));
        // The first status is stamped.
        let first = to_publish(None, loading.clone(), 100).unwrap();
        assert_eq!(first.since, 100);
        // The same again: nothing to write, however late.
        assert_eq!(to_publish(Some(&first), loading.clone(), 160), None);
        // The name arrives: written, and the state began when it began.
        let named = Status { backend: Some("haswell".to_string()), ..loading.clone() };
        let second = to_publish(Some(&first), named.clone(), 160).unwrap();
        assert_eq!((second.since, second.backend.as_deref()), (100, Some("haswell")));
        assert_eq!(to_publish(Some(&second), named.clone(), 200), None);
        // The state moves: stamped anew, with the name or without.
        let ready = Status { state: ServiceState::Ready, ..named.clone() };
        assert_eq!(to_publish(Some(&second), ready.clone(), 200).unwrap().since, 200);
        let both = Status { state: ServiceState::Ready, ..named };
        assert_eq!(to_publish(Some(&first), both, 300).unwrap().since, 300, "the name and the state at once");
        // The name goes with the server it was said of: a new state.
        let off = Status::of(ServiceState::Off, 0);
        assert_eq!(to_publish(Some(&second), off, 400).unwrap().since, 400);
    }

    #[test]
    fn the_log_is_cut_back_to_its_end() {
        let dir = std::env::temp_dir().join(format!("embrad-native-log-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let log = dir.join("native-model.log");
        std::fs::write(&log, "short\n").unwrap();
        trim_log(&log);
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "short\n", "under the limit it is left");
        assert_eq!(last_log_line(&log).as_deref(), Some("short"));

        let mut long = vec![b'a'; (LOG_MAX + 10) as usize];
        long.extend_from_slice(b"\nthe last line\n\n");
        std::fs::write(&log, &long).unwrap();
        trim_log(&log);
        let kept = std::fs::read(&log).unwrap();
        assert!(kept.len() as u64 > LOG_KEEP && (kept.len() as u64) < LOG_KEEP + 200, "{}", kept.len());
        assert!(kept.starts_with(b"[embrad] the log passed 8 MiB; its last 1 MiB follows\n"));
        assert_eq!(last_log_line(&log).as_deref(), Some("the last line"));
        assert_eq!(last_log_line(&dir.join("none")), None);
        std::fs::remove_dir_all(&dir).unwrap();
    }
}
