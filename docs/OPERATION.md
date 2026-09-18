# Operation

Day-to-day use of embraOS once it is running.

## The Session Model

Every interaction with embraOS happens in a persistent session. Sessions survive disconnections. When you reconnect, the full conversation history is restored, and if the session has been idle for 30 minutes or more the AI opens with a briefing on where things left off (quick reconnects — a phone unlocking, a browser tab waking — resume silently; `/switch`ing to a session always briefs).

You can run multiple named sessions for different contexts:

```
/new research         # Create a research-focused session
/new monitoring       # Create a monitoring session
/switch main          # Switch back to the main session
/sessions             # List all sessions
```

All sessions share the same intelligence — same memory, same identity, same soul. But each has its own conversation history and context.

**Stopping a stuck turn**: if the intelligence gets caught in a reasoning loop (far more common on local models than frontier ones), press **Esc** in the console while the turn is streaming, type `/stop`, or tap the **■** button that replaces Send on mobile. Generation stops immediately — the connection to the model server is severed — and the partial response stays in history marked as interrupted. Stops are fail-safe: they only ever affect the turn that was running when you pressed them, and they work during Learning Mode too.

**Deleting a session** is a conversation, not a keystroke: `/sessions delete <name>` has the intelligence summarize the session and ask why you're removing it; your reason is recorded, durable learnings are preserved to the knowledge graph (`remember`/`knowledge_promote`), and only then does the system soft-delete it. Deletion is soft for 7 days — the session disappears from listings and can't be attached, but `/sessions restore <name>` brings it back intact until the database's TTL reaper removes the data for good. Any slash command during the flow cancels it; a declining reply aborts it; the `learning` session (the sealed identity record) can never be deleted.

## Keyboard Shortcuts

**embraOS TUI** — in-conversation:

| Key | Action |
|---|---|
| `Enter` | Send message (or newline in `/ml` multi-line mode) |
| `Up/Down` | Scroll history (`PageUp/PageDown` = 10 rows) |
| `Shift+Up/Down` | Scroll the expression/reasoning panel (`Shift+PageUp/PageDown` = 5 rows; snaps back to the tail on your next message, on errors, or when new expression content arrives) |
| `Esc` | Stop the current turn (only while it is thinking/streaming — idle Esc is a no-op) |

*There is no exit or detach chord. The console is a supervised child on both transports — embra-web respawns it after 1 s (`crates/embra-web/src/pty_bridge.rs`), embrad supervises it on serial (`crates/embrad/src/supervisor.rs`) — so there is nothing to detach from: leave the web console by closing the tab, the serial console with QEMU's `Ctrl+A X` below. `Ctrl+<letter>` chords are deliberate no-ops. Enter sends whatever modifier is held (`Shift+Enter` and `Alt+Enter` send too — the console's Enter arm ignores modifiers, `crates/embra-console/src/terminal/mod.rs`); `/ml` is the multi-line path.*

*In the web console's `/ml` editor modal: Enter = newline, Ctrl/⌘+Enter or Send = submit, Esc/Cancel = discard.*

*Web console layout (`https://localhost:3345/embraOS`, desktop): the top bar carries the brand and version, **Guided setup: [Provider setup]** (its tooltip points at the Git & SSH commands in the sidebar), the role badge — **● Writer**, or **○ Read-only · operator N** with a **Take control** button (one writer at a time; other tabs observe), **⌘ Commands** (a filterable palette), **📎 Attach** (upload an image; dropping or pasting one on the terminal does the same) and **↗ mobile** (the chat view, automatic at ≤ 768 px). The left sidebar is the command list — a filter box above eight collapsible groups, the same groups as [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md): groups start collapsed, click a title to expand it (remembered per browser), type in the filter to narrow every group to its matches (Esc clears). Above the terminal, a full-width status strip shows one pill per service plus the CPU / memory / DATA / STATE / load meters, refreshed from a 5-second `GET /api/status` poll; a `provider` pill joins them once the brain has probed the active LLM endpoint (~30 s after boot) and reads down while the endpoint is unreachable. Buttons inject commands; the console is authoritative.*

**QEMU** — host-level (`run-qemu.sh` uses `-serial mon:stdio`, so `Ctrl+A` is the escape prefix):

| Key | Action |
|---|---|
| `Ctrl+A X` | Exit QEMU (powers off the VM) |
| `Ctrl+A C` | Switch between serial console and QEMU monitor |
| `Ctrl+A H` | Show all QEMU escape sequences |

## Current Limitations

- **API or remote LLM** — Anthropic Claude / Google Gemini require internet + a paid API key. The Ollama and LM Studio presets connect to an OpenAI-compatible server: a local one you operate (typically a Mac Studio or similar) or any hosted one — vLLM, Together, Fireworks, OpenRouter — via `/provider --setup <ollama|lm_studio>` (base URL without `/v1`, key at the Bearer step; https endpoints keep their implicit 443). Inference still happens on a separate host — on-device inference inside embraOS itself is Phase 5 scope ([ROADMAP.md](ROADMAP.md)).
- **QEMU only** — x86_64 on Ubuntu is the reference build; the [aarch64 / Apple Silicon](AARCH64-BUILD.md) and [Intel Mac](INTEL-MAC-BUILD.md) guides each carry their own verification banner (both re-verified end-to-end 2026-07-19 under the named-volume Docker Buildroot flow). Bare metal and broader architecture support are Phase 4 scope ([ROADMAP.md](ROADMAP.md))
- **Tested on limited platforms** — built and verified on Ubuntu 24.04 + 26.04 under QEMU 8.2.2; bootable image also runs under QEMU on Intel and Apple Silicon Macs
- **No MCP server modules** — the tool surface is the 116 built-in tools plus Guardian dynamic tools: operator-authored (`/guardian-define`) or intelligence-proposed (`guardian_propose`), both replicant-checked against the sealed soul, proposals operator-approved before they compile ([TOOL-REFERENCE.md](TOOL-REFERENCE.md), [REPLICANT-CHECK.md](REPLICANT-CHECK.md)). MCP server modules through the Guardian governance proxy are Phase 3 scope ([ROADMAP.md](ROADMAP.md))
- **No on-device LLM inference** — the conversational model always runs on another host or a hosted API; only the knowledge graph's sentence-embedding model (`bge-small-en-v1.5`, [KNOWLEDGE-GRAPH.md](KNOWLEDGE-GRAPH.md)) runs in-process. `embraOS-QNM` for sovereign on-host inference is Phase 5 scope; the OpenAI-compat presets are the foundation for Phase 3's hybrid local/API routing ([ROADMAP.md](ROADMAP.md)).
