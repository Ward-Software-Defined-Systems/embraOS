//! Host-side unit coverage for the vendored guest `html_text` helper.
//! It is normally `include_str!`'d into a wasm guest; here we `include!`
//! the *same source* (it is plain Rust — the prelude, not this file,
//! owns `#![no_std]`) and exercise `to_text` directly. `extern crate
//! alloc;` makes the helper's `use alloc::…;` resolve on the std host.
//! Integration coverage of the shipped form is in `fixture_roundtrip` /
//! the wasm-compiled doc example.

extern crate alloc;

mod html_text {
    include!("../src/guest/html_text.rs");
}
use html_text::to_text;

#[test]
fn strips_tags_and_collapses_whitespace() {
    let h = "<p>Hello   <b>world</b></p>\n<div>again</div>";
    assert_eq!(to_text(h), "Hello world again");
}

#[test]
fn drops_script_and_style_bodies() {
    assert_eq!(to_text("a<script>var x=1;</script>b"), "a b");
    assert_eq!(to_text("a<style>.c{color:red}</style>b"), "a b");
    // case-insensitive tag match
    assert_eq!(to_text("a<SCRIPT>nope</SCRIPT>b"), "a b");
}

#[test]
fn decodes_minimal_entity_set() {
    assert_eq!(to_text("x &amp; y &lt;tag&gt; &#65;&#x42; &nbsp;z"), "x & y <tag> AB z");
    assert_eq!(to_text("&quot;hi&quot; it&apos;s"), "\"hi\" it's");
}

#[test]
fn plain_text_unchanged_and_trimmed() {
    assert_eq!(to_text("  plain text  "), "plain text");
    assert_eq!(to_text("no markup here"), "no markup here");
}

#[test]
fn malformed_input_never_panics() {
    // Unterminated tag, stray '&', empty — must not panic.
    let _ = to_text("<p>ok");
    let _ = to_text("a & b &# &#zz; <");
    assert_eq!(to_text(""), "");
    assert_eq!(to_text("<p>ok"), "ok");
}

#[test]
fn a_quoted_greater_than_inside_an_attribute_does_not_end_the_tag() {
    // Embra#17: a `>` inside a quoted attribute leaked the rest of the tag
    // as page text. Both samples are from fetched pages.
    let discourse =
        r#"<link media="(width >= 40rem)" rel="stylesheet" data-target="chat_desktop" />after"#;
    assert_eq!(to_text(discourse), "after");
    let alpine = r#"<div x-data :class="width >= 1280 ? 'wide' : 'narrow'" style="max-width: 125rem; margin: 0 auto" >body</div>"#;
    assert_eq!(to_text(alpine), "body");
    // Single quotes too, and the other kind of quote inside a quoted value.
    assert_eq!(to_text(r#"<a title='a > b' data-x="it's">link</a>"#), "link");
}

#[test]
fn an_unclosed_quote_falls_back_to_the_first_closing_bracket() {
    // A stray quote must never swallow the document.
    assert_eq!(to_text(r#"<a title="x>rest</a> more"#), "rest more");
}

#[test]
fn a_comment_is_dropped_whole() {
    assert_eq!(to_text("a<!-- b > c -->d"), "a d");
    assert_eq!(to_text("a<!-- unterminated"), "a");
    // A script element with a quoted `>` in its attributes is still
    // dropped whole, body included.
    assert_eq!(
        to_text(r#"x<script type="text/x" data-x="a>b">var y = 1;</script>z"#),
        "x z"
    );
}

#[test]
fn unknown_entity_kept_literal() {
    // `&` that is not a recognized entity stays as text.
    assert_eq!(to_text("AT&T and R&D"), "AT&T and R&D");
}
