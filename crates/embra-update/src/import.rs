//! A backup that comes from outside: the archive of a snapshot, taken on
//! this machine or on another, read from a stream (the web console's
//! upload), checked while it is written under the backups directory, and
//! given the record a restore asks for. A download carries the archive alone
//! (`GET /api/backups/{name}`), so the record is made again from what the
//! archive itself says.
//!
//! What is checked, all of it before the entry gets its final name: the
//! stream is one gzip member holding one tar archive, read to its end (the
//! member's checksum with it); every member is a directory, a regular file
//! (a sparse one among them: a snapshot stores a file with holes, such as a
//! database's preallocated journal, as one) or a link; every path is under `state/`, `data/wardsondb/` or
//! `data/workspace/`, named once, in a directory the archive named before
//! it; none is a STATE entry a snapshot leaves out. A snapshot `create`
//! wrote passes, and a restore of what passes writes inside STATE and DATA's
//! two directories and nowhere else: no member is written through a link,
//! over another member or beside the trees a backup holds.
//!
//! The check is against a wrong file, a damaged one and one cut short. What
//! the archive says is restored as it is: whoever uploads a backup is
//! trusted with the machine, as whoever restores one is.

use std::cell::RefCell;
use std::collections::HashSet;
use std::fs::{self, File};
use std::io::{self, BufRead, BufReader, Read, Write};
use std::os::unix::ffi::OsStrExt;
use std::path::{Component, Path, PathBuf};
use std::rc::Rc;

use anyhow::{Context, Result};
use flate2::bufread::GzDecoder;
use sha2::{Digest, Sha256};

use crate::cli::valid_name;
use crate::paths::*;
use crate::snapshot::{Record, Roots, free_bytes, now_secs, sync_dir, utc_stamp};

/// The reads from the source, one buffer at a time.
const READ_BUFFER: usize = 1 << 20;
/// DATA's free space is looked at again each time this much has been written.
const RESERVE_CHECK_EVERY: u64 = 32 * 1024 * 1024;
/// The engine marker is a word; more than this of it is not read.
const ENGINE_MARKER_MAX: u64 = 64;

/// What `import` came to.
pub enum ImportOutcome {
    Imported(Box<Record>),
    /// The name or the archive was refused; nothing of it is kept.
    Refused(String),
}

/// A name an upload may take: a backup's name that is not an unfinished
/// entry's (`.partial` is what a crash leaves and `list` reports).
pub fn importable_name(name: &str) -> bool {
    valid_name(name) && !name.ends_with(PARTIAL_SUFFIX)
}

/// A snapshot's name is its UTC stamp, an underscore and its label
/// (`snapshot::create`): the stamp and the label of a name of that shape.
pub fn stamp_and_label(name: &str) -> Option<(&str, &str)> {
    let b = name.as_bytes();
    let shaped = b.len() > 18
        && b[..18].iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 | 17 => *c == b'_',
            _ => c.is_ascii_digit(),
        });
    shaped.then(|| (&name[..17], &name[18..]))
}

/// What a member of an archive is, as far as a backup goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MemberKind {
    Dir,
    File,
    Link,
    /// Anything else, by the words a refusal names it with.
    Other(&'static str),
}

impl MemberKind {
    fn of(kind: tar::EntryType) -> MemberKind {
        use tar::EntryType as T;
        match kind {
            T::Directory => MemberKind::Dir,
            // A file with holes is stored as a GNU sparse member (the tar
            // library's default when it writes, so every snapshot of a real
            // database holds some); read back, its size is the file's own.
            T::Regular | T::GNUSparse => MemberKind::File,
            T::Symlink => MemberKind::Link,
            T::Link => MemberKind::Other("a hard link"),
            T::Char | T::Block => MemberKind::Other("a device node"),
            T::Fifo => MemberKind::Other("a named pipe"),
            T::XGlobalHeader => MemberKind::Other("a global header"),
            _ => MemberKind::Other("of a kind no backup holds"),
        }
    }
}

/// The members read so far and what they add up to. `files` and `bytes`
/// count as a snapshot's plan does: the files and the links, and the bytes
/// of the files.
#[derive(Default)]
pub struct Members {
    dirs: HashSet<PathBuf>,
    named: HashSet<[u8; 16]>,
    pub files: u64,
    pub bytes: u64,
}

impl Members {
    /// Take one member in, or say why the archive is not a backup. Returns
    /// the path as the checks read it: its normal components, a leading `./`
    /// dropped.
    pub fn admit(&mut self, raw: &Path, kind: MemberKind, size: u64) -> std::result::Result<PathBuf, String> {
        let shown = raw.display();
        let mut path = PathBuf::new();
        for component in raw.components() {
            match component {
                Component::Normal(part) => path.push(part),
                Component::CurDir => {}
                _ => return Err(format!("its member \"{shown}\" is not a path inside the archive")),
            }
        }
        let parts: Vec<&std::ffi::OsStr> = path.iter().collect();
        let Some(top) = parts.first() else {
            // `./`: the root a tar of a folder names. Nothing is under that name.
            return match kind {
                MemberKind::Dir => Ok(path),
                _ => Err(format!("its member \"{shown}\" has no name")),
            };
        };
        if *top == "data" {
            if parts.get(1).is_some_and(|sub| *sub != DATABASE_DIR && *sub != WORKSPACE_DIR) {
                return Err(format!("its member \"{shown}\" is outside data/{DATABASE_DIR}/ and data/{WORKSPACE_DIR}/"));
            }
        } else if *top == "state" {
            if parts.get(1).is_some_and(|sub| STATE_EXCLUDED.iter().any(|x| *sub == *x)) {
                return Err(format!("its member \"{shown}\" is one no backup holds (state/{{{}}})", STATE_EXCLUDED.join(",")));
            }
        } else {
            return Err(format!("its member \"{shown}\" is outside state/ and data/"));
        }
        if let MemberKind::Other(what) = kind {
            return Err(format!("its member \"{shown}\" is {what}; a backup holds directories, files and links"));
        }
        // The two trees and DATA's two directories are directories.
        let a_root = parts.len() == 1 || (*top == "data" && parts.len() == 2);
        if a_root && kind != MemberKind::Dir {
            return Err(format!("its member \"{shown}\" is not a directory"));
        }
        if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty())
            && !self.dirs.contains(parent)
        {
            return Err(format!("its member \"{shown}\" is in \"{}\", which is not a directory the archive named before it", parent.display()));
        }
        if !self.named.insert(digest(&path)) {
            return Err(format!("its member \"{shown}\" is named twice"));
        }
        match kind {
            MemberKind::Dir => {
                self.dirs.insert(path.clone());
            }
            MemberKind::File => {
                self.files += 1;
                self.bytes = self.bytes.saturating_add(size);
            }
            MemberKind::Link => self.files += 1,
            // Refused above.
            MemberKind::Other(_) => {}
        }
        Ok(path)
    }

    /// At the archive's end: both trees were there.
    pub fn finish(&self) -> std::result::Result<(), String> {
        if self.dirs.contains(Path::new("state")) && self.dirs.contains(Path::new("data")) {
            Ok(())
        } else {
            Err("it holds no state/ and data/ directories".to_string())
        }
    }
}

/// A path's place in the set of the names seen: sixteen bytes of its
/// SHA-256, so an archive of a million files costs its names' hashes and not
/// its names.
fn digest(path: &Path) -> [u8; 16] {
    let mut out = [0u8; 16];
    out.copy_from_slice(&Sha256::digest(path.as_os_str().as_bytes())[..16]);
    out
}

/// What ended the writing, when it was not the source: this machine's
/// failure, or DATA's reserve.
enum Fault {
    Write(io::Error),
    Reserve { free: u64 },
}

/// How much room an import leaves and how it learns what is free; a test
/// passes its own.
pub struct Limits<'a> {
    pub check_every: u64,
    pub free: &'a dyn Fn(&Path) -> io::Result<u64>,
}

/// The source as the checks read it: every byte goes into the archive file
/// and into the hash on its way. A write that fails ends the reading, and so
/// does DATA going under its reserve (the services run during an upload, and
/// the database needs the room); either is kept apart from the source's own
/// errors, which are the archive's.
struct Tee<'a, R: Read> {
    source: R,
    file: File,
    hasher: Sha256,
    bytes: u64,
    since_check: u64,
    data: PathBuf,
    limits: &'a Limits<'a>,
    fault: Rc<RefCell<Option<Fault>>>,
}

impl<R: Read> Read for Tee<'_, R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        let n = self.source.read(buf)?;
        if n == 0 {
            return Ok(0);
        }
        if let Err(e) = self.file.write_all(&buf[..n]) {
            *self.fault.borrow_mut() = Some(Fault::Write(e));
            return Err(io::Error::other("the archive could not be written"));
        }
        self.hasher.update(&buf[..n]);
        self.bytes += n as u64;
        self.since_check += n as u64;
        if self.since_check >= self.limits.check_every {
            self.since_check = 0;
            if let Ok(free) = (self.limits.free)(&self.data)
                && free < RESERVE_BYTES
            {
                *self.fault.borrow_mut() = Some(Fault::Reserve { free });
                return Err(io::Error::other("DATA is under its reserve"));
            }
        }
        Ok(n)
    }
}

/// Why the reading of the members stopped.
enum Stop {
    /// A member no backup holds, or the trees missing: the refusal's words.
    Member(String),
    /// The stream itself: not gzip, not tar, cut short, a failed checksum.
    Stream(io::Error),
}

fn read_members<R: Read>(archive: &mut tar::Archive<R>, members: &mut Members, engine: &mut Option<String>) -> std::result::Result<(), Stop> {
    let marker = Path::new("data").join(DATABASE_DIR).join(ENGINE_MARKER);
    for entry in archive.entries().map_err(Stop::Stream)? {
        let mut entry = entry.map_err(Stop::Stream)?;
        let kind = MemberKind::of(entry.header().entry_type());
        let raw = entry.path().map_err(Stop::Stream)?.into_owned();
        let path = members.admit(&raw, kind, entry.size()).map_err(Stop::Member)?;
        if kind == MemberKind::File && path == marker {
            let mut word = Vec::new();
            (&mut entry).take(ENGINE_MARKER_MAX).read_to_end(&mut word).map_err(Stop::Stream)?;
            *engine = Some(String::from_utf8_lossy(&word).trim().to_string()).filter(|s| !s.is_empty());
        }
    }
    Ok(())
}

/// What follows the last member: the tar's padding to the member's end (its
/// checksum is verified there), and nothing after the member.
fn read_to_the_end<R: BufRead>(decoder: &mut GzDecoder<R>) -> std::result::Result<(), Stop> {
    let mut buf = vec![0u8; 64 * 1024];
    loop {
        let n = decoder.read(&mut buf).map_err(Stop::Stream)?;
        if n == 0 {
            break;
        }
        if buf[..n].iter().any(|b| *b != 0) {
            return Err(Stop::Member("data follows the end of its tar archive".to_string()));
        }
    }
    if !decoder.get_mut().fill_buf().map_err(Stop::Stream)?.is_empty() {
        return Err(Stop::Member("the file goes on after the compressed archive ends".to_string()));
    }
    Ok(())
}

/// Take an archive in under `name`: written into `<name>.partial` as it is
/// read and checked, hashed and fsynced, its record written beside it, then
/// the rename to its final name. A refusal or a failure removes what was
/// written; a crash leaves a `.partial` that `list` reports, `delete`
/// removes and the next import of the name replaces.
pub fn import(roots: &Roots, name: &str, source: impl Read, out: &mut impl Write) -> Result<ImportOutcome> {
    import_with(roots, name, source, out, &Limits { check_every: RESERVE_CHECK_EVERY, free: &free_bytes })
}

pub fn import_with(roots: &Roots, name: &str, source: impl Read, out: &mut impl Write, limits: &Limits<'_>) -> Result<ImportOutcome> {
    if !importable_name(name) {
        return Ok(ImportOutcome::Refused(format!(
            "import: not a name for a backup: {name:?} (letters, digits, _ . -, a letter or digit first, 80 at most, and not ending in {PARTIAL_SUFFIX}); nothing was kept"
        )));
    }
    let final_dir = roots.backups.join(name);
    if final_dir.exists() {
        return Ok(ImportOutcome::Refused(format!(
            "import: a backup named \"{name}\" exists already; delete it first (embra-update delete {name}, or /backup delete {name}) or give the archive another name; nothing was kept"
        )));
    }
    let partial = roots.backups.join(format!("{name}{PARTIAL_SUFFIX}"));
    if partial.exists() {
        fs::remove_dir_all(&partial).with_context(|| format!("remove {}", partial.display()))?;
    }
    fs::create_dir_all(&partial).with_context(|| format!("create {}", partial.display()))?;
    let outcome = receive(roots, name, &partial, source, out, limits);
    match outcome {
        Ok(ImportOutcome::Imported(record)) => {
            fs::rename(&partial, &final_dir).with_context(|| format!("rename to {}", final_dir.display()))?;
            sync_dir(&roots.backups)?;
            writeln!(out, "import: \"{name}\" is listed and can be restored")?;
            Ok(ImportOutcome::Imported(record))
        }
        other => {
            let _ = fs::remove_dir_all(&partial);
            other
        }
    }
}

fn receive(roots: &Roots, name: &str, partial: &Path, source: impl Read, out: &mut impl Write, limits: &Limits<'_>) -> Result<ImportOutcome> {
    let archive_path = partial.join(ARCHIVE);
    let file = File::create(&archive_path).with_context(|| format!("create {}", archive_path.display()))?;
    let fault = Rc::new(RefCell::new(None));
    let tee = Tee { source, file, hasher: Sha256::new(), bytes: 0, since_check: 0, data: roots.data(), limits, fault: Rc::clone(&fault) };
    let mut archive = tar::Archive::new(GzDecoder::new(BufReader::with_capacity(READ_BUFFER, tee)));
    let mut members = Members::default();
    let mut data_engine = None;
    let read = read_members(&mut archive, &mut members, &mut data_engine).and_then(|()| {
        let mut decoder = archive.into_inner();
        read_to_the_end(&mut decoder)?;
        members.finish().map_err(Stop::Member)?;
        Ok(decoder)
    });
    let decoder = match read {
        Ok(decoder) => decoder,
        Err(stop) => {
            return match (fault.borrow_mut().take(), stop) {
                (Some(Fault::Write(e)), _) => Err(anyhow::Error::new(e).context(format!("write {}", archive_path.display()))),
                (Some(Fault::Reserve { free }), _) => Ok(ImportOutcome::Refused(format!(
                    "import: DATA has {} MB free, under the {} MB an upload leaves for the database and the workspace; delete a backup (/backup delete <name>) and upload again; nothing was kept",
                    free / (1024 * 1024),
                    RESERVE_BYTES / (1024 * 1024)
                ))),
                (None, Stop::Member(why)) => Ok(ImportOutcome::Refused(format!("import: this is not a backup of embraOS: {why}; nothing was kept"))),
                (None, Stop::Stream(e)) => Ok(ImportOutcome::Refused(format!(
                    "import: the file could not be read as a backup's archive: it is damaged, cut short or not a gzip-compressed tar ({e}); nothing was kept"
                ))),
            };
        }
    };
    let Tee { file, hasher, bytes: bytes_archive, .. } = decoder.into_inner().into_inner();
    file.sync_all().with_context(|| format!("sync {}", archive_path.display()))?;
    drop(file);
    let sha256 = hex::encode(hasher.finalize());
    writeln!(
        out,
        "import: {} files, {} MB unpacked; the archive is {} MB, sha256 {}",
        members.files,
        members.bytes / (1024 * 1024),
        bytes_archive / (1024 * 1024),
        &sha256[..16]
    )?;

    let (timestamp, label) = match stamp_and_label(name) {
        Some((stamp, label)) => (stamp.to_string(), label.to_string()),
        None => (utc_stamp(now_secs()), name.to_string()),
    };
    let record = Record {
        format: FORMAT.to_string(),
        name: name.to_string(),
        timestamp,
        label,
        taken_by: UPLOADED.to_string(),
        release: None,
        data_engine,
        files: members.files,
        bytes_uncompressed: members.bytes,
        bytes_archive,
        sha256,
        excluded: Vec::new(),
        skipped: Vec::new(),
        created_by: "embra-update import".to_string(),
    };
    let record_path = partial.join(RECORD);
    let mut rf = File::create(&record_path).with_context(|| format!("create {}", record_path.display()))?;
    rf.write_all(serde_json::to_string_pretty(&record)?.as_bytes())?;
    rf.write_all(b"\n")?;
    rf.sync_all()?;
    drop(rf);
    sync_dir(partial)?;
    Ok(ImportOutcome::Imported(Box::new(record)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::snapshot::test_support::{populate, release, temp_roots};
    use crate::snapshot::{CreateOutcome, RestoreOutcome, create, list, restore, sha256_of, tree_files};
    use flate2::Compression;
    use flate2::write::GzEncoder;

    fn taken(outcome: ImportOutcome) -> Record {
        match outcome {
            ImportOutcome::Imported(r) => *r,
            ImportOutcome::Refused(m) => panic!("refused: {m}"),
        }
    }

    fn refusal(outcome: ImportOutcome) -> String {
        match outcome {
            ImportOutcome::Refused(m) => m,
            ImportOutcome::Imported(r) => panic!("taken in as {}", r.name),
        }
    }

    /// One member of a hand-made archive.
    enum M<'a> {
        Dir(&'a str),
        File(&'a str, &'a [u8]),
        Link(&'a str, &'a str),
        Hard(&'a str, &'a str),
        /// A file of this size, all holes but these bytes at its end.
        Sparse(&'a str, u64, &'a [u8]),
    }

    /// A number as a tar header holds it: eleven octal digits and a NUL.
    fn octal(n: u64) -> [u8; 12] {
        let mut field = [0u8; 12];
        field[..11].copy_from_slice(format!("{n:011o}").as_bytes());
        field
    }

    fn tar_of(members: &[M]) -> Vec<u8> {
        let mut b = tar::Builder::new(Vec::new());
        for m in members {
            let mut h = tar::Header::new_gnu();
            h.set_mode(0o644);
            match m {
                M::Dir(path) => {
                    h.set_entry_type(tar::EntryType::Directory);
                    h.set_mode(0o755);
                    h.set_size(0);
                    b.append_data(&mut h, path, io::empty()).unwrap();
                }
                M::File(path, body) => {
                    h.set_entry_type(tar::EntryType::Regular);
                    h.set_size(body.len() as u64);
                    b.append_data(&mut h, path, *body).unwrap();
                }
                M::Link(path, to) => {
                    h.set_entry_type(tar::EntryType::Symlink);
                    h.set_size(0);
                    b.append_link(&mut h, path, to).unwrap();
                }
                M::Hard(path, to) => {
                    h.set_entry_type(tar::EntryType::Link);
                    h.set_size(0);
                    b.append_link(&mut h, path, to).unwrap();
                }
                M::Sparse(path, real_size, tail) => {
                    h.set_entry_type(tar::EntryType::GNUSparse);
                    h.set_size(tail.len() as u64);
                    h.set_path(path).unwrap();
                    let gnu = h.as_gnu_mut().unwrap();
                    gnu.sparse[0].offset = octal(real_size - tail.len() as u64);
                    gnu.sparse[0].numbytes = octal(tail.len() as u64);
                    gnu.realsize = octal(*real_size);
                    h.set_cksum();
                    b.append(&h, *tail).unwrap();
                }
            }
        }
        b.into_inner().unwrap()
    }

    fn gz(tar: &[u8]) -> Vec<u8> {
        let mut e = GzEncoder::new(Vec::new(), Compression::fast());
        e.write_all(tar).unwrap();
        e.finish().unwrap()
    }

    /// The least a backup holds, and more.
    fn with_trees<'a>(more: Vec<M<'a>>) -> Vec<M<'a>> {
        let mut all: Vec<M<'a>> = vec![M::Dir("state"), M::Dir("data"), M::Dir("data/wardsondb")];
        all.extend(more);
        all
    }

    #[test]
    fn an_archive_a_snapshot_wrote_is_taken_in_and_restores_the_same_tree() {
        let home = temp_roots("import-home");
        populate(&home);
        // A database's journal: preallocated, mostly a hole. Where the
        // filesystem reports holes the snapshot stores it as a sparse member.
        let journal = home.data().join("wardsondb/journal0");
        let f = File::create(&journal).unwrap();
        f.set_len(4 * 1024 * 1024).unwrap();
        std::os::unix::fs::FileExt::write_all_at(&f, b"JOURNAL", 3 * 1024 * 1024).unwrap();
        f.sync_all().unwrap();
        drop(f);
        let mut out = Vec::new();
        let made = match create(&home, "manual", "manual", Some(&release("fjall")), &mut out).unwrap() {
            CreateOutcome::Created(r) => *r,
            CreateOutcome::Refused(m) => panic!("refused: {m}"),
        };
        let bytes = fs::read(home.backups.join(&made.name).join(ARCHIVE)).unwrap();
        let original = tree_files(&home.parent);

        // Another machine, freshly installed: empty STATE and DATA, no backups directory yet.
        let fresh = temp_roots("import-fresh");
        fs::remove_dir_all(&fresh.backups).unwrap();
        fs::write(fresh.state().join("last-known-time"), "time=1794700000\n").unwrap();
        let mut out = Vec::new();
        let record = taken(import(&fresh, &made.name, &bytes[..], &mut out).unwrap());
        assert_eq!(record.sha256, made.sha256, "the archive is kept byte for byte");
        assert_eq!(sha256_of(&fresh.backups.join(&made.name).join(ARCHIVE)).unwrap(), made.sha256);
        assert_eq!((record.files, record.bytes_uncompressed, record.bytes_archive), (made.files, made.bytes_uncompressed, made.bytes_archive));
        assert_eq!(record.data_engine.as_deref(), Some("fjall"), "the engine is read from the archive's own marker");
        assert_eq!((record.timestamp.as_str(), record.label.as_str()), (made.timestamp.as_str(), "manual"), "from the name");
        assert_eq!(record.taken_by, UPLOADED);
        assert_eq!(record.release, None, "the archive does not say which release took it");
        assert_eq!(record.format, FORMAT);
        let listed = list(&fresh);
        assert_eq!(listed.len(), 1);
        assert!(listed[0].complete && listed[0].uploaded());
        assert!(listed[0].line().ends_with(UPLOADED), "{}", listed[0].line());
        assert!(!fresh.backups.join(format!("{}{PARTIAL_SUFFIX}", made.name)).exists());
        assert!(String::from_utf8_lossy(&out).contains("is listed and can be restored"));

        // The restore puts the first machine's tree there, and keeps this machine's clock.
        let mut out = Vec::new();
        assert!(matches!(restore(&fresh, &made.name, Some(&release("fjall")), &mut out).unwrap(), RestoreOutcome::Restored));
        let restored = tree_files(&fresh.parent);
        for (path, content) in &original {
            let left_out = ["state/halt_reason", "state/next-boot", "state/last-known-time", "state/lost+found", "data/backups", "data/workspace/.guardian/cargo-home", "data/workspace/.guardian/target/debug"]
                .iter()
                .any(|x| path == x || path.starts_with(&format!("{x}/")))
                || path.ends_with("/tool.d");
            if left_out {
                continue;
            }
            assert_eq!(restored.get(path), Some(content), "{path}");
        }
        assert_eq!(restored.get("state/last-known-time").map(String::as_str), Some("time=1794700000\n"));
        assert_eq!(restored.get("data/workspace/link").map(String::as_str), Some("-> notes.txt"));
        assert_eq!(fs::read(fresh.data().join("wardsondb/journal0")).unwrap(), fs::read(&journal).unwrap(), "the journal, holes and all");
        // The other engine's release refuses it, by the engine the import read.
        let mut out = Vec::new();
        assert!(matches!(restore(&fresh, &made.name, Some(&release("rocksdb")), &mut out).unwrap(), RestoreOutcome::Refused(m) if m.contains("fjall") && m.contains("rocksdb")));
        fs::remove_dir_all(&home.parent).unwrap();
        fs::remove_dir_all(&fresh.parent).unwrap();
    }

    /// A file with holes is a sparse member in an archive, whatever the
    /// filesystem the snapshot was taken on reports: it is a file, counted
    /// at its own size, and restored with its holes as zeros. (The first
    /// version of this check refused such members, and with them every
    /// snapshot of a real database: its journals are preallocated.)
    #[test]
    fn a_sparse_member_is_a_file_of_its_real_size() {
        let roots = temp_roots("import-sparse");
        let archive = gz(&tar_of(&with_trees(vec![
            M::Dir("data/wardsondb/journals"),
            M::Sparse("data/wardsondb/journals/0", 1 << 20, b"JOURNAL!"),
            M::File("state/soul.sha256", b"abc"),
        ])));
        let mut seen = tar::Archive::new(flate2::read::GzDecoder::new(&archive[..]));
        assert!(seen.entries().unwrap().any(|e| e.unwrap().header().entry_type().is_gnu_sparse()), "the archive holds a sparse member");
        let mut out = Vec::new();
        let record = taken(import(&roots, "2026-11-12_214341_manual", &archive[..], &mut out).unwrap());
        assert_eq!((record.files, record.bytes_uncompressed), (2, (1 << 20) + 3), "the file's own size, not the bytes stored");
        assert!(matches!(restore(&roots, "2026-11-12_214341_manual", None, &mut out).unwrap(), RestoreOutcome::Restored));
        let restored = fs::read(roots.data().join("wardsondb/journals/0")).unwrap();
        assert_eq!(restored.len(), 1 << 20);
        assert!(restored.ends_with(b"JOURNAL!") && restored[..(1 << 20) - 8].iter().all(|b| *b == 0));
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn what_is_not_a_backup_is_refused_and_nothing_of_it_is_kept() {
        let roots = temp_roots("import-refused");
        let good = gz(&tar_of(&with_trees(vec![M::File("state/soul.sha256", b"abc")])));
        let mut cut = good.clone();
        cut.truncate(good.len() - 6);
        let mut longer = good.clone();
        longer.extend_from_slice(b"more");
        let mut padded = tar_of(&with_trees(vec![]));
        padded.extend_from_slice(b"not padding");
        let cases: Vec<(&str, Vec<u8>, &str)> = vec![
            ("text", b"these are my notes, not an archive".to_vec(), "damaged, cut short or not a gzip-compressed tar"),
            ("cut short", cut, "damaged, cut short or not a gzip-compressed tar"),
            ("gzip of no tar", gz(b"some text that is no tar archive at all, long enough to be read as a header block ........................................................................................................................................................................................................................................................................................................................................................................................................................................................"), "damaged, cut short or not a gzip-compressed tar"),
            ("more after the member", longer, "goes on after the compressed archive ends"),
            ("data after the tar", gz(&padded), "data follows the end of its tar archive"),
            ("elsewhere", gz(&tar_of(&with_trees(vec![M::Dir("etc"), M::File("etc/passwd", b"x")]))), "outside state/ and data/"),
            ("the backups", gz(&tar_of(&with_trees(vec![M::Dir("data/backups")]))), "outside data/wardsondb/ and data/workspace/"),
            ("the marker", gz(&tar_of(&with_trees(vec![M::File("state/next-boot", b"mode=restore\nname=x\n")]))), "one no backup holds"),
            ("the saved clock", gz(&tar_of(&with_trees(vec![M::File("state/last-known-time", b"time=1\n")]))), "one no backup holds"),
            ("a hard link", gz(&tar_of(&with_trees(vec![M::File("state/a", b"x"), M::Hard("state/b", "state/a")]))), "is a hard link"),
            ("no directory first", gz(&tar_of(&with_trees(vec![M::File("state/pki/ca.key", b"KEY")]))), "not a directory the archive named before it"),
            ("under a link", gz(&tar_of(&with_trees(vec![M::Link("state/out", "/etc"), M::File("state/out/passwd", b"x")]))), "not a directory the archive named before it"),
            ("under a file", gz(&tar_of(&with_trees(vec![M::File("state/f", b"x"), M::File("state/f/g", b"y")]))), "not a directory the archive named before it"),
            ("twice", gz(&tar_of(&with_trees(vec![M::File("state/f", b"x"), M::File("state/f", b"y")]))), "is named twice"),
            ("a tree that is a file", gz(&tar_of(&[M::Dir("state"), M::Dir("data"), M::File("data/workspace", b"x")])), "is not a directory"),
            ("one tree", gz(&tar_of(&[M::Dir("state"), M::File("state/soul.sha256", b"abc")])), "holds no state/ and data/ directories"),
            ("empty", gz(&tar_of(&[])), "holds no state/ and data/ directories"),
        ];
        for (what, bytes, expected) in cases {
            let mut out = Vec::new();
            let said = refusal(import(&roots, "2026-11-12_101200_manual", &bytes[..], &mut out).unwrap());
            assert!(said.contains(expected), "{what}: {said}");
            assert!(said.starts_with("import: ") && said.ends_with("nothing was kept"), "{what}: {said}");
            assert!(list(&roots).is_empty(), "{what}: an entry was left");
            assert_eq!(fs::read_dir(&roots.backups).unwrap().count(), 0, "{what}: something was left under the backups directory");
        }
        // The same members in a sound archive are taken in.
        let mut out = Vec::new();
        let record = taken(import(&roots, "2026-11-12_101200_manual", &good[..], &mut out).unwrap());
        assert_eq!((record.files, record.bytes_uncompressed, record.data_engine), (1, 3, None));
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn a_member_is_taken_by_its_path_its_kind_and_its_place() {
        let mut m = Members::default();
        for dir in ["./", "state", "./data", "data/wardsondb/", "data/workspace", "state/pki", "data/workspace/repo"] {
            m.admit(Path::new(dir), MemberKind::Dir, 0).unwrap_or_else(|e| panic!("{dir}: {e}"));
        }
        assert_eq!(m.admit(Path::new("./state/pki/ca.key"), MemberKind::File, 3), Ok(PathBuf::from("state/pki/ca.key")));
        assert_eq!(m.admit(Path::new("data/workspace/repo/head"), MemberKind::Link, 0), Ok(PathBuf::from("data/workspace/repo/head")));
        assert_eq!(m.admit(Path::new("data/workspace/repo/notes.txt"), MemberKind::File, 5), Ok(PathBuf::from("data/workspace/repo/notes.txt")));
        assert_eq!((m.files, m.bytes), (3, 8), "files and links are counted, and the files' bytes");
        for (path, kind, expected) in [
            ("../state/x", MemberKind::File, "not a path inside the archive"),
            ("/state/x", MemberKind::File, "not a path inside the archive"),
            ("state/../../etc/x", MemberKind::File, "not a path inside the archive"),
            ("./", MemberKind::File, "has no name"),
            ("boot/x", MemberKind::File, "outside state/ and data/"),
            ("data/ephemeral", MemberKind::Dir, "outside data/wardsondb/ and data/workspace/"),
            ("state/lost+found", MemberKind::Dir, "one no backup holds"),
            ("state/halt_reason", MemberKind::File, "one no backup holds"),
            ("state/fifo", MemberKind::Other("a named pipe"), "is a named pipe"),
            ("state/pki/ca.key", MemberKind::File, "named twice"),
            ("state/pki/", MemberKind::Dir, "named twice"),
            ("data/workspace/repo/head/x", MemberKind::File, "not a directory the archive named before it"),
            ("state/new/x", MemberKind::File, "not a directory the archive named before it"),
        ] {
            let said = m.admit(Path::new(path), kind, 1).expect_err(path);
            assert!(said.contains(expected), "{path}: {said}");
        }
        assert_eq!((m.files, m.bytes), (3, 8), "a refused member counts for nothing");
        assert_eq!(m.finish(), Ok(()));
        assert!(Members::default().finish().is_err());
        // A STATE entry that only starts like one a snapshot leaves out is a file like any other.
        assert!(m.admit(Path::new("state/next-boot.bak"), MemberKind::File, 1).is_ok());
    }

    #[test]
    fn an_upload_never_replaces_a_backup_and_a_crashed_one_is_replaced() {
        let roots = temp_roots("import-names");
        let good = gz(&tar_of(&with_trees(vec![M::File("state/soul.sha256", b"abc")])));
        let mut out = Vec::new();
        for bad in ["../etc", "", ".hidden", "a/b", "2026-11-12_101200_manual.partial", &"x".repeat(81)] {
            let said = refusal(import(&roots, bad, &good[..], &mut out).unwrap());
            assert!(said.contains("not a name for a backup"), "{bad}: {said}");
        }
        assert_eq!(fs::read_dir(&roots.backups).unwrap().count(), 0);
        // What a crash left under the name's partial is replaced.
        let stale = roots.backups.join(format!("mine{PARTIAL_SUFFIX}"));
        fs::create_dir_all(&stale).unwrap();
        fs::write(stale.join("junk"), b"half an upload").unwrap();
        let record = taken(import(&roots, "mine", &good[..], &mut out).unwrap());
        assert_eq!(record.label, "mine", "a name without a stamp is its own label");
        assert_eq!(record.timestamp.len(), 17, "and the stamp is the import's: {}", record.timestamp);
        assert!(!stale.exists() && !roots.backups.join("mine").join("junk").exists());
        // The same name again is refused, and the first stays as it was.
        let other = gz(&tar_of(&with_trees(vec![M::File("state/soul.sha256", b"other")])));
        let said = refusal(import(&roots, "mine", &other[..], &mut out).unwrap());
        assert!(said.contains("exists already") && said.contains("/backup delete mine"), "{said}");
        assert_eq!(sha256_of(&roots.backups.join("mine").join(ARCHIVE)).unwrap(), record.sha256);
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn an_upload_stops_when_data_goes_under_its_reserve() {
        let roots = temp_roots("import-reserve");
        // Incompressible enough to be read in more than one buffer of the check's size.
        let body: Vec<u8> = (0..400_000u32).flat_map(|i| i.wrapping_mul(2_654_435_761).to_le_bytes()).collect();
        let big = gz(&tar_of(&with_trees(vec![M::File("state/blob", &body)])));
        let mut out = Vec::new();
        let under = |_: &Path| -> io::Result<u64> { Ok(RESERVE_BYTES - 1) };
        let said = refusal(import_with(&roots, "big", &big[..], &mut out, &Limits { check_every: 4096, free: &under }).unwrap());
        assert!(said.contains("DATA has 255 MB free, under the 256 MB"), "{said}");
        assert_eq!(fs::read_dir(&roots.backups).unwrap().count(), 0);
        let enough = |_: &Path| -> io::Result<u64> { Ok(RESERVE_BYTES) };
        taken(import_with(&roots, "big", &big[..], &mut out, &Limits { check_every: 4096, free: &enough }).unwrap());
        fs::remove_dir_all(&roots.parent).unwrap();
    }

    #[test]
    fn the_stamp_and_the_label_come_from_a_name_of_a_snapshots_shape() {
        assert_eq!(stamp_and_label("2026-11-05_101200_manual"), Some(("2026-11-05_101200", "manual")));
        assert_eq!(stamp_and_label("2026-11-05_101200_pre-v0.24.0-phase2"), Some(("2026-11-05_101200", "pre-v0.24.0-phase2")));
        for other in ["mine", "2026-11-05_101200_", "2026-11-05_101200", "2026-11-05-101200_manual", "2026-11-05_10120x_manual", ""] {
            assert_eq!(stamp_and_label(other), None, "{other}");
        }
        assert!(importable_name("2026-11-05_101200_manual") && importable_name("mine"));
        assert!(!importable_name("mine.partial") && !importable_name("a/b"));
    }
}
