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
# Host-side extras (none reaches the OS):
#   EMBRA_VNC=1                 the display on a VNC server at 127.0.0.1:5900
#       (password "embraos": macOS Screen Sharing refuses a server without
#       one; loopback only, tunnel it with ssh -L). The serial line stays on
#       this terminal. The UEFI path paints GRUB's menu and the boot banner
#       there; the direct path's kernel line names only the serial console.
#   EMBRA_ISO=<embraos.iso>     boot the installer medium first; the image argument is then the
#                               TARGET disk (a blank qemu-img disk, raw or .qcow2, the installer writes)
#   EMBRA_ISO_AS=cdrom|usb      the medium as a virtual DVD (default) or a USB stick on xHCI (the hybrid GPT path)
#   EMBRA_DISK_IF=virtio|ahci   the target disk's bus (virtio)
#   EMBRA_DHCP_START=<address>  the first address QEMU's DHCP server leases
#       (10.0.2.200 shows the banner's second URL on the leased address).
#   EMBRA_USB_HOST=<vendor>:<product>   a USB device of this host handed to the
#       VM, by the ids lsusb shows (a Wi-Fi adapter: 0bda:c811)
#   EMBRA_MEMORY=<size>         the VM's memory, a number and M or G (4G)
#   EMBRA_CPUS=<n>              the VM's processors (4)
#   EMBRA_CPU=<model>           the VM's processor model as QEMU names it (host, max, Nehalem).
#       Without it QEMU's own default, a plain x86-64 without AVX; host is this
#       machine's processor and needs KVM or HVF. A native model (experimental)
#       reads a prompt several times faster with AVX2 than without.
# The clock's knob, which reaches the kernel line:
#   EMBRA_NTP=<host>[:<port>]|off   embra.ntp: the time server the OS asks in
#       this boot (else /network time's, the DHCP lease's, then time.cloudflare.com); off asks none
# The display console's knobs. They reach the kernel line, and act on a VM
# that has the display (EMBRA_VNC=1, which EMBRA_KIOSK=1 implies):
#   EMBRA_KIOSK=0|1             embra.kiosk: the display console off, or on; the OS's default is on with a display
#   EMBRA_KBD=<layout>          embra.kbd: the display console's keyboard layout (us)
#   EMBRA_KIOSK_MODE=<W>x<H>    embra.kioskmode: the display console's video mode (1920x1080 where the display offers it); native = the display's own
# An image named *.qcow2 is opened as qcow2, with the medium and without it
# (a disk the installer wrote boots from this launcher).
#
# macOS runs this file with its own /bin/bash, 3.2: an array that may be empty
# is expanded as ${A[@]+"${A[@]}"}, because that shell takes "${A[@]}" of an
# empty array for an unset variable under `set -u` and stops.

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

MEMORY="${EMBRA_MEMORY:-4G}"
CPUS="${EMBRA_CPUS:-4}"

# The VM's memory, processors and processor model. A native model
# (experimental) wants more than the defaults: its file's size and its
# window's memory beside the OS, and the instruction sets its speed comes
# from (QEMU's own default model is a plain processor without them).
if ! [[ "$MEMORY" =~ ^[1-9][0-9]*[MG]$ ]]; then
    echo "ERROR: EMBRA_MEMORY must be a number and M or G (4G, 12G, 8192M), got '$MEMORY'" >&2; exit 2
fi
if ! [[ "$CPUS" =~ ^[1-9][0-9]*$ ]]; then
    echo "ERROR: EMBRA_CPUS must be a number of processors, got '$CPUS'" >&2; exit 2
fi
if [ -n "${EMBRA_CPU:-}" ] && ! [[ "$EMBRA_CPU" =~ ^[A-Za-z0-9_.+=,-]+$ ]]; then
    echo "ERROR: EMBRA_CPU must be a processor model as QEMU names it (host, max, Nehalem), got '$EMBRA_CPU'" >&2; exit 2
fi
CPU_ARGS=()
if [ -n "${EMBRA_CPU:-}" ]; then
    CPU_ARGS=(-cpu "$EMBRA_CPU")
fi

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

# Detect host terminal size and pass it to the guest (the serial TUI's size).
# An unknown size is 80x24. Without a terminal on stdin stty fails: `|| true`
# keeps that failure from ending the script under `set -e -o pipefail`. A
# pseudo-terminal nobody gave a size answers "0 0": awk prints a dimension
# only when it is above zero.
HOST_COLS=$(stty size 2>/dev/null | awk '$2 > 0 {print $2}' || true)
HOST_ROWS=$(stty size 2>/dev/null | awk '$1 > 0 {print $1}' || true)
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
if [ -n "${EMBRA_NTP:-}" ]; then
    KNOBS="$KNOBS embra.ntp=${EMBRA_NTP}"
    echo "  Time server: ${EMBRA_NTP} (EMBRA_NTP)"
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
if [ -n "${EMBRA_KIOSK_MODE:-}" ]; then
    KNOBS="$KNOBS embra.kioskmode=${EMBRA_KIOSK_MODE}"
fi
if [ -n "${EMBRA_KBD:-}" ]; then
    KNOBS="$KNOBS embra.kbd=${EMBRA_KBD}"
fi

# The user-mode network. QEMU forwards the REST port (8443) to the host's
# loopback and the web console (3345) to every host interface. The two gRPC
# ports (50000, 50002) bind the guest's loopback and are not forwarded.
NIC="user,hostfwd=tcp:127.0.0.1:8443-:8443,hostfwd=tcp::3345-:3345"
if [ -n "${EMBRA_DHCP_START:-}" ]; then
    if ! [[ "$EMBRA_DHCP_START" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]]; then
        echo "ERROR: EMBRA_DHCP_START must be an IPv4 address, got '$EMBRA_DHCP_START'" >&2; exit 2
    fi
    NIC="$NIC,dhcpstart=$EMBRA_DHCP_START"
    echo "  DHCP: QEMU leases from $EMBRA_DHCP_START (EMBRA_DHCP_START)"
fi

# A USB device of this host handed to the VM, by the two hexadecimal ids
# lsusb shows for it: a Wi-Fi adapter, the one kind of network card QEMU
# does not emulate. QEMU needs write access to the device's node under
# /dev/bus/usb, and this host must not be using the adapter itself. QEMU
# says nothing when no device has the ids or when it may not open the node:
# it starts, and the VM has no adapter. On a Linux host the device is
# therefore looked for here first, and a node this user may not write stops
# the launcher with the udev rule that hands it over.
USB_HOST_ARGS=()
if [ -n "${EMBRA_USB_HOST:-}" ]; then
    if ! [[ "$EMBRA_USB_HOST" =~ ^[0-9a-fA-F]{4}:[0-9a-fA-F]{4}$ ]]; then
        echo "ERROR: EMBRA_USB_HOST must be <vendor>:<product> as lsusb shows them (0bda:c811), got '$EMBRA_USB_HOST'" >&2; exit 2
    fi
    USB_VENDOR="$(printf '%s' "${EMBRA_USB_HOST%%:*}" | tr 'A-F' 'a-f')"
    USB_PRODUCT="$(printf '%s' "${EMBRA_USB_HOST##*:}" | tr 'A-F' 'a-f')"
    if [ -d /sys/bus/usb/devices ]; then
        USB_NODE=""
        for usb_dev in /sys/bus/usb/devices/*; do
            [ -r "$usb_dev/idVendor" ] && [ -r "$usb_dev/idProduct" ] || continue
            if [ "$(cat "$usb_dev/idVendor")" = "$USB_VENDOR" ] && [ "$(cat "$usb_dev/idProduct")" = "$USB_PRODUCT" ]; then
                USB_NODE="$(printf '/dev/bus/usb/%03d/%03d' "$(cat "$usb_dev/busnum")" "$(cat "$usb_dev/devnum")")"
                break
            fi
        done
        if [ -z "$USB_NODE" ]; then
            echo "ERROR: no USB device $USB_VENDOR:$USB_PRODUCT is on this host (EMBRA_USB_HOST; lsusb shows what is)." >&2
            echo "       An adapter that shows itself as a disk first has other ids until this host has switched it (usb_modeswitch)." >&2
            exit 2
        fi
        if ! [ -r "$USB_NODE" ] || ! [ -w "$USB_NODE" ]; then
            echo "ERROR: $USB_NODE, the USB device $USB_VENDOR:$USB_PRODUCT, is not writable for $(id -un): QEMU would start without it and say nothing." >&2
            echo "       A udev rule hands the node to this user, now and each time the device is plugged in:" >&2
            echo "         echo 'SUBSYSTEM==\"usb\", ATTR{idVendor}==\"$USB_VENDOR\", ATTR{idProduct}==\"$USB_PRODUCT\", OWNER=\"$(id -un)\"' | sudo tee /etc/udev/rules.d/70-embraos-usb-host.rules" >&2
            echo "         sudo udevadm control --reload && sudo udevadm trigger --action=change --attr-match=idVendor=$USB_VENDOR --attr-match=idProduct=$USB_PRODUCT" >&2
            exit 2
        fi
    fi
    USB_HOST_ARGS=(-device qemu-xhci,id=xhci-host -device "usb-host,bus=xhci-host.0,vendorid=0x$USB_VENDOR,productid=0x$USB_PRODUCT")
    echo "  USB: this host's device $USB_VENDOR:$USB_PRODUCT is handed to the VM (EMBRA_USB_HOST)"
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
if [ -n "${EMBRA_CPU:-}" ]; then
    echo "  CPU model: $EMBRA_CPU (EMBRA_CPU)"
fi
echo "  Acceleration: $ACCEL_NAME"
echo "  Serial console: this terminal"
if [ "${EMBRA_TUI:-}" = "1" ]; then
    echo "  UI mode: serial TUI (EMBRA_TUI=1) — embra-web not started"
    echo "  Port forwards: 127.0.0.1:8443→8443 (REST)"
else
    echo "  UI mode: web console (default) — set EMBRA_TUI=1 for the serial TUI"
    echo "  Port forwards: 127.0.0.1:8443→8443 (REST), *:3345→3345 (HTTPS web)"
    echo "  Web console: the URL embraOS prints below at boot (it carries the access token, the same at every boot; accept the embraOS-CA cert)"
fi

# The disk: raw, or qcow2 when the image is named *.qcow2 (a disk the
# installer wrote), on virtio.
# The installer medium (EMBRA_ISO): the ISO boots first, the image argument
# is the disk the installer writes, on virtio or AHCI; the medium is a
# virtual DVD or a USB stick on xHCI (the hybrid GPT xorriso writes). UEFI
# only: the medium has no BIOS path.
case "${EMBRA_ISO_AS:-cdrom}" in cdrom|usb) ;; *) echo "ERROR: EMBRA_ISO_AS must be 'cdrom' or 'usb', got '$EMBRA_ISO_AS'" >&2; exit 2 ;; esac
case "${EMBRA_DISK_IF:-virtio}" in virtio|ahci) ;; *) echo "ERROR: EMBRA_DISK_IF must be 'virtio' or 'ahci', got '$EMBRA_DISK_IF'" >&2; exit 2 ;; esac
case "$IMAGE" in *.qcow2) DISK_FMT=qcow2 ;; *) DISK_FMT=raw ;; esac
DISK_ARGS=(-drive "file=$IMAGE,format=$DISK_FMT,if=virtio")
MEDIA_ARGS=()
if [ -n "${EMBRA_ISO:-}" ]; then
    [ -f "$EMBRA_ISO" ] || { echo "ERROR: EMBRA_ISO=$EMBRA_ISO is not a file" >&2; exit 2; }
    [ "$EMBRA_BOOT" = "uefi" ] || { echo "ERROR: the installer medium boots through the firmware; EMBRA_BOOT=direct has no medium" >&2; exit 2; }
    case "${EMBRA_DISK_IF:-virtio}" in
        virtio) DISK_ARGS=(-drive "file=$IMAGE,format=$DISK_FMT,if=none,id=target" -device virtio-blk-pci,drive=target,bootindex=1) ;;
        ahci)   DISK_ARGS=(-drive "file=$IMAGE,format=$DISK_FMT,if=none,id=target" -device ide-hd,drive=target,bootindex=1) ;;
    esac
    case "${EMBRA_ISO_AS:-cdrom}" in
        cdrom) MEDIA_ARGS=(-drive "file=$EMBRA_ISO,format=raw,if=none,id=medium,media=cdrom,readonly=on" -device ide-cd,drive=medium,bootindex=0) ;;
        usb)   MEDIA_ARGS=(-device qemu-xhci,id=xhci-medium -drive "file=$EMBRA_ISO,format=raw,if=none,id=medium,snapshot=on" -device usb-storage,drive=medium,bootindex=0) ;;
    esac
    echo "  Installer: $EMBRA_ISO as ${EMBRA_ISO_AS:-cdrom}; the target disk is $IMAGE ($DISK_FMT, ${EMBRA_DISK_IF:-virtio}); embra.install=<disk> with embra.license=accept on the knobs makes it unattended"
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
        ${CPU_ARGS[@]+"${CPU_ARGS[@]}"} \
        -m "$MEMORY" \
        -smp "$CPUS" \
        "${DISK_ARGS[@]}" \
        -kernel "$KERNEL" \
        -initrd "$INITRD" \
        -append "console=ttyS0 ro quiet embra.slot=a $WEB_CMDLINE $KNOBS" \
        "${DISPLAY_ARGS[@]}" \
        ${USB_HOST_ARGS[@]+"${USB_HOST_ARGS[@]}"} \
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
    ${CPU_ARGS[@]+"${CPU_ARGS[@]}"} \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -drive if=pflash,format=raw,readonly=on,file="$FW_CODE" \
    -drive if=pflash,format=raw,file="$VARS_SCRATCH" \
    "${DISK_ARGS[@]}" \
    ${MEDIA_ARGS[@]+"${MEDIA_ARGS[@]}"} \
    -smbios "type=11,value=${SMBIOS_VALUE}" \
    "${DISPLAY_ARGS[@]}" \
    ${USB_HOST_ARGS[@]+"${USB_HOST_ARGS[@]}"} \
    -serial mon:stdio \
    -nic "$NIC"
