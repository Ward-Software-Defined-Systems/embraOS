# embraOS — aarch64 Apple Silicon Build Guide

> **Target:** QEMU-bootable ARM64 disk image running natively on Apple Silicon via HVF —
> near-native speed, no 5–10× TCG software-emulation penalty.
>
> **Host:** macOS on Apple Silicon (M1/M2/M3/M4).
>
> ✅ **Status — re-verified end-to-end on Apple Silicon (2026-05-18).** Re-synced with
> the canonical x86_64 build at commit `f8cad9c` and verified on a MacBook (Apple M2,
> 8 GB) against Buildroot `2026.02.1`, storage engine `rocksdb`: the full build
> pipeline (Step 0.5 Trunk/WASM frontend → Rust cross-compile → initramfs → Step 3.5
> in-OS Rust toolchain → Buildroot, including the new `embra-web` and
> `embra-rust-toolchain` packages), HVF boot, soul formation, the serial TUI
> (`EMBRA_TUI=1`), and **embra-guardian-v1** (a dynamic tool compiled in-guest via
> `/opt/rust` and run in the wasmtime sandbox). embra-guardian-v1 remains an
> **experimental** feature upstream (project-wide, not aarch64-specific). On an 8 GB
> host, cap build parallelism — see the `JOBS` knob below. Re-run the [End-to-End
> Validation](#end-to-end-validation) checklist after any canonical-build bump and
> re-stamp this date.
>
> **Single source of truth (2026-05-19):** the aarch64 scripts are committed
> in-tree (`scripts/`, `buildroot/configs/`) and the arch-parameterized Buildroot
> tree (commit `f6a684a`) is end-to-end QEMU-verified on **both** x86_64 and
> aarch64. The former standalone Apple-Silicon build bundle has been retired —
> this guide now lives at `docs/AARCH64-BUILD.md`, its scripts in `scripts/`.

---

## Quick Start

Builds a QEMU-bootable ARM64 embraOS image that runs natively on Apple Silicon via HVF.
Steps 0.5–3 (frontend + Rust cross-compile + initramfs) run on macOS; Step 3.5 (in-OS
Rust toolchain) and Step 4 (Buildroot) run in a `linux/arm64` Docker container.

> **Default UI is the browser console.** `run-qemu-aarch64.sh` boots the
> **embra-web** console by default, served over HTTPS at
> **https://localhost:3345/embraOS** (accept the embraOS-CA cert on first visit). Set
> **`EMBRA_TUI=1`** before `run-qemu-aarch64.sh` to boot the serial TUI on this
> terminal instead — no image rebuild needed.

> **zsh paste gotcha:** macOS's default `zsh` doesn't treat `#` as a comment in
> interactive shells — pasting a multi-line code block from this guide makes
> any `#`-prefixed (or inline `… # comment`) line fail with `command not found`
> or `no matches found`. Run once per session (or add to `~/.zshrc`):
> ```bash
> setopt interactive_comments
> ```

### 1. Prerequisites (macOS, Apple Silicon)

```bash
# Rust via rustup — NOT Homebrew (brew uninstall rust first if present)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup target add aarch64-unknown-linux-musl     # this build
rustup target add wasm32-unknown-unknown         # embra-web frontend (Step 0.5)
cargo install trunk --locked                     # Step 0.5 aborts if trunk is missing
```

```bash
# QEMU + cross toolchain + protoc + cmake (Homebrew)
brew install qemu filosottile/musl-cross/musl-cross protobuf cmake

# Docker runtime — OrbStack recommended (faster/lighter than Docker Desktop on
# Apple Silicon; both default to linux/arm64 containers, no Rosetta overhead)
brew install orbstack
```

The `build-image-aarch64.sh` script auto-detects the `musl-cross` path and exports
`CC/CXX/AR/RANLIB`, the cargo linker, and the bindgen sysroot — **no manual
`~/.cargo/config.toml` setup is required** for the standard flow.

### 2. Clone & build the image

Clone embraOS and build. WardSONDB is vendored in-tree at `crates/wardsondb` —
no sibling repo is needed. The aarch64 scripts ship committed in-tree, so there
is nothing to place. Everything runs from the embraOS repo root.

```bash
# Clone into ~/projects (keeps source + build artifacts across reboots).
mkdir -p ~/projects && cd ~/projects
git clone https://github.com/Ward-Software-Defined-Systems/embraOS.git
cd ~/projects/embraOS

# The aarch64 scripts (scripts/build-image-aarch64.sh, run-qemu-aarch64.sh,
# create_initramfs.sh, embraos-backup-mac.sh) and
# buildroot/configs/embraos_aarch64_defconfig ship committed in-tree — a fresh
# clone already has them; there is nothing to copy.

# Steps 0.5–3 on macOS: Trunk/WASM frontend → Rust cross-compile → initramfs.
# (Stops at the Step 4 guard with the next command — that's expected on macOS.)
# No per-arch patching: the Buildroot tree is arch-parameterized; the
# aarch64 defconfig (BR2_aarch64=y) drives the .mk paths + kernel name.
./scripts/build-image-aarch64.sh --storage-engine rocksdb

# Step 3.5 (in-OS Rust toolchain) + Step 4 (Buildroot) in Docker (linux/arm64)
docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
```

### 3. Boot

```bash
./scripts/run-qemu-aarch64.sh                  # web console (default) — https://localhost:3345/embraOS
EMBRA_TUI=1 ./scripts/run-qemu-aarch64.sh      # stable Phase 1 serial TUI on this terminal
EMBRA_DB_VERBOSE=1 ./scripts/run-qemu-aarch64.sh   # opt-in per-request WardSONDB log lines
```

Press `Ctrl-A X` to exit QEMU. On first boot the Config Wizard runs (name, LLM provider
+ credentials, timezone), then Learning Mode forms and seals the soul.

> **Storage engine:** `--storage-engine rocksdb` (battle-tested) or `fjall` (pure
> Rust) is required and is baked into the `embrad` binary at build time. WardSONDB
> locks the choice into the DATA partition on first boot via a `.engine` marker —
> switching later requires wiping DATA. `--buildroot-only` ignores the flag (Rust is
> not rebuilt there). Back up before switching.

> **Buildroot version:** Defaults to `2026.02.1` (LTS, Ubuntu 26.04 era). Override with
> `BUILDROOT_VERSION=2024.02 ./scripts/build-image-aarch64.sh ...`. The **first** build
> after this guide's update switches `buildroot-src` from `2024.02` → `2026.02.1` and
> **wipes `buildroot-src/output/`** (one long full rebuild). The switch is refused if
> you have local changes in `buildroot-src` — commit/stash them or pin
> `BUILDROOT_VERSION` to the current checkout.

> **Build memory / parallel jobs (`JOBS`):** the Docker pass runs `make -j$(nproc)`.
> Buildroot's GCC + musl toolchain bootstrap is RAM-hungry, and the `2026.02.1` switch
> forces a full from-scratch rebuild — on a memory-limited host (e.g. OrbStack's 4 GB
> default VM on an 8 GB Mac, `nproc`=8 → `-j8`) that OOMs (`ar: … Cannot allocate
> memory`, `libc.a Error 1`). Cap parallelism with the `JOBS` env (defaults to all
> cores — unchanged from canonical; set it **only** when constrained), passed into the
> container with `-e`:
> ```bash
> docker run --rm -e JOBS=2 -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
>   "apt-get update && apt-get install -y build-essential gcc g++ \
>    unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools && \
>    FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
> ```
> On a Linux host (no Docker), just prefix: `JOBS=2 ./scripts/build-image-aarch64.sh
> --buildroot-only`.

> **In-OS Rust toolchain (Guardian):** Step 3.5 downloads a SHA-256-verified Rust
> toolchain — host `rust-1.94.1-aarch64-unknown-linux-musl` + arch-agnostic
> `rust-std-1.94.1-wasm32-unknown-unknown` — into `vendor/rust-toolchain`, baked into
> the rootfs at `/opt/rust` for embra-guardian-v1. On macOS it is deliberately
> **deferred to the Docker pass** (macOS lacks `sha256sum`/`xz`); `vendor/rust-toolchain`
> rides the `-v "$PWD":/work` bind mount. First build needs network for this and adds
> ~100 MB to the image. Override the pin with `RUST_TOOLCHAIN_VERSION=...`.

> **Port forwarding:** QEMU forwards 50000 (gRPC) and 8443 (REST); in the default
> web-console mode it also forwards 3345 (HTTPS — https://localhost:3345/embraOS). Test:
> ```bash
> curl http://localhost:8443/health
> ```

> **Backup & restore:** macOS can't loop-mount the image natively, so
> `embraos-backup-mac.sh` runs the Linux backup script inside a privileged Docker
> container. Stop the VM first (`Ctrl-A X`).
> ```bash
> ./scripts/embraos-backup-mac.sh backup --label pre-rebuild
> ./scripts/embraos-backup-mac.sh restore
> ./scripts/embraos-backup-mac.sh list
> ```
> Backups live in `~/embraOS_BACKUPS/` (override `EMBRAOS_BACKUP_DIR`) and are
> interchangeable with Ubuntu backups.

---

## The Gory Details

Everything below is reference: the build pipeline explained step by step, the
cross-compilation internals, what differs from x86_64, backup internals, and
troubleshooting for when the happy path breaks.

### Why this build

Running an x86_64 guest under QEMU on Apple Silicon falls back to TCG software
emulation — 5–10× slower. An ARM64 guest runs through **HVF** (Hypervisor.framework)
with `-cpu host` on the `virt` machine at near-native speed. This guide produces that
ARM64 image and boots it under `qemu-system-aarch64 -accel hvf`.

### macOS prerequisites — deep dive

**Rust via rustup, not Homebrew.** If Rust was installed via Homebrew, remove it first
to avoid PATH conflicts:

```bash
brew uninstall rust
```

**Docker (OrbStack recommended).** Used for both the Buildroot image build and the
backup/restore workflow (both need Linux-only tooling). Docker Desktop also works;
OrbStack is faster and lighter on Apple Silicon. Both default to `linux/arm64`
containers on Apple Silicon — host tools run natively, Buildroot cross-compiles the
kernel and packages for aarch64.

**Musl cross-linker configuration.** Homebrew installs `musl-cross` in different
locations by Mac:

| Mac | Homebrew prefix |
|---|---|
| Apple Silicon (M1/M2/M3/M4) | `/opt/homebrew/Cellar/musl-cross/` |
| Intel | `/usr/local/Cellar/musl-cross/` |

Verify the toolchain:

```bash
ls /opt/homebrew/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc 2>/dev/null || \
ls /usr/local/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc
```

> **Legacy note:** Earlier versions of this guide had you set the linker path in
> `~/.cargo/config.toml`. That still works but is no longer required — the build script
> overrides it via env vars. An old entry there is harmless.

### The build pipeline, step by step

`build-image-aarch64.sh` orchestrates the whole pipeline. On macOS it runs Steps 0.5–3,
logs that Step 3.5 is deferred, and exits at the Step 4 guard printing the Docker
command. The Docker `--buildroot-only` pass then runs Step 3.5 and Step 4.

- **Step 0.5 — embra-web frontend (Trunk → WASM).** Builds `crates/embra-web-ui` with
  Trunk so `embra-web` can embed `dist/` via `rust-embed`. wasm32 output is
  host-arch-agnostic — identical on x86_64 and aarch64 hosts. Aborts if `trunk` is
  missing. Runs only when not `--buildroot-only`.
- **Step 1 — Rust binaries (aarch64 musl static).** `cargo build --release --target
  aarch64-unknown-linux-musl`. ~30 minutes, mostly compilation. Builds all 12
  workspace members including `wardsondb` (vendored at `crates/wardsondb` — no
  sibling repo; the former Step 2 is gone).
- **Step 3 — initramfs.** `create_initramfs.sh` (RUST_TARGET-aware) packs `embra-init`
  as `/init` into `initramfs.cpio.gz`.
- **Step 3.5 — in-OS Rust toolchain.** See [In-OS Rust toolchain](#in-os-rust-toolchain-embra-guardian-v1)
  below. Unconditional w.r.t. `--buildroot-only`; **skipped on macOS** and run in the
  Docker/Linux pass.
- **Step 4 — Buildroot.** Linux-only. Clones Buildroot, ensures it is at
  `$BUILDROOT_VERSION` (default `2026.02.1`) via the safe-switch, dircleans the embraOS
  + upstream packages, configures `embraos_aarch64_defconfig`, and builds the kernel
  (`Image`) + SquashFS rootfs + `embraos.img`.
- **Step 5 — copy outputs.** `embraos.img` and `Image` → `output/images/`.

**Isolated cross-compilation validation (optional).** To validate Rust cross-compilation
before wiring in the full pipeline, set the env vars the build script sets automatically
and run `cargo` directly:

```bash
MUSL_CROSS_BIN=$(ls -d /opt/homebrew/Cellar/musl-cross/*/libexec/bin 2>/dev/null || \
                 ls -d /usr/local/Cellar/musl-cross/*/libexec/bin)
MUSL_CROSS_ROOT=$(dirname "$MUSL_CROSS_BIN")
MUSL_SYSROOT="${MUSL_CROSS_ROOT}/aarch64-linux-musl"

export PATH="$MUSL_CROSS_BIN:$PATH"
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
export CXX_aarch64_unknown_linux_musl=aarch64-linux-musl-g++
export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar
export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc

# Plain AR/RANLIB for deps whose build.rs invokes configure/make directly
# (tikv-jemalloc-sys, etc). Without these, jemalloc's configure falls back to
# macOS's BSD ar and produces a 96-byte empty archive — link later fails with
# undefined references to _rjem_malloc, _rjem_sdallocx, etc.
export AR=aarch64-linux-musl-ar
export RANLIB=aarch64-linux-musl-ranlib
export RANLIB_aarch64_unknown_linux_musl=aarch64-linux-musl-ranlib

# Point bindgen/clang at the musl sysroot (required for zstd-sys, rocksdb-sys, etc.)
# Without this, bindgen picks up Xcode's clang headers and fails with:
#   fatal error: 'stddef.h' file not found
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_musl="--sysroot=${MUSL_SYSROOT} -I${MUSL_SYSROOT}/include"

# Storage engine — must match what you'll pass to build-image-aarch64.sh later
export EMBRA_STORAGE_ENGINE=rocksdb   # or fjall

cd embraOS
# Builds every workspace member, including the vendored wardsondb —
# target/aarch64-unknown-linux-musl/release/wardsondb lands here directly.
cargo build --release --target aarch64-unknown-linux-musl --workspace
```

> **bindgen-based crates:** the in-tree `wardsondb` crate pulls in `zstd-sys` (and
> `rocksdb-sys` with the RocksDB backend), both of which use `bindgen`. Without
> `BINDGEN_EXTRA_CLANG_ARGS`, clang finds Xcode's SDK headers and can't resolve
> cross-compile references.

> **The `wardsondb` crate's `aws-lc-sys` dependency:** has a C/assembly build step. `ToolNotFound`
> errors mentioning `aarch64-linux-musl-gcc` mean the musl-cross path isn't on `PATH`
> or `CC_aarch64_unknown_linux_musl` isn't set. The build script handles both.

### In-OS Rust toolchain (embra-guardian-v1)

embra-guardian-v1 compiles dynamic tools — whether an operator pastes the Rust module
or the intelligence proposes one, both soul-checked first (intelligence proposals are
also operator-approved) — to WebAssembly **with a Rust toolchain inside the image**, then
runs them in a `wasmtime` sandbox. Step 3.5 stages that toolchain.

- **What it downloads:** host `rust-1.94.1-aarch64-unknown-linux-musl` (aarch64 because
  it runs *inside* the aarch64 guest; `aarch64-unknown-linux-musl` is a Tier-2
  *with-host-tools* target so a prebuilt `rustc`/`cargo` exists) + arch-agnostic
  `rust-std-1.94.1-wasm32-unknown-unknown`. Both are SHA-256-verified against the
  `.sha256` sidecars from `static.rust-lang.org`.
- **Where it goes:** staged into `vendor/rust-toolchain`, installed read-only at
  `/opt/rust` in the rootfs by the `embra-rust-toolchain` Buildroot package. `rustc`
  resolves its sysroot relative to its own binary, so the staged tree is
  position-independent under the prefix.
- **macOS handling:** Step 3.5 is **skipped on macOS** (no `sha256sum`/`xz` by default)
  and runs in the Docker/Linux `--buildroot-only` pass. `vendor/rust-toolchain` rides
  the `-v "$PWD":/work` bind mount, so it persists to the host tree. The version-file
  (`RUST_VERSION`) check makes re-runs idempotent (`already staged — skipping`).
- **Arch parameterization (no sed):** the Buildroot tree builds either arch from one
  committed copy. `buildroot/external.mk` derives
  `EMBRAOS_RUST_TARGET = $(if $(BR2_aarch64),aarch64-unknown-linux-musl,x86_64-unknown-linux-musl)`,
  and every package `.mk` (`embrad`, `embra-web`, `wardsondb`, …) uses
  `target/$(EMBRAOS_RUST_TARGET)/release` in its `_SITE`. `embra-rust-toolchain.mk` is
  arch-agnostic (it copies `vendor/rust-toolchain`, no target triple). The kernel
  filename is resolved by `post_image.sh` from `genimage.cfg.in`. Selecting the arch is
  entirely the defconfig's job (`embraos_aarch64_defconfig` sets `BR2_aarch64=y`).

### What Changed from x86_64

#### aarch64-specific files (committed in-tree)

| File | Purpose |
|---|---|
| `buildroot/configs/embraos_aarch64_defconfig` | ARM64 Buildroot configuration |
| `scripts/build-image-aarch64.sh` | Full build pipeline for aarch64 |
| `scripts/run-qemu-aarch64.sh` | QEMU launch with HVF acceleration |
| `scripts/embraos-backup-mac.sh` | macOS wrapper for backup/restore via Docker |

#### Modified files (backward-compatible — x86_64 build unchanged)

These are arch-**parameterized**, not arch-flipped: a single committed tree builds
both arches, so there is **no per-arch sed** and nothing to revert.

| File | Change |
|---|---|
| `scripts/create_initramfs.sh` | Respects `RUST_TARGET` env var; defaults to `x86_64-unknown-linux-musl` when unset |
| `buildroot/external.mk` | Defines `EMBRAOS_RUST_TARGET = $(if $(BR2_aarch64),aarch64-…,x86_64-…)` |
| `buildroot/package/*/*.mk` (7) | `_SITE` now uses `target/$(EMBRAOS_RUST_TARGET)/release` — arch-neutral |
| `buildroot/board/embraos/post_image.sh` | Detects the kernel (`Image`/`bzImage`) in `BINARIES_DIR` and renders the template |
| `buildroot/board/embraos/genimage.cfg` → `…/genimage.cfg.in` | Static config replaced by a template with `@KERNEL_IMAGE@` (resolved per build) |

#### How the aarch64 pipeline tracks canonical (`f8cad9c`)

| Area | aarch64 behaviour (matches canonical x86_64) |
|---|---|
| Step 0.5 (Trunk → WASM) | Builds `crates/embra-web-ui` before the cargo cross-build; needs host `trunk` + `wasm32-unknown-unknown` |
| Step 3.5 (in-OS Rust toolchain) | Stages `vendor/rust-toolchain` (aarch64 host triple); macOS-deferred to the Docker pass |
| Buildroot pin | `2024.02` → `2026.02.1` with the idempotent safe-switch (refuses on local changes; wipes `output/` on switch) |
| dirclean list | Adds `embra-web` and `embra-rust-toolchain` |
| defconfig packages | Adds `BR2_PACKAGE_EMBRA_WEB=y` and `BR2_PACKAGE_EMBRA_RUST_TOOLCHAIN=y` |

#### Defconfig differences (aarch64 vs x86_64)

Only these lines differ — everything else (packages, overlay, filesystem) is identical:

| Setting | x86_64 | aarch64 |
|---|---|---|
| Architecture | `BR2_x86_64=y` | `BR2_aarch64=y` |
| Kernel config method | `BR2_LINUX_KERNEL_USE_DEFCONFIG=y` | `BR2_LINUX_KERNEL_USE_ARCH_DEFAULT_CONFIG=y` |
| Kernel defconfig name | `BR2_LINUX_KERNEL_DEFCONFIG="x86_64"` | *(not needed — arch default is implicit)* |

> **Why `USE_ARCH_DEFAULT_CONFIG`?** Buildroot's `USE_DEFCONFIG` appends `_defconfig` to
> the value you provide. ARM64's generic kernel config is named `defconfig`, so
> `USE_DEFCONFIG="defconfig"` produces the invalid target `defconfig_defconfig`.
> `USE_ARCH_DEFAULT_CONFIG` uses the architecture's default directly.

#### QEMU differences

| Setting | x86_64 | aarch64 |
|---|---|---|
| Binary | `qemu-system-x86_64` | `qemu-system-aarch64` |
| Machine | (default, i440FX/q35) | `-machine virt` |
| Acceleration on macOS | HVF (guest is x86 → TCG fallback) | HVF (native — near-native speed) |
| CPU | `-cpu max` (TCG) | `-cpu host` (HVF) |
| Kernel image | `bzImage` | `Image` |
| Serial console | `console=ttyS0` | `console=ttyAMA0` |
| Default UI | embra-web console; `EMBRA_TUI=1` → serial TUI | identical |
| Port forwards | 50000 / 8443 / 3345 (web mode) | identical |

### Backup & Restore

The Ubuntu build uses `embraos-backup.sh` with `sudo` for loop-mounting disk image
partitions. macOS can't do this natively, so `embraos-backup-mac.sh` wraps the same
script inside a privileged Docker container. No `sudo` needed — Docker runs the
container as root.

#### Setup

`embraos-backup-mac.sh` ships committed in `scripts/` alongside the original
`embraos-backup.sh` (Ubuntu/Linux) — nothing to set up.

#### Usage

Same interface as the original — all commands work identically:

```bash
./scripts/embraos-backup-mac.sh backup --label pre-rebuild
./scripts/embraos-backup-mac.sh restore
./scripts/embraos-backup-mac.sh restore 2026-04-15_1430
./scripts/embraos-backup-mac.sh list
./scripts/embraos-backup-mac.sh verify
```

#### How it works

The wrapper spins up a privileged `ubuntu:24.04` container with two volume mounts:

| Container path | Host path | Purpose |
|---|---|---|
| `/work` | Project root | Disk image + scripts (read/write) |
| `/backups` | `~/embraOS_BACKUPS` | Backup storage (persists across runs) |

`--privileged` gives the container loop device access for `mount -o loop`. The container
installs `rsync`, `fdisk`, and `python3`, then runs the original `embraos-backup.sh`
unchanged. The QEMU-running check happens on the host side before Docker starts, since
the container can't see host processes. Backups made on Ubuntu and Mac are
interchangeable.

#### Workflow

```bash
# 1. Stop the VM (Ctrl-A X in QEMU console)

# 2. Backup
./scripts/embraos-backup-mac.sh backup --label pre-rebuild

# 3. Rebuild the image (Docker pass re-runs Step 3.5 — a no-op if already staged)
RUST_TARGET=aarch64-unknown-linux-musl ./scripts/create_initramfs.sh
docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"

# 4. Restore into the fresh image
./scripts/embraos-backup-mac.sh restore

# 5. Boot and verify
./scripts/run-qemu-aarch64.sh
# In TUI: /status, /sessions, memory_scan
```

### Troubleshooting — Step 0.5 / Step 3.5

#### `trunk: command not found` / wasm32 target missing

**Symptom:** Step 0.5 aborts with `ERROR: trunk not found`, or `trunk build` fails on a
missing `wasm32-unknown-unknown` target.

**Fix:**

```bash
rustup target add wasm32-unknown-unknown
cargo install trunk --locked
```

`trunk` and the wasm32 target are host-side only (wasm32 output is arch-agnostic — the
same on Apple Silicon and x86_64). Step 0.5 runs natively on macOS, not in Docker.

#### `ERROR: xz is required` / `sha256sum is required` / `curl is required`

**Symptom:** Step 3.5 aborts on a missing tool.

**Cause:** Only happens **off macOS** (on macOS Step 3.5 is deferred to the Docker pass
by design). The Docker `ubuntu:24.04` pass needs these.

**Fix:** Ensure the Docker `apt-get install` list includes `xz-utils curl` (already in
the Quick Start command; `sha256sum` ships in `coreutils` in the base image). On a bare
ARM64 Linux host: `apt-get install -y xz-utils curl coreutils`.

#### `sha256 verification failed for rust-1.94.1-aarch64-unknown-linux-musl.tar.xz`

**Cause:** Truncated/corrupt download, or a stale partial stage.

**Fix:** Remove the staged tree and re-run the Docker pass:

```bash
rm -rf vendor/rust-toolchain
```

The expected host-tarball hash is `244ef245…f21e`. If it persists, check network /
proxy interception of `static.rust-lang.org`.

#### `ERROR: buildroot-src has local changes; refusing to switch to 2026.02.1`

**Cause:** The first build after this guide's update switches Buildroot
`2024.02` → `2026.02.1`, and the safe-switch refuses to discard local changes in
`buildroot-src`.

**Fix:** Commit/stash the changes, or pin to the current checkout:

```bash
export BUILDROOT_VERSION=2024.02   # stay on the old pin
```

Note: the version switch wipes `buildroot-src/output/`, so the first `2026.02.1` build
is one long full rebuild.

#### embra-web shows a blank page

**Cause:** Step 0.5 didn't produce the frontend bundle.

**Fix:** Confirm `crates/embra-web-ui/dist` exists after Step 0.5; re-run
`./scripts/build-image-aarch64.sh --storage-engine rocksdb` (or
`(cd crates/embra-web-ui && trunk build --release)`), then rebuild the image so
`embra-web` re-embeds it. Or fall back to the serial TUI with `EMBRA_TUI=1`.

### Troubleshooting — Step 1 (Cross-Compilation)

#### `ToolNotFound: failed to find tool "aarch64-linux-musl-gcc"`

**Symptom:** `aws-lc-sys` build fails with repeated `ToolNotFound` referencing a path
like `/usr/local/Cellar/musl-cross/0.9.11/libexec/bin/aarch64-linux-musl-gcc`.

**Cause:** The path doesn't match the install location — wrong Homebrew prefix
(`/opt/homebrew` on Apple Silicon vs `/usr/local` on Intel) or wrong version number.

**Fix:** Find the real path:

```bash
ls /opt/homebrew/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc 2>/dev/null || \
ls /usr/local/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc
```

If the linker is found but the C compiler isn't located, set it explicitly:

```bash
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
```

#### `Could not find protoc`

**Symptom:** `embra-common` build fails with `Could not find protoc`.

**Cause:** `protoc` isn't installed. `embra-common` uses `prost-build` to compile
`.proto` files for gRPC at build time.

**Fix:** `brew install protobuf`, then re-run.

#### `fatal error: 'stddef.h' file not found` (bindgen)

**Symptom:** the workspace build fails during `zstd-sys` (or `rocksdb-sys`, both
pulled in by the in-tree `wardsondb` crate) with:

```
.../clang/21/include/stddef.h:39:15: fatal error: 'stddef.h' file not found
thread 'main' panicked at .../zstd-sys-.../build.rs:44:40:
Unable to generate bindings: ClangDiagnostic(...)
```

**Cause:** `bindgen` wraps `libclang`; on macOS clang finds Xcode's Command Line Tools
headers first, which include macOS-specific paths that don't exist cross-compiling.

**Fix:** Point bindgen at the musl-cross sysroot:

```bash
MUSL_CROSS_ROOT=$(dirname "$(dirname "$(which aarch64-linux-musl-gcc)")")
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_musl="--sysroot=${MUSL_CROSS_ROOT}/aarch64-linux-musl -I${MUSL_CROSS_ROOT}/aarch64-linux-musl/include"
```

The build script sets this automatically — only needed when running `cargo` directly.

#### `undefined reference to '_rjem_malloc'` / `'_rjem_sdallocx'` (jemalloc)

**Symptom:** Final link of `wardsondb` fails with dozens of undefined references:

```
wardsondb.cgu-NN.rcgu.o:(.text...): undefined reference to `_rjem_sdallocx'
wardsondb.cgu-NN.rcgu.o:(.text...): undefined reference to `_rjem_malloc'
collect2: error: ld returned 1 exit status
```

**Cause:** `tikv-jemalloc-sys` builds jemalloc via its own `./configure && make`.
Jemalloc's build uses `ar`/`ranlib`; on macOS those default to Apple's BSD versions,
which don't understand the ELF objects from `aarch64-linux-musl-gcc`. Configure and make
succeed, but `libjemalloc.a` is a **96-byte** empty archive — every jemalloc symbol is
undefined at link. The `AR_aarch64_unknown_linux_musl` cargo var is not forwarded to
jemalloc's `Makefile` (it reads plain `AR`/`RANLIB`).

**Fix:** Export plain `AR`/`RANLIB`, clear the empty archive, rebuild:

```bash
export AR=aarch64-linux-musl-ar
export RANLIB=aarch64-linux-musl-ranlib
export RANLIB_aarch64_unknown_linux_musl=aarch64-linux-musl-ranlib

rm -rf target/aarch64-unknown-linux-musl/release/build/tikv-jemalloc-sys-*
rm -rf target/aarch64-unknown-linux-musl/release/deps/libtikv_jemalloc*
rm -rf target/aarch64-unknown-linux-musl/release/deps/tikv_jemalloc*

cargo build --release --target aarch64-unknown-linux-musl
```

The build script handles all three exports automatically — only needed when running
`cargo` directly.

**Verification:** a healthy `libjemalloc.a` is ~30 MB:

```bash
ls -la target/aarch64-unknown-linux-musl/release/build/tikv-jemalloc-sys-*/out/build/lib/libjemalloc.a
```

If it's still 96 bytes, the env vars didn't take effect — run them one at a time in the
same shell.

> **zsh gotcha (paste).** If paste fails with `no matches found` or `command not
> found`, you missed the `setopt interactive_comments` callout at the top of
> [Quick Start](#quick-start) — that's the actual fix. (`setopt no_nomatch`
> suppresses the symptom but doesn't make `#` work as a comment.)

### Buildroot arch parameterization (how it works)

There is **no per-arch sed and nothing to revert** — the Buildroot external tree builds
either arch from a single committed copy:

- `buildroot/external.mk` defines
  `EMBRAOS_RUST_TARGET = $(if $(BR2_aarch64),aarch64-unknown-linux-musl,x86_64-unknown-linux-musl)`.
- Each package `.mk` `_SITE` uses `target/$(EMBRAOS_RUST_TARGET)/release`, so the binary
  path follows the architecture the defconfig selects (`embraos_aarch64_defconfig` sets
  `BR2_aarch64=y`; the x86_64 defconfig does not, so it resolves to the x86_64 triple).
- `embra-rust-toolchain.mk` is arch-agnostic (it copies `vendor/rust-toolchain`).
- The kernel filename (`bzImage` x86_64 / `Image` aarch64) is resolved by
  `buildroot/board/embraos/post_image.sh`, which detects the kernel Buildroot produced
  in `BINARIES_DIR` and renders `genimage.cfg.in` → `$(BUILD_DIR)/genimage.cfg`.

Confirm the parameterized form (no hardcoded triple should remain in the package `.mk`):

```bash
grep -rn 'EMBRAOS_RUST_TARGET\|unknown-linux-musl' buildroot/external.mk buildroot/package/*/*.mk
```

### Troubleshooting — Step 4 (Buildroot, in Docker)

#### `ar: … Cannot allocate memory` / `libc.a Error 1` (OOM building the toolchain)

**Symptom:** Buildroot dies while building its internal toolchain:

```
.../aarch64-buildroot-linux-musl/bin/ar: obj/src/search/hsearch.lo: Cannot allocate memory
make[1]: *** [Makefile:167: lib/libc.a] Error 1
make: *** [package/pkg-generic.mk:273: .../musl-1.2.6/.stamp_built] Error 2
```

**Cause:** ENOMEM — too many parallel compilers for the Docker VM's RAM, not a defect.
The script runs `make -j$(nproc)`; OrbStack defaults to a 4 GB VM with all host vCPUs
exposed, so an 8 GB Mac gets `-j8` in 4 GB. The `2024.02 → 2026.02.1` switch wipes
`output/` and forces a full from-scratch GCC + musl bootstrap (the peak-RAM phase), so a
build that previously succeeded incrementally now OOMs.

**Fix:** cap parallelism with `JOBS` (passed into the container with `-e`), and discard
the half-written toolchain dir so it rebuilds cleanly on resume:

```bash
docker run --rm -e JOBS=2 -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools && \
   (cd buildroot-src && make musl-dirclean) ; \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
```

Completed host packages are stamped and skipped, so the resume is shorter than the first
attempt. `-j2` is slow but completes unattended in 4 GB. Raising the VM instead
(`orb config set memory_mib 6144 && orb restart`; revert with `4096`) starves macOS on
an 8 GB Mac, so `JOBS` is the safer lever. `JOBS` defaults to all cores — only set it
when memory-constrained.

#### `git: command not found`

**Cause:** The Docker container lacks `git`; the build script clones Buildroot.

**Fix:** Ensure `git` is in the `apt-get install` list (already in the Quick Start
command).

#### `you should not run configure as root`

**Symptom:** Buildroot host-package `./configure` fails:

```
configure: error: you should not run configure as root
(set FORCE_UNSAFE_CONFIGURE=1 in environment to bypass this check)
```

**Cause:** Docker runs as root; GNU autoconf rejects root.

**Fix:** Prefix with `FORCE_UNSAFE_CONFIGURE=1` (already in the Quick Start command).

#### `Can't find default configuration "defconfig_defconfig"`

**Cause:** `BR2_LINUX_KERNEL_USE_DEFCONFIG` appends `_defconfig`; `DEFCONFIG="defconfig"`
becomes the invalid `defconfig_defconfig`.

**Fix:** `embraos_aarch64_defconfig` uses `BR2_LINUX_KERNEL_USE_ARCH_DEFAULT_CONFIG=y`
instead — confirm the copied defconfig has this and not `USE_DEFCONFIG`.

#### `ERROR: …/target/<triple>/release does not exist`

**Cause:** Buildroot's local-SITE rsync can't find the embraOS binaries for the
selected arch. With the parameterization this is **not** a missing-sed problem —
`EMBRAOS_RUST_TARGET` already resolves the path from `$(BR2_aarch64)`. It means the
cargo cross-build for that arch didn't run or left the dir empty: Steps 0.5–3 were
skipped, the wrong defconfig was used, or `target/<triple>/release/` has no binaries.

**Fix:** Run the macOS side first (`./scripts/build-image-aarch64.sh
--storage-engine <engine>`) so `target/aarch64-unknown-linux-musl/release/` is
populated, then the Docker `--buildroot-only` pass. Confirm `EMBRAOS_RUST_TARGET` is
present in `buildroot/external.mk` and the defconfig is `embraos_aarch64_defconfig`
(`BR2_aarch64=y`).

#### `stat(Image)` / `stat(bzImage)` failed, or `no kernel image found` (genimage)

**Symptom:** `post_image.sh` aborts with `ERROR: no kernel image (Image or bzImage)
found in …`, or genimage reports `stat(<kernel>) failed`.

**Cause:** No kernel in `BINARIES_DIR`. `post_image.sh` auto-detects `Image` (aarch64)
vs `bzImage` (x86_64) and renders `genimage.cfg.in`, so this means the **kernel build
itself** didn't produce an image — not a config edit you need to make.

**Fix:** Re-run Buildroot and inspect the log for the kernel build failure. No
`genimage.cfg` edit is needed or possible — it is generated from `genimage.cfg.in`.

#### `mkdosfs: not found`

**Symptom:** genimage fails formatting the VFAT boot partition:

```
/bin/sh: 1: mkdosfs: not found
ERROR: vfat(boot.vfat): failed to generate boot.vfat
```

**Cause:** `dosfstools` isn't installed in the Docker container.

**Fix:** Ensure `dosfstools` is in the `apt-get install` list (already in the Quick
Start command).

### Serial console (ttyAMA0) — no action needed

ARM64's `-machine virt` uses the PL011 UART (`/dev/ttyAMA0`), not 8250
(`/dev/ttyS0`); `run-qemu-aarch64.sh` sets `console=ttyAMA0` accordingly.

You will see `--device /dev/ttyS0` if you grep the source (`embrad`'s
`supervisor.rs` passes it to embra-console). **It is vestigial and harmless** —
embra-console's `terminal::run(_device)` ignores that argument and renders the TUI
to its inherited `stdout`, which embrad wires to the real console (`ttyAMA0` on
aarch64, `ttyS0` on x86_64). The serial TUI (`EMBRA_TUI=1`) therefore attaches
correctly on Apple Silicon with **no code change** — verified on aarch64.

### End-to-End Validation

This is the sequence used to verify the 2026-05-18 re-sync (see the status banner).
Re-run it as the regression checklist after any canonical-build bump:

1. **Boot — web console (default).** `./scripts/run-qemu-aarch64.sh`. Banner shows
   `UI mode: web console (default)` and `3345→3345 (HTTPS web)`. QEMU launches
   `qemu-system-aarch64 -machine virt -accel hvf -cpu host`, kernel `Image`,
   `console=ttyAMA0` — no kernel panic on the PL011 console.
2. **Config Wizard + Learning Mode** in the browser at
   `https://localhost:3345/embraOS` (accept the embraOS-CA cert) — name, LLM provider
   + credentials, timezone; full soul-formation conversation; soul sealed.
3. **REST gateway:** from the host, `curl http://localhost:8443/health` → healthy.
4. **Conversation session** — tool dispatch, memory writes to WardSONDB, session
   persistence; `/status` shows WardSONDB connected with populated collections.
5. **Guardian (embra-guardian-v1):** define a dynamic tool and invoke it — exercises
   the in-OS `/opt/rust` toolchain on aarch64 (Step 3.5).
6. **Serial-TUI path:** relaunch with `EMBRA_TUI=1 ./scripts/run-qemu-aarch64.sh`.
   Banner shows `UI mode: serial TUI (EMBRA_TUI=1)`; the TUI attaches on this
   terminal (verified on aarch64); `https://localhost:3345` does not respond.
7. **Reboot / soul verify:** `Ctrl-A X`, relaunch (web mode) — second-boot soul SHA-256
   verification passes; sessions/memory persisted on the DATA partition.

If all pass, the aarch64 image is functionally equivalent to x86_64. Re-stamp the status
banner at the top of this file with the new verified date and commit.
