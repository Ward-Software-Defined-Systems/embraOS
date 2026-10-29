//! The file and directory tools: `file_read` and the writer family, each
//! writer through the two jail steps (`jail.rs`).

use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;

use embra_tool_macro::embra_tool;
use embra_tools_core::DispatchError;
use schemars::JsonSchema;
use serde::Deserialize;

use crate::tools::jail::{jailed_remove_path, jailed_write_path, WORKSPACE_ROOT};
use crate::tools::registry::DispatchContext;

/// Ceiling (and default) for a single file_read call: the dispatcher cap in
/// tools/registry.rs minus TOOL_RESULT_ENVELOPE, so header + content +
/// continuation trailer always fit under MAX_TOOL_RESULT_SIZE. Derived, not
/// duplicated — the two were EQUAL before the sprint-6 fix, so a
/// full-ceiling read of a larger file overflowed on its own framing and
/// apply_max_size amputated the continuation trailer (replacing it with the
/// generic `[truncated: N bytes total]` marker, whose N is the result
/// length, not the file size — actively misleading on big files).
const FILE_READ_MAX: usize =
    crate::tools::registry::MAX_TOOL_RESULT_SIZE - crate::tools::registry::TOOL_RESULT_ENVELOPE;

/// Entry cap for file_read's directory-listing branch. Raised from the
/// original inline 200 (sprint-6): ~1000 entries is ≈40 KB of listing, far
/// under the result cap. The truncation message renders this const so the
/// message and the value cannot drift.
const DIR_LIST_MAX: u32 = 1000;

/// Read a file's contents. An absolute path reaches anything on the system
/// but the places that hold the operator's secrets (`jail::read_denied`:
/// /proc, STATE except its drop-ins, the database files, the workspace SSH
/// key), checked on the real path. Relative paths resolve against
/// `/embra/workspace/` so the call shape matches the rest of the file_*
/// family post #53. Closes Embra_Debug #57.
///
/// `raw_path` is absolute (anywhere) or workspace-relative; `offset` is the
/// byte the read starts at and `limit` the most bytes it reads, at most
/// `FILE_READ_MAX` whatever the caller passes (`read_window` derives both
/// from the tool's optional fields). When the read doesn't reach EOF, a
/// trailer tells the model how to continue with the next slice.
pub async fn file_read(raw_path: &str, offset: u64, limit: usize) -> String {
    let raw_path = raw_path.trim();
    let limit = limit.min(FILE_READ_MAX);
    if raw_path.is_empty() {
        return "file_read rejected (path is empty)".into();
    }

    // The read jail: a relative path resolves under /embra/workspace, an
    // absolute path is read as given, and the real path is checked against
    // the places the model's readers do not reach (`jail::read_denied`).
    // From here on the path shown is the real one.
    let path = match super::jail::readable(raw_path).await {
        Ok(p) => p.display().to_string(),
        Err(e) => return e.to_string(),
    };
    let file_path = std::path::Path::new(&path);

    if file_path.is_dir() {
        // List directory contents instead — offset/limit are meaningless here.
        match tokio::fs::read_dir(&path).await {
            Ok(mut entries) => {
                let mut listing = format!("=== Directory: {} ===\n", path);
                let mut count = 0u32;
                while let Ok(Some(entry)) = entries.next_entry().await {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let is_dir = entry.file_type().await.map(|t| t.is_dir()).unwrap_or(false);
                    let suffix = if is_dir { "/" } else { "" };
                    listing.push_str(&format!("  {}{}\n", name, suffix));
                    count += 1;
                    if count >= DIR_LIST_MAX {
                        listing.push_str(&format!("  ... (truncated at {} entries)\n", DIR_LIST_MAX));
                        break;
                    }
                }
                if count == 0 {
                    listing.push_str("  (empty directory)\n");
                }
                return listing;
            }
            Err(e) => return format!("Failed to read directory: {}", e),
        }
    }

    // File branch — seek + read_exact the requested slice.
    let meta = match tokio::fs::metadata(&path).await {
        Ok(m) => m,
        Err(e) => return format!("Failed to stat file: {}", e),
    };
    let size = meta.len();
    if offset >= size && size > 0 {
        return format!(
            "Offset {} is at or past EOF ({} bytes)",
            offset, size
        );
    }
    let window_end = (offset + limit as u64).min(size);
    let read_bytes = (window_end - offset) as usize;

    use tokio::io::{AsyncReadExt, AsyncSeekExt, SeekFrom};
    let mut f = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return format!("Failed to open file: {}", e),
    };
    if let Err(e) = f.seek(SeekFrom::Start(offset)).await {
        return format!("Failed to seek file: {}", e);
    }
    let mut buf = vec![0u8; read_bytes];
    if let Err(e) = f.read_exact(&mut buf).await {
        return format!("Failed to read file: {}", e);
    }

    // Preserve the existing binary-file detection: a null byte in the first
    // 1KB strongly suggests non-text content.
    let head = &buf[..buf.len().min(1024)];
    if head.contains(&0u8) {
        return format!(
            "{} is a binary file ({} bytes); for images use image_view",
            path, size
        );
    }

    // A window that ends inside a character gives the model half of it
    // here and half in the next chunk, each as U+FFFD. End the window
    // before that character; the trailer names where it starts. A window
    // that is nothing but the cut character is left alone — shortening
    // it would read nothing.
    if window_end < size {
        let cut = cut_character_len(&buf);
        if cut < buf.len() {
            buf.truncate(buf.len() - cut);
        }
    }

    // The ceiling is on what is RETURNED. A byte that is not UTF-8 reads
    // as U+FFFD, three bytes for one, so a window of the full ceiling from
    // a file in another encoding decodes to three times the ceiling — and
    // what the dispatcher then cuts off is the trailer. The text is decoded
    // within the ceiling and the window ends where the text does.
    let decoded = decode_within(&buf, FILE_READ_MAX);
    let read_end = offset + decoded.raw_len as u64;
    let content = decoded.text;
    let encoding_note = if decoded.replaced > 0 {
        format!(
            "; not UTF-8: {} byte sequence(s) shown as U+FFFD, writing this text back would alter the file",
            decoded.replaced
        )
    } else {
        String::new()
    };
    let more = if read_end < size {
        // Speak the JSON-arg vocabulary the model actually calls with (the
        // old trailer taught the legacy pipe syntax). Omitting `limit` in the
        // suggested continuation is deliberate: the default is the full
        // per-call ceiling, so resumed reads take maximum-size chunks. When
        // the caller chose a smaller explicit limit, nudge once per result —
        // small-chunk habits burn turns and can hit the tool-iteration cap.
        let tip = if limit < FILE_READ_MAX {
            " (tip: omit limit to read up to the full per-call ceiling in one call)"
        } else {
            ""
        };
        format!(
            "\n[... {} more bytes. Continue with offset={}{} ]",
            size - read_end,
            read_end,
            tip
        )
    } else {
        String::new()
    };
    format!(
        "=== {} ({} bytes, showing {}..{}{}) ===\n{}{}",
        path, size, offset, read_end, encoding_note, content, more
    )
}

/// A byte window read as text.
struct Decoded {
    text: String,
    /// How many bytes of the window the text stands for.
    raw_len: usize,
    /// How many byte sequences were not UTF-8 and read as U+FFFD.
    replaced: usize,
}

/// Decode a window as `String::from_utf8_lossy` does, stopping before the
/// text would pass `budget` bytes. It stops between characters and never
/// inside an invalid sequence, so the windows of a chunked read decode to
/// what one decoding of the file gives. A budget of 4 or more always makes
/// progress on a window that is not empty.
fn decode_within(window: &[u8], budget: usize) -> Decoded {
    const REPLACEMENT: &str = "\u{FFFD}";
    let mut text = String::with_capacity(window.len().min(budget));
    let mut raw_len = 0;
    let mut replaced = 0;
    for chunk in window.utf8_chunks() {
        let valid = chunk.valid();
        let room = budget - text.len();
        if valid.len() > room {
            let mut end = room;
            while !valid.is_char_boundary(end) {
                end -= 1;
            }
            text.push_str(&valid[..end]);
            raw_len += end;
            break;
        }
        text.push_str(valid);
        raw_len += valid.len();

        let invalid = chunk.invalid();
        if invalid.is_empty() {
            continue;
        }
        if budget - text.len() < REPLACEMENT.len() {
            break;
        }
        text.push_str(REPLACEMENT);
        raw_len += invalid.len();
        replaced += 1;
    }
    Decoded { text, raw_len, replaced }
}

/// The length of the character a byte window cuts at its end: 0 when the
/// window ends on a character boundary, else the 1–3 bytes of the
/// character that does not fit. Bytes that are not UTF-8 there count as
/// a boundary.
fn cut_character_len(window: &[u8]) -> usize {
    // A cut character has its first byte among the last three.
    for back in 1..=window.len().min(3) {
        let need = match window[window.len() - back] {
            0x80..=0xBF => continue, // a continuation byte: look further back
            0xC0..=0xDF => 2,
            0xE0..=0xEF => 3,
            0xF0..=0xF7 => 4,
            _ => return 0,
        };
        return if back < need { back } else { 0 };
    }
    0
}

/// Ensure parent directories exist for a path. The caller has run the jail
/// (`jail::real`), so the parents are created under the canonical ancestor.
pub(crate) async fn ensure_parent_dirs(path: &Path) -> Result<(), String> {
    if let Some(parent) = path.parent()
        && !parent.exists()
    {
        tokio::fs::create_dir_all(parent)
            .await
            .map_err(|e| format!("Failed to create directory {}: {}", parent.display(), e))?;
    }
    Ok(())
}

/// The `file_write` tool (overwrite; workspace restricted) under a root:
/// the lexical jail, the canonical check of what exists, then [`write_at`]
/// on the real path. The content is written exactly as given; nothing is
/// expanded. Parent directories are created. Production passes
/// `WORKSPACE_ROOT`; the jail tests pass a temporary directory.
pub(crate) async fn file_write_in(root: &str, raw_path: &str, content: &str) -> String {
    let path = match jailed_write_path(root, raw_path).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    write_at(&path, content).await
}

/// Post-jail core of [`file_write`]: parent-dir creation + atomic
/// write-and-rename + report. Split out (mirroring file_patch's `patch_at`)
/// so positive-path tests can exercise real writes outside the workspace
/// jail. The atomic writer (file_patch's temp+fsync+rename discipline in its
/// create-capable form) closes the F-19 failure class: a failed write can no
/// longer leave a partial file — the target is either fully replaced or
/// byte-identical to before the call.
pub(crate) async fn write_at(path: &Path, content: &str) -> String {
    if let Err(e) = ensure_parent_dirs(path).await {
        return e;
    }

    match crate::tools::file_patch::write_atomic_create(path, content.as_bytes()).await {
        Ok(()) => {
            let line_count = content.lines().count();
            format!(
                "Written {} bytes ({} lines) to {}",
                content.len(),
                line_count,
                path.display()
            )
        }
        Err(e) => format!("Failed to write file: {} — target left unchanged", e),
    }
}

/// The `file_append` tool (workspace restricted) under a root: the lexical
/// jail, the canonical check of what exists, then [`append_at`] on the real
/// path. Creates the file (and parent directories) if it doesn't exist.
///
/// Not atomic, unlike `file_write` and `file_patch`: an append that fails
/// midway leaves what it had written.
pub(crate) async fn file_append_in(root: &str, raw_path: &str, content: &str) -> String {
    let path = match jailed_write_path(root, raw_path).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    append_at(&path, content).await
}

/// Post-jail core of [`file_append`]: parent-dir creation, then an
/// append-mode write. Not atomic.
pub(crate) async fn append_at(path: &Path, content: &str) -> String {
    if let Err(e) = ensure_parent_dirs(path).await {
        return e;
    }

    use tokio::io::AsyncWriteExt;
    let result = tokio::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .await;

    match result {
        Ok(mut file) => {
            // tokio's File hands a write to the blocking pool and returns;
            // `flush` waits for it, so the report is true when it is given.
            let written = match file.write_all(content.as_bytes()).await {
                Ok(()) => file.flush().await,
                Err(e) => Err(e),
            };
            match written {
                Ok(()) => format!("Appended {} bytes to {}", content.len(), path.display()),
                Err(e) => format!("Failed to append: {}", e),
            }
        }
        Err(e) => format!("Failed to open file for append: {}", e),
    }
}

/// The `mkdir` tool (a directory and its parents; workspace restricted)
/// under a root: the lexical jail, the canonical check of what exists, then
/// the directories under the real ancestor.
pub(crate) async fn mkdir_in(root: &str, raw_path: &str) -> String {
    let path = match jailed_write_path(root, raw_path).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    if tokio::fs::symlink_metadata(&path).await.is_ok() {
        return format!("Directory already exists: {}", path.display());
    }

    match tokio::fs::create_dir_all(&path).await {
        Ok(()) => format!("Created directory: {}", path.display()),
        Err(e) => format!("Failed to create directory: {}", e),
    }
}

/// The `file_symlink` tool under a root. Workspace restricted (both the
/// target and the link path must be under `/embra/workspace/`); dangling
/// targets are allowed — operators can create a forward-reference symlink
/// and populate the target afterwards. The target keeps the lexical check
/// alone (a dangling target is allowed, so there may be nothing to
/// resolve); the link path runs the canonical check, so a link is never
/// created under a directory that leaves the workspace.
pub(crate) async fn file_symlink_in(root: &str, raw_target: &str, raw_link: &str) -> String {
    let target = match super::jail::lexical(root, raw_target) {
        Ok(p) => p,
        Err(e) => return e,
    };
    let link = match jailed_write_path(root, raw_link).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    if tokio::fs::symlink_metadata(&link).await.is_ok() {
        return format!(
            "file_symlink rejected (link path already exists: {})",
            link.display()
        );
    }

    match tokio::fs::symlink(&target, &link).await {
        Ok(()) => format!("Symlink created: {} → {}", link.display(), target),
        Err(e) => format!("file_symlink failed: {}", e),
    }
}

/// The `file_delete` tool (a file or a symlink; workspace restricted) under
/// a root: the lexical jail, then the canonical check of the parent
/// (`jail::real_parent`), so the entry is unlinked by its name and a link is
/// never followed. Refuses real directories (`dir_delete` removes those).
/// Handles symlinks cleanly:
/// - Dangling symlinks (target missing): unlinked normally; `Path::exists()`
///   would have returned false and misled the old implementation into a
///   "File not found" error.
/// - Symlinks to directories: unlinked (only the link goes; the target
///   directory is untouched). `Path::is_dir()` follows symlinks and would
///   have refused these.
pub(crate) async fn file_delete_in(root: &str, raw_path: &str) -> String {
    let path = match jailed_remove_path(root, raw_path).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    let shown = path.display();

    // symlink_metadata does NOT follow symlinks — critical for correct handling
    // of dangling links and links pointing at directories.
    let meta = match tokio::fs::symlink_metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return format!("File not found: {shown}");
        }
        Err(e) => return format!("Failed to stat {shown}: {e}"),
    };

    if meta.file_type().is_symlink() {
        // Unlink the link itself. Target state is irrelevant.
        return match tokio::fs::remove_file(&path).await {
            Ok(()) => format!("Deleted symlink: {shown}"),
            Err(e) => format!("Failed to delete {shown}: {e}"),
        };
    }

    if meta.is_dir() {
        return format!(
            "Cannot delete directory with file_delete (use dir_delete for directories): {shown}"
        );
    }

    match tokio::fs::remove_file(&path).await {
        Ok(()) => format!("Deleted: {shown}"),
        Err(e) => format!("Failed to delete {shown}: {e}"),
    }
}

/// The `file_move` tool (move or rename a file or a directory; workspace
/// restricted on both ends) under a root. The source runs the canonical
/// check of its parent (a link is moved as a link, never followed); the
/// destination runs the canonical check of what exists (no parent is created
/// under a directory that leaves the workspace).
pub(crate) async fn file_move_in(root: &str, raw_src: &str, raw_dst: &str) -> String {
    let src = match jailed_remove_path(root, raw_src).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    let dst = match jailed_write_path(root, raw_dst).await {
        Ok(p) => p,
        Err(e) => return e,
    };

    if tokio::fs::symlink_metadata(&src).await.is_err() {
        return format!("Source not found: {}", src.display());
    }

    if tokio::fs::symlink_metadata(&dst).await.is_ok() {
        return format!("Destination already exists: {}", dst.display());
    }

    // Ensure parent directory of destination exists
    if let Err(e) = ensure_parent_dirs(&dst).await {
        return e;
    }

    match tokio::fs::rename(&src, &dst).await {
        Ok(()) => format!("Moved: {} → {}", src.display(), dst.display()),
        Err(e) => format!(
            "Failed to move {} → {}: {}",
            src.display(),
            dst.display(),
            e
        ),
    }
}

/// The `dir_delete` tool (workspace restricted; an empty directory, or any
/// directory with `force`) under a root: the lexical jail, the root refused
/// by name, then the canonical check of the parent (`jail::real_parent`),
/// so a symlink to a directory is not a directory here and nothing is
/// followed.
pub(crate) async fn dir_delete_in(root: &str, raw_path: &str, force: bool) -> String {
    let lexical = match super::jail::lexical(root, raw_path) {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = super::jail::fence_owned_dirs(Path::new(root), Path::new(&lexical)) {
        return e;
    }
    if Path::new(&lexical) == Path::new(root) {
        return format!(
            "Refusing to delete the workspace itself ({root}); name a directory under it."
        );
    }
    let (root_canon, path) = match super::jail::real_parent(Path::new(root), Path::new(&lexical)).await {
        Ok(p) => p,
        Err(e) => return e,
    };
    if let Err(e) = super::jail::fence_owned_dirs(&root_canon, &path) {
        return e;
    }
    let shown = path.display();

    let meta = match tokio::fs::symlink_metadata(&path).await {
        Ok(m) => m,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            return format!("Directory not found: {shown}");
        }
        Err(e) => return format!("Failed to stat {shown}: {e}"),
    };

    if !meta.is_dir() {
        return format!("Not a directory (use file_delete for files and links): {shown}");
    }

    if force {
        match tokio::fs::remove_dir_all(&path).await {
            Ok(()) => format!("Deleted directory and all contents: {shown}"),
            Err(e) => format!("Failed to delete directory {shown}: {e}"),
        }
    } else {
        match tokio::fs::remove_dir(&path).await {
            Ok(()) => format!("Deleted empty directory: {shown}"),
            Err(e) => {
                if e.kind() == std::io::ErrorKind::DirectoryNotEmpty
                    || e.to_string().contains("not empty")
                    || e.to_string().contains("Directory not empty")
                {
                    format!(
                        "Directory is not empty: {shown}. Use dir_delete {shown} --force to remove with contents."
                    )
                } else {
                    format!("Failed to delete directory {shown}: {e}")
                }
            }
        }
    }
}

// -- Filesystem tools ---------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_read",
    description = "Read a file or list a directory. A directory listing shows every entry, hidden ones (names starting with a dot) included. path may be absolute (`/etc`, `/var/log` and the service logs all work; refused by their real path: `/proc`, the operator's STATE partition except its drop-in directories, the database files and the workspace SSH key) or workspace-relative (resolves under /embra/workspace/, matching file_write / mkdir / etc.). Read files WHOLE by default: omit offset and limit — one call returns up to the per-call ceiling (~2 MiB), and a larger file ends with a continuation trailer naming the exact offset to resume from. Pass offset (byte position) and limit (byte count) only when resuming from that trailer or when you deliberately need a specific slice — reading in small chunks wastes turns."
)]
pub struct FileReadArgs {
    pub path: String,
    #[serde(default)]
    pub offset: Option<u64>,
    #[serde(default)]
    pub limit: Option<u64>,
}

/// The window of a read from the tool's optional fields: the start of the
/// file and the per-call ceiling when they are absent, and never past the
/// ceiling (`FILE_READ_MAX`), which binds the text returned.
fn read_window(offset: Option<u64>, limit: Option<u64>) -> (u64, usize) {
    let limit = limit
        .and_then(|l| usize::try_from(l).ok())
        .unwrap_or(FILE_READ_MAX)
        .min(FILE_READ_MAX);
    (offset.unwrap_or(0), limit)
}

impl FileReadArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let (offset, limit) = read_window(self.offset, self.limit);
        Ok(file_read(&self.path, offset, limit).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_write",
    is_side_effectful = true,
    description = "Write (overwrite) a file under /embra/workspace. path may be absolute (`/embra/workspace/repo/notes.txt`) or workspace-relative (`repo/notes.txt`). content is written exactly as given: a newline in it is a newline in the file, and nothing is expanded."
)]
pub struct FileWriteArgs {
    pub path: String,
    pub content: String,
}

impl FileWriteArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(file_write_in(WORKSPACE_ROOT, self.path.trim(), &self.content).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_append",
    is_side_effectful = true,
    description = "Append to a file under /embra/workspace (creates if missing). path may be absolute or workspace-relative. content is appended exactly as given; nothing is expanded."
)]
pub struct FileAppendArgs {
    pub path: String,
    pub content: String,
}

impl FileAppendArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(file_append_in(WORKSPACE_ROOT, self.path.trim(), &self.content).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_delete",
    is_side_effectful = true,
    description = "Delete a file under /embra/workspace (files only, not directories). path may be absolute or workspace-relative. Handles symlinks without following them."
)]
pub struct FileDeleteArgs {
    pub path: String,
}

impl FileDeleteArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(file_delete_in(WORKSPACE_ROOT, self.path.trim()).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_move",
    is_side_effectful = true,
    description = "Move or rename a file or directory under /embra/workspace. source and destination may each be absolute or workspace-relative."
)]
pub struct FileMoveArgs {
    pub source: String,
    pub destination: String,
}

impl FileMoveArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(file_move_in(WORKSPACE_ROOT, self.source.trim(), self.destination.trim()).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_rename",
    is_side_effectful = true,
    description = "Alias for file_move. Move or rename a file or directory under /embra/workspace. source and destination may each be absolute or workspace-relative."
)]
pub struct FileRenameArgs {
    pub source: String,
    pub destination: String,
}

impl FileRenameArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        FileMoveArgs {
            source: self.source,
            destination: self.destination,
        }
        .run(ctx)
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "file_symlink",
    is_side_effectful = true,
    description = "Create a symbolic link at link_path pointing to target. Both paths must resolve under /embra/workspace and may each be absolute or workspace-relative. Dangling targets allowed."
)]
pub struct FileSymlinkArgs {
    pub target: String,
    pub link_path: String,
}

impl FileSymlinkArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        let (target, link) = (self.target.trim(), self.link_path.trim());
        if target.is_empty() {
            return Ok("file_symlink rejected (target is empty)".into());
        }
        if link.is_empty() {
            return Ok("file_symlink rejected (link path is empty)".into());
        }
        Ok(file_symlink_in(WORKSPACE_ROOT, target, link).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "dir_delete",
    is_side_effectful = true,
    description = "Remove a directory under /embra/workspace. path may be absolute or workspace-relative. By default refuses non-empty directories; force=true recursively deletes contents. The workspace root itself is never deleted."
)]
pub struct DirDeleteArgs {
    pub path: String,
    #[serde(default)]
    pub force: bool,
}

impl DirDeleteArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(dir_delete_in(WORKSPACE_ROOT, self.path.trim(), self.force).await)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "rmdir",
    is_side_effectful = true,
    description = "Alias for dir_delete. Remove a directory under /embra/workspace; path may be absolute or workspace-relative. Set force=true to recursively delete contents."
)]
pub struct RmdirArgs {
    pub path: String,
    #[serde(default)]
    pub force: bool,
}

impl RmdirArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        DirDeleteArgs {
            path: self.path,
            force: self.force,
        }
        .run(ctx)
        .await
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "mkdir",
    is_side_effectful = true,
    description = "Create a directory (and parents as needed) under /embra/workspace. path may be absolute or workspace-relative."
)]
pub struct MkdirArgs {
    pub path: String,
}

impl MkdirArgs {
    pub async fn run(self, _ctx: DispatchContext<'_>) -> Result<String, DispatchError> {
        Ok(mkdir_in(WORKSPACE_ROOT, self.path.trim()).await)
    }
}

#[cfg(test)]
mod native_args_tests {
    use super::*;

    #[tokio::test]
    async fn file_family_rejects_traversal_uniformly() {
        // Embra_Debug #52/#53: the file_* and dir_* families now share the
        // same resolver as the git_* family, so traversal rejections carry
        // the uniform `'..'` message (from resolve_workspace_path) instead
        // of silently slipping past the old prefix-only check.
        let rejections = [
            file_write_in(WORKSPACE_ROOT, "../etc/passwd", "content").await,
            file_append_in(WORKSPACE_ROOT, "../etc/passwd", "content").await,
            file_delete_in(WORKSPACE_ROOT, "../etc/passwd").await,
            file_move_in(WORKSPACE_ROOT, "../src", "../dst").await,
            file_symlink_in(WORKSPACE_ROOT, "/embra/workspace/a", "../etc/foo").await,
            mkdir_in(WORKSPACE_ROOT, "../etc").await,
            dir_delete_in(WORKSPACE_ROOT, "../etc", false).await,
            crate::tools::file_copy::file_copy_impl(
                serde_json::from_value(serde_json::json!({
                    "source": "x", "destination": "../etc/evil"
                }))
                .unwrap(),
            )
            .await,
        ];
        for msg in rejections.iter() {
            assert!(
                msg.contains("'..'"),
                "expected uniform traversal rejection, got: {}",
                msg
            );
        }
    }

    #[tokio::test]
    async fn file_read_resolves_relative_to_workspace() {
        // Embra_Debug #57: relative paths under file_read now join to
        // /embra/workspace/ rather than the service cwd, matching the rest
        // of the file_* family. We exercise this by reading a guaranteed-
        // missing file and asserting the "File not found" message echoes
        // the resolved absolute path, not the raw input.
        let msg = file_read("nonexistent-zzz-embra-debug-57", 0, FILE_READ_MAX).await;
        assert!(
            msg.contains("/embra/workspace/nonexistent-zzz-embra-debug-57"),
            "expected resolved workspace path in rejection, got: {}",
            msg
        );
        assert!(
            !msg.starts_with("File not found: nonexistent-zzz"),
            "raw relative input should have been resolved before stat, got: {}",
            msg
        );

        // Absolute paths still pass through unchanged — the "unrestricted"
        // contract for reads outside /embra/workspace/ is preserved.
        let abs_msg = file_read("/definitely/not/a/real/path/abs-probe", 0, FILE_READ_MAX).await;
        assert!(
            abs_msg.contains("/definitely/not/a/real/path/abs-probe"),
            "absolute path should echo literally, got: {}",
            abs_msg
        );
        assert!(
            !abs_msg.contains("/embra/workspace/definitely"),
            "absolute path should NOT have been joined to workspace, got: {}",
            abs_msg
        );
    }

    #[tokio::test]
    async fn file_family_accepts_relative_workspace_paths() {
        // Relative inputs should join through resolve_workspace_path and
        // surface filesystem-level errors (not the old "is not under
        // /embra/workspace" rejection) when /embra/workspace/ is unavailable
        // on the test host. A filesystem error proves the resolver let the
        // path through and the kernel rejected it — the outcome we want.
        let msg = file_write_in(WORKSPACE_ROOT, "Embra_Debug/_relpath_probe.txt", "ok").await;
        assert!(
            !msg.contains("is not under"),
            "old validate_workspace_path rejection leaked back in, got: {}",
            msg
        );
    }

    #[test]
    fn file_read_optional_offset_limit() {
        let a: FileReadArgs =
            serde_json::from_value(serde_json::json!({"path": "/tmp/x"})).unwrap();
        assert!(a.offset.is_none());
        assert!(a.limit.is_none());

        let b: FileReadArgs = serde_json::from_value(serde_json::json!({
            "path": "/tmp/x", "offset": 100, "limit": 500
        }))
        .unwrap();
        assert_eq!(b.offset, Some(100));
        assert_eq!(b.limit, Some(500));
    }

    /// #37: the typed fields replace the `path|offset|limit` string the
    /// tool once rebuilt and re-split. An absent offset is the start of the
    /// file, an absent limit the per-call ceiling, and no limit passes it.
    #[test]
    fn the_read_window_defaults_to_the_start_and_the_ceiling() {
        assert_eq!(read_window(None, None), (0, FILE_READ_MAX));
        assert_eq!(read_window(Some(100), None), (100, FILE_READ_MAX));
        assert_eq!(read_window(None, Some(500)), (0, 500));
        assert_eq!(read_window(Some(7), Some(FILE_READ_MAX as u64 + 1)), (7, FILE_READ_MAX));
        assert_eq!(read_window(Some(0), Some(u64::MAX)), (0, FILE_READ_MAX));
    }

    #[test]
    fn dir_delete_force_default_false() {
        let a: DirDeleteArgs =
            serde_json::from_value(serde_json::json!({"path": "/tmp/x"})).unwrap();
        assert!(!a.force);

        let b: DirDeleteArgs = serde_json::from_value(serde_json::json!({
            "path": "/tmp/x", "force": true
        }))
        .unwrap();
        assert!(b.force);
    }
}

#[cfg(test)]
mod file_io_caps_tests {
    use super::*;
    use std::path::PathBuf;

    // Same TempDir guard as file_patch's test mod (positive paths can't
    // touch /embra/workspace on dev hosts; no tempfile crate in the
    // workspace).
    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "embra-file-io-caps-test-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            TempDir(d)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Newline-free deterministic ASCII so content can be sliced out of the
    /// framed result by line position.
    fn ascii_bytes(len: usize) -> Vec<u8> {
        (0..len).map(|i| b'a' + (i % 26) as u8).collect()
    }

    #[test]
    fn file_read_ceiling_leaves_envelope_headroom() {
        // The sprint-6 defect was equality between the two caps; the ceiling
        // is now DERIVED from the dispatcher cap minus the framing reserve,
        // so this is exact by construction — the test pins the relationship
        // against a future re-hardcode.
        assert_eq!(
            FILE_READ_MAX + crate::tools::registry::TOOL_RESULT_ENVELOPE,
            crate::tools::registry::MAX_TOOL_RESULT_SIZE
        );
    }

    #[tokio::test]
    async fn full_ceiling_read_trailer_survives() {
        // The end-to-end contract the sprint-6 fix restores: a default
        // (no-limit) read of a larger-than-ceiling file returns a framed
        // result that fits under the dispatcher cap — so apply_max_size
        // leaves it alone — with the continuation trailer intact, and the
        // trailer's suggested continuation reproduces the rest of the file.
        let dir = TempDir::new("trailer");
        let target = dir.0.join("big.txt");
        let total = FILE_READ_MAX + 100_000;
        let data = ascii_bytes(total);
        std::fs::write(&target, &data).unwrap();
        let path = target.to_str().unwrap().to_string();

        let out = file_read(&path, 0, FILE_READ_MAX).await;
        assert!(
            out.len() <= crate::tools::registry::MAX_TOOL_RESULT_SIZE,
            "framed result must fit under the dispatcher cap ({} > {})",
            out.len(),
            crate::tools::registry::MAX_TOOL_RESULT_SIZE
        );
        assert!(!out.contains("[truncated:"), "dispatcher-style marker must not appear");
        assert!(
            out.contains(&format!("Continue with offset={} ", FILE_READ_MAX)),
            "trailer must name the resume offset, got tail: {}",
            &out[out.len().saturating_sub(160)..]
        );
        // Default read = full ceiling = no small-chunk tip.
        assert!(!out.contains("tip: omit limit"), "no tip on a default read");

        // Content slice is byte-exact: header is line 1, content is line 2
        // (newline-free by construction), trailer starts at the next newline.
        let body = &out[out.find('\n').unwrap() + 1..];
        let content_end = body.find('\n').unwrap_or(body.len());
        assert_eq!(&body.as_bytes()[..content_end], &data[..FILE_READ_MAX]);

        // Follow the trailer verbatim: offset continuation returns the rest.
        let cont = file_read(&path, FILE_READ_MAX as u64, FILE_READ_MAX).await;
        assert!(!cont.contains("[... "), "final chunk must end clean, got: {}", &cont[cont.len().saturating_sub(160)..]);
        let cont_body = &cont[cont.find('\n').unwrap() + 1..];
        assert_eq!(cont_body.as_bytes(), &data[FILE_READ_MAX..]);
    }

    /// The content of a framed file_read result and, when the read did
    /// not reach the end, the offset its trailer names.
    fn body_and_next(out: &str) -> (&str, Option<u64>) {
        let body = &out[out.find('\n').expect("header line") + 1..];
        match body.rfind("\n[... ") {
            Some(at) => {
                let trailer = &body[at..];
                let from = trailer.find("offset=").expect("offset in trailer") + "offset=".len();
                let digits: String = trailer[from..]
                    .chars()
                    .take_while(char::is_ascii_digit)
                    .collect();
                (&body[..at], Some(digits.parse().expect("offset")))
            }
            None => (body, None),
        }
    }

    /// Follow the trailers from the start of the file to its end.
    async fn read_in_chunks(path: &str, limit: Option<usize>) -> (String, usize) {
        let mut whole = String::new();
        let mut offset = 0u64;
        let mut calls = 0;
        loop {
            let out = file_read(path, offset, limit.unwrap_or(FILE_READ_MAX)).await;
            calls += 1;
            let (body, next) = body_and_next(&out);
            whole.push_str(body);
            match next {
                Some(n) => {
                    assert!(n > offset, "no progress at offset {offset}: {out}");
                    offset = n;
                }
                None => return (whole, calls),
            }
        }
    }

    #[test]
    fn a_cut_character_is_measured_from_its_first_byte() {
        let text = "a\u{e9}\u{2014}\u{1f600}";
        let bytes = text.as_bytes();
        for end in 0..=bytes.len() {
            let window = &bytes[..end];
            let cut = cut_character_len(window);
            // What is left is whole characters, and nothing whole was cut.
            let kept = std::str::from_utf8(&window[..end - cut])
                .unwrap_or_else(|e| panic!("window of {end} bytes, cut {cut}: {e}"));
            assert_eq!(cut == 0, text.is_char_boundary(end), "window of {end} bytes");
            assert!(text.starts_with(kept));
            assert!(cut <= 3);
        }
        // Not UTF-8: nothing to respect, nothing cut.
        assert_eq!(cut_character_len(b"abc\xff"), 0);
        assert_eq!(cut_character_len(b"abc\x80\x80\x80"), 0);
        assert_eq!(cut_character_len(b""), 0);
        // A window that starts inside a character and ends inside the next.
        assert_eq!(cut_character_len(b"\xa9 \xe2\x80"), 2);
    }

    #[tokio::test]
    async fn a_chunk_never_ends_inside_a_character() {
        // The ceiling falls between the two bytes of a character. Cut
        // there, the chunk ends in U+FFFD and the next one starts with
        // one: the model has read a file that is not the file, and what
        // it writes back is corrupted.
        let dir = TempDir::new("charcut");
        let target = dir.0.join("big.txt");
        let mut data = ascii_bytes(FILE_READ_MAX - 1);
        data.extend_from_slice("\u{e9}".as_bytes());
        data.extend_from_slice(&ascii_bytes(1000));
        std::fs::write(&target, &data).unwrap();
        let path = target.to_str().unwrap();

        let first = file_read(path, 0, FILE_READ_MAX).await;
        assert!(first.len() <= crate::tools::registry::MAX_TOOL_RESULT_SIZE);
        let (body, next) = body_and_next(&first);
        assert_eq!(next, Some(FILE_READ_MAX as u64 - 1), "the chunk ends before the character");
        assert_eq!(body.as_bytes(), &data[..FILE_READ_MAX - 1]);
        assert!(
            first.contains(&format!("showing 0..{}) ===", FILE_READ_MAX - 1)),
            "the header names what was read"
        );

        let (whole, calls) = read_in_chunks(path, None).await;
        assert_eq!(calls, 2);
        assert_eq!(whole.as_bytes(), &data[..]);
    }

    #[tokio::test]
    async fn chunked_reads_of_any_size_give_the_file_back() {
        // 2, 3 and 4 bytes a character. A limit under 4 cannot hold every
        // character, so it cannot promise this.
        let text = "caf\u{e9} \u{2014} \u{6f22}\u{5b57} \u{1f600} end. ".repeat(3);
        let dir = TempDir::new("anysize");
        let target = dir.0.join("text.txt");
        std::fs::write(&target, &text).unwrap();
        let path = target.to_str().unwrap();

        for limit in 4..=text.len() {
            let (whole, _) = read_in_chunks(path, Some(limit)).await;
            assert_eq!(whole, text, "limit {limit}");
        }
    }

    #[test]
    fn text_is_decoded_within_its_budget() {
        let lossy = |b: &[u8]| String::from_utf8_lossy(b).into_owned();
        // With room for everything it is `from_utf8_lossy`.
        let mixed = b"caf\xc3\xa9 \xff \xe2\x82 \xf0\x9f\x98\x80 end";
        let all = decode_within(mixed, 1024);
        assert_eq!(all.text, lossy(mixed));
        assert_eq!(all.raw_len, mixed.len());
        assert_eq!(all.replaced, 2);

        // Under every budget: the text fits, it is a prefix of the whole,
        // it stands for exactly the bytes it says, and it grows with the
        // budget.
        let mut last = 0;
        for budget in 4..=all.text.len() + 2 {
            let d = decode_within(mixed, budget);
            assert!(d.text.len() <= budget, "budget {budget}");
            assert!(all.text.starts_with(&d.text), "budget {budget}");
            assert_eq!(d.text, lossy(&mixed[..d.raw_len]), "budget {budget}");
            assert!(d.raw_len > 0 && d.raw_len >= last, "budget {budget}");
            last = d.raw_len;
        }
        assert_eq!(last, mixed.len());

        // Three bytes of text for one of file: a third of the budget.
        let latin1 = vec![0xE9u8; 100];
        let d = decode_within(&latin1, 30);
        assert_eq!((d.text.len(), d.raw_len, d.replaced), (30, 10, 10));
        let d = decode_within(&latin1, 31);
        assert_eq!((d.text.len(), d.raw_len, d.replaced), (30, 10, 10));
        // Nothing to read is nothing read.
        let d = decode_within(b"", 4);
        assert_eq!((d.text.as_str(), d.raw_len, d.replaced), ("", 0, 0));
    }

    #[tokio::test]
    async fn a_file_in_another_encoding_keeps_its_trailer() {
        // Latin-1 text: every byte is a character there and none of them
        // is UTF-8. Each reads as U+FFFD - three bytes for one - so a
        // window of the full ceiling decodes to three times the ceiling,
        // the dispatcher cuts it, and the trailer that says where to go on
        // is what gets cut.
        let dir = TempDir::new("latin1");
        let target = dir.0.join("latin1.txt");
        let data = vec![0xE9u8; FILE_READ_MAX + 1000];
        std::fs::write(&target, &data).unwrap();
        let path = target.to_str().unwrap();

        let first = file_read(path, 0, FILE_READ_MAX).await;
        assert!(
            first.len() <= crate::tools::registry::MAX_TOOL_RESULT_SIZE,
            "the framed result must fit under the dispatcher cap ({} > {})",
            first.len(),
            crate::tools::registry::MAX_TOOL_RESULT_SIZE
        );
        let (body, next) = body_and_next(&first);
        let next = next.expect("a trailer names where to go on");
        // One U+FFFD for every byte that was read, and the trailer resumes
        // at the first byte that was not.
        assert!(body.chars().all(|c| c == '\u{fffd}'));
        assert_eq!(body.chars().count() as u64, next);
        assert!(body.len() <= FILE_READ_MAX);
        // The reader is told what it is looking at.
        let header = first.lines().next().unwrap();
        assert!(header.contains(&format!("showing 0..{next}")), "{header}");
        assert!(header.contains("not UTF-8"), "{header}");

        let (whole, calls) = read_in_chunks(path, None).await;
        assert_eq!(whole.chars().count(), data.len(), "every byte read once");
        assert!(whole.chars().all(|c| c == '\u{fffd}'));
        assert_eq!(calls, 4);
    }

    #[tokio::test]
    async fn chunked_reads_decode_like_one_read() {
        // Text with stray bytes in it: invalid sequences of every length
        // between characters of every length. Wherever the chunks end,
        // the text is what one lossy decoding of the file gives.
        let mut data = Vec::new();
        for i in 0..200 {
            data.extend_from_slice("caf\u{e9} \u{2014} \u{1f600} ".as_bytes());
            data.extend_from_slice(match i % 5 {
                0 => b"\xff".as_slice(),
                1 => b"\xe2\x82".as_slice(),
                2 => b"\xf0\x9f\x98".as_slice(),
                3 => b"\x80\x80".as_slice(),
                _ => b"\xc3".as_slice(),
            });
            data.push(b'\n');
        }
        let want = String::from_utf8_lossy(&data).into_owned();
        assert!(want.contains('\u{fffd}'));
        let dir = TempDir::new("stray");
        let target = dir.0.join("stray.txt");
        std::fs::write(&target, &data).unwrap();
        let path = target.to_str().unwrap();

        for limit in [4usize, 5, 6, 7, 16, 61, 1000, data.len()] {
            let (whole, _) = read_in_chunks(path, Some(limit)).await;
            assert_eq!(whole, want, "limit {limit}");
        }
        // A file that IS UTF-8 says nothing about encodings.
        let clean = dir.0.join("clean.txt");
        std::fs::write(&clean, "caf\u{e9}\n").unwrap();
        let out = file_read(clean.to_str().unwrap(), 0, FILE_READ_MAX).await;
        assert!(!out.contains("UTF-8"), "{out}");
    }

    #[tokio::test]
    async fn full_read_just_under_cap_no_trailer_no_truncation() {
        // The band that silently corrupted before the fix: a file of exactly
        // the old ceiling overflowed the dispatcher cap on its header alone.
        let dir = TempDir::new("underband");
        let target = dir.0.join("edge.txt");
        let data = ascii_bytes(FILE_READ_MAX);
        std::fs::write(&target, &data).unwrap();

        let out = file_read(target.to_str().unwrap(), 0, FILE_READ_MAX).await;
        assert!(out.len() <= crate::tools::registry::MAX_TOOL_RESULT_SIZE);
        assert!(!out.contains("[truncated:"));
        assert!(!out.contains("[... "), "complete read must carry no trailer");
        let body = &out[out.find('\n').unwrap() + 1..];
        assert_eq!(body.as_bytes(), &data[..]);
    }

    #[tokio::test]
    async fn small_limit_read_gets_omit_limit_tip() {
        // The anti-chunking nudge fires exactly when the habit occurs: an
        // explicit limit below the ceiling that doesn't reach EOF.
        let dir = TempDir::new("tip");
        let target = dir.0.join("chunky.txt");
        std::fs::write(&target, ascii_bytes(10_000)).unwrap();
        let path = target.to_str().unwrap();

        let out = file_read(path, 0, 1000).await;
        assert!(out.contains("Continue with offset=1000"), "{out}");
        assert!(out.contains("tip: omit limit"), "{out}");

        // A complete small read carries neither trailer nor tip.
        let full = file_read(path, 0, FILE_READ_MAX).await;
        assert!(!full.contains("[... "), "{full}");
        assert!(!full.contains("tip:"), "{full}");
    }

    #[tokio::test]
    async fn dir_listing_caps_at_const() {
        let dir = TempDir::new("dircap");
        for i in 0..(DIR_LIST_MAX + 5) {
            std::fs::write(dir.0.join(format!("f{:05}", i)), b"x").unwrap();
        }
        let out = file_read(dir.0.to_str().unwrap(), 0, FILE_READ_MAX).await;
        assert!(
            out.contains(&format!("truncated at {} entries", DIR_LIST_MAX)),
            "truncation line must render the const, got tail: {}",
            &out[out.len().saturating_sub(120)..]
        );
    }

    /// A directory listing shows every entry, hidden ones included. It is the
    /// intelligence's only way to list a directory, and an operator check can
    /// depend on it: no Buildroot stamp file may be left at the top of
    /// `/opt/rust`. A filter like `ls`'s default would hide exactly those.
    #[tokio::test]
    async fn a_directory_listing_shows_hidden_entries() {
        let dir = TempDir::new("hidden");
        std::fs::create_dir(dir.0.join("bin")).unwrap();
        std::fs::create_dir(dir.0.join(".git")).unwrap();
        std::fs::write(dir.0.join("RUST_VERSION"), b"1.98.1").unwrap();
        std::fs::write(dir.0.join(".stamp_built"), b"").unwrap();
        let out = file_read(dir.0.to_str().unwrap(), 0, FILE_READ_MAX).await;
        for line in ["  bin/\n", "  .git/\n", "  RUST_VERSION\n", "  .stamp_built\n"] {
            assert!(out.contains(line), "{line:?} missing from:\n{out}");
        }
    }

    /// The model is told so. Without it, a model reasons from `ls` and takes
    /// a listing without dot-names to prove nothing.
    #[test]
    fn file_read_description_says_listings_show_hidden_entries() {
        let desc = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "file_read")
            .expect("file_read registered")
            .description;
        assert!(desc.contains("hidden ones (names starting with a dot) included"), "{desc}");
    }

    #[test]
    fn file_read_description_steers_whole_reads() {
        // Guard for the sprint-6 anti-chunking steer (precedent: guardian's
        // propose_description_embeds_the_validated_template) — the model must
        // keep being told to read whole and to chunk only from the trailer.
        let desc = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "file_read")
            .expect("file_read registered")
            .description;
        assert!(desc.contains("Read files WHOLE by default"), "{desc}");
        assert!(desc.contains("continuation trailer"), "{desc}");
    }

    #[tokio::test]
    async fn write_at_reports_bytes_and_lines() {
        let dir = TempDir::new("writeat");
        let target = dir.0.join("nested").join("notes.txt");
        let path = target.to_str().unwrap();

        let out = write_at(&target, "line 1\nline 2").await;
        assert_eq!(out, format!("Written 13 bytes (2 lines) to {}", path));
        assert_eq!(std::fs::read(&target).unwrap(), b"line 1\nline 2");
    }

    #[tokio::test]
    async fn write_at_overwrite_replaces_content() {
        let dir = TempDir::new("overwrite");
        let target = dir.0.join("f.txt");
        std::fs::write(&target, "the old content is much longer than the new").unwrap();

        let out = write_at(&target, "short").await;
        assert!(out.starts_with("Written 5 bytes"), "{out}");
        assert_eq!(std::fs::read(&target).unwrap(), b"short");
    }
}

#[cfg(test)]
mod write_jail_tests {
    //! The canonical half of the writer jail (`jail::real`, `jail::real_parent`)
    //! through the tools themselves, under a temporary root (#53 b3, #28 b2).
    use super::*;

    struct TempDir(PathBuf);
    impl TempDir {
        fn new(tag: &str) -> Self {
            static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
            let d = std::env::temp_dir().join(format!(
                "embra-write-jail-test-{}-{}-{}",
                tag,
                std::process::id(),
                SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&d).unwrap();
            TempDir(std::fs::canonicalize(&d).unwrap())
        }
        fn root(&self) -> &str {
            self.0.to_str().unwrap()
        }
        fn join(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn link(target: &Path, at: &Path) {
        std::os::unix::fs::symlink(target, at).unwrap();
    }

    fn is_link(p: &Path) -> bool {
        std::fs::symlink_metadata(p).map(|m| m.file_type().is_symlink()).unwrap_or(false)
    }

    #[tokio::test]
    async fn a_literal_backslash_n_in_content_lands_verbatim() {
        // The dispatcher is JSON-only and JSON carries real newlines; a
        // `\n` inside code the model writes is two characters, not a line
        // break (#28 b3).
        let t = TempDir::new("verbatim");
        let out = file_write_in(t.root(), "code.rs", r#"println!("a\nb");"#).await;
        assert!(out.starts_with("Written 17 bytes (1 lines)"), "{out}");
        assert_eq!(std::fs::read(t.join("code.rs")).unwrap(), br#"println!("a\nb");"#);
        let out = file_append_in(t.root(), "code.rs", r"\t").await;
        assert!(out.starts_with("Appended 2 bytes"), "{out}");
        assert_eq!(std::fs::read(t.join("code.rs")).unwrap(), br#"println!("a\nb");\t"#);
    }

    /// Nothing under a `.git` component is written, appended, created,
    /// linked, moved into or removed, by its own path or through a link that
    /// leads there; the repository itself can still be removed (#6).
    #[tokio::test]
    async fn a_writer_never_reaches_a_git_directory_even_through_a_link() {
        let t = TempDir::new("git-fence");
        std::fs::create_dir_all(t.join("repo/.git/hooks")).unwrap();
        std::fs::write(t.join("repo/.git/config"), "[core]\n").unwrap();
        std::fs::write(t.join("repo/note.txt"), "x").unwrap();
        link(&t.join("repo/.git/hooks"), &t.join("repo/hooks-link"));

        let refused = [
            file_write_in(t.root(), "repo/.git/hooks/pre-commit", "pwned").await,
            file_write_in(t.root(), "repo/hooks-link/pre-commit", "pwned").await,
            file_append_in(t.root(), "repo/.git/config", "[x]").await,
            mkdir_in(t.root(), "repo/.git/x").await,
            file_symlink_in(t.root(), "repo/note.txt", "repo/.git/hooks/post-commit").await,
            file_move_in(t.root(), "repo/note.txt", "repo/.git/hooks/pre-push").await,
            file_delete_in(t.root(), "repo/.git/config").await,
            dir_delete_in(t.root(), "repo/.git/hooks", true).await,
        ];
        for out in &refused {
            assert!(out.starts_with("Denied:") && out.contains(".git directory"), "{out}");
        }
        assert_eq!(std::fs::read_dir(t.join("repo/.git/hooks")).unwrap().count(), 0, "no hook");
        assert_eq!(std::fs::read(t.join("repo/.git/config")).unwrap(), b"[core]\n");
        assert!(t.join("repo/note.txt").exists());

        // The repository as a whole is the writer's to remove.
        let out = dir_delete_in(t.root(), "repo", true).await;
        assert!(!out.starts_with("Denied:"), "{out}");
        assert!(!t.join("repo").exists());
    }

    /// Nothing under the workspace `.ssh` directory is written, appended,
    /// created, linked, moved into or removed, by its own path or through a
    /// link that leads there; a `.ssh` inside a repository is an ordinary
    /// directory (William's live check of 2026-10-23).
    #[tokio::test]
    async fn a_writer_never_reaches_the_ssh_directory_even_through_a_link() {
        let t = TempDir::new("ssh-fence");
        std::fs::create_dir_all(t.join(".ssh")).unwrap();
        std::fs::write(t.join(".ssh/known_hosts"), "h ssh-ed25519 AAAA\n").unwrap();
        std::fs::write(t.join("note.txt"), "x").unwrap();
        link(&t.join(".ssh"), &t.join("ssh-link"));

        let refused = [
            file_write_in(t.root(), ".ssh/config", "Host *\n").await,
            file_write_in(t.root(), "ssh-link/config", "Host *\n").await,
            file_append_in(t.root(), ".ssh/known_hosts", "x\n").await,
            mkdir_in(t.root(), ".ssh/x").await,
            file_symlink_in(t.root(), "note.txt", ".ssh/authorized_keys").await,
            file_move_in(t.root(), "note.txt", ".ssh/rc").await,
            file_delete_in(t.root(), ".ssh/known_hosts").await,
            dir_delete_in(t.root(), ".ssh", true).await,
        ];
        for out in &refused {
            assert!(out.starts_with("Denied:") && out.contains("SSH directory"), "{out}");
        }
        assert_eq!(std::fs::read_dir(t.join(".ssh")).unwrap().count(), 1, "known_hosts alone");
        assert_eq!(std::fs::read(t.join(".ssh/known_hosts")).unwrap(), b"h ssh-ed25519 AAAA\n");
        assert!(t.join("note.txt").exists());

        let out = file_write_in(t.root(), "repo/.ssh/config", "Host example\n").await;
        assert!(!out.starts_with("Denied:"), "{out}");
        assert!(t.join("repo/.ssh/config").exists());
    }

    #[tokio::test]
    async fn an_in_jail_symlink_to_the_outside_is_not_written_through() {
        let t = TempDir::new("escape");
        let outside = TempDir::new("escape-outside");
        let sentinel = outside.join("sentinel.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        link(&outside.0, &t.join("esc"));
        link(&sentinel, &t.join("esc_file"));
        std::fs::write(t.join("inside.txt"), "x").unwrap();

        let refused = [
            file_write_in(t.root(), "esc_file", "pwned").await,
            file_write_in(t.root(), "esc/new.txt", "pwned").await,
            file_append_in(t.root(), "esc_file", "pwned").await,
            file_append_in(t.root(), "esc/log.txt", "pwned").await,
            mkdir_in(t.root(), "esc/sub/dir").await,
            file_symlink_in(t.root(), "inside.txt", "esc/link").await,
            file_move_in(t.root(), "inside.txt", "esc/moved.txt").await,
        ];
        for out in &refused {
            assert!(
                out.starts_with("Denied:") && out.contains("outside"),
                "{out}"
            );
        }

        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep", "the sentinel is byte-identical");
        let created: Vec<_> = std::fs::read_dir(&outside.0)
            .unwrap()
            .flatten()
            .map(|e| e.file_name())
            .collect();
        assert_eq!(created.len(), 1, "nothing created outside: {created:?}");
        assert!(t.join("inside.txt").exists(), "the source of the refused move stays");
    }

    #[tokio::test]
    async fn a_link_inside_the_jail_is_written_through_and_kept() {
        let t = TempDir::new("inside");
        std::fs::write(t.join("real.txt"), "old").unwrap();
        link(&t.join("real.txt"), &t.join("link.txt"));
        std::fs::create_dir(t.join("realdir")).unwrap();
        link(&t.join("realdir"), &t.join("dirlink"));

        let out = file_write_in(t.root(), "link.txt", "new").await;
        assert!(out.starts_with("Written 3 bytes"), "{out}");
        assert_eq!(std::fs::read(t.join("real.txt")).unwrap(), b"new");
        assert!(is_link(&t.join("link.txt")), "the link stays a link");

        let out = file_append_in(t.root(), "link.txt", "er").await;
        assert!(out.starts_with("Appended 2 bytes"), "{out}");
        assert_eq!(std::fs::read(t.join("real.txt")).unwrap(), b"newer");

        let out = mkdir_in(t.root(), "dirlink/sub").await;
        assert!(out.starts_with("Created directory"), "{out}");
        assert!(t.join("realdir/sub").is_dir());

        let out = file_write_in(t.root(), "dirlink/sub/f.txt", "x").await;
        assert!(out.starts_with("Written"), "{out}");
        assert_eq!(std::fs::read(t.join("realdir/sub/f.txt")).unwrap(), b"x");
    }

    #[tokio::test]
    async fn a_delete_or_a_move_names_a_link_and_never_follows_it() {
        let t = TempDir::new("unlink");
        let outside = TempDir::new("unlink-outside");
        let sentinel = outside.join("sentinel.txt");
        std::fs::write(&sentinel, "keep").unwrap();
        link(&sentinel, &t.join("link"));
        link(&outside.0, &t.join("dirlink"));

        let out = file_delete_in(t.root(), "link").await;
        assert!(out.starts_with("Deleted symlink"), "{out}");
        assert!(std::fs::symlink_metadata(t.join("link")).is_err());
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");

        // A link to a directory is not a directory for dir_delete ...
        let out = dir_delete_in(t.root(), "dirlink", true).await;
        assert!(out.starts_with("Not a directory"), "{out}");
        assert!(outside.0.is_dir() && sentinel.exists(), "the outside tree stands");
        // ... and file_delete removes the link itself.
        let out = file_delete_in(t.root(), "dirlink").await;
        assert!(out.starts_with("Deleted symlink"), "{out}");
        assert!(sentinel.exists());

        // A move of a link moves the link.
        link(&sentinel, &t.join("link2"));
        let out = file_move_in(t.root(), "link2", "moved").await;
        assert!(out.starts_with("Moved"), "{out}");
        assert!(is_link(&t.join("moved")));
        assert_eq!(std::fs::read(&sentinel).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn dir_delete_never_takes_the_workspace_root() {
        let t = TempDir::new("root");
        std::fs::write(t.join("keep.txt"), "x").unwrap();
        for (path, force) in [("", true), ("", false), (".", true), ("./", false), (t.root(), true)] {
            let out = dir_delete_in(t.root(), path, force).await;
            assert!(
                out.starts_with("Refusing to delete the workspace itself"),
                "{path:?} force={force}: {out}"
            );
        }
        assert!(t.join("keep.txt").exists());
        // A path that leaves is refused by the lexical jail.
        let out = dir_delete_in(t.root(), "/", true).await;
        assert!(out.starts_with("Denied:"), "{out}");
    }
}
