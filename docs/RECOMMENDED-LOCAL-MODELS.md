# Recommended Models for OpenAI-Compatible Providers

**Status:** Phase 1 stable since `v0.5.0-phase1` (2026-05-07). The model below is operator-vetted as full-toolset-capable (see also the README header callout for the same pick, surfaced for fresh GitHub readers); operator-overridable at wizard time.

The wizard's selector reads `GET /v1/models` from the configured server, so any pulled (Ollama) or loaded (LM Studio) model is selectable regardless of what's listed here.

Hardware mapping for the test fleet:

- **Ollama** runs on an **M1 Mac Mini, 16 GB unified memory**
- **LM Studio** runs on a **Mac Studio M4 Max, 128 GB unified memory**

---

## Vetted Models

The list is deliberately short. MoE models need a minimum active-parameter threshold for honest instruction-following: below ~27–49B active they become confabulation-prone under complex multi-step protocols — enough stored knowledge to sound authoritative, not enough working memory to track what they've actually done. Dense models have no total/active split, so their parameter count is honest. The pick below clears the threshold: Qwen3.6 27B is dense (27B = 27B active).

### Local (Ollama / LM Studio)

| Server | Model | Tag |
|--------|-------|-----|
| Ollama | Qwen3.6 27B (dense) | `qwen3.6:27b` |
| LM Studio | Qwen3.6 27B (dense, 8-bit) | `qwen/qwen3.6-27b` |

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

Set context size and KV cache via launchd env vars if needed — `OLLAMA_CONTEXT_LENGTH`, `OLLAMA_FLASH_ATTENTION`, `OLLAMA_KV_CACHE_TYPE`, set all three together (see Ollama's OpenAI-compat note: `"The OpenAI API does not have a way of setting the context size"`).

embra-brain sends sampler params and Ollama's `think: true` flag in each request — operators don't configure these.

`num_ctx` is not a documented field on Ollama's OpenAI-compat endpoint (per [`ollama#7063`](https://github.com/ollama/ollama/issues/7063), still open since 2024-10-01); for locally-loaded models a Modelfile (`PARAMETER num_ctx`) works around it.

### Bearer auth

Both servers accept bearer tokens but neither validates them by default:

- **Ollama:** front the daemon with a reverse proxy (nginx, Caddy) that validates `Authorization: Bearer …`
- **LM Studio:** set `LM_API_TOKEN` env var to the expected value before starting the server

embraOS's wizard prompts for an optional bearer; supply the same token the server is configured to accept. Empty bearer means no `Authorization` header sent.

---

## Operator Override

The list is operator-overridable at wizard time. Switching models post-wizard runs `/provider --setup <ollama|lm_studio>` (Sprint 5 reconfigure flow added in commit `4eb57e9`).

---

*Last updated: 2026-07-14.*
