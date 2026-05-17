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

    /// Focus the terminal so keystrokes go to the console (used after
    /// launching a guided/interactive setup that continues there).
    #[wasm_bindgen(js_name = embraTermFocus)]
    pub fn focus();

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

/// Inject a slash command followed by Enter. NB: a raw-mode PTY expects
/// carriage return (`\r`, 0x0D) for Enter — `\n` (0x0A) is Ctrl+J, which
/// crossterm decodes as `Char('j')` and the TUI inserts literally.
pub fn run_command(cmd: &str) {
    inject(&format!("{cmd}\r"));
}

/// Send a multi-line body as ONE verbatim user message: a bracketed
/// paste (`\x1b[200~ … \x1b[201~`) followed by Enter. embra-console (in
/// the web-pty build) enables crossterm bracketed paste, so the wrapped
/// body coalesces into a single `Event::Paste` staged into `pasted_lines`;
/// the trailing `\r` is a separate Enter that fires the verbatim send
/// path (no `.trim()`, no slash parsing — a leading `/`, a lone `.` line,
/// and surrounding whitespace all survive). The end marker must not occur
/// inside the body or it would close the paste early; a raw ESC can't be
/// typed into a `<textarea>`, but we neutralize it defensively anyway.
pub fn send_multiline(body: &str) {
    let safe = body.replace("\x1b[201~", "\u{1b} [201~");
    inject(&format!("\x1b[200~{safe}\x1b[201~\r"));
}
