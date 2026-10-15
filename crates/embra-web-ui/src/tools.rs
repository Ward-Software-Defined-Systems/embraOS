//! The sidebar's Tools tab: every tool call the brain made, newest first,
//! and the sheet that opens one — the input and the result, pretty and
//! highlighted — beside the sidebar, over the left part of the terminal.
//!
//! The data is `/ws/tools` (embra-web `tool_feed.rs`): a `backlog` first
//! (the calls embra-web still holds), then a `call` per finished call,
//! `online` and `offline` for the feed's state. The wire shape is pinned by
//! embra-web's `tool_call_json_shape_is_pinned` and `ToolCall` below mirrors
//! it. After a restart the list fills from the record through `/api/tools`,
//! and **Load older** pages further back. A `⚙ #n` marker in the terminal
//! is a link (embra-term.js) that opens the call with that number here.
//! Nothing here injects anything into the console.

use futures_util::StreamExt;
use gloo_net::websocket::{Message as WsMessage, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;

use crate::json_view::{json_pre, pretty_json_capped};

// ── The wire ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ToolMsg {
    Backlog { calls: Vec<ToolCall> },
    Call(ToolCall),
    Online,
    Offline,
}

/// One tool call, as embra-web sends it (`tool_bridge.rs`). A field that
/// moves there moves here with it.
#[derive(Debug, Deserialize, Default, Clone, PartialEq)]
#[serde(default)]
pub struct ToolCall {
    pub seq: u64,
    pub id: String,
    pub name: String,
    pub session: String,
    pub origin: String,
    pub job_id: String,
    pub started_at_ms: i64,
    pub started_at_rfc: String,
    pub elapsed_ms: u32,
    pub is_error: bool,
    pub stopped: bool,
    pub input: String,
    pub input_bytes: u32,
    pub result: String,
    pub result_bytes: u32,
    pub dropped: u32,
    pub from_record: bool,
}

impl ToolCall {
    /// What a call is known by in the list: the same for a live record and
    /// its read-back twin, so one that arrives twice is shown once.
    pub fn key(&self) -> String {
        format!("{}|{}|{}|{}", self.started_at_rfc, self.id, self.name, self.job_id)
    }
}

/// A page of `/api/tools`.
#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Page {
    calls: Vec<ToolCall>,
}

/// The most calls the tab keeps; the oldest go first.
pub const MAX_CALLS: usize = 500;
/// A page of the record.
pub const PAGE: u32 = 100;

/// Insert by (start, call number), newest first; a call already known is
/// not added again, and a live record replaces its read-back twin.
fn insert_sorted(list: &mut Vec<ToolCall>, call: ToolCall) {
    if let Some(i) = list.iter().position(|c| c.key() == call.key()) {
        if list[i].from_record && !call.from_record {
            list[i] = call;
        }
        return;
    }
    let at = list
        .iter()
        .position(|c| (c.started_at_ms, c.seq) < (call.started_at_ms, call.seq))
        .unwrap_or(list.len());
    list.insert(at, call);
}

/// This boot's call count for the tab: the highest call number among the
/// live rows (the brain's per-boot counter, the newest `⚙ #n` marker),
/// zero when no live row is in the list. A read-back row is history and
/// counts for nothing, so the count never reads 100 at boot.
pub fn calls_this_boot(list: &[ToolCall]) -> u64 {
    list.iter().filter(|c| !c.from_record).map(|c| c.seq).max().unwrap_or(0)
}

fn tools_ws_url() -> String {
    let location = web_sys::window().map(|w| w.location());
    let protocol = location.as_ref().and_then(|l| l.protocol().ok()).unwrap_or_default();
    let host = location.as_ref().and_then(|l| l.host().ok()).unwrap_or_default();
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    format!("{scheme}://{host}/ws/tools")
}

/// The socket task: reconnects for the app's lifetime, 1 s doubling to
/// 10 s between attempts, reset after a connection that delivered. The
/// first backlog that leaves the list short is topped up from the record.
pub fn start_ws(calls: RwSignal<Vec<ToolCall>>, online: RwSignal<bool>, more: RwSignal<bool>) {
    spawn_local(async move {
        let mut backoff_ms: u32 = 1000;
        let mut backfilled = false;
        loop {
            if let Ok(ws) = WebSocket::open(&tools_ws_url()) {
                let (_sink, mut stream) = ws.split();
                let mut delivered = false;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(WsMessage::Text(text)) => {
                            let Ok(msg) = serde_json::from_str::<ToolMsg>(&text) else {
                                continue;
                            };
                            delivered = true;
                            match msg {
                                ToolMsg::Backlog { calls: backlog } => {
                                    calls.update(|list| {
                                        for c in backlog {
                                            insert_sorted(list, c);
                                        }
                                        list.truncate(MAX_CALLS);
                                    });
                                    if !backfilled {
                                        backfilled = true;
                                        if calls.with_untracked(|l| l.len()) < PAGE as usize {
                                            fetch_older(calls, more, None);
                                        }
                                    }
                                }
                                ToolMsg::Call(c) => calls.update(|list| {
                                    insert_sorted(list, c);
                                    list.truncate(MAX_CALLS);
                                }),
                                ToolMsg::Online => online.set(true),
                                ToolMsg::Offline => online.set(false),
                            }
                        }
                        Ok(WsMessage::Bytes(_)) => {}
                        Err(_) => break,
                    }
                }
                if delivered {
                    backoff_ms = 1000;
                }
            }
            online.set(false);
            TimeoutFuture::new(backoff_ms).await;
            backoff_ms = (backoff_ms * 2).min(10_000);
        }
    });
}

/// One page of the record (`GET /api/tools`), newest first, before the
/// call named by `before` (its `started_at_rfc`) or the newest without
/// one; merged by key. `more` says whether the page came back full, which
/// is what shows **Load older**. A brain away (503) changes nothing.
pub fn fetch_older(calls: RwSignal<Vec<ToolCall>>, more: RwSignal<bool>, before: Option<String>) {
    spawn_local(async move {
        let limit = PAGE.to_string();
        let mut params: Vec<(&str, String)> = vec![("limit", limit)];
        if let Some(b) = before {
            params.push(("before", b));
        }
        let Ok(resp) = gloo_net::http::Request::get("/api/tools").query(params).send().await else {
            return;
        };
        if !resp.ok() {
            return;
        }
        let Ok(page) = resp.json::<Page>().await else {
            return;
        };
        let got = page.calls.len();
        calls.update(|list| {
            for c in page.calls {
                insert_sorted(list, c);
            }
        });
        more.set(got as u32 >= PAGE);
    });
}

/// The terminal's `⚙ #n` marker links (embra-term.js) dispatch
/// `embra-tool-open` on the window with the call number; `f` gets it.
pub fn on_marker_click(f: impl Fn(u64) + 'static) {
    let Some(win) = web_sys::window() else {
        return;
    };
    let cb = Closure::<dyn FnMut(web_sys::Event)>::new(move |ev: web_sys::Event| {
        let Some(custom) = ev.dyn_ref::<web_sys::CustomEvent>() else {
            return;
        };
        let detail = custom.detail();
        let seq = js_sys::Reflect::get(&detail, &JsValue::from_str("seq"))
            .ok()
            .and_then(|v| v.as_f64())
            .unwrap_or(0.0);
        if seq >= 1.0 {
            f(seq as u64);
        }
    });
    let _ = win.add_event_listener_with_callback("embra-tool-open", cb.as_ref().unchecked_ref());
    cb.forget();
}

// ── Text ─────────────────────────────────────────────────────────────

/// `HH:MM:SS` in the browser's zone.
fn local_clock(ms: i64) -> String {
    let d = js_sys::Date::new(&JsValue::from_f64(ms as f64));
    format!("{:02}:{:02}:{:02}", d.get_hours(), d.get_minutes(), d.get_seconds())
}

/// `0.8s` under ten seconds, `12s` under a minute, `2m 05s` beyond.
fn format_elapsed(ms: u32) -> String {
    let secs = f64::from(ms) / 1000.0;
    if secs < 10.0 {
        format!("{secs:.1}s")
    } else if secs < 60.0 {
        format!("{}s", secs.round() as u64)
    } else {
        let whole = u64::from(ms) / 1000;
        format!("{}m {:02}s", whole / 60, whole % 60)
    }
}

/// Where the call came from: its session, or the cron job.
fn source_of(c: &ToolCall) -> String {
    if c.origin == "cron" {
        if c.job_id.is_empty() { "cron".to_string() } else { format!("cron · {}", c.job_id) }
    } else if c.session.is_empty() {
        "turn".to_string()
    } else {
        c.session.clone()
    }
}

/// The row's second line.
fn sub_line(c: &ToolCall) -> String {
    let mut parts = vec![format_elapsed(c.elapsed_ms), source_of(c)];
    if c.stopped {
        parts.push("stopped by operator".to_string());
    }
    if c.from_record {
        parts.push("rec".to_string());
    }
    if c.dropped > 0 {
        parts.push(format!("{} lost before it", c.dropped));
    }
    parts.join(" · ")
}

fn status_text(c: &ToolCall) -> &'static str {
    if c.stopped {
        "stopped"
    } else if c.is_error {
        "error"
    } else {
        "ok"
    }
}

fn input_text(c: &ToolCall) -> String {
    let t = c.input.trim();
    if t.is_empty() || t == "{}" { "(no input)".to_string() } else { pretty_json_capped(&c.input, 65536) }
}

fn result_text(c: &ToolCall) -> String {
    if c.result.trim().is_empty() { "(no output)".to_string() } else { c.result.clone() }
}

/// The filter: name, session, origin, job id or call id.
fn call_matches(f_lc: &str, c: &ToolCall) -> bool {
    f_lc.is_empty()
        || c.name.to_lowercase().contains(f_lc)
        || c.session.to_lowercase().contains(f_lc)
        || c.origin.contains(f_lc)
        || c.job_id.to_lowercase().contains(f_lc)
        || c.id.to_lowercase().contains(f_lc)
}

// ── Components ───────────────────────────────────────────────────────

/// The Tools tab's body: a filter, the feed's state, the running call,
/// the list newest first, and **Load older** while the record has more.
#[component]
pub fn ToolsTab(
    calls: RwSignal<Vec<ToolCall>>,
    online: RwSignal<bool>,
    more: RwSignal<bool>,
    running: RwSignal<Option<String>>,
    tool_open: RwSignal<Option<String>>,
) -> impl IntoView {
    let filter = RwSignal::new(String::new());
    view! {
        <input class="nav-filter" type="text" placeholder="Filter tool calls…"
            prop:value=move || filter.get()
            on:input=move |e| filter.set(event_target_value(&e))
            on:keydown=move |e: leptos::ev::KeyboardEvent| {
                if e.key() == "Escape" { filter.set(String::new()); }
            } />
        {move || (!online.get()).then(|| view! {
            <div class="tool-offline">"feed offline — the list stays"</div>
        })}
        {move || running.get().map(|name| view! {
            <div class="tool-row running">
                <span class="tr-main"><span class="tr-name">{format!("⟳ {name}")}</span></span>
                <span class="tr-sub">"running…"</span>
            </div>
        })}
        <div class="tool-list">
            {move || {
                let f = filter.get().trim().to_lowercase();
                let selected = tool_open.get();
                calls.get().into_iter().filter(|c| call_matches(&f, c)).map(|c| {
                    let key = c.key();
                    let is_sel = selected.as_deref() == Some(key.as_str());
                    let sub = sub_line(&c);
                    // The call number is per boot: shown on a live row,
                    // where it matches the terminal's marker, never on a
                    // read-back row, whose number belongs to its own boot.
                    let seq = (c.seq > 0 && !c.from_record).then(|| format!("#{}", c.seq));
                    let status = if c.is_error { "!" } else { "✓" };
                    view! {
                        <button class="tool-row" class:err=c.is_error class:rec=c.from_record
                            class:selected=is_sel
                            on:click=move |_| tool_open.set(Some(key.clone()))>
                            <span class="tr-main">
                                <span class="tr-time">{local_clock(c.started_at_ms)}</span>
                                <span class="tr-name">{c.name.clone()}</span>
                                <span class="tr-status">{status}</span>
                                <span class="tr-seq">{seq}</span>
                            </span>
                            <span class="tr-sub">{sub}</span>
                        </button>
                    }
                }).collect_view()
            }}
            {move || more.get().then(|| view! {
                <button class="tool-more" on:click=move |_| {
                    let oldest = calls.with_untracked(|l| l.last().map(|c| c.started_at_rfc.clone()));
                    fetch_older(calls, more, oldest);
                }>"Load older"</button>
            })}
        </div>
    }
}

/// The call the operator picked, beside the sidebar: its head, the input
/// and the result as the Tools tab holds them.
#[component]
pub fn ToolSheet(calls: RwSignal<Vec<ToolCall>>, tool_open: RwSignal<Option<String>>) -> impl IntoView {
    view! {
        {move || {
            let key = tool_open.get()?;
            let c = calls.with(|l| l.iter().find(|c| c.key() == key).cloned())?;
            let title_seq = (c.seq > 0 && !c.from_record).then(|| format!(" #{}", c.seq));
            let status_cls = if c.is_error { "ts-status err" } else { "ts-status" };
            let mut meta = vec![local_clock(c.started_at_ms), format_elapsed(c.elapsed_ms), source_of(&c)];
            if !c.id.is_empty() {
                meta.push(c.id.clone());
            }
            let input_shown = c.input.len();
            let result_shown = c.result.len();
            Some(view! {
                <div class="tool-sheet" role="dialog" aria-label="Tool call">
                    <div class="ts-head">
                        <div class="ts-title">
                            <span class="t-name">{c.name.clone()}</span>
                            <span class="ts-seq">{title_seq}</span>
                            <span class=status_cls>{status_text(&c)}</span>
                        </div>
                        <div class="ts-meta">{meta.join(" · ")}</div>
                        <button class="ts-close" title="Close (Esc)"
                            on:click=move |_| tool_open.set(None)>"✕"</button>
                    </div>
                    {c.from_record.then(|| view! {
                        <div class="ts-note">"from the record: the input and the result are 2 KiB previews"</div>
                    })}
                    {(c.dropped > 0).then(|| view! {
                        <div class="ts-note">{format!("{} calls were lost before this one", c.dropped)}</div>
                    })}
                    <div class="t-section">"input"</div>
                    <pre class="t-pre">{json_pre(&input_text(&c))}</pre>
                    {(c.input_bytes as usize > input_shown).then(|| view! {
                        <div class="ts-note">{format!("showing {} of {} bytes", input_shown, c.input_bytes)}</div>
                    })}
                    <div class="t-section">"result"</div>
                    <pre class="t-pre">{json_pre(&result_text(&c))}</pre>
                    {(c.result_bytes as usize > result_shown).then(|| view! {
                        <div class="ts-note">{format!("showing {} of {} bytes", result_shown, c.result_bytes)}</div>
                    })}
                </div>
            })
        }}
    }
}
