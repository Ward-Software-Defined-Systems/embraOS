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

MEMORY="2G"
CPUS="2"

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

# embra-desktop (Stage 2): graphical session selection.
# When EMBRA_DESKTOP=1, swap the serial-only TUI launch for a GTK-windowed
# 1280x720 graphical session backed by virtio-gpu (Mesa llvmpipe path) and
# virtio keyboard + tablet input. The serial line is redirected to a file
# so embrad's stdio still has somewhere to land after embra-comp takes
# /dev/tty1. Use Ctrl-Alt-G to release the QEMU pointer grab.
DESKTOP_MODE="${EMBRA_DESKTOP:-0}"

# Detect host terminal size and pass to guest via kernel cmdline
HOST_COLS=$(stty size 2>/dev/null | awk '{print $2}')
HOST_ROWS=$(stty size 2>/dev/null | awk '{print $1}')
HOST_COLS=${HOST_COLS:-80}
HOST_ROWS=${HOST_ROWS:-24}

if [ "$DESKTOP_MODE" = "1" ]; then
    DISPLAY_ARGS=(
        -display gtk,gl=off
        -device virtio-gpu-pci,xres=1280,yres=720
        -device virtio-keyboard-pci
        -device virtio-tablet-pci
    )
    SERIAL_ARGS=(-serial "file:/tmp/embra-serial.log")
    # `embra.desktop=1` flips embrad's supervisor to register
    # embra-comp + embra-desktop in place of the serial-TTY
    # embra-console.
    KERNEL_CMDLINE="root=/dev/vda2 ro quiet embra.desktop=1"
    DISPLAY_DESC="GTK 1280x720 (virtio-gpu, llvmpipe)"
    SERIAL_DESC="/tmp/embra-serial.log"
else
    DISPLAY_ARGS=(-nographic)
    SERIAL_ARGS=(-serial mon:stdio)
    KERNEL_CMDLINE="console=ttyS0 root=/dev/vda2 ro quiet embra.cols=$HOST_COLS embra.rows=$HOST_ROWS"
    DISPLAY_DESC="serial only (-nographic)"
    SERIAL_DESC="this terminal"
fi

echo "Starting embraOS in QEMU..."
echo "  Image: $IMAGE"
echo "  Kernel: $KERNEL"
echo "  Initrd: $INITRD"
echo "  Memory: $MEMORY"
echo "  CPUs: $CPUS"
echo "  Acceleration: $ACCEL_NAME"
echo "  Display: $DISPLAY_DESC"
echo "  Serial console: $SERIAL_DESC"
echo "  Port forwards: 50000→50000 (gRPC), 8443→8443 (REST)"
echo ""
if [ "$DESKTOP_MODE" = "1" ]; then
    echo "Close the QEMU window or send SIGINT to exit."
else
    echo "Press Ctrl-A X to exit QEMU"
fi
echo ""

qemu-system-x86_64 \
    $ACCEL \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -drive file="$IMAGE",format=raw,if=virtio \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "$KERNEL_CMDLINE" \
    "${DISPLAY_ARGS[@]}" \
    "${SERIAL_ARGS[@]}" \
    -nic user,hostfwd=tcp::50000-:50000,hostfwd=tcp::8443-:8443 \
    -no-reboot
