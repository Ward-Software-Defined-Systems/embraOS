#!/bin/bash
# embraos-backup-mac.sh — macOS wrapper for embraos-backup.sh
#
# Runs the Linux backup/restore script inside a Docker container with
# loop device access for mounting disk image partitions.
#
# Usage (same interface as embraos-backup.sh):
#   ./scripts/embraos-backup-mac.sh backup                    # Backup STATE + DATA
#   ./scripts/embraos-backup-mac.sh backup --label pre-rebuild
#   ./scripts/embraos-backup-mac.sh restore                   # Restore most recent
#   ./scripts/embraos-backup-mac.sh restore 2026-04-15_1430   # Restore specific backup
#   ./scripts/embraos-backup-mac.sh list                      # List backups
#   ./scripts/embraos-backup-mac.sh verify                    # Verify disk image
#
# Requires: Docker (OrbStack or Docker Desktop)
#
# Environment (same as embraos-backup.sh):
#   EMBRAOS_IMAGE       Path to embraos.img (default: auto-detected)
#   EMBRAOS_BACKUP_DIR  Backup storage directory (default: ~/embraOS_BACKUPS)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EMBRAOS_ROOT="$(dirname "$SCRIPT_DIR")"
BACKUP_DIR="${EMBRAOS_BACKUP_DIR:-${HOME}/embraOS_BACKUPS}"

# Ensure backup directory exists on host
mkdir -p "$BACKUP_DIR"

# Check QEMU isn't running (catch it early on the host side)
if pgrep -f "qemu.*embraos" > /dev/null 2>&1; then
    echo -e "\033[0;31m[ERROR]\033[0m QEMU appears to be running with this image"
    echo "  Stop the VM before backup/restore to avoid data corruption"
    exit 1
fi

COMMAND="${1:-help}"

if [ "$COMMAND" = "help" ] || [ "$COMMAND" = "--help" ] || [ "$COMMAND" = "-h" ]; then
    echo "embraos-backup-mac.sh — macOS wrapper for embraOS backup/restore"
    echo ""
    echo "Runs embraos-backup.sh inside a Docker container with loop device"
    echo "access for mounting disk image partitions."
    echo ""
    echo "Usage:"
    echo "  $0 backup [--label NAME]     Backup STATE + DATA from disk image"
    echo "  $0 restore [BACKUP_NAME]     Restore into disk image (latest if no name)"
    echo "  $0 list                      List available backups"
    echo "  $0 verify                    Check disk image has valid data"
    echo ""
    echo "Environment:"
    echo "  EMBRAOS_IMAGE       Path to embraos.img (default: auto-detected)"
    echo "  EMBRAOS_BACKUP_DIR  Backup storage directory (default: ~/embraOS_BACKUPS)"
    echo ""
    echo "The VM must be stopped before backup or restore."
    echo "Docker (OrbStack or Docker Desktop) must be running."
    exit 0
fi

# Verify Docker is available
if ! command -v docker &>/dev/null; then
    echo -e "\033[0;31m[ERROR]\033[0m Docker not found. Install OrbStack or Docker Desktop."
    exit 1
fi

if ! docker info &>/dev/null 2>&1; then
    echo -e "\033[0;31m[ERROR]\033[0m Docker is not running. Start OrbStack or Docker Desktop."
    exit 1
fi

# Run the backup script inside a privileged Linux container
# --privileged: required for loop device mounting (mount -o loop)
# Volume mounts:
#   /work         → project root (contains disk image and scripts)
#   /backups      → host backup directory (persists across container runs)
# Environment:
#   EMBRAOS_BACKUP_DIR  → /backups (mapped location inside container)
#   EMBRAOS_ROOT        → /work (project root inside container)
docker run --rm -it \
    --privileged \
    -v "${EMBRAOS_ROOT}":/work \
    -v "${BACKUP_DIR}":/backups \
    -e EMBRAOS_BACKUP_DIR=/backups \
    -e EMBRAOS_ROOT=/work \
    -e DEBIAN_FRONTEND=noninteractive \
    ubuntu:24.04 \
    bash -c 'apt-get update -qq && apt-get install -y -qq rsync fdisk python3 && /work/scripts/embraos-backup.sh "$@"' -- "$@"
