//! The enterprise web shell: top bar (status pills + role + takeover),
//! left command nav, a per-command parameter modal, a guided
//! provider-setup launcher, the live xterm.js console, and a ⌘K palette.
//!
//! Every chrome action injects into the PTY — the embedded TUI stays
//! authoritative (parity-safe). Commands that need a value open a modal
//! first; the brain-driven multi-step setup is launched with its entry
//! parameter and then continues in the console (operator answers there).

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
        ("/provider", "switch"), ("/iter-cap", "tool cap"),
        ("/show-reasoning", "reasoning"),
    ]),
    ("Setup", &[
        ("/git-setup", "git"), ("/github-token", "gh token"),
        ("/ssh-keygen", "ssh key"), ("/ssh-copy-id", "ssh copy"),
        ("/feedback-loop", "feedback"),
    ]),
    ("Guardian", &[
        ("/guardian-define", "define"), ("/guardian list", "list"),
        ("/guardian status", "status"), ("/guardian show", "show"),
        ("/guardian delete", "delete"), ("/guardian key brave", "brave key"),
    ]),
    ("Help", &[("/help", "help"), ("/ml", "multiline")]),
];

/// A field in a command's parameter modal.
struct Field {
    label: &'static str,
    ph: &'static str,
    req: bool,
    secret: bool,
    /// Non-empty → render a `<select>` of these; empty → text input.
    choices: &'static [&'static str],
}

/// A command that needs input before it can be submitted.
struct Spec {
    cmd: &'static str,
    title: &'static str,
    note: &'static str,
    /// Separator between multiple field values on the command line.
    join: &'static str,
    /// Guided: after injecting, focus the console + show the banner so
    /// the operator answers the brain's follow-up prompts there.
    guided: bool,
    fields: &'static [Field],
}

const fn t(label: &'static str, ph: &'static str, req: bool) -> Field {
    Field { label, ph, req, secret: false, choices: &[] }
}
const fn sel(label: &'static str, choices: &'static [&'static str], req: bool) -> Field {
    Field { label, ph: "", req, secret: false, choices }
}

// Choices beginning with '(' are sentinels → treated as "no argument"
// (e.g. show current status) rather than a literal value.
const SPECS: &[Spec] = &[
    Spec { cmd: "/new", title: "New session", note: "Creates and switches to it.",
        join: " ", guided: false,
        fields: &[t("Session name", "my-session", true)] },
    Spec { cmd: "/switch", title: "Switch session", note: "Attach to an existing session.",
        join: " ", guided: false,
        fields: &[t("Session name", "existing-session", true)] },
    Spec { cmd: "/iter-cap", title: "Tool iteration cap",
        note: "Blank = show current. 'reset' = restore default.",
        join: " ", guided: false,
        fields: &[t("Max iterations (1–1000) / reset", "blank = show current", false)] },
    Spec { cmd: "/show-reasoning", title: "Reasoning panel",
        note: "Toggle the live-reasoning panel.", join: " ", guided: false,
        fields: &[sel("State", &["(show current)", "on", "off", "reset"], false)] },
    Spec { cmd: "/github-token", title: "GitHub token",
        note: "Blank = show status. Stored to STATE.", join: " ", guided: false,
        fields: &[Field { label: "Token", ph: "ghp_… (blank = status)",
            req: false, secret: true, choices: &[] }] },
    Spec { cmd: "/ssh-copy-id", title: "Copy SSH key to host",
        note: "Runs ssh-copy-id to the given target.", join: " ", guided: false,
        fields: &[t("user@host", "root@10.0.0.5", true)] },
    Spec { cmd: "/git-setup", title: "Git identity",
        note: "Both blank = show current config.", join: " | ", guided: false,
        fields: &[t("Name", "Ada Lovelace", false), t("Email", "ada@example.com", false)] },
    Spec { cmd: "/provider", title: "Provider",
        note: "Switch the active provider, or show status.", join: " ", guided: false,
        fields: &[sel("Action",
            &["(show status)", "anthropic", "gemini", "ollama", "lm_studio"], false)] },
    Spec { cmd: "/provider --setup", title: "Provider setup (guided)",
        note: "Pick a provider, then answer the prompts in the console below.",
        join: " ", guided: true,
        fields: &[sel("Provider",
            &["anthropic", "gemini", "ollama", "lm_studio"], true)] },
    Spec { cmd: "/guardian status", title: "Guardian tool status",
        note: "Build status + log tail for a dynamic tool.", join: " ", guided: false,
        fields: &[t("Tool name", "web_search", true)] },
    Spec { cmd: "/guardian show", title: "Show Guardian tool source",
        note: "Print the stored module source.", join: " ", guided: false,
        fields: &[t("Tool name", "web_search", true)] },
    Spec { cmd: "/guardian delete", title: "Delete Guardian tool",
        note: "Removes manifest, overlay, project, and artifact.", join: " ", guided: false,
        fields: &[t("Tool name", "web_search", true)] },
    Spec { cmd: "/guardian key brave", title: "Brave Search API key",
        note: "Enables web_search-capable tools. Blank = show status. Stored to STATE (0600).",
        join: " ", guided: false,
        fields: &[Field { label: "API key", ph: "brave key (blank = status)",
            req: false, secret: true, choices: &[] }] },
];

fn spec_idx(cmd: &str) -> Option<usize> {
    SPECS.iter().position(|s| s.cmd == cmd)
}

fn defaults(spec: &Spec) -> Vec<String> {
    spec.fields.iter()
        .map(|f| f.choices.first().copied().unwrap_or("").to_string())
        .collect()
}

/// Build the command line from a spec + field values. `None` = a required
/// field is empty (caller should reject).
fn build(spec: &Spec, vals: &[String]) -> Option<String> {
    for (f, v) in spec.fields.iter().zip(vals) {
        if f.req && v.trim().is_empty() {
            return None;
        }
    }
    let parts: Vec<String> = spec.fields.iter().zip(vals)
        .map(|(_, v)| {
            let v = v.trim();
            if v.starts_with('(') { String::new() } else { v.to_string() }
        })
        .filter(|v| !v.is_empty())
        .collect();
    Some(if parts.is_empty() {
        spec.cmd.to_string()
    } else {
        format!("{} {}", spec.cmd, parts.join(spec.join))
    })
}

fn flat() -> Vec<(&'static str, &'static str)> {
    let mut v: Vec<(&'static str, &'static str)> =
        GROUPS.iter().flat_map(|(_, cs)| cs.iter().copied()).collect();
    v.sort_by_key(|(c, _)| *c);
    v
}

#[component]
pub fn App() -> impl IntoView {
    let status = use_status();
    let role = RwSignal::new(("observer".to_string(), "none".to_string()));
    let palette_open = RwSignal::new(false);
    let filter = RwSignal::new(String::new());
    // Parameter modal: Some(spec index) when open.
    let modal = RwSignal::new(None::<usize>);
    let vals = RwSignal::new(Vec::<String>::new());
    // Guidance banner after launching a guided (brain-driven) flow.
    let guide = RwSignal::new(false);
    // Multi-line editor (/ml): a textarea overlay, mutually exclusive
    // with the parameter modal.
    let editor_open = RwSignal::new(false);
    let editor_text = RwSignal::new(String::new());
    // True when the editor was opened by /guardian-define: submit routes
    // to the brain's `/guardian define` path instead of a user message.
    let editor_guardian = RwSignal::new(false);

    let open_modal = move |i: usize| {
        vals.set(defaults(&SPECS[i]));
        modal.set(Some(i));
    };
    // Click handler shared by nav + palette: /ml opens the multi-line
    // editor; a command that needs a value opens its modal; everything
    // else injects straight away.
    let dispatch = move |c: &'static str| {
        if c == "/ml" || c == "/guardian-define" {
            editor_guardian.set(c == "/guardian-define");
            editor_text.set(String::new());
            modal.set(None); // mutual exclusivity
            editor_open.set(true);
            return;
        }
        match spec_idx(c) {
            Some(i) => open_modal(i),
            None => term::run_command(c),
        }
    };
    // Editor submit: send the body verbatim as one message (trailing
    // newlines stripped, matching the embra-desktop structured editor),
    // then reset + close. Empty / all-newline → close, send nothing.
    let submit_editor = move || {
        let body = editor_text.get();
        let trimmed = body.trim_end_matches('\n');
        if !trimmed.is_empty() {
            if editor_guardian.get() {
                term::send_guardian_define(trimmed);
            } else {
                term::send_multiline(trimmed);
            }
        }
        editor_text.set(String::new());
        editor_open.set(false);
        editor_guardian.set(false);
    };

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
                        modal.set(None);
                        editor_open.set(false);
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

    // Return focus to the xterm pane on the any-modal-open → all-closed
    // transition, so the user can start typing immediately after closing
    // a modal (submit, Cancel, Escape, or backdrop click). prev=None on
    // initial mount → no steal on first paint.
    Effect::new(move |prev: Option<bool>| {
        let any_open = palette_open.get()
            || modal.get().is_some()
            || editor_open.get();
        if prev == Some(true) && !any_open {
            term::focus();
        }
        any_open
    });

    view! {
        <div class="shell">
            <div class="topbar">
                <div class="brand">
                    <img class="logo" src="/assets/embra-logo.png" alt="embraOS" />
                    <span class="wordmark">"embraOS"</span>
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
                {GROUPS.iter().map(|(title, cmds)| {
                    let mut sorted: Vec<(&'static str, &'static str)> = cmds.to_vec();
                    sorted.sort_by_key(|(c, _)| *c);
                    view! {
                        <>
                            <h4>{*title}</h4>
                            {sorted.into_iter().map(|(c, d)| view! {
                                <button class="cmd" on:click=move |_| dispatch(c)>
                                    {c}" "<code>{d}</code>
                                </button>
                            }).collect_view()}
                        </>
                    }
                }).collect_view()}
            </div>

            <div class="main">
                <div class="wizard">
                    <span class="lbl">"Guided setup:"</span>
                    <button class="btn" on:click=move |_| {
                        if let Some(i) = spec_idx("/provider --setup") { open_modal(i); }
                    }>"Provider setup"</button>
                    <span class="lbl" style="margin-left:auto">
                        "Other setups (/git-setup, /github-token, /ssh-keygen) are in the nav."
                    </span>
                </div>
                {move || guide.get().then(|| view! {
                    <div class="banner">
                        <span>
                            "⮕ Setup started — answer the prompts in the console below \
                             (arrow keys + Enter for selectors)."
                        </span>
                        <button class="btn ghost" on:click=move |_| guide.set(false)>
                            "Dismiss"
                        </button>
                    </div>
                })}
                <div class="term-wrap"><div id="embra-term"></div></div>
                <div class="term-hint">
                    "Live embraOS console. Buttons inject commands; the console is authoritative."
                </div>
            </div>

            // ── Multi-line editor (/ml) ───────────────────────────────
            {move || editor_open.get().then(|| {
                // Fresh NodeRef per render so .focus() fires on each open.
                // `autofocus` alone is unreliable for dynamically-mounted nodes.
                let textarea_ref = NodeRef::<leptos::html::Textarea>::new();
                textarea_ref.on_load(|el| { let _ = el.focus(); });
                view! {
                <div class="palette-bg" on:click=move |_| editor_open.set(false)>
                    <div class="modal editor"
                        on:click=move |e: leptos::ev::MouseEvent| e.stop_propagation()>
                        <div class="m-head">
                            <b>{move || if editor_guardian.get() { "Define Guardian tool" } else { "Multi-line message" }}</b>
                            <code>{move || if editor_guardian.get() { "/guardian-define" } else { "/ml" }}</code>
                        </div>
                        <div class="m-note">
                            {move || if editor_guardian.get() {
                                "Paste a Guardian tool module (// guardian-tool: marker + GUARDIAN_* + fn run). Validated, then built in the background."
                            } else {
                                "Sent as one message. A leading / or a lone . line is literal."
                            }}
                        </div>
                        <div class="m-body">
                            <textarea
                                node_ref=textarea_ref
                                class="ml-input"
                                placeholder="Type or paste a multi-line message…"
                                prop:value=move || editor_text.get()
                                on:input=move |e| editor_text.set(event_target_value(&e))
                                on:keydown=move |e: leptos::ev::KeyboardEvent| {
                                    let k = e.key();
                                    if k == "Enter" && (e.ctrl_key() || e.meta_key()) {
                                        e.prevent_default();
                                        submit_editor();
                                    } else if k == "Escape" {
                                        e.prevent_default();
                                        editor_open.set(false);
                                    }
                                } />
                        </div>
                        <div class="m-actions">
                            <span class="m-hint">
                                "Ctrl+Enter (⌘+Enter) to send · Esc to cancel"
                            </span>
                            <button class="btn ghost"
                                on:click=move |_| editor_open.set(false)>"Cancel"</button>
                            <button class="btn"
                                on:click=move |_| submit_editor()>"Send"</button>
                        </div>
                    </div>
                </div>
                }
            })}

            // ── Parameter modal ───────────────────────────────────────
            {move || modal.get().map(|i| {
                let spec = &SPECS[i];
                // Shared submit path: Run button + Enter on any input/select.
                let submit_modal = move || {
                    let line = build(&SPECS[i], &vals.get());
                    match line {
                        Some(l) => {
                            term::run_command(&l);
                            if SPECS[i].guided {
                                term::focus();
                                guide.set(true);
                            }
                            modal.set(None);
                        }
                        None => {
                            if let Some(w) = web_sys::window() {
                                let _ = w.alert_with_message(
                                    "Please fill the required field(s).");
                            }
                        }
                    }
                };
                view! {
                    <div class="palette-bg" on:click=move |_| modal.set(None)>
                        <div class="modal"
                            on:click=move |e: leptos::ev::MouseEvent| e.stop_propagation()>
                            <div class="m-head">
                                <b>{spec.title}</b>
                                <code>{spec.cmd}</code>
                            </div>
                            <div class="m-note">{spec.note}</div>
                            <div class="m-body">
                                {spec.fields.iter().enumerate().map(|(fi, f)| {
                                    // Fresh refs per render; only fi == 0 wires
                                    // .focus() on mount so the first field grabs
                                    // focus when the modal opens.
                                    let input_ref = NodeRef::<leptos::html::Input>::new();
                                    let select_ref = NodeRef::<leptos::html::Select>::new();
                                    if fi == 0 {
                                        input_ref.on_load(|el| { let _ = el.focus(); });
                                        select_ref.on_load(|el| { let _ = el.focus(); });
                                    }
                                    let label = if f.req {
                                        format!("{} *", f.label)
                                    } else { f.label.to_string() };
                                    let input = if f.choices.is_empty() {
                                        let itype = if f.secret { "password" } else { "text" };
                                        view! {
                                            <input
                                                node_ref=input_ref
                                                type=itype
                                                placeholder=f.ph
                                                prop:value=move || vals.with(|v|
                                                    v.get(fi).cloned().unwrap_or_default())
                                                on:input=move |e| {
                                                    let nv = event_target_value(&e);
                                                    vals.update(|v| if fi < v.len() { v[fi] = nv });
                                                }
                                                on:keydown=move |e: leptos::ev::KeyboardEvent| {
                                                    if e.key() == "Enter" && !e.shift_key() {
                                                        e.prevent_default();
                                                        submit_modal();
                                                    }
                                                } />
                                        }.into_any()
                                    } else {
                                        view! {
                                            <select
                                                node_ref=select_ref
                                                prop:value=move || vals.with(|v|
                                                    v.get(fi).cloned().unwrap_or_default())
                                                on:change=move |e| {
                                                    let nv = event_target_value(&e);
                                                    vals.update(|v| if fi < v.len() { v[fi] = nv });
                                                }
                                                on:keydown=move |e: leptos::ev::KeyboardEvent| {
                                                    if e.key() == "Enter" && !e.shift_key() {
                                                        e.prevent_default();
                                                        submit_modal();
                                                    }
                                                }>
                                                {f.choices.iter().map(|c| view! {
                                                    <option value=*c>{*c}</option>
                                                }).collect_view()}
                                            </select>
                                        }.into_any()
                                    };
                                    view! {
                                        <label class="field">
                                            <span>{label}</span>{input}
                                        </label>
                                    }
                                }).collect_view()}
                            </div>
                            <div class="m-actions">
                                <button class="btn ghost"
                                    on:click=move |_| modal.set(None)>"Cancel"</button>
                                <button class="btn"
                                    on:click=move |_| submit_modal()>"Run"</button>
                            </div>
                        </div>
                    </div>
                }
            })}

            // ── Command palette ───────────────────────────────────────
            {move || palette_open.get().then(|| {
                // Fresh NodeRef per open; on_load focuses the filter input
                // so the operator can type immediately.
                let palette_input_ref = NodeRef::<leptos::html::Input>::new();
                palette_input_ref.on_load(|el| { let _ = el.focus(); });
                view! {
                    <div class="palette-bg" on:click=move |_| palette_open.set(false)>
                        <div class="palette"
                            on:click=move |e: leptos::ev::MouseEvent| e.stop_propagation()>
                            <input
                                node_ref=palette_input_ref
                                placeholder="Type a command…  (Esc to close)"
                                prop:value=move || filter.get()
                                on:input=move |e| filter.set(event_target_value(&e)) />
                            <div class="list">
                                {move || {
                                    let f = filter.get().to_lowercase();
                                    flat().into_iter()
                                        .filter(move |(c, d)| {
                                            f.is_empty() || c.contains(&f) || d.contains(&f)
                                        })
                                        .map(|(c, d)| view! {
                                            <div class="row" on:click=move |_| {
                                                palette_open.set(false);
                                                dispatch(c);
                                            }>
                                                <span>{c}</span><span class="d">{d}</span>
                                            </div>
                                        }).collect_view()
                                }}
                            </div>
                        </div>
                    </div>
                }
            })}
        </div>
    }
}
