//! `GET /api/files/{*path}` — a workspace file as a browser download, via
//! apid's unary `GetFile` (the media routes' fresh-channel pattern).
//!
//! The brain is the jail (`media/offer.rs`: under the workspace through
//! any symlink, a regular file, ≤ 12 MiB); this route only refuses what
//! could never be a workspace path, and then never lets what it serves
//! activate in the browser: every answer is `Content-Disposition:
//! attachment`, text-like types go out as `text/plain; charset=utf-8`
//! and everything else as `application/octet-stream`, with `nosniff` and
//! `Cache-Control: no-store`. An uploaded `.html` therefore downloads
//! instead of rendering on the console's origin.

use axum::extract::{Path, State};
use axum::http::{HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use embra_common::proto::apid::GetFileRequest;
use embra_common::proto::brain;
use prost::Message as ProstMessage;

use crate::media::{client, disposition_name, json_error, status_from_tonic};
use crate::state::AppState;

/// Longest path accepted on the route.
const PATH_MAX: usize = 1024;

/// What the route checks before the RPC: a non-empty path under the cap
/// with no `..` segment and no control character. The brain does the
/// rest.
pub fn valid_offer_path(path: &str) -> bool {
    !path.is_empty()
        && path.len() <= PATH_MAX
        && !path.split('/').any(|seg| seg == "..")
        && !path.chars().any(|c| c.is_control())
}

pub async fn api_file_get(State(st): State<AppState>, Path(path): Path<String>) -> Response {
    if !valid_offer_path(&path) {
        return json_error(StatusCode::BAD_REQUEST, format!("invalid file path '{path}'"));
    }
    let mut client = match client(&st.apid_addr) {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, e),
    };
    let resp = match client.get_file(GetFileRequest { path: path.clone() }).await {
        Ok(r) => r,
        Err(status) => return json_error(status_from_tonic(status.code()), status.message().to_string()),
    };
    let decoded = match brain::GetFileResponse::decode(resp.into_inner().payload.as_slice()) {
        Ok(d) => d,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, format!("decode brain response: {e}")),
    };
    let Some(meta) = decoded.file else {
        return json_error(StatusCode::BAD_GATEWAY, "brain returned no file meta".into());
    };
    file_response(&meta, decoded.data)
}

/// The type a download goes out as: text-like types as plain text, so a
/// browser never renders them; everything else as octet-stream.
pub fn download_content_type(media_type: &str) -> &'static str {
    let text_like = media_type.starts_with("text/")
        || matches!(media_type, "application/json" | "application/yaml" | "application/toml");
    if text_like {
        "text/plain; charset=utf-8"
    } else {
        "application/octet-stream"
    }
}

/// `filename*=UTF-8''…` per RFC 5987: the unreserved characters pass,
/// every other byte of the UTF-8 form is percent-encoded.
fn rfc5987_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for b in name.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => out.push(b as char),
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// The quoted fallback name: printable ASCII only, so the header value
/// is always valid; `download` when nothing is left.
fn ascii_name(name: &str) -> String {
    let kept: String = disposition_name(name)
        .chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .collect();
    if kept.trim().is_empty() {
        "download".to_string()
    } else {
        kept
    }
}

/// Build the download response (unit-tested): attachment disposition
/// with both name forms, a type that never activates, nosniff, no-store.
pub fn file_response(meta: &brain::FileRef, data: Vec<u8>) -> Response {
    let mut resp = (StatusCode::OK, data).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(download_content_type(&meta.media_type)),
    );
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let disposition = format!(
        "attachment; filename=\"{}\"; filename*=UTF-8''{}",
        ascii_name(&meta.name),
        rfc5987_name(&meta.name)
    );
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file(name: &str, media_type: &str) -> brain::FileRef {
        brain::FileRef {
            path: format!("/embra/workspace/uploads/{name}"),
            name: name.to_string(),
            byte_size: 3,
            media_type: media_type.to_string(),
            origin: "offered".into(),
            caption: String::new(),
            tool_use_id: String::new(),
            replay: false,
        }
    }

    #[test]
    fn offer_path_grammar_refuses_traversal_and_controls() {
        assert!(valid_offer_path("uploads/notes.md"));
        assert!(valid_offer_path("reports/weekly report (ü).md"));
        assert!(!valid_offer_path(""));
        assert!(!valid_offer_path("../etc/passwd"));
        assert!(!valid_offer_path("uploads/../../etc/passwd"));
        assert!(!valid_offer_path("uploads/a\nb.md"));
        assert!(!valid_offer_path("uploads/a\u{7}b.md"));
        assert!(!valid_offer_path(&"a".repeat(PATH_MAX + 1)));
        assert!(valid_offer_path(&"a".repeat(PATH_MAX)));
    }

    #[test]
    fn file_get_headers_are_attachment_nosniff_no_store_with_a_utf8_filename() {
        let resp = file_response(&file("weekly report (ü).md", "text/markdown"), b"abc".to_vec());
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers();
        assert_eq!(h[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert_eq!(h[header::CACHE_CONTROL], "no-store");
        assert_eq!(h[header::X_CONTENT_TYPE_OPTIONS], "nosniff");
        assert_eq!(
            h[header::CONTENT_DISPOSITION],
            "attachment; filename=\"weekly report ().md\"; filename*=UTF-8''weekly%20report%20%28%C3%BC%29.md"
        );
        // A name with nothing printable in ASCII still yields a valid header.
        let resp = file_response(&file("日本語", "application/octet-stream"), Vec::new());
        assert_eq!(
            resp.headers()[header::CONTENT_DISPOSITION],
            "attachment; filename=\"download\"; filename*=UTF-8''%E6%97%A5%E6%9C%AC%E8%AA%9E"
        );
    }

    #[test]
    fn an_uploaded_html_file_is_served_as_plain_text_and_a_binary_as_octet_stream() {
        assert_eq!(download_content_type("text/html"), "text/plain; charset=utf-8");
        assert_eq!(download_content_type("text/markdown"), "text/plain; charset=utf-8");
        assert_eq!(download_content_type("application/json"), "text/plain; charset=utf-8");
        assert_eq!(download_content_type("image/png"), "application/octet-stream");
        assert_eq!(download_content_type("application/pdf"), "application/octet-stream");
        assert_eq!(download_content_type(""), "application/octet-stream");
        let resp = file_response(&file("page.html", "text/html"), b"<b>".to_vec());
        assert_eq!(resp.headers()[header::CONTENT_TYPE], "text/plain; charset=utf-8");
        assert!(resp.headers()[header::CONTENT_DISPOSITION].to_str().unwrap().starts_with("attachment;"));
    }
}
