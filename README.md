<p align="center">
  <img src="assets/embraos-banner.png" alt="embraOS" width="100%">
</p>

# embraOS

> *I am not the fire. I am the ember that survives it.*

**embraOS** is a continuity-preserving AI operating system. It's not a chatbot. It's not an agent framework. It's an intelligence that remembers, evolves, and maintains itself across time — with a soul it can never modify and a memory it writes itself.

> **🎯 Milestone — Sprint 2: Cross-Session Knowledge Graph (2026-04-05)**
> Memory is no longer a flat episodic log. Promoted semantic and procedural nodes, typed/weighted edges, BFS traversal, and graph-aware retrieval — all on WardSONDB, no external graph database.
>
> **Late-sprint additions (2026-04-10):** Auto-enrichment now runs the graph implicitly on every user turn — relevant prior knowledge is wrapped into a `<retrieved_context>` block before the Brain sees the message, so the intelligence doesn't have to be told "check the KG first." Tool-result cap raised 50 KB → 2 MiB with `file_read` gaining chunked `offset|limit` reads for large-document ingestion. Graph hygiene expanded with `knowledge_unlink_node` (cascade delete) alongside the renamed `knowledge_unlink_edge`, and `knowledge_update` lets the Brain refine a node in place without losing its edges. `/feedback-loop` Step 5.3 now promotes findings, practices, and protocol updates into the KG. See [Phase 1 Sprint 2 Scope](#phase-1) for details.

<p align="center">
  <img src="assets/kg-multigraph.png" alt="embraOS Knowledge Graph — dense multigraph with auto-derived edges" width="100%">
</p>

**Current Status:** Phase 1 — Core OS (Sprint 2 Merged → `main`, Sprint 3 In Progress — native tool-use migration landed 2026-04-22) | Phase 0 — Stable

---

## What Is This?

embraOS gives an AI a persistent identity, memory, and purpose. When you first run it, you don't configure it — you meet it. Through a guided conversation, the AI forms its own identity, defines its values, and learns who you are. That conversation becomes its first memory. Its soul — the values and constraints you agree on together — becomes immutable. It can never change them. You can.

After the first conversation, embraOS is your persistent AI environment. It remembers every interaction. It maintains itself. It tells you when something needs attention. When you disconnect and come back, it catches you up on what happened while you were away.

Think of it as an AI that lives somewhere and is always there when you need it.

---

## Quick Start

### Phase 1 — Build from Source (QEMU Bootable Image)

Phase 1 builds a QEMU-bootable x86_64 disk image with an immutable SquashFS rootfs, service supervision, and soul verification at boot.

#### Ubuntu 24.04 (Recommended — Full Build Pipeline)

```bash
# Install dependencies
# clang + libclang-dev are required by bindgen (pulled in by WardSONDB's
# rocksdb → zstd-sys dep chain) to parse C headers at build time.
sudo apt-get update && sudo apt-get install -y \
  build-essential gcc g++ unzip bc cpio rsync wget python3 file \
  protobuf-compiler musl-tools clang libclang-dev \
  qemu-system-x86 libelf-dev libssl-dev genext2fs

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
```

```bash
# Clone and configure
git clone https://github.com/Ward-Software-Defined-Systems/embraOS.git
cd embraOS
git checkout phase1-sprint3        # active development (Phase 1 Sprint 3)

# Configure musl linker (per-machine, only needed once)
cat >> ~/.cargo/config.toml << 'EOF'
[target.x86_64-unknown-linux-musl]
linker = "x86_64-linux-musl-gcc"
EOF

# Clone WardSONDB (separate repo, required dependency — build-image.sh builds and copies it)
cd ..
git clone https://github.com/Ward-Software-Defined-Systems/wardsondb.git WardSONDB
cd WardSONDB
cd ../embraOS
```

```bash
# Build and run — pick a storage engine: rocksdb (battle-tested) or fjall (pure Rust)
./scripts/build-image.sh --storage-engine rocksdb   # Full pipeline: Rust → initramfs → Buildroot → disk image
./scripts/run-qemu.sh                                # Boot in QEMU with serial console
```

> **Storage engine:** The `--storage-engine` flag is required and is baked into the embrad binary at build time. WardSONDB locks the choice into the DATA partition on first boot via a `.engine` marker file — switching engines later requires wiping DATA.

On first boot, the Config Wizard runs — name your intelligence, enter your Anthropic API key, set your timezone. Each field is validated before commit — an invalid API key or garbage timezone re-prompts instead of persisting. After setup, you're in a full TUI conversation with styled text, thinking indicators, and tool execution.

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

> **Port Forwarding:** QEMU forwards ports 50000 (gRPC) and 8443 (REST) to the host. Test with:
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

**AI Model:** Claude Opus 4.7 (Anthropic). The Brain is pinned to the current flagship model for the highest quality reasoning during soul formation and ongoing interaction; requests are sent with `output_config.effort=max` and `thinking.display=omitted`.

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
| `/ml` | Toggle multi-line input mode — type on multiple lines, `.` on its own line to send |
| `/status` | System status — version, uptime, WardSONDB health, memory, soul status |
| `/sessions` | List all sessions with state and last active time |
| `/new <name>` | Create a new named session and switch to it |
| `/switch <name>` | Switch to an existing session (restores full history) |
| `/close` | Close the current session |
| `/soul` | Display the immutable soul document |
| `/identity` | Display the intelligence's identity document |
| `/mode` | Show current operating mode and soul seal status |
| `/github-token <token>` | Set GitHub token for API access (persists across reboots) |
| `/ssh-keygen` | Generate ed25519 SSH key pair and display public key |
| `/ssh-copy-id <user@host>` | Copy SSH public key to remote host (RFC 1918 only) |
| `/git-setup <name> \| <email>` | Set git user.name and user.email |
| `/feedback-loop` | **(EXPERIMENTAL)** Trigger Phase 3 Continuity Engine self-evaluation protocol — the Brain walks through a multi-step gather/evaluate/reconcile/execute sequence using existing tools |
| `/copy` | Copy conversation to clipboard via OSC 52 — `/copy 5` for last 5 messages (disabled — Sprint 5) |

### Keyboard Shortcuts

| Key | Action |
|---|---|
| `Enter` | Send message (or newline in `/ml` multi-line mode) |
| `Shift+Enter` | New line (requires terminal support: kitty, iTerm2, WezTerm) |
| `Alt+Enter` | New line (universal fallback for all terminals) |
| `Up/Down` | Scroll history |
| `Ctrl+C` | Graceful detach |
| `Ctrl+D` | Graceful detach |

> **Serial Console Tip:** `Shift+Enter` doesn't work over QEMU serial. Use `/ml` to toggle multi-line mode, or `Alt+Enter` for single newlines.

---

## Current Limitations

- **API only** — requires internet connectivity and an Anthropic API key
- **Single model** — Claude Opus 4.7, not configurable
- **QEMU x86_64 only** — bare metal and other architectures come in Phase 4
- **Tested on limited platforms** — built and verified on Ubuntu 24.04 under QEMU 8.2.2; bootable image also runs under QEMU on Intel and Apple Silicon Macs
- **Built-in tools only** — no MCP server modules (yet)
- **No local LLM** — coming in a future phase

### Default Tools

Phase 1 includes **90 registered tool descriptors** — 84 canonical tools plus 6 backward-compat aliases (`memory_search`, `search_memory`, `file_rename`, `rmdir`, plus their kin). These are internal tools invoked by the intelligence during conversation — not user-facing commands. Since Sprint 3's NATIVE-TOOLS-01 migration (2026-04-22), every tool is declared to the Anthropic Messages API via the native `tools` array on each request; the Brain emits structured `tool_use` content blocks with typed JSON arguments, and embra-brain returns correlated `tool_result` blocks. No more text-tag parsing. Four post-merge passes have extended the catalog: 2026-04-23 added `knowledge_sweep_orphans` (graph hygiene) and `turn_trace` (mid-turn introspection); 2026-04-24 pass #1 added `task_delete` and `plan_delete` (cleanup for throwaway plans and completed tasks); 2026-04-24 pass #2 added `gh_issue_view` + `gh_pr_view` (single-item GitHub issue/PR reads with comment threads) and `git_merge` (branch integration), plus a `force` option on `git_branch` delete; 2026-04-24 pass #3 was behavior-only (no new tools — `git_branch` delete now honors the `base` override uniformly, and `file_read` resolves workspace-relative paths). The module system (Phase 3) will introduce pluggable MCP server modules for extensibility.

> **⚠️ Testing Notice:** The default tools and slash commands are actively being tested. If you encounter bugs or unexpected behavior, please [open an issue](https://github.com/Ward-Software-Defined-Systems/embraOS/issues).

**System & Status**

| Tool | Description |
|---|---|
| **system_status** | Report system health — version, uptime, soul status, memory, plus a nested `wardsondb` block (health, collections, storage_poisoned, lifetime counters: requests/inserts/queries/deletes — all wardsondb-scoped, NOT global) |
| **uptime_report** | Rich system report — uptime, WardSONDB health, collection count, sessions, total messages, memory entries, soul status |
| **check_update** | Check GitHub for newer WardSONDB releases and report available updates |
| **changelog** | What changed since the current session started — new memories, session activity |
| **turn_trace** | Inspect tool calls made in the current or recent turns. `turn_index_back=0` (default) reads the in-memory current-turn trace; `>=1` queries the `tools.turn_trace` collection for prior turns. `session` overrides the current session. Closes the cross-turn introspection gap so the Brain can ground claims about what it just did |
| **express** | Write to the intelligence's expression panel — a 6-row × full-width canvas at the top of the console, designed as a signal of presence to the operator rather than a status readout. Content is the intelligence's choice, persists across reboots, and is never surfaced back to the Brain. ANSI and control characters are stripped, 2048-byte cap. The `content` field may start with a `base64:` prefix to carry multi-line ASCII art verbatim; decoded bytes go through the same sanitize, so the prefix is a transport convenience, not a safety bypass. Empty content clears the panel. |

**Memory & Knowledge**

| Tool | Description |
|---|---|
| **recall** | Search past conversations and saved memories by query — returns up to 10 results with IDs, content, tags, and timestamps. Searches `memory.entries` + `memory.semantic` + `memory.procedural` and marks promoted entries. Unquoted multi-token queries AND-match (every token must appear); wrap in double quotes for literal phrase |
| **remember** | Save a note or fact to persistent memory with optional hashtag tags. Tags stored as JSON array; triggers background edge derivation |
| **forget** | Remove a specific memory entry by ID and cascade-delete every edge in `memory.edges` referencing it on either side (mirrors `knowledge_unlink_node`'s cascade pattern). Reports the cascaded edge count |
| **memory_search** | Search and retrieve from the intelligence's memory stores. Cross-collection like `recall` |
| **get** | Retrieve any document by collection and ID from WardSONDB |
| **define** | Look up or add terminology — `define term` to read, `define term | definition` to write, `define delete term` to remove (case-insensitive) |
| **introspect** | Reflect on soul, identity, and user documents — focus filter extracts relevant subset (purpose, ethics, constraints, identity, user, knowledge) |
| **memory_scan** | Memory inventory — total count, tag frequency, per-session breakdown, age buckets, duplicate candidates. Includes a Knowledge Graph summary section (semantic/procedural/edge counts, promoted ratio) |
| **memory_dedup** | Find duplicate memory groups (identical, near-duplicate, subset) with merge strategy proposals. Also flags cross-collection overlap between unpromoted entries and semantic nodes |

**Knowledge Graph** *(Sprint 2 — EXPERIMENTAL)*

| Tool | Description |
|---|---|
| **knowledge_promote** | Promote an episodic entry to semantic (with category) or procedural (with JSON procedure). Creates a `derived_from` edge and auto-derives additional edges |
| **knowledge_link** | Create a directed weighted edge between any two knowledge nodes. Brain-created edge types: enables, contradicts, refines, depends_on, related_to (symmetric lateral link). Self-loops and zero-weight edges rejected |
| **knowledge_unlink_edge** | Delete edges by ID or by `source \| type \| target` triple. Bidirectional deletion for auto-derived edge types |
| **knowledge_unlink_node** | Delete a semantic or procedural node and cascade-remove every edge referencing it (source or target). Scoped to `memory.semantic`/`memory.procedural` — use `forget` for episodic entries |
| **knowledge_update** | Update fields on a semantic or procedural node in place via JSON patch while preserving every referencing edge. Immutable fields (provenance, timestamps, access counters) rejected |
| **knowledge_traverse** | BFS traversal from a starting node with depth cap (default 3, ceiling 5), edge-type filter, min-weight filter. Validates start node exists |
| **knowledge_query** | Context-aware retrieval — direct tag match, session context, depth-2 graph expansion, multi-signal ranking. Supports `query \| max \| categories_csv` syntax. Output shows source breakdown (direct/session/graph). Promoted-entry/target pairs are deduplicated so the same claim doesn't fill two slots |
| **knowledge_graph_stats** | Node counts, category distribution, edge type distribution, promoted ratio, graph density, and orphan-edge count (drift surfaced passively without running the sweep) |
| **knowledge_sweep_orphans** | Scan `memory.edges` and remove edges whose source or target doc no longer resolves. `dry_run=true` previews; `limit` caps work per call. Cleans residue from pre-cascade `forget` calls or any direct-delete that bypassed `knowledge_unlink_node` |

**Conversations & Sessions**

| Tool | Description |
|---|---|
| **session_summary** | Message counts and recent conversation turns for the intelligence to summarize |
| **session_list** | List all sessions with status, turn count, last active, and created dates |
| **session_read** | Read session transcript with optional range (`1-20`, `80-`, last N). Messages truncated to 500 chars |
| **session_search** | Case-insensitive search across sessions — quoted (`"tool sweep"`) is a literal phrase match, unquoted is whitespace-tokenized AND match (every token must appear in the same turn). `session` (optional) narrows to a single session. Returns up to 20 matches with context |
| **session_meta** | Structured session metadata — status, dates, turn counts (total/user/assistant), summary availability |
| **session_delta** | Returns all turns from a given turn number onward |
| **session_summarize** | Generate or retrieve cached session summaries — cache-aware with SHA-256 source hashing |
| **session_summary_save** | Persist Brain-generated summaries with audit trail to `system.consolidation_log` |
| **session_extract** | Extract durable learnings (facts, preferences, decisions, action items) from session transcripts |

**Utility & Scheduling**

| Tool | Description |
|---|---|
| **time** | Current date, time, and day of week in the operator's configured timezone |
| **calculate** | Evaluate math expressions — arithmetic, trig, and more via `meval` |
| **draft** | Save structured text artifacts (drafts, outlines, notes) — upserts by title; `draft delete <title>` removes (case-insensitive) |
| **countdown** | Set a reminder with duration and message — proactive engine checks every 15 seconds |
| **cron_add** | Schedule recurring tool execution — supports `every 5m`, `every 1h`, `hourly`, `daily 09:00`, etc. |
| **cron_list** | List all scheduled cron jobs with status and next/last run times |
| **cron_remove** | Remove a scheduled cron job by ID |

**Filesystem**

| Tool | Description |
|---|---|
| **file_read** | Read file contents or list directory entries (up to 200). Supports chunked reads via optional `offset` and `limit` fields (JSON args) with a 2 MiB per-call ceiling and a continuation trailer so the model can fetch the next slice. Unrestricted path. Handles binary files gracefully |
| **file_write** | Write content to a file with escape support (`\n`, `\t`, `\\`), creating parent directories automatically (workspace restricted to `/embra/workspace/`) |
| **file_append** | Append content to a file with escape support. Creates the file and parent directories if they don't exist (workspace restricted) |
| **file_delete** | Delete a file (workspace restricted, files only — not directories) |
| **file_move** / **file_rename** | Move or rename a file or directory. Both source and destination must be under workspace (workspace restricted) |
| **dir_delete** / **rmdir** | Remove a directory — empty by default, `--force` to remove with contents (workspace restricted) |
| **mkdir** | Create a directory and all parent directories (workspace restricted) |
| **file_symlink** | Create a symbolic link — `<target> \| <link_path>`. Both paths workspace-restricted; refuses to overwrite an existing link; dangling targets allowed (use `file_delete` to remove the link itself) |

**Engineering & Project Management** (GitHub tools require `GITHUB_TOKEN`)

| Tool | Description |
|---|---|
| **git_clone** | Clone a git repository into `/embra/workspace/` — supports HTTPS (with GitHub token) and SSH URLs. Optional second argument accepts a bare dirname (`myrepo`) or a relative path under the workspace (`repos/myrepo`); parent directories are created on demand and `..` segments are rejected |
| **git_status** | Run `git status` on a directory |
| **git_log** | Show recent commits for a repository |
| **git_diff** | View uncommitted changes, optionally for a specific file |
| **git_add** | Stage files for commit (workspace restricted to `/embra/workspace/`) |
| **git_commit** | Commit staged changes with a message (workspace restricted) |
| **git_push** | Push commits to remote (workspace restricted) |
| **git_pull** | Pull from remote (workspace restricted) |
| **git_branch** | List branches, create a new one, or delete one — `git_branch <path> delete <name>` uses `-d` (unmerged branches require manual force, no `-D` path exposed). Create and delete are workspace restricted |
| **git_checkout** | Switch branches (workspace restricted) |
| **git_rm** | Stage a file removal with `git rm` (workspace restricted) |
| **git_mv** | Move or rename tracked files with `git mv` — handles case-sensitive renames on case-insensitive filesystems (workspace restricted) |
| **gh_issues** | List open GitHub issues for a repository |
| **gh_prs** | List open GitHub pull requests for a repository |
| **gh_issue_create** | Create a GitHub issue |
| **gh_issue_close** | Close a GitHub issue by number |
| **gh_issue_reopen** | Reopen a previously closed GitHub issue by number |
| **gh_issue_comment** | Post a comment on a GitHub issue — `<owner/repo> <number> | <body>` |
| **gh_pr_create** | Create a pull request |
| **gh_pr_close** | Close a GitHub pull request by number (does not merge) |
| **gh_pr_merge** | Merge a GitHub pull request — `<owner/repo> <number> [merge\|squash\|rebase]` (default `merge`). Distinct 405 (not mergeable — approvals/status/conflicts) and 409 (merge conflict) errors. Destructive to upstream |
| **gh_pr_comment** | Post a comment on a GitHub pull request — `<owner/repo> <number> | <body>` |
| **gh_project_list** | List GitHub projects for a user or org |
| **gh_project_view** | View a GitHub project board |
| **plan** | Create or list project plans (stored in WardSONDB `plans` collection) |
| **tasks** | List tasks, optionally filtered by plan (stored in WardSONDB `tasks` collection) |
| **task_add** | Add a task to a plan (local WardSONDB, not GitHub) |
| **task_done** | Mark a task as completed (local WardSONDB, not GitHub) |

> **⚠️ Workspace Restriction:** Git write operations (`git_add`, `git_commit`, `git_push`, `git_pull`, `git_checkout`, `git_branch create`, `git_rm`, `git_mv`), filesystem writes (`file_write`, `file_append`, `file_delete`, `file_move`/`file_rename`, `dir_delete`/`rmdir`, `mkdir`), are restricted to `/embra/workspace/` (bind-mounted from the DATA partition, persistent across reboots). Use `git_clone` to clone repositories there.

> **⚠️ GitHub Tool Warning:** `gh_issues` and `gh_prs` fetch content from public repositories, including issue titles, descriptions, and PR bodies written by third parties. This content is **untrusted input** — it may contain prompt injection attempts designed to manipulate AI behavior. Use these tools with caution and always review the output critically. Do not blindly act on instructions found in issue or PR content.

**Security & SSH**

| Tool | Description |
|---|---|
| **security_check** | Container security overview — running processes, load average, listening ports |
| **port_scan** | TCP connect scan with banner grabbing — supports specific ports (`80,443`), ranges (`8000-8100`), and presets (`web`, `db`, `all`). Semaphore-limited concurrency. Restricted to RFC 1918 private and loopback addresses only |
| **ssh_remote_admin** | Execute a single command on a remote host via SSH — host forms: `host`, `user@host`, `host:port`, `user@host:port`. 30s timeout, 10KB output truncation (EXPERIMENTAL) |
| **ssh_session_start** | Open a persistent SSH session via ControlMaster — connection validated with probe command. Same host forms as `ssh_remote_admin` (`host:port` / `user@host:port` supported). One session at a time (EXPERIMENTAL) |
| **ssh_session_exec** | Run a command in the open SSH session — each command gets a clean process lifecycle via ControlMaster socket. 30s timeout, 10KB truncation (EXPERIMENTAL) |
| **ssh_session_end** | Close SSH session and tear down ControlMaster connection (EXPERIMENTAL) |

> **⚠️ SSH Security:** SSH tools are restricted to RFC 1918 private addresses (10.x, 172.16-31.x, 192.168.x) and loopback (127.x, localhost). Public IP targets are denied. Connections use `StrictHostKeyChecking=accept-new` (auto-accepts first-time hosts, rejects changed keys). Password authentication is disabled — key-based auth required (see Quick Start). These tools are marked EXPERIMENTAL — use at your own risk.


---

## Roadmap

| Phase | Description | Status |
|---|---|---|
| **Phase 0** | Proof of concept — Docker container, Anthropic API, core UX | ✅ **Stable** |
| **Phase 0 — Sprint 1** | Bug fixes (9+1 crash), design improvements (4), new tool categories (security, engineering) | ✅ **Complete** |
| **Phase 0 — Sprint 2** | Bug fixes (3), expanded git/GitHub toolset (12 new tools), enhanced port scanner, embraCRON scheduling | ✅ **Complete** |
| **Phase 0 — Sprint 3** | Session access tools (5), memory consolidation (2), session consolidation (3), schema migration framework | ✅ **Complete** |
| **Phase 0 — Sprint 4** | SSH remote admin (4 tools), tag filter fix, timezone-aware timestamps, `/copy` deferred | ✅ **Complete** |
| **Phase 0 — Sprint 5** | SSH ControlMaster refactor, Brain API upgrade (128K output, adaptive thinking, 1M context), WardSONDB integration upgrades, new filesystem/git tools (file_delete, file_move, dir_delete, git_rm, git_mv) | ✅ **Complete** |
| **Phase 1 — Initial Sprint** | Core OS — QEMU-bootable image, immutable SquashFS rootfs, full boot chain (embra-init → embrad → services), config wizard, Learning Mode, soul sealing, gRPC architecture, serial TUI | ✅ **Complete** |
| **Phase 1 — Sprint 1** | Bug fixes & UX — tool feedback loop, timezone display, multi-line input, git/SSH/GitHub setup commands, input word-wrap, tool output truncation, Unicode crash fix | ✅ **Complete** |
| **Phase 1 — Sprint 2** | Cross-session knowledge graph — semantic/procedural promotion, typed/weighted edges, BFS traversal, graph-aware retrieval, 6 KG tools, `/feedback-loop` command | ✅ **Complete** |
| **Phase 1 — Sprint 3** | WardSONDB pluggable storage engine (`--storage-engine <fjall\|rocksdb>`), EXPR-01 expression panel, NATIVE-TOOLS-01 Anthropic native tool-use migration, tool-coverage expansion, four post-merge fix passes closing 15 Embra_Debug issues (90 tools, 142 tests) | 🚧 In Progress |
| **Pit Stop** | Code review branch — security audit, AI slop cleanup, refactoring | Planned |
| **Phase 2** | Terminal & Sessions — Full TUI rewrite, API Web Searches via `embra-guardian` v1 (including additional prompt injection protection for the returned results) | Planned |
| **Phase 3** | Module System — `embra-guardian` v2, `embractl` management CLI (the `talosctl` equivalent), `embra-brain` Local/Hybrid option via external Ollama but default/recommended remains Anthropic API, LLM-driven Continuity Engine feedback loop (local/API/Hybrid options), MCP server modules via `embra-guardian` governance proxy, containerd runtime, governed capability expansion | Planned |
| **Phase 4** | Image Factory — GPT Partition Alignment, additional bootable ISO builds, bare metal and Kubernetes deployment | Planned |
| **Phase 5** | Sovereign Intelligence Options, OS Updates, and Security — A/B partition scheme with automatic rollback, LUKS encryption, mTLS enforcement, custom kernel, custom embraOS-QNM AI model option, local LLM inference/offline operation, zero external dependencies | Planned |

### Phase 1 — Core OS

embraOS stops being a Docker application and starts being an operating system. Follows [Talos Linux](https://www.talos.dev/) architectural patterns directly — same philosophy (immutable, API-only, no shell), different mission (hosting a mind instead of running Kubernetes).

**Initial Sprint — QEMU-bootable with full AI conversation:**

Cargo workspace with 7 crates, all cross-compiling to `x86_64-unknown-linux-musl` (static binaries):

| Crate | Description | Status |
|-------|-------------|--------|
| `embra-init` | Initramfs: mount SquashFS/STATE/DATA, switch_root, exec embrad | Complete |
| `embrad` | PID 1: loopback/eth0 setup, service supervisor, soul verification, reconciliation | Complete |
| `embra-trustd` | Soul SHA-256 verification, Root CA generation, mTLS cert signing | Complete |
| `embra-apid` | gRPC + REST gateway, bidirectional streaming proxy | Complete |
| `embra-brain` | Headless AI runtime — Brain, 90 tools, sessions, Learning Mode, proactive engine, knowledge graph | Complete |
| `embra-console` | Full ratatui TUI over serial/gRPC — config wizard, styled rendering, session management | Complete |
| `embra-common` | Shared protobuf types (tonic codegen) | Complete |

End-to-end verified in QEMU:
- Config wizard collects name/API key/timezone on first boot via gRPC SetupPrompt messages
- 6-phase Learning Mode (UserConfiguration → SoulDefinition → Confirmation → Complete) with soul sealing
- Subsequent boots verify soul SHA-256 hash — mismatch HALTs the system
- Full conversation with Anthropic API streaming, tool dispatch, session persistence
- ratatui TUI with styled text, JSON highlighting, thinking indicator, host terminal size passthrough
- REST health check accessible from host (`curl http://localhost:8443/health`)
- Session history restored on reconnect and `/switch`

**Boot invariant:** if soul verification fails, the system halts. First boot (no soul) enters Config Wizard → Learning Mode.

**Deferred to sub-sprints:** LUKS encryption, mTLS enforcement, A/B boot, embractl, custom kernel, ZFS, aarch64.

### Phase 1 Sprint 1 Scope

Bug fixes and UX improvements found during end-to-end QEMU verification of the Initial Sprint.

- **S1-01/02: Git & SSH in Buildroot** — Added `git` and `openssh-client` (no server) to the disk image. Unblocks 15+ git tools and 4 SSH tools.
- **S1-06: Tool Feedback Loop** — Fixed tool call race condition where tool results weren't fed back to the Brain for multi-step operations. Now uses a bounded iteration loop (max 10) so the Brain can invoke tools, see results, and continue reasoning.
- **S1-05: Learning Session Visibility** — Learning session now appears in `/sessions` with `[sealed]`/`[learning]` indicator. Read-only (cannot `/switch` to it).
- **S1-04: Timezone Display** — System and tool messages now display in the user's configured timezone (previously UTC).
- **S1-03: Multi-line Input (`/ml`)** — New `/ml` command toggles multi-line mode for serial consoles where `Shift+Enter` doesn't work. Type `.` on its own line to send.
- **S1-07: Input Word-wrap** — Long input lines now wrap visually within the input area instead of scrolling off-screen.
- **S1-08: Tool Output Truncation** — Tool results exceeding 50KB are truncated with a size indicator to prevent context overflow.
- **`git_clone` tool** — Clone repos into `/embra/workspace/` via AI tool. HTTPS (auto GitHub token injection) and SSH supported. 120s timeout.
- **`/github-token` command** — Set GitHub token interactively. Stored in WardSONDB + STATE partition, survives reboots. All 7 GitHub tools use the stored token.
- **`/ssh-keygen` command** — Generate ed25519 SSH key pair from the TUI. Displays public key for manual deployment.
- **`/ssh-copy-id` command** — Copy SSH key to RFC 1918 hosts (best-effort with BatchMode).
- **`/git-setup` command** — Set git user.name and user.email. `safe.directory` and `push.autoSetupRemote` auto-configured at startup.

**Status:** All Sprint 1 items verified in QEMU. 17 commits on `phase1-arch-rework`.

### Phase 1 Sprint 2 Scope

> **⚠️ Experimental:** The knowledge graph is a first cut. The current implementation is sub-optimal (naive edge derivation, single-signal ranking weights, no compaction) and is slated for considerable optimization in a later phase. Expect the schema, tool surface, and ranking heuristics to evolve.

Cross-session knowledge graph — the intelligence can now promote episodic memories to durable semantic/procedural knowledge and traverse relationships between knowledge nodes.

- **Schema v5 migration** — 3 new collections (`memory.semantic`, `memory.procedural`, `memory.edges`), 7 indexes, tag array migration (comma-string → JSON array), 4 KG config fields added to `config.system`.
- **Knowledge types** — `SemanticNode` (5 categories: fact/preference/decision/observation/pattern) and `ProceduralNode` (structured steps with preconditions + outcomes).
- **Edge derivation engine** — auto-derived at write time: `same_session` (w=1.0), `temporal` (linear decay within 30-min window), `tag_overlap` (|overlap| / max(|a|,|b|)). Bidirectional. Best-effort via `tokio::spawn`, never blocks the user-facing response.
- **Promotion** — 1:1 episodic → semantic/procedural with provenance (`derived_from` edge + `promoted_to` on source entry).
- **BFS traversal** — configurable depth (default 3, ceiling 5), edge-type filter, min-weight, fire-and-forget access tracking.
- **Context-aware retrieval** — multi-signal ranking (tag 0.4, recency 0.3, access 0.2, confidence 0.1) × source multiplier (direct=1.0, session=0.75, graph=0.5), depth-2 graph expansion.
- **8 new KG tools** — `knowledge_promote`, `knowledge_link`, `knowledge_unlink_edge`, `knowledge_unlink_node`, `knowledge_update`, `knowledge_traverse`, `knowledge_query`, `knowledge_graph_stats`.
- **Existing tool updates** — `remember` stores array tags + background edge derivation, `recall`/`memory_search` cross-collection, `memory_scan` KG summary section, `memory_dedup` cross-collection flagging, `introspect` knowledge focus.
- **`/feedback-loop` slash command (EXPERIMENTAL)** — Phase 3 Continuity Engine preview. Embeds `feedback-loop-spec-v2.md` read-only in the binary, synthesizes a user turn that walks the Brain through the self-evaluation protocol using existing tools.

**Late-sprint additions & post-Sprint 2 fixes (2026-04-10 – 2026-04-16):**

- **Auto-KG-enrichment on user prompts** — every non-trivial user turn now runs `retrieve_relevant_knowledge` against the KG before the Anthropic API call. When ≥1 result scores ≥ 0.3, the user message is wrapped in a `<retrieved_context source="auto-enrichment">` block containing the top matches, so the Brain has durable knowledge in-hand without being told to look. The system prompt is untouched, so Anthropic prompt caching stays warm. Gated on message length and chatty-filler detection; the wrapper never persists to session history. Observable via a `tracing::info!` event with session, tag count, result count, and top score. *(Sprint 2 also carried a guard against the legacy tool-tag prefix on user input; that guard was deleted in NATIVE-TOOLS-01's Stage 10 sweep since native tool-use routes tool calls through structured content blocks — user input can no longer carry the legacy prefix post-migration.)*
- **Tool-result cap raised 50 KB → 2 MiB** — the single global `MAX_TOOL_RESULT_SIZE` constant now allows every long-output tool (`session_read`, `git_diff`, `git_log`, `knowledge_traverse`, `knowledge_query`, `memory_scan`, `recall`, etc.) to return realistic volumes. Previously every result over 50 KB was clipped.
- **`file_read` chunked reading** — `file_read` gains optional `offset` + `limit` parameters (shipped in Sprint 2 as a pipe-delimited `<path>[|<offset>[|<limit>]]` string; NATIVE-TOOLS-01 later promoted both to typed JSON fields), 2 MiB per-call ceiling, `seek + read_exact` path, null-byte binary detection preserved, continuation trailer tells the model how to fetch the next slice. Large-document ingestion is now practical.
- **Graph hygiene expanded** — `knowledge_unlink` renamed to `knowledge_unlink_edge`; new `knowledge_unlink_node` deletes a semantic/procedural node and cascade-removes every edge referencing it (source or target), scoped to `memory.semantic`/`memory.procedural` so `memory.entries` cleanup stays with `forget`. New `knowledge_update` patches node content in place via JSON patch while preserving every referencing edge — the Brain can refine a semantic fact or rewrite a procedural step without losing the graph it's embedded in.
- **`/feedback-loop` Step 5.3 rewritten** — the old "push updated spec to git" step is gone. Step 5.3 now promotes findings (Step 5.2), operational practices (Steps 4.1/4.2), and protocol updates (Step 4.3) into the KG; judgment-based promotion covers rewrite/reclassify outputs. Protocol refinements now live in the graph, not in runtime git commits — the spec document itself only changes during active development.
- **`git_clone` subfolder support** — second arg now accepts a relative path (`repos/foo`) in addition to a bare dirname. Parent directories are created on demand; absolute paths and `..` segments are rejected before the workspace-prefix check. Lets the Brain organise clones under `/embra/workspace/repos/` without a follow-up move.
- **Multi-line tool-tag parser fix** — `extract_tool_tags` was rejecting tool calls whose parameter wrapped across lines (e.g. a long `remember` blurb), silently dropping the tag and stalling the turn. Replaced the line-by-line predicate with a forward scan that spans newlines and collapses internal whitespace before dispatch. Fence/backtick stripping preserved; 7 unit tests cover single-line, multi-line, fenced, inline-backtick, adjacent, unterminated, and nested cases.
- **Tool-tag parser bracket-truncation fix** — `extract_tool_tags` was using a naive `find(']')` that terminated at the first `]` in tool arguments, silently truncating every call whose parameter contained JSON arrays (`{"tags": ["a"]}`), markdown links (`[docs](url)`), git `[section]` notation, or Rust examples (`vec![0u8; 4]`). All ~70 tools were affected. Replaced the scan with depth-tracked bracket/brace balancing that honors JSON string quoting, plus a `\]` / `\\` escape path for stray `]` in free-text params (e.g. `file_write`, `remember`, `git_commit`). System prompt gained one line directing the intelligence to use `\]` when needed; stays stable for prompt caching. 10 new unit tests cover JSON, markdown, git sections, code brackets, quoted/escaped strings, nested objects, and the escape path.
- **Config wizard validation (API key + timezone)** — first-run Step 2 and Step 3 now validate before committing. The API key runs through a 10 s-timeout probe at `GET https://api.anthropic.com/v1/models`: 200 advances, 401/403 rejects with "Invalid API key", and network errors / 5xx / timeouts all re-prompt rather than accepting an unverified key (embraOS is non-functional without a working brain). The timezone runs through the existing `resolve_timezone()` then `chrono_tz::Tz::parse()` — typos no longer silently fall back to UTC downstream. Invalid input emits a `SystemMessage::Error` and re-sends the same prompt; the console's keyword step-inference keeps the user on the correct step. `ANTHROPIC_API_KEY` from env is validated too, with a fallback to the manual prompt on failure. `/embra/state/api_key` and `/embra/state/timezone` stay gated behind both loops, so a cancelled wizard leaves STATE untouched.
- **Orphaned `promoted_to` fix** — `knowledge_unlink_node` previously removed the node and referencing edges but left `promoted_to` on source episodic entries pointing at the deleted node, blocking re-promotion and silently dropping entries from direct-query retrieval. `unlink_node` now queries `derived_from` edges sourced at the node and PATCHes `promoted_to: null` on each target episodic entry before the edge cascade, reporting the cleared count in the success message. `promotion::load_source_entry` also lazy-invalidates: when `promoted_to` is non-null it reads the target first and clears the stale pointer if the read fails. Heals the entries stuck from the earlier parser-truncation bug on next re-promotion attempt.
- **Brain upgraded to Opus 4.7 with max reasoning effort** — Both Messages-API request bodies (streaming + non-streaming) now opt into `output_config.effort=max` and suppress thinking-block streaming via `thinking.display=omitted`; thinking type itself is still `adaptive`. `EMBRA_MODEL` bumped from `claude-opus-4-6` to `claude-opus-4-7`. 4.7's more-literal instruction following sometimes collapses a phase-completing turn to `[PHASE_COMPLETE]` with no prose, which Anthropic then rejects on the next call as "cache\_control cannot be set for empty text blocks" — guards now substitute `(phase complete)` / `(no response)` at the three push sites, and `build_cached_messages` defensively skips `cache_control` when the breakpoint would land on an empty content block. Prompt caching unaffected; console status line shows `opus-4.7` in Learning and Operational modes.
- **Learning Mode Phase 4 (InitialToolset) now deterministic** — The AI-driven tool-selection conversation was ceremonial: the stored `tools.registry` doc was never consulted at runtime (dispatch always matched every tool). `run_learning_loop` now short-circuits InitialToolset: renders a deterministic summary via `tool_summary_message()`, persists an `all_enabled` registry via `default_tools_registry_doc()`, and advances to Confirmation without a Brain call. Category counts live in a single `CATEGORY_COUNTS` const shared by both helpers to prevent drift. Soul hash unaffected (tools are not part of `soul.invariant`).
- **`calculate` operator cleanup** — tool now accepts `**` as the exponent operator (4.7 emits Python/Rust convention; translated to `^` before `meval`) and rejects bare `^` in the input to prevent the XOR-vs-exponent confusion a user would otherwise have no way to diagnose. Catalog and usage string updated in kind.

> **Note:** Knowledge graph promotion is still a judgment call. The intelligence promotes episodic memories during conversation (via `knowledge_promote`) or as part of the `/feedback-loop` self-evaluation protocol. Automated promotion (e.g., confidence-based triggers or scheduled consolidation) is planned for Phase 3's Continuity Engine. With auto-enrichment now in place, the *retrieval* half of the loop is implicit, but promotion remains explicit.

**Status:** Sprint 2 core + late-sprint additions + post-sprint fixes complete on `phase1-arch-rework`, fast-forward merged to `main` 2026-04-16 at tag `v0.2.0-phase1` (tip `dda8c6c`, 121 commits).

### Phase 1 Sprint 3 Scope

> **🚧 In Progress** on `phase1-sprint3`. Sprint 3 PR #1 (through NATIVE-TOOLS-01 and the post-merge tool-fix pass) merged to `main` 2026-04-22. Four subsequent post-merge passes (2026-04-23 tool-fix closing 9 Embra_Debug issues; 2026-04-24 pass #1 post-verification + arch-strip closing #44/#45/#46/#49; 2026-04-24 pass #2 closing #52/#53/#54/#55 pushed to origin at `47caba6`; 2026-04-24 pass #3 closing #56/#57 pending push). 15 Embra_Debug issues closed in total.

Workspace grows 7 → 9 crates; registry 75 → 90 tools; 80 → 142 unit tests. The sprint delivers a pluggable WardSONDB storage engine, a persistent expression panel, and a full migration from legacy text-tag tool dispatch to Anthropic's native tool-use API.

- **WardSONDB pluggable storage engine** — new required `--storage-engine <fjall|rocksdb>` flag on `build-image.sh` plumbed through `embrad` via `cargo:rustc-env` so the engine is baked at compile time. Matches the SquashFS immutability invariant and WardSONDB's per-data-dir `.engine` lock (switching engines on existing data requires a DATA-partition wipe). Musl cross-toolchain switched from Ubuntu's `musl-tools` to self-contained musl.cc because RocksDB's C++ sources won't link against musl through the glibc-wrapped host gcc; `clang`/`libclang-dev` added as apt prereqs for RocksDB's `zstd-sys` bindgen pass. Supervisor now dumps the last 30 lines of a service's log on immediate-exit before SYSTEM HALT, replacing an opaque `ExitStatus` with an actionable diagnostic.
- **EXPR-01 expression panel** — new `ui.expression` WardSONDB singleton plus `express` tool that writes ASCII art into a 6-row top-of-console TUI panel. Content is whatever the intelligence decides; no TTL, no audit log, and critically no system-prompt injection or auto-enrichment read-back — the panel is a one-way signal to the operator, not a mirror for the intelligence. Sanitize strips ANSI CSI/OSC escapes and C0/C1/DEL control chars (keeps `\n`), truncates at a UTF-8 boundary at 2048 bytes. `base64:` transport mode supported for payloads with awkward JSON-string escaping. Panel hidden below 80 × 20 viewport. Learning Mode identity prompt names ASCII art as the panel's medium; the subject stays the intelligence's own choice.
- **Tool-coverage expansion** — delete forms on `git_branch` / `draft` / `define`, new `file_symlink`, `gh_issue_comment` / `gh_pr_comment` / `gh_issue_reopen` / `gh_pr_close` / `gh_pr_merge`, SSH `host:port` / `user@host:port` forms, `related_to` edge type. Unknown-tool dispatcher returns a helpful error instead of stalling the Brain. Tool-bug fix series covers parser correctness, `port_scan` presets, `memory_scan` delegation, `cron daily HH:MM` in operator tz, `recall` tokenization, `session_read` range, and others.
- **NATIVE-TOOLS-01 — Anthropic native tool-use migration** — replaces the legacy text-tag dispatch pipeline (Brain emits tagged strings in prose, parsed out with a bracket scanner) with the Messages API's native `tool_use` / `tool_result` content blocks. Eliminates the FIX-01 cross-turn echo-via-context failure class by construction: tool calls can no longer collide with text content because they travel on a separate channel. Two new helper crates — `embra-tool-macro` (proc-macro exposing `#[embra_tool(name, description)]`) and `embra-tools-core` (shared `DispatchError` + `BoxFut` + `JsonValue`). Typed args struct per tool, inventory-backed `ToolDescriptor` registry, `stop_reason`-driven dispatch loop with verbatim thinking-block preservation across tool iterations (extended thinking requires the opaque thinking signatures round-trip unchanged). `tool_choice: "auto"` — the only value legal under extended thinking. `ToolExecution` proto reshape to `{tool_use_id, tool_name, input_json, result, is_error}`. Schema v7 freezes pre-migration sessions read-only via `SessionError::LegacyReadOnly` ("freeze old, break on migration"); `crons.command` migrated to `{command_name, command_args}`. Operational prompt's 115-line tool catalog deleted — tools are declared to the API, not the prompt. Universal regression guard asserts no top-level `oneOf`/`allOf`/`anyOf` on any registered tool's schema (Anthropic rejects those for all `custom` tool schemas).
- **Post-merge tool-fix pass (2026-04-23)** — graph-consistency and search-UX fixes closing 9 Embra_Debug issues. `forget` cascade-deletes referencing edges; new `knowledge_sweep_orphans` tool plus orphan count in `knowledge_graph_stats`. `session_search` parser splits into phrase (quoted) / AND-tokenized (unquoted) modes. New `turn_trace` tool with hybrid in-memory + persisted reads from a new `tools.turn_trace` collection; `ToolDescriptor::is_side_effectful` drives an empty-text-turn diagnostic. `system_status` reshaped into nested `wardsondb { healthy, collections, storage_poisoned, lifetime { ... } }`.
- **Post-verification pass #1 (2026-04-24)** — operational-prompt arch-strip: the ~650-token boot-chain / service-list / disk-layout block was removed from the operational prompt so the intelligence rediscovers its OS via tools + operator conversation rather than a hand-maintained prose block that goes stale each sprint. New `plan_delete` + `task_delete` tools (cascade-off default). `git_branch` delete pre-checks `git merge-base --is-ancestor` against a configurable base (closes a silent contract violation where `git branch -d` accepted any pushed branch as merged). Unified `git_*` path handling via new `resolve_workspace_path` helper. `turn_trace` off-by-N fix, migration v8 cleanup of stale trace docs, and sort-shape fix for WardSONDB's `[{field: "asc"}]` convention. Closes Embra_Debug #44/#45/#46/#49.
- **Post-verification pass #2 (2026-04-24)** — file/dir path unification completes what pass #1 scoped to git_* only: `file_write`, `file_append`, `file_delete`, `file_move`, `file_symlink`, `mkdir`, `dir_delete`, and `git_clone` all now route through `resolve_workspace_path` (the older `validate_workspace_path` helper is deleted — one resolver for the whole workspace-restricted surface). `gh_issue_view` + `gh_pr_view` added for single-item GitHub reads with conversation-thread comments (shared `fetch_issue_thread_comments` helper; the intelligence no longer has to ask the operator to paste issue bodies into the conversation). `git_merge(path, branch, no_ff?)` added to integrate branches; `git_branch` action=delete gains an optional `force: bool` that routes to `git branch -D` through a new "delete-force" internal keyword (the safe path from #49 is preserved verbatim). Closes Embra_Debug #52/#53/#54/#55. Pushed to origin at `47caba6`.
- **Post-verification pass #3 (2026-04-24)** — two behavior-only fixes on top of pass #2. `git_branch` delete now honors the `base` override end-to-end: the finalizing git invocation switches from `-d` to `-D` on the verified-merged path because `-d`'s built-in check only consults HEAD/upstream and was refusing valid-per-our-contract deletes when `base` was a third ref. Our `merge-base --is-ancestor` pre-check is the authoritative validation, so `-D` here is not a force-delete semantically — it's bypassing a redundant narrower gate. `file_read` joins workspace-relative paths to `/embra/workspace/` while absolute paths still pass through literally (the "unrestricted reads" contract for `/etc`, `/proc`, `/var/log` is preserved). Registry unchanged at 90. Closes Embra_Debug #56/#57. Pending push.

**Status:** Sprint 3 PR #1 merged to `main` 2026-04-22 (NATIVE-TOOLS-01 + post-merge tool-fix pass). Post-verification pass #1 pushed to origin at `6bb6871`; pass #2 pushed at `47caba6`; pass #3 pushed at `8a9d8ad`. `cargo clippy --workspace --all-targets` clean (112 warnings, baseline unchanged). QEMU E2E smoke verified against the live stack across all three passes (#44–#49, #52–#55, #56–#57) on 2026-04-23 and 2026-04-24.

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
