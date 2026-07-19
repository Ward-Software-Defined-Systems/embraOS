#!/bin/bash
# Full build: Rust binaries → initramfs → Buildroot → disk image
#
# Usage:
#   ./scripts/build-image.sh --storage-engine <fjall|rocksdb>
#   ./scripts/build-image.sh --buildroot-only
#
# --storage-engine selects the WardSONDB backend baked into the embrad binary.
# The choice is locked into the DATA partition on first boot via WardSONDB's
# .engine marker file — switching engines later requires wiping DATA.
#
# NOTE: Steps 0.5-3.5 (frontend + Rust cross-compile + initramfs + in-OS Rust
#       toolchain stage) work on macOS. Step 4 (Buildroot) requires a Linux
#       host. On macOS (Intel or Apple Silicon — Apple Silicon uses the
#       parallel build-image-aarch64.sh flow), use Docker:
#
#   ./scripts/build-image.sh --storage-engine rocksdb   # outer call bakes engine
#   docker run --rm -v "$PWD":/work -v embraos-br-x86_64:/work/buildroot-src \
#     -w /work ubuntu:24.04 bash -c \
#     "apt-get update && apt-get install -y build-essential gcc g++ \
#      unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools libelf-dev && \
#      FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image.sh --buildroot-only"
#
#   (The named volume keeps the Buildroot tree on the Docker VM's native
#    filesystem: building it on the bind mount exhausts the macOS file
#    provider's fd pool — "Too many open files".)
#
# libelf-dev is needed for the kernel's tools/objtool (ORC unwinder + stack
# validation, mostly x86_64). A typical dev Ubuntu host has it system-wide,
# which is why the canonical Linux build never tripped on it; a fresh
# ubuntu:24.04 container doesn't, so it has to be in the apt-get list.

set -euo pipefail

# Buildroot release pin. Override at runtime: BUILDROOT_VERSION=2024.02 ./scripts/build-image.sh ...
BUILDROOT_VERSION="${BUILDROOT_VERSION:-2026.02.1}"

# embra-guardian-v1 in-OS Rust toolchain pin (musl host + wasm32 std).
# Staged into vendor/rust-toolchain by Step 3.5 and installed at /opt/rust.
RUST_TOOLCHAIN_VERSION="${RUST_TOOLCHAIN_VERSION:-1.94.1}"

# macOS-compatible nproc
nproc_compat() {
    if command -v nproc &>/dev/null; then
        nproc
    else
        sysctl -n hw.ncpu
    fi
}

# Parallel build jobs for Buildroot. Defaults to all cores. Lower it on a
# memory-constrained host: Buildroot's GCC + musl toolchain bootstrap can
# OOM with many concurrent compilers in a small Docker VM (e.g. OrbStack's
# 4 GB default). Pass through Docker with:  docker run -e JOBS=2 ...
JOBS="${JOBS:-$(nproc_compat)}"

BUILDROOT_ONLY=false
STORAGE_ENGINE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --buildroot-only)
            BUILDROOT_ONLY=true
            shift
            ;;
        --storage-engine)
            STORAGE_ENGINE="${2:-}"
            shift 2
            ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            echo "Usage: $0 --storage-engine <fjall|rocksdb> [--buildroot-only]" >&2
            exit 2
            ;;
    esac
done

if [ "$BUILDROOT_ONLY" = false ]; then
    if [ -z "$STORAGE_ENGINE" ]; then
        echo "ERROR: --storage-engine <fjall|rocksdb> is required" >&2
        exit 2
    fi
    case "$STORAGE_ENGINE" in
        fjall|rocksdb) ;;
        *)
            echo "ERROR: --storage-engine must be 'fjall' or 'rocksdb', got '$STORAGE_ENGINE'" >&2
            exit 2
            ;;
    esac
    export EMBRA_STORAGE_ENGINE="$STORAGE_ENGINE"
    echo "=== embraOS build: storage engine = $STORAGE_ENGINE ==="
elif [ -n "$STORAGE_ENGINE" ]; then
    echo "WARNING: --storage-engine ignored with --buildroot-only (Rust not rebuilt;" >&2
    echo "         engine taken from the previously-baked embrad binary)" >&2
fi

if [ "$BUILDROOT_ONLY" = false ]; then
    # Musl cross-toolchain. Ubuntu's musl-tools wraps the host gcc and pulls in
    # a glibc-linked libstdc++ that won't link against musl — we need the self-
    # contained musl.cc toolchain for both C and C++ compilation.
    MUSL_CROSS="${MUSL_CROSS:-/opt/x86_64-linux-musl-cross}"
    if [ ! -x "$MUSL_CROSS/bin/x86_64-linux-musl-gcc" ]; then
        echo "ERROR: musl cross-toolchain not found at $MUSL_CROSS" >&2
        echo "  Install it with:" >&2
        echo "    cd /tmp && curl -LO https://musl.cc/x86_64-linux-musl-cross.tgz" >&2
        echo "    sudo tar -xzf x86_64-linux-musl-cross.tgz -C /opt" >&2
        exit 1
    fi
    export PATH="$MUSL_CROSS/bin:$PATH"
    export CC_x86_64_unknown_linux_musl=x86_64-linux-musl-gcc
    export CXX_x86_64_unknown_linux_musl=x86_64-linux-musl-g++
    export AR_x86_64_unknown_linux_musl=x86_64-linux-musl-ar
    export CARGO_TARGET_X86_64_UNKNOWN_LINUX_MUSL_LINKER=x86_64-linux-musl-gcc

    # Build the Leptos/WASM frontend BEFORE the cargo cross-build so
    # embra-web can embed crates/embra-web-ui/dist via rust-embed. This
    # is host/CI-only (no Node in the image; assets ride inside the
    # static musl binary).
    echo "=== Step 0.5: Build embra-web frontend (Trunk → WASM) ==="
    if ! command -v trunk &>/dev/null; then
        echo "ERROR: trunk not found (needed for the embra-web frontend)" >&2
        echo "  Install it with:" >&2
        echo "    rustup target add wasm32-unknown-unknown" >&2
        echo "    cargo install trunk --locked" >&2
        exit 1
    fi
    rustup target add wasm32-unknown-unknown
    (cd crates/embra-web-ui && trunk build --release)

    echo "=== Step 1: Build Rust binaries (musl static) ==="
    rustup target add x86_64-unknown-linux-musl
    cargo build --release --target x86_64-unknown-linux-musl
    # wardsondb is a workspace member (crates/wardsondb, vendored 2026-07-17)
    # and builds in Step 1 with the other binaries. The former Step 2
    # sibling-repo build/copy is gone; Buildroot's wardsondb.mk picks the
    # binary up from target/x86_64-unknown-linux-musl/release/ as before.
    # (Step numbering keeps the gap deliberately — Steps 3/3.5/4/5 are
    # referenced by name across the build guides.)

    echo "=== Step 3: Create initramfs ==="
    ./scripts/create_initramfs.sh
fi

# Runs unconditionally (also under --buildroot-only): Buildroot's local
# SITE rsync needs vendor/rust-toolchain present before Step 4.
echo "=== Step 3.5: Stage in-OS Rust toolchain (embra-guardian-v1) ==="
RUST_STAGE="$PWD/vendor/rust-toolchain"
if [ -x "$RUST_STAGE/bin/cargo" ] && \
   [ "$(cat "$RUST_STAGE/RUST_VERSION" 2>/dev/null || true)" = "$RUST_TOOLCHAIN_VERSION" ]; then
    echo "in-OS Rust toolchain already staged ($RUST_TOOLCHAIN_VERSION) — skipping"
else
    if ! command -v xz &>/dev/null; then
        echo "ERROR: xz is required to unpack the in-OS Rust toolchain" >&2
        exit 1
    fi
    RUST_DIST_BASE="${RUST_DIST_BASE:-https://static.rust-lang.org/dist}"
    RUST_HOST="rust-${RUST_TOOLCHAIN_VERSION}-x86_64-unknown-linux-musl"
    RUST_WASM="rust-std-${RUST_TOOLCHAIN_VERSION}-wasm32-unknown-unknown"
    RUST_TMP="$(mktemp -d)"
    trap 'rm -rf "$RUST_TMP"' EXIT
    for tb in "$RUST_HOST" "$RUST_WASM"; do
        echo "  downloading ${tb}.tar.xz"
        curl -fSL --retry 3 -o "$RUST_TMP/${tb}.tar.xz" \
            "$RUST_DIST_BASE/${tb}.tar.xz" \
            || { echo "ERROR: download of ${tb}.tar.xz failed" >&2; exit 1; }
        curl -fSL --retry 3 -o "$RUST_TMP/${tb}.tar.xz.sha256" \
            "$RUST_DIST_BASE/${tb}.tar.xz.sha256" \
            || { echo "ERROR: download of ${tb}.tar.xz.sha256 failed" >&2; exit 1; }
        ( cd "$RUST_TMP" \
            && printf '%s  %s\n' "$(awk '{print $1}' "${tb}.tar.xz.sha256")" "${tb}.tar.xz" \
               | sha256sum -c - ) \
            || { echo "ERROR: sha256 verification failed for ${tb}.tar.xz" >&2; exit 1; }
        tar -xf "$RUST_TMP/${tb}.tar.xz" -C "$RUST_TMP" \
            || { echo "ERROR: extract of ${tb}.tar.xz failed" >&2; exit 1; }
    done
    rm -rf "$RUST_STAGE"
    mkdir -p "$RUST_STAGE"
    "$RUST_TMP/$RUST_HOST/install.sh" --prefix="$RUST_STAGE" \
        --disable-ldconfig --without=rust-docs >/dev/null \
        || { echo "ERROR: Rust host install failed" >&2; exit 1; }
    "$RUST_TMP/$RUST_WASM/install.sh" --prefix="$RUST_STAGE" \
        --disable-ldconfig >/dev/null \
        || { echo "ERROR: wasm32 std install failed" >&2; exit 1; }
    rm -rf "$RUST_STAGE/share/doc" "$RUST_STAGE/share/man" \
           "$RUST_STAGE/lib/rustlib/src" 2>/dev/null || true
    echo "$RUST_TOOLCHAIN_VERSION" > "$RUST_STAGE/RUST_VERSION"
    rm -rf "$RUST_TMP"; trap - EXIT
    echo "  staged Rust $RUST_TOOLCHAIN_VERSION ($(du -sh "$RUST_STAGE" 2>/dev/null | cut -f1)) → $RUST_STAGE"
fi

echo "=== Step 4: Buildroot ==="
# Buildroot requires a Linux host (compiles Linux kernel, uses Linux-specific tools)
if [ "$(uname)" = "Darwin" ]; then
    echo "ERROR: Buildroot cannot build natively on macOS."
    echo ""
    echo "Steps 1-3 (Rust cross-compilation + initramfs) completed successfully."
    echo "Storage engine '${EMBRA_STORAGE_ENGINE:-<unset>}' was baked into the embrad binary."
    echo "To build the disk image, run Buildroot in Docker (no engine flag needed inside):"
    echo ""
    echo "  docker run --rm -v \"\$PWD\":/work -v embraos-br-x86_64:/work/buildroot-src \\"
    echo "    -w /work ubuntu:24.04 bash -c \\"
    echo "    \"apt-get update && apt-get install -y build-essential gcc g++ \\"
    echo "     unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools libelf-dev && \\"
    echo "     FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image.sh --buildroot-only\""
    echo ""
    echo "(The named volume keeps the Buildroot tree on the Docker VM's filesystem:"
    echo " building it on the bind mount exhausts macOS file-provider fds — EMFILE.)"
    echo ""
    echo "Or run this script on a Linux machine."
    exit 1
fi

# fd preflight: raise the soft fd limit to the hard cap and report state.
# Docker containers (OrbStack: soft 20480 / hard 1048576) default well below
# what a full Buildroot build can demand. Per-process only — macOS-side
# virtiofs-provider fd exhaustion (EMFILE on bind-mount paths) is outside
# any in-container limit's reach.
FD_SOFT="$(ulimit -Sn)"
FD_HARD="$(ulimit -Hn)"
if [ "$FD_SOFT" != "unlimited" ]; then
    FD_RAISE="$FD_HARD"
    if [ "$FD_RAISE" = "unlimited" ]; then
        FD_RAISE=1048576
    fi
    if [ "$FD_SOFT" -lt "$FD_RAISE" ]; then
        ulimit -n "$FD_RAISE" 2>/dev/null \
            || echo "WARNING: could not raise fd soft limit (soft $FD_SOFT, hard $FD_HARD)" >&2
    fi
fi
echo "fd limits: soft $(ulimit -Sn) / hard $(ulimit -Hn); system-wide $(awk '{print $1 "/" $3}' /proc/sys/fs/file-nr 2>/dev/null || echo 'n/a')"
echo "Buildroot parallelism: -j${JOBS} (override: docker run -e JOBS=N ...)"

BUILDROOT_DIR="${BUILDROOT_DIR:-buildroot-src}"
# Clone when missing OR empty: if BUILDROOT_DIR is a mountpoint (e.g. a Docker
# named volume), the directory always exists — an empty one must still get the
# clone (git clones into an existing empty dir).
if [ ! -d "$BUILDROOT_DIR" ] || [ -z "$(ls -A "$BUILDROOT_DIR" 2>/dev/null)" ]; then
    git clone https://github.com/buildroot/buildroot.git "$BUILDROOT_DIR"
fi
# Ensure the clone is at the pinned version. Switching versions wipes
# output/ to avoid mixing stale Buildroot state across releases; refuse
# the switch if the user has local changes in $BUILDROOT_DIR.
(
    cd "$BUILDROOT_DIR"
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        echo "ERROR: $BUILDROOT_DIR exists but is not a git checkout (non-empty, no .git)." >&2
        echo "       Remove or empty it so this script can clone Buildroot fresh." >&2
        exit 1
    fi
    current=$(git describe --tags --exact-match 2>/dev/null || echo "")
    if [ "$current" != "$BUILDROOT_VERSION" ]; then
        if ! git diff-index --quiet HEAD --; then
            echo "ERROR: $BUILDROOT_DIR has local changes; refusing to switch to $BUILDROOT_VERSION." >&2
            echo "       Commit/stash them, or set BUILDROOT_VERSION=${current:-<your-version>} to pin to the current checkout." >&2
            exit 1
        fi
        echo "=== Buildroot: switching ${current:-unknown} -> $BUILDROOT_VERSION (wiping output/) ==="
        git fetch --tags
        git checkout "$BUILDROOT_VERSION"
        rm -rf output/
    fi
)

# Clean stale Buildroot package caches so fresh binaries are picked up
# Includes embraOS packages AND upstream packages whose config may have changed
(cd "$BUILDROOT_DIR" && \
    for pkg in embrad embra-apid embra-trustd embra-brain embra-console embra-web wardsondb \
               embra-rust-toolchain git openssl libcurl openssh; do
        make "${pkg}-dirclean" 2>/dev/null || true
    done && \
    rm -f output/images/rootfs.squashfs output/images/embraos.img)

# Configure and build
(cd "$BUILDROOT_DIR" && \
    make BR2_EXTERNAL="$(pwd)/../buildroot" embraos_x86_64_defconfig && \
    make -j"$JOBS")

echo "=== Step 5: Copy outputs ==="
mkdir -p output/images
cp "$BUILDROOT_DIR/output/images/embraos.img" output/images/
cp "$BUILDROOT_DIR/output/images/bzImage" output/images/

echo ""
echo "Build complete!"
echo "Run: ./scripts/run-qemu.sh"
