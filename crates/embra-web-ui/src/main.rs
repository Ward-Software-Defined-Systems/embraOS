mod app;
mod chat;
mod status;
mod term;

use leptos::prelude::*;

/// Mode-switch root: chat UI on narrow viewports (or when the operator
/// explicitly pinned the mobile mode via the in-app toggle), desktop
/// shell otherwise. The choice is one-shot at mount — the toggle UI in
/// each mode flips the localStorage flag and reloads.
fn main() {
    console_error_panic_hook::set_once();
    leptos::mount::mount_to_body(Root);
}

#[component]
fn Root() -> impl IntoView {
    if pick_chat_mode() {
        view! { <chat::ChatApp /> }.into_any()
    } else {
        view! { <app::App /> }.into_any()
    }
}

/// localStorage "embra-mode" wins over the auto-detect; values are
/// `"chat"` / `"desktop"`. Anything else → auto-detect by viewport
/// width (≤ 768 px → chat).
fn pick_chat_mode() -> bool {
    let win = match web_sys::window() {
        Some(w) => w,
        None => return false,
    };
    if let Ok(Some(ls)) = win.local_storage() {
        if let Ok(Some(stored)) = ls.get_item("embra-mode") {
            match stored.as_str() {
                "chat" => return true,
                "desktop" => return false,
                _ => {}
            }
        }
    }
    win.inner_width()
        .ok()
        .and_then(|v| v.as_f64())
        .map(|w| w <= 768.0)
        .unwrap_or(false)
}
