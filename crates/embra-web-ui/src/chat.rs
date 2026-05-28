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
use wasm_bindgen_futures::spawn_local;

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
    };

    view! {
        <div class="chat-app">
            <ChatTopBar connected thinking thinking_tool session_name />
            <ChatScroll messages streaming />
            <ChatInput input connected on_send=send_msg />
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
        )
        .await;

        outbound.set(None);
        connected.set(false);
        thinking.set(false);

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
            messages.update(|m| m.push(Bubble::System { content, kind }));
        }
        ServerMsg::Tool {
            name,
            is_error,
            result,
            ..
        } => {
            messages.update(|m| {
                m.push(Bubble::Tool {
                    name,
                    is_error,
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
        ServerMsg::Reasoning { .. } => {
            // Phase 3 adds the reasoning panel. MVP drops these.
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
) -> impl IntoView {
    let switch_to_desktop = move |_: MouseEvent| {
        if let Some(win) = web_sys::window() {
            if let Ok(Some(ls)) = win.local_storage() {
                let _ = ls.set_item("embra-mode", "desktop");
            }
            let _ = win.location().reload();
        }
    };

    view! {
        <div class="chat-topbar">
            <div class="ct-brand">
                <img class="ct-logo" src="/assets/embra-logo.png" alt="embraOS" />
                <span class="ct-name">"embraOS"</span>
            </div>
            <div class="ct-status">
                {move || {
                    if !connected.get() {
                        view! { <span class="ct-dot down"></span><span class="ct-state">"reconnecting…"</span> }.into_any()
                    } else if thinking.get() {
                        let label = {
                            let t = thinking_tool.get();
                            if t.is_empty() { "thinking…".to_string() } else { format!("{t}…") }
                        };
                        view! { <span class="ct-dot warn"></span><span class="ct-state">{label}</span> }.into_any()
                    } else {
                        view! { <span class="ct-dot up"></span><span class="ct-state">"connected"</span> }.into_any()
                    }
                }}
                {move || {
                    let s = session_name.get();
                    if s.is_empty() { view! { <span></span> }.into_any() }
                    else { view! { <span class="ct-session">{s}</span> }.into_any() }
                }}
            </div>
            <button class="ct-toggle" title="Switch to desktop view"
                on:click=switch_to_desktop>"↗ desktop"</button>
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
            result,
        } => {
            let cls = if is_error {
                "bubble tool error"
            } else {
                "bubble tool ok"
            };
            let summary = truncate(result.trim(), 200);
            view! {
                <div class=cls>
                    <span class="t-name">{name}</span>
                    <span class="t-sep">" → "</span>
                    <span class="t-result">{summary}</span>
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
