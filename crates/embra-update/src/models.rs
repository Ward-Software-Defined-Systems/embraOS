//! Native models (experimental): the files of the catalogue's models on
//! DATA (`embra_boot::native_model`). A model's file is the catalogue's
//! bytes or it is not kept. A file read from standard input (the web
//! console's upload) is hashed while it is written under a `.partial` name;
//! it gets its final name, and the verification record embrad starts a
//! model by, only when its size and its sha256 are the catalogue's. `verify`
//! hashes an installed file again and writes that record, or removes a file
//! that is not the catalogue's.
//!
//! One job at a time: a lock in the models directory, held until the job
//! ends. DATA's reserve stays free, as beside a snapshot.
//!
//! The models are in no snapshot and no restore touches them: `snapshot`
//! names the two directories of DATA it holds, and this is neither. A model
//! is downloaded again, not backed up.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use embra_boot::native_model::{self as native, Installed, NativeModel, Verified};
use sha2::{Digest, Sha256};

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

/// The sha256 of a file, with progress lines.
fn hash_file(path: &Path, model: &NativeModel, out: &mut dyn Write) -> io::Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; CHUNK];
    let mut progress = Progress::new(model, "checked", 0);
    let mut done: u64 = 0;
    loop {
        let n = match file.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => n,
            Err(e) if e.kind() == io::ErrorKind::Interrupted => continue,
            Err(e) => return Err(e),
        };
        hasher.update(&buf[..n]);
        done += n as u64;
        progress.at(done, out);
    }
    Ok(hex::encode(hasher.finalize()))
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
    let path = model.file_path(&models);
    let len = match fs::metadata(&path) {
        Ok(meta) if meta.is_file() => meta.len(),
        _ => return Outcome::Refused(format!("refused: {} is not installed", model.id)),
    };
    let removed = |why: String| {
        let _ = fs::remove_file(&path);
        let _ = fs::remove_file(model.verified_path(&models));
        Outcome::Failed(format!("{why}: it is not {}'s file and was removed. Install {} again", model.label, model.id))
    };
    if len != model.bytes {
        return removed(format!("{} is {len} bytes and the catalogue's file is {}", model.file, model.bytes));
    }
    let _ = writeln!(out, "{}: checking {} ({} MB)", model.id, model.file, mb(model.bytes));
    let sha256 = match hash_file(&path, model, out) {
        Ok(sha256) => sha256,
        Err(e) => return Outcome::Failed(format!("reading {}: {e}", path.display())),
    };
    if sha256 != model.sha256 {
        return removed(format!("the sha256 of {} is {}…, not {}…", model.file, &sha256[..16], &model.sha256[..16]));
    }
    if let Err(reason) = record(model, &models) {
        return Outcome::Failed(reason);
    }
    let _ = writeln!(out, "{}: verified ({} MB, sha256 {}…)", model.id, mb(model.bytes), &sha256[..16]);
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
