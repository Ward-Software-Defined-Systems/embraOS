#!/bin/bash
# Post-build: prepare rootfs before SquashFS creation

BOARD_DIR="$(dirname "$0")"
TARGET_DIR="$1"

# Remove unnecessary files for minimal rootfs
rm -rf "${TARGET_DIR}/usr/share/man"
rm -rf "${TARGET_DIR}/usr/share/doc"

# Create mount points
mkdir -p "${TARGET_DIR}/embra/state"
mkdir -p "${TARGET_DIR}/embra/data"
mkdir -p "${TARGET_DIR}/embra/ephemeral"
mkdir -p "${TARGET_DIR}/mnt/initramfs"
mkdir -p "${TARGET_DIR}/mnt/root"
mkdir -p "${TARGET_DIR}/tmp"
mkdir -p "${TARGET_DIR}/run"
mkdir -p "${TARGET_DIR}/dev"
mkdir -p "${TARGET_DIR}/proc"
mkdir -p "${TARGET_DIR}/sys"

# DNS resolution — QEMU SLIRP provides DNS at 10.0.2.3
# Required for embra-brain to reach api.anthropic.com
mkdir -p "${TARGET_DIR}/etc"
echo "nameserver 10.0.2.3" > "${TARGET_DIR}/etc/resolv.conf"

# Workspace directory for git/file tool operations (writable, on DATA partition)
# This is a placeholder — in production, embrad creates this as a bind mount from DATA
mkdir -p "${TARGET_DIR}/embra/workspace/repos"
