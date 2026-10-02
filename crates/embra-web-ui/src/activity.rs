//! The activity strip: the top bar's live picture of what the brain does.
//!
//! Three stations on one link row: the model, embra-brain and the knowledge
//! graph. Text deltas stream as particles from the model to the brain,
//! database traffic as particles between the brain and the graph, a tool
//! call rises from the brain as a chip, and a readout under each station
//! says what the numbers are. Idle, the brain breathes; offline, the strip
//! dims and says so.
//!
//! The data is `/ws/activity` (embra-web `activity_feed.rs`): a `snapshot`
//! first and every ten seconds, a `tick` every 200 ms while something
//! happens, `offline` when the brain is away. Names and numbers only; the
//! wire shape is pinned by embra-web's `activity_json_shape_is_pinned` and
//! the structs below mirror it. Rendering is a `<canvas>` driven by
//! `requestAnimationFrame`; the strip never injects anything into the
//! console.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::f64::consts::TAU;
use std::rc::Rc;

use futures_util::StreamExt;
use gloo_net::websocket::{Message as WsMessage, futures::WebSocket};
use gloo_timers::future::TimeoutFuture;
use leptos::prelude::*;
use leptos::task::spawn_local;
use serde::Deserialize;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use web_sys::{CanvasRenderingContext2d, HtmlCanvasElement};

// ── The wire ─────────────────────────────────────────────────────────

#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "lowercase")]
enum ActivityMsg {
    Snapshot(Snapshot),
    Tick(Tick),
    Offline,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct Snapshot {
    provider: String,
    model: String,
    mode: String,
    in_turn: bool,
    session: String,
    totals: Totals,
    collections: Vec<Collection>,
    db_lifetime: Option<DbLifetime>,
    probe: Probe,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct Totals {
    model_calls: u64,
    model_errors: u64,
    tool_calls: u64,
    tool_errors: u64,
    retrievals: u64,
    embeddings: u64,
    notifications: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct Collection {
    name: String,
    docs: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct DbLifetime {
    requests: u64,
    inserts: u64,
    queries: u64,
    deletes: u64,
}

#[derive(Debug, Deserialize, Default, Clone)]
#[serde(default)]
struct Probe {
    state: String,
    latency_ms: u64,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Tick {
    text_chars: u32,
    reasoning_chars: u32,
    model_calls: u32,
    tools: Vec<Tool>,
    db: Vec<DbOps>,
    retrievals: Vec<Retrieval>,
    in_turn: bool,
    session: String,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct Tool {
    name: String,
    origin: String,
    finished: bool,
    elapsed_ms: u32,
    is_error: bool,
}

#[derive(Debug, Deserialize, Default)]
#[serde(default)]
struct DbOps {
    collection: String,
    verb: String,
    count: u64,
}

#[derive(Debug, Deserialize, Default, Clone, Copy)]
#[serde(default)]
struct Retrieval {
    candidates: u32,
    results: u32,
    top_score: f32,
}

// ── The scene ────────────────────────────────────────────────────────

/// Colors mirror the `:root` tokens of `assets/app.css`; a canvas cannot
/// read them without more plumbing than they are worth.
const TXT: (u8, u8, u8) = (0xec, 0xe2, 0xd8);
const TXT_DIM: (u8, u8, u8) = (0x9c, 0x8b, 0x7c);
const ACCENT: (u8, u8, u8) = (0xff, 0x7a, 0x1a);
const ACCENT_2: (u8, u8, u8) = (0xf5, 0xa6, 0x23);
const ACCENT_RED: (u8, u8, u8) = (0xd2, 0x44, 0x2a);
const BAD: (u8, u8, u8) = (0xe5, 0x53, 0x3a);
const PANEL_2: (u8, u8, u8) = (0x22, 0x17, 0x10);
const LINE: (u8, u8, u8) = (0x33, 0x23, 0x1a);

/// Below this width the readouts go; below `MIN_WIDTH` nothing is drawn.
const READOUT_MIN_WIDTH: f64 = 520.0;
const MIN_WIDTH: f64 = 320.0;
/// The rates under the stations are averages over this window.
const RATE_WINDOW_MS: f64 = 2000.0;
const MAX_PARTICLES: usize = 300;
const MAX_CHIPS: usize = 8;
const CHIP_LIFE_MS: f64 = 1800.0;
const RECALL_SHOWN_MS: f64 = 2000.0;
/// A character count stands in for tokens: about four characters each.
const CHARS_PER_TOKEN: f64 = 4.0;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Lane {
    ModelToBrain,
    BrainToGraph,
    GraphToBrain,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Text,
    Reasoning,
    Query,
    Write,
    Delete,
}

struct Particle {
    lane: Lane,
    kind: Kind,
    /// 0 at the lane's start, 1 at its end; negative while queued behind.
    pos: f64,
    /// Lane lengths per second.
    speed: f64,
}

struct Chip {
    label: String,
    born: f64,
    error: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Station {
    Model,
    Brain,
    Graph,
}

/// One recent tick's share of the rates.
struct RateSample {
    at: f64,
    text_chars: f64,
    db_ops: f64,
}

#[derive(Default)]
struct Viz {
    /// The socket is open.
    connected: bool,
    /// The feed has a brain behind it (false after `offline`).
    online: bool,
    snapshot: Option<Snapshot>,
    in_turn: bool,
    session: String,
    /// The node collections' document count, from the snapshot plus the
    /// writes seen since.
    nodes: u64,
    recent: VecDeque<RateSample>,
    particles: Vec<Particle>,
    chips: Vec<Chip>,
    twinkle: [f64; 5],
    link_model: f64,
    link_graph: f64,
    model_pulse: f64,
    tool_running: Option<(String, f64)>,
    last_tool: Option<(String, u32, bool)>,
    last_retrieval: Option<(Retrieval, f64)>,
    reduced_motion: bool,
    last_frame: f64,
}

impl Viz {
    fn apply(&mut self, msg: ActivityMsg, now: f64) {
        match msg {
            ActivityMsg::Snapshot(s) => {
                self.online = true;
                self.in_turn = s.in_turn;
                self.session = s.session.clone();
                self.nodes = s
                    .collections
                    .iter()
                    .filter(|c| is_node_collection(&c.name))
                    .map(|c| c.docs)
                    .sum();
                self.snapshot = Some(s);
            }
            ActivityMsg::Tick(t) => {
                self.online = true;
                self.in_turn = t.in_turn;
                if !t.session.is_empty() {
                    self.session = t.session.clone();
                }
                let db_ops: u64 = t.db.iter().map(|d| d.count).sum();
                self.recent.push_back(RateSample {
                    at: now,
                    text_chars: f64::from(t.text_chars),
                    db_ops: db_ops as f64,
                });
                if t.text_chars > 0 {
                    self.link_model = (f64::from(t.text_chars) / 60.0).clamp(self.link_model, 1.0);
                    self.spawn(Lane::ModelToBrain, Kind::Text, (t.text_chars / 10 + 1).min(8));
                }
                if t.reasoning_chars > 0 {
                    self.link_model = (f64::from(t.reasoning_chars) / 90.0).clamp(self.link_model, 1.0);
                    self.spawn(Lane::ModelToBrain, Kind::Reasoning, (t.reasoning_chars / 20 + 1).min(4));
                }
                if t.model_calls > 0 {
                    self.model_pulse = 1.0;
                }
                for tool in &t.tools {
                    let label = if tool.origin == "cron" {
                        format!("cron: {}", tool.name)
                    } else {
                        tool.name.clone()
                    };
                    if tool.finished {
                        self.tool_running = None;
                        self.last_tool = Some((tool.name.clone(), tool.elapsed_ms, tool.is_error));
                        self.chips.push(Chip {
                            label: format!("{label} {}", secs(tool.elapsed_ms)),
                            born: now,
                            error: tool.is_error,
                        });
                        if self.chips.len() > MAX_CHIPS {
                            self.chips.remove(0);
                        }
                    } else {
                        self.tool_running = Some((label, now));
                    }
                }
                for ops in &t.db {
                    let kind = match ops.verb.as_str() {
                        "write" => Kind::Write,
                        "delete" => Kind::Delete,
                        _ => Kind::Query,
                    };
                    let n = u32::try_from(ops.count).unwrap_or(u32::MAX).min(6);
                    self.spawn(Lane::BrainToGraph, kind, n);
                    if kind == Kind::Query {
                        self.spawn(Lane::GraphToBrain, Kind::Query, 1);
                    }
                    if kind == Kind::Write && is_node_collection(&ops.collection) {
                        self.nodes = self.nodes.saturating_add(ops.count);
                    }
                    let dot = ops.collection.bytes().map(usize::from).sum::<usize>() % self.twinkle.len();
                    self.twinkle[dot] = 1.0;
                }
                if db_ops > 0 {
                    self.link_graph = (db_ops as f64 / 12.0).clamp(self.link_graph, 1.0);
                }
                if let Some(r) = t.retrievals.last() {
                    self.last_retrieval = Some((*r, now));
                }
            }
            ActivityMsg::Offline => {
                self.online = false;
                self.tool_running = None;
            }
        }
    }

    fn spawn(&mut self, lane: Lane, kind: Kind, n: u32) {
        if self.reduced_motion {
            return;
        }
        for i in 0..n {
            let i = f64::from(i);
            self.particles.push(Particle {
                lane,
                kind,
                pos: -i * 0.12,
                speed: 1.1 + 0.08 * (i % 3.0),
            });
        }
        if self.particles.len() > MAX_PARTICLES {
            let excess = self.particles.len() - MAX_PARTICLES;
            self.particles.drain(0..excess);
        }
    }

    /// Advance everything that moves or fades by `dt` seconds.
    fn step(&mut self, now: f64, dt: f64) {
        for p in &mut self.particles {
            p.pos += p.speed * dt;
        }
        self.particles.retain(|p| p.pos <= 1.0);
        self.chips.retain(|c| now - c.born < CHIP_LIFE_MS);
        while self.recent.front().is_some_and(|s| now - s.at > RATE_WINDOW_MS) {
            self.recent.pop_front();
        }
        let decay = |v: &mut f64, tau: f64| *v *= (-dt / tau).exp();
        decay(&mut self.model_pulse, 0.5);
        decay(&mut self.link_model, 0.8);
        decay(&mut self.link_graph, 0.8);
        for t in &mut self.twinkle {
            decay(t, 0.4);
        }
    }

    fn tokens_per_second(&self) -> f64 {
        self.recent.iter().map(|s| s.text_chars).sum::<f64>() / CHARS_PER_TOKEN / (RATE_WINDOW_MS / 1000.0)
    }

    fn ops_per_second(&self) -> f64 {
        self.recent.iter().map(|s| s.db_ops).sum::<f64>() / (RATE_WINDOW_MS / 1000.0)
    }

    fn dimmed(&self) -> bool {
        !(self.connected && self.online)
    }
}

/// The collections whose documents are nodes of the knowledge graph.
fn is_node_collection(name: &str) -> bool {
    matches!(name, "memory.entries" | "memory.semantic" | "memory.procedural")
}

fn secs(ms: u32) -> String {
    let s = f64::from(ms) / 1000.0;
    if s >= 10.0 { format!("{s:.0}s") } else { format!("{s:.1}s") }
}

fn thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, ch) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(ch);
    }
    out
}

fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(max.saturating_sub(1)).collect();
        out.push('…');
        out
    }
}

fn rgba((r, g, b): (u8, u8, u8), a: f64) -> String {
    format!("rgba({r},{g},{b},{:.3})", a.clamp(0.0, 1.0))
}

// ── Layout and hit-testing ───────────────────────────────────────────

struct Layout {
    model_x: f64,
    brain_x: f64,
    graph_x: f64,
    link_y: f64,
    readout_y: f64,
}

fn layout(w: f64) -> Layout {
    Layout {
        model_x: 16.0,
        brain_x: w / 2.0,
        graph_x: w - 22.0,
        link_y: 15.0,
        readout_y: 35.0,
    }
}

/// Which station a pointer at `x` (CSS px) is over, on a strip `w` wide.
fn hit_test(x: f64, w: f64) -> Option<Station> {
    let l = layout(w);
    if (x - l.model_x).abs() <= 34.0 {
        Some(Station::Model)
    } else if (x - l.brain_x).abs() <= 60.0 {
        Some(Station::Brain)
    } else if (x - l.graph_x).abs() <= 34.0 {
        Some(Station::Graph)
    } else {
        None
    }
}

fn tooltip_text(station: Station, v: &Viz) -> String {
    let snap = v.snapshot.clone().unwrap_or_default();
    match station {
        Station::Model => {
            let mut lines = vec![if snap.model.is_empty() {
                "no LLM provider configured".to_string()
            } else {
                format!("{} · {}", snap.provider, snap.model)
            }];
            let mut probe = format!("endpoint {}", if snap.probe.state.is_empty() { "unknown" } else { &snap.probe.state });
            if snap.probe.latency_ms > 0 {
                probe.push_str(&format!(" · {} ms", snap.probe.latency_ms));
            }
            lines.push(probe);
            lines.push(format!("model calls {} · errors {}", snap.totals.model_calls, snap.totals.model_errors));
            lines.join("\n")
        }
        Station::Brain => {
            let mut lines = vec![format!(
                "session {} · {}",
                if v.session.is_empty() { "none" } else { &v.session },
                if snap.mode.is_empty() { "mode unknown" } else { &snap.mode }
            )];
            lines.push(if let Some((tool, _)) = &v.tool_running {
                format!("running {tool}")
            } else if v.in_turn {
                "in a turn".to_string()
            } else {
                "idle".to_string()
            });
            if let Some((name, ms, err)) = &v.last_tool {
                lines.push(format!("last tool {name} {}{}", secs(*ms), if *err { " · error" } else { "" }));
            }
            lines.push(format!("tool calls {} · errors {}", snap.totals.tool_calls, snap.totals.tool_errors));
            lines.push(format!("notifications {}", snap.totals.notifications));
            lines.join("\n")
        }
        Station::Graph => {
            let mut lines: Vec<String> = snap
                .collections
                .iter()
                .map(|c| format!("{} {}", c.name, thousands(c.docs)))
                .collect();
            if lines.is_empty() {
                lines.push("collections not sampled yet".to_string());
            }
            if let Some(db) = &snap.db_lifetime {
                lines.push(format!(
                    "WardSONDB requests {} · inserts {} · queries {} · deletes {}",
                    thousands(db.requests),
                    thousands(db.inserts),
                    thousands(db.queries),
                    thousands(db.deletes)
                ));
            }
            if let Some((r, _)) = &v.last_retrieval {
                lines.push(format!("last recall {} of {} · top {:.2}", r.results, r.candidates, r.top_score));
            }
            lines.push(format!("retrievals {} · embeddings {}", snap.totals.retrievals, snap.totals.embeddings));
            lines.join("\n")
        }
    }
}

// ── Drawing ──────────────────────────────────────────────────────────

fn draw(ctx: &CanvasRenderingContext2d, w: f64, h: f64, v: &Viz, now: f64) {
    ctx.clear_rect(0.0, 0.0, w, h);
    if w < MIN_WIDTH {
        return;
    }
    let l = layout(w);
    let dim = if v.dimmed() { 0.4 } else { 1.0 };
    let t = now / 1000.0;

    // Links.
    let link = |x0: f64, x1: f64, glow: f64| {
        ctx.begin_path();
        ctx.move_to(x0, l.link_y);
        ctx.line_to(x1, l.link_y);
        ctx.set_line_width(1.0);
        ctx.set_stroke_style_str(&rgba(TXT_DIM, (0.10 + 0.5 * glow) * dim));
        ctx.stroke();
    };
    link(l.model_x + 8.0, l.brain_x - 10.0, v.link_model);
    link(l.brain_x + 10.0, l.graph_x - 14.0, v.link_graph);

    // Particles.
    for p in &v.particles {
        if p.pos < 0.0 {
            continue;
        }
        let (x0, x1) = match p.lane {
            Lane::ModelToBrain => (l.model_x + 8.0, l.brain_x - 10.0),
            Lane::BrainToGraph => (l.brain_x + 10.0, l.graph_x - 14.0),
            Lane::GraphToBrain => (l.graph_x - 14.0, l.brain_x + 10.0),
        };
        let x = x0 + (x1 - x0) * p.pos;
        let y = l.link_y + 2.0 * (p.pos * TAU * 1.5).sin();
        let (color, hollow) = match p.kind {
            Kind::Text => (TXT, false),
            Kind::Reasoning => (TXT_DIM, true),
            Kind::Query => (ACCENT_2, false),
            Kind::Write => (ACCENT, false),
            Kind::Delete => (ACCENT_RED, false),
        };
        ctx.begin_path();
        let _ = ctx.arc(x, y, 1.8, 0.0, TAU);
        if hollow {
            ctx.set_line_width(1.0);
            ctx.set_stroke_style_str(&rgba(color, 0.8 * dim));
            ctx.stroke();
        } else {
            ctx.set_fill_style_str(&rgba(color, 0.9 * dim));
            ctx.fill();
        }
    }

    // The model station: a dot and a halo that flashes on a model call.
    let dot = |x: f64, r: f64, color: (u8, u8, u8), a: f64| {
        ctx.begin_path();
        let _ = ctx.arc(x, l.link_y, r, 0.0, TAU);
        ctx.set_fill_style_str(&rgba(color, a * dim));
        ctx.fill();
    };
    dot(l.model_x, 9.0, TXT, 0.08 + 0.4 * v.model_pulse);
    dot(l.model_x, 4.5, TXT, 0.9);

    // The brain: breathes idle, glows in a turn, sweeps while a tool runs.
    let breath = if v.in_turn {
        0.45 + 0.15 * (t * TAU).sin()
    } else {
        0.15 + 0.12 * (0.5 + 0.5 * (t * TAU * 0.15).sin())
    };
    dot(l.brain_x, 13.0, ACCENT, if v.reduced_motion { 0.2 } else { breath });
    dot(l.brain_x, 6.5, ACCENT, 0.95);
    if v.tool_running.is_some() && !v.reduced_motion {
        let start = (t * TAU / 1.2) % TAU;
        ctx.begin_path();
        let _ = ctx.arc(l.brain_x, l.link_y, 10.5, start, start + TAU / 4.0);
        ctx.set_line_width(1.5);
        ctx.set_stroke_style_str(&rgba(ACCENT_2, 0.9 * dim));
        ctx.stroke();
    }

    // The knowledge graph: a constellation whose dots twinkle per request.
    const OFFSETS: [(f64, f64); 5] = [(0.0, 0.0), (-9.0, -6.0), (9.0, -5.0), (-7.0, 7.0), (8.0, 7.0)];
    const EDGES: [(usize, usize); 5] = [(0, 1), (0, 2), (0, 3), (0, 4), (1, 3)];
    ctx.set_line_width(1.0);
    ctx.set_stroke_style_str(&rgba(ACCENT_2, 0.25 * dim));
    for (a, b) in EDGES {
        ctx.begin_path();
        ctx.move_to(l.graph_x + OFFSETS[a].0, l.link_y + OFFSETS[a].1);
        ctx.line_to(l.graph_x + OFFSETS[b].0, l.link_y + OFFSETS[b].1);
        ctx.stroke();
    }
    for (i, (dx, dy)) in OFFSETS.iter().enumerate() {
        ctx.begin_path();
        let r = if i == 0 { 3.0 } else { 2.2 };
        let _ = ctx.arc(l.graph_x + dx, l.link_y + dy, r, 0.0, TAU);
        ctx.set_fill_style_str(&rgba(ACCENT_2, (0.5 + 0.5 * v.twinkle[i]) * dim));
        ctx.fill();
    }

    // Chips rise from the brain and fade.
    ctx.set_font("11px Inter, system-ui, sans-serif");
    ctx.set_text_align("center");
    ctx.set_text_baseline("middle");
    for (i, chip) in v.chips.iter().enumerate() {
        let age = ((now - chip.born) / CHIP_LIFE_MS).clamp(0.0, 1.0);
        let rise = if v.reduced_motion { 8.0 } else { 4.0 + 12.0 * age };
        let alpha = if age > 0.6 { (1.0 - age) / 0.4 } else { 1.0 };
        let x = l.brain_x + 56.0 * (i as f64 - (v.chips.len() as f64 - 1.0) / 2.0);
        let y = l.link_y - rise;
        let label = if chip.error { format!("! {}", chip.label) } else { chip.label.clone() };
        let width = 7.0 * label.chars().count() as f64 + 10.0;
        ctx.set_fill_style_str(&rgba(PANEL_2, 0.9 * alpha * dim));
        ctx.fill_rect(x - width / 2.0, y - 8.0, width, 16.0);
        ctx.set_stroke_style_str(&rgba(if chip.error { BAD } else { LINE }, alpha * dim));
        ctx.stroke_rect(x - width / 2.0, y - 8.0, width, 16.0);
        ctx.set_fill_style_str(&rgba(if chip.error { BAD } else { TXT }, alpha * dim));
        let _ = ctx.fill_text(&label, x, y);
    }

    // Readouts.
    if w < READOUT_MIN_WIDTH {
        return;
    }
    ctx.set_font("13px Inter, system-ui, sans-serif");
    ctx.set_text_baseline("alphabetic");
    let snap = v.snapshot.as_ref();
    let dim_text = rgba(TXT_DIM, dim);
    let text = rgba(TXT, dim);

    ctx.set_text_align("left");
    let model = snap.map(|s| s.model.as_str()).unwrap_or("");
    let tps = v.tokens_per_second();
    let model_line = if model.is_empty() {
        "no model".to_string()
    } else if tps >= 0.5 {
        format!("{} · {:.0} tok/s", clip(model, 22), tps)
    } else {
        clip(model, 26)
    };
    ctx.set_fill_style_str(&text);
    let _ = ctx.fill_text(&model_line, l.model_x - 8.0, l.readout_y);

    ctx.set_text_align("center");
    let brain_line = if !v.connected {
        "activity feed connecting…".to_string()
    } else if !v.online {
        "activity feed offline".to_string()
    } else if let Some((tool, since)) = &v.tool_running {
        format!("embra-brain · {} {:.1}s", clip(tool, 20), (now - since) / 1000.0)
    } else if v.in_turn {
        "embra-brain · thinking".to_string()
    } else {
        "embra-brain · idle".to_string()
    };
    ctx.set_fill_style_str(if v.dimmed() { &dim_text } else { &text });
    let _ = ctx.fill_text(&brain_line, l.brain_x, l.readout_y);

    ctx.set_text_align("right");
    let mut graph_line = format!("{} nodes", thousands(v.nodes));
    let ops = v.ops_per_second();
    if ops >= 0.5 {
        graph_line.push_str(&format!(" · {ops:.0} ops/s"));
    }
    if let Some((r, at)) = &v.last_retrieval
        && now - at < RECALL_SHOWN_MS
    {
        graph_line.push_str(&format!(" · recall {}/{}", r.results, r.candidates));
    }
    ctx.set_fill_style_str(&text);
    let _ = ctx.fill_text(&graph_line, l.graph_x + 22.0, l.readout_y);
}

// ── The component ────────────────────────────────────────────────────

type RafClosure = Rc<RefCell<Option<Closure<dyn FnMut(f64)>>>>;

fn now_ms() -> f64 {
    js_sys::Date::now()
}

fn activity_ws_url() -> String {
    let location = web_sys::window().map(|w| w.location());
    let protocol = location.as_ref().and_then(|l| l.protocol().ok()).unwrap_or_default();
    let host = location.as_ref().and_then(|l| l.host().ok()).unwrap_or_default();
    let scheme = if protocol == "https:" { "wss" } else { "ws" };
    format!("{scheme}://{host}/ws/activity")
}

/// The socket task: reconnects for the app's lifetime, 1 s doubling to
/// 10 s between attempts, reset after a connection that delivered.
fn start_ws(state: Rc<RefCell<Viz>>, aria: RwSignal<String>) {
    spawn_local(async move {
        let mut backoff_ms: u32 = 1000;
        loop {
            if let Ok(ws) = WebSocket::open(&activity_ws_url()) {
                state.borrow_mut().connected = true;
                let (_sink, mut stream) = ws.split();
                let mut delivered = false;
                while let Some(item) = stream.next().await {
                    match item {
                        Ok(WsMessage::Text(text)) => {
                            if let Ok(msg) = serde_json::from_str::<ActivityMsg>(&text) {
                                delivered = true;
                                let mut v = state.borrow_mut();
                                v.apply(msg, now_ms());
                                aria.set(describe(&v));
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
            {
                let mut v = state.borrow_mut();
                v.connected = false;
                v.online = false;
                v.tool_running = None;
            }
            TimeoutFuture::new(backoff_ms).await;
            backoff_ms = (backoff_ms * 2).min(10_000);
        }
    });
}

/// The strip in words, for the canvas's label.
fn describe(v: &Viz) -> String {
    let model = v.snapshot.as_ref().map(|s| s.model.as_str()).unwrap_or("");
    format!(
        "Activity: model {}; embra-brain {}; knowledge graph {} nodes",
        if model.is_empty() { "not configured" } else { model },
        if !v.online {
            "offline"
        } else if v.in_turn {
            "in a turn"
        } else {
            "idle"
        },
        thousands(v.nodes)
    )
}

/// The render loop. The closure holds itself through `RafClosure` and lives
/// for the app's lifetime, like the terminal's callbacks in `term.rs`.
fn start_raf(canvas: HtmlCanvasElement, state: Rc<RefCell<Viz>>) {
    let Some(window) = web_sys::window() else {
        return;
    };
    let Some(ctx) = canvas
        .get_context("2d")
        .ok()
        .flatten()
        .and_then(|o| o.dyn_into::<CanvasRenderingContext2d>().ok())
    else {
        return;
    };
    let holder: RafClosure = Rc::new(RefCell::new(None));
    let kick = holder.clone();
    let again = window.clone();
    *kick.borrow_mut() = Some(Closure::new(move |_: f64| {
        let now = now_ms();
        {
            let mut v = state.borrow_mut();
            let dt = if v.last_frame > 0.0 { ((now - v.last_frame) / 1000.0).min(0.25) } else { 0.0 };
            v.last_frame = now;
            v.step(now, dt);
        }
        let rect = canvas.get_bounding_client_rect();
        let (w, h) = (rect.width(), rect.height());
        let dpr = again.device_pixel_ratio().max(1.0);
        let (pw, ph) = ((w * dpr).round() as u32, (h * dpr).round() as u32);
        if canvas.width() != pw || canvas.height() != ph {
            canvas.set_width(pw);
            canvas.set_height(ph);
        }
        let _ = ctx.set_transform(dpr, 0.0, 0.0, dpr, 0.0, 0.0);
        draw(&ctx, w, h, &state.borrow(), now);
        if let Some(cb) = holder.borrow().as_ref() {
            let _ = again.request_animation_frame(cb.as_ref().unchecked_ref());
        }
    }));
    if let Some(cb) = kick.borrow().as_ref() {
        let _ = window.request_animation_frame(cb.as_ref().unchecked_ref());
    }
}

fn prefers_reduced_motion() -> bool {
    web_sys::window()
        .and_then(|w| w.match_media("(prefers-reduced-motion: reduce)").ok().flatten())
        .is_some_and(|m| m.matches())
}

#[component]
pub fn ActivityStrip() -> impl IntoView {
    let canvas_ref = NodeRef::<leptos::html::Canvas>::new();
    let state: Rc<RefCell<Viz>> = Rc::new(RefCell::new(Viz {
        reduced_motion: prefers_reduced_motion(),
        ..Default::default()
    }));
    let aria = RwSignal::new("Activity: connecting".to_string());
    // The tooltip: the station's text and where it sits.
    let tip = RwSignal::new(None::<(String, String)>);

    let started = Rc::new(std::cell::Cell::new(false));
    {
        let state = state.clone();
        let started = started.clone();
        Effect::new(move |_| {
            if started.get() {
                return;
            }
            let Some(canvas) = canvas_ref.get() else {
                return;
            };
            started.set(true);
            start_ws(state.clone(), aria);
            start_raf(canvas, state.clone());
        });
    }

    let hover_state = state.clone();
    let on_move = move |ev: web_sys::MouseEvent| {
        let Some(canvas) = canvas_ref.get() else {
            return;
        };
        let w = f64::from(canvas.client_width());
        let x = f64::from(ev.offset_x());
        let next = hit_test(x, w).map(|station| {
            let text = tooltip_text(station, &hover_state.borrow());
            let style = match station {
                Station::Model => "left:0".to_string(),
                Station::Brain => format!("left:{}px;transform:translateX(-50%)", w / 2.0),
                Station::Graph => "right:0".to_string(),
            };
            (style, text)
        });
        if tip.with_untracked(|t| t.as_ref().map(|(s, _)| s.clone())) != next.as_ref().map(|(s, _)| s.clone()) {
            tip.set(next);
        } else if let Some(next) = next {
            tip.update(|t| {
                if let Some(t) = t {
                    t.1 = next.1;
                }
            });
        }
    };

    view! {
        <div class="activity-wrap" on:mouseleave=move |_| tip.set(None)>
            <canvas node_ref=canvas_ref class="activity" role="img"
                aria-label=move || aria.get()
                on:mousemove=on_move />
            {move || tip.get().map(|(style, text)| view! {
                <div class="activity-tip" style=style>{text}</div>
            })}
        </div>
    }
}
