# Recommended Models for OpenAI-Compatible Providers

**Status:** May 2026 starter snapshot — not a contract. Updated independently of the spec; never enforced in code.

This doc tracks operator-recommended models for each OpenAI-compat preset (Ollama, LM Studio) shipping under Sprint 5 OPENAI-COMPAT-PROVIDER-01. The list reflects testing on reference hardware as of the Sprint 5 close. Models are operator-overridable at wizard time — the wizard's selector reads `GET /v1/models` from the configured server, so any pulled or loaded model is selectable regardless of what this doc lists.

---

## Reference Hardware

- **Mac Studio M4 Max, 128 GB unified memory.** Sufficient for ≤ 35B-A* models in 4-bit MLX quants (≤ ~24 GB on-device); larger models run but with thermal/throughput caveats.
- Recommendations apply to this class of hardware. On 64 GB or 32 GB systems prefer the secondary recommendations.

---

## Server-Side Setup Notes

### Ollama context size

Ollama's default `num_ctx` is **2048** regardless of the model's native max context — this is an intentional hardware safety floor that catches operators by surprise. embraOS sends 8–11 k system prompt tokens plus tool history; a 32 k+ context is recommended.

**The OpenAI-compat path cannot set `num_ctx`** (verified verbatim from Ollama docs: *"The OpenAI API does not have a way of setting the context size"*). Operators must extend it via a Modelfile on the server side:

```bash
cat > Modelfile <<'EOF'
FROM gpt-oss:20b
PARAMETER num_ctx 32768
EOF
ollama create gpt-oss-32k -f Modelfile
```

Then pick `gpt-oss-32k` in the embraOS wizard's model selector. Repeat per model.

### LM Studio headless

For Mac Studio servers running headless (no GUI session), prefer the **`llmster` daemon** over the desktop app:

```bash
curl -fsSL https://lmstudio.ai/install.sh | bash
llmster service install     # systemd-style on Linux; launchd on macOS
llmster server start
```

The desktop app also works fine when an interactive session is available.

### Bearer auth

Both Ollama and LM Studio accept bearer tokens but neither validates them by default. Operators wanting auth should:

- **Ollama:** front the daemon with a reverse proxy (nginx, Caddy) that validates `Authorization: Bearer …`.
- **LM Studio:** set `LM_API_TOKEN` env var to the expected value before starting the server.

embraOS's wizard prompts for an optional bearer; supply the same token the server is configured to accept. Empty bearer means no `Authorization` header sent.

---

## Ollama Recommendations

### Primary: `qwen3.6:35b`

- **Size:** ~24 GB (35B-A3B MoE, 4-bit Q4_K_M)
- **Context:** 256 K native (set via Modelfile per above)
- **Tool calling:** Native via the `qwen3_coder` parser; reliable on the OpenAI-compat path
- **Quant note:** **Prefer Q4_K_M tags over IQ-quants** on Mac M4 Max. Per llama.cpp issue #21655, IQ-quants exhibit a ~3.8× slowdown on Apple Silicon Metal due to a regression that hasn't landed a fix as of this writing. Q4_K_M is the safe default.

### Secondary: `gpt-oss:20b`

- **Size:** ~16 GB (native MXFP4)
- **Context:** 128 K native
- **Tool calling:** Native; gpt-oss is OpenAI's launch-partner model for the Ollama relationship and gets first-class support
- **Reasoning:** Configurable `reasoning_effort` (`low` / `medium` / `high` / `none`) — embraOS sends `"high"` per Locked Decision #4
- **CoT round-trip:** Required (reasoning content via `delta.reasoning` field). embraOS handles automatically per Locked Decision #10.
- **License:** Apache 2.0
- **Use case:** Faster iteration than the 35B Qwen, lighter memory footprint; preferred when running other workloads alongside.

### Available but not primary: `gpt-oss:120b`

Runs on M4 Max but performance is constrained — expect noticeable latency on tool-call-heavy workloads. Useful for occasional comparison testing; not recommended for regular operational use on this class of hardware.

### Avoid (as of May 2026)

- **`qwen3.5:122b`** — Ollama tool-calling renderer/parser bugs (#14493, #14601, #14745) cause intermittent malformed tool calls. Re-check before pulling; fixes may have landed.
- **Older harmony-format models** running on pre-PR-#11759 Ollama versions can leak `<|channel|>analysis` tokens into tool-call name fields. embraOS's always-on harmony sanitization (Locked Decision #11) catches this with `tracing::warn!` telemetry, but the dispatch will still fail with "unknown tool" for the leaked name. Prefer current-version Ollama.

---

## LM Studio Recommendations

### Primary: `unsloth/Qwen3.6-35B-A3B-UD-MLX-4bit`

- **Size:** ~20 GB (35B-A3B MoE, 4-bit MLX)
- **Context:** 256 K native
- **Tool calling:** Strong via LM Studio's tool-calling middleware
- **Performance:** ~2× faster than Ollama llama.cpp on Apple Silicon thanks to MLX backend
- **Reasoning control:** **Prompt-based**, not OpenAI-parameter-based. Qwen3 has thinking integrated into the same model (no separate `-Thinking-` variant per Qwen team's blog at `qwenlm.github.io/blog/qwen3`). Default mode is **thinking-on**. Toggle per turn with `/think` or `/no_think` in user / system messages. **`reasoning_effort: "high"` is NOT honored** — LM Studio's middleware doesn't have custom KVs mapped for this model and logs `No valid custom reasoning fields found` if sent. embraOS's `model_supports_reasoning_effort` heuristic correctly omits the parameter for Qwen3 model IDs.
- **Thinking output handling:** Without LM Studio middleware to extract `<think>...</think>` blocks into `reasoning_content`, raw thinking text can leak into the visible `content` field. Operators who want clean responses should add `/no_think` to message prompts. Sprint 6 D-item: embraOS-side `<think>...</think>` detection + extraction into `Block::ProviderOpaque{kind:"reasoning"}` is a candidate enhancement.

### Secondary: `unsloth/Qwen3.6-27B-UD-MLX-4bit`

- **Size:** ~16 GB (27B dense flagship coding model, 4-bit MLX)
- **Context:** 256 K native
- **Use case:** Lighter than the 35B-A3B; preferred when running other workloads alongside.
- **Reasoning control:** Same as the 35B variant — `/think` / `/no_think` prompts, no OpenAI `reasoning_effort` mapping in LM Studio's middleware.

### Also Viable

- **`mlx-community/Qwen3.5-27B` 4-bit** — older but stable, well-tested with LM Studio's tool-calling middleware. A safer fallback if newer Qwen3.6 drops cause issues.

---

## Reasoning Controls Per Model Family

Different model families expose reasoning-mode controls through different mechanisms. embraOS sends `reasoning_effort: "high"` only to families that have it wired through; for everything else, the parameter is omitted and operators rely on prompt-based controls.

| Family | Control mechanism | embraOS sends `reasoning_effort` | Operator-side toggle |
|---|---|---|---|
| **gpt-oss** (`gpt-oss:20b`, `gpt-oss:120b`) | OpenAI `reasoning_effort` string param | ✅ `"high"` (Locked Decision #4) | Not needed — embraOS handles |
| **OpenAI o-series** (`o1-mini`, `o1-preview`, `o3-mini`, `o3-pro`, `o4-mini`) | OpenAI `reasoning_effort` string param | ✅ `"high"` | Not needed — embraOS handles |
| **DeepSeek-R1 / R2** | OpenAI `reasoning_effort` string param via Ollama | ✅ `"high"` | Not needed — embraOS handles |
| **Qwen3 family** (Qwen3, Qwen3.6, including `*-A3B` MoE) | **Prompt-based:** `/think` (default) and `/no_think` per-turn directives | ❌ omitted (LM Studio middleware doesn't bridge) | Add `/no_think` to messages to suppress thinking output |
| **Models with `-thinking` substring** (Claude-thinking variants, explicit Qwen-thinking builds, etc.) | Vendor-specific; embraOS treats as reasoning-aware | ✅ `"high"` (heuristic match) | Vendor docs |
| **Standard non-reasoning models** (Llama 3.x, Mistral, base Qwen2.5, Gemma) | None | ❌ omitted | Not applicable |

**Where the matrix comes from:** `provider::openai_compat::model_supports_reasoning_effort(model_id)` is the source of truth for "send vs omit" — case-insensitive substring match on `gpt-oss`, `o1-mini`/`o1-preview`/`o3-mini`/`o3-pro`/`o4-mini`, `-thinking`, `deepseek-r1`/`deepseek-r2`. Adding a new family is a one-line if-branch in `crates/embra-brain/src/provider/openai_compat/mod.rs`.

**Why Qwen3 is in the omit column even though it reasons:**
- Qwen3 supports thinking architecturally (per Qwen team's blog: "both thinking and non-thinking modes are integrated into the same post-trained models").
- Default mode is thinking-on. `/think` / `/no_think` toggles per turn.
- LM Studio's OpenAI-compat middleware does NOT translate `reasoning_effort: "high"` to Qwen3's internal toggle — sending it produces a `No valid custom reasoning fields found in model 'X'` warning in LM Studio's server logs and is silently dropped.
- So embraOS omits the parameter; operators control thinking via prompt directives.

**Reasoning content streaming:**
- gpt-oss / o-series / DeepSeek-R: server emits `delta.reasoning` (Ollama) or `delta.reasoning_content` (LM Studio newer default). embraOS's defensive multi-key parser handles both per Step 0 C1.
- Qwen3 via LM Studio without middleware: `<think>...</think>` blocks appear inline in `content`, not in `reasoning`/`reasoning_content`. embraOS treats this as visible content; thinking text leaks to the operator UI unless `/no_think` is used.
- Sprint 6 D-item candidate: embraOS-side `<think>...</think>` extraction into `Block::ProviderOpaque{kind:"reasoning"}` for clean separation regardless of server middleware.

---

## Operator Override Notice

Both lists are **operator-overridable at wizard time**. The wizard's model selector reads `GET /v1/models` from the configured server and presents whatever's actually available. Any pulled (Ollama) or loaded (LM Studio) model is selectable regardless of what this doc lists.

Switching models post-wizard requires re-running the wizard until Sprint 6 D1 lands a `/provider --select-model [<preset>]` runtime swap.

---

## Update Cadence

This doc updates as new models land or quants improve. The OPENAI-COMPAT-PROVIDER-01 spec is unchanged by these updates. To propose a new recommendation, file an issue with:

- Model id (the exact string `GET /v1/models` returns)
- Reference hardware tested on
- Quant + size on disk
- Tool-calling reliability observation (>= 100 turns of testing recommended)
- Comparison vs current primary recommendation

---

*Last updated: 2026-05-04 (Sprint 5 close + Qwen3 reasoning-control clarification).*
