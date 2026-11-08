mod activity;
mod app;
mod chat;
mod json_view;
mod status;
mod term;
mod tools;

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
    match current_mode() {
        Mode::Chat => view! { <chat::ChatApp /> }.into_any(),
        Mode::Kiosk | Mode::Desktop => view! { <app::App /> }.into_any(),
    }
}

/// Which shell the page mounts. The kiosk (the display console: cog on the
/// machine's own display, `?kiosk=1` in the query) is the desktop shell
/// whatever the width or the stored choice; otherwise localStorage
/// "embra-mode" wins over the auto-detect (`"chat"` / `"desktop"`), and
/// anything else auto-detects by viewport width (≤ 768 px → chat).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Mode {
    Kiosk,
    Chat,
    Desktop,
}

fn pick_mode(search: &str, stored: Option<&str>, width: Option<f64>) -> Mode {
    if search.split(['?', '&']).any(|kv| kv == "kiosk=1") {
        return Mode::Kiosk;
    }
    match stored {
        Some("chat") => return Mode::Chat,
        Some("desktop") => return Mode::Desktop,
        _ => {}
    }
    if width.is_some_and(|w| w <= 768.0) { Mode::Chat } else { Mode::Desktop }
}

/// `pick_mode` over this window; the kiosk marks `<body class="kiosk">` for
/// the stylesheet and the terminal script.
fn current_mode() -> Mode {
    let Some(win) = web_sys::window() else { return Mode::Desktop };
    let search = win.location().search().unwrap_or_default();
    let stored = win
        .local_storage()
        .ok()
        .flatten()
        .and_then(|ls| ls.get_item("embra-mode").ok().flatten());
    let width = win.inner_width().ok().and_then(|v| v.as_f64());
    let mode = pick_mode(&search, stored.as_deref(), width);
    if mode == Mode::Kiosk
        && let Some(body) = win.document().and_then(|d| d.body())
    {
        let _ = body.class_list().add_1("kiosk");
    }
    mode
}

#[cfg(test)]
mod mode_tests {
    use super::*;

    #[test]
    fn kiosk_mode_is_the_query_flag_and_never_the_chat_view() {
        assert_eq!(pick_mode("?token=abc&kiosk=1", Some("chat"), Some(400.0)), Mode::Kiosk);
        assert_eq!(pick_mode("?kiosk=1", None, None), Mode::Kiosk);
        assert_eq!(pick_mode("?kiosk=0", None, Some(400.0)), Mode::Chat);
        assert_eq!(pick_mode("?kioskx=1", None, Some(1024.0)), Mode::Desktop);
        assert_eq!(pick_mode("", Some("chat"), Some(1920.0)), Mode::Chat);
        assert_eq!(pick_mode("", Some("desktop"), Some(400.0)), Mode::Desktop);
        assert_eq!(pick_mode("", None, Some(768.0)), Mode::Chat);
        assert_eq!(pick_mode("", None, Some(769.0)), Mode::Desktop);
    }
}
