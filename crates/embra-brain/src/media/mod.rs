//! Media — operator-attached and tool-produced images.
//!
//! Ownership split:
//! - `ingest`: sniff + normalize bytes (decode limits, EXIF orientation,
//!   downscale to the vision tier's long edge, re-encode ladder).
//! - `store`: the `/embra/workspace/MEDIA/` file store — id grammar,
//!   atomic writes, sidecars, reads that re-sniff before serving.
//! - `replay`: session history → IR with the inline-image ceiling.
//! - `text`: the uploads that are not images — plain files under
//!   `/embra/workspace/uploads/`, handled by path.
//! - `offer`: a workspace file handed to the operator as a download
//!   (`file_offer`, `/download`, the `GetFile` RPC).
//!
//! Images NEVER ride a `String` path: not the tool-result text (the 2 MiB
//! byte cap would cut a base64 payload silently), not the persisted turn
//! content, not the proto `content` field. They travel as
//! `ToolImage` (raw bytes, tools-core) → `ImageData` (base64, IR) →
//! per-provider wire blocks, and persist as `AttachmentRef`s pointing at
//! the store.

pub mod ingest;
pub mod offer;
pub mod replay;
pub mod store;
pub mod text;

use base64::{engine::general_purpose::STANDARD, Engine as _};
use embra_tools_core::ToolImage;

use crate::provider::ir::ImageData;
use crate::provider::ProviderKind;

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
/// Pixel ceiling, checked from the header BEFORE decode: 64 megapixels,
/// the area of the former 8000×8000 per-edge cap, in any shape. A
/// full-page screenshot is tall and narrow (800×20,000 px is 16
/// megapixels, 61 MiB decoded); the per-edge cap refused it while it
/// admitted ten times that memory in a square. A decoded RGBA buffer at
/// this cap is 256 MiB, half the decoder's allocation ceiling below. The
/// vision API's own 8000×8000 limit applies to what is sent, and nothing
/// is sent above [`MEDIA_LONG_EDGE_MAX`].
pub const MEDIA_DECODE_PIXELS_MAX: u64 = 64_000_000;
/// Decoder allocation ceiling (`image::Limits::max_alloc`): the belt to
/// the header check's brace.
pub const MEDIA_DECODE_ALLOC_MAX: u64 = 512 * 1024 * 1024;
/// Images accepted on one user message (explicit ids ∪ staged).
pub const MEDIA_MAX_PER_MESSAGE: usize = 10;
/// Inline-replay ceiling, newest-first over the session history.
pub const MEDIA_HISTORY_MAX_IMAGES: usize = 20;
/// Inline-replay byte ceiling (raw bytes; ≈ ×4/3 on the wire — 16 MiB
/// raw ≈ 21 MB base64, under Anthropic's 32 MB request cap with room for
/// text). Gemini's cap is lower: see [`history_image_bytes_for`].
pub const MEDIA_HISTORY_MAX_BYTES: u64 = 16 * 1024 * 1024;
/// The inline-replay byte ceiling on Gemini, which caps a request that
/// carries inline images at 20 MB in all: "text prompts, system
/// instructions, and inline bytes" (ai.google.dev, image understanding).
/// 12 MiB raw ≈ 16.8 MB in base64 leaves over 3 MB of it for the system
/// prompt, the tools and the history text.
pub const MEDIA_HISTORY_MAX_BYTES_GEMINI: u64 = 12 * 1024 * 1024;

/// The inline-replay byte ceiling for the provider a turn goes to.
pub fn history_image_bytes_for(kind: ProviderKind) -> u64 {
    match kind {
        ProviderKind::Gemini => MEDIA_HISTORY_MAX_BYTES_GEMINI,
        ProviderKind::Anthropic | ProviderKind::Ollama | ProviderKind::LmStudio => {
            MEDIA_HISTORY_MAX_BYTES
        }
    }
}
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

    /// Gemini's request cap with inline images (ai.google.dev, image
    /// understanding): 20 MB for text, system instructions and inline
    /// bytes together.
    const GEMINI_INLINE_REQUEST_MAX: u64 = 20 * 1_000_000;

    #[test]
    fn caps_pinned() {
        assert_eq!(MEDIA_UPLOAD_MAX, 12 * 1024 * 1024);
        assert_eq!(MEDIA_INLINE_MAX, 1_572_864);
        assert_eq!(MEDIA_LONG_EDGE_MAX, 2576);
        assert_eq!(MEDIA_DECODE_PIXELS_MAX, 64_000_000);
        // A decoded RGBA buffer at the pixel cap fits under the decoder's
        // allocation ceiling with room for the decoder's own buffers.
        const { assert!(MEDIA_DECODE_PIXELS_MAX * 4 * 2 <= MEDIA_DECODE_ALLOC_MAX) };
        assert_eq!(MEDIA_MAX_PER_MESSAGE, 10);
        assert_eq!(MEDIA_HISTORY_MAX_IMAGES, 20);
        assert_eq!(MEDIA_HISTORY_MAX_BYTES, 16 * 1024 * 1024);
        assert_eq!(MEDIA_HISTORY_MAX_BYTES_GEMINI, 12 * 1024 * 1024);
        assert_eq!(text::TEXT_UPLOAD_MAX, 2 * 1024 * 1024);
        assert_eq!(text::TEXT_INLINE_MAX, 48 * 1024);
        assert_eq!(text::TEXT_HISTORY_MAX_BYTES, 256 * 1024);
        const { assert!(text::TEXT_INLINE_MAX <= text::TEXT_UPLOAD_MAX) };
        const { assert!(text::TEXT_INLINE_MAX as u64 <= text::TEXT_HISTORY_MAX_BYTES) };
        // A text upload is checked after the image gate: its cap sits
        // under the upload gate, or the gate's message would be the one
        // the operator reads.
        const { assert!(text::TEXT_UPLOAD_MAX <= MEDIA_UPLOAD_MAX) };
        // Relations between constants are checked when this test is
        // compiled, not when it runs: breaking one stops the build.
        // The replay ceiling must stay under Anthropic's 32 MB request
        // cap after base64 inflation (×4/3).
        const { assert!(MEDIA_HISTORY_MAX_BYTES * 4 / 3 < 32 * 1_000_000) };
        // Gemini's stays under its 20 MB cap with 3 MB left for the text.
        const { assert!(MEDIA_HISTORY_MAX_BYTES_GEMINI * 4 / 3 + 3 * 1_000_000 < GEMINI_INLINE_REQUEST_MAX) };
        // One inline image can never exceed the API's per-image cap.
        const { assert!(MEDIA_INLINE_MAX * 4 / 3 < 10 * 1_000_000) };
    }

    /// Each provider's replay budget fits its own request cap.
    #[test]
    fn each_provider_replays_images_under_its_request_cap() {
        assert_eq!(history_image_bytes_for(ProviderKind::Gemini), MEDIA_HISTORY_MAX_BYTES_GEMINI);
        for kind in [ProviderKind::Anthropic, ProviderKind::Ollama, ProviderKind::LmStudio] {
            assert_eq!(history_image_bytes_for(kind), MEDIA_HISTORY_MAX_BYTES, "{kind:?}");
        }
        assert!(history_image_bytes_for(ProviderKind::Gemini) * 4 / 3 < GEMINI_INLINE_REQUEST_MAX);
    }
}
