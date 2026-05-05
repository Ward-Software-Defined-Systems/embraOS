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

### Ollama (Mac Mini, 16 GB)

- TBD — testing in progress

### LM Studio (Mac Studio, 128 GB)

- TBD — testing in progress

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
