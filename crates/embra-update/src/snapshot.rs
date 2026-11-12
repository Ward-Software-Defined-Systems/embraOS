//! Snapshots of STATE and DATA: a tar.gz under the backups directory with a
//! record beside it, the restore that puts one back, the list, the delete,
//! and the rotation of the automatic ones. The roots are a value, so the
//! tests run all of it under a temporary tree.
//!
//! What a snapshot holds: STATE whole but a halt's reason, the next-boot
//! marker, the saved clock and `lost+found`; DATA's database directory whole; DATA's
//! workspace but the Guardian's build tree (its built artifacts kept: boot
//! reconcile loads them by hash) and its cargo home. Links stay links;
//! special files are skipped and named. Entries are `state/…` and
//! `data/…`, so the archive unpacks under the one parent of both.

use std::fs::{self, File};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use embra_boot::Release;
use flate2::Compression;
use flate2::read::GzDecoder;
use flate2::write::GzEncoder;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::cli::valid_name;
use crate::paths::*;

/// Where STATE and DATA are (`<parent>/state`, `<parent>/data`) and where
/// the backups go.
pub struct Roots {
    pub parent: PathBuf,
    pub backups: PathBuf,
}

impl Roots {
    pub fn os() -> Roots {
        Roots { parent: PathBuf::from(EMBRA_ROOT), backups: PathBuf::from(BACKUPS_DIR) }
    }

    pub fn state(&self) -> PathBuf {
        self.parent.join("state")
    }

    pub fn data(&self) -> PathBuf {
        self.parent.join("data")
    }
}

/// `backup.json` beside the archive.
#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub format: String,
    pub name: String,
    /// UTC, `YYYY-MM-DD_HHMMSS`, the name's prefix; for an upload under a
    /// name without one, when it was taken in.
    pub timestamp: String,
    pub label: String,
    /// `manual`, `scheduled` or `pending-slot`; `uploaded` for an archive
    /// that came from outside (`import`).
    pub taken_by: String,
    /// The release that took the snapshot; none for an upload, whose archive
    /// does not say.
    pub release: Option<ReleaseRecord>,
    /// WardSONDB's engine marker, when the database directory had one.
    pub data_engine: Option<String>,
    pub files: u64,
    pub bytes_uncompressed: u64,
    pub bytes_archive: u64,
    pub sha256: String,
    pub excluded: Vec<String>,
    pub skipped: Vec<String>,
    pub created_by: String,
}

#[derive(Serialize, Deserialize, Debug, Clone, PartialEq, Eq)]
pub struct ReleaseRecord {
    pub version: String,
    pub build_id: String,
    pub arch: String,
    pub engine: String,
}

impl From<&Release> for ReleaseRecord {
    fn from(r: &Release) -> Self {
        ReleaseRecord { version: r.version.clone(), build_id: r.build_id.clone(), arch: r.arch.clone(), engine: r.engine.clone() }
    }
}

/// One entry of the listing.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Listed {
    pub name: String,
    /// The record and the archive are both there and the name is final.
    pub complete: bool,
    pub archive_bytes: u64,
    pub record: Option<Record>,
}

impl Listed {
    pub fn label(&self) -> Option<&str> {
        self.record.as_ref().map(|r| r.label.as_str())
    }

    /// The archive came from outside (`import`): whatever its label says,
    /// it is not a snapshot this machine took.
    pub fn uploaded(&self) -> bool {
        self.record.as_ref().is_some_and(|r| r.taken_by == UPLOADED)
    }

    /// `NAME  STATE  SIZE  label  release  taken_by`, one line.
    pub fn line(&self) -> String {
        let state = if self.complete { "ok" } else { "incomplete" };
        let size = format!("{} MB", self.archive_bytes / (1024 * 1024));
        match &self.record {
            Some(r) => format!(
                "{}  {state}  {size}  {}  {}  {}",
                self.name,
                r.label,
                r.release.as_ref().map(|x| x.version.as_str()).unwrap_or("-"),
                r.taken_by
            ),
            None => format!("{}  {state}  {size}", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Kind {
    Dir,
    File(u64),
    Symlink,
}

#[derive(Debug, Clone)]
struct Entry {
    archive: PathBuf,
    source: PathBuf,
    kind: Kind,
}

/// What a snapshot would hold.
pub struct Plan {
    entries: Vec<Entry>,
    pub bytes: u64,
    pub files: u64,
    pub skipped: Vec<String>,
}

/// Whether a path relative to the workspace is left out of a snapshot.
fn workspace_excluded(rel: &Path) -> bool {
    let rel_str = rel.to_string_lossy();
    if rel_str == GUARDIAN_CARGO_HOME || rel_str.starts_with(&format!("{GUARDIAN_CARGO_HOME}/")) {
        return true;
    }
    if rel_str == GUARDIAN_TARGET || rel_str.starts_with(&format!("{GUARDIAN_TARGET}/")) {
        // The chain down to the artifacts directory, and the artifacts.
        if GUARDIAN_ARTIFACTS.starts_with(&*rel_str) {
            return false;
        }
        if let Some(file) = rel_str.strip_prefix(&format!("{GUARDIAN_ARTIFACTS}/")) {
            return !(file.ends_with(".wasm") && !file.contains('/'));
        }
        return true;
    }
    false
}

/// Whether a path relative to STATE is left out.
fn state_excluded(rel: &Path) -> bool {
    rel.components().count() == 1 && STATE_EXCLUDED.iter().any(|x| rel == Path::new(x))
}

fn walk(root: &Path, archive_prefix: &Path, excluded: &dyn Fn(&Path) -> bool, plan: &mut Plan) -> Result<()> {
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let dir = root.join(&rel);
        let mut names: Vec<_> = fs::read_dir(&dir)
            .with_context(|| format!("read {}", dir.display()))?
            .filter_map(|e| e.ok().map(|e| e.file_name()))
            .collect();
        names.sort();
        for name in names {
            let rel_child = rel.join(&name);
            if excluded(&rel_child) {
                continue;
            }
            let source = root.join(&rel_child);
            let meta = fs::symlink_metadata(&source).with_context(|| format!("stat {}", source.display()))?;
            let archive = archive_prefix.join(&rel_child);
            let ft = meta.file_type();
            if ft.is_symlink() {
                plan.entries.push(Entry { archive, source, kind: Kind::Symlink });
                plan.files += 1;
            } else if ft.is_dir() {
                plan.entries.push(Entry { archive, source, kind: Kind::Dir });
                stack.push(rel_child);
            } else if ft.is_file() {
                plan.bytes += meta.len();
                plan.files += 1;
                plan.entries.push(Entry { archive, source, kind: Kind::File(meta.len()) });
            } else {
                plan.skipped.push(format!("{} (special file)", archive.display()));
            }
        }
    }
    Ok(())
}

pub fn plan(roots: &Roots) -> Result<Plan> {
    let mut plan = Plan { entries: Vec::new(), bytes: 0, files: 0, skipped: Vec::new() };
    let state = roots.state();
    if !state.is_dir() {
        bail!("STATE is not a directory: {}", state.display());
    }
    plan.entries.push(Entry { archive: PathBuf::from("state"), source: state.clone(), kind: Kind::Dir });
    walk(&state, Path::new("state"), &state_excluded, &mut plan)?;
    let data = roots.data();
    plan.entries.push(Entry { archive: PathBuf::from("data"), source: data.clone(), kind: Kind::Dir });
    for sub in [DATABASE_DIR, WORKSPACE_DIR] {
        let dir = data.join(sub);
        if !dir.is_dir() {
            continue;
        }
        let prefix = Path::new("data").join(sub);
        plan.entries.push(Entry { archive: prefix.clone(), source: dir.clone(), kind: Kind::Dir });
        if sub == WORKSPACE_DIR {
            walk(&dir, &prefix, &workspace_excluded, &mut plan)?;
        } else {
            walk(&dir, &prefix, &|_| false, &mut plan)?;
        }
    }
    Ok(plan)
}

/// The free-space rule: what the snapshot needs plus the reserve.
pub fn enough_space(free: u64, needed: u64, reserve: u64) -> bool {
    free >= needed.saturating_add(reserve)
}

/// Bytes free for an unprivileged writer on the filesystem of `path`.
pub fn free_bytes(path: &Path) -> io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let c = std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path with a NUL byte"))?;
    // SAFETY: a zeroed statvfs struct the call fills; the path is NUL-terminated.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(io::Error::last_os_error());
    }
    // The field types differ by platform (u64 on 64-bit Linux, narrower
    // elsewhere); the product is computed wide and saturated.
    let bytes = u128::from(st.f_bavail) * u128::from(st.f_frsize);
    Ok(u64::try_from(bytes).unwrap_or(u64::MAX))
}

/// Bytes of the regular files under `path` (links not followed); 0 when absent.
pub fn tree_bytes(path: &Path) -> u64 {
    let mut total = 0;
    let mut stack = vec![path.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            let Ok(meta) = e.metadata() else { continue };
            if meta.file_type().is_symlink() {
                continue;
            }
            if meta.is_dir() {
                stack.push(e.path());
            } else if meta.is_file() {
                total += meta.len();
            }
        }
    }
    total
}

/// A writer that hashes and counts what passes through it.
pub struct HashingWriter<W: Write> {
    inner: W,
    hasher: Sha256,
    bytes: u64,
}

impl<W: Write> HashingWriter<W> {
    pub fn new(inner: W) -> Self {
        HashingWriter { inner, hasher: Sha256::new(), bytes: 0 }
    }

    pub fn finish(self) -> (W, String, u64) {
        (self.inner, hex::encode(self.hasher.finalize()), self.bytes)
    }
}

impl<W: Write> Write for HashingWriter<W> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let n = self.inner.write(buf)?;
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> io::Result<()> {
        self.inner.flush()
    }
}

pub fn sha256_of(path: &Path) -> io::Result<String> {
    let mut f = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; 1 << 20];
    loop {
        let n = f.read(&mut buf)?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

/// `YYYY-MM-DD_HHMMSS` in UTC for seconds since the epoch.
pub fn utc_stamp(secs: u64) -> String {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    let (h, m, s) = (rem / 3600, (rem % 3600) / 60, rem % 60);
    // Howard Hinnant's civil-from-days.
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let mo = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if mo <= 2 { y + 1 } else { y };
    format!("{y:04}-{mo:02}-{d:02}_{h:02}{m:02}{s:02}")
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

pub(crate) fn sync_dir(path: &Path) -> io::Result<()> {
    File::open(path)?.sync_all()
}

/// What `create` came to.
pub enum CreateOutcome {
    Created(Box<Record>),
    Refused(String),
}

/// Take a snapshot: the plan, the free-space rule, the archive written into
/// `<name>.partial`, hashed as it is written and fsynced, the record, then
/// the rename to its final name. A crash leaves a `.partial` that `list`
/// reports and `delete` removes.
pub fn create(roots: &Roots, label: &str, taken_by: &str, release: Option<&Release>, out: &mut impl Write) -> Result<CreateOutcome> {
    let plan = plan(roots)?;
    let free = free_bytes(&roots.data()).with_context(|| format!("free space of {}", roots.data().display()))?;
    if !enough_space(free, plan.bytes, RESERVE_BYTES) {
        return Ok(CreateOutcome::Refused(format!(
            "refused: DATA has {} MB free; the snapshot needs {} MB plus a reserve of {} MB. Delete a backup (embra-update delete <name>, or /backup delete) and try again",
            free / (1024 * 1024),
            plan.bytes / (1024 * 1024),
            RESERVE_BYTES / (1024 * 1024)
        )));
    }
    writeln!(out, "plan: {} files, {} MB, {} skipped", plan.files, plan.bytes / (1024 * 1024), plan.skipped.len())?;
    let timestamp = utc_stamp(now_secs());
    let name = format!("{timestamp}_{label}");
    let final_dir = roots.backups.join(&name);
    if final_dir.exists() {
        bail!("a backup named {name} exists already");
    }
    let partial = roots.backups.join(format!("{name}{PARTIAL_SUFFIX}"));
    if partial.exists() {
        fs::remove_dir_all(&partial)?;
    }
    fs::create_dir_all(&partial).with_context(|| format!("create {}", partial.display()))?;

    let archive_path = partial.join(ARCHIVE);
    let file = File::create(&archive_path).with_context(|| format!("create {}", archive_path.display()))?;
    let encoder = GzEncoder::new(HashingWriter::new(file), Compression::fast());
    let mut builder = tar::Builder::new(encoder);
    builder.follow_symlinks(false);
    for e in &plan.entries {
        match e.kind {
            Kind::Dir => builder.append_dir(&e.archive, &e.source),
            Kind::File(_) | Kind::Symlink => builder.append_path_with_name(&e.source, &e.archive),
        }
        .with_context(|| format!("archive {}", e.source.display()))?;
    }
    let encoder = builder.into_inner().context("finish the archive")?;
    let hashing = encoder.finish().context("finish the compression")?;
    let (file, sha256, bytes_archive) = hashing.finish();
    file.sync_all()?;
    drop(file);
    writeln!(out, "archive: {} MB written, sha256 {}", bytes_archive / (1024 * 1024), &sha256[..16])?;

    let data_engine = fs::read_to_string(roots.data().join(DATABASE_DIR).join(ENGINE_MARKER)).ok().map(|s| s.trim().to_string());
    let record = Record {
        format: FORMAT.to_string(),
        name: name.clone(),
        timestamp,
        label: label.to_string(),
        taken_by: taken_by.to_string(),
        release: release.map(ReleaseRecord::from),
        data_engine,
        files: plan.files,
        bytes_uncompressed: plan.bytes,
        bytes_archive,
        sha256,
        excluded: vec![
            format!("state/{{{}}}", STATE_EXCLUDED.join(",")),
            format!("data/{WORKSPACE_DIR}/{GUARDIAN_TARGET} (except {GUARDIAN_ARTIFACTS}/*.wasm)"),
            format!("data/{WORKSPACE_DIR}/{GUARDIAN_CARGO_HOME}"),
            "data/backups".to_string(),
        ],
        skipped: plan.skipped.clone(),
        created_by: "embra-update".to_string(),
    };
    let record_path = partial.join(RECORD);
    let mut rf = File::create(&record_path)?;
    rf.write_all(serde_json::to_string_pretty(&record)?.as_bytes())?;
    rf.write_all(b"\n")?;
    rf.sync_all()?;
    drop(rf);
    sync_dir(&partial)?;
    fs::rename(&partial, &final_dir).with_context(|| format!("rename to {}", final_dir.display()))?;
    sync_dir(&roots.backups)?;
    Ok(CreateOutcome::Created(Box::new(record)))
}

/// Every entry under the backups directory, newest first (the names start
/// with the UTC stamp).
pub fn list(roots: &Roots) -> Vec<Listed> {
    let Ok(rd) = fs::read_dir(&roots.backups) else { return Vec::new() };
    let mut out: Vec<Listed> = rd
        .flatten()
        .filter(|e| e.path().is_dir())
        .map(|e| {
            let name = e.file_name().to_string_lossy().to_string();
            let dir = e.path();
            let record = fs::read_to_string(dir.join(RECORD)).ok().and_then(|t| serde_json::from_str::<Record>(&t).ok());
            let archive_bytes = fs::metadata(dir.join(ARCHIVE)).map(|m| m.len()).unwrap_or(0);
            let complete = !name.ends_with(PARTIAL_SUFFIX) && record.is_some() && dir.join(ARCHIVE).is_file();
            Listed { name, complete, archive_bytes, record }
        })
        .collect();
    out.sort_by(|a, b| b.name.cmp(&a.name));
    out
}

/// Whether a complete snapshot with this label was taken here: what
/// `snapshot --once` asks before a release's first run. An uploaded archive
/// with the label is another machine's or another time's and does not count.
pub fn taken_here(entries: &[Listed], label: &str) -> bool {
    entries.iter().any(|e| e.complete && !e.uploaded() && e.label() == Some(label))
}

/// Remove the automatic snapshots beyond the newest `KEEP_AUTOMATIC`; a
/// manual one is never removed, and neither is an uploaded one, whatever
/// its label. The names removed.
pub fn rotate(roots: &Roots) -> Result<Vec<String>> {
    let automatic: Vec<Listed> = list(roots)
        .into_iter()
        .filter(|e| e.complete && !e.uploaded() && e.label().is_some_and(|l| l.starts_with(AUTOMATIC_PREFIX)))
        .collect();
    let mut removed = Vec::new();
    for e in automatic.iter().skip(KEEP_AUTOMATIC) {
        fs::remove_dir_all(roots.backups.join(&e.name)).with_context(|| format!("remove {}", e.name))?;
        removed.push(e.name.clone());
    }
    Ok(removed)
}

pub enum RestoreOutcome {
    Restored,
    Refused(String),
}

/// Put a snapshot back: the record and the archive checked against each
/// other, the engine against the running release, the space, then STATE
/// emptied (but `lost+found` and the saved clock), the database and
/// workspace directories removed, the archive unpacked under the parent,
/// and a sync.
pub fn restore(roots: &Roots, name: &str, release: Option<&Release>, out: &mut impl Write) -> Result<RestoreOutcome> {
    if !valid_name(name) {
        return Ok(RestoreOutcome::Refused(format!("restore: not a backup name: {name:?}")));
    }
    let dir = roots.backups.join(name);
    let record: Record = match fs::read_to_string(dir.join(RECORD)) {
        Ok(t) => serde_json::from_str(&t).with_context(|| format!("parse {}", dir.join(RECORD).display()))?,
        Err(_) => return Ok(RestoreOutcome::Refused(format!("restore: \"{name}\" is incomplete or missing (no {RECORD}); nothing was changed"))),
    };
    let archive = dir.join(ARCHIVE);
    if !archive.is_file() {
        return Ok(RestoreOutcome::Refused(format!("restore: \"{name}\" has no {ARCHIVE}; nothing was changed")));
    }
    if record.format != FORMAT {
        return Ok(RestoreOutcome::Refused(format!("restore: \"{name}\" is a {} record, not {FORMAT}", record.format)));
    }
    writeln!(out, "check: hashing {}", archive.display())?;
    let sha = sha256_of(&archive)?;
    if sha != record.sha256 {
        return Ok(RestoreOutcome::Refused(format!("restore: the archive of \"{name}\" does not match its record (sha256 {} vs {}); nothing was changed", &sha[..16], &record.sha256[..16.min(record.sha256.len())])));
    }
    if let (Some(data_engine), Some(rel)) = (&record.data_engine, release)
        && data_engine != &rel.engine
    {
        return Ok(RestoreOutcome::Refused(format!(
            "restore: \"{name}\" holds a {data_engine} database and this release runs {}; a database of the other engine cannot be opened. seed-state.sh --wipe data on the host is the way to start over; nothing was changed",
            rel.engine
        )));
    }
    let data = roots.data();
    let free = free_bytes(&data)?;
    let current = tree_bytes(&data.join(DATABASE_DIR)) + tree_bytes(&data.join(WORKSPACE_DIR));
    if !enough_space(free.saturating_add(current), record.bytes_uncompressed, RESERVE_BYTES) {
        return Ok(RestoreOutcome::Refused(format!(
            "restore: DATA has {} MB free ({} MB freed by the restore); \"{name}\" unpacks to {} MB plus a reserve of {} MB; nothing was changed",
            free / (1024 * 1024),
            current / (1024 * 1024),
            record.bytes_uncompressed / (1024 * 1024),
            RESERVE_BYTES / (1024 * 1024)
        )));
    }

    writeln!(out, "restore: emptying STATE and the database and workspace directories")?;
    let state = roots.state();
    for e in fs::read_dir(&state).with_context(|| format!("read {}", state.display()))?.flatten() {
        if STATE_KEPT_BY_RESTORE.iter().any(|kept| e.file_name() == *kept) {
            continue;
        }
        let p = e.path();
        if fs::symlink_metadata(&p)?.is_dir() {
            fs::remove_dir_all(&p)?;
        } else {
            fs::remove_file(&p)?;
        }
    }
    for sub in [DATABASE_DIR, WORKSPACE_DIR] {
        let p = data.join(sub);
        if p.exists() {
            fs::remove_dir_all(&p).with_context(|| format!("remove {}", p.display()))?;
        }
    }
    writeln!(out, "restore: unpacking {} files ({} MB)", record.files, record.bytes_uncompressed / (1024 * 1024))?;
    let mut ar = tar::Archive::new(GzDecoder::new(File::open(&archive)?));
    ar.set_preserve_permissions(true);
    ar.set_unpack_xattrs(false);
    ar.set_overwrite(true);
    ar.unpack(&roots.parent).with_context(|| format!("unpack {} under {}", archive.display(), roots.parent.display()))?;
    sync_all();
    writeln!(out, "restore: done — \"{name}\" is in place")?;
    Ok(RestoreOutcome::Restored)
}

#[cfg(target_os = "linux")]
fn sync_all() {
    // SAFETY: sync(2) takes no arguments and cannot fail.
    unsafe { libc::sync() };
}

#[cfg(not(target_os = "linux"))]
fn sync_all() {}

/// Remove one backup by name; `false` when there is none.
pub fn delete(roots: &Roots, name: &str) -> Result<bool> {
    if !valid_name(name) && !name.strip_suffix(PARTIAL_SUFFIX).is_some_and(valid_name) {
        bail!("not a backup name: {name:?}");
    }
    let dir = roots.backups.join(name);
    if !dir.is_dir() {
        return Ok(false);
    }
    fs::remove_dir_all(&dir).with_context(|| format!("remove {}", dir.display()))?;
    Ok(true)
}

/// Every path of a tree with its content, for the tests' comparisons.
#[cfg(test)]
pub fn tree_files(root: &Path) -> std::collections::BTreeMap<String, String> {
    let mut out = std::collections::BTreeMap::new();
    let mut stack = vec![PathBuf::new()];
    while let Some(rel) = stack.pop() {
        let Ok(rd) = fs::read_dir(root.join(&rel)) else { continue };
        for e in rd.flatten() {
            let r = rel.join(e.file_name());
            let meta = fs::symlink_metadata(e.path()).unwrap();
            let key = r.to_string_lossy().to_string();
            if meta.file_type().is_symlink() {
                out.insert(key, format!("-> {}", fs::read_link(e.path()).unwrap().display()));
            } else if meta.is_dir() {
                out.insert(key, "dir".into());
                stack.push(r);
            } else {
                out.insert(key, fs::read_to_string(e.path()).unwrap_or_else(|_| "?".into()));
            }
        }
    }
    out
}

/// A temporary STATE and DATA and what a machine keeps in them, for this
/// module's tests and the import's.
#[cfg(test)]
pub mod test_support {
    use super::*;
    use std::os::unix::fs::symlink;

    pub fn temp_roots(tag: &str) -> Roots {
        let parent = std::env::temp_dir().join(format!("embra-update-{tag}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&parent);
        let roots = Roots { parent: parent.clone(), backups: parent.join("data").join("backups") };
        fs::create_dir_all(roots.state()).unwrap();
        fs::create_dir_all(roots.data()).unwrap();
        fs::create_dir_all(&roots.backups).unwrap();
        roots
    }

    pub fn populate(roots: &Roots) {
        let s = roots.state();
        fs::write(s.join("soul.sha256"), "abc").unwrap();
        fs::create_dir_all(s.join("pki")).unwrap();
        fs::write(s.join("pki/ca.key"), "KEY").unwrap();
        fs::create_dir_all(s.join("ca-certificates")).unwrap();
        fs::write(s.join("ca-certificates/op.pem"), "PEM").unwrap();
        fs::write(s.join("halt_reason"), "old halt").unwrap();
        fs::write(s.join("next-boot"), "mode=snapshot").unwrap();
        fs::write(s.join("last-known-time"), "time=1794500000\n").unwrap();
        fs::create_dir_all(s.join("lost+found")).unwrap();
        let d = roots.data();
        fs::create_dir_all(d.join("wardsondb/db")).unwrap();
        fs::write(d.join("wardsondb/.engine"), "fjall\n").unwrap();
        fs::write(d.join("wardsondb/db/seg1"), "SEG").unwrap();
        let w = d.join("workspace");
        fs::create_dir_all(w.join(".guardian/target/wasm32-unknown-unknown/release")).unwrap();
        fs::create_dir_all(w.join(".guardian/target/debug")).unwrap();
        fs::create_dir_all(w.join(".guardian/cargo-home")).unwrap();
        fs::create_dir_all(w.join(".ssh")).unwrap();
        fs::write(w.join("notes.txt"), "notes").unwrap();
        symlink("notes.txt", w.join("link")).unwrap();
        fs::write(w.join(".guardian/target/wasm32-unknown-unknown/release/tool.wasm"), "WASM").unwrap();
        fs::write(w.join(".guardian/target/wasm32-unknown-unknown/release/tool.d"), "deps").unwrap();
        fs::write(w.join(".guardian/target/debug/junk"), "JUNK").unwrap();
        fs::write(w.join(".guardian/cargo-home/reg"), "REG").unwrap();
        fs::write(w.join(".ssh/id_ed25519"), "SSHKEY").unwrap();
    }

    pub fn release(engine: &str) -> Release {
        Release::parse(&format!("VERSION=0.24.0-phase2\nBUILD_ID=abc\nARCH=x86_64\nENGINE={engine}\nBUILT=now\n")).unwrap()
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    #[test]
    fn a_snapshot_round_trips_under_a_temporary_root_and_keeps_links_as_links() {
        let roots = temp_roots("roundtrip");
        populate(&roots);
        let mut out = Vec::new();
        let record = match create(&roots, "manual", "manual", Some(&release("fjall")), &mut out).unwrap() {
            CreateOutcome::Created(r) => *r,
            CreateOutcome::Refused(m) => panic!("refused: {m}"),
        };
        assert_eq!(record.format, FORMAT);
        assert_eq!(record.label, "manual");
        assert_eq!(record.data_engine.as_deref(), Some("fjall"));
        assert_eq!(record.release.as_ref().unwrap().version, "0.24.0-phase2");
        assert!(record.name.ends_with("_manual"));
        let listed = list(&roots);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].complete);
        assert_eq!(listed[0].name, record.name);
        assert!(listed[0].line().starts_with(&record.name));
        assert_eq!(sha256_of(&roots.backups.join(&record.name).join(ARCHIVE)).unwrap(), record.sha256);

        // What the archive holds, read back by tar: links as links, the
        // Guardian build tree only as its artifacts, no halt reason, no
        // marker, nothing of the cargo home.
        let mut ar = tar::Archive::new(GzDecoder::new(File::open(roots.backups.join(&record.name).join(ARCHIVE)).unwrap()));
        let mut entries: Vec<(String, String)> = Vec::new();
        for e in ar.entries().unwrap() {
            let e = e.unwrap();
            let kind = if e.header().entry_type().is_symlink() {
                format!("-> {}", e.link_name().unwrap().unwrap().display())
            } else if e.header().entry_type().is_dir() {
                "dir".into()
            } else {
                "file".into()
            };
            entries.push((e.path().unwrap().to_string_lossy().to_string(), kind));
        }
        let names: Vec<&str> = entries.iter().map(|(n, _)| n.as_str()).collect();
        assert!(names.contains(&"state/soul.sha256"));
        assert!(names.contains(&"state/pki/ca.key"));
        assert!(names.contains(&"data/wardsondb/.engine"));
        assert!(names.contains(&"data/workspace/notes.txt"));
        assert!(names.contains(&"data/workspace/.guardian/target/wasm32-unknown-unknown/release/tool.wasm"));
        assert!(names.contains(&"data/workspace/.ssh/id_ed25519"));
        for absent in ["state/halt_reason", "state/next-boot", "state/last-known-time", "state/lost+found", "data/workspace/.guardian/target/debug/junk", "data/workspace/.guardian/target/wasm32-unknown-unknown/release/tool.d", "data/workspace/.guardian/cargo-home/reg", "data/backups"] {
            assert!(!names.contains(&absent), "{absent} must be left out");
        }
        assert!(entries.iter().any(|(n, k)| n == "data/workspace/link" && k == "-> notes.txt"));

        // Change the tree, then restore: the original is back, the excluded
        // files are gone (not restored), the link is a link.
        fs::write(roots.state().join("soul.sha256"), "tampered").unwrap();
        fs::remove_file(roots.data().join("workspace/notes.txt")).unwrap();
        fs::write(roots.state().join("stray"), "x").unwrap();
        // The machine has run on since the snapshot, and its clock was saved again.
        fs::write(roots.state().join("last-known-time"), "time=1794600000\n").unwrap();
        let mut out = Vec::new();
        assert!(matches!(restore(&roots, &record.name, Some(&release("fjall")), &mut out).unwrap(), RestoreOutcome::Restored));
        let files = tree_files(&roots.parent);
        assert_eq!(files.get("state/soul.sha256").map(String::as_str), Some("abc"));
        assert_eq!(files.get("data/workspace/notes.txt").map(String::as_str), Some("notes"));
        assert_eq!(files.get("data/workspace/link").map(String::as_str), Some("-> notes.txt"));
        assert_eq!(files.get("data/wardsondb/db/seg1").map(String::as_str), Some("SEG"));
        assert!(!files.contains_key("state/halt_reason"), "the halt reason is not restored");
        assert!(!files.contains_key("state/next-boot"));
        assert!(!files.contains_key("state/stray"), "STATE was emptied first");
        assert!(files.contains_key("state/lost+found"), "lost+found stays");
        assert_eq!(
            files.get("state/last-known-time").map(String::as_str),
            Some("time=1794600000\n"),
            "the saved clock is the machine's: no snapshot holds it and a restore leaves the latest one in place"
        );
        assert!(!files.contains_key("data/workspace/.guardian/target/debug/junk"));
        assert!(files.contains_key("data/workspace/.guardian/target/wasm32-unknown-unknown/release/tool.wasm"));
        assert!(files.contains_key(&format!("data/backups/{}", record.name)), "the backups survive a restore");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn the_free_space_rule_is_needed_plus_the_reserve() {
        assert!(enough_space(1000, 500, 400));
        assert!(enough_space(900, 500, 400));
        assert!(!enough_space(899, 500, 400));
        assert!(enough_space(u64::MAX, u64::MAX, 1), "no overflow");
        assert!(free_bytes(Path::new("/")).unwrap() > 0);
        assert!(free_bytes(Path::new("/nonexistent/dir")).is_err());
    }

    fn fake_entry(roots: &Roots, name: &str, label: &str, engine: Option<&str>, partial: bool) {
        fake_entry_by(roots, name, label, "manual", engine, partial);
    }

    fn fake_entry_by(roots: &Roots, name: &str, label: &str, taken_by: &str, engine: Option<&str>, partial: bool) {
        let dir = roots.backups.join(if partial { format!("{name}{PARTIAL_SUFFIX}") } else { name.to_string() });
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join(ARCHIVE), b"not a real archive").unwrap();
        let sha = sha256_of(&dir.join(ARCHIVE)).unwrap();
        let record = Record {
            format: FORMAT.into(),
            name: name.into(),
            timestamp: name.split('_').take(2).collect::<Vec<_>>().join("_"),
            label: label.into(),
            taken_by: taken_by.into(),
            release: None,
            data_engine: engine.map(str::to_string),
            files: 1,
            bytes_uncompressed: 10,
            bytes_archive: 18,
            sha256: sha,
            excluded: vec![],
            skipped: vec![],
            created_by: "test".into(),
        };
        if !partial {
            fs::write(dir.join(RECORD), serde_json::to_string(&record).unwrap()).unwrap();
        }
    }

    #[test]
    fn a_partial_snapshot_is_listed_as_incomplete_and_never_restored() {
        let roots = temp_roots("partial");
        fake_entry(&roots, "2026-11-05_101200_manual", "manual", None, true);
        let listed = list(&roots);
        assert_eq!(listed.len(), 1);
        assert!(!listed[0].complete);
        assert!(listed[0].line().contains("incomplete"));
        let mut out = Vec::new();
        assert!(matches!(restore(&roots, "2026-11-05_101200_manual.partial", None, &mut out).unwrap(), RestoreOutcome::Refused(_)));
        assert!(matches!(restore(&roots, "2026-11-05_101200_manual", None, &mut out).unwrap(), RestoreOutcome::Refused(m) if m.contains("incomplete or missing")));
        assert!(matches!(restore(&roots, "../etc", None, &mut out).unwrap(), RestoreOutcome::Refused(m) if m.contains("not a backup name")));
        assert!(delete(&roots, "2026-11-05_101200_manual.partial").unwrap());
        assert!(!delete(&roots, "2026-11-05_101200_manual").unwrap());
        assert!(delete(&roots, "../etc").is_err());
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn rotation_removes_the_oldest_automatic_snapshot_and_never_a_manual_one() {
        let roots = temp_roots("rotate");
        fake_entry(&roots, "2026-11-01_000000_pre-v0.22.0", "pre-v0.22.0", None, false);
        fake_entry(&roots, "2026-11-02_000000_pre-v0.23.0", "pre-v0.23.0", None, false);
        fake_entry(&roots, "2026-11-03_000000_pre-v0.24.0", "pre-v0.24.0", None, false);
        fake_entry(&roots, "2026-10-01_000000_nightly", "nightly", None, false);
        let removed = rotate(&roots).unwrap();
        assert_eq!(removed, vec!["2026-11-01_000000_pre-v0.22.0".to_string()]);
        let names: Vec<String> = list(&roots).into_iter().map(|e| e.name).collect();
        assert_eq!(names, vec!["2026-11-03_000000_pre-v0.24.0", "2026-11-02_000000_pre-v0.23.0", "2026-10-01_000000_nightly"]);
        assert!(rotate(&roots).unwrap().is_empty());
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    /// An uploaded archive keeps the label it was taken under on another
    /// machine or at another time. It is the operator's: the rotation never
    /// removes it, and it never passes for the snapshot a release takes
    /// before its first run.
    #[test]
    fn an_uploaded_backup_is_never_rotated_and_never_stands_in_for_a_snapshot_taken_here() {
        let roots = temp_roots("uploaded");
        fake_entry_by(&roots, "2026-10-01_000000_pre-v0.21.0", "pre-v0.21.0", UPLOADED, None, false);
        fake_entry_by(&roots, "2026-10-02_000000_pre-v0.24.0", "pre-v0.24.0", UPLOADED, None, false);
        fake_entry(&roots, "2026-11-01_000000_pre-v0.22.0", "pre-v0.22.0", None, false);
        fake_entry(&roots, "2026-11-02_000000_pre-v0.23.0", "pre-v0.23.0", None, false);
        let entries = list(&roots);
        assert_eq!(entries.iter().filter(|e| e.uploaded()).count(), 2);
        assert!(!taken_here(&entries, "pre-v0.24.0"), "the uploaded one is not this release's snapshot");
        assert!(taken_here(&entries, "pre-v0.23.0"));
        assert!(!taken_here(&entries, "pre-v0.20.0"));
        assert!(rotate(&roots).unwrap().is_empty(), "two automatic ones taken here: nothing is over the count");
        fake_entry(&roots, "2026-11-03_000000_pre-v0.24.0", "pre-v0.24.0", None, false);
        assert!(taken_here(&list(&roots), "pre-v0.24.0"));
        assert_eq!(rotate(&roots).unwrap(), vec!["2026-11-01_000000_pre-v0.22.0".to_string()]);
        let names: Vec<String> = list(&roots).into_iter().map(|e| e.name).collect();
        assert!(names.contains(&"2026-10-01_000000_pre-v0.21.0".to_string()) && names.contains(&"2026-10-02_000000_pre-v0.24.0".to_string()), "{names:?}");
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_restore_refuses_another_engine_and_a_mismatched_archive() {
        let roots = temp_roots("engine");
        fake_entry(&roots, "2026-11-05_101200_manual", "manual", Some("fjall"), false);
        let mut out = Vec::new();
        assert!(matches!(
            restore(&roots, "2026-11-05_101200_manual", Some(&release("rocksdb")), &mut out).unwrap(),
            RestoreOutcome::Refused(m) if m.contains("fjall") && m.contains("rocksdb")
        ));
        fs::write(roots.backups.join("2026-11-05_101200_manual").join(ARCHIVE), b"changed").unwrap();
        assert!(matches!(
            restore(&roots, "2026-11-05_101200_manual", Some(&release("fjall")), &mut out).unwrap(),
            RestoreOutcome::Refused(m) if m.contains("does not match its record")
        ));
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn the_timestamp_is_utc_and_sorts() {
        assert_eq!(utc_stamp(0), "1970-01-01_000000");
        assert_eq!(utc_stamp(951_782_400), "2000-02-29_000000");
        assert_eq!(utc_stamp(1_793_900_000), "2026-11-05_173320");
        assert!(utc_stamp(1_793_900_000) < utc_stamp(1_793_900_001));
    }

    #[test]
    fn the_hashing_writer_counts_and_hashes_what_passes() {
        let mut w = HashingWriter::new(Vec::new());
        w.write_all(b"hello ").unwrap();
        w.write_all(b"world").unwrap();
        let (inner, sha, n) = w.finish();
        assert_eq!(inner, b"hello world");
        assert_eq!(n, 11);
        assert_eq!(sha, "b94d27b9934d3e08a52e52d7da7dabfac484efe37a5380ee9088f7ace2efcde9");
    }

    #[test]
    fn the_workspace_exclusions_keep_the_artifacts_and_drop_the_rest_of_the_build_tree() {
        for kept in [".guardian", ".guardian/target", ".guardian/target/wasm32-unknown-unknown", ".guardian/target/wasm32-unknown-unknown/release", ".guardian/target/wasm32-unknown-unknown/release/t.wasm", ".guardian/tools/t/src/lib.rs", "notes.txt", ".ssh/id_ed25519"] {
            assert!(!workspace_excluded(Path::new(kept)), "{kept} is kept");
        }
        for dropped in [".guardian/target/debug", ".guardian/target/debug/x", ".guardian/target/wasm32-unknown-unknown/release/t.d", ".guardian/target/wasm32-unknown-unknown/release/deps/x.wasm", ".guardian/target/wasm32-unknown-unknown/debug", ".guardian/cargo-home", ".guardian/cargo-home/registry"] {
            assert!(workspace_excluded(Path::new(dropped)), "{dropped} is left out");
        }
        assert!(state_excluded(Path::new("halt_reason")));
        assert!(state_excluded(Path::new("next-boot")));
        assert!(state_excluded(Path::new(embra_boot::clock::LAST_KNOWN_NAME)));
        assert_eq!(embra_boot::clock::LAST_KNOWN_NAME, "last-known-time");
        assert!(STATE_KEPT_BY_RESTORE.contains(&"lost+found") && STATE_KEPT_BY_RESTORE.contains(&"last-known-time"));
        assert!(!state_excluded(Path::new("pki/halt_reason")));
        assert!(!state_excluded(Path::new("soul.sha256")));
    }
}
