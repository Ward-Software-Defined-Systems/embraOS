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
use image::{DynamicImage, ImageDecoder, ImageFormat, ImageReader, Limits};
use once_cell::sync::Lazy;
use tokio::sync::Semaphore;

use super::{
    JPEG_QUALITY, MEDIA_DECODE_ALLOC_MAX, MEDIA_DECODE_PIXELS_MAX, MEDIA_INLINE_MAX, MEDIA_LONG_EDGE_MAX,
    MEDIA_NORMALIZE_CONCURRENCY, MEDIA_UPLOAD_MAX,
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
    #[error("part {part} does not exist; the image has {parts} parts (part=1 to part={parts})")]
    NoSuchPart { part: usize, parts: usize },
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

/// What the header says, before any pixel buffer exists: the format, the
/// dimensions as stored (before the EXIF orientation) and the orientation.
pub(super) struct Probe {
    pub kind: ImageKind,
    pub width: u32,
    pub height: u32,
    pub orientation: Orientation,
}

/// The header probe, shared by every decode path: the byte gate, the
/// sniff, the decoder's header read under [`decode_limits`], then this
/// module's size check. A decode bomb is refused here, by a message that
/// names its size; the decoder's allocation ceiling on the pixel decode is
/// the belt to this brace.
pub(super) fn probe(bytes: &[u8]) -> Result<Probe, IngestError> {
    if bytes.len() > MEDIA_UPLOAD_MAX {
        return Err(IngestError::TooLarge(bytes.len(), MEDIA_UPLOAD_MAX));
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

/// Header dimensions only (no pixel decode) — used to register a
/// generated image at FULL resolution in the store while the model gets
/// the normalized copy. Same limits/bomb guard as [`normalize_blocking`].
pub fn probe_dimensions(bytes: &[u8]) -> Result<(ImageKind, u32, u32), IngestError> {
    let p = probe(bytes)?;
    Ok((p.kind, p.width, p.height))
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

/// Encode ladder: lossless sources (PNG/GIF/WebP) try PNG first; anything
/// that does not fit under [`MEDIA_INLINE_MAX`] falls to JPEG at
/// [`JPEG_QUALITY`], then progressively smaller/lower-quality JPEGs down
/// to 768 px q45 (where even incompressible noise fits under the cap).
fn encode_ladder(img: DynamicImage, kind: ImageKind) -> Result<Normalized, IngestError> {
    let (w, h) = (img.width(), img.height());
    if matches!(kind, ImageKind::Png | ImageKind::Gif | ImageKind::Webp) {
        let mut out = Cursor::new(Vec::new());
        img.write_to(&mut out, ImageFormat::Png)
            .map_err(|e| IngestError::Decode(e.to_string()))?;
        let out = out.into_inner();
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

    #[test]
    fn the_pixel_cap_is_an_area_in_any_shape() {
        // At the cap in three shapes, and one pixel row or column over.
        for (w, h) in [(8000, 8000), (800, 80_000), (80_000, 800)] {
            let (kind, pw, ph) = probe_dimensions(&png_header_claiming(w, h)).unwrap();
            assert_eq!((kind, pw, ph), (ImageKind::Png, w, h));
        }
        for (w, h) in [(8001, 8000), (800, 80_001), (80_001, 800)] {
            assert!(
                matches!(
                    probe_dimensions(&png_header_claiming(w, h)),
                    Err(IngestError::PixelLimit { width, height }) if width == w && height == h
                ),
                "{w}x{h}"
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

    #[test]
    fn probe_dimensions_reads_header_without_decoding() {
        let (kind, w, h) = probe_dimensions(&png_fixture(7, 5)).unwrap();
        assert_eq!((kind, w, h), (ImageKind::Png, 7, 5));
        assert!(matches!(probe_dimensions(b"<svg/>"), Err(IngestError::NotAnImage)));
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
