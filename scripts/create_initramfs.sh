#!/bin/bash
# Create initramfs cpio archive containing embra-init
#
# Supports both x86_64 and aarch64 builds via RUST_TARGET env var:
#   RUST_TARGET=aarch64-unknown-linux-musl ./scripts/create_initramfs.sh
#
# Defaults to x86_64-unknown-linux-musl if not set.

set -euo pipefail

RUST_TARGET="${RUST_TARGET:-x86_64-unknown-linux-musl}"
INITRAMFS_DIR=$(mktemp -d)
EMBRA_INIT="target/${RUST_TARGET}/release/embra-init"

if [ ! -f "$EMBRA_INIT" ]; then
    echo "ERROR: embra-init not found at ${EMBRA_INIT}"
    echo "Build it first:"
    echo "  cargo build --release --bin embra-init --target ${RUST_TARGET}"
    exit 1
fi

echo "Building initramfs for target: ${RUST_TARGET}"

# Create initramfs directory structure
mkdir -p "${INITRAMFS_DIR}"/{bin,dev,proc,sys,mnt/root,tmp}

# Copy embra-init as /init (kernel runs /init from initramfs)
cp "$EMBRA_INIT" "${INITRAMFS_DIR}/init"
chmod +x "${INITRAMFS_DIR}/init"

# Create basic device nodes
mknod -m 622 "${INITRAMFS_DIR}/dev/console" c 5 1 2>/dev/null || true
mknod -m 666 "${INITRAMFS_DIR}/dev/null" c 1 3 2>/dev/null || true

# Create cpio archive
cd "${INITRAMFS_DIR}"
find . | cpio -o -H newc 2>/dev/null | gzip > "${OLDPWD}/initramfs.cpio.gz"
cd "${OLDPWD}"

echo "Created initramfs.cpio.gz ($(du -h initramfs.cpio.gz | cut -f1)) [${RUST_TARGET}]"

# Cleanup
rm -rf "${INITRAMFS_DIR}"
