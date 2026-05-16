//! The enterprise web shell: top bar (status pills + role + takeover),
//! left command nav, guided-setup launchers, the live xterm.js console,
//! and a ⌘K command palette.
//!
//! Every chrome action injects into the PTY — the embedded TUI stays
//! authoritative (parity-safe).

use leptos::prelude::*;
use wasm_bindgen::prelude::*;

use crate::status::use_status;
use crate::term;

/// Sidebar command groups (label, command, hint).
const GROUPS: &[(&str, &[(&str, &str)])] = &[
    ("Session", &[
        ("/status", "overview"), ("/sessions", "list"), ("/new", "new"),
        ("/switch", "switch"), ("/close", "close"), ("/mode", "mode"),
    ]),
    ("Identity", &[("/soul", "soul"), ("/identity", "identity")]),
    ("Provider", &[
        ("/provider", "list"), ("/iter-cap", "tool cap"),
        ("/show-reasoning", "reasoning"),
    ]),
    ("Setup", &[
        ("/git-setup", "git"), ("/github-token", "gh token"),
        ("/ssh-keygen", "ssh key"), ("/ssh-copy-id", "ssh copy"),
        ("/feedback-loop", "feedback"),
    ]),
    ("Help", &[("/help", "help"), ("/ml", "multiline")]),
];

/// Guided-setup launchers (label, command).
const WIZARDS: &[(&str, &str)] = &[
    ("Provider setup", "/provider --setup"),
    ("Git setup", "/git-setup"),
    ("GitHub token", "/github-token"),
    ("SSH keygen", "/ssh-keygen"),
];

fn flat() -> Vec<(&'static str, &'static str)> {
    GROUPS.iter().flat_map(|(_, cs)| cs.iter().copied()).collect()
}

#[component]
pub fn App() -> impl IntoView {
    let status = use_status();
    // (role, owner)
    let role = RwSignal::new(("observer".to_string(), "none".to_string()));
    let palette_open = RwSignal::new(false);
    let filter = RwSignal::new(String::new());

    // One-shot: boot the terminal, wire role updates, add ⌘K / Esc.
    Effect::new(move |_| {
        term::init("embra-term");
        term::on_role(move |r, o| role.set((r, o)));

        if let Some(win) = web_sys::window() {
            let cb = Closure::<dyn FnMut(web_sys::KeyboardEvent)>::new(
                move |ev: web_sys::KeyboardEvent| {
                    let k = ev.key();
                    if (ev.ctrl_key() || ev.meta_key()) && k == "k" {
                        ev.prevent_default();
                        palette_open.update(|b| *b = !*b);
                    } else if k == "Escape" {
                        palette_open.set(false);
                    }
                },
            );
            let _ = win.add_event_listener_with_callback(
                "keydown",
                cb.as_ref().unchecked_ref(),
            );
            cb.forget();
        }
    });

    view! {
        <div class="shell">
            <div class="topbar">
                <div class="brand">
                    "embra"<b>"OS"</b>
                    <span class="ver">
                        {move || status.get().version
                            .map(|v| format!("v{v}"))
                            .unwrap_or_else(|| "console".into())}
                    </span>
                </div>
                <div class="pills">
                    {move || status.get().services.into_iter().map(|s| {
                        let cls = if s.state == "up" { "pill up" } else { "pill down" };
                        view! {
                            <span class=cls title=s.detail.clone()>
                                <span class="dot"></span>{s.name}
                            </span>
                        }
                    }).collect_view()}
                </div>
                <div class="role">
                    {move || {
                        let (r, o) = role.get();
                        if r == "writer" {
                            view! { <span class="badge writer">"● Writer"</span> }
                                .into_any()
                        } else {
                            view! {
                                <>
                                    <span class="badge observer">
                                        {format!("○ Read-only · operator {o}")}
                                    </span>
                                    <button class="btn" on:click=move |_| {
                                        let ok = web_sys::window()
                                            .and_then(|w| w.confirm_with_message(
                                                "Take control of the console? The current operator becomes read-only."
                                            ).ok())
                                            .unwrap_or(false);
                                        if ok { term::takeover(); }
                                    }>"Take control"</button>
                                </>
                            }.into_any()
                        }
                    }}
                    <button class="btn ghost"
                        on:click=move |_| palette_open.update(|b| *b = !*b)>
                        "⌘ Commands"
                    </button>
                </div>
            </div>

            <div class="nav">
                {GROUPS.iter().map(|(title, cmds)| view! {
                    <>
                        <h4>{*title}</h4>
                        {cmds.iter().map(|(c, d)| {
                            let c = *c;
                            view! {
                                <button class="cmd"
                                    on:click=move |_| term::run_command(c)>
                                    {c}" "<code>{*d}</code>
                                </button>
                            }
                        }).collect_view()}
                    </>
                }).collect_view()}
            </div>

            <div class="main">
                <div class="wizard">
                    <span class="lbl">"Guided setup:"</span>
                    {WIZARDS.iter().map(|(label, cmd)| {
                        let cmd = *cmd;
                        view! {
                            <button class="btn ghost"
                                on:click=move |_| term::run_command(cmd)>
                                {*label}
                            </button>
                        }
                    }).collect_view()}
                </div>
                <div class="term-wrap"><div id="embra-term"></div></div>
                <div class="term-hint">
                    "Live embraOS console. Buttons inject commands; the console is authoritative."
                </div>
            </div>

            {move || palette_open.get().then(|| {
                let f = filter.get().to_lowercase();
                view! {
                    <div class="palette-bg" on:click=move |_| palette_open.set(false)>
                        <div class="palette"
                            on:click=move |e: leptos::ev::MouseEvent| e.stop_propagation()>
                            <input
                                placeholder="Type a command…  (Esc to close)"
                                prop:value=move || filter.get()
                                on:input=move |e| filter.set(event_target_value(&e)) />
                            <div class="list">
                                {flat().into_iter()
                                    .filter(move |(c, d)| {
                                        f.is_empty() || c.contains(&f) || d.contains(&f)
                                    })
                                    .map(|(c, d)| view! {
                                        <div class="row" on:click=move |_| {
                                            term::run_command(c);
                                            palette_open.set(false);
                                        }>
                                            <span>{c}</span><span class="d">{d}</span>
                                        </div>
                                    }).collect_view()}
                            </div>
                        </div>
                    </div>
                }
            })}
        </div>
    }
}
