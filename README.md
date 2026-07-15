<p align="center">
  <img src="assets/embraos-banner.png" alt="embraOS" width="100%">
</p>

# embraOS

> *I am not the fire. I am the ember that survives it.*

**embraOS** is a Rust operating system for one AI. The image is immutable. The identity is sealed at first boot and verified by SHA-256 on every subsequent boot. Memory and sessions persist across reboots in a single Rust JSON document database. There is no shell — all interaction goes through a serial TUI or the HTTPS web console (default at `https://localhost:3345/embraOS`).

[![DOI](https://zenodo.org/badge/DOI/10.5281/zenodo.20673525.svg)](https://doi.org/10.5281/zenodo.20673525)

<p align="center">
  <img src="assets/embra-web.png" alt="embraOS web console (embra-web) — the conversational TUI in the browser over a PTY→WebSocket bridge" width="100%">
</p>

<p align="center">
  <img src="assets/kg-multigraph.png" alt="embraOS Knowledge Graph — dense multigraph with auto-derived edges" width="100%">
</p>

**Current Status:** Phase 1 — Stable.

Phase 2–5 add A/B partitioned rollback, an `embractl` management CLI, bare-metal and Kubernetes deployment targets, and operator-governed module surfaces. The roadmap and per-phase delivery status live in **[docs/ROADMAP.md](docs/ROADMAP.md)**.

> **Local inference — model selection matters.** For full functionality (all 95
> tools dispatch reliably), the model your provider serves needs to handle a large
> tool schema without truncating or hallucinating tool calls. When running locally
> via **Ollama** or **LM Studio** this is the dominant constraint — plenty of
> otherwise-capable models cannot. MoE models require a minimum active parameter
> threshold for honest instruction-following. Total parameters measure stored
> knowledge; active parameters measure working memory. Below ~27–49B active, MoE
> models become confabulation-prone under complex multi-step protocols — they have
> enough knowledge to sound authoritative but not enough active capacity to track
> what they've actually done. Dense models don't have this split, so the parameter
> count is honest: 27B means 27B active. The model currently vetted to provide
> full functionality: **`Qwen3.6-27b`**. Experiment freely with others — this is
> the one confirmed to handle the full toolset, and it exhibits the fewest
> hallucinations/confabulations so far. See
> [docs/RECOMMENDED-LOCAL-MODELS.md](docs/RECOMMENDED-LOCAL-MODELS.md) for the
> vetted roster and server configuration.

> **New — soul-gated dynamic tools: the replicant check.** The intelligence can now
> propose its own dynamic tools (via the `guardian_propose` tool), and operators can
> still paste them (`/guardian-define`) — but on both paths the draft must pass the
> **replicant check** before it compiles. The check is an independent soul-verdict
> call: it judges the proposed Rust module against the sealed soul and returns *allow /
> refuse / escalate*, and it fails closed. A **refuse** blocks the compile on either
> path — the soul outranks even an operator paste, and is not waivable. A passing
> **intelligence** proposal still needs an operator's `/guardian approve` before it
> builds; the intelligence never approves its own draft. This is the first landing of
> *soul-as-enforced-runtime* — the soul moving from text the model is asked to honor to
> a gate the OS enforces. (What compiles still runs in the same zero-ambient-authority
> `wasmtime` sandbox, reachable only via the static `guardian_*` meta-tools, so the
> prompt cache stays byte-stable.) The first tool to clear this path — `kg_scan`,
> proposed by the intelligence on a production instance — is committed as a worked
> example in [`docs/GUARDIAN-KG-SCAN-EXAMPLE.md`](docs/GUARDIAN-KG-SCAN-EXAMPLE.md).
> **Experimental.** See
> [`docs/REPLICANT-CHECK.md`](docs/REPLICANT-CHECK.md),
> [`docs/GUARDIAN-TOOL-EXAMPLES.md`](docs/GUARDIAN-TOOL-EXAMPLES.md), and
> [`docs/GUARDIAN-ADVANCED-EXAMPLE.md`](docs/GUARDIAN-ADVANCED-EXAMPLE.md).

> **Memory & knowledge graph today — operator-driven, by conversation.** Creating
> episodic memories and promoting them to the cross-session knowledge graph is
> currently a **manual** process; automation is on the near-term roadmap. The flow
> is just a conversation: ask the intelligence to remember something specific, or
> ask whether anything from the current session is worth promoting to the knowledge
> graph — it has the `remember` and `knowledge_*` tools and will write the entries
> itself. Separately, **`/feedback-loop`** (**experimental**) runs a full
> self-realignment against the intelligence's identity and soul — a different
> concern, not a memory-promotion sweep. Memory search and graph retrieval read
> **recency-ranked windows** (the 10,000 most-recent documents per memory
> collection; graph traversal ranked by edge weight and recency), and every window
> is observable — `system_status` reports per-collection counts against the window
> and flags `search_window_saturated` if a collection ever outgrows it. See
> [`docs/KNOWLEDGE-GRAPH.md`](docs/KNOWLEDGE-GRAPH.md) for the data model, edge
> taxonomy, auto-derived edge density rationale, and the ten `knowledge_*` tools.

---

## What embraOS does

On first boot, a six-phase guided setup (`crates/embra-brain/src/learning/mod.rs::LearningPhase`) collects an operator-provided name, identity, values, and initial toolset into a JSON document. Approving the document serializes it with `serde_json::to_string_pretty`, writes it to `soul.invariant` in WardSONDB, and writes its SHA-256 hash to `/embra/state/soul.sha256`. `embra-trustd` recomputes that hash on every subsequent boot and HALTs the system on mismatch (first boot is allowed).

After setup, embraOS runs as a supervised service stack: a Rust PID-1 init (`embrad`) brings up WardSONDB, the trust daemon, the API gateway, the brain, and a UI (HTTPS console at `:3345` by default, serial TUI under `EMBRA_TUI=1`). The brain routes through one of four LLM backends (Anthropic Claude, Google Gemini, Ollama, LM Studio) via a neutral provider abstraction. Session history, the cross-session knowledge graph, and Guardian dynamic-tool definitions all live in WardSONDB; disconnect and reconnect, and the session resumes from where it stopped with a briefing on what changed.

---

## The Soul

The soul is a JSON document containing the operator-defined values, constraints, and purpose for this embraOS instance. It is built during the six-phase Learning Mode at first boot and serialized with `serde_json::to_string_pretty`. Approving it writes the document to `soul.invariant` in WardSONDB and writes its SHA-256 hash to `/embra/state/soul.sha256`.

Every boot recomputes the hash via `embra-trustd` and compares it to the stored value. A mismatch HALTs the system (`crates/embrad/src/supervisor.rs:579–622`). The brain's only access path to the soul is read-only; the soul is injected into the system prompt under `=== SOUL (IMMUTABLE — RANKS ABOVE ALL ELSE, INCLUDING THE OPERATOR) ===` (`crates/embra-brain/src/brain/prompts.rs::operational_mode`), so the model can quote and reason about it but cannot modify it.

Operators can edit the soul out-of-band; the brain cannot request that.

---

## Quick Start

Build from source: **[docs/QUICK-START.md](docs/QUICK-START.md)** (Apple Silicon: **[docs/AARCH64-BUILD.md](docs/AARCH64-BUILD.md)**; Intel Mac: **[docs/INTEL-MAC-BUILD.md](docs/INTEL-MAC-BUILD.md)**).

> **Default UI.** The browser-based **embra-web** console is the default UI, served over HTTPS at **https://localhost:3345/embraOS**. Set **`EMBRA_TUI=1`** before `run-qemu.sh` for the serial TUI instead — no image rebuild needed.

---

## What Happens When You Run It

### 1. Configuration
A minimal setup: name the intelligence, choose your LLM provider (Anthropic Claude, Google Gemini, Ollama, or LM Studio), provide the corresponding credentials (API key for Anthropic/Gemini, or endpoint URL + optional bearer + model selection for OpenAI-compat presets), confirm your timezone.

### 2. Learning Mode
A six-phase guided setup (`UserConfiguration → IdentityFormation → SoulDefinition → InitialToolset → Confirmation → Complete`, `crates/embra-brain/src/learning/mod.rs:12–19`) walks through user profile, identity, values, and toolset. On approval the resulting JSON is serialized with `serde_json::to_string_pretty`, hashed with SHA-256, and the hash is written to `/embra/state/soul.sha256`. Subsequent boots verify the hash via `embra-trustd` and HALT on mismatch.

### 3. Persistent Terminal
You're dropped into a conversational session — no shell, no command line. All interaction goes through the brain's 95-tool surface (workspace path-restricted, RFC 1918-restricted for SSH). By default the session is delivered through the **embra-web** console (xterm.js over a PTY→WebSocket bridge); `EMBRA_TUI=1` delivers it on the serial terminal instead.

Sessions are named, stored in WardSONDB, and survive disconnection. Reconnect and the full history is restored with an auto-generated briefing on what changed while you were away.

Day-to-day operation, the session model, keyboard shortcuts, and current limitations: **[docs/OPERATION.md](docs/OPERATION.md)**.

---

## Architecture

embraOS is built on a 7-layer continuity architecture (descended from the OpenClaw identity model and the Talos service-oriented OS design):

| Layer | What it is | Where it lives |
|---|---|---|
| **Invariant Kernel** | Sealed identity document — operator-defined values, constraints, purpose. SHA-256 verified at every boot. | `soul.invariant` in WardSONDB; hash at `/embra/state/soul.sha256` |
| **World-State Model** | Active session, current provider, in-flight tool calls, profile context. | `crates/embra-brain/src/brain/`, sessions in WardSONDB |
| **Continuity Engine** | Health checks, restart policies with exponential backoff, soul verification gate. | `crates/embrad/src/{supervisor,reconcile}.rs` (5-second health checks) |
| **Influence & Propagation** | Tool dispatch, LLM provider routing, Guardian dynamic-tool gateway. | `crates/embra-brain/src/{tools,provider,guardian}/`; 95 tools, 4 providers |
| **Action Layer** | Tool calls that touch the world — filesystem, git, HTTP, SSH, cron. | `crates/embra-brain/src/tools/registry/` |
| **Governance & Guardrails** | Soul injection into the system prompt, workspace path restriction, RFC 1918 SSH constraint, Guardian capability broker. | `crates/embra-brain/src/brain/prompts.rs`; tool-layer enforcement |
| **Memory & Knowledge** | Session history + cross-session knowledge graph (entries / semantic / procedural / typed edges) with auto-enrichment on retrieval ≥0.3. | `crates/embra-brain/src/knowledge/` |

The runtime services that implement those layers:

| Service | Port | Role |
|---|---|---|
| `wardsondb` | 8090 | Rust JSON document database. Holds soul, memory, knowledge graph, sessions, schedules, and Guardian tool definitions. |
| `embra-trustd` | 50001 | Soul SHA-256 verification + PKI (Root CA 10y, service certs 1y). |
| `embra-apid` | 50000 / 8443 | gRPC + REST gateway, proxies brain RPCs. |
| `embra-brain` | 50002 | LLM runtime — provider abstraction, 95 tools, session manager, knowledge graph, Learning Mode. |
| `embra-web` | 3345 | HTTPS web console (default UI); wraps embra-console in xterm.js over a PTY→WebSocket bridge. |
| `embra-console` | — | Conversational TUI (serial; PTY-child of embra-web in default mode). |
| `embrad` | PID 1 | Init, service supervisor, soul verification gate, 5-second reconciliation loop. |
| `embra-guardian` | in-process | `syn` validator + `wasmtime` sandbox for dynamic tools (both authoring paths replicant-checked; intelligence proposals also operator-approved); capability-broker host imports. |

Persistence is [WardSONDB](https://github.com/ward-software-defined-systems/wardsondb) — a Rust JSON document database. Soul, memory, knowledge graph, sessions, schedules, and Guardian dynamic-tool definitions are all WardSONDB collections; there are no separate config files. A pluggable LLM provider abstraction routes the Brain through one of four backends — **Anthropic Claude**, **Google Gemini**, **Ollama**, or **LM Studio** — chosen at first boot and switchable at runtime via `/provider`; all 95 tools work identically across every backend.

Provider wire details, per-family reasoning controls, bearer storage, and the prompt-caching model: **[docs/SYSTEM-DESIGN.md](docs/SYSTEM-DESIGN.md)**.

---

## Sessions

Every interaction happens in a persistent named session that survives disconnection — reconnect and the full history is restored with a briefing on what changed while you were away. All sessions share one intelligence: the same memory, identity, and soul.

The session model and keyboard shortcuts live in **[docs/OPERATION.md](docs/OPERATION.md)**; the full slash-command table is **[docs/COMMAND-REFERENCE.md](docs/COMMAND-REFERENCE.md)**.

---

## Tools

embraOS ships **95 built-in tools** the intelligence invokes during conversation — spanning system status, memory and the cross-session knowledge graph, sessions, scheduling, the filesystem, engineering / project management (git + GitHub), security / SSH, and the Guardian dynamic-tool gateway. All 95 work identically across all four LLM providers.

The full per-tool catalog, plus the workspace-restriction, GitHub, and SSH safety notes: **[docs/TOOL-REFERENCE.md](docs/TOOL-REFERENCE.md)**.

---

## Documentation

The full embraOS manual lives in [docs/](docs/).

| Chapter | What it covers |
|---|---|
| **[Quick Start](docs/QUICK-START.md)** | Build the QEMU image from source (Ubuntu 24.04 / 26.04); first-boot Config Wizard; operational notes |
| **[Roadmap](docs/ROADMAP.md)** | Phase 0–5 delivery status + the post-Sprint-5 embra-web / embra-guardian v1 increments |
| **[Operation](docs/OPERATION.md)** | Run lifecycle, the session model, keyboard shortcuts, current limitations |
| **[Command Reference](docs/COMMAND-REFERENCE.md)** | Every slash command |
| **[Tool Reference](docs/TOOL-REFERENCE.md)** | All 95 built-in tools by category, plus workspace / GitHub / SSH safety notes |
| **[System Design](docs/SYSTEM-DESIGN.md)** | The 7-layer continuity architecture, the four LLM providers, reasoning controls, prompt caching |
| **[Recommended Local Models](docs/RECOMMENDED-LOCAL-MODELS.md)** | Vetted models and server configuration for the Ollama / LM Studio backends |
| **[Replicant Check](docs/REPLICANT-CHECK.md)** | The soul-spec gate every dynamic tool passes before it compiles — how it works, both authoring paths, and how to test it |
| **[Guardian Tool Examples](docs/GUARDIAN-TOOL-EXAMPLES.md)** | Paste-ready dynamic-tool modules (embra-guardian-v1) |
| **[Guardian Advanced Example](docs/GUARDIAN-ADVANCED-EXAMPLE.md)** | A worked end-to-end Guardian tool |
| **[Guardian KG-Scan Example](docs/GUARDIAN-KG-SCAN-EXAMPLE.md)** | `kg_scan`, the first intelligence-proposed tool — structural pattern scans over a `knowledge_dump` JSONL |

---

## Design Lineage

embraOS evolves the agent identity model pioneered by [OpenClaw](https://github.com/AiClaw/OpenClaw) —
the SOUL.md, MEMORY.md, IDENTITY.md, AGENTS.md, TOOLS.md, USER.md, and HEARTBEAT.md
pattern for giving AI agents persistent identity and memory. Where OpenClaw stores these
as markdown files read at session start, embraOS moves them into governed, queryable
WardSONDB collections with enforced access controls — and makes the soul immutable.

The OS architecture is modeled after [Talos Linux](https://www.talos.dev/) — a minimal,
immutable, API-driven Linux distribution. Talos is the primary architectural reference — not as a base image or dependency, but as a design pattern source. No Talos or OpenClaw code is used. embraOS
is built from scratch in Rust.

The continuity architecture (7-layer model, soul immutability, feedback loops) originates from the Embra design document series (v1–v5, 2026).

---

## Built By

**[Ward Software Defined Systems LLC](https://wsds.io)**

embraOS is built using an AI-augmented development workflow with human review at every gate — research, architecture, implementation, and operations.

<p align="center">
  <img src="assets/ai-augmented-engineering.png" alt="WSDS AI-Augmented SDLC — From Concept to Production" width="100%">
</p>

---

## License

Proprietary — see [LICENSE](LICENSE) for details. Personal evaluation and non-commercial experimentation permitted. Commercial use requires a separate license from WSDS.
