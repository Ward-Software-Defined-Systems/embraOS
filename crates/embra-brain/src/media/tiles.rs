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
//! edge is within the box. Each cut then moves to the line with the
//! least ink within [`MEDIA_TILE_SNAP`] pixels, the nearer one at a tie,
//! so a line of text is not split between two tiles (the review of
//! 2026-10-18): a blank line wins outright, and beside a figure that
//! crosses every line in the window the gap between two lines of text
//! wins over a line of text (the recheck: a cut next to a floated figure
//! fell back to the even cut and split both). One call hands the model
//! [`MEDIA_TILES_PER_VIEW`] tiles; the rest come part by part.

use image::{DynamicImage, GenericImageView};

use super::ingest::{self, IngestError, Normalized};
use super::{MEDIA_LONG_EDGE_MAX, MEDIA_TILES_PER_VIEW, MEDIA_TILE_PAST, MEDIA_TILE_SNAP};

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
    /// The even tile length along the axis, in source pixels, before the
    /// cuts move; the last tile takes what is left.
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

    /// The longest a tile may be along the axis and still fit the box at
    /// the short edge's scale.
    pub fn tile_max(&self) -> u32 {
        MEDIA_LONG_EDGE_MAX.max(self.short())
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

    /// How far a cut may move each way: [`MEDIA_TILE_SNAP`], or less when
    /// a tile has less room under its box (two cuts moving apart lengthen
    /// the tile between them by twice the window).
    pub fn snap_window(&self) -> u32 {
        MEDIA_TILE_SNAP.min((self.tile_max() - self.tile_len) / 2)
    }

    /// The even cuts: `total + 1` positions from 0 to the long edge.
    pub fn nominal_cuts(&self) -> Vec<u32> {
        (0..=self.total)
            .map(|i| (i as u32 * self.tile_len).min(self.long()))
            .collect()
    }

    /// The cuts for `img`: each interior cut moved to the line with the
    /// least ink within the snap window, the even cut when none has less.
    pub fn cuts(&self, img: &DynamicImage) -> Vec<u32> {
        let window = self.snap_window();
        let mut cuts = self.nominal_cuts();
        for cut in &mut cuts[1..self.total] {
            *cut = self.snap(img, *cut, window);
        }
        cuts
    }

    /// The line with the least ink within `window` of `nominal`, the
    /// nearer one at a tie and the later one at the same distance; the
    /// even cut when no line has less. The edges of the image are never
    /// cuts.
    fn snap(&self, img: &DynamicImage, nominal: u32, window: u32) -> u32 {
        let mut best = (self.ink(img, nominal), 0u32, nominal);
        for d in 1..=window {
            for at in [nominal + d, nominal.saturating_sub(d)] {
                if at == 0 || at >= self.long() {
                    continue;
                }
                let ink = self.ink(img, at);
                if (ink, d) < (best.0, best.1) {
                    best = (ink, d, at);
                }
            }
        }
        best.2
    }

    /// The ink on a line across the short edge (a row for `Down`, a
    /// column for `Right`): the pixels that differ from the line's first
    /// pixel or from its last, whichever count is smaller, so a figure at
    /// one margin is counted against the background at the other. Zero on
    /// a line of one colour.
    fn ink(&self, img: &DynamicImage, at: u32) -> u32 {
        let (first, last, pixels): (_, _, Box<dyn Iterator<Item = image::Rgba<u8>>>) = match self.axis {
            Axis::Down => (
                img.get_pixel(0, at),
                img.get_pixel(img.width() - 1, at),
                Box::new((0..img.width()).map(move |x| img.get_pixel(x, at))),
            ),
            Axis::Right => (
                img.get_pixel(at, 0),
                img.get_pixel(at, img.height() - 1),
                Box::new((0..img.height()).map(move |y| img.get_pixel(at, y))),
            ),
        };
        let (mut not_first, mut not_last) = (0u32, 0u32);
        for p in pixels {
            not_first += u32::from(p != first);
            not_last += u32::from(p != last);
        }
        not_first.min(not_last)
    }

    /// Tile `index` (1-based) between its cuts.
    pub fn rect_at(&self, cuts: &[u32], index: usize) -> TileRect {
        let start = cuts[index - 1];
        let len = cuts[index] - start;
        match self.axis {
            Axis::Down => TileRect { index, x: 0, y: start, width: self.width, height: len },
            Axis::Right => TileRect { index, x: start, y: 0, width: len, height: self.height },
        }
    }

    /// The tiles of `part` (1-based), or `None` for a part that does not exist.
    pub fn part_rects(&self, cuts: &[u32], part: usize) -> Option<Vec<TileRect>> {
        if part == 0 || part > self.parts() {
            return None;
        }
        let first = (part - 1) * MEDIA_TILES_PER_VIEW;
        let last = (first + MEDIA_TILES_PER_VIEW).min(self.total);
        Some((first..last).map(|i| self.rect_at(cuts, i + 1)).collect())
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

    /// The tiles' size as cut: "800×2000" when every tile has the same
    /// length, "800×1970–2030" when the cuts moved or the last is shorter.
    pub fn tile_size_text(&self, cuts: &[u32]) -> String {
        let lens = cuts.windows(2).map(|w| w[1] - w[0]);
        let lo = lens.clone().min().unwrap_or(0);
        let hi = lens.max().unwrap_or(0);
        let along = if lo == hi { format!("{lo}") } else { format!("{lo}–{hi}") };
        match self.axis {
            Axis::Down => format!("{}×{along}", self.width),
            Axis::Right => format!("{along}×{}", self.height),
        }
    }

    /// Every part with its tiles and its span, for the header: at most
    /// twelve listed, the rest counted.
    pub fn parts_text(&self, cuts: &[u32]) -> String {
        const LISTED: usize = 12;
        let coord = self.axis.coordinate();
        let parts = self.parts();
        let mut items: Vec<String> = (1..=parts.min(LISTED))
            .map(|part| {
                let first = (part - 1) * MEDIA_TILES_PER_VIEW + 1;
                let last = (part * MEDIA_TILES_PER_VIEW).min(self.total);
                let (a, _) = self.span(&self.rect_at(cuts, first));
                let (_, b) = self.span(&self.rect_at(cuts, last));
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

/// The tiles of one part, with the cuts of the whole image.
#[derive(Debug, Clone)]
pub struct Tiled {
    pub plan: TilePlan,
    pub part: usize,
    pub cuts: Vec<u32>,
    pub tiles: Vec<Tile>,
}

/// What `image_view` shows: the image whole, as `normalize` makes it,
/// with the source's dimensions after orientation; or one part of its
/// tiles.
#[derive(Debug, Clone)]
pub enum View {
    Whole { image: Normalized, source_width: u32, source_height: u32 },
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
    let (source_width, source_height) = probe.oriented_dimensions();
    if source_width.max(source_height) <= MEDIA_TILE_PAST {
        return ingest::normalize_blocking(bytes).map(|image| View::Whole { image, source_width, source_height });
    }
    let img = ingest::decode(&bytes, &probe)?;
    let (source_width, source_height) = (img.width(), img.height());
    let Some(plan) = TilePlan::plan(source_width, source_height) else {
        return ingest::fit_whole(img, probe.kind).map(|image| View::Whole { image, source_width, source_height });
    };
    let cuts = plan.cuts(&img);
    let rects = plan
        .part_rects(&cuts, part)
        .ok_or(IngestError::NoSuchPart { part, parts: plan.parts() })?;
    let mut tiles = Vec::with_capacity(rects.len());
    for rect in rects {
        let piece: DynamicImage = img.crop_imm(rect.x, rect.y, rect.width, rect.height);
        let image = ingest::fit_whole(piece, probe.kind)?;
        tiles.push(Tile { rect, image });
    }
    Ok(View::Tiled(Tiled { plan, part, cuts, tiles }))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use image::{ImageBuffer, ImageFormat, Rgba};
    use std::io::Cursor;

    /// A PNG whose rows carry their band in the red channel (`y / 1000`),
    /// so a tile's first pixel tells where it was cut from. Every row is
    /// one colour, so every row is a blank line and the cuts stay even.
    pub(crate) fn banded_png(w: u32, h: u32) -> Vec<u8> {
        let img: ImageBuffer<Rgba<u8>, Vec<u8>> =
            ImageBuffer::from_fn(w, h, |_, y| Rgba([(y / 1000) as u8, 0, 0, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img).write_to(&mut out, ImageFormat::Png).unwrap();
        out.into_inner()
    }

    /// Vertical stripes everywhere (no row is one colour) except the rows
    /// `blank` says are, which are white.
    fn striped(w: u32, h: u32, blank: impl Fn(u32) -> bool) -> DynamicImage {
        DynamicImage::ImageRgba8(ImageBuffer::from_fn(w, h, |x, y| {
            if blank(y) {
                Rgba([255, 255, 255, 255])
            } else if x % 2 == 0 {
                Rgba([0, 0, 0, 255])
            } else {
                Rgba([255, 255, 255, 255])
            }
        }))
    }

    /// A page with a floated figure down its left margin (a checker in
    /// columns 0..220 on the rows `figure` says) beside lines of text
    /// (stripes in columns 260..760 on 18 of every 22 rows; the other 4
    /// are the gap between two lines). No row in the figure's band is one
    /// colour.
    fn text_beside_a_figure(w: u32, h: u32, figure: impl Fn(u32) -> bool) -> DynamicImage {
        DynamicImage::ImageRgba8(ImageBuffer::from_fn(w, h, |x, y| {
            let white = Rgba([255, 255, 255, 255]);
            let black = Rgba([0, 0, 0, 255]);
            if x < 220 && figure(y) {
                if (x / 10 + y / 10) % 2 == 0 { black } else { white }
            } else if (260..760).contains(&x) && y % 22 >= 4 && x % 2 == 0 {
                black
            } else {
                white
            }
        }))
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
        assert_eq!(p.tile_size_text(&p.nominal_cuts()), "800×2000");
        // A long page: eight tiles in two parts.
        let p = TilePlan::plan(800, 20_000).unwrap();
        assert_eq!((p.axis, p.total, p.tile_len, p.parts()), (Axis::Down, 8, 2500, 2));
        // The same page on its side runs left to right.
        let p = TilePlan::plan(20_000, 800).unwrap();
        assert_eq!((p.axis, p.total, p.tile_len), (Axis::Right, 8, 2500));
        assert_eq!(p.tile_size_text(&p.nominal_cuts()), "2500×800");
        // One pixel past the edge: three equal tiles, not 2576 + 2576 + 1.
        let p = TilePlan::plan(800, MEDIA_TILE_PAST + 1).unwrap();
        assert_eq!((p.total, p.tile_len), (3, 1718));
        // A short edge over the box: square tiles, brought down to it.
        let p = TilePlan::plan(3000, 30_000).unwrap();
        assert_eq!((p.total, p.tile_len, p.scale_percent()), (10, 3000, 85));
        let p = TilePlan::plan(6000, 12_000).unwrap();
        assert_eq!((p.total, p.tile_len, p.scale_percent()), (2, 6000, 42));
        // The last tile is shorter when the length does not divide: the
        // size text says so.
        let p = TilePlan::plan(800, 16_642).unwrap();
        assert_eq!((p.total, p.tile_len), (7, 2378));
        assert_eq!(p.tile_size_text(&p.nominal_cuts()), "800×2374–2378");
    }

    #[test]
    fn the_tiles_cover_the_long_axis_without_a_gap_or_an_overlap() {
        for (w, h) in [(800, 8000), (800, 20_000), (20_000, 800), (800, 5153), (3000, 30_000), (1, 64_000_000)] {
            let p = TilePlan::plan(w, h).unwrap();
            let cuts = p.nominal_cuts();
            assert_eq!(cuts.len(), p.total + 1);
            let mut next = 0u32;
            for i in 1..=p.total {
                let r = p.rect_at(&cuts, i);
                let (a, b) = p.span(&r);
                assert_eq!(a, next, "{w}x{h} tile {i} starts where the last ended");
                assert!(b >= a && b - a < p.tile_len, "{w}x{h} tile {i} is at most a tile long");
                assert_eq!(if p.axis == Axis::Down { r.width } else { r.height }, p.short(), "{w}x{h} tile {i} keeps the short edge");
                next = b + 1;
            }
            assert_eq!(next, p.long(), "{w}x{h}: the last tile ends at the edge");
        }
    }

    #[test]
    fn a_part_holds_four_tiles_and_the_last_part_the_rest() {
        let p = TilePlan::plan(800, 20_000).unwrap();
        let cuts = p.nominal_cuts();
        let one: Vec<usize> = p.part_rects(&cuts, 1).unwrap().iter().map(|r| r.index).collect();
        let two: Vec<usize> = p.part_rects(&cuts, 2).unwrap().iter().map(|r| r.index).collect();
        assert_eq!((one, two), (vec![1, 2, 3, 4], vec![5, 6, 7, 8]));
        assert_eq!(p.part_rects(&cuts, 0), None);
        assert_eq!(p.part_rects(&cuts, 3), None);
        assert_eq!(p.parts_text(&cuts), "1 = tiles 1–4 (y 0–9999), 2 = tiles 5–8 (y 10000–19999)");
        // Nine tiles: the third part is one tile.
        let p = TilePlan::plan(800, 22_000).unwrap();
        let cuts = p.nominal_cuts();
        assert_eq!((p.total, p.parts()), (9, 3));
        assert_eq!(p.part_rects(&cuts, 3).unwrap().len(), 1);
        assert_eq!(p.tiles_text(3), "tile 9");
        assert!(p.parts_text(&cuts).ends_with("3 = tile 9 (y 19560–21999)"), "{}", p.parts_text(&cuts));
        // Many parts are counted, not all listed.
        let p = TilePlan::plan(800, 200_000).unwrap();
        let cuts = p.nominal_cuts();
        assert!(p.parts() > 12);
        assert!(p.parts_text(&cuts).ends_with(&format!("… and {} more", p.parts() - 12)), "{}", p.parts_text(&cuts));
    }

    #[test]
    fn a_cut_moves_to_the_nearest_blank_line_within_the_window_and_otherwise_stays() {
        let p = TilePlan::plan(400, 6000).unwrap();
        assert_eq!((p.total, p.tile_len, p.snap_window()), (3, 2000, MEDIA_TILE_SNAP));
        // Blank rows 30 px past each even cut: the cuts move there.
        let img = striped(400, 6000, |y| y % 1000 == 30);
        assert_eq!(p.cuts(&img), vec![0, 2030, 4030, 6000]);
        assert_eq!(p.tile_size_text(&p.cuts(&img)), "400×1970–2030");
        // Blank rows 60 px past: outside the window, the even cuts stand.
        let img = striped(400, 6000, |y| y % 1000 == 60);
        assert_eq!(p.cuts(&img), p.nominal_cuts());
        // Blank rows on both sides: the nearer wins, the later one at a tie.
        let img = striped(400, 6000, |y| y % 2000 == 1990 || y % 2000 == 5);
        assert_eq!(p.cuts(&img), vec![0, 2005, 4005, 6000]);
        // No blank row at all: the even cuts.
        let img = striped(400, 6000, |_| false);
        assert_eq!(p.cuts(&img), p.nominal_cuts());
        // Left to right: blank columns.
        let p = TilePlan::plan(6000, 400).unwrap();
        let img = DynamicImage::ImageRgba8(ImageBuffer::from_fn(6000, 400, |x, y| {
            if x % 1000 == 30 || y % 2 == 1 { Rgba([255, 255, 255, 255]) } else { Rgba([0, 0, 0, 255]) }
        }));
        assert_eq!(p.cuts(&img), vec![0, 2030, 4030, 6000]);
        // The window shrinks when a tile has little room under its box.
        let p = TilePlan::plan(800, 9997).unwrap();
        assert_eq!((p.tile_len, p.snap_window()), (2500, 38));
        // The edges are never cuts, whatever the window.
        let p = TilePlan::plan(400, 5160).unwrap();
        assert_eq!((p.total, p.tile_len), (3, 1720));
        let img = striped(400, 5160, |_| true);
        assert_eq!(p.cuts(&img)[3], 5160);
    }

    #[test]
    fn beside_a_figure_the_cut_falls_in_the_gap_between_two_lines_of_text() {
        // The recheck of 2026-10-18: a floated figure filled the whole
        // window at two cuts, so no row was one colour and the even cut
        // stood, through a line of text and the figure. The least ink in
        // the window is a gap between two lines beside the figure.
        let p = TilePlan::plan(800, 8000).unwrap();
        let img = text_beside_a_figure(800, 8000, |y| (1800..2200).contains(&y));
        let cuts = p.cuts(&img);
        // 2000 is a text row (2000 % 22 = 20); the nearest gap rows are
        // 2002..2005, and the nearer wins.
        assert_eq!(cuts[1], 2002, "{cuts:?}");
        // The other cuts have no figure: a gap row within the window
        // (4000 % 22 = 18 -> 4004; 6000 % 22 = 16 -> 6006).
        assert_eq!(&cuts[2..], &[4004, 6006, 8000], "{cuts:?}");
        // Every row equally busy: the even cut stands.
        let img = striped(800, 8000, |_| false);
        assert_eq!(p.cuts(&img), p.nominal_cuts());
    }

    #[test]
    fn a_cut_never_lengthens_a_tile_past_its_box() {
        // Four tiles of exactly 2576: the window is zero and blank rows a
        // pixel off the even cuts do not move them.
        let p = TilePlan::plan(800, 10_304).unwrap();
        assert_eq!((p.total, p.tile_len, p.snap_window()), (4, 2576, 0));
        let img = striped(800, 10_304, |y| y % 2576 == 1 || y % 2576 == 2575);
        assert_eq!(p.cuts(&img), vec![0, 2576, 5152, 7728, 10_304]);
        // With room under the box the window is bounded by half of it, so
        // two cuts moving apart stay within a tile of the box's length.
        let p = TilePlan::plan(800, 9997).unwrap();
        let img = striped(800, 9997, |y| y % 2500 == 2500 - 38 || y % 2500 == 38);
        let cuts = p.cuts(&img);
        let longest = cuts.windows(2).map(|w| w[1] - w[0]).max().unwrap();
        assert!(longest <= p.tile_max(), "{cuts:?}");
        assert_eq!(cuts, vec![0, 2538, 5038, 7538, 9997]);
    }

    #[test]
    fn a_long_png_comes_back_in_tiles_at_full_scale_and_a_part_past_the_last_is_refused() {
        let png = banded_png(600, 11_000);
        let View::Tiled(t) = view_blocking(png.clone(), 1).unwrap() else { panic!("a long image is tiled") };
        assert_eq!((t.plan.total, t.plan.tile_len, t.plan.parts(), t.part), (5, 2200, 2, 1));
        assert_eq!(t.cuts, vec![0, 2200, 4400, 6600, 8800, 11_000], "every row is blank: the even cuts stand");
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
        // A short image takes normalize's path, whatever part is asked,
        // and names its source size.
        let View::Whole { image, source_width, source_height } =
            view_blocking(ingest::tests::png_fixture(800, 4385), 2).unwrap()
        else {
            panic!("a short image is whole")
        };
        assert_eq!((source_width, source_height), (800, 4385));
        assert_eq!((image.width, image.height), (470, 2576));
    }
}
