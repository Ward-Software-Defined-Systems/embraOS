#!/bin/bash
# Launch embraOS in QEMU — aarch64 with HVF hardware acceleration on Apple Silicon
#
# This runs at near-native speed via HVF (no TCG software emulation penalty).
# Requires: brew install qemu

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
    echo "Build it first with: ./scripts/build-image-aarch64.sh"
    exit 1
fi

# ARM64 kernel is "Image" (flat binary), not "bzImage" (x86 boot wrapper)
KERNEL_NAME="Image"

# Find kernel alongside the image
IMAGE_DIR="$(dirname "$IMAGE")"
KERNEL="${IMAGE_DIR}/${KERNEL_NAME}"
if [ ! -f "$KERNEL" ]; then
    # Fall back to other known locations
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

MEMORY="2G"
CPUS="2"

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

echo "Starting embraOS in QEMU (aarch64)..."
echo "  Image: $IMAGE"
echo "  Kernel: $KERNEL"
echo "  Initrd: $INITRD"
echo "  Memory: $MEMORY"
echo "  CPUs: $CPUS"
echo "  Acceleration: $ACCEL_NAME"
echo "  Serial console: this terminal"
echo "  Port forwards: 50000→50000 (gRPC), 8443→8443 (REST)"
echo ""
echo "Press Ctrl-A X to exit QEMU"
echo ""

# Detect host terminal size and pass to guest via kernel cmdline
HOST_COLS=$(stty size 2>/dev/null | awk '{print $2}')
HOST_ROWS=$(stty size 2>/dev/null | awk '{print $1}')
HOST_COLS=${HOST_COLS:-80}
HOST_ROWS=${HOST_ROWS:-24}

# ARM64 virt machine uses PL011 UART (ttyAMA0), not 8250 (ttyS0)
# Direct kernel boot via -kernel — no UEFI firmware needed for QEMU dev workflow
qemu-system-aarch64 \
    -machine virt \
    $ACCEL \
    $CPU_MODEL \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -drive file="$IMAGE",format=raw,if=virtio \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "console=ttyAMA0 root=/dev/vda2 ro quiet embra.cols=$HOST_COLS embra.rows=$HOST_ROWS" \
    -nographic \
    -serial mon:stdio \
    -nic user,hostfwd=tcp::50000-:50000,hostfwd=tcp::8443-:8443 \
    -no-reboot
