# embraOS — aarch64 Apple Silicon Build Guide

> **Target:** QEMU-bootable ARM64 disk image running natively on Apple Silicon via HVF —
> near-native speed, no 5–10× TCG software-emulation penalty.
>
> **Host:** macOS on Apple Silicon (M1/M2/M3/M4).
>
> ✅ **Status — re-verified end-to-end on Apple Silicon (2026-07-19).** Verified on a
> MacBook (Apple M2, 8 GB) against Buildroot `2026.02.1` under the **named-volume
> Docker Buildroot flow** (note below; build also green on an M1 Mac Mini): the full build
> pipeline (Step 0.5 Trunk/WASM frontend → Rust cross-compile → initramfs → Step 3.5
> in-OS Rust toolchain → Buildroot, including the new `embra-web` and
> `embra-rust-toolchain` packages), HVF boot, soul formation, the serial TUI
> (`EMBRA_TUI=1`), and **embra-guardian-v1** (a dynamic tool compiled in-guest via
> `/opt/rust` and run in the wasmtime sandbox). embra-guardian-v1 remains an
> **experimental** feature upstream (project-wide, not aarch64-specific). The build's
> memory wants are in the `JOBS` note below (WebKit's compile sizes the Docker VM). Re-run the [End-to-End
> Validation](#end-to-end-validation) checklist after any canonical-build bump and
> re-stamp this date.
>
> **2026-10-25 — the build passes with everything that landed since.** The toolchain
> bump to Rust 1.98.1, Step 3.6 (the embedding model) and the hardened Buildroot hooks
> (`set -euo pipefail`) landed after the 2026-07-19 run; the operator's Apple Silicon
> build of 2026-10-25 passed with all three. The x86_64 Linux build validates them on
> every rebuild. The End-to-End Validation checklist's date above stands until the
> checklist is re-run in full.
>
> **2026-11-06 — the Phase 2 image on Apple Silicon.** The operator's build and run
> of 2026-11-06 (branch `phase2`): the build with macOS's BSD cpio, the disk booting
> itself through the edk2 firmware and GRUB's menu into slot A, the display window
> (`ramfb`) with the boot log and the console URL, a bridged NIC through
> `vmnet-bridged` with the leased URL opened from another machine, DHCP, and the
> backup wrapper on a RocksDB image. Step 1 of the checklist below carries it.
>
> **2026-11-09 — the wave-6 image on Apple Silicon.** The operator's build of
> 2026-11-09 (branch `phase2`: the display console's stack, every firmware family,
> 2048 MiB slots) completed in Docker under OrbStack with the VM's memory at 64 GiB
> and Buildroot's 17 jobs, in about 45 minutes; every assert passed (128 fragment
> symbols read back, 134 of 134 firmware families, 1383 modules in 71 MB, the
> squashfs 753 MiB of the 2048 MiB slot, the image 8514 MiB), and the image booted
> through GRUB's menu to the banner under QEMU/HVF, the lease in 0.2 s. The one
> kernel line of that boot, `PCI: OF: of_root node is NULL, cannot create PCI host
> bridge node`, is the arm64 base configuration's Raspberry Pi RP1 driver selecting
> `PCI_DYNAMIC_OF_NODES`, printed under an ACPI boot where there is no device tree:
> expected and harmless. The same boot's `Freed the initramfs: 0 MiB` line showed
> the probe-time firmware archive unpacking to nothing on both architectures, fixed
> in `fb6399c`; an image built before it carries the old archive until rebuilt. The
> image rebuilt after the fix (`--buildroot-only`, from `182e731`) booted the same
> way on 2026-11-09 and said `Freed the initramfs: 4 MiB`: the five probe-time
> blobs (cxgb4, ice, qed) and the init, there being no microcode on aarch64; an
> x86_64 image of the same build says 31 MiB, its microcode archive included.
>
> **2026-11-10 — the display console on Apple Silicon.** The operator's run of
> 2026-11-10 under QEMU/HVF with a virtio-gpu display and a USB keyboard and tablet
> on xHCI (the command under [Boot](#3-boot)): after the boot log the window shows
> the web console at 1920x1080, which takes the keyboard, the pointer and a click.
> With `-device ramfb` in place of the virtio-gpu device (the firmware's framebuffer,
> which had shown the boot log on 2026-11-06) the same run passed that day: after
> the boot log the window shows the web console at 800x600, which takes the
> keyboard, the pointer and a click.
>
> **2026-11-11 — the installer medium on Apple Silicon.** The aarch64 build writes
> `embraos.iso` from `9e42bd1` on. The operator's runs of 2026-11-11: the medium
> works under VMware Fusion, where an ISO is a VM's DVD drive, and under QEMU in
> three layouts of the launcher: the DVD onto a virtio-blk disk (the default), the
> DVD onto a virtio-scsi disk (`EMBRA_DISK_IF=scsi`) and the USB stick onto an
> NVMe disk (`EMBRA_ISO_AS=usb` with `EMBRA_DISK_IF=nvme`). Each installed disk
> then boots alone on its bus. The commands are under [Boot](#3-boot). The VM
> images of this architecture (`embraos.ova`, `embraos.vmdk`, `embraos.vhdx`) are
> **TBD** on a hypervisor until run.
>
> **2026-07-19 — named-volume Buildroot flow.** The Docker pass now mounts a named
> volume over `buildroot-src`: building the Buildroot tree on the bind mount
> exhausts the Docker runtime's macOS-side file provider (`Too many open files` —
> see [Troubleshooting — Step 4](#troubleshooting--step-4-buildroot-in-docker)).
> Operator-verified 2026-07-19: builds green on the M2, M1, and Intel-Mac hosts,
> and the full E2E checklist re-passed — the banner date above reflects it.
>
> **Single source of truth (2026-05-19):** the aarch64 scripts are committed
> in-tree (`scripts/`, `buildroot/configs/`) and the arch-parameterized Buildroot
> tree (commit `f6a684a`) is end-to-end QEMU-verified on **both** x86_64 and
> aarch64. The former standalone Apple-Silicon build bundle has been retired —
> this guide now lives at `docs/BUILD/AARCH64-BUILD.md`, its scripts in `scripts/`.

---

## Quick Start

Builds a QEMU-bootable ARM64 embraOS image that runs natively on Apple Silicon via HVF.
Steps 0.5–3 (frontend + Rust cross-compile + initramfs) run on macOS; Step 3.5 (in-OS
Rust toolchain), Step 3.6 (the embedding model) and Step 4 (Buildroot) run in a
`linux/arm64` Docker container.

> **Default UI is the browser console.** `run-qemu-aarch64.sh` boots the
> **embra-web** console by default, served over HTTPS at
> **https://localhost:3345/embraOS** — open it from the URL embraOS prints at boot, which
> carries this boot's access token (`?token=…`; accept the embraOS-CA cert on first
> visit; the token changes at every boot and is shown nowhere else). Set
> **`EMBRA_TUI=1`** before `run-qemu-aarch64.sh` to boot the serial TUI on this
> terminal instead — no image rebuild needed. The serial TUI is functional but is
> preferred for debugging; the web console is the primary interface. The disk
> boots itself: the edk2 firmware Homebrew's qemu ships, then GRUB's menu on the
> EFI System Partition (`EMBRA_BOOT=uefi`, the default; validated on Apple Silicon
> 2026-11-06). `EMBRA_BOOT=direct` boots the kernel and the
> initramfs from the host as before, no firmware and no menu.

> **zsh paste gotcha:** macOS's default `zsh` doesn't treat `#` as a comment in
> interactive shells — pasting a multi-line code block from this guide makes
> any `#`-prefixed (or inline `… # comment`) line fail with `command not found`
> or `no matches found`. Run once per session (or add to `~/.zshrc`):
> ```bash
> setopt interactive_comments
> ```

### 1. Prerequisites (macOS, Apple Silicon)

```bash
# Rust via rustup — NOT Homebrew (brew uninstall rust first if present).
# The checkout pins its compiler in rust-toolchain.toml; rustup installs
# that version on the first cargo run inside the repository.
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

# Step 3.5 (in-OS Rust toolchain) + Step 4 (Buildroot) in Docker (linux/arm64).
# The Buildroot tree lives in the named volume embraos-br-aarch64 — the Docker
# VM's native filesystem. Building it on the bind mount instead exhausts the
# macOS file provider's fd pool ("Too many open files") — see Troubleshooting.
docker run --rm -v "$PWD":/work -v embraos-br-aarch64:/work/buildroot-src \
  -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools qemu-utils && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
```

### 3. Boot

```bash
./scripts/run-qemu-aarch64.sh                  # web console (default) — the URL printed at boot (https://localhost:3345/embraOS?token=…)
EMBRA_TUI=1 ./scripts/run-qemu-aarch64.sh      # serial TUI on this terminal: functional, preferred for debugging
EMBRA_DB_VERBOSE=1 ./scripts/run-qemu-aarch64.sh   # opt-in per-request WardSONDB log lines
EMBRA_BOOT=direct ./scripts/run-qemu-aarch64.sh    # kernel + initramfs from the host: no firmware, no GRUB menu
EMBRA_DHCP_START=10.0.2.200 ./scripts/run-qemu-aarch64.sh   # QEMU's DHCP leases this address: the banner's second URL
EMBRA_ISO=output/images/embraos.iso ./scripts/run-qemu-aarch64.sh disk.qcow2   # boot the installer onto a blank qemu-img disk
EMBRA_ISO_AS=usb EMBRA_DISK_IF=nvme ...            # the medium as a USB stick, the target on NVMe (a DVD on virtio-scsi and
                                                   # virtio-blk by default; EMBRA_DISK_IF=scsi is virtio-scsi)
EMBRA_DISK_IF=nvme ./scripts/run-qemu-aarch64.sh disk.qcow2   # a disk on that bus without the medium (*.qcow2 is opened as qcow2)
```

> **Installing from the ISO (validated on Apple Silicon under QEMU and under VMware Fusion, 2026-11-11).** The build
> writes `output/images/embraos.iso`, the installer medium of
> [QUICK-START.md](QUICK-START.md) for aarch64: GRUB's arm64 loader at
> `EFI/BOOT/BOOTAA64.EFI`, the same menu and the same installer, a DVD through El
> Torito or a USB stick through its GPT. Under QEMU:
> ```bash
> qemu-img create -f qcow2 disk.qcow2 40G
> EMBRA_ISO=output/images/embraos.iso ./scripts/run-qemu-aarch64.sh disk.qcow2   # GRUB's menu, then the installer on this terminal
> ./scripts/run-qemu-aarch64.sh disk.qcow2                                       # the installed disk on its own
> ```
> The medium is a virtual DVD on a virtio-scsi controller of its own
> (`EMBRA_ISO_AS=usb`: a USB stick on xHCI) and the disk is virtio-blk
> (`EMBRA_DISK_IF=nvme` or `scsi`, with the medium and without it). The medium
> stays first in the boot order, so after the installer's reboot its menu is
> back: leave QEMU with `Ctrl-A X` and start the disk alone. A hypervisor takes
> the ISO as a new VM's DVD drive, with a blank disk beside it. A release carries
> the medium as `embraos-<tag>-aarch64-<engine>.iso` with its checksum line and
> signature ([UPDATES-AND-BACKUPS.md](../OPERATIONS/UPDATES-AND-BACKUPS.md)).
> Validated on Apple Silicon on 2026-11-11 under VMware Fusion and under QEMU: as
> a DVD onto a virtio-blk disk and onto a virtio-scsi disk, and as a USB stick
> onto an NVMe disk, each installed disk booting alone on its bus. Every other
> hypervisor of this architecture is **TBD** until run.

> **Display window and a bridged network (validated on Apple Silicon: the window 2026-11-10, the bridged network 2026-11-06).**
> `run-qemu-aarch64.sh` has no display switch (the x86_64 launcher's `EMBRA_VNC=1`
> has no aarch64 counterpart yet). To see the console on a window — the boot log
> and then the display console, the web console with keyboard and mouse — run QEMU
> by hand with a virtio-gpu device and a USB keyboard and tablet on xHCI (the
> `virt` machine has no keyboard or pointer of its own):
> ```bash
> cp "$(brew --prefix)/share/qemu/edk2-arm-vars.fd" /tmp/embraos-vars.fd
> qemu-system-aarch64 -machine virt -accel hvf -cpu host -m 4G -smp 4 \
>   -drive if=pflash,format=raw,readonly=on,file="$(brew --prefix)/share/qemu/edk2-aarch64-code.fd" \
>   -drive if=pflash,format=raw,file=/tmp/embraos-vars.fd \
>   -drive file=output/images/embraos.img,format=raw,if=virtio \
>   -smbios "type=11,value=embra.cols=$(tput cols) embra.rows=$(tput lines)" \
>   -device virtio-gpu-pci -display cocoa -serial mon:stdio \
>   -device qemu-xhci -device usb-kbd -device usb-tablet \
>   -nic user,hostfwd=tcp:127.0.0.1:50000-:50000,hostfwd=tcp:127.0.0.1:8443-:8443,hostfwd=tcp::3345-:3345
> ```
> `-device ramfb` in place of `-device virtio-gpu-pci` gives the firmware's own
> framebuffer, which the kernel's `simpledrm` takes over: GRUB's menu, the boot log
> and the banner with the URL are validated on it (2026-11-06), and the display
> console with the keyboard, the pointer and a click (2026-11-10). The display is
> 800x600 there: edk2's `ramfb` driver (`OvmfPkg/QemuRamfbDxe`) offers 640x480,
> 800x600 and 1024x768 and starts in the second, so the request for 1920x1080
> leaves it as it is.
>
> For a lease from the LAN instead of QEMU's user network, replace the `-nic user,…`
> line with `-nic vmnet-bridged,ifname=en0,model=virtio-net-pci` (`en0`, or the
> interface `route -n get default` names) and run the command under `sudo`:
> Apple's vmnet framework wants an entitlement Homebrew's QEMU lacks. There are no
> port forwards in that mode; the banner's second URL, on the leased address, is the
> one to open, from this Mac or from another machine on the LAN (the certificate
> names `localhost`, so the browser warns about the name once).

Press `Ctrl-A X` to exit QEMU. On first boot the Config Wizard runs (name, the provider gateway or direct, LLM provider
+ credentials, timezone), then Learning Mode forms and seals the soul.
Once the soul is sealed, run `/embeddings backfill` once: the seed knowledge packs
load before the wizard writes a configuration, so their nodes have no vectors until
backfilled (see `docs/BUILD/QUICK-START.md`).

> **Upgrading from the pre-volume flow? Delete the stale Mac-side `buildroot-src/`.**
> `run-qemu-aarch64.sh` prefers `buildroot-src/output/images/` over `output/images/`
> (where Step 5 copies the volume build's image + kernel), so a leftover
> `buildroot-src/` from the old bind-mount flow shadows every freshly built image
> with a stale one. Delete it — this also returns ~60 GB of disk:
> ```bash
> rm -rf buildroot-src
> ```
> Fresh clones never have a Mac-side `buildroot-src/` (the tree lives in the named
> volume), so this is one-time migration cleanup only.
> An explicit image outranks both locations: `./scripts/run-qemu-aarch64.sh /path/to/embraos.img`, or `EMBRAOS_IMAGE=/path/to/embraos.img ./scripts/run-qemu-aarch64.sh` (argument → `$EMBRAOS_IMAGE` → `buildroot-src/output/images/` → `output/images/`).

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

> **Build memory / parallel jobs (`JOBS`):** the Docker pass runs Buildroot at `nproc`
> jobs per package. Two phases want memory. The GCC + musl toolchain bootstrap dies in
> a 4 GB VM at `-j8` (`ar: … Cannot allocate memory`, `libc.a Error 1`). WPE WebKit's
> compile is the heaviest phase of the build: measured on the x86_64 build of the same
> WebKit 2.50.5 (GCC 14.3.0, `-O2 -g0`), a WebCore unified-source bundle compiles in a
> 3 GiB process, the heaviest in 5 GiB, and the link of libWPEWebKit takes 1.1 GiB.
> Buildroot runs WebKit's ninja at `nproc+1` jobs, 17 on a 16-core Mac, so plan about
> 3.5 GiB of VM memory per job: 60 GiB at 17 jobs, 16 GiB at 4; a 64 GiB VM completed
> the build at 17 jobs in about 45 minutes (2026-11-09). Give the VM the memory
> (OrbStack: `orb config set memory_mib 61440 && orb restart`; Docker Desktop:
> Settings, Resources) or cap the jobs with `JOBS`, passed into the container with
> `-e`: the script hands it to the make-based packages through make's jobserver and to
> the cmake ones (WebKit, LLVM) as `BR2_JLEVEL`. `JOBS` defaults to all cores; set it
> only when the VM is smaller than the jobs want. Too little memory shows as `Killed
> signal terminated program cc1plus`
> ([troubleshooting](#killed-signal-terminated-program-cc1plus-oom-compiling-webkit)).
> ```bash
> docker run --rm -e JOBS=4 -v "$PWD":/work \
>   -v embraos-br-aarch64:/work/buildroot-src -w /work ubuntu:24.04 bash -c \
>   "apt-get update && apt-get install -y build-essential gcc g++ \
>    unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools qemu-utils && \
>    FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
> ```
> On a Linux host (no Docker), just prefix: `JOBS=4 ./scripts/build-image-aarch64.sh
> --buildroot-only`.

> **In-OS Rust toolchain (Guardian):** Step 3.5 downloads a SHA-256-verified Rust
> toolchain — host `rust-1.98.1-aarch64-unknown-linux-musl` + arch-agnostic
> `rust-std-1.98.1-wasm32-unknown-unknown` — into `vendor/rust-toolchain`, baked into
> the rootfs at `/opt/rust` for embra-guardian-v1. On macOS it is deliberately
> **deferred to the Docker pass** (macOS lacks `sha256sum`/`xz`); `vendor/rust-toolchain`
> rides the `-v "$PWD":/work` bind mount. First build needs network for this. The
> toolchain is the largest item in the image: about 285 MiB of the root filesystem, as
> compressed in the SquashFS. Override the pin with `RUST_TOOLCHAIN_VERSION=...`. An
> image with a different pin leaves existing Guardian tools unloaded until the operator
> runs `/guardian rebuild --all`.

> **Embedding model (Step 3.6):** Downloads `BAAI/bge-small-en-v1.5` (~133 MB, SHA-256-verified) into
> `vendor/embedding-model` and bakes it into the rootfs for in-process semantic KG retrieval.
> Architecture-independent — the same bytes serve x86_64 and aarch64 — and, like Step 3.5, it is
> **skipped on macOS** and run by the Docker Buildroot pass instead. The build fails if the model is
> missing from the rootfs rather than shipping keyword-only retrieval.

> **Port forwarding:** QEMU forwards 50000 (gRPC) and 8443 (REST) on the host's loopback only,
> and 3345 (HTTPS — https://localhost:3345/embraOS, behind this boot's access token) on every host interface, in both UI modes;
> other machines on the host's network reach the web console and nothing else, and only with the URL embraOS printed at boot. apid's REST routes are `/health`,
> `/version` and `/status` (the brain's status incl. the LLM provider probe; 503 while
> the brain is away). Test:
> ```bash
> curl http://localhost:8443/health
> curl http://localhost:8443/status
> ```

> **Backup & restore:** macOS can't loop-mount the image natively, so
> `embraos-backup-mac.sh` runs the Linux backup script inside a privileged Docker
> container. Stop the VM first (`Ctrl-A X`).
> ```bash
> ./scripts/embraos-backup-mac.sh backup --label pre-rebuild
> ./scripts/embraos-backup-mac.sh restore            # latest; restore <name> for a specific one (asks y/N, then replaces STATE + DATA)
> ./scripts/embraos-backup-mac.sh list
> ./scripts/embraos-backup-mac.sh verify
> ./scripts/embraos-backup-mac.sh --image ~/images/embraos.img verify   # an image outside the project
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
logs that Steps 3.5 and 3.6 are deferred, and exits at the Step 4 guard printing the
Docker command. The Docker `--buildroot-only` pass then runs Steps 3.5, 3.6 and 4.

- **Step 0.5 — embra-web frontend (Trunk → WASM).** Builds `crates/embra-web-ui` with
  Trunk so `embra-web` can embed `dist/` via `rust-embed`. wasm32 output is
  host-arch-agnostic — identical on x86_64 and aarch64 hosts. Aborts if `trunk` is
  missing. Runs only when not `--buildroot-only`.
- **Step 1 — Rust binaries (aarch64 musl static).** `cargo build --release --target
  aarch64-unknown-linux-musl`. ~30 minutes, mostly compilation. Builds all 12
  workspace members including `wardsondb` (vendored at `crates/wardsondb` — no
  sibling repo; the former Step 2 is gone).
- **Step 3 — initramfs.** `create_initramfs.sh` (RUST_TARGET-aware) packs `embra-init`
  as `/init` into `initramfs.cpio.gz`, which `post_build.sh` places in the slot's
  `/boot` beside the kernel.
- **Step 3.5 — in-OS Rust toolchain.** See [In-OS Rust toolchain](#in-os-rust-toolchain-embra-guardian-v1)
  below. Unconditional w.r.t. `--buildroot-only`; **skipped on macOS** and run in the
  Docker/Linux pass.
- **Step 4 — Buildroot.** Linux-only. Clones Buildroot (into an existing-but-empty
  `buildroot-src` too — the named-volume mountpoint case), ensures it is at
  `$BUILDROOT_VERSION` (default `2026.02.1`) via the safe-switch, dircleans the embraOS
  + upstream packages, configures `embraos_aarch64_defconfig`, and builds the kernel
  (`Image`, installed into the rootfs at `/boot`) + GRUB (`BOOTAA64.EFI`) + SquashFS
  rootfs + `embraos.img`. On macOS the Docker pass mounts the
  named volume `embraos-br-aarch64` at `buildroot-src`, keeping the tree and all build
  output on the Docker VM's native filesystem instead of the bind mount.
- **Step 5 — copy outputs.** `embraos.img`, `Image` (the direct boot path) and
  `rootfs.squashfs` (the slot image an OS update ships) → `output/images/` (copied
  out of the volume onto the host tree), then the same disk converted for the other
  hypervisors with `qemu-img`, a folder each under `output/images/`:
  `vmware/embraos.vmdk` (a disk for Fusion and VirtualBox), `hyper-v/embraos.vhdx`
  (Hyper-V generation 2, Secure Boot off) and `vmware/embraos.ova` (VMware
  Fusion on Apple Silicon; hardware version 20, guest type "Other 6.x or later Linux
  (64-bit)", the disk on an NVMe controller, a VMXNET3 adapter and a USB 3
  controller for the keyboard and the pointer: the kernel builds VMware's
  paravirtual SCSI driver for x86 alone, so the aarch64 descriptor does not
  declare the controller the x86_64 one does). Each is checked against the raw
  image's size; the aarch64 ones are **TBD** until booted on a hypervisor of that
  architecture. `parallels/` holds `embraos.vmx` beside a copy of the VMDK: the
  machine Parallels Desktop imports on an Intel Mac
  ([QUICK-START.md](QUICK-START.md)), written here with VMware's Arm guest type
  (`arm-other6xlinux-64`) and the disk on SATA. It is **TBD**: no import of it
  has been run, and Parallels' list of what Apple silicon Macs do not support
  ([KB 128914](https://kb.parallels.com/en/128914), reviewed 2026-07-07) names
  the import of VMware machines and NVMe disks, so the import may be refused
  there and the installer medium may be the one way in. Beside the raw image
  `embraos.iso`, the installer medium for aarch64
  (`write_installer_iso`: xorriso and GRUB's arm64 modules, both built by
  Buildroot inside the Docker pass), validated on Apple Silicon under QEMU and
  under VMware Fusion (2026-11-11; [Boot](#3-boot)).

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

- **What it downloads:** host `rust-1.98.1-aarch64-unknown-linux-musl` (aarch64 because
  it runs *inside* the aarch64 guest; `aarch64-unknown-linux-musl` is a Tier-2
  *with-host-tools* target so a prebuilt `rustc`/`cargo` exists) + arch-agnostic
  `rust-std-1.98.1-wasm32-unknown-unknown`. Both are SHA-256-verified against the
  pins in `scripts/rust-toolchain.sha256` (the `xz_hash` values of the signed channel
  manifest), never against a checksum fetched beside the download.
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
| `scripts/seed-state-mac.sh` | macOS wrapper for STATE/DATA pre-seeding via Docker |

#### Modified files (backward-compatible — x86_64 build unchanged)

These are arch-**parameterized**, not arch-flipped: a single committed tree builds
both arches, so there is **no per-arch sed** and nothing to revert.

| File | Change |
|---|---|
| `scripts/create_initramfs.sh` | Respects `RUST_TARGET` env var; defaults to `x86_64-unknown-linux-musl` when unset |
| `buildroot/external.mk` | Defines `EMBRAOS_RUST_TARGET = $(if $(BR2_aarch64),aarch64-…,x86_64-…)` |
| `buildroot/package/*/*.mk` (7) | `_SITE` now uses `target/$(EMBRAOS_RUST_TARGET)/release` — arch-neutral |
| `buildroot/board/embraos/post_image.sh` | Detects the kernel (`Image`/`bzImage`) in `BINARIES_DIR`, stages the ESP (GRUB's loader, `grub.cfg` rendered from `grub.cfg.in` with the kernel name and `console=ttyAMA0`, `grubenv`), renders `genimage.cfg.in` from `layout.sh`, runs genimage and checks the layout |
| `buildroot/board/embraos/genimage.cfg` → `…/genimage.cfg.in` | Static config replaced by a template: the sizes from `layout.sh` (resolved per build); the kernel lives inside each slot's SquashFS, so the template names no kernel |

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
| Machine | `-machine q35` on the UEFI path, QEMU's default `pc` on the direct path | `-machine virt` |
| Firmware (UEFI path) | OVMF (`/usr/share/OVMF/OVMF_CODE_4M.fd`, the `ovmf` package) or QEMU's `edk2-x86_64-code.fd` | `edk2-aarch64-code.fd` (Homebrew's qemu) or AAVMF (`qemu-efi-aarch64`) — validated on Apple Silicon 2026-11-06 |
| Loader | `EFI/BOOT/BOOTX64.EFI` | `EFI/BOOT/BOOTAA64.EFI` |
| Acceleration on macOS | `-accel hvf` (`run-qemu.sh` passes it on every Mac; an x86_64 guest runs under HVF on an Intel Mac only) | `-accel hvf` (native — near-native speed) |
| CPU | QEMU's default (`run-qemu.sh` passes no `-cpu`) | `-cpu host` under HVF, `-cpu max` otherwise |
| Kernel image | `bzImage` | `Image` |
| Consoles | `console=tty0 console=ttyS0` | `console=tty0 console=ttyAMA0` (the serial line, last, owns `/dev/console`; embra-init's and embrad's boot messages go to it and to the display both; both consoles rendered into `grub.cfg` by `post_image.sh`, the serial one alone passed by the direct path) |
| Default UI | embra-web console; `EMBRA_TUI=1` → serial TUI | identical |
| Port forwards | 127.0.0.1:50000 / 127.0.0.1:8443 / *:3345 (both UI modes) | identical |
| Display | `EMBRA_VNC=1` — a VNC server on the host's loopback beside the serial line | no launcher switch; `-device virtio-gpu-pci -display cocoa` with a USB keyboard and tablet by hand ([Boot](#3-boot)) |
| Network | QEMU's user network; `EMBRA_DHCP_START=<address>` picks the leased address | identical; a bridged NIC through `vmnet-bridged` under `sudo` ([Boot](#3-boot)) |
| Display console | the web console on the display through WPE WebKit and cog (validated under QEMU, on ESXi and on Hyper-V) | the same stack; validated under QEMU on Apple Silicon on a virtio-gpu display with a USB keyboard and tablet (2026-11-10: the page at 1920x1080, the keyboard, the pointer and a click; the command under [Boot](#3-boot)) and on `ramfb`, the firmware's framebuffer (2026-11-10: the page at 800x600, the keyboard, the pointer and a click); the launcher has no display switch (WebKit's compile is the heaviest phase of the build: about 3.5 GiB of VM memory per job, the `JOBS` note above; the aarch64 build with this stack completes: 2026-11-09, OrbStack's VM at 64 GiB, 17 jobs, about 45 minutes) |
| Kernel fragments | `linux.fragment` + `linux-x86_64.fragment`, read back at build time; no modules | `linux.fragment` alone, read back the same way; the arm64 base configuration keeps 1383 modules nothing loads, 71 MB uncompressed under `/lib/modules` (the 2026-11-09 build) |
| Installer medium | `embraos.iso`, UEFI, bare metal and the hypervisors (validated under QEMU, on ESXi and on Hyper-V) | `embraos.iso` for this architecture: GRUB's arm64 loader (`BOOTAA64.EFI`), the same menu and installer, `EMBRA_ISO` on the launcher ([Boot](#3-boot)); validated on Apple Silicon under QEMU and under VMware Fusion (2026-11-11) |
| Microcode | `/boot/ucode.cpio` with Intel's and AMD's files, loaded first | the empty archive, so GRUB's line is the same |
| Pre-wave-6 tree | `make util-linux-libs-dirclean util-linux-dirclean mesa3d-dirclean eudev-dirclean` once | the same, inside the Docker build |

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
./scripts/embraos-backup-mac.sh --image ~/images/embraos.img verify   # a specific image
```

`--image <path>` (or `EMBRAOS_IMAGE`) targets an image anywhere on the host: the
wrapper resolves it host-side and bind-mounts its directory into the container
(read-write, since `restore` writes the image), then hands the container path to
the inner script.

#### How it works

The wrapper spins up a privileged `ubuntu:24.04` container with two volume mounts — three when the image lives outside the project root:

| Container path | Host path | Purpose |
|---|---|---|
| `/work` | Project root | Disk image + scripts (read/write) |
| `/backups` | `~/embraOS_BACKUPS` | Backup storage (persists across runs) |
| `/mnt/image` | Image's directory | Only when `--image`/`EMBRAOS_IMAGE` points outside the project root (read-write — `restore` writes the image) |

`--privileged` gives the container loop device access for `mount -o loop`. The container
installs `rsync` and `python3`, then runs the original `embraos-backup.sh`
unchanged (partition geometry comes from `partx`, in the base image). The QEMU-running check happens on the host side before Docker starts, since
the container can't see host processes. Backups made on Ubuntu and Mac are
interchangeable.

#### Workflow

```bash
# 1. Stop the VM (Ctrl-A X in QEMU console)

# 2. Backup
./scripts/embraos-backup-mac.sh backup --label pre-rebuild

# 3. Rebuild the image (Docker pass re-runs Step 3.5 — a no-op if already staged)
RUST_TARGET=aarch64-unknown-linux-musl ./scripts/create_initramfs.sh
docker run --rm -v "$PWD":/work -v embraos-br-aarch64:/work/buildroot-src \
  -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools qemu-utils && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"

# 4. Restore into the fresh image
./scripts/embraos-backup-mac.sh restore

# 5. Boot and verify
./scripts/run-qemu-aarch64.sh
# In TUI: /status, /sessions, memory_scan
```

### Pre-seeding STATE and DATA

`seed-state.sh` writes operator-provided files into a built image's STATE/DATA partitions
before first boot — root CA certificates, knowledge packs, importable intelligence graphs,
a soul hash. It loop-mounts ext4, which macOS cannot do, so `seed-state-mac.sh` wraps it in
a privileged Docker container the same way `embraos-backup-mac.sh` does.

The seeding logic itself is arch-neutral (partition layout comes from the one
`genimage.cfg.in` that builds both arches), so this wrapper is equally correct for x86_64
images — it is a macOS-host tool, not an ARM tool.

#### Setup

`seed-state-mac.sh` ships committed in `scripts/` alongside `seed-state.sh` — nothing to set up.

#### Usage

Same options as `seed-state.sh`:

```bash
./scripts/seed-state-mac.sh --ca-dir /path/to/dir-with-rootCA.pem
./scripts/seed-state-mac.sh --seed-dir ~/my-packs --import-dir Imported_Intelligence   # your own packs; the OS's are in the image
./scripts/seed-state-mac.sh --wipe state,data --yes       # clean first boot, no confirmation prompt
./scripts/seed-state-mac.sh --dry-run --ca-dir ~/certs    # print the command, run nothing
```

#### How it works

| Container path | Host path | Purpose |
|---|---|---|
| `/work` | Project root | Disk image + scripts (read/write) |
| `/mnt/image` | Image's directory | Only when the image lives outside the project root |
| `/mnt/{phase0,import,seed,ca}` | The matching `--*-dir` | Only for directories outside the project root (read-only) |

Host paths are translated automatically: anything inside the project root becomes a
`/work/...` path, anything outside gets its own bind mount. The mapping is printed before
the container starts — worth reading, because Docker Desktop's file-sharing allowlist fails
*silently*, presenting an empty directory rather than an error (OrbStack has no allowlist).

Unlike the backup wrapper, this one installs **no apt packages**: partition geometry comes
from `partx` and mounting from `mount`, both in `ubuntu:24.04`'s base image. It runs offline.

The QEMU-running check happens on the host side before Docker starts, since the container
can't see host processes. It compares the running VM's disk image against the one being
seeded, so a VM running a *different* image doesn't block the seed.

#### Workflow

Order matters — **seed after the build, not before**. Every Buildroot pass regenerates empty
`state.ext4`/`data.ext4` and overwrites `output/images/embraos.img`, so a seed applied before
a rebuild is silently discarded.

```bash
# 1. Build (Docker pass), which copies embraos.img out to output/images/
docker run --rm -v "$PWD":/work -v embraos-br-aarch64:/work/buildroot-src \
  -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools qemu-utils && \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"

# 2. Seed the freshly built image
./scripts/seed-state-mac.sh --ca-dir /path/to/dir-with-rootCA.pem

# 3. Boot
./scripts/run-qemu-aarch64.sh
# Confirm in-session: system_logs service=embrad   → names each accepted cert file
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

#### `sha256 verification failed for rust-1.98.1-aarch64-unknown-linux-musl.tar.xz`

**Cause:** Truncated/corrupt download, or a stale partial stage.

**Fix:** Remove the staged tree and re-run the Docker pass:

```bash
rm -rf vendor/rust-toolchain
```

The expected hash is the pin in `scripts/rust-toolchain.sha256`, and the script verifies
the download against it. If it persists, check network / proxy interception of
`static.rust-lang.org`. A `no pinned sha256 for …` error means `RUST_TOOLCHAIN_VERSION`
moved without its pins: take the `xz_hash` values from
`https://static.rust-lang.org/dist/channel-rust-<version>.toml` and add the three lines.

#### `ERROR: linux-6.18.60.tar.xz does not match its pin` / `no pinned sha256 for linux-…`

The kernel is pinned the same way (Step 4, before Buildroot runs, inside the Docker pass on a
Mac): `LINUX_KERNEL_VERSION` in `scripts/lib/image-common.sh`, the same version in both
defconfigs (`BR2_LINUX_KERNEL_CUSTOM_VERSION_VALUE`, with the headers series
`BR2_PACKAGE_HOST_LINUX_HEADERS_CUSTOM_<x_y>` of its first two digits), and the hash line in
`scripts/linux-kernel.sha256`, kernel.org's own from the signed `sha256sums.asc` of the
series. Buildroot checks no hash for a custom kernel version, so this check is the only one.
A mismatch removes the tarball from `buildroot-src/dl/linux/` (the named volume) and stops;
a missing line means the version moved without its pin. `KERNEL_MIRROR` overrides the
`cdn.kernel.org` base.

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
docker run --rm -e JOBS=2 -v "$PWD":/work \
  -v embraos-br-aarch64:/work/buildroot-src -w /work ubuntu:24.04 bash -c \
  "apt-get update && apt-get install -y build-essential gcc g++ \
   unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools qemu-utils && \
   (cd buildroot-src && make musl-dirclean) ; \
   FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
```

Completed host packages are stamped and skipped, so the resume is shorter than the first
attempt. `-j2` is slow but takes the toolchain through 4 GB; the image as a whole no
longer fits a 4 GB VM, since WebKit's heaviest compile alone takes 5 GiB (next entry).
`JOBS` defaults to all cores — only set it when the VM is smaller than the jobs want.

#### `Killed signal terminated program cc1plus` (OOM compiling WebKit)

**Symptom:** deep into WPE WebKit's build, one or more WebCore compiles die at once:

```
[4077/8277] Building CXX object Source/WebCore/CMakeFiles/WebCore.dir/__/__/WebCore/DerivedSources/unified-sources/UnifiedSource-3a52ce78-2.cpp.o
FAILED: [code=1] Source/WebCore/CMakeFiles/WebCore.dir/__/__/WebCore/DerivedSources/unified-sources/UnifiedSource-3a52ce78-2.cpp.o
aarch64-buildroot-linux-musl-g++.br_real: fatal error: Killed signal terminated program cc1plus
compilation terminated.
ninja: build stopped: subcommand failed.
make: *** [package/pkg-generic.mk:273: /work/buildroot-src/output/build/wpewebkit-2.50.5/.stamp_built] Error 1
```

**Cause:** the VM's OOM killer, not the compiler. A WebCore unified-source bundle
compiles in a 3 GiB process and the heaviest in 5 GiB (measured on the x86_64 build of
the same WebKit; the link takes 1.1 GiB), and Buildroot runs WebKit's ninja at
`nproc+1` jobs, 17 on a 16-core Mac: more than most VMs hold. Buildroot passes a
cmake package's ninja `-j$(PARALLEL_JOBS)`, which `BR2_JLEVEL=0` sets to `nproc+1`
whatever the top-level `-j` says, and ninja 1.13 ignores make 4.3's pipe-style
jobserver (`Ignoring jobserver: Pipe-based protocol is not supported`), so `JOBS`
reaches this phase only as `BR2_JLEVEL`, which the script passes since 2026-11-08; an
older checkout ran it at `nproc+1` whatever `JOBS` said.

**Fix:** give the VM about 3.5 GiB per job (`orb config set memory_mib 61440 && orb
restart` for 17 jobs on OrbStack; Docker Desktop: Settings, Resources) or cap the jobs
(`-e JOBS=4` in a 16 GiB VM), then rerun the same command. The build resumes inside
WebKit: ninja skips the finished objects, and nothing needs a dirclean.

#### `Too many open files` (EMFILE) — Buildroot tree on the bind mount

**Symptom:** the Buildroot pass dies with varying errors that include `Too many open
files` — often mid-kernel-compile, at a different point on each run. Adding
`--ulimit nofile=1048576:1048576` to `docker run` does not help.

**Cause:** host-side, not container-side. When the Buildroot tree lives on the
`-v "$PWD":/work` bind mount, every file it touches goes through the Docker runtime's
macOS-side file provider, which holds a file descriptor per accessed inode in a single
host process capped by macOS's `kern.maxfilesperproc` (~138k by default; measured on
OrbStack — Docker Desktop's VirtioFS has the same per-inode behavior). A cold Buildroot
build touches 300k+ inodes (the kernel source alone is ~87k files), so a single build
exhausts the pool, and the resulting EMFILE propagates into the container through the
file-sharing layer — no in-container `ulimit` can prevent it (the build script already
raises the container-side soft limit automatically; that guards a different, guest-side
failure mode).

**Fix:** build in the named volume — the default flow above
(`-v embraos-br-aarch64:/work/buildroot-src`) keeps the tree on the Docker VM's native
filesystem, bypassing the file provider entirely. Operator-verified 2026-07-19 on an
8 GB M2 that failed every bind-mount variant. Stopgap if you must build on the bind
mount: raise the host caps and restart the Docker runtime first —
`sudo sysctl kern.maxfiles=1048576 kern.maxfilesperproc=524288` (resets on Mac reboot).

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

**Symptom:** genimage fails formatting the EFI System Partition:

```
/bin/sh: 1: mkdosfs: not found
ERROR: vfat(esp.vfat): failed to generate esp.vfat
```

**Cause:** `dosfstools` isn't installed in the Docker container. Since the Phase 2
layout both defconfigs build Buildroot's own `mkdosfs`
(`BR2_PACKAGE_HOST_DOSFSTOOLS=y`), so this failure belongs to a Phase 1 tree.

**Fix:** Ensure `dosfstools` is in the `apt-get install` list (already in the Quick
Start command).

### Troubleshooting — backup & seeding (loop mounts in Docker)

#### `mount: … failed to setup loop device` / `device is busy` on a later run

**Symptom:** `seed-state-mac.sh` or `embraos-backup-mac.sh` fails to mount a partition,
or reports the image busy, after an earlier run was killed rather than exiting normally.

**Cause:** both wrappers mount with `mount -o loop`, which allocates the loop device with
autoclear — so it detaches on `umount`, and normal *and* failed exits are clean. A
`docker kill`, a Docker Desktop restart, or an OOM-kill is not: the loop device survives
in the Docker VM's kernel still pinning the image file. The container is gone, so nothing
on the Mac shows the leak.

**Fix:** list the VM's loop devices from a throwaway container and detach the specific
one holding your image.

```bash
docker run --rm --privileged ubuntu:24.04 losetup -a
docker run --rm --privileged ubuntu:24.04 losetup -d /dev/loopN   # the one from above
```

Do **not** use `losetup -D`. It detaches *every* loop device in the Docker VM, including
ones other containers are using — the VM's kernel is shared across all of them.

A leak is only ever a nuisance if you notice it. The failure mode worth avoiding is the
silent one: a second offset mount of the same byte range can *succeed*, giving two
independent ext4 mounts of one partition, which corrupts deterministically. That is also
why both wrappers refuse to run while a QEMU has the image open, and why the VM must be
stopped before seeding or restoring.

### Serial console (ttyAMA0) — no action needed

ARM64's `-machine virt` uses the PL011 UART (`/dev/ttyAMA0`), not 8250
(`/dev/ttyS0`); `post_image.sh` renders `console=ttyAMA0` into `grub.cfg`, and
`run-qemu-aarch64.sh`'s direct path passes it as well.

You will see `--device /dev/ttyS0` if you grep the source (`embrad`'s
`supervisor.rs` passes it to embra-console). **It is vestigial and harmless** —
embra-console's `run_tui(_device)` ignores that argument and renders the TUI
to its inherited `stdout`, which embrad wires to the real console (`ttyAMA0` on
aarch64, `ttyS0` on x86_64). The serial TUI (`EMBRA_TUI=1`) therefore attaches
correctly on Apple Silicon with **no code change** — verified on aarch64.

### End-to-End Validation

This is the sequence used to verify the 2026-07-19 run (see the status banner).
Re-run it as the regression checklist after any canonical-build bump:

1. **Boot — web console (default).** `./scripts/run-qemu-aarch64.sh`. Banner shows
   `UI mode: web console (default)` and `3345→3345 (HTTPS web)`. QEMU launches
   `qemu-system-aarch64 -machine virt -accel hvf -cpu host` with the edk2
   firmware; GRUB's menu shows on this terminal and boots slot A (`Image` from
   the slot's SquashFS, `console=ttyAMA0`) — no kernel panic on the PL011
   console (validated 2026-11-06; `EMBRA_BOOT=direct` boots the kernel from the
   host as before, no firmware and no menu).
2. **Config Wizard + Learning Mode** in the browser at the URL embraOS printed at
   boot (`https://localhost:3345/embraOS?token=…`; accept the embraOS-CA cert) — name, the provider gateway or direct, LLM provider
   + credentials, timezone; full soul-formation conversation; soul sealed.
3. **REST gateway:** from the host, `curl http://localhost:8443/health` → healthy; `curl http://localhost:8443/status` → `ok:true` with the `services` map.
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
