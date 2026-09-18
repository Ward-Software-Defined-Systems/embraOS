# embraOS Quick Start

Build a QEMU-bootable embraOS disk image from source and boot it. The project landing page is [../README.md](../README.md).

> **Default UI.** The browser-based **embra-web** console is the default UI, served
> over HTTPS at **https://localhost:3345/embraOS** (accept the embraOS-CA cert on
> first visit). It wraps the same Phase 1 conversational TUI in xterm.js over a
> PTY→WebSocket bridge. Set **`EMBRA_TUI=1`** before `run-qemu.sh` to boot the
> serial TUI instead — no image rebuild needed.

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
  qemu-system-x86 libcrypt-dev libelf-dev libssl-dev genext2fs

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

# Install Rust
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

# Default UI is the embra-web console — https://localhost:3345/embraOS
./scripts/run-qemu.sh                                # Boot in QEMU — web console (default)

# Or fall back to the serial TUI on this terminal (no rebuild needed)
EMBRA_TUI=1 ./scripts/run-qemu.sh                    # Boot in QEMU — serial TUI

# Optional: per-request WardSONDB log lines (default: lifecycle/warn/error only)
EMBRA_DB_VERBOSE=1 ./scripts/run-qemu.sh             # embra.dbverbose=1 → wardsondb --verbose
EMBRA_LOG_LEVEL=info,kg::traversal=debug ./scripts/run-qemu.sh  # embra.loglevel=… → brain EMBRA_LOG
                                                     # (read back in-session via system_logs; no spaces)
EMBRA_TUI=1 EMBRA_GRAPHICS=sixel ./scripts/run-qemu.sh  # serial TUI image pane: halfblocks (default) |
                                                     # sixel | kitty | iterm2 | auto | off → embra.graphics=…
                                                     # (the web console's PTY is always sixel)
```

> **Storage engine:** The `--storage-engine` flag is required and is baked into the embrad binary at build time. WardSONDB locks the choice into the DATA partition on first boot via a `.engine` marker file — switching engines later requires wiping DATA.

> **Buildroot version:** Defaults to `2026.02.1` (LTS, designed for Ubuntu 26.04 era). Override with `BUILDROOT_VERSION=2024.02 ./scripts/build-image.sh ...` if you need to fall back on an older host.

> **In-OS Rust toolchain:** Guardian dynamic tools compile inside the image, so `build-image.sh` Step 3.5 downloads a pinned toolchain (musl host + `wasm32` std, SHA-256-verified) from `static.rust-lang.org`, caches it under `vendor/rust-toolchain`, and bakes it into the rootfs at `/opt/rust`. The first build needs network for this and adds ~100 MB to the image. The pin is `1.94.1` (the toolchain CI builds with); override it with `RUST_TOOLCHAIN_VERSION=... ./scripts/build-image.sh ...`.

> **Embedding model:** Semantic knowledge-graph retrieval runs in-process, so `build-image.sh` **Step 3.6** downloads `BAAI/bge-small-en-v1.5` (~133 MB, SHA-256-verified against a pin), caches it under `vendor/embedding-model`, and bakes it into the rootfs at `/usr/share/embra/models/`. Like the toolchain above, the first build needs network for it; later builds skip the download when the cached copy still matches the pin. The build **fails** if the model is missing from the rootfs rather than shipping an image whose retrieval silently falls back to keyword-only. Operators can override the baked copy per instance by seeding `/embra/state/models/<name>` on STATE.

> **Build overrides:** `build-image.sh` reads a few environment knobs — `JOBS` (Buildroot parallelism, default all cores; lower it on a memory-constrained host), `MUSL_CROSS` (musl toolchain root, default `/opt/x86_64-linux-musl-cross`), `BUILDROOT_DIR` (default `buildroot-src`), the `BUILDROOT_VERSION` and `RUST_TOOLCHAIN_VERSION` pins above, and `RUST_DIST_BASE` / `EMBED_BASE` (mirror bases for the Step 3.5 toolchain and Step 3.6 model downloads). Prefix the command: `JOBS=4 ./scripts/build-image.sh --storage-engine rocksdb`.

On first boot, the Config Wizard runs — name your intelligence, choose your LLM provider (Anthropic Claude, Google Gemini, Ollama, or LM Studio), enter the corresponding credentials (API key for Anthropic/Gemini; endpoint URL + optional bearer + selected model for the OpenAI-compat presets), set your timezone. Each field is validated before commit — an invalid API key, unreachable endpoint, or garbage timezone re-prompts instead of persisting. The Ollama / LM Studio sub-flow probes `GET /v1/models` against your endpoint and presents a model selector populated from the live server response. After setup, you're in a full TUI conversation with styled text, thinking indicators, and tool execution.

### Notes

The following apply once the image is built. They are not part of the build pipeline.

> **Terminal Size (serial TUI only):** With `EMBRA_TUI=1`, `run-qemu.sh` reads your terminal size once (`stty size`) and passes it to the guest on the kernel command line (`embra.cols`/`embra.rows`) — maximize the terminal before launching; resizing it afterwards does not update the TUI layout. The web console has no such limit: it follows the browser window.

> **Image Search Order:** `run-qemu.sh` (and `run-qemu-aarch64.sh`) resolves the disk image in this order — the same precedence `seed-state.sh` and `embraos-backup.sh` use, so the image you seed or back up is the one you boot:
> 1. Explicit path passed as the argument: `./scripts/run-qemu.sh /path/to/embraos.img`
> 2. `$EMBRAOS_IMAGE` — the escape hatch when a stale `buildroot-src/` shadows a newer image
> 3. `buildroot-src/output/images/embraos.img` (Buildroot output, always freshest)
> 4. `output/images/embraos.img` (alternative output location)
>
> The kernel (`bzImage`) is looked for beside the image first, then in the two default locations. The initramfs is always `./initramfs.cpio.gz`, and the default image and kernel paths are relative to the current directory as well — run the script from the repository root.

> **Clean First Boot:** To reset and trigger the Config Wizard again (e.g., to change API key):
> ```bash
> ./scripts/seed-state.sh --wipe state,data       # macOS: ./scripts/seed-state-mac.sh
> ```
> Reformats the named partitions in place as ext4 with their original labels, after a
> typed confirmation. Wiping STATE destroys the soul hash, PKI and API keys; wiping DATA
> destroys WardSONDB — all memory, sessions and the workspace. Neither is reversible, and
> the VM must be stopped.
>
> Use `--wipe state` or `--wipe data` to reset just one (`--wipe all` = `state,data`), `--yes` to skip the prompt, and
> combine with the seeding flags to reset and re-seed in a single pass:
> ```bash
> ./scripts/seed-state.sh --wipe state,data --ca-dir /path/to/dir-with-rootCA.pem
> ```
> Partition geometry is read from the GPT at run time, so this stays correct as partitions
> shift between builds.

> **Port Forwarding:** QEMU forwards 50000 (gRPC), 8443 (REST) and 3345 (the HTTPS web console — https://localhost:3345/embraOS) in both UI modes; only the launch banner differs. apid's REST surface is `/health`, `/version` and `/status` — `/status` proxies the brain's `GetSystemStatus` (2-second timeout, HTTP 503 while the brain is away) and carries the active LLM provider's last endpoint probe as `llm-provider` / `llm-provider.detail`. Test with:
> ```bash
> curl http://localhost:8443/health
> curl http://localhost:8443/status
> ```

> **Backup & Restore:** `scripts/embraos-backup.sh` preserves STATE and DATA partitions across image rebuilds. This is a file-level backup — WardSONDB does not need to be running. The VM must be stopped.
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
> Backups are stored in `~/embraOS_BACKUPS/` by default — under `sudo` that is still the invoking user's home, resolved from `$SUDO_USER` (override with `EMBRAOS_BACKUP_DIR`). `restore` prints the backup's metadata, asks `Continue? [y/N]`, then empties the image's STATE and DATA partitions before copying the backup in — a fresh image's seed contents are replaced, not merged. Each backup includes STATE (soul hash, PKI certs), DATA (WardSONDB collections, workspace), and metadata with SHA-256 of the source image. To target another image pass `--image <path>`; `EMBRAOS_IMAGE` works too, but it must come *after* `sudo` (`sudo EMBRAOS_IMAGE=… ./scripts/embraos-backup.sh …`) — sudo's default `env_reset` drops a variable set before it, and the script would silently back up the default image.

---

## Self-hosted git servers (private CA)

To let the git tools reach a self-hosted GitLab/Gitea whose HTTPS certificate chains to a private root CA (e.g. an mkcert development CA), drop the CA's `*.pem`/`*.crt` file(s) into `/embra/state/ca-certificates/` — on a disk image, via:

```bash
./scripts/seed-state.sh --ca-dir /path/to/dir-with-rootCA.pem
# macOS (no losetup/ext4 — runs the same script in a privileged container):
./scripts/seed-state-mac.sh --ca-dir /path/to/dir-with-rootCA.pem
```

With no image argument, `seed-state.sh` resolves the same one `run-qemu.sh` boots
(`$EMBRAOS_IMAGE`, then `buildroot-src/output/images/`, then `output/images/`), so you
cannot seed one image and boot another. Pass a path — positional or `--image <path>` — to
override. The VM must be stopped — seeding an image QEMU has open corrupts it, and the
script refuses. `./scripts/seed-state.sh --help` lists every flag. Beyond `--ca-dir`,
`--seed-dir` (knowledge packs), `--import-dir` (intelligence graphs) and `--wipe`:
`--phase0-data <dir>` copies `<dir>/wardsondb/` onto DATA (size-checked first; its `.engine`
marker must match the image's `--storage-engine`), `--soul-hash <hash>` writes STATE's
`soul.sha256`, and `EMBRAOS_ROOT` re-anchors the default image paths when the script runs
from another directory.

At the next boot, embrad merges the drop-ins with the stock CA bundle (public hosts like github.com keep working) and exports `GIT_SSL_CAINFO`/`SSL_CERT_FILE` for every service, so `git_clone`/`git_push`/`git_pull` trust the server. The boot log line (readable in-session via the `system_logs` tool, service `embrad`) names each accepted cert file. Scope: this covers the git/OpenSSL path plus the `gl_*` GitLab API tools (whose HTTP client adds the same drop-ins as trust anchors) — the other Rust-side HTTP clients (providers, guardian `http_get`) keep their compiled-in public roots. For private repos, set a per-host token with `/git-token <host> <token>` after boot; the same token authenticates the `gl_*` issue/merge-request tools.

---

The day-to-day session model, slash commands, and keyboard shortcuts live in [OPERATION.md](OPERATION.md) and [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md). GitHub and SSH setup are slash commands run from the conversational TUI after boot — see [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md) (`/github-token`, `/git-token`, `/ssh-keygen`, `/ssh-copy-id`, `/git-setup`).
