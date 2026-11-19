//! The page's theme.
//!
//! The page's colours are tokens of its stylesheet (`assets/app.css`). Two
//! things draw outside the stylesheet and take their colours from the same
//! tokens: the terminal, in its script, and the activity strip, on its
//! canvas, which asks for them here.

use std::cell::Cell;

use wasm_bindgen::prelude::*;

#[wasm_bindgen]
extern "C" {
    /// A colour token's value on the page's root.
    #[wasm_bindgen(js_name = embraThemeToken)]
    pub fn token(name: &str) -> String;
}

thread_local! {
    static EPOCH: Cell<u32> = const { Cell::new(0) };
}

/// A count that moves each time the page's colours change.
pub fn epoch() -> u32 {
    EPOCH.with(Cell::get)
}
