//! The themes of the web console.
//!
//! The browser's page, the phone's page and the machine's own display are
//! one page, and one theme is in force for the whole machine. Its name is
//! kept in one small file on STATE ([`STATE_FILE`]), which two doors write:
//! the brain's `/theme` and the web console's picker (`POST /api/theme`).
//! The web console reads it for every page it serves and for every status
//! answer, so a page is drawn in the stored theme from its first paint and
//! an open page follows a change at its next poll.
//!
//! The colours are in the page's stylesheet (`embra-web-ui/assets/app.css`),
//! one block of tokens a theme, selected by the `data-theme` attribute. This
//! list is what the brain, the web console and the page agree on: a name
//! here has a block there (`the_stylesheet_has_a_block_for_every_theme`,
//! embra-web), and a name that is not here is no theme.

/// One theme.
#[derive(Debug, PartialEq, Eq)]
pub struct Theme {
    /// What the operator types, what the file holds, and the value of the
    /// stylesheet's `data-theme`.
    pub name: &'static str,
    /// One line for a listing and for the picker.
    pub note: &'static str,
    /// Whether its text holds the enhanced contrast of WCAG 2.1 (7:1,
    /// level AAA). Every theme holds the minimum (4.5:1, level AA).
    pub aaa: bool,
}

/// The themes, in the order a listing shows them. The first is the default.
pub const THEMES: &[Theme] = &[
    Theme { name: "ember", note: "flame orange on warm black", aaa: false },
    Theme { name: "nebula", note: "pink and cyan on deep violet", aaa: false },
    Theme { name: "lagoon", note: "turquoise and coral on deep teal", aaa: false },
    Theme { name: "forest", note: "lime and gold on dark green", aaa: false },
    Theme { name: "cobalt", note: "yellow and sky blue on navy; no state is told by red against green", aaa: false },
    Theme { name: "daylight", note: "dark ink on warm paper", aaa: false },
    Theme { name: "contrast-dark", note: "high contrast on black (WCAG AAA)", aaa: true },
    Theme { name: "contrast-light", note: "high contrast on white (WCAG AAA)", aaa: true },
];

/// The theme of a machine that has stored none.
pub const DEFAULT: &str = THEMES[0].name;

/// Where the machine's theme is kept: one line, a theme's name. On STATE,
/// so it travels with a backup; no tool reads STATE.
pub const STATE_FILE: &str = "/embra/state/theme";

/// The contrast a theme's text holds to against its background.
pub const fn text_contrast(theme: &Theme) -> f64 {
    if theme.aaa { 7.0 } else { 4.5 }
}

/// The theme of that name, as the operator may have typed it.
pub fn find(name: &str) -> Option<&'static Theme> {
    let name = name.trim();
    THEMES.iter().find(|theme| theme.name.eq_ignore_ascii_case(name))
}

/// The default theme.
pub fn default_theme() -> &'static Theme {
    &THEMES[0]
}

/// The theme a stored file names: its first line. A file that names no
/// theme of this build (an empty one, one written by a later release) is
/// the default, so a page is never left without a theme.
pub fn from_stored(text: &str) -> &'static Theme {
    text.lines().next().and_then(find).unwrap_or_else(default_theme)
}

/// The theme stored at `path`; the default when there is no file or it
/// cannot be read.
#[cfg(not(target_arch = "wasm32"))]
pub fn read(path: &std::path::Path) -> &'static Theme {
    match std::fs::read_to_string(path) {
        Ok(text) => from_stored(&text),
        Err(_) => default_theme(),
    }
}

/// Store `name` at `path` as the machine's theme. A name that is no theme
/// is refused and nothing is written. The file is written beside its place
/// and moved into it, so a reader finds the old name or the new one.
#[cfg(not(target_arch = "wasm32"))]
pub fn write(path: &std::path::Path, name: &str) -> std::io::Result<&'static Theme> {
    use std::io::Write as _;
    let theme = find(name).ok_or_else(|| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, format!("no theme is called {:?}", name.trim()))
    })?;
    let mut beside = path.as_os_str().to_owned();
    beside.push(".new");
    let beside = std::path::PathBuf::from(beside);
    let written = (|| {
        let mut file = std::fs::File::create(&beside)?;
        file.write_all(theme.name.as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::rename(&beside, path)
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&beside);
    }
    written.map(|()| theme)
}

/// Forget the stored theme: the machine is on the default again.
#[cfg(not(target_arch = "wasm32"))]
pub fn reset(path: &std::path::Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Err(e) if e.kind() != std::io::ErrorKind::NotFound => Err(e),
        _ => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scratch(name: &str) -> std::path::PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-theme-{}-{name}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("a scratch directory");
        dir.join("theme")
    }

    #[test]
    fn the_names_are_distinct_lowercase_words_and_the_first_is_the_default() {
        assert_eq!(DEFAULT, THEMES[0].name);
        for (at, theme) in THEMES.iter().enumerate() {
            assert!(
                !theme.name.is_empty()
                    && theme.name.bytes().all(|b| b.is_ascii_lowercase() || b == b'-')
                    && !theme.name.starts_with('-')
                    && !theme.name.ends_with('-'),
                "{:?} is typed by an operator and written into an attribute",
                theme.name
            );
            assert!(!theme.note.is_empty() && !theme.note.ends_with('.'), "{:?}", theme.note);
            assert!(THEMES[..at].iter().all(|other| other.name != theme.name), "{} twice", theme.name);
        }
        // `reset` is the command's own word and can never be a theme.
        assert!(find("reset").is_none());
    }

    #[test]
    fn a_name_is_found_as_it_was_typed_and_nothing_else_is() {
        assert_eq!(find(" Ember \n").map(|t| t.name), Some("ember"));
        for not_a_theme in ["", "embers", "ember dark", "../ember", "ember\0"] {
            assert!(find(not_a_theme).is_none(), "{not_a_theme:?}");
        }
    }

    #[test]
    fn a_stored_file_that_names_no_theme_is_the_default() {
        assert_eq!(from_stored("ember\n").name, "ember");
        for stored in ["", "\n", "a-theme-of-a-later-release\n", "ember and more\n"] {
            assert_eq!(from_stored(stored).name, DEFAULT, "{stored:?}");
        }
    }

    #[test]
    fn a_theme_is_written_read_back_and_forgotten() {
        let path = scratch("round-trip");
        assert_eq!(read(&path).name, DEFAULT, "no file yet");
        for theme in THEMES {
            assert_eq!(write(&path, theme.name).expect("a write").name, theme.name);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{}\n", theme.name));
            assert_eq!(read(&path).name, theme.name);
        }
        assert!(!std::path::Path::new(&format!("{}.new", path.display())).exists(), "nothing is left beside it");
        reset(&path).expect("a reset");
        assert!(!path.exists());
        reset(&path).expect("a second reset finds nothing and says nothing");
        assert_eq!(read(&path).name, DEFAULT);
    }

    #[test]
    fn a_name_that_is_no_theme_is_refused_and_the_file_is_left_alone() {
        let path = scratch("refused");
        write(&path, DEFAULT).expect("a write");
        let err = write(&path, "plaid").expect_err("no such theme");
        assert_eq!(err.kind(), std::io::ErrorKind::InvalidInput);
        assert_eq!(err.to_string(), "no theme is called \"plaid\"");
        assert_eq!(std::fs::read_to_string(&path).unwrap(), format!("{DEFAULT}\n"));
    }

    #[test]
    fn a_write_that_fails_leaves_nothing_beside_the_file() {
        let path = scratch("no-such").parent().unwrap().join("missing-dir").join("theme");
        assert!(write(&path, DEFAULT).is_err());
        assert!(!path.parent().unwrap().exists());
    }
}
