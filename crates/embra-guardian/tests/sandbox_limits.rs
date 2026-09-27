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

use std::time::{Duration, Instant};

use embra_guardian::caps::Capabilities;
use embra_guardian::host::{WasmHost, DEFAULT_MEMORY_CAP};
use embra_guardian::GuardianError;

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
