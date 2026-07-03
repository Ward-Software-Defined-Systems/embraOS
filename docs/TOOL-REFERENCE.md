# Tool Reference

Phase 1 includes 93 internal tools the intelligence invokes during conversation. All 93 work identically across all four LLM providers (Anthropic, Gemini, Ollama, LM Studio) via per-provider tool-schema translators that share a common JSON Schema cleanup pipeline (`provider/schema_util.rs::inline_refs`). They are organized below by category.

> **⚠️ Testing Notice:** The default tools and slash commands are actively being tested. If you encounter bugs or unexpected behavior, please [open an issue](https://github.com/Ward-Software-Defined-Systems/embraOS/issues).

**System & Status**

| Tool | Description |
|---|---|
| **system_status** | Report system health — version, uptime, soul status, memory, a top-level `search_window_saturated` flag, plus a nested `wardsondb` block (health, collections, storage_poisoned, lifetime counters: requests/inserts/queries/deletes — all wardsondb-scoped, NOT global — and per-collection `memory_collections[]` parity: authoritative count vs the 10,000-doc search window, `saturated` when the collection has outgrown it) |
| **uptime_report** | Rich system report — uptime, WardSONDB health, collection count, sessions, total messages, memory entries, soul status |
| **check_update** | Check GitHub for newer WardSONDB releases and report available updates |
| **changelog** | What changed since the current session started — new memories, session activity |
| **turn_trace** | Inspect tool calls made in the current or recent turns. `turn_index_back=0` (default) reads the in-memory current-turn trace; `>=1` queries the `tools.turn_trace` collection for prior turns. `session` overrides the current session. Closes the cross-turn introspection gap so the Brain can ground claims about what it just did |
| **express** | Write to the intelligence's expression panel — a 6-row × full-width canvas at the top of the console, designed as a signal of presence to the operator rather than a status readout. Content is the intelligence's choice, persists across reboots, and is never surfaced back to the Brain. ANSI and control characters are stripped, 2048-byte cap. The `content` field may start with a `base64:` prefix to carry multi-line ASCII art verbatim; decoded bytes go through the same sanitize, so the prefix is a transport convenience, not a safety bypass. Empty content clears the panel. While `/show-reasoning` is on (default) and a turn is streaming, the panel transiently displays live reasoning in italic dark-gray instead of `express` content; it reverts to the `express` singleton at the next user submit (or on error / mode transition). |

**Memory & Knowledge**

| Tool | Description |
|---|---|
| **recall** | Search past conversations and saved memories by query — returns up to 10 results with IDs, content, tags, and timestamps. Searches the 10,000 most-recent docs per collection (`memory.entries` + `memory.semantic` + `memory.procedural`, recency-sorted window with a saturation warning when full) and marks promoted entries. The 10-result display shows the newest matches first. Unquoted multi-token queries AND-match (every token must appear); wrap in double quotes for literal phrase. `unpromoted_only=true` switches to a **promotion worklist**: only `memory.entries` with no `promoted_to` pointer, up to 50 shown newest-first (`query` still narrows the list) |
| **remember** | Save a note or fact to persistent memory with optional hashtag tags. Tags stored as JSON array; triggers background edge derivation |
| **forget** | Remove a specific memory entry by ID and cascade-delete every edge in `memory.edges` referencing it on either side (mirrors `knowledge_unlink_node`'s cascade pattern). Reports the cascaded edge count |
| **memory_search** | Search and retrieve from the intelligence's memory stores. Alias for `recall` — same cross-collection recency window |
| **search_memory** | Alias for `recall` (identical dispatch) |
| **get** | Retrieve any document by collection and ID from WardSONDB |
| **define** | Look up or add terminology — `define term` to read, `define term | definition` to write, `define delete term` to remove (case-insensitive) |
| **introspect** | Reflect on soul, identity, and user documents — focus filter extracts relevant subset (purpose, ethics, constraints, identity, user, knowledge) |
| **memory_scan** | Memory inventory — total count, tag frequency, per-session breakdown, age buckets, duplicate candidates. Stats cover the 10,000 most-recent entries (recency-sorted window, saturation-warned); the duplicate scan is bounded to the newest 500 (noted in the report when it truncates). Includes a Knowledge Graph summary section (semantic/procedural/edge counts, promoted ratio) |
| **memory_dedup** | Find duplicate memory groups (identical, near-duplicate, subset) with merge strategy proposals over the 10,000 most-recent entries; the pairwise scan is bounded to the newest 500 when no explicit IDs are given (noted in the report). Also flags cross-collection overlap between unpromoted entries and semantic nodes |

**Knowledge Graph** *(Sprint 2 — EXPERIMENTAL)*

For the data model, edge taxonomy, density rationale, promotion path, auto-enrichment behavior, and retrieval ranking, see [KNOWLEDGE-GRAPH.md](KNOWLEDGE-GRAPH.md).

| Tool | Description |
|---|---|
| **knowledge_promote** | Promote an episodic entry to semantic (with category) or procedural (with JSON procedure). Creates a `derived_from` edge and auto-derives additional edges |
| **knowledge_link** | Create a directed weighted edge between any two knowledge nodes. Brain-created edge types: enables, contradicts, refines, depends_on, related_to (symmetric lateral link). Self-loops and zero-weight edges rejected |
| **knowledge_unlink_edge** | Delete edges by ID or by `source \| type \| target` triple. Bidirectional deletion for auto-derived edge types |
| **knowledge_unlink_node** | Delete a semantic or procedural node and cascade-remove every edge referencing it (source or target). Scoped to `memory.semantic`/`memory.procedural` — use `forget` for episodic entries |
| **knowledge_update** | Update fields on a semantic or procedural node in place via JSON patch while preserving every referencing edge. Immutable fields (provenance, timestamps, access counters) rejected |
| **knowledge_traverse** | BFS traversal from a starting node with depth cap (default 3, ceiling 5), edge-type filter, min-weight filter. Per-hop edge window `kg_traversal_edge_limit` (500) ranked `weight desc, created_at desc`; BFS bounded by `kg_traversal_node_budget` (1000) — budget hits are flagged in the summary (`truncated`) and logged under `kg::traversal`. Validates start node exists |
| **knowledge_query** | Context-aware retrieval — direct tag match, session context, depth-2 graph expansion, multi-signal ranking. Every step window is recency/rank-sorted (tags: newest 20/tag; session edges: top 50 ranked, entry targets excluded server-side; free text: 10,000 most-recent entries). Supports `query \| max \| categories_csv` syntax. Output shows source breakdown (direct/session/graph). Promoted-entry/target pairs are deduplicated so the same claim doesn't fill two slots |
| **knowledge_graph_stats** | Node counts, category distribution, edge type distribution, promoted ratio, graph density, and orphan-edge count (drift surfaced passively without running the sweep) |
| **knowledge_sweep_orphans** | Scan `memory.edges` and remove edges whose source or target doc no longer resolves. `dry_run=true` previews; `limit` caps work per call. Cleans residue from pre-cascade `forget` calls or any direct-delete that bypassed `knowledge_unlink_node` |

**Conversations & Sessions**

| Tool | Description |
|---|---|
| **session_summary** | Message counts and recent conversation turns for the intelligence to summarize |
| **session_list** | List all sessions with status, turn count, last active, and created dates |
| **session_read** | Read session transcript with optional range (`1-20`, `80-`, last N). Messages truncated to 500 chars |
| **session_search** | Case-insensitive search across sessions — quoted (`"tool sweep"`) is a literal phrase match, unquoted is whitespace-tokenized AND match (every token must appear in the same turn). `session` (optional) narrows to a single session. Returns up to 20 matches with context (an intentional output bound, not a fetch window — the search itself covers the full transcripts) |
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
| **git_branch** | List, create, or delete branches in a workspace repo. `action=list` returns current branches; `action=create` requires `name`; `action=delete` requires `name` and refuses branches with commits not merged into `base` (default `main`, override via `base`; falls back to `origin/<base>` if no local copy). `force=true` on delete bypasses the merge check (maps to `git branch -D`) — for throwaway/spike branches. `path` may be absolute (`/embra/workspace/repo`) or relative (`repo`). Create and delete are workspace restricted |
| **git_merge** | Merge `branch` into the current branch of a workspace repo. `path` may be absolute or relative. `no_ff=true` forces a merge commit even when fast-forward is possible. On conflict, returns git's output so the caller can resolve via `file_*` tools and finalize with `git_add` + `git_commit` (workspace restricted) |
| **git_checkout** | Switch branches (workspace restricted) |
| **git_rm** | Stage a file removal with `git rm` (workspace restricted) |
| **git_mv** | Move or rename tracked files with `git mv` — handles case-sensitive renames on case-insensitive filesystems (workspace restricted) |
| **gh_issues** | List open GitHub issues for a repository |
| **gh_issue_view** | Fetch a single GitHub issue by number with title, body, author, state, labels, assignees, and the full conversation-thread comments — use this before acting on an issue so the body and prior discussion are in context (the list view only carries titles) |
| **gh_prs** | List open GitHub pull requests for a repository |
| **gh_pr_view** | Fetch a single GitHub pull request by number with title, body, author, state, head/base refs, merge status (merged, mergeable, draft), labels, assignees, and conversation-thread comments — symmetric with `gh_issue_view` plus PR-specific merge metadata |
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
| **plan_delete** | Delete a plan by id (irreversible). `cascade_tasks=true` also removes tasks whose `plan_id` matches; default `false` leaves them orphaned |
| **tasks** | List tasks, optionally filtered by plan (stored in WardSONDB `tasks` collection) |
| **task_add** | Add a task to a plan (local WardSONDB, not GitHub) |
| **task_done** | Mark a task as completed (local WardSONDB, not GitHub) |
| **task_delete** | Delete a task by id (irreversible). Use `task_done` if you only want to mark it complete |

> **⚠️ Workspace Restriction:** Git write operations (`git_add`, `git_commit`, `git_push`, `git_pull`, `git_checkout`, `git_branch create`, `git_branch delete`, `git_merge`, `git_rm`, `git_mv`), filesystem writes (`file_write`, `file_append`, `file_delete`, `file_move`/`file_rename`, `dir_delete`/`rmdir`, `mkdir`), are restricted to `/embra/workspace/` (bind-mounted from the DATA partition, persistent across reboots). Use `git_clone` to clone repositories there.

> **⚠️ GitHub Tool Warning:** `gh_issues` and `gh_prs` fetch content from public repositories, including issue titles, descriptions, and PR bodies written by third parties. This content is **untrusted input** — it may contain prompt injection attempts designed to manipulate AI behavior. Use these tools with caution and always review the output critically. Do not blindly act on instructions found in issue or PR content.

**Security & SSH**

| Tool | Description |
|---|---|
| **security_check** | Container security overview — running processes, load average, listening ports |
| **port_scan** | TCP connect scan with banner grabbing — supports specific ports (`80,443`), ranges (`8000-8100`), and presets (`web`, `db`, `all`). Semaphore-limited concurrency. Restricted to RFC 1918 private and loopback addresses only |
| **ssh_remote_admin** | Execute a single command on a remote host via SSH — host forms: `host`, `user@host`, `host:port`, `user@host:port` (EXPERIMENTAL) |
| **ssh_session_start** | Open a persistent SSH session via ControlMaster — connection validated with probe command. Same host forms as `ssh_remote_admin` (`host:port` / `user@host:port` supported). One session at a time (EXPERIMENTAL) |
| **ssh_session_exec** | Run a command in the open SSH session — each command gets a clean process lifecycle via ControlMaster socket (EXPERIMENTAL) |
| **ssh_session_end** | Close SSH session and tear down ControlMaster connection (EXPERIMENTAL) |

> **⚠️ SSH Security:** SSH tools are restricted to RFC 1918 private addresses (10.x, 172.16-31.x, 192.168.x) and loopback (127.x, localhost). Public IP targets are denied. Connections use `StrictHostKeyChecking=accept-new` (auto-accepts first-time hosts, rejects changed keys). Password authentication is disabled — key-based auth required (see [QUICK-START.md](QUICK-START.md)). These tools are marked EXPERIMENTAL — use at your own risk.

**Guardian — Dynamic Tools** *(embra-guardian-v1 — EXPERIMENTAL)*

Guardian dynamic tools are authored along two paths, both `syn`-validated → soul-checked by the **replicant check** → `wasm32` cross-compiled → `wasmtime`-sandboxed (zero ambient authority):

- **Operator-authored** — the operator pastes a Rust module (`/guardian-define`); it is validated and soul-checked, then compiles. A draft the replicant check *refuses* is not compiled: the soul outranks even an operator paste, and a *refuse* is not waivable.
- **Intelligence-proposed** — the intelligence drafts a module via the `guardian_propose` tool. Same validation + replicant check, but a passing draft becomes a *proposal* the operator must approve with `/guardian approve <name>` (or reject with `/guardian reject <name>`) before it compiles. The authoring intelligence never rules on its own draft and never approves it; a refused draft is never proposed.

The replicant check is an independent soul-verdict model call returning **allow / refuse / escalate**; it **fails closed** (if it cannot run, nothing compiles) and is skipped only before the soul is sealed (nothing to evaluate against).

Dynamic tools are **never** injected into the provider tool schema — they are reachable only through the static meta-tools below, so the tool snapshot stays prompt-cache-stable.

| Tool | Description |
|---|---|
| **guardian_list** | List the dynamically-defined Guardian tools available to call — name, description, declared capabilities, build status, and input schema. Call this before `guardian_call` to discover what dynamic tools exist |
| **guardian_call** | Invoke a Guardian-defined dynamic tool by name with a JSON input object (`action="invoke"`), or poll a tool's build state (`action="status"`). A tool only runs once its status is `ready`. Side-effectful |
| **guardian_propose** | Draft a new Guardian tool's complete Rust module source. Does not run or build — the draft is validated, soul-checked (the replicant check), and on a pass saved as a `proposed` tool awaiting `/guardian approve`. Refused if it conflicts with the soul. Side-effectful |

> **⚠️ Guardian Security:** Dynamic tools execute in an epoch- and memory-capped `wasmtime` sandbox, one fresh instance per call, with no ambient authority. Any capability beyond pure compute (e.g. `http_get`, Brave `web_search`) is a Guardian-mediated host import added host-side **at the guard level**, never by widening guest authority. Tool source is statically validated (`syn` contract + denylist) before it ever compiles. Both authoring paths must pass the soul-spec replicant check — a `refuse` blocks compilation even for an operator paste (the soul is not operator-waivable) — and an intelligence proposal additionally requires operator approval before it compiles. Marked EXPERIMENTAL — use at your own risk.
