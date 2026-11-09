#!/bin/bash
# Launch embraOS in QEMU with hardware acceleration (auto-detected).
#
# Two boot paths:
#   EMBRA_BOOT=uefi (default)  The disk boots itself: OVMF, then GRUB on the
#       EFI System Partition with its menu (slot A or B, Diagnostics, Update,
#       Backup and restore). The host-side knobs below reach the kernel line
#       through one SMBIOS OEM string that grub.cfg appends — nothing is
#       rebuilt, and nothing is typed in the menu.
#   EMBRA_BOOT=direct          QEMU loads the kernel and the initramfs from
#       the host and the knobs go on -append: no firmware needed and no menu,
#       the path of a kernel or embra-init change without an image rebuild.
#
# Firmware for the UEFI path: EMBRA_FW_CODE/EMBRA_FW_VARS, else the OVMF or
# edk2 images of the host (Ubuntu: the ovmf package; macOS: Homebrew's qemu).
# The variable store is copied per run and removed on exit.
#
# Host-side extras (neither reaches the OS):
#   EMBRA_VNC=1                 the display on a VNC server at 127.0.0.1:5900
#       (password "embraos": macOS Screen Sharing refuses a server without
#       one; loopback only, tunnel it with ssh -L). The serial line stays on
#       this terminal. The UEFI path paints GRUB's menu and the boot banner
#       there; the direct path's kernel line names only the serial console.
#   EMBRA_KIOSK=0|1             the display console off, or on (implies EMBRA_VNC=1); the OS's default is on with a display
#   EMBRA_KBD=<layout>          the display console's keyboard layout (us)
#   EMBRA_ISO=<embraos.iso>     boot the installer medium first; the image argument is then the
#                               TARGET disk (a blank qemu-img disk, raw or .qcow2, the installer writes)
#   EMBRA_ISO_AS=cdrom|usb      the medium as a virtual DVD (default) or a USB stick on xHCI (the hybrid GPT path)
#   EMBRA_DISK_IF=virtio|ahci   the target disk's bus (virtio)
#   EMBRA_DHCP_START=<address>  the first address QEMU's DHCP server leases
#       (10.0.2.200 shows the banner's second URL on the leased address).

set -euo pipefail

EMBRA_BOOT="${EMBRA_BOOT:-uefi}"
case "$EMBRA_BOOT" in
    uefi|direct) ;;
    *) echo "ERROR: EMBRA_BOOT must be 'uefi' (default) or 'direct', got '$EMBRA_BOOT'" >&2; exit 2 ;;
esac

# Find image — argument, then $EMBRAOS_IMAGE, then buildroot output (always
# freshest), then output/images (the precedence seed-state.sh and
# embraos-backup.sh share)
if [ -n "${1:-}" ]; then
    IMAGE="$1"
elif [ -n "${EMBRAOS_IMAGE:-}" ]; then
    IMAGE="$EMBRAOS_IMAGE"
elif [ -f "buildroot-src/output/images/embraos.img" ]; then
    IMAGE="buildroot-src/output/images/embraos.img"
elif [ -f "output/images/embraos.img" ]; then
    IMAGE="output/images/embraos.img"
else
    echo "ERROR: No disk image found."
    echo "Build it first with: ./scripts/build-image.sh"
    exit 1
fi

MEMORY="4G"
CPUS="4"

# Auto-detect best acceleration
ACCEL=""
ACCEL_NAME="none (TCG software emulation)"
if [ "$(uname)" = "Darwin" ]; then
    ACCEL="-accel hvf"
    ACCEL_NAME="HVF (macOS)"
elif [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL="-accel kvm"
    ACCEL_NAME="KVM (Linux)"
fi

# Detect host terminal size and pass it to the guest (the serial TUI's size)
HOST_COLS=$(stty size 2>/dev/null | awk '{print $2}')
HOST_ROWS=$(stty size 2>/dev/null | awk '{print $1}')
HOST_COLS=${HOST_COLS:-80}
HOST_ROWS=${HOST_ROWS:-24}

# The knobs, the same on both paths (embrad reads the last occurrence of each):
#   EMBRA_TUI=1         embra.tui=1 — the serial TUI instead of the web console
#                       (on the UEFI path it overrides GRUB's embra.web=1)
#   EMBRA_DB_VERBOSE=1  embra.dbverbose=1 — WardSONDB per-request log lines
#                       (the log lives on the 512M boot-wiped tmpfs)
#   EMBRA_LOG_LEVEL     embra.loglevel=<spec> — the brain's log filter, an
#                       env-filter Targets spec such as "info,kg::traversal=debug"
#                       (no spaces; read back with the system_logs tool)
#   EMBRA_GRAPHICS      embra.graphics=<mode> — the serial TUI's image protocol:
#                       halfblocks (default) | sixel | kitty | iterm2 | auto | off
KNOBS="embra.cols=$HOST_COLS embra.rows=$HOST_ROWS"
if [ "${EMBRA_TUI:-}" = "1" ]; then
    KNOBS="$KNOBS embra.tui=1"
fi
if [ "${EMBRA_DB_VERBOSE:-}" = "1" ]; then
    KNOBS="$KNOBS embra.dbverbose=1"
    echo "  WardSONDB: verbose request logging (EMBRA_DB_VERBOSE=1)"
fi
if [ -n "${EMBRA_LOG_LEVEL:-}" ]; then
    KNOBS="$KNOBS embra.loglevel=${EMBRA_LOG_LEVEL}"
    echo "  Brain log filter: ${EMBRA_LOG_LEVEL} (EMBRA_LOG_LEVEL)"
fi
if [ -n "${EMBRA_GRAPHICS:-}" ]; then
    KNOBS="$KNOBS embra.graphics=${EMBRA_GRAPHICS}"
    echo "  Serial graphics: ${EMBRA_GRAPHICS} (EMBRA_GRAPHICS)"
fi
# The two switches take 1 and nothing else; another value (a stray comma:
# EMBRA_TUI="1,") would be ignored in silence and the web console would
# come up as if nothing had been asked.
for switch in EMBRA_TUI EMBRA_DB_VERBOSE; do
    case "${!switch:-}" in
        ""|0|1) ;;
        *) echo "WARNING: $switch='${!switch}' is not 1 and is ignored (the switch is $switch=1)" >&2 ;;
    esac
done

# The user-mode network: the forwarded ports, and the DHCP range's first
# address when asked for.
# The display console: EMBRA_KIOSK=0 boots without it, EMBRA_KIOSK=1 asks
# for it (and implies the VNC display below); the OS's own default is on
# where a display exists. EMBRA_KBD=<xkb layout> is its keyboard layout (us).
case "${EMBRA_KIOSK:-}" in
    0|1) KNOBS="$KNOBS embra.kiosk=${EMBRA_KIOSK}" ;;
    "") ;;
    *) echo "WARNING: EMBRA_KIOSK='$EMBRA_KIOSK' is not 0 or 1 and is ignored" >&2 ;;
esac
if [ -n "${EMBRA_KBD:-}" ]; then
    KNOBS="$KNOBS embra.kbd=${EMBRA_KBD}"
fi

NIC="user,hostfwd=tcp:127.0.0.1:50000-:50000,hostfwd=tcp:127.0.0.1:8443-:8443,hostfwd=tcp::3345-:3345"
if [ -n "${EMBRA_DHCP_START:-}" ]; then
    if ! [[ "$EMBRA_DHCP_START" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]]; then
        echo "ERROR: EMBRA_DHCP_START must be an IPv4 address, got '$EMBRA_DHCP_START'" >&2; exit 2
    fi
    NIC="$NIC,dhcpstart=$EMBRA_DHCP_START"
    echo "  DHCP: QEMU leases from $EMBRA_DHCP_START (EMBRA_DHCP_START)"
fi

# The display: none (the serial line on this terminal), or a VNC server on
# the loopback beside it.
# Headless: no VGA device at all, so the OS sees no display and starts no
# display console for a screen nobody watches (embra.kiosk decides on a
# display that exists). EMBRA_VNC=1, and EMBRA_KIOSK=1 which implies it,
# give the VM standard VGA, a USB tablet (an absolute pointer, the kind VNC
# drives) beside the PS/2 keyboard, and the VNC server.
DISPLAY_ARGS=(-nographic -vga none)
if [ "${EMBRA_VNC:-}" = "1" ] || [ "${EMBRA_KIOSK:-}" = "1" ]; then
    DISPLAY_ARGS=(-display none -object secret,id=vncpw,data=embraos -vga std -vnc 127.0.0.1:0,password-secret=vncpw -device qemu-xhci -device usb-tablet)
    echo "  Display: VNC at 127.0.0.1:5900, password embraos (EMBRA_VNC=1; from another machine: ssh -L 5901:127.0.0.1:5900 <this host>, then localhost:5901)"
elif [ -n "${EMBRA_VNC:-}" ] && [ "$EMBRA_VNC" != "0" ]; then
    echo "WARNING: EMBRA_VNC='$EMBRA_VNC' is not 1 and is ignored (the switch is EMBRA_VNC=1)" >&2
fi

# find_firmware: FW_CODE and FW_VARS for the UEFI path. The override pair
# first, then the host's images (the vars template must match its code image:
# the 4M OVMF pair, or QEMU's edk2 pair).
find_firmware() {
    if [ -n "${EMBRA_FW_CODE:-}" ]; then
        FW_CODE="$EMBRA_FW_CODE"
        FW_VARS="${EMBRA_FW_VARS:-}"
        [ -f "$FW_CODE" ] && [ -f "$FW_VARS" ] || {
            echo "ERROR: EMBRA_FW_CODE and EMBRA_FW_VARS must both name existing firmware images" >&2
            exit 1
        }
        return
    fi
    local brew_share="" pair code vars
    if command -v brew >/dev/null 2>&1; then
        brew_share="$(brew --prefix 2>/dev/null)/share/qemu"
    fi
    for pair in \
        "/usr/share/OVMF/OVMF_CODE_4M.fd:/usr/share/OVMF/OVMF_VARS_4M.fd" \
        "/usr/share/OVMF/OVMF_CODE.fd:/usr/share/OVMF/OVMF_VARS.fd" \
        "/usr/share/edk2/x64/OVMF_CODE.4m.fd:/usr/share/edk2/x64/OVMF_VARS.4m.fd" \
        "/usr/share/edk2/ovmf/OVMF_CODE.fd:/usr/share/edk2/ovmf/OVMF_VARS.fd" \
        "/usr/share/qemu/edk2-x86_64-code.fd:/usr/share/qemu/edk2-i386-vars.fd" \
        "${brew_share}/edk2-x86_64-code.fd:${brew_share}/edk2-i386-vars.fd"; do
        code="${pair%%:*}"
        vars="${pair#*:}"
        if [ -f "$code" ] && [ -f "$vars" ]; then
            FW_CODE="$code"
            FW_VARS="$vars"
            return
        fi
    done
    echo "ERROR: no UEFI firmware found for the x86_64 guest." >&2
    echo "  Ubuntu/Debian: sudo apt-get install ovmf   (/usr/share/OVMF/OVMF_CODE_4M.fd)" >&2
    echo "  macOS:         brew install qemu           (\$(brew --prefix)/share/qemu/edk2-x86_64-code.fd)" >&2
    echo "  Or name the pair: EMBRA_FW_CODE=<code.fd> EMBRA_FW_VARS=<vars.fd> ./scripts/run-qemu.sh" >&2
    echo "  Or boot without firmware: EMBRA_BOOT=direct ./scripts/run-qemu.sh" >&2
    exit 1
}

echo "Starting embraOS in QEMU..."
echo "  Image: $IMAGE"
echo "  Boot: $EMBRA_BOOT"
echo "  Memory: $MEMORY"
echo "  CPUs: $CPUS"
echo "  Acceleration: $ACCEL_NAME"
echo "  Serial console: this terminal"
if [ "${EMBRA_TUI:-}" = "1" ]; then
    echo "  UI mode: serial TUI (EMBRA_TUI=1) — embra-web not started"
    echo "  Port forwards: 127.0.0.1:50000→50000 (gRPC), 127.0.0.1:8443→8443 (REST)"
else
    echo "  UI mode: web console (default) — set EMBRA_TUI=1 for the serial TUI"
    echo "  Port forwards: 127.0.0.1:50000→50000 (gRPC), 127.0.0.1:8443→8443 (REST), *:3345→3345 (HTTPS web)"
    echo "  Web console: the URL embraOS prints below at boot (it carries this boot's access token; accept the embraOS-CA cert)"
fi

# The installer medium (EMBRA_ISO): the ISO boots first, the image argument
# is the disk the installer writes (raw, or qcow2 by its suffix), on virtio
# or AHCI; the medium is a virtual DVD or a USB stick on xHCI (the hybrid
# GPT xorriso writes). UEFI only: the medium has no BIOS path.
case "${EMBRA_ISO_AS:-cdrom}" in cdrom|usb) ;; *) echo "ERROR: EMBRA_ISO_AS must be 'cdrom' or 'usb', got '$EMBRA_ISO_AS'" >&2; exit 2 ;; esac
case "${EMBRA_DISK_IF:-virtio}" in virtio|ahci) ;; *) echo "ERROR: EMBRA_DISK_IF must be 'virtio' or 'ahci', got '$EMBRA_DISK_IF'" >&2; exit 2 ;; esac
DISK_ARGS=(-drive "file=$IMAGE,format=raw,if=virtio")
MEDIA_ARGS=()
if [ -n "${EMBRA_ISO:-}" ]; then
    [ -f "$EMBRA_ISO" ] || { echo "ERROR: EMBRA_ISO=$EMBRA_ISO is not a file" >&2; exit 2; }
    [ "$EMBRA_BOOT" = "uefi" ] || { echo "ERROR: the installer medium boots through the firmware; EMBRA_BOOT=direct has no medium" >&2; exit 2; }
    case "$IMAGE" in *.qcow2) DISK_FMT=qcow2 ;; *) DISK_FMT=raw ;; esac
    case "${EMBRA_DISK_IF:-virtio}" in
        virtio) DISK_ARGS=(-drive "file=$IMAGE,format=$DISK_FMT,if=none,id=target" -device virtio-blk-pci,drive=target,bootindex=1) ;;
        ahci)   DISK_ARGS=(-drive "file=$IMAGE,format=$DISK_FMT,if=none,id=target" -device ide-hd,drive=target,bootindex=1) ;;
    esac
    case "${EMBRA_ISO_AS:-cdrom}" in
        cdrom) MEDIA_ARGS=(-drive "file=$EMBRA_ISO,format=raw,if=none,id=medium,media=cdrom,readonly=on" -device ide-cd,drive=medium,bootindex=0) ;;
        usb)   MEDIA_ARGS=(-device qemu-xhci,id=xhci-medium -drive "file=$EMBRA_ISO,format=raw,if=none,id=medium,snapshot=on" -device usb-storage,drive=medium,bootindex=0) ;;
    esac
    echo "  Installer: $EMBRA_ISO as ${EMBRA_ISO_AS:-cdrom}; the target disk is $IMAGE ($DISK_FMT, ${EMBRA_DISK_IF:-virtio}); embra.install=<disk> on the knobs makes it unattended"
fi

if [ "$EMBRA_BOOT" = "direct" ]; then
    # Kernel and initramfs from the host, as before GRUB: the kernel beside
    # the image first, then the two default locations; the initramfs is
    # always ./initramfs.cpio.gz.
    IMAGE_DIR="$(dirname "$IMAGE")"
    KERNEL="${IMAGE_DIR}/bzImage"
    if [ ! -f "$KERNEL" ]; then
        for k in buildroot-src/output/images/bzImage output/images/bzImage; do
            if [ -f "$k" ]; then KERNEL="$k"; break; fi
        done
    fi
    INITRD="initramfs.cpio.gz"
    if [ ! -f "$INITRD" ]; then
        echo "ERROR: initramfs.cpio.gz not found. Run ./scripts/create_initramfs.sh first."
        exit 1
    fi
    if [ ! -f "$KERNEL" ]; then
        echo "ERROR: bzImage not found. Build with ./scripts/build-image.sh first."
        exit 1
    fi
    # The web console unless EMBRA_TUI=1 (the GRUB entries say embra.web=1
    # themselves; here the kernel line is ours).
    WEB_CMDLINE="embra.web=1"
    if [ "${EMBRA_TUI:-}" = "1" ]; then
        WEB_CMDLINE=""
    fi
    echo "  Kernel: $KERNEL"
    echo "  Initrd: $INITRD"
    echo ""
    echo "Press Ctrl-A X to exit QEMU"
    echo ""
    # -no-reboot: a kernel panic on this developer path ends QEMU instead of
    # looping; the OS's own reboots (an update, /reboot) belong to the UEFI
    # path. embra.slot=a names the slot the kernel came from.
    qemu-system-x86_64 \
        $ACCEL \
        -m "$MEMORY" \
        -smp "$CPUS" \
        -drive file="$IMAGE",format=raw,if=virtio \
        -kernel "$KERNEL" \
        -initrd "$INITRD" \
        -append "console=ttyS0 ro quiet embra.slot=a $WEB_CMDLINE $KNOBS" \
        "${DISPLAY_ARGS[@]}" \
        -serial mon:stdio \
        -nic "$NIC" \
        -no-reboot
    exit $?
fi

find_firmware
VARS_SCRATCH="$(mktemp "${TMPDIR:-/tmp}/embraos-vars.XXXXXX")"
cp "$FW_VARS" "$VARS_SCRATCH"
trap 'rm -f "$VARS_SCRATCH"' EXIT
# QEMU's option syntax: a comma inside a value is written twice.
SMBIOS_VALUE="${KNOBS//,/,,}"
echo "  Firmware: $FW_CODE (vars copied per run)"
echo "  GRUB: the menu below; the host knobs ride the SMBIOS string: $KNOBS"
echo ""
echo "Press Ctrl-A X to exit QEMU"
echo ""

# No -kernel/-initrd/-append: GRUB loads the slot's kernel and initramfs
# from its squashfs. No -no-reboot: an update and /reboot end in a reboot
# through the menu, and a slot that fails to come up is bounded by GRUB's
# tries. q35: the machine every UEFI guest expects (AHCI, e1000e, PCIe).
qemu-system-x86_64 \
    -machine q35 \
    $ACCEL \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -drive if=pflash,format=raw,readonly=on,file="$FW_CODE" \
    -drive if=pflash,format=raw,file="$VARS_SCRATCH" \
    "${DISK_ARGS[@]}" \
    "${MEDIA_ARGS[@]}" \
    -smbios "type=11,value=${SMBIOS_VALUE}" \
    "${DISPLAY_ARGS[@]}" \
    -serial mon:stdio \
    -nic "$NIC"
