//! Tiles: a long image shown at full scale in pieces along its long axis,
//! instead of shrunk whole into the vision tier's box.
//!
//! A full-page screenshot 800 px wide and 20,000 px tall fits the box as
//! a strip of 103×2576 px, which nobody can read. Cut into tiles of
//! 800×2500 px it is shown as captured. The planner ([`TilePlan`]) is
//! pure and tested on numbers; [`view`] runs the decode and the cuts on a
//! blocking thread behind the ingest semaphore, like `normalize`.
//!
//! The rule: an image whose long edge fits within a 2× shrink
//! ([`MEDIA_TILE_PAST`]) is shown whole, as before (measured 2026-10-17:
//! 1.7× down stays readable, 3.1× does not). Past it the long axis is
//! cut into equal tiles no longer than `max(2576, short edge)`, so every
//! tile fits the box at the short edge's scale: unscaled when the short
//! edge is within the box. One call hands the model
//! [`MEDIA_TILES_PER_VIEW`] tiles; the rest come part by part.

use image::DynamicImage;

use super::ingest::{self, IngestError, Normalized};
use super::{MEDIA_LONG_EDGE_MAX, MEDIA_TILES_PER_VIEW, MEDIA_TILE_PAST};

/// The long axis: the order the tiles are read in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Axis {
    /// Tiles run top to bottom; a tile's span is in `y`.
    Down,
    /// Tiles run left to right; a tile's span is in `x`.
    Right,
}

impl Axis {
    /// The coordinate a span is given in.
    pub fn coordinate(self) -> &'static str {
        match self {
            Axis::Down => "y",
            Axis::Right => "x",
        }
    }

    /// The reading order, in words.
    pub fn order(self) -> &'static str {
        match self {
            Axis::Down => "top to bottom",
            Axis::Right => "left to right",
        }
    }
}

/// How a long image is cut. Built by [`TilePlan::plan`] from the
/// dimensions after orientation; `None` from `plan` means "shown whole".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TilePlan {
    pub width: u32,
    pub height: u32,
    pub axis: Axis,
    /// A tile's length along the axis, in source pixels; the last tile
    /// takes what is left, which is shorter or equal.
    pub tile_len: u32,
    pub total: usize,
}

/// One tile's place in the source, 1-based `index`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TileRect {
    pub index: usize,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl TilePlan {
    /// The plan for an image of `width`×`height`, or `None` when it is
    /// shown whole: a long edge within [`MEDIA_TILE_PAST`], or a shape
    /// that gives one tile (a square past the edge, for instance).
    pub fn plan(width: u32, height: u32) -> Option<TilePlan> {
        let long = width.max(height);
        let short = width.min(height);
        if short == 0 || long <= MEDIA_TILE_PAST {
            return None;
        }
        let tile_max = MEDIA_LONG_EDGE_MAX.max(short);
        let total = long.div_ceil(tile_max) as usize;
        if total < 2 {
            return None;
        }
        let tile_len = long.div_ceil(total as u32);
        let axis = if height >= width { Axis::Down } else { Axis::Right };
        Some(TilePlan { width, height, axis, tile_len, total })
    }

    pub fn long(&self) -> u32 {
        self.width.max(self.height)
    }

    pub fn short(&self) -> u32 {
        self.width.min(self.height)
    }

    /// A tile's size as cut, width × height.
    pub fn tile_size(&self) -> (u32, u32) {
        match self.axis {
            Axis::Down => (self.width, self.tile_len),
            Axis::Right => (self.tile_len, self.height),
        }
    }

    /// Percent of the source scale a tile is shown at: 100 when the short
    /// edge fits the box, less when the tile is brought down to it.
    pub fn scale_percent(&self) -> u32 {
        if self.short() <= MEDIA_LONG_EDGE_MAX {
            100
        } else {
            (u64::from(MEDIA_LONG_EDGE_MAX) * 100 / u64::from(self.short())) as u32
        }
    }

    /// How many calls show the whole image, [`MEDIA_TILES_PER_VIEW`] tiles each.
    pub fn parts(&self) -> usize {
        self.total.div_ceil(MEDIA_TILES_PER_VIEW)
    }

    /// Tile `index` (1-based, within `total`).
    pub fn rect(&self, index: usize) -> TileRect {
        let start = (index - 1) as u32 * self.tile_len;
        let len = self.tile_len.min(self.long() - start);
        match self.axis {
            Axis::Down => TileRect { index, x: 0, y: start, width: self.width, height: len },
            Axis::Right => TileRect { index, x: start, y: 0, width: len, height: self.height },
        }
    }

    /// The tiles of `part` (1-based), or `None` for a part that does not exist.
    pub fn part(&self, part: usize) -> Option<Vec<TileRect>> {
        if part == 0 || part > self.parts() {
            return None;
        }
        let first = (part - 1) * MEDIA_TILES_PER_VIEW;
        let last = (first + MEDIA_TILES_PER_VIEW).min(self.total);
        Some((first..last).map(|i| self.rect(i + 1)).collect())
    }

    /// A tile's first and last coordinate along the axis.
    pub fn span(&self, rect: &TileRect) -> (u32, u32) {
        match self.axis {
            Axis::Down => (rect.y, rect.y + rect.height - 1),
            Axis::Right => (rect.x, rect.x + rect.width - 1),
        }
    }

    /// "tiles 1–4" or "tile 5": the tiles of one part.
    pub fn tiles_text(&self, part: usize) -> String {
        let first = (part - 1) * MEDIA_TILES_PER_VIEW + 1;
        let last = (part * MEDIA_TILES_PER_VIEW).min(self.total);
        if first == last { format!("tile {first}") } else { format!("tiles {first}–{last}") }
    }

    /// Every part with its tiles and its span, for the header: at most
    /// twelve listed, the rest counted.
    pub fn parts_text(&self) -> String {
        const LISTED: usize = 12;
        let coord = self.axis.coordinate();
        let parts = self.parts();
        let mut items: Vec<String> = (1..=parts.min(LISTED))
            .map(|part| {
                let first = (part - 1) * MEDIA_TILES_PER_VIEW + 1;
                let last = (part * MEDIA_TILES_PER_VIEW).min(self.total);
                let (a, _) = self.span(&self.rect(first));
                let (_, b) = self.span(&self.rect(last));
                format!("{part} = {} ({coord} {a}–{b})", self.tiles_text(part))
            })
            .collect();
        if parts > LISTED {
            items.push(format!("… and {} more", parts - LISTED));
        }
        items.join(", ")
    }
}

/// One tile, cut and brought into the box.
#[derive(Debug, Clone)]
pub struct Tile {
    pub rect: TileRect,
    pub image: Normalized,
}

/// The tiles of one part.
#[derive(Debug, Clone)]
pub struct Tiled {
    pub plan: TilePlan,
    pub part: usize,
    pub tiles: Vec<Tile>,
}

/// What `image_view` shows: the image whole, as `normalize` makes it, or
/// one part of its tiles.
#[derive(Debug, Clone)]
pub enum View {
    Whole(Normalized),
    Tiled(Tiled),
}

/// The view of `bytes` for `part`, on a blocking thread behind the
/// normalize semaphore.
pub async fn view(bytes: Vec<u8>, part: usize) -> Result<View, IngestError> {
    ingest::on_blocking_thread(move || view_blocking(bytes, part)).await
}

/// Synchronous core of [`view`]. A short image takes `normalize`'s path,
/// untouched bytes included; a long one is decoded once, oriented,
/// planned on its real dimensions and cut.
pub fn view_blocking(bytes: Vec<u8>, part: usize) -> Result<View, IngestError> {
    let probe = ingest::probe(&bytes)?;
    if probe.width.max(probe.height) <= MEDIA_TILE_PAST {
        return ingest::normalize_blocking(bytes).map(View::Whole);
    }
    let img = ingest::decode(&bytes, &probe)?;
    let Some(plan) = TilePlan::plan(img.width(), img.height()) else {
        return ingest::fit_whole(img, probe.kind).map(View::Whole);
    };
    let rects = plan.part(part).ok_or(IngestError::NoSuchPart { part, parts: plan.parts() })?;
    let mut tiles = Vec::with_capacity(rects.len());
    for rect in rects {
        let piece: DynamicImage = img.crop_imm(rect.x, rect.y, rect.width, rect.height);
        let image = ingest::fit_whole(piece, probe.kind)?;
        tiles.push(Tile { rect, image });
    }
    Ok(View::Tiled(Tiled { plan, part, tiles }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::{GenericImageView, ImageBuffer, ImageFormat, Rgba};
    use std::io::Cursor;

    /// A PNG whose rows carry their band in the red channel (`y / 1000`),
    /// so a tile's first pixel tells where it was cut from.
    pub(crate) fn banded_png(w: u32, h: u32) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(w, h, |_, y| Rgba([(y / 1000) as u8, 0, 0, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    #[test]
    fn a_short_image_is_whole_and_a_long_one_is_cut_evenly_along_its_long_axis() {
        // Within a 2× shrink: whole. The measured readable case among them.
        for (w, h) in [(800, 4385), (800, MEDIA_TILE_PAST), (MEDIA_TILE_PAST, 800), (2576, 2576), (6000, 6000)] {
            assert_eq!(TilePlan::plan(w, h), None, "{w}x{h}");
        }
        // The measured unreadable case: four tiles of 800×2000, unscaled.
        let p = TilePlan::plan(800, 8000).unwrap();
        assert_eq!((p.axis, p.total, p.tile_len, p.parts(), p.scale_percent()), (Axis::Down, 4, 2000, 1, 100));
        assert_eq!(p.tile_size(), (800, 2000));
        // A long page: eight tiles in two parts.
        let p = TilePlan::plan(800, 20_000).unwrap();
        assert_eq!((p.axis, p.total, p.tile_len, p.parts()), (Axis::Down, 8, 2500, 2));
        // The same page on its side runs left to right.
        let p = TilePlan::plan(20_000, 800).unwrap();
        assert_eq!((p.axis, p.total, p.tile_len, p.tile_size()), (Axis::Right, 8, 2500, (2500, 800)));
        // One pixel past the edge: three equal tiles, not 2576 + 2576 + 1.
        let p = TilePlan::plan(800, MEDIA_TILE_PAST + 1).unwrap();
        assert_eq!((p.total, p.tile_len), (3, 1718));
        // A short edge over the box: square tiles, brought down to it.
        let p = TilePlan::plan(3000, 30_000).unwrap();
        assert_eq!((p.total, p.tile_len, p.scale_percent()), (10, 3000, 85));
        let p = TilePlan::plan(6000, 12_000).unwrap();
        assert_eq!((p.total, p.tile_len, p.scale_percent()), (2, 6000, 42));
    }

    #[test]
    fn the_tiles_cover_the_long_axis_without_a_gap_or_an_overlap() {
        for (w, h) in [(800, 8000), (800, 20_000), (20_000, 800), (800, 5153), (3000, 30_000), (1, 64_000_000)] {
            let p = TilePlan::plan(w, h).unwrap();
            let mut next = 0u32;
            for i in 1..=p.total {
                let r = p.rect(i);
                let (a, b) = p.span(&r);
                assert_eq!(a, next, "{w}x{h} tile {i} starts where the last ended");
                assert!(b >= a && b - a < p.tile_len, "{w}x{h} tile {i} is at most a tile long");
                let (tw, th) = (r.width, r.height);
                assert_eq!(if p.axis == Axis::Down { tw } else { th }, p.short(), "{w}x{h} tile {i} keeps the short edge");
                next = b + 1;
            }
            assert_eq!(next, p.long(), "{w}x{h}: the last tile ends at the edge");
        }
    }

    #[test]
    fn a_part_holds_four_tiles_and_the_last_part_the_rest() {
        let p = TilePlan::plan(800, 20_000).unwrap();
        let one: Vec<usize> = p.part(1).unwrap().iter().map(|r| r.index).collect();
        let two: Vec<usize> = p.part(2).unwrap().iter().map(|r| r.index).collect();
        assert_eq!((one, two), (vec![1, 2, 3, 4], vec![5, 6, 7, 8]));
        assert_eq!(p.part(0), None);
        assert_eq!(p.part(3), None);
        assert_eq!(p.parts_text(), "1 = tiles 1–4 (y 0–9999), 2 = tiles 5–8 (y 10000–19999)");
        // Nine tiles: the third part is one tile.
        let p = TilePlan::plan(800, 22_000).unwrap();
        assert_eq!((p.total, p.parts()), (9, 3));
        assert_eq!(p.part(3).unwrap().len(), 1);
        assert_eq!(p.tiles_text(3), "tile 9");
        assert!(p.parts_text().ends_with("3 = tile 9 (y 19560–21999)"), "{}", p.parts_text());
        // Many parts are counted, not all listed.
        let p = TilePlan::plan(800, 200_000).unwrap();
        assert!(p.parts() > 12);
        assert!(p.parts_text().ends_with(&format!("… and {} more", p.parts() - 12)), "{}", p.parts_text());
    }

    #[test]
    fn a_long_png_comes_back_in_tiles_at_full_scale_and_a_part_past_the_last_is_refused() {
        let png = banded_png(600, 11_000);
        let View::Tiled(t) = view_blocking(png.clone(), 1).unwrap() else { panic!("a long image is tiled") };
        assert_eq!((t.plan.total, t.plan.tile_len, t.plan.parts(), t.part), (5, 2200, 2, 1));
        assert_eq!(t.tiles.len(), 4);
        for (i, tile) in t.tiles.iter().enumerate() {
            assert_eq!(tile.rect, TileRect { index: i + 1, x: 0, y: i as u32 * 2200, width: 600, height: 2200 });
            assert_eq!((tile.image.width, tile.image.height), (600, 2200), "unscaled");
            assert!(tile.image.transformed);
            // The first row of the tile is the row it was cut at.
            let decoded = image::load_from_memory(&tile.image.bytes).unwrap();
            assert_eq!(decoded.get_pixel(0, 0).0[0], (i as u32 * 2200 / 1000) as u8, "tile {}", i + 1);
        }
        let View::Tiled(t) = view_blocking(png.clone(), 2).unwrap() else { panic!("part two is tiled") };
        assert_eq!(t.tiles.len(), 1);
        assert_eq!(t.tiles[0].rect, TileRect { index: 5, x: 0, y: 8800, width: 600, height: 2200 });
        let err = view_blocking(png, 3).unwrap_err();
        assert_eq!(err.to_string(), "part 3 does not exist; the image has 2 parts (part=1 to part=2)");
        // A short image takes normalize's path, whatever part is asked.
        let View::Whole(n) = view_blocking(ingest::tests::png_fixture(800, 4385), 2).unwrap() else {
            panic!("a short image is whole")
        };
        assert_eq!((n.width, n.height), (470, 2576));
    }
}
