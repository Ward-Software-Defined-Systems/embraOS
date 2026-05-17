//! End-to-end test over a committed wasm fixture built from the *real*
//! validator + scaffold + prelude + vendored `json` + capability shim
//! (regenerate with `cargo run -p embra-guardian --example gen_fixture`).
//! Keeps `cargo test` self-contained — no in-OS toolchain needed.
//!
//! The `probe` tool: `{a,b,url?}` -> `{sum, fetched}`; `fetched` is the
//! `host::http_get` result when `url` is present, else `null`.

use std::sync::Arc;
use std::time::Duration;

use embra_guardian::caps::{Capabilities, EgressPolicy, HttpResponse, HttpTransport};
use embra_guardian::host::WasmHost;

const PROBE_WASM: &[u8] = include_bytes!("fixtures/probe.wasm");

const D: Duration = Duration::from_secs(5);
const MEM: usize = 64 << 20;

struct StubHttp;
impl HttpTransport for StubHttp {
    fn get(&self, _u: &str, _t: Duration, _m: usize) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            content_type: "application/json".into(),
            body: b"{\"page\":\"ok\"}".to_vec(),
        })
    }
}

#[test]
fn pure_compute_path() {
    let host = WasmHost::new().unwrap();
    let m = host.precompile(PROBE_WASM).unwrap();
    let out = host
        .call(&m, r#"{"a":2,"b":40}"#, Capabilities::none(), D, MEM)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["sum"], 42, "prelude+json round-trip; out={out}");
    assert!(v["fetched"].is_null());
}

#[test]
fn capability_path_through_generated_host_shim() {
    let host = WasmHost::new().unwrap();
    let m = host.precompile(PROBE_WASM).unwrap();
    let caps = Capabilities::with_http(Arc::new(StubHttp), EgressPolicy::default());
    let out = host
        .call(
            &m,
            r#"{"a":1,"b":1,"url":"https://1.1.1.1/"}"#,
            caps,
            D,
            MEM,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["sum"], 2);
    let fetched = v["fetched"].as_str().expect("fetched is a JSON string");
    assert!(fetched.contains("\"ok\":true"), "guard wrapper: {fetched}");
    assert!(fetched.contains("page"), "stub body present: {fetched}");
}

#[test]
fn capability_denied_when_not_granted() {
    // url present but no http capability granted -> guard says not granted.
    let host = WasmHost::new().unwrap();
    let m = host.precompile(PROBE_WASM).unwrap();
    let out = host
        .call(
            &m,
            r#"{"a":0,"b":0,"url":"https://1.1.1.1/"}"#,
            Capabilities::none(),
            D,
            MEM,
        )
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    let fetched = v["fetched"].as_str().unwrap();
    assert!(fetched.contains("not granted"), "fetched={fetched}");
}

#[test]
fn malformed_input_returns_structured_error() {
    let host = WasmHost::new().unwrap();
    let m = host.precompile(PROBE_WASM).unwrap();
    let out = host
        .call(&m, "definitely not json", Capabilities::none(), D, MEM)
        .unwrap();
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert!(v["error"].is_string(), "json parse error surfaced: {out}");
}
