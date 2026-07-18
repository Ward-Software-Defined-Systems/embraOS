# embraOS — Intel Mac Build Guide

> **Target:** QEMU-bootable x86_64 disk image running natively on Intel Mac via HVF —
> Apple's Hypervisor.framework accelerates the x86_64 guest at near-native speed.
>
> **Host:** macOS on Intel (any Mac with an `x86_64` CPU + macOS 10.10+).
>
> ✅ **Status — verified end-to-end on Intel Mac (2026-05-20).** Verified by
> William on Intel MacBook Pro 16" against Buildroot `2026.02.1`, storage
> engine `fjall`, at commit `bac1d08`: the full build pipeline (Step 0.5
> Trunk/WASM frontend → Rust cross-compile → initramfs → Step 3.5 in-OS
> Rust toolchain → Docker Buildroot pass), HVF-accelerated boot under
> `qemu-system-x86_64`, Config Wizard + soul-formation, the serial TUI
> (`EMBRA_TUI=1`), embra-guardian-v1 (a dynamic tool compiled in-guest via
> `/opt/rust` and run in the wasmtime sandbox), and reboot + second-boot
> soul SHA-256 verify. embra-guardian-v1 remains an experimental
> feature upstream (project-wide, not Intel-Mac-specific). Re-run the
> [End-to-End Validation](#end-to-end-validation) checklist after any
> canonical-build bump and re-stamp this date.
>
> **Why a separate guide:** `scripts/build-image.sh` is shaped around a Linux
> host (auto-detects musl.cc's `/opt/x86_64-linux-musl-cross`, expects `xz` and
> `sha256sum` on PATH). On Intel Mac you stage equivalent toolchain bits from
> Homebrew + GNU coreutils via a one-time env-var prelude. The Apple-Silicon flow
> (`build-image-aarch64.sh` + `docs/AARCH64-BUILD.md`) automates the same
> staging; Intel Mac uses the canonical script directly.

---

## Quick Start

Builds the x86_64 embraOS image that runs natively on Intel Mac via HVF. Steps
0.5–3.5 (frontend + Rust cross-compile + initramfs + in-OS Rust toolchain stage)
run on macOS; Step 4 (Buildroot) runs in a `linux/amd64` Docker container.

> **Default UI is the browser console.** `run-qemu.sh` boots the **embra-web**
> console by default, served over HTTPS at
> **https://localhost:3345/embraOS** (accept the embraOS-CA cert on first visit).
> Set **`EMBRA_TUI=1`** before `run-qemu.sh` to boot the serial TUI on this
> terminal instead — no image rebuild needed.

> **zsh paste gotcha:** macOS's default `zsh` doesn't treat `#` as a comment in
> interactive shells — pasting a multi-line code block from this guide makes
> any `#`-prefixed (or inline `… # comment`) line fail with `command not found`
> or `no matches found`. Run once per session (or add to `~/.zshrc`):
> ```bash
> setopt interactive_comments
> ```

### 1. Prerequisites (Intel Mac, x86_64)

```bash
# Rust via rustup — NOT Homebrew (brew uninstall rust first if present)
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup target add x86_64-unknown-linux-musl     # this build
rustup target add wasm32-unknown-unknown        # embra-web frontend (Step 0.5)
cargo install trunk --locked                    # Step 0.5 aborts if trunk is missing
```

```bash
# QEMU + cross-toolchain + protoc + cmake + GNU coreutils (Homebrew)
brew install qemu protobuf cmake xz coreutils
brew install filosottile/musl-cross/musl-cross

# Docker runtime — OrbStack recommended (faster/lighter than Docker Desktop);
# both default to linux/amd64 containers on Intel Mac, no platform flag needed
brew install orbstack
```

Add Homebrew's GNU coreutils to PATH so Step 3.5 finds `sha256sum`. macOS ships
`shasum -a 256`, not `sha256sum`; coreutils' `gnubin` directory aliases all GNU
tools under their native (non-`g`-prefixed) names:

```bash
export PATH="$(brew --prefix coreutils)/libexec/gnubin:$PATH"
# add the line to ~/.zshrc or ~/.bash_profile for persistence
```

### 2. Cross-build environment — one-time per shell

`build-image.sh`'s native x86_64 path expects musl.cc's
`/opt/x86_64-linux-musl-cross` (Linux convention). On Intel Mac you point the
same set of env vars at Homebrew's `musl-cross` keg. The aarch64 script
automates this for Apple Silicon; on Intel Mac you paste the block below into
your shell once per session before invoking `build-image.sh`:

```bash
# Auto-discover Homebrew's musl-cross (Intel Mac prefix: /usr/local/Cellar)
MUSL_CROSS_BIN="$(ls -d /usr/local/Cellar/musl-cross/*/libexec/bin | head -n1)"
MUSL_CROSS_ROOT="$(dirname "$MUSL_CROSS_BIN")"
MUSL_SYSROOT="$MUSL_CROSS_ROOT/x86_64-linux-musl"

# build-image.sh checks $MUSL_CROSS/bin/x86_64-linux-musl-gcc (default: Linux
# musl.cc layout /opt/x86_64-linux-musl-cross); Homebrew lays it out under
# libexec/, so point MUSL_CROSS at the libexec root so the sanity check resolves.
export MUSL_CROSS="$MUSL_CROSS_ROOT"

export PATH="$MUSL_CROSS_BIN:$PATH"
export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
export CXX_x86_64_unknown_linux_musl=x86_64-linux-musl-g++
export AR_x86_64_unknown_linux_musl=x86_64-linux-musl-ar
export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc

# Plain AR/RANLIB for deps whose build.rs invokes configure/make directly
# (e.g. tikv-jemalloc-sys). Without these, jemalloc's ./configure picks up
# macOS's BSD ar and produces a 96-byte empty libjemalloc.a — the wardsondb
# link later fails with `undefined reference to _rjem_malloc`.
export AR=x86_64-linux-musl-ar
export RANLIB=x86_64-linux-musl-ranlib
export RANLIB_x86_64_unknown_linux_musl=x86_64-linux-musl-ranlib

# Point bindgen/clang at the musl sysroot — required for zstd-sys + rocksdb-sys.
# Without this, clang picks up Xcode's headers and fails with
# `fatal error: 'stddef.h' file not found`.
export BINDGEN_EXTRA_CLANG_ARGS_x86_64_unknown_linux_musl="--sysroot=${MUSL_SYSROOT} -I${MUSL_SYSROOT}/include"
```

> **Don't simplify the glob to `$(brew --prefix musl-cross)`.** `brew --prefix`
> returns the versionless symlink root, where the toolchain binaries live in
> `bin/` — but the `x86_64-linux-musl/` sysroot only exists under the versioned
> `Cellar/` directory used here. Without the sysroot, bindgen-based crates
> (zstd-sys, rocksdb-sys) fail.

### 3. Clone & build the image

Clone embraOS and build. WardSONDB is vendored in-tree at `crates/wardsondb` —
no sibling repo is needed. The canonical build scripts ship committed in-tree.

```bash
# Clone into ~/projects (keeps source + build artifacts across reboots).
mkdir -p ~/projects && cd ~/projects
git clone https://github.com/Ward-Software-Defined-Systems/embraOS.git
cd ~/projects/embraOS

# Steps 0.5–3.5 on macOS: Trunk/WASM → Rust cross-compile → initramfs → in-OS
# Rust toolchain stage. Exits at the Step 4 guard printing the Docker command —
# that's expected on macOS.
./scripts/build-image.sh --storage-engine rocksdb     # or: fjall

# Step 4 (Buildroot) in Docker (linux/amd64 by default on Intel Mac)
docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools libelf-dev && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image.sh --buildroot-only"
```

### 4. Boot

```bash
./scripts/run-qemu.sh                  # web console (default) — https://localhost:3345/embraOS
EMBRA_TUI=1 ./scripts/run-qemu.sh      # stable Phase 1 serial TUI on this terminal
EMBRA_DB_VERBOSE=1 ./scripts/run-qemu.sh   # opt-in per-request WardSONDB log lines
```

Press `Ctrl-A X` to exit QEMU. On first boot the Config Wizard runs (name, LLM
provider + credentials, timezone), then Learning Mode forms and seals the soul.

> **Storage engine:** `--storage-engine rocksdb` (battle-tested) or `fjall` (pure
> Rust) is required and is baked into the `embrad` binary at build time.
> WardSONDB locks the choice into the DATA partition on first boot via a
> `.engine` marker — switching later requires wiping DATA. `--buildroot-only`
> ignores the flag (Rust is not rebuilt there). Back up before switching.

> **Buildroot version:** Defaults to `2026.02.1` (LTS, Ubuntu 26.04 era).
> Override with `BUILDROOT_VERSION=2024.02 ./scripts/build-image.sh ...`. The
> first build after switching versions wipes `buildroot-src/output/` (one long
> full rebuild). The switch is refused if you have local changes in
> `buildroot-src` — commit/stash them or pin `BUILDROOT_VERSION` to the current
> checkout.

> **Build memory / parallel jobs (`JOBS`):** the Docker pass runs
> `make -j$(nproc)`. Buildroot is RAM-hungry — on a memory-limited host
> (e.g. OrbStack's 4 GB default VM, `nproc`=8 → `-j8`) it OOMs in several
> flavors. The toolchain bootstrap dies with `libc.a Error 1` /
> `ar: … Cannot allocate memory`; regular package compiles fail with bare
> `Cannot allocate memory` while reading small headers (e.g. openssh's
> `sshd-auth.c` on `<sys/wait.h>`); the kernel hits the same on its own
> headers (`<uapi/linux/sem.h>`) or in `fixdep`. All have the same fix — cap
> parallelism with `JOBS` (passed into the container with `-e`):
> ```bash
> docker run --rm -e JOBS=2 -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
>   "apt-get update && apt-get install -y build-essential gcc g++ \
>    unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools libelf-dev && \
>    FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image.sh --buildroot-only"
> ```
> `JOBS` defaults to all cores (unchanged from canonical) — only set it when
> memory-constrained.

> **In-OS Rust toolchain (Step 3.5):** Downloads a SHA-256-verified
> `rust-1.94.1-x86_64-unknown-linux-musl` (host triple = guest arch, not
> build-host arch) + `rust-std-1.94.1-wasm32-unknown-unknown` into
> `vendor/rust-toolchain`, baked into the rootfs at `/opt/rust` for
> embra-guardian-v1. Needs `xz` + `sha256sum` on PATH — `brew install xz
> coreutils` + the gnubin PATH from step 1 satisfies both. First build needs
> network for this and adds ~100 MB to the image. Override the pin with
> `RUST_TOOLCHAIN_VERSION=...`.

> **Port forwarding:** QEMU forwards 50000 (gRPC) and 8443 (REST); in the
> default web-console mode it also forwards 3345 (HTTPS —
> https://localhost:3345/embraOS). Test:
> ```bash
> curl http://localhost:8443/health
> ```

> **Backup & restore:** macOS can't loop-mount the image natively, so
> `embraos-backup-mac.sh` runs the Linux backup script inside a privileged
> Docker container (arch-agnostic — same script works for x86_64 and aarch64
> images). Stop the VM first (`Ctrl-A X`).
> ```bash
> ./scripts/embraos-backup-mac.sh backup --label pre-rebuild
> ./scripts/embraos-backup-mac.sh restore
> ./scripts/embraos-backup-mac.sh list
> ```
> Backups live in `~/embraOS_BACKUPS/` (override `EMBRAOS_BACKUP_DIR`) and are
> interchangeable with Ubuntu and Apple-Silicon backups.

---

## End-to-End Validation

Run this sequence on the Intel Mac after the first successful build; re-run as
the regression checklist after any canonical-build bump, and re-stamp the status
banner at the top of this file.

1. **Boot — web console (default).** `./scripts/run-qemu.sh`. Banner shows
   `Acceleration: HVF (macOS)`, `UI mode: web console (default)`, and
   `3345→3345 (HTTPS web)`. No kernel panic on `ttyS0`.
2. **Config Wizard + Learning Mode** in the browser at
   `https://localhost:3345/embraOS` (accept the embraOS-CA cert) — name, LLM
   provider + credentials, timezone; full soul-formation conversation; soul
   sealed.
3. **REST gateway:** from the host, `curl http://localhost:8443/health` →
   healthy.
4. **Conversation session** — tool dispatch, memory writes to WardSONDB, session
   persistence; `/status` shows WardSONDB connected with populated collections.
5. **Guardian (embra-guardian-v1):** define a small dynamic tool via `/guardian`
   and invoke it — exercises the in-OS `/opt/rust` toolchain on the x86_64
   guest (Step 3.5 baked correctly).
6. **Serial-TUI path:** relaunch with `EMBRA_TUI=1 ./scripts/run-qemu.sh`.
   Banner shows `UI mode: serial TUI (EMBRA_TUI=1)`; the TUI attaches on this
   terminal; `https://localhost:3345` does not respond.
7. **Reboot / soul verify:** `Ctrl-A X`, relaunch (web mode) — second-boot soul
   SHA-256 verification passes; sessions/memory persisted on the DATA partition.

If all pass, the Intel Mac image is functionally equivalent to the canonical
Ubuntu build. Stamp the status banner at the top of this file with the date +
commit you validated at, mirroring the [aarch64 guide](AARCH64-BUILD.md)'s
wording.
