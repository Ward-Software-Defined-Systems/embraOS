#!/bin/bash
# Launch embraOS in QEMU — aarch64 with HVF hardware acceleration on Apple Silicon
#
# This runs at near-native speed via HVF (no TCG software emulation penalty).
# Requires: brew install qemu
#
# Two boot paths:
#   EMBRA_BOOT=uefi (default)  The disk boots itself: the edk2/AAVMF firmware,
#       then GRUB on the EFI System Partition with its menu (slot A or B,
#       Diagnostics, Update, Backup and restore). The host-side knobs below
#       reach the kernel line through one SMBIOS OEM string that grub.cfg
#       appends — nothing is rebuilt, and nothing is typed in the menu.
#   EMBRA_BOOT=direct          QEMU loads the kernel and the initramfs from
#       the host and the knobs go on -append: no firmware needed and no menu.
#
# Firmware for the UEFI path: EMBRA_FW_CODE/EMBRA_FW_VARS, else the host's
# images (macOS: Homebrew's qemu ships edk2-aarch64-code.fd; Ubuntu: the
# qemu-efi-aarch64 package). The variable store is copied per run.

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
    echo "Build it first with: ./scripts/build-image-aarch64.sh"
    exit 1
fi

# ARM64 kernel is "Image" (flat binary), not "bzImage" (x86 boot wrapper)
KERNEL_NAME="Image"

MEMORY="4G"
CPUS="4"

# Auto-detect best acceleration
ACCEL=""
ACCEL_NAME="none (TCG software emulation)"
CPU_MODEL="-cpu max"
if [ "$(uname)" = "Darwin" ]; then
    # Apple Silicon — HVF gives near-native speed for aarch64 guests
    ACCEL="-accel hvf"
    ACCEL_NAME="HVF (Apple Silicon — native)"
    CPU_MODEL="-cpu host"
elif [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL="-accel kvm"
    ACCEL_NAME="KVM (Linux)"
    CPU_MODEL="-cpu host"
fi

# Detect host terminal size and pass it to the guest (the serial TUI's size).
# Without a terminal on stdin stty fails: `|| true` keeps that failure from
# ending the script under `set -e -o pipefail`, so the 80x24 below applies.
HOST_COLS=$(stty size 2>/dev/null | awk '{print $2}' || true)
HOST_ROWS=$(stty size 2>/dev/null | awk '{print $1}' || true)
HOST_COLS=${HOST_COLS:-80}
HOST_ROWS=${HOST_ROWS:-24}

# The knobs, the same on both paths (embrad reads the last occurrence of
# each); see run-qemu.sh for what each one does.
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
# EMBRA_TUI="1,") would be ignored in silence.
for switch in EMBRA_TUI EMBRA_DB_VERBOSE; do
    case "${!switch:-}" in
        ""|0|1) ;;
        *) echo "WARNING: $switch='${!switch}' is not 1 and is ignored (the switch is $switch=1)" >&2 ;;
    esac
done

# The user-mode network: the forwarded ports, and EMBRA_DHCP_START=<address>
# as the first address QEMU's DHCP server leases (see run-qemu.sh).
NIC="user,hostfwd=tcp:127.0.0.1:50000-:50000,hostfwd=tcp:127.0.0.1:8443-:8443,hostfwd=tcp::3345-:3345"
if [ -n "${EMBRA_DHCP_START:-}" ]; then
    if ! [[ "$EMBRA_DHCP_START" =~ ^[0-9]{1,3}(\.[0-9]{1,3}){3}$ ]]; then
        echo "ERROR: EMBRA_DHCP_START must be an IPv4 address, got '$EMBRA_DHCP_START'" >&2; exit 2
    fi
    NIC="$NIC,dhcpstart=$EMBRA_DHCP_START"
    echo "  DHCP: QEMU leases from $EMBRA_DHCP_START (EMBRA_DHCP_START)"
fi

# find_firmware: FW_CODE and FW_VARS for the UEFI path. The virt machine
# wants 64 MiB flash images; the edk2 and AAVMF pairs are padded so.
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
        "${brew_share}/edk2-aarch64-code.fd:${brew_share}/edk2-arm-vars.fd" \
        "/usr/share/AAVMF/AAVMF_CODE.fd:/usr/share/AAVMF/AAVMF_VARS.fd" \
        "/usr/share/qemu/edk2-aarch64-code.fd:/usr/share/qemu/edk2-arm-vars.fd" \
        "/usr/share/edk2/aarch64/QEMU_EFI-pflash.raw:/usr/share/edk2/aarch64/vars-template-pflash.raw"; do
        code="${pair%%:*}"
        vars="${pair#*:}"
        if [ -f "$code" ] && [ -f "$vars" ]; then
            FW_CODE="$code"
            FW_VARS="$vars"
            return
        fi
    done
    echo "ERROR: no UEFI firmware found for the aarch64 guest." >&2
    echo "  macOS:         brew install qemu                 (\$(brew --prefix)/share/qemu/edk2-aarch64-code.fd)" >&2
    echo "  Ubuntu/Debian: sudo apt-get install qemu-efi-aarch64   (/usr/share/AAVMF/AAVMF_CODE.fd)" >&2
    echo "  Or name the pair: EMBRA_FW_CODE=<code.fd> EMBRA_FW_VARS=<vars.fd> ./scripts/run-qemu-aarch64.sh" >&2
    echo "  Or boot without firmware: EMBRA_BOOT=direct ./scripts/run-qemu-aarch64.sh" >&2
    exit 1
}

echo "Starting embraOS (aarch64) in QEMU..."
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

if [ "$EMBRA_BOOT" = "direct" ]; then
    # Kernel and initramfs from the host, as before GRUB.
    IMAGE_DIR="$(dirname "$IMAGE")"
    KERNEL="${IMAGE_DIR}/${KERNEL_NAME}"
    if [ ! -f "$KERNEL" ]; then
        for k in "buildroot-src/output/images/${KERNEL_NAME}" "output/images/${KERNEL_NAME}"; do
            if [ -f "$k" ]; then KERNEL="$k"; break; fi
        done
    fi
    INITRD="initramfs.cpio.gz"
    if [ ! -f "$INITRD" ]; then
        echo "ERROR: initramfs.cpio.gz not found. Run ./scripts/create_initramfs.sh first."
        exit 1
    fi
    if [ ! -f "$KERNEL" ]; then
        echo "ERROR: ${KERNEL_NAME} not found. Build with ./scripts/build-image-aarch64.sh first."
        exit 1
    fi
    WEB_CMDLINE="embra.web=1"
    if [ "${EMBRA_TUI:-}" = "1" ]; then
        WEB_CMDLINE=""
    fi
    echo "  Kernel: $KERNEL"
    echo "  Initrd: $INITRD"
    echo ""
    echo "Press Ctrl-A X to exit QEMU"
    echo ""
    # ARM64 virt machine uses PL011 UART (ttyAMA0), not 8250 (ttyS0).
    qemu-system-aarch64 \
        -machine virt \
        $ACCEL \
        $CPU_MODEL \
        -m "$MEMORY" \
        -smp "$CPUS" \
        -drive file="$IMAGE",format=raw,if=virtio \
        -kernel "$KERNEL" \
        -initrd "$INITRD" \
        -append "console=ttyAMA0 ro quiet embra.slot=a $WEB_CMDLINE $KNOBS" \
        -nographic \
        -serial mon:stdio \
        -nic "$NIC" \
        -no-reboot
    exit $?
fi

find_firmware
VARS_SCRATCH="$(mktemp "${TMPDIR:-/tmp}/embraos-vars.XXXXXX")"
cp "$FW_VARS" "$VARS_SCRATCH"
trap 'rm -f "$VARS_SCRATCH"' EXIT
SMBIOS_VALUE="${KNOBS//,/,,}"
echo "  Firmware: $FW_CODE (vars copied per run)"
echo "  GRUB: the menu below; the host knobs ride the SMBIOS string: $KNOBS"
echo ""
echo "Press Ctrl-A X to exit QEMU"
echo ""

qemu-system-aarch64 \
    -machine virt \
    $ACCEL \
    $CPU_MODEL \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -drive if=pflash,format=raw,readonly=on,file="$FW_CODE" \
    -drive if=pflash,format=raw,file="$VARS_SCRATCH" \
    -drive file="$IMAGE",format=raw,if=virtio \
    -smbios "type=11,value=${SMBIOS_VALUE}" \
    -nographic \
    -serial mon:stdio \
    -nic "$NIC"
