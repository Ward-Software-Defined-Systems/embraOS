#!/bin/bash
# Launch embraOS in QEMU with hardware acceleration (auto-detected)

set -euo pipefail

# Find image — check buildroot output first (always freshest), then output/images
if [ -n "${1:-}" ]; then
    IMAGE="$1"
elif [ -f "buildroot-src/output/images/embraos.img" ]; then
    IMAGE="buildroot-src/output/images/embraos.img"
elif [ -f "output/images/embraos.img" ]; then
    IMAGE="output/images/embraos.img"
else
    echo "ERROR: No disk image found."
    echo "Build it first with: ./scripts/build-image.sh"
    exit 1
fi

# Find kernel and initramfs alongside the image
IMAGE_DIR="$(dirname "$IMAGE")"
KERNEL="${IMAGE_DIR}/bzImage"
if [ ! -f "$KERNEL" ]; then
    # Fall back to other known locations
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

echo "Starting embraOS in QEMU..."
echo "  Image: $IMAGE"
echo "  Kernel: $KERNEL"
echo "  Initrd: $INITRD"
echo "  Memory: $MEMORY"
echo "  CPUs: $CPUS"
echo "  Acceleration: $ACCEL_NAME"

# UI mode — web console by default (this is the embra-web branch).
# EMBRA_TUI=1 boots the serial TUI instead: with embra.web=1 absent from
# the kernel cmdline, embrad registers embra-console (Phase-1 TUI on this
# terminal) and does NOT start embra-web. No image rebuild needed.
if [ "${EMBRA_TUI:-}" = "1" ]; then
    WEB_CMDLINE=""
else
    WEB_CMDLINE="embra.web=1"
fi

# WardSONDB request-log verbosity — per-request lines are opt-in server-side
# (--verbose). EMBRA_DB_VERBOSE=1 sets embra.dbverbose=1 on the kernel
# cmdline; embrad then appends --verbose to the wardsondb args. Default off:
# the log lives on the 512M boot-wiped tmpfs. Requires a WardSONDB build
# that knows --verbose (older builds reject the flag and won't start).
if [ "${EMBRA_DB_VERBOSE:-}" = "1" ]; then
    DBLOG_CMDLINE="embra.dbverbose=1"
    echo "  WardSONDB: verbose request logging (EMBRA_DB_VERBOSE=1)"
else
    DBLOG_CMDLINE=""
fi

# Brain log filtering — EMBRA_LOG_LEVEL sets embra.loglevel=<spec> on the
# kernel cmdline; embrad forwards it to embra-brain as EMBRA_LOG
# (env-filter-style Targets spec, e.g. "info,kg::traversal=debug"; no
# spaces — kernel args are space-split). Default unset = INFO. Read the
# result from inside a session via the system_logs tool.
if [ -n "${EMBRA_LOG_LEVEL:-}" ]; then
    LOGLEVEL_CMDLINE="embra.loglevel=${EMBRA_LOG_LEVEL}"
    echo "  Brain log filter: ${EMBRA_LOG_LEVEL} (EMBRA_LOG_LEVEL)"
else
    LOGLEVEL_CMDLINE=""
fi

# Serial-console graphics — EMBRA_GRAPHICS sets embra.graphics=<mode> on
# the kernel cmdline; embrad forwards it to the serial embra-console as
# EMBRA_TUI_GRAPHICS (the protocol its in-TUI media pane renders with):
# halfblocks (default: plain cells, any terminal), sixel / kitty / iterm2
# (opt in when THIS terminal supports the protocol — iTerm2, WezTerm,
# kitty, foot), auto (query the terminal; 1 s timeout), off. Only the
# serial TUI (EMBRA_TUI=1) is affected — the web console's PTY is sixel.
if [ -n "${EMBRA_GRAPHICS:-}" ]; then
    GRAPHICS_CMDLINE="embra.graphics=${EMBRA_GRAPHICS}"
    echo "  Serial graphics: ${EMBRA_GRAPHICS} (EMBRA_GRAPHICS)"
else
    GRAPHICS_CMDLINE=""
fi

echo "  Serial console: this terminal"
if [ -n "$WEB_CMDLINE" ]; then
    echo "  UI mode: web console (default) — set EMBRA_TUI=1 for the serial TUI"
    echo "  Port forwards: 50000→50000 (gRPC), 8443→8443 (REST), 3345→3345 (HTTPS web)"
    echo "  Web console: https://localhost:3345/embraOS  (accept the embraOS-CA cert)"
else
    echo "  UI mode: serial TUI (EMBRA_TUI=1) — embra-web not started"
    echo "  Port forwards: 50000→50000 (gRPC), 8443→8443 (REST)"
fi
echo ""
echo "Press Ctrl-A X to exit QEMU"
echo ""

# Detect host terminal size and pass to guest via kernel cmdline
HOST_COLS=$(stty size 2>/dev/null | awk '{print $2}')
HOST_ROWS=$(stty size 2>/dev/null | awk '{print $1}')
HOST_COLS=${HOST_COLS:-80}
HOST_ROWS=${HOST_ROWS:-24}

qemu-system-x86_64 \
    $ACCEL \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -drive file="$IMAGE",format=raw,if=virtio \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "console=ttyS0 root=/dev/vda2 ro quiet embra.cols=$HOST_COLS embra.rows=$HOST_ROWS $WEB_CMDLINE $DBLOG_CMDLINE $LOGLEVEL_CMDLINE $GRAPHICS_CMDLINE" \
    -nographic \
    -serial mon:stdio \
    -nic user,hostfwd=tcp::50000-:50000,hostfwd=tcp::8443-:8443,hostfwd=tcp::3345-:3345 \
    -no-reboot
