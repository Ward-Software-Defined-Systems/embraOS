# embraOS Change Log

Chronological merge log for `main`, anchored on tagged releases. Each entry's headline summary is pulled from the corresponding sprint section in [../ARCHITECTURE.md](../ARCHITECTURE.md) (local doc, gitignored) — that file remains canonical for design narrative; this file is the navigable release index.

**Last updated:** 2026-05-21.

---

## Unreleased — `main` since v0.5.0-phase1

`main` advanced past `v0.5.0-phase1` with seven post-Sprint-5 increments through 2026-05-21. Counts after the increments: 92 tools (90 → 92 at the embra-guardian-v1 merge for the `guardian_call`/`guardian_list` meta-tools); embra-brain 438 tests / embra-console 21 / embra-guardian 52; workspace 9 → 11 members; no version bump from `0.5.0-phase1`.

**`4f5db06` — structured + precedence soul/identity/profile rendering (2026-05-15).**

**embra-web web console (8 commits, 2026-05-16, fast-forward to `acb241e`).** Self-hosted HTTPS console on `:3345` that wraps the real serial TUI in xterm.js over a PTY→WebSocket bridge. Now the **default UI** (`run-qemu.sh` sets kernel cmdline `embra.web=1`; `EMBRA_TUI=1` → stable serial-TUI fallback, no rebuild). **Experimental.** 2 new crates (`embra-web` workspace member + workspace-excluded `embra-web-ui` Leptos/WASM); workspace 9 → 10 members; no proto/schema/registry change; +0 tests.

**embra-guardian-v1 (fast-forward 2026-05-17).** Operator-authored dynamic WASM tools pulled forward from Phase 2: `syn`-validated Rust module → in-OS prebaked Rust toolchain → `wasm32` → `wasmtime` sandbox, capability-broker host imports including `http_get` and Brave `web_search`, `guardian_call`/`guardian_list` meta-tool gateway, new `embra-guardian` crate #11. Followed by the README→OS-manual restructure chain (`e3f5643`→`f8cad9c`, including `24f66b4` curl apt-dep and `f8cad9c` Guardian natural-language phrasing).

**`f6a684a` — aarch64/Apple-Silicon dual-arch Buildroot parameterization (2026-05-19).** One committed Buildroot tree builds both x86_64 and aarch64. Both arches QEMU-verified by William 2026-05-19.

**`c33047f`→`bac1d08` — Intel Mac build guide + Docker-helper apt-get sync (2026-05-20).** New `docs/INTEL-MAC-BUILD.md` plus minimal-scope Docker-helper apt-get sync in `scripts/build-image.sh`. Full end-to-end verified on Intel MacBook Pro 16" (HVF accel, Buildroot 2026.02.1, fjall) including Config Wizard, soul-formation, Guardian dynamic-tool round-trip, EMBRA_TUI=1 serial mode, and reboot soul-verify.

**`b4e94d1`→`7a75a25` — embra-web modal focus return + embra-brain session-resume briefing (2026-05-21).** Modal focus-return Effect at App root watches palette/modal/editor state and calls `term::focus()` on any-open→all-closed. Session-resume briefing dispatch on `SessionAttach` AND the `/switch` slash command (the latter added in `7a75a25` after an operator reported `/switch` skipped the briefing).

**`981818c` — README header callouts (2026-05-21).** Two operator-facing callouts: local-inference model vetting (DeepSeek-v4-Pro:cloud + three Qwen variants confirmed full-toolset-capable) and "memory & knowledge graph today — operator-driven, by conversation" (manual today, automation near-term; `/feedback-loop` is soul/identity realignment, not memory promotion).

The `embra-web` and `embra-guardian-v1` feature branches were retired (deleted local + origin) 2026-05-18 once merged — both are fully in `main`. `embra-desktop` is now the only remaining separate workstream branch (`3588236`, forked from `69200e9`; 27 commits; NOT merged; built on `embra-desktop.ops.wsds`).

---

## v0.5.0-phase1 — 2026-05-07 (`69200e9`)

**Phase 1 declared stable. Sprint 5 OPENAI-COMPAT-PROVIDER-01 (Ollama + LM Studio) + REASONING-STREAM-01 follow-up.**

Adds Ollama and LM Studio as additional `LlmProvider` implementations via a single `OpenAICompatProvider` with `OpenAiCompatPreset::{Ollama, LmStudio}` discriminator. Schema v11 (idempotent `openai_compat` field stamp on `config.system:config`); STATE bearer plumbing at `/embra/state/bearer_<preset>` mode 0600; 4-way wizard with Endpoint → Bearer → Probe-and-Select sub-flow in `crates/embra-brain/src/setup/wizard.rs`. Defensive multi-key reasoning parser; `Block::ProviderOpaque{kind:"reasoning"}` CoT round-trip per OpenAI cookbook preserve/drop rules; harmony-token sanitization always-on. `/provider --setup <ollama|lm_studio>` 4-step post-wizard reconfigure with bearer hot-reload via env var. Model-aware `reasoning_effort` gating via `model_supports_reasoning_effort` heuristic.

REASONING-STREAM-01 follow-up (added 2026-05-06): streams provider reasoning / chain-of-thought deltas to the existing 6-row expression panel as they arrive. Anthropic via `display: "summarized"` (was `"omitted"`), Gemini via new `includeThoughts: true` on `thinking_config`, OpenAI-compat via existing `delta.reasoning` / `delta.reasoning_content` paths now also emitting `StreamEvent::ReasoningDelta`. Load-bearing privacy contract: `ReasoningDelta` MUST NOT enter `full_response` / `accum_text` / session history / IR round-trip Text blocks; provider signatures stay on `Block::ProviderOpaque` exactly as before. Default-on with opt-out via `SystemConfig.show_reasoning: Option<bool>` + `/show-reasoning <on|off>`. Operator-verified live across all four providers 2026-05-06.

90 ToolDescriptors unchanged. Operator-driven post-Stage-6 fix wave (8 commits 2026-05-04) covered live-smoke surprises: bearer step yes/no Selector, `tool_choice: "auto"` string form (LM Studio rejects Anthropic's object form), `properties: {}` stamp for parameterless schemas (LM Studio's zod validator rejects missing field), provider-aware operational-mode entry precheck, `reasoning_effort` omission for non-reasoning models. `docs/RECOMMENDED-MODELS.md` shipped as operator-facing starter doc with per-family reasoning-control matrix.

Full detail: see ARCHITECTURE.md (grep `Phase 1 Sprint 5 — OPENAI-COMPAT-PROVIDER-01` and `Phase 1 Sprint 5 follow-up — REASONING-STREAM-01`).

---

## v0.4.0-phase1 — 2026-04-25 (`c57d9cf`)

**Sprint 4 — GEMINI-PROVIDER-01: Pluggable LLM Provider + Gemini 3.1 Pro.**

The spec defines a provider abstraction (`LlmProvider` trait + neutral IR) so `embra-brain` can swap the Anthropic backend for Google's Gemini 3.1 Pro without touching the loop driver, prompts, sessions, or tools. Stages 1–10: provider trait + neutral IR; Anthropic refactor (zero behavior change); Gemini wire types + tool schema translator (UPPERCASE types, `$ref`/`definitions` inliner, `anyOf` supported, `oneOf`/`allOf` rejected); Gemini SSE streaming parser; HTTP driver + `LlmProvider` impl; Context Cache lifecycle (simplified — drop TTL-refresh-mid-turn, drop 4096-token threshold, 4-event telemetry); wizard provider step; `/provider` slash command + cross-provider session block (hard refuse via `SystemMessage::Error`); WardSONDB schema migration v9; supervisor wiring + status line.

Operator directives brought in-sprint: **D8** (`EMBRA_GEMINI_MODEL` override + per-turn `gemini::diag` telemetry) and **D2** (per-provider API keys, schema v10, `/provider --setup` multi-turn runtime key add). Six-commit post-smoke fix chain on `main` 2026-04-24/25: per-call key resolution (`/provider` swap was sending stale boot-time key), workspace 0.2 → 0.4 + `CARGO_PKG_VERSION` propagation, learning `CATEGORY_COUNTS` 85 → 90, `/help` refresh, `ModeTransition` emit on `/provider` swap, `Brain:` token in wizard+learning transitions, `SessionManager::create` stamps `meta.provider`, server-stale `cachedContent` 403/404 recovery. Post-merge hotfix `5470b37` (2026-04-26) repairs the cross-provider guard; post-merge fix `ebf8fec` (2026-04-28) replaces the silent `MAX_TOOL_ITERATIONS` break with a graceful capping sequence (synthetic `tool_result` for every undispatched call + final summary + `SystemMessage::Warning`); cap raised 10 → 100 per operator testing; adds `/iter-cap` runtime knob.

Also landed in this window: **PR #3 "Ubuntu 26.04 build"** (`2e66077`, 2026-05-02) — Buildroot 2024.02 → 2026.02.1 LTS bump for Ubuntu 26.04 host compat (GCC 15 `-std=gnu23`, libxcrypt split from glibc, M4-gnulib `[[nodiscard]]`, CMake 4.x).

90 ToolDescriptors (unchanged). 211 workspace tests (was 142 at Sprint 3 close).

Full detail: see ARCHITECTURE.md (grep `Phase 1 Sprint 4 — GEMINI-PROVIDER-01`).

---

## v0.3.0-phase1 — 2026-04-24 (`da469a6`, PR #2 merge)

**Sprint 3 — NATIVE-TOOLS-01: Native Tool-Use Migration + four post-merge passes.**

Migration from literal-tag `[TOOL:name args]` text dispatch to the Anthropic Messages API's native tool-use surface. Text channel and tool channel are now separated at the API layer via structured `tool_use` / `tool_result` content blocks; the FIX-01 cross-turn echo-via-context failure class is structurally impossible post-migration. Workspace grows 7 → 9 crates with two new members: `embra-tools-core` (shared `DispatchError` / `BoxFut` / `JsonValue`) and `embra-tool-macro` (proc-macro attribute `#[embra_tool(name, description, is_side_effectful?)]`). New tool registry hub at `crates/embra-brain/src/tools/registry.rs` with `inventory::collect!(ToolDescriptor)` autoregistration and a 2 MiB result cap.

Earlier Sprint 3 work also bundled into this release: WardSONDB pluggable storage engine (rocksdb / fjall, locked into DATA on first boot via `.engine` marker); EXPR-01 expression panel; tool-bug fix series (13 commits); FIX-01 residual diagnostic; tool coverage expansion (7 commits); unknown-tool dispatcher fix; post-verification bug triage.

Four post-merge passes since Sprint 3 PR #1 (`fcb8035`, 2026-04-22): 2026-04-23 tool-fix pass (9 Embra_Debug issues closed); 2026-04-24 pass #1 (arch-strip + #44/#45/#46/#49); pass #2 (#52/#53/#54/#55); pass #3 (#56/#57). 15 Embra_Debug issues closed in total. 90 ToolDescriptors (post-NATIVE-TOOLS-01 + tool-fix-pass additions). 142 workspace tests. QEMU E2E smoke verified across all three post-verification passes.

Full detail: see ARCHITECTURE.md (grep `Phase 1 Sprint 3 — NATIVE-TOOLS-01` and the four Sprint 3 post-merge pass markers).

---

## v0.2.0-phase1 — `phase1-arch-rework` pre-merge (`dda8c6c`)

**Phase 1 complete — Sprint 2 cross-session knowledge graph + post-sprint polish.**

Cross-session knowledge graph split across three WardSONDB collections plus an edge layer: `memory.entries` (episodic turns), `memory.semantic` (promoted facts/preferences/decisions/observations/patterns), `memory.procedural` (promoted structured procedures), and `memory.edges` (typed, weighted, directed edges with auto-derived `same_session` / `temporal` / `tag_overlap` and brain-created `enables` / `contradicts` / `refines` / `depends_on` / `derived_from`). Tools with prefix `knowledge_`: `promote`, `link`, `unlink_edge`, `unlink_node`, `update`, `traverse`, `query`, `graph_stats`. Auto-enrichment wraps the in-flight user message in a `<retrieved_context>` block when retrieval yields ≥1 result scoring ≥0.3. Post-sprint polish: auto-enrichment refinements, config-wizard validation, Opus 4.7 upgrade, empty-assistant guards.

Full detail: see ARCHITECTURE.md (grep `Phase 1 Sprint 2 — Cross-Session Knowledge Graph` and the preceding `Phase 1 Sprint 1` bug-fix pass).

---

## v0.1.0-phase0 — Phase 0 Sprint 5 Complete (`78a4f86`)

**Phase 0 proof-of-concept (Docker-containerized).**

Phase 0 implementation was a Docker-containerized proof-of-concept running `embrad` as the main process with WardSONDB as a child process. Single-container architecture; `embra-brain` not yet split out as a separate service; no inter-process gRPC; conversational terminal directly in the embrad process. Configuration via Config Wizard at first run, six-phase Learning Mode to seal the soul (serialized with `serde_json::to_string_pretty` and SHA-256-hashed), then a persistent conversational terminal with a brain backed by the Anthropic API.

WardSONDB collections established at Phase 0: `soul.invariant`, `config.system`, `memory.identity` / `memory.user` / `memory.entries`, `sessions.*.meta` / `history` / `summary`, `tools.registry`, `knowledge.definitions`, plus operational collections (`drafts`, `reminders`, `plans`, `tasks`, `crons`, `system.migrations`, `system.consolidation_log`).

Full detail: see ARCHITECTURE.md `🔵 Phase 0 Architecture` (starts line 1463).

---

## Initial commit — Phase 0 founding (`330631e`)

embraOS founding commit — initial continuity-preserving AI operating system Phase 0 implementation. Derived from the v3 design document (`embra.ai.agent Design v3`, 2026-03-13).
