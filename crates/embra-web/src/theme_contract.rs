//! The page's stylesheet held to what the themes promise.
//!
//! The web console's colours are tokens, one block a theme, in
//! `embra-web-ui/assets/app.css`; the themes' names are
//! `embra_tools_core::theme::THEMES`. These tests read the stylesheet and
//! compute what the manual says of it: a block for every theme and the same
//! tokens in each; the contrast WCAG 2.1 asks on every pair that is read
//! (4.5:1 for text, 7:1 in the two themes that say so, 3:1 for a field's
//! edge and the focus ring); no colour written outside the blocks, no text
//! dimmed by opacity or set under twelve pixels, the keyboard's place shown,
//! and less motion for a reader who asked for it.
//!
//! Three things a stylesheet does not show are held here too, each read
//! from the source that draws it: the words the activity strip draws on its
//! canvas, the ring of a card in the theme picker, and the lines the web
//! console writes into the terminal itself.
//!
//! They live in this crate because CI runs its tests and not the page's.

use embra_tools_core::theme::{self, THEMES};
use std::collections::BTreeMap;

const CSS: &str = include_str!("../../embra-web-ui/assets/app.css");
/// The activity strip: it draws on a canvas, so no rule of the stylesheet
/// sets its text.
const STRIP: &str = include_str!("../../embra-web-ui/src/activity.rs");
/// The two places the web console writes lines of its own into the terminal.
const TERMINAL_PAGE: &str = include_str!("../../embra-web-ui/assets/embra-term.js");
const PTY_BRIDGE: &str = include_str!("pty_bridge.rs");

/// The grounds a text is read on, and the tokens that are read as text.
const GROUNDS: [&str; 5] = ["bg", "bg-2", "panel", "panel-2", "bg-glow"];
const TEXT: [&str; 9] = ["txt", "txt-dim", "accent", "accent-2", "good", "warn", "bad", "json-key", "json-str"];
/// What is read as a shape and not as text: a field's edge, the focus
/// ring, the wordmark's last colour. WCAG 1.4.11 asks 3:1 of these.
const SHAPES: [&str; 3] = ["control-line", "focus", "accent-red"];
/// Text on a fill.
const ON_FILL: [(&str, &str); 4] =
    [("on-accent", "accent"), ("on-accent", "accent-2"), ("on-good", "good"), ("on-danger", "danger")];

type Tokens = BTreeMap<String, String>;

/// The theme blocks of the stylesheet, in its order: a name and its tokens.
fn blocks() -> Vec<(String, Tokens)> {
    let mut out = Vec::new();
    let mut rest = CSS;
    while let Some(at) = rest.find("[data-theme=\"") {
        let after = &rest[at + 13..];
        let name = &after[..after.find('"').expect("the name's end")];
        let open = after.find('{').expect("the block's start");
        // A selector list ends at its `{`: anything else before it is a use of the attribute in a rule.
        let selector = &after[name.len()..open];
        let close = after.find('}').expect("the block's end");
        if selector.trim() == "\"]" {
            let mut tokens = Tokens::new();
            for declaration in after[open + 1..close].split(';') {
                let declaration = declaration.rsplit("*/").next().unwrap_or(declaration).trim();
                if let Some((token, value)) = declaration.strip_prefix("--").and_then(|d| d.split_once(':')) {
                    assert!(tokens.insert(token.trim().to_string(), value.trim().to_string()).is_none(), "{name}: --{token} twice");
                }
            }
            out.push((name.to_string(), tokens));
        }
        rest = &after[close..];
    }
    out
}

fn channels(hex: &str) -> [f64; 4] {
    let hex = hex.strip_prefix('#').unwrap_or_else(|| panic!("{hex} is a hex colour"));
    assert!(matches!(hex.len(), 6 | 8) && hex.bytes().all(|b| b.is_ascii_hexdigit()), "#{hex} is #rrggbb or #rrggbbaa");
    let at = |i: usize| f64::from(u8::from_str_radix(&hex[i..i + 2], 16).unwrap());
    [at(0), at(2), at(4), if hex.len() == 8 { at(6) / 255.0 } else { 1.0 }]
}

/// WCAG 2.1's relative luminance of an sRGB colour.
fn luminance(rgb: [f64; 4]) -> f64 {
    let linear = |v: f64| {
        let v = v / 255.0;
        if v <= 0.04045 { v / 12.92 } else { ((v + 0.055) / 1.055).powf(2.4) }
    };
    0.2126 * linear(rgb[0]) + 0.7152 * linear(rgb[1]) + 0.0722 * linear(rgb[2])
}

/// WCAG 2.1's contrast ratio of two opaque colours.
fn contrast(a: [f64; 4], b: [f64; 4]) -> f64 {
    let (a, b) = (luminance(a), luminance(b));
    (a.max(b) + 0.05) / (a.min(b) + 0.05)
}

/// A colour with an alpha, laid over an opaque one.
fn over(top: [f64; 4], under: [f64; 4]) -> [f64; 4] {
    let mix = |i: usize| top[i] * top[3] + under[i] * (1.0 - top[3]);
    [mix(0), mix(1), mix(2), 1.0]
}

fn colour(tokens: &Tokens, token: &str) -> [f64; 4] {
    channels(tokens.get(token).unwrap_or_else(|| panic!("--{token} is defined")))
}

#[test]
fn the_contrast_is_wcags() {
    let (black, white) = (channels("#000000"), channels("#ffffff"));
    assert!((contrast(black, white) - 21.0).abs() < 1e-9);
    assert!((contrast(white, white) - 1.0).abs() < 1e-9);
    // The two greys every table of the criterion quotes: the lightest that passes 4.5:1 on white, and the next.
    assert!(contrast(channels("#767676"), white) >= 4.5);
    assert!(contrast(channels("#777777"), white) < 4.5);
    assert_eq!(over(channels("#ffffff80"), black).map(f64::round), [128.0, 128.0, 128.0, 1.0]);
}

#[test]
fn the_stylesheet_has_a_block_for_every_theme_in_the_lists_order_and_no_other() {
    let in_css: Vec<String> = blocks().into_iter().map(|(name, _)| name).collect();
    let in_list: Vec<&str> = THEMES.iter().map(|t| t.name).collect();
    assert_eq!(in_css, in_list);
    // The default is also what a root with no attribute, or an unknown one, shows.
    assert!(CSS.contains(&format!(":root, [data-theme=\"{}\"] {{", theme::DEFAULT)));
    assert_eq!(CSS.matches(":root, [data-theme=").count(), 1);
}

#[test]
fn every_theme_defines_the_same_tokens_each_a_colour_but_the_floor() {
    let blocks = blocks();
    let first: Vec<&String> = blocks[0].1.keys().collect();
    assert!(first.len() >= 40, "{} tokens", first.len());
    for (name, tokens) in &blocks {
        assert_eq!(tokens.keys().collect::<Vec<_>>(), first, "{name}");
        for (token, value) in tokens {
            match token.as_str() {
                "term-floor" => assert!(value.parse::<f64>().is_ok_and(|f| (1.0..=21.0).contains(&f)), "{name}: {value}"),
                "term-selection" => assert_eq!(value.len(), 9, "{name}: the selection is a colour with an alpha"),
                _ => assert_eq!((value.len(), channels(value)[3]), (7, 1.0), "{name}: --{token} is an opaque #rrggbb"),
            }
        }
    }
}

/// The promise: every theme's text holds 4.5:1 against every ground it
/// is read on (WCAG 2.1 level AA, 1.4.3), and 7:1 in the themes that
/// say so (level AAA, 1.4.6); a field's edge and the focus ring hold
/// 3:1 (1.4.11).
#[test]
fn every_pair_that_is_read_holds_its_contrast_in_every_theme() {
    let mut under = Vec::new();
    for ((name, tokens), listed) in blocks().iter().zip(THEMES) {
        let floor = theme::text_contrast(listed);
        let mut hold = |what: String, fg: [f64; 4], bg: [f64; 4], need: f64| {
            let ratio = contrast(fg, bg);
            if ratio < need {
                under.push(format!("{name}: {what} is {ratio:.2}:1, under {need}:1"));
            }
        };
        for ground in GROUNDS {
            let bg = colour(tokens, ground);
            for text in TEXT {
                hold(format!("--{text} on --{ground}"), colour(tokens, text), bg, floor);
            }
            for shape in SHAPES {
                hold(format!("--{shape} on --{ground}"), colour(tokens, shape), bg, 3.0);
            }
        }
        for (text, fill) in ON_FILL {
            hold(format!("--{text} on --{fill}"), colour(tokens, text), colour(tokens, fill), floor);
        }
    }
    assert!(under.is_empty(), "{} pairs under their floor:\n{}", under.len(), under.join("\n"));
}

/// The terminal holds every character to the theme's floor against its
/// own cell (`--term-floor`, xterm's `minimumContrastRatio`): a console
/// program names sixteen colours and no ground. The palette itself keeps
/// every colour it uses as text at 3:1 against the terminal's ground, so
/// that the floor shifts a shade and never a hue; the selection is seen
/// against that ground too. In the two high-contrast themes the selection
/// sits a little under 3:1: text at 7:1 against both ends leaves no room
/// for a third step, and the text has the priority.
#[test]
fn the_terminals_colours_hold_theirs() {
    for ((name, tokens), listed) in blocks().iter().zip(THEMES) {
        let floor: f64 = tokens["term-floor"].parse().unwrap();
        assert!(floor >= theme::text_contrast(listed), "{name}: the terminal's floor is {floor}");
        let ground = colour(tokens, "bg");
        for ansi in (1..8).chain(9..16) {
            let ratio = contrast(colour(tokens, &format!("ansi-{ansi}")), ground);
            assert!(ratio >= 3.0, "{name}: --ansi-{ansi} is {ratio:.2}:1 against the terminal's ground");
        }
        let selection = contrast(over(colour(tokens, "term-selection"), ground), ground);
        let need = if listed.aaa { 2.5 } else { 3.0 };
        assert!(selection >= need, "{name}: the selection is {selection:.2}:1 against the terminal's ground");
    }
}

/// The rules of the stylesheet, outside the theme blocks: a selector and
/// its declarations, with the at-rule it sits in.
fn rules() -> Vec<(String, String, String)> {
    let body = &CSS[CSS.find(":root {").expect("the block of sizes after the themes")..];
    let mut clean = String::new();
    let mut rest = body;
    while let Some(at) = rest.find("/*") {
        clean.push_str(&rest[..at]);
        rest = &rest[at + rest[at..].find("*/").expect("a comment's end") + 2..];
    }
    clean.push_str(rest);
    let (mut out, mut at_rule, mut selector, mut depth) = (Vec::new(), String::new(), String::new(), 0usize);
    let mut text = String::new();
    for c in clean.chars() {
        match c {
            '{' => {
                let head = text.trim().to_string();
                text.clear();
                if head.starts_with('@') && depth == 0 {
                    at_rule = head;
                } else {
                    selector = head;
                }
                depth += 1;
            }
            '}' => {
                if !selector.is_empty() {
                    out.push((at_rule.clone(), std::mem::take(&mut selector), text.trim().to_string()));
                } else {
                    at_rule.clear();
                }
                text.clear();
                depth -= 1;
            }
            _ => text.push(c),
        }
    }
    assert_eq!(depth, 0, "the braces of the stylesheet pair up");
    out
}

/// A colour is a token: a theme cannot replace a literal. The four rules
/// that keep theirs draw over a picture, which no theme changes.
#[test]
fn no_rule_writes_a_colour_of_its_own_but_the_four_over_a_picture() {
    let mut found = Vec::new();
    for (_, selector, declarations) in rules() {
        let literal = declarations.split(|c: char| !(c.is_ascii_alphanumeric() || c == '#' || c == '(')).any(|word| {
            (word.starts_with('#') && matches!(word.len(), 4 | 5 | 7 | 9) && word[1..].bytes().all(|b| b.is_ascii_hexdigit()))
                || word.starts_with("rgb(")
                || word.starts_with("rgba(")
                || word.starts_with("hsl(")
                || word.starts_with("hsla(")
        });
        if literal {
            found.push(selector);
        }
    }
    assert_eq!(found, [".pending-thumb .pt-remove", ".pending-thumb .pt-err", ".lightbox", ".lightbox-cap"]);
}

/// Text is dimmed by a token that holds its contrast, never by opacity,
/// which no contrast figure sees. What may fade: a control that is
/// switched off, a picture, and an animation's own steps.
#[test]
fn no_text_is_dimmed_by_opacity() {
    for (at_rule, selector, declarations) in rules() {
        // `opacity: 1` fades nothing: it undoes a browser's own fade (a placeholder's in Firefox).
        let fades = declarations.split(';').any(|d| {
            d.trim().strip_prefix("opacity:").is_some_and(|v| v.trim().parse::<f64>().map_or(true, |v| v < 1.0))
        });
        if !fades || at_rule.starts_with("@keyframes") {
            continue;
        }
        let allowed = selector.contains(":disabled") || selector.ends_with(" img") || selector.ends_with(".off");
        assert!(allowed, "`{selector}` fades what it holds: {declarations}");
    }
}

#[test]
fn the_keyboards_place_is_shown_and_no_rule_hides_it() {
    let ring = rules().into_iter().find(|(_, selector, _)| selector == ":focus-visible").expect("a rule for :focus-visible");
    assert!(ring.2.contains("outline:") && ring.2.contains("var(--focus)"), "{}", ring.2);
    for (_, selector, declarations) in rules() {
        for declaration in declarations.split(';') {
            let declaration: String = declaration.split_whitespace().collect();
            assert!(
                !matches!(declaration.as_str(), "outline:none" | "outline:0" | "outline-width:0" | "outline-style:none"),
                "`{selector}` hides the focus ring"
            );
        }
    }
}

#[test]
fn no_text_is_set_under_twelve_pixels() {
    for (_, selector, declarations) in rules() {
        for declaration in declarations.split(';') {
            if let Some(size) = declaration.trim().strip_prefix("font-size:").and_then(|v| v.trim().strip_suffix("px")) {
                let size: f64 = size.parse().unwrap_or_else(|_| panic!("`{selector}`: font-size {size}px"));
                assert!(size >= 12.0, "`{selector}` sets text at {size}px");
            }
        }
    }
}

/// A reader who asked the system for less motion gets none from the page.
#[test]
fn the_page_answers_a_request_for_less_motion() {
    let rule = rules().into_iter().find(|(at_rule, _, _)| at_rule.contains("prefers-reduced-motion: reduce"));
    let (_, selector, declarations) = rule.expect("a rule for prefers-reduced-motion");
    assert!(selector.starts_with('*'), "{selector}");
    assert!(declarations.contains("animation-duration") && declarations.contains("transition-duration"), "{declarations}");
}


/// The strip's words are drawn on a canvas, where no rule above reaches
/// them. While its feed was away they were drawn at 40% opacity, which put
/// "activity feed offline" at about 2:1 in every theme, and a tool call's
/// chip was named at eleven pixels. A word is drawn at twelve pixels or
/// more, and the fill it is drawn with never carries the strip's dimming:
/// away, the readouts take the theme's dim text colour whole.
#[test]
fn the_strips_words_keep_their_size_and_their_contrast() {
    let lines: Vec<&str> = STRIP.lines().collect();
    let mut fonts = 0;
    for line in &lines {
        if let Some(at) = line.find("set_font(\"") {
            let size = &line[at + 10..];
            let size: f64 = size[..size.find("px").expect("a size in pixels")].parse().expect("a number of pixels");
            assert!(size >= 12.0, "the strip sets text at {size}px: {}", line.trim());
            fonts += 1;
        }
    }
    assert!(fonts >= 2, "the strip's fonts were read: {fonts}");

    let mut words = 0;
    for (at, line) in lines.iter().enumerate() {
        if !line.contains(".fill_text(") {
            continue;
        }
        let fill = lines[..at].iter().rev().find(|l| l.contains("set_fill_style_str(")).expect("a fill before the text");
        let dimmed = fill.split(|c: char| !c.is_alphanumeric() && c != '_').any(|word| word == "dim");
        assert!(!dimmed, "a word of the strip is drawn dimmed by opacity: {}", fill.trim());
        words += 1;
    }
    assert!(words >= 4, "the strip's words were read: {words}");
    assert!(
        STRIP.contains("let text = rgba(readout_ink(&pal, v.dimmed()), 1.0);"),
        "the readouts are drawn whole, in the colour `readout_ink` gives"
    );
}

/// A card of the picker carries its own theme's tokens, and the ring of
/// the keyboard is drawn around it, on the page. With the card's own colour
/// the ring of a dark theme's card was white on a light page's white. The
/// grid keeps the page's colour and the card's ring takes it: the page's
/// ring on the page's grounds is what `every_pair_that_is_read` holds.
#[test]
fn a_picker_cards_ring_is_the_pages() {
    let rules = rules();
    let of = |selector: &str| {
        rules.iter().find(|(_, s, _)| s == selector).unwrap_or_else(|| panic!("a rule for `{selector}`")).2.clone()
    };
    let grid: String = of(".theme-grid").split_whitespace().collect();
    assert!(grid.contains("--page-focus:var(--focus)"), "{grid}");
    let ring: String = of(".theme-card:focus-visible").split_whitespace().collect();
    assert!(ring.contains("outline-color:var(--page-focus)"), "{ring}");
    for (name, tokens) in blocks() {
        assert!(!tokens.contains_key("page-focus"), "`{name}` sets the page's ring for the cards inside it");
    }
}

/// The terminal holds faint text to half the theme's figure and draws it
/// at half opacity. The console program marks nothing faint; the web
/// console's own lines ("connection lost", an upload's) were, and stood at
/// about 3:1 in the light themes. They are written in the terminal's dim
/// colour, which the terminal holds to the whole figure.
#[test]
fn the_web_console_writes_none_of_its_own_lines_faint() {
    for (name, source) in [("embra-term.js", TERMINAL_PAGE), ("pty_bridge.rs", PTY_BRIDGE)] {
        assert!(!source.contains("\\x1b[2m"), "{name} writes a faint line");
        assert!(source.contains("\\x1b[90m[embra-web]"), "{name} writes its lines in the dim colour");
    }
}
