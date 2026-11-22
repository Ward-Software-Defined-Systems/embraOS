//! A native model's file (experimental) through the web console, behind the
//! console's token like every route: the way in on a machine that cannot
//! reach the catalogue's source.
//!
//! `PUT /api/models/{id}` takes the model's file as the body and hands it to
//! the updater's `model-import`, which checks it against the catalogue (its
//! size, then the sha256 compiled into the image) while it writes it, and
//! keeps nothing of a file that is not the catalogue's. `GET
//! /api/models/{id}/upload-check` says beforehand what such a `PUT` would be
//! told. `GET /api/models` lists the catalogue with each model's file name
//! and size, so that a page can tell which model a picked file is.
//!
//! The file is several gigabytes: it is streamed to the updater as it
//! arrives and never held. The routes read DATA themselves and reach no
//! service; the brain is not asked, and loads nothing (`/native use` does).

use std::path::Path;
use std::process::Stdio;
use std::time::Duration;

use axum::Json;
use axum::body::Body;
use axum::extract::{Path as UrlPath, Query, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use embra_boot::native_model::{self as native, Installed, NativeModel};
use futures_util::StreamExt;
use serde_json::json;
use tokio::io::AsyncWriteExt;

use crate::backups::{RESERVE_BYTES, Refusal, UploadCheck, free_bytes, reason, refuse};
use crate::media::json_error;
use crate::state::AppState;

/// Where the OS keeps the models' files.
pub const MODELS_ROOT: &str = native::MODELS_DIR;
/// How long the updater has to finish once the upload's last byte was handed
/// to it: the file's sync, its rename, its record. It hashed as it wrote.
const FINISH_TIMEOUT: Duration = Duration::from_secs(600);

const BUSY: &str = "another model file is being uploaded; wait for it to end or cancel it";

fn catalogue_ids() -> String {
    native::CATALOGUE.iter().map(|m| m.id).collect::<Vec<_>>().join(", ")
}

/// How a model's file stands, in the updater's words for its list.
fn stands(model: &NativeModel, models: &Path) -> &'static str {
    match native::installed(model, models) {
        Installed::Verified => "installed",
        Installed::No => "not installed",
        Installed::Partial(_) => "a download stopped",
        Installed::Unverified(_) => "installed, not usable",
    }
}

/// `GET /api/models`: the catalogue, with what a page needs to tell which
/// model a file is (the file's name and its size) and how each stands.
pub async fn api_models_list(State(st): State<AppState>) -> Response {
    let models: Vec<_> = native::CATALOGUE
        .iter()
        .map(|m| json!({ "id": m.id, "label": m.label, "file": m.file, "bytes": m.bytes, "url": m.url(), "state": stands(m, &st.models_root) }))
        .collect();
    (StatusCode::OK, Json(json!({ "experimental": true, "models": models }))).into_response()
}

/// Why an upload of `bytes` for the model `id` is refused before it starts,
/// if it is; else the model. `free` is what DATA has, and `job_runs` whether
/// a model job holds the lock now (an install, an upload, a removal). The
/// updater's `model-import` holds the same rules; what it finds in the file
/// comes after the last byte.
pub fn upload_refusal(models: &Path, id: &str, bytes: Option<u64>, free: u64, job_runs: bool) -> Result<&'static NativeModel, Refusal> {
    let refuse = |status, error: String| Err(Refusal { status, error });
    let Some(model) = native::by_id(id) else {
        let shown: String = id.chars().take(60).collect();
        return refuse(StatusCode::NOT_FOUND, format!("{shown:?} is not a native model of this release (experimental): {}", catalogue_ids()));
    };
    if native::installed(model, models) == Installed::Verified {
        return refuse(
            StatusCode::CONFLICT,
            format!("{} is installed and checked already; /native remove {} first, to put its file in again", model.id, model.id),
        );
    }
    if job_runs {
        return refuse(
            StatusCode::CONFLICT,
            "a model job runs (an install, an upload or a removal); /native shows it, and it has to end first".to_string(),
        );
    }
    let Some(bytes) = bytes else {
        return refuse(
            StatusCode::LENGTH_REQUIRED,
            "the upload has to say its size (a Content-Length header; a browser and curl -T <file> send one)".to_string(),
        );
    };
    if bytes != model.bytes {
        return refuse(
            StatusCode::BAD_REQUEST,
            format!(
                "this file is {bytes} bytes, and the catalogue's file of {} is {} ({}, {} bytes)",
                model.id, model.file, model.quantisation, model.bytes
            ),
        );
    }
    if bytes.checked_add(RESERVE_BYTES).is_none_or(|needed| free < needed) {
        return refuse(
            StatusCode::INSUFFICIENT_STORAGE,
            format!(
                "DATA has {} MB free; the file is {} MB and leaves a reserve of {} MB. Free some (/backup delete <name>, /native remove <model>) and upload again",
                free / (1024 * 1024),
                bytes.div_ceil(1024 * 1024),
                RESERVE_BYTES / (1024 * 1024)
            ),
        );
    }
    Ok(model)
}

/// The refusal known before the first byte, with the room DATA was read to
/// have: the handlers read it, a test names it.
fn precheck(models: &Path, id: &str, bytes: Option<u64>, free: std::io::Result<u64>) -> Result<&'static NativeModel, Refusal> {
    match free {
        Ok(free) => upload_refusal(models, id, bytes, free, native::job_running(models)),
        Err(e) => Err(Refusal { status: StatusCode::INTERNAL_SERVER_ERROR, error: format!("the room on DATA could not be read: {e}") }),
    }
}

/// `GET /api/models/{id}/upload-check?bytes=N`: what a `PUT` of that size
/// for that model would be told before its first byte. A browser asks this
/// first: over HTTP/1.1 an answer sent while the body is still on its way
/// may never reach the page.
pub async fn api_model_upload_check(State(st): State<AppState>, UrlPath(id): UrlPath<String>, Query(q): Query<UploadCheck>) -> Response {
    let free = free_bytes(&st.models_root);
    check_model(&st, &id, q.bytes, free)
}

fn check_model(st: &AppState, id: &str, bytes: Option<u64>, free: std::io::Result<u64>) -> Response {
    if st.model_upload.try_lock().is_err() {
        return json_error(StatusCode::CONFLICT, BUSY.to_string());
    }
    match precheck(&st.models_root, id, bytes, free) {
        Ok(model) => (StatusCode::OK, Json(json!({ "ok": true, "model": model.id, "file": model.file }))).into_response(),
        Err(r) => json_error(r.status, r.error),
    }
}

/// The updater's `model-import <id>` with the file on its standard input.
/// The child never carries the console's token (`access::TOKEN_ENV`).
pub(crate) fn import_command(updater: &Path, model: &NativeModel) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(updater);
    cmd.arg("model-import")
        .arg(model.id)
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
    /// Checked against the catalogue and installed; the bytes taken.
    Installed(u64),
    /// The updater refused the file; its reason. Nothing of it is kept.
    Refused(String),
    /// The updater failed, or could not be run.
    Failed(String),
    /// The client went away before the upload's end.
    Interrupted(String),
}

/// Hand the body to the updater as it arrives. The whole body is read
/// whatever the updater says on the way: when it ends early the rest is
/// dropped, and the answer goes out after the last byte, where a browser
/// that is still sending can read it.
async fn receive(updater: &Path, models: &Path, model: &'static NativeModel, body: Body) -> Received {
    let mut child = match import_command(updater, model).spawn() {
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
                // The updater is ended in the middle of its writing: what it
                // had written is no download to go on from, and goes.
                drop(pipe);
                let _ = child.start_kill();
                let _ = child.wait().await;
                let _ = tokio::fs::remove_file(model.partial_path(models)).await;
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
        Ok(Err(e)) => return Received::Failed(format!("the updater could not be waited for: {e}")),
        Err(_) => return Received::Failed(format!("the updater did not finish within {} s of the upload's end", FINISH_TIMEOUT.as_secs())),
    };
    match out.status.code() {
        // 3: the model was installed and checked meanwhile, and nothing was to do.
        Some(0) | Some(3) => Received::Installed(taken),
        Some(2) => Received::Refused(reason(&out.stderr).trim_start_matches("refused: ").to_string()),
        Some(n) => Received::Failed(format!("the updater ended with code {n}: {}", reason(&out.stderr))),
        None => Received::Failed("the updater was ended by a signal".to_string()),
    }
}

/// `PUT /api/models/{id}`: the model's file as the body. One upload at a
/// time. 201 when the updater checked it against the catalogue and
/// installed it; 422 with its reason when the file is not the catalogue's
/// or was cut short, and nothing of it is kept.
pub async fn api_model_put(State(st): State<AppState>, UrlPath(id): UrlPath<String>, headers: HeaderMap, body: Body) -> Response {
    let free = free_bytes(&st.models_root);
    put_model(&st, &id, &headers, body, free).await
}

async fn put_model(st: &AppState, id: &str, headers: &HeaderMap, body: Body, free: std::io::Result<u64>) -> Response {
    let bytes = headers.get(header::CONTENT_LENGTH).and_then(|v| v.to_str().ok()).and_then(|v| v.parse::<u64>().ok());
    let Ok(one_at_a_time) = st.model_upload.clone().try_lock_owned() else {
        return refuse(headers, body, StatusCode::CONFLICT, BUSY.to_string()).await;
    };
    let model = match precheck(&st.models_root, id, bytes, free) {
        Ok(model) => model,
        Err(r) => {
            // The door is not held while a refused body is read and dropped.
            drop(one_at_a_time);
            return refuse(headers, body, r.status, r.error).await;
        }
    };
    tracing::info!(model = model.id, bytes = model.bytes, "model upload: receiving");
    match receive(&st.updater, &st.models_root, model, body).await {
        Received::Installed(taken) => {
            tracing::info!(model = model.id, bytes = taken, "model upload: checked and installed");
            (StatusCode::CREATED, Json(json!({ "model": model.id, "file": model.file, "bytes": taken }))).into_response()
        }
        Received::Refused(why) => {
            tracing::info!(model = model.id, "model upload: refused: {why}");
            json_error(StatusCode::UNPROCESSABLE_ENTITY, why)
        }
        Received::Failed(why) => {
            tracing::warn!(model = model.id, "model upload: failed: {why}");
            json_error(StatusCode::INTERNAL_SERVER_ERROR, why)
        }
        Received::Interrupted(why) => {
            tracing::info!(model = model.id, "model upload: interrupted, nothing kept: {why}");
            json_error(StatusCode::BAD_REQUEST, "the upload was interrupted; nothing was kept".to_string())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::HeaderValue;
    use serde_json::Value;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;
    use std::sync::Arc;

    const FOUR: &str = "qwen3.5-4b";

    fn four() -> &'static NativeModel {
        native::by_id(FOUR).unwrap()
    }

    /// A models directory of this test's own, and a stand-in for the
    /// updater: a shell script that is given `model-import <id>` and the
    /// file on its standard input, as the updater is.
    fn scratch(tag: &str, updater: &str) -> (PathBuf, PathBuf) {
        let dir = std::env::temp_dir().join(format!("embra-web-model-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let root = dir.join("models");
        std::fs::create_dir_all(&root).unwrap();
        let script = dir.join("updater.sh");
        std::fs::write(&script, format!("#!/bin/sh\n[ \"$1\" = ready ] && exit 0\n{}", updater.replace("ROOT", &root.to_string_lossy()))).unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        // A test beside this one may fork while the script was open for
        // writing here (ETXTBSY): it is run once here, until it can be
        // (`backups::tests::scratch`).
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
        st.models_root = Arc::from(root);
        st.updater = Arc::from(updater);
        st
    }

    /// The headers of an upload that says the catalogue's size for the 4B.
    fn sized(len: u64) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(header::CONTENT_LENGTH, HeaderValue::from_str(&len.to_string()).unwrap());
        h
    }

    async fn answer(resp: Response) -> (StatusCode, Value) {
        let status = resp.status();
        let bytes = axum::body::to_bytes(resp.into_body(), 1 << 20).await.unwrap();
        (status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
    }

    /// Put the model's file there as the updater leaves it: of the
    /// catalogue's size (sparse) with the record that vouches for it.
    fn install(model: &NativeModel, models: &Path) {
        std::fs::create_dir_all(model.dir(models)).unwrap();
        std::fs::File::create(model.file_path(models)).unwrap().set_len(model.bytes).unwrap();
        let meta = std::fs::metadata(model.file_path(models)).unwrap();
        native::write_verified(&model.verified_path(models), &native::Verified::of(model, &meta)).unwrap();
    }

    #[test]
    fn an_upload_is_refused_before_its_first_byte_on_its_model_its_size_and_the_room() {
        let (root, _) = scratch("refusal", "");
        let model = four();
        let room = model.bytes + RESERVE_BYTES;
        assert_eq!(upload_refusal(&root, FOUR, Some(model.bytes), room, false).map(|m| m.id), Ok(FOUR), "the file and the reserve fit exactly");

        let refused = |id: &str, bytes: Option<u64>, free: u64, job: bool| upload_refusal(&root, id, bytes, free, job).unwrap_err();
        let r = refused("qwen3.5-27b", Some(1), u64::MAX, false);
        assert_eq!(r.status, StatusCode::NOT_FOUND);
        assert_eq!(r.error, "\"qwen3.5-27b\" is not a native model of this release (experimental): qwen3.5-4b, qwen3.5-9b");
        for traversal in ["../etc", "a/b", "", ".hidden"] {
            assert_eq!(refused(traversal, Some(1), u64::MAX, false).status, StatusCode::NOT_FOUND, "{traversal:?}: an id is a catalogue's or nothing");
        }
        assert_eq!(refused(FOUR, None, u64::MAX, false).status, StatusCode::LENGTH_REQUIRED);
        let r = refused(FOUR, Some(1234), u64::MAX, false);
        assert_eq!(r.status, StatusCode::BAD_REQUEST);
        assert_eq!(r.error, "this file is 1234 bytes, and the catalogue's file of qwen3.5-4b is Qwen3.5-4B-Q4_K_M.gguf (Q4_K_M, 2740937888 bytes)");
        let r = refused(FOUR, Some(model.bytes), room - 1, false);
        assert_eq!(r.status, StatusCode::INSUFFICIENT_STORAGE);
        assert!(r.error.starts_with("DATA has 2869 MB free; the file is 2614 MB and leaves a reserve of 256 MB."), "{}", r.error);
        let r = refused(FOUR, Some(model.bytes), u64::MAX, true);
        assert_eq!(r.status, StatusCode::CONFLICT);
        assert!(r.error.starts_with("a model job runs (an install, an upload or a removal)"), "{}", r.error);

        // A model that is installed and checked takes no second file.
        install(model, &root);
        let r = refused(FOUR, Some(model.bytes), u64::MAX, false);
        assert_eq!(r.status, StatusCode::CONFLICT);
        assert_eq!(r.error, "qwen3.5-4b is installed and checked already; /native remove qwen3.5-4b first, to put its file in again");
        // The other model is not touched by it.
        let nine = native::by_id("qwen3.5-9b").unwrap();
        assert_eq!(upload_refusal(&root, "qwen3.5-9b", Some(nine.bytes), u64::MAX, false).map(|m| m.id), Ok("qwen3.5-9b"));
    }

    #[tokio::test]
    async fn the_catalogue_is_listed_with_each_files_name_size_and_state() {
        let (root, script) = scratch("list", "");
        install(four(), &root);
        let (status, body) = answer(api_models_list(State(state(&root, &script))).await).await;
        assert_eq!(status, StatusCode::OK);
        assert_eq!(body["experimental"], true);
        let models = body["models"].as_array().unwrap();
        assert_eq!(models.len(), native::CATALOGUE.len());
        assert_eq!(
            (models[0]["id"].as_str(), models[0]["file"].as_str(), models[0]["bytes"].as_u64(), models[0]["state"].as_str()),
            (Some(FOUR), Some("Qwen3.5-4B-Q4_K_M.gguf"), Some(2_740_937_888), Some("installed"))
        );
        assert_eq!((models[1]["id"].as_str(), models[1]["state"].as_str()), (Some("qwen3.5-9b"), Some("not installed")));
        assert!(models[0]["url"].as_str().unwrap().starts_with("https://huggingface.co/unsloth/Qwen3.5-4B-GGUF/resolve/"), "{}", models[0]["url"]);
    }

    /// Reads the file to its end, as the updater does, and says it is done.
    const TAKES_IT_IN: &str = "cat > \"ROOT/$2.taken\"\necho \"$1 $2\" > \"ROOT/args\"\necho \"$2: installed and verified\"\n";
    /// Refuses after the first bytes, as the updater does for a file that is
    /// not the catalogue's.
    const REFUSES: &str = "head -c 10 > /dev/null\necho 'qwen3.5-4b: reading' >&2\necho 'refused: the file has the size of Qwen3.5-4B-Q4_K_M.gguf and its sha256 is 0011223344556677…, not 00fe7986ff5f6b46…; nothing of it is kept' >&2\nexit 2\n";
    const FAILS: &str = "cat > /dev/null\necho 'writing /embra/data/models/qwen3.5-4b/Qwen3.5-4B-Q4_K_M.gguf.partial: No space left on device (os error 28)' >&2\nexit 1\n";
    /// Writes the model's partial file until its input ends, one process.
    const WRITES_ON: &str = "mkdir -p \"ROOT/$2\" && exec cat > \"ROOT/$2/Qwen3.5-4B-Q4_K_M.gguf.partial\"\n";

    /// DATA with all the room a test's upload asks for: the room is named
    /// here, never read from the host the test runs on.
    const ROOMY: fn() -> std::io::Result<u64> = || Ok(u64::MAX);

    /// The precheck wants the catalogue's size in the header; the stand-in
    /// reads what comes. A short body under the right header is what a test
    /// sends: the route hands over what arrives and the updater judges it.
    #[tokio::test]
    async fn an_upload_is_handed_to_the_updater_and_answered_after_its_last_byte() {
        let (root, script) = scratch("put", TAKES_IT_IN);
        let st = state(&root, &script);
        let body = vec![7u8; 300_000];
        let (status, said) = answer(put_model(&st, FOUR, &sized(four().bytes), Body::from(body.clone()), ROOMY()).await).await;
        assert_eq!(status, StatusCode::CREATED, "{said}");
        assert_eq!((said["model"].as_str(), said["file"].as_str(), said["bytes"].as_u64()), (Some(FOUR), Some("Qwen3.5-4B-Q4_K_M.gguf"), Some(300_000)));
        assert_eq!(std::fs::read(root.join("qwen3.5-4b.taken")).unwrap(), body, "every byte reached the updater");
        assert_eq!(std::fs::read_to_string(root.join("args")).unwrap().trim(), "model-import qwen3.5-4b");

        // A file that is not the catalogue's: the updater's reason, after the whole body was read.
        let (root, script) = scratch("refused", REFUSES);
        let (status, said) = answer(put_model(&state(&root, &script), FOUR, &sized(four().bytes), Body::from(vec![1u8; 500_000]), ROOMY()).await).await;
        assert_eq!(status, StatusCode::UNPROCESSABLE_ENTITY);
        assert_eq!(
            said["error"],
            "the file has the size of Qwen3.5-4B-Q4_K_M.gguf and its sha256 is 0011223344556677…, not 00fe7986ff5f6b46…; nothing of it is kept"
        );

        // The updater fails on the way.
        let (root, script) = scratch("failed", FAILS);
        let (status, said) = answer(put_model(&state(&root, &script), FOUR, &sized(four().bytes), Body::from(vec![1u8; 1000]), ROOMY()).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(said["error"].as_str().unwrap().starts_with("the updater ended with code 1: writing /embra/data/models/"), "{said}");

        // No updater on this machine.
        let st = state(&root, Path::new("/nonexistent/embra-update"));
        let (status, said) = answer(put_model(&st, FOUR, &sized(four().bytes), Body::from(vec![1u8; 10]), ROOMY()).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert!(said["error"].as_str().unwrap().contains("could not be run"), "{said}");

        // A refusal known beforehand: no updater is run for it.
        let (root, script) = scratch("known", TAKES_IT_IN);
        let (status, said) = answer(put_model(&state(&root, &script), "qwen3.5-27b", &sized(10), Body::from(vec![1u8; 10]), ROOMY()).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{said}");
        let (status, _) = answer(put_model(&state(&root, &script), FOUR, &sized(10), Body::from(vec![1u8; 10]), ROOMY()).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "another size than the catalogue's");
        // DATA without the room: said before a byte is taken.
        let (status, said) = answer(put_model(&state(&root, &script), FOUR, &sized(four().bytes), Body::from(vec![1u8; 10]), Ok(1024)).await).await;
        assert_eq!(status, StatusCode::INSUFFICIENT_STORAGE, "{said}");
        let (status, said) = answer(put_model(&state(&root, &script), FOUR, &sized(four().bytes), Body::from(vec![1u8; 10]), Err(std::io::Error::other("no such filesystem"))).await).await;
        assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
        assert_eq!(said["error"], "the room on DATA could not be read: no such filesystem");
        assert!(!root.join("args").exists(), "the updater was not run");
        // The route itself reads the room of the directory it was given.
        let (status, _) = answer(api_model_put(State(state(&root, &script)), UrlPath("qwen3.5-27b".to_string()), sized(10), Body::from(vec![1u8; 10])).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
    }

    /// A client that goes away in the middle: the updater is ended, and what
    /// it had written of the file goes, since it is no download to go on from.
    #[tokio::test]
    async fn an_interrupted_upload_ends_the_updater_and_leaves_nothing() {
        let (root, script) = scratch("interrupted", WRITES_ON);
        let frames = futures_util::stream::iter(vec![
            Ok::<_, std::io::Error>(axum::body::Bytes::from(vec![5u8; 200_000])),
            Err(std::io::Error::new(std::io::ErrorKind::ConnectionReset, "the browser went away")),
        ]);
        let (status, said) = answer(put_model(&state(&root, &script), FOUR, &sized(four().bytes), Body::from_stream(frames), ROOMY()).await).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        assert_eq!(said["error"], "the upload was interrupted; nothing was kept");
        assert!(!four().partial_path(&root).exists(), "what the updater had written is gone");
    }

    #[tokio::test]
    async fn one_upload_at_a_time_and_the_check_says_what_a_put_would_be_told() {
        let (root, script) = scratch("one", TAKES_IT_IN);
        let st = state(&root, &script);
        let check = |st: AppState, id: &'static str, bytes: Option<u64>| async move { answer(check_model(&st, id, bytes, ROOMY())).await };
        let (status, said) = check(st.clone(), FOUR, Some(four().bytes)).await;
        assert_eq!(status, StatusCode::OK, "{said}");
        assert_eq!((said["model"].as_str(), said["file"].as_str()), (Some(FOUR), Some("Qwen3.5-4B-Q4_K_M.gguf")));
        let (status, said) = check(st.clone(), FOUR, Some(12)).await;
        assert_eq!(status, StatusCode::BAD_REQUEST, "{said}");
        let (status, _) = check(st.clone(), "nothing", Some(12)).await;
        assert_eq!(status, StatusCode::NOT_FOUND);
        // The route itself: the same answer for what no room decides.
        let (status, _) = answer(api_model_upload_check(State(st.clone()), UrlPath("nothing".to_string()), Query(UploadCheck { bytes: Some(12) })).await).await;
        assert_eq!(status, StatusCode::NOT_FOUND);

        // While one upload holds the door, a second and a check are told so.
        let held = st.model_upload.clone().try_lock_owned().unwrap();
        let (status, said) = check(st.clone(), FOUR, Some(four().bytes)).await;
        assert_eq!((status, said["error"].as_str()), (StatusCode::CONFLICT, Some(BUSY)));
        let (status, said) = answer(put_model(&st, FOUR, &sized(four().bytes), Body::from(vec![1u8; 10]), ROOMY()).await).await;
        assert_eq!((status, said["error"].as_str()), (StatusCode::CONFLICT, Some(BUSY)));
        drop(held);
    }

    #[test]
    fn the_updater_is_given_model_import_and_the_id_and_never_the_token() {
        let cmd = import_command(Path::new(crate::backups::UPDATER), four());
        let std_cmd = cmd.as_std();
        assert_eq!(std_cmd.get_program(), crate::backups::UPDATER);
        assert_eq!(std_cmd.get_args().collect::<Vec<_>>(), ["model-import", FOUR]);
        let envs: Vec<_> = std_cmd.get_envs().collect();
        assert_eq!(envs, [(std::ffi::OsStr::new(crate::access::TOKEN_ENV), None)], "the token is taken out and nothing is added");
    }

    /// The upload's rules are the updater's, each written twice: the
    /// command, the reserve, and the answer for a model that is there.
    #[test]
    fn the_uploads_rules_are_the_updaters() {
        let read = |rel: &str| std::fs::read_to_string(Path::new(env!("CARGO_MANIFEST_DIR")).join("..").join(rel)).unwrap_or_else(|e| panic!("{rel}: {e}"));
        let cli = read("embra-update/src/cli.rs");
        assert!(cli.contains("\"model-import\" =>"), "the updater has its model-import command");
        let models = read("embra-update/src/models.rs");
        assert!(models.contains("enough_space(free, model.bytes, RESERVE_BYTES)"), "the updater holds the same reserve for a model's file");
        assert!(models.contains("is installed and verified; nothing to do"), "the updater takes no second file for an installed model");
        assert_eq!(MODELS_ROOT, "/embra/data/models");
    }
}
