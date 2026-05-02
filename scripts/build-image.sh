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
# NOTE: Steps 1-3 (Rust cross-compilation + initramfs) work on macOS.
#       Step 4 (Buildroot) requires a Linux host. On macOS, use Docker:
#
#   ./scripts/build-image.sh --storage-engine rocksdb   # outer call bakes engine
#   docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
#     "apt-get update && apt-get install -y build-essential gcc g++ \
#      unzip bc cpio rsync wget python3 file && \
#      ./scripts/build-image.sh --buildroot-only"

set -euo pipefail

# Buildroot release pin. Override at runtime: BUILDROOT_VERSION=2024.02 ./scripts/build-image.sh ...
BUILDROOT_VERSION="${BUILDROOT_VERSION:-2026.02.1}"

# macOS-compatible nproc
nproc_compat() {
    if command -v nproc &>/dev/null; then
        nproc
    else
        sysctl -n hw.ncpu
    fi
}

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

    echo "=== Step 1: Build Rust binaries (musl static) ==="
    rustup target add x86_64-unknown-linux-musl
    cargo build --release --target x86_64-unknown-linux-musl

    echo "=== Step 2: Build WardSONDB (from separate repo) ==="
    WARDSONDB_DIR="${WARDSONDB_DIR:-../WardSONDB}"
    if [ -d "$WARDSONDB_DIR" ]; then
        (cd "$WARDSONDB_DIR" && cargo build --release --target x86_64-unknown-linux-musl)
        cp "$WARDSONDB_DIR/target/x86_64-unknown-linux-musl/release/wardsondb" \
           target/x86_64-unknown-linux-musl/release/wardsondb
    else
        echo "WARNING: WardSONDB directory not found at $WARDSONDB_DIR"
        echo "Set WARDSONDB_DIR to the WardSONDB repository path"
    fi

    echo "=== Step 3: Create initramfs ==="
    ./scripts/create_initramfs.sh
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
    echo "  docker run --rm -v \"\$PWD\":/work -w /work ubuntu:24.04 bash -c \\"
    echo "    \"apt-get update && apt-get install -y build-essential gcc g++ \\"
    echo "     unzip bc cpio rsync wget python3 file && \\"
    echo "     ./scripts/build-image.sh --buildroot-only\""
    echo ""
    echo "Or run this script on a Linux machine."
    exit 1
fi

BUILDROOT_DIR="${BUILDROOT_DIR:-buildroot-src}"
if [ ! -d "$BUILDROOT_DIR" ]; then
    git clone https://github.com/buildroot/buildroot.git "$BUILDROOT_DIR"
fi
# Ensure the clone is at the pinned version. Switching versions wipes
# output/ to avoid mixing stale Buildroot state across releases; refuse
# the switch if the user has local changes in $BUILDROOT_DIR.
(
    cd "$BUILDROOT_DIR"
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
    for pkg in embrad embra-apid embra-trustd embra-brain embra-console wardsondb \
               git openssl libcurl openssh; do
        make "${pkg}-dirclean" 2>/dev/null || true
    done && \
    rm -f output/images/rootfs.squashfs output/images/embraos.img)

# Configure and build
(cd "$BUILDROOT_DIR" && \
    make BR2_EXTERNAL="$(pwd)/../buildroot" embraos_x86_64_defconfig && \
    make -j$(nproc_compat))

echo "=== Step 5: Copy outputs ==="
mkdir -p output/images
cp "$BUILDROOT_DIR/output/images/embraos.img" output/images/
cp "$BUILDROOT_DIR/output/images/bzImage" output/images/

echo ""
echo "Build complete!"
echo "Run: ./scripts/run-qemu.sh"
