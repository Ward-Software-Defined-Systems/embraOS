#!/bin/bash
# Full build: Rust binaries → initramfs → Buildroot → disk image
#
# NOTE: Steps 1-3 (Rust cross-compilation + initramfs) work on macOS.
#       Step 4 (Buildroot) requires a Linux host. On macOS, use Docker:
#
#   docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
#     "apt-get update && apt-get install -y build-essential gcc g++ \
#      unzip bc cpio rsync wget python3 file && \
#      ./scripts/build-image.sh --buildroot-only"

set -euo pipefail

# macOS-compatible nproc
nproc_compat() {
    if command -v nproc &>/dev/null; then
        nproc
    else
        sysctl -n hw.ncpu
    fi
}

BUILDROOT_ONLY=false
if [ "${1:-}" = "--buildroot-only" ]; then
    BUILDROOT_ONLY=true
    shift
fi

if [ "$BUILDROOT_ONLY" = false ]; then
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
    echo "To build the disk image, run Buildroot in Docker:"
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
    (cd "$BUILDROOT_DIR" && git checkout 2024.02)  # Use a stable release
fi

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
