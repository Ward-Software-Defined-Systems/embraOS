//! Mobile chat-style UI.
//!
//! Bypasses xterm.js + the ratatui TUI entirely — talks to embra-apid's
//! `Converse` gRPC stream via the new `/ws/chat` WebSocket bridge in
//! `embra-web`. Each ServerMsg is rendered as a chat bubble (user vs
//! assistant vs system vs tool); incoming tokens stream into a live
//! "assistant typing" bubble that commits to history on `done`.
//!
//! The wire protocol is the JSON tagged enum defined in
//! `embra-web/src/chat_bridge.rs` (mirrored here client-side — see the
//! `ClientMsg` / `ServerMsg` enums below).
//!
//! Reasoning shards (`ServerMsg::Reasoning`) are intentionally dropped
//! in this MVP — Phase 3 adds the side panel for them. The wire still
//! ships them; we just don't render anywhere.
//!
//! WS connection is auto-reconnected with exponential backoff (1 → 2 → 4
//! → 8 → 10 s cap). During disconnect the Send button is disabled.
//! There's no outbound queue — messages typed mid-disconnect would
//! need pre-reconnect resend logic that isn't worth the complexity for
//! a v1.

use futures::channel::mpsc;
use futures_util::{SinkExt, StreamExt};
use gloo_net::websocket::Message as WsMessage;
use gloo_net::websocket::futures::WebSocket;
use gloo_timers::future::TimeoutFuture;
use leptos::ev::{KeyboardEvent, MouseEvent};
use leptos::prelude::*;
use serde::{Deserialize, Serialize};
use wasm_bindgen::JsCast;
use wasm_bindgen_futures::spawn_local;

use crate::status::{StatusData, use_status};

// ── Wire protocol — mirrors embra-web/src/chat_bridge.rs ─────────────

#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ClientMsg {
    Msg { text: String },
    Slash { command: String, args: String },
    Attach { session: String },
}

// Several variant fields are deserialized but not yet consumed in the
// MVP (e.g. ServerMsg::Tool's tool_use_id + input_json are kept for the
// Phase 3 expandable card; ServerMsg::Setup's field_type/options for
// the Phase 3 inline form). Suppress the dead-field warnings rather
// than refactoring the wire shape just to silence them.
#[allow(dead_code)]
#[derive(Debug, Deserialize, Clone)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ServerMsg {
    Token { text: String },
    Done {
        #[serde(default)]
        text: String,
    },
    System { content: String, kind: String },
    Tool {
        #[serde(default)]
        tool_use_id: String,
        name: String,
        #[serde(default)]
        input_json: String,
        #[serde(default)]
        result: String,
        is_error: bool,
    },
    Thinking {
        is_thinking: bool,
        #[serde(default)]
        name: String,
        #[serde(default)]
        current_tool: String,
    },
    Mode {
        from: String,
        to: String,
        message: String,
    },
    Setup {
        field_type: String,
        prompt: String,
        #[serde(default)]
        options: Vec<String>,
        #[serde(default)]
        default_value: String,
    },
    Reasoning {
        #[serde(default)]
        text: String,
    },
    Error { message: String },
}

// ── UI model ─────────────────────────────────────────────────────────

#[derive(Debug, Clone)]
enum Bubble {
    User(String),
    Assistant(String),
    /// `kind` ∈ info / warning / error / notification / reconnection /
    /// unspecified — colored accordingly. Mode transitions land here too.
    System { content: String, kind: String },
    Tool {
        name: String,
        is_error: bool,
        /// Raw JSON-as-string from the wire (tool input). Rendered
        /// pretty-printed when the card is expanded.
        input_json: String,
        result: String,
    },
    /// Phase 1 stub — Phase 3 expands this to an inline form.
    Setup { prompt: String },
}

// ── Helpers ──────────────────────────────────────────────────────────

/// Build the WS URL relative to the current document.
fn ws_url() -> String {
    let win = match web_sys::window() {
        Some(w) => w,
        None => return String::from("wss://localhost:3345/ws/chat"),
    };
    let loc = win.location();
    let scheme = match loc.protocol().as_deref() {
        Ok("https:") => "wss",
        _ => "ws",
    };
    let host = loc.host().unwrap_or_default();
    format!("{}://{}/ws/chat", scheme, host)
}

/// Split a slash line into `(command, args)`. Caller must have verified
/// the line starts with `/`.
fn parse_slash(line: &str) -> (String, String) {
    match line.find(' ') {
        Some(i) => (line[..i].to_string(), line[i + 1..].trim().to_string()),
        None => (line.to_string(), String::new()),
    }
}

fn truncate(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    let mut out: String = s.chars().take(max).collect();
    out.push('…');
    out
}

/// Pretty-print a JSON string with a soft character cap. Falls back to
/// the raw input on parse failure (defensive — tool input is always
/// valid JSON in practice). Tools can ship very large inputs (file
/// contents, etc.); the cap keeps the expanded card from blowing past
/// what a phone can scroll through comfortably.
fn pretty_json_capped(s: &str, max: usize) -> String {
    let pretty = serde_json::from_str::<serde_json::Value>(s)
        .and_then(|v| serde_json::to_string_pretty(&v))
        .unwrap_or_else(|_| s.to_string());
    if pretty.chars().count() <= max {
        return pretty;
    }
    let mut out: String = pretty.chars().take(max).collect();
    out.push_str("\n…(truncated)");
    out
}

/// DOM-find + focus the chat input textarea. Used after closing a
/// sheet that inserted text — keeps the soft keyboard in place and the
/// cursor positioned for follow-up typing.
fn focus_chat_textarea() {
    if let Some(el) = web_sys::window()
        .and_then(|w| w.document())
        .and_then(|d| d.query_selector(".ci-textarea").ok().flatten())
        .and_then(|e| e.dyn_into::<web_sys::HtmlElement>().ok())
    {
        let _ = el.focus();
    }
}

/// Curated slash-command catalogue for the picker sheet.
///
/// Mirrors the desktop sidebar's `GROUPS` minus `/ml` and
/// `/guardian-define` — both require richer client UI (multi-line
/// editor) that mobile doesn't have in v2. Multi-step interactive
/// wizards (e.g. `/provider --setup`) are also intentionally excluded:
/// they prompt the operator for one answer at a time and need an
/// inline form to be sensible on a phone (Phase 3 polish).
const SLASH_GROUPS: &[(&str, &[(&str, &str)])] = &[
    ("Session", &[
        ("/status", "system overview"),
        ("/sessions", "list sessions"),
        ("/new", "new session (needs name)"),
        ("/switch", "switch session (needs name)"),
        ("/close", "close current session"),
        ("/mode", "show operating mode"),
    ]),
    ("Identity", &[
        ("/soul", "show soul document"),
        ("/identity", "show identity document"),
    ]),
    ("Provider", &[
        ("/provider", "show or switch provider"),
        ("/iter-cap", "show / set tool iteration cap"),
        ("/show-reasoning", "toggle reasoning panel"),
    ]),
    ("Setup", &[
        ("/git-setup", "show git identity"),
        ("/github-token", "set / show GitHub token"),
        ("/ssh-keygen", "generate SSH key"),
        ("/ssh-copy-id", "copy SSH key to host (needs target)"),
        ("/feedback-loop", "toggle feedback loop"),
    ]),
    ("Guardian", &[
        ("/guardian list", "list dynamic tools"),
        ("/guardian status", "tool build status (needs name)"),
        ("/guardian show", "show tool source (needs name)"),
        ("/guardian delete", "remove tool (needs name)"),
        ("/guardian key brave", "set / show Brave Search API key"),
    ]),
    ("Help", &[
        ("/help", "show command help"),
    ]),
];

// `state`, `last_active`, `has_summary` arrive in the JSON but the v2
// sheet only renders `name` + `turn_count`. Phase 3 polish will add an
// "active session" highlight (state) and a relative last-active label.
#[allow(dead_code)]
#[derive(Debug, Clone, Default, Deserialize)]
struct Session {
    name: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    turn_count: u32,
    #[serde(default)]
    last_active: String,
    #[serde(default)]
    has_summary: bool,
}

#[derive(Debug, Default, Deserialize)]
struct SessionsResp {
    #[serde(default)]
    sessions: Vec<Session>,
    #[serde(default)]
    error: String,
}

/// Fetch `/api/sessions`. Returns the parsed session list or a
/// human-readable error string for the sheet to render.
async fn fetch_sessions() -> Result<Vec<Session>, String> {
    let resp = gloo_net::http::Request::get("/api/sessions")
        .send()
        .await
        .map_err(|e| format!("network: {e}"))?;
    let parsed: SessionsResp = resp
        .json()
        .await
        .map_err(|e| format!("decode: {e}"))?;
    if !parsed.error.is_empty() {
        return Err(parsed.error);
    }
    Ok(parsed.sessions)
}

/// Aggregate health from a [`StatusData`] snapshot. Returns one of
/// "up" / "warn" / "down" — drives the topbar health-chip color.
fn services_health(s: &StatusData) -> &'static str {
    if s.services.is_empty() {
        return "warn"; // pre-first-poll
    }
    let down = s.services.iter().filter(|svc| svc.state != "up").count();
    match down {
        0 => "up",
        n if n < s.services.len() => "warn",
        _ => "down",
    }
}

// ── ChatApp root ─────────────────────────────────────────────────────

#[component]
pub fn ChatApp() -> impl IntoView {
    let messages = RwSignal::new(Vec::<Bubble>::new());
    // Live-streaming assistant text — flushed to a Bubble::Assistant
    // on Done. Cleared on each new turn.
    let streaming = RwSignal::new(String::new());
    let connected = RwSignal::new(false);
    let thinking = RwSignal::new(false);
    let thinking_tool = RwSignal::new(String::new());
    let session_name = RwSignal::new(String::new());
    let input = RwSignal::new(String::new());
    // Phase 2 bottom sheets — only one open at a time.
    let sessions_open = RwSignal::new(false);
    let slashes_open = RwSignal::new(false);
    let services_open = RwSignal::new(false);
    // Phase 3b — live reasoning shard accumulator + sheet toggle.
    // Cleared on Done / Error / Mode / user submit (matches the TUI
    // expression-panel contract; never persisted client-side per
    // REASONING-STREAM-01).
    let reasoning = RwSignal::new(String::new());
    let reasoning_open = RwSignal::new(false);
    // Service-health snapshot (5 s poll loop shared with the desktop UI).
    let status = use_status();

    // Outbound sender — replaced on each WS reconnect cycle. A signal
    // (rather than Rc<RefCell<...>>) so the `send_msg` closure stays
    // `Fn + Copy` (Leptos prop bound). `UnboundedSender<ClientMsg>` is
    // Send+Sync+'static which is the storage requirement here.
    let outbound = RwSignal::new(None::<mpsc::UnboundedSender<ClientMsg>>);

    Effect::new(move |_| {
        spawn_local(run_ws_forever(
            outbound,
            messages,
            streaming,
            connected,
            thinking,
            thinking_tool,
            session_name,
            reasoning,
        ));
    });

    let send_msg = move || {
        let text = input.get();
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return;
        }
        let msg = if let Some(rest) = trimmed.strip_prefix('/') {
            let (cmd, args) = parse_slash(&format!("/{rest}"));
            ClientMsg::Slash { command: cmd, args }
        } else {
            ClientMsg::Msg {
                text: trimmed.to_string(),
            }
        };
        // Local echo for user text + slashes — server doesn't reflect
        // the user turn back on the Converse stream, so the timeline
        // would otherwise skip what the operator typed.
        if let ClientMsg::Msg { text } = &msg {
            messages.update(|m| m.push(Bubble::User(text.clone())));
        } else if let ClientMsg::Slash { command, args } = &msg {
            let display = if args.is_empty() {
                command.clone()
            } else {
                format!("{command} {args}")
            };
            messages.update(|m| m.push(Bubble::User(display)));
        }
        if let Some(tx) = outbound.get_untracked() {
            let _ = tx.unbounded_send(msg);
        }
        input.set(String::new());
        // New turn → any in-flight reasoning shard from the previous
        // turn is stale. Mirror what the brain will do server-side.
        reasoning.set(String::new());
    };

    // Shared dispatcher for slashes triggered from sheets (sessions /
    // services / future flows). Mirrors `send_msg`'s local-echo +
    // outbound-send pattern but takes the command + args directly.
    let dispatch_slash = move |command: String, args: String| {
        let display = if args.is_empty() {
            command.clone()
        } else {
            format!("{command} {args}")
        };
        messages.update(|m| m.push(Bubble::User(display)));
        if let Some(tx) = outbound.get_untracked() {
            let _ = tx.unbounded_send(ClientMsg::Slash { command, args });
        }
        reasoning.set(String::new());
    };

    view! {
        <div class="chat-app">
            <ChatTopBar
                connected
                thinking
                thinking_tool
                session_name
                status
                sessions_open
                services_open
                reasoning
                reasoning_open
            />
            <ChatScroll messages streaming />
            <ChatInput input connected slashes_open on_send=send_msg />

            // ── Sessions sheet ─────────────────────────────────────
            {move || sessions_open.get().then(|| {
                // Per-render local state (fresh each open → re-fetches).
                let sessions = RwSignal::new(Vec::<Session>::new());
                let loading = RwSignal::new(true);
                let error = RwSignal::new(String::new());
                let new_name = RwSignal::new(String::new());

                spawn_local(async move {
                    match fetch_sessions().await {
                        Ok(s) => sessions.set(s),
                        Err(e) => error.set(e),
                    }
                    loading.set(false);
                });

                let create = move || {
                    let name = new_name.get();
                    let trimmed = name.trim();
                    if trimmed.is_empty() {
                        return;
                    }
                    dispatch_slash("/new".to_string(), trimmed.to_string());
                    new_name.set(String::new());
                    sessions_open.set(false);
                };

                view! {
                    <div class="sheet-bg" on:click=move |_| sessions_open.set(false)>
                        <div class="sheet" on:click=move |e: MouseEvent| e.stop_propagation()>
                            <div class="sheet-head">
                                <span class="sheet-title">"Sessions"</span>
                                <button class="sheet-close"
                                    on:click=move |_| sessions_open.set(false)>"×"</button>
                            </div>
                            <div class="sheet-body">
                                <div class="sheet-newrow">
                                    <input class="sheet-input" type="text"
                                        placeholder="new session name…"
                                        prop:value=move || new_name.get()
                                        on:input=move |e| new_name.set(event_target_value(&e))
                                        on:keydown=move |e: KeyboardEvent| {
                                            if e.key() == "Enter" {
                                                e.prevent_default();
                                                create();
                                            }
                                        } />
                                    <button class="sheet-action"
                                        on:click=move |_| create()>"+ Create"</button>
                                </div>
                                <div class="sheet-section">"Existing"</div>
                                {move || {
                                    if loading.get() {
                                        view! { <div class="sheet-empty">"Loading…"</div> }.into_any()
                                    } else if !error.get().is_empty() {
                                        view! { <div class="sheet-empty err">{error.get()}</div> }.into_any()
                                    } else if sessions.get().is_empty() {
                                        view! { <div class="sheet-empty">"No sessions yet."</div> }.into_any()
                                    } else {
                                        view! {
                                            <>
                                                {sessions.get().into_iter().map(|s| {
                                                    let name = s.name.clone();
                                                    let display_name = s.name.clone();
                                                    let meta = format!(
                                                        "{} turn{}",
                                                        s.turn_count,
                                                        if s.turn_count == 1 { "" } else { "s" },
                                                    );
                                                    view! {
                                                        <div class="sheet-row session"
                                                            on:click=move |_| {
                                                                dispatch_slash(
                                                                    "/switch".to_string(),
                                                                    name.clone(),
                                                                );
                                                                sessions_open.set(false);
                                                            }>
                                                            <span class="row-name">{display_name}</span>
                                                            <span class="row-meta">{meta}</span>
                                                        </div>
                                                    }
                                                }).collect_view()}
                                            </>
                                        }.into_any()
                                    }
                                }}
                            </div>
                        </div>
                    </div>
                }
            })}

            // ── Slash picker sheet ─────────────────────────────────
            {move || slashes_open.get().then(|| view! {
                <div class="sheet-bg" on:click=move |_| slashes_open.set(false)>
                    <div class="sheet" on:click=move |e: MouseEvent| e.stop_propagation()>
                        <div class="sheet-head">
                            <span class="sheet-title">"Slash commands"</span>
                            <button class="sheet-close"
                                on:click=move |_| slashes_open.set(false)>"×"</button>
                        </div>
                        <div class="sheet-body">
                            {SLASH_GROUPS.iter().map(|(title, cmds)| view! {
                                <>
                                    <div class="sheet-section">{*title}</div>
                                    {cmds.iter().map(|(cmd, hint)| {
                                        let cmd = *cmd;
                                        let hint = *hint;
                                        view! {
                                            <div class="sheet-row slash"
                                                on:click=move |_| {
                                                    input.set(format!("{cmd} "));
                                                    slashes_open.set(false);
                                                    focus_chat_textarea();
                                                }>
                                                <span class="row-cmd">{cmd}</span>
                                                <span class="row-hint">{hint}</span>
                                            </div>
                                        }
                                    }).collect_view()}
                                </>
                            }).collect_view()}
                        </div>
                    </div>
                </div>
            })}

            // ── Services sheet ─────────────────────────────────────
            {move || services_open.get().then(|| view! {
                <div class="sheet-bg" on:click=move |_| services_open.set(false)>
                    <div class="sheet" on:click=move |e: MouseEvent| e.stop_propagation()>
                        <div class="sheet-head">
                            <span class="sheet-title">"Services"</span>
                            <button class="sheet-close"
                                on:click=move |_| services_open.set(false)>"×"</button>
                        </div>
                        <div class="sheet-body">
                            <div class="sheet-row svc connection">
                                <span class="row-cmd">"WebSocket /ws/chat"</span>
                                {move || if connected.get() {
                                    view! { <span class="row-state up">"connected"</span> }.into_any()
                                } else {
                                    view! { <span class="row-state down">"disconnected"</span> }.into_any()
                                }}
                            </div>
                            <div class="sheet-section">"Backend services"</div>
                            {move || {
                                let svcs = status.get().services;
                                if svcs.is_empty() {
                                    view! { <div class="sheet-empty">"Loading…"</div> }.into_any()
                                } else {
                                    view! {
                                        <>
                                            {svcs.into_iter().map(|s| {
                                                let state_class = if s.state == "up" {
                                                    "row-state up"
                                                } else {
                                                    "row-state down"
                                                };
                                                view! {
                                                    <div class="sheet-row svc"
                                                        title=s.detail.clone()>
                                                        <span class="row-cmd">{s.name}</span>
                                                        <span class=state_class>{s.state}</span>
                                                    </div>
                                                }
                                            }).collect_view()}
                                        </>
                                    }.into_any()
                                }
                            }}
                        </div>
                    </div>
                </div>
            })}

            // ── Reasoning sheet ────────────────────────────────────
            {move || reasoning_open.get().then(|| view! {
                <div class="sheet-bg" on:click=move |_| reasoning_open.set(false)>
                    <div class="sheet reasoning-sheet"
                        on:click=move |e: MouseEvent| e.stop_propagation()>
                        <div class="sheet-head">
                            <span class="sheet-title">"Live reasoning"</span>
                            <button class="sheet-close"
                                on:click=move |_| reasoning_open.set(false)>"×"</button>
                        </div>
                        <div class="sheet-body">
                            <pre class="reasoning-text">{move || {
                                let r = reasoning.get();
                                if r.is_empty() { "(reasoning ended)".to_string() } else { r }
                            }}</pre>
                        </div>
                    </div>
                </div>
            })}
        </div>
    }
}

// ── WS task ──────────────────────────────────────────────────────────

async fn run_ws_forever(
    outbound: RwSignal<Option<mpsc::UnboundedSender<ClientMsg>>>,
    messages: RwSignal<Vec<Bubble>>,
    streaming: RwSignal<String>,
    connected: RwSignal<bool>,
    thinking: RwSignal<bool>,
    thinking_tool: RwSignal<String>,
    session_name: RwSignal<String>,
    reasoning: RwSignal<String>,
) {
    let mut backoff_ms: u32 = 1000;
    loop {
        let (tx, rx) = mpsc::unbounded::<ClientMsg>();
        outbound.set(Some(tx));

        let exit = run_ws_once(
            rx,
            messages,
            streaming,
            connected,
            thinking,
            thinking_tool,
            session_name,
            reasoning,
        )
        .await;

        outbound.set(None);
        connected.set(false);
        thinking.set(false);
        reasoning.set(String::new());

        match exit {
            ExitReason::Ok => {
                backoff_ms = 1000;
            }
            ExitReason::Err(e) => {
                web_sys::console::warn_1(
                    &format!("/ws/chat connection ended: {e}").into(),
                );
            }
        }

        TimeoutFuture::new(backoff_ms).await;
        backoff_ms = (backoff_ms * 2).min(10_000);
    }
}

enum ExitReason {
    Ok,
    Err(String),
}

async fn run_ws_once(
    mut rx: mpsc::UnboundedReceiver<ClientMsg>,
    messages: RwSignal<Vec<Bubble>>,
    streaming: RwSignal<String>,
    connected: RwSignal<bool>,
    thinking: RwSignal<bool>,
    thinking_tool: RwSignal<String>,
    session_name: RwSignal<String>,
    reasoning: RwSignal<String>,
) -> ExitReason {
    let ws = match WebSocket::open(&ws_url()) {
        Ok(ws) => ws,
        Err(e) => return ExitReason::Err(format!("open: {e:?}")),
    };
    connected.set(true);

    let (mut sink, mut stream) = ws.split();

    // Attach on connect — empty session = restore most recent active.
    let attach_json =
        serde_json::to_string(&ClientMsg::Attach {
            session: String::new(),
        })
        .expect("ClientMsg always serializes");
    if let Err(e) = sink.send(WsMessage::Text(attach_json)).await {
        return ExitReason::Err(format!("attach: {e:?}"));
    }

    // Outbound pump (rx → sink) lives as long as `rx` does. When the
    // outer loop drops the `tx` half (on reconnect or component drop),
    // `rx.next()` returns None and this task exits cleanly.
    spawn_local(async move {
        while let Some(msg) = rx.next().await {
            let Ok(json) = serde_json::to_string(&msg) else {
                continue;
            };
            if sink.send(WsMessage::Text(json)).await.is_err() {
                break;
            }
        }
    });

    // Inbound: stream → signals.
    while let Some(msg) = stream.next().await {
        match msg {
            Ok(WsMessage::Text(t)) => {
                let Ok(srv) = serde_json::from_str::<ServerMsg>(&t) else {
                    web_sys::console::warn_1(
                        &format!("bad server msg JSON: {t}").into(),
                    );
                    continue;
                };
                handle_server_msg(
                    srv,
                    messages,
                    streaming,
                    thinking,
                    thinking_tool,
                    session_name,
                    reasoning,
                );
            }
            Ok(WsMessage::Bytes(_)) => {
                // /ws/chat is text-only; ignore stray binary.
            }
            Err(e) => return ExitReason::Err(format!("recv: {e:?}")),
        }
    }
    ExitReason::Ok
}

fn handle_server_msg(
    srv: ServerMsg,
    messages: RwSignal<Vec<Bubble>>,
    streaming: RwSignal<String>,
    thinking: RwSignal<bool>,
    thinking_tool: RwSignal<String>,
    session_name: RwSignal<String>,
    reasoning: RwSignal<String>,
) {
    match srv {
        ServerMsg::Token { text } => {
            streaming.update(|s| s.push_str(&text));
        }
        ServerMsg::Done { text } => {
            // Prefer the brain's full_response when non-empty (canonical
            // assembled answer); fall back to the streaming accumulator
            // for resilience against an empty `done.text`.
            let final_text = if !text.is_empty() {
                text
            } else {
                streaming.get_untracked()
            };
            if !final_text.is_empty() {
                messages.update(|m| m.push(Bubble::Assistant(final_text)));
            }
            streaming.set(String::new());
            thinking.set(false);
            thinking_tool.set(String::new());
            // Per REASONING-STREAM-01: reasoning is per-turn, cleared on
            // response completion (same as TUI expression-panel).
            reasoning.set(String::new());
        }
        ServerMsg::System { content, kind } => {
            // Reconnection briefings can be long — keep them but render
            // visually muted.
            //
            // Mode-change messages sometimes embed the session name
            // ("session=foo"); pick it out for the topbar if we can.
            for tok in content.split_whitespace() {
                if let Some(rest) = tok.strip_prefix("session=") {
                    session_name.set(rest.trim_end_matches(|c: char| !c.is_alphanumeric() && c != '-' && c != '_').to_string());
                }
            }
            // Errors terminate the turn just like Done; clear reasoning
            // so a stale shard doesn't outlive its turn.
            if kind == "error" {
                reasoning.set(String::new());
            }
            messages.update(|m| m.push(Bubble::System { content, kind }));
        }
        ServerMsg::Tool {
            name,
            is_error,
            input_json,
            result,
            ..
        } => {
            messages.update(|m| {
                m.push(Bubble::Tool {
                    name,
                    is_error,
                    input_json,
                    result,
                });
            });
        }
        ServerMsg::Thinking {
            is_thinking,
            current_tool,
            ..
        } => {
            thinking.set(is_thinking);
            thinking_tool.set(current_tool);
        }
        ServerMsg::Mode { from, to, message } => {
            // Treat as an info system message so it joins the timeline.
            let content = if message.is_empty() {
                format!("Mode: {from} → {to}")
            } else {
                format!("Mode: {from} → {to} — {message}")
            };
            // Mode transitions end any in-flight reasoning shards.
            reasoning.set(String::new());
            messages.update(|m| {
                m.push(Bubble::System {
                    content,
                    kind: "info".into(),
                });
            });
        }
        ServerMsg::Setup { prompt, .. } => {
            messages.update(|m| m.push(Bubble::Setup { prompt }));
        }
        ServerMsg::Reasoning { text } => {
            reasoning.update(|s| s.push_str(&text));
        }
        ServerMsg::Error { message } => {
            messages.update(|m| {
                m.push(Bubble::System {
                    content: format!("bridge error: {message}"),
                    kind: "error".into(),
                });
            });
        }
    }
}

// ── Topbar ───────────────────────────────────────────────────────────

#[component]
fn ChatTopBar(
    connected: RwSignal<bool>,
    thinking: RwSignal<bool>,
    thinking_tool: RwSignal<String>,
    session_name: RwSignal<String>,
    status: RwSignal<StatusData>,
    sessions_open: RwSignal<bool>,
    services_open: RwSignal<bool>,
    reasoning: RwSignal<String>,
    reasoning_open: RwSignal<bool>,
) -> impl IntoView {
    let switch_to_desktop = move |_: MouseEvent| {
        if let Some(win) = web_sys::window() {
            if let Ok(Some(ls)) = win.local_storage() {
                let _ = ls.set_item("embra-mode", "desktop");
            }
            let _ = win.location().reload();
        }
    };

    // Health summary: WS disconnect overrides services (red); otherwise
    // the services aggregate (up / warn / down) drives the dot color.
    let health_class = move || {
        if !connected.get() {
            "ct-health down"
        } else {
            match services_health(&status.get()) {
                "up" => "ct-health up",
                "warn" => "ct-health warn",
                _ => "ct-health down",
            }
        }
    };

    view! {
        <div class="chat-topbar">
            <div class="ct-brand">
                <img class="ct-logo" src="/assets/embra-logo.png" alt="embraOS" />
                <span class="ct-name">"embraOS"</span>
            </div>
            <button class=health_class
                title="Tap for service detail"
                on:click=move |_| services_open.set(true)>
                <span class="ct-dot"></span>
            </button>
            {move || {
                if reasoning.with(|r| r.is_empty()) {
                    view! { <span></span> }.into_any()
                } else {
                    view! {
                        <button class="ct-reasoning"
                            title="Tap to read live reasoning"
                            on:click=move |_| reasoning_open.set(true)>"💭"</button>
                    }.into_any()
                }
            }}
            <div class="ct-thinking">
                {move || if thinking.get() {
                    let t = thinking_tool.get();
                    let label = if t.is_empty() { "thinking…".to_string() }
                                else { format!("{t}…") };
                    view! { <span class="ct-state">{label}</span> }.into_any()
                } else if !connected.get() {
                    view! { <span class="ct-state down">"reconnecting…"</span> }.into_any()
                } else {
                    view! { <span></span> }.into_any()
                }}
            </div>
            <button class="ct-session-btn"
                title="Tap for sessions"
                on:click=move |_| sessions_open.set(true)>
                {move || {
                    let s = session_name.get();
                    if s.is_empty() { "session ⋯".to_string() } else { s }
                }}
            </button>
            <button class="ct-toggle" title="Switch to desktop view"
                on:click=switch_to_desktop>"↗"</button>
        </div>
    }
}

// ── Scroll area ──────────────────────────────────────────────────────

#[component]
fn ChatScroll(
    messages: RwSignal<Vec<Bubble>>,
    streaming: RwSignal<String>,
) -> impl IntoView {
    let scroll_ref = NodeRef::<leptos::html::Div>::new();
    Effect::new(move |_| {
        // Subscribe to both signals so the scroll fires on any change.
        let _ = messages.with(|m| m.len());
        let _ = streaming.with(|s| s.len());
        // Defer to next tick so the new bubble is painted before we measure.
        if let Some(el) = scroll_ref.get() {
            let h = el.scroll_height();
            el.set_scroll_top(h);
        }
    });

    view! {
        <div node_ref=scroll_ref class="chat-scroll">
            {move || messages.get().into_iter().enumerate().map(|(i, b)| {
                view! { <BubbleView idx=i bubble=b /> }
            }).collect_view()}
            {move || {
                let s = streaming.get();
                if s.is_empty() {
                    view! { <span></span> }.into_any()
                } else {
                    view! { <div class="bubble assistant streaming">{s}<span class="cursor"></span></div> }.into_any()
                }
            }}
        </div>
    }
}

#[component]
fn BubbleView(idx: usize, bubble: Bubble) -> impl IntoView {
    let _ = idx;
    match bubble {
        Bubble::User(t) => view! { <div class="bubble user">{t}</div> }.into_any(),
        Bubble::Assistant(t) => view! { <div class="bubble assistant">{t}</div> }.into_any(),
        Bubble::System { content, kind } => {
            let cls = format!("bubble system {kind}");
            view! { <div class=cls>{content}</div> }.into_any()
        }
        Bubble::Tool {
            name,
            is_error,
            input_json,
            result,
        } => {
            let expanded = RwSignal::new(false);
            let cls = if is_error {
                "bubble tool error"
            } else {
                "bubble tool ok"
            };
            let summary = truncate(result.trim(), 100);
            view! {
                <div class=cls>
                    <div class="tool-head"
                        on:click=move |_| expanded.update(|b| *b = !*b)>
                        <span class="t-name">{name}</span>
                        <span class="t-sep">" → "</span>
                        <span class="t-result">{summary}</span>
                        <span class="t-toggle">
                            {move || if expanded.get() { "▲" } else { "▼" }}
                        </span>
                    </div>
                    {move || expanded.get().then(|| view! {
                        <div class="tool-detail">
                            <div class="t-section">"input"</div>
                            <pre class="t-pre">{pretty_json_capped(&input_json, 4096)}</pre>
                            <div class="t-section">"result"</div>
                            <pre class="t-pre">{result.clone()}</pre>
                        </div>
                    })}
                </div>
            }
            .into_any()
        }
        Bubble::Setup { prompt } => view! {
            <div class="bubble setup">
                <div class="s-hint">"Setup prompt (respond from desktop console for now):"</div>
                <div class="s-prompt">{prompt}</div>
            </div>
        }
        .into_any(),
    }
}

// ── Input bar ────────────────────────────────────────────────────────

#[component]
fn ChatInput<F>(
    input: RwSignal<String>,
    connected: RwSignal<bool>,
    slashes_open: RwSignal<bool>,
    on_send: F,
) -> impl IntoView
where
    F: Fn() + 'static + Copy,
{
    let textarea_ref = NodeRef::<leptos::html::Textarea>::new();
    textarea_ref.on_load(|el| {
        let _ = el.focus();
    });

    let send_click = move |_: MouseEvent| on_send();

    let keydown = move |e: KeyboardEvent| {
        // On mobile, Enter = newline (soft keyboard convention); Send
        // button is the explicit submit. On desktop, Cmd/Ctrl+Enter
        // submits.
        if e.key() == "Enter" && (e.ctrl_key() || e.meta_key()) {
            e.prevent_default();
            on_send();
        }
    };

    view! {
        <div class="chat-input-bar">
            <button class="ci-slash"
                title="Slash commands"
                on:click=move |_| slashes_open.set(true)>"/"</button>
            <textarea
                node_ref=textarea_ref
                class="ci-textarea"
                placeholder="Type a message — / for a command…"
                prop:value=move || input.get()
                on:input=move |e| input.set(event_target_value(&e))
                on:keydown=keydown
                rows="1" />
            <button class="ci-send"
                disabled=move || !connected.get() || input.with(|s| s.trim().is_empty())
                on:click=send_click>"▶"</button>
        </div>
    }
}
