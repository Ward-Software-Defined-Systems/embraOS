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

# The disk is a whole number of MiB: 1 MiB in front of the first partition
# (the protective MBR, the GPT header and its entry array, then the 1 MiB
# alignment), the partitions, and 1 MiB at the end that holds the backup GPT
# (33 sectors). Left to itself genimage ends the image 20 KiB after the last
# partition. Every hypervisor's own tools make whole-MiB disks, VMware's
# sparse-extent format wants a capacity that is a multiple of its 64 KiB
# grain, and Azure refuses a VHD that is not whole MiB. genimage.cfg.in takes
# the size and places the backup GPT on its last sector; the check below reads
# both back from the written image (post_image.sh, assert_image_layout).
embraos_mib() {    # <genimage size in K, M or G> → MiB; a K size is rounded up
    case "$1" in
        *G) echo $(( ${1%G} * 1024 )) ;;
        *M) echo "${1%M}" ;;
        *K) echo $(( (${1%K} + 1023) / 1024 )) ;;
        *)  echo "layout.sh: '$1' is not a size in K, M or G" >&2; return 1 ;;
    esac
}
EMBRAOS_IMAGE_SIZE_MIB=$(( 1 + $(embraos_mib "$EMBRAOS_ESP_SIZE") + 2 * $(embraos_mib "$EMBRAOS_SLOT_SIZE") \
    + $(embraos_mib "$EMBRAOS_STATE_SIZE") + $(embraos_mib "$EMBRAOS_DATA_SIZE") + 1 ))
EMBRAOS_IMAGE_SIZE="${EMBRAOS_IMAGE_SIZE_MIB}M"

# embraos_check_whole_mib_gpt <image>: the image is EMBRAOS_IMAGE_SIZE_MIB MiB
# long, the GPT header at sector 1 names the last sector as its backup's, and
# the backup header is on that sector and says so itself. od, dd and head are
# on every build host. Prints the reason and returns 1.
embraos_check_whole_mib_gpt() {
    embraos_img="$1"
    embraos_bytes=$(stat -c %s "$embraos_img" 2>/dev/null || stat -f %z "$embraos_img")
    embraos_want=$(( EMBRAOS_IMAGE_SIZE_MIB * 1048576 ))
    [ "$embraos_bytes" -eq "$embraos_want" ] || {
        echo "ERROR: $embraos_img is $embraos_bytes bytes, not the $EMBRAOS_IMAGE_SIZE_MIB MiB ($embraos_want) of layout.sh" >&2
        return 1
    }
    embraos_last=$(( embraos_bytes / 512 - 1 ))
    [ "$(dd if="$embraos_img" bs=512 skip=1 count=1 2>/dev/null | head -c 8)" = "EFI PART" ] || {
        echo "ERROR: $embraos_img has no GPT header at sector 1" >&2
        return 1
    }
    embraos_alt=$(od -An -tu8 -j $(( 512 + 32 )) -N 8 "$embraos_img" | tr -d ' ')
    [ "$embraos_alt" -eq "$embraos_last" ] || {
        echo "ERROR: the GPT header of $embraos_img names sector $embraos_alt as its backup; the last sector is $embraos_last" >&2
        return 1
    }
    [ "$(dd if="$embraos_img" bs=512 skip="$embraos_last" count=1 2>/dev/null | head -c 8)" = "EFI PART" ] || {
        echo "ERROR: $embraos_img has no backup GPT header on its last sector ($embraos_last)" >&2
        return 1
    }
    embraos_mine=$(od -An -tu8 -j $(( embraos_last * 512 + 24 )) -N 8 "$embraos_img" | tr -d ' ')
    [ "$embraos_mine" -eq "$embraos_last" ] || {
        echo "ERROR: the backup GPT header of $embraos_img says it is at sector $embraos_mine, not $embraos_last" >&2
        return 1
    }
}
