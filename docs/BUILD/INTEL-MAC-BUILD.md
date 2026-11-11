# embraOS — Intel Mac Build Guide

> **Target:** QEMU-bootable x86_64 disk image running natively on Intel Mac via HVF —
> Apple's Hypervisor.framework accelerates the x86_64 guest at near-native speed.
>
> **Host:** macOS on Intel (a Mac with an `x86_64` CPU and a macOS that Homebrew supports).
>
> ✅ **Status — re-verified end-to-end on Intel Mac (2026-07-19).** Verified by
> William on Intel MacBook Pro 16" against Buildroot `2026.02.1` under the
> **named-volume Docker Buildroot flow** (note below): the full build pipeline (Step 0.5
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
> **2026-11-07 — the Phase 2 image on the Intel Mac.** Verified by William on the
> Intel MacBook Pro 16" at `5be9bdf` (`--storage-engine rocksdb`): the full build
> (Step 0.5 through the Docker Buildroot pass, with Rust 1.98.1, Step 3.6's
> embedding model and the hardened Buildroot hooks, all TBD here since the
> 2026-07-19 run), the UEFI boot under HVF (Homebrew's `edk2-x86_64-code.fd`,
> GRUB's menu, slot A), the DHCP lease, the display window with GRUB, the boot log
> and the console URL, and a bridged NIC through `vmnet-bridged` with the leased
> URL (the note under [Boot](#4-boot)).
> The Config Wizard, the soul and the steps after them in the
> [End-to-End Validation](#end-to-end-validation) were not re-run on this date.
>
> **2026-11-10 — the display console on the Intel Mac.** Verified by William on
> the Intel MacBook Pro 16" under QEMU/HVF with a virtio-gpu display and a USB
> tablet on xHCI (the command under [Boot](#4-boot)): after the boot log the window
> shows the web console, which takes the keyboard, the pointer and a click. An
> image of `22022ff` came up at 1280x800 there; one of `1ba8df5` comes up at
> 1920x1080, by default and with `embra.kioskmode=1920x1080` in the `-smbios`
> value. QEMU's window reports its size to the guest a second after every resize,
> the kernel's own console then keeps only the display modes its 1280x800
> framebuffer holds, and embrad found no 1920x1080 in that list; from `1ba8df5` it
> asks the kernel to detect the displays again before it gives up on a mode.
>
> **2026-11-11 — the launcher under macOS's bash.** macOS runs
> `scripts/run-qemu.sh` with its own `/bin/bash`, 3.2, and that shell stops at an
> empty array under `set -u`. The launcher of `3bfd40f` through `79821a4` expands
> one on every launch without `EMBRA_ISO` (`MEDIA_ARGS[@]: unbound variable`,
> before QEMU starts; reproduced with bash 3.2.57 on the Linux build host);
> `0db28a8` expands it in a form that shell accepts. Verified by William on the
> Intel Mac on 2026-11-11: `./scripts/run-qemu.sh` works there.
>
> **2026-11-11 — Parallels Desktop on the Intel Mac.** Parallels Desktop 26.5
> takes the build two ways, both run on this Mac on 2026-11-11
> ([QUICK-START.md](QUICK-START.md), the Parallels note): the machine in
> `output/images/parallels/` (`embraos.vmx` with its VMDK) converts and boots to
> the web console, its display console at 1920x1080, and `embraos.iso` installs
> on a machine set to UEFI with Secure Boot off. William ran the install and the
> import through Parallels' window. The pointer and a click on the display
> console there are **TBD**.
>
> **2026-07-19 — named-volume Buildroot flow.** The Docker pass now mounts a
> named volume over `buildroot-src`: bind-mount Buildroot trees exhaust the
> Docker runtime's macOS-side file-provider fd pool (`Too many open files` —
> observed and volume-fix-verified on the Apple-Silicon hosts; same mechanism
> on Intel). Re-verified on Intel + re-stamped 2026-07-19 (banner above).
>
> **Why a separate guide:** `scripts/build-image.sh` is shaped around a Linux
> host (auto-detects musl.cc's `/opt/x86_64-linux-musl-cross`, expects `xz` and
> `sha256sum` on PATH). On Intel Mac you stage equivalent toolchain bits from
> Homebrew + GNU coreutils via a one-time env-var prelude. The Apple-Silicon flow
> (`build-image-aarch64.sh` + `docs/BUILD/AARCH64-BUILD.md`) automates the same
> staging; Intel Mac uses the canonical script directly.

---

## Quick Start

Builds the x86_64 embraOS image that runs natively on Intel Mac via HVF. Steps
0.5–3.6 (frontend + Rust cross-compile + initramfs + the in-OS Rust toolchain and
embedding-model stages) run on macOS; Step 4 (Buildroot) runs in a `linux/amd64`
Docker container.

> **Default UI is the browser console.** `run-qemu.sh` boots the **embra-web**
> console by default, served over HTTPS at
> **https://localhost:3345/embraOS** — open it from the URL embraOS prints at boot,
> which carries this boot's access token (`?token=…`; accept the embraOS-CA cert
> on first visit; the token changes at every boot and is shown nowhere else).
> Set **`EMBRA_TUI=1`** before `run-qemu.sh` to boot the serial TUI on this
> terminal instead — no image rebuild needed. The serial TUI is functional but is
> preferred for debugging; the web console is the primary interface. The disk
> boots itself: the edk2 firmware Homebrew's qemu ships (`edk2-x86_64-code.fd`),
> then GRUB's menu on the EFI System Partition (`EMBRA_BOOT=uefi`, the default;
> validated on an Intel Mac 2026-11-07). `EMBRA_BOOT=direct` boots the
> kernel and the initramfs from the host as before, no firmware and no menu.

> **zsh paste gotcha:** macOS's default `zsh` doesn't treat `#` as a comment in
> interactive shells — pasting a multi-line code block from this guide makes
> any `#`-prefixed (or inline `… # comment`) line fail with `command not found`
> or `no matches found`. Run once per session (or add to `~/.zshrc`):
> ```bash
> setopt interactive_comments
> ```

### 1. Prerequisites (Intel Mac, x86_64)

```bash
# Rust via rustup — NOT Homebrew (brew uninstall rust first if present).
# The checkout pins its compiler in rust-toolchain.toml; rustup installs
# that version on the first cargo run inside the repository.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup target add x86_64-unknown-linux-musl     # this build
rustup target add wasm32-unknown-unknown        # embra-web frontend (Step 0.5)
cargo install trunk --locked                    # Step 0.5 aborts if trunk is missing
which -a cargo                                  # only ~/.cargo/bin/cargo: see the note below
```

> **A Homebrew Rust ahead of rustup breaks Step 0.5.** Homebrew installs
> `/usr/local/bin/cargo`, which on an Intel Mac comes before `~/.cargo/bin` on
> PATH. `rustup target add wasm32-unknown-unknown` then reports the target
> `up to date` (it looks at the pinned 1.98.1 toolchain), and Trunk's `cargo`
> fails a moment later with `can't find crate for core` and
> `the wasm32-unknown-unknown target may not be installed` (Homebrew's Rust
> ships only the host's standard library; Step 1's musl target is missing from
> it as well). `which -a cargo` listing `/usr/local/bin/cargo` first is the
> tell; `brew uninstall rust` (or `~/.cargo/bin` first on PATH) is the fix.

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

# Step 4 (Buildroot) in Docker (linux/amd64 by default on Intel Mac). The
# Buildroot tree lives in the named volume embraos-br-x86_64 — the Docker VM's
# native filesystem. Building it on the bind mount instead exhausts the macOS
# file provider's fd pool ("Too many open files") — see the note below.
docker run --rm -v "$PWD":/work -v embraos-br-x86_64:/work/buildroot-src \
  -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools qemu-utils libelf-dev && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image.sh --buildroot-only"
```

### 4. Boot

```bash
./scripts/run-qemu.sh                  # web console (default) — the URL printed at boot (https://localhost:3345/embraOS?token=…)
EMBRA_TUI=1 ./scripts/run-qemu.sh      # serial TUI on this terminal: functional, preferred for debugging
EMBRA_DB_VERBOSE=1 ./scripts/run-qemu.sh   # opt-in per-request WardSONDB log lines
EMBRA_BOOT=direct ./scripts/run-qemu.sh    # kernel + initramfs from the host: no firmware, no GRUB menu
```

Press `Ctrl-A X` to exit QEMU. On first boot the Config Wizard runs (name, the provider gateway or direct, LLM
provider + credentials, timezone), then Learning Mode forms and seals the soul.
Once the soul is sealed, run `/embeddings backfill` once: the seed knowledge packs
load before the wizard writes a configuration, so their nodes have no vectors until
backfilled (see `docs/BUILD/QUICK-START.md`).

> **Display window and a bridged network (validated on the Intel Mac: the bridged network and the standard-VGA window 2026-11-07, the virtio-gpu window 2026-11-10).**
> `run-qemu.sh`'s display switch, `EMBRA_VNC=1`, binds a VNC server to
> `127.0.0.1:5900`, the port macOS Screen Sharing holds on a Mac. To see the
> console on a window — the boot log and then the display console, the web console
> with keyboard and mouse — run QEMU by hand with a virtio-gpu device, a USB tablet
> on xHCI for the pointer (the machine's own PS/2 keyboard types) and a Cocoa
> display:
> ```bash
> cp "$(brew --prefix)/share/qemu/edk2-i386-vars.fd" /tmp/embraos-vars.fd
> qemu-system-x86_64 -machine q35 -accel hvf -m 4G -smp 4 \
>   -drive if=pflash,format=raw,readonly=on,file="$(brew --prefix)/share/qemu/edk2-x86_64-code.fd" \
>   -drive if=pflash,format=raw,file=/tmp/embraos-vars.fd \
>   -drive file=output/images/embraos.img,format=raw,if=virtio \
>   -smbios "type=11,value=embra.cols=$(tput cols) embra.rows=$(tput lines)" \
>   -vga virtio -display cocoa -serial mon:stdio \
>   -device qemu-xhci -device usb-tablet \
>   -nic user,hostfwd=tcp:127.0.0.1:50000-:50000,hostfwd=tcp:127.0.0.1:8443-:8443,hostfwd=tcp::3345-:3345
> ```
> This window is validated on the Intel Mac (2026-11-10: the boot log on the
> display, then the display console at 1920x1080 with the keyboard, the pointer
> and a click; the mode needs an image from `1ba8df5` on, an older one comes up
> at 1280x800 in this window), and the devices under QEMU on Linux. `-vga std` in
> place of `-vga virtio` gives the standard VGA device, which the kernel's `bochs`
> driver takes over: GRUB's menu, the boot log and the banner with the URL are
> validated on it on the Intel Mac (2026-11-07, without the tablet line).
>
> For a lease from the LAN instead of QEMU's user network, replace the `-nic user,…`
> line with `-nic vmnet-bridged,ifname=en0,model=virtio-net-pci` (`en0`, or the
> interface `route -n get default` names) and run the command under `sudo`:
> Apple's vmnet framework wants an entitlement Homebrew's QEMU lacks. There are no
> port forwards in that mode; the banner's second URL, on the leased address, is the
> one to open, from this Mac or from another machine on the LAN (the certificate
> names `localhost`, so the browser warns about the name once). The same recipe
> for Apple Silicon, with `-device virtio-gpu-pci` and a USB keyboard beside the
> tablet, is in [AARCH64-BUILD.md](AARCH64-BUILD.md).

> **Upgrading from the pre-volume flow? Delete the stale Mac-side
> `buildroot-src/`.** `run-qemu.sh` prefers `buildroot-src/output/images/` over
> `output/images/` (where Step 5 copies the volume build's image + kernel), so
> a leftover `buildroot-src/` from the old bind-mount flow shadows every
> freshly built image with a stale one. Delete it — this also returns ~60 GB:
> ```bash
> rm -rf buildroot-src
> ```
> Fresh clones never have a Mac-side `buildroot-src/` (the tree lives in the
> named volume), so this is one-time migration cleanup only.
> An explicit image outranks both locations: `./scripts/run-qemu.sh /path/to/embraos.img`, or `EMBRAOS_IMAGE=/path/to/embraos.img ./scripts/run-qemu.sh` (argument → `$EMBRAOS_IMAGE` → `buildroot-src/output/images/` → `output/images/`).

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

> **Build memory / parallel jobs (`JOBS`):** the Docker pass runs Buildroot at
> `nproc` jobs per package. Buildroot is RAM-hungry — on a memory-limited host
> (e.g. OrbStack's 4 GB default VM, `nproc`=8 → `-j8`) it OOMs in several
> flavors. The toolchain bootstrap dies with `libc.a Error 1` /
> `ar: … Cannot allocate memory`; regular package compiles fail with bare
> `Cannot allocate memory` while reading small headers (e.g. openssh's
> `sshd-auth.c` on `<sys/wait.h>`); the kernel hits the same on its own
> headers (`<uapi/linux/sem.h>`) or in `fixdep`; WPE WebKit's compile, the
> heaviest phase of the build, dies with `Killed signal terminated program
> cc1plus`: a WebCore unified-source bundle compiles in a 3 GiB process, the
> heaviest in 5 GiB (measured on this build's WebKit 2.50.5, GCC 14.3.0; the
> link takes 1.1 GiB), and Buildroot runs it at `nproc+1` jobs, so plan about
> 3.5 GiB of VM memory per job (a 4 GB VM no longer completes the image at any
> job count). All have the same fix — give the VM the memory or cap
> parallelism with `JOBS` (passed into the container with `-e`; the script
> hands it to the make-based packages through make's jobserver and to the
> cmake ones, WebKit and LLVM, as `BR2_JLEVEL`):
> ```bash
> docker run --rm -e JOBS=2 -v "$PWD":/work \
>   -v embraos-br-x86_64:/work/buildroot-src -w /work ubuntu:24.04 bash -c \
>   "apt-get update && apt-get install -y build-essential gcc g++ \
>    unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools qemu-utils libelf-dev && \
>    FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image.sh --buildroot-only"
> ```
> `JOBS` defaults to all cores (unchanged from canonical) — only set it when
> memory-constrained.

> **File-descriptor exhaustion (`Too many open files`):** bind-mount Buildroot
> builds die with varying errors including EMFILE — the Docker runtime's
> macOS-side file provider holds a file descriptor per accessed inode (one host
> process, capped by `kern.maxfilesperproc` ≈ 138k), and a cold Buildroot tree
> touches 300k+ inodes; no in-container `--ulimit` can prevent it. The named
> volume in the commands above keeps the tree on the Docker VM's native
> filesystem, bypassing the provider entirely. Mechanism + bind-mount stopgap
> detailed in [AARCH64-BUILD.md](AARCH64-BUILD.md)'s Step-4 troubleshooting —
> identical on Intel; observed and volume-fix-verified on the Apple-Silicon
> hosts (2026-07-19).

> **In-OS Rust toolchain (Step 3.5):** Downloads a SHA-256-verified
> `rust-1.98.1-x86_64-unknown-linux-musl` (host triple = guest arch, not
> build-host arch) + `rust-std-1.98.1-wasm32-unknown-unknown` into
> `vendor/rust-toolchain`, baked into the rootfs at `/opt/rust` for
> embra-guardian-v1. Needs `xz` + `sha256sum` on PATH — `brew install xz
> coreutils` + the gnubin PATH from step 1 satisfies both. First build needs
> network for this. The toolchain is the largest item in the image: about
> 320 MiB of a root filesystem of about 813 MiB, as compressed in the SquashFS (the firmware for bare metal is about 270 MiB of that and the display console's stack about 90 MiB)
> (measured on the x86_64 Linux build, 2026-11-08).
> Override the pin with `RUST_TOOLCHAIN_VERSION=...`. An image with a different
> pin leaves existing Guardian tools unloaded until the operator runs
> `/guardian rebuild --all`.

> **Embedding model (Step 3.6):** Downloads `BAAI/bge-small-en-v1.5` (~133 MB, SHA-256-verified) into
> `vendor/embedding-model` and bakes it into the rootfs for in-process semantic KG retrieval.
> Runs **natively on macOS** like Step 3.5 — only Step 4 (Buildroot) goes to Docker — so it needs
> Homebrew's GNU coreutils on `PATH` for `sha256sum`, the same prerequisite Step 3.5 already has.
> The build fails if the model is missing from the rootfs rather than shipping an image whose
> retrieval silently falls back to keyword-only.

> **Port forwarding:** QEMU forwards 50000 (gRPC) and 8443 (REST) on the host's loopback only,
> and 3345 (HTTPS — https://localhost:3345/embraOS, behind this boot's access token) on every host interface, in both UI modes;
> other machines on the host's network reach the web console and nothing else, and only with the URL embraOS printed at boot. apid's REST routes are
> `/health`, `/version` and `/status` (the brain's status incl. the LLM provider
> probe; 503 while the brain is away). Test:
> ```bash
> curl http://localhost:8443/health
> curl http://localhost:8443/status
> ```

> **Backup & restore:** macOS can't loop-mount the image natively, so
> `embraos-backup-mac.sh` runs the Linux backup script inside a privileged
> Docker container (arch-agnostic — same script works for x86_64 and aarch64
> images). Stop the VM first (`Ctrl-A X`).
> ```bash
> ./scripts/embraos-backup-mac.sh backup --label pre-rebuild
> ./scripts/embraos-backup-mac.sh restore            # latest; restore <name> for a specific one (asks y/N, then replaces STATE + DATA)
> ./scripts/embraos-backup-mac.sh list
> ./scripts/embraos-backup-mac.sh verify
> ./scripts/embraos-backup-mac.sh --image ~/images/embraos.img verify   # a specific image
> ```
> Backups live in `~/embraOS_BACKUPS/` (override `EMBRAOS_BACKUP_DIR`) and are
> interchangeable with Ubuntu and Apple-Silicon backups. `--image <path>` (or
> `EMBRAOS_IMAGE`) targets an image outside the project — the wrapper bind-mounts
> its directory into the container.

> **Pre-seeding STATE/DATA:** same constraint, same shape — `seed-state-mac.sh`
> runs `seed-state.sh` inside a privileged Docker container (also arch-agnostic).
> Host paths are translated to container paths automatically; the mapping is
> printed before the container starts. Seed **after** the build — a Buildroot
> pass regenerates empty STATE/DATA and discards an earlier seed.
> ```bash
> ./scripts/seed-state-mac.sh --ca-dir /path/to/dir-with-rootCA.pem
> ./scripts/seed-state-mac.sh --dry-run --seed-dir ~/my-packs   # print only
> ```
> Unlike the backup wrapper this installs no apt packages, so it works offline.
> If either wrapper is ever killed mid-run (`docker kill`, Docker restart, OOM) a loop
> device can survive in the Docker VM pinning the image — see "Troubleshooting — backup
> & seeding" in [AARCH64-BUILD.md](AARCH64-BUILD.md) for the detach recipe.

---

## End-to-End Validation

Run this sequence on the Intel Mac after the first successful build; re-run as
the regression checklist after any canonical-build bump, and re-stamp the status
banner at the top of this file.

1. **Boot — web console (default).** `./scripts/run-qemu.sh`. Banner shows
   `Acceleration: HVF (macOS)`, `UI mode: web console (default)`, and
   `3345→3345 (HTTPS web)`; GRUB's menu shows on this terminal and boots slot A
   (the UEFI path, validated on an Intel Mac 2026-11-07; `EMBRA_BOOT=direct` is
   the older path). No kernel panic on `ttyS0`.
2. **Config Wizard + Learning Mode** in the browser at the URL embraOS printed at
   boot (`https://localhost:3345/embraOS?token=…`; accept the embraOS-CA cert) — name, the provider gateway or direct, LLM
   provider + credentials, timezone; full soul-formation conversation; soul
   sealed.
3. **REST gateway:** from the host, `curl http://localhost:8443/health` →
   healthy; `curl http://localhost:8443/status` → `ok:true` with the `services` map.
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
