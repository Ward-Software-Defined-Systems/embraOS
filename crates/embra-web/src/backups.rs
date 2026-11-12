//! The OS's backups (`embra-update`) through the web console, behind the
//! console's token like every route.
//!
//! `GET /api/backups/{name}` is a backup the OS took, as a browser download,
//! streamed from DATA. `PUT /api/backups/{name}` is the way back: the
//! archive of a backup, streamed to the updater's `import`, which checks it
//! while it writes it under the backups directory and gives it the record a
//! restore asks for. `GET /api/backups/{name}/upload-check` says beforehand
//! what such a `PUT` would be told.
//!
//! An archive carries STATE's secrets, so it lives outside the workspace the
//! model's tools reach and passes through here alone. The routes read and
//! write the directory themselves and reach no service.

use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use axum::Json;
use axum::body::{Body, Bytes};
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use futures_util::{StreamExt, stream};
use serde_json::{Value, json};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::media::json_error;
use crate::state::AppState;

/// Where the OS keeps its backups.
pub const BACKUPS_ROOT: &str = "/embra/data/backups";
/// The archive and its record inside an entry.
pub const ARCHIVE: &str = "backup.tar.gz";
pub const RECORD: &str = "backup.json";
/// An entry that is not finished: what a crashed snapshot or upload leaves.
pub const PARTIAL_SUFFIX: &str = ".partial";
/// The OS's updater. It owns the format of a backup; an upload is handed to
/// its `import`.
pub const UPDATER: &str = "/usr/bin/embra-update";
/// What an upload leaves free on DATA beside the archive: the updater's
/// `RESERVE_BYTES`, which its `import` holds the writing to as well.
pub const RESERVE_BYTES: u64 = 256 * 1024 * 1024;
/// How long the updater has to finish once the upload's last byte was handed
/// to it: the archive's fsync, the record, the rename.
const FINISH_TIMEOUT: Duration = Duration::from_secs(600);
const CHUNK: usize = 64 * 1024;

/// A backup's name: one path component of letters, digits, `_`, `.` and
/// `-`, starting with a letter or digit, at most 80 characters: never a
/// slash, never `..`, never a hidden entry.
pub fn valid_backup_name(name: &str) -> bool {
    let mut chars = name.chars();
    match chars.next() {
        Some(c) if c.is_ascii_alphanumeric() => {}
        _ => return false,
    }
    name.len() <= 80 && chars.all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// The archive to serve, when the entry is complete (its record beside it).
pub fn archive_path(root: &Path, name: &str) -> Option<PathBuf> {
    if !valid_backup_name(name) {
        return None;
    }
    let dir = root.join(name);
    if !dir.join(RECORD).is_file() {
        return None;
    }
    let archive = dir.join(ARCHIVE);
    archive.is_file().then_some(archive)
}

pub async fn api_backup_get(State(st): State<AppState>, UrlPath(name): UrlPath<String>) -> Response {
    if !valid_backup_name(&name) {
        return json_error(StatusCode::BAD_REQUEST, format!("invalid backup name '{name}'"));
    }
    let Some(path) = archive_path(&st.backups_root, &name) else {
        return json_error(StatusCode::NOT_FOUND, format!("no complete backup named '{name}'"));
    };
    let file = match tokio::fs::File::open(&path).await {
        Ok(f) => f,
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("open {}: {e}", path.display())),
    };
    let len = match file.metadata().await {
        Ok(m) => m.len(),
        Err(e) => return json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("stat {}: {e}", path.display())),
    };
    let body = Body::from_stream(stream::try_unfold(file, |mut f| async move {
        let mut buf = vec![0u8; CHUNK];
        let n = f.read(&mut buf).await?;
        if n == 0 {
            Ok::<_, std::io::Error>(None)
        } else {
            buf.truncate(n);
            Ok(Some((Bytes::from(buf), f)))
        }
    }));
    let mut resp = (StatusCode::OK, body).into_response();
    download_headers(resp.headers_mut(), &name, len);
    resp
}

/// The headers of a backup download (unit-tested): an attachment named
/// `<name>.tar.gz`, octet-stream, nosniff, no-store, its length.
pub fn download_headers(h: &mut axum::http::HeaderMap, name: &str, len: u64) {
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("application/octet-stream"));
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    if let Ok(v) = HeaderValue::from_str(&len.to_string()) {
        h.insert(header::CONTENT_LENGTH, v);
    }
    if let Ok(v) = HeaderValue::from_str(&format!("attachment; filename=\"{name}.tar.gz\"")) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
}

// ── The way back: an upload ─────────────────────────────────────────────

/// An upload refused before a byte of it is taken.
#[derive(Debug, PartialEq, Eq)]
pub struct Refusal {
    pub status: StatusCode,
    pub error: String,
}

/// Why an upload of `bytes` under `name` is refused before it starts, if it
/// is: the name, an entry of that name, the size, the room. `free` is what
/// DATA has. The updater's `import` holds the same name rule and the same
/// reserve; what it finds in the archive comes after the last byte.
pub fn upload_refusal(root: &Path, name: &str, bytes: Option<u64>, free: u64) -> Option<Refusal> {
    let refuse = |status, error: String| Some(Refusal { status, error });
    if !valid_backup_name(name) || name.ends_with(PARTIAL_SUFFIX) {
        return refuse(
            StatusCode::BAD_REQUEST,
            format!("not a name for a backup: {name:?} (letters, digits, _ . -; a letter or digit first; 80 at most; not ending in {PARTIAL_SUFFIX})"),
        );
    }
    if root.join(name).exists() {
        return refuse(
            StatusCode::CONFLICT,
            format!("a backup named \"{name}\" exists already; delete it first (/backup delete {name}) or upload the file under another name"),
        );
    }
    let Some(bytes) = bytes else {
        return refuse(
            StatusCode::LENGTH_REQUIRED,
            "the upload has to say its size (a Content-Length header; a browser and curl -T <file> send one)".to_string(),
        );
    };
    if bytes == 0 {
        return refuse(StatusCode::BAD_REQUEST, "the upload is empty".to_string());
    }
    if bytes.checked_add(RESERVE_BYTES).is_none_or(|needed| free < needed) {
        return refuse(
            StatusCode::INSUFFICIENT_STORAGE,
            format!(
                "DATA has {} MB free; the upload is {} MB and leaves a reserve of {} MB. Delete a backup (/backup delete <name>) and upload again",
                free / (1024 * 1024),
                bytes.div_ceil(1024 * 1024),
                RESERVE_BYTES / (1024 * 1024)
            ),
        );
    }
    None
}

/// Bytes free for a writer on the filesystem that holds `path`, read at its
/// nearest directory that exists: a machine that never took a backup has no
/// backups directory yet.
pub fn free_bytes(path: &Path) -> std::io::Result<u64> {
    use std::os::unix::ffi::OsStrExt;
    let mut at = path;
    while !at.exists() {
        at = at.parent().ok_or_else(|| std::io::Error::new(std::io::ErrorKind::NotFound, "no directory to measure"))?;
    }
    let c = std::ffi::CString::new(at.as_os_str().as_bytes())
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "path with a NUL byte"))?;
    // SAFETY: a zeroed statvfs struct the call fills; the path is NUL-terminated.
    let mut st: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(c.as_ptr(), &mut st) } != 0 {
        return Err(std::io::Error::last_os_error());
    }
    // The field types differ by platform; the product is computed wide and saturated.
    let bytes = u128::from(st.f_bavail) * u128::from(st.f_frsize);
    Ok(u64::try_from(bytes).unwrap_or(u64::MAX))
}

fn precheck(root: &Path, name: &str, bytes: Option<u64>) -> Option<Refusal> {
    match free_bytes(root) {
        Ok(free) => upload_refusal(root, name, bytes, free),
        Err(e) => Some(Refusal { status: StatusCode::INTERNAL_SERVER_ERROR, error: format!("the room on DATA could not be read: {e}") }),
    }
}

const BUSY: &str = "another backup is being uploaded; wait for it to end or cancel it";

/// Answer an upload that is refused before its body is read. A client that
/// waits for the go-ahead (`Expect: 100-continue`: curl, for a file over
/// 1 MiB) has sent nothing yet and is answered at once. Any other client is
/// already sending, and its body is read and dropped first: an answer
/// written into a connection that still holds unread request data is lost
/// when the connection closes (the kernel resets it), and the client sees a
/// broken connection in place of the reason. A browser never waits for the
/// go-ahead, which is why its page asks `upload-check` first.
async fn refuse(headers: &HeaderMap, body: Body, status: StatusCode, error: String) -> Response {
    let waits = headers.get(header::EXPECT).and_then(|v| v.to_str().ok()).is_some_and(|v| v.trim().eq_ignore_ascii_case("100-continue"));
    if !waits {
        let mut frames = body.into_data_stream();
        while let Some(frame) = frames.next().await {
            if frame.is_err() {
                break;
            }
        }
    }
    json_error(status, error)
}

#[derive(serde::Deserialize)]
pub struct UploadCheck {
    bytes: Option<u64>,
}

/// `GET /api/backups/{name}/upload-check?bytes=N`: what a `PUT` of that size
/// under that name would be told before its first byte. A browser asks this
/// first: over HTTP/1.1 an answer sent while the body is still on its way
/// may never reach the page.
pub async fn api_backup_upload_check(State(st): State<AppState>, UrlPath(name): UrlPath<String>, Query(q): Query<UploadCheck>) -> Response {
    if st.backup_upload.try_lock().is_err() {
        return json_error(StatusCode::CONFLICT, BUSY.to_string());
    }
    match precheck(&st.backups_root, &name, q.bytes) {
        Some(r) => json_error(r.status, r.error),
        None => (StatusCode::OK, Json(json!({ "ok": true, "name": name }))).into_response(),
    }
}

/// The updater's `import <name>` with the archive on its standard input. The
/// child never carries the console's token (`access::TOKEN_ENV`).
pub(crate) fn import_command(updater: &Path, name: &str) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(updater);
    cmd.arg("import")
        .arg(name)
        .env_remove(crate::access::TOKEN_ENV)
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    cmd
}

/// What an upload came to.
#[derive(Debug, PartialEq, Eq)]
enum Received {
    /// Checked, recorded and listed; the bytes taken.
    Imported(u64),
    /// The updater refused the archive; its reason.
    Refused(String),
    /// The updater failed, or could not be run.
    Failed(String),
    /// The client went away before the upload's end.
    Interrupted(String),
}

/// The updater's reason: its last stderr line, without the command's name.
fn reason(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let line = text.lines().rev().find(|l| !l.trim().is_empty()).unwrap_or("no reason given").trim();
    line.strip_prefix("import: ").unwrap_or(line).to_string()
}

/// Hand the body to the updater as it arrives. The whole body is read
/// whatever the updater says on the way: when it ends early (the file is not
/// a backup) the rest is dropped, and the answer goes out after the last
/// byte, where a browser that is still sending can read it.
async fn receive(updater: &Path, root: &Path, name: &str, body: Body) -> Received {
    let partial = root.join(format!("{name}{PARTIAL_SUFFIX}"));
    let mut child = match import_command(updater, name).spawn() {
        Ok(c) => c,
        Err(e) => return Received::Failed(format!("{} could not be run: {e}", updater.display())),
    };
    let mut pipe = child.stdin.take();
    let mut frames = body.into_data_stream();
    let mut taken: u64 = 0;
    while let Some(frame) = frames.next().await {
        let chunk = match frame {
            Ok(chunk) => chunk,
            Err(e) => {
                drop(pipe);
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = tokio::fs::remove_dir_all(&partial).await;
                return Received::Interrupted(e.to_string());
            }
        };
        taken += chunk.len() as u64;
        if let Some(to_updater) = pipe.as_mut()
            && to_updater.write_all(&chunk).await.is_err()
        {
            pipe = None;
        }
    }
    drop(pipe);
    let out = match tokio::time::timeout(FINISH_TIMEOUT, child.wait_with_output()).await {
        Ok(Ok(out)) => out,
        Ok(Err(e)) => {
            let _ = tokio::fs::remove_dir_all(&partial).await;
            return Received::Failed(format!("the updater could not be waited for: {e}"));
        }
        Err(_) => {
            let _ = tokio::fs::remove_dir_all(&partial).await;
            return Received::Failed(format!("the updater did not finish within {} s of the upload's end", FINISH_TIMEOUT.as_secs()));
        }
    };
    match out.status.code() {
        Some(0) => Received::Imported(taken),
        code => {
            let _ = tokio::fs::remove_dir_all(&partial).await;
            match code {
                Some(2) => Received::Refused(reason(&out.stderr)),
                Some(n) => Received::Failed(format!("the updater ended with code {n}: {}", reason(&out.stderr))),
                None => Received::Failed("the updater was ended by a signal".to_string()),
            }
        }
    }
}

/// `PUT /api/backups/{name}`: the archive of a backup as the body, any size
/// DATA has room for beside its reserve. One upload at a time. 201 with the
/// record the updater wrote; 422 with its reason when the file is not a
/// backup, is damaged or was cut short, and nothing of it is kept.
pub async fn api_backup_put(State(st): State<AppState>, UrlPath(name): UrlPath<String>, headers: HeaderMap, body: Body) -> Response {
    let bytes = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let Ok(one_at_a_time) = st.backup_upload.clone().try_lock_owned() else {
        return refuse(&headers, body, StatusCode::CONFLICT, BUSY.to_string()).await;
    };
    if let Some(r) = precheck(&st.backups_root, &name, bytes) {
        // The door is not held while a refused body is read and dropped.
        drop(one_at_a_time);
        return refuse(&headers, body, r.status, r.error).await;
    }
    tracing::info!(backup = %name, bytes = bytes.unwrap_or(0), "backup upload: receiving");
    match receive(&st.updater, &st.backups_root, &name, body).await {
        Received::Imported(taken) => {
            let record = tokio::fs::read_to_string(st.backups_root.join(&name).join(RECORD)).await.ok().and_then(|t| serde_json::from_str::<Value>(&t).ok());
            tracing::info!(backup = %name, bytes = taken, "backup upload: checked and listed");
            match record {
                Some(record) => (StatusCode::CREATED, Json(json!({ "name": name, "bytes": taken, "record": record }))).into_response(),
                None => json_error(StatusCode::INTERNAL_SERVER_ERROR, format!("\"{name}\" was taken in, but its record cannot be read")),
            }
        }
        Received::Refused(why) => {
            tracing::info!(backup = %name, "backup upload: refused: {why}");
            json_error(StatusCode::UNPROCESSABLE_ENTITY, why)
        }
        Received::Failed(why) => {
            tracing::warn!(backup = %name, "backup upload: failed: {why}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, why)
        }
        Received::Interrupted(why) => {
            tracing::info!(backup = %name, "backup upload: interrupted, nothing kept: {why}");
            json_error(StatusCode::BAD_REQUEST, "the upload was interrupted; nothing was kept".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn a_backup_name_grammar_refuses_traversal_slashes_and_controls() {
        assert!(valid_backup_name("2026-11-05_101200_pre-v0.24.0-phase2"));
        assert!(valid_backup_name("nightly"));
        for bad in ["", "..", ".hidden", "a/b", "a\\b", "a b", "a\nb", "../etc", "a\u{7}b", &"x".repeat(81)] {
            assert!(!valid_backup_name(bad), "{bad:?}");
        }
    }

    #[test]
    fn a_backup_download_is_an_attachment_with_a_length_and_an_incomplete_entry_is_not_found() {
        let root = std::env::temp_dir().join(format!("embra-web-backups-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let complete = root.join("2026-11-05_101200_manual");
        std::fs::create_dir_all(&complete).unwrap();
        std::fs::write(complete.join(ARCHIVE), b"tar bytes").unwrap();
        std::fs::write(complete.join(RECORD), b"{}").unwrap();
        let partial = root.join("2026-11-04_101200_manual.partial");
        std::fs::create_dir_all(&partial).unwrap();
        std::fs::write(partial.join(ARCHIVE), b"half").unwrap();
        assert_eq!(archive_path(&root, "2026-11-05_101200_manual"), Some(complete.join(ARCHIVE)));
        assert_eq!(archive_path(&root, "2026-11-04_101200_manual.partial"), None, "no record beside it");
        assert_eq!(archive_path(&root, "missing"), None);
        assert_eq!(archive_path(&root, "../etc"), None);
        std::fs::remove_dir_all(&root).unwrap();

        let mut h = axum::http::HeaderMap::new();
        download_headers(&mut h, "2026-11-05_101200_manual", 9);
        assert_eq!(h[header::CONTENT_TYPE], "application/octet-stream");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(h[header::CONTENT_LENGTH], "9");
        assert_eq!(h[header::CONTENT_DISPOSITION], "attachment; filename=\"2026-11-05_101200_manual.tar.gz\"");
    }

    /// A backups directory of this test's own, and a stand-in for the
    /// updater: a shell script that is given `import <name>` and the archive
    /// on its standard input, as the updater is.
    fn scratch(tag: &str, updater: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("embra-web-upload-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("backups");
        std::fs::create_dir_all(&root).unwrap();
        let script = dir.join("updater.sh");
        std::fs::write(&script, format!("#!/bin/sh\n[ \"$1\" = ready ] && exit 0\n{}", updater.replace("ROOT", &root.to_string_lossy()))).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A test beside this one may fork while the script was open for
        // writing here, and its child holds that descriptor until it execs:
        // the script cannot be run for that moment (ETXTBSY). It is run once
        // here, until it can be.
        for _ in 0..400 {
            match std::process::Command::new(&script).arg("ready").status() {
                Err(e) if e.raw_os_error() == Some(libc::ETXTBSY) => std::thread::sleep(Duration::from_millis(5)),
                other => {
                    assert!(other.unwrap().success(), "the stand-in updater runs");
                    break;
                }
            }
        }
        (root, script)
    }

    fn state(root: &Path, updater: &Path) -> AppState {
        let (mut st, _input) = AppState::stub();
        st.backups_root = Arc::from(root);
        st.updater = Arc::from(updater);
        st
    }

    fn sized(len: usize) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&len.to_string()).unwrap());
        h
    }

    async fn answer(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Takes the archive in as the updater does: under the name's partial
    /// first, the record beside it, then the rename.
    const TAKES_IT_IN: &str = "d=\"ROOT/$2\"\nmkdir -p \"$d.partial\" && cat > \"$d.partial/backup.tar.gz\" && printf '{\"format\":\"backup.v1\",\"name\":\"%s\",\"taken_by\":\"uploaded\",\"files\":3}\\n' \"$2\" > \"$d.partial/backup.json\" && mv \"$d.partial\" \"$d\"\n";
    /// Refuses after the first bytes, as the updater does for a file that is
    /// no backup, and leaves what it had written.
    const REFUSES_EARLY: &str = "mkdir -p \"ROOT/$2.partial\"\nhead -c 10 > \"ROOT/$2.partial/backup.tar.gz\"\necho 'plan: reading' >&2\necho 'import: this is not a backup of embraOS: its member \"etc/passwd\" is outside state/ and data/; nothing was kept' >&2\nexit 2\n";
    const FAILS: &str = "cat > /dev/null\necho 'write /embra/data/backups/x.partial/backup.tar.gz: No space left on device (os error 28)' >&2\nexit 1\n";
    /// Writes until its input ends, one process, so that ending it ends the writing.
    const WRITES_ON: &str = "mkdir -p \"ROOT/$2.partial\" && exec cat > \"ROOT/$2.partial/backup.tar.gz\"\n";

    #[test]
    fn an_upload_is_refused_before_its_first_byte_on_its_name_its_size_and_the_room() {
        let (root, _) = scratch("refusal", "");
        std::fs::create_dir_all(root.join("2026-11-05_101200_manual")).unwrap();
        let gib = 1024 * 1024 * 1024;
        assert_eq!(upload_refusal(&root, "2026-11-12_101200_manual", Some(gib), 2 * gib), None);
        assert_eq!(upload_refusal(&root, "mine", Some(1), RESERVE_BYTES + 1), None, "the archive and the reserve fit exactly");
        for bad in ["../etc", "a/b", ".hidden", "", "mine.partial", &"x".repeat(81)] {
            let r = upload_refusal(&root, bad, Some(1), u64::MAX).unwrap_or_else(|| panic!("{bad:?} passed"));
            assert_eq!(r.status, StatusCode::BAD_REQUEST, "{bad:?}");
            assert!(r.error.contains("not a name for a backup"), "{}", r.error);
        }
        let r = upload_refusal(&root, "2026-11-05_101200_manual", Some(1), u64::MAX).unwrap();
        assert_eq!(r.status, StatusCode::CONFLICT);
        assert!(r.error.contains("exists already") && r.error.contains("/backup delete 2026-11-05_101200_manual"), "{}", r.error);
        assert_eq!(upload_refusal(&root, "mine", None, u64::MAX).unwrap().status, StatusCode::LENGTH_REQUIRED);
        assert_eq!(upload_refusal(&root, "mine", Some(0), u64::MAX).unwrap().status, StatusCode::BAD_REQUEST);
        let r = upload_refusal(&root, "mine", Some(gib), gib + RESERVE_BYTES - 1).unwrap();
        assert_eq!(r.status, StatusCode::INSUFFICIENT_STORAGE);
        assert!(r.error.contains("DATA has 1279 MB free; the upload is 1024 MB and leaves a reserve of 256 MB"), "{}", r.error);
        assert_eq!(upload_refusal(&root, "mine", Some(u64::MAX), u64::MAX).unwrap().status, StatusCode::INSUFFICIENT_STORAGE, "no overflow");
        // The room is read where a directory exists: a machine that never took a backup has none.
        assert!(free_bytes(&root.join("not/there/yet")).unwrap() > 0);
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn an_upload_is_handed_to_the_updater_whole_and_answered_with_its_record() {
        let (root, updater) = scratch("taken", TAKES_IT_IN);
        let st = state(&root, &updater);
        let archive: Vec<u8> = (0..300_000u32).flat_map(|i| i.to_le_bytes()).collect();
        let (status, check) = answer(
            api_backup_upload_check(State(st.clone()), UrlPath("2026-11-12_101200_manual".into()), Query(UploadCheck { bytes: Some(archive.len() as u64) })).await,
        )
        .await;
        assert_eq!((status, &check["ok"], &check["name"]), (StatusCode::OK, &json!(true), &json!("2026-11-12_101200_manual")));
        let chunks: Vec<Result<Bytes, std::io::Error>> = archive.chunks(7919).map(|c| Ok(Bytes::copy_from_slice(c))).collect();
        let resp = api_backup_put(State(st.clone()), UrlPath("2026-11-12_101200_manual".into()), sized(archive.len()), Body::from_stream(stream::iter(chunks))).await;
        let (status, body) = answer(resp).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        assert_eq!(body["name"], "2026-11-12_101200_manual");
        assert_eq!(body["bytes"], archive.len());
        assert_eq!(body["record"]["taken_by"], "uploaded");
        assert_eq!(body["record"]["files"], 3);
        assert_eq!(std::fs::read(root.join("2026-11-12_101200_manual").join(ARCHIVE)).unwrap(), archive, "every byte reached the updater, in order");
        // It is there now: the same name is refused, by the check and by the upload, and the download serves it.
        let (status, again) = answer(
            api_backup_upload_check(State(st.clone()), UrlPath("2026-11-12_101200_manual".into()), Query(UploadCheck { bytes: Some(1) })).await,
        )
        .await;
        assert_eq!(status, StatusCode::CONFLICT, "{again}");
        let (status, again) = answer(api_backup_put(State(st.clone()), UrlPath("2026-11-12_101200_manual".into()), sized(3), Body::from("abc")).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(again["error"].as_str().unwrap().contains("exists already"));
        assert_eq!(std::fs::read(root.join("2026-11-12_101200_manual").join(ARCHIVE)).unwrap(), archive);
        assert_eq!(archive_path(&root, "2026-11-12_101200_manual"), Some(root.join("2026-11-12_101200_manual").join(ARCHIVE)));
        // An upload without its size, and one under a name no backup has, never reach the updater.
        let (status, _) = answer(api_backup_put(State(st.clone()), UrlPath("mine".into()), HeaderMap::new(), Body::from("abc")).await).await;
        assert_eq!(status, StatusCode::LENGTH_REQUIRED);
        let (status, _) = answer(api_backup_put(State(st.clone()), UrlPath("mine.partial".into()), sized(3), Body::from("abc")).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(!root.join("mine").exists() && !root.join("mine.partial").exists());
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn a_refused_archive_is_read_to_its_end_before_the_answer_and_nothing_is_kept() {
        let (root, updater) = scratch("refused", REFUSES_EARLY);
        let st = state(&root, &updater);
        let polled = Arc::new(AtomicUsize::new(0));
        let seen = Arc::clone(&polled);
        // Well past what a pipe holds: the updater is gone long before the last chunk.
        let chunks = stream::iter(0..256).map(move |_| {
            seen.fetch_add(1, Ordering::SeqCst);
            Ok::<_, std::io::Error>(Bytes::from(vec![7u8; 16 * 1024]))
        });
        let resp = api_backup_put(State(st.clone()), UrlPath("wrong".into()), sized(256 * 16 * 1024), Body::from_stream(chunks)).await;
        let (status, body) = answer(resp).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY, "{body}");
        assert_eq!(
            body["error"], "this is not a backup of embraOS: its member \"etc/passwd\" is outside state/ and data/; nothing was kept",
            "the updater's last line, without the command's name"
        );
        assert_eq!(polled.load(Ordering::SeqCst), 256, "the rest of the upload was read and dropped");
        assert!(!root.join("wrong").exists() && !root.join("wrong.partial").exists(), "what the updater left is removed");
        // A failure of the updater is the server's, with its reason.
        let (root, updater) = scratch("failed", FAILS);
        let st = state(&root, &updater);
        let (status, body) = answer(api_backup_put(State(st.clone()), UrlPath("mine".into()), sized(3), Body::from("abc")).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["error"].as_str().unwrap().contains("code 1") && body["error"].as_str().unwrap().contains("No space left on device"), "{body}");
        // And an updater that is not there says so.
        let st = state(&root, Path::new("/nonexistent/embra-update"));
        let (status, body) = answer(api_backup_put(State(st), UrlPath("mine".into()), sized(3), Body::from("abc")).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(body["error"].as_str().unwrap().contains("could not be run"), "{body}");
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn an_interrupted_upload_ends_the_updater_and_leaves_nothing() {
        let (root, updater) = scratch("interrupted", WRITES_ON);
        let st = state(&root, &updater);
        let chunks: Vec<Result<Bytes, std::io::Error>> = vec![
            Ok(Bytes::from(vec![1u8; 4096])),
            Ok(Bytes::from(vec![2u8; 4096])),
            Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "the browser went away")),
        ];
        let resp = api_backup_put(State(st.clone()), UrlPath("half".into()), sized(1 << 20), Body::from_stream(stream::iter(chunks))).await;
        let (status, body) = answer(resp).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert!(body["error"].as_str().unwrap().contains("interrupted"), "{body}");
        assert!(!root.join("half").exists() && !root.join("half.partial").exists());
        // The door is open again: the next upload is not told another one runs.
        assert!(st.backup_upload.try_lock().is_ok());
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    /// A refusal that is known before the body: a client already sending
    /// has its body read and dropped before the answer, so the answer is
    /// read; one that waits for the go-ahead is answered at once and sends
    /// nothing.
    #[tokio::test]
    async fn a_refusal_known_beforehand_is_answered_after_the_body_or_at_once_to_a_client_that_waits() {
        let (root, updater) = scratch("beforehand", TAKES_IT_IN);
        let st = state(&root, &updater);
        std::fs::create_dir_all(root.join("taken")).unwrap();
        let counted = |polled: &Arc<AtomicUsize>| {
            let seen = Arc::clone(polled);
            Body::from_stream(stream::iter(0..64).map(move |_| {
                seen.fetch_add(1, Ordering::SeqCst);
                Ok::<_, std::io::Error>(Bytes::from(vec![7u8; 4096]))
            }))
        };
        let polled = Arc::new(AtomicUsize::new(0));
        let (status, body) = answer(api_backup_put(State(st.clone()), UrlPath("taken".into()), sized(64 * 4096), counted(&polled)).await).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(polled.load(Ordering::SeqCst), 64, "a client that is sending is read to its end first");
        assert!(st.backup_upload.try_lock().is_ok(), "and the door was not held meanwhile");
        let polled = Arc::new(AtomicUsize::new(0));
        let mut waits = sized(64 * 4096);
        waits.insert(header::EXPECT, HeaderValue::from_static("100-continue"));
        let (status, body) = answer(api_backup_put(State(st.clone()), UrlPath("taken".into()), waits, counted(&polled)).await).await;
        assert_eq!(status, StatusCode::CONFLICT, "{body}");
        assert_eq!(polled.load(Ordering::SeqCst), 0, "a client that waits for the go-ahead is answered before it sends");
        // The same for an upload that finds another one running.
        let held = st.backup_upload.clone().try_lock_owned().unwrap();
        let polled = Arc::new(AtomicUsize::new(0));
        let (status, _) = answer(api_backup_put(State(st.clone()), UrlPath("mine".into()), sized(64 * 4096), counted(&polled)).await).await;
        assert_eq!((status, polled.load(Ordering::SeqCst)), (StatusCode::CONFLICT, 64));
        drop(held);
        assert!(!root.join("mine").exists() && !root.join("mine.partial").exists());
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[tokio::test]
    async fn one_upload_at_a_time() {
        let (root, updater) = scratch("busy", TAKES_IT_IN);
        let st = state(&root, &updater);
        let held = st.backup_upload.clone().try_lock_owned().unwrap();
        let (status, body) = answer(api_backup_put(State(st.clone()), UrlPath("mine".into()), sized(3), Body::from("abc")).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(body["error"].as_str().unwrap().contains("another backup is being uploaded"), "{body}");
        let (status, _) = answer(api_backup_upload_check(State(st.clone()), UrlPath("mine".into()), Query(UploadCheck { bytes: Some(3) })).await).await;
        assert_eq!(status, StatusCode::CONFLICT);
        assert!(!root.join("mine").exists());
        drop(held);
        let (status, body) = answer(api_backup_put(State(st), UrlPath("mine".into()), sized(3), Body::from("abc")).await).await;
        assert_eq!(status, StatusCode::CREATED, "{body}");
        std::fs::remove_dir_all(root.parent().unwrap()).unwrap();
    }

    #[test]
    fn the_updater_is_given_import_and_the_name_and_never_the_token() {
        let cmd = import_command(Path::new(UPDATER), "2026-11-12_101200_manual");
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), UPDATER);
        assert_eq!(std_cmd.get_args().collect::<Vec<_>>(), ["import", "2026-11-12_101200_manual"]);
        let envs: Vec<_> = std_cmd.get_envs().collect();
        assert_eq!(envs, [(std::ffi::OsStr::new(crate::access::TOKEN_ENV), None)], "the token is taken out and nothing is added");
        assert_eq!(reason(b"plan: x\nimport: the file could not be read; nothing was kept\n\n"), "the file could not be read; nothing was kept");
        assert_eq!(reason(b""), "no reason given");
    }

    /// The upload's rules are the updater's, each written twice: the reserve,
    /// the entry's file names, the suffix of an unfinished entry, the name
    /// grammar's length.
    #[test]
    fn the_uploads_rules_are_the_updaters() {
        let read = |rel: &str| std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let paths = read("embra-update/src/paths.rs");
        for line in [
            "pub const RESERVE_BYTES: u64 = 256 * 1024 * 1024;",
            &format!("pub const ARCHIVE: &str = \"{ARCHIVE}\";"),
            &format!("pub const RECORD: &str = \"{RECORD}\";"),
            &format!("pub const PARTIAL_SUFFIX: &str = \"{PARTIAL_SUFFIX}\";"),
            &format!("pub const BACKUPS_DIR: &str = \"{BACKUPS_ROOT}\";"),
        ] {
            assert!(paths.contains(line), "embra-update/src/paths.rs no longer says: {line}");
        }
        assert_eq!(RESERVE_BYTES, 256 * 1024 * 1024);
        let cli = read("embra-update/src/cli.rs");
        assert!(cli.contains("valid_word(s, 80)"), "a backup's name is 80 characters at most in the updater");
        assert!(cli.contains("\"import\" =>"), "the updater has its import command");
        let import = read("embra-update/src/import.rs");
        assert!(import.contains("valid_name(name) && !name.ends_with(PARTIAL_SUFFIX)"), "the updater refuses the same names");
        let brain = read("embra-brain/src/grpc_service/update.rs");
        assert!(brain.contains(&format!("const UPDATER: &str = \"{UPDATER}\";")), "the brain runs the same updater");
    }
}
