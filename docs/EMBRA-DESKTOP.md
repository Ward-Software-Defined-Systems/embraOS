# embra-desktop — In-OS GUI Migration (Experimental)

**Status:** Experimental, on the `embra-desktop` branch only. Phase 1 stable on `main` is unchanged. Phase 2 published roadmap commitment ("Full TUI rewrite") is unchanged. If this experiment validates, scope negotiation is a separate conversation.

**Branch:** `embra-desktop` (created 2026-05-07 from `main` at `v0.5.0-phase1` `69200e9`).

## What This Replaces

The serial-TTY ratatui TUI (`crates/embra-console/`) connected to `embra-apid` over gRPC is replaced (within this branch only) by a graphical operator interface running inside embraOS:

- `embra-comp` — single-fullscreen-client Wayland kiosk compositor on smithay
- `embra-desktop` — iced GUI client consuming `embra-console-core`

Both clients use the same gRPC contract as the TUI; no brain-side or apid-side changes.

## Architecture

```
QEMU + virtio-gpu (1280x720)
  └── kernel + DRM/virtio-gpu driver
        └── embrad (PID 1)
              ├── wardsondb / embra-trustd / embra-apid / embra-brain (unchanged)
              └── embra-comp (Wayland kiosk on /dev/tty1)
                    └── embra-desktop (iced client → apid:50000)
```

Detection: when the kernel cmdline carries `embra.desktop=1` AND `/sbin/embra-comp` is present in the rootfs, `embrad` registers `embra-comp` + `embra-desktop` instead of `embra-console`.

## Components

| Crate | Role | LoC | Notes |
|---|---|---|---|
| `embra-console-core` | UI-agnostic shared core | ~900 | gRPC client, state machine, slash commands, reasoning buffer, styled-text parsers, neutral style enums. Both TUI and GUI consume it. |
| `embra-comp` | Wayland kiosk compositor | ~700 (winit) + scaffold (tty-udev) | smithay 0.7. Single fullscreen toplevel; second toplevel denied. `--winit` for nested dev, `--halt-reason` for soul-mismatch UX. |
| `embra-desktop` | iced GUI client | ~600 | iced 0.14 with `tiny-skia` software renderer. Four-panel layout (header / expression-or-reasoning / conversation / input). gRPC subscription bridge with reconnect-on-failure. Keyboard shortcuts (selector arrows, scroll, Ctrl+C). Auto-scroll to latest. |

## Build Modes

```bash
# Default — graphics defconfig (Mesa3D + Wayland + fonts)
./scripts/build-image.sh --storage-engine fjall

# Fallback — pre-experiment minimal defconfig (TUI on serial, no graphics)
EMBRA_NO_DESKTOP=1 ./scripts/build-image.sh --storage-engine fjall

# QEMU run modes
./scripts/run-qemu.sh                      # serial TUI (-nographic)
EMBRA_DESKTOP=1 ./scripts/run-qemu.sh      # graphical session (1280x720 GTK window)
```

The graphics defconfig adds ~85-95 MB to `rootfs.squashfs` (LLVM + Mesa3D dominate). Cap is 200 MB; current build is well under.

## Stage Summary

| Stage | Status | Description |
|---|---|---|
| 0 | ✅ | Doc-verification artifact (gitignored at `embraOS-Phase1-Implementation/embra-desktop/DOC-VERIFICATION.md`) — confirmed smithay 0.7, iced 0.14, Mesa-on-musl in Buildroot 2026.02.1 |
| 1 | ✅ | `embra-console-core` extraction — UI-agnostic logic moved out of the TUI |
| 2 | ✅ | Buildroot graphics packages — Mesa/Wayland/seatd/fonts. **Mesa-under-musl canary PASSED** |
| 3a/3b | ✅ | `embra-comp` smithay scaffold + winit (nested) backend |
| 3c-structural | ✅ | session/udev/halt scaffold + Buildroot package recipe (gated off in defconfig) |
| 3c-final | ⏳ | Full DRM scanout pipeline (DrmDevice + GbmDevice + EGLContext + GlesRenderer + DrmCompositor + VBlank loop) — ~400-600 LOC, deferred for focused session against QEMU |
| 4a | ✅ | iced client scaffold — four-panel layout against static AppState |
| 4b | ✅ | gRPC subscription bridge + Submit routing |
| 4c | ✅ | Keyboard shortcuts (selector arrows, scroll, Ctrl+C exit) |
| 4d | partial | Auto-scroll to latest. Modal styling, multi-line (`text_editor` swap), Alt+Enter, theme palette polish — pending |
| 5 | ✅ | embrad supervisor wiring — desktop-mode detection, embra-comp + embra-desktop service definitions, dup2 stdio handover, soul-halt embra-comp spawn |
| 6 | this | Documentation + version markers |

## Deferred Work to Activate Production Boot

When you want the QEMU image to actually boot into the graphical session, these are the remaining steps:

1. **Stage 3c-final**: implement `crates/embra-comp/src/tty_udev.rs::run` body — DRM/EGL/GLES pipeline, libinput routing, VBlank loop. Anvil's `udev.rs` (~1500 LOC) is the seed; kiosk variant skips MultiRenderer/GpuManager.
2. **Cross-compile pipeline**: extend `scripts/build-image.sh` with a second cargo pass for `embra-comp` + `embra-desktop` that points `PKG_CONFIG_LIBDIR` / `PKG_CONFIG_SYSROOT_DIR` at Buildroot's `output/staging/` so musl-link finds pixman/xkbcommon/wayland/etc.
3. **Defconfig flip**: uncomment `BR2_PACKAGE_EMBRA_COMP=y` and `BR2_PACKAGE_EMBRA_DESKTOP=y` in `buildroot/configs/embraos_x86_64_defconfig`.
4. **embra-desktop Buildroot recipe**: mirror `buildroot/package/embra-comp/` for `embra-desktop`.
5. **rebuild + test** with `EMBRA_DESKTOP=1 ./scripts/run-qemu.sh`.

Until those land, the iced GUI client runs **host-side only** against a brain in QEMU:

```bash
# Terminal 1: boot embraOS in TUI mode
./scripts/run-qemu.sh

# Terminal 2: connect from the host
cargo run -p embra-desktop -- --apid-addr http://localhost:50000
```

## Host Dev Dependencies

For `cargo build/test/run` of `embra-comp` and `embra-desktop` on `wsds-devops`:

```bash
sudo apt install -y \
    libseat-dev libudev-dev libinput-dev \
    libgbm-dev libdrm-dev \
    libegl1-mesa-dev libgles2-mesa-dev \
    libpixman-1-dev libxkbcommon-dev libwayland-dev
```

These are link-time deps; runtime libs in the rootfs are provided by Buildroot.

## Key Locked Decisions (from Stage 0)

| Decision | Choice | Why |
|---|---|---|
| Topology | In-OS graphical session | The experiment is in-OS, not host client |
| Toolkit | iced 0.14 (bare, not libcosmic) | libcosmic pre-1.0 churn risk; iced is published-stable |
| Compositor | smithay-built kiosk | cosmic-comp brings full-DE features we don't need |
| Renderer | iced `tiny-skia` software path | Smaller binary; no Vulkan loader; CPU-only @ 1280×720 is fine |
| libc | Stay on musl | Uniform with `main`; canary verified Mesa+musl works |
| TUI fallback | Retained behind `EMBRA_NO_DESKTOP=1` | Quick recovery / no-graphics builds |
| Audio | Out of scope | If Embra ever speaks, separate effort |
| Image-size cap | 200 MB rootfs.squashfs | Current build well under |

## Privacy / Security Invariants Preserved

- gRPC contract unchanged — both TUI and GUI consume identical `ConsoleEvent` decode arms
- Reasoning privacy contract: `ReasoningDelta` still off `full_response`, never persisted, never replayed (`embra-console-core::events::handle_console_event` is the single reducer)
- 90 ToolDescriptors unchanged
- Soul verification path unchanged; halt-mode embra-comp adds visual UX without altering the underlying halt invariant
- No new attack surface: embra-comp doesn't open external sockets; embra-desktop connects to `127.0.0.1:50000` only
