//! `file_copy` — host-side file and directory-tree copying.
//!
//! Spec of record: the approved plan (2026-08-21, §"Spec of record"),
//! mirrored locally as `embraOS-Phase1-Implementation/Sprint 6/file_copy_spec.md`
//! for the consumer shakedown. Hard requirements:
//!
//! 1. Contents never enter the conversation — the copy is host-side, so a
//!    binary file (a generated image in `MEDIA/`) or a large tree costs the
//!    call, not the bytes. This is the capability `file_read` → `file_write`
//!    cannot provide (binary gate, 2 MiB ceiling, truncate-on-first-chunk).
//! 2. Source unrestricted (file_read's read policy, widens nothing);
//!    destination inside the `/embra/workspace` writer jail, re-verified
//!    after `canonicalize` with component-wise `starts_with` (file_patch's
//!    rule — `/embra/workspace-evil` does not pass).
//! 3. Exact final path — never copy INTO a directory; never merge trees.
//! 4. Atomic per file (file_patch's temp + fsync + rename discipline via the
//!    shared [`commit_temp`] tail); all-or-nothing per tree ([`RollbackGuard`]:
//!    rename the half-built root aside FIRST, then remove — fires on any
//!    error and on the dispatcher's timeout dropping the future).
//! 5. Budgets are hard: the byte budget is enforced on bytes actually READ,
//!    not on stat sizes — a procfs file that claims 0 bytes cannot fill the
//!    DATA partition WardSONDB lives on.
//!
//! Symlinks inside a tree are recreated, never followed; a link whose target
//! would resolve outside the jail once it lands in the destination is skipped
//! and reported (the lexical writer jail follows links, so such a link would
//! be a later `file_write` escape — the same reason `file_symlink` jails its
//! targets). Errors are prose in the success channel, matching the file_*
//! family convention; every refusal ends `Nothing copied.` (or `Destination
//! unchanged.` when a pre-existing destination was involved).

use std::path::{Component, Path, PathBuf};

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use super::engineering::{resolve_workspace_path, WORKSPACE_ROOT};
use super::file_patch::{commit_temp, temp_path_for};
use crate::tools::registry::DispatchContext;

// ---------------------------------------------------------------------------
// Budgets (pinned by `caps_pinned`)
// ---------------------------------------------------------------------------

/// Bytes one call may write. DATA is a 2 GiB partition shared with
/// WardSONDB; half a GiB per call keeps a runaway copy from taking the
/// database down before the free-space reserve can.
pub(crate) const FILE_COPY_MAX_BYTES: u64 = 512 * 1024 * 1024;

/// Entries (files + dirs + links + skipped) one tree walk may visit. Bounds
/// the pre-pass on any source, including virtual filesystems.
pub(crate) const FILE_COPY_MAX_ENTRIES: usize = 100_000;

/// Bytes that must remain free on the destination filesystem after the copy.
pub(crate) const FILE_COPY_FREE_RESERVE: u64 = 256 * 1024 * 1024;

/// Temp-file tag (see `file_patch::temp_path_for`): a leaked
/// `.name.embra-copy.pid.seq.tmp` is attributable to this tool.
const TMP_TAG: &str = "embra-copy";

/// Report list cap (file_patch §8.3 convention).
const ENUM_CAP: usize = 10;

/// Streaming copy buffer.
const COPY_BUF: usize = 1024 * 1024;

// ---------------------------------------------------------------------------
// Pure core — no I/O
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq)]
enum SkipReason {
    /// fifo / socket / char / block device — never opened.
    Special(&'static str),
    /// Symlink whose target resolves outside the jail at its NEW location.
    SymlinkOutside(PathBuf),
    /// `symlink_metadata` / `read_link` failed.
    Unreadable(String),
}

impl SkipReason {
    fn describe(&self) -> String {
        match self {
            SkipReason::Special(kind) => format!("special file: {kind}"),
            SkipReason::SymlinkOutside(resolved) => format!(
                "symlink target outside {}: {}",
                WORKSPACE_ROOT,
                resolved.display()
            ),
            SkipReason::Unreadable(e) => format!("unreadable: {e}"),
        }
    }
}

/// What a tree copy will do, computed by the pre-pass. All paths are relative
/// to the source/destination roots; `dirs` is parent-before-child with `[0]`
/// the root itself (empty relative path).
#[derive(Debug, Default)]
struct CopyPlan {
    files: Vec<(PathBuf, u64)>,
    dirs: Vec<(PathBuf, u32)>,
    symlinks: Vec<(PathBuf, PathBuf)>,
    skipped: Vec<(PathBuf, SkipReason)>,
    excluded: Vec<(PathBuf, bool)>,
    total_bytes: u64,
    entries: usize,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Caps {
    pub(crate) max_bytes: u64,
    pub(crate) max_entries: usize,
    pub(crate) free_reserve: u64,
}

impl Default for Caps {
    fn default() -> Self {
        Caps {
            max_bytes: FILE_COPY_MAX_BYTES,
            max_entries: FILE_COPY_MAX_ENTRIES,
            free_reserve: FILE_COPY_FREE_RESERVE,
        }
    }
}

/// Everything `copy_at` needs besides the two paths. `jail_root` is a
/// parameter (not the `WORKSPACE_ROOT` const) so positive-path tests can run
/// in a temp dir — with a hard-coded jail every `/tmp` symlink would count
/// as "outside" and the recreate/skip tests would contradict each other.
#[derive(Debug, Clone)]
pub(crate) struct CopyOptions {
    pub(crate) recursive: bool,
    pub(crate) overwrite: bool,
    pub(crate) dry_run: bool,
    pub(crate) exclude: Vec<String>,
    pub(crate) jail_root: PathBuf,
    pub(crate) caps: Caps,
    /// Test hook: fail after this many files were copied into a tree.
    pub(crate) fail_after_files: Option<usize>,
    /// Test hook: bytes reported as available instead of `statvfs`.
    pub(crate) available_override: Option<u64>,
}

impl CopyOptions {
    fn production(args: &FileCopyArgs) -> Self {
        CopyOptions {
            recursive: args.recursive,
            overwrite: args.overwrite,
            dry_run: args.dry_run,
            exclude: args.exclude.clone().unwrap_or_default(),
            jail_root: PathBuf::from(WORKSPACE_ROOT),
            caps: Caps::default(),
            fail_after_files: None,
            available_override: None,
        }
    }
}

/// Resolve `.` and `..` textually WITHOUT touching the filesystem — a copied
/// symlink may legitimately dangle. `..` at the root clamps at the root
/// (kernel behaviour).
fn normalize_lexical(p: &Path) -> PathBuf {
    let mut out: Vec<Component> = Vec::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            Component::ParentDir => {
                if out.len() > 1 {
                    out.pop();
                }
            }
            other => out.push(other),
        }
    }
    out.iter().collect()
}

/// A symlink about to be recreated at `link_new` (a path in the DESTINATION
/// tree) pointing at `raw`. Relative targets resolve against the link's NEW
/// parent — a link harmless in the source can point outside the jail once it
/// lands somewhere else. `Ok(())` = inside; `Err(resolved)` = outside.
///
/// Lexical is sound here: every directory inside the new tree is created by
/// the tool (the root must not pre-exist) and the ancestors above it were
/// canonicalized, so no intermediate component of `link_new` can be a
/// pre-existing symlink. Component-wise `starts_with`, so `workspace-evil`
/// does not pass. The caller additionally canonicalizes the resolved target
/// when it exists (a pre-existing escaping link elsewhere in the workspace).
fn symlink_target_ok(link_new: &Path, raw: &Path, jail: &Path) -> Result<(), PathBuf> {
    let joined = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        link_new
            .parent()
            .unwrap_or_else(|| Path::new("/"))
            .join(raw)
    };
    let resolved = normalize_lexical(&joined);
    if resolved.starts_with(jail) {
        Ok(())
    } else {
        Err(resolved)
    }
}

/// `cp -r a a/b` guard — component-wise, so `a-other` is not "inside" `a`.
fn dest_inside_source(src: &Path, dst: &Path) -> bool {
    dst.starts_with(src)
}

fn plural(n: usize, one: &str, many: &str) -> String {
    if n == 1 {
        format!("1 {one}")
    } else {
        format!("{n} {many}")
    }
}

fn special_kind(ft: &std::fs::FileType) -> &'static str {
    use std::os::unix::fs::FileTypeExt;
    if ft.is_fifo() {
        "fifo"
    } else if ft.is_socket() {
        "socket"
    } else if ft.is_char_device() {
        "char device"
    } else if ft.is_block_device() {
        "block device"
    } else {
        "unknown"
    }
}

fn mode_of(meta: &std::fs::Metadata) -> u32 {
    use std::os::unix::fs::PermissionsExt;
    meta.permissions().mode() & 0o777
}

/// Render a capped list: `a, b, c … and N more`.
fn capped_list(items: &[String]) -> String {
    if items.len() <= ENUM_CAP {
        items.join(", ")
    } else {
        format!(
            "{} … and {} more",
            items[..ENUM_CAP].join(", "),
            items.len() - ENUM_CAP
        )
    }
}

/// Single-file report (spec §7). `dst_sha` is `None` on a dry run.
#[allow(clippy::too_many_arguments)]
fn render_file_report(
    src: &Path,
    dst: &Path,
    bytes: u64,
    mode: u32,
    src_sha: &str,
    dst_sha: Option<&str>,
    replaced: Option<u64>,
    dry_run: bool,
) -> String {
    let short = &src_sha[..16.min(src_sha.len())];
    let mut out = String::new();
    if dry_run {
        out.push_str("DRY RUN — no write\n");
        out.push_str(&format!(
            "Would copy {} → {}: {} bytes, mode {:04o}, sha256 {}",
            src.display(),
            dst.display(),
            bytes,
            mode,
            short
        ));
        if let Some(old) = replaced {
            out.push_str(&format!(" — would replace existing {old} bytes (mode kept)"));
        }
        return out;
    }
    let kept = if replaced.is_some() { " (kept)" } else { "" };
    out.push_str(&format!(
        "Copied {} → {}: {} bytes, mode {:04o}{}, sha256 {}",
        src.display(),
        dst.display(),
        bytes,
        mode,
        kept,
        short
    ));
    match dst_sha {
        Some(d) if d == src_sha => out.push_str(" (source and destination match)"),
        Some(d) => out.push_str(&format!(
            " — VERIFY FAILED: destination sha256 {} differs from the bytes read",
            &d[..16.min(d.len())]
        )),
        None => {}
    }
    if let Some(old) = replaced {
        out.push_str(&format!(" — replaced existing {old} bytes"));
    }
    out
}

/// Outcome of the copy pass, for the tree report.
struct Executed {
    copied_bytes: u64,
    mode_failures: usize,
}

/// Tree report (spec §7). Counts come from the plan; `executed` is `None`
/// on a dry run.
fn render_tree_report(
    src: &Path,
    dst: &Path,
    plan: &CopyPlan,
    executed: Option<&Executed>,
    dry_run: bool,
) -> String {
    let mut out = String::new();
    let verb = if dry_run {
        out.push_str("DRY RUN — no write\n");
        "Would copy"
    } else {
        "Copied"
    };
    let bytes = executed.map(|e| e.copied_bytes).unwrap_or(plan.total_bytes);
    out.push_str(&format!(
        "{verb} {}/ → {}/: {}, {}, {}, {} bytes",
        src.display(),
        dst.display(),
        plural(plan.files.len(), "file", "files"),
        plural(plan.dirs.len(), "directory", "directories"),
        plural(plan.symlinks.len(), "symlink", "symlinks"),
        bytes
    ));
    if !plan.excluded.is_empty() {
        let items: Vec<String> = plan
            .excluded
            .iter()
            .map(|(p, is_dir)| {
                if *is_dir {
                    format!("{}/ (directory)", p.display())
                } else {
                    format!("{} (file)", p.display())
                }
            })
            .collect();
        out.push_str(&format!("\n  excluded: {}", capped_list(&items)));
    }
    for (i, (p, reason)) in plan.skipped.iter().enumerate() {
        if i == ENUM_CAP {
            out.push_str(&format!(
                "\n  skipped: … and {} more",
                plan.skipped.len() - ENUM_CAP
            ));
            break;
        }
        match reason {
            SkipReason::SymlinkOutside(resolved) => out.push_str(&format!(
                "\n  skipped: {} → {} ({})",
                p.display(),
                resolved.display(),
                reason.describe()
            )),
            _ => out.push_str(&format!("\n  skipped: {} ({})", p.display(), reason.describe())),
        }
    }
    if let Some(e) = executed {
        if e.copied_bytes != plan.total_bytes {
            out.push_str(&format!(
                "\n  note: source changed during the copy — planned {} bytes, copied {} bytes",
                plan.total_bytes, e.copied_bytes
            ));
        }
        if e.mode_failures > 0 {
            out.push_str(&format!(
                "\n  note: could not apply the source mode on {} {}",
                e.mode_failures,
                if e.mode_failures == 1 { "directory" } else { "directories" }
            ));
        }
    }
    out
}

// ---------------------------------------------------------------------------
// I/O primitives
// ---------------------------------------------------------------------------

/// Bytes available to an unprivileged writer on the filesystem holding `dir`.
/// One `statvfs(2)` call — the shape embra-web's `metrics::read_disk_info`
/// already ships on musl; re-implemented because embra-web is not in this
/// crate's dependency tree. `f_bavail` (not `f_bfree`, which includes the
/// root reserve) in units of `f_frsize`.
// The `as u64` casts are identity on x86_64/aarch64 (glibc and musl) but
// libc's `fsblkcnt_t` / `f_frsize` are narrower on other targets — keep the
// portable shape (embra-web's read_disk_info does the same).
#[allow(clippy::unnecessary_cast)]
fn available_bytes(dir: &Path) -> Result<u64, String> {
    use std::os::unix::ffi::OsStrExt;
    let c_path = std::ffi::CString::new(dir.as_os_str().as_bytes())
        .map_err(|_| format!("path contains an interior NUL byte: {}", dir.display()))?;
    let mut buf: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: `buf` is a valid, zero-initialized repr(C) statvfs that the
    // call fills in; `c_path` is NUL-terminated and outlives the call. No
    // Rust invariants cross the boundary.
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut buf) };
    if rc != 0 {
        return Err(format!(
            "could not check free space on {}: {}",
            dir.display(),
            std::io::Error::last_os_error()
        ));
    }
    Ok((buf.f_bavail as u64).saturating_mul(buf.f_frsize as u64))
}

/// Copy `src` into `tmp` with a hard ceiling on bytes actually READ (stat
/// sizes are advisory — procfs reports 0 for files with content). Fsyncs the
/// temp. Returns `(bytes, sha256 hex)`. Blocking — runs inside
/// `spawn_blocking`.
fn copy_bounded_blocking(src: &Path, tmp: &Path, limit: u64) -> Result<(u64, String), String> {
    use std::io::{Read, Write};
    let mut reader =
        std::fs::File::open(src).map_err(|e| format!("failed to open source: {e}"))?;
    let mut writer =
        std::fs::File::create(tmp).map_err(|e| format!("failed to create temp file: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    let mut total: u64 = 0;
    loop {
        let n = reader
            .read(&mut buf)
            .map_err(|e| format!("failed to read source: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            return Err(format!(
                "source yielded more than {limit} bytes — over the file_copy byte budget (stat size is advisory; the budget counts bytes actually read)"
            ));
        }
        hasher.update(&buf[..n]);
        writer
            .write_all(&buf[..n])
            .map_err(|e| format!("failed to write temp file: {e}"))?;
    }
    writer
        .sync_all()
        .map_err(|e| format!("failed to fsync temp file: {e}"))?;
    Ok((total, hex::encode(hasher.finalize())))
}

fn sha256_blocking(path: &Path) -> Result<String, String> {
    use std::io::Read;
    let mut f = std::fs::File::open(path).map_err(|e| format!("failed to open: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("failed to read: {e}"))?;
        if n == 0 {
            break;
        }
        hasher.update(&buf[..n]);
    }
    Ok(hex::encode(hasher.finalize()))
}

async fn sha256_file(path: &Path) -> Result<String, String> {
    let p = path.to_path_buf();
    tokio::task::spawn_blocking(move || sha256_blocking(&p))
        .await
        .map_err(|e| format!("hash task failed: {e}"))?
}

/// Removes a staged temp if the future is dropped before `commit_temp`
/// consumed it (the dispatcher's timeout). Disarmed on success.
struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

/// One file, atomically: same-directory temp ← bounded stream (source mode
/// applied) → shared `commit_temp` (prior destination's mode/ownership
/// restored when overwriting; rename; dir fsync). Returns `(bytes, sha256)`.
async fn copy_file_atomic(
    src: &Path,
    dst: &Path,
    prior: Option<&std::fs::Metadata>,
    src_mode: u32,
    limit: u64,
) -> Result<(u64, String), String> {
    use std::os::unix::fs::PermissionsExt;

    let tmp = temp_path_for(dst, TMP_TAG)?;
    let mut guard = TempGuard {
        path: tmp.clone(),
        armed: true,
    };

    let (s, t) = (src.to_path_buf(), tmp.clone());
    let (bytes, sha) = tokio::task::spawn_blocking(move || copy_bounded_blocking(&s, &t, limit))
        .await
        .map_err(|e| format!("copy task failed: {e}"))??;

    tokio::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(src_mode))
        .await
        .map_err(|e| format!("failed to set permissions on temp file: {e}"))?;

    // commit_temp removes the temp itself on a pre-rename failure; after a
    // successful rename the temp path no longer exists. Either way the guard
    // has nothing left to do.
    commit_temp(&tmp, dst, prior, false).await?;
    guard.armed = false;
    Ok((bytes, sha))
}

/// Tree-level all-or-nothing. The destination root must not pre-exist (the
/// tool creates it, so it owns every byte under it). `wipe` renames the root
/// to a hidden sibling FIRST — one atomic directory-entry swap, so nobody
/// observes a half-built tree at the requested path even if the removal
/// below fails — then removes it. `Drop` covers the dispatcher's timeout
/// dropping the future; the explicit `rollback_now` is the error path.
///
/// Blocking in `Drop` is deliberate: bounded by the entry cap, failure path
/// only, and `spawn_blocking` from a destructor is non-deterministic for
/// tests and may never run during runtime shutdown. Documented residual: a
/// blocking op still in flight after the drop can leave a temp inside the
/// renamed `…embra-copy-rollback…` sibling — inert and attributable.
struct RollbackGuard {
    root: PathBuf,
    armed: bool,
}

impl RollbackGuard {
    fn wipe(root: &Path) -> Option<PathBuf> {
        static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let name = root
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "root".to_string());
        let doomed = root.with_file_name(format!(
            ".{name}.embra-copy-rollback.{}.{}",
            std::process::id(),
            SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
        ));
        let target = match std::fs::rename(root, &doomed) {
            Ok(()) => doomed,
            Err(_) => root.to_path_buf(),
        };
        match std::fs::remove_dir_all(&target) {
            Ok(()) => None,
            Err(_) => Some(target),
        }
    }

    /// Error path: roll back now and report a leftover, if any.
    fn rollback_now(&mut self) -> Option<PathBuf> {
        self.armed = false;
        Self::wipe(&self.root)
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for RollbackGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = Self::wipe(&self.root);
        }
    }
}

/// Pre-pass: walk the source with an explicit stack, entries sorted by name
/// (stable plans, stable reports), `symlink_metadata` per entry (never
/// follows — cycle-safe and dangling-safe). Budget bail-outs happen DURING
/// the walk, so a virtual-filesystem source is bounded by the entry cap.
async fn plan_tree(src_root: &Path, dst_root: &Path, opts: &CopyOptions) -> Result<CopyPlan, String> {
    let mut plan = CopyPlan::default();
    let root_meta = tokio::fs::symlink_metadata(src_root)
        .await
        .map_err(|e| format!("failed to stat {}: {e}", src_root.display()))?;
    plan.dirs.push((PathBuf::new(), mode_of(&root_meta)));

    let mut stack: Vec<PathBuf> = vec![PathBuf::new()];
    while let Some(rel_dir) = stack.pop() {
        let abs_dir = src_root.join(&rel_dir);
        let mut names: Vec<std::ffi::OsString> = Vec::new();
        let mut rd = tokio::fs::read_dir(&abs_dir)
            .await
            .map_err(|e| format!("failed to read {}: {e}", abs_dir.display()))?;
        while let Some(entry) = rd
            .next_entry()
            .await
            .map_err(|e| format!("failed to read {}: {e}", abs_dir.display()))?
        {
            names.push(entry.file_name());
        }
        names.sort();

        let mut children: Vec<PathBuf> = Vec::new();
        for name in names {
            let rel = rel_dir.join(&name);
            plan.entries += 1;
            if plan.entries > opts.caps.max_entries {
                return Err(format!(
                    "source tree {} has more than {} entries — the file_copy entry budget. Narrow the copy (exclude) or copy subtrees.",
                    src_root.display(),
                    opts.caps.max_entries
                ));
            }
            let abs = src_root.join(&rel);
            let md = match tokio::fs::symlink_metadata(&abs).await {
                Ok(m) => m,
                Err(e) => {
                    plan.skipped.push((rel, SkipReason::Unreadable(e.to_string())));
                    continue;
                }
            };
            let ft = md.file_type();
            if opts
                .exclude
                .iter()
                .any(|x| std::ffi::OsStr::new(x) == name.as_os_str())
            {
                plan.excluded.push((rel, ft.is_dir()));
                continue;
            }
            if ft.is_symlink() {
                let raw = match tokio::fs::read_link(&abs).await {
                    Ok(r) => r,
                    Err(e) => {
                        plan.skipped.push((rel, SkipReason::Unreadable(e.to_string())));
                        continue;
                    }
                };
                let link_new = dst_root.join(&rel);
                match symlink_target_ok(&link_new, &raw, &opts.jail_root) {
                    Ok(()) => {
                        // A pre-existing escaping link elsewhere in the
                        // workspace (git_clone can create them): if the
                        // lexical target exists, its real path must also be
                        // inside the jail.
                        let lexical = if raw.is_absolute() {
                            normalize_lexical(&raw)
                        } else {
                            normalize_lexical(&link_new.parent().unwrap_or_else(|| Path::new("/")).join(&raw))
                        };
                        match tokio::fs::canonicalize(&lexical).await {
                            Ok(real) if !real.starts_with(&opts.jail_root) => {
                                plan.skipped.push((rel, SkipReason::SymlinkOutside(real)));
                            }
                            _ => plan.symlinks.push((rel, raw)),
                        }
                    }
                    Err(resolved) => plan.skipped.push((rel, SkipReason::SymlinkOutside(resolved))),
                }
            } else if ft.is_dir() {
                plan.dirs.push((rel.clone(), mode_of(&md)));
                children.push(rel);
            } else if ft.is_file() {
                plan.total_bytes += md.len();
                if plan.total_bytes > opts.caps.max_bytes {
                    return Err(format!(
                        "source tree {} holds more than {} bytes ({} MiB) — over the file_copy per-call budget. Copy in parts or exclude large subtrees.",
                        src_root.display(),
                        opts.caps.max_bytes,
                        opts.caps.max_bytes / (1024 * 1024)
                    ));
                }
                plan.files.push((rel, md.len()));
            } else {
                plan.skipped.push((rel, SkipReason::Special(special_kind(&ft))));
            }
        }
        // LIFO + reverse = children visited in sorted order.
        for c in children.into_iter().rev() {
            stack.push(c);
        }
    }
    Ok(plan)
}

/// Copy pass over a plan. Directories are created in plan order (default
/// mode); files go through the bounded atomic copy with a RUNNING budget;
/// symlinks are recreated verbatim; source modes land LAST, deepest-first and
/// best-effort (a `0555` directory applied early would defeat rollback).
/// A source file that vanished or changed type is a hard error — a silently
/// incomplete tree is the failure this path exists to prevent.
async fn execute_tree(
    src_root: &Path,
    dst_root: &Path,
    plan: &CopyPlan,
    opts: &CopyOptions,
) -> Result<Executed, String> {
    use std::os::unix::fs::PermissionsExt;

    for (rel, _) in plan.dirs.iter().skip(1) {
        let d = dst_root.join(rel);
        tokio::fs::create_dir(&d)
            .await
            .map_err(|e| format!("failed to create {}: {e}", d.display()))?;
    }

    let mut copied: u64 = 0;
    let mut remaining = opts.caps.max_bytes;
    for (i, (rel, _planned)) in plan.files.iter().enumerate() {
        if opts.fail_after_files == Some(i) {
            return Err("injected failure (test)".to_string());
        }
        let src = src_root.join(rel);
        let dst = dst_root.join(rel);
        let meta = tokio::fs::symlink_metadata(&src)
            .await
            .map_err(|e| format!("{} vanished during the copy: {e}", rel.display()))?;
        if !meta.file_type().is_file() {
            return Err(format!("{} changed type during the copy", rel.display()));
        }
        let (n, _sha) = copy_file_atomic(&src, &dst, None, mode_of(&meta), remaining)
            .await
            .map_err(|e| format!("{}: {e}", rel.display()))?;
        remaining = remaining.saturating_sub(n);
        copied += n;
    }

    for (rel, raw) in &plan.symlinks {
        let link = dst_root.join(rel);
        tokio::fs::symlink(raw, &link)
            .await
            .map_err(|e| format!("failed to create symlink {}: {e}", link.display()))?;
    }

    let mut mode_failures = 0usize;
    for (rel, mode) in plan.dirs.iter().rev() {
        if tokio::fs::set_permissions(dst_root.join(rel), std::fs::Permissions::from_mode(*mode))
            .await
            .is_err()
        {
            mode_failures += 1;
        }
    }

    Ok(Executed {
        copied_bytes: copied,
        mode_failures,
    })
}

/// Walk up from `p` until a component exists (by `symlink_metadata`, so a
/// dangling link counts as existing and the later `create_dir_all` reports
/// it instead of silently building beside it).
async fn nearest_existing_ancestor(p: &Path) -> PathBuf {
    let mut cur = p.to_path_buf();
    loop {
        if tokio::fs::symlink_metadata(&cur).await.is_ok() {
            return cur;
        }
        match cur.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => cur = parent.to_path_buf(),
            _ => return PathBuf::from("/"),
        }
    }
}

fn nothing(msg: impl std::fmt::Display) -> String {
    format!("Error: {msg}\nNothing copied.")
}

fn unchanged(msg: impl std::fmt::Display) -> String {
    format!("Error: {msg}\nDestination unchanged.")
}

// ---------------------------------------------------------------------------
// Post-jail core (the /tmp-testable seam)
// ---------------------------------------------------------------------------

/// Everything after the lexical destination jail and source canonicalization:
/// destination triage, canonical re-verify against `opts.jail_root`, the
/// same-path / inside-source guards, then the file or tree copy. `src` must
/// already be canonical.
pub(crate) async fn copy_at(src: &Path, dst: &Path, opts: &CopyOptions) -> String {
    // -- source shape -------------------------------------------------------
    let src_meta = match tokio::fs::metadata(src).await {
        Ok(m) => m,
        Err(e) => return nothing(format!("failed to stat source {}: {e}", src.display())),
    };
    let src_is_dir = src_meta.is_dir();
    if !src_is_dir && !src_meta.is_file() {
        return nothing(format!(
            "source {} is a special file ({}) — file_copy only copies regular files, directories and symlinks",
            src.display(),
            special_kind(&src_meta.file_type())
        ));
    }

    // -- destination triage (never copy INTO a directory; never merge) -------
    let dst_exists = match tokio::fs::symlink_metadata(dst).await {
        Ok(m) if m.is_dir() => return nothing(into_dir_msg(dst, src)),
        Ok(m) if m.file_type().is_symlink() => match tokio::fs::metadata(dst).await {
            Ok(t) if t.is_dir() => return nothing(into_dir_msg(dst, src)),
            Ok(_) => true,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                return nothing(format!(
                    "destination {} is a dangling symlink — file_delete it first",
                    dst.display()
                ))
            }
            Err(e) => return nothing(format!("failed to stat destination {}: {e}", dst.display())),
        },
        Ok(_) => true,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return nothing(format!("failed to stat destination {}: {e}", dst.display())),
    };

    if src_is_dir {
        if !opts.recursive {
            return nothing(format!(
                "source {} is a directory — pass recursive=true to copy a directory tree",
                src.display()
            ));
        }
        if opts.overwrite {
            return nothing(
                "overwrite applies to single files only — a tree copy never merges into an existing destination",
            );
        }
        if dst_exists {
            return unchanged(format!(
                "destination {} already exists — file_copy never merges trees; copy to a new path or remove it first",
                dst.display()
            ));
        }
    } else {
        if !opts.exclude.is_empty() {
            return nothing(format!(
                "exclude applies to directory copies only (source {} is a file)",
                src.display()
            ));
        }
        if dst_exists && !opts.overwrite {
            return unchanged(format!(
                "destination {} already exists — pass overwrite=true to replace it (files only)",
                dst.display()
            ));
        }
    }
    for x in &opts.exclude {
        if x.is_empty() || x.contains('/') {
            return nothing(format!(
                "exclude entries are exact basenames (no '/' or globs): {x:?}"
            ));
        }
    }

    // -- destination canonicalization + jail re-verify -----------------------
    // The nearest existing ancestor is canonicalized and checked BEFORE any
    // directory is created (a symlinked ancestor pointing outside the jail
    // must not get parents built under it); the write path then creates the
    // missing parents under the canonical ancestor.
    let anc = nearest_existing_ancestor(dst).await;
    let anc_canon = match tokio::fs::canonicalize(&anc).await {
        Ok(p) => p,
        Err(e) => return nothing(format!("failed to resolve {}: {e}", anc.display())),
    };
    if !anc_canon.starts_with(&opts.jail_root) {
        return format!(
            "Denied: destination '{}' resolves outside {} (real path {})\nNothing copied.",
            dst.display(),
            opts.jail_root.display(),
            anc_canon.display()
        );
    }
    let dst_final = match dst.strip_prefix(&anc) {
        // An empty remainder (the destination itself exists) must not be
        // joined: `join("")` appends a trailing separator, and rename(2)
        // refuses `file/` with ENOTDIR.
        Ok(rest) if !rest.as_os_str().is_empty() => anc_canon.join(rest),
        _ => anc_canon.clone(),
    };
    if dst_final == src {
        return nothing(format!(
            "source and destination are the same path ({})",
            src.display()
        ));
    }
    if src_is_dir && dest_inside_source(src, &dst_final) {
        return nothing(format!(
            "destination {} is inside the source tree {} — cannot copy a directory into itself",
            dst_final.display(),
            src.display()
        ));
    }

    // -- free space ----------------------------------------------------------
    let available = match opts.available_override {
        Some(a) => a,
        None => match available_bytes(&anc_canon) {
            Ok(a) => a,
            Err(e) => return nothing(e),
        },
    };
    let fs_label = opts.jail_root.display().to_string();
    let free_space_check = |planned: u64| -> Option<String> {
        if planned.saturating_add(opts.caps.free_reserve) > available {
            Some(nothing(format!(
                "this copy needs {planned} bytes but only {available} bytes are free on {fs_label}, and the file_copy reserve keeps {} bytes for WardSONDB",
                opts.caps.free_reserve
            )))
        } else {
            None
        }
    };

    // ======================================================================
    // Single file
    // ======================================================================
    if !src_is_dir {
        let planned = src_meta.len();
        if planned > opts.caps.max_bytes {
            return nothing(format!(
                "source {} is {} bytes — over the {} byte ({} MiB) file_copy per-call budget",
                src.display(),
                planned,
                opts.caps.max_bytes,
                opts.caps.max_bytes / (1024 * 1024)
            ));
        }
        if let Some(msg) = free_space_check(planned) {
            return msg;
        }
        let src_mode = mode_of(&src_meta);
        let prior = if dst_exists {
            match tokio::fs::metadata(&dst_final).await {
                Ok(m) => Some(m),
                Err(e) => return unchanged(format!("failed to stat destination {}: {e}", dst_final.display())),
            }
        } else {
            None
        };
        let replaced = prior.as_ref().map(|m| m.len());

        if opts.dry_run {
            // Bounded like the real copy: a stat-lying source is reported,
            // not streamed to exhaustion.
            let (s, limit) = (src.to_path_buf(), opts.caps.max_bytes);
            let probe = match tokio::task::spawn_blocking(move || sha256_bounded_blocking(&s, limit)).await {
                Ok(r) => r,
                Err(e) => Err(format!("hash task failed: {e}")),
            };
            return match probe {
                Ok((bytes, sha)) => render_file_report(
                    src, &dst_final, bytes, src_mode, &sha, None, replaced, true,
                ),
                Err(e) => nothing(e),
            };
        }

        if let Some(parent) = dst_final.parent()
            && let Err(e) = tokio::fs::create_dir_all(parent).await
        {
            return nothing(format!("failed to create directory {}: {e}", parent.display()));
        }
        let (bytes, sha) =
            match copy_file_atomic(src, &dst_final, prior.as_ref(), src_mode, opts.caps.max_bytes).await {
                Ok(v) => v,
                Err(e) => {
                    return if dst_exists {
                        unchanged(e)
                    } else {
                        nothing(e)
                    }
                }
            };
        let dst_sha = match sha256_file(&dst_final).await {
            Ok(s) => s,
            Err(e) => return format!(
                "Copied {} → {}: {} bytes, but could not verify the destination: {e}",
                src.display(),
                dst_final.display(),
                bytes
            ),
        };
        let mode = if dst_exists {
            prior.as_ref().map(mode_of).unwrap_or(src_mode)
        } else {
            src_mode
        };
        return render_file_report(
            src, &dst_final, bytes, mode, &sha, Some(&dst_sha), replaced, false,
        );
    }

    // ======================================================================
    // Directory tree
    // ======================================================================
    let plan = match plan_tree(src, &dst_final, opts).await {
        Ok(p) => p,
        Err(e) => return nothing(e),
    };
    if let Some(msg) = free_space_check(plan.total_bytes) {
        return msg;
    }
    if opts.dry_run {
        return render_tree_report(src, &dst_final, &plan, None, true);
    }

    if let Some(parent) = dst_final.parent()
        && let Err(e) = tokio::fs::create_dir_all(parent).await
    {
        return nothing(format!("failed to create directory {}: {e}", parent.display()));
    }
    // create_dir (not create_dir_all): the root must not pre-exist — that is
    // what lets the guard own everything under it.
    if let Err(e) = tokio::fs::create_dir(&dst_final).await {
        return nothing(format!("failed to create {}: {e}", dst_final.display()));
    }
    let mut guard = RollbackGuard {
        root: dst_final.clone(),
        armed: true,
    };

    match execute_tree(src, &dst_final, &plan, opts).await {
        Ok(executed) => {
            let report = render_tree_report(src, &dst_final, &plan, Some(&executed), false);
            guard.disarm();
            report
        }
        Err(e) => {
            let leftover = guard.rollback_now();
            let mut msg = format!(
                "Error: {e}\nRolled back: {} removed, nothing of the tree remains.",
                dst_final.display()
            );
            if let Some(l) = leftover {
                msg.push_str(&format!(
                    "\n  note: rollback could not fully remove {} — remove it with dir_delete force=true",
                    l.display()
                ));
            }
            msg
        }
    }
}

fn into_dir_msg(dst: &Path, src: &Path) -> String {
    let name = src
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "<name>".to_string());
    format!(
        "destination {} is an existing directory — file_copy never copies INTO a directory; give the full target path (e.g. {}/{})",
        dst.display(),
        dst.display(),
        name
    )
}

/// Dry-run probe: hash the source under the same byte ceiling the real copy
/// enforces.
fn sha256_bounded_blocking(src: &Path, limit: u64) -> Result<(u64, String), String> {
    use std::io::Read;
    let mut f = std::fs::File::open(src).map_err(|e| format!("failed to open source: {e}"))?;
    let mut hasher = Sha256::new();
    let mut buf = vec![0u8; COPY_BUF];
    let mut total: u64 = 0;
    loop {
        let n = f.read(&mut buf).map_err(|e| format!("failed to read source: {e}"))?;
        if n == 0 {
            break;
        }
        total += n as u64;
        if total > limit {
            return Err(format!(
                "source yields more than {limit} bytes — over the file_copy byte budget (stat size is advisory; the budget counts bytes actually read)"
            ));
        }
        hasher.update(&buf[..n]);
    }
    Ok((total, hex::encode(hasher.finalize())))
}

// ---------------------------------------------------------------------------
// Jail wrapper + args
// ---------------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_copy",
    is_side_effectful = true,
    description = "Copy a file or a directory tree host-side — contents never enter the conversation, so size is decoupled from the 2 MiB read ceiling and binary files (images, archives) copy intact; this is how a generated image in /embra/workspace/MEDIA/ reaches a repo. source may be ANY readable path (like file_read); destination must resolve under /embra/workspace (workspace-relative or absolute) and is the exact final path — file_copy never copies INTO a directory, so name the target file or directory yourself. Files: atomic (same-directory temp + fsync + rename); an existing destination is refused unless overwrite=true (files only — replaced atomically, keeping its mode and ownership; the way to restore from a backup). Directories need recursive=true; the destination must not exist (trees never merge); symlinks are recreated, not followed (links whose target would resolve outside /embra/workspace are skipped and reported); special files are skipped; .git is copied unless listed in exclude (exact basenames, no globs); a failure part-way removes everything written — all-or-nothing. Per-call budgets: 512 MiB, 100000 entries, and 256 MiB must stay free on the data partition. dry_run=true walks, validates and reports (bytes, counts, skips) without writing anything. The report gives absolute byte counts, mode, and for single files a sha256 of source and destination."
)]
#[serde(deny_unknown_fields)]
pub struct FileCopyArgs {
    /// File or directory to copy. Any readable absolute path, or
    /// workspace-relative. A symlink source is followed.
    pub source: String,
    /// The exact final path (never a directory to copy INTO), under
    /// /embra/workspace. Missing parent directories are created.
    pub destination: String,
    /// Required to copy a directory tree.
    #[serde(default)]
    pub recursive: bool,
    /// Replace an existing destination FILE atomically (keeps its mode and
    /// ownership). Never applies to directory copies.
    #[serde(default)]
    pub overwrite: bool,
    /// Walk, validate and report without writing anything.
    #[serde(default)]
    pub dry_run: bool,
    /// Exact basenames to skip (and not descend into) during a directory
    /// copy, e.g. [".git", "target"]. No globs.
    #[serde(default)]
    pub exclude: Option<Vec<String>>,
}

impl FileCopyArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(file_copy_impl(self).await)
    }
}

pub(crate) async fn file_copy_impl(args: FileCopyArgs) -> String {
    // Destination jail FIRST (lexical: uniform '..' / Denied messages even
    // though the source is unjailed), then the source via file_read's
    // permissive rule — the copy is a read of the source and a write of the
    // destination, and each side keeps its family's policy.
    let dst = match resolve_workspace_path(&args.destination) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if args.source.trim().is_empty() {
        return nothing("source is empty");
    }
    let src_path = crate::media::store::resolve_read_path(&args.source);
    let src = match tokio::fs::canonicalize(&src_path).await {
        Ok(p) => p,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return nothing(format!("source not found: {}", src_path.display()))
        }
        Err(e) => return nothing(format!("failed to resolve source {}: {e}", src_path.display())),
    };
    let opts = CopyOptions::production(&args);
    copy_at(&src, Path::new(&dst), &opts).await
}

// ---------------------------------------------------------------------------
// Tests — T1..T20 are the acceptance tests from the spec (§9), numbered to
// match; u01.. are pure-core units.
// ---------------------------------------------------------------------------

#[cfg(test)]
mod file_copy_tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "embra-file-copy-test-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            // Canonical so `copy_at`'s canonical destination and the jail
            // compare component-wise (macOS /tmp is a symlink; Linux usually
            // not — canonicalizing makes the tests host-independent).
            TempDir(std::fs::canonicalize(&d).unwrap())
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn opts(dir: &TempDir) -> CopyOptions {
        CopyOptions {
            recursive: false,
            overwrite: false,
            dry_run: false,
            exclude: vec![],
            jail_root: dir.0.clone(),
            caps: Caps::default(),
            fail_after_files: None,
            // statvfs on /tmp works everywhere, but a CI runner with a nearly
            // full disk must not flake the suite: tests pin a large value and
            // T19 exercises the refusal explicitly.
            available_override: Some(u64::MAX / 4),
        }
    }

    fn write(p: &Path, bytes: &[u8]) {
        if let Some(parent) = p.parent() {
            std::fs::create_dir_all(parent).unwrap();
        }
        std::fs::write(p, bytes).unwrap();
    }

    fn mode(p: &Path) -> u32 {
        std::fs::symlink_metadata(p).unwrap().permissions().mode() & 0o777
    }

    fn leftovers(dir: &Path, tag: &str) -> Vec<String> {
        let mut out = Vec::new();
        let mut stack = vec![dir.to_path_buf()];
        while let Some(d) = stack.pop() {
            if let Ok(rd) = std::fs::read_dir(&d) {
                for e in rd.flatten() {
                    let name = e.file_name().to_string_lossy().into_owned();
                    if name.contains(tag) {
                        out.push(e.path().display().to_string());
                    }
                    if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                        stack.push(e.path());
                    }
                }
            }
        }
        out
    }

    /// A small tree: root/{a.txt, sub/b.txt, sub/deep/c.bin, link -> a.txt}
    fn make_tree(root: &Path) {
        write(&root.join("a.txt"), b"alpha\n");
        write(&root.join("sub/b.txt"), b"bravo bravo\n");
        write(&root.join("sub/deep/c.bin"), &[0u8, 1, 2, 3, 255]);
        std::os::unix::fs::symlink("a.txt", root.join("link")).unwrap();
    }

    // -- acceptance ---------------------------------------------------------

    #[tokio::test]
    async fn t01_single_file_reports_bytes_mode_and_sha() {
        let dir = TempDir::new("t01");
        let src = dir.0.join("src.bin");
        write(&src, b"hello world");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o640)).unwrap();
        let dst = dir.0.join("out/copy.bin");

        let out = copy_at(&src, &dst, &opts(&dir)).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert!(out.contains(": 11 bytes, mode 0640, sha256 "), "{out}");
        assert!(out.contains("(source and destination match)"), "{out}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"hello world");
        assert_eq!(mode(&dst), 0o640);
        // sha256("hello world") = b94d27b9…
        assert!(out.contains("sha256 b94d27b993"), "{out}");
        assert!(leftovers(&dir.0, "embra-copy").is_empty());
    }

    #[tokio::test]
    async fn t02_overwrite_replaces_atomically_and_keeps_dst_mode() {
        let dir = TempDir::new("t02");
        let src = dir.0.join("backup.md");
        write(&src, b"restored content");
        std::fs::set_permissions(&src, std::fs::Permissions::from_mode(0o644)).unwrap();
        let dst = dir.0.join("live.md");
        write(&dst, b"bad");
        std::fs::set_permissions(&dst, std::fs::Permissions::from_mode(0o600)).unwrap();

        let mut o = opts(&dir);
        o.overwrite = true;
        let out = copy_at(&src, &dst, &o).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert!(out.contains("mode 0600 (kept)"), "{out}");
        assert!(out.contains("— replaced existing 3 bytes"), "{out}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"restored content");
        assert_eq!(mode(&dst), 0o600);
        assert!(leftovers(&dir.0, "embra-copy").is_empty());
    }

    #[tokio::test]
    async fn t03_existing_destination_without_overwrite_refuses() {
        let dir = TempDir::new("t03");
        let src = dir.0.join("a");
        let dst = dir.0.join("b");
        write(&src, b"new");
        write(&dst, b"old");
        let out = copy_at(&src, &dst, &opts(&dir)).await;
        assert!(out.starts_with("Error:"), "{out}");
        assert!(out.contains("overwrite=true"), "{out}");
        assert!(out.ends_with("Destination unchanged."), "{out}");
        assert_eq!(std::fs::read(&dst).unwrap(), b"old");
    }

    #[tokio::test]
    async fn t04_destination_is_directory_refuses() {
        let dir = TempDir::new("t04");
        let src = dir.0.join("a.txt");
        write(&src, b"x");
        let dst = dir.0.join("docs");
        std::fs::create_dir_all(&dst).unwrap();
        let out = copy_at(&src, &dst, &opts(&dir)).await;
        assert!(out.contains("never copies INTO a directory"), "{out}");
        assert!(out.contains("docs/a.txt"), "{out}");
        assert!(out.ends_with("Nothing copied."), "{out}");
        assert!(std::fs::read_dir(&dst).unwrap().next().is_none());
    }

    #[tokio::test]
    async fn t05_source_equals_destination_refuses() {
        let dir = TempDir::new("t05");
        let src = dir.0.join("same.txt");
        write(&src, b"x");
        let mut o = opts(&dir);
        o.overwrite = true;
        let out = copy_at(&src, &src, &o).await;
        assert!(out.contains("same path"), "{out}");
        // Through a symlink alias: both canonicalize to one path.
        let alias = dir.0.join("alias");
        std::os::unix::fs::symlink(&src, &alias).unwrap();
        let out = copy_at(&src, &alias, &o).await;
        assert!(out.contains("same path"), "{out}");
        assert_eq!(std::fs::read(&src).unwrap(), b"x");
    }

    #[tokio::test]
    async fn t06_destination_inside_source_refuses() {
        let dir = TempDir::new("t06");
        let root = dir.0.join("a");
        make_tree(&root);
        let mut o = opts(&dir);
        o.recursive = true;
        let out = copy_at(&root, &root.join("b"), &o).await;
        assert!(out.contains("inside the source tree"), "{out}");
        assert!(!root.join("b").exists());
        // A sibling with a shared prefix is NOT inside.
        let out = copy_at(&root, &dir.0.join("a-other"), &o).await;
        assert!(out.starts_with("Copied "), "{out}");
    }

    #[tokio::test]
    async fn t07_directory_without_recursive_names_the_flag() {
        let dir = TempDir::new("t07");
        let root = dir.0.join("tree");
        make_tree(&root);
        let out = copy_at(&root, &dir.0.join("copy"), &opts(&dir)).await;
        assert!(out.contains("recursive=true"), "{out}");
        assert!(!dir.0.join("copy").exists());
    }

    #[tokio::test]
    async fn t08_tree_copy_counts_and_modes() {
        let dir = TempDir::new("t08");
        let root = dir.0.join("tree");
        make_tree(&root);
        std::fs::set_permissions(&root.join("sub"), std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&root.join("a.txt"), std::fs::Permissions::from_mode(0o600)).unwrap();
        let dst = dir.0.join("out/copy");
        let mut o = opts(&dir);
        o.recursive = true;
        let out = copy_at(&root, &dst, &o).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert!(
            out.contains(": 3 files, 3 directories, 1 symlink, 23 bytes"),
            "{out}"
        );
        assert_eq!(std::fs::read(dst.join("a.txt")).unwrap(), b"alpha\n");
        assert_eq!(std::fs::read(dst.join("sub/b.txt")).unwrap(), b"bravo bravo\n");
        assert_eq!(std::fs::read(dst.join("sub/deep/c.bin")).unwrap(), &[0u8, 1, 2, 3, 255]);
        assert_eq!(mode(&dst.join("sub")), 0o700);
        assert_eq!(mode(&dst.join("a.txt")), 0o600);
        assert!(leftovers(&dir.0, "embra-copy").is_empty());
    }

    #[tokio::test]
    async fn t09_symlink_recreated_not_followed() {
        let dir = TempDir::new("t09");
        let root = dir.0.join("tree");
        make_tree(&root);
        std::os::unix::fs::symlink("../a.txt", root.join("sub/up")).unwrap();
        let dst = dir.0.join("copy");
        let mut o = opts(&dir);
        o.recursive = true;
        let out = copy_at(&root, &dst, &o).await;
        assert!(out.contains("2 symlinks"), "{out}");
        let link = dst.join("link");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read_link(&link).unwrap(), PathBuf::from("a.txt"));
        assert_eq!(std::fs::read_link(dst.join("sub/up")).unwrap(), PathBuf::from("../a.txt"));
        // Resolves through the COPY, not the source.
        assert_eq!(std::fs::read(dst.join("sub/up")).unwrap(), b"alpha\n");
    }

    #[tokio::test]
    async fn t10_symlink_outside_jail_skipped() {
        let dir = TempDir::new("t10");
        let root = dir.0.join("tree");
        make_tree(&root);
        std::os::unix::fs::symlink("/etc/passwd", root.join("escape")).unwrap();
        // Relative climb out of the jail too.
        std::os::unix::fs::symlink("../../../../../../../etc/hosts", root.join("sub/climb")).unwrap();
        let dst = dir.0.join("copy");
        let mut o = opts(&dir);
        o.recursive = true;
        let out = copy_at(&root, &dst, &o).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert!(out.contains("1 symlink,"), "{out}");
        assert!(
            out.contains("skipped: escape → /etc/passwd (symlink target outside"),
            "{out}"
        );
        assert!(out.contains("skipped: sub/climb → /etc/hosts"), "{out}");
        assert!(std::fs::symlink_metadata(dst.join("escape")).is_err());
        assert!(std::fs::symlink_metadata(dst.join("sub/climb")).is_err());
    }

    #[tokio::test]
    async fn t11_special_file_skipped_never_opened() {
        let dir = TempDir::new("t11");
        let root = dir.0.join("tree");
        make_tree(&root);
        let fifo = root.join("pipe");
        let c = std::ffi::CString::new(fifo.to_str().unwrap()).unwrap();
        // SAFETY: valid NUL-terminated path; mkfifo has no other contract.
        let rc = unsafe { libc::mkfifo(c.as_ptr(), 0o600) };
        assert_eq!(rc, 0, "mkfifo failed: {}", std::io::Error::last_os_error());
        let dst = dir.0.join("copy");
        let mut o = opts(&dir);
        o.recursive = true;
        // An open() on the fifo would block this test forever.
        let out = copy_at(&root, &dst, &o).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert!(out.contains("skipped: pipe (special file: fifo)"), "{out}");
        assert!(std::fs::symlink_metadata(dst.join("pipe")).is_err());
        // Single-file copy of a special file is refused before any open.
        let out = copy_at(&fifo, &dir.0.join("pipe-copy"), &opts(&dir)).await;
        assert!(out.contains("special file (fifo)"), "{out}");
    }

    #[tokio::test]
    async fn t12_exclude_matches_exact_basenames() {
        let dir = TempDir::new("t12");
        let root = dir.0.join("repo");
        make_tree(&root);
        write(&root.join(".git/HEAD"), b"ref: refs/heads/main\n");
        write(&root.join(".git/objects/ab/cd"), b"blob");
        write(&root.join("target/debug/bin"), b"elf");
        write(&root.join("git"), b"a file literally named git");
        let dst = dir.0.join("copy");
        let mut o = opts(&dir);
        o.recursive = true;
        o.exclude = vec![".git".into(), "target".into()];
        let out = copy_at(&root, &dst, &o).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert!(out.contains("excluded: .git/ (directory), target/ (directory)"), "{out}");
        assert!(!dst.join(".git").exists());
        assert!(!dst.join("target").exists());
        assert_eq!(std::fs::read(dst.join("git")).unwrap(), b"a file literally named git");
        // Without exclude, .git is copied like anything else.
        let dst2 = dir.0.join("copy2");
        o.exclude = vec![];
        let out = copy_at(&root, &dst2, &o).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert_eq!(std::fs::read(dst2.join(".git/HEAD")).unwrap(), b"ref: refs/heads/main\n");
    }

    #[tokio::test]
    async fn t13_dry_run_writes_nothing() {
        let dir = TempDir::new("t13");
        let root = dir.0.join("tree");
        make_tree(&root);
        let dst = dir.0.join("deep/er/copy");
        let mut o = opts(&dir);
        o.recursive = true;
        o.dry_run = true;
        let dry = copy_at(&root, &dst, &o).await;
        assert!(dry.starts_with("DRY RUN — no write\nWould copy "), "{dry}");
        assert!(dry.contains(": 3 files, 3 directories, 1 symlink, 23 bytes"), "{dry}");
        assert!(!dst.exists());
        assert!(!dir.0.join("deep").exists(), "dry run must not create parents");

        // Single-file dry run: same numbers, no file, no parents.
        let fdst = dir.0.join("nope/f.txt");
        let fdry = copy_at(&root.join("a.txt"), &fdst, &opts_dry(&dir)).await;
        assert!(fdry.starts_with("DRY RUN — no write\nWould copy "), "{fdry}");
        assert!(fdry.contains(": 6 bytes, mode 0"), "{fdry}");
        assert!(!dir.0.join("nope").exists());

        // Real run reports the identical numbers.
        o.dry_run = false;
        let real = copy_at(&root, &dst, &o).await;
        assert!(real.contains(": 3 files, 3 directories, 1 symlink, 23 bytes"), "{real}");
    }

    fn opts_dry(dir: &TempDir) -> CopyOptions {
        let mut o = opts(dir);
        o.dry_run = true;
        o
    }

    #[tokio::test]
    async fn t14_budget_refusals_name_the_numbers() {
        let dir = TempDir::new("t14");
        let root = dir.0.join("tree");
        make_tree(&root);
        let mut o = opts(&dir);
        o.recursive = true;
        o.caps.max_bytes = 10;
        let out = copy_at(&root, &dir.0.join("c1"), &o).await;
        assert!(out.contains("holds more than 10 bytes"), "{out}");
        assert!(out.ends_with("Nothing copied."), "{out}");
        assert!(!dir.0.join("c1").exists());

        o.caps = Caps::default();
        o.caps.max_entries = 2;
        let out = copy_at(&root, &dir.0.join("c2"), &o).await;
        assert!(out.contains("more than 2 entries"), "{out}");
        assert!(!dir.0.join("c2").exists());

        // Single file over the budget is refused on the stat, before any read.
        let mut f = opts(&dir);
        f.caps.max_bytes = 3;
        let out = copy_at(&root.join("a.txt"), &dir.0.join("c3"), &f).await;
        assert!(out.contains("is 6 bytes — over the 3 byte"), "{out}");
        assert!(!dir.0.join("c3").exists());
    }

    #[tokio::test]
    async fn t15_rollback_leaves_no_destination_root() {
        let dir = TempDir::new("t15");
        let root = dir.0.join("tree");
        make_tree(&root);
        let dst = dir.0.join("out/copy");
        let mut o = opts(&dir);
        o.recursive = true;
        o.fail_after_files = Some(1);
        let out = copy_at(&root, &dst, &o).await;
        assert!(out.starts_with("Error: injected failure"), "{out}");
        assert!(out.contains("Rolled back:"), "{out}");
        assert!(!dst.exists(), "destination root must be gone");
        assert!(leftovers(&dir.0, "embra-copy").is_empty(), "{:?}", leftovers(&dir.0, "embra-copy"));
        // Source untouched.
        assert_eq!(std::fs::read(root.join("a.txt")).unwrap(), b"alpha\n");
        assert_eq!(std::fs::read(root.join("sub/b.txt")).unwrap(), b"bravo bravo\n");
    }

    #[tokio::test]
    async fn t16_jail_rejects_traversal_and_outside_paths() {
        let a = FileCopyArgs {
            source: "x".into(),
            destination: "../etc/evil".into(),
            recursive: false,
            overwrite: false,
            dry_run: false,
            exclude: None,
        };
        let msg = file_copy_impl(a).await;
        assert!(msg.contains("'..'"), "expected uniform traversal rejection, got: {msg}");

        let b = FileCopyArgs {
            source: "x".into(),
            destination: "/etc/evil".into(),
            recursive: false,
            overwrite: false,
            dry_run: false,
            exclude: None,
        };
        let msg = file_copy_impl(b).await;
        assert!(msg.starts_with("Denied:"), "expected outside-workspace denial, got: {msg}");

        // A canonical escape: the destination's existing ancestor is a
        // symlink pointing outside the jail.
        let dir = TempDir::new("t16");
        let outside = TempDir::new("t16-outside");
        std::os::unix::fs::symlink(&outside.0, dir.0.join("esc")).unwrap();
        let src = dir.0.join("a.txt");
        write(&src, b"x");
        let out = copy_at(&src, &dir.0.join("esc/new/a.txt"), &opts(&dir)).await;
        assert!(out.starts_with("Denied:"), "{out}");
        assert!(!outside.0.join("new").exists(), "no parents built outside the jail");
    }

    #[tokio::test]
    async fn t17_dry_run_reports_skips_and_exclusions() {
        let dir = TempDir::new("t17");
        let root = dir.0.join("tree");
        make_tree(&root);
        std::os::unix::fs::symlink("/etc/passwd", root.join("escape")).unwrap();
        write(&root.join(".git/HEAD"), b"x");
        let mut o = opts(&dir);
        o.recursive = true;
        o.exclude = vec![".git".into()];
        o.dry_run = true;
        let dry = copy_at(&root, &dir.0.join("c"), &o).await;
        o.dry_run = false;
        let real = copy_at(&root, &dir.0.join("c"), &o).await;
        let tail = |s: &str| s.lines().skip(1).map(str::to_string).collect::<Vec<_>>();
        let mut dry_lines = tail(&dry);
        let mut real_lines = real.lines().map(str::to_string).collect::<Vec<_>>();
        // First lines differ only in the verb.
        assert_eq!(dry_lines.remove(0).replace("Would copy", "Copied"), real_lines.remove(0));
        assert_eq!(dry_lines, real_lines);
        assert!(dry.contains("excluded: .git/ (directory)"), "{dry}");
        assert!(dry.contains("skipped: escape → /etc/passwd"), "{dry}");
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn t18_bounded_stream_enforces_byte_budget() {
        // /proc/self/status reports a stat size of 0 but has content: the
        // stat-based check passes, the stream bound must catch it.
        let src = PathBuf::from("/proc/self/status");
        if std::fs::metadata(&src).is_err() {
            return;
        }
        let dir = TempDir::new("t18");
        let mut o = opts(&dir);
        o.caps.max_bytes = 16;
        let dst = dir.0.join("status-copy");
        let out = copy_at(&src, &dst, &o).await;
        assert!(out.contains("more than 16 bytes"), "{out}");
        assert!(!dst.exists());
        assert!(leftovers(&dir.0, "embra-copy").is_empty());
        // Same bound on a dry run.
        o.dry_run = true;
        let out = copy_at(&src, &dst, &o).await;
        assert!(out.contains("more than 16 bytes"), "{out}");
    }

    #[tokio::test]
    async fn t19_free_space_reserve_refuses() {
        let dir = TempDir::new("t19");
        let src = dir.0.join("a.txt");
        write(&src, b"12345");
        let mut o = opts(&dir);
        o.caps.free_reserve = 100;
        o.available_override = Some(104);
        let out = copy_at(&src, &dir.0.join("b.txt"), &o).await;
        assert!(out.contains("needs 5 bytes but only 104 bytes are free"), "{out}");
        assert!(out.contains("keeps 100 bytes for WardSONDB"), "{out}");
        assert!(!dir.0.join("b.txt").exists());
        o.available_override = Some(105);
        let out = copy_at(&src, &dir.0.join("b.txt"), &o).await;
        assert!(out.starts_with("Copied "), "{out}");
    }

    #[tokio::test]
    async fn t20_vanished_source_file_rolls_back() {
        let dir = TempDir::new("t20");
        let root = dir.0.join("tree");
        make_tree(&root);
        let dst = dir.0.join("copy");
        let mut o = opts(&dir);
        o.recursive = true;
        let plan = plan_tree(&root, &dst, &o).await.unwrap();
        std::fs::create_dir(&dst).unwrap();
        std::fs::remove_file(root.join("sub/b.txt")).unwrap();
        let err = execute_tree(&root, &dst, &plan, &o).await.err().unwrap();
        assert!(err.contains("vanished during the copy"), "{err}");
        assert_eq!(RollbackGuard::wipe(&dst), None);
        assert!(!dst.exists());
    }

    // -- description / caps / registration -----------------------------------

    #[test]
    fn file_copy_description_steers_exact_path() {
        let desc = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "file_copy")
            .expect("file_copy registered")
            .description;
        assert!(desc.contains("exact final path"), "{desc}");
        assert!(desc.contains("never copies INTO a directory"), "{desc}");
        assert!(desc.contains("contents never enter the conversation"), "{desc}");
        assert!(desc.contains("recursive=true"), "{desc}");
        assert!(desc.contains("512 MiB"), "{desc}");
    }

    #[test]
    fn caps_pinned() {
        assert_eq!(FILE_COPY_MAX_BYTES, 512 * 1024 * 1024);
        assert_eq!(FILE_COPY_MAX_ENTRIES, 100_000);
        assert_eq!(FILE_COPY_FREE_RESERVE, 256 * 1024 * 1024);
        assert_eq!(ENUM_CAP, 10);
    }

    #[test]
    fn file_copy_registered_with_plain_object_schema() {
        let names: Vec<&'static str> = inventory::iter::<crate::tools::registry::ToolDescriptor>()
            .map(|d| d.name)
            .collect();
        assert!(names.contains(&"file_copy"), "file_copy registered");
        let schema = schemars::schema_for!(FileCopyArgs);
        let v = serde_json::to_value(&schema).unwrap();
        assert!(v.get("oneOf").is_none());
        assert!(v.get("allOf").is_none());
        assert!(v.get("anyOf").is_none());
        assert_eq!(v.get("type").and_then(|t| t.as_str()), Some("object"));
        let props = v.get("properties").unwrap();
        for f in ["source", "destination", "recursive", "overwrite", "dry_run", "exclude"] {
            assert!(props.get(f).is_some(), "schema lacks {f}");
        }
    }

    // -- pure core ------------------------------------------------------------

    #[test]
    fn u01_normalize_lexical_resolves_dotdot_and_clamps_at_root() {
        assert_eq!(normalize_lexical(Path::new("/a/b/../c/./d")), PathBuf::from("/a/c/d"));
        assert_eq!(normalize_lexical(Path::new("/a/../../..")), PathBuf::from("/"));
        assert_eq!(normalize_lexical(Path::new("/")), PathBuf::from("/"));
    }

    #[test]
    fn u02_symlink_target_relative_resolves_at_new_location() {
        let jail = Path::new("/embra/workspace");
        let link = Path::new("/embra/workspace/copy/sub/link");
        assert!(symlink_target_ok(link, Path::new("../a.txt"), jail).is_ok());
        assert!(symlink_target_ok(link, Path::new("../../x"), jail).is_ok()); // /embra/workspace/x
        let err = symlink_target_ok(link, Path::new("../../../state/key"), jail).unwrap_err();
        assert_eq!(err, PathBuf::from("/embra/state/key")); // one level too far
    }

    #[test]
    fn u03_symlink_target_absolute_inside_and_outside() {
        let jail = Path::new("/embra/workspace");
        let link = Path::new("/embra/workspace/copy/link");
        assert!(symlink_target_ok(link, Path::new("/embra/workspace/repos/x"), jail).is_ok());
        assert_eq!(
            symlink_target_ok(link, Path::new("/etc/passwd"), jail).unwrap_err(),
            PathBuf::from("/etc/passwd")
        );
    }

    #[test]
    fn u04_symlink_target_rejects_workspace_evil_prefix() {
        let jail = Path::new("/embra/workspace");
        let link = Path::new("/embra/workspace/copy/link");
        assert!(symlink_target_ok(link, Path::new("/embra/workspace-evil/x"), jail).is_err());
        assert!(!dest_inside_source(Path::new("/a/tree"), Path::new("/a/tree-other/x")));
        assert!(dest_inside_source(Path::new("/a/tree"), Path::new("/a/tree/x")));
    }

    #[test]
    fn u05_dangling_target_still_classified() {
        // No filesystem access: a target that does not exist anywhere is
        // still classified by where it WOULD land.
        let jail = Path::new("/embra/workspace");
        let link = Path::new("/embra/workspace/copy/link");
        assert!(symlink_target_ok(link, Path::new("does/not/exist"), jail).is_ok());
        assert!(symlink_target_ok(link, Path::new("/nonexistent/elsewhere"), jail).is_err());
    }

    #[tokio::test]
    async fn u07_plan_order_is_sorted_and_parent_before_child() {
        let dir = TempDir::new("u07");
        let root = dir.0.join("tree");
        write(&root.join("z/zz.txt"), b"1");
        write(&root.join("a/aa.txt"), b"22");
        write(&root.join("a/b/c.txt"), b"333");
        write(&root.join("m.txt"), b"4444");
        let mut o = opts(&dir);
        o.recursive = true;
        let plan = plan_tree(&root, &dir.0.join("out"), &o).await.unwrap();
        let files: Vec<String> = plan.files.iter().map(|(p, _)| p.display().to_string()).collect();
        assert_eq!(files, vec!["m.txt", "a/aa.txt", "a/b/c.txt", "z/zz.txt"]);
        let dirs: Vec<String> = plan.dirs.iter().map(|(p, _)| p.display().to_string()).collect();
        assert_eq!(dirs, vec!["", "a", "z", "a/b"]);
        assert_eq!(plan.total_bytes, 10);
        assert_eq!(plan.entries, 7);
    }

    #[tokio::test]
    async fn u08_plan_never_descends_into_symlinked_dir() {
        let dir = TempDir::new("u08");
        let root = dir.0.join("tree");
        write(&root.join("real/f.txt"), b"x");
        std::os::unix::fs::symlink("real", root.join("alias")).unwrap();
        // A cycle: real/loop -> ..
        std::os::unix::fs::symlink("..", root.join("real/loop")).unwrap();
        let mut o = opts(&dir);
        o.recursive = true;
        let plan = plan_tree(&root, &dir.0.join("out"), &o).await.unwrap();
        assert_eq!(plan.files.len(), 1);
        assert_eq!(plan.symlinks.len(), 2);
        assert_eq!(plan.dirs.len(), 2);
    }

    #[test]
    fn u09_render_report_shape_single_and_tree() {
        let s = render_file_report(
            Path::new("/s/a"), Path::new("/d/a"), 11, 0o644, "abcdef0123456789ffff", Some("abcdef0123456789ffff"), None, false,
        );
        assert_eq!(
            s,
            "Copied /s/a → /d/a: 11 bytes, mode 0644, sha256 abcdef0123456789 (source and destination match)"
        );
        let m = render_file_report(
            Path::new("/s/a"), Path::new("/d/a"), 11, 0o644, "aaaa", Some("bbbb"), Some(3), false,
        );
        assert!(m.contains("VERIFY FAILED"), "{m}");
        assert!(m.ends_with("— replaced existing 3 bytes"), "{m}");

        let mut plan = CopyPlan::default();
        plan.dirs.push((PathBuf::new(), 0o755));
        plan.files.push((PathBuf::from("a"), 5));
        plan.total_bytes = 5;
        let t = render_tree_report(Path::new("/s"), Path::new("/d"), &plan, Some(&Executed { copied_bytes: 4, mode_failures: 1 }), false);
        assert!(t.starts_with("Copied /s/ → /d/: 1 file, 1 directory, 0 symlinks, 4 bytes"), "{t}");
        assert!(t.contains("note: source changed during the copy — planned 5 bytes, copied 4 bytes"), "{t}");
        assert!(t.contains("note: could not apply the source mode on 1 directory"), "{t}");
    }

    #[test]
    fn u10_render_report_caps_lists_at_enum_cap() {
        let mut plan = CopyPlan::default();
        plan.dirs.push((PathBuf::new(), 0o755));
        for i in 0..15 {
            plan.excluded.push((PathBuf::from(format!("x{i}")), false));
            plan.skipped.push((PathBuf::from(format!("s{i}")), SkipReason::Special("fifo")));
        }
        let t = render_tree_report(Path::new("/s"), Path::new("/d"), &plan, None, true);
        assert!(t.contains("x9 (file) … and 5 more"), "{t}");
        assert!(t.contains("skipped: … and 5 more"), "{t}");
        assert_eq!(t.matches("skipped: s").count(), ENUM_CAP, "{t}");
    }

    #[test]
    fn u11_args_defaults_and_deny_unknown_fields() {
        let a: FileCopyArgs =
            serde_json::from_value(serde_json::json!({"source": "a", "destination": "b"})).unwrap();
        assert!(!a.recursive && !a.overwrite && !a.dry_run && a.exclude.is_none());
        let err = serde_json::from_value::<FileCopyArgs>(
            serde_json::json!({"source": "a", "destination": "b", "recursiv": true}),
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown field"), "{err}");
    }

    #[test]
    fn u12_sha256_known_vector_and_bounded_copy() {
        let dir = TempDir::new("u12");
        let src = dir.0.join("abc");
        write(&src, b"abc");
        assert_eq!(
            sha256_blocking(&src).unwrap(),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
        let tmp = dir.0.join("tmp");
        let (n, sha) = copy_bounded_blocking(&src, &tmp, 3).unwrap();
        assert_eq!(n, 3);
        assert!(sha.starts_with("ba7816bf"));
        let err = copy_bounded_blocking(&src, &dir.0.join("tmp2"), 2).unwrap_err();
        assert!(err.contains("more than 2 bytes"), "{err}");
    }

    #[test]
    fn u13_available_bytes_positive_for_temp_dir() {
        let dir = TempDir::new("u13");
        let avail = available_bytes(&dir.0).unwrap();
        assert!(avail > 0);
        assert!(available_bytes(&dir.0.join("missing")).is_err());
    }

    #[tokio::test]
    async fn u14_exclude_with_file_source_and_overwrite_with_tree_refuse() {
        let dir = TempDir::new("u14");
        let root = dir.0.join("tree");
        make_tree(&root);
        let mut o = opts(&dir);
        o.exclude = vec![".git".into()];
        let out = copy_at(&root.join("a.txt"), &dir.0.join("x"), &o).await;
        assert!(out.contains("exclude applies to directory copies only"), "{out}");

        let mut o = opts(&dir);
        o.recursive = true;
        o.overwrite = true;
        let out = copy_at(&root, &dir.0.join("y"), &o).await;
        assert!(out.contains("overwrite applies to single files only"), "{out}");
        assert!(!dir.0.join("y").exists());

        let mut o = opts(&dir);
        o.recursive = true;
        o.exclude = vec!["a/b".into()];
        let out = copy_at(&root, &dir.0.join("z"), &o).await;
        assert!(out.contains("exact basenames"), "{out}");
    }

    #[tokio::test]
    async fn u15_symlink_destination_resolved_never_replaced() {
        // file_patch §5 rule: overwriting through a symlink patches the
        // target and leaves the link a link.
        let dir = TempDir::new("u15");
        let src = dir.0.join("new.txt");
        write(&src, b"new");
        let real = dir.0.join("real.txt");
        write(&real, b"old");
        let link = dir.0.join("link.txt");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let mut o = opts(&dir);
        o.overwrite = true;
        let out = copy_at(&src, &link, &o).await;
        assert!(out.starts_with("Copied "), "{out}");
        assert!(std::fs::symlink_metadata(&link).unwrap().file_type().is_symlink());
        assert_eq!(std::fs::read(&real).unwrap(), b"new");

        // A dangling destination link is refused.
        let dangling = dir.0.join("dangling");
        std::os::unix::fs::symlink(dir.0.join("nowhere"), &dangling).unwrap();
        let out = copy_at(&src, &dangling, &o).await;
        assert!(out.contains("dangling symlink"), "{out}");
    }
}
