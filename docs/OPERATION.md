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
| `Alt+Enter` | New line |
| `Up/Down` | Scroll history (`PageUp/PageDown` = 10 rows) |
| `Shift+Up/Down` | Scroll the expression/reasoning panel (`Shift+PageUp/PageDown` = 5 rows; snaps back to the tail on your next message, on errors, or when new expression content arrives) |
| `Esc` | Stop the current turn (only while it is thinking/streaming — idle Esc is a no-op) |
| `Ctrl+C` | Graceful detach |
| `Ctrl+D` | Graceful detach |

*In the web console's `/ml` editor modal: Enter = newline, Ctrl/⌘+Enter or Send = submit, Esc/Cancel = discard.*

**QEMU** — host-level (`run-qemu.sh` uses `-serial mon:stdio`, so `Ctrl+A` is the escape prefix):

| Key | Action |
|---|---|
| `Ctrl+A X` | Exit QEMU (powers off the VM) |
| `Ctrl+A C` | Switch between serial console and QEMU monitor |
| `Ctrl+A H` | Show all QEMU escape sequences |

## Current Limitations

- **API or remote-style local LLM** — Anthropic Claude / Google Gemini require internet + a paid API key. Ollama and LM Studio (Sprint 5) connect to a local-network OpenAI-compat server you operate (typically a Mac Studio or similar). Inference still happens on a separate host — true on-device inference inside embraOS itself comes in Phase 5.
- **QEMU x86_64 (recommended)** — an experimental aarch64/Apple Silicon build is available — see the [aarch64 / Apple Silicon](AARCH64-BUILD.md) (re-synced with the canonical build at `f8cad9c` and end-to-end QEMU-verified on both x86_64 and aarch64 — aarch64 on a MacBook M2, 2026-05-18/19 — including embra-guardian-v1's in-OS toolchain) and [Intel Mac](INTEL-MAC-BUILD.md) (pending Intel Mac validation) build guides; bare metal and broader architecture support come in Phase 4
- **Tested on limited platforms** — built and verified on Ubuntu 24.04 + 26.04 under QEMU 8.2.2; bootable image also runs under QEMU on Intel and Apple Silicon Macs
- **Built-in tools only** — no MCP server modules (yet)
- **No on-device LLM inference** — Phase 5 will add `embraOS-QNM` for sovereign on-host inference. Sprint 5's OpenAI-compat support is the foundation for Phase 3's hybrid local/API routing.
