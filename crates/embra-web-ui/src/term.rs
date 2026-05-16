//! Thin bindings to the global `embraTerm*` functions defined by
//! `/assets/embra-term.js` (xterm.js + WebSocket glue).

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    #[wasm_bindgen(js_name = embraTermInit)]
    fn embra_term_init(el_id: &str);

    #[wasm_bindgen(js_name = embraTermInject)]
    pub fn inject(text: &str);

    #[wasm_bindgen(js_name = embraTermKey)]
    pub fn key(code: &str);

    #[wasm_bindgen(js_name = embraTermTakeover)]
    pub fn takeover();

    #[wasm_bindgen(js_name = embraTermSetRoleCb)]
    fn embra_term_set_role_cb(cb: &Closure<dyn FnMut(String, String)>);
}

/// Boot xterm + the WebSocket against `el_id`.
pub fn init(el_id: &str) {
    embra_term_init(el_id);
}

/// Register a `(role, owner)` listener; the closure is leaked to live for
/// the whole session (CSR app lifetime).
pub fn on_role(cb: impl FnMut(String, String) + 'static) {
    let closure = Closure::<dyn FnMut(String, String)>::new(cb);
    embra_term_set_role_cb(&closure);
    closure.forget();
}

/// Inject a slash command (adds the trailing newline the TUI expects).
pub fn run_command(cmd: &str) {
    inject(&format!("{cmd}\n"));
}
