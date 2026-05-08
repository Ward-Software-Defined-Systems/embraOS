# embra-desktop — In-OS GUI Migration (Experimental)

**Status:** Experimental, on the `embra-desktop` branch only. Phase 1 stable on `main` is unchanged. Phase 2 published roadmap commitment ("Full TUI rewrite") is unchanged. If this experiment validates, scope negotiation is a separate conversation.

**Branch:** `embra-desktop` (created 2026-05-07 from `main` at `v0.5.0-phase1` `69200e9`).

## What This Replaces

The serial-TTY ratatui TUI (`crates/embra-console/`) connected to `embra-apid` over gRPC is replaced (within this branch only) by a graphical operator interface running inside embraOS:

- `cage` — single-fullscreen-client Wayland kiosk compositor (wlroots-based, in Buildroot). Owns `/dev/tty1`, `/dev/dri/card0`, and `/dev/input/*` at boot.
- `embra-desktop` — iced GUI client consuming `embra-console-core`. Runs as cage's only fullscreen Wayland client.

Both clients use the same gRPC contract as the TUI; no brain-side or apid-side changes.

## Architecture

```
QEMU + virtio-gpu (1280x720)
  └── kernel + DRM/virtio-gpu driver
        └── embrad (PID 1)
              ├── wardsondb / embra-trustd / embra-apid / embra-brain (unchanged)
              └── cage (Wayland kiosk on /dev/tty1)
                    └── embra-desktop (iced client → apid:50000)
```

Detection: when the kernel cmdline carries `embra.desktop=1` AND `/usr/bin/cage` and `/usr/bin/embra-desktop` are both present in the rootfs, `embrad` spawns `cage -- /usr/bin/embra-desktop` instead of `embra-console`.

## Components

| Crate / Package | Role | Source | Notes |
|---|---|---|---|
| `embra-console-core` | UI-agnostic shared core | this repo | gRPC client, state machine, slash commands, reasoning buffer, styled-text parsers, neutral style enums. Both TUI and GUI consume it. |
| `embra-desktop` | iced GUI client | this repo | iced 0.14 with `tiny-skia` software renderer. Four-panel layout (header / expression-or-reasoning / conversation / input). gRPC subscription bridge with reconnect-on-failure. Keyboard shortcuts (selector arrows, scroll, Ctrl+C). Auto-scroll to latest. |
| `cage` | Wayland kiosk compositor | Buildroot `BR2_PACKAGE_CAGE` | wlroots-based, C. Spawns one fullscreen Wayland client and supervises it. Handles DRM/KMS scanout, libinput input, seat management. |
| `embra-comp` | Smithay kiosk compositor (winit only) | this repo | Retained as a host-side dev tool. Run via `cargo run -p embra-comp -- --winit` for nested-Wayland iteration without booting the QEMU image. NOT shipped in the rootfs. |

## Build & Run

```bash
# Default — graphics defconfig (Mesa3D + Wayland + cage + embra-desktop)
./scripts/build-image.sh --storage-engine fjall

# Fallback — pre-experiment minimal defconfig (TUI on serial, no graphics)
EMBRA_NO_DESKTOP=1 ./scripts/build-image.sh --storage-engine fjall

# QEMU run modes
./scripts/run-qemu.sh                      # serial TUI (-nographic)
EMBRA_DESKTOP=1 ./scripts/run-qemu.sh      # graphical session (1280x720 GTK window)
```

The graphics defconfig adds ~85-95 MB to `rootfs.squashfs` (LLVM + Mesa3D dominate; cage + wlroots are small). Cap is 200 MB; current build well under.

## Build Pipeline

`scripts/build-image.sh` cross-compiles `embra-desktop` against Buildroot's staging tree because iced 0.14 pulls libwayland-client / xkbcommon / softbuffer via FFI — the standalone `/opt/x86_64-linux-musl-cross` toolchain doesn't ship those libs.

Steps:

1. **Step 1**: cargo build all non-FFI crates with the standalone musl.cc toolchain (existing).
2. **Step 4**: Buildroot's main pass — kernel + userspace libs + Buildroot packages.
3. **Step 4.5**: cargo build `embra-desktop` against Buildroot's staging (toolchain + pkg-config switched to Buildroot's host musl).
4. **Step 4.6**: stage the binary into `buildroot/board/embraos/rootfs_overlay/usr/bin/embra-desktop`.
5. **Step 4.7**: re-run Buildroot to fold the overlay binary into `rootfs.squashfs`.

`embra-comp` is excluded from cross-compile (its smithay deps need the same staging plumbing, but it's no longer in the boot path).

## Stage Summary

| Stage | Status | Description |
|---|---|---|
| 0 | ✅ | Doc-verification (gitignored at `embraOS-Phase1-Implementation/embra-desktop/DOC-VERIFICATION.md`) |
| 1 | ✅ | `embra-console-core` extraction |
| 2 | ✅ | Buildroot graphics packages — Mesa-under-musl canary **PASSED** |
| 3a/3b | ✅ | `embra-comp` smithay scaffold + winit (host-side dev) |
| 3c | replaced | tty-udev DRM body deferred — replaced by cage pivot |
| 4a-4d | ✅ | iced client (scaffold, gRPC subscription, keyboard shortcuts, auto-scroll) |
| 5 | ✅ | embrad supervisor wiring + desktop-mode detection |
| 6 | ✅ | Documentation + cage pivot |

## Host Dev Dependencies

For `cargo build/test/run` of `embra-comp` and `embra-desktop` on `wsds-devops`:

```bash
sudo apt install -y \
    libseat-dev libudev-dev libinput-dev \
    libgbm-dev libdrm-dev \
    libegl1-mesa-dev libgles2-mesa-dev \
    libpixman-1-dev libxkbcommon-dev libwayland-dev
```

These are link-time deps; the Buildroot rootfs provides runtime libs.

## Key Locked Decisions (from Stage 0)

| Decision | Choice | Why |
|---|---|---|
| Topology | In-OS graphical session | The experiment is in-OS, not host client |
| GUI toolkit | iced 0.14 (bare, not libcosmic) | libcosmic pre-1.0 churn risk; iced is published-stable |
| Compositor | cage (wlroots, C) | Originally tried smithay-built kiosk; pivoted because writing a production smithay tty-udev backend is a multi-day focused effort and cage ships today, validated |
| Renderer | iced `tiny-skia` software path | Smaller binary; no Vulkan loader; CPU-only @ 1280×720 is fine |
| libc | Stay on musl | Uniform with `main`; canary verified Mesa+musl works |
| TUI fallback | Retained behind `EMBRA_NO_DESKTOP=1` | Quick recovery / no-graphics builds |
| Audio | Out of scope | If Embra ever speaks, separate effort |
| Image-size cap | 200 MB rootfs.squashfs | Current build well under |

## Soul-Halt UX

When the trustd soul-verification fails at boot, embrad calls `halt_system()` which writes the reason to `/embra/state/halt_reason` and halts the kernel. Pre-pivot we'd planned a visible halt-screen rendered by `embra-comp --halt-reason`; that path was scaffolded but not implemented (rendering text to a DRM framebuffer without a compositor is its own piece of work). Operators on a graphics boot currently see whatever the kernel framebuffer console showed last — typically blank. Post-mortem reads the halt reason from STATE.

A small `embra-fb-halt` helper that renders text via `/dev/fb0` could land later if the operator-visible halt screen turns out to matter.

## Privacy / Security Invariants Preserved

- gRPC contract unchanged — both TUI and GUI consume identical `ConsoleEvent` decode arms
- Reasoning privacy contract: `ReasoningDelta` still off `full_response`, never persisted, never replayed (`embra-console-core::events::handle_console_event` is the single reducer)
- 90 ToolDescriptors unchanged
- Soul verification path unchanged
- No new outbound network surface: `embra-desktop` connects to `127.0.0.1:50000` only; cage doesn't open external sockets
