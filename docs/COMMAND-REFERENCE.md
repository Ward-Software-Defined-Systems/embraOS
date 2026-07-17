# Command Reference

Every slash command available in an embraOS session. See also [Keyboard Shortcuts](OPERATION.md#keyboard-shortcuts).

| Command | Description |
|---|---|
| `/help` | Show all commands and keyboard shortcuts |
| `/ml` | Open a multi-line message editor. **Web console (default):** a textarea modal — Ctrl/⌘+Enter or **Send** to submit, Esc/Cancel to discard; sent verbatim as one message. **Serial TUI (`EMBRA_TUI=1`):** toggles dot-terminator mode — type lines, `.` on its own line to send |
| `/status` | System status — version, uptime, WardSONDB health, memory, soul status |
| `/sessions` | List all sessions, most recently active first, with state, turn count, and last active time (configured timezone) |
| `/new <name>` | Create a new named session and switch to it. Duplicate names are refused (`/switch` to attach an existing session; a soft-deleted name stays reserved through its grace period) |
| `/switch <name>` | Switch to an existing session (restores full history) |
| `/sessions delete <name>` | Guided deletion: the intelligence summarizes the session, asks your reason, preserves durable memories to the knowledge graph, then soft-deletes it (7-day grace period; any slash command mid-flow cancels; the `learning` session is never deletable) |
| `/sessions restore <name>` | Undo a soft delete during its grace period (the session returns Detached — `/switch` to attach) |
| `/close` | Close the current session |
| `/soul` | Display the immutable soul document |
| `/identity` | Display the intelligence's identity document |
| `/mode` | Show current operating mode and soul seal status |
| `/github-token <token>` | Set GitHub token for API access (persists across reboots) |
| `/ssh-keygen` | Generate ed25519 SSH key pair and display public key |
| `/ssh-copy-id <user@host>` | Copy SSH public key to remote host (RFC 1918 only) |
| `/git-setup <name> \| <email>` | Set git user.name and user.email |
| `/provider` | Show active LLM provider, model, and session |
| `/provider <anthropic\|gemini\|ollama\|lm_studio>` | Switch provider for future turns. Requires no active session — close the current one with `/close` first. Autonomous in-turn switches queue and apply after the loop completes |
| `/provider --setup <anthropic\|gemini>` | Add/replace an API key for the named provider without re-running the wizard — multi-turn flow: type the command, then type the key on the next message. Auto-targets the missing provider when `<kind>` is omitted |
| `/provider --setup <ollama\|lm_studio>` | Reconfigure endpoint URL, bearer token, and selected model for an OpenAI-compat preset — 4-step flow (Endpoint → Bearer choice → Bearer token? → Model selection). Pre-fills current values; cancel anytime with any other slash command. Bearer hot-reloads via `EMBRA_<PRESET>_BEARER` env var (no brain restart) |
| `/model` | Show the active Anthropic model and the available options |
| `/model <opus-4.7\|opus-4.8\|fable-5>` | Switch the Anthropic model (Anthropic provider only; default opus-4.8). Persists to `SystemConfig.anthropic_model`; takes effect on the next user message — the provider is rebuilt per turn. The `EMBRA_ANTHROPIC_MODEL` env var takes precedence over the persisted value. Switching models is a one-time prompt-cache reset (caches are model-scoped) |
| `/effort` | Show the Anthropic `output_config.effort` level (default `max`) |
| `/effort <low\|medium\|high\|xhigh\|max>` | Set the Anthropic effort level (Anthropic provider only). Persists to `SystemConfig.anthropic_effort`; takes effect on the next user message. The `EMBRA_ANTHROPIC_EFFORT` env var takes precedence. Lower effort trades depth for latency/cost — relevant on Fable 5, where `max`-effort turns can run many minutes |
| `/iter-cap` | Show the current per-turn tool iteration cap (default 100) |
| `/iter-cap <N>` | Set the per-turn tool iteration cap (1..=1000). Persisted via `SystemConfig`; takes effect on the next user message. On cap-hit the loop emits a warning frame, asks the model to summarize, and terminates gracefully |
| `/iter-cap reset` | Restore the default iteration cap (100) |
| `/show-reasoning` | Show whether live reasoning / chain-of-thought streams to the expression panel (default on) |
| `/show-reasoning <on\|off>` | Toggle live reasoning streaming. When on, the panel renders the model's reasoning in italic dark-gray during a turn (Anthropic `display: "summarized"`, Gemini `includeThoughts: true`, OpenAI-compat already-on); reverts to the operator-set `express` content when idle. When off, providers omit reasoning from request bodies entirely (no token cost) and the panel only shows operator-set expressions. Persists past `ResponseDone` until the next user message |
| `/show-reasoning reset` | Restore the default (on) |
| `/feedback-loop` | **(EXPERIMENTAL)** Trigger Phase 3 Continuity Engine self-evaluation protocol — the Brain walks through a multi-step gather/evaluate/reconcile/execute sequence using existing tools |
| `/guardian-define` | **(`embra-guardian-v1` branch — experimental)** Open the multi-line editor to paste a Rust module defining a dynamic tool; validated synchronously **and soul-checked (the replicant check — a `refuse` blocks compilation; the soul outranks even an operator paste)**, then compiled to WASM in the background (poll with `/guardian status <name>`) |
| `/guardian list \| status <name> \| show <name> \| delete <name>` | **(`embra-guardian-v1` branch)** List dynamic tools (status + declared capabilities), show one's build status + log tail + any replicant verdict, print its stored source, or remove it (manifest, overlay, project, artifact) |
| `/guardian approve <name> \| reject <name>` | **(`embra-guardian-v1` branch)** Approve or reject a tool the intelligence *proposed* via `guardian_propose` (a draft that passed the soul-spec replicant check). Approve compiles it (background build); reject discards the proposal. Only `proposed` tools are affected — built tools still go through `/guardian delete` |
| `/guardian key brave <token>` | **(`embra-guardian-v1` branch)** Set the Brave Search API key host-side (STATE, `0600`) to enable `web_search`-capable tools; omit `<token>` to check status — the key is never echoed, never in a guest module, the manifest, or results |
| `/copy` | Copy conversation to clipboard via OSC 52 — `/copy 5` for last 5 messages (disabled — Sprint 5) |
