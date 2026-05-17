//! R1 + capability-broker gate spike. Proves, offline:
//!   1. `embra-guardian` (wasmtime + cranelift + reqwest/rustls) compiles
//!      AND links for `x86_64-unknown-linux-musl` (static) — build this
//!      example for that target; a successful binary is the gate.
//!   2. A pure-compute guest round-trips JSON through `host::call`.
//!   3. A guest that imports the Guardian-mediated `guardian::http_get`
//!      capability round-trips through the `Caller` → guest-`galloc`
//!      callback path (mock transport, public IP literal → no DNS, guard
//!      passes deterministically with no network).

use std::sync::Arc;
use std::time::Duration;

use embra_guardian::caps::{Capabilities, EgressPolicy, HttpResponse, HttpTransport};
use embra_guardian::host::WasmHost;

const ECHO_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "galloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $p))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param $ptr i32) (param $len i32) (result i64)
    (i64.or
      (i64.shl (i64.extend_i32_u (local.get $ptr)) (i64.const 32))
      (i64.extend_i32_u (local.get $len)))))
"#;

const HTTP_WAT: &str = r#"
(module
  (import "guardian" "http_get" (func $hget (param i32 i32) (result i64)))
  (memory (export "memory") 1)
  (global $heap (mut i32) (i32.const 1024))
  (func (export "galloc") (param $len i32) (result i32)
    (local $p i32)
    (local.set $p (global.get $heap))
    (global.set $heap (i32.add (global.get $heap) (local.get $len)))
    (local.get $p))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param $ptr i32) (param $len i32) (result i64)
    (call $hget (local.get $ptr) (local.get $len))))
"#;

struct DemoTransport;
impl HttpTransport for DemoTransport {
    fn get(&self, _u: &str, _t: Duration, _m: usize) -> Result<HttpResponse, String> {
        Ok(HttpResponse {
            status: 200,
            content_type: "application/json".into(),
            body: b"{\"mock\":true}".to_vec(),
        })
    }
}

fn main() -> anyhow::Result<()> {
    let host = WasmHost::new()?;

    // (2) pure compute, no capabilities granted.
    let echo = host.precompile(&wat::parse_str(ECHO_WAT)?)?;
    let input = r#"{"msg":"hello guardian","n":42}"#;
    let out = host.call(
        &echo,
        input,
        Capabilities::none(),
        Duration::from_secs(5),
        64 * 1024 * 1024,
    )?;
    assert_eq!(out, input, "echo round-trip mismatch");

    // (3) capability-broker: guest invokes guardian::http_get; the host
    // reads the URL, runs the egress guard (public IP literal passes
    // offline), calls the mock transport, writes the JSON back via the
    // guest's galloc.
    let httpmod = host.precompile(&wat::parse_str(HTTP_WAT)?)?;
    let caps = Capabilities::with_http(Arc::new(DemoTransport), EgressPolicy::default());
    let got = host.call(
        &httpmod,
        "https://1.1.1.1/",
        caps,
        Duration::from_secs(5),
        64 * 1024 * 1024,
    )?;
    let v: serde_json::Value = serde_json::from_str(&got)?;
    assert_eq!(v["ok"], true, "capability call should succeed: {got}");
    assert_eq!(v["body"], "{\"mock\":true}", "mock body should round-trip");

    println!(
        "spike OK: echo {} B + capability http_get round-trip via wasmtime/cranelift",
        out.len()
    );
    Ok(())
}
