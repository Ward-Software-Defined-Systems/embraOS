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

# Workspace mount point (embrad bind-mounts /embra/data/workspace here at boot)
mkdir -p "${TARGET_DIR}/embra/workspace"

# Defense-in-depth: lock the root account.
# The Buildroot skeleton leaves /etc/shadow with an empty root password,
# which means anyone with shell access can become root without credentials.
# embraOS has no login paths today (no getty on the console, no SSH server in
# the defconfig), so an empty-password root is not currently exploitable —
# but file_read is unrestricted and `/etc/shadow` is readable, so agent
# compromise via prompt injection (flagged in Sprint 3 sweep #11) would hand
# over a useful credential for free. Locking it removes that value while
# leaving the account structure intact so future tooling (su, su-exec) can
# still reason about UID 0. See Embra_Debug #11.
if [ -f "${TARGET_DIR}/etc/shadow" ]; then
    sed -i 's/^root:[^:]*:/root:*:/' "${TARGET_DIR}/etc/shadow"
fi
