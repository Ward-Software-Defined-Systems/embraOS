#!/bin/bash
# Post-image: stage the EFI System Partition, render genimage.cfg from its
# template and assemble the disk image, then check the layout genimage wrote.
#
# The kernel filename differs by arch (x86_64 -> bzImage, aarch64 -> Image)
# and the sizes come from layout.sh, so genimage.cfg.in is a template rendered
# into BUILD_DIR. One committed config builds both arches — no per-arch sed.

# A failed step stops the build; the directories Buildroot exports must be
# set, since `rm -rf` and genimage are rooted on them (#33).
set -euo pipefail
: "${BUILD_DIR:?post_image.sh runs under Buildroot, which exports BUILD_DIR}"
: "${BINARIES_DIR:?post_image.sh runs under Buildroot, which exports BINARIES_DIR}"
: "${TARGET_DIR:?post_image.sh runs under Buildroot, which exports TARGET_DIR}"
: "${BR2_CONFIG:?post_image.sh runs under Buildroot, which exports BR2_CONFIG}"
: "${HOST_DIR:?post_image.sh runs under Buildroot, which exports HOST_DIR}"

BOARD_DIR="$(cd "$(dirname "$0")" && pwd)"
# shellcheck source=layout.sh
source "${BOARD_DIR}/layout.sh"
GENIMAGE_CFG_IN="${BOARD_DIR}/genimage.cfg.in"
GENIMAGE_CFG="${BUILD_DIR}/genimage.cfg"
GENIMAGE_TMP="${BUILD_DIR}/genimage.tmp"

if [ -f "${BINARIES_DIR}/Image" ]; then
    KERNEL_IMAGE="Image"
elif [ -f "${BINARIES_DIR}/bzImage" ]; then
    KERNEL_IMAGE="bzImage"
else
    echo "ERROR: no kernel image (Image or bzImage) found in ${BINARIES_DIR}" >&2
    exit 1
fi
echo "kernel: ${KERNEL_IMAGE} (inside each slot's squashfs at /boot)"

# The ESP tree genimage copies whole: GRUB's loader, grub.cfg and grubenv land
# under EFI/BOOT.
EFI_PART="${BINARIES_DIR}/efi-part"
mkdir -p "${EFI_PART}/EFI/BOOT"

sed -e "s/@ESP_SIZE@/${EMBRAOS_ESP_SIZE}/" \
    -e "s/@SLOT_SIZE@/${EMBRAOS_SLOT_SIZE}/" \
    -e "s/@STATE_SIZE@/${EMBRAOS_STATE_SIZE}/" \
    -e "s/@DATA_SIZE@/${EMBRAOS_DATA_SIZE}/" \
    "${GENIMAGE_CFG_IN}" > "${GENIMAGE_CFG}"
if grep -q '@[A-Z_]*@' "${GENIMAGE_CFG}"; then
    echo "ERROR: a placeholder of ${GENIMAGE_CFG_IN} was not rendered:" >&2
    grep -n '@[A-Z_]*@' "${GENIMAGE_CFG}" >&2
    exit 1
fi

rm -rf "${GENIMAGE_TMP}"
# The Phase 1 ESP image, stale in a reused output tree
rm -f "${BINARIES_DIR}/boot.vfat"

genimage \
    --rootpath "${TARGET_DIR}" \
    --tmppath "${GENIMAGE_TMP}" \
    --inputpath "${BINARIES_DIR}" \
    --outputpath "${BINARIES_DIR}" \
    --config "${GENIMAGE_CFG}"

# The layout check: the partitions genimage wrote carry the names of
# layout.sh in order, the first starts at sector 2048 (1 MiB) and every start
# is a multiple of 2048 — what `align = 1M` promises, verified on the image
# rather than trusted.
read -r -a EXPECTED <<< "${EMBRAOS_PARTITION_NAMES}"
mapfile -t ROWS < <(partx -g -o NR,START,NAME -- "${BINARIES_DIR}/embraos.img")
[ "${#ROWS[@]}" -eq "${#EXPECTED[@]}" ] || {
    echo "ERROR: embraos.img has ${#ROWS[@]} partitions, expected ${#EXPECTED[@]} (${EMBRAOS_PARTITION_NAMES})" >&2
    exit 1
}
i=0
for row in "${ROWS[@]}"; do
    read -r nr start name <<< "${row}"
    [ "${name}" = "${EXPECTED[$i]}" ] || {
        echo "ERROR: partition ${nr} is named '${name}', expected '${EXPECTED[$i]}'" >&2
        exit 1
    }
    if [ "$i" -eq 0 ] && [ "${start}" -ne 2048 ]; then
        echo "ERROR: partition 1 starts at sector ${start}, expected 2048 (1 MiB alignment)" >&2
        exit 1
    fi
    [ $((start % 2048)) -eq 0 ] || {
        echo "ERROR: partition ${nr} (${name}) starts at sector ${start}, not a multiple of 2048" >&2
        exit 1
    }
    i=$((i + 1))
done
echo "image layout: ${EMBRAOS_PARTITION_NAMES}, 1 MiB aligned"
