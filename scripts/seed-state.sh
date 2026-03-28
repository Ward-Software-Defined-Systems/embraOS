#!/bin/bash
# Pre-seed STATE and DATA partitions for testing.
# Copies existing Phase 0 data so the system boots directly to Operational mode
# instead of going through Learning Mode.
#
# Usage: ./scripts/seed-state.sh [--phase0-data /path/to/phase0/data] [--soul-hash <hash>]
#
# NOTE: This script requires Linux (losetup). On macOS, use a Linux VM or Docker.

set -euo pipefail

IMAGE="${1:-output/images/embraos.img}"
PHASE0_DATA=""
SOUL_HASH=""

shift || true
while [ $# -gt 0 ]; do
    case "$1" in
        --phase0-data) PHASE0_DATA="$2"; shift 2 ;;
        --soul-hash) SOUL_HASH="$2"; shift 2 ;;
        *) echo "Unknown option: $1"; exit 1 ;;
    esac
done

if [ ! -f "$IMAGE" ]; then
    echo "ERROR: Image not found: $IMAGE"
    exit 1
fi

echo "Seeding disk image: $IMAGE"

# Set up loop device with partition scanning
LOOPDEV=$(sudo losetup --find --show --partscan "$IMAGE")
echo "Loop device: $LOOPDEV"

# Create temporary mount points
MOUNT_STATE=$(mktemp -d)
MOUNT_DATA=$(mktemp -d)

cleanup() {
    sudo umount "$MOUNT_STATE" 2>/dev/null || true
    sudo umount "$MOUNT_DATA" 2>/dev/null || true
    rmdir "$MOUNT_STATE" "$MOUNT_DATA" 2>/dev/null || true
    sudo losetup -d "$LOOPDEV" 2>/dev/null || true
}
trap cleanup EXIT

# Mount STATE (partition 3) and DATA (partition 4)
sudo mount "${LOOPDEV}p3" "$MOUNT_STATE"
sudo mount "${LOOPDEV}p4" "$MOUNT_DATA"

echo "STATE mounted at $MOUNT_STATE"
echo "DATA mounted at $MOUNT_DATA"

# Seed WardSONDB data from Phase 0
if [ -n "$PHASE0_DATA" ] && [ -d "$PHASE0_DATA" ]; then
    echo "Copying Phase 0 WardSONDB data..."
    sudo mkdir -p "$MOUNT_DATA/wardsondb"
    sudo cp -r "$PHASE0_DATA"/wardsondb/* "$MOUNT_DATA/wardsondb/" 2>/dev/null || true
    echo "Done."
else
    echo "No Phase 0 data specified (--phase0-data). Skipping WardSONDB seed."
    echo "First boot will enter Learning Mode."
fi

# Seed soul hash
if [ -n "$SOUL_HASH" ]; then
    echo "Writing soul hash to STATE..."
    echo "$SOUL_HASH" | sudo tee "$MOUNT_STATE/soul.sha256" > /dev/null
    echo "Done."
else
    echo "No soul hash specified (--soul-hash). First boot will allow Learning Mode."
fi

# Create PKI directory (embra-trustd will generate CA on first run)
sudo mkdir -p "$MOUNT_STATE/pki"

echo ""
echo "Seed complete. Partitions will be unmounted on exit."
