//! Media — operator-attached and tool-produced images.
//!
//! Ownership split:
//! - `ingest`: sniff + normalize bytes (decode limits, EXIF orientation,
//!   downscale to the vision tier's long edge, re-encode ladder).
//! - `store`: the `/embra/workspace/MEDIA/` file store — id grammar,
//!   atomic writes, sidecars, reads that re-sniff before serving.
//! - `replay`: session history → IR with the inline-image ceiling.
//!
//! Images NEVER ride a `String` path: not the tool-result text (the 2 MiB
//! byte cap would cut a base64 payload silently), not the persisted turn
//! content, not the proto `content` field. They travel as
//! `ToolImage` (raw bytes, tools-core) → `ImageData` (base64, IR) →
//! per-provider wire blocks, and persist as `AttachmentRef`s pointing at
//! the store.

pub mod ingest;
pub mod replay;
pub mod store;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use embra_tools_core::ToolImage;

use crate::provider::ir::ImageData;

pub use store::{MediaMeta, MediaOrigin, MediaStore};

/// Largest upload / import / generated file accepted (raw bytes). Also
/// the ceiling every `GetMedia` response stays under, so
/// `embra_common::GRPC_MAX_MESSAGE_BYTES` (16 MiB) always fits it plus
/// framing.
pub const MEDIA_UPLOAD_MAX: usize = 12 * 1024 * 1024;
/// Largest NORMALIZED image handed to a model inline (per image). Bounds
/// the per-turn request growth under the inline-replay policy.
pub const MEDIA_INLINE_MAX: usize = 1_572_864;
/// Longest edge after normalization. Matches the high-resolution vision
/// tier (Claude 4.7+): the API downscales anything larger server-side, so
/// more pixels are pure payload.
pub const MEDIA_LONG_EDGE_MAX: u32 = 2576;
/// Header-declared dimension ceiling checked BEFORE decode (the vision
/// API's own 8000×8000 limit; a decode bomb is refused here).
pub const MEDIA_DECODE_DIM_MAX: u32 = 8000;
/// Decoder allocation ceiling (`image::Limits::max_alloc`).
pub const MEDIA_DECODE_ALLOC_MAX: u64 = 512 * 1024 * 1024;
/// Images accepted on one user message (explicit ids ∪ staged).
pub const MEDIA_MAX_PER_MESSAGE: usize = 10;
/// Inline-replay ceiling, newest-first over the session history.
pub const MEDIA_HISTORY_MAX_IMAGES: usize = 20;
/// Inline-replay byte ceiling (raw bytes; ≈ ×4/3 on the wire — 16 MiB
/// raw ≈ 21 MB base64, under the 32 MB request cap with room for text).
pub const MEDIA_HISTORY_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// Operator-facing name cap (original filename).
pub const MEDIA_NAME_MAX: usize = 120;
/// Concurrent normalizations (decode is CPU + memory heavy).
pub const MEDIA_NORMALIZE_CONCURRENCY: usize = 2;
/// JPEG quality for the re-encode ladder.
pub const JPEG_QUALITY: u8 = 85;

/// Proto `MediaRef` from a stored meta (the operator-facing frame).
pub fn media_ref_frame(
    meta: &MediaMeta,
    dir: &std::path::Path,
    replay: bool,
    tool_use_id: &str,
    caption: &str,
) -> embra_common::proto::brain::MediaRef {
    embra_common::proto::brain::MediaRef {
        id: meta.id.clone(),
        media_type: meta.media_type.clone(),
        width: meta.width,
        height: meta.height,
        byte_size: meta.byte_size,
        name: meta.name.clone(),
        origin: meta.origin.as_str().to_string(),
        path: meta.path_in(dir).display().to_string(),
        caption: caption.to_string(),
        tool_use_id: tool_use_id.to_string(),
        replay,
    }
}

/// Proto `MediaRef` rebuilt from a persisted `AttachmentRef` (history
/// replay on SessionAttach). `origin` is inferred from the id prefix.
pub fn media_ref_from_attachment(
    r: &crate::brain::AttachmentRef,
) -> embra_common::proto::brain::MediaRef {
    let origin = store::parse_media_id(&r.id)
        .map(|o| o.as_str().to_string())
        .unwrap_or_else(|_| "attached".to_string());
    embra_common::proto::brain::MediaRef {
        id: r.id.clone(),
        media_type: r.media_type.clone(),
        width: r.width,
        height: r.height,
        byte_size: r.bytes,
        name: r.name.clone(),
        origin,
        path: r.path.clone(),
        caption: String::new(),
        tool_use_id: String::new(),
        replay: true,
    }
}

/// Proto `MediaRef` from a tool's `MediaRefMeta` (tool loop emit).
pub fn media_ref_from_tool(
    m: &embra_tools_core::MediaRefMeta,
    tool_use_id: &str,
) -> embra_common::proto::brain::MediaRef {
    embra_common::proto::brain::MediaRef {
        id: m.id.clone(),
        media_type: m.media_type.clone(),
        width: m.width,
        height: m.height,
        byte_size: m.byte_size,
        name: m.name.clone(),
        origin: m.origin.clone(),
        path: m.path.clone(),
        caption: m.caption.clone(),
        tool_use_id: tool_use_id.to_string(),
        replay: false,
    }
}

/// Persisted ref for an image a tool produced/viewed.
pub fn attachment_ref_from_tool(m: &embra_tools_core::MediaRefMeta) -> crate::brain::AttachmentRef {
    crate::brain::AttachmentRef {
        id: m.id.clone(),
        name: m.name.clone(),
        media_type: m.media_type.clone(),
        width: m.width,
        height: m.height,
        bytes: m.byte_size,
        path: m.path.clone(),
    }
}

/// Convert a tool's raw images into IR images (base64 at this boundary,
/// exactly once).
pub fn tool_images_to_ir(images: Vec<ToolImage>) -> Vec<ImageData> {
    images
        .into_iter()
        .map(|img| ImageData {
            media_type: img.media_type,
            data_b64: std::sync::Arc::from(STANDARD.encode(&img.data)),
            width: img.width,
            height: img.height,
            name: img.name,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_images_to_ir_base64_encodes_once() {
        let out = tool_images_to_ir(vec![ToolImage {
            media_type: "image/png".into(),
            data: vec![0x89, b'P', b'N', b'G'],
            width: 1,
            height: 1,
            name: "px.png".into(),
            media_ref: None,
        }]);
        assert_eq!(out.len(), 1);
        assert_eq!(&*out[0].data_b64, "iVBORw==");
        assert_eq!(out[0].media_type, "image/png");
        assert_eq!(out[0].name, "px.png");
    }

    #[test]
    fn caps_pinned() {
        assert_eq!(MEDIA_UPLOAD_MAX, 12 * 1024 * 1024);
        assert_eq!(MEDIA_INLINE_MAX, 1_572_864);
        assert_eq!(MEDIA_LONG_EDGE_MAX, 2576);
        assert_eq!(MEDIA_MAX_PER_MESSAGE, 10);
        assert_eq!(MEDIA_HISTORY_MAX_IMAGES, 20);
        assert_eq!(MEDIA_HISTORY_MAX_BYTES, 16 * 1024 * 1024);
        // The replay ceiling must stay under the vision API's 32 MB
        // request cap after base64 inflation (×4/3).
        assert!(MEDIA_HISTORY_MAX_BYTES * 4 / 3 < 32 * 1_000_000);
        // One inline image can never exceed the API's per-image cap.
        assert!(MEDIA_INLINE_MAX * 4 / 3 < 10 * 1_000_000);
    }
}
