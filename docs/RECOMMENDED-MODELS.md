# Recommended Models for OpenAI-Compatible Providers

**Status:** Active testing in progress. Sections marked TBD are pending operator validation.

This doc lists models actively tested for each OpenAI-compat preset (Ollama, LM Studio). Models are operator-overridable at wizard time — the wizard's selector reads `GET /v1/models` from the configured server, so any pulled or loaded model is selectable regardless of what's listed here.

Hardware mapping for the test fleet:
- **Ollama** runs on the **M1 Mac Mini, 16 GB unified memory**
- **LM Studio** runs on the **Mac Studio M4 Max, 128 GB unified memory**

---

## Server-Side Setup

### Ollama context size (Mac Mini, 16 GB)

**The OpenAI-compat path cannot set context size in the API request body** (verbatim from Ollama docs: *"The OpenAI API does not have a way of setting the context size"*). Set it server-side via env vars. The full triple is required together — setting only `OLLAMA_CONTEXT_LENGTH` without the other two will OOM at large contexts.

The setup path depends on how Ollama is installed:

**Mac — Ollama.app (.dmg installer or `brew install --cask ollama`):**

```bash
launchctl setenv OLLAMA_CONTEXT_LENGTH 131072   # 128K
launchctl setenv OLLAMA_FLASH_ATTENTION 1       # required for KV quantization to take effect
launchctl setenv OLLAMA_KV_CACHE_TYPE q8_0      # halves KV memory vs fp16; negligible quality cost
# Quit Ollama.app from menu bar, relaunch
```

**Mac — `brew services start ollama` (Homebrew-managed launchd service):**

Same `launchctl setenv` triple as above. The Homebrew launchd service inherits the user's launchd environment, so the env vars propagate. Restart with `brew services restart ollama`. (Note: `brew services restart` may regenerate the plist on some operations per Homebrew discussion #6196, but `launchctl setenv` persists at the launchd level independently.)

**Mac — `brew install ollama` running `ollama serve` manually in a terminal:**

`launchctl setenv` doesn't reliably propagate to interactive shells. Prefix the env vars directly on the command:

```bash
OLLAMA_CONTEXT_LENGTH=131072 \
OLLAMA_FLASH_ATTENTION=1 \
OLLAMA_KV_CACHE_TYPE=q8_0 \
ollama serve
```

Or add `export` lines to `~/.zshrc` and start a new shell before `ollama serve`.

**Linux (systemd):**

```bash
sudo systemctl edit ollama.service
# Add under [Service]:
#   Environment="OLLAMA_CONTEXT_LENGTH=131072"
#   Environment="OLLAMA_FLASH_ATTENTION=1"
#   Environment="OLLAMA_KV_CACHE_TYPE=q8_0"
sudo systemctl daemon-reload && sudo systemctl restart ollama
```

**Why all three together:** Without `OLLAMA_FLASH_ATTENTION=1`, KV cache quantization silently falls back to f16 (per `ollama/ollama#13337`) and OOMs at large contexts. Without `OLLAMA_KV_CACHE_TYPE=q8_0`, KV cache memory grows past what's available on 16 GB. Without `OLLAMA_CONTEXT_LENGTH=131072`, Ollama's 2 K default truncates embraOS's request.

**Per-model fitting:** the env var sets the server-wide ceiling, but actual usable context depends on model weights + KV cost. On 16 GB:
- 8B-class models (Granite 4.1, Hermes 3, Qwen2.5-Coder): full 128 K fits comfortably (~3 GB KV @ q8_0)
- Dense 14B models (e.g. Qwen3:14b): practical ceiling ~32 K — KV grows fast on dense 14B; setting 128 K won't make 128 K usable

**Verify after restart:** `ollama ps` while a model is loaded should show context size matching the env var (not 4096 / 2048).

### LM Studio context size (Mac Studio, 128 GB)

LM Studio loads models with whatever context size the operator chose at load time. Set context per-model in LM Studio's "My Models" → model config or in the Server tab's model configuration. embraOS surfaces a warning if the wizard's expected context exceeds what the model is loaded with.

For Qwen3.6 / Qwen3-Coder family models supporting 256 K native, set context to 262144 in LM Studio's load config.

### Bearer auth

Both Ollama and LM Studio accept bearer tokens but neither validates them by default. For auth:

- **Ollama:** front the daemon with a reverse proxy (nginx, Caddy) that validates `Authorization: Bearer …`
- **LM Studio:** set `LM_API_TOKEN` env var to the expected value before starting the server

embraOS's wizard prompts for an optional bearer; supply the same token the server is configured to accept. Empty bearer means no `Authorization` header sent.

---

## Active Testing Roster

Models selected for embraOS smoke-testing, ordered by recommendation priority. The **Result** column gets populated as operator testing confirms behavior against the 90-tool registry. All entries below are pre-validation — none have been embraOS-confirmed against the full tool surface yet.

### Ollama (Mac Mini, 16 GB)

| Pick | Model | Tag | Size | Native ctx | Rationale | Result |
|---|---|---|---|---|---|---|
| 1 | IBM Granite 4.1 8B | `granite4.1:8b` | ~5 GB (Q4_K_M) | 128 K | OpenAI-tool-format-native (lowest wire-format risk on Sprint 5's OpenAI-compat path); BFCL V3 = 68.27; modern training (Oct 2025+) | TBD |
| 2 | Hermes 3 Llama 3.1 8B | `hermes3:8b` | ~4.7 GB (Q4_0) | 128 K (Llama 3.1 + YaRN) | Highest measured 8B-class tool-call reliability (~88% single-turn / ~91% valid JSON per BFCL v3 May 2026); uses `<tool_call>` chat-template tags — verify Ollama bridges to `delta.tool_calls` before relying | TBD |
| 3 | Qwen2.5-Coder 7B | `qwen2.5-coder:7b` | ~4.5 GB (Q4_K_M) | 32 K (128 K with YaRN) | Established option; function call template updated March 2025; HumanEval 88.4% | TBD |
| — | Qwen3 14B | `qwen3:14b` | ~9 GB (Q4_K_M) | 32 K | Currently in test. Default-thinking-on (needs `/no_think` per prompt). KV grows fast on dense 14B — practical ceiling ~32 K on 16 GB regardless of `OLLAMA_CONTEXT_LENGTH` setting. Lower tool-call quality vs the 8B-class tool-tuned alternatives above | In progress |

### LM Studio (Mac Studio, 128 GB)

| Pick | Model | Hugging Face tag | Size | Native ctx | Rationale | Result |
|---|---|---|---|---|---|---|
| 1 | OpenAI gpt-oss 120B | `mlx-community/gpt-oss-120b-4bit` | ~60–65 GB (MLX 4-bit) | 128 K | OpenAI-trained for tool calling, native OpenAI tool format (lowest wire-format risk); MMLU-Pro 90.0%; Apache 2.0 license | TBD |
| 2 | Qwen3-Coder 30B-A3B | `lmstudio-community/Qwen3-Coder-30B-A3B-Instruct-MLX-4bit` | ~17 GB (MLX 4-bit) | 256 K (1 M with YaRN) | "Most agentic code model in the Qwen series"; 30 B / 3.3 B active MoE = inference cost of a 3 B model; designed for repository-scale tool use | TBD |
| 3 | Qwen3.6 35B-A3B | `unsloth/Qwen3.6-35B-A3B-UD-MLX-4bit` | ~20 GB (MLX 4-bit) | 256 K | Existing reference from earlier Sprint 5 testing pass; default-thinking-on (use `/no_think` for clean tool-test passes) | TBD |

**Cross-platform caveats:**
- "<14 B models prone to issues for tool calling" per multiple 2026 surveys, with explicit exceptions for tool-tuned models (Hermes 3, Granite 4.1 fit the exception; vanilla Qwen2.5/Llama 3.1 8B do not)
- Format risk varies: gpt-oss + Granite are clean OpenAI format; Hermes uses `<tool_call>` tags (Ollama bridges); Qwen3 family is `/think` / `/no_think` prompt-toggled
- None of the above are validated against embraOS's 90-tool registry specifically — operator testing populates the Result column

---

## Operator Override Notice

Both lists are operator-overridable at wizard time. The wizard's model selector reads `GET /v1/models` from the configured server and presents whatever's actually available. Any pulled (Ollama) or loaded (LM Studio) model is selectable regardless of what this doc lists.

Switching models post-wizard runs `/provider --setup <ollama|lm_studio>` (Sprint 5 reconfigure flow added in commit `4eb57e9`).

---

## Update Cadence

This doc updates as new models pass operator testing. To add an entry to the testing roster:

- Model id (the exact string `GET /v1/models` returns)
- Quant + size on disk
- Tool-calling reliability observation (≥ 100 turns of testing recommended)

---

*Last updated: 2026-05-05. Update as new models pass operator testing.*
