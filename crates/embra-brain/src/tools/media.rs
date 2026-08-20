//! Media tools — `image_view` (Wave 1) and `image_generate` (Wave 2).
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
use crate::provider::image::{
    self as image_provider, ImageGenError, ImageGenRequest, ImageProvider, OutputFormat, IMAGE_GEN_TIMEOUT,
};

/// Reference images accepted per generation call.
pub const IMAGE_GENERATE_MAX_REFERENCES: usize = 4;

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


#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "image_generate",
    description = "Generate an image from a text prompt with the configured image backend (Gemini image models; default gemini-3-pro-image — the operator sets it with /image-provider). The result is written to /embra/workspace/MEDIA/gen-<id>.<png|jpg> at full resolution and a JSON summary {id, path, width, height, bytes, media_type, model, provider} is returned; unless return_image=false the image is ALSO returned to you as an image block so you can check it against the prompt before telling the operator. aspect_ratio: 1:1|3:2|2:3|3:4|4:3|4:5|5:4|9:16|16:9|21:9. size: 512px|1K|2K|4K — 4K only with output_format=jpeg (a 4K PNG exceeds the 12 MiB store ceiling); gemini-3.1-flash-lite-image renders 1K only. output_format: png (default) | jpeg. reference_images: up to 4 media ids or workspace image paths used as visual references or edit sources (for edits, describe the change in the prompt). Not configured → an explanation naming /image-provider. Side-effectful: writes a file and spends API credits.",
    is_side_effectful = true
)]
pub struct ImageGenerateArgs {
    /// What to render. Be concrete: subject, style, composition, text to include.
    pub prompt: String,
    /// One of 1:1, 3:2, 2:3, 3:4, 4:3, 4:5, 5:4, 9:16, 16:9, 21:9.
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// One of 512px, 1K, 2K, 4K (4K requires output_format=jpeg).
    #[serde(default)]
    pub size: Option<String>,
    /// png (default) or jpeg.
    #[serde(default)]
    pub output_format: Option<String>,
    /// Media ids (att-/gen-/view-…) or workspace image paths, max 4.
    #[serde(default)]
    pub reference_images: Option<Vec<String>>,
    /// Return the generated image to you as an image block (default true).
    #[serde(default)]
    pub return_image: Option<bool>,
}

impl ImageGenerateArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<ToolOutput, DispatchError> {
        let provider = match image_provider::resolve_image_provider(ctx.config) {
            Ok(p) => p,
            Err(e) => return Ok(ToolOutput::text(format!("image_generate: {e}"))),
        };
        image_generate(&MediaStore::default_store(), provider.as_ref(), self, ctx.session_name).await
    }
}

/// Short filesystem-friendly name from the prompt (operator-facing).
pub fn slug_from_prompt(prompt: &str, ext: &str) -> String {
    let mut slug = String::new();
    let mut last_dash = true;
    for c in prompt.chars().flat_map(|c| c.to_lowercase()) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
            last_dash = false;
        } else if !last_dash {
            slug.push('-');
            last_dash = true;
        }
        if slug.len() >= 40 {
            break;
        }
    }
    let slug = slug.trim_matches('-');
    if slug.is_empty() {
        format!("generated.{ext}")
    } else {
        format!("{slug}.{ext}")
    }
}

/// Load one reference image (media id or workspace path — `file_read`'s
/// unjailed read policy), normalized so the request stays small.
async fn load_reference(store: &MediaStore, spec: &str) -> Result<(String, Vec<u8>), String> {
    let bytes = if store::parse_media_id(spec).is_ok() {
        store.get(spec).await.map(|(_, b)| b).map_err(|e| e.to_string())?
    } else {
        let resolved = store::resolve_read_path(spec);
        let md = tokio::fs::metadata(&resolved)
            .await
            .map_err(|e| format!("{}: {e}", resolved.display()))?;
        if !md.is_file() {
            return Err(format!("{} is not a file", resolved.display()));
        }
        if md.len() as usize > MEDIA_UPLOAD_MAX {
            return Err(format!("{} is {} bytes; the limit is {}", resolved.display(), md.len(), MEDIA_UPLOAD_MAX));
        }
        tokio::fs::read(&resolved)
            .await
            .map_err(|e| format!("{}: {e}", resolved.display()))?
    };
    let n = ingest::normalize(bytes).await.map_err(|e| e.to_string())?;
    Ok((n.kind.media_type().to_string(), n.bytes))
}

/// Store-/provider-parameterized core (unit-tested with a fake provider).
pub async fn image_generate(
    store: &MediaStore,
    provider: &dyn ImageProvider,
    args: ImageGenerateArgs,
    session: &str,
) -> Result<ToolOutput, DispatchError> {
    let output_format = match args.output_format.as_deref() {
        None | Some("") => OutputFormat::Png,
        Some(s) => match OutputFormat::parse(s) {
            Some(f) => f,
            None => return Ok(ToolOutput::text(format!("image_generate: output_format '{s}' is not png or jpeg"))),
        },
    };
    let mut reference_images = Vec::new();
    if let Some(refs) = &args.reference_images {
        if refs.len() > IMAGE_GENERATE_MAX_REFERENCES {
            return Ok(ToolOutput::text(format!(
                "image_generate: {} reference images; the limit is {}",
                refs.len(),
                IMAGE_GENERATE_MAX_REFERENCES
            )));
        }
        for spec in refs {
            match load_reference(store, spec).await {
                Ok(pair) => reference_images.push(pair),
                Err(e) => return Ok(ToolOutput::text(format!("image_generate: reference '{spec}' — {e}"))),
            }
        }
    }
    let req = ImageGenRequest {
        prompt: args.prompt.clone(),
        aspect_ratio: args.aspect_ratio.clone().filter(|s| !s.trim().is_empty()),
        size: args.size.clone().filter(|s| !s.trim().is_empty()),
        output_format,
        reference_images,
    };
    if let Err(e) = image_provider::validate_request(provider.model(), &req) {
        return Ok(ToolOutput::text(format!("image_generate: {e}")));
    }

    let started = std::time::Instant::now();
    let generated = match tokio::time::timeout(IMAGE_GEN_TIMEOUT, provider.generate(&req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(ImageGenError::Api { status, body })) => {
            return Ok(ToolOutput::text(format!(
                "image_generate: the {} API answered {}: {}. Not retried — adjust the prompt/options or try again later.",
                provider.kind().as_str(),
                status,
                body
            )));
        }
        Ok(Err(e)) => return Ok(ToolOutput::text(format!("image_generate: {e}"))),
        Err(_) => {
            return Ok(ToolOutput::text(format!(
                "image_generate: no response within {}s — the request was abandoned (nothing was written). Try a smaller size or a faster model (/image-provider model gemini-3.1-flash-image).",
                IMAGE_GEN_TIMEOUT.as_secs()
            )));
        }
    };
    let Some(first) = generated.images.into_iter().next() else {
        return Ok(ToolOutput::text("image_generate: the API returned no image".to_string()));
    };
    // Trust the bytes, not the declared MIME: sniff + header dims, full-res
    // into the store, a normalized copy for the model.
    let (kind, width, height) = match ingest::probe_dimensions(&first.data) {
        Ok(t) => t,
        Err(e) => return Ok(ToolOutput::text(format!("image_generate: the API returned unusable image bytes: {e}"))),
    };
    if first.media_type != kind.media_type() {
        tracing::warn!(
            target: "media",
            declared = %first.media_type,
            sniffed = kind.media_type(),
            "image_generate: declared MIME disagrees with the bytes; trusting the bytes"
        );
    }
    let full = ingest::Normalized {
        bytes: first.data,
        kind,
        width,
        height,
        transformed: false,
    };
    let name = slug_from_prompt(&args.prompt, kind.ext());
    let meta = store
        .put(MediaOrigin::Generated, &name, session, full.clone())
        .await
        .map_err(|e| DispatchError::Handler(format!("image_generate: store write failed: {e}")))?;
    let path = meta.path_in(store.dir());
    let elapsed_ms = started.elapsed().as_millis();

    let summary = serde_json::json!({
        "id": meta.id,
        "path": path.display().to_string(),
        "width": meta.width,
        "height": meta.height,
        "bytes": meta.byte_size,
        "media_type": meta.media_type,
        "model": provider.model(),
        "provider": provider.kind().as_str(),
        "elapsed_ms": elapsed_ms,
    });
    let mut text = serde_json::to_string_pretty(&summary).unwrap_or_else(|_| summary.to_string());
    if let Some(t) = generated.text.as_deref() {
        text.push_str("\nModel note: ");
        text.push_str(t.trim());
    }
    let return_image = args.return_image.unwrap_or(true);
    let caption: String = args.prompt.chars().take(140).collect();
    let mut out = ToolOutput::text(String::new());
    if return_image {
        match ingest::normalize(full.bytes.clone()).await {
            Ok(n) => {
                text.push_str("\nThe generated image follows as an image block — check it against the prompt.");
                out = out.with_image(ToolImage {
                    media_type: n.kind.media_type().to_string(),
                    width: n.width,
                    height: n.height,
                    name: meta.name.clone(),
                    media_ref: Some(store::to_ref_meta(&meta, store.dir(), &caption)),
                    data: n.bytes,
                });
            }
            Err(e) => text.push_str(&format!("\n(The image was saved but could not be normalized for inline viewing: {e}; use image_view on the path.)")),
        }
    } else {
        // Display-only: the operator's UI gets the MediaRef frame, the
        // model gets the summary text only.
        text.push_str("\n(return_image=false: the operator's UI shows it; you did not receive the image — use image_view on the path if you need to inspect it.)");
        out = out.with_media_ref(store::to_ref_meta(&meta, store.dir(), &caption));
    }
    out.text = text;
    Ok(out)
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

    /// Fake backend: returns a fixed PNG (or an error) without the network.
    struct FakeProvider {
        result: std::sync::Mutex<Option<Result<image_provider::ImageGenResponse, ImageGenError>>>,
        seen: std::sync::Mutex<Option<ImageGenRequest>>,
        model: String,
    }

    impl FakeProvider {
        fn ok(model: &str, bytes: Vec<u8>) -> Self {
            FakeProvider {
                result: std::sync::Mutex::new(Some(Ok(image_provider::ImageGenResponse {
                    images: vec![image_provider::GeneratedImage { media_type: "image/png".into(), data: bytes }],
                    text: Some("Here it is.".into()),
                }))),
                seen: std::sync::Mutex::new(None),
                model: model.into(),
            }
        }
        fn err(model: &str, e: ImageGenError) -> Self {
            FakeProvider {
                result: std::sync::Mutex::new(Some(Err(e))),
                seen: std::sync::Mutex::new(None),
                model: model.into(),
            }
        }
    }

    #[async_trait::async_trait]
    impl ImageProvider for FakeProvider {
        fn kind(&self) -> image_provider::ImageProviderKind {
            image_provider::ImageProviderKind::Gemini
        }
        fn model(&self) -> &str {
            &self.model
        }
        async fn generate(&self, req: &ImageGenRequest) -> Result<image_provider::ImageGenResponse, ImageGenError> {
            *self.seen.lock().unwrap() = Some(req.clone());
            self.result.lock().unwrap().take().expect("one call")
        }
    }

    fn gen_args(prompt: &str) -> ImageGenerateArgs {
        ImageGenerateArgs {
            prompt: prompt.into(),
            aspect_ratio: None,
            size: None,
            output_format: None,
            reference_images: None,
            return_image: None,
        }
    }

    #[tokio::test]
    async fn image_generate_writes_gen_file_and_returns_summary() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::ok("gemini-3-pro-image", png_fixture(9, 6));
        let mut args = gen_args("A lighthouse at dusk, poster style!");
        args.aspect_ratio = Some("16:9".into());
        args.size = Some("2K".into());
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        // Request shape the backend saw.
        let seen = provider.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(seen.size.as_deref(), Some("2K"));
        assert_eq!(seen.output_format, OutputFormat::Png);
        // Summary JSON + model note + the image block.
        let json_part = out.text.split("\nModel note:").next().unwrap();
        let summary: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert!(summary["id"].as_str().unwrap().starts_with("gen-"));
        assert_eq!(summary["width"], 9);
        assert_eq!(summary["height"], 6);
        assert_eq!(summary["model"], "gemini-3-pro-image");
        assert_eq!(summary["provider"], "gemini");
        assert!(out.text.contains("Model note: Here it is."));
        assert!(out.text.contains("follows as an image block"));
        assert_eq!(out.images.len(), 1);
        let r = out.images[0].media_ref.as_ref().unwrap();
        assert_eq!(r.origin, "generated");
        assert_eq!(r.name, "a-lighthouse-at-dusk-poster-style.png");
        assert!(r.caption.starts_with("A lighthouse"));
        assert!(std::path::Path::new(&r.path).exists());
        assert_eq!(std::fs::read(&r.path).unwrap(), png_fixture(9, 6), "full-res bytes stored untouched");
        assert!(out.media_refs.is_empty());
    }

    #[tokio::test]
    async fn image_generate_return_image_false_is_display_only() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::ok("gemini-3-pro-image", png_fixture(3, 3));
        let mut args = gen_args("tiny");
        args.return_image = Some(false);
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(out.images.is_empty(), "nothing goes to the model");
        assert_eq!(out.media_refs.len(), 1, "but the operator UI is told");
        assert!(out.text.contains("return_image=false"));
    }

    #[tokio::test]
    async fn image_generate_rejects_bad_options_before_calling() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::ok("gemini-3-pro-image", png_fixture(3, 3));
        let mut args = gen_args("x");
        args.size = Some("4K".into()); // PNG by default → refused locally
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(out.text.contains("4K is only available with output_format=jpeg"), "{}", out.text);
        assert!(provider.seen.lock().unwrap().is_none(), "never called the backend");
        let mut args = gen_args("x");
        args.output_format = Some("webp".into());
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(out.text.contains("not png or jpeg"));
        let mut args = gen_args("x");
        args.reference_images = Some(vec!["nope-id".into()]);
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(out.text.contains("reference 'nope-id'"), "{}", out.text);
        assert_eq!(store.usage().await.unwrap().0, 0, "nothing written on refusal");
    }

    #[tokio::test]
    async fn image_generate_surfaces_api_errors_and_not_configured() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::err("gemini-3-pro-image", ImageGenError::Api { status: 429, body: "quota".into() });
        let out = image_generate(&store, &provider, gen_args("x"), "s").await.unwrap();
        assert!(out.text.contains("answered 429: quota"), "{}", out.text);
        assert!(out.images.is_empty());
        // Not configured at the resolve step (no key anywhere, no provider).
        let cfg = crate::config::tests_support::minimal_cfg();
        // SAFETY: test-only env isolation — ensure no image key leaks in.
        unsafe { std::env::remove_var(image_provider::IMAGE_KEY_ENV) };
        let err = match image_provider::resolve_image_provider(&cfg) {
            Ok(_) => panic!("a minimal config must not resolve a backend (a key is present in this environment?)"),
            Err(e) => e,
        };
        assert!(matches!(err, ImageGenError::NotConfigured(m) if m.contains("/image-provider key")));
    }

    #[test]
    fn slug_from_prompt_is_filesystem_friendly() {
        assert_eq!(slug_from_prompt("A lighthouse at dusk, poster style!", "png"), "a-lighthouse-at-dusk-poster-style.png");
        assert_eq!(slug_from_prompt("   ", "jpg"), "generated.jpg");
        assert!(slug_from_prompt(&"word ".repeat(40), "png").len() <= 45);
    }

    #[test]
    fn image_generate_description_and_schema() {
        let d = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "image_generate")
            .expect("image_generate registered");
        assert!(d.is_side_effectful);
        assert!(d.description.contains("/image-provider"));
        assert!(d.description.contains("4K only with output_format=jpeg"));
        let schema = (d.input_schema)();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("oneOf").is_none() && schema.get("anyOf").is_none() && schema.get("allOf").is_none());
        assert_eq!(schema["properties"]["prompt"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["prompt"]));
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
