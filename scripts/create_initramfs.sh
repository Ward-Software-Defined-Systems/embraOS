#!/bin/bash
# Create initramfs cpio archive containing embra-init

set -euo pipefail

INITRAMFS_DIR=$(mktemp -d)
EMBRA_INIT="target/x86_64-unknown-linux-musl/release/embra-init"

if [ ! -f "$EMBRA_INIT" ]; then
    echo "ERROR: embra-init not found. Build it first:"
    echo "  cargo build --release --bin embra-init --target x86_64-unknown-linux-musl"
    exit 1
fi

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

echo "Created initramfs.cpio.gz ($(du -h initramfs.cpio.gz | cut -f1))"

# Cleanup
rm -rf "${INITRAMFS_DIR}"
