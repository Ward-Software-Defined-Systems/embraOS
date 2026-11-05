# shellcheck shell=bash
# Partition lookup by GPT name, shared by seed-state.sh, embraos-backup.sh and
# the image scripts. The names come from buildroot/board/embraos/layout.sh
# (esp, rootfs_a, rootfs_b, state, data); a Phase 1 image carries `state`
# and `data` under the same names, so both layouts work. The numbers moved
# when the two OS slots arrived, the names did not.
#
# partx reads the table from an image file, prints bare numbers with no
# device column (immune to spaces in the image path), and lives in
# util-linux, Priority:required, so the Docker wrappers install nothing.

# partition_by_name <image> <gpt name> → "START SECTORS" (512-byte sectors),
# nothing when the image has no partition of that name.
partition_by_name() {
    partx -g -o START,SECTORS,NAME -- "$1" 2>/dev/null \
        | awk -v n="$2" '$3 == n { print $1, $2; exit }'
}
