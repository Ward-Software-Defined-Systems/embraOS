//! `PUT /api/media` (upload) and `GET /api/media/{id}` (serve) — the
//! browser's door to the brain's MEDIA store, via apid's unary
//! `PutMedia` / `GetMedia` (same fresh-channel pattern as `/api/stop`).
//!
//! Upload is a raw body (no multipart — reqwest/axum multipart is not in
//! the tree and the browser can `fetch` a Blob body directly):
//!   headers `Content-Type: image/*` (a hint; the brain sniffs bytes),
//!           `X-Embra-Name: <percent-encoded filename>`,
//!           `X-Embra-Session: <session name>` (staging owner, optional).
//! Response JSON `{id, media_type, width, height, bytes, name, url}`.
//!
//! Serving sets `Content-Type` from the store's sidecar, `nosniff`, an
//! immutable cache policy (ids are content-stable) and an inline
//! disposition with the sanitized name. Ids are validated here BEFORE the
//! RPC so a malformed path never reaches the brain.
//!
//! Trust model: unchanged from the rest of `/api/*` — TLS only, no auth
//! (the `/ws/chat` channel already drives tools and writes files). What
//! this route adds is a disk-fill / decode vector, bounded by the body
//! limit here and the brain's size/dimension caps.

use axum::Json;
use axum::body::Bytes;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use embra_common::proto::apid::embra_api_client::EmbraApiClient;
use embra_common::proto::apid::{GetMediaRequest, PutMediaRequest};
use embra_common::proto::brain;
use prost::Message as ProstMessage;
use serde_json::{Value, json};
use tonic::transport::Channel;

use crate::state::AppState;

/// Mirrors the brain's `MEDIA_UPLOAD_MAX` (12 MiB). The router's
/// `DefaultBodyLimit` is this plus slack so the brain — not axum's
/// default 2 MiB limit — is what rejects an oversize upload with a clear
/// message.
pub const MEDIA_UPLOAD_MAX: usize = 12 * 1024 * 1024;
pub const MEDIA_BODY_LIMIT: usize = MEDIA_UPLOAD_MAX + 64 * 1024;
const NAME_MAX: usize = 120;

fn client(apid_addr: &str) -> Result<EmbraApiClient<Channel>, String> {
    let endpoint = Channel::from_shared(apid_addr.to_string())
        .map_err(|e| format!("invalid apid endpoint: {e}"))?;
    Ok(EmbraApiClient::new(endpoint.connect_lazy())
        .max_decoding_message_size(embra_common::GRPC_MAX_MESSAGE_BYTES))
}

/// Strict media-id grammar (mirror of the brain's `parse_media_id`):
/// `att-|gen-<YYYYMMDDTHHMMSSZ>-<8 hex>` or `view-<16 hex>`.
pub fn valid_media_id(id: &str) -> bool {
    if id.is_empty() || id.len() > 40 {
        return false;
    }
    let Some((prefix, rest)) = id.split_once('-') else {
        return false;
    };
    let hex = |s: &str| !s.is_empty() && s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    match prefix {
        "view" => rest.len() == 16 && hex(rest),
        "att" | "gen" => {
            let Some((stamp, h)) = rest.split_once('-') else {
                return false;
            };
            stamp.len() == 16
                && stamp.as_bytes()[8] == b'T'
                && stamp.as_bytes()[15] == b'Z'
                && stamp[..8].bytes().all(|b| b.is_ascii_digit())
                && stamp[9..15].bytes().all(|b| b.is_ascii_digit())
                && h.len() == 8
                && hex(h)
        }
        _ => false,
    }
}

/// Minimal percent-decoding for the `X-Embra-Name` header (UTF-8 names
/// can't ride a header raw). Invalid sequences pass through as-is.
fn percent_decode(s: &str) -> String {
    let bytes = s.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() {
            let hex = &s[i + 1..i + 3];
            if let Ok(v) = u8::from_str_radix(hex, 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn header_str<'a>(headers: &'a HeaderMap, name: &str) -> &'a str {
    headers
        .get(name)
        .and_then(|v| v.to_str().ok())
        .unwrap_or("")
}

/// Strip anything that would break a `Content-Disposition` quoted string.
fn disposition_name(name: &str) -> String {
    name.chars()
        .filter(|c| !c.is_control() && *c != '"' && *c != '\\')
        .take(NAME_MAX)
        .collect()
}

fn json_error(status: StatusCode, msg: String) -> Response {
    (status, Json(json!({ "error": msg }))).into_response()
}

fn status_from_tonic(code: tonic::Code) -> StatusCode {
    match code {
        tonic::Code::InvalidArgument => StatusCode::BAD_REQUEST,
        tonic::Code::NotFound => StatusCode::NOT_FOUND,
        tonic::Code::ResourceExhausted => StatusCode::PAYLOAD_TOO_LARGE,
        tonic::Code::Unavailable => StatusCode::SERVICE_UNAVAILABLE,
        _ => StatusCode::BAD_GATEWAY,
    }
}

pub fn media_json(m: &brain::MediaRef) -> Value {
    json!({
        "id": m.id,
        "media_type": m.media_type,
        "width": m.width,
        "height": m.height,
        "bytes": m.byte_size,
        "name": m.name,
        "origin": m.origin,
        "path": m.path,
        "url": format!("/api/media/{}", m.id),
    })
}

pub async fn api_media_put(State(st): State<AppState>, headers: HeaderMap, body: Bytes) -> Response {
    if body.is_empty() {
        return json_error(StatusCode::BAD_REQUEST, "empty upload".into());
    }
    if body.len() > MEDIA_UPLOAD_MAX {
        return json_error(
            StatusCode::PAYLOAD_TOO_LARGE,
            format!("image is {} bytes; the limit is {} bytes", body.len(), MEDIA_UPLOAD_MAX),
        );
    }
    let name: String = percent_decode(header_str(&headers, "x-embra-name"))
        .chars()
        .filter(|c| !c.is_control())
        .take(NAME_MAX)
        .collect();
    let session = header_str(&headers, "x-embra-session").to_string();
    let media_type_hint = header_str(&headers, "content-type").to_string();
    let mut client = match client(&st.apid_addr) {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, e),
    };
    let resp = client
        .put_media(PutMediaRequest {
            session_name: session,
            name,
            media_type_hint,
            data: body.to_vec(),
        })
        .await;
    match resp {
        Ok(r) => match brain::PutMediaResponse::decode(r.into_inner().payload.as_slice()) {
            Ok(decoded) => match decoded.media {
                Some(m) => Json(media_json(&m)).into_response(),
                None => json_error(StatusCode::BAD_GATEWAY, "brain returned no media meta".into()),
            },
            Err(e) => json_error(StatusCode::BAD_GATEWAY, format!("decode brain response: {e}")),
        },
        Err(status) => json_error(status_from_tonic(status.code()), status.message().to_string()),
    }
}

pub async fn api_media_get(State(st): State<AppState>, Path(id): Path<String>) -> Response {
    if !valid_media_id(&id) {
        return json_error(StatusCode::BAD_REQUEST, format!("invalid media id '{id}'"));
    }
    let mut client = match client(&st.apid_addr) {
        Ok(c) => c,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, e),
    };
    let resp = match client.get_media(GetMediaRequest { id: id.clone() }).await {
        Ok(r) => r,
        Err(status) => return json_error(status_from_tonic(status.code()), status.message().to_string()),
    };
    let decoded = match brain::GetMediaResponse::decode(resp.into_inner().payload.as_slice()) {
        Ok(d) => d,
        Err(e) => return json_error(StatusCode::BAD_GATEWAY, format!("decode brain response: {e}")),
    };
    let Some(meta) = decoded.media else {
        return json_error(StatusCode::BAD_GATEWAY, "brain returned no media meta".into());
    };
    media_response(&meta, decoded.data)
}

/// Build the byte response with the store-declared type and the
/// immutable/nosniff header set (unit-tested).
pub fn media_response(meta: &brain::MediaRef, data: Vec<u8>) -> Response {
    let media_type = if matches!(
        meta.media_type.as_str(),
        "image/png" | "image/jpeg" | "image/gif" | "image/webp"
    ) {
        meta.media_type.clone()
    } else {
        "application/octet-stream".to_string()
    };
    let mut resp = (StatusCode::OK, data).into_response();
    let h = resp.headers_mut();
    if let Ok(v) = HeaderValue::from_str(&media_type) {
        h.insert(header::CONTENT_TYPE, v);
    }
    h.insert(header::CACHE_CONTROL, HeaderValue::from_static("private, max-age=31536000, immutable"));
    h.insert(header::X_CONTENT_TYPE_OPTIONS, HeaderValue::from_static("nosniff"));
    let disposition = format!("inline; filename=\"{}\"", disposition_name(&meta.name));
    if let Ok(v) = HeaderValue::from_str(&disposition) {
        h.insert(header::CONTENT_DISPOSITION, v);
    }
    resp
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn media_id_grammar_mirrors_the_brain() {
        assert!(valid_media_id("att-20260820T153012Z-1a2b3c4d"));
        assert!(valid_media_id("gen-20260820T153012Z-deadbeef"));
        assert!(valid_media_id("view-0123456789abcdef"));
        for bad in ["", "../x", "att-20260820T153012Z-1A2B3C4D", "att-20260820T153012Z-1a2b3c4d.png", "x-y", "view-0123"] {
            assert!(!valid_media_id(bad), "{bad}");
        }
    }

    #[test]
    fn media_get_headers_are_nosniff_immutable() {
        let meta = brain::MediaRef {
            id: "att-20260820T153012Z-1a2b3c4d".into(),
            media_type: "image/png".into(),
            width: 1,
            height: 1,
            byte_size: 4,
            name: "a \"quoted\".png".into(),
            origin: "attached".into(),
            path: String::new(),
            caption: String::new(),
            tool_use_id: String::new(),
            replay: false,
        };
        let resp = media_response(&meta, vec![1, 2, 3, 4]);
        assert_eq!(resp.status(), StatusCode::OK);
        let h = resp.headers();
        assert_eq!(h.get(header::CONTENT_TYPE).unwrap(), "image/png");
        assert_eq!(h.get(header::CACHE_CONTROL).unwrap(), "private, max-age=31536000, immutable");
        assert_eq!(h.get(header::X_CONTENT_TYPE_OPTIONS).unwrap(), "nosniff");
        assert_eq!(h.get(header::CONTENT_DISPOSITION).unwrap(), "inline; filename=\"a quoted.png\"");
        // An unexpected type from the store never becomes an active MIME.
        let mut svg = meta.clone();
        svg.media_type = "image/svg+xml".into();
        let resp = media_response(&svg, vec![]);
        assert_eq!(resp.headers().get(header::CONTENT_TYPE).unwrap(), "application/octet-stream");
    }

    #[test]
    fn percent_decode_handles_utf8_and_passthrough() {
        assert_eq!(percent_decode("caf%C3%A9.png"), "café.png");
        assert_eq!(percent_decode("plain.png"), "plain.png");
        assert_eq!(percent_decode("bad%zz"), "bad%zz");
        assert_eq!(percent_decode("trail%"), "trail%");
    }

    #[test]
    fn body_limit_leaves_room_for_the_brain_to_answer() {
        assert!(MEDIA_BODY_LIMIT > MEDIA_UPLOAD_MAX);
        assert!(MEDIA_BODY_LIMIT < embra_common::GRPC_MAX_MESSAGE_BYTES);
    }
}
