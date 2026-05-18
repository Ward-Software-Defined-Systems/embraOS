//! Embedded `wasmtime` sandbox host. Instantiates an untrusted guest
//! module fresh per call (strong isolation; a panic/UB/trap in the guest
//! cannot corrupt the host or another call) and marshals JSON in/out per
//! the [`crate::abi`] contract.
//!
//! Sandbox guarantees layered here:
//! * **Zero ambient authority** — the guest is `wasm32-unknown-unknown`
//!   `#![no_std]`; the only host functions it can reach are the
//!   Guardian-mediated capabilities registered on the [`wasmtime::Linker`]
//!   (v1: `guardian::http_get`), and only with a per-call grant.
//! * **Memory cap** — `StoreLimits` bounds linear-memory growth.
//! * **Wall-clock cap** — epoch interruption traps a runaway guest.
//! * **Output cap** — oversize output is rejected before allocation.

use std::sync::mpsc;
use std::time::Duration;

use wasmtime::{
    Caller, Config, Engine, Extern, Linker, Module, Store, StoreLimits, StoreLimitsBuilder,
};

use crate::abi;
use crate::caps::{guarded_http_get, guarded_web_search, Capabilities};
use crate::error::GuardianError;

/// Hard ceiling on guest output, mirroring the registry's
/// `MAX_TOOL_RESULT_SIZE` (the brain re-caps too; this stops a hostile
/// guest forcing a multi-GiB host allocation first).
pub const MAX_OUTPUT: usize = 2 * 1024 * 1024;

/// Default per-call wall-clock budget.
pub const DEFAULT_DEADLINE: Duration = Duration::from_secs(5);
/// Default per-call linear-memory ceiling.
pub const DEFAULT_MEMORY_CAP: usize = 64 * 1024 * 1024;

/// Per-call store data: resource limits + the capabilities granted to
/// *this* tool invocation.
pub struct StoreData {
    limits: StoreLimits,
    caps: Capabilities,
}

/// One process-global engine + linker (compilation settings and the
/// capability surface are fixed). Modules are compiled once per tool and
/// reused; only `Store`/`Instance` are per-call.
pub struct WasmHost {
    engine: Engine,
    linker: Linker<StoreData>,
}

impl WasmHost {
    pub fn new() -> Result<Self, GuardianError> {
        let mut cfg = Config::new();
        // Wall-clock interruption: a background ticker bumps the engine
        // epoch after the deadline; the guest traps at the next loop
        // back-edge / call. Deterministic enough for a 5 s budget and far
        // cheaper than fuel metering.
        cfg.epoch_interruption(true);
        let engine =
            Engine::new(&cfg).map_err(|e| GuardianError::Instantiate(e.to_string()))?;

        let mut linker: Linker<StoreData> = Linker::new(&engine);
        // The ONLY host surface. A module importing anything else fails
        // instantiation (defense-in-depth alongside the validator). The
        // capability self-gates on the per-call grant in `StoreData`.
        linker
            .func_wrap(
                abi::IMPORT_MODULE,
                abi::CAP_HTTP_GET,
                |mut caller: Caller<'_, StoreData>, url_ptr: u32, url_len: u32| -> u64 {
                    host_http_get(&mut caller, url_ptr, url_len)
                },
            )
            .map_err(|e| GuardianError::Instantiate(e.to_string()))?;
        linker
            .func_wrap(
                abi::IMPORT_MODULE,
                abi::CAP_WEB_SEARCH,
                |mut caller: Caller<'_, StoreData>, q_ptr: u32, q_len: u32| -> u64 {
                    host_web_search(&mut caller, q_ptr, q_len)
                },
            )
            .map_err(|e| GuardianError::Instantiate(e.to_string()))?;

        Ok(Self { engine, linker })
    }

    pub fn engine(&self) -> &Engine {
        &self.engine
    }

    /// Compile guest wasm bytes once (cranelift). Reused across calls.
    pub fn precompile(&self, wasm: &[u8]) -> Result<Module, GuardianError> {
        Module::new(&self.engine, wasm).map_err(|e| GuardianError::Compile(e.to_string()))
    }

    /// Run `input` (UTF-8 JSON) through the guest with the given
    /// capability grants, returning its UTF-8 JSON output. Fresh
    /// `Store`/`Instance` per call.
    pub fn call(
        &self,
        module: &Module,
        input: &str,
        caps: Capabilities,
        deadline: Duration,
        memory_cap: usize,
    ) -> Result<String, GuardianError> {
        let data = StoreData {
            limits: StoreLimitsBuilder::new()
                .memory_size(memory_cap)
                .instances(1)
                .build(),
            caps,
        };
        let mut store = Store::new(&self.engine, data);
        store.limiter(|d| &mut d.limits);
        // One epoch tick == over budget. The ticker thread waits up to
        // `deadline`; a finished call drops `tx`, unblocking it early so
        // it never interrupts a well-behaved guest.
        store.set_epoch_deadline(1);
        let (tx, rx) = mpsc::channel::<()>();
        let ticker_engine = self.engine.clone();
        let ticker = std::thread::spawn(move || {
            if rx.recv_timeout(deadline).is_err() {
                ticker_engine.increment_epoch();
            }
        });

        let result = self.call_inner(&mut store, module, input);

        drop(tx);
        let _ = ticker.join();
        result
    }

    fn call_inner(
        &self,
        store: &mut Store<StoreData>,
        module: &Module,
        input: &str,
    ) -> Result<String, GuardianError> {
        let instance = self
            .linker
            .instantiate(&mut *store, module)
            .map_err(|e| map_trap(e, GuardianError::Instantiate(String::new())))?;

        let memory = instance
            .get_memory(&mut *store, abi::EXPORT_MEMORY)
            .ok_or(GuardianError::AbiMissing(abi::EXPORT_MEMORY))?;
        let galloc = instance
            .get_typed_func::<u32, u32>(&mut *store, abi::EXPORT_ALLOC)
            .map_err(|_| GuardianError::AbiMissing(abi::EXPORT_ALLOC))?;
        let guardian_run = instance
            .get_typed_func::<(u32, u32), u64>(&mut *store, abi::EXPORT_RUN)
            .map_err(|_| GuardianError::AbiMissing(abi::EXPORT_RUN))?;
        let gfree = instance
            .get_typed_func::<(u32, u32), ()>(&mut *store, abi::EXPORT_FREE)
            .map_err(|_| GuardianError::AbiMissing(abi::EXPORT_FREE))?;

        let in_bytes = input.as_bytes();
        let in_len: u32 = in_bytes.len().try_into().map_err(|_| {
            GuardianError::OutputTooLarge { got: in_bytes.len(), cap: u32::MAX as usize }
        })?;

        let in_ptr = galloc
            .call(&mut *store, in_len)
            .map_err(|e| map_trap(e, GuardianError::Trap("galloc".into())))?;
        memory
            .write(&mut *store, in_ptr as usize, in_bytes)
            .map_err(|e| GuardianError::MemoryAccess(e.to_string()))?;

        let packed = guardian_run
            .call(&mut *store, (in_ptr, in_len))
            .map_err(|e| map_trap(e, GuardianError::Trap("guardian_run".into())))?;
        let (out_ptr, out_len) = abi::unpack(packed);
        let out_len = out_len as usize;

        if out_len > MAX_OUTPUT {
            return Err(GuardianError::OutputTooLarge { got: out_len, cap: MAX_OUTPUT });
        }

        let mut buf = vec![0u8; out_len];
        memory
            .read(&*store, out_ptr as usize, &mut buf)
            .map_err(|e| GuardianError::MemoryAccess(e.to_string()))?;

        // Best-effort frees; the output is already copied out.
        let _ = gfree.call(&mut *store, (out_ptr, out_len as u32));
        let _ = gfree.call(&mut *store, (in_ptr, in_len));

        String::from_utf8(buf).map_err(|_| GuardianError::NonUtf8)
    }
}

/// `guardian::http_get` host import. Reads the URL from guest memory, runs
/// the policy-guarded fetch, then hands the JSON result back by calling
/// the guest's own `galloc` and writing into its linear memory. Returns
/// [`abi::pack`]ed `(ptr, len)`; `0` only on a catastrophic ABI failure
/// (no `memory`/`galloc`), which the guest treats as an empty result.
fn host_http_get(caller: &mut Caller<'_, StoreData>, url_ptr: u32, url_len: u32) -> u64 {
    let Some(Extern::Memory(memory)) = caller.get_export(abi::EXPORT_MEMORY) else {
        return 0;
    };
    let Some(Extern::Func(galloc_fn)) = caller.get_export(abi::EXPORT_ALLOC) else {
        return 0;
    };
    let Ok(galloc) = galloc_fn.typed::<u32, u32>(&*caller) else {
        return 0;
    };

    let url = {
        let data = memory.data(&*caller);
        let (start, end) = (url_ptr as usize, url_ptr as usize + url_len as usize);
        match data.get(start..end) {
            Some(b) => String::from_utf8_lossy(b).into_owned(),
            None => return 0,
        }
    };

    let caps = caller.data().caps.clone();
    let result = guarded_http_get(&caps, &url);
    let bytes = result.into_bytes();
    let Ok(out_len) = u32::try_from(bytes.len()) else {
        return 0;
    };

    let Ok(out_ptr) = galloc.call(&mut *caller, out_len) else {
        return 0;
    };
    if memory.write(&mut *caller, out_ptr as usize, &bytes).is_err() {
        return 0;
    }
    abi::pack(out_ptr, out_len)
}

/// `guardian::web_search` host import. Same marshalling as
/// [`host_http_get`]; the policy/credential live in `guarded_web_search`
/// + the per-call `Capabilities.search` provider (host-side only).
fn host_web_search(caller: &mut Caller<'_, StoreData>, q_ptr: u32, q_len: u32) -> u64 {
    let Some(Extern::Memory(memory)) = caller.get_export(abi::EXPORT_MEMORY) else {
        return 0;
    };
    let Some(Extern::Func(galloc_fn)) = caller.get_export(abi::EXPORT_ALLOC) else {
        return 0;
    };
    let Ok(galloc) = galloc_fn.typed::<u32, u32>(&*caller) else {
        return 0;
    };

    let query = {
        let data = memory.data(&*caller);
        let (start, end) = (q_ptr as usize, q_ptr as usize + q_len as usize);
        match data.get(start..end) {
            Some(b) => String::from_utf8_lossy(b).into_owned(),
            None => return 0,
        }
    };

    let caps = caller.data().caps.clone();
    let result = guarded_web_search(&caps, &query);
    let bytes = result.into_bytes();
    let Ok(out_len) = u32::try_from(bytes.len()) else {
        return 0;
    };

    let Ok(out_ptr) = galloc.call(&mut *caller, out_len) else {
        return 0;
    };
    if memory.write(&mut *caller, out_ptr as usize, &bytes).is_err() {
        return 0;
    }
    abi::pack(out_ptr, out_len)
}

/// Map a wasmtime error to the right `GuardianError`, distinguishing an
/// epoch interrupt (timeout) and a memory-limit trip (OOM) from a generic
/// guest trap.
fn map_trap(err: wasmtime::Error, generic: GuardianError) -> GuardianError {
    if let Some(trap) = err.downcast_ref::<wasmtime::Trap>() {
        return match trap {
            wasmtime::Trap::Interrupt => GuardianError::Timeout(DEFAULT_DEADLINE),
            other => GuardianError::Trap(other.to_string()),
        };
    }
    let msg = err.to_string();
    if msg.contains("exceeds memory limits") || msg.contains("memory minimum size") {
        return GuardianError::Oom;
    }
    match generic {
        GuardianError::Trap(_) => GuardianError::Trap(msg),
        other => other,
    }
}
