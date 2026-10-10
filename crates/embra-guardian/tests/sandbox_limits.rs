//! Characterization tests for the sandbox limits: the wall-clock deadline,
//! the linear-memory cap, and trap classification. `host::map_trap` tells
//! a memory-limit failure apart from a generic trap by matching wasmtime's
//! error TEXT, so a wasmtime upgrade that rewords the message would turn
//! `Oom` into `Instantiate`/`Trap` without failing to compile. These tests
//! are the tripwire.
//!
//! Guests are hand-written `wat` (no toolchain needed) exporting the four
//! ABI items the host resolves: `memory`, `galloc`, `gfree`,
//! `guardian_run`.

use std::sync::Arc;
use std::time::{Duration, Instant};

use embra_guardian::caps::{Capabilities, EgressPolicy, HttpResponse, HttpTransport};
use embra_guardian::host::{WasmHost, DEADLINE_WITH_HTTP, DEFAULT_DEADLINE, DEFAULT_MEMORY_CAP};
use embra_guardian::GuardianError;

/// The committed probe tool: `{a,b,url?}` → `{sum, fetched}`; it calls
/// `host::http_get` when `url` is given, so a slow transport makes a
/// guest that waits inside a host import.
const PROBE_WASM: &[u8] = include_bytes!("fixtures/probe.wasm");

/// A transport that takes a second to answer.
struct SlowHttp;
impl HttpTransport for SlowHttp {
    fn get(&self, _u: &str, _t: Duration, _m: usize) -> Result<HttpResponse, String> {
        std::thread::sleep(Duration::from_secs(1));
        Ok(HttpResponse {
            status: 200,
            content_type: "application/json".into(),
            body: b"{\"page\":\"ok\"}".to_vec(),
            location: None,
        })
    }
}

const SHORT_DEADLINE: Duration = Duration::from_millis(250);
const DEADLINE: Duration = Duration::from_secs(5);

fn compile(host: &WasmHost, wat: &str) -> wasmtime::Module {
    let wasm = wat::parse_str(wat).expect("fixture wat parses");
    host.precompile(&wasm).expect("fixture compiles")
}

/// `guardian_run` never returns.
const SPIN: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "galloc") (param i32) (result i32) (i32.const 1024))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64)
    (loop $spin (br $spin))
    (i64.const 0)))
"#;

/// Declares 2000 pages (128 MiB) of initial memory — over the 64 MiB cap.
const GREEDY_AT_INSTANTIATION: &str = r#"
(module
  (memory (export "memory") 2000)
  (func (export "galloc") (param i32) (result i32) (i32.const 1024))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64) (i64.const 0)))
"#;

/// Starts at one page and asks for 2000 more at run time. Returns an empty
/// output when the grow was denied (`memory.grow` yields -1) and traps when
/// it was granted.
const GREEDY_AT_RUNTIME: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "galloc") (param i32) (result i32) (i32.const 1024))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64)
    (if (i32.ne (memory.grow (i32.const 2000)) (i32.const -1))
      (then (unreachable)))
    (i64.const 0)))
"#;

/// `guardian_run` executes `unreachable`.
const TRAPS: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "galloc") (param i32) (result i32) (i32.const 1024))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64) (unreachable)))
"#;

#[test]
fn runaway_guest_is_interrupted_and_classified_as_timeout() {
    let host = WasmHost::new().unwrap();
    let m = compile(&host, SPIN);
    let started = Instant::now();
    let err = host
        .call(&m, "{}", Capabilities::none(), SHORT_DEADLINE, DEFAULT_MEMORY_CAP)
        .expect_err("a spinning guest must not return");
    assert!(
        matches!(err, GuardianError::Timeout(_)),
        "expected Timeout, got {err:?}"
    );
    // Interrupted near the deadline, not at some far larger bound.
    assert!(
        started.elapsed() < DEADLINE,
        "interrupt took {:?}",
        started.elapsed()
    );
}

#[test]
fn the_timeout_names_the_deadline_the_call_was_given() {
    let host = WasmHost::new().unwrap();
    let m = compile(&host, SPIN);
    let err = host
        .call(&m, "{}", Capabilities::none(), SHORT_DEADLINE, DEFAULT_MEMORY_CAP)
        .expect_err("a spinning guest must not return");
    assert!(matches!(err, GuardianError::Timeout(d) if d == SHORT_DEADLINE), "{err:?}");
}

#[test]
fn a_timeout_in_one_call_does_not_trap_another() {
    // One engine, two calls at once: a guest that spins past a 250 ms
    // deadline, and the probe tool waiting a second inside a host import
    // under a 5 s deadline. The second must finish. (One ticker per call
    // bumping the engine's epoch used to trap both.)
    let host = Arc::new(WasmHost::new().unwrap());
    let spin = compile(&host, SPIN);
    let probe = host.precompile(PROBE_WASM).unwrap();
    std::thread::scope(|scope| {
        let h = host.clone();
        let spinner = scope.spawn(move || {
            h.call(&spin, "{}", Capabilities::none(), SHORT_DEADLINE, DEFAULT_MEMORY_CAP)
        });
        let h = host.clone();
        let waiter = scope.spawn(move || {
            let caps = Capabilities::with_http(Arc::new(SlowHttp), EgressPolicy::default());
            h.call(&probe, r#"{"a":1,"b":1,"url":"https://1.1.1.1/"}"#, caps, DEADLINE, DEFAULT_MEMORY_CAP)
        });
        let spun = spinner.join().unwrap();
        assert!(matches!(spun, Err(GuardianError::Timeout(_))), "{spun:?}");
        let waited = waiter.join().unwrap().expect("the waiting call completes on its own deadline");
        let v: serde_json::Value = serde_json::from_str(&waited).unwrap();
        assert_eq!(v["sum"], 2, "{waited}");
        assert!(!v["fetched"].is_null(), "{waited}");
    });
}

#[test]
fn the_http_deadline_is_the_fetch_budget_plus_five_seconds() {
    assert_eq!(DEADLINE_WITH_HTTP, EgressPolicy::default().timeout + DEFAULT_DEADLINE);
}

#[test]
fn initial_memory_over_the_cap_is_classified_as_oom() {
    let host = WasmHost::new().unwrap();
    let m = compile(&host, GREEDY_AT_INSTANTIATION);
    let err = host
        .call(&m, "{}", Capabilities::none(), DEADLINE, DEFAULT_MEMORY_CAP)
        .expect_err("128 MiB of initial memory must not fit a 64 MiB cap");
    assert!(
        matches!(err, GuardianError::Oom),
        "expected Oom, got {err:?} — did wasmtime reword its memory-limit error?"
    );
}

#[test]
fn runtime_growth_over_the_cap_is_denied() {
    let host = WasmHost::new().unwrap();
    let m = compile(&host, GREEDY_AT_RUNTIME);
    let out = host
        .call(&m, "{}", Capabilities::none(), DEADLINE, DEFAULT_MEMORY_CAP)
        .expect("a denied grow is not a host error — the guest sees -1");
    assert_eq!(out, "", "the guest took its grow-was-denied branch");
}

#[test]
fn guest_trap_is_classified_as_trap() {
    let host = WasmHost::new().unwrap();
    let m = compile(&host, TRAPS);
    let err = host
        .call(&m, "{}", Capabilities::none(), DEADLINE, DEFAULT_MEMORY_CAP)
        .expect_err("unreachable must trap");
    assert!(
        matches!(err, GuardianError::Trap(_)),
        "expected Trap, got {err:?}"
    );
}
