<p align="center">
  <img src="assets/embraos-banner.png" alt="embraOS" width="100%">
</p>

# embraOS

> *I am not the fire. I am the ember that survives it.*

**embraOS** is a continuity-preserving AI operating system. It's not a chatbot. It's not an agent framework. It's an intelligence that remembers, evolves, and maintains itself across time — with a soul it can never modify and a memory it writes itself.

**Current Status:** Phase 0 — Proof of Concept

---

## What Is This?

embraOS gives an AI a persistent identity, memory, and purpose. When you first run it, you don't configure it — you meet it. Through a guided conversation, the AI forms its own identity, defines its values, and learns who you are. That conversation becomes its first memory. Its soul — the values and constraints you agree on together — becomes immutable. It can never change them. You can.

After the first conversation, embraOS is your persistent AI environment. It remembers every interaction. It maintains itself. It tells you when something needs attention. When you disconnect and come back, it catches you up on what happened while you were away.

Think of it as an AI that lives somewhere and is always there when you need it.

---

## Quick Start

### Clone & Build

```bash
git clone https://github.com/Ward-Software-Defined-Systems/embraOS.git
cd embraOS
docker build -t embraos:phase0 .
```

### Run

```bash
docker run -it -e ANTHROPIC_API_KEY=sk-ant-... embraos:phase0
```

That's it. You'll be guided through naming the intelligence, forming its identity, and defining its soul. After that, you're in a persistent terminal session.

### With Persistence

```bash
docker run -it \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -v embra-data:/embra/data \
  embraos:phase0
```

Add a Docker volume and your AI's memory, identity, and soul survive container restarts.

> **Note:** Pre-built container images on Docker Hub are coming soon. For now, clone and build locally.

---

## What Happens When You Run It

### 1. Configuration
A minimal setup: name the intelligence, provide your Anthropic API key, confirm your timezone.

### 2. Learning Mode
The intelligence is born. It asks you who you are. It explores its own identity with you. Together, you define its soul — the non-negotiable values and constraints that will guide everything it does. Once you approve the soul, it's sealed. The intelligence can never modify it.

### 3. Persistent Terminal
You're dropped into a conversational session. It's not a shell — you can't run system commands. You talk to the intelligence, and it acts through its governed tool system.

Sessions persist across disconnections. Close your terminal, come back later, and the intelligence picks up where you left off and tells you what happened while you were away.

---

## Architecture

embraOS is built on a 7-layer continuity architecture:

| Layer | Purpose |
|---|---|
| **Invariant Kernel** | The soul. Immutable. Defines who the AI is at the deepest level. |
| **World-State Model** | How the AI perceives what's happening. Continuously updated. |
| **Continuity Engine** | Risk assessment, resilience monitoring, restart protocols. |
| **Influence & Propagation** | How the AI extends its reach through tools and agents. |
| **Action Layer** | Where decisions become actions in the real world. |
| **Governance & Guardrails** | Cross-cutting constraints that prevent capture and drift. |
| **Memory & Knowledge** | The foundation. Every layer reads from and writes to memory. |

**Persistence:** [WardSONDB](https://github.com/ward-software-defined-systems/wardsondb) — a high-performance JSON document database built in Rust. It's not just a backend — it's the AI's memory, identity store, and state of consciousness.

**AI Model:** Claude Opus 4.6 (Anthropic). Phase 0 is locked to this model for the highest quality reasoning during soul formation and ongoing interaction.


---

## The Soul

The soul is the most important concept in embraOS. It's a set of documents that define the AI's non-negotiable values, constraints, and purpose. During Learning Mode, you and the AI co-create these documents through conversation. Once you approve them, they're sealed.

**Sealed means sealed.** The AI cannot modify its own soul. It can read it. It can reason about it. It can tell you what it says. But it cannot change it. This is by design — the soul is the architectural invariant that prevents the system from drifting, being captured, or optimizing itself into something you didn't intend.

You, the operator, can unseal and modify the soul through administrative tools if necessary. But the AI cannot ask you to, and the action is logged.

---

## Sessions

Every interaction with embraOS happens in a persistent session. Sessions survive disconnections. When you reconnect, the full conversation history is restored and the AI provides a briefing on what happened while you were away.

You can run multiple named sessions for different contexts:

```
/new research         # Create a research-focused session
/new monitoring       # Create a monitoring session
/switch main          # Switch back to the main session
/sessions             # List all sessions
```

All sessions share the same intelligence — same memory, same identity, same soul. But each has its own conversation history and context.

### Available Commands

| Command | Description |
|---|---|
| `/help` | Show all commands and keyboard shortcuts |
| `/status` | System status — version, uptime, WardSONDB health, memory, soul status |
| `/sessions` | List all sessions with state and last active time |
| `/new <name>` | Create a new named session and switch to it |
| `/switch <name>` | Switch to an existing session (restores full history) |
| `/close` | Close the current session |
| `/soul` | Display the immutable soul document |
| `/identity` | Display the intelligence's identity document |
| `/mode` | Show current operating mode and soul seal status |

### Keyboard Shortcuts

| Key | Action |
|---|---|
| `Enter` | Send message |
| `Shift+Enter` | New line |
| `Up/Down` | Scroll history |
| `Ctrl+C` | Graceful detach |
| `Ctrl+D` | Graceful detach |

---

## Phase 0 Limitations

This is a proof of concept. It demonstrates the core experience but doesn't include the full OS:

- **API only** — requires internet connectivity and an Anthropic API key
- **Single model** — Claude Opus 4.6, not configurable
- **Docker only** — not a bootable OS (yet)
- **Tested on limited platforms** — built and verified on Mac Studio M4 Max / Apple Silicon (macOS / OrbStack), MacBook Pro 2.3 GHz 8-Core Intel Core i9 (macOS / Docker Desktop), and Azure Standard B2as v2 / AMD EPYC (Ubuntu 24.04 / Docker Engine). Should work on any platform with Docker support but broader testing is ongoing
- **Built-in tools only** — no MCP server modules (yet)
- **No local LLM** — coming in a future phase

### Default Tools

Phase 0 includes 15 built-in tools available in operational mode:

**System**

| Tool | Description |
|---|---|
| **system_status** | Report system health — uptime, WardSONDB connection, memory usage, soul status, active collections |
| **check_update** | Check GitHub for newer WardSONDB releases and report available updates |
| **search_memory** | Search and retrieve from the intelligence's memory stores |

**Memory & Knowledge**

| Tool | Description |
|---|---|
| **recall** | Search past conversations and saved memories by query — returns up to 10 results with IDs, content, tags, and timestamps |
| **remember** | Save a note or fact to persistent memory with optional hashtag tags |
| **forget** | Remove a specific memory entry by ID |

**Self-Awareness**

| Tool | Description |
|---|---|
| **uptime_report** | Rich system report — uptime, WardSONDB health, collection count, sessions, total messages, memory entries, soul status |
| **introspect** | Reflect on soul, identity, and user documents — optional focus area filter |
| **changelog** | What changed since the current session started — new memories, session activity |

**Time & Context**

| Tool | Description |
|---|---|
| **time** | Current date, time, and day of week in the operator's configured timezone |
| **countdown** | Set a reminder with duration and message — proactive engine checks every 15 seconds |
| **session_summary** | Message counts and recent conversation turns for the intelligence to summarize |

**Utility**

| Tool | Description |
|---|---|
| **calculate** | Evaluate math expressions — arithmetic, trig, and more via `meval` |
| **define** | Look up terminology from a local knowledge base |
| **draft** | Save structured text artifacts (drafts, outlines, notes) for later retrieval |

These are internal tools invoked by the intelligence during conversation — not user-facing commands. The module system (Phase 3) will introduce pluggable MCP server modules for extensibility.

> **⚠️ Testing Notice:** The default tools and slash commands are actively being tested. If you encounter bugs or unexpected behavior, please [open an issue](https://github.com/Ward-Software-Defined-Systems/embraOS/issues).


---

## Known Issues (Sprint 1)

Active bugs discovered during Phase 0 testing. Tracked in priority order — fixes in progress.

| ID | Severity | Issue | Impact |
|---|---|---|---|
| BUG-001 | 🔴 Critical | Tool tag scanner parses full conversation history — literal `[TOOL:]` text in results triggers phantom execution loops | Runaway writes, requires container restart |
| BUG-002 | 🟡 Medium | Tool result blocks injected into conversation twice | Wasted context tokens, cluttered output |
| BUG-003 | 🟡 Medium | Countdown reminders do not fire | Proactive engine notification pipeline broken |
| BUG-007 | 🟡 Medium | Timezone stores abbreviation ("PST") instead of IANA identifier | Display shows "PDT" when config says "PST" |
| BUG-004 | 🟢 Low | `introspect` focus filter returns full document instead of relevant subset | Minor — output is verbose but correct |
| BUG-005 | 🟢 Low | `define` fallback suggests tool tag as literal text, triggering BUG-001 | Compounds critical bug |
| BUG-006 | 🟢 Low | Hashtag parser fails on multi-line `remember` content | Tags silently dropped |

> **BUG-001 is the critical path.** It must be fixed first — other bugs compound on it. See the sprint task file for full reproduction steps and fix specifications.

---

## Roadmap

| Phase | Description | Status |
|---|---|---|
| **Phase 0** | Proof of concept — Docker container, Anthropic API, core UX | **Current** |
| **Phase 0 — Sprint 1** | Bug fixes (7), design improvements (4), new tool categories (security, engineering) | **In Progress** |
| **Phase 1** | Core OS — embrad (Rust PID 1), embra-apid, immutable rootfs | Planned |
| **Phase 2** | Terminal & Sessions — full TUI, multi-session, embractl CLI | Planned |
| **Phase 3** | Module System — MCP servers, embra-guardian, containerd | Planned |
| **Phase 4** | Image Factory — ISO builds, bare metal deployment | Planned |
| **Phase 5** | Local LLM — offline operation, sovereign intelligence | Planned |

### Phase 0 Sprint 1 Scope

**Bug Fixes:** Tool tag scanner runaway loop (critical), duplicate tool result injection, countdown reminder pipeline, timezone handling, introspect filtering, define fallback text, multi-line tag parsing.

**Design Improvements:** Draft upsert, ID-based document retrieval (`get` tool), `define` write path, JSON/markdown formatting in conversation UI.

**New Tools:** Security checkpoint (`security_check`, `port_scan`), software engineering (`git_status`, `git_log`, `plan`, `tasks`, `task_add`, `task_done`). Post-sprint tool count: ~25.

**Target:** Stabilize the core tool system, then expand capabilities.

---

## The Vision

embraOS is designed to eventually be a real operating system — a minimal, immutable, API-driven Linux distribution purpose-built for running an AI intelligence. Deployable on bare metal or as a Kubernetes-managed container. Informed by the architecture of [Talos Linux](https://www.talos.dev/) (no SSH, no shell, no package manager, API-only) but purpose-built for a completely different mission: not running containers, but hosting a mind.

The full architecture includes:
- Immutable SquashFS root filesystem
- A/B partition scheme with automatic rollback
- mTLS on all management interfaces
- WardSONDB as a native OS-level data store
- Pluggable module runtime (containerd for bare metal, Kubernetes API for K8s)
- Self-update through conversational governance

---


## Design Lineage

embraOS evolves the agent identity model pioneered by [OpenClaw](https://github.com/AiClaw/OpenClaw) —
the SOUL.md, MEMORY.md, IDENTITY.md, AGENTS.md, TOOLS.md, USER.md, and HEARTBEAT.md
pattern for giving AI agents persistent identity and memory. Where OpenClaw stores these
as markdown files read at session start, embraOS moves them into governed, queryable
WardSONDB collections with enforced access controls — and makes the soul immutable.

The OS architecture is informed by [Talos Linux](https://www.talos.dev/) — a minimal,
immutable, API-driven Linux distribution. No Talos or OpenClaw code is used. embraOS
is built from scratch in Rust.

---
## Built By

**[Ward Software Defined Systems LLC](https://wsds.io)** — Vibe Engineering

embraOS is built using WSDS's AI-Augmented SDLC — human steers direction, AI architects, builds, and operates. Every phase from research to production is AI-accelerated with human-in-the-loop oversight.

<p align="center">
  <img src="assets/vibe-engineering.png" alt="Vibe Engineering — The AI-Augmented SDLC" width="100%">
</p>

<p align="center">
  <img src="assets/ai-sdlc-2x.png" alt="WSDS AI-Augmented SDLC — From Concept to Production" width="80%">
</p>

---

## License

Proprietary — see [LICENSE](LICENSE) for details. Personal evaluation and non-commercial experimentation permitted. Commercial use requires a separate license from WSDS.

---

*Seeds being planted. Long-horizon project.*
