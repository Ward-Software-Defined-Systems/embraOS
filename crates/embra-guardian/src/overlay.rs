//! Process-global Guardian runtime — the parallel dispatch overlay the
//! `guardian_call` meta-tool consults. Mirrors the `tools::registry`
//! `Lazy` precedent: a `OnceLock`, set once during `embra-brain` boot
//! reconcile, so `DispatchContext` and the 90 static tools are untouched
//! (zero blast radius, prompt-cache invariant preserved).
//!
//! Dynamic tools are NEVER added to the provider tool schema — they are
//! reachable only *through* the static `guardian_call` gateway.

use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use serde_json::Value;
use wasmtime::Module;

use crate::error::GuardianError;
use crate::host::WasmHost;

/// A compiled, ready-to-invoke dynamic tool.
pub struct CompiledTool {
    pub name: String,
    pub description: String,
    pub schema: Value,
    pub caps: Vec<String>,
    pub module: Module,
}

/// Engine + the live overlay of compiled tools + the pinned toolchain
/// version (reconcile rebuilds tools whose persisted version differs).
pub struct GuardianRuntime {
    host: WasmHost,
    tools: RwLock<HashMap<String, Arc<CompiledTool>>>,
    toolchain_version: String,
}

static RUNTIME: OnceLock<GuardianRuntime> = OnceLock::new();

impl GuardianRuntime {
    pub fn host(&self) -> &WasmHost {
        &self.host
    }
    pub fn toolchain_version(&self) -> &str {
        &self.toolchain_version
    }

    pub fn get(&self, name: &str) -> Option<Arc<CompiledTool>> {
        self.tools.read().ok()?.get(name).cloned()
    }

    /// Snapshot for `guardian_list` (name, description, caps, schema).
    pub fn list(&self) -> Vec<(String, String, Vec<String>, Value)> {
        self.tools
            .read()
            .map(|m| {
                let mut v: Vec<_> = m
                    .values()
                    .map(|t| {
                        (t.name.clone(), t.description.clone(), t.caps.clone(), t.schema.clone())
                    })
                    .collect();
                v.sort_by(|a, b| a.0.cmp(&b.0));
                v
            })
            .unwrap_or_default()
    }

    /// Compile `wasm` and (re)insert it into the overlay.
    pub fn compile_insert(
        &self,
        name: &str,
        description: &str,
        schema: Value,
        caps: Vec<String>,
        wasm: &[u8],
    ) -> Result<(), GuardianError> {
        let module = self.host.precompile(wasm)?;
        let tool = Arc::new(CompiledTool {
            name: name.to_string(),
            description: description.to_string(),
            schema,
            caps,
            module,
        });
        self.tools
            .write()
            .map_err(|_| GuardianError::Persistence("overlay lock poisoned".into()))?
            .insert(name.to_string(), tool);
        Ok(())
    }

    /// Drop a tool from the overlay (an in-flight call holds its own
    /// `Arc<CompiledTool>` and completes safely).
    pub fn remove(&self, name: &str) {
        if let Ok(mut m) = self.tools.write() {
            m.remove(name);
        }
    }

    pub fn len(&self) -> usize {
        self.tools.read().map(|m| m.len()).unwrap_or(0)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Initialize the global runtime (idempotent — safe to call once per
/// boot; a concurrent double-init keeps the first).
pub fn init(toolchain_version: impl Into<String>) -> Result<&'static GuardianRuntime, GuardianError> {
    if let Some(r) = RUNTIME.get() {
        return Ok(r);
    }
    let rt = GuardianRuntime {
        host: WasmHost::new()?,
        tools: RwLock::new(HashMap::new()),
        toolchain_version: toolchain_version.into(),
    };
    let _ = RUNTIME.set(rt);
    Ok(RUNTIME.get().expect("runtime set"))
}

/// The global runtime once [`init`] has run (else `None`).
pub fn runtime() -> Option<&'static GuardianRuntime> {
    RUNTIME.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    use crate::caps::Capabilities;

    // Minimal ABI-shaped echo guest (matches the spike).
    const ECHO_WAT: &str = r#"
(module
  (memory (export "memory") 1)
  (global $h (mut i32) (i32.const 1024))
  (func (export "galloc") (param $l i32) (result i32)
    (local $p i32) (local.set $p (global.get $h))
    (global.set $h (i32.add (global.get $h) (local.get $l))) (local.get $p))
  (func (export "gfree") (param i32 i32))
  (func (export "guardian_run") (param $p i32) (param $l i32) (result i64)
    (i64.or (i64.shl (i64.extend_i32_u (local.get $p)) (i64.const 32))
            (i64.extend_i32_u (local.get $l)))))
"#;

    #[test]
    fn init_then_compile_get_list_remove() {
        let rt = init("1.94.1-test").expect("init");
        assert_eq!(rt.toolchain_version(), "1.94.1-test");
        let wasm = wat::parse_str(ECHO_WAT).unwrap();
        rt.compile_insert(
            "echo_tool",
            "echoes input",
            serde_json::json!({"type":"object","properties":{}}),
            vec![],
            &wasm,
        )
        .unwrap();

        let t = rt.get("echo_tool").expect("present");
        let out = rt
            .host()
            .call(&t.module, "{\"a\":1}", Capabilities::none(), Duration::from_secs(5), 64 << 20)
            .unwrap();
        assert_eq!(out, "{\"a\":1}");

        let listed = rt.list();
        assert!(listed.iter().any(|(n, ..)| n == "echo_tool"));

        rt.remove("echo_tool");
        assert!(rt.get("echo_tool").is_none());
    }
}
