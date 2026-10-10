//! Host-side coverage for the vendored guest `json` helper. It is normally
//! `include_str!`'d into a wasm guest; here we `include!` the *same source*
//! (the prelude, not this file, owns `#![no_std]`). The shipped tools' wasm
//! round trips exercise it end to end; this pins the parse, the escapes
//! and the sizing that keeps a guest's arena small.

extern crate alloc;

mod json {
    include!("../src/guest/json.rs");
}
use json::{Json, arr, b, n, null, obj, parse, s, stringify};

#[test]
fn a_value_round_trips_through_its_text() {
    let text = r#"{"a":[1,2.5,-3,true,false,null],"s":"q\"b\\s\/n\nr\rt\tb\bf\fué 😀 \u0001","e":{},"n":[]}"#;
    let v = parse(text).unwrap();
    assert_eq!(v.get("s").as_str(), Some("q\"b\\s/n\nr\rt\tb\u{8}f\u{c}u\u{e9} \u{1f600} \u{1}"));
    assert_eq!(v.get("a").idx(1).as_f64(), Some(2.5));
    assert!(v.get("a").idx(5).is_null());
    let out = stringify(&v);
    assert_eq!(parse(&out).unwrap(), v);
    // The output's escapes: a control character as \u00XX, the slash and
    // the multi-byte characters plain, whole numbers without a `.0`.
    assert!(out.contains(r"\u0001") && out.contains("u\u{e9} \u{1f600}") && out.contains("s/n"), "{out}");
    assert!(out.starts_with(r#"{"a":[1,2.5,-3,true,false,null]"#), "{out}");
    assert_eq!(parse(r#""open"#).unwrap_err(), "unterminated string");
    assert!(parse("[1,").is_err());
}

#[test]
fn the_output_is_sized_once() {
    // Every escape kind, multi-byte text and nesting: the pre-pass is exact,
    // so the string never grows while it is written.
    let v = parse(
        r#"{"k\"ey":["a\\b\n\r\t\u0008\u000c\u0002 é 漢 😀",{"x":null,"y":[true,false]}],"e":"","z":{},"l":[]}"#,
    )
    .unwrap();
    let out = stringify(&v);
    assert_eq!(out.capacity(), out.len(), "{out}");
    // A number is estimated, and the estimate holds for the usual ones.
    let out = stringify(&parse("[1,42,2.5,-0.125,1e21,123456789]").unwrap());
    assert!(out.capacity() >= out.len(), "{out}");
}

#[test]
fn a_parsed_string_is_sized_from_its_raw_span() {
    // The decoded string never exceeds the span to the closing quote, so
    // the capacity is the span and nothing reallocates.
    for (raw, want) in [
        (r#""plain""#, "plain"),
        (r#""a\"b\\c\n""#, "a\"b\\c\n"),
        (r#""é😀""#, "\u{e9}\u{1f600}"),
        ("\"\"", ""),
    ] {
        match parse(raw).unwrap() {
            Json::Str(s) => {
                assert_eq!(s, want, "{raw}");
                assert_eq!(s.capacity(), raw.len() - 2, "{raw}");
            }
            other => panic!("{raw}: {other:?}"),
        }
    }
    // A quote behind a backslash does not end the span.
    let v = parse(r#"{"s":"a\"b","t":"c"}"#).unwrap();
    assert_eq!((v.get("s").as_str(), v.get("t").as_str()), (Some("a\"b"), Some("c")));
}

#[test]
fn a_value_built_with_the_constructors_reads_back() {
    // What a tool's `run` builds: an object in the order given, read back
    // with the accessors; a missing key or index is null, never a panic.
    let v = obj(vec![
        ("ok", b(true)),
        ("count", n(2.0)),
        ("items", arr(vec![s("a"), null(), n(-1.5)])),
        ("none", null()),
    ]);
    assert_eq!(stringify(&v), r#"{"ok":true,"count":2,"items":["a",null,-1.5],"none":null}"#);
    assert_eq!(v.get("ok").as_bool(), Some(true));
    assert_eq!(v.get("count").as_f64(), Some(2.0));
    assert_eq!(v.get("items").as_array().map(<[Json]>::len), Some(3));
    assert_eq!(v.get("items").idx(0).as_str(), Some("a"));
    assert!(v.get("items").idx(1).is_null() && v.get("items").idx(9).is_null());
    assert!(v.get("none").is_null() && v.get("missing").is_null());
    assert_eq!(v.get("ok").as_str(), None);
    assert_eq!(v.get("count").as_bool(), None);
}
