//! Media tools — `image_view` (Wave 1). `image_generate` lands in Wave 2.
//!
//! These are the only tools whose `run()` returns `ToolOutput` with
//! images: the bytes ride `ToolImage` (raw), become `Block::ToolResult.
//! images` in the IR, and each provider places them on its wire. The text
//! part is a one-line header so the model knows what it is looking at.

use embra_tool_macro::embra_tool;
use embra_tools_core::{DispatchError, ToolImage, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;

use super::registry::DispatchContext;
use crate::media::store::{self, MediaOrigin, MediaStore};
use crate::media::{ingest, MEDIA_UPLOAD_MAX};

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "image_view",
    description = "Look at an image file (PNG, JPEG, GIF or WebP) — the image itself is returned to you as an image block, not text. Use this instead of file_read for any image: screenshots, diagrams, photos, rendered charts, images the operator mentioned by path. path may be absolute or workspace-relative (/embra/workspace). The file is normalized to the vision tier before you see it (long edge ≤ 2576 px, ≤ 1.5 MiB; originals untouched). Images already in /embra/workspace/MEDIA keep their id; any other file gets a content-addressed view-<hash> copy there so the operator's UI can show what you looked at (viewing the same file twice reuses the copy). Files over 12 MiB and non-image files are refused with an explanation."
)]
pub struct ImageViewArgs {
    /// Path to the image file — absolute, or relative to /embra/workspace.
    pub path: String,
}

impl ImageViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<ToolOutput, DispatchError> {
        image_view(&MediaStore::default_store(), &self.path, ctx.session_name).await
    }
}

/// Store-parameterized core (unit-tested against a temp store). User-facing
/// problems come back as `Ok(text)` per house style; only store corruption
/// is a hard `DispatchError::Handler`.
pub async fn image_view(store: &MediaStore, path: &str, session: &str) -> Result<ToolOutput, DispatchError> {
    let resolved = store::resolve_read_path(path);
    let md = match tokio::fs::metadata(&resolved).await {
        Ok(md) => md,
        Err(e) => return Ok(ToolOutput::text(format!("image_view: {} — {}", resolved.display(), e))),
    };
    if !md.is_file() {
        return Ok(ToolOutput::text(format!("image_view: {} is not a file", resolved.display())));
    }
    if md.len() as usize > MEDIA_UPLOAD_MAX {
        return Ok(ToolOutput::text(format!(
            "image_view: {} is {} bytes; the limit is {} bytes",
            resolved.display(),
            md.len(),
            MEDIA_UPLOAD_MAX
        )));
    }

    // A file that IS a store entry keeps its id (no copy); anything else
    // is normalized into a content-addressed `view-` copy.
    let in_store = resolved.parent() == Some(store.dir())
        && resolved
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| store::parse_media_id(stem).is_ok());
    let (meta, data) = if in_store {
        let id = resolved.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        match store.get(&id).await {
            Ok(pair) => pair,
            Err(e) => return Err(DispatchError::Handler(format!("image_view: {e}"))),
        }
    } else {
        let bytes = match tokio::fs::read(&resolved).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::text(format!("image_view: {} — {}", resolved.display(), e))),
        };
        let normalized = match ingest::normalize(bytes).await {
            Ok(n) => n,
            Err(e) => return Ok(ToolOutput::text(format!("image_view: {} — {}", resolved.display(), e))),
        };
        let name = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_string();
        let data = normalized.bytes.clone();
        let meta = store
            .put(MediaOrigin::Viewed, &name, session, normalized)
            .await
            .map_err(|e| DispatchError::Handler(format!("image_view: store copy failed: {e}")))?;
        (meta, data)
    };

    let header = format!(
        "=== image {} ({}×{}, {} bytes, {}; media id {}) ===\nThe image follows as an image block.",
        resolved.display(),
        meta.width,
        meta.height,
        data.len(),
        meta.media_type,
        meta.id
    );
    let image = ToolImage {
        media_type: meta.media_type.clone(),
        width: meta.width,
        height: meta.height,
        name: meta.name.clone(),
        media_ref: Some(store::to_ref_meta(&meta, store.dir(), "")),
        data,
    };
    Ok(ToolOutput::text(header).with_image(image))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::media::ingest::tests::png_fixture;

    struct TempDir(std::path::PathBuf);
    impl TempDir {
        fn new() -> Self {
            let p = std::env::temp_dir().join(format!(
                "embra-image-view-{}-{}",
                std::process::id(),
                uuid::Uuid::new_v4().simple()
            ));
            std::fs::create_dir_all(&p).unwrap();
            TempDir(p)
        }
    }
    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    #[tokio::test]
    async fn image_view_rejects_non_image_and_missing() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let svg = dir.0.join("x.png");
        std::fs::write(&svg, b"<svg/>").unwrap();
        let out = image_view(&store, svg.to_str().unwrap(), "s").await.unwrap();
        assert!(out.images.is_empty());
        assert!(out.text.contains("not a supported image"), "{}", out.text);
        let out = image_view(&store, dir.0.join("nope.png").to_str().unwrap(), "s").await.unwrap();
        assert!(out.images.is_empty());
        assert!(out.text.starts_with("image_view:"));
        let out = image_view(&store, dir.0.to_str().unwrap(), "s").await.unwrap();
        assert!(out.text.contains("is not a file"));
    }

    #[tokio::test]
    async fn image_view_outside_media_makes_view_copy_once() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let src = dir.0.join("diagram.png");
        std::fs::write(&src, png_fixture(6, 4)).unwrap();
        let out = image_view(&store, src.to_str().unwrap(), "s").await.unwrap();
        assert_eq!(out.images.len(), 1);
        let img = &out.images[0];
        assert_eq!(img.media_type, "image/png");
        assert_eq!((img.width, img.height), (6, 4));
        assert_eq!(img.data, png_fixture(6, 4), "untransformed bytes pass through");
        let r = img.media_ref.as_ref().expect("store ref");
        assert!(r.id.starts_with("view-"));
        assert_eq!(r.origin, "viewed");
        assert!(out.text.starts_with("=== image "));
        assert!(out.text.contains(&r.id));
        // Second view of the same bytes → same id, one file.
        let again = image_view(&store, src.to_str().unwrap(), "s").await.unwrap();
        assert_eq!(again.images[0].media_ref.as_ref().unwrap().id, r.id);
        assert_eq!(store.usage().await.unwrap().0, 1);
        // Viewing the stored copy itself keeps its id (no new file).
        let stored_path = r.path.clone();
        let third = image_view(&store, &stored_path, "s").await.unwrap();
        assert_eq!(third.images[0].media_ref.as_ref().unwrap().id, r.id);
        assert_eq!(store.usage().await.unwrap().0, 1);
    }

    #[test]
    fn image_view_description_steers_image_files() {
        // Drift guard (precedent: file_read_description_steers_whole_reads):
        // the model must keep being told to use this instead of file_read.
        let desc = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "image_view")
            .expect("image_view registered")
            .description;
        assert!(desc.contains("instead of file_read"), "{desc}");
        assert!(desc.contains("image block"), "{desc}");
    }

    #[test]
    fn image_view_schema_is_plain_object() {
        let schema = (crate::tools::registry::all_descriptors()
            .find(|d| d.name == "image_view")
            .unwrap()
            .input_schema)();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("oneOf").is_none() && schema.get("anyOf").is_none() && schema.get("allOf").is_none());
        assert_eq!(schema["properties"]["path"]["type"], "string");
    }
}
