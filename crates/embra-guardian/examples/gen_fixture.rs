//! Generates the committed wasm test fixture by running the *real*
//! validator + scaffold over a sample tool, into the dir given as argv[1].
//! Used by `scripts`/manual to (re)build `tests/fixtures/probe.wasm`.
//! Prints the project path and artifact filename on two lines.

use std::path::Path;

// A non-trivial sample: parses input JSON (vendored `json`), sums two
// numbers, and optionally fetches a URL through the `http_get`
// capability — exercising prelude + json + scaffold host shim together.
const SAMPLE: &str = r##"
// guardian-tool: probe
const GUARDIAN_NAME: &str = "probe";
const GUARDIAN_DESC: &str = "Sum a+b; optionally fetch `url` via the http_get capability.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"},"url":{"type":"string"}}}"#;
const GUARDIAN_CAPS: &[&str] = &["http_get"];
fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let a = v.get("a").as_f64().unwrap_or(0.0);
    let b = v.get("b").as_f64().unwrap_or(0.0);
    let fetched = match v.get("url").as_str() {
        Some(u) if !u.is_empty() => json::s(&host::http_get(u)),
        _ => json::null(),
    };
    json::stringify(&json::obj(vec![
        ("sum", json::n(a + b)),
        ("fetched", fetched),
    ]))
}
"##;

fn main() -> anyhow::Result<()> {
    let out = std::env::args()
        .nth(1)
        .expect("usage: gen_fixture <outdir> [module.rs]");
    // Optional argv[2]: a module source file (else the built-in SAMPLE).
    // Lets the doc examples be compiled to wasm32 for verification.
    let src: String = match std::env::args().nth(2) {
        Some(path) => std::fs::read_to_string(&path)
            .map_err(|e| anyhow::anyhow!("read {path}: {e}"))?,
        None => SAMPLE.to_string(),
    };
    let m = embra_guardian::validate(&src, &[])
        .map_err(|e| anyhow::anyhow!("module failed validation: {e}"))?;
    let p = embra_guardian::scaffold(Path::new(&out), &m)?;
    println!("{}", p.project.display());
    println!("{}", p.artifact_name);
    Ok(())
}
