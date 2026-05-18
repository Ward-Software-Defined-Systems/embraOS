#!/bin/bash
# Post-image: assemble the final disk image with genimage.
#
# The kernel filename differs by arch (x86_64 -> bzImage, aarch64 ->
# Image), so genimage.cfg.in is a template: resolve @KERNEL_IMAGE@ from
# whatever kernel Buildroot produced in BINARIES_DIR and render the real
# config into BUILD_DIR. One committed config builds both arches — no
# per-arch sed.

BOARD_DIR="$(dirname "$0")"
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

sed "s/@KERNEL_IMAGE@/${KERNEL_IMAGE}/" "${GENIMAGE_CFG_IN}" > "${GENIMAGE_CFG}"

rm -rf "${GENIMAGE_TMP}"

genimage \
    --rootpath "${TARGET_DIR}" \
    --tmppath "${GENIMAGE_TMP}" \
    --inputpath "${BINARIES_DIR}" \
    --outputpath "${BINARIES_DIR}" \
    --config "${GENIMAGE_CFG}"
