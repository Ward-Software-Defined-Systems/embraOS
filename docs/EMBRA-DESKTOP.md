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

### Host setup (one-time)

The image build needs the Rust toolchain, the standalone musl.cc cross-toolchain at `/opt/x86_64-linux-musl-cross`, and Buildroot's standard prerequisites — see the main README for the canonical host-prereq list.

If you also want to iterate on `embra-comp` or `embra-desktop` host-side via `cargo run` (nested-Wayland mode — exercises the GUI without booting QEMU), install the matching link-time graphics deps:

```bash
sudo apt install -y \
    libseat-dev libudev-dev libinput-dev \
    libgbm-dev libdrm-dev \
    libegl1-mesa-dev libgles2-mesa-dev \
    libpixman-1-dev libxkbcommon-dev libwayland-dev
```

These apt packages are only needed for host-side cargo iteration; the Buildroot rootfs provides matching runtime libs separately.

### Build the image

```bash
# Default — graphics defconfig (Mesa3D + Wayland + cage + embra-desktop)
./scripts/build-image.sh --storage-engine fjall

# Fallback — pre-experiment minimal defconfig (TUI on serial, no graphics)
EMBRA_NO_DESKTOP=1 ./scripts/build-image.sh --storage-engine fjall
```

The graphics defconfig adds ~85-95 MB to `rootfs.squashfs` (LLVM + Mesa3D dominate; cage + wlroots are small). Cap is 200 MB; current build well under.

### Run

```bash
./scripts/run-qemu.sh                      # serial TUI (-nographic)
EMBRA_DESKTOP=1 ./scripts/run-qemu.sh      # graphical session (1280x720 SDL/GTK window)
```

| Env var | Default | Effect |
|---|---|---|
| `EMBRA_DESKTOP` | `0` | `1` flips embrad to spawn cage + iced GUI instead of the serial TUI. |
| `EMBRA_DISPLAY` | `auto` | `gtk\|sdl\|vnc\|spice` — overrides QEMU's display backend. Default picks SDL when a display server is reachable, VNC on `localhost:5900` otherwise. |
| `EMBRA_SERIAL_LOG` | `$HOME/embraos-serial.log` | Guest serial output path (graphics mode only — TUI mode goes to stdio). Lives on persistent storage so the file survives a host-VM crash; the script also pre-creates the file, fsyncs metadata, and runs a 1s `sync --data` loop while QEMU is up. |
| `EMBRA_CPU` | (auto) | Override the auto-picked QEMU `-cpu` model. Auto = `host` on bare-metal Linux/Darwin, `qemu64` (QEMU default) inside any hypervisor. Mostly used for diagnosing nested-virt issues — e.g. `EMBRA_CPU=Nehalem` drops AVX/AES claims some nested KVM implementations can't honor. |
| `EMBRA_NO_DESKTOP` | (build-time) | `1` selects the minimal defconfig (TUI only, no Mesa3D/cage in the rootfs). |

QEMU's own stderr (CPU-feature warnings, KVM init notices) is captured to `${EMBRA_SERIAL_LOG%.log}-qemu.log` (default `$HOME/embraos-serial-qemu.log`) — useful when QEMU dies before any guest serial output is produced.

## Build Pipeline

`scripts/build-image.sh` cross-compiles `embra-desktop` against Buildroot's staging tree because iced 0.14 pulls libwayland-client / xkbcommon / softbuffer via FFI — the standalone `/opt/x86_64-linux-musl-cross` toolchain doesn't ship those libs.

**Two non-obvious Kconfig hinges** (both bit us with silent-drop failures during bring-up — Buildroot doesn't fail the build, it just writes a `# <pkg> needs ...` comment in the resolved `.config` and moves on):

- **`BR2_TOOLCHAIN_BUILDROOT_CXX=y`** is mandatory. Mesa3D, LLVM, wlroots, and cage all depend on it transitively (Mesa3D's `depends on BR2_INSTALL_LIBSTDCPP`). Without it the toolchain doesn't ship a `g++` and all four packages drop — `/usr/bin/cage` is never built and `embrad` falls back to TUI even with `embra.desktop=1` set.
- **`BR2_PACKAGE_MESA3D_LLVM=y`** must be explicit. It is *not* auto-selected by `BR2_PACKAGE_LLVM=y` + `BR2_PACKAGE_MESA3D_GALLIUM_DRIVER_LLVMPIPE=y`. Without it, LLVMPIPE drops → no Gallium driver → `BR2_PACKAGE_MESA3D_GBM` drops → `BR2_PACKAGE_HAS_LIBGBM` is never provided → cage and wlroots silently drop.

Sanity check after build: `grep -E '^BR2_PACKAGE_(CAGE|WLROOTS|MESA3D|MESA3D_LLVM|MESA3D_GBM|HAS_LIBGBM)=y' buildroot-src/.config` should return six matches. If any are missing, grep for `# <pkg> needs` to find the unsatisfied dep.

Steps:

1. **Step 1**: cargo build all non-FFI crates with the standalone musl.cc toolchain. Produces static-pie binaries (`embrad`, `embra-apid`, `embra-trustd`, `embra-brain`, `embra-console`).
2. **Step 4**: Buildroot's main pass — kernel + userspace libs + Buildroot packages.
3. **Step 4.5**: cargo build `embra-desktop` against Buildroot's staging (toolchain + pkg-config switched to Buildroot's host musl).
   **Critical:** `CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=-crt-static"`. iced 0.14 → winit 0.30 → wayland-client 0.31 calls `dlopen("libwayland-client.so.0")` at runtime, and a fully-static musl binary can't reliably load `.so` files. Symptom of getting this wrong: `embra-desktop` panics with `WaylandError(Connection(NoWaylandLib))`. The rootfs ships `/lib/ld-musl-x86_64.so.1` plus the needed libs in `/usr/lib`, so dynamic-link is fine; only `embra-desktop` opts out of crt-static.
4. **Step 4.6**: stage the binary into `buildroot/board/embraos/rootfs_overlay/usr/bin/embra-desktop`.
5. **Step 4.7**: re-run Buildroot to fold the overlay binary into `rootfs.squashfs`.

`embra-comp` is excluded from cross-compile (its smithay deps need the same staging plumbing, but it's no longer in the boot path).

## Boot Wiring (embrad runtime setup)

cage and embra-desktop need a handful of things at runtime that aren't in the SquashFS overlay or QEMU's defaults. `embrad` (PID 1) sets them up before spawning cage.

| What | Where | Why |
|---|---|---|
| `/dev/shm` tmpfs mount | `crates/embrad/src/mount.rs` (after `/dev/pts`) | wlroots `shm_open()` for keymap + dmabuf format table. Without it, cage hands clients a stub keymap → segfault inside `libxkbcommon` on first keypress (`cage[N]: segfault at 80 ... in libxkbcommon.so.0.9.2`). |
| `/run/user/0` mode 0700 | `crates/embrad/src/mount.rs` (after the `/run` tmpfs) | XDG basedir spec. cage's libwayland binds its socket at `$XDG_RUNTIME_DIR/wayland-0`; bind silently fails if the dir doesn't exist. cage's manpage doesn't document the requirement. The `post_build.sh` placeholder is masked by the tmpfs mount. |
| udevd daemon | `crates/embrad/src/mount.rs::start_udevd` | cage / wlroots / libinput need device-add events for `/dev/input/*` and `/dev/dri/*`. Provided by eudev (`BR2_PACKAGE_EUDEV=y`). |
| `XDG_RUNTIME_DIR=/run/user/0` env | `crates/embrad/src/supervisor.rs` cage spawn | XDG basedir env that libwayland reads. |
| cage stderr → `/dev/console` | `crates/embrad/src/supervisor.rs` cage spawn | Visibility. cage's stdout goes to `/dev/tty1` (graphics surface, invisible to operator); routing stderr to `/dev/console` (= ttyS0 = host's `/tmp/embra-serial.log`) makes cage's wlroots/EGL/libseat error output debuggable. |

Kernel cmdline (set by `scripts/run-qemu.sh` in graphics mode):

| Flag | Why |
|---|---|
| `console=tty0 console=ttyS0` | tty0 = visible kernel framebuffer console; ttyS0 *last* so `/dev/console` maps to it for `embrad`'s tracing logs. |
| `vt.handoff=1` | Kernel framebuffer console releases the CRTC when cage takes DRM master, instead of fighting back with damage-update workqueue items (which produced repeating `[CRTC:N:crtc-0] vblank wait timed out` WARNINGs). |
| `vt.global_cursor_default=0` | Hides kernel text cursor so it doesn't flicker through cage's surface during the handoff. |
| `embra.desktop=1` | `embrad` supervisor switch — spawns `cage -- /usr/bin/embra-desktop` instead of `embra-console`. |

QEMU display devices (graphics mode):

| Flag | Why |
|---|---|
| `-vga none` | Strips QEMU's default Cirrus/std VGA card. Without it, the guest sees TWO display devices (default VGA + virtio-gpu). SDL/GTK opens two windows, `/dev/dri/card0` ends up bound to VGA, cage opens the wrong DRM device, and the kernel framebuffer keeps writing to the unclaimed virtio-gpu surface. |
| `-device virtio-gpu-pci,xres=1280,yres=720` | Software-rendered display device matching iced's window size. |
| `-device virtio-keyboard-pci -device virtio-tablet-pci` | Input devices via virtio. |

QEMU acceleration (graphics or TUI mode):

| Flag / behavior | Why |
|---|---|
| `-accel kvm` if `/dev/kvm` is r/w; `-accel hvf` on Darwin; otherwise TCG | Auto-detected. KVM/HVF give native speed; TCG is software emulation, ~5-10× slower but always works. |
| `-cpu host` *only when bare metal* | Gated on `systemd-detect-virt == none` in `scripts/run-qemu.sh:46-65`. Inside a hypervisor (Parallels, VMware, Hyper-V, KVM-on-KVM) the L0 advertises CPU features through CPUID that it can't actually emulate when the L1 guest tries to use them — passing `-cpu host` causes hard lockups of the L1 VM. We hit this on Parallels: kernel decompressed, jumped, used a passthrough feature on first instruction, the entire Ubuntu VM froze. |
| **Parallels-Intel: nested virt is unstable, period** | After fixing every other lockup cause, KVM-on-Parallels-Intel still hard-locks the host VM ~1s into boot — in BOTH TUI and graphics modes, with the default `qemu64` model AND with `-cpu host`. The `-cpu host` gate is necessary but not sufficient. Not fixable from the QEMU side; the L0's CPUID lies. **Disable nested virtualization in the Parallels VM settings and accept TCG.** ~30s boot to UI is the working steady state on this host class. |
| `EMBRA_CPU=<model>` override | Bypass the auto-picked CPU model for diagnostic A/B testing on hosts where nested KVM might work — e.g. `EMBRA_CPU=Nehalem` for a constrained nested-friendly model, or `EMBRA_CPU=host` to force passthrough regardless of the gate. Has not rescued KVM-on-Parallels-Intel. |

## Common Build & Boot Pitfalls

Compiled list of traps we hit while bringing this up. If you hit one of these symptoms, jump to the cause.

| Symptom | Cause | Fix |
|---|---|---|
| Boot reaches `Service embra-console is healthy`, QEMU window shows kernel text only | cage missing from rootfs (Buildroot silently dropped it) | Verify `BR2_TOOLCHAIN_BUILDROOT_CXX=y` and `BR2_PACKAGE_MESA3D_LLVM=y` in resolved `buildroot-src/.config`. Grep for `# .* needs` comments to find unmet deps. Toolchain change requires `rm -rf buildroot-src/output && ./scripts/build-image.sh`. |
| Boot reaches `Service cage is healthy`, screen frozen on last kernel printk | `/run/user/0` not created at runtime → libwayland socket bind silently failed | `embrad`'s `mount_pseudofs` creates `/run/user/0` mode 0700 after the `/run` tmpfs mount. |
| Two QEMU windows open | Default VGA card + virtio-gpu both present | `-vga none` in `scripts/run-qemu.sh` graphics-mode `DISPLAY_ARGS`. |
| Repeating `[CRTC:N:crtc-0] vblank wait timed out` WARNINGs | Kernel framebuffer console fighting cage for the CRTC | `vt.handoff=1` on kernel cmdline. |
| `embra-desktop` panics with `WaylandError(Connection(NoWaylandLib))` | Static-pie musl binary can't dlopen `libwayland-client.so.0` | Drop `+crt-static` for `embra-desktop` in Step 4.5 (`CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_RUSTFLAGS="-C target-feature=-crt-static"`). |
| `cage[N]: segfault at 80 ... in libxkbcommon.so.0.9.2` on keyboard input | wlroots `shm_open()` for keymap fails — `/dev/shm` not mounted | `embrad`'s `mount_pseudofs` mounts `/dev/shm` tmpfs after `/dev/pts`. |
| Parallels-Intel host VM freezes when running QEMU+KVM | KVM-on-Parallels-Intel is broadly unstable. The `-cpu host` gate (`systemd-detect-virt == none` in `scripts/run-qemu.sh`) is necessary but not sufficient — qemu64 default also locks up in both TUI and graphics modes (~1s into boot, fast enough that even our 1s `sync` loop loses the logs). | **Disable nested virtualization in the Parallels VM settings.** TCG is the working default on this host class. `-cpu host` gate stays in regardless. |
| Boot is slow (~30s) under Parallels-Intel | TCG software emulation. KVM is not a viable path on this host class (see entry above). | Live with it, or move dev to bare-metal Linux / a different hypervisor where the `-cpu host` gate auto-engages. |
| Logs missing after a host-VM crash | `/tmp` is tmpfs on Ubuntu 26.04 — wiped on reboot. Page cache lost on hard reset. | Log path defaults to `$HOME/embraos-serial.log` (persistent). Script pre-creates files + runs background `sync --data` loop. Sub-second crashes can still lose recent writes — for those, route serial off-VM (shared folder / network), not yet implemented. |

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
| 7 | ✅ | Wiring catch-up (toolchain CXX, MESA3D_LLVM, `/run/user/0`, `/dev/shm`, embra-desktop dynamic-link, `-vga none`, `vt.handoff=1`, cage stderr → /dev/console, `-cpu host` gated to bare metal). Boot now reaches the iced UI on first run; keyboard input works. See "Boot Wiring" + "Common Build & Boot Pitfalls" sections. |

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
