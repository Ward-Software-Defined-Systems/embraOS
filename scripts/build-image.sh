#!/bin/bash
# Full build: Rust binaries → initramfs → Buildroot → disk image

set -euo pipefail

echo "=== Step 1: Build Rust binaries (musl static) ==="
rustup target add x86_64-unknown-linux-musl

# Build all workspace binaries
cargo build --release --target x86_64-unknown-linux-musl

echo "=== Step 2: Build WardSONDB (from separate repo) ==="
# Assumes WardSONDB is at ../WardSONDB or specified by WARDSONDB_DIR
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

echo "=== Step 4: Buildroot ==="
# Clone Buildroot if not present
BUILDROOT_DIR="${BUILDROOT_DIR:-buildroot-src}"
if [ ! -d "$BUILDROOT_DIR" ]; then
    git clone https://github.com/buildroot/buildroot.git "$BUILDROOT_DIR"
    (cd "$BUILDROOT_DIR" && git checkout 2024.02)  # Use a stable release
fi

# Configure and build
(cd "$BUILDROOT_DIR" && \
    make BR2_EXTERNAL="$(pwd)/../buildroot" embraos_x86_64_defconfig && \
    make -j$(nproc))

echo "=== Step 5: Copy outputs ==="
mkdir -p output/images
cp "$BUILDROOT_DIR/output/images/embraos.img" output/images/
cp "$BUILDROOT_DIR/output/images/bzImage" output/images/

echo ""
echo "Build complete!"
echo "Run: ./scripts/run-qemu.sh"
