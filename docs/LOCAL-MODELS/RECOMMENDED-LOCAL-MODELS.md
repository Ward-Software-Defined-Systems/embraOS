# Recommended Models for OpenAI-Compatible Providers

**Status:** Phase 1 stable since `v0.5.0-phase1` (2026-05-07). The models below are operator-vetted as full-toolset-capable (the README header callout lists the same picks); operator-overridable at wizard time.

The wizard's selector reads `GET /v1/models` from the configured server, so any pulled (Ollama) or loaded (LM Studio) model is selectable regardless of what's listed here.

## The test fleet

| Host | Server | Memory |
|---|---|---|
| M1 Mac Mini | Ollama | 16 GB unified |
| Mac Studio M4 Max | LM Studio and Ollama | 128 GB unified |
| x86_64 server (Ubuntu 24.04), AMD Ryzen 7 5800X: 8 cores, 16 threads | Ollama | 128 GB DDR4 system memory; NVIDIA RTX 3080 Ti, 12 GB of VRAM; the smaller models on x86_64 and NVIDIA, under evaluation |

The table says which *server* runs where, not that every model fits on every host; the fit is under each table below.

---

## Vetted Models

**Why the list is short.** MoE models need a minimum active-parameter count for honest instruction-following: below ~27–49B active they become confabulation-prone under complex multi-step protocols, with enough stored knowledge to sound authoritative and not enough working memory to track what they have done. A dense model has no total/active split, so its parameter count is honest. Both vetted picks are dense: Qwen3.6 27B and Qwen3.8 27B (27B = 27B active).

**The picks.** **Qwen3.8 27B** is the current recommendation. **Qwen3.6 27B** remains vetted and fully supported; an instance that works needs no migration. Each was vetted against the complete tool surface of its date, not a subset: Qwen3.6 27B on 2026-07-10 (95 tools), Qwen3.8 27B on 2026-09-08 (116 tools; 117 today: `check_update` was removed, `file_offer` and `reminder_list` added). Qwen3.8 27B works on Ollama and on LM Studio (operator-verified 2026-10-08).

**The `guardian_call` fix.** Both vettings predate the fix of 2026-10-07 ([CHANGE-LOG.md](../CHANGE-LOG.md)): before it, on Ollama, the input of a dynamic-tool invoke arrived as JSON text and never reached the tool as an object. On the fixed build, `gemma4:12b` and `glm-5.3:cloud` on Ollama each sent the input as an object and the search returned results (2026-10-07; the CHANGE-LOG entry has the traces). Those `gemma4:12b` runs used a GGUF build on a 16 GB NVIDIA GPU at Ollama's default context of 4,096 tokens, where Ollama trims every request to fit ([below](#when-a-session-outgrows-the-context-length)): they show how the input arrived, not how Gemma 4 works with embraOS.

### Local (Ollama / LM Studio)

| Server | Model | Tag | Size | Context | Effort sent (default) | Status |
|--------|-------|-----|------|---------|-----------------------|--------|
| Ollama | Qwen3.8 27B (dense) | `qwen3.8:27b-mlx` | 18GB | 256K | none (`medium`) | vetted |
| Ollama | Qwen3.6 27B (dense) | `qwen3.6:27b-mlx` | 19GB | 256K | none (thinking on) | vetted |
| Ollama | Gemma 4 12B (dense) | `gemma4:12b-mlx` | 7.7GB | 256K | none (thinking on) | under evaluation |
| Ollama | Gemma 4 31B (dense) | `gemma4:31b-mlx` | 19GB | 256K | none (thinking on) | under evaluation |
| Ollama | Gemma 4 12B (dense, Q4_K_M GGUF) | `gemma4:12b` | 8.0GB | 256K | none (thinking on) | under evaluation |
| Ollama | Nemotron 3.5 Super 120B (MoE, 12B active, NVFP4 MLX) | `nemotron-3.5-super:120b-a12b-nvfp4` | 80GB | 1M | none (thinking on) | under evaluation |
| LM Studio | Qwen3.8 27B (dense, 8-bit) | `qwen/qwen3.8-27b` | 30GB | 256K | none (`xhigh`) | vetted |
| LM Studio | Qwen3.6 27B (dense, 8-bit) | `qwen/qwen3.6-27b` | 30GB | 256K | none (thinking on) | vetted |

- **Builds.** The `-mlx` tags are the MLX builds, which the Macs run; `nemotron-3.5-super:120b-a12b-nvfp4` is an MLX build too (the library marks it MLX). The plain tag of each of the four `-mlx` models (`gemma4:12b`, `qwen3.8:27b`, …) lists both builds, and a pull takes one by platform: the MLX build on an Apple Silicon Mac, the GGUF build anywhere else (`runnerPreferences` in Ollama's `manifest/manifest.go`). So `gemma4:12b` is a GGUF build on the x86_64 server. The `RUNNER` column of `ollama ps` shows which build is loaded. On a GGUF build, Ollama trims a request to fit its context window without an error ([Ollama](#ollama-mac-mini-mac-studio-x86_64-server) under Server Configuration).
- **Figures.** Sizes and context lengths are the Ollama library's own for the tags listed, read 2026-10-13; for `gemma4:12b`, the figure of its GGUF build (`gemma4:12b-it-q4_K_M`). For `nemotron-3.5-super:120b-a12b-nvfp4`, read 2026-10-20, the library shows no context; 1M is the model's own (`max_position_embeddings` 1,048,576 in the tag's `config.json`). The LM Studio sizes are the weights of the 8-bit MLX builds (`lmstudio-community/Qwen3.8-27B-MLX-8bit` and `…/Qwen3.6-27B-MLX-8bit`, 29.5 GB each).
- **Gemma 4.** 12B and 31B are dense; the family's MoE is the `gemma4:26b` tag (25.2B total, 3.8B active), below the active-parameter threshold above. The three Gemma 4 rows are under evaluation by the operator and not yet vetted against the tool surface.
- **Nemotron 3.5 Super.** A MoE: 120B total, 12B active per token, below the active-parameter threshold above. The tag needs Ollama 0.40.5 or later (`requires` in its config). The row is under evaluation by the operator and not yet vetted against the tool surface.
- **Fit.** The 27B tags and `gemma4:31b-mlx` need more than the Mac Mini's 16 GB of unified memory and run on the Mac Studio; `gemma4:12b-mlx` fits the Mac Mini. `nemotron-3.5-super:120b-a12b-nvfp4` needs 80GB, more than the Mac Mini or the x86_64 server's GPU holds; it fits the Mac Studio. On the x86_64 server, `gemma4:12b` loads entirely onto the 12 GB GPU at 4,096 tokens (2026-10-13). Whether it stays there at the 64000 tokens it needs ([Ollama](#ollama-mac-mini-mac-studio-x86_64-server)) is TBD.
- **Effort.** The column is what the brain sends as `reasoning_effort` while `/effort` is unset, and in parentheses what the server then uses. The brain sends nothing to a local row: `reasoning_effort_for_model` (`crates/embra-brain/src/provider/openai_compat/mod.rs`) names none of them. Ollama then uses the model's `thinking.default` from `/api/show` ([docs.ollama.com/capabilities/thinking](https://docs.ollama.com/capabilities/thinking)): thinking on, and `medium` for Qwen3.8, which Ollama sets in `server/model_thinking.go`. LM Studio uses the model's own fields: Reasoning Effort `xhigh` for Qwen3.8, Enable Thinking on for Qwen3.6. `/effort xhigh` gives Qwen3.8 `xhigh` on Ollama too. Read 2026-10-20; the levels are under [Reasoning effort](#reasoning-effort).

### Ollama Cloud

A `:cloud` tag is served by ollama.com through the local Ollama daemon, so it reaches embraOS through the Ollama preset like any pulled model: nothing to fit in local memory, and nothing local about the inference either. The active-parameter rule above applies the same way.

| Model | Tag | Parameters | Context | Input | Effort sent (default) | Status |
|---|---|---|---|---|---|---|
| DeepSeek V4 Pro | `deepseek-v4-pro:cloud` | 1.6T total, 49B active (MoE) | 1M | text | `max` | vetted, preferred |
| DeepSeek V4.1 Flash | `deepseek-v4.1-flash:cloud` | 763B total; 8B active in prefill, 16B in decode (MoE) | 1M | text, image | none (`high`) | under evaluation |
| GLM 5.3 | `glm-5.3:cloud` | 753B | 1M | text | none (`max`) | under evaluation |
| Kimi K3 | `kimi-k3:cloud` | 2.8T (MoE) | 1M | text, image | none (`max`) | under evaluation |

Figures are the Ollama library's, read 2026-10-08 (`deepseek-v4.1-flash:cloud` 2026-10-20). `kimi-k3:cloud` and `deepseek-v4.1-flash:cloud` take image input. The brain sends `reasoning_effort: max` to `deepseek-v4-pro:cloud`, whose own default on ollama.com is `low`, and nothing to the other three, which run at ollama.com's default in parentheses (its `/api/show`, 2026-10-20). `/effort` sets a level for any of them (see [COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md)).

---

## Server Configuration

The request is the same for both servers. embra-brain sends the model, the messages, the tool manifest, `stream`, and `reasoning_effort` where the model documents it (`crates/embra-brain/src/provider/openai_compat/wire.rs`). It sends no sampling parameters, no chat-template settings, no `think` flag and no `num_ctx`. Everything else, the context length included, is the server's own per-model configuration.

### LM Studio (Mac Studio)

Per-model load config in LM Studio's "My Models":

```
Context length:        262144
```

The preset `embraOS_Qwen3.8-27B` in [`LOCAL_MODEL_SETTINGS/LM-Studio-Examples/`](LOCAL_MODEL_SETTINGS/LM-Studio-Examples/) pins the three prediction settings that matter for a tool-calling session. Import it in "My Models", then select it for the model. It carries no load settings; the load configuration above stays as it is.

| Setting | Value | Why |
|---|---|---|
| `contextOverflowPolicy` | `stopAtLimit` | The server stops at the context limit instead of truncating the prompt. A truncation drops part of the tool manifest or the history without a word. |
| `repeatPenalty` | `1` | The penalty on repeated tokens is off. Tool-call JSON and code repeat tokens by nature. |
| `minPSampling` | `0` | Min-p sampling is off. |

### Ollama (Mac Mini, Mac Studio, x86_64 server)

**Recommended GGUF model settings.** Three environment variables on the Ollama server, set together:

```
OLLAMA_CONTEXT_LENGTH=64000     # at least 64000, Ollama's own figure for agents; the brain warns below it
OLLAMA_FLASH_ATTENTION=1        # depending on the age and number of GPUs you are running, this can be left unset and Ollama will auto-configure
OLLAMA_KV_CACHE_TYPE=q8_0       # halves the cache's memory against f16, Ollama's default; needs flash attention
```

| Where Ollama runs | How to set them |
|---|---|
| macOS, the Ollama application | `launchctl setenv OLLAMA_CONTEXT_LENGTH 64000`, one command per variable, then restart Ollama |
| Linux, the systemd service | `systemctl edit ollama.service`, one `Environment="OLLAMA_CONTEXT_LENGTH=64000"` line per variable under `[Service]`, then `systemctl daemon-reload` and `systemctl restart ollama` |

Then check `ollama ps`: the `CONTEXT` column is the window the model is loaded with, and the `PROCESSOR` column says where it sits. A window that does not fit in VRAM moves part of the model to the CPU. The variables and their values are Ollama's own ([docs.ollama.com/faq](https://docs.ollama.com/faq), [docs.ollama.com/context-length](https://docs.ollama.com/context-length)).

**Why the server sets the window.** The brain reaches Ollama through its OpenAI-compatible endpoint, which has no field for the context length (Ollama's OpenAI-compat note: `"The OpenAI API does not have a way of setting the context size"`; `num_ctx` is not a field there, per [`ollama#7063`](https://github.com/ollama/ollama/issues/7063), still open since 2024-10-01). What the window is depends on the build:

| Build | Which tags | The window | A request past it |
|---|---|---|---|
| MLX | the `-mlx` tags and `nemotron-3.5-super:120b-a12b-nvfp4`; a plain tag pulled on an Apple Silicon Mac | the model's own maximum: 256K for the four `-mlx` tags, 1M for the Nemotron tag; `OLLAMA_CONTEXT_LENGTH` and the default change only the figure `ollama ps` shows | refused (HTTP 400); Ollama never trims an MLX request |
| GGUF | a plain tag pulled anywhere else, such as `gemma4:12b` on the x86_64 server | `OLLAMA_CONTEXT_LENGTH`; or a Modelfile's `PARAMETER num_ctx`, which overrides it; or else Ollama's default from the GPU memory it detects: 4,096 tokens under 24 GiB, 32,768 from 24 to 48 GiB, 262,144 from 48 GiB; never more than the length the model was trained for | trimmed to fit, without an error |

The server logs its default at startup as `vram-based default context`; on the x86_64 server, `total_vram="11.6 GiB" default_num_ctx=4096`. The tool list the brain sends is 78 KB of JSON, about 20,000 tokens, so the 4,096 default cannot hold even the tools, and Ollama trims every request to fit ([below](#when-a-session-outgrows-the-context-length)).

**The brain checks the window.** Every 5 minutes the provider probe reads Ollama's `/api/ps`, the list of loaded models behind `ollama ps`.

- **The warning.** When a GGUF model is loaded with a window below 64000 tokens, the probe says so once. The notice is critical when the window cannot hold even the tool list, and normal otherwise. It says so once more when the window is fixed.
- **Where it shows.** `/status` shows the reading as `provider.context_window`, and the provider pill's tooltip says it.
- **What never warns.** An MLX build and a cloud model.
- **When the window is not read.** The probe reads nothing when the model unloads within 5 minutes of a turn (`OLLAMA_KEEP_ALIVE` under 5m), behind a proxy that blocks `/api/ps`, or from an Ollama too old to report the window.
- **When the remedy does not help.** A Modelfile's `PARAMETER num_ctx` overrides `OLLAMA_CONTEXT_LENGTH`. Ollama also caps the window at the length the model was trained for.

### When a session outgrows the context length

Every turn replays the whole session: the system prompt, the history, the turn's attachments and, within the turn, every tool result. Nothing is trimmed on the brain's side, and no `num_ctx` is sent; the server's context length is the one limit. At that limit a server either refuses the request, and the brain says so, or trims it, and nobody is told.

| Server | At the limit | What the operator sees |
|---|---|---|
| LM Studio with the preset | Refuses: the stream opens and answers inside it with one error frame, then closes. | `Brain error: the model server answered with an error: Input does not fit in context length. The input has 359277 tokens, but the context length only supports 262144 tokens.` The turn ends and its text is persisted with the marker `(turn ended by a provider error; see the error above)`. |
| Ollama, an MLX build | Refuses: HTTP 400 before any stream opens (`input length (N tokens) exceeds the model's maximum context length (M tokens)`; read from Ollama's source, `mlxrunner/pipeline.go`, not yet seen live). | The same error, the same marker. |
| Ollama, a GGUF build | Trims. First the oldest messages go, keeping the system prompt and the latest message, logged at debug level only (`truncating input messages which exceed context length`, with `OLLAMA_DEBUG=1`). If the system prompt, the tool list and the latest message still do not fit, the prompt itself is cut: the first few tokens and the last ones, about half the window in all, logged at WARN (`truncating input prompt`; `server/prompt.go` and `llm/llama_server.go` in Ollama's source). | Nothing. The model answers from what is left and the turn ends normally. |

**A refused turn stays refused.** The brain logs the refusal at ERROR with the sizes of the tool results that went out with the request. The history only grows, so continue in a new session (`/new <name>`; the knowledge graph and the memories are shared).

**A trimmed session loses its earlier turns, its identity or its tools with no error anywhere.** The brain sees the window Ollama loaded and warns about a short one ([above](#ollama-mac-mini-mac-studio-x86_64-server)), but it does not see each trim. At the 4,096-token default every request is cut, because the tool list alone does not fit. On the x86_64 server, each of the 29 requests of a test on `gemma4:12b` was 20,037 to 20,126 tokens, and Ollama kept 2,051 of them, the first 5 and the last 2,046 (`truncating input prompt limit=2051 prompt=20044 keep=5 new=2051`, 2026-10-13). The model misses its identity, most of its tools and, in a tool loop, the operator's own message. The operator first found this on a 16 GB NVIDIA GPU (2026-10-13).

**Turn silent trimming off.** Use a server that refuses: Ollama's MLX builds, or LM Studio with the preset (`contextOverflowPolicy: stopAtLimit`). Ollama has no server setting that turns trimming off for a GGUF build, and the OpenAI-compatible endpoint cannot ask for it per request (`truncate` is a field of Ollama's native `/api/chat` only). On a GGUF build:

- set `OLLAMA_CONTEXT_LENGTH` ([above](#ollama-mac-mini-mac-studio-x86_64-server)), so trimming starts only when a session outgrows a real window;
- read Ollama's log for trims: `truncating input prompt` appears at WARN, but a dropped message appears only with `OLLAMA_DEBUG=1`.

**What fills a session fastest** is a full `introspect` mid-session (a second copy of the sealed graph, ~70K tokens on a large graph) and a whole-file read of a dump. Read a dump in slices with `file_read`'s `offset` and `limit`, and feed one to a dynamic tool through `guardian_call`'s `data_file`, which never enters the context. Tool results live within their turn: a later turn replays only the text the intelligence wrote.

### Reasoning effort

| Model | Levels |
|---|---|
| Qwen3.8 | `low`, `medium`, `xhigh` (`high` is not valid for this model) |
| Nemotron 3.5 Super | `medium` |
| DeepSeek V4 Pro, DeepSeek V4.1 Flash, GLM 5.3, Kimi K3 | `low`, `high`, `max` |
| Qwen3.6, Gemma 4 | none: thinking is on or off |

The levels are each server's own, read 2026-10-20: Ollama's `/api/show`, from the model's renderer in Ollama's source; ollama.com's `/api/show` for the `:cloud` tags; LM Studio's model pages. What the brain sends while no level is set, and the default the server then uses, is the Effort column of each table above.

Set a level with `/effort <level>` while the Ollama or LM Studio preset is active. The level is sent verbatim and the server validates it; Ollama runs a level the model does not list at the model's default ([docs.ollama.com/capabilities/thinking](https://docs.ollama.com/capabilities/thinking)). `/effort` warns when a level is outside the set the brain knows for the model (`known_effort_ladder`). Unset (the default), the brain sends `max` to DeepSeek V4 Pro and no field to any other model; `/effort reset` clears a stored level. See [COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md).

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
See [COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md).

---

*Last updated: 2026-10-21.*
