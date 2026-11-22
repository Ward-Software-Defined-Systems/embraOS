//! Native models (experimental): the files of the catalogue's models on
//! DATA (`embra_boot::native_model`). A model's file is the catalogue's
//! bytes or it is not kept. A file read from standard input (the web
//! console's upload) is hashed while it is written under a `.partial` name;
//! it gets its final name, and the verification record embrad starts a
//! model by, only when its size and its sha256 are the catalogue's. A
//! download (`install`) is written the same way from the catalogue's URL, and
//! what a stopped one left under the `.partial` name is the start of the
//! next: the bytes there are hashed again and the rest is asked for with an
//! HTTP range. `verify` hashes an installed file again and writes that
//! record, or removes a file that is not the catalogue's.
//!
//! One job at a time: a lock in the models directory, held until the job
//! ends. DATA's reserve stays free, as beside a snapshot.
//!
//! The models are in no snapshot and no restore touches them: `snapshot`
//! names the two directories of DATA it holds, and this is neither. A model
//! is downloaded again, not backed up.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use embra_boot::native_model::{self as native, Installed, NativeModel, Verified};
use sha2::{Digest, Sha256};

use crate::fetch::{self, Opened};
use crate::paths::RESERVE_BYTES;
use crate::snapshot::{Roots, enough_space, free_bytes, sync_dir, tree_bytes};
use crate::Outcome;

/// The models' directory under DATA.
const MODELS_SUBDIR: &str = "models";
/// The lock every job takes, in the models' directory.
const LOCK_FILE: &str = ".lock";
/// The reads from a source and from a file, one buffer at a time.
const CHUNK: usize = 4 * 1024 * 1024;
/// DATA's free space is looked at again each time this much has been written.
const RESERVE_CHECK_EVERY: u64 = 32 * 1024 * 1024;
/// A progress line each time this many percent more of a file are done.
const PROGRESS_STEP: u64 = 5;
/// The requests one download makes at most. Each after the first asks for
/// the rest, and only when the one before brought bytes.
const REQUESTS_MAX: u32 = 20;

/// The models' directory: `native::MODELS_DIR` in the OS.
pub fn models_dir(roots: &Roots) -> PathBuf {
    roots.data().join(MODELS_SUBDIR)
}

/// The settings embrad follows: `native::SETTINGS_PATH` in the OS.
fn settings_path(roots: &Roots) -> PathBuf {
    roots.state().join("native-model").join("settings")
}

fn mb(bytes: u64) -> u64 {
    bytes / (1024 * 1024)
}

/// When DATA's free space is looked at, and by what: a test stands in for
/// the filesystem.
pub struct Limits<'a> {
    pub check_every: u64,
    pub free: &'a dyn Fn(&Path) -> io::Result<u64>,
}

/// One job at a time. The lock is the kernel's: it goes when the job ends,
/// however it ends.
pub struct JobLock {
    _file: File,
}

/// Take the lock, or say that another job holds it.
pub fn job_lock(models: &Path) -> Result<JobLock, Outcome> {
    use std::os::unix::io::AsRawFd;
    let failed = |what: &str, e: io::Error| Outcome::Failed(format!("{what} {}: {e}", models.display()));
    fs::create_dir_all(models).map_err(|e| failed("create", e))?;
    let file = File::create(models.join(LOCK_FILE)).map_err(|e| failed("the lock under", e))?;
    // SAFETY: a valid descriptor, held by `file` for as long as the lock is.
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
        let e = io::Error::last_os_error();
        return Err(match e.kind() {
            io::ErrorKind::WouldBlock => Outcome::Refused(
                "refused: another model job is running (an install, an import, a check or a removal); it has to end first".to_string(),
            ),
            _ => failed("the lock under", e),
        });
    }
    Ok(JobLock { _file: file })
}

/// A progress line each [`PROGRESS_STEP`] percent of a file: twenty a file
/// at most, whatever its size and the speed.
struct Progress<'a> {
    model: &'a NativeModel,
    verb: &'static str,
    next: u64,
}

impl<'a> Progress<'a> {
    fn new(model: &'a NativeModel, verb: &'static str, done: u64) -> Self {
        let mut progress = Progress { model, verb, next: 0 };
        progress.next = progress.step_after(done);
        progress
    }

    fn percent(&self, done: u64) -> u64 {
        (u128::from(done) * 100 / u128::from(self.model.bytes.max(1))) as u64
    }

    fn step_after(&self, done: u64) -> u64 {
        (self.percent(done) / PROGRESS_STEP + 1) * PROGRESS_STEP
    }

    fn at(&mut self, done: u64, out: &mut dyn Write) {
        if self.percent(done) >= self.next && done < self.model.bytes {
            let _ = writeln!(
                out,
                "{}: {} {} of {} MB ({}%)",
                self.model.id,
                self.verb,
                mb(done),
                mb(self.model.bytes),
                self.percent(done)
            );
            self.next = self.step_after(done);
        }
    }
}

/// How a copy into a model's file ended.
enum Copied {
    /// The catalogue's size arrived and the source ended there.
    Whole,
    /// The source ended early, after so many bytes.
    Short(u64),
    /// The source runs past the catalogue's size.
    Long,
    /// DATA fell under its reserve: so many bytes are free.
    Reserve(u64),
    /// The source failed after so many bytes.
    Source(u64, io::Error),
    /// This machine's disk refused a write.
    Disk(io::Error),
}

/// What a copy into a model's file is for: the model, DATA and how its
/// reserve is watched, and the word a progress line says the bytes were.
struct CopyJob<'a> {
    model: &'a NativeModel,
    limits: &'a Limits<'a>,
    data: &'a Path,
    verb: &'static str,
}

/// Copy `source` to the end of `dest`, which holds `done` bytes that
/// `hasher` has seen: no more than the catalogue's size, hashed as written,
/// DATA's reserve looked at as it goes.
fn copy_hashed(job: &CopyJob, source: &mut dyn Read, dest: &mut File, hasher: &mut Sha256, mut done: u64, out: &mut dyn Write) -> Copied {
    let (model, limits) = (job.model, job.limits);
    let mut buf = vec![0u8; CHUNK];
    let mut progress = Progress::new(model, job.verb, done);
    let mut next_check = done.saturating_add(limits.check_every);
    loop {
        let n = match source.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Copied::Source(done, e),
        };
        if done + n as u64 > model.bytes {
            return Copied::Long;
        }
        if let Err(e) = dest.write_all(&buf[..n]) {
            return Copied::Disk(e);
        }
        hasher.update(&buf[..n]);
        done += n as u64;
        progress.at(done, out);
        if done >= next_check {
            next_check = done.saturating_add(limits.check_every);
            if let Ok(free) = (limits.free)(job.data)
                && free < RESERVE_BYTES
            {
                return Copied::Reserve(free);
            }
        }
    }
    if done < model.bytes { Copied::Short(done) } else { Copied::Whole }
}

/// Feed `hasher` the first `len` bytes of `path`, with progress lines.
fn hash_into(hasher: &mut Sha256, path: &Path, len: u64, model: &NativeModel, out: &mut dyn Write) -> io::Result<()> {
    let mut file = File::open(path)?;
    let mut buf = vec![0u8; CHUNK];
    let mut progress = Progress::new(model, "checked", 0);
    let mut done: u64 = 0;
    while done < len {
        let want = (len - done).min(buf.len() as u64) as usize;
        let n = match file.read(&mut buf[..want]) {
            Ok(0) => {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, format!("it ended after {done} of {len} bytes")));
            }
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        done += n as u64;
        progress.at(done, out);
    }
    Ok(())
}

/// What a check of an installed file found.
enum Checked {
    /// The catalogue's file, with its sha256: its record is written.
    Catalogue(String),
    /// Another file, removed with its record: how it differs.
    Removed(String),
}

/// Hash the installed file. The catalogue's gets its record; any other is
/// removed.
fn check_file(model: &NativeModel, models: &Path, out: &mut dyn Write) -> Result<Checked, String> {
    let path = model.file_path(models);
    let len = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?.len();
    let removed = |why: String| {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(model.verified_path(models));
        Ok(Checked::Removed(why))
    };
    if len != model.bytes {
        return removed(format!("{} is {len} bytes and the catalogue's file is {}", model.file, model.bytes));
    }
    let _ = writeln!(out, "{}: checking {} ({} MB)", model.id, model.file, mb(model.bytes));
    let mut hasher = Sha256::new();
    hash_into(&mut hasher, &path, len, model, out).map_err(|e| format!("reading {}: {e}", path.display()))?;
    let sha256 = hex::encode(hasher.finalize());
    if sha256 != model.sha256 {
        return removed(format!("the sha256 of {} is {}…, not {}…", model.file, &sha256[..16], &model.sha256[..16]));
    }
    record(model, models)?;
    Ok(Checked::Catalogue(sha256))
}

/// Give a whole, hashed `.partial` its final name and write the record that
/// vouches for it: synced, renamed, the directory synced, then the record.
/// A crash between the two leaves a file without a record, which reads as
/// never checked.
fn adopt(model: &NativeModel, models: &Path, partial: File) -> Result<(), String> {
    let (from, to, dir) = (model.partial_path(models), model.file_path(models), model.dir(models));
    partial.sync_all().map_err(|e| format!("sync {}: {e}", from.display()))?;
    drop(partial);
    fs::rename(&from, &to).map_err(|e| format!("rename {} to {}: {e}", from.display(), to.display()))?;
    sync_dir(&dir).map_err(|e| format!("sync {}: {e}", dir.display()))?;
    record(model, models)
}

/// Write the verification record of the file as it stands now.
fn record(model: &NativeModel, models: &Path) -> Result<(), String> {
    let (path, dir) = (model.file_path(models), model.dir(models));
    let meta = fs::metadata(&path).map_err(|e| format!("{}: {e}", path.display()))?;
    native::write_verified(&model.verified_path(models), &Verified::of(model, &meta))
        .map_err(|e| format!("write {}: {e}", model.verified_path(models).display()))?;
    sync_dir(&dir).map_err(|e| format!("sync {}: {e}", dir.display()))
}

/// What a model's coming leaves on DATA, against what a snapshot of STATE
/// and DATA needs: an update takes one before a new release first runs, and
/// a DATA filled with models would refuse it. Said, never enforced.
fn space_note(roots: &Roots, limits: &Limits) -> Option<String> {
    let free = (limits.free)(&roots.data()).ok()?;
    let plan = crate::snapshot::plan(roots).ok()?;
    let mut note = format!(
        "DATA has {} MB free; a snapshot of STATE and DATA needs {} MB plus a reserve of {} MB",
        mb(free),
        mb(plan.bytes),
        mb(RESERVE_BYTES)
    );
    if !enough_space(free, plan.bytes, RESERVE_BYTES) {
        note.push_str(
            ". WARNING: the snapshot an update takes before a new release first runs would be refused until space is freed (/backup delete, /native remove)",
        );
    }
    Some(note)
}

/// The state of a model's file as the listing says it.
fn state_words(model: &NativeModel, models: &Path) -> String {
    match native::installed(model, models) {
        Installed::No => "not installed".to_string(),
        Installed::Partial(bytes) => format!("a download stopped at {} of {} MB", mb(bytes), mb(model.bytes)),
        Installed::Unverified(reason) => format!("installed, not usable: {reason}"),
        Installed::Verified => "installed".to_string(),
    }
}

/// `model-list`: a line a model of the catalogue, then DATA's free space.
pub fn list_command(roots: &Roots, out: &mut impl Write) -> Outcome {
    let models = models_dir(roots);
    for model in &native::CATALOGUE {
        let _ = writeln!(
            out,
            "{}  {} ({}, {} MB, {}): {}",
            model.id,
            model.label,
            model.quantisation,
            mb(model.bytes),
            model.licence,
            state_words(model, &models)
        );
    }
    if let Ok(free) = free_bytes(&roots.data()) {
        let _ = writeln!(out, "DATA has {} MB free", mb(free));
    }
    Outcome::Done
}

/// `model-import <id>`: the model's file read from `source` (standard
/// input: the web console's upload).
pub fn import_command(roots: &Roots, model: &NativeModel, source: impl Read, out: &mut impl Write) -> Outcome {
    import_with(roots, model, source, out, &Limits { check_every: RESERVE_CHECK_EVERY, free: &free_bytes })
}

pub fn import_with(roots: &Roots, model: &NativeModel, mut source: impl Read, out: &mut impl Write, limits: &Limits) -> Outcome {
    let models = models_dir(roots);
    let _lock = match job_lock(&models) {
        Ok(lock) => lock,
        Err(outcome) => return outcome,
    };
    if native::installed(model, &models) == Installed::Verified {
        return Outcome::Nothing(format!("{} is installed and verified; nothing to do", model.id));
    }
    let partial_path = model.partial_path(&models);
    if let Err(e) = fs::create_dir_all(model.dir(&models)) {
        return Outcome::Failed(format!("create {}: {e}", model.dir(&models).display()));
    }
    // What an earlier download left gives way: this brings the whole file.
    let _ = fs::remove_file(&partial_path);
    let free = match (limits.free)(&roots.data()) {
        Ok(free) => free,
        Err(e) => return Outcome::Failed(format!("free space of {}: {e}", roots.data().display())),
    };
    if !enough_space(free, model.bytes, RESERVE_BYTES) {
        return Outcome::Refused(format!(
            "refused: DATA has {} MB free; {} needs {} MB plus a reserve of {} MB",
            mb(free),
            model.label,
            mb(model.bytes),
            mb(RESERVE_BYTES)
        ));
    }
    let mut partial = match File::create(&partial_path) {
        Ok(file) => file,
        Err(e) => return Outcome::Failed(format!("create {}: {e}", partial_path.display())),
    };
    let _ = writeln!(out, "{}: reading {} ({} MB)", model.id, model.file, mb(model.bytes));
    let mut hasher = Sha256::new();
    let data = roots.data();
    let job = CopyJob { model, limits, data: &data, verb: "read" };
    let copied = copy_hashed(&job, &mut source, &mut partial, &mut hasher, 0, out);
    let nothing_kept = |outcome: Outcome| {
        let _ = fs::remove_file(&partial_path);
        outcome
    };
    match copied {
        Copied::Whole => {}
        Copied::Short(got) => {
            return nothing_kept(Outcome::Refused(format!(
                "refused: the file ended after {got} of {} bytes; nothing of it is kept",
                model.bytes
            )));
        }
        Copied::Long => {
            return nothing_kept(Outcome::Refused(format!(
                "refused: the file runs past {} bytes, the size of {}; nothing of it is kept",
                model.bytes, model.file
            )));
        }
        Copied::Reserve(free) => {
            return nothing_kept(Outcome::Refused(format!(
                "refused: DATA is down to {} MB free and its reserve of {} MB stays; nothing of the file is kept",
                mb(free),
                mb(RESERVE_BYTES)
            )));
        }
        Copied::Source(got, e) => {
            return nothing_kept(Outcome::Failed(format!("reading the file failed after {got} bytes: {e}; nothing of it is kept")));
        }
        Copied::Disk(e) => return nothing_kept(Outcome::Failed(format!("writing {}: {e}", partial_path.display()))),
    }
    let sha256 = hex::encode(hasher.finalize());
    if sha256 != model.sha256 {
        return nothing_kept(Outcome::Refused(format!(
            "refused: the file has the size of {} and its sha256 is {}…, not {}…; nothing of it is kept",
            model.file,
            &sha256[..16],
            &model.sha256[..16]
        )));
    }
    if let Err(reason) = adopt(model, &models, partial) {
        return Outcome::Failed(reason);
    }
    let _ = writeln!(out, "{}: installed and verified ({} MB, sha256 {}…)", model.id, mb(model.bytes), &sha256[..16]);
    if let Some(note) = space_note(roots, limits) {
        let _ = writeln!(out, "{note}");
    }
    Outcome::Done
}

/// `model-verify <id>`: the installed file hashed again. The catalogue's
/// file gets its record; any other is removed.
pub fn verify_command(roots: &Roots, model: &NativeModel, out: &mut impl Write) -> Outcome {
    let models = models_dir(roots);
    let _lock = match job_lock(&models) {
        Ok(lock) => lock,
        Err(outcome) => return outcome,
    };
    if !model.file_path(&models).is_file() {
        return Outcome::Refused(format!("refused: {} is not installed", model.id));
    }
    match check_file(model, &models, out) {
        Ok(Checked::Catalogue(sha256)) => {
            let _ = writeln!(out, "{}: verified ({} MB, sha256 {}…)", model.id, mb(model.bytes), &sha256[..16]);
            Outcome::Done
        }
        Ok(Checked::Removed(why)) => {
            Outcome::Failed(format!("{why}: it is not {}'s file and was removed. Install {} again", model.label, model.id))
        }
        Err(reason) => Outcome::Failed(reason),
    }
}

/// `model-install <id>`: the model's file downloaded from the catalogue's
/// URL.
pub fn install_command(roots: &Roots, model: &NativeModel, out: &mut impl Write) -> Outcome {
    install_with(roots, model, &model.url(), out, &Limits { check_every: RESERVE_CHECK_EVERY, free: &free_bytes })
}

/// The download. A file that is there is checked first and nothing is asked
/// for when it is the catalogue's. What an earlier download left under the
/// `.partial` name is hashed again and the rest asked for with a range; a
/// source that takes no range sends the whole again, once. A connection that
/// ends early is followed by a request for the rest as long as it brought
/// bytes, [`REQUESTS_MAX`] requests at most; what arrived stays for the next
/// run. The whole file's sha256 decides: the catalogue's, or nothing is kept.
pub fn install_with(roots: &Roots, model: &NativeModel, url: &str, out: &mut impl Write, limits: &Limits) -> Outcome {
    let models = models_dir(roots);
    let _lock = match job_lock(&models) {
        Ok(lock) => lock,
        Err(outcome) => return outcome,
    };
    match native::installed(model, &models) {
        Installed::Verified => return Outcome::Nothing(format!("{} is installed and verified; nothing to do", model.id)),
        Installed::Unverified(reason) => {
            let _ = writeln!(out, "{}: a file is there ({reason})", model.id);
            match check_file(model, &models, out) {
                Ok(Checked::Catalogue(sha256)) => {
                    let _ = writeln!(out, "{}: it is the catalogue's file; verified ({} MB, sha256 {}…)", model.id, mb(model.bytes), &sha256[..16]);
                    return Outcome::Done;
                }
                Ok(Checked::Removed(why)) => {
                    let _ = writeln!(out, "{}: {why}: it is not {}'s file and was removed", model.id, model.label);
                }
                Err(reason) => return Outcome::Failed(reason),
            }
        }
        Installed::Partial(_) | Installed::No => {}
    }
    let partial_path = model.partial_path(&models);
    if let Err(e) = fs::create_dir_all(model.dir(&models)) {
        return Outcome::Failed(format!("create {}: {e}", model.dir(&models).display()));
    }
    let mut partial = match OpenOptions::new().create(true).append(true).open(&partial_path) {
        Ok(file) => file,
        Err(e) => return Outcome::Failed(format!("open {}: {e}", partial_path.display())),
    };
    let io_failed = |what: &str, e: io::Error| Outcome::Failed(format!("{what} {}: {e}", partial_path.display()));
    let mut done = match partial.metadata() {
        Ok(meta) => meta.len(),
        Err(e) => return io_failed("read", e),
    };
    let mut hasher = Sha256::new();
    if done > model.bytes {
        let _ = writeln!(out, "{}: what an earlier download left is larger than the file; starting again", model.id);
        if let Err(e) = partial.set_len(0) {
            return io_failed("empty", e);
        }
        done = 0;
    } else if done > 0 {
        let _ = writeln!(out, "{}: {} of {} MB are there from an earlier download; checking them", model.id, mb(done), mb(model.bytes));
        if let Err(e) = hash_into(&mut hasher, &partial_path, done, model, out) {
            return io_failed("read", e);
        }
    }
    // An empty `.partial` is no download: it goes when the job ends without a byte.
    let ended = |done: u64, outcome: Outcome| {
        if done == 0 {
            let _ = fs::remove_file(&partial_path);
        }
        outcome
    };
    let data = roots.data();
    let free = match (limits.free)(&data) {
        Ok(free) => free,
        Err(e) => return ended(done, Outcome::Failed(format!("free space of {}: {e}", data.display()))),
    };
    if !enough_space(free, model.bytes - done, RESERVE_BYTES) {
        let what = if done > 0 { format!("the rest of {}", model.label) } else { model.label.to_string() };
        return ended(
            done,
            Outcome::Refused(format!(
                "refused: DATA has {} MB free; {what} needs {} MB plus a reserve of {} MB",
                mb(free),
                mb(model.bytes - done),
                mb(RESERVE_BYTES)
            )),
        );
    }
    let another_size = |size: u64| {
        Outcome::Failed(format!(
            "the source's file is {size} bytes and the catalogue's {} is {}; nothing was downloaded",
            model.file, model.bytes
        ))
    };
    if done < model.bytes {
        let _ = writeln!(out, "{}: downloading {} ({} MB) from {url}", model.id, model.file, mb(model.bytes));
    }
    let client = match fetch::client() {
        Ok(client) => client,
        Err(reason) => return ended(done, Outcome::Failed(reason)),
    };
    let (mut requests, mut sent_whole_again) = (0u32, false);
    while done < model.bytes {
        requests += 1;
        let mut before = done;
        let mut body = match fetch::open_from(&client, url, done) {
            Ok(Opened::Rest { body, total }) => {
                if total != model.bytes {
                    return ended(done, another_size(total));
                }
                body
            }
            Ok(Opened::Whole { body, length }) => {
                if let Some(length) = length.filter(|length| *length != model.bytes) {
                    return ended(done, another_size(length));
                }
                if done > 0 {
                    if sent_whole_again {
                        return Outcome::Failed(format!(
                            "the source takes no ranges and the download stopped again, after {done} of {} bytes; what arrived is kept",
                            model.bytes
                        ));
                    }
                    let _ = writeln!(out, "{}: the source takes no ranges; the file comes again from its first byte", model.id);
                    if let Err(e) = partial.set_len(0) {
                        return io_failed("empty", e);
                    }
                    (done, before, hasher, sent_whole_again) = (0, 0, Sha256::new(), true);
                }
                body
            }
            Err(reason) => return ended(done, Outcome::Failed(reason)),
        };
        let job = CopyJob { model, limits, data: &data, verb: "downloaded" };
        let (got, why) = match copy_hashed(&job, &mut body, &mut partial, &mut hasher, done, out) {
            Copied::Whole => {
                done = model.bytes;
                continue;
            }
            Copied::Short(got) => (got, "the connection ended".to_string()),
            Copied::Source(got, e) => (got, format!("the connection failed ({})", fetch::error_chain(&e))),
            Copied::Long => {
                let _ = fs::remove_file(&partial_path);
                return Outcome::Failed(format!(
                    "the source sent more than {} bytes, the size of {}; nothing of it is kept",
                    model.bytes, model.file
                ));
            }
            Copied::Reserve(free) => {
                return Outcome::Refused(format!(
                    "refused: DATA is down to {} MB free and its reserve of {} MB stays; what arrived is kept (/native remove frees it)",
                    mb(free),
                    mb(RESERVE_BYTES)
                ));
            }
            Copied::Disk(e) => return io_failed("write", e),
        };
        done = got;
        if got == before || requests >= REQUESTS_MAX {
            return ended(
                done,
                Outcome::Failed(format!(
                    "the download stopped after {got} of {} bytes: {why}. What arrived is kept, and installing again resumes it",
                    model.bytes
                )),
            );
        }
        let _ = writeln!(out, "{}: {why} after {} of {} MB; asking for the rest", model.id, mb(got), mb(model.bytes));
    }
    let sha256 = hex::encode(hasher.finalize());
    if sha256 != model.sha256 {
        let _ = fs::remove_file(&partial_path);
        return Outcome::Failed(format!(
            "the download has the size of {} and its sha256 is {}…, not {}…; nothing of it is kept. Install {} again",
            model.file,
            &sha256[..16],
            &model.sha256[..16],
            model.id
        ));
    }
    if let Err(reason) = adopt(model, &models, partial) {
        return Outcome::Failed(reason);
    }
    let _ = writeln!(out, "{}: installed and verified ({} MB, sha256 {}…)", model.id, mb(model.bytes), &sha256[..16]);
    if let Some(note) = space_note(roots, limits) {
        let _ = writeln!(out, "{note}");
    }
    Outcome::Done
}

/// `model-remove <id>`: the model's directory, with whatever of it is
/// there. Refused for the model the settings keep loaded.
pub fn remove_command(roots: &Roots, model: &NativeModel, out: &mut impl Write) -> Outcome {
    if let Ok(Some(wanted)) = native::read_settings(&settings_path(roots))
        && wanted.model.id == model.id
    {
        return Outcome::Refused(format!("refused: {} is the model kept loaded; free it first (/native off)", model.id));
    }
    let models = models_dir(roots);
    let _lock = match job_lock(&models) {
        Ok(lock) => lock,
        Err(outcome) => return outcome,
    };
    let dir = model.dir(&models);
    if !dir.exists() {
        return Outcome::Nothing(format!("{} is not installed; nothing to do", model.id));
    }
    let freed = tree_bytes(&dir);
    if let Err(e) = fs::remove_dir_all(&dir) {
        return Outcome::Failed(format!("remove {}: {e}", dir.display()));
    }
    let _ = sync_dir(&models);
    let _ = writeln!(out, "{}: removed, {} MB freed", model.id, mb(freed));
    Outcome::Done
}

#[cfg(test)]
pub mod test_support {
    use super::*;

    /// A stand-in for a catalogue model whose file is `content`: a test
    /// cannot afford the real ones.
    pub fn tiny(id: &'static str, content: &[u8]) -> NativeModel {
        let sha256: &'static str = Box::leak(hex::encode(Sha256::digest(content)).into_boxed_str());
        NativeModel { id, label: "Tiny", file: "tiny.gguf", bytes: content.len() as u64, sha256, ..native::CATALOGUE[0] }
    }

    /// Room for anything.
    pub fn roomy(_: &Path) -> io::Result<u64> {
        Ok(u64::MAX / 2)
    }

    pub fn words(outcome: &Outcome) -> (&'static str, &str) {
        match outcome {
            Outcome::Done => ("done", ""),
            Outcome::Refused(m) => ("refused", m),
            Outcome::Nothing(m) => ("nothing", m),
            Outcome::Failed(m) => ("failed", m),
        }
    }

    /// The names in a model's directory.
    pub fn names(model: &NativeModel, models: &Path) -> Vec<String> {
        let mut names: Vec<String> = fs::read_dir(model.dir(models))
            .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().into_owned()).collect())
            .unwrap_or_default();
        names.sort();
        names
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;
    use crate::snapshot::test_support::{populate, release, temp_roots};

    const CONTENT: &[u8] = b"GGUF: sixteen bytes and some more, enough for a few reads";
    const LIMITS: Limits = Limits { check_every: 8, free: &roomy };

    #[test]
    fn the_directories_are_the_contracts() {
        let os = Roots::os();
        assert_eq!(models_dir(&os), Path::new(native::MODELS_DIR));
        assert_eq!(settings_path(&os), Path::new(native::SETTINGS_PATH));
    }

    #[test]
    fn the_catalogues_file_read_whole_is_installed_with_its_record() {
        let roots = temp_roots("model-import");
        populate(&roots);
        let model = tiny("tiny", CONTENT);
        let models = models_dir(&roots);
        assert_eq!(native::installed(&model, &models), Installed::No);
        let mut out = Vec::new();
        let outcome = import_with(&roots, &model, CONTENT, &mut out, &LIMITS);
        assert_eq!(words(&outcome), ("done", ""));
        assert_eq!(fs::read(model.file_path(&models)).unwrap(), CONTENT);
        assert_eq!(native::installed(&model, &models), Installed::Verified);
        assert_eq!(names(&model, &models), ["tiny.gguf", "verified"], "no .partial and no temporary record is left");
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("tiny: installed and verified"), "{said}");
        assert!(said.contains("a snapshot of STATE and DATA needs"), "{said}");
        assert!(!said.contains("WARNING"), "{said}");

        // The same file again: nothing to do, and the source is not read.
        let outcome = import_with(&roots, &model, &b"anything"[..], &mut Vec::new(), &LIMITS);
        assert_eq!(words(&outcome), ("nothing", "tiny is installed and verified; nothing to do"));
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_short_a_long_and_a_wrong_file_are_refused_and_nothing_of_them_is_kept() {
        let roots = temp_roots("model-refused");
        let model = tiny("tiny", CONTENT);
        let models = models_dir(&roots);
        let mut wrong = CONTENT.to_vec();
        wrong[7] ^= 1;
        let mut long = CONTENT.to_vec();
        long.push(b'!');
        for (what, source, part) in [
            ("short", &CONTENT[..CONTENT.len() - 1], "the file ended after 56 of 57 bytes"),
            ("empty", &CONTENT[..0], "the file ended after 0 of 57 bytes"),
            ("long", &long[..], "the file runs past 57 bytes, the size of tiny.gguf"),
            ("wrong", &wrong[..], "has the size of tiny.gguf and its sha256 is"),
        ] {
            // What a download left before is gone as well: the import replaced it.
            fs::create_dir_all(model.dir(&models)).unwrap();
            fs::write(model.partial_path(&models), b"an earlier download").unwrap();
            let outcome = import_with(&roots, &model, source, &mut Vec::new(), &LIMITS);
            let (kind, reason) = words(&outcome);
            assert_eq!(kind, "refused", "{what}: {reason}");
            assert!(reason.contains(part) && reason.contains("nothing of it is kept"), "{what}: {reason}");
            assert_eq!(names(&model, &models), Vec::<String>::new(), "{what}");
            assert_eq!(native::installed(&model, &models), Installed::No, "{what}");
        }
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    /// A source that fails after its first bytes.
    struct Cut(usize);
    impl Read for Cut {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            if self.0 == 0 {
                return Err(io::Error::new(io::ErrorKind::ConnectionReset, "the upload was cut"));
            }
            let n = self.0.min(buf.len()).min(CONTENT.len());
            buf[..n].copy_from_slice(&CONTENT[..n]);
            self.0 -= n;
            Ok(n)
        }
    }

    #[test]
    fn a_source_that_fails_is_a_failure_and_nothing_is_kept() {
        let roots = temp_roots("model-cut");
        let model = tiny("tiny", CONTENT);
        let outcome = import_with(&roots, &model, Cut(10), &mut Vec::new(), &LIMITS);
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "failed");
        assert!(reason.contains("failed after 10 bytes") && reason.contains("the upload was cut"), "{reason}");
        assert_eq!(names(&model, &models_dir(&roots)), Vec::<String>::new());
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn the_reserve_of_data_stays_free_before_and_during_an_import() {
        let roots = temp_roots("model-reserve");
        let model = tiny("tiny", CONTENT);
        let models = models_dir(&roots);
        // Before: the file and the reserve do not both fit.
        let tight = |_: &Path| Ok(RESERVE_BYTES + 56);
        let outcome = import_with(&roots, &model, CONTENT, &mut Vec::new(), &Limits { check_every: 8, free: &tight });
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "refused");
        assert_eq!(reason, "refused: DATA has 256 MB free; Tiny needs 0 MB plus a reserve of 256 MB");
        assert_eq!(names(&model, &models), Vec::<String>::new());
        // One byte more and it fits.
        let fits = |_: &Path| Ok(RESERVE_BYTES + 57);
        assert_eq!(words(&import_with(&roots, &model, CONTENT, &mut Vec::new(), &Limits { check_every: 8, free: &fits })).0, "done");
        fs::remove_dir_all(model.dir(&models)).unwrap();

        // During: something else fills DATA while the file is written.
        let calls = std::cell::Cell::new(0u32);
        let filling = |_: &Path| {
            calls.set(calls.get() + 1);
            Ok(if calls.get() == 1 { RESERVE_BYTES + 1_000 } else { RESERVE_BYTES - 1 })
        };
        let outcome = import_with(&roots, &model, CONTENT, &mut Vec::new(), &Limits { check_every: 8, free: &filling });
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "refused");
        assert!(reason.contains("DATA is down to 255 MB free and its reserve of 256 MB stays"), "{reason}");
        assert_eq!(names(&model, &models), Vec::<String>::new());
        assert_eq!(calls.get(), 2, "asked once before and once while writing");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_model_that_fills_data_is_installed_with_a_warning_about_the_next_snapshot() {
        let roots = temp_roots("model-warning");
        populate(&roots);
        let model = tiny("tiny", CONTENT);
        // Room for the file and the reserve when asked before, the reserve alone after.
        let calls = std::cell::Cell::new(0u32);
        let free = |_: &Path| {
            calls.set(calls.get() + 1);
            Ok(if calls.get() == 1 { RESERVE_BYTES + 1_000 } else { RESERVE_BYTES })
        };
        let mut out = Vec::new();
        let outcome = import_with(&roots, &model, CONTENT, &mut out, &Limits { check_every: 1 << 20, free: &free });
        assert_eq!(words(&outcome).0, "done");
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("WARNING: the snapshot an update takes before a new release first runs would be refused"), "{said}");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_check_gives_the_catalogues_file_its_record_and_removes_any_other() {
        let roots = temp_roots("model-verify");
        let model = tiny("tiny", CONTENT);
        let models = models_dir(&roots);
        let (kind, reason) = {
            let outcome = verify_command(&roots, &model, &mut Vec::new());
            (words(&outcome).0, words(&outcome).1.to_string())
        };
        assert_eq!((kind, reason.as_str()), ("refused", "refused: tiny is not installed"));

        // The right bytes that nobody vouched for: a crash after the rename, a copy by hand.
        fs::create_dir_all(model.dir(&models)).unwrap();
        fs::write(model.file_path(&models), CONTENT).unwrap();
        assert!(matches!(native::installed(&model, &models), Installed::Unverified(_)));
        let mut out = Vec::new();
        assert_eq!(words(&verify_command(&roots, &model, &mut out)).0, "done");
        assert_eq!(native::installed(&model, &models), Installed::Verified);
        assert!(String::from_utf8(out).unwrap().contains("tiny: verified"));

        // The same size, other bytes.
        let mut wrong = CONTENT.to_vec();
        wrong[0] ^= 0x20;
        fs::write(model.file_path(&models), &wrong).unwrap();
        let outcome = verify_command(&roots, &model, &mut Vec::new());
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "failed");
        assert!(reason.starts_with("the sha256 of tiny.gguf is ") && reason.ends_with("it is not Tiny's file and was removed. Install tiny again"), "{reason}");
        assert_eq!(names(&model, &models), Vec::<String>::new(), "the file and its record are gone");

        // Another size: no need to read it.
        fs::write(model.file_path(&models), &CONTENT[..10]).unwrap();
        let outcome = verify_command(&roots, &model, &mut Vec::new());
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "failed");
        assert!(reason.starts_with("tiny.gguf is 10 bytes and the catalogue's file is 57"), "{reason}");
        assert_eq!(names(&model, &models), Vec::<String>::new());
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_removal_takes_the_directory_and_spares_the_model_kept_loaded() {
        let roots = temp_roots("model-remove");
        // The settings name a catalogue model, so the test removes that one's directory.
        let model = &native::CATALOGUE[0];
        let models = models_dir(&roots);
        let outcome = remove_command(&roots, model, &mut Vec::new());
        assert_eq!(words(&outcome), ("nothing", "qwen3.5-4b is not installed; nothing to do"));

        fs::create_dir_all(model.dir(&models)).unwrap();
        fs::write(model.partial_path(&models), vec![0u8; 3 * 1024 * 1024]).unwrap();
        native::write_settings(&settings_path(&roots), Some(&native::Wanted::new(model))).unwrap();
        let outcome = remove_command(&roots, model, &mut Vec::new());
        assert_eq!(words(&outcome), ("refused", "refused: qwen3.5-4b is the model kept loaded; free it first (/native off)"));
        assert!(model.partial_path(&models).exists());

        // Another model kept loaded, or none: this one goes.
        native::write_settings(&settings_path(&roots), Some(&native::Wanted::new(&native::CATALOGUE[1]))).unwrap();
        let mut out = Vec::new();
        assert_eq!(words(&remove_command(&roots, model, &mut out)).0, "done");
        assert_eq!(String::from_utf8(out).unwrap(), "qwen3.5-4b: removed, 3 MB freed\n");
        assert!(!model.dir(&models).exists());
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_second_job_is_refused_while_one_runs() {
        let roots = temp_roots("model-lock");
        let model = tiny("tiny", CONTENT);
        let models = models_dir(&roots);
        let held = job_lock(&models).ok().expect("the first job takes the lock");
        for outcome in [
            import_with(&roots, &model, CONTENT, &mut Vec::new(), &LIMITS),
            verify_command(&roots, &model, &mut Vec::new()),
            remove_command(&roots, &model, &mut Vec::new()),
        ] {
            let (kind, reason) = words(&outcome);
            assert_eq!(kind, "refused");
            assert!(reason.contains("another model job is running"), "{reason}");
        }
        assert_eq!(names(&model, &models), Vec::<String>::new(), "the refused import wrote nothing");
        drop(held);
        assert_eq!(words(&import_with(&roots, &model, CONTENT, &mut Vec::new(), &LIMITS)).0, "done");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn the_listing_says_how_each_model_stands() {
        let roots = temp_roots("model-list");
        let models = models_dir(&roots);
        let four = &native::CATALOGUE[0];
        fs::create_dir_all(four.dir(&models)).unwrap();
        fs::write(four.partial_path(&models), vec![0u8; 5 * 1024 * 1024]).unwrap();
        let nine = &native::CATALOGUE[1];
        fs::create_dir_all(nine.dir(&models)).unwrap();
        fs::write(nine.file_path(&models), b"not the file").unwrap();
        let mut out = Vec::new();
        assert_eq!(words(&list_command(&roots, &mut out)).0, "done");
        let said = String::from_utf8(out).unwrap();
        let lines: Vec<&str> = said.lines().collect();
        assert_eq!(lines[0], "qwen3.5-4b  Qwen3.5 4B (Q4_K_M, 2613 MB, Apache-2.0): a download stopped at 5 of 2613 MB");
        assert_eq!(lines[1], "qwen3.5-9b  Qwen3.5 9B (Q4_K_M, 5417 MB, Apache-2.0): installed, not usable: the file was never checked");
        assert!(lines[2].starts_with("DATA has ") && lines[2].ends_with(" MB free"), "{said}");
        assert_eq!(lines.len(), 3);
        assert_eq!(state_words(&tiny("tiny", CONTENT), &models), "not installed");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn progress_is_a_line_every_five_percent_and_none_at_the_end() {
        let model = NativeModel { bytes: 1_000, ..tiny("tiny", CONTENT) };
        let mut progress = Progress::new(&model, "read", 0);
        let mut out = Vec::new();
        for done in [10, 49, 50, 51, 99, 100, 149, 400, 999, 1_000] {
            progress.at(done, &mut out);
        }
        let said = String::from_utf8(out).unwrap();
        let percents: Vec<&str> = said.lines().map(|l| l.rsplit_once('(').unwrap().1).collect();
        assert_eq!(percents, ["5%)", "10%)", "40%)", "99%)"], "{said}");
        assert!(said.starts_with("tiny: read 0 of 0 MB (5%)\n"), "{said}");
        // A resumed copy starts counting from what is there.
        let mut resumed = Progress::new(&model, "read", 620);
        let mut out = Vec::new();
        for done in [630, 649, 650] {
            resumed.at(done, &mut out);
        }
        assert_eq!(String::from_utf8(out).unwrap().lines().count(), 1);
    }

    use crate::fetch::test_support::{Answer, range_start, serve, step};

    /// A file a connection can be cut in the middle of: 800,000 bytes.
    fn file() -> Vec<u8> {
        (0..200_000u32).flat_map(u32::to_le_bytes).collect()
    }

    /// The rest of `content` from a byte on, as a server that takes ranges
    /// answers.
    fn part(content: &[u8], from: usize) -> Answer {
        Answer::new("HTTP/1.1 206 Partial Content", &content[from..])
            .header(format!("content-range: bytes {from}-{}/{}", content.len() - 1, content.len()))
    }

    /// The byte each request asked from, in the order the requests came.
    fn asked(heads: &std::sync::mpsc::Receiver<String>) -> Vec<Option<u64>> {
        heads.try_iter().map(|head| range_start(&head)).collect()
    }

    /// Nothing listens here: a request to it fails the test that made it.
    const NOBODY: &str = "http://127.0.0.1:1/unused";

    #[test]
    fn a_download_is_installed_with_its_record_after_one_request() {
        let roots = temp_roots("model-download");
        let content = file();
        let model = tiny("tiny", &content);
        let models = models_dir(&roots);
        let whole = content.clone();
        let (url, heads) = serve(vec![step(move |_| Answer::new("HTTP/1.1 200 OK", &whole))]);
        let mut out = Vec::new();
        assert_eq!(words(&install_with(&roots, &model, &url, &mut out, &LIMITS)), ("done", ""));
        assert_eq!(fs::read(model.file_path(&models)).unwrap(), content);
        assert_eq!(native::installed(&model, &models), Installed::Verified);
        assert_eq!(names(&model, &models), ["tiny.gguf", "verified"]);
        assert_eq!(asked(&heads), [None], "one request, for the whole");
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains(&format!("tiny: downloading tiny.gguf (0 MB) from {url}")), "{said}");
        assert!(said.contains("tiny: installed and verified"), "{said}");
        // Again: nothing to do, and nobody is asked.
        assert_eq!(
            words(&install_with(&roots, &model, NOBODY, &mut Vec::new(), &LIMITS)),
            ("nothing", "tiny is installed and verified; nothing to do")
        );
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_connection_that_ends_early_is_followed_by_a_request_for_the_rest() {
        let roots = temp_roots("model-resume");
        let content = file();
        let model = tiny("tiny", &content);
        let (first, second, third) = (content.clone(), content.clone(), content.clone());
        let (url, heads) = serve(vec![
            // The length promised, and the connection closed short of it.
            step(move |_| Answer::new("HTTP/1.1 200 OK", &first).cut_after(300_000)),
            // No length promised, and the body simply ends.
            step(move |head| part(&second, range_start(head).unwrap() as usize).without_length().cut_after(200_000)),
            step(move |head| part(&third, range_start(head).unwrap() as usize)),
        ]);
        let mut out = Vec::new();
        let outcome = install_with(&roots, &model, &url, &mut out, &LIMITS);
        let said = String::from_utf8(out).unwrap();
        assert_eq!(words(&outcome), ("done", ""), "{said}");
        assert_eq!(asked(&heads), [None, Some(300_000), Some(500_000)]);
        assert_eq!(fs::read(model.file_path(&models_dir(&roots))).unwrap(), content);
        assert_eq!(said.matches("asking for the rest").count(), 2, "{said}");
        assert!(said.contains("tiny: the connection ended after 0 of 0 MB; asking for the rest"), "{said}");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn what_an_earlier_download_left_is_hashed_again_and_the_rest_asked_for() {
        let roots = temp_roots("model-partial");
        let content = file();
        let model = tiny("tiny", &content);
        let models = models_dir(&roots);
        let start_with = |left: &[u8]| {
            let _ = fs::remove_dir_all(model.dir(&models));
            fs::create_dir_all(model.dir(&models)).unwrap();
            fs::write(model.partial_path(&models), left).unwrap();
        };
        let the_rest = || {
            let rest = content.clone();
            serve(vec![step(move |head| part(&rest, range_start(head).unwrap() as usize))])
        };

        start_with(&content[..123_456]);
        let (url, heads) = the_rest();
        let mut out = Vec::new();
        assert_eq!(words(&install_with(&roots, &model, &url, &mut out, &LIMITS)), ("done", ""));
        assert_eq!(asked(&heads), [Some(123_456)]);
        assert_eq!(native::installed(&model, &models), Installed::Verified);
        assert!(String::from_utf8(out).unwrap().contains("tiny: 0 of 0 MB are there from an earlier download; checking them"));

        // Bytes that were not the file's: the whole hashes wrong and nothing is kept.
        let mut left = content[..123_456].to_vec();
        left[1_000] ^= 0xff;
        start_with(&left);
        let (url, _heads) = the_rest();
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS);
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "failed");
        assert!(reason.starts_with("the download has the size of tiny.gguf and its sha256 is "), "{reason}");
        assert!(reason.ends_with("nothing of it is kept. Install tiny again"), "{reason}");
        assert_eq!(names(&model, &models), Vec::<String>::new());

        // All of it is there (a run that ended between the last byte and the rename): nobody is asked.
        start_with(&content);
        assert_eq!(words(&install_with(&roots, &model, NOBODY, &mut Vec::new(), &LIMITS)), ("done", ""));
        assert_eq!(native::installed(&model, &models), Installed::Verified);

        // More than the file is no start of it: the whole is asked for.
        let mut more = content.clone();
        more.extend_from_slice(b"and more");
        start_with(&more);
        let whole = content.clone();
        let (url, heads) = serve(vec![step(move |_| Answer::new("HTTP/1.1 200 OK", &whole))]);
        assert_eq!(words(&install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS)), ("done", ""));
        assert_eq!(asked(&heads), [None]);
        assert_eq!(fs::read(model.file_path(&models)).unwrap(), content);
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_source_that_takes_no_range_sends_the_file_again_once() {
        let roots = temp_roots("model-noranges");
        let content = file();
        let model = tiny("tiny", &content);
        let models = models_dir(&roots);
        fs::create_dir_all(model.dir(&models)).unwrap();
        fs::write(model.partial_path(&models), &content[..100_000]).unwrap();
        let whole = content.clone();
        let (url, heads) = serve(vec![step(move |_| Answer::new("HTTP/1.1 200 OK", &whole))]);
        let mut out = Vec::new();
        assert_eq!(words(&install_with(&roots, &model, &url, &mut out, &LIMITS)), ("done", ""));
        assert_eq!(asked(&heads), [Some(100_000)], "the rest was asked for");
        assert_eq!(fs::read(model.file_path(&models)).unwrap(), content, "the whole came, and was written from the first byte");
        assert!(String::from_utf8(out).unwrap().contains("tiny: the source takes no ranges; the file comes again from its first byte"));

        // Cut on such a source: a further request would bring the whole a third time.
        fs::remove_dir_all(model.dir(&models)).unwrap();
        fs::create_dir_all(model.dir(&models)).unwrap();
        fs::write(model.partial_path(&models), &content[..100_000]).unwrap();
        let (first, second) = (content.clone(), content.clone());
        let (url, heads) = serve(vec![
            step(move |_| Answer::new("HTTP/1.1 200 OK", &first).cut_after(50_000)),
            step(move |_| Answer::new("HTTP/1.1 200 OK", &second)),
        ]);
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS);
        assert_eq!(
            words(&outcome),
            ("failed", "the source takes no ranges and the download stopped again, after 50000 of 800000 bytes; what arrived is kept")
        );
        assert_eq!(asked(&heads), [Some(100_000), Some(50_000)]);
        assert_eq!(fs::read(model.partial_path(&models)).unwrap(), &content[..50_000]);
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_file_of_another_size_at_the_source_is_refused_before_a_byte_is_written() {
        let roots = temp_roots("model-size");
        let content = file();
        let model = tiny("tiny", &content);
        let models = models_dir(&roots);
        let other = content.clone();
        let (url, _heads) = serve(vec![step(move |_| Answer::new("HTTP/1.1 200 OK", &other).declaring(800_001))]);
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS);
        let another_size = "the source's file is 800001 bytes and the catalogue's tiny.gguf is 800000; nothing was downloaded";
        assert_eq!(words(&outcome), ("failed", another_size));
        assert_eq!(names(&model, &models), Vec::<String>::new(), "no empty .partial is left");

        // The same on a resume: what was there stays as it was.
        fs::write(model.partial_path(&models), &content[..1_000]).unwrap();
        let (url, _heads) = serve(vec![step(|_| {
            Answer::new("HTTP/1.1 206 Partial Content", b"x").header("content-range: bytes 1000-800000/800001".into())
        })]);
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS);
        assert_eq!(words(&outcome), ("failed", another_size));
        assert_eq!(fs::read(model.partial_path(&models)).unwrap(), &content[..1_000]);
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_request_that_brings_no_byte_ends_the_job_and_twenty_requests_are_the_most() {
        let roots = temp_roots("model-nothing");
        let content = file();
        let model = tiny("tiny", &content);
        let models = models_dir(&roots);
        let whole = content.clone();
        let (url, heads) = serve(vec![step(move |_| Answer::new("HTTP/1.1 200 OK", &whole).cut_after(0))]);
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS);
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "failed");
        assert!(reason.starts_with("the download stopped after 0 of 800000 bytes: the connection failed ("), "{reason}");
        assert!(reason.ends_with("). What arrived is kept, and installing again resumes it"), "{reason}");
        assert!(!reason.contains("http"), "the reason names no address: {reason}");
        assert_eq!(asked(&heads), [None], "no second request after one that brought nothing");
        assert_eq!(names(&model, &models), Vec::<String>::new(), "an empty .partial is no download");

        let (url, _heads) = serve(vec![step(|_| Answer::new("HTTP/1.1 404 Not Found", b"no such file"))]);
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS);
        assert_eq!(words(&outcome), ("failed", format!("GET {url} → HTTP 404: Not Found").as_str()));
        assert_eq!(names(&model, &models), Vec::<String>::new());

        let outcome = install_with(&roots, &model, NOBODY, &mut Vec::new(), &LIMITS);
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "failed");
        assert!(reason.starts_with(&format!("GET {NOBODY}: ")), "{reason}");
        assert_eq!(reason.matches("http").count(), 1, "the address as it was given, once: {reason}");

        // A byte a request, each asking for the rest: the twentieth request is the last.
        let mut script = Vec::new();
        for _ in 0..REQUESTS_MAX {
            let rest = content.clone();
            script.push(step(move |head| match range_start(head) {
                None => Answer::new("HTTP/1.1 200 OK", &rest).cut_after(1),
                Some(from) => part(&rest, from as usize).cut_after(1),
            }));
        }
        let (url, heads) = serve(script);
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &LIMITS);
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "failed");
        assert!(reason.starts_with("the download stopped after 20 of 800000 bytes"), "{reason}");
        assert_eq!(asked(&heads), (0..20).map(|n| (n > 0).then_some(n)).collect::<Vec<_>>());
        assert_eq!(fs::read(model.partial_path(&models)).unwrap(), &content[..20], "what arrived is kept");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_file_that_is_there_is_checked_before_anybody_is_asked() {
        let roots = temp_roots("model-there");
        let content = file();
        let model = tiny("tiny", &content);
        let models = models_dir(&roots);
        fs::create_dir_all(model.dir(&models)).unwrap();
        fs::write(model.file_path(&models), &content).unwrap();
        // The catalogue's bytes without a record: verified, and no request.
        let mut out = Vec::new();
        assert_eq!(words(&install_with(&roots, &model, NOBODY, &mut out, &LIMITS)), ("done", ""));
        assert_eq!(native::installed(&model, &models), Installed::Verified);
        let said = String::from_utf8(out).unwrap();
        assert!(said.contains("tiny: a file is there (the file was never checked)"), "{said}");
        assert!(said.contains("tiny: it is the catalogue's file; verified"), "{said}");

        // Other bytes under the file's name, and no record: removed, then downloaded.
        let mut wrong = content.clone();
        wrong[5] ^= 1;
        fs::write(model.file_path(&models), &wrong).unwrap();
        fs::remove_file(model.verified_path(&models)).unwrap();
        let whole = content.clone();
        let (url, heads) = serve(vec![step(move |_| Answer::new("HTTP/1.1 200 OK", &whole))]);
        let mut out = Vec::new();
        assert_eq!(words(&install_with(&roots, &model, &url, &mut out, &LIMITS)), ("done", ""));
        assert_eq!(asked(&heads), [None]);
        assert_eq!(fs::read(model.file_path(&models)).unwrap(), content);
        assert_eq!(native::installed(&model, &models), Installed::Verified);
        assert!(String::from_utf8(out).unwrap().contains("it is not Tiny's file and was removed"));
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn the_reserve_stops_a_download_and_what_arrived_is_kept() {
        let roots = temp_roots("model-download-reserve");
        let content = file();
        let model = tiny("tiny", &content);
        let models = models_dir(&roots);
        // Before: the file and the reserve do not both fit, and nobody is asked.
        let tight = |_: &Path| Ok(RESERVE_BYTES + 799_999);
        let outcome = install_with(&roots, &model, NOBODY, &mut Vec::new(), &Limits { check_every: 8, free: &tight });
        assert_eq!(words(&outcome), ("refused", "refused: DATA has 256 MB free; Tiny needs 0 MB plus a reserve of 256 MB"));
        assert_eq!(names(&model, &models), Vec::<String>::new());
        // With a part there, the rest is what has to fit.
        fs::write(model.partial_path(&models), &content[..700_000]).unwrap();
        let rest_does_not_fit = |_: &Path| Ok(RESERVE_BYTES + 99_999);
        let outcome = install_with(&roots, &model, NOBODY, &mut Vec::new(), &Limits { check_every: 8, free: &rest_does_not_fit });
        assert_eq!(words(&outcome), ("refused", "refused: DATA has 256 MB free; the rest of Tiny needs 0 MB plus a reserve of 256 MB"));
        assert_eq!(fs::metadata(model.partial_path(&models)).unwrap().len(), 700_000, "what was there stays");

        // During: DATA fills while the rest arrives.
        let calls = std::cell::Cell::new(0u32);
        let filling = |_: &Path| {
            calls.set(calls.get() + 1);
            Ok(if calls.get() == 1 { RESERVE_BYTES + 1_000_000 } else { RESERVE_BYTES - 1 })
        };
        let rest = content.clone();
        let (url, _heads) = serve(vec![step(move |head| part(&rest, range_start(head).unwrap() as usize))]);
        let outcome = install_with(&roots, &model, &url, &mut Vec::new(), &Limits { check_every: 8, free: &filling });
        let (kind, reason) = words(&outcome);
        assert_eq!(kind, "refused");
        assert_eq!(reason, "refused: DATA is down to 255 MB free and its reserve of 256 MB stays; what arrived is kept (/native remove frees it)");
        let kept = fs::read(model.partial_path(&models)).unwrap();
        assert!(kept.len() > 700_000 && kept == content[..kept.len()], "{}", kept.len());
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    /// The models are DATA's, and no backup is: a snapshot holds none of
    /// them, and a restore leaves them as they are. `snapshot` names the two
    /// directories of DATA it archives and replaces; this pins that the
    /// models' directory is neither.
    #[test]
    fn the_models_are_in_no_snapshot_and_a_restore_leaves_them() {
        let roots = temp_roots("model-snapshot");
        populate(&roots);
        let model = tiny("tiny", CONTENT);
        let models = models_dir(&roots);
        assert_eq!(words(&import_with(&roots, &model, CONTENT, &mut Vec::new(), &LIMITS)).0, "done");
        assert_ne!(MODELS_SUBDIR, crate::paths::DATABASE_DIR);
        assert_ne!(MODELS_SUBDIR, crate::paths::WORKSPACE_DIR);

        let record = match crate::snapshot::create(&roots, "manual", "manual", Some(&release("fjall")), &mut Vec::new()).unwrap() {
            crate::snapshot::CreateOutcome::Created(record) => *record,
            crate::snapshot::CreateOutcome::Refused(reason) => panic!("refused: {reason}"),
        };
        let archive = roots.backups.join(&record.name).join(crate::paths::ARCHIVE);
        let mut tar = tar::Archive::new(flate2::read::GzDecoder::new(File::open(archive).unwrap()));
        let members: Vec<String> = tar.entries().unwrap().map(|e| e.unwrap().path().unwrap().to_string_lossy().into_owned()).collect();
        assert!(members.iter().any(|m| m.starts_with("data/wardsondb")), "{members:?}");
        assert!(!members.iter().any(|m| m.contains("models") || m.contains("tiny")), "{members:?}");

        // The machine runs on: the model is removed and another file comes.
        fs::write(model.dir(&models).join("later"), b"after the snapshot").unwrap();
        fs::write(roots.state().join("soul.sha256"), "tampered").unwrap();
        let restored = crate::snapshot::restore(&roots, &record.name, Some(&release("fjall")), &mut Vec::new()).unwrap();
        assert!(matches!(restored, crate::snapshot::RestoreOutcome::Restored));
        assert_eq!(fs::read_to_string(roots.state().join("soul.sha256")).unwrap(), "abc", "STATE is the snapshot's");
        assert_eq!(names(&model, &models), ["later", "tiny.gguf", "verified"], "the models' directory is as the machine left it");
        assert_eq!(native::installed(&model, &models), Installed::Verified);
        fs::remove_dir_all(&roots.parent).unwrap();
    }
}
