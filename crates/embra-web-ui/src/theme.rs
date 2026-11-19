//! The page's theme.
//!
//! One theme is in force for the whole machine (`embra_tools_core::theme`).
//! embra-web writes it into the page it serves, as `data-theme` on the root,
//! so the first paint is already in it; an open page follows a change at its
//! next status poll (`follow`). The colours are the stylesheet's: showing a
//! theme is setting the attribute. Two things draw outside the stylesheet
//! and are told: the terminal (`embraTermApplyTheme`) and the activity
//! strip, which compares `epoch` at each frame.
//!
//! The picker is the operator's way to choose: a press shows a theme on this
//! page alone, Apply stores it for the machine (`POST /api/theme`), and
//! leaving any other way puts back what was shown before. It types nothing
//! into the console, so it works from a browser that only observes, while a
//! turn runs and before the first session.

use std::cell::Cell;

use embra_tools_core::theme::{self as themes, THEMES, Theme};
use leptos::prelude::*;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::spawn_local;

#[wasm_bindgen]
extern "C" {
    /// The terminal takes the colours of the theme now on the page's root.
    #[wasm_bindgen(js_name = embraTermApplyTheme)]
    fn apply_to_terminal();

    /// A colour token's value under the theme now on the page's root.
    #[wasm_bindgen(js_name = embraThemeToken)]
    pub fn token(name: &str) -> String;
}

thread_local! {
    static EPOCH: Cell<u32> = const { Cell::new(0) };
    static PREVIEWING: Cell<bool> = const { Cell::new(false) };
}

/// A count that moves each time the page's theme changes.
pub fn epoch() -> u32 {
    EPOCH.with(Cell::get)
}

fn root() -> Option<web_sys::Element> {
    web_sys::window()?.document()?.document_element()
}

/// The theme the page shows.
pub fn shown() -> &'static Theme {
    root()
        .and_then(|root| root.get_attribute("data-theme"))
        .and_then(|name| themes::find(&name))
        .unwrap_or_else(themes::default_theme)
}

/// Show `theme` on this page.
pub fn show(theme: &Theme) {
    let Some(root) = root() else { return };
    if root.get_attribute("data-theme").as_deref() == Some(theme.name) {
        return;
    }
    let _ = root.set_attribute("data-theme", theme.name);
    apply_to_terminal();
    EPOCH.with(|epoch| epoch.set(epoch.get().wrapping_add(1)));
}

/// Follow the machine's theme as a status answer names it. Not while the
/// picker is open: what it shows is the operator's, until Apply or Cancel.
pub fn follow(stored: &str) {
    if PREVIEWING.with(Cell::get) {
        return;
    }
    if let Some(theme) = themes::find(stored) {
        show(theme);
    }
}

/// Store `name` as the machine's theme.
async fn store(name: &'static str) -> Result<(), String> {
    let request = gloo_net::http::Request::post("/api/theme")
        .json(&serde_json::json!({ "theme": name }))
        .map_err(|e| format!("the request could not be built: {e}"))?;
    let response = request.send().await.map_err(|e| format!("the web console did not answer: {e}"))?;
    if response.ok() {
        return Ok(());
    }
    let status = response.status();
    let said = response
        .json::<serde_json::Value>()
        .await
        .ok()
        .and_then(|body| body.get("error").and_then(|e| e.as_str()).map(str::to_string));
    Err(said.unwrap_or_else(|| format!("the web console answered {status}")))
}

fn focus_the_chosen_card() {
    if let Some(card) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(".theme-card[aria-checked=\"true\"]").ok().flatten())
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = card.focus();
    }
}

/// The theme picker: a modal on the desktop page and the display console,
/// a sheet on the phone's page. Mounted while `open` is true.
#[component]
pub fn ThemePicker(open: RwSignal<bool>, #[prop(optional)] sheet: bool) -> impl IntoView {
    let before = shown();
    let choice = RwSignal::new(before.name);
    let error = RwSignal::new(None::<String>);
    let busy = RwSignal::new(false);
    // However the picker goes away (Cancel, Escape, a press beside it), the
    // page goes back to what it showed, unless Apply stored the choice.
    let applied = RwSignal::new(false);
    PREVIEWING.with(|p| p.set(true));
    on_cleanup(move || {
        if !applied.get_untracked() {
            show(before);
        }
        PREVIEWING.with(|p| p.set(false));
    });
    // The keyboard starts on the theme in use, so Escape and the arrow of a
    // screen reader have somewhere to be.
    gloo_timers::callback::Timeout::new(0, focus_the_chosen_card).forget();

    let cancel = move || open.set(false);
    let apply = move || {
        let name = choice.get_untracked();
        if name == before.name {
            open.set(false);
            return;
        }
        busy.set(true);
        error.set(None);
        spawn_local(async move {
            match store(name).await {
                Ok(()) => {
                    applied.set(true);
                    open.set(false);
                }
                Err(said) => {
                    error.set(Some(format!("Not stored: {said}.")));
                    busy.set(false);
                }
            }
        });
    };
    let on_key = move |ev: leptos::ev::KeyboardEvent| {
        if ev.key() == "Escape" {
            ev.prevent_default();
            cancel();
        }
    };

    let cards = move || {
        THEMES
            .iter()
            .map(|theme| {
                let chosen = move || choice.get() == theme.name;
                view! {
                    <button type="button" class="theme-card" data-theme=theme.name role="radio"
                        aria-checked=move || chosen().to_string()
                        on:click=move |_| {
                            choice.set(theme.name);
                            show(theme);
                        }>
                        <span class="tc-chips" aria-hidden="true">
                            <i class="c-panel"></i><i class="c-accent"></i><i class="c-accent-2"></i>
                            <i class="c-good"></i><i class="c-bad"></i>
                        </span>
                        <span class="tc-name">{theme.name}</span>
                        <span class="tc-note">{theme.note}</span>
                        <span class="tc-mark">{move || mark(theme.name, before.name, choice.get())}</span>
                    </button>
                }
            })
            .collect_view()
    };
    let said = move || error.get().map(|text| view! { <div class="m-error" role="alert">{text}</div> });

    if sheet {
        view! {
            <div class="sheet-bg" on:click=move |_| cancel() on:keydown=on_key>
                <div class="sheet themes" role="dialog" aria-modal="true" aria-label="Theme"
                    on:click=move |e: leptos::ev::MouseEvent| e.stop_propagation()>
                    <div class="sheet-head">
                        <span class="sheet-title">"Theme"</span>
                        <button class="sheet-close" aria-label="Close" on:click=move |_| cancel()>"×"</button>
                    </div>
                    <div class="sheet-body">
                        <div class="theme-grid" role="radiogroup" aria-label="Themes">{cards}</div>
                        {said}
                        <div class="theme-actions">
                            <span class="m-hint">{NOTE}</span>
                            <button class="sheet-action" disabled=move || busy.get()
                                on:click=move |_| apply()>"Apply"</button>
                        </div>
                    </div>
                </div>
            </div>
        }
        .into_any()
    } else {
        view! {
            <div class="palette-bg" on:click=move |_| cancel() on:keydown=on_key>
                <div class="modal themes" role="dialog" aria-modal="true" aria-label="Theme"
                    on:click=move |e: leptos::ev::MouseEvent| e.stop_propagation()>
                    <div class="m-head"><b>"Theme"</b><code>"/theme"</code></div>
                    <div class="m-note">{NOTE}</div>
                    <div class="m-body">
                        <div class="theme-grid" role="radiogroup" aria-label="Themes">{cards}</div>
                    </div>
                    {said}
                    <div class="m-actions">
                        <button class="btn ghost" on:click=move |_| cancel()>"Cancel"</button>
                        <button class="btn" disabled=move || busy.get() on:click=move |_| apply()>"Apply"</button>
                    </div>
                </div>
            </div>
        }
        .into_any()
    }
}

/// What the picker says of itself.
const NOTE: &str = "One theme for the whole machine: every browser, the phone's page and the display follow. A press shows a theme on this page; Apply keeps it.";

/// The line under a card: which theme the machine is on, and which one the
/// page is showing for now.
fn mark(card: &str, in_use: &str, chosen: &str) -> &'static str {
    match (card == in_use, card == chosen) {
        (true, true) => "in use",
        (true, false) => "in use until Apply",
        (false, true) => "shown now",
        (false, false) => "",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_card_says_which_theme_is_in_use_and_which_is_only_shown() {
        assert_eq!(mark("ember", "ember", "ember"), "in use");
        assert_eq!(mark("ember", "ember", "nebula"), "in use until Apply");
        assert_eq!(mark("nebula", "ember", "nebula"), "shown now");
        assert_eq!(mark("lagoon", "ember", "nebula"), "");
    }
}
