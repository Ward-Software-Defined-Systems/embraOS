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
use crate::media::{ingest, tiles, MEDIA_LONG_EDGE_MAX, MEDIA_TILES_PER_VIEW, MEDIA_UPLOAD_MAX};
use crate::provider::image::{self as image_provider, ImageGenError, ImageGenRequest, ImageProvider};

/// Reference images accepted per generation call.
pub const IMAGE_GENERATE_MAX_REFERENCES: usize = 4;

// One part of a tiled view is one tool result: the dispatcher's image cap
// must hold every tile, or `apply_caps` would drop the last ones with a note.
const _: () = assert!(MEDIA_TILES_PER_VIEW <= super::registry::MAX_TOOL_RESULT_IMAGES);

#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "image_view",
    description = "Look at an image file (PNG, JPEG, GIF or WebP) — the image itself is returned to you as an image block, not text. Use this instead of file_read for any image: screenshots, diagrams, photos, rendered charts, images the operator mentioned by path. path may be absolute or workspace-relative (/embra/workspace), under file_read's read rule (/proc, the operator's STATE partition and the database files are refused by their real path). The file is normalized to the vision tier before you see it (long edge ≤ 2576 px, ≤ 1.5 MiB; originals untouched; the header names the source size when it was shrunk). A long image, such as a full-page screenshot with a long edge over 5152 px, is cut along that axis into tiles that keep full scale, four tiles per call, the cuts on blank lines where there are any: the header names each tile's coordinates and the parts; part counts from 1 and part=2, 3, … shows the next group; on an image shown whole, part is ignored and the header says so. Images over 64 megapixels are refused. Images already in /embra/workspace/MEDIA keep their id; any other file gets a content-addressed view-<hash> copy there (one per tile) so the operator's UI can show what you looked at (viewing the same file twice reuses the copy). Files over 12 MiB and non-image files are refused with an explanation."
)]
pub struct ImageViewArgs {
    /// Path to the image file — absolute, or relative to /embra/workspace.
    pub path: String,
    /// For a long image shown in tiles: which group of four tiles to show, counting from 1 (the default). Ignored on an image shown whole.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub part: Option<u32>,
}

impl ImageViewArgs {
    pub async fn run(self, ctx: DispatchContext<'_>) -> Result<ToolOutput, DispatchError> {
        let part = self.part.map_or(1, |p| p as usize);
        image_view(&MediaStore::default_store(), &self.path, part, ctx.session_name).await
    }
}

/// A GIF stored before uploads became PNG goes to the model as the PNG of
/// its first frame, which every provider takes (Gemini takes no GIF). Any
/// other stored image is returned as stored.
async fn readable_by_every_provider(
    mut meta: store::MediaMeta,
    data: Vec<u8>,
) -> Result<(store::MediaMeta, Vec<u8>), String> {
    if meta.media_type != "image/gif" {
        return Ok((meta, data));
    }
    let n = ingest::normalize(data).await.map_err(|e| e.to_string())?;
    meta.media_type = n.kind.media_type().to_string();
    meta.width = n.width;
    meta.height = n.height;
    Ok((meta, n.bytes))
}

/// Store-parameterized core (unit-tested against a temp store). User-facing
/// problems come back as `Ok(text)` per house style; only store corruption
/// is a hard `DispatchError::Handler`.
pub async fn image_view(
    store: &MediaStore,
    path: &str,
    part: usize,
    session: &str,
) -> Result<ToolOutput, DispatchError> {
    // The read jail (`jail::readable`): the real path, or the refusal.
    let resolved = match crate::tools::jail::readable(path).await {
        Ok(p) => p,
        Err(e) => return Ok(ToolOutput::text(format!("image_view: {e}"))),
    };
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

    if part == 0 {
        return Ok(ToolOutput::text(format!(
            "image_view: {} — part counts from 1; part=1 is the first group of tiles",
            resolved.display()
        )));
    }

    // A file that IS a store entry keeps its id (no copy); anything else
    // is normalized into a content-addressed `view-` copy.
    let in_store = resolved.parent() == Some(store.dir())
        && resolved
            .file_stem()
            .and_then(|s| s.to_str())
            .is_some_and(|stem| store::parse_media_id(stem).is_ok());
    let (meta, data, source) = if in_store {
        let id = resolved.file_stem().and_then(|s| s.to_str()).unwrap_or_default().to_string();
        let (meta, data) = match store.get(&id).await {
            Ok(pair) => pair,
            Err(e) => return Err(DispatchError::Handler(format!("image_view: {e}"))),
        };
        match readable_by_every_provider(meta, data).await {
            Ok((meta, data)) => (meta, data, None),
            Err(e) => return Ok(ToolOutput::text(format!("image_view: {} — {}", resolved.display(), e))),
        }
    } else {
        let bytes = match tokio::fs::read(&resolved).await {
            Ok(b) => b,
            Err(e) => return Ok(ToolOutput::text(format!("image_view: {} — {}", resolved.display(), e))),
        };
        let name = resolved
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("image")
            .to_string();
        let (normalized, source) = match tiles::view(bytes, part).await {
            Ok(tiles::View::Whole { image, source_width, source_height }) => (image, Some((source_width, source_height))),
            Ok(tiles::View::Tiled(tiled)) => return tiled_output(store, &resolved, &name, session, tiled).await,
            Err(e) => return Ok(ToolOutput::text(format!("image_view: {} — {}", resolved.display(), e))),
        };
        let data = normalized.bytes.clone();
        let meta = store
            .put(MediaOrigin::Viewed, &name, session, normalized)
            .await
            .map_err(|e| DispatchError::Handler(format!("image_view: store copy failed: {e}")))?;
        (meta, data, source)
    };

    // A source brought down to the box is named with its size and the
    // scale, so a reader can tell a shrunk page from one shown as it is.
    let shrunk = source.filter(|&(sw, sh)| (sw, sh) != (meta.width, meta.height));
    let mut header = match shrunk {
        Some((sw, sh)) => format!(
            "=== image {} ({sw}×{sh} px, shown at {}×{}, {}%; {} bytes, {}; media id {}) ===\nThe image follows as an image block.",
            resolved.display(),
            meta.width,
            meta.height,
            percent(meta.width.max(meta.height), sw.max(sh)),
            data.len(),
            meta.media_type,
            meta.id
        ),
        None => format!(
            "=== image {} ({}×{}, {} bytes, {}; media id {}) ===\nThe image follows as an image block.",
            resolved.display(),
            meta.width,
            meta.height,
            data.len(),
            meta.media_type,
            meta.id
        ),
    };
    if part > 1 {
        header.push_str("\n(part applies to a long image shown in tiles; this image is shown whole.)");
    }
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

/// The tiled form of `image_view`: every tile a `Viewed` copy in the store
/// and an image block, the header naming where each tile was cut and
/// which parts remain.
async fn tiled_output(
    store: &MediaStore,
    resolved: &std::path::Path,
    name: &str,
    session: &str,
    tiled: tiles::Tiled,
) -> Result<ToolOutput, DispatchError> {
    let plan = tiled.plan;
    let coord = plan.axis.coordinate();
    let any_capped = tiled.tiles.iter().any(|t| brought_down(&t.rect, &t.image));
    let mut scale = if plan.scale_percent() == 100 {
        "at full scale".to_string()
    } else {
        format!("scaled to {}% ({} px on the short edge)", plan.scale_percent(), MEDIA_LONG_EDGE_MAX)
    };
    if any_capped {
        scale.push_str(" except where noted");
    }
    let mut text = format!(
        "=== image {} ({}×{} px; shown in {} tiles of {} px, {}, {}) ===\nPart {} of {}: {}.\n",
        resolved.display(),
        plan.width,
        plan.height,
        plan.total,
        plan.tile_size_text(&tiled.cuts),
        plan.axis.order(),
        scale,
        tiled.part,
        plan.parts(),
        plan.tiles_text(tiled.part)
    );
    let mut images = Vec::with_capacity(tiled.tiles.len());
    for tile in tiled.tiles {
        let (a, b) = plan.span(&tile.rect);
        let capped = if brought_down(&tile.rect, &tile.image) { " (brought down to fit the 1.5 MiB cap)" } else { "" };
        // The store keeps a name's basename, so no slash in it.
        let tile_name = format!("{name} [tile {} of {}]", tile.rect.index, plan.total);
        let caption = format!("tile {}/{}, {coord} {a}–{b}", tile.rect.index, plan.total);
        let data = tile.image.bytes.clone();
        let meta = store
            .put(MediaOrigin::Viewed, &tile_name, session, tile.image)
            .await
            .map_err(|e| DispatchError::Handler(format!("image_view: store copy failed: {e}")))?;
        text.push_str(&format!(
            "Tile {} ({coord} {a}–{b}): {}×{}, {} bytes, {}; media id {}{capped}\n",
            tile.rect.index,
            meta.width,
            meta.height,
            data.len(),
            meta.media_type,
            meta.id
        ));
        images.push(ToolImage {
            media_type: meta.media_type.clone(),
            width: meta.width,
            height: meta.height,
            name: meta.name.clone(),
            media_ref: Some(store::to_ref_meta(&meta, store.dir(), &caption)),
            data,
        });
    }
    if plan.parts() > 1 {
        text.push_str(&format!("Parts: {}; ask with part=N.\n", plan.parts_text(&tiled.cuts)));
    }
    text.push_str("The tiles follow as image blocks, in this order.");
    let mut out = ToolOutput::text(text);
    out.images = images;
    Ok(out)
}

/// `part` of `whole` in percent, rounded to the nearest.
fn percent(part: u32, whole: u32) -> u64 {
    (u64::from(part) * 100 + u64::from(whole) / 2) / u64::from(whole)
}

/// A tile the encode ladder had to bring below its box to fit the inline
/// cap: its long edge is under what the box allows a tile of that size.
fn brought_down(rect: &tiles::TileRect, image: &ingest::Normalized) -> bool {
    let box_edge = MEDIA_LONG_EDGE_MAX.min(rect.width.max(rect.height));
    image.width.max(image.height) < box_edge
}


#[derive(Debug, Deserialize, JsonSchema)]
#[embra_tool(
    name = "image_generate",
    description = "Generate an image from a text prompt with the image backend the operator configures with /image-provider: Gemini's image models (default gemini-nano-banana-2.1) or Higgsfield's (default xai/grok-imagine-image-2.0). The result is written to /embra/workspace/MEDIA/gen-<id>.<jpg|png|webp> at full resolution, in the format the backend returned, and a JSON summary {id, path, width, height, bytes, media_type, model, provider, elapsed_ms} is returned; unless return_image=false the image is ALSO returned to you as an image block so you can check it against the prompt before telling the operator. aspect_ratio: every model renders 1:1|3:2|2:3|3:4|4:3|9:16|16:9, and some render more. size, the model's own when omitted: on Gemini 1K|2K|4K, with 512 on gemini-3.1-flash-image only, and 1K only on gemini-3.1-flash-lite-image and gemini-2.5-flash-image; on Higgsfield 1K|2K on xai/grok-imagine-image-2.0 and z-image/turbo, 720p|1080p on higgsfield-ai/soul/v2/standard, and none on ideogram/v4.0, whose size is its own. A size, a ratio, a prompt's length or a reference the configured model does not take is refused before the call, and the refusal names what it takes. reference_images: up to 4 media ids or workspace image paths used as visual references or edit sources (for edits, describe the change in the prompt), on Gemini's models and on Higgsfield's xai/grok-imagine-image-2.0, and one on ideogram/v4.0. reference_weight: 1 to 100, on ideogram/v4.0 only and with its reference image: how strongly the picture keeps the reference (a lower value keeps less of it; the model's own when omitted). A call to Higgsfield waits for its picture and can take minutes. Not configured → an explanation naming /image-provider. Side-effectful: writes a file and spends API credits.",
    is_side_effectful = true
)]
pub struct ImageGenerateArgs {
    /// What to render. Be concrete: subject, style, composition, text to include.
    pub prompt: String,
    /// Every model renders 1:1, 3:2, 2:3, 3:4, 4:3, 9:16, 16:9; some render more.
    #[serde(default)]
    pub aspect_ratio: Option<String>,
    /// The configured model's: on Gemini 1K, 2K, 4K (512 on gemini-3.1-flash-image only); on Higgsfield 1K or 2K, or 720p or 1080p on higgsfield-ai/soul/v2/standard. Omitted: the model's own.
    #[serde(default)]
    pub size: Option<String>,
    /// Media ids (att-/gen-/view-…) or image paths file_read can read, max 4.
    #[serde(default)]
    pub reference_images: Option<Vec<String>>,
    /// How strongly the picture keeps the reference image, 1 to 100: on ideogram/v4.0 only, with a reference image. A lower value keeps less of it. Omitted: the model's own.
    #[serde(default)]
    #[schemars(range(min = 1))]
    pub reference_weight: Option<u32>,
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

/// Load one reference image (media id or a path under `file_read`'s read
/// rule, `jail::readable`), normalized so the request stays small.
async fn load_reference(store: &MediaStore, spec: &str) -> Result<(String, Vec<u8>), String> {
    let bytes = if store::parse_media_id(spec).is_ok() {
        store.get(spec).await.map(|(_, b)| b).map_err(|e| e.to_string())?
    } else {
        let resolved = crate::tools::jail::readable(spec).await.map_err(|e| e.to_string())?;
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
    let refs = args.reference_images.as_deref().unwrap_or_default();
    if refs.len() > IMAGE_GENERATE_MAX_REFERENCES {
        return Ok(ToolOutput::text(format!(
            "image_generate: {} reference images; the limit is {}",
            refs.len(),
            IMAGE_GENERATE_MAX_REFERENCES
        )));
    }
    let aspect_ratio = args.aspect_ratio.clone().filter(|s| !s.trim().is_empty());
    let size = args.size.clone().filter(|s| !s.trim().is_empty());
    // Before a reference is read: a model that takes none refuses them unread.
    if let Err(e) = image_provider::validate_options(
        provider.model(),
        &args.prompt,
        aspect_ratio.as_deref(),
        size.as_deref(),
        refs.len(),
        args.reference_weight,
    ) {
        return Ok(ToolOutput::text(format!("image_generate: {e}")));
    }
    let mut reference_images = Vec::new();
    for spec in refs {
        match load_reference(store, spec).await {
            Ok(pair) => reference_images.push(pair),
            Err(e) => return Ok(ToolOutput::text(format!("image_generate: reference '{spec}' — {e}"))),
        }
    }
    let req = ImageGenRequest {
        prompt: args.prompt.clone(),
        aspect_ratio,
        size,
        reference_images,
        reference_weight: args.reference_weight,
    };

    let started = std::time::Instant::now();
    let limit = provider.kind().time_limit();
    let generated = match tokio::time::timeout(limit, provider.generate(&req)).await {
        Ok(Ok(r)) => r,
        Ok(Err(ImageGenError::Api { status, body })) => {
            return Ok(ToolOutput::text(format!("image_generate: {}", provider.kind().api_error_words(status, &body))));
        }
        Ok(Err(e)) => return Ok(ToolOutput::text(format!("image_generate: {e}"))),
        Err(_) => return Ok(ToolOutput::text(format!("image_generate: {}", provider.kind().timeout_words(limit)))),
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
    // The picture's record names who made it: the tool's answer is gone
    // with its turn, and the picture stays.
    let meta = store
        .put_generated(&name, session, full.clone(), provider.kind().as_str(), provider.model())
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
    use crate::media::ingest::tests::{gif_fixture, jpeg_fixture_with_orientation, png_fixture};

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

    /// A GIF stored before uploads became PNG (put as a GIF here, the old
    /// way) is shown to the model as PNG; a GIF file outside the store is
    /// normalized like any image and comes back as PNG too.
    #[tokio::test]
    async fn image_view_shows_a_gif_as_png() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let old = ingest::Normalized {
            bytes: gif_fixture(5, 2),
            kind: ingest::ImageKind::Gif,
            width: 5,
            height: 2,
            transformed: false,
        };
        let meta = store.put(MediaOrigin::Attached, "anim.gif", "s", old).await.unwrap();
        assert_eq!(meta.media_type, "image/gif");
        let stored = store::to_attachment_ref(&meta, store.dir()).path;
        let out = image_view(&store, &stored, 1, "s").await.unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/png");
        assert_eq!((out.images[0].width, out.images[0].height), (5, 2));
        assert_eq!(ingest::sniff(&out.images[0].data), Some(ingest::ImageKind::Png));

        let src = dir.0.join("loop.gif");
        std::fs::write(&src, gif_fixture(3, 3)).unwrap();
        let out = image_view(&store, src.to_str().unwrap(), 1, "s").await.unwrap();
        assert_eq!(out.images[0].media_type, "image/png");
    }

    /// A long image comes back in tiles at full scale, four per part, each
    /// a `view-` copy; the header names the cuts and the parts.
    #[tokio::test]
    async fn image_view_shows_a_long_image_in_tiles_four_per_part() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let src = dir.0.join("page.png");
        std::fs::write(&src, crate::media::tiles::tests::banded_png(600, 11_000)).unwrap();
        let out = image_view(&store, src.to_str().unwrap(), 1, "s").await.unwrap();
        assert_eq!(out.images.len(), 4, "{}", out.text);
        assert!(
            out.text.contains("shown in 5 tiles of 600×2200 px, top to bottom, at full scale"),
            "{}",
            out.text
        );
        assert!(out.text.contains("Part 1 of 2: tiles 1–4."), "{}", out.text);
        assert!(out.text.contains("Tile 4 (y 6600–8799): 600×2200"), "{}", out.text);
        assert!(
            out.text.contains("Parts: 1 = tiles 1–4 (y 0–8799), 2 = tile 5 (y 8800–10999); ask with part=N."),
            "{}",
            out.text
        );
        for (i, img) in out.images.iter().enumerate() {
            assert_eq!((img.width, img.height), (600, 2200));
            assert_eq!(img.name, format!("page.png [tile {} of 5]", i + 1));
            let r = img.media_ref.as_ref().expect("store ref");
            assert!(r.id.starts_with("view-"));
            assert_eq!(r.caption, format!("tile {}/5, y {}–{}", i + 1, i * 2200, (i + 1) * 2200 - 1));
            assert!(out.text.contains(&r.id));
        }
        assert_eq!(store.usage().await.unwrap().0, 4);
        let out = image_view(&store, src.to_str().unwrap(), 2, "s").await.unwrap();
        assert_eq!(out.images.len(), 1, "{}", out.text);
        assert!(out.text.contains("Part 2 of 2: tile 5."), "{}", out.text);
        assert_eq!(store.usage().await.unwrap().0, 5);
        let out = image_view(&store, src.to_str().unwrap(), 3, "s").await.unwrap();
        assert!(out.images.is_empty());
        assert!(out.text.contains("part 3 does not exist; the image has 2 parts"), "{}", out.text);
        // A short image is shown whole whatever part is asked, and says so.
        let short = dir.0.join("short.png");
        std::fs::write(&short, png_fixture(6, 4)).unwrap();
        let out = image_view(&store, short.to_str().unwrap(), 2, "s").await.unwrap();
        assert_eq!(out.images.len(), 1);
        assert!(out.text.contains("shown whole"), "{}", out.text);
        assert!(out.text.contains("(6×4, "), "a source shown as it is names one size: {}", out.text);
        // part counts from 1 on every path: 0 is refused before the split.
        for path in [src.to_str().unwrap(), short.to_str().unwrap()] {
            let out = image_view(&store, path, 0, "s").await.unwrap();
            assert!(out.images.is_empty());
            assert!(out.text.contains("part counts from 1"), "{}", out.text);
        }
    }

    /// A source brought down to the box names its own size and the scale,
    /// so a reader can tell a shrunk page from one shown as it is.
    #[tokio::test]
    async fn image_view_names_the_source_size_and_the_scale_of_a_shrunk_image() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let src = dir.0.join("tall.png");
        std::fs::write(&src, png_fixture(300, 4000)).unwrap();
        let out = image_view(&store, src.to_str().unwrap(), 1, "s").await.unwrap();
        assert_eq!(out.images.len(), 1);
        assert_eq!((out.images[0].width, out.images[0].height), (193, 2576));
        assert!(out.text.contains("(300×4000 px, shown at 193×2576, 64%; "), "{}", out.text);
    }

    #[tokio::test]
    async fn image_view_rejects_non_image_and_missing() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let svg = dir.0.join("x.png");
        std::fs::write(&svg, b"<svg/>").unwrap();
        let out = image_view(&store, svg.to_str().unwrap(), 1, "s").await.unwrap();
        assert!(out.images.is_empty());
        assert!(out.text.contains("not a supported image"), "{}", out.text);
        let out = image_view(&store, dir.0.join("nope.png").to_str().unwrap(), 1, "s").await.unwrap();
        assert!(out.images.is_empty());
        assert!(out.text.starts_with("image_view:"));
        let out = image_view(&store, dir.0.to_str().unwrap(), 1, "s").await.unwrap();
        assert!(out.text.contains("is not a file"));
    }

    #[tokio::test]
    async fn image_view_outside_media_makes_view_copy_once() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let src = dir.0.join("diagram.png");
        std::fs::write(&src, png_fixture(6, 4)).unwrap();
        let out = image_view(&store, src.to_str().unwrap(), 1, "s").await.unwrap();
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
        let again = image_view(&store, src.to_str().unwrap(), 1, "s").await.unwrap();
        assert_eq!(again.images[0].media_ref.as_ref().unwrap().id, r.id);
        assert_eq!(store.usage().await.unwrap().0, 1);
        // Viewing the stored copy itself keeps its id (no new file).
        let stored_path = r.path.clone();
        let third = image_view(&store, &stored_path, 1, "s").await.unwrap();
        assert_eq!(third.images[0].media_ref.as_ref().unwrap().id, r.id);
        assert_eq!(store.usage().await.unwrap().0, 1);
    }

    /// What the image endpoint returns: a JPEG, here without a rotation.
    fn jpeg_fixture(w: u32, h: u32) -> Vec<u8> {
        jpeg_fixture_with_orientation(w, h, 1)
    }

    /// Fake backend: returns a fixed image (or an error) without the
    /// network, declared as the endpoint declares it.
    struct FakeProvider {
        result: std::sync::Mutex<Option<Result<image_provider::ImageGenResponse, ImageGenError>>>,
        seen: std::sync::Mutex<Option<ImageGenRequest>>,
        model: String,
        kind: image_provider::ImageProviderKind,
    }

    impl FakeProvider {
        fn ok(model: &str, bytes: Vec<u8>) -> Self {
            Self::answering(image_provider::ImageProviderKind::Gemini, model, Ok(image_provider::ImageGenResponse {
                images: vec![image_provider::GeneratedImage {
                    media_type: image_provider::OUTPUT_MIME.into(),
                    data: bytes,
                }],
                text: Some("Here it is.".into()),
            }))
        }
        fn err(model: &str, e: ImageGenError) -> Self {
            Self::answering(image_provider::ImageProviderKind::Gemini, model, Err(e))
        }
        fn answering(
            kind: image_provider::ImageProviderKind,
            model: &str,
            result: Result<image_provider::ImageGenResponse, ImageGenError>,
        ) -> Self {
            FakeProvider {
                result: std::sync::Mutex::new(Some(result)),
                seen: std::sync::Mutex::new(None),
                model: model.into(),
                kind,
            }
        }
    }

    #[async_trait::async_trait]
    impl ImageProvider for FakeProvider {
        fn kind(&self) -> image_provider::ImageProviderKind {
            self.kind
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
            reference_images: None,
            reference_weight: None,
            return_image: None,
        }
    }

    #[tokio::test]
    async fn image_generate_writes_gen_file_and_returns_summary() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::ok("gemini-nano-banana-2.1", jpeg_fixture(9, 6));
        let mut args = gen_args("A lighthouse at dusk, poster style!");
        args.aspect_ratio = Some("16:9".into());
        args.size = Some("2K".into());
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        // Request shape the backend saw.
        let seen = provider.seen.lock().unwrap().clone().unwrap();
        assert_eq!(seen.aspect_ratio.as_deref(), Some("16:9"));
        assert_eq!(seen.size.as_deref(), Some("2K"));
        // Summary JSON + model note + the image block.
        let json_part = out.text.split("\nModel note:").next().unwrap();
        let summary: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert!(summary["id"].as_str().unwrap().starts_with("gen-"));
        assert_eq!(summary["width"], 9);
        assert_eq!(summary["height"], 6);
        assert_eq!(summary["media_type"], "image/jpeg");
        assert!(summary["path"].as_str().unwrap().ends_with(".jpg"), "{summary}");
        assert_eq!(summary["model"], "gemini-nano-banana-2.1");
        assert_eq!(summary["provider"], "gemini");
        assert!(out.text.contains("Model note: Here it is."));
        assert!(out.text.contains("follows as an image block"));
        assert_eq!(out.images.len(), 1);
        assert_eq!(out.images[0].media_type, "image/jpeg");
        assert_eq!(ingest::sniff(&out.images[0].data), Some(ingest::ImageKind::Jpeg));
        let r = out.images[0].media_ref.as_ref().unwrap();
        assert_eq!(r.origin, "generated");
        assert_eq!(r.name, "a-lighthouse-at-dusk-poster-style.jpg");
        assert!(r.caption.starts_with("A lighthouse"));
        assert!(std::path::Path::new(&r.path).exists());
        assert_eq!(std::fs::read(&r.path).unwrap(), jpeg_fixture(9, 6), "full-res bytes stored untouched");
        assert!(out.media_refs.is_empty());
        // The picture's record names the provider and the model, as the
        // summary does: the record is what stays.
        let record = store.meta(summary["id"].as_str().unwrap()).await.unwrap();
        assert_eq!((record.provider.as_deref(), record.model.as_deref()), (Some("gemini"), Some("gemini-nano-banana-2.1")));
    }

    /// The call with nothing but a prompt is the request the API takes:
    /// no size and no ratio of the tool's own, on the configured model.
    #[tokio::test]
    async fn a_call_with_only_a_prompt_asks_for_no_size_and_no_ratio() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::ok(image_provider::DEFAULT_GEMINI_IMAGE_MODEL, jpeg_fixture(8, 8));
        let out = image_generate(&store, &provider, gen_args("a lighthouse"), "s").await.unwrap();
        let seen = provider.seen.lock().unwrap().clone().expect("the backend was called");
        assert_eq!((seen.size, seen.aspect_ratio), (None, None));
        assert!(seen.reference_images.is_empty());
        assert_eq!(out.images.len(), 1, "{}", out.text);
        // An argument the tool no longer has is no reason to refuse a
        // call: a model that still sends `output_format` gets its image.
        let args: ImageGenerateArgs =
            serde_json::from_value(serde_json::json!({"prompt": "a lighthouse", "output_format": "png"})).unwrap();
        assert_eq!(args.prompt, "a lighthouse");
    }

    /// The file's type and name follow the bytes that came back, whatever
    /// the API declared them to be.
    #[tokio::test]
    async fn the_stored_file_is_named_by_the_bytes_that_came_back() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::ok("gemini-nano-banana-2.1", png_fixture(5, 4));
        let out = image_generate(&store, &provider, gen_args("a seal"), "s").await.unwrap();
        let json_part = out.text.split("\nModel note:").next().unwrap();
        let summary: serde_json::Value = serde_json::from_str(json_part).unwrap();
        assert_eq!(summary["media_type"], "image/png");
        assert!(summary["path"].as_str().unwrap().ends_with(".png"), "{summary}");
        assert_eq!(out.images[0].media_ref.as_ref().unwrap().name, "a-seal.png");
    }

    #[tokio::test]
    async fn image_generate_return_image_false_is_display_only() {
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::ok("gemini-nano-banana-2.1", jpeg_fixture(3, 3));
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
        let provider = FakeProvider::ok("gemini-nano-banana-2.1", jpeg_fixture(3, 3));
        // A size the model does not render, and the description of one
        // that is no value of the API: each refused here, by name.
        let mut args = gen_args("x");
        args.size = Some("512".into());
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(
            out.text.contains("gemini-nano-banana-2.1 renders 1K|2K|4K; size 512 is rendered by gemini-3.1-flash-image"),
            "{}",
            out.text
        );
        let mut args = gen_args("x");
        args.size = Some("512px".into());
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(out.text.contains("size '512px' is not one of 512|1K|2K|4K"), "{}", out.text);
        let mut args = gen_args("x");
        args.aspect_ratio = Some("7:3".into());
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(out.text.contains("aspect_ratio '7:3' is not one of"), "{}", out.text);
        assert!(provider.seen.lock().unwrap().is_none(), "never called the backend");
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
        let schema = (d.input_schema)();
        assert_eq!(schema["type"], "object");
        assert!(schema.get("oneOf").is_none() && schema.get("anyOf").is_none() && schema.get("allOf").is_none());
        assert_eq!(schema["properties"]["prompt"]["type"], "string");
        assert_eq!(schema["required"], serde_json::json!(["prompt"]));
    }

    /// What the model is told about the tool is what the tool does: each
    /// provider's default model, the ratios every model renders, each
    /// model's sizes as its row has them, where references go, and no
    /// option the tool does not have. The words are read from the provider
    /// module, so a model or a size changed there without the description
    /// fails here.
    #[test]
    fn the_description_names_the_default_model_and_every_size_and_ratio_as_the_tool_takes_them() {
        use image_provider::{image_model, sizes_of, ImageProviderKind, References, ASPECT_RATIOS, IMAGE_MODELS};
        let d = crate::tools::registry::all_descriptors()
            .find(|d| d.name == "image_generate")
            .expect("image_generate registered");
        let desc = d.description;
        for kind in ImageProviderKind::ALL {
            assert!(desc.contains(&format!("{}'s", kind.label())), "{}: {desc}", kind.label());
            assert!(desc.contains(&format!("(default {})", kind.default_model())), "{desc}");
        }
        // The ratios every model renders, in the tool's order.
        let common: Vec<&str> = ASPECT_RATIOS.iter().copied().filter(|r| IMAGE_MODELS.iter().all(|m| m.ratios.contains(r))).collect();
        assert!(desc.contains(&format!("aspect_ratio: every model renders {}, and some render more.", common.join("|"))), "{desc}");
        // Each model's sizes: Gemini's by its default's and what sets a
        // model apart; Higgsfield's model by model.
        let gemini_default = sizes_of(ImageProviderKind::Gemini.default_model());
        assert!(desc.contains(&format!("on Gemini {},", gemini_default.join("|"))), "{desc}");
        for id in ImageProviderKind::Gemini.models() {
            let sizes = sizes_of(id);
            if sizes == gemini_default {
                continue;
            }
            assert!(desc.contains(id), "{id} renders {sizes:?} and the description does not name it: {desc}");
            for size in sizes.iter().filter(|s| !gemini_default.contains(s)) {
                assert!(desc.contains(&format!("{size} on {id} only")), "{desc}");
            }
            if sizes == ["1K"] {
                assert!(desc.contains("1K only on "), "{desc}");
            }
        }
        let higgsfield = &desc[desc.find("on Higgsfield ").expect("Higgsfield's sizes")..];
        for id in ImageProviderKind::Higgsfield.models() {
            let sizes = sizes_of(id);
            let said = if sizes.is_empty() { format!("none on {id}") } else { sizes.join("|") };
            assert!(higgsfield.contains(id) && higgsfield.contains(&said), "{id}: {said}: {desc}");
        }
        // References go where a model takes them: the clause says Gemini's
        // models, names each Higgsfield model that takes one ("one on" the
        // model that takes a single one) and none that takes none, and names
        // Higgsfield not at all while none of its models takes one.
        let clause = &desc[desc.find("reference_images: ").expect("the references' clause")..];
        let clause = &clause[..clause.find(". ").expect("the clause ends")];
        assert!(clause.contains("on Gemini's models"), "{clause}");
        let higgsfield: Vec<_> = IMAGE_MODELS.iter().filter(|m| m.kind == ImageProviderKind::Higgsfield).collect();
        if higgsfield.iter().all(|m| m.references == References::None) {
            assert!(!clause.contains("Higgsfield"), "no Higgsfield model takes a reference: {clause}");
        }
        for model in higgsfield {
            let one = format!("one on {}", model.id);
            match model.references {
                References::None => assert!(!clause.contains(model.id), "{} takes none: {clause}", model.id),
                References::Url { .. } => assert!(clause.contains(&one), "{clause}"),
                References::Urls { .. } => assert!(clause.contains(model.id) && !clause.contains(&one), "{clause}"),
                References::Inline => panic!("{} takes inline references", model.id),
            }
        }
        assert!(image_model(ImageProviderKind::Higgsfield.default_model()).is_some());
        for gone in ["512px", "output_format"] {
            assert!(!desc.contains(gone), "the description still says {gone}: {desc}");
        }
        let schema = (d.input_schema)();
        let properties: Vec<&str> = schema["properties"].as_object().unwrap().keys().map(String::as_str).collect();
        assert_eq!(properties, ["aspect_ratio", "prompt", "reference_images", "reference_weight", "return_image", "size"]);
        let size = schema["properties"]["size"]["description"].as_str().unwrap();
        assert!(!size.contains("512px") && !size.contains("output_format"), "{size}");
        // A reference's weight: the clause and the argument name each model
        // whose row has the setting, with its bounds, and no other model.
        // The argument's schema carries the least of the rows' lower bounds
        // and no upper one: no parameter of the manifest has a `maximum`,
        // and a keyword no provider has been sent is not tried on all of
        // them for a hint; the bounds are checked before a call.
        let clause = &desc[desc.find("reference_weight: ").expect("the weight's clause")..];
        let clause = &clause[..clause.find(". ").expect("the clause ends")];
        let argument = schema["properties"]["reference_weight"].clone();
        let said = argument["description"].as_str().unwrap();
        let weighted: Vec<_> = IMAGE_MODELS.iter().filter_map(|m| m.reference_weight.map(|w| (m.id, w))).collect();
        assert!(!weighted.is_empty(), "no model has the setting: the argument and its clause go");
        for model in IMAGE_MODELS {
            let named = model.reference_weight.is_some();
            assert_eq!(clause.contains(model.id), named, "{}: {clause}", model.id);
            assert_eq!(said.contains(model.id), named, "{}: {said}", model.id);
        }
        for (id, weight) in &weighted {
            let range = format!("{} to {}", weight.min, weight.max);
            assert!(clause.contains(&range) && said.contains(&range), "{id}: {range}: {clause} / {said}");
        }
        let least = weighted.iter().map(|(_, w)| w.min).min().unwrap();
        assert_eq!(argument["minimum"].as_f64(), Some(f64::from(least)), "{argument}");
        assert!(argument.get("maximum").is_none(), "{argument}");
        assert!(argument["type"].to_string().contains("integer"), "an integer, as the page takes it: {argument}");
    }

    /// A reference's weight reaches the model whose page has the setting,
    /// with its reference. On any other model, and without a reference, it
    /// is refused before a file is read and before the backend is called.
    #[tokio::test]
    async fn a_reference_weight_goes_with_its_reference_to_the_model_that_takes_one() {
        use image_provider::{GeneratedImage, ImageGenResponse, ImageProviderKind};
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let stored = store
            .put(MediaOrigin::Attached, "poster.png", "s", ingest::normalize(png_fixture(6, 6)).await.unwrap())
            .await
            .unwrap();
        let answer = || Ok(ImageGenResponse { images: vec![GeneratedImage { media_type: "image/png".into(), data: png_fixture(5, 4) }], text: None });
        let provider = FakeProvider::answering(ImageProviderKind::Higgsfield, "ideogram/v4.0", answer());
        let mut args = gen_args("the same poster, its triangle green");
        args.reference_images = Some(vec![stored.id.clone()]);
        args.reference_weight = Some(40);
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert_eq!(out.images.len(), 1, "{}", out.text);
        let seen = provider.seen.lock().unwrap().clone().expect("the backend was called");
        assert_eq!((seen.reference_weight, seen.reference_images.len()), (Some(40), 1));
        // As the model sends it: an integer of the schema.
        let args: ImageGenerateArgs = serde_json::from_value(serde_json::json!({"prompt": "x y", "reference_weight": 40})).unwrap();
        assert_eq!(args.reference_weight, Some(40));
        // Refused before a file is read: the reference named does not exist.
        let refusals = [
            ("xai/grok-imagine-image-2.0", Some("/no/such/picture.png"), 40, "image_generate: invalid image request: xai/grok-imagine-image-2.0 takes no reference_weight; ideogram/v4.0 does"),
            ("ideogram/v4.0", Some("/no/such/picture.png"), 0, "image_generate: invalid image request: reference_weight 0 is not between 1 and 100"),
            ("ideogram/v4.0", None, 40, "image_generate: invalid image request: reference_weight says how strongly the picture keeps a reference image, and none was given"),
        ];
        for (model, reference, weight, words) in refusals {
            let provider = FakeProvider::answering(ImageProviderKind::Higgsfield, model, Err(ImageGenError::NoImage("never asked".into())));
            let mut args = gen_args("the same poster, its triangle green");
            args.reference_images = reference.map(|path| vec![path.to_string()]);
            args.reference_weight = Some(weight);
            let out = image_generate(&store, &provider, args, "s").await.unwrap();
            assert!(out.text.starts_with(words), "{model}: {}", out.text);
            assert!(provider.seen.lock().unwrap().is_none(), "{model}: never called the backend");
        }
    }

    /// A Higgsfield picture is stored in the format it came in, under its
    /// own extension, and the summary names the provider.
    #[tokio::test]
    async fn a_higgsfield_picture_is_stored_in_the_format_it_came_in() {
        use image_provider::{GeneratedImage, ImageGenResponse, ImageProviderKind};
        let webp = {
            let picture = image::RgbaImage::from_fn(6, 4, |x, y| image::Rgba([(x * 40) as u8, (y * 60) as u8, 90, 255]));
            let mut out = Vec::new();
            image::codecs::webp::WebPEncoder::new_lossless(&mut out)
                .encode(&picture, 6, 4, image::ExtendedColorType::Rgba8)
                .unwrap();
            out
        };
        for (bytes, ext, media_type) in [(png_fixture(5, 4), "png", "image/png"), (webp, "webp", "image/webp")] {
            let dir = TempDir::new();
            let store = MediaStore::at(dir.0.join("MEDIA"));
            let answer = ImageGenResponse { images: vec![GeneratedImage { media_type: media_type.into(), data: bytes.clone() }], text: None };
            let provider = FakeProvider::answering(ImageProviderKind::Higgsfield, "xai/grok-imagine-image-2.0", Ok(answer));
            let out = image_generate(&store, &provider, gen_args("a harbour at dawn"), "s").await.unwrap();
            let summary: serde_json::Value = serde_json::from_str(out.text.split("\nThe generated image").next().unwrap()).unwrap();
            assert_eq!(summary["provider"], "higgsfield");
            assert_eq!(summary["model"], "xai/grok-imagine-image-2.0");
            assert_eq!(summary["media_type"], media_type);
            assert!(summary["path"].as_str().unwrap().ends_with(&format!(".{ext}")), "{summary}");
            assert_eq!(std::fs::read(summary["path"].as_str().unwrap()).unwrap(), bytes, "stored as it came");
            let record = store.meta(summary["id"].as_str().unwrap()).await.unwrap();
            assert_eq!((record.provider.as_deref(), record.model.as_deref()), (Some("higgsfield"), Some("xai/grok-imagine-image-2.0")));
        }
    }

    /// A model that takes no reference refuses one before a file is read:
    /// the refusal is the model's, not the missing file's.
    #[tokio::test]
    async fn a_model_that_takes_no_reference_refuses_one_before_a_file_is_read() {
        use image_provider::ImageProviderKind;
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let provider = FakeProvider::answering(ImageProviderKind::Higgsfield, "z-image/turbo", Err(ImageGenError::NoImage("never asked".into())));
        let mut args = gen_args("a lighthouse");
        args.reference_images = Some(vec!["/no/such/picture.png".into()]);
        let out = image_generate(&store, &provider, args, "s").await.unwrap();
        assert!(out.text.starts_with("image_generate: invalid image request: z-image/turbo takes no reference image"), "{}", out.text);
        assert!(provider.seen.lock().unwrap().is_none(), "never called the backend");
    }

    /// Each provider has its own words for a refusal of its API and for a
    /// call that ran out of time: Higgsfield's say that a request it took
    /// may still be charged, and never that nothing was written.
    #[tokio::test]
    async fn each_provider_says_a_refusal_and_a_call_that_ran_out_in_its_own_words() {
        use image_provider::ImageProviderKind;
        assert_eq!(
            ImageProviderKind::Gemini.api_error_words(429, "quota"),
            "the gemini API answered 429: quota. Not retried — adjust the prompt/options or try again later."
        );
        assert_eq!(ImageProviderKind::Higgsfield.api_error_words(403, "Insufficient balance."), "Higgsfield answered 403: Insufficient balance.");
        let limit = std::time::Duration::from_secs(300);
        assert!(ImageProviderKind::Gemini.timeout_words(limit).contains("abandoned (nothing was written)"));
        let words = ImageProviderKind::Higgsfield.timeout_words(limit);
        assert!(words.starts_with("no answer within 300s") && words.contains("may still complete and be charged"), "{words}");
        assert!(!words.contains("nothing was written"));
        assert!(ImageProviderKind::Higgsfield.time_limit() > ImageProviderKind::Gemini.time_limit());
        let dir = TempDir::new();
        let store = MediaStore::at(dir.0.join("MEDIA"));
        let refused = ImageGenError::Api { status: 403, body: "Insufficient balance. Higgsfield refused the account".into() };
        let provider = FakeProvider::answering(ImageProviderKind::Higgsfield, "z-image/turbo", Err(refused));
        let out = image_generate(&store, &provider, gen_args("x"), "s").await.unwrap();
        assert_eq!(out.text, "image_generate: Higgsfield answered 403: Insufficient balance. Higgsfield refused the account");
    }

    #[test]
    fn the_scale_percent_rounds_to_the_nearest() {
        assert_eq!(percent(2576, 4385), 59, "58.75 rounds up");
        assert_eq!(percent(2576, 4000), 64, "64.4 rounds down");
        assert_eq!(percent(2576, 2576), 100);
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
