#!/bin/bash
# embraos-backup.sh — Backup and restore STATE and DATA partitions across image rebuilds
#
# Usage:
#   ./embraos-backup.sh backup                    # Backup from current disk image
#   ./embraos-backup.sh backup --label "pre-rebuild"  # Backup with a label
#   ./embraos-backup.sh restore                   # Restore most recent backup into disk image
#   ./embraos-backup.sh restore 2026-03-29_1430   # Restore a specific backup
#   ./embraos-backup.sh list                      # List available backups
#   ./embraos-backup.sh verify                    # Verify current disk image has valid data
#
# Prerequisites:
#   - Must run as root (or with sudo) for loop mount
#   - VM must be STOPPED (partitions can't be mounted while QEMU has the image open)
#   - mount, rsync, fdisk, sha256sum
#
# What gets backed up:
#   STATE partition (/dev/vda3 → /embra/state):
#     - soul.sha256 (soul hash)
#     - pki/ (Root CA, service certs)
#     - halt_reason (if present)
#
#   DATA partition (/dev/vda4 → /embra/data):
#     - wardsondb/ (all collections: soul, identity, memory, sessions, config, tools, etc.)
#       This is the fjall data directory — SST segments, WAL, partition files
#
# Ordering: WardSONDB does NOT need to be running. This is a file-level backup.
# fjall picks up the data directory on next start. The files are crash-consistent
# as long as the VM was shut down cleanly (embrad sends SIGTERM → WardSONDB flushes).

set -euo pipefail

# --- Configuration ---
SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EMBRAOS_ROOT="${EMBRAOS_ROOT:-$(dirname "$SCRIPT_DIR")}"

# Find image — same logic as run-qemu.sh: buildroot output first, then output/images
if [ -n "${EMBRAOS_IMAGE:-}" ]; then
    IMAGE="$EMBRAOS_IMAGE"
elif [ -f "${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img"
elif [ -f "${EMBRAOS_ROOT}/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/output/images/embraos.img"
else
    IMAGE=""  # Will be caught by check_image()
fi
# Resolve the real user's home even under sudo (sudo sets HOME to /root)
REAL_HOME="${HOME}"
if [ -n "${SUDO_USER:-}" ]; then
    REAL_HOME=$(getent passwd "$SUDO_USER" | cut -d: -f6)
fi
BACKUP_DIR="${EMBRAOS_BACKUP_DIR:-${REAL_HOME}/embraOS_BACKUPS}"
mkdir -p "$BACKUP_DIR"
# Ensure the backup dir is owned by the real user, not root
if [ -n "${SUDO_USER:-}" ]; then
    chown "${SUDO_USER}:${SUDO_USER}" "$BACKUP_DIR"
fi

# Partition numbers in the GPT layout (from genimage.cfg)
# Partition 1 = boot, 2 = rootfs (SquashFS), 3 = STATE, 4 = DATA
STATE_PART_NUM=3
DATA_PART_NUM=4
SECTOR_SIZE=512

# Colors
RED='\033[0;31m'
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
CYAN='\033[0;36m'
NC='\033[0m'

# --- Helper Functions ---

log_info()  { echo -e "${GREEN}[INFO]${NC}  $*"; }
log_warn()  { echo -e "${YELLOW}[WARN]${NC}  $*"; }
log_error() { echo -e "${RED}[ERROR]${NC} $*"; }
log_step()  { echo -e "${CYAN}[STEP]${NC}  $*"; }

check_root() {
    if [ "$(id -u)" -ne 0 ]; then
        log_error "This script must be run as root (need loop mount access)"
        echo "  Usage: sudo $0 $*"
        exit 1
    fi
}

check_image() {
    if [ -z "$IMAGE" ] || [ ! -f "$IMAGE" ]; then
        log_error "Disk image not found"
        echo "  Searched: buildroot-src/output/images/embraos.img"
        echo "            output/images/embraos.img"
        echo "  Set EMBRAOS_IMAGE to override"
        exit 1
    fi
    log_info "Using image: $IMAGE"
}

check_vm_stopped() {
    if pgrep -f "qemu.*embraos" > /dev/null 2>&1; then
        log_error "QEMU appears to be running with this image"
        echo "  Stop the VM before backup/restore to avoid data corruption"
        echo "  Run: Ctrl-A X in the QEMU console, or kill the QEMU process"
        exit 1
    fi
}

# Parse the GPT partition table to get byte offset and size for a partition.
# losetup --partscan mishandles non-standard GPT layouts (e.g., partitions
# starting at sector 34 instead of 2048), so we read the table with fdisk
# and mount directly with -o loop,offset=X,sizelimit=Y.
#
# Usage: get_partition_geometry <part_num>
# Sets: PART_OFFSET (bytes), PART_SIZE (bytes)
get_partition_geometry() {
    local part_num=$1

    # fdisk -l outputs lines like:
    #   output/images/embraos.img3 158730  683017  524288  256M Linux filesystem
    # We need the start sector and sector count (columns 2 and 4)
    local line
    line=$(fdisk -l "$IMAGE" 2>/dev/null | grep "\.img${part_num} " || true)

    if [ -z "$line" ]; then
        # Try alternate format without .img prefix
        line=$(fdisk -l "$IMAGE" 2>/dev/null | grep "${part_num} " | tail -1 || true)
    fi

    if [ -z "$line" ]; then
        log_error "Could not find partition $part_num in disk image"
        fdisk -l "$IMAGE" 2>/dev/null
        exit 1
    fi

    # Extract start sector and sector count
    # fdisk GPT output: Device Start End Sectors Size Type
    local start_sector sectors
    start_sector=$(echo "$line" | awk '{print $2}')
    sectors=$(echo "$line" | awk '{print $4}')

    PART_OFFSET=$((start_sector * SECTOR_SIZE))
    PART_SIZE=$((sectors * SECTOR_SIZE))

    log_info "Partition $part_num: offset=${PART_OFFSET} (sector ${start_sector}), size=${PART_SIZE} (${sectors} sectors)"
}

# Mount a partition from the disk image using calculated offset.
# Usage: mount_partition <part_num> <mount_point>
mount_partition() {
    local part_num=$1
    local mount_point=$2

    mkdir -p "$mount_point"

    get_partition_geometry "$part_num"

    mount -o loop,offset=${PART_OFFSET},sizelimit=${PART_SIZE} "$IMAGE" "$mount_point"
    log_info "Mounted partition ${part_num} → ${mount_point}"
}

cleanup_mounts() {
    local mount_point
    for mount_point in "${MOUNT_STATE:-}" "${MOUNT_DATA:-}"; do
        if [ -n "$mount_point" ] && mountpoint -q "$mount_point" 2>/dev/null; then
            umount "$mount_point"
            log_info "Unmounted $mount_point"
        fi
    done
}

cleanup() {
    cleanup_mounts
    # Remove temp mount points
    rmdir "${MOUNT_STATE:-/nonexistent}" 2>/dev/null || true
    rmdir "${MOUNT_DATA:-/nonexistent}" 2>/dev/null || true
}

trap cleanup EXIT

# --- Backup ---

do_backup() {
    local label="${1:-}"
    local timestamp=$(date +%Y-%m-%d_%H%M%S)
    local backup_name="${timestamp}"
    if [ -n "$label" ]; then
        backup_name="${timestamp}_${label}"
    fi
    local backup_path="${BACKUP_DIR}/${backup_name}"

    check_root
    check_image
    check_vm_stopped

    log_step "Creating backup: ${backup_name}"
    mkdir -p "${backup_path}/state"
    mkdir -p "${backup_path}/data"

    # Mount partitions
    MOUNT_STATE=$(mktemp -d /tmp/embra-state.XXXXXX)
    MOUNT_DATA=$(mktemp -d /tmp/embra-data.XXXXXX)

    mount_partition $STATE_PART_NUM "$MOUNT_STATE"
    mount_partition $DATA_PART_NUM "$MOUNT_DATA"

    # --- Backup STATE ---
    log_step "Backing up STATE partition..."
    rsync -a --info=progress2 "${MOUNT_STATE}/" "${backup_path}/state/"

    local state_files=$(find "${backup_path}/state" -type f | wc -l)
    local state_size=$(du -sh "${backup_path}/state" | cut -f1)
    log_info "STATE: ${state_files} files, ${state_size}"

    # Check for soul hash
    if [ -f "${backup_path}/state/soul.sha256" ]; then
        local soul_hash=$(cat "${backup_path}/state/soul.sha256")
        log_info "Soul hash: ${soul_hash:0:16}..."
    else
        log_warn "No soul hash found (first-run state or missing)"
    fi

    # Check for PKI
    if [ -d "${backup_path}/state/pki" ]; then
        local pki_files=$(find "${backup_path}/state/pki" -type f | wc -l)
        log_info "PKI: ${pki_files} files"
    fi

    # --- Backup DATA ---
    log_step "Backing up DATA partition..."
    rsync -a --info=progress2 "${MOUNT_DATA}/" "${backup_path}/data/"

    local data_files=$(find "${backup_path}/data" -type f | wc -l)
    local data_size=$(du -sh "${backup_path}/data" | cut -f1)
    log_info "DATA: ${data_files} files, ${data_size}"

    # Count WardSONDB collections (fjall stores each as a partition under partitions/)
    if [ -d "${backup_path}/data/wardsondb" ]; then
        local partitions_dir="${backup_path}/data/wardsondb/partitions"
        if [ -d "$partitions_dir" ]; then
            # Count collection partitions (named like "soul.invariant#docs", "memory.entries#docs")
            local collection_count=$(find "$partitions_dir" -maxdepth 1 -type d -name "*#docs" | wc -l)
            log_info "WardSONDB: ${collection_count} collection partitions"
        else
            log_warn "WardSONDB data directory exists but no partitions/ subdirectory"
        fi
    else
        log_warn "No WardSONDB data directory found"
    fi

    # --- Backup workspace (if it exists on DATA) ---
    if [ -d "${MOUNT_DATA}/workspace" ] || [ -d "${MOUNT_DATA}/../embra/workspace" ]; then
        local ws_path="${MOUNT_DATA}/workspace"
        if [ -d "$ws_path" ]; then
            mkdir -p "${backup_path}/workspace"
            rsync -a --info=progress2 "${ws_path}/" "${backup_path}/workspace/"
            local ws_size=$(du -sh "${backup_path}/workspace" | cut -f1)
            log_info "Workspace: ${ws_size}"
        fi
    fi

    # --- Metadata ---
    cat > "${backup_path}/backup.json" << EOF
{
    "timestamp": "${timestamp}",
    "label": "${label}",
    "image": "$(basename "$IMAGE")",
    "image_sha256": "$(sha256sum "$IMAGE" | cut -d' ' -f1)",
    "state_files": ${state_files},
    "state_size": "${state_size}",
    "data_files": ${data_files},
    "data_size": "${data_size}",
    "hostname": "$(hostname)",
    "created_by": "embraos-backup.sh"
}
EOF

    # Unmount
    cleanup_mounts

    # Fix ownership so the real user can access backups without sudo
    if [ -n "${SUDO_USER:-}" ]; then
        chown -R "${SUDO_USER}:${SUDO_USER}" "${backup_path}"
    fi

    log_info ""
    log_info "═══════════════════════════════════════════════════"
    log_info " Backup complete: ${backup_name}"
    log_info " Location: ${backup_path}"
    log_info " STATE: ${state_size} (${state_files} files)"
    log_info " DATA:  ${data_size} (${data_files} files)"
    log_info "═══════════════════════════════════════════════════"
}

# --- Restore ---

do_restore() {
    local target="${1:-}"

    check_root
    check_image
    check_vm_stopped

    # Find backup to restore
    if [ -z "$target" ]; then
        # Use most recent backup
        target=$(ls -1d "${BACKUP_DIR}"/2* 2>/dev/null | sort -r | head -1)
        if [ -z "$target" ]; then
            log_error "No backups found in ${BACKUP_DIR}"
            exit 1
        fi
        target=$(basename "$target")
    fi

    local backup_path="${BACKUP_DIR}/${target}"
    if [ ! -d "$backup_path" ]; then
        log_error "Backup not found: ${backup_path}"
        echo "  Available backups:"
        do_list
        exit 1
    fi

    log_step "Restoring from: ${target}"

    if [ -f "${backup_path}/backup.json" ]; then
        log_info "Backup metadata:"
        cat "${backup_path}/backup.json" | python3 -m json.tool 2>/dev/null || cat "${backup_path}/backup.json"
        echo ""
    fi

    # Confirm
    echo -e "${YELLOW}This will OVERWRITE the STATE and DATA partitions in:${NC}"
    echo "  ${IMAGE}"
    echo ""
    read -p "Continue? [y/N] " confirm
    if [ "${confirm,,}" != "y" ]; then
        log_info "Restore cancelled"
        exit 0
    fi

    # Mount partitions
    MOUNT_STATE=$(mktemp -d /tmp/embra-state.XXXXXX)
    MOUNT_DATA=$(mktemp -d /tmp/embra-data.XXXXXX)

    mount_partition $STATE_PART_NUM "$MOUNT_STATE"
    mount_partition $DATA_PART_NUM "$MOUNT_DATA"

    # --- Restore STATE ---
    if [ -d "${backup_path}/state" ]; then
        log_step "Restoring STATE partition..."

        # Clear existing state (but preserve the directory structure)
        find "$MOUNT_STATE" -mindepth 1 -delete 2>/dev/null || true

        rsync -a --info=progress2 "${backup_path}/state/" "${MOUNT_STATE}/"

        local state_files=$(find "$MOUNT_STATE" -type f | wc -l)
        log_info "STATE restored: ${state_files} files"

        if [ -f "${MOUNT_STATE}/soul.sha256" ]; then
            log_info "Soul hash present: $(cat "${MOUNT_STATE}/soul.sha256" | head -c 16)..."
        fi
    else
        log_warn "No STATE data in backup — skipping"
    fi

    # --- Restore DATA ---
    if [ -d "${backup_path}/data" ]; then
        log_step "Restoring DATA partition..."

        # Clear existing data
        # IMPORTANT: This removes any seed data from a fresh image build
        find "$MOUNT_DATA" -mindepth 1 -delete 2>/dev/null || true

        rsync -a --info=progress2 "${backup_path}/data/" "${MOUNT_DATA}/"

        local data_files=$(find "$MOUNT_DATA" -type f | wc -l)
        log_info "DATA restored: ${data_files} files"
    else
        log_warn "No DATA in backup — skipping"
    fi

    # --- Restore workspace ---
    if [ -d "${backup_path}/workspace" ]; then
        log_step "Restoring workspace..."
        local ws_target="${MOUNT_DATA}/workspace"
        mkdir -p "$ws_target"
        rsync -a --info=progress2 "${backup_path}/workspace/" "${ws_target}/"
        log_info "Workspace restored"
    fi

    # Sync to disk
    sync

    # Unmount
    cleanup_mounts

    log_info ""
    log_info "═══════════════════════════════════════════════════"
    log_info " Restore complete from: ${target}"
    log_info ""
    log_info " Next steps:"
    log_info "   1. Boot the VM:  ./scripts/run-qemu.sh"
    log_info "   2. Verify data:  Embra should show reconnection briefing"
    log_info "   3. Run checks:   /status, /sessions, then ask Embra"
    log_info "      to run memory_scan and session_list"
    log_info "═══════════════════════════════════════════════════"
}

# --- List ---

do_list() {
    if [ ! -d "$BACKUP_DIR" ] || [ -z "$(ls -A "$BACKUP_DIR" 2>/dev/null)" ]; then
        log_info "No backups found in ${BACKUP_DIR}"
        return
    fi

    echo ""
    echo -e "${CYAN}Available backups:${NC}"
    echo "─────────────────────────────────────────────────────────"
    printf "%-28s  %-10s  %-10s  %s\n" "NAME" "STATE" "DATA" "LABEL"
    echo "─────────────────────────────────────────────────────────"

    for dir in $(ls -1d "${BACKUP_DIR}"/2* 2>/dev/null | sort -r); do
        local name=$(basename "$dir")
        local state_size="—"
        local data_size="—"
        local label=""

        if [ -d "${dir}/state" ]; then
            state_size=$(du -sh "${dir}/state" 2>/dev/null | cut -f1)
        fi
        if [ -d "${dir}/data" ]; then
            data_size=$(du -sh "${dir}/data" 2>/dev/null | cut -f1)
        fi
        if [ -f "${dir}/backup.json" ]; then
            label=$(python3 -c "import json; print(json.load(open('${dir}/backup.json')).get('label',''))" 2>/dev/null || echo "")
        fi

        printf "%-28s  %-10s  %-10s  %s\n" "$name" "$state_size" "$data_size" "$label"
    done
    echo ""
}

# --- Verify ---

do_verify() {
    check_root
    check_image

    log_step "Verifying disk image: $(basename "$IMAGE")"

    MOUNT_STATE=$(mktemp -d /tmp/embra-state.XXXXXX)
    MOUNT_DATA=$(mktemp -d /tmp/embra-data.XXXXXX)

    mount_partition $STATE_PART_NUM "$MOUNT_STATE"
    mount_partition $DATA_PART_NUM "$MOUNT_DATA"

    local all_ok=true

    echo ""
    echo -e "${CYAN}STATE partition:${NC}"

    # Soul hash
    if [ -f "${MOUNT_STATE}/soul.sha256" ]; then
        local hash=$(cat "${MOUNT_STATE}/soul.sha256")
        echo -e "  Soul hash:    ${GREEN}present${NC} (${hash:0:16}...)"
    else
        echo -e "  Soul hash:    ${YELLOW}missing${NC} (first-run or unseeded)"
    fi

    # PKI
    if [ -d "${MOUNT_STATE}/pki" ]; then
        local ca_cert="${MOUNT_STATE}/pki/ca.crt"
        if [ -f "$ca_cert" ]; then
            echo -e "  Root CA:      ${GREEN}present${NC}"
        else
            echo -e "  Root CA:      ${YELLOW}missing${NC}"
        fi
        local pki_count=$(find "${MOUNT_STATE}/pki" -type f | wc -l)
        echo -e "  PKI files:    ${pki_count}"
    else
        echo -e "  PKI:          ${YELLOW}not initialized${NC}"
    fi

    echo ""
    echo -e "${CYAN}DATA partition:${NC}"

    # WardSONDB
    if [ -d "${MOUNT_DATA}/wardsondb" ]; then
        local db_size=$(du -sh "${MOUNT_DATA}/wardsondb" | cut -f1)
        local db_files=$(find "${MOUNT_DATA}/wardsondb" -type f | wc -l)
        local collection_count=$(find "${MOUNT_DATA}/wardsondb/partitions" -maxdepth 1 -type d -name "*#docs" 2>/dev/null | wc -l)
        echo -e "  WardSONDB:    ${GREEN}present${NC} (${db_size}, ${db_files} files, ${collection_count} collections)"

        # Check for key collection partitions (fjall: partitions/{name}#docs/)
        local pdir="${MOUNT_DATA}/wardsondb/partitions"
        local has_soul=$(find "$pdir" -maxdepth 1 -type d -name "soul.invariant#docs" 2>/dev/null | head -1)
        local has_sessions=$(find "$pdir" -maxdepth 1 -type d -name "sessions.*#docs" 2>/dev/null | head -1)
        local has_memory=$(find "$pdir" -maxdepth 1 -type d -name "memory.*#docs" 2>/dev/null | head -1)
        local has_config=$(find "$pdir" -maxdepth 1 -type d -name "config.*#docs" 2>/dev/null | head -1)

        [ -n "$has_soul" ]     && echo -e "    soul.invariant:  ${GREEN}✓${NC}" || echo -e "    soul.invariant:  ${YELLOW}✗${NC}"
        [ -n "$has_config" ]   && echo -e "    config.system:   ${GREEN}✓${NC}" || echo -e "    config.system:   ${YELLOW}✗${NC}"
        [ -n "$has_memory" ]   && echo -e "    memory.*:        ${GREEN}✓${NC}" || echo -e "    memory.*:        ${YELLOW}✗${NC}"
        [ -n "$has_sessions" ] && echo -e "    sessions.*:      ${GREEN}✓${NC}" || echo -e "    sessions.*:      ${YELLOW}✗${NC}"
    else
        echo -e "  WardSONDB:    ${RED}missing${NC}"
        all_ok=false
    fi

    # Workspace
    if [ -d "${MOUNT_DATA}/workspace" ]; then
        local ws_size=$(du -sh "${MOUNT_DATA}/workspace" | cut -f1)
        echo -e "  Workspace:    ${GREEN}present${NC} (${ws_size})"
    else
        echo -e "  Workspace:    ${YELLOW}empty${NC}"
    fi

    # Unmount
    cleanup_mounts

    echo ""
    if $all_ok; then
        log_info "Verification passed — image appears to have valid data"
    else
        log_warn "Verification found issues — see above"
    fi
}

# --- Main ---

COMMAND="${1:-help}"
shift || true

case "$COMMAND" in
    backup)
        LABEL=""
        while [ $# -gt 0 ]; do
            case "$1" in
                --label) LABEL="$2"; shift 2 ;;
                *) LABEL="$1"; shift ;;
            esac
        done
        do_backup "$LABEL"
        ;;
    restore)
        do_restore "${1:-}"
        ;;
    list)
        do_list
        ;;
    verify)
        do_verify
        ;;
    help|--help|-h)
        echo "embraos-backup.sh — Backup and restore embraOS STATE and DATA partitions"
        echo ""
        echo "Usage:"
        echo "  sudo $0 backup [--label NAME]     Backup STATE + DATA from disk image"
        echo "  sudo $0 restore [BACKUP_NAME]     Restore into disk image (latest if no name)"
        echo "  sudo $0 list                      List available backups"
        echo "  sudo $0 verify                    Check disk image has valid data"
        echo ""
        echo "Environment:"
        echo "  EMBRAOS_IMAGE       Path to embraos.img (default: buildroot-src/output/images/ then output/images/)"
        echo "  EMBRAOS_BACKUP_DIR  Backup storage directory (default: ~/embraOS_BACKUPS)"
        echo "  EMBRAOS_ROOT        Project root (default: parent of scripts/)"
        echo ""
        echo "Workflow:"
        echo "  1. Stop the VM"
        echo "  2. sudo $0 backup --label pre-rebuild"
        echo "  3. Rebuild the image (./scripts/build-image.sh)"
        echo "  4. sudo $0 restore"
        echo "  5. Start the VM (./scripts/run-qemu.sh)"
        echo "  6. Verify: /status, /sessions, memory_scan"
        echo ""
        echo "The VM must be stopped before backup or restore."
        echo "WardSONDB does not need to be running — this is a file-level operation."
        ;;
    *)
        log_error "Unknown command: $COMMAND"
        echo "  Run '$0 help' for usage"
        exit 1
        ;;
esac
