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
#   ./scripts/embraos-backup-mac.sh --image ~/images/embraos.img verify
#
# Requires: Docker (OrbStack or Docker Desktop)
#
# Image resolution (host side): --image <path> → $EMBRAOS_IMAGE →
#   <root>/buildroot-src/output/images/embraos.img → <root>/output/images/embraos.img
#   (the precedence seed-state.sh, run-qemu.sh and embraos-backup.sh share).
#   The host path is translated to a container path — bind-mounted read-write
#   when it lives outside the project root — and handed to the inner script as
#   --image. EMBRAOS_IMAGE is consumed here and never forwarded with -e: a
#   macOS host path is meaningless inside the container.
#
# Environment:
#   EMBRAOS_IMAGE       Path to embraos.img (overridden by --image)
#   EMBRAOS_BACKUP_DIR  Backup storage directory (default: ~/embraOS_BACKUPS)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
EMBRAOS_ROOT="$(dirname "$SCRIPT_DIR")"
BACKUP_DIR="${EMBRAOS_BACKUP_DIR:-${HOME}/embraOS_BACKUPS}"

RED='\033[0;31m'
NC='\033[0m'
die() { echo -e "${RED}[ERROR]${NC} $*" >&2; exit 1; }

# --- Global option: --image (everything else passes through to the inner script)
IMAGE_ARG=""
ARGS=()
while [ $# -gt 0 ]; do
    case "$1" in
        --image)   [ $# -ge 2 ] || die "--image requires a value"
                   IMAGE_ARG="$2"; shift 2 ;;
        --image=*) IMAGE_ARG="${1#--image=}"; shift ;;
        *)         ARGS+=("$1"); shift ;;
    esac
done
set -- ${ARGS[@]+"${ARGS[@]}"}

COMMAND="${1:-help}"

if [ "$COMMAND" = "help" ] || [ "$COMMAND" = "--help" ] || [ "$COMMAND" = "-h" ]; then
    echo "embraos-backup-mac.sh — macOS wrapper for embraOS backup/restore"
    echo ""
    echo "Runs embraos-backup.sh inside a Docker container with loop device"
    echo "access for mounting disk image partitions."
    echo ""
    echo "Usage:"
    echo "  $0 [--image PATH] backup [--label NAME]     Backup STATE + DATA from disk image"
    echo "  $0 [--image PATH] restore [BACKUP_NAME]     Restore into disk image (latest if no name)"
    echo "  $0 list                                     List available backups"
    echo "  $0 [--image PATH] verify                    Check disk image has valid data"
    echo ""
    echo "Options:"
    echo "  --image PATH        Disk image to operate on (default: auto-detected under the project;"
    echo "                      a path outside it is bind-mounted into the container)"
    echo ""
    echo "Environment:"
    echo "  EMBRAOS_IMAGE       Same as --image"
    echo "  EMBRAOS_BACKUP_DIR  Backup storage directory (default: ~/embraOS_BACKUPS)"
    echo ""
    echo "The VM must be stopped before backup or restore."
    echo "Docker (OrbStack or Docker Desktop) must be running."
    exit 0
fi

# Ensure backup directory exists on host
mkdir -p "$BACKUP_DIR"

# Check QEMU isn't running (catch it early on the host side)
if pgrep -f "qemu.*embraos" > /dev/null 2>&1; then
    die "QEMU appears to be running with this image
  Stop the VM before backup/restore to avoid data corruption"
fi

# Verify Docker is available
if ! command -v docker &>/dev/null; then
    die "Docker not found. Install OrbStack or Docker Desktop."
fi

if ! docker info &>/dev/null 2>&1; then
    die "Docker is not running. Start OrbStack or Docker Desktop."
fi

# --- Image resolution (host side) ---------------------------------------
if [ -n "$IMAGE_ARG" ]; then
    IMAGE="$IMAGE_ARG"; IMAGE_SOURCE="--image"
elif [ -n "${EMBRAOS_IMAGE:-}" ]; then
    IMAGE="$EMBRAOS_IMAGE"; IMAGE_SOURCE="\$EMBRAOS_IMAGE"
elif [ -f "${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img"; IMAGE_SOURCE="buildroot-src (freshest build)"
elif [ -f "${EMBRAOS_ROOT}/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/output/images/embraos.img"; IMAGE_SOURCE="output/images"
else
    # `list` needs no image; for every other command the inner script
    # reports the missing image with its own guidance.
    IMAGE=""; IMAGE_SOURCE=""
fi

no_colon() {
    case "$1" in
        *:*) die "path contains ':', which docker -v cannot express: $1" ;;
    esac
}
no_colon "$EMBRAOS_ROOT"
no_colon "$BACKUP_DIR"

# Run the backup script inside a privileged Linux container
# --privileged: required for loop device mounting (mount -o loop)
# Volume mounts:
#   /work         → project root (scripts, and the image when it lives there)
#   /backups      → host backup directory (persists across container runs)
#   /mnt/image    → the image's directory, only when it lives outside the project
# Environment:
#   EMBRAOS_BACKUP_DIR  → /backups (mapped location inside container)
#   EMBRAOS_ROOT        → /work (project root inside container)
DOCKER_ARGS=(--rm -it --privileged
    -v "${EMBRAOS_ROOT}:/work"
    -v "${BACKUP_DIR}:/backups"
    -e EMBRAOS_BACKUP_DIR=/backups
    -e EMBRAOS_ROOT=/work
    -e DEBIAN_FRONTEND=noninteractive)
INNER_ARGS=()

if [ -n "$IMAGE" ]; then
    [ -f "$IMAGE" ] || die "image not found (via ${IMAGE_SOURCE}): $IMAGE"
    IMG_ABS="$(cd "$(dirname "$IMAGE")" && pwd -P)/$(basename "$IMAGE")"
    no_colon "$IMG_ABS"
    case "$IMG_ABS" in
        "$EMBRAOS_ROOT"/*)
            IMG_CPATH="/work/${IMG_ABS#"$EMBRAOS_ROOT"/}"
            IMG_NOTE="" ;;
        *)
            # Image: read-write. Bind its directory and keep the basename.
            DOCKER_ARGS+=(-v "$(dirname "$IMG_ABS"):/mnt/image")
            IMG_CPATH="/mnt/image/$(basename "$IMG_ABS")"
            IMG_NOTE="   (bind mount, read-write)" ;;
    esac
    INNER_ARGS+=(--image "$IMG_CPATH")
    echo "Image: ${IMG_ABS}  (from ${IMAGE_SOURCE})"
    echo "   →   ${IMG_CPATH}${IMG_NOTE}"
fi

docker run "${DOCKER_ARGS[@]}" \
    ubuntu:24.04 \
    bash -c 'apt-get update -qq && apt-get install -y -qq rsync fdisk python3 && exec /work/scripts/embraos-backup.sh "$@"' \
    -- ${INNER_ARGS[@]+"${INNER_ARGS[@]}"} "$@"
