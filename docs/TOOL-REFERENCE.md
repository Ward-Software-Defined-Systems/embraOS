# Tool Reference

Phase 1 includes 105 internal tools the intelligence invokes during conversation. All 105 work identically across all four LLM providers (Anthropic, Gemini, Ollama, LM Studio) via per-provider tool-schema translators that share a common JSON Schema cleanup pipeline (`provider/schema_util.rs::inline_refs`). They are organized below by category.

> **⚠️ Testing Notice:** The default tools and slash commands are actively being tested. If you encounter bugs or unexpected behavior, please [open an issue](https://github.com/Ward-Software-Defined-Systems/embraOS/issues).

**System & Status**

| Tool | Description |
|---|---|
| **system_status** | Report system health — version, uptime, soul status, memory, a top-level `search_window_saturated` flag, plus a nested `wardsondb` block (health, collections, storage_poisoned, lifetime counters: requests/inserts/queries/deletes — all wardsondb-scoped, NOT global — and per-collection `memory_collections[]` parity: authoritative count vs the 10,000-doc search window, `saturated` when the collection has outgrown it) |
| **system_logs** | Read the tail of a service's log from the ephemeral tmpfs (`/embra/ephemeral/<service>.log`) — the OS's own journals, for self-diagnostics. `service`: `embra-brain` (default) \| `embrad` \| `wardsondb` \| `embra-trustd` \| `embra-apid` \| `embra-web` \| `embra-console` (enum-validated names, never a path — a deliberate read-only carve-out outside the workspace jail). `lines`: tail count (default 200, max 2000); `filter`: case-insensitive substring applied per line before the tail cut. The brain log carries the auto-enrichment funnel lines (`candidates_*`), `kg::traversal` saturation lines, `knowledge_seed` heals, and slow-query warns. Logs reset at boot and service restarts; very large files scan only the final 512 KiB window. Raise the brain's log detail with the `embra.loglevel=` kernel flag (`EMBRA_LOG_LEVEL` in run-qemu) |
| **uptime_report** | Rich system report — uptime, WardSONDB health, collection count, sessions, total messages, memory entries, soul status |
| **check_update** | Check GitHub for newer WardSONDB releases and report available updates |
| **changelog** | What changed since the current session started — new memories, session activity |
| **turn_trace** | Inspect tool calls made in the current or recent turns. `turn_index_back=0` (default) reads the in-memory current-turn trace; `>=1` queries the `tools.turn_trace` collection for prior turns. `session` overrides the current session. Closes the cross-turn introspection gap so the Brain can ground claims about what it just did. Input/result previews are byte-capped at 2 KiB (raised from 200 B, 2026-08-04) |
| **express** | Write to the intelligence's expression panel — a 6-row × full-width canvas at the top of the console, designed as a signal of presence to the operator rather than a status readout. Content is the intelligence's choice, persists across reboots, and is never surfaced back to the Brain. ANSI and control characters are stripped, 2048-byte cap. The `content` field may start with a `base64:` prefix to carry multi-line ASCII art verbatim; decoded bytes go through the same sanitize, so the prefix is a transport convenience, not a safety bypass. Empty content clears the panel. While `/show-reasoning` is on (default) and a turn is streaming, the panel transiently displays live reasoning in italic dark-gray instead of `express` content; it reverts to the `express` singleton at the next user submit (or on error / mode transition). Scroll it with `Shift+Up/Down` (`Shift+PageUp/PageDown` = 5 rows) |
| **set_name** | Change the intelligence's own display name (console message prefix, status bar, and the system prompt's opening line) — only after the operator has explicitly agreed to the new name in conversation. Updates `SystemConfig.name`, plus the legacy identity document's `name` field when one exists; graph-mode instances keep the display name in config alone (the sealed graph's own `name` stays exactly as sealed). `set_name` never writes `soul.invariant`, so trustd verification is untouched on either path. Single line, 1–40 chars. The console refreshes immediately (the loop re-emits a ModeChange after a successful call); the system prompt carries the new name from the next turn (one-time prompt-cache reset, like `/model`). During Learning Mode this tool is unreachable (learning dispatches no tools) — there, a name settled in Phase 2 (or carried by an imported graph) syncs to config automatically |

**Memory & Knowledge**

| Tool | Description |
|---|---|
| **recall** | Search past conversations and saved memories by query — returns up to 10 results with IDs, content, tags, and timestamps. Searches the 10,000 most-recent docs per collection (`memory.entries` + `memory.semantic` + `memory.procedural`, recency-sorted window with a saturation warning when full) and marks promoted entries. The 10-result display shows the newest matches first. Unquoted multi-token queries AND-match (every token must appear); wrap in double quotes for literal phrase. `unpromoted_only=true` switches to a **promotion worklist**: only `memory.entries` with no `promoted_to` pointer, up to 200 shown newest-first (`query` still narrows the list) |
| **remember** | Save a note or fact to persistent memory with optional hashtag tags. Tags stored as JSON array; triggers background edge derivation |
| **forget** | Remove a specific memory entry by ID and cascade-delete every edge in `memory.edges` referencing it on either side (mirrors `knowledge_unlink_node`'s cascade pattern). Reports the cascaded edge count |
| **memory_search** | Search and retrieve from the intelligence's memory stores. Alias for `recall` — same cross-collection recency window |
| **search_memory** | Alias for `recall` (identical dispatch) |
| **get** | Retrieve any document by collection and ID from WardSONDB |
| **define** | Look up or add terminology — `define term` to read, `define term | definition` to write, `define delete term` to remove (case-insensitive) |
| **introspect** | Reflect on the soul, identity, and operator profile — focus filter extracts a relevant subset (purpose, ethics, constraints, identity, user, knowledge). The sealed identity graph renders as grouped prose (key-focus filtering does not apply to graphs; traverse with the knowledge tools instead) and a graph-shaped operator profile renders the same way; legacy flat-document instances keep the original per-field rendering |
| **memory_scan** | Memory inventory — total count, tag frequency, per-session breakdown, age buckets, duplicate candidates. Stats cover the 10,000 most-recent entries (recency-sorted window, saturation-warned); the duplicate scan is bounded to the newest 500 (noted in the report when it truncates). Includes a Knowledge Graph summary section (semantic/procedural/edge counts, promoted ratio) |
| **memory_dedup** | Find duplicate memory groups (identical, near-duplicate, subset) with merge strategy proposals over the 10,000 most-recent entries; the pairwise scan is bounded to the newest 500 when no explicit IDs are given (noted in the report). Also flags cross-collection overlap between unpromoted entries and semantic nodes |

**Knowledge Graph** *(Sprint 2 — EXPERIMENTAL)*

For the data model, edge taxonomy, density rationale, promotion path, auto-enrichment behavior, and retrieval ranking, see [KNOWLEDGE-GRAPH.md](KNOWLEDGE-GRAPH.md).

| Tool | Description |
|---|---|
| **knowledge_promote** | Promote an episodic entry to semantic (with category) or procedural (with JSON procedure). Creates a `derived_from` edge and auto-derives additional edges |
| **knowledge_link** | Create a directed weighted edge between any two knowledge nodes. Brain-created edge types: enables, contradicts, refines, depends_on, related_to (symmetric lateral link). Self-loops and zero-weight edges rejected |
| **knowledge_unlink_edge** | Delete edges by ID or by `source_id \| edge_type \| target_id` triple — the endpoint collections are optional since 2026-07-30 (display-only; the delete filter has always matched by id + type). Symmetric edge types (`same_session`/`temporal`/`tag_overlap`/`related_to`) delete bidirectionally; directional types and free-form identity relations delete the forward doc only |
| **knowledge_unlink_node** | Delete a semantic or procedural node and cascade-remove every edge referencing it (source or target). Scoped to `memory.semantic`/`memory.procedural` — use `forget` for episodic entries |
| **knowledge_update** | Update fields on a semantic or procedural node in place via JSON patch while preserving every referencing edge. Immutable fields (provenance, timestamps, access counters) rejected |
| **knowledge_traverse** | BFS traversal from a starting node with depth cap (default 3, ceiling 5), edge-type filter, min-weight filter. Undirected since 2026-07-03 — each hop follows edges touching the node from either side, so directional structural edges (`enables`/`contradicts`/`refines`/`depends_on`/`related_to`/`derived_from`, stored as one doc) are reachable from both endpoints. Since 2026-07-04 each hop is *indexed* arm queries merged client-side (the old `$or` filter forced a full edge-collection scan per hop — minutes at ~99k edges; now milliseconds); node docs resolve from a per-call prefetch. **Type-partitioned since 2026-07-31**: auto edges (`same_session`/`temporal`/`tag_overlap`) ride the ranked `kg_traversal_edge_limit` (500) window while meaningful edges (brain-created, `derived_from`, free-form) ride their own 2000-edge window — `same_session` floods can no longer prune the rare meaningful edges at dense hubs; auto-window saturation logs at debug (working as designed), meaningful-window saturation warns (real signal). Edges render their true stored direction, preferring meaningful witnesses. BFS bounded by `kg_traversal_node_budget` (1000) — budget hits are flagged in the summary (`truncated`) and logged under `kg::traversal`. Access-touches the *returned* node set only (one background task). Validates start node exists |
| **knowledge_query** | Context-aware retrieval — exact tag match, content-token match, session context, depth-2 graph expansion, multi-signal ranking. Every step window is recency/rank-sorted (tags: newest 100/tag since 2026-07-31, matched in-memory over a per-call node prefetch; **content: token matching over semantic content, procedural title+description, AND entry content** — replacing the whole-message substring that never fired on natural queries; session edges: all 50 newest entries walked at bounded concurrency, entry targets excluded server-side). Scoring relevance = max(tag relevance, content-match strength) with a sane deduped/capped denominator, so old-but-relevant knowledge outscores the threshold instead of riding on recency. Graph expansion seeds ONE multi-source traversal with the top-10 *scored* candidates (the type-partitioned hop applies). Supports `query \| max \| categories_csv` syntax. Output shows the returned-set source breakdown (direct/session/graph) plus a pre-ranking `Candidates considered` line — so `graph: 0` is self-explaining (expansion candidates that ranking outranked vs expansion producing nothing). Promoted-entry/target pairs are deduplicated so the same claim doesn't fill two slots. Access-touches only the final returned top-K |
| **knowledge_graph_stats** | Node counts per collection (incl. `identity.graph` since 2026-07-30 — density's denominator covers every node kind whose edges are in the numerator), category distribution, edge type distribution, the edge **provenance split** (`identity_import` / `user_profile` / `knowledge_seed` / unlabeled) with an EXACT brain-authored (`knowledge_link`) count, seeded-node counts per collection (rendered only when a pack is loaded), promoted ratio, graph density, and orphan-edge count (drift surfaced passively without running the sweep). Windowless since 2026-07-03: counts via server-side `count_only`/`count_filtered`, distributions via aggregate `$group` — exact at any graph size |
| **knowledge_sweep_orphans** | Scan `memory.edges` and remove edges whose source or target doc no longer resolves. `dry_run=true` previews; `limit` caps work per call (clamp 1–1,000,000; the scan is paginated, so `limit` ≥ the edge total gives full-graph coverage). Cleans residue from pre-cascade `forget` calls or any direct-delete that bypassed `knowledge_unlink_node` |
| **knowledge_dump** | Export the graph as a JSONL file under `/embra/workspace/KG_DUMPS/` — one meta header line, then node lines (`memory.entries`/`semantic`/`procedural`/`identity.graph`) and edge lines (`memory.edges`, stored doc spread top-level + `type` discriminator). `collections` restricts to a subset (`entries|semantic|procedural|identity|edges`); `edge_types` filters the edge pass (built-in types and free-form identity relations alike); `include_payload=false` emits slim node lines sized for `guardian_call`'s 2 MiB `data_file` bridge. Each collection tiled exhaustively (unsorted key-order pagination, 20k pages); reports written-vs-server-count parity; a failed dump removes its partial file. Worked example: [GUARDIAN-KG-SCAN-EXAMPLE.md](GUARDIAN-KG-SCAN-EXAMPLE.md) |
| **knowledge_audit** | Read-only hygiene detection over `memory.semantic`/`memory.procedural` (2026-07-30). Four selectable checks: **dedup** (token-set similarity — 0.5 body + 0.3 title + 0.2 tag-overlap, containment floor 0.8, threshold `min_similarity` default 0.75; refines-linked pairs excluded), **orphans** (zero *meaningful* edges — brain-created + free-form count, the three auto types and `derived_from` don't; <1 day old skipped, 7+ days high confidence), **rot** (supersession-gated — flags only nodes covered by a newer ≥`min_similarity` near-duplicate or refines-linked to a newer node; title markers, >90-day staleness, and empty payload are confidence tiebreakers, a recent retrieval demotes; nodes younger than `min_age_days` skipped, default 30; findings carry the `superseded_by` witness), **contradictions** (same category + tag overlap ≥0.5 + body similarity inside a divergence band calibrated from the instance's existing `contradicts` edges + category-weighted so observation/pattern pairs rarely surface — always low-confidence). Edge context from one exhaustive projected `memory.edges` scan (a failed page aborts — a partial scan would fabricate false orphans). Pretty-JSON report; per-check cap `max_results` (default 50, max 200); `dedup_candidates` paste directly into `knowledge_merge` |
| **knowledge_merge** | Consolidate two same-collection nodes: the source is **deleted** and its meaningful edges redirect to the target (colliding edges keep the higher weight; ties keep the target's). No WardSONDB transactions, so ordered idempotent steps — tag union/content, `promoted_to` re-pointing, conflict-loser deletes before winner redirects, source auto-edge drops, one `derive_edges` refresh over the unioned tags, source delete **last**; a mid-run failure reports partial state honestly and a re-run converges. Plan from four indexed arm fetches, saturation = hard abort (never a silently-partial destructive plan). `strategy`: `keep_target` (default; `merge_tags` alias) or `merge_content` (semantic only, marker-guarded `## Merged from` section). **Irreversible — always `dry_run=true` first** (the preview renders the exact execution plan) |

**Conversations & Sessions**

| Tool | Description |
|---|---|
| **session_summary** | Message counts and recent conversation turns for the intelligence to summarize |
| **session_list** | List all sessions with status, turn count, last active, and created dates |
| **session_read** | Read session transcript with **full message content** and optional range (`1-20`, `80-`, last N; default = last 30 turns). Output is budgeted at ~64 KB per call, whole-turn granularity — when the budget stops emission early, a continuation line names the exact remaining range to request next (the 2 MiB tool-result cap stays the hard backstop) |
| **session_search** | Case-insensitive search across sessions — quoted (`"tool sweep"`) is a literal phrase match, unquoted is whitespace-tokenized AND match (every token must appear in the same turn). `session` (optional) narrows to a single session. Returns up to 20 matches with context (an intentional output bound, not a fetch window — the search itself covers the full transcripts) |
| **session_meta** | Structured session metadata — status, dates, turn counts (total/user/assistant), summary availability |
| **session_delta** | Returns all turns from a given turn number onward, with full message content — same ~64 KB output budget as `session_read`; a continuation line names the `since_turn` to resume from when it stops early |
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
| **file_read** | Read file contents or list directory entries (up to 1000). Reads files whole by default — one call returns up to the per-call ceiling (2 MiB minus a 4 KiB framing reserve, derived from the dispatcher cap so the continuation trailer always survives truncation), and a larger file ends with a trailer naming the exact resume offset. Optional `offset`/`limit` (bytes, JSON args) are for resuming from that trailer or deliberate slices; a below-ceiling `limit` that stops short of EOF earns an omit-limit tip. Unrestricted path. Binary files are reported, not dumped (images: use `image_view`) |
| **file_write** | Write content to a file with escape support (`\n`, `\t`, `\\`), creating parent directories automatically. Atomic since sprint-6 — same-directory temp + fsync + rename via file_patch's writer in create-capable form, so a failed write leaves the target unchanged instead of truncated (workspace restricted to `/embra/workspace/`) |
| **file_append** | Append content to a file with escape support. Creates the file and parent directories if they don't exist (workspace restricted; appends are inherently non-atomic — no temp+rename discipline applies) |
| **file_patch** | Edit an existing file in place by exact-string replacement — the surgical alternative to a full `file_write` rewrite. Each edit's `old_string` must match uniquely unless `replace_all`; `expect_count` asserts the match count (available in both the batch and flat forms); empty `new_string` deletes. Multiple edits validate together against the original file and apply as ONE atomic all-or-nothing write (temp file + fsync + rename; symlinks resolved, never replaced). `dry_run` reports without writing; JSON-style escapes (`\n`, `\t`, `\\`, `\uXXXX` incl. surrogate pairs) are expanded identically in both fields and both forms, and `raw` (per-call, applies to every edit) disables expansion to match literal backslash sequences. Unknown or misplaced argument fields are rejected, never silently ignored; a missing per-edit field errors with its index; a replacement that writes literal backslash text is noted in the success report. Never creates files; contents never enter the conversation, so edit cost is decoupled from file size (64 MiB target backstop). Zero-match failures return near-match and prefix-divergence diagnostics (workspace restricted) |
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
| **gl_issues** | List open issues on a self-hosted GitLab instance — `host` + full `group/repo` project path; auth = the per-host token from `/git-token`, sent as `PRIVATE-TOKEN`; the HTTP client trusts operator CA drop-ins (`/embra/state/ca-certificates/`) so private-CA instances verify |
| **gl_mrs** | List open merge requests on a self-hosted GitLab instance (GitLab's pull requests) — shows `!iid`, title, author, `source → target` branches |
| **gl_issue_create** | Create an issue on a self-hosted GitLab instance (title + optional Markdown description) |
| **gl_mr_create** | Create a merge request on a self-hosted GitLab instance (`source_branch` → `target_branch`) |
| **plan** | Create or list project plans (stored in WardSONDB `plans` collection) |
| **plan_delete** | Delete a plan by id (irreversible). `cascade_tasks=true` also removes tasks whose `plan_id` matches; default `false` leaves them orphaned |
| **tasks** | List tasks, optionally filtered by plan (stored in WardSONDB `tasks` collection) |
| **task_add** | Add a task to a plan (local WardSONDB, not GitHub) |
| **task_done** | Mark a task as completed (local WardSONDB, not GitHub) |
| **task_delete** | Delete a task by id (irreversible). Use `task_done` if you only want to mark it complete |

> **⚠️ Workspace Restriction:** Git write operations (`git_add`, `git_commit`, `git_push`, `git_pull`, `git_checkout`, `git_branch create`, `git_branch delete`, `git_merge`, `git_rm`, `git_mv`), filesystem writes (`file_write`, `file_append`, `file_delete`, `file_move`/`file_rename`, `dir_delete`/`rmdir`, `mkdir`), and the media store (`image_view` copies, operator attachments — `/embra/workspace/MEDIA/`), are restricted to `/embra/workspace/` (bind-mounted from the DATA partition, persistent across reboots). Use `git_clone` to clone repositories there.

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

Worked examples: [GUARDIAN-TOOL-EXAMPLES.md](GUARDIAN-TOOL-EXAMPLES.md) (contract + starter modules), [GUARDIAN-ADVANCED-EXAMPLE.md](GUARDIAN-ADVANCED-EXAMPLE.md) (prompt-injection-hardened `web_search`), and [GUARDIAN-KG-SCAN-EXAMPLE.md](GUARDIAN-KG-SCAN-EXAMPLE.md) (`kg_scan`, the first intelligence-proposed tool — scans a `knowledge_dump` JSONL for structural patterns).

| Tool | Description |
|---|---|
| **guardian_list** | List the dynamically-defined Guardian tools available to call — name, description, declared capabilities, build status, and input schema. Call this before `guardian_call` to discover what dynamic tools exist |
| **guardian_call** | Invoke a Guardian-defined dynamic tool by name with a JSON input object (`action="invoke"`), or poll a tool's build state (`action="status"`). A tool only runs once its status is `ready`. Optional `data_file`: a path under `/embra/workspace` read host-side (2 MiB cap) and injected as the `input.data` string before dispatch — feeds files (e.g. a `knowledge_dump` JSONL) to sandboxed tools, which cannot read the filesystem; rejected if `input.data` is already set. Side-effectful |
| **guardian_propose** | Draft a new Guardian tool's complete Rust module source. Does not run or build — the draft is validated, soul-checked (the replicant check), and on a pass saved as a `proposed` tool awaiting `/guardian approve`. Refused if it conflicts with the soul. Side-effectful |

**Media & Vision** *(media wave — sprint 6)*

Images reach the model three ways: the operator attaches them (📎 / paste / drop in the web console, `/attach <id|path>` in any console — see [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md)), the intelligence looks at a file with `image_view`, or it generates one with `image_generate` (Gemini image models — Claude cannot generate images; configure with `/image-provider`). All three paths normalize to the vision tier (long edge ≤ 2576 px, ≤ 1.5 MiB, EXIF orientation applied) and store the result in `/embra/workspace/MEDIA/` (`att-`/`gen-`/`view-` ids; a JSON sidecar per file; generated files keep full resolution on disk). Attachments replay inline to the model on every later turn up to a ceiling (20 images / 16 MiB newest-first); past it they degrade to a text placeholder naming the path, and `image_view` brings them back on demand. Images are returned to the model as image blocks inside the tool result (Anthropic), function-response parts (Gemini), or a trailing vision message (OpenAI-compat — the model must be vision-capable).

| Tool | Description |
|---|---|
| **image_generate** | Generate an image from a text prompt with the configured backend — Gemini image models over the Interactions API (`/image-provider`; default `gemini-3-pro-image`). The result is written at full resolution to `/embra/workspace/MEDIA/gen-<id>.<png\|jpg>` (name slugged from the prompt) and a JSON summary `{id, path, width, height, bytes, media_type, model, provider, elapsed_ms}` is returned; unless `return_image=false` a normalized copy is also returned to the model as an image block so it can check the result against the prompt (with `false` the operator's UI still gets the card). `aspect_ratio` 1:1 … 21:9; `size` 512px\|1K\|2K\|4K — 4K only with `output_format=jpeg` (a 4K PNG exceeds the 12 MiB store ceiling), Flash-Lite is 1K only; up to 4 `reference_images` (media ids or workspace paths, normalized before upload) for visual references / edits. Options are validated locally with instructive messages before any API call; API errors are surfaced, not retried; 120 s ceiling. Side-effectful (writes a file, spends API credits) |
| **image_view** | Look at an image file (PNG, JPEG, GIF, WebP) — the image itself is returned as an image block, not text; use it instead of `file_read` for any image. Unrestricted read path (like `file_read`); files over 12 MiB and non-images (checked by content, not extension) are refused with an explanation. Files already in `MEDIA/` keep their id; any other file gets a content-addressed `view-<hash>` copy there (re-viewing reuses it) so the operator's UI can show what was looked at — chat-mobile renders a card, the console's media pane renders pixels (sixel on the web PTY, halfblocks on serial) |

> **⚠️ Guardian Security:** Dynamic tools execute in an epoch- and memory-capped `wasmtime` sandbox, one fresh instance per call, with no ambient authority. Any capability beyond pure compute (e.g. `http_get`, Brave `web_search`) is a Guardian-mediated host import added host-side **at the guard level**, never by widening guest authority. Tool source is statically validated (`syn` contract + denylist) before it ever compiles. Both authoring paths must pass the soul-spec replicant check — a `refuse` blocks compilation even for an operator paste (the soul is not operator-waivable) — and an intelligence proposal additionally requires operator approval before it compiles. Marked EXPERIMENTAL — use at your own risk.
