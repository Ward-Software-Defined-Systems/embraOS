//! In-TUI image rendering (media wave) via `ratatui-image`.
//!
//! Protocol selection is NOT detected by querying the terminal on the web
//! path: embra-web spawns this console at boot, before any browser is
//! attached, so a stdin query would go unanswered. Instead:
//! - `EMBRA_TUI_GRAPHICS` names the protocol (embra-web sets `sixel` on
//!   its PTY; embrad forwards `embra.graphics=` for the serial console).
//! - Cell geometry comes from the PTY winsize pixel fields (`TIOCGWINSZ`
//!   `ws_xpixel`/`ws_ypixel` — the browser's resize frames carry them),
//!   re-read on every `Event::Resize`. A pixel protocol with UNKNOWN
//!   geometry degrades to halfblocks rather than guessing a cell size
//!   (a wrong guess spills sixel outside the pane).
//! - Serial default = halfblocks (plain cells, any terminal, no escape
//!   protocol on the line). `auto` is the operator opt-in that runs the
//!   1 s stdin query (iTerm2/WezTerm/foot hosts) — only ever before the
//!   event-reader thread starts, since the query reads stdin.

use std::io::Cursor;

use embra_common::proto::brain::MediaRef;
use image::{DynamicImage, ImageReader, Limits};
use ratatui_image::picker::{Picker, ProtocolType};
use ratatui_image::protocol::StatefulProtocol;

/// Rows the media band occupies (incl. borders) when visible.
pub const MEDIA_PANE_ROWS: u16 = 12;
/// Minimum terminal rows for the band to show at all (mirrors the
/// expression panel's hide-on-small rule, one notch stricter).
pub const MEDIA_PANE_MIN_ROWS: u16 = 30;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum GraphicsMode {
    Off,
    Halfblocks,
    Sixel,
    Kitty,
    Iterm2,
    Auto,
}

impl GraphicsMode {
    pub fn parse(s: &str) -> Option<Self> {
        match s.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" => Some(GraphicsMode::Off),
            "halfblocks" | "halfblock" | "blocks" => Some(GraphicsMode::Halfblocks),
            "sixel" => Some(GraphicsMode::Sixel),
            "kitty" => Some(GraphicsMode::Kitty),
            "iterm2" | "iterm" | "iip" => Some(GraphicsMode::Iterm2),
            "auto" | "query" => Some(GraphicsMode::Auto),
            _ => None,
        }
    }

    /// `EMBRA_TUI_GRAPHICS`, else the surface default: sixel on the web
    /// PTY, halfblocks on serial. An unparseable value falls back to the
    /// surface default (logged by the caller).
    pub fn resolve(env_value: Option<&str>, web_pty: bool) -> Self {
        env_value
            .and_then(GraphicsMode::parse)
            .unwrap_or(if web_pty { GraphicsMode::Sixel } else { GraphicsMode::Halfblocks })
    }

    pub fn as_str(self) -> &'static str {
        match self {
            GraphicsMode::Off => "off",
            GraphicsMode::Halfblocks => "halfblocks",
            GraphicsMode::Sixel => "sixel",
            GraphicsMode::Kitty => "kitty",
            GraphicsMode::Iterm2 => "iterm2",
            GraphicsMode::Auto => "auto",
        }
    }

    fn pixel_protocol(self) -> Option<ProtocolType> {
        match self {
            GraphicsMode::Sixel => Some(ProtocolType::Sixel),
            GraphicsMode::Kitty => Some(ProtocolType::Kitty),
            GraphicsMode::Iterm2 => Some(ProtocolType::Iterm2),
            _ => None,
        }
    }
}

/// Cell size in pixels from the PTY winsize, when the pixel fields are
/// populated (embra-web plumbs them from the browser).
pub fn winsize_font_size() -> Option<(u16, u16)> {
    // SAFETY: TIOCGWINSZ fills a plain C struct; stdout is the terminal.
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::ioctl(libc::STDOUT_FILENO, libc::TIOCGWINSZ, &mut ws) };
    if rc != 0 || ws.ws_col == 0 || ws.ws_row == 0 || ws.ws_xpixel == 0 || ws.ws_ypixel == 0 {
        return None;
    }
    let fw = ws.ws_xpixel / ws.ws_col;
    let fh = ws.ws_ypixel / ws.ws_row;
    if fw == 0 || fh == 0 {
        return None;
    }
    Some((fw, fh))
}

/// Halfblocks need no real geometry; ratatui-image's own default ratio.
const FALLBACK_FONT: (u16, u16) = (10, 20);

pub struct Graphics {
    mode: GraphicsMode,
    picker: Option<Picker>,
    /// True when the active picker is rendering the REQUESTED pixel
    /// protocol (vs. the halfblocks fallback for unknown geometry).
    geometry_known: bool,
}

impl Graphics {
    /// Build once at startup. For `Auto` this writes/reads stdin (1 s
    /// timeout) — call it BEFORE the event-reader thread exists.
    pub fn init(mode: GraphicsMode) -> Self {
        let (picker, geometry_known) = match mode {
            GraphicsMode::Off => (None, false),
            GraphicsMode::Auto => match Picker::from_query_stdio() {
                Ok(p) => (Some(p), true),
                Err(_) => {
                    let mut p = Picker::from_fontsize(FALLBACK_FONT);
                    p.set_protocol_type(ProtocolType::Halfblocks);
                    (Some(p), false)
                }
            },
            GraphicsMode::Halfblocks => {
                let mut p = Picker::from_fontsize(winsize_font_size().unwrap_or(FALLBACK_FONT));
                p.set_protocol_type(ProtocolType::Halfblocks);
                (Some(p), true)
            }
            GraphicsMode::Sixel | GraphicsMode::Kitty | GraphicsMode::Iterm2 => {
                Self::pixel_picker(mode, winsize_font_size())
            }
        };
        Graphics { mode, picker, geometry_known }
    }

    fn pixel_picker(mode: GraphicsMode, font: Option<(u16, u16)>) -> (Option<Picker>, bool) {
        match font {
            Some(fs) => {
                let mut p = Picker::from_fontsize(fs);
                if let Some(proto) = mode.pixel_protocol() {
                    p.set_protocol_type(proto);
                }
                (Some(p), true)
            }
            None => {
                // Geometry unknown (no browser attached yet, or an old
                // embra-web): halfblocks until a Resize brings pixels.
                let mut p = Picker::from_fontsize(FALLBACK_FONT);
                p.set_protocol_type(ProtocolType::Halfblocks);
                (Some(p), false)
            }
        }
    }

    /// Re-read the winsize geometry (on `Event::Resize`). Returns true
    /// when the picker changed, so callers rebuild the pane's protocol.
    pub fn refresh_geometry(&mut self) -> bool {
        if !matches!(self.mode, GraphicsMode::Sixel | GraphicsMode::Kitty | GraphicsMode::Iterm2 | GraphicsMode::Halfblocks) {
            return false;
        }
        let font = winsize_font_size();
        let current = self.picker.as_ref().map(Picker::font_size);
        if self.mode == GraphicsMode::Halfblocks {
            // Geometry only affects the aspect math; refresh quietly.
            let fs = font.unwrap_or(FALLBACK_FONT);
            if current == Some(fs) {
                return false;
            }
            let mut p = Picker::from_fontsize(fs);
            p.set_protocol_type(ProtocolType::Halfblocks);
            self.picker = Some(p);
            return true;
        }
        match font {
            Some(fs) if !self.geometry_known || current != Some(fs) => {
                let (p, known) = Self::pixel_picker(self.mode, Some(fs));
                self.picker = p;
                self.geometry_known = known;
                true
            }
            _ => false,
        }
    }

    pub fn enabled(&self) -> bool {
        self.picker.is_some()
    }

    /// What is actually being rendered right now.
    pub fn active_protocol(&self) -> &'static str {
        match self.picker.as_ref().map(Picker::protocol_type) {
            None => "off",
            Some(ProtocolType::Halfblocks) => "halfblocks",
            Some(ProtocolType::Sixel) => "sixel",
            Some(ProtocolType::Kitty) => "kitty",
            Some(ProtocolType::Iterm2) => "iterm2",
        }
    }

    pub fn font_size(&self) -> (u16, u16) {
        self.picker.as_ref().map(Picker::font_size).unwrap_or(FALLBACK_FONT)
    }

    pub fn make_protocol(&self, image: DynamicImage) -> Option<StatefulProtocol> {
        self.picker.as_ref().map(|p| p.new_resize_protocol(image))
    }
}

/// The pane's model: the decoded image (kept so geometry changes can
/// rebuild the encoder) plus the live protocol state.
pub struct MediaPane {
    pub meta: MediaRef,
    pub image: DynamicImage,
    pub protocol: StatefulProtocol,
}

impl std::fmt::Debug for MediaPane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MediaPane")
            .field("id", &self.meta.id)
            .field("image", &(self.image.width(), self.image.height()))
            .finish()
    }
}

/// Decode stored bytes for the pane, thumbnailed to `target_px` (the
/// pane's pixel box × 2 for HiDPI headroom; ratatui-image fits the rest).
/// Decode limits mirror the brain's ingest guards.
pub fn decode_for_pane(bytes: &[u8], target_px: (u32, u32)) -> Result<DynamicImage, String> {
    let mut reader = ImageReader::new(Cursor::new(bytes))
        .with_guessed_format()
        .map_err(|e| e.to_string())?;
    let mut limits = Limits::default();
    limits.max_image_width = Some(8000);
    limits.max_image_height = Some(8000);
    limits.max_alloc = Some(512 * 1024 * 1024);
    reader.limits(limits);
    let img = reader.decode().map_err(|e| e.to_string())?;
    let (tw, th) = (target_px.0.max(64), target_px.1.max(64));
    if img.width() > tw || img.height() > th {
        Ok(img.thumbnail(tw, th))
    } else {
        Ok(img)
    }
}

/// Pixel box of the pane's inner area for the current geometry.
pub fn pane_target_px(viewport_cols: u16, font: (u16, u16)) -> (u32, u32) {
    let inner_cols = viewport_cols.saturating_sub(2) as u32;
    let inner_rows = MEDIA_PANE_ROWS.saturating_sub(2) as u32;
    (
        (inner_cols * font.0 as u32 * 2).clamp(64, 2576),
        (inner_rows * font.1 as u32 * 2).clamp(64, 2576),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn graphics_mode_parses_cmdline_values() {
        assert_eq!(GraphicsMode::parse("sixel"), Some(GraphicsMode::Sixel));
        assert_eq!(GraphicsMode::parse("SIXEL "), Some(GraphicsMode::Sixel));
        assert_eq!(GraphicsMode::parse("halfblocks"), Some(GraphicsMode::Halfblocks));
        assert_eq!(GraphicsMode::parse("kitty"), Some(GraphicsMode::Kitty));
        assert_eq!(GraphicsMode::parse("iterm2"), Some(GraphicsMode::Iterm2));
        assert_eq!(GraphicsMode::parse("auto"), Some(GraphicsMode::Auto));
        assert_eq!(GraphicsMode::parse("off"), Some(GraphicsMode::Off));
        assert_eq!(GraphicsMode::parse("bogus"), None);
    }

    #[test]
    fn graphics_mode_defaults_sixel_on_pty_halfblocks_on_serial() {
        assert_eq!(GraphicsMode::resolve(None, true), GraphicsMode::Sixel);
        assert_eq!(GraphicsMode::resolve(None, false), GraphicsMode::Halfblocks);
        assert_eq!(GraphicsMode::resolve(Some("bogus"), false), GraphicsMode::Halfblocks);
        assert_eq!(GraphicsMode::resolve(Some("kitty"), false), GraphicsMode::Kitty);
        assert_eq!(GraphicsMode::resolve(Some("off"), true), GraphicsMode::Off);
    }

    #[test]
    fn pixel_protocol_without_geometry_falls_back_to_halfblocks() {
        // No winsize pixels in a test process → halfblocks, flagged unknown.
        let (p, known) = Graphics::pixel_picker(GraphicsMode::Sixel, None);
        assert!(!known);
        assert_eq!(p.unwrap().protocol_type(), ProtocolType::Halfblocks);
        let (p, known) = Graphics::pixel_picker(GraphicsMode::Sixel, Some((9, 18)));
        assert!(known);
        let p = p.unwrap();
        assert_eq!(p.protocol_type(), ProtocolType::Sixel);
        assert_eq!(p.font_size(), (9, 18));
    }

    #[test]
    fn pane_target_px_tracks_geometry() {
        assert_eq!(pane_target_px(82, (8, 16)), (80 * 8 * 2, 10 * 16 * 2));
        assert_eq!(pane_target_px(400, (10, 20)), (2576, 400));
        assert_eq!(pane_target_px(2, (8, 16)), (64, 320));
    }

    #[test]
    fn decode_for_pane_thumbnails_large_images() {
        let img = image::RgbaImage::from_fn(600, 300, |x, _| image::Rgba([(x % 256) as u8, 0, 0, 255]));
        let mut out = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img).write_to(&mut out, image::ImageFormat::Png).unwrap();
        let decoded = decode_for_pane(&out.into_inner(), (300, 300)).unwrap();
        assert_eq!((decoded.width(), decoded.height()), (300, 150));
        assert!(decode_for_pane(b"not an image", (64, 64)).is_err());
    }
}
