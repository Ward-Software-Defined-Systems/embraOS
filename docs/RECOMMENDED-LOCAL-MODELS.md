# Recommended Models for OpenAI-Compatible Providers

**Status:** Active testing in progress. Models below are locked for Sprint 5 smoke-testing; operator-overridable at wizard time.

The wizard's selector reads `GET /v1/models` from the configured server, so any pulled (Ollama) or loaded (LM Studio) model is selectable regardless of what's listed here.

Hardware mapping for the test fleet:

- **Ollama** runs on the **M1 Mac Mini, 16 GB unified memory**
- **LM Studio** runs on the **Mac Studio M4 Max, 128 GB unified memory**

Both servers run the **Qwen3.x family** for behavioral parity: same chat template, same thinking-mode semantics, same tool-call format, same `ProviderOpaque` thinking-block handling.

---

## Active Testing Roster

### Mac Studio M4 Max, 128 GB (LM Studio)

| Pick | Model | HuggingFace tag | Size | Native ctx | Role |
|------|-------|-----------------|------|------------|------|
| 1 | Qwen3.6 35B-A3B (MLX 8-bit) | `unsloth/Qwen3.6-35B-A3B-MLX-8bit` | ~37.7 GB | 262K | Embra-brain primary |
| 2 | Qwen3.6 35B-A3B (MLX 4-bit) | `unsloth/Qwen3.6-35B-A3B-UD-MLX-4bit` | ~21 GB | 262K | Fast-iteration / fallback |

**Notes:**
- Both ship with Unsloth's tool-calling fixes (nested object parsing) and Developer Role support.
- Multimodal model run text-only; vision tower unused by embraOS.
- 4-bit Pick 2 is ~30–40% faster inference at near-equivalent quality (Unsloth Dynamic 2.0). Use during active development; promote to 8-bit for production-class testing.

### Mac Mini M1, 16 GB (Ollama)

| Pick | Model | Tag | Size | Configured ctx | Role |
|------|-------|-----|------|----------------|------|
| 1 | Qwen3.5 9B | `qwen3.5:9b` | 6.6 GB (Q4_K_M) | 128K | Behavioral CI tier |

**Notes:**
- Native context is 256K; configured to 128K for 100% GPU on 16 GB (operator-verified). Sliding-window attention keeps KV cache small enough that 128K fits.
- Same Qwen3.x family as the Studio for behavioral parity. Mini test results are predictive of Studio behavior.
- **Not a deployment target.** This tier is for wire-format CI and behavioral smoke tests, not production embra-brain operation.

---

## Server Configuration

### LM Studio (Mac Studio)

Per-model load config in LM Studio's "My Models":

```
Context length:        262144
Flash attention:       enabled
KV cache:              f16
GPU offload:           Max
```

embra-brain sends sampler params and chat-template config (including `chat_template_kwargs.preserve_thinking: true` for KG continuity) in each request — operators don't configure these.

### Ollama (Mac Mini)

Set context size and KV cache via launchd env vars (see Ollama's OpenAI-compat note: `"The OpenAI API does not have a way of setting the context size"`).

```bash
launchctl setenv OLLAMA_CONTEXT_LENGTH 131072
launchctl setenv OLLAMA_FLASH_ATTENTION 1
launchctl setenv OLLAMA_KV_CACHE_TYPE q8_0
# Quit and relaunch Ollama.app
```

All three vars are required together. Without `OLLAMA_FLASH_ATTENTION=1`, KV cache quantization silently falls back to f16 (per `ollama/ollama#13337`).

embra-brain sends sampler params and Ollama's `think: true` flag in each request — operators don't configure these.

### Bearer auth

Both servers accept bearer tokens but neither validates them by default:

- **Ollama:** front the daemon with a reverse proxy (nginx, Caddy) that validates `Authorization: Bearer …`
- **LM Studio:** set `LM_API_TOKEN` env var to the expected value before starting the server

embraOS's wizard prompts for an optional bearer; supply the same token the server is configured to accept. Empty bearer means no `Authorization` header sent.

---

## Verification

After deploying both boxes:

```bash
# Mac Studio (LM Studio):
curl http://studio:1234/v1/models | jq '.data[].id'
# Expect: unsloth/Qwen3.6-35B-A3B-MLX-8bit and -UD-MLX-4bit

# Mac Mini (Ollama):
ollama pull qwen3.5:9b
ollama ps
# PROCESSOR column must show 100% GPU.
# If split, drop OLLAMA_CONTEXT_LENGTH (try 65536, then 32768).
```

---

## Operator Override

Both lists are operator-overridable at wizard time. Switching models post-wizard runs `/provider --setup <ollama|lm_studio>` (Sprint 5 reconfigure flow added in commit `4eb57e9`).

---

*Last updated: 2026-05-06.*
