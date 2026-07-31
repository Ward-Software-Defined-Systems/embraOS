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

# Importable intelligence graphs (kg-native-identity): bake the repo's
# Imported_Intelligence/*.graph.json examples read-only into the rootfs so
# every fresh boot can offer the Learning-Mode import with zero
# provisioning. STATE's imported-intelligence/ wins filename collisions.
# The folder's README.md is host-side authoring documentation — .graph.json
# only, by construction.
REPO_ROOT="${BOARD_DIR}/../../.."
if ls "${REPO_ROOT}/Imported_Intelligence"/*.graph.json >/dev/null 2>&1; then
    mkdir -p "${TARGET_DIR}/usr/share/embra/imported-intelligence"
    cp "${REPO_ROOT}/Imported_Intelligence"/*.graph.json \
       "${TARGET_DIR}/usr/share/embra/imported-intelligence/"
fi

# Seed knowledge packs (knowledge.v1): bake the repo's committed
# Seed_Knowledge/*.knowledge.json packs read-only into the rootfs — the
# brain's boot reconcile ensures their nodes/edges exist in the live KG on
# every boot. STATE's seed-knowledge/ wins filename collisions; the
# folder's README.md is host-side authoring documentation.
if ls "${REPO_ROOT}/Seed_Knowledge"/*.knowledge.json >/dev/null 2>&1; then
    mkdir -p "${TARGET_DIR}/usr/share/embra/seed-knowledge"
    cp "${REPO_ROOT}/Seed_Knowledge"/*.knowledge.json \
       "${TARGET_DIR}/usr/share/embra/seed-knowledge/"
fi

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
