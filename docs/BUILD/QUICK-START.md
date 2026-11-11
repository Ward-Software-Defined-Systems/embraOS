# embraOS Quick Start

Build a QEMU-bootable embraOS disk image from source and boot it. The project landing page is [../../README.md](../../README.md).

> **Default UI.** The browser-based **embra-web** console is the default UI, served
> over HTTPS at **https://localhost:3345/embraOS** — open it from the URL embraOS
> prints at boot, which carries this boot's access token (`?token=…`; accept the
> embraOS-CA cert on first visit). The token changes at every boot and is shown
> nowhere else; the first visit sets a cookie and the address bar loses it, so a
> phone or another workstation needs the URL with the token once. `/` and every
> other path answer 404; a tab left open across a reboot gets 401 until it is
> opened from the new URL. It wraps the same Phase 1 conversational TUI in xterm.js over a
> PTY→WebSocket bridge. Set **`EMBRA_TUI=1`** before `run-qemu.sh` to boot the
> serial TUI instead — no image rebuild needed. The serial TUI is functional but is
> preferred for debugging; the web console is the primary interface.

## Phase 1 — Build from Source (QEMU Bootable Image)

Phase 1 builds a QEMU-bootable x86_64 disk image with an immutable SquashFS rootfs, service supervision, and soul verification at boot.

> **Apple Silicon (aarch64) hosts:** follow [AARCH64-BUILD.md](AARCH64-BUILD.md) instead — the Buildroot tree is arch-parameterized but the Apple-Silicon build runs through `scripts/build-image-aarch64.sh`.
>
> **Intel Mac hosts:** follow [INTEL-MAC-BUILD.md](INTEL-MAC-BUILD.md).

### Ubuntu 24.04 / 26.04 (Recommended — Full Build Pipeline)

```bash
# Install dependencies
# clang + libclang-dev are required by bindgen (pulled in by the in-tree
# wardsondb crate's rocksdb → zstd-sys dep chain) to parse C headers at
# build time.
# libcrypt-dev provides crypt.h for Buildroot's host-mkpasswd build —
# Ubuntu 26.04 split crypt.h out of glibc into the standalone libxcrypt.
# xz-utils unpacks the pinned in-OS Rust toolchain that build-image.sh
# Step 3.5 bakes into the image (the Guardian dynamic-tool substrate).
sudo apt-get update && sudo apt-get install -y \
  build-essential gcc g++ unzip xz-utils bc cpio rsync wget curl python3 file git \
  protobuf-compiler musl-tools clang libclang-dev \
  qemu-system-x86 ovmf libcrypt-dev libelf-dev libssl-dev genext2fs

# Install musl cross-toolchain (gcc+g++ with a matching musl libstdc++).
# Ubuntu's musl-tools only wraps the host gcc and drags in a glibc-linked
# libstdc++ — which won't link against musl. WardSONDB's rocksdb backend is
# C++, so we need a self-contained musl toolchain from musl.cc.
cd /tmp
curl -LO https://musl.cc/x86_64-linux-musl-cross.tgz
sudo tar -xzf x86_64-linux-musl-cross.tgz -C /opt
# Put the toolchain on PATH for ad-hoc cargo builds (build-image.sh also
# auto-detects /opt/x86_64-linux-musl-cross even if PATH isn't set).
echo 'export PATH="/opt/x86_64-linux-musl-cross/bin:$PATH"' >> ~/.bashrc
source ~/.bashrc
x86_64-linux-musl-gcc --version
x86_64-linux-musl-g++ --version

# Install Rust. The checkout pins its compiler in rust-toolchain.toml
# (1.98.1, with clippy, rustfmt, and the wasm32 + musl targets); rustup
# installs it on the first cargo run inside the repository.
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh
source ~/.cargo/env
rustup target add x86_64-unknown-linux-musl

# embra-web frontend (Leptos/WASM) — build-image.sh Step 0.5 builds it
# with Trunk and aborts if trunk is missing.
rustup target add wasm32-unknown-unknown
cargo install trunk --locked          # CI pins Trunk 0.21.14 — add --version 0.21.14 to match
```

```bash
# Clone and configure — use ~/projects so source + build artifacts persist
# across reboots (the /tmp toolchain step above is fine because /opt persists).
mkdir -p ~/projects && cd ~/projects
git clone https://github.com/Ward-Software-Defined-Systems/embraOS.git
cd ~/projects/embraOS

# Configure musl linker (per-machine, only needed once)
cat >> ~/.cargo/config.toml << 'EOF'
[target.x86_64-unknown-linux-musl]
linker = "x86_64-linux-musl-gcc"
EOF
```

```bash
# Build and run — pick a storage engine: rocksdb (battle-tested) or fjall (pure Rust)
./scripts/build-image.sh --storage-engine rocksdb   # Full pipeline: Rust → initramfs → Buildroot → disk image
                                                    # Leaves output/images/embraos.img (QEMU) and the same disk as
                                                    # embraos.vmdk (Parallels, Fusion, VirtualBox), embraos.vhdx
                                                    # (Hyper-V gen 2, Secure Boot off), embraos.ova (ESXi 8.0+, vCenter:
                                                    # hardware version 20, guest "Other 6.x or later Linux (64-bit)")
                                                    # — qemu-img (qemu-utils) converts them; the OVA boots on ESXi 8.0 U2
                                                    # as imported, the VHDX on Hyper-V gen 2, the VMDK is TBD until booted there —
                                                    # and embraos.iso, the UEFI installer for bare metal and the hypervisors
                                                    # (a DVD, or a USB stick written with dd; the aarch64 build writes
                                                    # the same medium for its architecture, TBD until booted)

# Default UI is the embra-web console — open the URL embraOS prints at boot (https://localhost:3345/embraOS?token=…)
./scripts/run-qemu.sh                                # Boot in QEMU — OVMF, GRUB's menu (3 s), slot A, web console (default)
EMBRA_BOOT=direct ./scripts/run-qemu.sh              # Kernel and initramfs from the host: no firmware, no menu

# Or fall back to the serial TUI on this terminal (no rebuild needed; functional, preferred for debugging)
EMBRA_TUI=1 ./scripts/run-qemu.sh                    # Boot in QEMU — serial TUI

# Optional: per-request WardSONDB log lines (default: lifecycle/warn/error only)
EMBRA_DB_VERBOSE=1 ./scripts/run-qemu.sh             # embra.dbverbose=1 → wardsondb --verbose
EMBRA_LOG_LEVEL=info,kg::traversal=debug ./scripts/run-qemu.sh  # embra.loglevel=… → brain EMBRA_LOG
                                                     # (read back in-session via system_logs; no spaces)
EMBRA_TUI=1 EMBRA_GRAPHICS=sixel ./scripts/run-qemu.sh  # serial TUI image pane: halfblocks (default) |
                                                     # sixel | kitty | iterm2 | auto | off → embra.graphics=…
                                                     # (the web console's PTY is always sixel)

# Host-side extras (nothing reaches the OS)
EMBRA_VNC=1 ./scripts/run-qemu.sh                    # the display on a VNC server at 127.0.0.1:5900, password embraos, with
                                                     # a USB tablet and the keyboard: the display console runs there
                                                     # (loopback only: ssh -L 5901:127.0.0.1:5900 <host>, then localhost:5901)
EMBRA_KIOSK=0 ./scripts/run-qemu.sh                  # the display without its console (embra.kiosk=0): boot log and banner only
EMBRA_KBD=de ./scripts/run-qemu.sh                   # the display console's keyboard layout (embra.kbd=…; us)
EMBRA_KIOSK_MODE=1280x800 ./scripts/run-qemu.sh      # the display console's video mode (embra.kioskmode=…; 1920x1080 where the
                                                     # display offers it; native = the display's own)
EMBRA_ISO=output/images/embraos.iso ./scripts/run-qemu.sh disk.qcow2   # boot the installer onto a blank qemu-img disk
EMBRA_ISO_AS=usb EMBRA_DISK_IF=ahci ...              # the medium as a USB stick, the target on AHCI (cdrom and virtio by default)
EMBRA_DHCP_START=10.0.2.200 ./scripts/run-qemu.sh    # QEMU's DHCP leases this address: the banner's second URL
```

> **The disk boots itself:** the image is a 1 MiB-aligned GPT with the EFI System Partition (GRUB, `grub.cfg`, `grubenv`), two OS slots (`rootfs_a`, `rootfs_b`; each a SquashFS with its own kernel and initramfs under `/boot` — slot B is empty until the first OS update), STATE and DATA (the sizes and names in `buildroot/board/embraos/layout.sh`). `run-qemu.sh` boots it through UEFI by default: the `ovmf` package above provides the firmware (`EMBRA_FW_CODE`/`EMBRA_FW_VARS` name another pair; the variable store is copied per run), GRUB's menu shows on this terminal for three seconds and boots slot A, and the `EMBRA_*` knobs below reach the kernel line as one SMBIOS string the menu appends, so nothing is typed. The menu also offers Diagnostics (the serial TUI, debug logging, database request logging, make a slot the default, boot a slot once, the boot state, the command-line knobs — press `e` on an entry, add a knob at the end of its first line, after the title, and boot with Ctrl-x; `embra.tui=1` is the in-OS spelling of `EMBRA_TUI=1`), Update embraOS, Backup and restore, Reboot, Halt and Firmware setup. `EMBRA_BOOT=direct` keeps the old path — the kernel and the initramfs from the host, no firmware and no menu — for a kernel or embra-init change without an image rebuild.

> **Network and display:** at boot embraOS asks for a DHCP lease on `eth0` (BusyBox `udhcpc`; the lease's address, route and resolvers apply, and the client keeps renewing) and falls back to the static QEMU user-network address `10.0.2.15/24` when none arrives within 10 s — under `run-qemu.sh` QEMU's own DHCP server leases that same address, so nothing changes for you. On a bridged network or a real host the banner prints a second console URL on the leased address beside the `localhost` one (the certificate names `localhost` and `10.0.2.15`, so the browser warns about the name once there). To see it in QEMU: `EMBRA_DHCP_START=10.0.2.200 ./scripts/run-qemu.sh` leases `10.0.2.200`. The kernel line names the display (`console=tty0`) before the serial line, and every boot message of embra-init and embrad goes to both, so a hypervisor with a display shows GRUB's menu, the boot log, the banner with the console URL and the reason a boot stopped there too (once the web console or the serial TUI is up embrad logs to a file; the serial TUI itself stays on the serial line); `run-qemu.sh` keeps the serial line on this terminal, and `EMBRA_VNC=1` adds the display on a VNC server at `127.0.0.1:5900` (password `embraos`, loopback only, so tunnel it with `ssh -L`; macOS Screen Sharing needs the password). On a machine with a display the display is a console of its own: once embra-web is up, embraOS renders the web console there (WPE WebKit through cog's DRM platform, drawn in software; keyboard and mouse through libinput) — the same console a browser reaches at the URL, one session shared under the same writer-and-observers rule, the display the first writer at boot; `embra.kiosk=0` on the kernel line (`EMBRA_KIOSK=0`, or GRUB's Diagnostics) boots without it. On the display the right mouse button opens the console's own menu, Copy and Paste: the clipboard is the display's own, so what is copied there is pasted there (the terminal's selection, or the page's; into the terminal while the display is the writer, or into a text field of the page), and a browser on another machine keeps its own menu and clipboard. The menu's last entry, This console's URL, shows the banner's URL with this boot's access token again, for a browser on another machine: the display console covers the banner's copy, and the web console gives the URL to the machine's own display and to no other browser. The menu and its three entries are validated under QEMU on virtio-gpu in a scratch image (2026-11-10), and the menu in the operator's runs under QEMU on an x86_64 Linux desktop, on an Intel Mac and on Apple Silicon (2026-11-10). The display console asks for 1920x1080 wherever the display offers it, the width that holds the status row on one line: GRUB puts `video=efifb:1920x1080` on the kernel line for the firmware's framebuffer, and embrad names the mode to cog for a display with a driver of its own (QEMU's VGA and virtio-gpu, Hyper-V). `embra.kioskmode=<width>x<height>` (`EMBRA_KIOSK_MODE=`) asks for another mode and `embra.kioskmode=native` for the display's own; a mode the display does not offer leaves the display's own. The mode is set once, at boot: a VM window resized later does not change it. Validated under QEMU, on ESXi 8.0 U2 and on Hyper-V generation 2 (2026-11-09): the page, the keyboard, the click and the 1920x1080 mode on each. On aarch64 it is validated under QEMU on Apple Silicon on a virtio-gpu display (2026-11-10: the page, the keyboard, the pointer, a click and the 1920x1080 mode; the command is in [AARCH64-BUILD.md](AARCH64-BUILD.md)). In QEMU's window on an Intel Mac it is validated on a virtio-gpu display with the same five (2026-11-10; the command is in [INTEL-MAC-BUILD.md](INTEL-MAC-BUILD.md)); the mode there needs an image from `1ba8df5` on: a QEMU window reports its size to the guest, the kernel's console then lists only the modes its own framebuffer holds, and embrad now asks the kernel to detect the displays again before it gives up on a mode. On Apple Silicon it is validated on the firmware's framebuffer as well (QEMU's `ramfb`, 2026-11-10: the page, the keyboard, the pointer and a click, at 800x600). QEMU's `ramfb` comes up at 800x600 on either architecture: edk2's driver for it offers 640x480, 800x600 and 1024x768 and starts in the second, so the request for 1920x1080 leaves it there, and `embra.kioskmode=1024x768` asks for its largest (validated on x86_64 in a scratch run, 2026-11-10). `embra.kioskmode=` is validated through GRUB's Diagnostics menu (2026-11-09), and under QEMU on the standard VGA, virtio-gpu and a firmware framebuffer in a scratch image. The click and Hyper-V's display rest on the third and fourth patches carried on cog (`buildroot/patches/cog/`, 2026-11-09; [OPEN-PROBLEMS.md](../OPEN-PROBLEMS.md)): an image built before them presses no tab or button from the display and shows no display console on Hyper-V. **TBD** on Parallels and bare metal until run there. The image carries the drivers other hypervisors present — AHCI, IDE, NVMe, virtio-scsi, e1000/e1000e, vmxnet3, pvscsi (the OVA's controller; ESXi 8.0 U2 boots from it, QEMU's OVMF cannot), VMware's LSI Logic Parallel and SAS controllers (on ESXi a disk installed from the ISO boots behind either one) — and Hyper-V's synthetic devices (a generation 2 VM with Secure Boot off boots from the VHDX, takes its lease and serves the console). For bare metal it carries the wired NICs of servers and desktops with their USB cousins, the RAID and SAS controllers, SD/eMMC, NVMe behind Intel's VMD, laptop touchpads and touch panels, the TPM and the temperature sensors, every driver built in ([SYSTEM-DESIGN.md](../SYSTEM-DESIGN.md) lists them by group), with every linux-firmware family on the rootfs and the CPU microcode loaded early; the bare-metal run itself is **TBD** until one happens.

> **Install on bare metal:** `output/images/embraos.iso`, or a release's `embraos-<tag>-x86_64-<engine>.iso` from GitHub Releases (checked with its `.sha256` line and the signature beside it, [UPDATES-AND-BACKUPS.md](../OPERATIONS/UPDATES-AND-BACKUPS.md)), boots on UEFI firmware (Secure Boot off) from a DVD or a USB stick (`dd if=output/images/embraos.iso of=/dev/sdX bs=4M status=progress` and `sync`). Its menu installs on both consoles, the serial line or the display alone; the installer lists the machine's disks, asks for a number and then the disk's name, and writes the image's layout to it — the EFI System Partition, slot A with this build, an empty slot B, STATE, and DATA to the end of the disk — then asks for the medium to be removed and reboots into GRUB's menu from the new disk. Everything on the chosen disk is erased; the other disks are not touched; the firmware boots the ESP's removable-media path, so no NVRAM entry is written. A disk that grows later (a VM's virtual disk enlarged in the hypervisor) is taken up at the next boot: when a gibibyte or more follows DATA, embrad grows the partition and the filesystem to the disk's end before the services start, and says so on both consoles. Validated under QEMU (OVMF) as a DVD onto a virtio disk and as a USB stick onto an AHCI disk, each installed disk booting on its own to the web console, on ESXi 8.0 U2, where the installed disk boots behind VMware's LSI Logic SAS and LSI Logic Parallel controllers as well, and on Hyper-V generation 2, where the installed disk boots to the display console (2026-11-09); a second install over an installed disk is validated too. With the stick attached as UAS (QEMU's `usb-uas`, on xHCI and behind EHCI) the installer's own run is validated from the medium's kernel on (2026-11-10): the medium found, the image written and verified, the installed disk booting on its own. That kernel was started through its EFI stub: OVMF 2024.02 has no UAS driver and QEMU's device no bulk-only mode, so the firmware cannot read such a stick; a UAS stick booted by real firmware is **TBD** with bare metal. Parallels and bare metal are **TBD** until run. The aarch64 build writes the same medium for its architecture ([AARCH64-BUILD.md](AARCH64-BUILD.md)), **TBD** until booted there.

> **Storage engine:** The `--storage-engine` flag is required and is baked into the embrad binary at build time. WardSONDB locks the choice into the DATA partition on first boot via a `.engine` marker file — switching engines later requires wiping DATA.

> **A Buildroot tree from before 2026-11-08:** Buildroot never rebuilds a package whose options changed. On a tree that built the image before the display console, run `make util-linux-libs-dirclean util-linux-dirclean mesa3d-dirclean eudev-dirclean` in `buildroot-src` once (e2fsprogs needs the libuuid that the older util-linux build left out; Mesa and eudev take their new options), then build as usual. A fresh tree needs nothing. A patch added or changed under `buildroot/patches/` needs no step either: the image scripts clean its package before `make`.

> **Buildroot version:** Defaults to `2026.02.1` (LTS, designed for Ubuntu 26.04 era). Override with `BUILDROOT_VERSION=2024.02 ./scripts/build-image.sh ...` if you need to fall back on an older host.

> **Linux kernel:** The image runs a pinned kernel from the 6.18 longterm line (`6.18.60`, supported upstream to the end of 2028), not Buildroot's own "latest": Buildroot 2026.02's latest is the 6.19 series, end of life since 2026-04-22 and never fixed for CVE-2026-98432. The version is named in three places that the scripts check against each other — `LINUX_KERNEL_VERSION` in `scripts/lib/image-common.sh`, `BR2_LINUX_KERNEL_CUSTOM_VERSION_VALUE` in both defconfigs (with the headers series `BR2_PACKAGE_HOST_LINUX_HEADERS_CUSTOM_6_18`, which follows the first two digits), and the hash line in `scripts/linux-kernel.sha256`. Buildroot checks no hash for a custom kernel version, so Step 4 downloads `linux-<version>.tar.xz` from `cdn.kernel.org` into `buildroot-src/dl/linux/` and verifies it against the pinned line (kernel.org's own, from the signed `sha256sums.asc`) before Buildroot unpacks it; a tarball that does not match is removed and the build stops. To move the kernel: set the three places, take the new line from `https://cdn.kernel.org/pub/linux/kernel/v6.x/sha256sums.asc`, rebuild both architectures. `KERNEL_MIRROR` overrides the download base. The QEMU scripts boot with `quiet`, which keeps the kernel's own `Linux version` banner off the serial console; the running kernel is named by `system_status` (`kernel`), `/status` and `uptime_report` (`Kernel:`).

> **In-OS Rust toolchain:** Guardian dynamic tools compile inside the image, so `build-image.sh` Step 3.5 downloads a pinned toolchain (musl host + `wasm32` std, SHA-256-verified against the pins in `scripts/rust-toolchain.sha256`) from `static.rust-lang.org`, caches it under `vendor/rust-toolchain`, and bakes it into the rootfs at `/opt/rust`. The first build needs network for this. The toolchain is the largest item in the image: about 320 MiB of a root filesystem of about 813 MiB, as compressed in the SquashFS (the firmware for bare metal is about 270 MiB of that and the display console's stack about 90 MiB) (x86_64, measured 2026-11-08; the target directory is repopulated on every build, and a stale one had inflated the figure to 491 MiB). The pin is `1.98.1` — the same version as the host compiler in `rust-toolchain.toml`, but a separate setting (the pins, the download check and the post-Buildroot checks of the root filesystem live in `scripts/lib/image-common.sh`, which both image scripts source); override it with `RUST_TOOLCHAIN_VERSION=... ./scripts/build-image.sh ...`. The brain loads a Guardian tool only when it was built by the toolchain the image carries, so an image with a different pin leaves the existing tools unloaded (`/guardian list` marks them `NOT LOADED`) until the operator runs `/guardian rebuild --all`.

> **Two tools ship ready:** the image carries the prompt-injection-hardened `web_search` Guardian tool and `http_request`, a guarded curl for pages, resources and API work, and the brain builds both at boot. `web_search` stays inert until you set a Brave Search key with `/guardian key brave <token>` ([GUARDIAN-ADVANCED-EXAMPLE.md](../GUARDIAN/GUARDIAN-ADVANCED-EXAMPLE.md)). `http_request` needs no key: a credential for a host goes in with `/guardian secret <host> <header> <value>` and is injected host-side, and a private host is admitted with `/guardian egress allow <host>` ([GUARDIAN-HTTP-REQUEST-EXAMPLE.md](../GUARDIAN/GUARDIAN-HTTP-REQUEST-EXAMPLE.md)).

> **Embedding model:** Semantic knowledge-graph retrieval runs in-process, so `build-image.sh` **Step 3.6** downloads `BAAI/bge-small-en-v1.5` (~133 MB, SHA-256-verified against a pin), caches it under `vendor/embedding-model`, and bakes it into the rootfs at `/usr/share/embra/models/`. Like the toolchain above, the first build needs network for it; later builds skip the download when the cached copy still matches the pin. The build **fails** if the model is missing from the rootfs rather than shipping an image whose retrieval silently falls back to keyword-only. Operators can override the baked copy per instance by seeding `/embra/state/models/<name>` on STATE.

> **Lockfiles:** The image scripts build with `--locked` (`trunk build` in Step 0.5, `cargo build` in Step 1), so an image is always built from the committed `Cargo.lock` and `crates/embra-web-ui/Cargo.lock`. After a dependency change, update and commit the lockfile first — a lock that no longer matches its manifest stops the build instead of being re-resolved.

> **Build overrides:** `build-image.sh` reads a few environment knobs — `JOBS` (Buildroot parallelism, default all cores: the make-based packages take it through make's jobserver, the cmake ones, WebKit and LLVM, as `BR2_JLEVEL`; lower it on a memory-constrained host, since WebKit's compile wants about 3.5 GiB per job, the figures in [AARCH64-BUILD.md](AARCH64-BUILD.md)), `MUSL_CROSS` (musl toolchain root, default `/opt/x86_64-linux-musl-cross`), `BUILDROOT_DIR` (default `buildroot-src`), the `BUILDROOT_VERSION`, `LINUX_KERNEL_VERSION` and `RUST_TOOLCHAIN_VERSION` pins above, and `RUST_DIST_BASE` / `KERNEL_MIRROR` / `EMBED_BASE` (mirror bases for the Step 3.5 toolchain and Step 3.6 model downloads). Prefix the command: `JOBS=4 ./scripts/build-image.sh --storage-engine rocksdb`.

On first boot, the Config Wizard runs — name your intelligence, say whether the providers are reached directly or through WSDS Local AI Gateway (its unified listener must identify itself; [OPERATIONS/LOCAL-AI-GATEWAY.md](../OPERATIONS/LOCAL-AI-GATEWAY.md)), choose your LLM provider (Anthropic Claude, Google Gemini, Ollama, or LM Studio), enter the corresponding credentials (API key for Anthropic/Gemini; endpoint URL + optional bearer + selected model for the OpenAI-compat presets), set your timezone. Each field is validated before commit — an invalid API key, unreachable endpoint, or garbage timezone re-prompts instead of persisting. The Ollama / LM Studio sub-flow probes `GET /v1/models` against your endpoint, or through the gateway's `/ollama` or `/lmstudio` prefix when one was given (the endpoint may then be `none`), and presents a model selector populated from the live server response. After setup, you're in a full TUI conversation with styled text, thinking indicators, and tool execution.

> **After the soul is sealed, run `/embeddings backfill` once.** The seed knowledge packs load at first boot, before the Config Wizard has written a configuration, so their nodes carry no vectors and semantic similarity cannot find them. Nothing embeds them later on its own. The backfill runs locally (about 55 ms per node) and `/embeddings` shows the coverage before and after.

### Notes

The following apply once the image is built. They are not part of the build pipeline.

> **Terminal Size (serial TUI only):** With `EMBRA_TUI=1`, `run-qemu.sh` reads your terminal size once (`stty size`) and passes it to the guest on the kernel command line (`embra.cols`/`embra.rows`) — maximize the terminal before launching; resizing it afterwards does not update the TUI layout. The web console has no such limit: it follows the browser window.

> **Image Search Order:** `run-qemu.sh` (and `run-qemu-aarch64.sh`) resolves the disk image in this order — the same precedence `seed-state.sh` and `embraos-backup.sh` use, so the image you seed or back up is the one you boot:
> 1. Explicit path passed as the argument: `./scripts/run-qemu.sh /path/to/embraos.img`
> 2. `$EMBRAOS_IMAGE` — the escape hatch when a stale `buildroot-src/` shadows a newer image
> 3. `buildroot-src/output/images/embraos.img` (Buildroot output, always freshest)
> 4. `output/images/embraos.img` (alternative output location)
>
> On the direct path (`EMBRA_BOOT=direct`) the kernel (`bzImage`) is looked for beside the image first, then in the two default locations, and the initramfs is always `./initramfs.cpio.gz`; the UEFI path loads both from the slot inside the image. The default paths are relative to the current directory — run the script from the repository root.

> **Clean First Boot:** To reset and trigger the Config Wizard again (e.g., to change API key):
> ```bash
> ./scripts/seed-state.sh --wipe state,data       # macOS: ./scripts/seed-state-mac.sh
> ```
> Reformats the named partitions in place as ext4 with their original labels, after a
> typed confirmation. Wiping STATE destroys the soul hash, PKI and API keys; wiping DATA
> destroys WardSONDB — all memory, sessions, the workspace and the in-OS backups under
> `/embra/data/backups`. Neither is reversible, and
> the VM must be stopped. `--wipe data` alone also removes STATE's `soul.sha256`: a sealed
> hash with no soul document halts the next boot (the tamper check), and a wiped DATA is
> meant to start over.
>
> Use `--wipe state` or `--wipe data` to reset just one (`--wipe all` = `state,data`), `--yes` to skip the prompt, and
> combine with the seeding flags to reset and re-seed in a single pass:
> ```bash
> ./scripts/seed-state.sh --wipe state,data --ca-dir /path/to/dir-with-rootCA.pem
> ```
> Partition geometry is read from the GPT at run time, so this stays correct as partitions
> shift between builds.

> **Halted boot:** embrad (PID 1) never exits. A failed soul verification, a fatal boot error (a partition that does not mount, a service that cannot start) or a panic in embrad halts the machine and writes the reason to `/embra/state/halt_reason` on STATE. After the next boot, ask the intelligence to read `/embra/state/halt_reason` (`file_read` reaches that file; the rest of STATE is refused to the tools, except the operator's drop-in directories). When the machine does not boot at all, stop the VM and take a backup — `sudo ./scripts/embraos-backup.sh backup --label halted` — and read `STATE/halt_reason` in it under `~/embraOS_BACKUPS/`.

> **Port Forwarding:** QEMU forwards 50000 (gRPC) and 8443 (REST) on the host's loopback only, and 3345 (the HTTPS web console — https://localhost:3345/embraOS, behind this boot's access token) on every host interface, in both UI modes; only the launch banner differs. Other machines on the host's network reach the web console and nothing else, and only with the URL embraOS printed at boot; the per-port table is in [SYSTEM-DESIGN.md](../SYSTEM-DESIGN.md#network-exposure). apid's REST surface is `/health`, `/version` and `/status` — `/status` proxies the brain's `GetSystemStatus` (2-second timeout, HTTP 503 while the brain is away) and carries the active LLM provider's last endpoint probe as `llm-provider` / `llm-provider.detail`. Test with:
> ```bash
> curl http://localhost:8443/health
> curl http://localhost:8443/status
> ```

> **OS updates:** the image carries two OS slots; `/update` fetches a release's signed manifest from the GitHub releases, `/update apply` writes the release into the inactive slot and GRUB tries it at the next boot, falling back if it does not come up ([OPERATIONS/UPDATES-AND-BACKUPS.md](../OPERATIONS/UPDATES-AND-BACKUPS.md)). `scripts/release-update.sh` prepares and signs a release's artifacts; `scripts/update-signing.pub` holds the keys the image trusts (the operator's first release signing key since `98a6573`; several keys may sit there across a rotation).

> **Backup & Restore:** the OS takes its own backups too — `/backup create` in a session or the boot menu's Backup and restore, snapshots under `/embra/data/backups` that the web console streams to the browser ([OPERATIONS/UPDATES-AND-BACKUPS.md](../OPERATIONS/UPDATES-AND-BACKUPS.md)). The host script below is the path across an image rebuild and off the machine; its copy of DATA leaves `backups/` out. `scripts/embraos-backup.sh` preserves STATE and DATA partitions across image rebuilds. This is a file-level backup — WardSONDB does not need to be running. The VM must be stopped.
> ```bash
> # Before rebuilding the image
> sudo ./scripts/embraos-backup.sh backup --label pre-rebuild   # or a bare label: backup pre-rebuild
>
> # After rebuilding — latest backup, or restore <name> for a specific one
> sudo ./scripts/embraos-backup.sh restore
>
> # List available backups
> ./scripts/embraos-backup.sh list
>
> # Verify disk image has valid data
> sudo ./scripts/embraos-backup.sh verify
>
> # Target a specific image (default: the one run-qemu.sh boots)
> sudo ./scripts/embraos-backup.sh --image /path/to/embraos.img verify
> ```
> Backups are stored in `~/embraOS_BACKUPS/` by default — under `sudo` that is still the invoking user's home, resolved from `$SUDO_USER` (override with `EMBRAOS_BACKUP_DIR`). `restore` prints the backup's metadata, asks `Continue? [y/N]`, then empties the image's STATE and DATA partitions before copying the backup in — a fresh image's seed contents are replaced, not merged. Each backup includes STATE (soul hash, PKI certs), DATA (WardSONDB collections, workspace), and metadata with the SHA-256 of the source image and the database engine (`data_engine`). The summary after the copy, and `verify`, describe the database by its `.engine` marker — fjall's collections by name, RocksDB's SST files — and assume neither engine. To target another image pass `--image <path>`; `EMBRAOS_IMAGE` works too, but it must come *after* `sudo` (`sudo EMBRAOS_IMAGE=… ./scripts/embraos-backup.sh …`) — sudo's default `env_reset` drops a variable set before it, and the script would silently back up the default image.
>
> **After restoring onto a rebuilt image:** Guardian dynamic tools are compiled inside the OS, and the brain loads one only if it was built by the toolchain the image carries. When a new image has a different in-OS toolchain, the tools come back `NOT LOADED` (`/guardian list`). `/guardian rebuild --all` builds them again from their stored modules; each passes validation and the replicant check before it compiles.

---

## Self-hosted git servers (private CA)

To let the git tools reach a self-hosted GitLab/Gitea whose HTTPS certificate chains to a private root CA (e.g. an mkcert development CA), drop the CA's `*.pem`/`*.crt` file(s) into `/embra/state/ca-certificates/` — on a disk image, via:

```bash
./scripts/seed-state.sh --ca-dir /path/to/dir-with-rootCA.pem
# macOS (no losetup/ext4 — runs the same script in a privileged container):
./scripts/seed-state-mac.sh --ca-dir /path/to/dir-with-rootCA.pem
```

The same drop-ins are trusted by every HTTP client of the brain beside the compiled-in roots — the GitLab tools, the Guardian transport of `http_request`, and the providers while `/local-ai-gateway` routes them through an HTTPS gateway with its own certificate authority ([OPERATIONS/LOCAL-AI-GATEWAY.md](../OPERATIONS/LOCAL-AI-GATEWAY.md)) — read once at the first client a boot builds, so a seeded certificate takes effect at the next boot.

With no image argument, `seed-state.sh` resolves the same one `run-qemu.sh` boots
(`$EMBRAOS_IMAGE`, then `buildroot-src/output/images/`, then `output/images/`), so you
cannot seed one image and boot another. Pass a path — positional or `--image <path>` — to
override. The VM must be stopped — seeding an image QEMU has open corrupts it, and the
script refuses. `./scripts/seed-state.sh --help` lists every flag. Beyond `--ca-dir`,
`--seed-dir` (your own knowledge packs — the OS's are in the image, and a copy in STATE is ignored), `--import-dir` (intelligence graphs) and `--wipe`:
`--phase0-data <dir>` copies `<dir>/wardsondb/` onto DATA (size-checked first; its `.engine`
marker must match the image's `--storage-engine`), `--soul-hash <hash>` writes STATE's
`soul.sha256`, and `EMBRAOS_ROOT` re-anchors the default image paths when the script runs
from another directory.

At the next boot, embrad merges the drop-ins with the stock CA bundle (public hosts like github.com keep working) and exports `GIT_SSL_CAINFO`/`SSL_CERT_FILE` for every service, so `git_clone`/`git_push`/`git_pull` trust the server. The boot log line (readable in-session via the `system_logs` tool, service `embrad`) names each accepted cert file. Scope: this covers the git/OpenSSL path, the `gl_*` GitLab API tools and the Guardian tools' HTTP transport (`http_get`, `http_request`), whose HTTP clients add the same drop-ins as trust anchors — the provider clients keep their compiled-in public roots. For private repos, set a per-host token with `/git-token <host> <token>` after boot; the same token authenticates the `gl_*` issue/merge-request tools.

---

The day-to-day session model, slash commands, and keyboard shortcuts live in [OPERATION.md](../OPERATIONS/OPERATION.md) and [COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md). GitHub and SSH setup are slash commands run from the conversational TUI after boot — see [COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md) (`/github-token`, `/git-token`, `/ssh-keygen`, `/ssh-copy-id`, `/git-setup`).
