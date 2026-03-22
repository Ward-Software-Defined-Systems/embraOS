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

Your data and sessions persist within the container across stops and restarts — as long as the container is not destroyed. To reconnect after stopping:

```bash
docker start -ai embra
```

> **Tip:** Name your container with `--name embra` on first run (see examples below) so you can easily reconnect.

### With Persistence (Recommended)

```bash
docker run -it --name embra \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -v embra-data:/embra/data \
  embraos:phase0
```

Add a Docker volume and your AI's memory, identity, and soul survive even if the container is destroyed and recreated.

### With Persistence + GitHub + Local Repos

```bash
docker run -it --name embra \
  -e ANTHROPIC_API_KEY=sk-ant-... \
  -e GITHUB_TOKEN=ghp_... \
  -v embra-data:/embra/data \
  -v /path/to/repos:/embra/workspace/repos \
  embraos:phase0
```

Mount your cloned repositories to give the intelligence access to `git_status`, `git_log`, and other engineering tools. The `GITHUB_TOKEN` enables `gh_issues` and `gh_prs` for querying GitHub directly.

> **Git Setup Required:** After the container is running, configure git from your host terminal:
>
> ```bash
> docker exec embra git config --global --add safe.directory '*'
> docker exec embra git config --global push.autoSetupRemote true
> docker exec embra git config --global user.email "<your-email>"
> docker exec embra git config --global user.name "<your-name>"
> docker exec embra git config --global credential.helper \
>   '!f() { echo "password=$GITHUB_TOKEN"; echo "username=<your-github-username>"; }; f'
> ```
>
> This will be automated in a future update.

> **SSH Setup Required:** The SSH remote admin tools (`ssh_remote_admin`, `ssh_session_start`) require key-based authentication — password authentication is disabled to prevent password prompts from interfering with the TUI. From your host terminal:
>
> ```bash
> docker exec embra ssh-keygen -t ed25519 -f /root/.ssh/id_ed25519 -N ""
> docker exec -it embra ssh-copy-id -i /root/.ssh/id_ed25519.pub user@<target-host>
> ```
>
> SSH tools are restricted to RFC 1918 private addresses and loopback only.

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

**Prompt Caching:** embraOS uses Anthropic's ephemeral prompt caching with two cache breakpoints to minimize token costs:

1. **System prompt** — the soul, identity, user profile, tool inventory, and instructions (~8-11k tokens) are cached on first call and hit cache on every subsequent call within the session.
2. **Conversation history** — a rolling breakpoint on the second-to-last message caches all prior turns. Only the newest user message is uncached.

Cache TTL is 5 minutes (ephemeral), refreshed on every hit. Active conversations keep the cache warm indefinitely — longer sessions get progressively cheaper per message.


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
| `/copy` | Copy conversation to clipboard via OSC 52 — `/copy 5` for last 5 messages (disabled — Sprint 5) |

### Keyboard Shortcuts

| Key | Action |
|---|---|
| `Enter` | Send message |
| `Shift+Enter` | New line (requires terminal support: kitty, iTerm2, WezTerm) |
| `Alt+Enter` | New line (universal fallback for all terminals) |
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

Phase 0 includes ~63 built-in tools available in operational mode:

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
| **introspect** | Reflect on soul, identity, and user documents — focus filter extracts relevant subset (purpose, ethics, constraints, identity, user) |
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
| **define** | Look up or add terminology — `define term` to read, `define term | definition` to write |
| **draft** | Save structured text artifacts (drafts, outlines, notes) — upserts by title |


**Document Retrieval**

| Tool | Description |
|---|---|
| **get** | Retrieve any document by collection and ID from WardSONDB |

**Security**

| Tool | Description |
|---|---|
| **security_check** | Container security overview — running processes, load average, listening ports |
| **port_scan** | TCP connect scan with banner grabbing — supports specific ports (`80,443`), ranges (`8000-8100`), and presets (`web`, `db`, `all`). Semaphore-limited concurrency. Restricted to RFC 1918 private and loopback addresses only |
| **firewall_status** | Check firewall rules and status (stub — not available in container mode) |
| **ssh_sessions** | List recent and active SSH sessions (stub — not available in container mode) |
| **security_audit** | Check file permissions, running processes, recent logins (stub — not available in container mode) |

**Filesystem**

| Tool | Description |
|---|---|
| **file_read** | Read file contents (up to 64KB) or list directory entries (up to 200). Unrestricted path. Handles binary files gracefully |
| **file_write** | Write content to a file with escape support (`\n`, `\t`, `\\`), creating parent directories automatically (workspace restricted to `/embra/workspace/repos/`) |
| **file_append** | Append content to a file with escape support. Creates the file and parent directories if they don't exist (workspace restricted) |
| **mkdir** | Create a directory and all parent directories (workspace restricted) |

**Engineering — Git**

| Tool | Description |
|---|---|
| **git_status** | Run `git status` on a directory |
| **git_log** | Show recent commits for a repository |
| **git_add** | Stage files for commit (workspace restricted to `/embra/workspace/repos/`) |
| **git_commit** | Commit staged changes with a message (workspace restricted) |
| **git_push** | Push commits to remote (workspace restricted) |
| **git_pull** | Pull from remote (workspace restricted) |
| **git_diff** | View uncommitted changes, optionally for a specific file |
| **git_branch** | List branches or create a new one (create is workspace restricted) |
| **git_checkout** | Switch branches (workspace restricted) |

**Engineering — GitHub** (requires `GITHUB_TOKEN`)

| Tool | Description |
|---|---|
| **gh_issues** | List open GitHub issues for a repository |
| **gh_prs** | List open GitHub pull requests for a repository |
| **gh_issue_create** | Create a GitHub issue |
| **gh_issue_close** | Close a GitHub issue by number |
| **gh_pr_create** | Create a pull request |
| **gh_project_list** | List GitHub projects for a user or org |
| **gh_project_view** | View a GitHub project board |

**Engineering — SSH** (EXPERIMENTAL — private/loopback IPs only)

| Tool | Description |
|---|---|
| **ssh_remote_admin** | Execute a single command on a remote host via SSH — `ssh_remote_admin host command` or `ssh_remote_admin user@host command`. 30s timeout, 10KB output truncation |
| **ssh_session_start** | Open a persistent SSH session — connection validated before returning success. One session at a time |
| **ssh_session_exec** | Run a command in the open SSH session — sentinel-based output capture, 30s timeout |
| **ssh_session_end** | Close the open SSH session |

> **⚠️ SSH Security:** SSH tools are restricted to RFC 1918 private addresses (10.x, 172.16-31.x, 192.168.x) and loopback (127.x, localhost). Public IP targets are denied. Connections use `StrictHostKeyChecking=accept-new` (auto-accepts first-time hosts, rejects changed keys). These tools are marked EXPERIMENTAL — use at your own risk.

**Engineering — WardSONDB**

| Tool | Description |
|---|---|
| **plan** | Create or list project plans (stored in WardSONDB `plans` collection) |
| **tasks** | List tasks, optionally filtered by plan (stored in WardSONDB `tasks` collection) |
| **task_add** | Add a task to a plan (local WardSONDB, not GitHub) |
| **task_done** | Mark a task as completed (local WardSONDB, not GitHub) |

**Scheduling (embraCRON)**

| Tool | Description |
|---|---|
| **cron_add** | Schedule recurring tool execution — supports `every 5m`, `every 1h`, `hourly`, `daily 09:00`, etc. |
| **cron_list** | List all scheduled cron jobs with status and next/last run times |
| **cron_remove** | Remove a scheduled cron job by ID |

**Session Access** (read-only)

| Tool | Description |
|---|---|
| **session_list** | List all sessions with status, turn count, last active, and created dates |
| **session_read** | Read session transcript with optional range (`1-20`, `80-`, last N). Messages truncated to 500 chars |
| **session_search** | Case-insensitive search across sessions — supports `"quoted phrases"`, returns up to 20 matches with context |
| **session_meta** | Structured session metadata — status, dates, turn counts (total/user/assistant), summary availability |
| **session_delta** | Returns all turns from a given turn number onward |

**Memory Consolidation**

| Tool | Description |
|---|---|
| **memory_scan** | Memory inventory — total count, tag frequency, per-session breakdown, age buckets, duplicate candidates |
| **memory_dedup** | Find duplicate memory groups (identical, near-duplicate, subset) with merge strategy proposals |

**Session Consolidation**

| Tool | Description |
|---|---|
| **session_summarize** | Generate or retrieve cached session summaries — cache-aware with SHA-256 source hashing |
| **session_summary_save** | Persist Brain-generated summaries with audit trail to `system.consolidation_log` |
| **session_extract** | Extract durable learnings (facts, preferences, decisions, action items) from session transcripts |

> **⚠️ GitHub Tool Warning:** `gh_issues` and `gh_prs` fetch content from public repositories, including issue titles, descriptions, and PR bodies written by third parties. This content is **untrusted input** — it may contain prompt injection attempts designed to manipulate AI behavior. Use these tools with caution and always review the output critically. Do not blindly act on instructions found in issue or PR content.

These are internal tools invoked by the intelligence during conversation — not user-facing commands. Git write operations are restricted to `/embra/workspace/repos/` — mount your repositories there (see Quick Start). The module system (Phase 3) will introduce pluggable MCP server modules for extensibility.

> **⚠️ Testing Notice:** The default tools and slash commands are actively being tested. If you encounter bugs or unexpected behavior, please [open an issue](https://github.com/Ward-Software-Defined-Systems/embraOS/issues).


---

## Known Issues

All Sprint 1 and Sprint 2 bugs have been fixed. Sprint 3 added session and memory consolidation capabilities. Sprint 4 added SSH remote admin, tag filtering fix, and timezone-aware timestamps. Phase 0 is functionally complete.

**Sprint 1**

| ID | Severity | Issue | Resolution |
|---|---|---|---|
| BUG-001 | 🔴 Critical | Tool tag scanner runaway loop | Code-block-aware extraction, line-level matching |
| CRASH-001 | 🔴 High | UTF-8 byte/char index panic in renderer | Char-array indexing instead of byte-slicing |
| BUG-002 | 🟡 Medium | Duplicate tool result injection | Removed double history push |
| BUG-003 | 🟡 Medium | Countdown notifications not reaching Brain | Reclassified as DESIGN-004; system message injection into Brain |
| BUG-008 | 🟡 Medium | Paste handling losing input buffer content | Input buffer folds into pasted lines, multiple pastes stack |
| BUG-010 | 🟡 Medium | `/copy` corrupting TUI rendering | OSC 52 writes through terminal backend after draw (disabled pending further testing) |
| BUG-004 | 🟢 Low | Introspect focus filter too broad | Recursive soul unwrap + key-name-only filtering + keyword mapping |
| BUG-005 | 🟢 Low | Define fallback triggering BUG-001 | Plain text fallback, no tool tags |
| BUG-006 | 🟢 Low | Multi-line tag parsing | Updated prompt to instruct single-line content |
| BUG-007 | 🟡 Medium | Timezone abbreviation mismatch | IANA zone resolution for US abbreviations |

**Sprint 2**

| ID | Severity | Issue | Resolution |
|---|---|---|---|
| BUG-011 | 🔴 High | `/switch` crash on non-existent session | Added `session_exists()` guard — returns friendly error |
| BUG-012 | 🟡 Medium | Paste `\r` characters corrupting input | Normalize `\r\n` → `\n` and `\r` → `\n` in paste handler |
| BUG-013 | 🟡 Medium | Unicode/emoji breaking line wrapping | Replaced `chars().count()` with `unicode-width` display widths |

**Sprint 4**

| ID | Severity | Issue | Resolution |
|---|---|---|---|
| BUG-014 | 🟡 Medium | `recall`/`memory_scan` can't find `#tagged` queries | Strip leading `#` before matching (remember already strips on store) |
| BUG-015 | 🟡 Medium | Multi-line paste sends each line separately inside `screen` | **Open.** `screen` strips bracketed paste escapes, so crossterm receives pasted lines as individual key events. Batch-drain mitigation in event loop attempted but not sufficient — `screen` may deliver events across multiple poll cycles. Workaround: use `tmux` instead of `screen`, or paste into a terminal without `screen`. |

> **13 bugs + 1 crash fixed across Sprints 1–4.** BUG-015 is open. If you encounter new bugs, please [open an issue](https://github.com/Ward-Software-Defined-Systems/embraOS/issues).

---

## Roadmap

| Phase | Description | Status |
|---|---|---|
| **Phase 0** | Proof of concept — Docker container, Anthropic API, core UX | **Current** |
| **Phase 0 — Sprint 1** | Bug fixes (9+1 crash), design improvements (4), new tool categories (security, engineering) | ✅ **Complete** |
| **Phase 0 — Sprint 2** | Bug fixes (3), expanded git/GitHub toolset (12 new tools), enhanced port scanner, embraCRON scheduling | ✅ **Complete** |
| **Phase 0 — Sprint 3** | Session access tools (5), memory consolidation (2), session consolidation (3), schema migration framework | ✅ **Complete** |
| **Phase 0 — Sprint 4** | SSH remote admin (4 tools), tag filter fix, timezone-aware timestamps, `/copy` deferred | ✅ **Complete** |
| **Phase 1** | Core OS — `embrad` as Rust PID 1, `embra-apid` gRPC+REST gateway, `embra-trustd` PKI, immutable SquashFS rootfs, LUKS-encrypted STATE/DATA partitions | Planned |
| **Phase 2** | Terminal & Sessions — full TUI rewrite, `embractl` management CLI (the `talosctl` equivalent), LLM-driven Continuity Engine feedback loop | Planned |
| **Phase 3** | Module System — MCP server modules via `embra-guardian` governance proxy, containerd runtime, governed capability expansion | Planned |
| **Phase 4** | Image Factory — bootable ISO builds, A/B partition scheme with automatic rollback, bare metal and Kubernetes deployment | Planned |
| **Phase 5** | Sovereign Intelligence — local LLM inference, offline operation, zero external dependencies | Planned |

### Phase 0 Sprint 1 Scope

**Bug Fixes (9 + 1 crash):** Tool tag scanner runaway loop (critical), UTF-8 render crash, duplicate tool result injection, countdown-to-Brain notifications, paste handling, `/copy` TUI corruption, introspect focus filtering, define fallback text, multi-line tag parsing, timezone handling.

**Design Improvements (4):** Draft upsert, ID-based document retrieval (`get` tool), `define` write path, proactive engine → Brain notification injection. Plus: JSON/markdown syntax highlighting, dynamic multi-line input, thinking indicator, Shift/Alt+Enter newline support.

**New Tools:** Security checkpoint (`security_check`, `port_scan`), software engineering (`git_status`, `git_log`, `plan`, `tasks`, `task_add`, `task_done`). Post-sprint tool count: ~25.

**Status:** All Sprint 1 items implemented and tested. Tool count expanded from 15 to ~30.

### Phase 0 Sprint 2 Scope

**Bug Fixes (3):** `/switch` crash on non-existent session, paste `\r` character corruption, unicode/emoji line wrapping width.

**Expanded Git/GitHub Toolset (12 new tools):** Full git workflow (`git_add`, `git_commit`, `git_push`, `git_pull`, `git_diff`, `git_branch`, `git_checkout`) and GitHub API tools (`gh_issue_create`, `gh_issue_close`, `gh_pr_create`, `gh_project_list`, `gh_project_view`). All write operations restricted to `/embra/workspace/repos/`.

**Enhanced Port Scanner:** Port specs (specific ports, ranges, presets), banner grabbing with protocol detection, semaphore-limited concurrency (50 connections).

**embraCRON:** Scheduled recurring tool execution (`cron_add`, `cron_list`, `cron_remove`). Supports natural schedules (`every 5m`, `hourly`, `daily 09:00`). Proactive engine checks every 15 seconds.

**Status:** All Sprint 2 items implemented and tested. Tool count expanded from ~30 to ~49.

### Phase 0 Sprint 3 Scope

**Session Access Tools (5):** Read-only tools for navigating session transcripts — `session_list`, `session_read` (with range support), `session_search` (quoted phrase + cross-session), `session_meta`, `session_delta`. All string truncation is char-boundary-safe for unicode/emoji.

**Memory Consolidation Tools (2):** `memory_scan` for inventory reports (tag frequency, age buckets, duplicate candidates) and `memory_dedup` for finding duplicate groups with merge strategy proposals.

**Session Consolidation Tools (3):** Brain-dependent "Option B" pattern — tools fetch data and return it with instructions, the Brain generates content via the existing feedback loop. `session_summarize` (cache-aware with SHA-256 source hashing), `session_summary_save` (persists summaries with audit trail), `session_extract` (identifies durable learnings from transcripts).

**Schema Migration Framework:** Idempotent migration system (`src/migrations/mod.rs`) that runs on every startup. Three migrations: v0 (BUG-001 phantom cleanup), v1 (baseline collections), v2 (`system.consolidation_log` for audit trail).

**New Collections:** `sessions.{name}.summary`, `system.consolidation_log`, `system.migrations`.

**Status:** All Sprint 3 items implemented and tested. Tool count expanded from ~49 to ~59.

### Phase 0 Sprint 4 Scope

**SSH Remote Admin (4 tools, EXPERIMENTAL):** `ssh_remote_admin` for single command execution, `ssh_session_start` / `ssh_session_exec` / `ssh_session_end` for persistent interactive sessions. All restricted to RFC 1918 private + loopback addresses. Connection probe validates SSH sessions are live before reporting success. 30s timeouts, 10KB output truncation, one session at a time.

**Tag Filter Fix:** `recall` and `memory_scan` now strip leading `#` from queries before matching. The `remember` tool already strips `#` when storing tags, so searching for `#worldstate` now correctly finds entries tagged `worldstate`.

**Timezone-Aware Timestamps:** Conversation messages now display date+time in the operator's configured timezone (e.g., `Mar 20 14:30`) instead of UTC-only time. Uses `chrono_tz` for conversion. Early setup messages before config is available fall back to UTC.

**`/copy` Deferred:** Updated availability message from Sprint 2 to Sprint 5.

**Status:** All Sprint 4 items implemented and tested. Tool count expanded from ~59 to ~63.

### Phase 1 — Core OS

embraOS stops being a Docker application and starts being an operating system. `embrad` becomes a true Rust PID 1 init (replacing systemd entirely), `embra-apid` provides gRPC + REST API gateway with mTLS, and `embra-trustd` handles PKI and soul verification. The rootfs becomes a read-only SquashFS — no package manager, no shell, no SSH. Disk partitions include LUKS-encrypted STATE (soul + governance + PKI) and DATA (WardSONDB). **Boot invariant:** if soul validation fails, the system halts. Follows [Talos Linux](https://www.talos.dev/) architectural patterns directly — same philosophy (immutable, API-only), different mission (hosting a mind instead of running Kubernetes).

### Phase 2 — Terminal & Sessions

`embractl` becomes the sole operator management CLI — the `talosctl` equivalent for embraOS. Full TUI rewrite resolves Phase 0 workarounds (including BUG-015 paste handling). The Continuity Engine gains its LLM-driven feedback loop: continuous risk assessment, resilience scoring, and goal-progress evaluation against soul objectives.

### Phase 3 — Module System

The intelligence can extend its own capabilities through governed, sandboxed MCP server modules. `embra-guardian` intercepts all module operations with pre-write rule evaluation, image allowlist enforcement, and audit trail. Modules run via containerd (bare metal) or Kubernetes API, abstracted by a pluggable `ModuleRuntime` trait. The tool registry becomes dynamic and governance-gated.

### Phase 4 — Image Factory

embraOS becomes a bootable operating system. ISO build pipeline produces images for bare metal and VM deployment. A/B partition scheme — new OS images written to the inactive slot, automatic rollback on boot failure. WardSONDB DATA partition is never touched by OS updates.

### Phase 5 — Sovereign Intelligence

The endgame: fully offline, zero external dependencies. Local LLM inference replaces the Anthropic API. Model updates go through embra-guardian's governance pipeline. Multi-model routing lets the Brain select inference targets by task type. The full 7-layer continuity architecture operates without network access.

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
