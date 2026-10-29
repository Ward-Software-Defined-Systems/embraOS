//! Characterization tests for the host's read of a guest's output (#55
//! items 5 and 6): the packed `(ptr, len)` a guest returns is attacker
//! controlled, and the host maps every hostile value to its own error
//! before the brain sees a byte. `MAX_OUTPUT` is a strict `>` (a result of
//! exactly the cap passes), an out-of-bounds read is `MemoryAccess`, bytes
//! that are not UTF-8 are `NonUtf8`, and a module without an ABI export is
//! `AbiMissing` before anything runs.
//!
//! Guests are hand-written `wat`, as in `sandbox_limits.rs`. The packed
//! return is `(ptr as u64) << 32 | len` (`abi::pack`).

use std::time::Duration;

use embra_guardian::caps::Capabilities;
use embra_guardian::host::{WasmHost, DEFAULT_MEMORY_CAP, MAX_OUTPUT};
use embra_guardian::GuardianError;

const DEADLINE: Duration = Duration::from_secs(5);

fn compile(host: &WasmHost, wat: &str) -> wasmtime::Module {
    let wasm = wat::parse_str(wat).expect("fixture wat parses");
    host.precompile(&wasm).expect("fixture compiles")
}

fn call(wat: &str) -> Result<String, GuardianError> {
    let host = WasmHost::new().unwrap();
    let m = compile(&host, wat);
    host.call(&m, "{}", Capabilities::none(), DEADLINE, DEFAULT_MEMORY_CAP)
}

/// `guardian_run` claims an output one byte over the cap: ptr 0, len
/// 2 MiB + 1. One page suffices, the check runs before any read.
const OVER_THE_CAP: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "galloc") (param i32) (result i32) (i32.const 1024))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64) (i64.const 0x0000000000200001)))
"#;

/// Exactly the cap: 32 pages hold the 2 MiB the guest claims at ptr 0,
/// `galloc` hands out 0 so the input lands at the start, and the rest is
/// zero bytes, which are valid UTF-8.
const AT_THE_CAP: &str = r#"
(module
  (memory (export "memory") 32)
  (func (export "galloc") (param i32) (result i32) (i32.const 0))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64) (i64.const 0x0000000000200000)))
"#;

/// ptr 0xFFFF0000, len 64, on a single page: nothing there to read.
const OUT_OF_BOUNDS: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "galloc") (param i32) (result i32) (i32.const 1024))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64) (i64.const 0xFFFF000000000040)))
"#;

/// One byte, 0xff, at offset 2048; the output names it: ptr 2048, len 1.
const NOT_UTF8: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "galloc") (param i32) (result i32) (i32.const 1024))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64)
    (i32.store8 (i32.const 2048) (i32.const 255))
    (i64.const 0x0000080000000001)))
"#;

/// Valid wasm, no `galloc` export: it passes no validator here and fails
/// at the ABI, before `guardian_run` could run.
const WITHOUT_GALLOC: &str = r#"
(module
  (memory (export "memory") 1)
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param i32 i32) (result i64) (i64.const 0)))
"#;

#[test]
fn an_output_one_byte_over_the_cap_is_refused_with_the_figures() {
    let err = call(OVER_THE_CAP).expect_err("over the cap");
    assert!(
        matches!(err, GuardianError::OutputTooLarge { got, cap } if got == MAX_OUTPUT + 1 && cap == MAX_OUTPUT),
        "expected OutputTooLarge, got {err:?}"
    );
}

#[test]
fn an_output_of_exactly_the_cap_is_read() {
    let out = call(AT_THE_CAP).expect("the cap itself passes: the check is a strict greater-than");
    assert_eq!(out.len(), MAX_OUTPUT);
    assert!(out.starts_with("{}"), "the input the host wrote at ptr 0 leads the output");
}

#[test]
fn an_output_outside_the_guest_memory_is_a_memory_access_error() {
    let err = call(OUT_OF_BOUNDS).expect_err("nothing to read there");
    assert!(matches!(err, GuardianError::MemoryAccess(_)), "expected MemoryAccess, got {err:?}");
}

#[test]
fn an_output_that_is_not_utf8_is_refused_as_such() {
    let err = call(NOT_UTF8).expect_err("0xff is no UTF-8");
    assert!(matches!(err, GuardianError::NonUtf8), "expected NonUtf8, got {err:?}");
}

#[test]
fn a_module_without_galloc_fails_at_the_abi_before_it_runs() {
    let err = call(WITHOUT_GALLOC).expect_err("no allocator export");
    assert!(matches!(err, GuardianError::AbiMissing("galloc")), "expected AbiMissing(galloc), got {err:?}");
}
