# Recommended Models for OpenAI-Compatible Providers

**Status:** Phase 1 stable since `v0.5.0-phase1` (2026-05-07). The models below are operator-vetted as full-toolset-capable (see also the README header callout for the same picks, surfaced for fresh GitHub readers); operator-overridable at wizard time.

The wizard's selector reads `GET /v1/models` from the configured server, so any pulled (Ollama) or loaded (LM Studio) model is selectable regardless of what's listed here.

Hardware mapping for the test fleet:

- **Ollama** runs on an **M1 Mac Mini, 16 GB unified memory**
- **LM Studio and Ollama** run on a **Mac Studio M4 Max, 128 GB unified memory**

---

## Vetted Models

The list is deliberately short. MoE models need a minimum active-parameter threshold for honest instruction-following: below ~27–49B active they become confabulation-prone under complex multi-step protocols — enough stored knowledge to sound authoritative, not enough working memory to track what they've actually done. Dense models have no total/active split, so their parameter count is honest. Both vetted picks clear the threshold: Qwen3.6 27B and Qwen3.8 27B are dense (27B = 27B active).

**Qwen3.8 27B** is the current recommendation; **Qwen3.6 27B** remains vetted and fully supported — there is no need to migrate an instance that is working. Each was vetted against the complete tool surface of its date, not a subset: Qwen3.6 27B on 2026-07-10 (95 tools), Qwen3.8 27B on 2026-09-08 (116 tools; 117 today: `check_update` was removed, `file_offer` and `reminder_list` added). Both vettings predate the `guardian_call` fix of 2026-10-07 ([CHANGE-LOG.md](CHANGE-LOG.md)): before it, on Ollama, the input of a dynamic-tool invoke arrived as JSON text and never reached the tool as an object. On the fixed build, `gemma4:12b` and `glm-5.3:cloud` on Ollama each sent the input as an object and the search returned results (2026-10-07; the CHANGE-LOG entry has the traces). Those `gemma4:12b` runs used a GGUF build on a 16 GB NVIDIA GPU at Ollama's default context of 4,096 tokens, where Ollama trims every request to fit without an error ([below](#when-a-session-outgrows-the-context-length)): they show how the input arrived, not how Gemma 4 works with embraOS. Qwen3.8 27B works on Ollama and on LM Studio (operator-verified 2026-10-08).

### Local (Ollama / LM Studio)

| Server | Model | Tag | Size | Context | Status |
|--------|-------|-----|------|---------|--------|
| Ollama | Qwen3.8 27B (dense) | `qwen3.8:27b-mlx` | 18GB | 256K | vetted |
| Ollama | Qwen3.6 27B (dense) | `qwen3.6:27b-mlx` | 19GB | 256K | vetted |
| Ollama | Gemma 4 12B (dense) | `gemma4:12b-mlx` | 7.7GB | 256K | under evaluation |
| Ollama | Gemma 4 31B (dense) | `gemma4:31b-mlx` | 19GB | 256K | under evaluation |
| LM Studio | Qwen3.8 27B (dense, 8-bit) | `qwen/qwen3.8-27b` | 30GB | 256K | vetted |
| LM Studio | Qwen3.6 27B (dense, 8-bit) | `qwen/qwen3.6-27b` | 30GB | 256K | vetted |

The Ollama tags are the MLX builds, the ones the test fleet runs. A tag without
`-mlx` is a GGUF build, on which Ollama trims a request to fit its context
window without an error ([Ollama](#ollama-mac-mini-mac-studio) under Server
Configuration). Sizes
and context lengths are the Ollama library's own figures for the tags listed,
read 2026-10-13. The LM Studio sizes are the weights of the 8-bit MLX builds
(`lmstudio-community/Qwen3.8-27B-MLX-8bit` and `…/Qwen3.6-27B-MLX-8bit`, 29.5 GB
each). Gemma 4 12B and 31B are dense; the family's MoE is the `gemma4:26b` tag
(25.2B total, 3.8B active), below the active-parameter threshold above. The two
Gemma 4 rows are under evaluation by the operator and not yet vetted against the
tool surface. The 27B tags and `gemma4:31b-mlx` need more than the Mac Mini's 16 GB
of unified memory and run on the Mac Studio; `gemma4:12b-mlx` fits the Mac Mini. The
hardware mapping above says which *server* runs where, not that every model fits
on every host.

### Ollama Cloud

A `:cloud` tag is served by ollama.com through the local Ollama daemon, so it reaches
embraOS through the Ollama preset like any pulled model: nothing to fit in local
memory, and nothing local about the inference either. The active-parameter rule
above applies the same way.

| Model | Tag | Parameters | Context | Input | Status |
|---|---|---|---|---|---|
| DeepSeek V4 Pro | `deepseek-v4-pro:cloud` | 1.6T total, 49B active (MoE) | 1M | text | vetted, preferred |
| GLM 5.3 | `glm-5.3:cloud` | 753B | 1M | text | under evaluation |
| Kimi K3 | `kimi-k3:cloud` | 2.8T (MoE) | 1M | text, image | under evaluation |

Figures are the Ollama library's, read 2026-10-08. Only `kimi-k3:cloud` takes image
input. `deepseek-v4-pro:cloud` gets `reasoning_effort: max` from the brain unless
`/effort` sets a level (see [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md)).

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

The request is the same as for LM Studio: no sampling parameters and no `think` flag. Thinking follows `reasoning_effort` where the model documents it (`/effort`); everything else is the server's own configuration.

The context length is the server's too. The brain reaches Ollama through its OpenAI-compatible endpoint, which has no field for it (Ollama's OpenAI-compat note: `"The OpenAI API does not have a way of setting the context size"`; `num_ctx` is not a field there, per [`ollama#7063`](https://github.com/ollama/ollama/issues/7063), still open since 2024-10-01). What sets it depends on the build:

- **MLX builds** (the `-mlx` tags above). The window is the model's own maximum, 256K for the four tags. `OLLAMA_CONTEXT_LENGTH` and the default below change only the figure `ollama ps` shows. Ollama never trims an MLX request.
- **GGUF builds** (any tag without `-mlx`). The window is `OLLAMA_CONTEXT_LENGTH`, or a Modelfile's `PARAMETER num_ctx`, or else a default Ollama picks from the GPU memory it detects: 4,096 tokens under 24 GiB, 32,768 from 24 to 48 GiB, 262,144 from 48 GiB ([docs.ollama.com/context-length](https://docs.ollama.com/context-length); the server logs its pick at startup as `vram-based default context`). The tool list the brain sends is 78 KB of JSON, about 20,000 tokens, so the 4,096 default cannot hold even the tools, and Ollama trims every request to fit ([below](#when-a-session-outgrows-the-context-length)). Set `OLLAMA_CONTEXT_LENGTH` to at least 64000, Ollama's own figure for agents, together with `OLLAMA_FLASH_ATTENTION` and `OLLAMA_KV_CACHE_TYPE` (launchd environment variables on a Mac). Then check the `CONTEXT` and `PROCESSOR` columns of `ollama ps`: a window that does not fit in VRAM moves part of the model to the CPU.

### When a session outgrows the context length

Every turn replays the whole session: the system prompt, the history, the turn's attachments and, within the turn, every tool result. Nothing is trimmed on the brain's side, and no `num_ctx` is sent; the server's context length is the one limit. At that limit a server either refuses the request, and the brain says so, or trims it and nobody is told.

**Refused: LM Studio with the preset, and Ollama's MLX builds.** Ollama answers HTTP 400 before any stream opens (`input length (N tokens) exceeds the model's maximum context length (M tokens)`; read from Ollama's source, `mlxrunner/pipeline.go`, not yet seen live). LM Studio opens the stream and answers inside it with one error frame, then closes. Either way the brain shows the server's own message as an error (`Brain error: the model server answered with an error: Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens.` is LM Studio's), logs it at ERROR with the sizes of the tool results that went out with the request, ends the turn, and persists the turn's text with the marker `(turn ended by a provider error; see the error above)`. A refused turn stays refused: the history only grows, so continue in a new session (`/new <name>`; the knowledge graph and the memories are shared).

**Trimmed without a word: Ollama's GGUF builds.** Ollama trims a request that does not fit unless the request turns trimming off, and the OpenAI-compatible endpoint cannot: `truncate` is a field of Ollama's native `/api/chat` only. First it drops the oldest messages, keeping the system prompt and the latest message, and logs that only at debug level (`truncating input messages which exceed context length`, with `OLLAMA_DEBUG=1`). If the system prompt, the tool list and the latest message still do not fit, it cuts the prompt itself: it keeps the first few tokens and the last ones, about half the window in all, and logs `truncating input prompt` at WARN (`server/prompt.go` and `llm/llama_server.go` in Ollama's source). The model answers from what is left and the turn ends normally. No error reaches the brain, so nothing reaches the operator either. This is why a session on a GGUF build can lose its earlier turns, its identity or its tools with no error anywhere. At the 4,096-token default every request is cut, because the tool list alone does not fit. The model sees about the last 2,000 tokens. It misses its identity, most of its tools and, in a tool loop, the operator's own message. The operator found this on a 16 GB NVIDIA GPU (2026-10-13).

**Turn silent trimming off.** Use a server that refuses: Ollama's MLX builds, or LM Studio with the preset (`contextOverflowPolicy: stopAtLimit`). Ollama has no server setting that turns trimming off for a GGUF build, and the OpenAI-compatible endpoint cannot ask for it per request. On a GGUF build:

- set `OLLAMA_CONTEXT_LENGTH` ([above](#ollama-mac-mini-mac-studio)), so trimming starts only when a session outgrows a real window;
- read Ollama's log for trims: `truncating input prompt` appears at WARN, but a dropped message appears only with `OLLAMA_DEBUG=1`.

What fills a session fastest is a full `introspect` mid-session (a second copy of the sealed graph, ~70K tokens on a large graph) and a whole-file read of a dump; read a dump in slices with `file_read`'s `offset` and `limit`, and feed one to a dynamic tool through `guardian_call`'s `data_file`, which never enters the context. Tool results live within their turn: a later turn replays only the text the intelligence wrote.

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

*Last updated: 2026-10-13.*
