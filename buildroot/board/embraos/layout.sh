# shellcheck shell=sh
# The disk layout both architectures share: the sizes genimage.cfg.in is
# rendered with (post_image.sh) and the GPT partition names, in order.
# scripts/lib/image-common.sh sources this file for the build-time checks,
# and crates/embra-boot's test reads it so the names the OS looks partitions
# up by are the names genimage writes. A size is a genimage size (K, M, G).
EMBRAOS_ESP_SIZE="64M"
EMBRAOS_SLOT_SIZE="1024M"      # each rootfs slot; the squashfs must fit with the margin free
EMBRAOS_STATE_SIZE="256M"
EMBRAOS_DATA_SIZE="4G"
EMBRAOS_SLOT_MARGIN_PCT=10
EMBRAOS_PARTITION_NAMES="esp rootfs_a rootfs_b state data"
