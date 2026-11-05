#!/bin/bash
# Post-build: prepare rootfs before SquashFS creation

# A failed step stops the build, and an unset or empty target directory
# stops it here: every `rm -rf` below is rooted on it (#33).
set -euo pipefail

BOARD_DIR="$(dirname "$0")"
TARGET_DIR="${1:?post_build.sh needs the target directory as its argument}"
# The Rust target triple (BR2_ROOTFS_POST_SCRIPT_ARGS in both defconfigs): the
# release stamp is read from beside the binaries Buildroot picks up, so it
# describes the build that produced them, under --buildroot-only as well.
RUST_TARGET="${2:-}"
[ -n "$RUST_TARGET" ] || {
    echo "ERROR: post_build.sh needs the Rust target as its second argument (BR2_ROOTFS_POST_SCRIPT_ARGS)" >&2
    exit 1
}
REPO_ROOT="${BOARD_DIR}/../../.."

# Remove unnecessary files for minimal rootfs
rm -rf "${TARGET_DIR}/usr/share/man"
rm -rf "${TARGET_DIR}/usr/share/doc"

# Create mount points
mkdir -p "${TARGET_DIR}/embra/state"
mkdir -p "${TARGET_DIR}/embra/data"
mkdir -p "${TARGET_DIR}/embra/ephemeral"
mkdir -p "${TARGET_DIR}/embra/boot"
mkdir -p "${TARGET_DIR}/mnt/initramfs"
mkdir -p "${TARGET_DIR}/mnt/root"
mkdir -p "${TARGET_DIR}/tmp"
mkdir -p "${TARGET_DIR}/run"
mkdir -p "${TARGET_DIR}/dev"
mkdir -p "${TARGET_DIR}/proc"
mkdir -p "${TARGET_DIR}/sys"

# Each OS slot carries its own kernel and initramfs under /boot: the kernel
# is installed by Buildroot (BR2_LINUX_KERNEL_INSTALL_TARGET; bzImage on
# x86_64, Image on aarch64, never named here), the initramfs is the one
# scripts/create_initramfs.sh wrote in the repository root. GRUB loads both
# from the slot's squashfs.
KERNELS=0
for k in bzImage Image; do
    [ -f "${TARGET_DIR}/boot/${k}" ] && KERNELS=$((KERNELS + 1))
done
[ "$KERNELS" -eq 1 ] || {
    echo "ERROR: expected exactly one kernel under ${TARGET_DIR}/boot (BR2_LINUX_KERNEL_INSTALL_TARGET), found ${KERNELS}" >&2
    exit 1
}
[ -f "${REPO_ROOT}/initramfs.cpio.gz" ] || {
    echo "ERROR: ${REPO_ROOT}/initramfs.cpio.gz is missing — run scripts/create_initramfs.sh" >&2
    exit 1
}
install -m 0644 "${REPO_ROOT}/initramfs.cpio.gz" "${TARGET_DIR}/boot/initramfs.cpio.gz"

# The release stamp (scripts/lib/image-common.sh::write_release_file): what
# this slot is, read by embrad, embra-update and the status surfaces.
RELEASE_FILE="${REPO_ROOT}/target/${RUST_TARGET}/release/embraos-release"
[ -f "$RELEASE_FILE" ] || {
    echo "ERROR: ${RELEASE_FILE} is missing — a full build writes it beside the binaries" >&2
    exit 1
}
mkdir -p "${TARGET_DIR}/etc"
install -m 0644 "$RELEASE_FILE" "${TARGET_DIR}/etc/embraos-release"

# What GRUB shows for this slot in its menu: one `set` line it can `source`
# from the slot's squashfs. A value that could break out of the quotes is
# refused rather than escaped.
VERSION=$(sed -n 's/^VERSION=//p' "$RELEASE_FILE" | head -n1)
BUILD_ID=$(sed -n 's/^BUILD_ID=//p' "$RELEASE_FILE" | head -n1)
BUILT=$(sed -n 's/^BUILT=//p' "$RELEASE_FILE" | head -n1)
for v in "$VERSION" "$BUILD_ID" "$BUILT"; do
    [ -n "$v" ] || { echo "ERROR: ${RELEASE_FILE} lacks VERSION, BUILD_ID or BUILT" >&2; exit 1; }
    case "$v" in
        *['"$\\']*|*$'\n'*)
            echo "ERROR: a release value carries a quote, a dollar, a backslash or a newline: ${v}" >&2
            exit 1 ;;
    esac
done
printf 'set slot_version="%s (%s, %s)"\n' "$VERSION" "$BUILD_ID" "${BUILT%%T*}" \
    > "${TARGET_DIR}/boot/embraos-slot.cfg"

# DNS resolution — QEMU SLIRP provides DNS at 10.0.2.3
# Required for embra-brain to reach api.anthropic.com
echo "nameserver 10.0.2.3" > "${TARGET_DIR}/etc/resolv.conf"

# Workspace mount point (embrad bind-mounts /embra/data/workspace here at boot)
mkdir -p "${TARGET_DIR}/embra/workspace"

# Importable intelligence graphs (kg-native-identity): bake the repo's
# Imported_Intelligence/*.graph.json examples read-only into the rootfs so
# every fresh boot can offer the Learning-Mode import with zero
# provisioning. STATE's imported-intelligence/ wins filename collisions.
# The folder's README.md is host-side authoring documentation — .graph.json
# only, by construction.
if ls "${REPO_ROOT}/Imported_Intelligence"/*.graph.json >/dev/null 2>&1; then
    mkdir -p "${TARGET_DIR}/usr/share/embra/imported-intelligence"
    cp "${REPO_ROOT}/Imported_Intelligence"/*.graph.json \
       "${TARGET_DIR}/usr/share/embra/imported-intelligence/"
fi

# Seed knowledge packs (knowledge.v1): bake the repo's committed
# Seed_Knowledge/*.knowledge.json packs read-only into the rootfs — the
# brain's boot reconcile ensures their nodes/edges exist in the live KG on
# every boot. A pack the image ships loads from the image; a STATE copy of
# the same pack is skipped and named (STATE's seed-knowledge/ is for the
# operator's own packs; `knowledge/seed.rs::scan_dirs`). The folder's
# README.md is host-side authoring documentation.
if ls "${REPO_ROOT}/Seed_Knowledge"/*.knowledge.json >/dev/null 2>&1; then
    mkdir -p "${TARGET_DIR}/usr/share/embra/seed-knowledge"
    cp "${REPO_ROOT}/Seed_Knowledge"/*.knowledge.json \
       "${TARGET_DIR}/usr/share/embra/seed-knowledge/"
fi

# No SSH server in the image (security invariant, see the defconfig).
# With the server option off a fresh build tree never gets these files. A
# tree that was built while it was on still has them: Buildroot's target
# directory persists across builds, and an option that is switched off
# removes nothing. They go here, on every build. The client (ssh, scp, sftp,
# ssh-keygen, ssh-copy-id) and its configuration stay.
rm -f "${TARGET_DIR}/usr/sbin/sshd" \
      "${TARGET_DIR}/usr/libexec/sshd-session" \
      "${TARGET_DIR}/usr/libexec/sshd-auth" \
      "${TARGET_DIR}/usr/libexec/sftp-server" \
      "${TARGET_DIR}/etc/ssh/sshd_config"

# Defense-in-depth: lock the root account.
# The Buildroot skeleton leaves /etc/shadow with an empty root password,
# which means anyone with shell access can become root without credentials.
# embraOS has no login paths today (nothing starts a getty, and there is no
# SSH server in the image), so an empty-password root is not currently
# exploitable —
# but file_read is unrestricted and `/etc/shadow` is readable, so agent
# compromise via prompt injection (flagged in Sprint 3 sweep #11) would hand
# over a useful credential for free. Locking it removes that value while
# leaving the account structure intact so future tooling (su, su-exec) can
# still reason about UID 0. See Embra_Debug #11.
if [ -f "${TARGET_DIR}/etc/shadow" ]; then
    sed -i 's/^root:[^:]*:/root:*:/' "${TARGET_DIR}/etc/shadow"
fi
