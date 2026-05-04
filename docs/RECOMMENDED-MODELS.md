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
- **Reasoning:** `reasoning_content` field (newer LM Studio default per 0.3.23+ changelog); embraOS's defensive multi-key parser handles both `reasoning` and `reasoning_content` per Step 0 C1.

### Secondary: `unsloth/Qwen3.6-27B-UD-MLX-4bit`

- **Size:** ~16 GB (27B dense flagship coding model, 4-bit MLX)
- **Context:** 256 K native
- **Use case:** Lighter than the 35B-A3B; preferred when running other workloads alongside.

### Also Viable

- **`mlx-community/Qwen3.5-27B` 4-bit** — older but stable, well-tested with LM Studio's tool-calling middleware. A safer fallback if newer Qwen3.6 drops cause issues.

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

*Last updated: 2026-05-04 (Sprint 5 close).*
