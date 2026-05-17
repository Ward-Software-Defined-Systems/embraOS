//! Guardian dynamic-tool ABI — the contract between the host
//! (`embra-guardian::host`, running inside `embra-brain`) and an
//! untrusted `wasm32-unknown-unknown` guest module.
//!
//! The guest is `#![no_std]` + `alloc`, exports its default linear
//! `memory`, and three `extern "C"` functions plus three metadata
//! statics. The host never instantiates a guest to read metadata —
//! `GUARDIAN_NAME`/`DESC`/`SCHEMA` are extracted from the source AST at
//! validate time (`validator`), so only `galloc`/`guardian_run`/`gfree`
//! are exercised at run time.
//!
//! Call sequence (`host::call`):
//!   1. `in_ptr  = galloc(input_len)` ; host writes UTF-8 JSON input.
//!   2. `packed  = guardian_run(in_ptr, input_len)` .
//!   3. `(out_ptr, out_len) = unpack(packed)` ; host reads UTF-8 JSON out.
//!   4. `gfree(out_ptr, out_len)` ; `gfree(in_ptr, input_len)` .
//!
//! `packed` is `(out_ptr as u64) << 32 | (out_len as u64)` so a single
//! `u64` return carries both the pointer and the length — wasm32 has no
//! multi-value return in the stable `extern "C"` ABI we target.

/// Default exported linear memory name emitted by a wasm32 cdylib.
pub const EXPORT_MEMORY: &str = "memory";
/// Guest allocator: `fn galloc(len: u32) -> u32` (offset into `memory`).
pub const EXPORT_ALLOC: &str = "galloc";
/// Guest deallocator: `fn gfree(ptr: u32, len: u32)`.
pub const EXPORT_FREE: &str = "gfree";
/// Guest entry: `fn guardian_run(ptr: u32, len: u32) -> u64` (see [`pack`]).
pub const EXPORT_RUN: &str = "guardian_run";

/// Metadata statics, read from the source AST at validate time — never by
/// instantiating untrusted wasm.
pub const STATIC_NAME: &str = "GUARDIAN_NAME";
pub const STATIC_DESC: &str = "GUARDIAN_DESC";
pub const STATIC_SCHEMA: &str = "GUARDIAN_SCHEMA";
/// Declared-capabilities static (`GUARDIAN_CAPS = ["http_get"]`, `[]` for
/// pure-compute). Read from the AST by `validator`; never executed.
pub const STATIC_CAPS: &str = "GUARDIAN_CAPS";

// --- Capability-broker ABI (host imports the guest may declare) ---
/// wasm import module name for all Guardian-mediated capabilities.
pub const IMPORT_MODULE: &str = "guardian";
/// Capability: guarded HTTP GET. `fn(url_ptr: u32, url_len: u32) -> u64`
/// (returns [`pack`]ed ptr/len of a JSON result the host wrote into guest
/// memory via the guest's own `galloc`).
pub const CAP_HTTP_GET: &str = "http_get";
/// The complete v1 capability allowlist (`validator` rejects anything else).
pub const KNOWN_CAPS: &[&str] = &[CAP_HTTP_GET];

/// Pack a `(ptr, len)` pair into the single `u64` `guardian_run` returns.
#[inline]
pub const fn pack(ptr: u32, len: u32) -> u64 {
    ((ptr as u64) << 32) | (len as u64)
}

/// Inverse of [`pack`]. Returns `(ptr, len)`.
#[inline]
pub const fn unpack(packed: u64) -> (u32, u32) {
    ((packed >> 32) as u32, (packed & 0xffff_ffff) as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pack_unpack_roundtrips() {
        for (p, l) in [(0u32, 0u32), (1, 2), (0xDEAD_BEEF, 0x0BAD_F00D), (u32::MAX, u32::MAX)] {
            assert_eq!(unpack(pack(p, l)), (p, l));
        }
    }

    #[test]
    fn pack_layout_is_ptr_high_len_low() {
        assert_eq!(pack(0x1234_5678, 0x9ABC_DEF0), 0x1234_5678_9ABC_DEF0);
    }
}
