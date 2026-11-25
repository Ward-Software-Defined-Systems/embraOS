//! Sniff + normalize image bytes for the vision path.
//!
//! Normalization is what makes the inline-replay policy affordable: every
//! stored attachment is ≤ [`MEDIA_LONG_EDGE_MAX`] px on its long edge (the
//! vision tier's own ceiling — the API downscales past it anyway) and
//! ≤ [`MEDIA_INLINE_MAX`] bytes, with EXIF orientation applied (the API
//! ignores EXIF, so an un-rotated phone photo arrives sideways).
//!
//! Bytes that need no transform (small, upright, within the edge cap) are
//! kept byte-identical — nothing is re-encoded gratuitously. A GIF is the
//! exception: it is always stored as the PNG of its first frame, because
//! Gemini takes no GIF (its image types are PNG, JPEG, WebP, HEIC and
//! HEIF) and a vision model reads one frame anyway.

use std::io::Cursor;

use image::codecs::jpeg::JpegEncoder;
use image::imageops::FilterType;
use image::metadata::Orientation;
use image::codecs::png::{CompressionType, FilterType as PngFilter, PngEncoder};
use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader, Limits};
use once_cell::sync::Lazy;
use tokio::sync::Semaphore;

use super::{
    JPEG_QUALITY, MEDIA_DECODE_ALLOC_MAX, MEDIA_DECODE_PIXELS_MAX, MEDIA_GENERATED_RAW_MAX, MEDIA_INLINE_MAX,
    MEDIA_LONG_EDGE_MAX, MEDIA_NORMALIZE_CONCURRENCY, MEDIA_UPLOAD_MAX,
};

/// The four raster formats the vision APIs accept. SVG/BMP/TIFF/HEIC are
/// refused at the sniff (HEIC in particular is what iOS produces by
/// default — the browser-side picker asks for `image/*` and Safari
/// transcodes to JPEG on upload).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageKind {
    Png,
    Jpeg,
    Gif,
    Webp,
}

impl ImageKind {
    pub fn media_type(self) -> &'static str {
        match self {
            ImageKind::Png => "image/png",
            ImageKind::Jpeg => "image/jpeg",
            ImageKind::Gif => "image/gif",
            ImageKind::Webp => "image/webp",
        }
    }

    /// File extension used by the store.
    pub fn ext(self) -> &'static str {
        match self {
            ImageKind::Png => "png",
            ImageKind::Jpeg => "jpg",
            ImageKind::Gif => "gif",
            ImageKind::Webp => "webp",
        }
    }

    pub fn from_media_type(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "image/png" => Some(ImageKind::Png),
            "image/jpeg" | "image/jpg" => Some(ImageKind::Jpeg),
            "image/gif" => Some(ImageKind::Gif),
            "image/webp" => Some(ImageKind::Webp),
            _ => None,
        }
    }

    fn format(self) -> ImageFormat {
        match self {
            ImageKind::Png => ImageFormat::Png,
            ImageKind::Jpeg => ImageFormat::Jpeg,
            ImageKind::Gif => ImageFormat::Gif,
            ImageKind::Webp => ImageFormat::WebP,
        }
    }
}

/// Magic-byte sniff. Never trusts a declared content type or an
/// extension — a renamed SVG/HTML is exactly the thing this refuses.
pub fn sniff(bytes: &[u8]) -> Option<ImageKind> {
    if bytes.starts_with(&[0x89, b'P', b'N', b'G', 0x0D, 0x0A, 0x1A, 0x0A]) {
        return Some(ImageKind::Png);
    }
    if bytes.starts_with(&[0xFF, 0xD8, 0xFF]) {
        return Some(ImageKind::Jpeg);
    }
    if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
        return Some(ImageKind::Gif);
    }
    if bytes.len() >= 12 && &bytes[0..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        return Some(ImageKind::Webp);
    }
    None
}

#[derive(Debug, thiserror::Error)]
pub enum IngestError {
    #[error("not a supported image (PNG, JPEG, GIF or WebP required; checked by content, not name)")]
    NotAnImage,
    #[error("image is {0} bytes; the limit is {1} bytes")]
    TooLarge(usize, usize),
    #[error(
        "image is {width}x{height} px, {mp} megapixels; the limit is {limit} megapixels (8000x8000, or any shape of that area)",
        mp = megapixels(.width, .height),
        limit = MEDIA_DECODE_PIXELS_MAX / 1_000_000
    )]
    PixelLimit { width: u32, height: u32 },
    #[error("image decode failed: {0}")]
    Decode(String),
    #[error("{}", no_such_part(.part, .parts))]
    NoSuchPart { part: usize, parts: usize },
    #[error(
        "image is {bytes} bytes, and encoded again at JPEG quality {quality} it is still {smallest} bytes; the limit is {limit} bytes"
    )]
    NoFit { bytes: usize, smallest: usize, quality: u8, limit: usize },
}

/// Output of [`normalize`]: the bytes to store / send, plus what they are.
#[derive(Debug, Clone)]
pub struct Normalized {
    pub bytes: Vec<u8>,
    pub kind: ImageKind,
    pub width: u32,
    pub height: u32,
    /// False when the input bytes were kept byte-identical.
    pub transformed: bool,
}

static NORMALIZE_SLOTS: Lazy<Semaphore> = Lazy::new(|| Semaphore::new(MEDIA_NORMALIZE_CONCURRENCY));

/// Normalize on a blocking thread, at most [`MEDIA_NORMALIZE_CONCURRENCY`]
/// at a time (decode is CPU- and memory-heavy; the VM has 4 GB).
pub async fn normalize(bytes: Vec<u8>) -> Result<Normalized, IngestError> {
    on_blocking_thread(move || normalize_blocking(bytes)).await
}

/// Runs a decode on a blocking thread behind the normalize semaphore: the
/// one seat for every pixel decode, `tiles::view` included.
pub(super) async fn on_blocking_thread<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, IngestError> + Send + 'static,
) -> Result<T, IngestError> {
    let _permit = NORMALIZE_SLOTS
        .acquire()
        .await
        .map_err(|e| IngestError::Decode(e.to_string()))?;
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|e| IngestError::Decode(format!("normalize task failed: {e}")))?
}

/// The decoder's limits: the allocation ceiling alone, the belt to the
/// header check's brace. The size check is this module's
/// ([`MEDIA_DECODE_PIXELS_MAX`], an area, which the decoder's per-edge
/// limits cannot express), and it runs on the header before any pixel
/// buffer exists, so a refusal names the image's size and the cap. A
/// per-edge limit set here would refuse while the decoder is being made,
/// before this module gets to look, with "Image size exceeds limit" and
/// no number.
fn decode_limits() -> Limits {
    let mut limits = Limits::default();
    limits.max_alloc = Some(MEDIA_DECODE_ALLOC_MAX);
    limits
}

/// The header check: the pixel count against [`MEDIA_DECODE_PIXELS_MAX`].
fn over_pixel_cap(w: u32, h: u32) -> bool {
    u64::from(w) * u64::from(h) > MEDIA_DECODE_PIXELS_MAX
}

/// Megapixels with one decimal, for the refusal's text.
fn megapixels(w: &u32, h: &u32) -> String {
    let pixels = u64::from(*w) * u64::from(*h);
    format!("{}.{}", pixels / 1_000_000, (pixels % 1_000_000) / 100_000)
}

/// The refusal of a part that does not exist, with the range that does.
fn no_such_part(part: &usize, parts: &usize) -> String {
    if *parts == 1 {
        format!("part {part} does not exist; the image has 1 part (only part=1)")
    } else {
        format!("part {part} does not exist; the image has {parts} parts (part=1 to part={parts})")
    }
}

/// What the header says, before any pixel buffer exists: the format, the
/// dimensions as stored (before the EXIF orientation) and the orientation.
pub(super) struct Probe {
    pub kind: ImageKind,
    pub width: u32,
    pub height: u32,
    pub orientation: Orientation,
}

impl Probe {
    /// The dimensions once the orientation is applied: a quarter turn
    /// swaps them.
    pub fn oriented_dimensions(&self) -> (u32, u32) {
        match self.orientation {
            Orientation::Rotate90
            | Orientation::Rotate270
            | Orientation::Rotate90FlipH
            | Orientation::Rotate270FlipH => (self.height, self.width),
            _ => (self.width, self.height),
        }
    }
}

/// The header probe, shared by every decode path: the byte gate, the
/// sniff, the decoder's header read under [`decode_limits`], then this
/// module's size check. A decode bomb is refused here, by a message that
/// names its size; the decoder's allocation ceiling on the pixel decode is
/// the belt to this brace.
pub(super) fn probe(bytes: &[u8]) -> Result<Probe, IngestError> {
    probe_within(bytes, MEDIA_UPLOAD_MAX)
}

/// [`probe`] with the byte gate at `ceiling`: a generated picture is taken
/// from its provider heavier than the store keeps one
/// ([`fit_generated`]).
fn probe_within(bytes: &[u8], ceiling: usize) -> Result<Probe, IngestError> {
    if bytes.len() > ceiling {
        return Err(IngestError::TooLarge(bytes.len(), ceiling));
    }
    let kind = sniff(bytes).ok_or(IngestError::NotAnImage)?;
    let mut reader = ImageReader::with_format(Cursor::new(bytes), kind.format());
    reader.limits(decode_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| IngestError::Decode(e.to_string()))?;
    let (width, height) = decoder.dimensions();
    if over_pixel_cap(width, height) {
        return Err(IngestError::PixelLimit { width, height });
    }
    let orientation = decoder.orientation().unwrap_or(Orientation::NoTransforms);
    Ok(Probe { kind, width, height, orientation })
}

/// The JPEG qualities a generated picture over the store's ceiling is
/// tried at, the highest first. Measured on Gemini's own `4K` pictures of
/// a detailed subject (JPEGs written without quantization, 12.6 to
/// 15.9 MiB): this encoder brings one of up to 13.7 MiB under the ceiling
/// at the first rung, with a PSNR near 47.5 dB against the file as it
/// came, and the heaviest at the third (43.9 dB). The tail is for what was
/// not seen: per-pixel noise at the same 17 megapixels fits at 80.
const GENERATED_FIT_QUALITIES: [u8; 11] = [98, 97, 96, 95, 94, 92, 90, 88, 85, 80, 75];

/// The largest colour profile a JPEG's segments carry (255 segments of
/// 65519 bytes): the encoder fails the whole picture on a larger one.
const JPEG_PROFILE_MAX: usize = 255 * 65_519;

/// What was done to a generated picture that came back over the store's
/// ceiling.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Refit {
    /// Its size as the provider returned it.
    pub from_bytes: usize,
    /// Its format as the provider returned it.
    pub from_kind: ImageKind,
    /// The JPEG quality it is stored at.
    pub quality: u8,
}

/// A generated picture as the store keeps it, at FULL resolution (the
/// model gets a normalized copy): on a blocking thread, behind the
/// normalize semaphore.
pub async fn fit_generated(bytes: Vec<u8>) -> Result<(Normalized, Option<Refit>), IngestError> {
    on_blocking_thread(move || fit_generated_blocking(bytes)).await
}

/// Synchronous core of [`fit_generated`], with the two ceilings in force.
pub fn fit_generated_blocking(bytes: Vec<u8>) -> Result<(Normalized, Option<Refit>), IngestError> {
    fit_within(bytes, MEDIA_UPLOAD_MAX, MEDIA_GENERATED_RAW_MAX)
}

/// A picture within `ceiling` is kept byte for byte, with the dimensions
/// its header states and no pixel decoded. One over it, and within
/// `raw_ceiling`, is decoded and written again as a JPEG at the highest
/// quality of [`GENERATED_FIT_QUALITIES`] that fits: its pixel dimensions
/// and its colour profile are kept, its orientation is applied, and
/// nothing else of the file's own is carried over (a provider's signed
/// content credentials cover the bytes they came in, so they would not
/// hold for these). One that fits at no quality is refused with both
/// sizes. Same header check and decode limits as [`normalize_blocking`].
fn fit_within(bytes: Vec<u8>, ceiling: usize, raw_ceiling: usize) -> Result<(Normalized, Option<Refit>), IngestError> {
    let p = probe_within(&bytes, raw_ceiling)?;
    if bytes.len() <= ceiling {
        return Ok((
            Normalized { bytes, kind: p.kind, width: p.width, height: p.height, transformed: false },
            None,
        ));
    }
    let from_bytes = bytes.len();
    let (img, profile) = decode_with_profile(&bytes, &p)?;
    drop(bytes);
    let (width, height) = (img.width(), img.height());
    let rgb = img.into_rgb8();
    let mut smallest = (0, 0);
    for quality in GENERATED_FIT_QUALITIES {
        let mut out = Cursor::new(Vec::new());
        let mut encoder = JpegEncoder::new_with_quality(&mut out, quality);
        if let Some(profile) = &profile {
            // The JPEG encoder takes every profile; the size it cannot
            // write was left out where the profile was read.
            let _ = encoder.set_icc_profile(profile.clone());
        }
        rgb.write_with_encoder(encoder)
            .map_err(|e| IngestError::Decode(e.to_string()))?;
        let out = out.into_inner();
        if out.len() <= ceiling {
            return Ok((
                Normalized { bytes: out, kind: ImageKind::Jpeg, width, height, transformed: true },
                Some(Refit { from_bytes, from_kind: p.kind, quality }),
            ));
        }
        smallest = (out.len(), quality);
    }
    Err(IngestError::NoFit { bytes: from_bytes, smallest: smallest.0, quality: smallest.1, limit: ceiling })
}

/// The pixel decode under [`decode_limits`], the orientation applied. The
/// probe has run, so the size is under the cap.
pub(super) fn decode(bytes: &[u8], probe: &Probe) -> Result<DynamicImage, IngestError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), probe.kind.format());
    reader.limits(decode_limits());
    let decoder = reader
        .into_decoder()
        .map_err(|e| IngestError::Decode(e.to_string()))?;
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| IngestError::Decode(e.to_string()))?;
    if probe.orientation != Orientation::NoTransforms {
        img.apply_orientation(probe.orientation);
    }
    Ok(img)
}

/// [`decode`], and the colour profile the file carries when a JPEG can
/// carry it on: for a picture that is written again at full resolution,
/// whose colours are read by that profile.
fn decode_with_profile(bytes: &[u8], probe: &Probe) -> Result<(DynamicImage, Option<Vec<u8>>), IngestError> {
    let mut reader = ImageReader::with_format(Cursor::new(bytes), probe.kind.format());
    reader.limits(decode_limits());
    let mut decoder = reader
        .into_decoder()
        .map_err(|e| IngestError::Decode(e.to_string()))?;
    let profile = decoder
        .icc_profile()
        .ok()
        .flatten()
        .filter(|profile| !profile.is_empty() && profile.len() <= JPEG_PROFILE_MAX);
    let mut img = DynamicImage::from_decoder(decoder).map_err(|e| IngestError::Decode(e.to_string()))?;
    if probe.orientation != Orientation::NoTransforms {
        img.apply_orientation(probe.orientation);
    }
    Ok((img, profile))
}

/// A decoded image brought into the vision tier's box: the long edge to
/// [`MEDIA_LONG_EDGE_MAX`] when it is over, then the encode ladder.
pub(super) fn fit_whole(mut img: DynamicImage, kind: ImageKind) -> Result<Normalized, IngestError> {
    if img.width().max(img.height()) > MEDIA_LONG_EDGE_MAX {
        // `resize` fits within the box preserving aspect ratio — exactly
        // "long edge ≤ cap".
        img = img.resize(MEDIA_LONG_EDGE_MAX, MEDIA_LONG_EDGE_MAX, FilterType::Lanczos3);
    }
    encode_ladder(img, kind)
}

/// Synchronous core of [`normalize`] (unit-tested directly).
pub fn normalize_blocking(bytes: Vec<u8>) -> Result<Normalized, IngestError> {
    let p = probe(&bytes)?;
    let needs_rotate = p.orientation != Orientation::NoTransforms;
    let needs_scale = p.width.max(p.height) > MEDIA_LONG_EDGE_MAX;
    // A GIF never stays a GIF (module doc): the ladder below writes its
    // first frame as PNG.
    let needs_reencode = p.kind == ImageKind::Gif;
    if !needs_rotate && !needs_scale && !needs_reencode && bytes.len() <= MEDIA_INLINE_MAX {
        return Ok(Normalized {
            bytes,
            kind: p.kind,
            width: p.width,
            height: p.height,
            transformed: false,
        });
    }
    let img = decode(&bytes, &p)?;
    fit_whole(img, p.kind)
}

/// PNG at the library's default compression level with adaptive filtering.
/// `DynamicImage::write_to` encodes at the fastest level, which on a page
/// capture writes ten times the bytes of this one to save a few
/// milliseconds (measured 2026-10-18 on a text-like 800×2000 tile:
/// 508,864 B in 3 ms against 50,490 B in 21 ms; the best level 35,574 B in
/// 28 ms). The bytes go to the model inline and into the store, and the
/// size cap and the replay budgets count them.
fn encode_png(img: &DynamicImage) -> Result<Vec<u8>, IngestError> {
    let mut out = Cursor::new(Vec::new());
    let encoder = PngEncoder::new_with_quality(&mut out, CompressionType::Default, PngFilter::Adaptive);
    img.write_with_encoder(encoder)
        .map_err(|e| IngestError::Decode(e.to_string()))?;
    Ok(out.into_inner())
}

/// Encode ladder: lossless sources (PNG/GIF/WebP) try PNG first; anything
/// that does not fit under [`MEDIA_INLINE_MAX`] falls to JPEG at
/// [`JPEG_QUALITY`], then progressively smaller/lower-quality JPEGs down
/// to 768 px q45 (where even incompressible noise fits under the cap).
fn encode_ladder(img: DynamicImage, kind: ImageKind) -> Result<Normalized, IngestError> {
    let (w, h) = (img.width(), img.height());
    if matches!(kind, ImageKind::Png | ImageKind::Gif | ImageKind::Webp) {
        let out = encode_png(&img)?;
        if out.len() <= MEDIA_INLINE_MAX {
            return Ok(Normalized {
                bytes: out,
                kind: ImageKind::Png,
                width: w,
                height: h,
                transformed: true,
            });
        }
    }
    // Real photos land on the first rung; the tail exists so that even
    // incompressible per-pixel noise terminates under the inline cap.
    let rungs: [(u32, u8); 7] = [
        (MEDIA_LONG_EDGE_MAX, JPEG_QUALITY),
        (MEDIA_LONG_EDGE_MAX, 70),
        (2048, 72),
        (1568, 60),
        (1280, 55),
        (1024, 50),
        (768, 45),
    ];
    let mut current = img;
    let mut last: Option<(Vec<u8>, u32, u32)> = None;
    for (edge, quality) in rungs {
        if current.width().max(current.height()) > edge {
            current = current.resize(edge, edge, FilterType::Lanczos3);
        }
        let rgb = current.to_rgb8();
        let mut out = Cursor::new(Vec::new());
        let encoder = JpegEncoder::new_with_quality(&mut out, quality);
        rgb.write_with_encoder(encoder)
            .map_err(|e| IngestError::Decode(e.to_string()))?;
        let out = out.into_inner();
        let fits = out.len() <= MEDIA_INLINE_MAX;
        last = Some((out, current.width(), current.height()));
        if fits {
            break;
        }
    }
    let (bytes, w, h) = last.expect("ladder has at least one rung");
    Ok(Normalized {
        bytes,
        kind: ImageKind::Jpeg,
        width: w,
        height: h,
        transformed: true,
    })
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::{ImageBuffer, Rgba};

    /// Tiny PNG fixture (solid color), `w`×`h`.
    pub(crate) fn png_fixture(w: u32, h: u32) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(w, h, |x, _| Rgba([(x % 256) as u8, 40, 200, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    pub(crate) fn gif_fixture(w: u32, h: u32) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(w, h, |x, _| Rgba([(x % 256) as u8, 90, 30, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Gif)
            .unwrap();
        out.into_inner()
    }

    /// Small, upright and under every cap, a GIF is still re-encoded:
    /// stored as PNG, which every provider takes.
    #[test]
    fn a_gif_is_stored_as_png() {
        let gif = gif_fixture(4, 3);
        assert_eq!(sniff(&gif), Some(ImageKind::Gif));
        let n = normalize_blocking(gif).unwrap();
        assert_eq!(n.kind, ImageKind::Png);
        assert_eq!(n.kind.media_type(), "image/png");
        assert!(n.transformed);
        assert_eq!((n.width, n.height), (4, 3));
        assert_eq!(sniff(&n.bytes), Some(ImageKind::Png));
    }

    /// JPEG fixture with an EXIF orientation tag (`orientation` 1..=8),
    /// `w`×`h` before rotation. Builds the APP1 segment by hand and
    /// splices it right after SOI.
    pub(crate) fn jpeg_fixture_with_orientation(w: u32, h: u32, orientation: u16) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(w, h, |x, y| Rgba([(x * 7 % 256) as u8, (y * 3 % 256) as u8, 90, 255]));
        let mut out = Cursor::new(Vec::new());
        let rgb = DynamicImage::ImageRgba8(img).to_rgb8();
        rgb.write_with_encoder(JpegEncoder::new_with_quality(&mut out, 90))
            .unwrap();
        let jpeg = out.into_inner();
        // TIFF header (little-endian) + one IFD with the Orientation tag.
        let mut tiff: Vec<u8> = Vec::new();
        tiff.extend_from_slice(b"II");
        tiff.extend_from_slice(&42u16.to_le_bytes());
        tiff.extend_from_slice(&8u32.to_le_bytes()); // IFD0 offset
        tiff.extend_from_slice(&1u16.to_le_bytes()); // one entry
        tiff.extend_from_slice(&0x0112u16.to_le_bytes()); // Orientation
        tiff.extend_from_slice(&3u16.to_le_bytes()); // SHORT
        tiff.extend_from_slice(&1u32.to_le_bytes()); // count
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&0u16.to_le_bytes()); // value padding
        tiff.extend_from_slice(&0u32.to_le_bytes()); // next IFD
        let mut app1: Vec<u8> = Vec::new();
        app1.extend_from_slice(b"Exif\0\0");
        app1.extend_from_slice(&tiff);
        let len = (app1.len() + 2) as u16;
        let mut spliced = Vec::with_capacity(jpeg.len() + app1.len() + 4);
        spliced.extend_from_slice(&jpeg[..2]); // SOI
        spliced.extend_from_slice(&[0xFF, 0xE1]);
        spliced.extend_from_slice(&len.to_be_bytes());
        spliced.extend_from_slice(&app1);
        spliced.extend_from_slice(&jpeg[2..]);
        spliced
    }

    #[test]
    fn sniff_identifies_png_jpeg_gif_webp() {
        assert_eq!(sniff(&png_fixture(2, 2)), Some(ImageKind::Png));
        assert_eq!(sniff(&jpeg_fixture_with_orientation(2, 2, 1)), Some(ImageKind::Jpeg));
        assert_eq!(sniff(b"GIF89a\x01\x00\x01\x00"), Some(ImageKind::Gif));
        let mut webp = b"RIFF".to_vec();
        webp.extend_from_slice(&[0, 0, 0, 0]);
        webp.extend_from_slice(b"WEBPVP8 ");
        assert_eq!(sniff(&webp), Some(ImageKind::Webp));
    }

    #[test]
    fn sniff_rejects_svg_and_html() {
        assert_eq!(sniff(b"<svg xmlns='http://www.w3.org/2000/svg'></svg>"), None);
        assert_eq!(sniff(b"<!doctype html><html></html>"), None);
        assert_eq!(sniff(b"BM\x00\x00"), None); // BMP
        assert_eq!(sniff(b""), None);
        assert!(matches!(normalize_blocking(b"<svg/>".to_vec()), Err(IngestError::NotAnImage)));
    }

    #[test]
    fn normalize_keeps_small_png_bytes_untouched() {
        let png = png_fixture(8, 6);
        let n = normalize_blocking(png.clone()).unwrap();
        assert!(!n.transformed);
        assert_eq!(n.bytes, png);
        assert_eq!((n.width, n.height), (8, 6));
        assert_eq!(n.kind, ImageKind::Png);
    }

    #[test]
    fn normalize_downscales_long_edge_to_max() {
        let png = png_fixture(MEDIA_LONG_EDGE_MAX + 424, 300);
        let n = normalize_blocking(png).unwrap();
        assert!(n.transformed);
        assert_eq!(n.width, MEDIA_LONG_EDGE_MAX);
        assert!(n.height < 300 && n.height > 200, "aspect preserved: {}", n.height);
        assert!(n.bytes.len() <= MEDIA_INLINE_MAX);
    }

    #[test]
    fn normalize_applies_exif_orientation() {
        // Orientation 6 = rotate 90° CW: a 40×20 source becomes 20×40.
        let jpeg = jpeg_fixture_with_orientation(40, 20, 6);
        let n = normalize_blocking(jpeg).unwrap();
        assert!(n.transformed);
        assert_eq!((n.width, n.height), (20, 40));
        // Upright sources are untouched.
        let upright = jpeg_fixture_with_orientation(40, 20, 1);
        let n = normalize_blocking(upright.clone()).unwrap();
        assert!(!n.transformed);
        assert_eq!(n.bytes, upright);
    }

    /// A valid PNG header that claims `w`×`h`, with a 2×2 image's data
    /// behind it: what a decode bomb looks like to the header probe.
    fn png_header_claiming(w: u32, h: u32) -> Vec<u8> {
        let mut bomb = png_fixture(2, 2);
        // IHDR width/height live at bytes 16..24 (big-endian u32 each).
        bomb[16..20].copy_from_slice(&w.to_be_bytes());
        bomb[20..24].copy_from_slice(&h.to_be_bytes());
        // The chunk's checksum covers its type and data (bytes 12..29) and
        // follows them. Made right again, so that the header is a valid one
        // and what refuses the image is its size, not a broken checksum.
        let crc = crc32(&bomb[12..29]);
        bomb[29..33].copy_from_slice(&crc.to_be_bytes());
        bomb
    }

    #[test]
    fn normalize_rejects_oversized_dimensions_before_decode() {
        // The size check is this module's and speaks first, naming the
        // size and the cap. A per-edge limit on the decoder would refuse
        // while the decoder is made, with "Image size exceeds limit" and no
        // number (what a full-page screenshot got until 2026-10-17). No
        // pixel buffer exists either way.
        match normalize_blocking(png_header_claiming(9000, 9000)) {
            Err(e @ IngestError::PixelLimit { width: 9000, height: 9000 }) => {
                assert_eq!(
                    e.to_string(),
                    "image is 9000x9000 px, 81.0 megapixels; the limit is 64 megapixels (8000x8000, or any shape of that area)"
                );
            }
            other => panic!("expected a refusal naming its size, got {other:?}"),
        }
        // The cap is an area: a tall, narrow page capture far over the
        // former 8000 px edge is under it, so the probe admits it and what
        // fails is the pixel decode of a header with a 2×2 image's data
        // behind it, not the size.
        assert!(matches!(
            normalize_blocking(png_header_claiming(800, 20_000)),
            Err(IngestError::Decode(_))
        ));
    }

    /// Glyph-like boxes on ruled lines: what a page capture looks like to
    /// an encoder (repetitive, not trivial).
    pub(crate) fn text_like_png(w: u32, h: u32) -> DynamicImage {
        let mut img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_pixel(w, h, Rgba([255, 255, 255, 255]));
        for y in 0..h {
            let (row, yy) = (y / 22, y % 22);
            if !(4..=17).contains(&yy) || row % 9 == 8 {
                continue;
            }
            for x in 40..w.saturating_sub(40) {
                let (col, xx) = ((x - 40) / 9, (x - 40) % 9);
                if xx >= 7 {
                    continue;
                }
                let ink = match (col * 31 + row * 17) % 7 {
                    0 => xx == 0 || xx == 6 || yy == 4 || yy == 17,
                    1 => yy % 4 == 0,
                    2 => xx % 3 == 0,
                    3 => (xx + yy) % 5 == 0,
                    4 => xx == 3,
                    5 => yy == 10 || xx == 0,
                    _ => (xx * yy) % 4 == 0,
                };
                if ink {
                    img.put_pixel(x, y, Rgba([20, 20, 20, 255]));
                }
            }
        }
        DynamicImage::ImageRgba8(img)
    }

    #[test]
    fn a_png_is_encoded_at_the_default_level_not_the_fastest() {
        let img = text_like_png(400, 600);
        let mut fast = Cursor::new(Vec::new());
        img.write_to(&mut fast, ImageFormat::Png).unwrap();
        let ours = encode_ladder(img.clone(), ImageKind::Png).unwrap();
        assert_eq!(ours.kind, ImageKind::Png);
        assert!(
            ours.bytes.len() * 3 < fast.get_ref().len(),
            "default level {} B against the fastest {} B",
            ours.bytes.len(),
            fast.get_ref().len()
        );
        let back = image::load_from_memory(&ours.bytes).unwrap();
        assert_eq!(back.to_rgba8(), img.to_rgba8(), "lossless");
    }

    #[test]
    fn a_missing_part_is_refused_with_the_range_that_exists() {
        assert_eq!(no_such_part(&2, &1), "part 2 does not exist; the image has 1 part (only part=1)");
        assert_eq!(no_such_part(&9, &2), "part 9 does not exist; the image has 2 parts (part=1 to part=2)");
    }

    #[test]
    fn the_pixel_cap_is_an_area_in_any_shape() {
        // At the cap in three shapes, and one pixel row or column over.
        for (w, h) in [(8000, 8000), (800, 80_000), (80_000, 800)] {
            let p = probe(&png_header_claiming(w, h)).unwrap();
            assert_eq!((p.kind, p.width, p.height), (ImageKind::Png, w, h));
        }
        for (w, h) in [(8001, 8000), (800, 80_001), (80_001, 800)] {
            assert!(
                matches!(
                    probe(&png_header_claiming(w, h)),
                    Err(IngestError::PixelLimit { width, height }) if width == w && height == h
                ),
                "{w}x{h}"
            );
            // A generated picture meets the same check, on its header.
            assert!(
                matches!(
                    fit_generated_blocking(png_header_claiming(w, h)),
                    Err(IngestError::PixelLimit { width, height }) if width == w && height == h
                ),
                "{w}x{h} as a generated picture"
            );
        }
        assert_eq!(megapixels(&800, &20_000), "16.0");
        assert_eq!(megapixels(&800, &4385), "3.5");
    }

    /// CRC-32 as PNG uses it, bit by bit: for the 17 bytes of one header.
    fn crc32(bytes: &[u8]) -> u32 {
        let mut crc = 0xFFFF_FFFFu32;
        for &byte in bytes {
            crc ^= u32::from(byte);
            for _ in 0..8 {
                crc = if crc & 1 == 1 { (crc >> 1) ^ 0xEDB8_8320 } else { crc >> 1 };
            }
        }
        !crc
    }

    #[test]
    fn the_checksum_helper_agrees_with_the_fixture() {
        // The untouched fixture carries the checksum its encoder wrote.
        let png = png_fixture(2, 2);
        assert_eq!(crc32(&png[12..29]).to_be_bytes(), png[29..33]);
    }

    /// A JPEG of `w`x`h` that weighs more than `over` bytes: comment
    /// segments of zeros after the first marker, which every decoder
    /// passes over. Heavy at no cost to decode.
    pub(crate) fn heavy_jpeg_fixture(w: u32, h: u32, orientation: u16, over: usize) -> Vec<u8> {
        let jpeg = jpeg_fixture_with_orientation(w, h, orientation);
        let mut heavy = Vec::with_capacity(over + jpeg.len() + 70_000);
        heavy.extend_from_slice(&jpeg[..2]);
        while heavy.len() + jpeg.len() <= over + 2 {
            heavy.extend_from_slice(&[0xFF, 0xFE, 0xFF, 0xFF]);
            heavy.resize(heavy.len() + 0xFFFF - 2, 0);
        }
        heavy.extend_from_slice(&jpeg[2..]);
        assert!(heavy.len() > over);
        heavy
    }

    /// A PNG of `w`x`h` that weighs more than `over` bytes: after its
    /// header, chunks of a private ancillary type, which a decoder passes
    /// over. One megabyte of zeros each, so one checksum serves them all.
    pub(crate) fn heavy_png_fixture(w: u32, h: u32, over: usize) -> Vec<u8> {
        let png = png_fixture(w, h);
        let mut chunk = b"emBr".to_vec();
        chunk.resize(4 + (1 << 20), 0);
        let checksum = crc32(&chunk).to_be_bytes();
        let mut heavy = Vec::with_capacity(over + png.len() + (1 << 21));
        heavy.extend_from_slice(&png[..33]);
        while heavy.len() + png.len() <= over + 33 {
            heavy.extend_from_slice(&(1u32 << 20).to_be_bytes());
            heavy.extend_from_slice(&chunk);
            heavy.extend_from_slice(&checksum);
        }
        heavy.extend_from_slice(&png[33..]);
        assert!(heavy.len() > over);
        heavy
    }

    /// Per-pixel noise: a picture no encoder makes small.
    fn noise_rgb(w: u32, h: u32) -> DynamicImage {
        let mut seed: u32 = 0x9E37_79B9;
        let img: ImageBuffer<image::Rgb<u8>, Vec<u8>> = ImageBuffer::from_fn(w, h, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            image::Rgb([(seed & 0xFF) as u8, ((seed >> 8) & 0xFF) as u8, ((seed >> 16) & 0xFF) as u8])
        });
        DynamicImage::ImageRgb8(img)
    }

    /// A generated picture within the ceiling is the provider's file, byte
    /// for byte, with the dimensions its header states: nothing is decoded
    /// (the orientation of a turned JPEG is not applied), and a size equal
    /// to the ceiling is within it.
    #[test]
    fn a_generated_picture_within_the_ceiling_is_kept_byte_for_byte() {
        let png = png_fixture(7, 5);
        let (kept, refit) = fit_within(png.clone(), png.len(), 1 << 20).unwrap();
        assert_eq!(refit, None);
        assert_eq!(kept.bytes, png);
        assert!(!kept.transformed);
        assert_eq!((kept.kind, kept.width, kept.height), (ImageKind::Png, 7, 5));
        let turned = jpeg_fixture_with_orientation(8, 4, 6);
        let (kept, refit) = fit_generated_blocking(turned.clone()).unwrap();
        assert_eq!((refit, kept.bytes == turned, kept.width, kept.height), (None, true, 8, 4));
        assert!(matches!(fit_generated_blocking(b"<svg/>".to_vec()), Err(IngestError::NotAnImage)));
    }

    /// Over the ceiling a picture is written again at the highest quality
    /// of the ladder that fits, and a size equal to the ceiling fits. The
    /// ceiling is lowered rung by rung to just under what the last rung
    /// wrote: each time the next quality of the ladder answers, and under
    /// the last one the picture is refused with both sizes.
    #[test]
    fn a_generated_picture_over_the_ceiling_is_written_at_the_highest_quality_that_fits() {
        assert_eq!(GENERATED_FIT_QUALITIES, [98, 97, 96, 95, 94, 92, 90, 88, 85, 80, 75]);
        let mut source = Cursor::new(Vec::new());
        noise_rgb(96, 64)
            .write_with_encoder(JpegEncoder::new_with_quality(&mut source, 100))
            .unwrap();
        let source = source.into_inner();
        let raw = source.len();
        let mut ceiling = source.len() - 1;
        for expected in GENERATED_FIT_QUALITIES {
            let (fitted, refit) = fit_within(source.clone(), ceiling, raw).unwrap();
            assert_eq!(
                refit,
                Some(Refit { from_bytes: source.len(), from_kind: ImageKind::Jpeg, quality: expected }),
                "under a ceiling of {ceiling}"
            );
            assert!(fitted.bytes.len() <= ceiling, "{} over {ceiling}", fitted.bytes.len());
            assert!(fitted.transformed);
            assert_eq!((fitted.kind, fitted.width, fitted.height), (ImageKind::Jpeg, 96, 64));
            assert_eq!(sniff(&fitted.bytes), Some(ImageKind::Jpeg));
            // A ceiling of exactly this size is met by the same quality.
            let (again, at) = fit_within(source.clone(), fitted.bytes.len(), raw).unwrap();
            assert_eq!((at.unwrap().quality, again.bytes == fitted.bytes), (expected, true));
            ceiling = fitted.bytes.len() - 1;
        }
        let err = fit_within(source.clone(), ceiling, raw).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!(
                "image is {} bytes, and encoded again at JPEG quality 75 it is still {} bytes; the limit is {ceiling} bytes",
                source.len(),
                ceiling + 1
            )
        );
    }

    /// The two ceilings in force are the store's and the one a generated
    /// picture is taken at: a picture just over the first is fitted, one
    /// just over the second is refused before anything of it is read.
    #[test]
    fn a_generated_picture_is_fitted_between_the_stores_ceiling_and_the_raw_one() {
        assert_eq!(MEDIA_GENERATED_RAW_MAX, 4 * MEDIA_UPLOAD_MAX);
        let heavy = heavy_jpeg_fixture(9, 6, 1, MEDIA_UPLOAD_MAX);
        let weight = heavy.len();
        assert!(weight > MEDIA_UPLOAD_MAX && weight < MEDIA_UPLOAD_MAX + 70_000, "{weight}");
        assert!(matches!(normalize_blocking(heavy.clone()), Err(IngestError::TooLarge(..))), "no upload is this heavy");
        let (fitted, refit) = fit_generated_blocking(heavy).unwrap();
        assert_eq!(refit, Some(Refit { from_bytes: weight, from_kind: ImageKind::Jpeg, quality: 98 }));
        assert!(fitted.bytes.len() < 4096, "{}", fitted.bytes.len());
        assert_eq!((fitted.kind, fitted.width, fitted.height), (ImageKind::Jpeg, 9, 6));
        let mut too_heavy = jpeg_fixture_with_orientation(2, 2, 1);
        too_heavy.resize(MEDIA_GENERATED_RAW_MAX + 1, 0);
        let err = fit_generated_blocking(too_heavy).unwrap_err();
        assert_eq!(
            err.to_string(),
            format!("image is {} bytes; the limit is {} bytes", MEDIA_GENERATED_RAW_MAX + 1, MEDIA_GENERATED_RAW_MAX)
        );
    }

    /// What a picture keeps when it is written again: its pixel size, its
    /// colours' profile, and its orientation applied (the new file carries
    /// no tag for it). A PNG comes out a JPEG.
    #[test]
    fn a_picture_written_again_keeps_its_pixels_its_profile_and_its_upright_view() {
        // A profile: any bytes, carried as they are.
        let profile: Vec<u8> = (0..700u32).map(|i| (i * 7 % 251) as u8).collect();
        let mut source = Cursor::new(Vec::new());
        let mut encoder = JpegEncoder::new_with_quality(&mut source, 100);
        encoder.set_icc_profile(profile.clone()).unwrap();
        noise_rgb(40, 24).write_with_encoder(encoder).unwrap();
        let source = source.into_inner();
        let (fitted, refit) = fit_within(source.clone(), source.len() - 1, source.len()).unwrap();
        assert_eq!(refit.unwrap().from_kind, ImageKind::Jpeg);
        assert_eq!((fitted.width, fitted.height), (40, 24));
        let mut decoder = ImageReader::with_format(Cursor::new(&fitted.bytes), ImageFormat::Jpeg)
            .into_decoder()
            .unwrap();
        assert_eq!(decoder.icc_profile().unwrap(), Some(profile), "the profile is in the new file");
        // The pixels are the source's own, as near as the quality allows.
        let (before, after) = (image::load_from_memory(&source).unwrap().to_rgb8(), image::load_from_memory(&fitted.bytes).unwrap().to_rgb8());
        assert_eq!((after.width(), after.height()), (40, 24), "no pixel is dropped or added");
        let worst = before.as_raw().iter().zip(after.as_raw()).map(|(a, b)| a.abs_diff(*b)).max().unwrap();
        assert!(worst <= 48, "a sample moved by {worst}");
        // A turned picture is stored upright: 8x4 with a quarter turn is 4x8.
        let turned = heavy_jpeg_fixture(8, 4, 6, 100_000);
        let (fitted, _) = fit_within(turned.clone(), 100_000, turned.len()).unwrap();
        assert_eq!((fitted.width, fitted.height), (4, 8));
        let upright = image::load_from_memory(&fitted.bytes).unwrap();
        assert_eq!((upright.width(), upright.height()), (4, 8));
        // A PNG over the ceiling is stored as a JPEG.
        let mut png = Cursor::new(Vec::new());
        noise_rgb(48, 48).write_to(&mut png, ImageFormat::Png).unwrap();
        let png = png.into_inner();
        let (fitted, refit) = fit_within(png.clone(), png.len() - 1, png.len()).unwrap();
        assert_eq!(refit.unwrap().from_kind, ImageKind::Png);
        assert_eq!((fitted.kind, sniff(&fitted.bytes), fitted.width, fitted.height), (ImageKind::Jpeg, Some(ImageKind::Jpeg), 48, 48));
        assert!(fitted.bytes.len() < png.len());
    }

    /// A colour profile larger than a JPEG's segments carry is left out,
    /// and the picture is written: the encoder fails the whole file on
    /// one. (A PNG carries a profile of any size.)
    #[test]
    fn a_profile_no_jpeg_can_carry_is_left_out_and_the_picture_is_written() {
        let mut png = Cursor::new(Vec::new());
        let mut encoder = PngEncoder::new(&mut png);
        encoder.set_icc_profile(vec![0u8; JPEG_PROFILE_MAX + 1]).unwrap();
        noise_rgb(32, 32).write_with_encoder(encoder).unwrap();
        let png = png.into_inner();
        let p = probe_within(&png, png.len()).unwrap();
        let mut decoder = ImageReader::with_format(Cursor::new(&png), ImageFormat::Png).into_decoder().unwrap();
        assert_eq!(decoder.icc_profile().unwrap().map(|profile| profile.len()), Some(JPEG_PROFILE_MAX + 1), "the fixture carries it");
        assert_eq!(decode_with_profile(&png, &p).unwrap().1, None);
        let (fitted, refit) = fit_within(png.clone(), png.len() - 1, png.len()).unwrap();
        assert_eq!((fitted.kind, refit.unwrap().from_kind), (ImageKind::Jpeg, ImageKind::Png));
        let mut decoder = ImageReader::with_format(Cursor::new(&fitted.bytes), ImageFormat::Jpeg).into_decoder().unwrap();
        assert_eq!(decoder.icc_profile().unwrap(), None);
    }

    #[test]
    fn normalize_rejects_oversize_bytes() {
        let mut big = png_fixture(2, 2);
        big.resize(MEDIA_UPLOAD_MAX + 1, 0);
        assert!(matches!(normalize_blocking(big), Err(IngestError::TooLarge(..))));
    }

    #[test]
    fn normalize_reencodes_to_jpeg_when_png_exceeds_inline_max() {
        // Noise PNGs don't compress: 1400×1400 random RGB ≈ 8 MB as PNG
        // (under the upload cap, far over the inline cap), so the ladder
        // must fall through to JPEG and land under MEDIA_INLINE_MAX.
        let mut seed: u32 = 0x9E37_79B9;
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> = ImageBuffer::from_fn(1400, 1400, |_, _| {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            Rgba([(seed & 0xFF) as u8, ((seed >> 8) & 0xFF) as u8, ((seed >> 16) & 0xFF) as u8, 255])
        });
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut out, ImageFormat::Png)
            .unwrap();
        let png = out.into_inner();
        assert!(png.len() > MEDIA_INLINE_MAX, "fixture must exceed the inline cap: {}", png.len());
        let n = normalize_blocking(png).unwrap();
        assert!(n.transformed);
        assert_eq!(n.kind, ImageKind::Jpeg);
        assert!(n.bytes.len() <= MEDIA_INLINE_MAX, "{} > cap", n.bytes.len());
        assert_eq!(sniff(&n.bytes), Some(ImageKind::Jpeg));
    }
}
