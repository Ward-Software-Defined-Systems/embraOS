// Scaffold-owned guest prelude. Prepended verbatim to every generated
// tool's `src/lib.rs`; the intelligence never writes or sees this. It
// owns `#![no_std]`, the allocator, the panic handler, the `json` module
// declaration, and the three ABI exports. The pasted module contributes
// only `GUARDIAN_*` metadata + `fn run(&str) -> String` (+ pure helpers),
// which the validator has already scanned. NOT compiled inside
// embra-guardian — `include_str!`'d as text.
#![no_std]
#![allow(dead_code)]
#[macro_use]
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

mod json;

// Bump allocator over a fixed per-instance arena. The host instantiates a
// fresh module per call, so linear memory (and this arena) is zeroed and
// reset every invocation — `dealloc` is intentionally a no-op.
const ARENA: usize = 8 * 1024 * 1024;
static mut ARENA_BUF: [u8; ARENA] = [0; ARENA];
static mut ARENA_OFF: usize = 0;

struct Bump;
unsafe impl core::alloc::GlobalAlloc for Bump {
    unsafe fn alloc(&self, l: core::alloc::Layout) -> *mut u8 {
        let align = l.align();
        let off = (ARENA_OFF + align - 1) & !(align - 1);
        if off + l.size() > ARENA {
            return core::ptr::null_mut();
        }
        ARENA_OFF = off + l.size();
        core::ptr::addr_of_mut!(ARENA_BUF).cast::<u8>().add(off)
    }
    unsafe fn dealloc(&self, _p: *mut u8, _l: core::alloc::Layout) {}
}

#[global_allocator]
static GUARDIAN_ALLOC: Bump = Bump;

#[panic_handler]
fn guardian_panic(_info: &core::panic::PanicInfo) -> ! {
    core::arch::wasm32::unreachable()
}

// ABI: host calls `galloc` for an input buffer, writes UTF-8 JSON, calls
// `guardian_run`, reads the packed `(ptr,len)` of the UTF-8 JSON output,
// then calls `gfree` (no-op for the bump arena).
#[no_mangle]
pub extern "C" fn galloc(len: u32) -> u32 {
    let mut v: Vec<u8> = Vec::with_capacity(len as usize);
    let p = v.as_mut_ptr() as u32;
    core::mem::forget(v);
    p
}

#[no_mangle]
pub extern "C" fn gfree(_ptr: u32, _len: u32) {}

#[no_mangle]
pub extern "C" fn guardian_run(ptr: u32, len: u32) -> u64 {
    let input: &str = unsafe {
        let slice = core::slice::from_raw_parts(ptr as *const u8, len as usize);
        match core::str::from_utf8(slice) {
            Ok(s) => s,
            Err(_) => "",
        }
    };
    let out = run(input).into_bytes();
    let out_len = out.len() as u32;
    let mut held = core::mem::ManuallyDrop::new(out);
    let out_ptr = held.as_mut_ptr() as u32;
    ((out_ptr as u64) << 32) | (out_len as u64)
}
