# Recommended Models for OpenAI-Compatible Providers

**Status:** Phase 1 stable since `v0.5.0-phase1` (2026-05-07). The models below are operator-vetted as full-toolset-capable (see also the README header callout for the same picks, surfaced for fresh GitHub readers); operator-overridable at wizard time.

The wizard's selector reads `GET /v1/models` from the configured server, so any pulled (Ollama) or loaded (LM Studio) model is selectable regardless of what's listed here.

Hardware mapping for the test fleet:

- **Ollama** runs on an **M1 Mac Mini, 16 GB unified memory**
- **LM Studio and Ollama** run on a **Mac Studio M4 Max, 128 GB unified memory**

---

## Vetted Models

The list is deliberately short. MoE models need a minimum active-parameter threshold for honest instruction-following: below ~27–49B active they become confabulation-prone under complex multi-step protocols — enough stored knowledge to sound authoritative, not enough working memory to track what they've actually done. Dense models have no total/active split, so their parameter count is honest. Both vetted picks clear the threshold: Qwen3.6 27B and Qwen3.8 27B are dense (27B = 27B active).

**Qwen3.8 27B** is the current recommendation; **Qwen3.6 27B** remains vetted and fully supported — there is no need to migrate an instance that is working. Each was vetted against the complete tool surface of its date, not a subset: Qwen3.6 27B on 2026-07-10 (95 tools), Qwen3.8 27B on 2026-09-08 (116 tools; 115 since `check_update` was removed). Both vettings predate the `guardian_call` fix of 2026-10-07 ([CHANGE-LOG.md](CHANGE-LOG.md)): before it, on Ollama, the input of a dynamic-tool invoke arrived as JSON text and never reached the tool as an object. The invoke is re-checked per server on the fixed build: TBD.

### Local (Ollama / LM Studio)

| Server | Model | Tag | Size | Context | Status |
|--------|-------|-----|------|---------|--------|
| Ollama | Qwen3.8 27B (dense) | `qwen3.8:27b` | 18GB | 256K | vetted |
| Ollama | Qwen3.6 27B (dense) | `qwen3.6:27b` | 18–19GB | 256K | vetted |
| Ollama | Gemma 4 12B (dense) | `gemma4:12b` | 7.7–8.0GB | 256K | under evaluation |
| Ollama | Gemma 4 31B (dense) | `gemma4:31b` | 19–20GB | 256K | under evaluation |
| LM Studio | Qwen3.8 27B (dense, 8-bit) | `qwen/qwen3.8-27b` | 30GB | 256K | vetted |
| LM Studio | Qwen3.6 27B (dense, 8-bit) | `qwen/qwen3.6-27b` | 30GB | 256K | vetted |

Sizes and context lengths are the Ollama library's own figures for the tags
listed, read 2026-10-08; a range is the library's, for a tag with more than one
build. The LM Studio sizes are the weights of the 8-bit MLX builds
(`lmstudio-community/Qwen3.8-27B-MLX-8bit` and `…/Qwen3.6-27B-MLX-8bit`, 29.5 GB
each). Gemma 4 12B and 31B are dense; the family's MoE is the `gemma4:26b` tag
(25.2B total, 3.8B active), below the active-parameter threshold above. The two
Gemma 4 rows are under evaluation by the operator and not yet vetted against the
tool surface. The 27B tags and `gemma4:31b` need more than the Mac Mini's 16 GB
of unified memory and run on the Mac Studio; `gemma4:12b` fits the Mac Mini. The
hardware mapping above says which *server* runs where, not that every model fits
on every host.

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

embra-brain sends no sampling parameters and no chat-template settings. Its request carries the model, the messages, the tool manifest, `stream` and `reasoning_effort` where the model documents it (`crates/embra-brain/src/provider/openai_compat/wire.rs`), so everything else is the server's own per-model configuration.

That is what the preset in [`LOCAL_MODEL_SETTINGS/LM-Studio-Examples/`](LOCAL_MODEL_SETTINGS/LM-Studio-Examples/) pins. `embraOS_Qwen3.8-27B` (import it in "My Models", then select it for the model) sets the three prediction settings that matter for a tool-calling session: `contextOverflowPolicy: stopAtLimit`, so the server stops at the context limit instead of truncating the prompt — a truncation drops part of the tool manifest or the history without a word; `repeatPenalty: 1`, which turns the penalty on repeated tokens off — tool-call JSON and code repeat tokens by nature; and `minPSampling: 0`, which turns min-p sampling off. It carries no load settings; the load configuration above stays as it is.

### Ollama (Mac Mini, Mac Studio)

Set context size and KV cache via launchd env vars if needed — `OLLAMA_CONTEXT_LENGTH`, `OLLAMA_FLASH_ATTENTION`, `OLLAMA_KV_CACHE_TYPE`, set all three together (see Ollama's OpenAI-compat note: `"The OpenAI API does not have a way of setting the context size"`).

The request is the same as for LM Studio: no sampling parameters and no `think` flag. Thinking follows `reasoning_effort` where the model documents it (`/effort`); everything else is the server's own configuration.

`num_ctx` is not a documented field on Ollama's OpenAI-compat endpoint (per [`ollama#7063`](https://github.com/ollama/ollama/issues/7063), still open since 2024-10-01); for locally-loaded models a Modelfile (`PARAMETER num_ctx`) works around it.

### Reasoning effort

Qwen3.8 accepts `reasoning_effort` `low` | `medium` | `xhigh` (default `xhigh`; `high` is not valid for this model). Set it with `/effort <level>` while the Ollama or LM Studio preset is active — the level is sent verbatim, the server validates it, and `/effort` warns when a level is outside the model's documented set. Unset (the default) sends no field, so the server's per-model default applies; `/effort reset` clears a stored level. Qwen3.6 takes no field at all. See [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md).

### Bearer auth

Both servers accept bearer tokens but neither validates them by default:

- **Ollama:** front the daemon with a reverse proxy (nginx, Caddy) that validates `Authorization: Bearer …`
- **LM Studio:** set `LM_API_TOKEN` env var to the expected value before starting the server

embraOS's wizard prompts for an optional bearer; supply the same token the server is configured to accept. Empty bearer means no `Authorization` header sent.

---

## Operator Override

The list is operator-overridable at wizard time. Switching models post-wizard runs `/provider --setup <ollama|lm_studio>` (Sprint 5 reconfigure flow added in commit `4eb57e9`). The same flow points either preset at any OpenAI-compatible server, local or hosted — vLLM, Together, Fireworks, OpenRouter: enter the base URL without `/v1` and the API key at the Bearer step; https endpoints keep their implicit 443.

---

## If a Model Gets Stuck

Local models occasionally fall into reasoning loops — one unbounded streaming
response that never finishes (the tool-iteration cap doesn't apply; it counts
tool round-trips, not tokens). Interrupt it with **`/stop`** (console: press
**Esc** while the turn streams; mobile: the **■** button that replaces Send).
Generation stops immediately — the connection to the Ollama/LM Studio server
is severed — and the partial response stays in history marked as interrupted.
See [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md).

---

*Last updated: 2026-10-08.*
