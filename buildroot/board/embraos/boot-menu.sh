#!/bin/bash
# boot-menu.sh: the installed disk's boot menu, rendered from grub.cfg.in,
# as functions. One rendered file serves three places. post_build.sh puts it
# into the slot's root filesystem (/usr/share/embra/grub.cfg): a release is a
# slot, an OS update writes a slot and no ESP, and the updater brings the
# slot's menu to the ESP once the slot is the confirmed default
# (embra-update boot-menu). post_image.sh installs that very file on the
# image's ESP, and the installer medium takes its copy of the ESP's tree. So
# a machine that was updated boots the menu a machine that was installed
# boots. Run by itself, it calls one function:
#
#   boot-menu.sh <function> <arguments>

# embraos_serial_console <Buildroot's .config>: the serial console the menu
# names on the kernel line. A configuration that cannot be read has no
# answer: the x86 console is no default.
embraos_serial_console() {
    [ -r "$1" ] || { echo "ERROR: boot menu: Buildroot's configuration '$1' cannot be read" >&2; return 1; }
    if grep -q '^BR2_aarch64=y' "$1"; then
        echo ttyAMA0
    else
        echo ttyS0
    fi
}

# embraos_render_boot_menu <grub.cfg.in> <kernel image> <serial console> <out> [<grub-script-check>]
# The template with its two placeholders filled, written to <out>; refused
# when a placeholder is left, when a value is not one of the two the
# template is written for, when the kernel and the console are not one
# architecture's, and when the checker, if one is given, refuses the result.
embraos_render_boot_menu() {
    local template="$1" kernel="$2" console="$3" out="$4" lint="${5:-}"
    case "$kernel" in
        bzImage|Image) ;;
        *) echo "ERROR: boot menu: '$kernel' is no kernel image name (bzImage, Image)" >&2; return 1 ;;
    esac
    case "$console" in
        ttyS0|ttyAMA0) ;;
        *) echo "ERROR: boot menu: '$console' is no serial console (ttyS0, ttyAMA0)" >&2; return 1 ;;
    esac
    case "$kernel/$console" in
        bzImage/ttyS0|Image/ttyAMA0) ;;
        *) echo "ERROR: boot menu: $kernel and $console are not one architecture's (bzImage with ttyS0, Image with ttyAMA0)" >&2; return 1 ;;
    esac
    [ -f "$template" ] || { echo "ERROR: boot menu: no template at $template" >&2; return 1; }
    mkdir -p "$(dirname "$out")"
    sed -e "s/@KERNEL_IMAGE@/${kernel}/g" \
        -e "s/@SERIAL_CONSOLE@/${console}/g" \
        "$template" > "$out"
    if grep -q '@[A-Z_]*@' "$out"; then
        echo "ERROR: a placeholder of $(basename "$template") was not rendered:" >&2
        grep -n '@[A-Z_]*@' "$out" >&2
        return 1
    fi
    if [ -n "$lint" ]; then
        [ -x "$lint" ] || { echo "ERROR: boot menu: $lint is missing (host-grub2 builds with the grub2 package)" >&2; return 1; }
        "$lint" "$out" || { echo "ERROR: grub-script-check refused the boot menu" >&2; return 1; }
    fi
    return 0
}

# Run by itself: one function and its arguments.
if [ "${BASH_SOURCE[0]}" = "$0" ] && [ $# -ge 1 ]; then
    set -euo pipefail
    fn="$1"; shift
    case "$fn" in
        embraos_serial_console|embraos_render_boot_menu) "$fn" "$@" ;;
        *) echo "boot-menu.sh: no function $fn" >&2; exit 2 ;;
    esac
fi
