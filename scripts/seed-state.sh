#!/bin/bash
# Pre-seed STATE and DATA partitions for testing.
# Copies existing Phase 0 data so the system boots directly to Operational mode
# instead of going through Learning Mode.
#
# Usage: ./scripts/seed-state.sh [--phase0-data /path/to/phase0/data] [--soul-hash <hash>] [--import-dir /path/to/graphs] [--seed-dir /path/to/packs] [--ca-dir /path/to/certs]
#
# --import-dir copies *.graph.json intelligence files into STATE's
# imported-intelligence/ directory so Learning Mode offers them for import
# at first boot (they take precedence over the rootfs-baked examples on
# filename collisions).
#
# --seed-dir copies *.knowledge.json seed packs into STATE's
# seed-knowledge/ directory; the brain's boot reconcile loads them into the
# knowledge graph (STATE wins filename collisions with the rootfs-baked
# packs).
#
# --ca-dir copies *.pem / *.crt root CA certificates into STATE's
# ca-certificates/ directory; embrad merges them with the stock CA bundle
# at boot and exports GIT_SSL_CAINFO/SSL_CERT_FILE so the git tools trust
# self-hosted git servers (e.g. GitLab behind an mkcert CA).
#
# NOTE: This script requires Linux (losetup). On macOS, use a Linux VM or Docker.

set -euo pipefail

IMAGE="${1:-output/images/embraos.img}"
PHASE0_DATA=""
SOUL_HASH=""
IMPORT_DIR=""
SEED_DIR=""
CA_DIR=""

shift || true
while [ $# -gt 0 ]; do
    case "$1" in
        --phase0-data) PHASE0_DATA="$2"; shift 2 ;;
        --soul-hash) SOUL_HASH="$2"; shift 2 ;;
        --import-dir) IMPORT_DIR="$2"; shift 2 ;;
        --seed-dir) SEED_DIR="$2"; shift 2 ;;
        --ca-dir) CA_DIR="$2"; shift 2 ;;
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

# Seed importable intelligence graphs (kg-native-identity)
if [ -n "$IMPORT_DIR" ]; then
    if [ -d "$IMPORT_DIR" ] && ls "$IMPORT_DIR"/*.graph.json >/dev/null 2>&1; then
        echo "Copying intelligence graphs into STATE imported-intelligence/..."
        sudo mkdir -p "$MOUNT_STATE/imported-intelligence"
        sudo cp "$IMPORT_DIR"/*.graph.json "$MOUNT_STATE/imported-intelligence/"
        echo "Done: $(ls "$IMPORT_DIR"/*.graph.json | wc -l) file(s)."
    else
        echo "ERROR: --import-dir '$IMPORT_DIR' has no *.graph.json files"
        exit 1
    fi
fi

# Seed knowledge packs (knowledge.v1)
if [ -n "$SEED_DIR" ]; then
    if [ -d "$SEED_DIR" ] && ls "$SEED_DIR"/*.knowledge.json >/dev/null 2>&1; then
        echo "Copying seed knowledge packs into STATE seed-knowledge/..."
        sudo mkdir -p "$MOUNT_STATE/seed-knowledge"
        sudo cp "$SEED_DIR"/*.knowledge.json "$MOUNT_STATE/seed-knowledge/"
        echo "Done: $(ls "$SEED_DIR"/*.knowledge.json | wc -l) file(s)."
    else
        echo "ERROR: --seed-dir '$SEED_DIR' has no *.knowledge.json files"
        exit 1
    fi
fi

# Seed operator root CA certificates (git/OpenSSL trust for self-hosted servers)
if [ -n "$CA_DIR" ]; then
    if [ -d "$CA_DIR" ] && { ls "$CA_DIR"/*.pem >/dev/null 2>&1 || ls "$CA_DIR"/*.crt >/dev/null 2>&1; }; then
        echo "Copying operator CA certificates into STATE ca-certificates/..."
        sudo mkdir -p "$MOUNT_STATE/ca-certificates"
        sudo cp "$CA_DIR"/*.pem "$MOUNT_STATE/ca-certificates/" 2>/dev/null || true
        sudo cp "$CA_DIR"/*.crt "$MOUNT_STATE/ca-certificates/" 2>/dev/null || true
        echo "Done: $(ls "$CA_DIR"/*.pem "$CA_DIR"/*.crt 2>/dev/null | wc -l) file(s)."
    else
        echo "ERROR: --ca-dir '$CA_DIR' has no *.pem or *.crt files"
        exit 1
    fi
fi

# Create PKI directory (embra-trustd will generate CA on first run)
sudo mkdir -p "$MOUNT_STATE/pki"

echo ""
echo "Seed complete. Partitions will be unmounted on exit."
