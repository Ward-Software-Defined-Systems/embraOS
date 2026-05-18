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
fn unknown_entity_kept_literal() {
    // `&` that is not a recognized entity stays as text.
    assert_eq!(to_text("AT&T and R&D"), "AT&T and R&D");
}
