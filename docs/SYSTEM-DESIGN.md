# System Design

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

**Persistence:** [WardSONDB](https://github.com/ward-software-defined-systems/wardsondb) — a high-performance Rust JSON document database. It is the single durable store for runtime state: soul, memory entries, the knowledge graph, sessions, schedules, and Guardian dynamic-tool definitions.

**AI Model:** A pluggable LLM provider abstraction routes the Brain through one of four backends, selected at first boot and switchable at runtime via `/provider <kind>`:

- **Anthropic Claude Opus 4.7** (default) — requests sent with `output_config.effort=max` and `thinking.display=summarized` (`=omitted` when `/show-reasoning off`). Summarized thinking deltas stream live to the expression panel; signed thinking blocks still round-trip via `Block::ProviderOpaque` for the model's own continuity. Native ephemeral prompt caching with breakpoints on system + penultimate user message + last tool entry.
- **Google Gemini 3.1 Pro** — requests sent with `thinkingLevel=high`, `includeThoughts=true` (`false` when `/show-reasoning off`), and `maxOutputTokens=64000`. Thought-flagged parts stream live to the expression panel; thoughtSignatures stay on `Block::ProviderOpaque`. Native explicit context cache via a `GeminiCacheManager` singleton with `(session, fingerprint, TTL)` validation.
- **Ollama** (Sprint 5) — local or remote-style OpenAI-compat backend. `/v1/chat/completions` POST to the configured endpoint (default `http://localhost:11434`). Supports gpt-oss family (with `reasoning_effort: "high"`), DeepSeek-R1/R2, and standard non-reasoning models. Bearer authentication optional.
- **LM Studio** (Sprint 5) — local OpenAI-compat backend, default `http://localhost:1234`. Same wire shape as Ollama. Recommended for Apple Silicon hosts via the `llmster` daemon (~2× faster than Ollama on Mac M4 Max thanks to MLX backend).

The loop driver consumes a neutral intermediate representation (`Block::{Text, ToolCall, ToolResult, ProviderOpaque}` and `TurnOutcome::{EndTurn, ToolUse, MaxTokens, Pause, EarlyStop}`); each provider owns its own wire types, streaming parser, and tool schema translator. All 92 tools work identically across all four backends. Sessions are pinned to the provider that recorded them — cross-provider session attach is hard-blocked. Ollama and LM Studio share a single `OpenAICompatProvider` with a `ProviderKind` discriminator; future OpenAI-compat backends (vLLM, Together, Fireworks, OpenRouter) drop in as additional preset variants.

**Reasoning controls per family:**
- **gpt-oss / OpenAI o-series / DeepSeek-R1·R2 / `-thinking` variants** — embraOS sends OpenAI-compat `reasoning_effort: "high"` automatically (gated on `model_supports_reasoning_effort` heuristic).
- **Qwen3 family** (Qwen3, Qwen3.6, including `*-A3B` MoE) — thinking is integrated into the same model and toggled via `/think` and `/no_think` directives in user/system messages. `reasoning_effort` is omitted to avoid `No valid custom reasoning fields found` server warnings. See `RECOMMENDED-LOCAL-MODELS.md` for full per-family details.
- **Standard non-reasoning models** — no reasoning controls; embraOS omits all reasoning parameters.

**Bearer storage (OpenAI-compat):** STATE files at `/embra/state/bearer_<preset>` with mode `0600` (security retroactively applied to Anthropic/Gemini api_key files in Sprint 5). Per-call resolution from `EMBRA_OLLAMA_BEARER` / `EMBRA_LM_STUDIO_BEARER` env vars so post-swap turns pick up the new value without a brain restart.

**Prompt Caching:** embraOS uses each provider's native caching mechanism to minimize token costs.

*Anthropic — ephemeral prompt caching* (two cache breakpoints):

1. **System prompt** — the soul, identity, user profile, tool inventory, and instructions (~8-11k tokens) are cached on first call and hit cache on every subsequent call within the session.
2. **Conversation history** — a rolling breakpoint on the second-to-last message caches all prior turns. Only the newest user message is uncached.

Cache TTL is 5 minutes (ephemeral), refreshed on every hit. Active conversations keep the cache warm indefinitely — longer sessions get progressively cheaper per message.

*Gemini — explicit context caching* (one cache handle per session):

A `GeminiCacheManager` singleton stores one cached-content handle in WardSONDB at `provider.gemini_cache:current`. On each turn, the stored handle is validated by `(session, fingerprint, TTL)` and either reused (`cache:hit`), deleted-and-recreated (`cache:miss` — `session_changed` / `stale` / `expired`), or freshly created (`cache:create`). The fingerprint is `sha256(system_prompt || \x00 || tools_json)` truncated to 16 hex chars, so any soul/tool drift produces a clean miss. If `cachedContents.create` returns 4xx (Gemini 3.1 Pro Preview is not explicitly listed as caching-eligible in Google's docs), the call falls back to per-request `systemInstruction` + `tools` and the system continues to function. Server-side GC of a cache mid-session is detected at request time (`403/404 CachedContent not found`) and recovered with a single inline retry.

*Ollama / LM Studio — server-side keep-warm* (no client-side cache):

OpenAI-compat backends don't expose a caching mechanism on the wire. Ollama keeps the model warm via `OLLAMA_KEEP_ALIVE` (operator-configured server-side, transparent to embraOS); LM Studio handles resident model state internally. embraOS sends the full system + history on every request — the cost optimization happens server-side via the loaded model staying in GPU/Metal memory between turns. No mid-turn cache invalidation race because there's no cache handle to invalidate.
