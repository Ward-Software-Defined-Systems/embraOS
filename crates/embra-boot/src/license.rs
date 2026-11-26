//! The licence the OS is released under: where its text is in the root
//! filesystem, and how a text is named.
//!
//! The image build installs the repository's `LICENSE` at [`PATH`], byte for
//! byte, and reads it back (`assert_license_text`,
//! `scripts/lib/image-common.sh`); the installer medium carries the same
//! file at its root. The installer shows the text before it writes anything
//! (`embra-update`'s `install.rs`). An acceptance is of one text: it names
//! the text by its version line, and a record of one adds the text's
//! SHA-256, computed by whoever keeps the record (this crate has no
//! hashing).

/// Where the licence's text is in the root filesystem.
pub const PATH: &str = "/usr/share/licenses/embraos/LICENSE";

/// The text's first line.
pub const TITLE: &str = "embraOS PROPRIETARY LICENSE";

/// The longest version line that is taken for one.
const VERSION_LINE_MAX: usize = 80;

/// The text's version line, its third ("Version 2.0, November 26, 2026").
/// `None` when the text has not the licence's shape: the title on its first
/// line, and on its third a line of printable ASCII that begins with
/// `Version` and a number.
pub fn version_line(text: &str) -> Option<&str> {
    let mut lines = text.lines();
    if lines.next()?.trim_end() != TITLE {
        return None;
    }
    lines.next()?;
    let line = lines.next()?.trim_end();
    let rest = line.strip_prefix("Version ")?;
    let shaped = rest.starts_with(|c: char| c.is_ascii_digit())
        && line.len() <= VERSION_LINE_MAX
        && line.bytes().all(|b| b == b' ' || b.is_ascii_graphic());
    shaped.then_some(line)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The repository's own text has the shape, and its version line is
    /// what the installer and the OS name it by. The image build holds the
    /// same two lines of it (`assert_license_text`).
    #[test]
    fn the_repositorys_licence_has_its_title_and_its_version_on_the_third_line() {
        let text = include_str!("../../../LICENSE");
        assert_eq!(text.lines().next(), Some(TITLE));
        let version = version_line(text).expect("a version line");
        assert!(version.starts_with("Version ") && version.contains(", 20"), "{version}");
        // What the installer shows a screen at a time: plain text of a console's width.
        assert!(text.is_ascii(), "the text is shown on a serial line");
        assert!(text.lines().all(|line| line.len() <= 74), "74 columns at most");
        assert_eq!(PATH, "/usr/share/licenses/embraos/LICENSE");
    }

    #[test]
    fn a_text_without_the_licences_shape_has_no_version_line() {
        assert_eq!(version_line("embraOS PROPRIETARY LICENSE\n\nVersion 2.0, November 26, 2026\n\nbody"), Some("Version 2.0, November 26, 2026"));
        assert_eq!(version_line("embraOS PROPRIETARY LICENSE\r\n\r\nVersion 3, 2027\r\n"), Some("Version 3, 2027"), "line ends of either kind");
        for text in [
            "",
            "embraOS PROPRIETARY LICENSE",
            "embraOS PROPRIETARY LICENSE\n\n",
            "embraOS PROPRIETARY LICENSE\nVersion 2.0, November 26, 2026\n",
            "Another licence\n\nVersion 2.0, November 26, 2026\n",
            "embraOS PROPRIETARY LICENSE\n\nversion 2.0\n",
            "embraOS PROPRIETARY LICENSE\n\nVersion two\n",
            "embraOS PROPRIETARY LICENSE\n\nVersion 2.0 \u{202e}\n",
            "embraOS PROPRIETARY LICENSE\n\nVersion 2.0\tx\n",
        ] {
            assert_eq!(version_line(text), None, "{text:?}");
        }
        let long = format!("embraOS PROPRIETARY LICENSE\n\nVersion 2.0, {}\n", "x".repeat(80));
        assert_eq!(version_line(&long), None);
    }
}
