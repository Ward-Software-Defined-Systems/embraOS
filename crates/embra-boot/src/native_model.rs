//! Native models (experimental): what embrad, the brain and the updater
//! agree on. A native model is a GGUF file on DATA that a build of
//! llama.cpp's server runs on this machine, on loopback alone; the brain
//! reaches it as one more OpenAI-compatible endpoint.
//!
//! The catalogue names the models: each is two files of one revision of a
//! repository, the model itself and the file it reads images through (its
//! projector), each with its size and its sha256, so what runs is what was
//! tested. A model whose image file is not on the machine runs and reads
//! text alone.
//!
//! One job at a time changes the models' directory (an install, an import,
//! a check, a removal): a lock on a file in it, which the updater takes and
//! any reader can ask about.
//!
//! Three records, one `key=value` a line. Each has one parser, and a record
//! is taken whole or not at all: a line the parser does not accept refuses
//! the record, and the reason is a sentence for the operator.
//!
//! ```text
//! /embra/state/native-model/settings        the brain writes, embrad follows
//!     model=qwen3.5-4b
//!     context=65536
//! /run/embra/native-model-status            embrad writes, the brain reads
//!     state=ready
//!     model=qwen3.5-4b
//!     context=65536
//!     pid=412
//!     since=96
//!     exits=0
//!     images=yes                            the server was started with the image file
//! /embra/data/models/qwen3.5-4b/verified    the updater writes, embrad reads
//!     file=Qwen3.5-4B-Q4_K_M.gguf
//!     bytes=2740937888
//!     sha256=00fe7986…
//!     mtime=1763829000.123456789
//! /embra/data/models/qwen3.5-4b/verified-images    the same, of the image file
//! ```

use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

/// The address the model server listens on: loopback alone. The brain is
/// its one client, and it takes no key.
pub const HOST: &str = "127.0.0.1";
/// Its port, beside the database's 8090.
pub const PORT: u16 = 8091;
/// The base the brain's requests are built on.
pub const BASE_URL: &str = "http://127.0.0.1:8091";

/// The model server, a build of llama.cpp's `llama-server`.
pub const SERVER_PATH: &str = "/usr/bin/llama-server";
/// The CPU backends it loads one of when it starts, by what the processor
/// can do.
pub const BACKEND_DIR: &str = "/usr/lib/llama-cpp";

/// The model files, a directory a model. On DATA, and outside the snapshots
/// and restores of STATE and DATA: a model is downloaded again, not backed
/// up.
pub const MODELS_DIR: &str = "/embra/data/models";
/// The directory of the settings on STATE.
pub const SETTINGS_DIR: &str = "/embra/state/native-model";
/// The settings: the model embrad keeps loaded.
pub const SETTINGS_PATH: &str = "/embra/state/native-model/settings";
/// What embrad made of the settings.
pub const STATUS_PATH: &str = "/run/embra/native-model-status";
/// What the model server printed since this boot.
pub const LOG_PATH: &str = "/embra/ephemeral/native-model.log";

/// Where the model files are downloaded from.
pub const DOWNLOAD_BASE: &str = "https://huggingface.co";

/// The context window a model is given when the operator names none
/// (William, 2026-11-22).
pub const DEFAULT_CONTEXT: u32 = 65_536;
/// The smallest window a model can be given. The server runs with less, and
/// nothing embraOS sends fits under it; 0 would mean the model's own largest.
pub const SMALLEST_CONTEXT: u32 = 1_024;
/// The tokens of an image for the catalogue's models: what their server asks
/// for at least ("Qwen-VL models require at minimum 1024 image tokens"), and
/// at most as well. Measured with the 4B: small text misread at the
/// server's own default, read right at this.
pub const IMAGE_TOKENS: u32 = 1_024;

/// The name of a model's verification record, in the model's directory.
const VERIFIED_FILE: &str = "verified";
/// The record of its image file, beside it.
const VERIFIED_IMAGES_FILE: &str = "verified-images";
/// The file a model job locks, in the models' directory.
const JOB_LOCK_FILE: &str = ".lock";
/// What a download is called until it is whole: the file's name and this.
const PARTIAL_SUFFIX: &str = ".partial";
/// How much of a record's line a refusal quotes.
const SHOWN: usize = 120;
/// How long a status's reason is at most.
const REASON_MAX: usize = 300;

/// One of a model's two files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Part {
    /// The model itself.
    Model,
    /// What the model reads images through (its projector). Without it the
    /// model runs and reads text alone.
    Images,
}

impl Part {
    /// Both, the model first: it is what a machine needs to run one at all.
    pub const ALL: [Part; 2] = [Part::Model, Part::Images];

    /// What the operator reads.
    pub fn words(self) -> &'static str {
        match self {
            Part::Model => "model file",
            Part::Images => "image file",
        }
    }
}

/// A file of the catalogue: its name, in the repository and on DATA, its
/// size and its sha256, lower-case hex.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CatalogueFile {
    pub file: &'static str,
    pub bytes: u64,
    pub sha256: &'static str,
}

/// A model of the catalogue: two files of one revision of a repository.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NativeModel {
    /// The name the operator types, the model's directory, and the name the
    /// server answers to.
    pub id: &'static str,
    /// The name the operator reads.
    pub label: &'static str,
    /// The file's name, in the repository and on DATA.
    pub file: &'static str,
    /// How the weights were quantised, for the listing.
    pub quantisation: &'static str,
    /// The file's size.
    pub bytes: u64,
    /// The file's sha256, lower-case hex.
    pub sha256: &'static str,
    /// The file the model reads images through, of the same revision.
    pub images: CatalogueFile,
    /// How many tokens the model server makes of an image, whatever its
    /// size: the server's own figure for these models is too few for small
    /// text (it says so when it loads one), and without a ceiling a large
    /// image is four times the tokens and six times the wait.
    pub image_tokens: u32,
    /// The repository under [`DOWNLOAD_BASE`].
    pub repository: &'static str,
    /// The repository's revision the file is taken from.
    pub revision: &'static str,
    /// The licence of the weights.
    pub licence: &'static str,
    /// The window it is given when the operator names none.
    pub default_context: u32,
    /// The largest window the model was trained for.
    pub largest_context: u32,
}

/// The native models. Each file was downloaded whole and hashed before its
/// values were written here (2026-11-22).
pub const CATALOGUE: [NativeModel; 2] = [
    NativeModel {
        id: "qwen3.5-4b",
        label: "Qwen3.5 4B",
        file: "Qwen3.5-4B-Q4_K_M.gguf",
        quantisation: "Q4_K_M",
        bytes: 2_740_937_888,
        sha256: "00fe7986ff5f6b463e62455821146049db6f9313603938a70800d1fb69ef11a4",
        images: CatalogueFile {
            file: "mmproj-F16.gguf",
            bytes: 672_423_616,
            sha256: "cd88edcf8d031894960bb0c9c5b9b7e1fea6ebee02b9f7ce925a00d12891f864",
        },
        image_tokens: IMAGE_TOKENS,
        repository: "unsloth/Qwen3.5-4B-GGUF",
        revision: "e87f176479d0855a907a41277aca2f8ee7a09523",
        licence: "Apache-2.0",
        default_context: DEFAULT_CONTEXT,
        largest_context: 262_144,
    },
    NativeModel {
        id: "qwen3.5-9b",
        label: "Qwen3.5 9B",
        file: "Qwen3.5-9B-Q4_K_M.gguf",
        quantisation: "Q4_K_M",
        bytes: 5_680_522_464,
        sha256: "03b74727a860a56338e042c4420bb3f04b2fec5734175f4cb9fa853daf52b7e8",
        images: CatalogueFile {
            file: "mmproj-F16.gguf",
            bytes: 918_166_080,
            sha256: "f70dc3509053962b0d0d3ee8a7eacebf5d60aa560cad78254ae8698516ae029f",
        },
        image_tokens: IMAGE_TOKENS,
        repository: "unsloth/Qwen3.5-9B-GGUF",
        revision: "3885219b6810b007914f3a7950a8d1b469d598a5",
        licence: "Apache-2.0",
        default_context: DEFAULT_CONTEXT,
        largest_context: 262_144,
    },
];

/// The model of this name.
pub fn by_id(id: &str) -> Option<&'static NativeModel> {
    CATALOGUE.iter().find(|m| m.id == id)
}

impl NativeModel {
    /// One of the model's two files, as the catalogue names it.
    pub fn part(&self, part: Part) -> CatalogueFile {
        match part {
            Part::Model => CatalogueFile { file: self.file, bytes: self.bytes, sha256: self.sha256 },
            Part::Images => self.images,
        }
    }

    /// Which of the model's files has this size, when one has: how an
    /// upload that names the model says which file it brings.
    pub fn part_of_size(&self, bytes: u64) -> Option<Part> {
        Part::ALL.into_iter().find(|part| self.part(*part).bytes == bytes)
    }

    /// Where a file is downloaded from: the pinned revision, never a
    /// branch.
    pub fn url_of(&self, part: Part) -> String {
        format!("{DOWNLOAD_BASE}/{}/resolve/{}/{}", self.repository, self.revision, self.part(part).file)
    }

    /// Where the model file is downloaded from.
    pub fn url(&self) -> String {
        self.url_of(Part::Model)
    }

    /// The model's directory under `models` ([`MODELS_DIR`] on a machine).
    pub fn dir(&self, models: &Path) -> PathBuf {
        models.join(self.id)
    }

    /// One of the model's files.
    pub fn path_of(&self, part: Part, models: &Path) -> PathBuf {
        self.dir(models).join(self.part(part).file)
    }

    /// Its download until it is whole.
    pub fn partial_of(&self, part: Part, models: &Path) -> PathBuf {
        self.dir(models).join(format!("{}{PARTIAL_SUFFIX}", self.part(part).file))
    }

    /// Its verification record.
    pub fn verified_of(&self, part: Part, models: &Path) -> PathBuf {
        self.dir(models).join(match part {
            Part::Model => VERIFIED_FILE,
            Part::Images => VERIFIED_IMAGES_FILE,
        })
    }

    /// The model file.
    pub fn file_path(&self, models: &Path) -> PathBuf {
        self.path_of(Part::Model, models)
    }

    /// The model file's download until it is whole.
    pub fn partial_path(&self, models: &Path) -> PathBuf {
        self.partial_of(Part::Model, models)
    }

    /// The model file's verification record.
    pub fn verified_path(&self, models: &Path) -> PathBuf {
        self.verified_of(Part::Model, models)
    }

    /// A window this model can be given. The error is a sentence for the
    /// operator.
    pub fn context_ok(&self, context: u32) -> Result<u32, String> {
        if (SMALLEST_CONTEXT..=self.largest_context).contains(&context) {
            Ok(context)
        } else {
            Err(format!(
                "{context} is not a context window for {}: {SMALLEST_CONTEXT} to {} tokens",
                self.label, self.largest_context
            ))
        }
    }
}

/// The lines of a record that carry a setting, each as `(key, value, the
/// line as a refusal quotes it)`; a line without `=` refuses the record.
fn fields(text: &str) -> Result<Vec<(&str, &str, String)>, String> {
    let mut out = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let shown: String = line.chars().take(SHOWN).collect();
        let Some((key, value)) = line.split_once('=') else {
            return Err(format!("{shown}: not a line of the record"));
        };
        out.push((key.trim(), value.trim(), shown));
    }
    Ok(out)
}

/// Sets a field once: a second line for it refuses the record.
fn once<T>(slot: &mut Option<T>, value: T, shown: &str) -> Result<(), String> {
    if slot.is_some() {
        return Err(format!("{shown}: named twice"));
    }
    *slot = Some(value);
    Ok(())
}

/// Writes a record whole: a temporary file beside the target, synced, then
/// renamed over it, so a reader never sees half a record.
fn write_whole(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let tmp = path.with_extension("tmp");
    let mut file = std::fs::File::create(&tmp)?;
    file.write_all(text.as_bytes())?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&tmp, path)
}

/// A record's text, or `None` when there is no file.
fn read_record(path: &Path, what: &str) -> Result<Option<String>, String> {
    match std::fs::read_to_string(path) {
        Ok(text) => Ok(Some(text)),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(e) => Err(format!("{what} could not be read: {e}")),
    }
}

// ---------------------------------------------------------------------------
// The job lock: the updater takes it, anyone asks about it.

/// A lock over the whole of an open file, of the kind that belongs to the
/// open file and not to the process (`F_OFD_SETLK`): it goes when the file
/// is closed, however its holder ends, and whether one is held can be asked
/// without taking one (`F_OFD_GETLK`).
fn whole_file(kind: libc::c_int) -> libc::flock {
    // SAFETY: a struct of integers; zero is a valid value of each.
    let mut lock: libc::flock = unsafe { std::mem::zeroed() };
    lock.l_type = kind as libc::c_short;
    lock.l_whence = libc::SEEK_SET as libc::c_short;
    lock
}

/// Take the models' job lock. `Some` is the lock, held until the file is
/// dropped; `None` when another job holds it. The directory is made when it
/// is not there.
pub fn job_lock(models: &Path) -> std::io::Result<Option<std::fs::File>> {
    use std::os::unix::io::AsRawFd;
    std::fs::create_dir_all(models)?;
    let file = std::fs::OpenOptions::new().create(true).truncate(false).write(true).open(models.join(JOB_LOCK_FILE))?;
    let lock = whole_file(libc::F_WRLCK);
    // SAFETY: a valid descriptor and a lock description that lives for the call.
    if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_SETLK, &lock) } == 0 {
        return Ok(Some(file));
    }
    let e = std::io::Error::last_os_error();
    match e.raw_os_error() {
        Some(libc::EAGAIN) | Some(libc::EACCES) => Ok(None),
        _ => Err(e),
    }
}

/// Whether a model job runs now: its lock is held. Nothing is taken to find
/// out, so a job that starts at the same moment is not refused for it.
pub fn job_running(models: &Path) -> bool {
    use std::os::unix::io::AsRawFd;
    let Ok(file) = std::fs::File::open(models.join(JOB_LOCK_FILE)) else { return false };
    let mut lock = whole_file(libc::F_RDLCK);
    // SAFETY: a valid descriptor and a lock description the call writes into.
    let asked = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_OFD_GETLK, &mut lock) };
    asked == 0 && lock.l_type != libc::F_UNLCK as libc::c_short
}

// ---------------------------------------------------------------------------
// The settings: the brain writes, embrad follows.

/// The model the operator asked to be kept loaded, and its window.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Wanted {
    pub model: &'static NativeModel,
    pub context: u32,
}

impl Wanted {
    /// The model with its own default window.
    pub fn new(model: &'static NativeModel) -> Self {
        Wanted { model, context: model.default_context }
    }
}

/// Parses the settings: the model wanted, or `None` when the record names
/// none. A record without a `context` line gives the model its default.
pub fn parse_settings(text: &str) -> Result<Option<Wanted>, String> {
    let (mut model, mut context) = (None, None);
    for (key, value, shown) in fields(text)? {
        match key {
            "model" => {
                let named = by_id(value).ok_or_else(|| format!("{shown}: not a native model this release knows"))?;
                once(&mut model, named, &shown)?;
            }
            "context" => {
                let tokens: u32 = value.parse().map_err(|_| format!("{shown}: not a number of tokens"))?;
                once(&mut context, tokens, &shown)?;
            }
            _ => return Err(format!("{shown}: not a setting this release knows")),
        }
    }
    match (model, context) {
        (None, None) => Ok(None),
        (None, Some(_)) => Err("a context window without a model".to_string()),
        (Some(model), context) => {
            let context = model.context_ok(context.unwrap_or(model.default_context))?;
            Ok(Some(Wanted { model, context }))
        }
    }
}

/// The settings' text.
pub fn settings_text(wanted: Option<&Wanted>) -> String {
    let mut text = String::from(
        "# embraOS native model (experimental), written by /native. embrad follows a change within seconds.\n",
    );
    if let Some(wanted) = wanted {
        text.push_str(&format!("model={}\ncontext={}\n", wanted.model.id, wanted.context));
    }
    text
}

/// The settings on this machine: no file is no model wanted.
pub fn read_settings(path: &Path) -> Result<Option<Wanted>, String> {
    match read_record(path, "the native model's settings")? {
        Some(text) => parse_settings(&text),
        None => Ok(None),
    }
}

/// Writes the settings whole; `None` writes a record that names no model.
pub fn write_settings(path: &Path, wanted: Option<&Wanted>) -> std::io::Result<()> {
    write_whole(path, &settings_text(wanted))
}

// ---------------------------------------------------------------------------
// The status: embrad writes, the brain reads.

/// What became of the settings.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ServiceState {
    /// No model is asked for; no server runs.
    Off,
    /// The kernel line switched native models off (`embra.native=0`).
    Disabled,
    /// A model is asked for and cannot be run: its file is not installed, or
    /// changed since it was checked, or the settings were refused. The
    /// reason says which.
    Unavailable,
    /// The server runs and is reading the model.
    Loading,
    /// The server answers.
    Ready,
    /// The server ended again and again before it answered; nothing more is
    /// tried until the settings change.
    Halted,
}

impl ServiceState {
    pub const ALL: [ServiceState; 6] = [
        ServiceState::Off,
        ServiceState::Disabled,
        ServiceState::Unavailable,
        ServiceState::Loading,
        ServiceState::Ready,
        ServiceState::Halted,
    ];

    pub fn as_str(self) -> &'static str {
        match self {
            ServiceState::Off => "off",
            ServiceState::Disabled => "disabled",
            ServiceState::Unavailable => "unavailable",
            ServiceState::Loading => "loading",
            ServiceState::Ready => "ready",
            ServiceState::Halted => "halted",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        Self::ALL.into_iter().find(|s| s.as_str() == text)
    }

    /// Whether a server process stands behind the state.
    pub fn runs(self) -> bool {
        match self {
            ServiceState::Loading | ServiceState::Ready => true,
            ServiceState::Off | ServiceState::Disabled | ServiceState::Unavailable | ServiceState::Halted => false,
        }
    }
}

/// What embrad says of the model server.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Status {
    pub state: ServiceState,
    /// The model the state is about, when the settings name one.
    pub model: Option<&'static NativeModel>,
    /// Its window.
    pub context: Option<u32>,
    /// The server's process, while one runs.
    pub pid: Option<u32>,
    /// When the state began, in seconds since the machine booted
    /// ([`uptime_secs`]): a clock that does not jump when the time is set.
    pub since: u64,
    /// How often the server ended since the settings last changed.
    pub exits: u32,
    /// Whether the server was started with the model's image file: with it
    /// the model reads images, without it text alone.
    pub images: bool,
    /// One line for the operator: why the model cannot be run, or how the
    /// server's last run ended. Empty when there is nothing to say.
    pub reason: String,
}

impl Status {
    /// A state with nothing else to say about it.
    pub fn of(state: ServiceState, since: u64) -> Self {
        Status { state, model: None, context: None, pid: None, since, exits: 0, images: false, reason: String::new() }
    }

    /// The record's text. The reason is kept to one line.
    pub fn to_text(&self) -> String {
        let mut text = format!("state={}\n", self.state.as_str());
        if let Some(model) = self.model {
            text.push_str(&format!("model={}\n", model.id));
        }
        if let Some(context) = self.context {
            text.push_str(&format!("context={context}\n"));
        }
        if let Some(pid) = self.pid {
            text.push_str(&format!("pid={pid}\n"));
        }
        text.push_str(&format!("since={}\nexits={}\n", self.since, self.exits));
        if self.images {
            text.push_str("images=yes\n");
        }
        let reason = one_line(&self.reason);
        if !reason.is_empty() {
            text.push_str(&format!("reason={reason}\n"));
        }
        text
    }
}

/// A reason as the record carries it: one line, [`REASON_MAX`] characters at
/// most.
pub fn one_line(reason: &str) -> String {
    let flat: String = reason.split_whitespace().collect::<Vec<_>>().join(" ");
    flat.chars().take(REASON_MAX).collect()
}

/// Parses the status. A state with a server behind it names its model, its
/// window and its process.
pub fn parse_status(text: &str) -> Result<Status, String> {
    let (mut state, mut model, mut context, mut pid) = (None, None, None, None);
    let (mut since, mut exits, mut reason, mut images) = (None, None, None, None);
    for (key, value, shown) in fields(text)? {
        match key {
            "images" => once(
                &mut images,
                match value {
                    "yes" => true,
                    "no" => false,
                    _ => return Err(format!("{shown}: yes or no")),
                },
                &shown,
            )?,
            "state" => once(&mut state, ServiceState::parse(value).ok_or_else(|| format!("{shown}: not a state"))?, &shown)?,
            "model" => once(&mut model, by_id(value).ok_or_else(|| format!("{shown}: not a native model this release knows"))?, &shown)?,
            "context" => once(&mut context, value.parse::<u32>().map_err(|_| format!("{shown}: not a number of tokens"))?, &shown)?,
            "pid" => once(
                &mut pid,
                value.parse::<u32>().ok().filter(|p| *p > 0).ok_or_else(|| format!("{shown}: not a process"))?,
                &shown,
            )?,
            "since" => once(&mut since, value.parse::<u64>().map_err(|_| format!("{shown}: not a number of seconds"))?, &shown)?,
            "exits" => once(&mut exits, value.parse::<u32>().map_err(|_| format!("{shown}: not a count"))?, &shown)?,
            "reason" => once(&mut reason, one_line(value), &shown)?,
            _ => return Err(format!("{shown}: not a field this release knows")),
        }
    }
    let state = state.ok_or("the status names no state")?;
    if state.runs() && (model.is_none() || context.is_none() || pid.is_none()) {
        return Err(format!("the status is {} without a model, a window and a process", state.as_str()));
    }
    Ok(Status {
        state,
        model,
        context,
        pid,
        since: since.unwrap_or(0),
        exits: exits.unwrap_or(0),
        images: images.unwrap_or(false),
        reason: reason.unwrap_or_default(),
    })
}

/// The status on this machine; `None` when embrad has written none.
pub fn read_status(path: &Path) -> Result<Option<Status>, String> {
    match read_record(path, "the native model's status")? {
        Some(text) => parse_status(&text).map(Some),
        None => Ok(None),
    }
}

/// Writes the status whole.
pub fn write_status(path: &Path, status: &Status) -> std::io::Result<()> {
    write_whole(path, &status.to_text())
}

/// Seconds since the machine booted, the clock of a status's `since`. 0 where
/// the kernel does not say.
pub fn uptime_secs() -> u64 {
    std::fs::read_to_string("/proc/uptime")
        .ok()
        .and_then(|text| text.split(['.', ' ']).next().and_then(|secs| secs.trim().parse().ok()))
        .unwrap_or(0)
}

// ---------------------------------------------------------------------------
// The verification record: the updater writes, embrad reads.

/// What the updater recorded when it had hashed a model file: the file as it
/// was then. A start compares the file's size and time with it and hashes
/// nothing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Verified {
    /// The file's name.
    pub file: String,
    /// Its size.
    pub bytes: u64,
    /// Its sha256, lower-case hex.
    pub sha256: String,
    /// Its modification time when it was hashed: seconds and nanoseconds.
    pub mtime: (i64, i64),
}

fn sha256_ok(text: &str) -> bool {
    text.len() == 64 && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A file's name and nothing more: no directory, no parent.
fn file_name_ok(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && name.bytes().all(|b| b.is_ascii_graphic() && b != b'/')
}

impl Verified {
    /// The record of a catalogue file whose hash was just found right:
    /// `meta` is the file's own.
    pub fn of_part(model: &NativeModel, part: Part, meta: &std::fs::Metadata) -> Self {
        let file = model.part(part);
        Verified {
            file: file.file.to_string(),
            bytes: meta.len(),
            sha256: file.sha256.to_string(),
            mtime: (meta.mtime(), meta.mtime_nsec()),
        }
    }

    /// The record of the model file.
    pub fn of(model: &NativeModel, meta: &std::fs::Metadata) -> Self {
        Self::of_part(model, Part::Model, meta)
    }

    /// The record's text.
    pub fn to_text(&self) -> String {
        format!(
            "file={}\nbytes={}\nsha256={}\nmtime={}.{:09}\n",
            self.file, self.bytes, self.sha256, self.mtime.0, self.mtime.1
        )
    }

    /// Whether the record is of this catalogue file.
    fn names(&self, file: &CatalogueFile) -> bool {
        self.file == file.file && self.bytes == file.bytes && self.sha256 == file.sha256
    }
}

/// Parses a verification record: its four fields, each once.
pub fn parse_verified(text: &str) -> Result<Verified, String> {
    let (mut file, mut bytes, mut sha256, mut mtime) = (None, None, None, None);
    for (key, value, shown) in fields(text)? {
        match key {
            "file" if file_name_ok(value) => once(&mut file, value.to_string(), &shown)?,
            "file" => return Err(format!("{shown}: not a file's name")),
            "bytes" => once(&mut bytes, value.parse::<u64>().map_err(|_| format!("{shown}: not a size"))?, &shown)?,
            "sha256" if sha256_ok(value) => once(&mut sha256, value.to_string(), &shown)?,
            "sha256" => return Err(format!("{shown}: not a sha256")),
            "mtime" => {
                let time = value
                    .split_once('.')
                    .and_then(|(secs, nanos)| {
                        let nanos_ok = nanos.len() == 9 && nanos.bytes().all(|b| b.is_ascii_digit());
                        Some((secs.parse::<i64>().ok()?, nanos_ok.then(|| nanos.parse::<i64>().ok())??))
                    })
                    .ok_or_else(|| format!("{shown}: not a time"))?;
                once(&mut mtime, time, &shown)?;
            }
            _ => return Err(format!("{shown}: not a field this release knows")),
        }
    }
    match (file, bytes, sha256, mtime) {
        (Some(file), Some(bytes), Some(sha256), Some(mtime)) => Ok(Verified { file, bytes, sha256, mtime }),
        _ => Err("the record lacks one of file, bytes, sha256 and mtime".to_string()),
    }
}

/// Writes a model's verification record whole.
pub fn write_verified(path: &Path, verified: &Verified) -> std::io::Result<()> {
    write_whole(path, &verified.to_text())
}

/// How a model's file stands on a machine.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Installed {
    /// No file.
    No,
    /// A download that stopped: so many bytes of it are there.
    Partial(u64),
    /// A file is there that no valid record vouches for; the sentence says
    /// why. Hashing it again settles it.
    Unverified(String),
    /// The catalogue's file, hashed, and unchanged since.
    Verified,
}

/// How `model`'s file stands under `models`: what is there, and whether the
/// record beside it still describes it. Nothing is hashed.
pub fn installed(model: &NativeModel, models: &Path) -> Installed {
    installed_part(model, Part::Model, models)
}

/// The same of either of the model's files.
pub fn installed_part(model: &NativeModel, part: Part, models: &Path) -> Installed {
    let meta = match std::fs::metadata(model.path_of(part, models)) {
        Ok(meta) => meta,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return match std::fs::metadata(model.partial_of(part, models)) {
                Ok(partial) if partial.is_file() => Installed::Partial(partial.len()),
                _ => Installed::No,
            };
        }
        Err(e) => return Installed::Unverified(format!("the file cannot be read: {e}")),
    };
    if !meta.is_file() {
        return Installed::Unverified("what stands in the file's place is not a file".to_string());
    }
    let record = match read_record(&model.verified_of(part, models), "the verification record") {
        Ok(Some(text)) => parse_verified(&text),
        Ok(None) => return Installed::Unverified("the file was never checked".to_string()),
        Err(reason) => Err(reason),
    };
    let record = match record {
        Ok(record) => record,
        Err(reason) => return Installed::Unverified(format!("the verification record is not valid ({reason})")),
    };
    if !record.names(&model.part(part)) {
        return Installed::Unverified("the file was checked as another release's file".to_string());
    }
    if meta.len() != record.bytes || (meta.mtime(), meta.mtime_nsec()) != record.mtime {
        return Installed::Unverified("the file changed since it was checked".to_string());
    }
    Installed::Verified
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str, len: usize) -> bool {
        text.len() == len && text.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }

    #[test]
    fn the_catalogue_pins_two_files_of_one_revision_per_model() {
        let ids: Vec<&str> = CATALOGUE.iter().map(|m| m.id).collect();
        assert_eq!(ids, ["qwen3.5-4b", "qwen3.5-9b"]);
        for model in &CATALOGUE {
            assert_eq!(by_id(model.id), Some(model));
            // The id is a directory's name and the name the server answers to.
            assert!(file_name_ok(model.id) && model.id.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'.' || b == b'-'), "{}", model.id);
            assert!(file_name_ok(model.file) && model.file.ends_with(".gguf"), "{}", model.file);
            assert!(hex(model.sha256, 64), "{}: sha256", model.id);
            assert!(hex(model.revision, 40), "{}: a revision is a commit, never a branch", model.id);
            assert!(model.bytes > 1 << 30, "{}", model.id);
            assert_eq!(model.default_context, DEFAULT_CONTEXT);
            assert!(model.context_ok(model.default_context).is_ok());
            assert!(SMALLEST_CONTEXT < model.default_context && model.default_context <= model.largest_context);
            assert_eq!(
                model.url(),
                format!("https://huggingface.co/{}/resolve/{}/{}", model.repository, model.revision, model.file)
            );
            assert!(!model.label.is_empty() && !model.licence.is_empty() && !model.quantisation.is_empty());
            // Its image file: another file of the same revision, with a name
            // and a size of its own, so that a size alone says which of the
            // two an upload brings.
            let images = model.part(Part::Images);
            assert_eq!(images, model.images);
            assert_eq!(model.part(Part::Model), CatalogueFile { file: model.file, bytes: model.bytes, sha256: model.sha256 });
            assert!(file_name_ok(images.file) && images.file.ends_with(".gguf") && images.file != model.file, "{}", images.file);
            assert!(hex(images.sha256, 64) && images.sha256 != model.sha256, "{}: the image file's sha256", model.id);
            assert!(images.bytes > 100 << 20 && images.bytes < model.bytes, "{}", model.id);
            assert_eq!((model.part_of_size(model.bytes), model.part_of_size(images.bytes), model.part_of_size(1)), (Some(Part::Model), Some(Part::Images), None));
            assert_eq!(
                model.url_of(Part::Images),
                format!("https://huggingface.co/{}/resolve/{}/{}", model.repository, model.revision, images.file)
            );
            assert_eq!(model.image_tokens, IMAGE_TOKENS);
        }
        assert_eq!((CATALOGUE[0].bytes, CATALOGUE[1].bytes), (2_740_937_888, 5_680_522_464));
        assert_eq!((CATALOGUE[0].images.bytes, CATALOGUE[1].images.bytes), (672_423_616, 918_166_080));
        // No two files of the catalogue have one size: an upload that names
        // no model is told apart by its size as well.
        let mut sizes: Vec<u64> = CATALOGUE.iter().flat_map(|m| Part::ALL.map(|part| m.part(part).bytes)).collect();
        sizes.sort_unstable();
        sizes.dedup();
        assert_eq!(sizes.len(), CATALOGUE.len() * 2);
        assert_eq!((Part::Model.words(), Part::Images.words()), ("model file", "image file"));
        assert_eq!(by_id("qwen3.5-27b"), None);
        assert_eq!(by_id(""), None);
        assert_eq!(BASE_URL, format!("http://{HOST}:{PORT}"));
        assert!(SETTINGS_PATH.starts_with(SETTINGS_DIR));
    }

    #[test]
    fn a_models_paths_are_under_its_own_directory() {
        let model = &CATALOGUE[0];
        let models = Path::new("/embra/data/models");
        assert_eq!(model.dir(models), Path::new("/embra/data/models/qwen3.5-4b"));
        assert_eq!(model.file_path(models), Path::new("/embra/data/models/qwen3.5-4b/Qwen3.5-4B-Q4_K_M.gguf"));
        assert_eq!(model.partial_path(models), Path::new("/embra/data/models/qwen3.5-4b/Qwen3.5-4B-Q4_K_M.gguf.partial"));
        assert_eq!(model.verified_path(models), Path::new("/embra/data/models/qwen3.5-4b/verified"));
        // The image file stands beside it, with a download and a record of its own.
        assert_eq!(model.path_of(Part::Images, models), Path::new("/embra/data/models/qwen3.5-4b/mmproj-F16.gguf"));
        assert_eq!(model.partial_of(Part::Images, models), Path::new("/embra/data/models/qwen3.5-4b/mmproj-F16.gguf.partial"));
        assert_eq!(model.verified_of(Part::Images, models), Path::new("/embra/data/models/qwen3.5-4b/verified-images"));
        assert_eq!((model.path_of(Part::Model, models), model.verified_of(Part::Model, models)), (model.file_path(models), model.verified_path(models)));
    }

    #[test]
    fn a_window_is_between_the_smallest_and_the_models_largest() {
        let model = &CATALOGUE[0];
        assert_eq!(model.context_ok(SMALLEST_CONTEXT), Ok(SMALLEST_CONTEXT));
        assert_eq!(model.context_ok(262_144), Ok(262_144));
        for bad in [0, 1, SMALLEST_CONTEXT - 1, 262_145, u32::MAX] {
            let reason = model.context_ok(bad).unwrap_err();
            assert!(reason.contains("Qwen3.5 4B") && reason.contains("262144"), "{reason}");
        }
    }

    #[test]
    fn the_settings_read_back_and_no_model_is_a_record_too() {
        let wanted = Wanted { model: &CATALOGUE[1], context: 32_768 };
        assert_eq!(parse_settings(&settings_text(Some(&wanted))), Ok(Some(wanted)));
        assert_eq!(parse_settings(&settings_text(None)), Ok(None));
        assert_eq!(parse_settings(""), Ok(None));
        assert_eq!(parse_settings("\n# a note\n\n"), Ok(None));
        // Without a window the model gets its own.
        assert_eq!(parse_settings("model=qwen3.5-4b\n"), Ok(Some(Wanted::new(&CATALOGUE[0]))));
        assert_eq!(Wanted::new(&CATALOGUE[0]).context, 65_536);
        assert_eq!(parse_settings("  context = 4096 \n  model = qwen3.5-4b  \n"), Ok(Some(Wanted { model: &CATALOGUE[0], context: 4_096 })));
    }

    #[test]
    fn a_settings_record_with_a_line_that_is_not_accepted_is_refused_whole() {
        for (record, part) in [
            ("model=qwen3.5-4b\nthreads=8\n", "not a setting this release knows"),
            ("model=qwen9\n", "not a native model this release knows"),
            ("model=qwen3.5-4b\nmodel=qwen3.5-9b\n", "named twice"),
            ("model=qwen3.5-4b\ncontext=4096\ncontext=8192\n", "named twice"),
            ("context=4096\n", "a context window without a model"),
            ("model=qwen3.5-4b\ncontext=many\n", "not a number of tokens"),
            ("model=qwen3.5-4b\ncontext=-1\n", "not a number of tokens"),
            ("model=qwen3.5-4b\ncontext=0\n", "is not a context window"),
            ("model=qwen3.5-4b\ncontext=262145\n", "is not a context window"),
            ("model=qwen3.5-4b\nload it\n", "not a line of the record"),
            ("model=\n", "not a native model this release knows"),
            ("MODEL=qwen3.5-4b\n", "not a setting this release knows"),
        ] {
            let reason = parse_settings(record).expect_err(record);
            assert!(reason.contains(part), "{record:?}: {reason}");
        }
        // A refusal quotes the line, and no more than a line's worth of it.
        let long = format!("model={}\n", "x".repeat(4_000));
        assert!(parse_settings(&long).unwrap_err().len() < 200);
    }

    #[test]
    fn the_settings_are_written_whole_and_a_missing_file_wants_no_model() {
        let dir = crate::net::tests::scratch("native-settings");
        let path = dir.join("native-model").join("settings");
        assert_eq!(read_settings(&path), Ok(None));
        let wanted = Wanted::new(&CATALOGUE[0]);
        write_settings(&path, Some(&wanted)).unwrap();
        assert_eq!(read_settings(&path), Ok(Some(wanted)));
        assert!(!path.with_extension("tmp").exists(), "the temporary file is gone");
        assert!(std::fs::read_to_string(&path).unwrap().starts_with("# embraOS native model (experimental)"));
        write_settings(&path, None).unwrap();
        assert_eq!(read_settings(&path), Ok(None));
        // What is no text is no record, and says so.
        std::fs::write(&path, [0xff, 0xfe, 0x00]).unwrap();
        assert!(read_settings(&path).unwrap_err().contains("could not be read"));
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn every_state_reads_back_by_its_name() {
        for state in ServiceState::ALL {
            assert_eq!(ServiceState::parse(state.as_str()), Some(state));
        }
        assert_eq!(ServiceState::parse("Ready"), None);
        assert_eq!(ServiceState::parse(""), None);
        let running: Vec<&str> = ServiceState::ALL.into_iter().filter(|s| s.runs()).map(ServiceState::as_str).collect();
        assert_eq!(running, ["loading", "ready"]);
    }

    #[test]
    fn the_status_reads_back_with_its_reason_on_one_line() {
        let ready = Status {
            state: ServiceState::Ready,
            model: Some(&CATALOGUE[0]),
            context: Some(65_536),
            pid: Some(412),
            since: 96,
            exits: 0,
            images: false,
            reason: String::new(),
        };
        assert_eq!(ready.to_text(), "state=ready\nmodel=qwen3.5-4b\ncontext=65536\npid=412\nsince=96\nexits=0\n");
        assert_eq!(parse_status(&ready.to_text()), Ok(ready.clone()));

        // A server that was started with the model's image file says so, in
        // one more line; a record without the line is a server without it.
        let seeing = Status { images: true, ..ready };
        assert_eq!(seeing.to_text(), "state=ready\nmodel=qwen3.5-4b\ncontext=65536\npid=412\nsince=96\nexits=0\nimages=yes\n");
        assert_eq!(parse_status(&seeing.to_text()), Ok(seeing));
        assert_eq!(parse_status("state=off\nimages=no\n").map(|s| s.images), Ok(false));
        assert_eq!(parse_status("state=off\nimages=maybe\n"), Err("images=maybe: yes or no".to_string()));
        assert_eq!(parse_status("state=off\nimages=yes\nimages=yes\n"), Err("images=yes: named twice".to_string()));

        let off = Status::of(ServiceState::Off, 7);
        assert_eq!(off.to_text(), "state=off\nsince=7\nexits=0\n");
        assert_eq!(parse_status(&off.to_text()), Ok(off));

        let mut halted = Status::of(ServiceState::Halted, 300);
        halted.model = Some(&CATALOGUE[1]);
        halted.context = Some(65_536);
        halted.exits = 3;
        halted.reason = "ended with signal 9\nafter 4 s  = out of memory?".to_string();
        let text = halted.to_text();
        assert_eq!(text.lines().count(), 6, "{text}");
        let back = parse_status(&text).unwrap();
        assert_eq!(back.reason, "ended with signal 9 after 4 s = out of memory?");
        assert_eq!((back.state, back.model, back.exits), (ServiceState::Halted, Some(&CATALOGUE[1]), 3));
        // A reason is cut, never a record made longer than a reader takes.
        assert_eq!(one_line(&"é".repeat(1_000)).chars().count(), 300);
    }

    #[test]
    fn a_status_that_is_not_whole_is_refused() {
        for (record, part) in [
            ("", "names no state"),
            ("model=qwen3.5-4b\n", "names no state"),
            ("state=sleeping\n", "not a state"),
            ("state=ready\n", "without a model, a window and a process"),
            ("state=loading\nmodel=qwen3.5-4b\ncontext=65536\n", "without a model, a window and a process"),
            ("state=ready\nmodel=qwen3.5-4b\ncontext=65536\npid=0\n", "not a process"),
            ("state=off\nstate=ready\n", "named twice"),
            ("state=off\nmodel=llama\n", "not a native model this release knows"),
            ("state=off\nsince=soon\n", "not a number of seconds"),
            ("state=off\nexits=-2\n", "not a count"),
            ("state=off\nrss=1\n", "not a field this release knows"),
            ("state=off\nnonsense\n", "not a line of the record"),
        ] {
            let reason = parse_status(record).expect_err(record);
            assert!(reason.contains(part), "{record:?}: {reason}");
        }
    }

    #[test]
    fn the_status_is_written_whole_and_a_missing_one_is_none() {
        let dir = crate::net::tests::scratch("native-status");
        let path = dir.join("embra").join("native-model-status");
        assert_eq!(read_status(&path), Ok(None));
        let mut status = Status::of(ServiceState::Unavailable, 12);
        status.model = Some(&CATALOGUE[0]);
        status.reason = "the file was never checked".to_string();
        write_status(&path, &status).unwrap();
        assert_eq!(read_status(&path), Ok(Some(status)));
        assert!(!path.with_extension("tmp").exists(), "the temporary file is gone");
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn the_verification_record_reads_back() {
        let record = Verified {
            file: "Qwen3.5-4B-Q4_K_M.gguf".to_string(),
            bytes: 2_740_937_888,
            sha256: CATALOGUE[0].sha256.to_string(),
            mtime: (1_763_829_000, 5),
        };
        let text = record.to_text();
        assert!(text.ends_with("mtime=1763829000.000000005\n"), "{text}");
        assert_eq!(parse_verified(&text), Ok(record.clone()));
        assert!(record.names(&CATALOGUE[0].part(Part::Model)) && !record.names(&CATALOGUE[1].part(Part::Model)));
        assert!(!record.names(&CATALOGUE[0].part(Part::Images)), "a record of the model file says nothing for the image file");
        // A time before the epoch is a time.
        let old = Verified { mtime: (-3, 999_999_999), ..record };
        assert_eq!(parse_verified(&old.to_text()), Ok(old));
    }

    #[test]
    fn a_verification_record_that_is_not_whole_is_refused() {
        let sha = CATALOGUE[0].sha256;
        let whole = format!("file=m.gguf\nbytes=10\nsha256={sha}\nmtime=5.000000000\n");
        assert!(parse_verified(&whole).is_ok());
        for (record, part) in [
            (String::new(), "lacks one of"),
            (format!("file=m.gguf\nbytes=10\nsha256={sha}\n"), "lacks one of"),
            (whole.replace("file=m.gguf", "file=../m.gguf"), "not a file's name"),
            (whole.replace("file=m.gguf", "file="), "not a file's name"),
            (whole.replace("file=m.gguf", "file=.."), "not a file's name"),
            (whole.replace("bytes=10", "bytes=ten"), "not a size"),
            (whole.replace(sha, &sha.to_uppercase()), "not a sha256"),
            (whole.replace(sha, &sha[..63]), "not a sha256"),
            (whole.replace("mtime=5.000000000", "mtime=5"), "not a time"),
            (whole.replace("mtime=5.000000000", "mtime=5.5"), "not a time"),
            (whole.replace("mtime=5.000000000", "mtime=5.-00000001"), "not a time"),
            (whole.replace("mtime=5.000000000", "mtime=now.000000000"), "not a time"),
            (format!("{whole}bytes=11\n"), "named twice"),
            (format!("{whole}checked=yes\n"), "not a field this release knows"),
            (format!("{whole}checked\n"), "not a line of the record"),
        ] {
            let reason = parse_verified(&record).expect_err(&record);
            assert!(reason.contains(part), "{record:?}: {reason}");
        }
    }

    /// A stand-in catalogue entry whose files a test can afford to write.
    fn small() -> NativeModel {
        NativeModel {
            id: "small",
            file: "small.gguf",
            bytes: 5,
            images: CatalogueFile { file: "small-images.gguf", bytes: 3, sha256: CATALOGUE[0].images.sha256 },
            ..CATALOGUE[0]
        }
    }

    /// Each of a model's two files stands by itself: the image file can be
    /// there or not, checked or not, whatever the model file is.
    #[test]
    fn the_image_file_is_installed_by_a_record_of_its_own() {
        let models = crate::net::tests::scratch("native-installed-images");
        let model = small();
        std::fs::create_dir_all(model.dir(&models)).unwrap();
        std::fs::write(model.file_path(&models), b"gguf!").unwrap();
        write_verified(&model.verified_path(&models), &Verified::of(&model, &std::fs::metadata(model.file_path(&models)).unwrap())).unwrap();
        assert_eq!((installed(&model, &models), installed_part(&model, Part::Images, &models)), (Installed::Verified, Installed::No));

        std::fs::write(model.partial_of(Part::Images, &models), b"mm").unwrap();
        assert_eq!(installed_part(&model, Part::Images, &models), Installed::Partial(2));
        std::fs::rename(model.partial_of(Part::Images, &models), model.path_of(Part::Images, &models)).unwrap();
        std::fs::write(model.path_of(Part::Images, &models), b"mmp").unwrap();
        assert_eq!(installed_part(&model, Part::Images, &models), Installed::Unverified("the file was never checked".to_string()));

        // The model file's record says nothing for the image file.
        let wrong = Verified::of(&model, &std::fs::metadata(model.path_of(Part::Images, &models)).unwrap());
        write_verified(&model.verified_of(Part::Images, &models), &wrong).unwrap();
        assert_eq!(installed_part(&model, Part::Images, &models), Installed::Unverified("the file was checked as another release's file".to_string()));

        let record = Verified::of_part(&model, Part::Images, &std::fs::metadata(model.path_of(Part::Images, &models)).unwrap());
        assert_eq!((record.file.as_str(), record.bytes, record.sha256.as_str()), ("small-images.gguf", 3, model.images.sha256));
        write_verified(&model.verified_of(Part::Images, &models), &record).unwrap();
        assert_eq!((installed(&model, &models), installed_part(&model, Part::Images, &models)), (Installed::Verified, Installed::Verified));

        // And the other way round: the model file gone, the image file as it was.
        std::fs::remove_file(model.file_path(&models)).unwrap();
        assert_eq!((installed(&model, &models), installed_part(&model, Part::Images, &models)), (Installed::No, Installed::Verified));
        std::fs::remove_dir_all(&models).unwrap();
    }

    #[test]
    fn a_file_is_installed_when_its_record_still_describes_it() {
        let models = crate::net::tests::scratch("native-installed");
        let model = small();
        assert_eq!(installed(&model, &models), Installed::No);

        std::fs::create_dir_all(model.dir(&models)).unwrap();
        std::fs::write(model.partial_path(&models), b"gg").unwrap();
        assert_eq!(installed(&model, &models), Installed::Partial(2));

        // The file without a record: nobody checked it.
        std::fs::rename(model.partial_path(&models), model.file_path(&models)).unwrap();
        std::fs::write(model.file_path(&models), b"gguf!").unwrap();
        assert_eq!(installed(&model, &models), Installed::Unverified("the file was never checked".to_string()));

        // The record of the file as it is.
        let meta = std::fs::metadata(model.file_path(&models)).unwrap();
        let record = Verified::of(&model, &meta);
        assert_eq!((record.file.as_str(), record.bytes, record.sha256.as_str()), ("small.gguf", 5, model.sha256));
        write_verified(&model.verified_path(&models), &record).unwrap();
        assert_eq!(installed(&model, &models), Installed::Verified);

        // The same bytes written at another time: changed, as far as a start can tell.
        let moved = Verified { mtime: (record.mtime.0 - 60, record.mtime.1), ..record.clone() };
        write_verified(&model.verified_path(&models), &moved).unwrap();
        assert_eq!(installed(&model, &models), Installed::Unverified("the file changed since it was checked".to_string()));

        // Another size on disk than the record's.
        write_verified(&model.verified_path(&models), &record).unwrap();
        std::fs::write(model.file_path(&models), b"gguf!!").unwrap();
        assert_eq!(installed(&model, &models), Installed::Unverified("the file changed since it was checked".to_string()));

        // A record of another file: a release whose catalogue pinned other bytes.
        let other = Verified { sha256: CATALOGUE[1].sha256.to_string(), ..record.clone() };
        write_verified(&model.verified_path(&models), &other).unwrap();
        assert_eq!(installed(&model, &models), Installed::Unverified("the file was checked as another release's file".to_string()));

        // A record that is no record.
        std::fs::write(model.verified_path(&models), "checked=yes\n").unwrap();
        match installed(&model, &models) {
            Installed::Unverified(reason) => assert!(reason.starts_with("the verification record is not valid ("), "{reason}"),
            other => panic!("{other:?}"),
        }

        // A directory in the file's place.
        std::fs::remove_file(model.file_path(&models)).unwrap();
        std::fs::create_dir(model.file_path(&models)).unwrap();
        assert_eq!(installed(&model, &models), Installed::Unverified("what stands in the file's place is not a file".to_string()));
        std::fs::remove_dir_all(&models).unwrap();
    }

    /// The three parsers take whatever stands in a file: none panics, and
    /// none accepts what it did not write.
    #[test]
    fn hostile_bytes_never_panic_a_parser() {
        let mut samples: Vec<String> = [
            "=", "==", "=\n=\n", "\0", "state=\0", "model=\u{202e}qwen3.5-4b", "context=99999999999999999999",
            "since=-1", "pid=4294967296", "mtime=.", "mtime=9223372036854775808.000000000", "mtime=1.0000000000",
            "bytes=18446744073709551616", "file=a/b", "reason==\n", "\r\n\r\n", "#", "state", "state=ready\r\nmodel=qwen3.5-4b\r\ncontext=65536\r\npid=1\r\n",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        samples.push("x".repeat(1 << 16));
        samples.push("model=".to_string() + &"é".repeat(5_000));
        samples.push("a=b\n".repeat(10_000));
        // A fixed stream of bytes, read as text the way a file is.
        let mut seed: u64 = 0x9e37_79b9_7f4a_7c15;
        for round in 0..2_000 {
            let mut bytes = Vec::with_capacity(96);
            for _ in 0..(8 + round % 89) {
                seed = seed.wrapping_mul(6_364_136_223_846_793_005).wrapping_add(1_442_695_040_888_963_407);
                // Mostly the bytes a record is made of, so lines and keys do form.
                let pick = (seed >> 33) as usize;
                let alphabet = b"=\n\n #.-0123456789abcdefstmoelnxzivrpyhqgwu";
                bytes.push(if pick.is_multiple_of(11) { (seed >> 40) as u8 } else { alphabet[pick % alphabet.len()] });
            }
            samples.push(String::from_utf8_lossy(&bytes).into_owned());
        }
        let mut accepted = 0;
        for sample in &samples {
            accepted += usize::from(matches!(parse_settings(sample), Ok(Some(_))));
            accepted += usize::from(parse_status(sample).is_ok());
            accepted += usize::from(parse_verified(sample).is_ok());
        }
        // One sample is a whole status with CRLF line ends; nothing else forms a record.
        assert_eq!(accepted, 1, "of {} samples", samples.len());
    }

    #[test]
    fn the_job_lock_is_held_by_one_and_seen_by_all_without_being_taken() {
        let models = crate::net::tests::scratch("native-lock").join("models");
        assert!(!job_running(&models), "no directory, no job");
        let held = job_lock(&models).unwrap().expect("the first job takes the lock");
        assert!(job_running(&models));
        assert!(job_lock(&models).unwrap().is_none(), "a second job is told that one runs");
        // Asking takes nothing: the holder still holds it, and asking again says the same.
        assert!(job_running(&models) && job_running(&models));
        assert!(job_lock(&models).unwrap().is_none());
        drop(held);
        assert!(!job_running(&models), "the lock goes with the file");
        let again = job_lock(&models).unwrap();
        assert!(again.is_some() && job_running(&models));
        drop(again);
        // The lock's file is no model's directory and no file of a model.
        assert!(CATALOGUE.iter().all(|m| m.id != JOB_LOCK_FILE && m.file != JOB_LOCK_FILE));
        std::fs::remove_dir_all(models.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_uptime_is_read_from_the_kernel() {
        // On Linux the clock runs; a second call is not earlier.
        let (first, second) = (uptime_secs(), uptime_secs());
        assert!(second >= first);
        if Path::new("/proc/uptime").exists() {
            assert!(first > 0);
        }
    }
}
