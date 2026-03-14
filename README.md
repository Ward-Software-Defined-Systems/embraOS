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

```bash
docker run -it -e ANTHROPIC_API_KEY=sk-ant-... wardsdsllc/embraos:v0.1.0-phase0
```

That's it. You'll be guided through naming the intelligence, forming its identity, and defining its soul. After that, you're in a persistent terminal session.

### With Persistence

```bash
docker run -it \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -v embra-data:/embra/data \
  wardsdsllc/embraos:v0.1.0-phase0
```

Add a Docker volume and your AI's memory, identity, and soul survive container restarts.

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

See [ARCHITECTURE.md](ARCHITECTURE.md) for the full technical reference.

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

---

## Phase 0 Limitations

This is a proof of concept. It demonstrates the core experience but doesn't include the full OS:

- **API only** — requires internet connectivity and an Anthropic API key
- **Single model** — Claude Opus 4.6, not configurable
- **Docker only** — not a bootable OS (yet)
- **Built-in tools only** — no MCP server modules (yet)
- **No local LLM** — coming in a future phase

---

## Roadmap

| Phase | Description | Status |
|---|---|---|
| **Phase 0** | Proof of concept — Docker container, Anthropic API, core UX | **Current** |
| **Phase 1** | Core OS — embrad (Rust PID 1), embra-apid, immutable rootfs | Planned |
| **Phase 2** | Terminal & Sessions — full TUI, multi-session, embractl CLI | Planned |
| **Phase 3** | Module System — MCP servers, embra-guardian, containerd | Planned |
| **Phase 4** | Image Factory — ISO builds, bare metal deployment | Planned |
| **Phase 5** | Local LLM — offline operation, sovereign intelligence | Planned |

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

## Built By

**Ward Software Defined Systems LLC** — AI-Augmented SDLC

- Phase 1 (Research & Design): Claude.ai
- Phase 2 (Implementation): Claude Code
- Phase 3 (Testing & Ops): Axiom, OpenClaw, Claude Opus

---

## License

Proprietary — see [LICENSE](LICENSE) for details. Personal evaluation and non-commercial experimentation permitted. Commercial use requires a separate license from WSDS.

---

*Seeds being planted. Long-horizon project.*
