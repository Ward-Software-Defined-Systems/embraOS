# Operation

Day-to-day use of embraOS once it is running.

## The Session Model

Every interaction with embraOS happens in a persistent session. Sessions survive disconnections. When you reconnect, the full conversation history is restored and the AI provides a briefing on what happened while you were away.

You can run multiple named sessions for different contexts:

```
/new research         # Create a research-focused session
/new monitoring       # Create a monitoring session
/switch main          # Switch back to the main session
/sessions             # List all sessions
```

All sessions share the same intelligence — same memory, same identity, same soul. But each has its own conversation history and context.

## Keyboard Shortcuts

**embraOS TUI** — in-conversation:

| Key | Action |
|---|---|
| `Enter` | Send message (or newline in `/ml` multi-line mode) |
| `Alt+Enter` | New line |
| `Up/Down` | Scroll history |
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
- **QEMU x86_64 (recommended)** — an experimental aarch64/Apple Silicon build is available — see the [aarch64 / Apple Silicon build guide](AARCH64-BUILD.md) (re-synced with the canonical build at `f8cad9c` and end-to-end QEMU-verified on both x86_64 and aarch64 — aarch64 on a MacBook M2, 2026-05-18/19 — including embra-guardian-v1's in-OS toolchain); bare metal and broader architecture support come in Phase 4
- **Tested on limited platforms** — built and verified on Ubuntu 24.04 + 26.04 under QEMU 8.2.2; bootable image also runs under QEMU on Intel and Apple Silicon Macs
- **Built-in tools only** — no MCP server modules (yet)
- **No on-device LLM inference** — Phase 5 will add `embraOS-QNM` for sovereign on-host inference. Sprint 5's OpenAI-compat support is the foundation for Phase 3's hybrid local/API routing.
