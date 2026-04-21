# embraOS — aarch64 Apple Silicon Build Guide

> **Target:** QEMU-bootable ARM64 disk image running natively on Apple Silicon via HVF
>
> **Host:** macOS on Apple Silicon (M1/M2/M3/M4)
>
> **Payoff:** Near-native QEMU performance via HVF — eliminates the 5–10x TCG software emulation penalty of running x86_64 guests on Apple Silicon
>
> **Status:** ✅ Verified end-to-end on MacBook Air M1 (2026-04-15) — builds clean, boots to TUI under HVF.

---

## Prerequisites

### Rust (via rustup — not Homebrew)

```bash
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
```

If Rust was previously installed via Homebrew, uninstall it first to avoid PATH conflicts:

```bash
brew uninstall rust
```

### Docker (OrbStack Recommended)

OrbStack is the recommended Docker runtime for macOS. It's used for both the Buildroot image build and the backup/restore workflow (both require Linux tools not available on macOS).

```bash
brew install orbstack
```

> Docker Desktop also works, but OrbStack is faster and lighter on Apple Silicon. On Apple Silicon, both default to `linux/arm64` containers — no Rosetta overhead.

### QEMU and Cross-Compilation Toolchain

```bash
brew install qemu
brew install filosottile/musl-cross/musl-cross
brew install protobuf    # Required — protoc compiles .proto files for gRPC (embra-common)
brew install cmake        # Required — aws-lc-sys build in WardSONDB uses cmake
```

### Rust Targets

```bash
rustup target add aarch64-unknown-linux-musl    # ARM64 (this build)
rustup target add x86_64-unknown-linux-musl     # x86_64 (existing build)
```

### Musl Cross-Linker Configuration

The `build-image-aarch64.sh` script auto-detects the `musl-cross` path and sets `PATH`, `CC`, `CXX`, `AR`, and the cargo linker env vars. **No manual `~/.cargo/config.toml` setup is required** for the standard build flow.

If you're running `cargo` directly (outside the build script, e.g., for isolated dependency validation), you'll need to set the env vars yourself. The Step 1 section below shows the exact commands.

Homebrew installs `musl-cross` in different locations depending on the Mac:

| Mac | Homebrew prefix |
|---|---|
| Apple Silicon (M1/M2/M3/M4) | `/opt/homebrew/Cellar/musl-cross/` |
| Intel | `/usr/local/Cellar/musl-cross/` |

Verify the toolchain is installed:

```bash
# Apple Silicon (most likely)
ls /opt/homebrew/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc

# Intel Mac (fallback)
ls /usr/local/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc
```

> **Legacy note:** Earlier versions of this guide had you set the linker path in `~/.cargo/config.toml`. That still works but is no longer required — the build script overrides it via env vars. If you have an old entry there, it's harmless.

---

## Build Steps

### Storage Engine Selection

embraOS builds require a `--storage-engine` flag when compiling Rust binaries. This selects which WardSONDB backend gets baked into `embrad`:

| Engine | Description |
|---|---|
| `fjall` | Pure-Rust LSM-tree (original backend) |
| `rocksdb` | RocksDB backend (requires C++ toolchain) |

> **⚠️ The engine choice is locked into the DATA partition on first boot** via WardSONDB's `.engine` marker file. Switching engines later requires wiping DATA. Back up before switching (see Backup & Restore).

The flag only matters for the Rust compilation stage. `--buildroot-only` ignores it — the engine is already baked into the binary from the earlier Rust build.

### Step 1 — Rust Cross-Compilation Validation

The recommended path is to run Steps 1-3 via `build-image-aarch64.sh` (shown in Step 2 below) — it handles the musl-cross path auto-detection, sets `CC/CXX/AR/linker` env vars for C++ support (needed by the RocksDB backend), and bakes the storage engine choice into the `embrad` binary.

For isolated validation (before wiring into the full build), you can run cargo directly. ~30 minutes, mostly waiting on compilation.

```bash
# Set up cross-compilation env vars (the build script does this automatically)
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
export EMBRA_STORAGE_ENGINE=fjall   # or rocksdb

# Build embraOS workspace
cd embraOS
cargo build --release --target aarch64-unknown-linux-musl --workspace

# Build WardSONDB (separate repo)
cd ../WardSONDB
cargo build --release --target aarch64-unknown-linux-musl

# Copy WardSONDB binary into embraOS target directory
# (Buildroot package and build-image-aarch64.sh expect it here)
cp target/aarch64-unknown-linux-musl/release/wardsondb \
   ../embraOS/target/aarch64-unknown-linux-musl/release/wardsondb

cd ../embraOS
```

> **Note on bindgen-based crates:** WardSONDB pulls in `zstd-sys` (and optionally `rocksdb-sys` with the RocksDB backend), both of which use `bindgen` to parse C headers. Without `BINDGEN_EXTRA_CLANG_ARGS`, clang finds Xcode's SDK headers and can't resolve cross-compile references. The env var above points bindgen at the musl sysroot instead.

> **Note on WardSONDB's `aws-lc-sys` dependency:** `aws-lc-sys` (Amazon's crypto library) has a C/assembly build step. If you see `ToolNotFound` errors mentioning `aarch64-linux-musl-gcc` during C file compilation, either the musl-cross path isn't in `PATH` or the `CC_aarch64_unknown_linux_musl` env var isn't set. The build script handles both automatically.

If both compile clean, proceed to Step 2.

### Step 2 — Initramfs + Buildroot Image

Place the aarch64 defconfig alongside the existing x86_64 one, update the initramfs script, then build.

#### File Placement

```
buildroot/
  configs/
    embraos_x86_64_defconfig    ← existing (unchanged)
    embraos_aarch64_defconfig   ← new (add alongside)

scripts/
    build-image-aarch64.sh      ← new
    run-qemu-aarch64.sh         ← new
    create_initramfs.sh         ← replace (backward-compatible, defaults to x86_64)
```

#### Build

```bash
# Copy new files into place
cp embraos_aarch64_defconfig buildroot/configs/
cp build-image-aarch64.sh scripts/
cp run-qemu-aarch64.sh scripts/
cp create_initramfs.sh scripts/
chmod +x scripts/build-image-aarch64.sh scripts/run-qemu-aarch64.sh scripts/create_initramfs.sh

# Run Steps 1-3 (Rust cross-compilation + initramfs) natively on macOS
# The build script handles musl-cross path detection, CC/CXX/AR env vars,
# and bakes the storage engine into the embrad binary.
./scripts/build-image-aarch64.sh --storage-engine fjall

# Update Buildroot package .mk files to use aarch64 target path
# (these default to x86_64-unknown-linux-musl from the original build)
find buildroot/package -name '*.mk' -exec \
  sed -i '' 's|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|g' {} +

# Update genimage.cfg — ARM64 kernel is "Image", not "bzImage"
sed -i '' 's/"bzImage"/"Image"/' buildroot/board/embraos/genimage.cfg

# Buildroot requires Linux — run in Docker (OrbStack or Docker Desktop)
# (Docker defaults to linux/arm64 on Apple Silicon — no Rosetta overhead)
# No --storage-engine flag needed inside Docker — Rust is not rebuilt there.
docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget python3 file git dosfstools && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
```

> **Swap `fjall` for `rocksdb`** in the first command to use the RocksDB backend. The rest of the commands are identical.

The `--buildroot-only` flag skips Steps 1–3 (Rust compilation + initramfs) inside Docker since you already ran them natively on macOS — it only runs the Buildroot stage, which picks up the pre-built binaries and initramfs from the working tree.

A few hours, mostly Buildroot compiling the kernel and packages.

### Step 3 — QEMU Boot

```bash
./scripts/run-qemu-aarch64.sh
```

This boots with HVF hardware acceleration (`-cpu host`, `-accel hvf`) on the ARM64 `virt` machine. Press `Ctrl-A X` to exit QEMU.

#### What to Watch For on First Boot

| Symptom | Likely Cause | Fix |
|---|---|---|
| Kernel panic on missing drivers | ARM64 generic defconfig missing a virtio option | Add the option to `board/embraos/linux.fragment` |
| embra-console not attaching | Hardcoded `/dev/ttyS0` in source | Grep for `ttyS0` in embrad and embra-console — change to `/dev/ttyAMA0` or `/dev/console` |
| WardSONDB not starting | Service spawn or port binding issue | Check embrad logs — listen address should be `0.0.0.0:8090` regardless of arch |
| Partitions not found | Device path mismatch | Both x86_64 and aarch64 use `if=virtio` → `/dev/vda*` — verify `embra-init` isn't hardcoding paths |
| Boot hangs after kernel messages | initramfs not loading or `/init` not found | Confirm `initramfs.cpio.gz` was built with aarch64 binaries (Step 2) |

#### Serial Console Device Note

ARM64's `-machine virt` uses PL011 UART (`/dev/ttyAMA0`), not 8250 (`/dev/ttyS0`). The kernel cmdline in `run-qemu-aarch64.sh` already specifies `console=ttyAMA0`. However, if embrad spawns embra-console with a hardcoded `--device /dev/ttyS0` argument, that device won't exist. Grep the source:

```bash
grep -r "ttyS0" crates/
```

The cleanest fix options (in order of preference):

1. Have embra-console default to `/dev/console` (kernel maps this to whatever `console=` specifies)
2. Have embrad read the console device from the kernel cmdline (you already parse `embra.cols` / `embra.rows`)
3. Hardcode `/dev/ttyAMA0` in the aarch64 build (least portable)

### Step 4 — End-to-End Validation

Once the boot lands in the TUI, run through the same validation sequence as x86_64:

1. **Config Wizard** — name, API key, timezone
2. **Learning Mode** — full soul formation conversation
3. **Reboot** — `Ctrl-A X`, then relaunch with `./scripts/run-qemu-aarch64.sh` — soul SHA-256 verification on second boot
4. **Conversation session** — tool dispatch, memory writes to WardSONDB, session persistence
5. **REST gateway** — from the host: `curl http://localhost:8443/health`
6. **WardSONDB health** — use `/status` in the TUI to confirm WardSONDB is connected and collections are populated

If all six pass, the aarch64 image is functionally equivalent to x86_64.

---

## Backup & Restore

The Ubuntu build uses `embraos-backup.sh` with `sudo` for loop-mounting disk image partitions. macOS can't do this natively, so `embraos-backup-mac.sh` wraps the same script inside a privileged Docker container. No `sudo` needed — Docker runs the container as root.

### Setup

Drop the wrapper into `scripts/` alongside the original:

```bash
cp embraos-backup-mac.sh scripts/
chmod +x scripts/embraos-backup-mac.sh
```

```
scripts/
    embraos-backup.sh         ← original (Ubuntu/Linux)
    embraos-backup-mac.sh     ← macOS wrapper (uses Docker)
```

### Usage

Same interface as the original — all commands work identically:

```bash
# Backup before rebuilding the image
./scripts/embraos-backup-mac.sh backup --label pre-rebuild

# Restore after rebuilding
./scripts/embraos-backup-mac.sh restore

# Restore a specific backup
./scripts/embraos-backup-mac.sh restore 2026-04-15_1430

# List available backups
./scripts/embraos-backup-mac.sh list

# Verify disk image has valid data
./scripts/embraos-backup-mac.sh verify
```

### How It Works

The wrapper spins up a privileged `ubuntu:24.04` container with two volume mounts:

| Container path | Host path | Purpose |
|---|---|---|
| `/work` | Project root | Disk image + scripts (read/write) |
| `/backups` | `~/embraOS_BACKUPS` | Backup storage (persists across runs) |

`--privileged` gives the container loop device access for `mount -o loop`. The container installs `rsync`, `fdisk`, and `python3`, then runs the original `embraos-backup.sh` unchanged. The QEMU-running check happens on the host side before Docker starts, since the container can't see host processes.

Backups are stored at `~/embraOS_BACKUPS` on the Mac (override with `EMBRAOS_BACKUP_DIR`). Backups made on Ubuntu and Mac are interchangeable.

### Workflow

```bash
# 1. Stop the VM (Ctrl-A X in QEMU console)

# 2. Backup
./scripts/embraos-backup-mac.sh backup --label pre-rebuild

# 3. Rebuild the image
RUST_TARGET=aarch64-unknown-linux-musl ./scripts/create_initramfs.sh
docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget python3 file git dosfstools && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"

# 4. Restore into the fresh image
./scripts/embraos-backup-mac.sh restore

# 5. Boot and verify
./scripts/run-qemu-aarch64.sh
# In TUI: /status, /sessions, memory_scan
```

---

## What Changed from x86_64

### New Files (add alongside existing)

| File | Purpose |
|---|---|
| `buildroot/configs/embraos_aarch64_defconfig` | ARM64 Buildroot configuration |
| `scripts/build-image-aarch64.sh` | Full build pipeline for aarch64 |
| `scripts/run-qemu-aarch64.sh` | QEMU launch with HVF acceleration |
| `scripts/embraos-backup-mac.sh` | macOS wrapper for backup/restore via Docker |

### Modified Files (backward-compatible)

| File | Change |
|---|---|
| `scripts/create_initramfs.sh` | Respects `RUST_TARGET` env var; defaults to `x86_64-unknown-linux-musl` when unset |
| `buildroot/board/embraos/genimage.cfg` | `bzImage` → `Image` (ARM64 kernel binary name) |
| `buildroot/package/*/*.mk` | `x86_64-unknown-linux-musl` → `aarch64-unknown-linux-musl` (Rust target path) |

### Defconfig Differences (aarch64 vs x86_64)

Three lines differ:

| Setting | x86_64 | aarch64 |
|---|---|---|
| Architecture | `BR2_x86_64=y` | `BR2_aarch64=y` |
| Kernel config method | `BR2_LINUX_KERNEL_USE_DEFCONFIG=y` | `BR2_LINUX_KERNEL_USE_ARCH_DEFAULT_CONFIG=y` |
| Kernel defconfig name | `BR2_LINUX_KERNEL_DEFCONFIG="x86_64"` | *(not needed — arch default is implicit)* |

> **Why `USE_ARCH_DEFAULT_CONFIG`?** Buildroot's `USE_DEFCONFIG` appends `_defconfig` to the value you provide. ARM64's generic kernel config is named `defconfig` (not `something_defconfig`), so `USE_DEFCONFIG="defconfig"` produces the invalid target `defconfig_defconfig`. `USE_ARCH_DEFAULT_CONFIG` uses the architecture's default directly.

### QEMU Differences

| Setting | x86_64 | aarch64 |
|---|---|---|
| Binary | `qemu-system-x86_64` | `qemu-system-aarch64` |
| Machine | (default, i440FX/q35) | `-machine virt` |
| Acceleration on macOS | HVF (but guest is x86 → TCG fallback) | HVF (native — near-native speed) |
| CPU | `-cpu max` (TCG) | `-cpu host` (HVF) |
| Kernel image | `bzImage` | `Image` |
| Serial console | `console=ttyS0` | `console=ttyAMA0` |

---

## Troubleshooting — Step 1 (Cross-Compilation)

Issues encountered during the initial aarch64 cross-compilation on Apple Silicon.

### `ToolNotFound: failed to find tool "aarch64-linux-musl-gcc"`

**Symptom:** `aws-lc-sys` build fails with repeated `ToolNotFound` errors referencing a path like `/usr/local/Cellar/musl-cross/0.9.11/libexec/bin/aarch64-linux-musl-gcc`.

**Cause:** The linker path in `~/.cargo/config.toml` doesn't match the actual install location. Two common reasons:

1. **Wrong Homebrew prefix** — Apple Silicon uses `/opt/homebrew`, Intel uses `/usr/local`
2. **Wrong version number** — `musl-cross` was installed at a different version than `0.9.11`

**Fix:** Find the real path and update `~/.cargo/config.toml`:

```bash
# Find the actual binary
ls /opt/homebrew/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc 2>/dev/null || \
ls /usr/local/Cellar/musl-cross/*/libexec/bin/aarch64-linux-musl-gcc

# Update ~/.cargo/config.toml with the correct path
```

If the linker is found but `aws-lc-sys` still can't locate the C compiler, set it explicitly:

```bash
export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
```

### `Could not find protoc`

**Symptom:** `embra-common` build fails with `Could not find protoc` error.

**Cause:** `protoc` (the Protocol Buffers compiler) is not installed. `embra-common` uses `prost-build` to compile `.proto` files for gRPC at build time.

**Fix:**

```bash
brew install protobuf
```

Then re-run the build. This is listed in the Prerequisites section but easy to miss on a fresh machine.

### `fatal error: 'stddef.h' file not found` (bindgen)

**Symptom:** WardSONDB build fails during `zstd-sys` (or `rocksdb-sys` with the RocksDB backend) with:

```
/Library/Developer/CommandLineTools/usr/lib/clang/21/include/stddef.h:39:15:
fatal error: 'stddef.h' file not found

thread 'main' panicked at .../zstd-sys-.../build.rs:44:40:
Unable to generate bindings: ClangDiagnostic(...)
```

**Cause:** `bindgen` (used by `zstd-sys`, `rocksdb-sys`, and similar crates) wraps `libclang`. On macOS, clang locates Xcode's Command Line Tools headers first, but those headers include macOS-specific paths that don't exist in a cross-compile context. The result is a header resolution failure.

**Fix:** Point bindgen at the musl-cross sysroot via `BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_musl`:

```bash
# Auto-detect the musl sysroot
MUSL_CROSS_ROOT=$(dirname "$(dirname "$(which aarch64-linux-musl-gcc)")")
export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_musl="--sysroot=${MUSL_CROSS_ROOT}/aarch64-linux-musl -I${MUSL_CROSS_ROOT}/aarch64-linux-musl/include"
```

Then re-run the build. The build script `build-image-aarch64.sh` sets this automatically — this fix is only needed when running `cargo` directly.

### `undefined reference to '_rjem_malloc'` / `'_rjem_sdallocx'` (jemalloc)

**Symptom:** Final link of `wardsondb` binary fails with dozens of undefined references:

```
wardsondb.cgu-NN.rcgu.o:(.text...): undefined reference to `_rjem_sdallocx'
wardsondb.cgu-NN.rcgu.o:(.text...): undefined reference to `_rjem_malloc'
more undefined references to `_rjem_sdallocx' follow
collect2: error: ld returned 1 exit status
```

**Cause:** `tikv-jemalloc-sys` builds jemalloc by invoking its `./configure && make` directly. Jemalloc's build system uses `ar` and `ranlib` — and on macOS those default to Apple's BSD versions, which don't understand the ELF object files produced by `aarch64-linux-musl-gcc`. Configure succeeds, make runs without errors, but the resulting `libjemalloc.a` is **96 bytes** — an empty archive header with no objects inside. At the final link stage, every jemalloc symbol comes up undefined.

The `AR_aarch64_unknown_linux_musl` env var that cargo's `cc` crate uses is not forwarded to jemalloc's `Makefile` — it reads plain `AR` and `RANLIB`.

**Fix:** Export plain `AR` and `RANLIB` as well:

```bash
export AR=aarch64-linux-musl-ar
export RANLIB=aarch64-linux-musl-ranlib
export RANLIB_aarch64_unknown_linux_musl=aarch64-linux-musl-ranlib

# Clean the empty archive so cargo rebuilds jemalloc
rm -rf target/aarch64-unknown-linux-musl/release/build/tikv-jemalloc-sys-*
rm -rf target/aarch64-unknown-linux-musl/release/deps/libtikv_jemalloc*
rm -rf target/aarch64-unknown-linux-musl/release/deps/tikv_jemalloc*

# Rebuild
cargo build --release --target aarch64-unknown-linux-musl
```

The build script handles all three exports automatically — this fix is only needed when running `cargo` directly.

**Verification:** after rebuild, a healthy `libjemalloc.a` is ~30 MB:

```bash
ls -la target/aarch64-unknown-linux-musl/release/build/tikv-jemalloc-sys-*/out/build/lib/libjemalloc.a
```

If it's still 96 bytes, the env vars didn't take effect — run them one at a time in the same shell.

> **zsh gotcha:** `zsh` treats unescaped `?` in comment lines as a glob pattern and throws `no matches found` errors. If you hit that while pasting multi-line command blocks, run `setopt no_nomatch` once per session.

---

## Buildroot Package Note

The Buildroot `.mk` files for embraOS packages hardcode `x86_64-unknown-linux-musl` as the Rust target path. For aarch64 builds, these must be updated before running Buildroot. The build steps above include a `sed` command that handles this automatically.

To verify or fix manually:

```bash
# Check for hardcoded x86_64 paths
grep -rn "x86_64-unknown-linux-musl" buildroot/package/

# Fix all at once
find buildroot/package -name '*.mk' -exec \
  sed -i '' 's|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|g' {} +
```

> **⚠️ This is a brute-force fix that makes the `.mk` files aarch64-only.** If you switch back to building x86_64, reverse the change. The proper long-term fix is parameterizing with a variable in each `.mk` file:
> ```makefile
> EMBRAOS_RUST_TARGET = $(if $(BR2_aarch64),aarch64-unknown-linux-musl,x86_64-unknown-linux-musl)
> ```

---

## Troubleshooting — Step 2 (Buildroot)

Issues encountered during the Buildroot image build in Docker.

### `git: command not found`

**Symptom:** `build-image-aarch64.sh` fails immediately at Step 4 with `git: command not found`.

**Cause:** The Docker container doesn't have `git` installed. The build script clones the Buildroot repository if it doesn't already exist.

**Fix:** Add `git` to the `apt-get install` list in the Docker command. (Already included in the build steps above.)

### `you should not run configure as root`

**Symptom:** Buildroot's host package builds (tar, cpio, etc.) fail during `./configure` with:

```
configure: error: you should not run configure as root
(set FORCE_UNSAFE_CONFIGURE=1 in environment to bypass this check)
```

**Cause:** Docker runs as root by default. GNU autoconf's configure scripts reject root execution as a safety check.

**Fix:** Prefix the build command with `FORCE_UNSAFE_CONFIGURE=1`. This is standard for Buildroot-in-Docker workflows. (Already included in the build steps above.)

### `Can't find default configuration "defconfig_defconfig"`

**Symptom:** Kernel build fails with:

```
Can't find default configuration "arch/arm64/configs/defconfig_defconfig"!
```

**Cause:** Buildroot's `BR2_LINUX_KERNEL_USE_DEFCONFIG` automatically appends `_defconfig` to the value. Setting `DEFCONFIG="defconfig"` produces the invalid name `defconfig_defconfig`.

**Fix:** In `embraos_aarch64_defconfig`, replace:

```
BR2_LINUX_KERNEL_USE_DEFCONFIG=y
BR2_LINUX_KERNEL_DEFCONFIG="defconfig"
```

With:

```
BR2_LINUX_KERNEL_USE_ARCH_DEFAULT_CONFIG=y
```

### `ERROR: /work/buildroot/../target/x86_64-unknown-linux-musl/release does not exist`

**Symptom:** Buildroot package sync fails because it's looking for binaries in the x86_64 target directory.

**Cause:** The Buildroot package `.mk` files for embraOS crates hardcode `x86_64-unknown-linux-musl` as the Rust target path. The aarch64 binaries are in `target/aarch64-unknown-linux-musl/release/` instead.

**Fix:** Update all `.mk` files before running Buildroot:

```bash
find buildroot/package -name '*.mk' -exec \
  sed -i '' 's|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|g' {} +
```

This is included in the build steps above. See the [Buildroot Package Note](#buildroot-package-note) for the long-term parameterization approach.

### `stat(bzImage) failed: No such file or directory`

**Symptom:** Buildroot completes the kernel build and package installs, then fails during `post_image.sh` / genimage:

```
ERROR: file(bzImage): stat(.../bzImage) failed: No such file or directory
ERROR: vfat(boot.vfat): could not setup bzImage
```

**Cause:** `buildroot/board/embraos/genimage.cfg` references `bzImage` (the x86_64 kernel binary name). ARM64 produces `Image` instead.

**Fix:**

```bash
sed -i '' 's/"bzImage"/"Image"/' buildroot/board/embraos/genimage.cfg
```

This is included in the build steps above.

> **⚠️ Like the `.mk` fix, this makes `genimage.cfg` aarch64-only.** Reverse it if switching back to x86_64. The long-term fix is a parameterized genimage config or separate configs per architecture.

### `mkdosfs: not found`

**Symptom:** Buildroot completes kernel and rootfs builds, then genimage fails while creating the FAT boot partition:

```
INFO: vfat(boot.vfat): cmd: "mkdosfs   '.../boot.vfat'" (stderr):
/bin/sh: 1: mkdosfs: not found
ERROR: vfat(boot.vfat): failed to generate boot.vfat
```

**Cause:** `mkdosfs` (part of `dosfstools`) isn't installed in the Docker container. genimage uses it to format the VFAT boot partition.

**Fix:** Add `dosfstools` to the `apt-get install` list in the Docker command. (Already included in the build steps above.)
