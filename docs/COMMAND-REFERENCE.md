# Command Reference

Every slash command available in an embraOS session, grouped as in the web console's sidebar (the chat-mobile picker and the console `/help` use the same groups). See also [Keyboard Shortcuts](OPERATION.md#keyboard-shortcuts).

## Session

| Command | Description |
|---|---|
| `/sessions` | List all sessions, most recently active first, with state, turn count, and last active time (configured timezone) |
| `/new <name>` | Create a new named session and switch to it. Duplicate names are refused (`/switch` to attach an existing session; a soft-deleted name stays reserved through its grace period) |
| `/switch <name>` | Switch to an existing session (restores full history) |
| `/close` | Close the current session |
| `/sessions delete <name>` | Guided deletion: the intelligence summarizes the session, asks your reason, preserves durable memories to the knowledge graph, then soft-deletes it (7-day grace period; any slash command mid-flow cancels; the `learning` session is never deletable) |
| `/sessions restore <name>` | Undo a soft delete during its grace period (the session returns Detached — `/switch` to attach) |

## Turn

| Command | Description |
|---|---|
| `/ml` | Open a multi-line message editor. **Web console (default):** a textarea modal — Ctrl/⌘+Enter or **Send** to submit, Esc/Cancel to discard; sent verbatim as one message. **Serial TUI (`EMBRA_TUI=1`):** toggles dot-terminator mode — type lines, `.` on its own line to send |
| `/stop` | Stop a stuck in-flight turn (local models can loop in unbounded reasoning). Console: type it or press **Esc** while the turn streams; mobile: the ■ button that replaces Send while busy. The partial response is kept, marked `(response interrupted by operator stop)` |
| `/iter-cap` | Show the current per-turn tool iteration cap (default 100) |
| `/iter-cap <N>` | Set the per-turn tool iteration cap (1..=1000). Persisted via `SystemConfig`; takes effect on the next user message. On cap-hit the loop emits a warning frame, asks the model to summarize, and terminates gracefully |
| `/iter-cap reset` | Restore the default iteration cap (100) |
| `/show-reasoning` | Show whether live reasoning / chain-of-thought streams to the expression panel (default on) |
| `/show-reasoning <on\|off>` | Toggle live reasoning streaming. When on, the panel renders the model's reasoning in italic dark-gray during a turn (Anthropic `display: "summarized"`, Gemini `includeThoughts: true`, OpenAI-compat already-on); reverts to the operator-set `express` content when idle. When off, providers omit reasoning from request bodies entirely (no token cost) and the panel only shows operator-set expressions. Persists past `ResponseDone` until the next user message |
| `/show-reasoning reset` | Restore the default (on) |

## Model

| Command | Description |
|---|---|
| `/provider` | Show active LLM provider, model, and session |
| `/provider <anthropic\|gemini\|ollama\|lm_studio>` | Switch provider for future turns. Requires no active session — close the current one with `/close` first. Autonomous in-turn switches queue and apply after the loop completes |
| `/provider --setup <anthropic\|gemini>` | Add/replace an API key for the named provider without re-running the wizard — multi-turn flow: type the command, then type the key on the next message. Auto-targets the missing provider when `<kind>` is omitted |
| `/provider --setup <ollama\|lm_studio>` | Reconfigure endpoint URL, bearer token, and selected model for an OpenAI-compat preset — 4-step flow (Endpoint → Bearer choice → Bearer token? → Model selection). Pre-fills current values; cancel anytime with any other slash command. Bearer hot-reloads via `EMBRA_<PRESET>_BEARER` env var (no brain restart) |
| `/model` | Show the active Anthropic model and the available options |
| `/model <opus-5\|opus-4.8\|fable-5>` | Switch the Anthropic model (Anthropic provider only; default opus-5). Persists to `SystemConfig.anthropic_model`; takes effect on the next user message — the provider is rebuilt per turn. The `EMBRA_ANTHROPIC_MODEL` env var takes precedence over the persisted value. Switching models is a one-time prompt-cache reset (caches are model-scoped). The legacy persisted `opus-4.7` value keeps working but is no longer selectable |
| `/effort` | Show the Anthropic `output_config.effort` level (default `max`) |
| `/effort <low\|medium\|high\|xhigh\|max>` | Set the Anthropic effort level (Anthropic provider only). Persists to `SystemConfig.anthropic_effort`; takes effect on the next user message. The `EMBRA_ANTHROPIC_EFFORT` env var takes precedence. Lower effort trades depth for latency/cost — relevant on Fable 5, where `max`-effort turns can run many minutes |

## Media

| Command | Description |
|---|---|
| `/attach <id\|path>` | Stage an image for your **next message**. `<id>` is a media id from an upload (the web console's 📎 button, drag-drop, or paste does the upload and types this command for you); `<path>` is any readable image file (absolute, or relative to `/embra/workspace`) — it is normalized (EXIF-rotated, long edge ≤ 2576 px, ≤ 1.5 MiB) and copied into `/embra/workspace/MEDIA/`. Staging is per session and survives `/switch` (keyed to the session it was staged in); at most 10 images per message. Available once the soul is sealed. On Ollama/LM Studio the loaded model must be vision-capable |
| `/attach` \| `/attach list` | Show the images staged for the active session and the MEDIA store usage |
| `/attach clear` | Drop the staged images (the files stay in the store) |
| `/media` | **Console-local.** Show the last image in the TUI media pane — a 12-row band under the expression panel (needs a ≥ 30-row terminal). Web console: sixel via xterm's image addon; serial: halfblocks by default, or sixel/kitty/iTerm2 when the host terminal supports it (`EMBRA_GRAPHICS=<mode>` → `embra.graphics=`; `auto` queries the terminal). The chat-mobile UI renders images inline instead |
| `/media off` \| `/media <id>` | Hide the pane, or fetch a specific media id into it |
| `/image-provider` | Show the `image_generate` backend: provider (explicit or default), model, key status (env / dedicated STATE key / Gemini LLM key fallback / not set) |
| `/image-provider gemini` | Use Gemini image models (the only backend today; also the default when unset) |
| `/image-provider model <id>` | Pick the model: `gemini-3-pro-image` (default, 4K-capable), `gemini-3.1-flash-image`, `gemini-3.1-flash-lite-image` (1K only), `gemini-2.5-flash-image` (legacy). Persists to `SystemConfig.image_model`; takes effect on the next `image_generate` call |
| `/image-provider key <token>` \| `key` \| `key remove` | Set a dedicated image-generation key (`/embra/state/api_key_image_gemini`, `0600`, never echoed), show whether one is set, or delete it. Precedence: `EMBRA_IMAGE_API_KEY` env > this key > the Gemini LLM key |
| `/image-provider clear` | Reset provider + model to the defaults (the key file is untouched) |

## Git & SSH

| Command | Description |
|---|---|
| `/git-setup <name> \| <email>` | Set git user.name and user.email |
| `/github-token <token>` | Set GitHub token for API access (persists across reboots) |
| `/git-token <host> <token>` | Set a token for a self-hosted git server (GitLab/Gitea over HTTPS; injected as an `oauth2` rewrite for `git_clone`/`git_push`/`git_pull`). No args lists configured hosts; `/git-token <host> remove` deletes. github.com stays on `/github-token` |
| `/ssh-keygen` | Generate ed25519 SSH key pair and display public key |
| `/ssh-copy-id <user@host>` | Copy SSH public key to remote host (RFC 1918 only) |

## Guardian

| Command | Description |
|---|---|
| `/guardian-define` | **(`embra-guardian-v1` branch — experimental)** Open the multi-line editor to paste a Rust module defining a dynamic tool; validated synchronously **and soul-checked (the replicant check — a `refuse` blocks compilation; the soul outranks even an operator paste)**, then compiled to WASM in the background (poll with `/guardian status <name>`) |
| `/guardian list \| status <name> \| show <name> \| delete <name>` | **(`embra-guardian-v1` branch)** List dynamic tools (status + declared capabilities), show one's build status + log tail + any replicant verdict, print its stored source, or remove it (manifest, overlay, project, artifact) |
| `/guardian approve <name> \| reject <name>` | **(`embra-guardian-v1` branch)** Approve or reject a tool the intelligence *proposed* via `guardian_propose` (a draft that passed the soul-spec replicant check). Approve compiles it (background build); reject discards the proposal. Only `proposed` tools are affected — built tools still go through `/guardian delete` |
| `/guardian key brave <token>` | **(`embra-guardian-v1` branch)** Set the Brave Search API key host-side (STATE, `0600`) to enable `web_search`-capable tools; omit `<token>` to check status — the key is never echoed, never in a guest module, the manifest, or results |

## Identity

| Command | Description |
|---|---|
| `/soul` | Display the immutable soul — the sealed identity graph as grouped prose + the seal header (legacy flat-soul instances: the raw JSON document) |
| `/identity` | Display the intelligence's identity — the sealed identity graph it is part of (legacy instances: the separate identity document) |
| `/mode` | Show current operating mode and soul seal status |

## System

| Command | Description |
|---|---|
| `/help` | Show all commands and keyboard shortcuts |
| `/status` | System status — version, uptime, WardSONDB health, memory, soul status |
| `/feedback-loop` | **(EXPERIMENTAL)** Trigger Phase 3 Continuity Engine self-evaluation protocol — the Brain walks through a multi-step gather/evaluate/reconcile/execute sequence using existing tools |
| `/copy` | Copy conversation to clipboard via OSC 52 — `/copy 5` for last 5 messages (disabled — Sprint 5) |
