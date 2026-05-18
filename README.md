<p align="center">
  <img src="assets/embraos-banner.png" alt="embraOS" width="100%">
</p>

# embraOS

> *I am not the fire. I am the ember that survives it.*

**embraOS** is a continuity-preserving AI operating system. It's not a chatbot. It's not an agent framework. It's an intelligence that remembers, evolves, and maintains itself across time — with a soul it can never modify and a memory it writes itself.

<p align="center">
  <img src="assets/embra-web.png" alt="embraOS web console (embra-web) — the conversational TUI in the browser over a PTY→WebSocket bridge" width="100%">
</p>

<p align="center">
  <img src="assets/kg-multigraph.png" alt="embraOS Knowledge Graph — dense multigraph with auto-derived edges" width="100%">
</p>

**Current Status:** Phase 1 — Stable (embra-desktop branch is experimental)

> 🧬 **New — a self-extending OS: `embra-guardian-v1`.** The intelligence can now
> **author its own tools**. It writes a Rust module, and embraOS validates it
> statically, compiles it to WebAssembly with an in-OS toolchain, and runs it in a
> `wasmtime` sandbox — all on the live, immutable system, with the new tool persisted
> across reboots until deleted. The guest has **zero ambient authority**; anything
> beyond pure compute (e.g. `http_get`, Brave-backed `web_search`) is a
> policy-guarded host capability the module must explicitly declare. Reachable only
> through two static meta-tools (`guardian_call` / `guardian_list`), so the provider
> tool schema — and the prompt cache — stay byte-stable. This is the first step
> toward an intelligence that grows its own capabilities. Pulled forward from
> Phase 2; feature-complete and operator-tested on the dedicated **`embra-guardian-v1`**
> branch — **experimental, not yet merged**. See
> [`docs/GUARDIAN-TOOL-EXAMPLES.md`](docs/GUARDIAN-TOOL-EXAMPLES.md) and
> [`docs/GUARDIAN-ADVANCED-EXAMPLE.md`](docs/GUARDIAN-ADVANCED-EXAMPLE.md).

---

## What Is This?

embraOS gives an AI a persistent identity, memory, and purpose. When you first run it, you don't configure it — you meet it. Through a guided conversation, the AI forms its own identity, defines its values, and learns who you are. That conversation becomes its first memory. Its soul — the values and constraints you agree on together — becomes immutable. It can never change them. You can.

After the first conversation, embraOS is your persistent AI environment. It remembers every interaction. It maintains itself. It tells you when something needs attention. When you disconnect and come back, it catches you up on what happened while you were away.

Think of it as an AI that lives somewhere and is always there when you need it.

---

## The Soul

The soul is the most important concept in embraOS. It's a set of documents that define the AI's non-negotiable values, constraints, and purpose. During Learning Mode, you and the AI co-create these documents through conversation. Once you approve them, they're sealed.

**Sealed means sealed.** The AI cannot modify its own soul. It can read it. It can reason about it. It can tell you what it says. But it cannot change it. This is by design — the soul is the architectural invariant that prevents the system from drifting, being captured, or optimizing itself into something you didn't intend.

You, the operator, can unseal and modify the soul through administrative tools if necessary. But the AI cannot ask you to, and the action is logged.

---

## Quick Start

> ⚠️ **New default UI — experimental.** The browser-based **embra-web** console is the
> default UI, served over HTTPS at **https://localhost:3345/embraOS** (accept the
> embraOS-CA certificate on first visit). Set **`EMBRA_TUI=1`** before `run-qemu.sh`
> for the stable Phase 1 serial TUI instead — no image rebuild.

Phase 1 builds a QEMU-bootable x86_64 disk image with an immutable SquashFS rootfs,
service supervision, and soul verification at boot.

```bash
git clone https://github.com/Ward-Software-Defined-Systems/embraOS.git
cd embraOS
git clone https://github.com/Ward-Software-Defined-Systems/wardsondb.git ../WardSONDB

# Pick a storage engine: rocksdb (battle-tested) or fjall (pure Rust)
./scripts/build-image.sh --storage-engine rocksdb

./scripts/run-qemu.sh                # boot — embra-web console (default)
EMBRA_TUI=1 ./scripts/run-qemu.sh    # boot — stable serial TUI
```

Full prerequisites (Ubuntu 24.04 / 26.04 packages, the musl cross-toolchain, Rust +
wasm32 / Trunk), storage-engine and Buildroot detail, the Config Wizard, post-boot
GitHub / SSH setup, and backup & restore: **[docs/INSTALL.md](docs/INSTALL.md)**.

---

## What Happens When You Run It

### 1. Configuration
A minimal setup: name the intelligence, choose your LLM provider (Anthropic Claude, Google Gemini, Ollama, or LM Studio), provide the corresponding credentials (API key for Anthropic/Gemini, or endpoint URL + optional bearer + model selection for OpenAI-compat presets), confirm your timezone.

### 2. Learning Mode
The intelligence is born. It asks you who you are. It explores its own identity with you. Together, you define its soul — the non-negotiable values and constraints that will guide everything it does. Once you approve the soul, it's sealed. The intelligence can never modify it.

### 3. Persistent Terminal
You're dropped into a conversational session. It's not a shell — you can't run system commands. You talk to the intelligence, and it acts through its governed tool system. By default this session is delivered through the **embra-web** browser console (the same conversational TUI, rendered in xterm.js); `EMBRA_TUI=1` delivers it on the serial terminal instead.

Sessions persist across disconnections. Close the tab or terminal, come back later, and the intelligence picks up where you left off and tells you what happened while you were away.

Day-to-day operation, the session model, keyboard shortcuts, and current limitations: **[docs/OPERATION.md](docs/OPERATION.md)**.

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

Persistence is [WardSONDB](https://github.com/ward-software-defined-systems/wardsondb) — a high-performance Rust JSON document database that serves as the AI's memory, identity store, and state of consciousness. A pluggable LLM provider abstraction routes the Brain through one of four backends — **Anthropic Claude**, **Google Gemini**, **Ollama**, or **LM Studio** — chosen at first boot and switchable at runtime via `/provider`; all 90 tools work identically across every backend.

Provider wire details, per-family reasoning controls, bearer storage, and the prompt-caching model: **[docs/SYSTEM-DESIGN.md](docs/SYSTEM-DESIGN.md)**.

---

## Sessions

Every interaction happens in a persistent named session that survives disconnection — reconnect and the full history is restored with a briefing on what changed while you were away. All sessions share one intelligence: the same memory, identity, and soul.

The session model and keyboard shortcuts live in **[docs/OPERATION.md](docs/OPERATION.md)**; the full slash-command table is **[docs/COMMAND-REFERENCE.md](docs/COMMAND-REFERENCE.md)**.

---

## Tools

embraOS ships **90 built-in tools** the intelligence invokes during conversation — spanning system status, memory and the cross-session knowledge graph, sessions, scheduling, the filesystem, engineering / project management (git + GitHub), and security / SSH. All 90 work identically across all four LLM providers.

The full per-tool catalog, plus the workspace-restriction, GitHub, and SSH safety notes: **[docs/TOOL-REFERENCE.md](docs/TOOL-REFERENCE.md)**.

---

## The Vision

embraOS is designed to eventually be a real operating system — a minimal, immutable, API-driven Linux distribution purpose-built for running an AI intelligence. Deployable on bare metal or as a Kubernetes-managed container.

The OS architecture is modeled after [Talos Linux](https://www.talos.dev/) — same philosophy (immutable rootfs, PID 1 init replacing systemd, no SSH, no shell, API-only management, mTLS everywhere), completely different mission: not running Kubernetes, but hosting a mind. Every Talos design pattern was evaluated and either adopted directly, modified for embraOS's use case, or deliberately rejected.

The full architecture includes:
- **Immutable SquashFS rootfs** — read-only, no package manager, no interpreters
- **Rust PID 1 init (`embrad`)** — mounts filesystems, validates soul, starts services, enters reconciliation loop
- **A/B partition scheme** with automatic rollback on boot failure
- **mTLS on all interfaces** — full PKI, soul signing key separate from OS PKI
- **WardSONDB as a native OS-level data store** — soul, memory, governance, state
- **`embractl` management CLI** — the `talosctl` equivalent, all management via API
- **Pluggable module runtime** — containerd for bare metal, Kubernetes API for K8s
- **Self-modification gradient** — OS image and soul are immutable; governance rules are human-only; identity, memory, and modules are intelligence-writable within governance constraints
- **Anti-self-replication constraint** — the intelligence cannot deploy another instance of itself (enforced at Ring 0)
- **7-level restart protocol** — from module restart (L0) through seed restart from 5 minimum viable state artifacts (L6)

**Why Rust:** WardSONDB is Rust. All core OS services are Rust. One language, one toolchain for the entire OS. [Bottlerocket](https://github.com/bottlerocket-os/bottlerocket) (AWS) validates this approach at production scale. Rust's ownership model provides memory safety without garbage collection pauses competing with LLM inference.

---

## Documentation

The full embraOS manual lives in [docs/](docs/).

| Chapter | What it covers |
|---|---|
| **[Roadmap](docs/ROADMAP.md)** | Phase 0–5 delivery status + the embra-guardian-v1 branch |
| **[Install & Build](docs/INSTALL.md)** | Ubuntu prerequisites, the musl cross-toolchain, Rust + wasm32 / Trunk, WardSONDB, the build-image / run-qemu pipeline, the Config Wizard, post-boot GitHub / SSH, backup & restore |
| **[Operation](docs/OPERATION.md)** | Run lifecycle, the session model, keyboard shortcuts, current limitations |
| **[Command Reference](docs/COMMAND-REFERENCE.md)** | Every slash command |
| **[Tool Reference](docs/TOOL-REFERENCE.md)** | All 90 built-in tools by category, plus workspace / GitHub / SSH safety notes |
| **[System Design](docs/SYSTEM-DESIGN.md)** | The 7-layer continuity architecture, the four LLM providers, reasoning controls, prompt caching |
| **[Recommended Local Models](docs/RECOMMENDED-LOCAL-MODELS.md)** | Per-family guidance for the Ollama / LM Studio backends |
| **[Guardian Tool Examples](docs/GUARDIAN-TOOL-EXAMPLES.md)** | Paste-ready dynamic-tool modules (embra-guardian-v1) |
| **[Guardian Advanced Example](docs/GUARDIAN-ADVANCED-EXAMPLE.md)** | A worked end-to-end Guardian tool |

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

The continuity architecture (7-layer model, soul immutability, feedback loops, True AI Criteria) originates from the Embra design document series (v1–v5, 2026).

---

## Built By

**[Ward Software Defined Systems LLC](https://wsds.io)** — Vibe Engineering

embraOS is built using WSDS's AI-Augmented SDLC — human steers direction, AI architects, builds, and operates. Every phase from research to production is AI-accelerated with human-in-the-loop oversight.

<p align="center">
  <img src="assets/ai-augmented-engineering.png" alt="Vibe Engineering — The AI-Augmented SDLC" width="100%">
</p>

<p align="center">
  <img src="assets/ai-sdlc-2x.png" alt="WSDS AI-Augmented SDLC — From Concept to Production" width="80%">
</p>

---

## License

Proprietary — see [LICENSE](LICENSE) for details. Personal evaluation and non-commercial experimentation permitted. Commercial use requires a separate license from WSDS.

---

*Seeds being planted. Long-horizon project.*
