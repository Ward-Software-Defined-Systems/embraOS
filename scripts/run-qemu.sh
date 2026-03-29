#!/bin/bash
# Launch embraOS in QEMU with hardware acceleration (auto-detected)

set -euo pipefail

IMAGE="${1:-output/images/embraos.img}"
MEMORY="2G"
CPUS="2"

if [ ! -f "$IMAGE" ]; then
    echo "ERROR: Image not found: $IMAGE"
    echo "Build it first with: ./scripts/build-image.sh"
    exit 1
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

echo "Starting embraOS in QEMU..."
echo "  Image: $IMAGE"
echo "  Memory: $MEMORY"
echo "  CPUs: $CPUS"
echo "  Acceleration: $ACCEL_NAME"
echo "  Serial console: this terminal"
echo "  Port forwards: 50000→50000 (gRPC), 8443→8443 (REST)"
echo ""
echo "Press Ctrl-A X to exit QEMU"
echo ""

qemu-system-x86_64 \
    $ACCEL \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -drive file="$IMAGE",format=raw,if=virtio \
    -kernel output/images/bzImage \
    -initrd initramfs.cpio.gz \
    -append "console=ttyS0 root=/dev/vda2 ro quiet" \
    -nographic \
    -serial mon:stdio \
    -nic user,hostfwd=tcp::50000-:50000,hostfwd=tcp::8443-:8443 \
    -no-reboot
