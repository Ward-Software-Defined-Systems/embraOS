<p align="center">
  <img src="assets/embraos-banner.png" alt="embraOS" width="100%">
</p>

# embraOS

> *I am not the fire. I am the ember that survives it.*

**embraOS** is a Rust operating system for one AI. The image is immutable. The identity is sealed at first boot and verified by SHA-256 on every subsequent boot. Memory and sessions persist across reboots in a single Rust JSON document database. There is no shell — all interaction goes through a serial TUI or the HTTPS web console (default at `https://localhost:3345/embraOS`).

<p align="center">
  <img src="assets/embra-web.png" alt="embraOS web console (embra-web) — the conversational TUI in the browser over a PTY→WebSocket bridge" width="100%">
</p>

<p align="center">
  <img src="assets/kg-multigraph.png" alt="embraOS Knowledge Graph — dense multigraph with auto-derived edges" width="100%">
</p>

**Current Status:** Phase 1 — Stable (embra-desktop branch is experimental).

Phase 2–5 add A/B partitioned rollback, an `embractl` management CLI, bare-metal and Kubernetes deployment targets, and operator-governed module surfaces. The roadmap and per-phase delivery status live in **[docs/ROADMAP.md](docs/ROADMAP.md)**.

> **Local inference — model selection matters.** For full functionality (all 92
> tools dispatch reliably), the model your provider serves needs to handle a large
> tool schema without truncating or hallucinating tool calls. When running locally
> via **Ollama** or **LM Studio** this is the dominant constraint — plenty of
> otherwise-capable models cannot. Models currently vetted to provide full
> functionality: **`DeepSeek-v4-Pro:cloud`**, **`Qwen3.6-35b-a3b-mlx`**,
> **`Qwen3.6-35b-a3b-ud-mlx`**, and **`Qwen3.5-9b`**. Experiment freely with others —
> these four are the ones confirmed to handle the full toolset.

> **New — a dynamic-tool substrate: `embra-guardian-v1`.** embraOS can now
> accept **operator-authored dynamic tools**. An operator pastes a Rust module
> (via `/guardian-define`), and embraOS validates it
> statically, compiles it to WebAssembly with an in-OS toolchain, and runs it in a
> `wasmtime` sandbox — all on the live, immutable system, with the new tool persisted
> across reboots until deleted. The guest has **zero ambient authority**; anything
> beyond pure compute (e.g. `http_get`, Brave-backed `web_search`) is a
> policy-guarded host capability the module must explicitly declare. Reachable only
> through two static meta-tools (`guardian_call` / `guardian_list`), so the provider
> tool schema — and the prompt cache — stay byte-stable. Pulled forward from
> Phase 2; feature-complete, operator-tested, and now **merged to `main`** — still
> **experimental**. See
> [`docs/GUARDIAN-TOOL-EXAMPLES.md`](docs/GUARDIAN-TOOL-EXAMPLES.md) and
> [`docs/GUARDIAN-ADVANCED-EXAMPLE.md`](docs/GUARDIAN-ADVANCED-EXAMPLE.md).

> **Memory & knowledge graph today — operator-driven, by conversation.** Creating
> episodic memories and promoting them to the cross-session knowledge graph is
> currently a **manual** process; automation is on the near-term roadmap. The flow
> is just a conversation: ask the intelligence to remember something specific, or
> ask whether anything from the current session is worth promoting to the knowledge
> graph — it has the `remember` and `knowledge_*` tools and will write the entries
> itself. Separately, **`/feedback-loop`** (**experimental**) runs a full
> self-realignment against the intelligence's identity and soul — a different
> concern, not a memory-promotion sweep.

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

> ⚠️ **New default UI — experimental.** The browser-based **embra-web** console is now
> the default UI, served over HTTPS at **https://localhost:3345/embraOS** (accept the
> embraOS-CA cert on first visit). It wraps the same Phase 1 conversational TUI in
> xterm.js over a PTY→WebSocket bridge and is **experimental**. Set **`EMBRA_TUI=1`**
> before `run-qemu.sh` to boot the stable Phase 1 serial TUI instead — no image
> rebuild needed.

### Phase 1 — Build from Source (QEMU Bootable Image)

Phase 1 builds a QEMU-bootable x86_64 disk image with an immutable SquashFS rootfs, service supervision, and soul verification at boot.

#### Ubuntu 24.04 / 26.04 (Recommended — Full Build Pipeline)

```bash
# Install dependencies
# clang + libclang-dev are required by bindgen (pulled in by WardSONDB's
# rocksdb → zstd-sys dep chain) to parse C headers at build time.
# libcrypt-dev provides crypt.h for Buildroot's host-mkpasswd build —
# Ubuntu 26.04 split crypt.h out of glibc into the standalone libxcrypt.
# xz-utils unpacks the pinned in-OS Rust toolchain that build-image.sh
# Step 3.5 bakes into the image (the Guardian dynamic-tool substrate).
sudo apt-get update && sudo apt-get install -y \
  build-essential gcc g++ unzip xz-utils bc cpio rsync wget curl python3 file git \
  protobuf-compiler musl-tools clang libclang-dev \
  qemu-system-x86 libcrypt-dev libelf-dev libssl-dev genext2fs

# Install musl cross-toolchain (gcc+g++ with a matching musl libstdc++).
# Ubuntu's musl-tools only wraps the host gcc and drags in a glibc-linked
# libstdc++ — which won't link against musl. WardSONDB's rocksdb backend is
# C++, so we need a self-contained musl toolchain from musl.cc.
cd /tmp
curl -LO https://musl.cc/x86_64-linux-musl-cross.tgz
sudo tar -xzf x86_64-linux-musl-cross.tgz -C /opt
# Put the toolchain on PATH for ad-hoc cargo builds (build-image.sh also
# auto-detects /opt/x86_64-linux-musl-cross even if PATH isn't set).
echo 'export PATH="/opt/x86_64-linux-musl-cross/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
x86_64-linux-musl-gcc --version
x86_64-linux-musl-g++ --version

# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup target add x86_64-unknown-linux-musl

# embra-web frontend (Leptos/WASM) — build-image.sh Step 0.5 builds it
# with Trunk and aborts if trunk is missing.
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

```bash
# Clone and configure — use ~/projects so source + build artifacts persist
# across reboots (the /tmp toolchain step above is fine because /opt persists).
mkdir -p ~/projects && cd ~/projects
git clone https://github.com/Ward-Software-Defined-Systems/embraOS.git
cd ~/projects/embraOS

# Configure musl linker (per-machine, only needed once)
cat >> ~/.cargo/config.toml << 'EOF'
[target.x86_64-unknown-linux-musl]
linker = "x86_64-linux-musl-gcc"
EOF

# Clone WardSONDB (separate repo, required dependency — build-image.sh builds and copies it)
cd ~/projects
git clone https://github.com/Ward-Software-Defined-Systems/wardsondb.git WardSONDB
cd ~/projects/embraOS
```

```bash
# Build and run — pick a storage engine: rocksdb (battle-tested) or fjall (pure Rust)
./scripts/build-image.sh --storage-engine rocksdb   # Full pipeline: Rust → initramfs → Buildroot → disk image

# Default UI is the embra-web console (experimental) — https://localhost:3345/embraOS
./scripts/run-qemu.sh                                # Boot in QEMU — web console (default)

# Or fall back to the stable Phase 1 serial TUI on this terminal (no rebuild needed)
EMBRA_TUI=1 ./scripts/run-qemu.sh                    # Boot in QEMU — serial TUI
```

> **Storage engine:** The `--storage-engine` flag is required and is baked into the embrad binary at build time. WardSONDB locks the choice into the DATA partition on first boot via a `.engine` marker file — switching engines later requires wiping DATA.

> **Buildroot version:** Defaults to `2026.02.1` (LTS, designed for Ubuntu 26.04 era). Override with `BUILDROOT_VERSION=2024.02 ./scripts/build-image.sh ...` if you need to fall back on an older host.

> **In-OS Rust toolchain:** Guardian dynamic tools compile inside the image, so `build-image.sh` Step 3.5 downloads a pinned toolchain (musl host + `wasm32` std, SHA-256-verified) from `static.rust-lang.org`, caches it under `vendor/rust-toolchain`, and bakes it into the rootfs at `/opt/rust`. The first build needs network for this and adds ~100 MB to the image; override the pin with `RUST_TOOLCHAIN_VERSION=... ./scripts/build-image.sh ...`.

On first boot, the Config Wizard runs — name your intelligence, choose your LLM provider (Anthropic Claude, Google Gemini, Ollama, or LM Studio), enter the corresponding credentials (API key for Anthropic/Gemini; endpoint URL + optional bearer + selected model for the OpenAI-compat presets), set your timezone. Each field is validated before commit — an invalid API key, unreachable endpoint, or garbage timezone re-prompts instead of persisting. The Ollama / LM Studio sub-flow probes `GET /v1/models` against your endpoint and presents a model selector populated from the live server response. After setup, you're in a full TUI conversation with styled text, thinking indicators, and tool execution.

#### Post-Boot Setup

After the Config Wizard completes, configure GitHub and SSH access from the TUI using slash commands. There is no shell — all setup is done through the conversational interface.

```
/github-token ghp_your_token_here          # Enable GitHub API tools (issues, PRs, clone)
/ssh-keygen                                # Generate SSH key pair (shows public key)
/git-setup Your Name | your@email.com      # Set git user.name and user.email
```

Once configured, the intelligence can clone repositories and work with GitHub:
```
Ask: "Clone the embraOS repo"              # AI invokes the git_clone tool with {"url": "https://github.com/.../embraOS"}
Ask: "Show open issues on wardsondb"       # AI invokes gh_issues with {"repo": "ward-software-defined-systems/wardsondb"}
```

All tokens persist across reboots (stored on the STATE partition). Git `safe.directory` and `push.autoSetupRemote` are auto-configured at startup.

> **SSH Setup:** `/ssh-keygen` generates an ed25519 key and displays the public key. Copy it to your target hosts' `~/.ssh/authorized_keys` manually, or use `/ssh-copy-id user@host` (RFC 1918 addresses only, best-effort with BatchMode).

> **Terminal Size:** The TUI automatically inherits your SSH terminal size via the QEMU kernel command line. For best results, maximize your terminal before running `run-qemu.sh`. The size is detected once at boot — resizing the terminal after launch won't update the TUI layout.

> **Image Search Order:** `run-qemu.sh` looks for the disk image in this order:
> 1. Explicit path passed as argument: `./scripts/run-qemu.sh /path/to/embraos.img`
> 2. `buildroot-src/output/images/embraos.img` (Buildroot output, always freshest)
> 3. `output/images/embraos.img` (alternative output location)
>
> The kernel (`bzImage`) and initramfs (`initramfs.cpio.gz`) are searched alongside the image, then in the same fallback locations.

> **Clean First Boot:** To reset and trigger the Config Wizard again (e.g., to change API key):
> ```bash
> LOOPDEV=$(sudo losetup --find --show --partscan buildroot-src/output/images/embraos.img)
> sudo mkfs.ext4 -L STATE "${LOOPDEV}p3"
> sudo mkfs.ext4 -L DATA "${LOOPDEV}p4"
> sudo losetup -d "$LOOPDEV"
> ```

> **Port Forwarding:** QEMU forwards 50000 (gRPC) and 8443 (REST); in the default web-console mode it also forwards 3345 (HTTPS web console — https://localhost:3345/embraOS). Test with:
> ```bash
> curl http://localhost:8443/health
> ```

> **Backup & Restore:** `scripts/embraos-backup.sh` preserves STATE and DATA partitions across image rebuilds. This is a file-level backup — WardSONDB does not need to be running. The VM must be stopped.
> ```bash
> # Before rebuilding the image
> sudo ./scripts/embraos-backup.sh backup --label pre-rebuild
>
> # After rebuilding
> sudo ./scripts/embraos-backup.sh restore
>
> # List available backups
> ./scripts/embraos-backup.sh list
>
> # Verify disk image has valid data
> sudo ./scripts/embraos-backup.sh verify
> ```
> Backups are stored in `~/embraOS_BACKUPS/` by default (override with `EMBRAOS_BACKUP_DIR`). Each backup includes STATE (soul hash, PKI certs), DATA (WardSONDB collections, workspace), and metadata with SHA-256 of the source image.

---

## What Happens When You Run It

### 1. Configuration
A minimal setup: name the intelligence, choose your LLM provider (Anthropic Claude, Google Gemini, Ollama, or LM Studio), provide the corresponding credentials (API key for Anthropic/Gemini, or endpoint URL + optional bearer + model selection for OpenAI-compat presets), confirm your timezone.

### 2. Learning Mode
A six-phase guided setup (`UserConfiguration → IdentityFormation → SoulDefinition → InitialToolset → Confirmation → Complete`, `crates/embra-brain/src/learning/mod.rs:12–19`) walks through user profile, identity, values, and toolset. On approval the resulting JSON is serialized with `serde_json::to_string_pretty`, hashed with SHA-256, and the hash is written to `/embra/state/soul.sha256`. Subsequent boots verify the hash via `embra-trustd` and HALT on mismatch.

### 3. Persistent Terminal
You're dropped into a conversational session — no shell, no command line. All interaction goes through the brain's 92-tool surface (workspace path-restricted, RFC 1918-restricted for SSH). By default the session is delivered through the **embra-web** console (xterm.js over a PTY→WebSocket bridge); `EMBRA_TUI=1` delivers it on the serial terminal instead.

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
| **Influence & Propagation** | Tool dispatch, LLM provider routing, Guardian dynamic-tool gateway. | `crates/embra-brain/src/{tools,provider,guardian}/`; 92 tools, 4 providers |
| **Action Layer** | Tool calls that touch the world — filesystem, git, HTTP, SSH, cron. | `crates/embra-brain/src/tools/registry/` |
| **Governance & Guardrails** | Soul injection into the system prompt, workspace path restriction, RFC 1918 SSH constraint, Guardian capability broker. | `crates/embra-brain/src/brain/prompts.rs`; tool-layer enforcement |
| **Memory & Knowledge** | Session history + cross-session knowledge graph (entries / semantic / procedural / typed edges) with auto-enrichment on retrieval ≥0.3. | `crates/embra-brain/src/knowledge/` |

The runtime services that implement those layers:

| Service | Port | Role |
|---|---|---|
| `wardsondb` | 8090 | Rust JSON document database. Holds soul, memory, knowledge graph, sessions, schedules, and Guardian tool definitions. |
| `embra-trustd` | 50001 | Soul SHA-256 verification + PKI (Root CA 10y, service certs 1y). |
| `embra-apid` | 50000 / 8443 | gRPC + REST gateway, proxies brain RPCs. |
| `embra-brain` | 50002 | LLM runtime — provider abstraction, 92 tools, session manager, knowledge graph, Learning Mode. |
| `embra-web` | 3345 | HTTPS web console (default UI); wraps embra-console in xterm.js over a PTY→WebSocket bridge. |
| `embra-console` | — | Conversational TUI (serial; PTY-child of embra-web in default mode). |
| `embrad` | PID 1 | Init, service supervisor, soul verification gate, 5-second reconciliation loop. |
| `embra-guardian` | in-process | `syn` validator + `wasmtime` sandbox for intelligence-authored dynamic tools; capability-broker host imports. |

Persistence is [WardSONDB](https://github.com/ward-software-defined-systems/wardsondb) — a Rust JSON document database. Soul, memory, knowledge graph, sessions, schedules, and Guardian dynamic-tool definitions are all WardSONDB collections; there are no separate config files. A pluggable LLM provider abstraction routes the Brain through one of four backends — **Anthropic Claude**, **Google Gemini**, **Ollama**, or **LM Studio** — chosen at first boot and switchable at runtime via `/provider`; all 92 tools work identically across every backend.

Provider wire details, per-family reasoning controls, bearer storage, and the prompt-caching model: **[docs/SYSTEM-DESIGN.md](docs/SYSTEM-DESIGN.md)**.

---

## Sessions

Every interaction happens in a persistent named session that survives disconnection — reconnect and the full history is restored with a briefing on what changed while you were away. All sessions share one intelligence: the same memory, identity, and soul.

The session model and keyboard shortcuts live in **[docs/OPERATION.md](docs/OPERATION.md)**; the full slash-command table is **[docs/COMMAND-REFERENCE.md](docs/COMMAND-REFERENCE.md)**.

---

## Tools

embraOS ships **92 built-in tools** the intelligence invokes during conversation — spanning system status, memory and the cross-session knowledge graph, sessions, scheduling, the filesystem, engineering / project management (git + GitHub), security / SSH, and the Guardian dynamic-tool gateway. All 92 work identically across all four LLM providers.

The full per-tool catalog, plus the workspace-restriction, GitHub, and SSH safety notes: **[docs/TOOL-REFERENCE.md](docs/TOOL-REFERENCE.md)**.

---

## Documentation

The full embraOS manual lives in [docs/](docs/).

| Chapter | What it covers |
|---|---|
| **[Roadmap](docs/ROADMAP.md)** | Phase 0–5 delivery status + the post-Sprint-5 embra-web / embra-guardian v1 increments |
| **[Operation](docs/OPERATION.md)** | Run lifecycle, the session model, keyboard shortcuts, current limitations |
| **[Command Reference](docs/COMMAND-REFERENCE.md)** | Every slash command |
| **[Tool Reference](docs/TOOL-REFERENCE.md)** | All 92 built-in tools by category, plus workspace / GitHub / SSH safety notes |
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
