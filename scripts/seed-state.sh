#!/bin/bash
# Pre-seed STATE and DATA partitions for testing.
# Copies existing Phase 0 data so the system boots directly to Operational mode
# instead of going through Learning Mode.
#
# Usage: ./scripts/seed-state.sh [<image>] [--image <path>] [--phase0-data <dir>]
#                                [--soul-hash <hash>] [--import-dir <dir>]
#                                [--seed-dir <dir>] [--ca-dir <dir>]
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
# Image resolution (same precedence as run-qemu.sh and embraos-backup.sh, so
# you always seed the image QEMU will actually boot):
#   explicit arg / --image  →  $EMBRAOS_IMAGE  →  buildroot-src/output/images/
#   →  output/images/
#
# Privileges: loop-mounting needs root. Run it plainly (sudo is invoked per
# command) or under sudo — both work.
#
# NOTE: This script requires Linux. On macOS use ./scripts/seed-state-mac.sh,
# which runs this same script inside a privileged Docker container.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
EMBRAOS_ROOT="${EMBRAOS_ROOT:-$(dirname "$SCRIPT_DIR")}"

# Partition numbers from buildroot/board/embraos/genimage.cfg.in — one template
# builds both arches, so this layout is identical on x86_64 and aarch64.
# 1 = boot (vfat), 2 = rootfs (SquashFS), 3 = STATE, 4 = DATA
STATE_PART_NUM=3
DATA_PART_NUM=4
SECTOR_SIZE=512

die() { echo "ERROR: $*" >&2; exit 1; }

usage() {
    cat <<'EOF'
seed-state.sh — pre-seed STATE and DATA partitions of an embraOS disk image

Usage:
  ./scripts/seed-state.sh [<image>] [options]

Options:
  --image <path>          Disk image to seed (same as the positional argument)
  --wipe <targets>        Reformat before seeding: state | data | state,data | all
  --yes                   Skip the wipe confirmation (for non-interactive use)
  --phase0-data <dir>     Copy <dir>/wardsondb/ into DATA
  --soul-hash <hash>      Write <hash> to STATE/soul.sha256
  --import-dir <dir>      Copy *.graph.json into STATE/imported-intelligence/
  --seed-dir <dir>        Copy *.knowledge.json into STATE/seed-knowledge/
  --ca-dir <dir>          Copy *.pem / *.crt into STATE/ca-certificates/
  -h, --help              Show this help

--wipe reformats the named partitions (ext4, original labels) before any
seeding, so a clean first boot and a fresh seed are one command:

  ./scripts/seed-state.sh --wipe state,data                # back to Config Wizard
  ./scripts/seed-state.sh --wipe state,data --ca-dir <dir> # reset, then seed

Wiping STATE destroys the soul hash, PKI and API keys; wiping DATA destroys
WardSONDB — all memory, sessions and the workspace. Both are irreversible.

Environment:
  EMBRAOS_IMAGE   Disk image path (overridden by the argument/--image)
  EMBRAOS_ROOT    Project root (default: parent of scripts/)

Image is searched in this order — matching run-qemu.sh, so you seed what boots:
  argument/--image, $EMBRAOS_IMAGE,
  buildroot-src/output/images/embraos.img, output/images/embraos.img

On macOS use ./scripts/seed-state-mac.sh (same options).
EOF
}

# --- Argument parsing ---------------------------------------------------
# The image is an optional positional accepted anywhere, or --image. Both the
# documented form (`seed-state.sh <image> --ca-dir <dir>`) and a bare
# `seed-state.sh --ca-dir <dir>` work; the old code treated $1 as the image
# unconditionally, so the latter died with "Unknown option: <dir>".

IMAGE_ARG=""
PHASE0_DATA=""
SOUL_HASH=""
IMPORT_DIR=""
SEED_DIR=""
CA_DIR=""
WIPE=""
ASSUME_YES=0

need() { [ "$1" -ge 2 ] || die "$2 requires a value"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image)       need $# --image;       IMAGE_ARG="$2";   shift 2 ;;
        --wipe)        need $# --wipe;        WIPE="$2";        shift 2 ;;
        --yes|-y)      ASSUME_YES=1; shift ;;
        --phase0-data) need $# --phase0-data; PHASE0_DATA="$2"; shift 2 ;;
        --soul-hash)   need $# --soul-hash;   SOUL_HASH="$2";   shift 2 ;;
        --import-dir)  need $# --import-dir;  IMPORT_DIR="$2";  shift 2 ;;
        --seed-dir)    need $# --seed-dir;    SEED_DIR="$2";    shift 2 ;;
        --ca-dir)      need $# --ca-dir;      CA_DIR="$2";      shift 2 ;;
        -h|--help)     usage; exit 0 ;;
        -*)            echo "Unknown option: $1" >&2; echo >&2; usage >&2; exit 1 ;;
        *)             [ -z "$IMAGE_ARG" ] || die "unexpected extra argument: $1"
                       IMAGE_ARG="$1"; shift ;;
    esac
done

# --- Image resolution ---------------------------------------------------
# Same precedence as run-qemu.sh / run-qemu-aarch64.sh / embraos-backup.sh.
# An explicitly-passed relative path stays relative to the CWD; the defaults
# are anchored on EMBRAOS_ROOT so the script works from any directory.

if [ -n "$IMAGE_ARG" ]; then
    IMAGE="$IMAGE_ARG"; IMAGE_SOURCE="argument"
elif [ -n "${EMBRAOS_IMAGE:-}" ]; then
    IMAGE="$EMBRAOS_IMAGE"; IMAGE_SOURCE="\$EMBRAOS_IMAGE"
elif [ -f "${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img"; IMAGE_SOURCE="buildroot-src (freshest build)"
elif [ -f "${EMBRAOS_ROOT}/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/output/images/embraos.img"; IMAGE_SOURCE="output/images"
else
    echo "ERROR: No disk image found." >&2
    echo "  Searched: ${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img" >&2
    echo "            ${EMBRAOS_ROOT}/output/images/embraos.img" >&2
    echo "  Pass one explicitly or set EMBRAOS_IMAGE. Build with ./scripts/build-image.sh" >&2
    exit 1
fi

[ -f "$IMAGE" ] || die "Image not found: $IMAGE"

echo "Seeding disk image: $IMAGE"
echo "  (resolved from: $IMAGE_SOURCE)"

# --- Safety: refuse to seed an image a running VM has open ---------------
# Seeding an image QEMU has open corrupts it. Unlike embraos-backup.sh's bare
# pgrep, this compares each running QEMU's `-drive file=` against the resolved
# target, so a VM running a DIFFERENT image does not block seeding this one —
# with multiple builds around, a coarse check blocks legitimate work. It stays
# fail-closed: a QEMU whose image cannot be determined refuses the seed.
#
# Inside the Docker wrapper this is a no-op (a container cannot see host
# processes) — seed-state-mac.sh runs its own copy of the check on the host.

resolve_path() {
    readlink -f "$1" 2>/dev/null || echo "$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"
}

check_no_vm_using_image() {
    local target pid cmd tok path unknown=0
    pgrep -f "qemu.*embraos" >/dev/null 2>&1 || return 0
    target=$(resolve_path "$IMAGE")

    for pid in $(pgrep -f "qemu.*embraos" 2>/dev/null); do
        cmd=$(ps -o args= -p "$pid" 2>/dev/null || true)
        tok=""
        [ -n "$cmd" ] && tok=$(echo "$cmd" | tr ' ,' '\n\n' | grep '^file=' | head -1 || true)
        if [ -z "$tok" ]; then unknown=1; continue; fi

        path="${tok#file=}"
        # run-qemu.sh passes a repo-relative path; anchor it the same way.
        case "$path" in /*) ;; *) path="${EMBRAOS_ROOT}/${path}" ;; esac

        if [ "$(resolve_path "$path")" = "$target" ]; then
            echo "ERROR: QEMU (pid $pid) is running with this image" >&2
            echo "  $target" >&2
            echo "  Stop the VM before seeding to avoid corrupting it" >&2
            echo "  Run: Ctrl-A X in the QEMU console, or kill the QEMU process" >&2
            exit 1
        fi
    done

    if [ "$unknown" -eq 1 ]; then
        echo "ERROR: a QEMU process is running but its disk image could not be determined" >&2
        echo "  Refusing to seed rather than risk corrupting a live image" >&2
        exit 1
    fi
}

check_no_vm_using_image

# --- Privileges ---------------------------------------------------------
# Empty when already root (the Docker wrapper's container, or `sudo seed-state.sh`);
# ubuntu:24.04 ships no sudo binary at all.
if [ "$(id -u)" -eq 0 ]; then
    SUDO=""
elif command -v sudo >/dev/null 2>&1; then
    SUDO="sudo"
else
    die "not root and sudo not found (loop-mounting needs root)"
fi

# --- Partition geometry + mounting --------------------------------------
# Read the GPT directly and mount by byte offset rather than using
# `losetup --partscan` + ${LOOPDEV}pN. Two reasons: this image's GPT starts at
# sector 34 rather than 2048, which partscan mishandles (see the same note in
# embraos-backup.sh), and inside a container the /dev/loopNpM partition nodes
# depend on udev and are unreliable.
#
# Geometry comes from partx, not fdisk as in embraos-backup.sh — deliberate,
# don't "fix" one into the other. partx prints bare numbers with no device
# column (immune to spaces in the image path) and lives in util-linux, which is
# Priority:required, so the Docker wrapper installs no packages at all. fdisk
# is a separate Ubuntu package.

get_partition_geometry() {
    local part_num=$1 line start_sector sectors

    # partx exits 0 with empty output for a missing partition — test the value.
    line=$(partx -g -o START,SECTORS --nr "$part_num" "$IMAGE" 2>/dev/null || true)
    [ -n "$line" ] || die "could not read partition $part_num from $IMAGE (not an embraOS image?)"

    read -r start_sector sectors <<< "$line" || true
    [ -n "$start_sector" ] && [ -n "$sectors" ] || die "malformed geometry for partition $part_num: $line"

    PART_OFFSET=$((start_sector * SECTOR_SIZE))
    PART_SIZE=$((sectors * SECTOR_SIZE))
}

mount_partition() {
    local part_num=$1 mount_point=$2
    get_partition_geometry "$part_num"
    # mount(8) allocates the loop device itself with autoclear, so umount detaches it.
    $SUDO mount -o loop,offset=${PART_OFFSET},sizelimit=${PART_SIZE} "$IMAGE" "$mount_point"
    echo "Mounted partition ${part_num} → ${mount_point} (offset=${PART_OFFSET} size=${PART_SIZE})"
}

# --- Optional wipe ------------------------------------------------------
# Runs BEFORE anything is mounted — reformatting a mounted filesystem would
# corrupt it. Replaces the old hand-run QUICK-START recipe, which used
# `losetup --partscan` + `${LOOPDEV}p3` and so was both fragile on this
# sector-34 GPT and impossible on macOS.

wipe_partition() {   # wipe_partition <part_num> <label>
    local part_num=$1 label=$2
    get_partition_geometry "$part_num"
    # `mke2fs -E offset=` writes ONLY within [offset, offset+size) — no loop
    # device at all, so this behaves identically on a Linux host and inside the
    # Docker wrapper, and cannot leak a loop device. e2fsprogs is Priority:
    # required, so the wrapper's container still installs nothing.
    $SUDO mkfs.ext4 -q -F -E offset="${PART_OFFSET}" -L "$label" \
        "$IMAGE" "$((PART_SIZE / 1024))k"
    echo "  wiped $label (p${part_num}, $((PART_SIZE / 1048576)) MiB)"
}

if [ -n "$WIPE" ]; then
    unknown=$(echo "$WIPE" | tr ',' '\n' | grep -vxE 'state|data|all|' || true)
    [ -z "$unknown" ] || die "--wipe: unknown target(s): $(echo $unknown | tr '\n' ' ')
  valid targets: state, data, all (comma-separated)"

    wipe_state=0; wipe_data=0
    case ",$WIPE," in *,state,*|*,all,*) wipe_state=1 ;; esac
    case ",$WIPE," in *,data,*|*,all,*)  wipe_data=1 ;; esac
    [ "$wipe_state" -eq 1 ] || [ "$wipe_data" -eq 1 ] || die "--wipe: no targets given"

    echo ""
    echo "WIPE — reformats these partitions in $IMAGE:"
    [ "$wipe_state" -eq 1 ] && echo "  STATE (p3) — soul hash, PKI, API keys, config"
    [ "$wipe_data" -eq 1 ]  && echo "  DATA  (p4) — WardSONDB: all memory, sessions, workspace"
    echo "Everything on the listed partition(s) is destroyed. This cannot be undone."

    if [ "$ASSUME_YES" -eq 0 ]; then
        [ -t 0 ] || die "--wipe needs confirmation but stdin is not a terminal — pass --yes"
        printf "Type 'wipe' to confirm: "
        read -r confirm || confirm=""
        [ "$confirm" = "wipe" ] || die "aborted — nothing was written"
    fi

    [ "$wipe_state" -eq 1 ] && wipe_partition "$STATE_PART_NUM" STATE
    [ "$wipe_data" -eq 1 ]  && wipe_partition "$DATA_PART_NUM" DATA
    echo ""
fi

MOUNT_STATE=""
MOUNT_DATA=""

cleanup() {
    sync 2>/dev/null || true
    local mp
    for mp in "${MOUNT_STATE:-}" "${MOUNT_DATA:-}"; do
        if [ -n "$mp" ] && mountpoint -q "$mp" 2>/dev/null; then
            $SUDO umount "$mp" || true
        fi
    done
    rmdir "${MOUNT_STATE:-/nonexistent}" "${MOUNT_DATA:-/nonexistent}" 2>/dev/null || true
}
trap cleanup EXIT

MOUNT_STATE=$(mktemp -d)
MOUNT_DATA=$(mktemp -d)

mount_partition "$STATE_PART_NUM" "$MOUNT_STATE"
mount_partition "$DATA_PART_NUM" "$MOUNT_DATA"

echo "STATE mounted at $MOUNT_STATE"
echo "DATA mounted at $MOUNT_DATA"

# Seed WardSONDB data from Phase 0
if [ -n "$PHASE0_DATA" ]; then
    # A bad path used to fall through to the "skipping" branch below, so a typo
    # produced a seemingly-successful run. Hard-fail like the other dir flags.
    [ -d "$PHASE0_DATA" ] || die "--phase0-data '$PHASE0_DATA' is not a directory"
    SRC_DB="$PHASE0_DATA/wardsondb"
    [ -d "$SRC_DB" ] || die "--phase0-data '$PHASE0_DATA' has no wardsondb/ subdirectory"

    # Pre-flight the size. DATA is a fixed 2 GiB (genimage.cfg.in) and a real
    # WardSONDB directory can exceed it — fail before writing anything rather
    # than leaving a half-copied database behind. Coarse by design: du/df
    # blocks, no filesystem-overhead margin; it catches the gross case.
    need_kb=""; free_kb=""
    if out=$($SUDO du -sk "$SRC_DB" 2>/dev/null); then read -r need_kb _ <<< "$out"; fi
    # df -P guarantees one unwrapped line: Filesystem Blocks Used Avail Cap Mount
    if out=$($SUDO df -Pk "$MOUNT_DATA" 2>/dev/null | tail -1); then
        read -r _ _ _ free_kb _ <<< "$out"
    fi
    if [ -n "$need_kb" ] && [ -n "$free_kb" ] && [ "$need_kb" -gt "$free_kb" ]; then
        die "Phase 0 WardSONDB data does not fit on DATA
  need $((need_kb / 1024)) MiB, available $((free_kb / 1024)) MiB
  Grow DATA in buildroot/board/embraos/genimage.cfg.in and rebuild, or seed less data."
    fi

    if [ -n "$need_kb" ]; then
        echo "Copying Phase 0 WardSONDB data ($((need_kb / 1024)) MiB)..."
    else
        echo "Copying Phase 0 WardSONDB data..."
    fi
    $SUDO mkdir -p "$MOUNT_DATA/wardsondb"

    # "$SRC_DB/." — NOT "$SRC_DB"/*. The glob silently skips dotfiles, and
    # WardSONDB's `.engine` marker is one, written at data_dir/.engine (embrad
    # passes --data-dir /embra/data/wardsondb, so it lands inside this very
    # directory). Losing it defeats the engine-mismatch guard: check_engine_marker
    # returns Ok when the marker is absent, so the image stamps its own engine
    # over data written by the other one instead of refusing to start.
    #
    # No `2>/dev/null || true` either — that swallowed ENOSPC and reported
    # success, leaving a truncated database that reads as a healthy seed.
    if ! $SUDO cp -r "$SRC_DB/." "$MOUNT_DATA/wardsondb/"; then
        die "copy failed — DATA now holds a PARTIAL WardSONDB directory.
  Do not boot this image. Wipe DATA (or rebuild) and re-seed."
    fi

    if [ -f "$MOUNT_DATA/wardsondb/.engine" ]; then
        echo "Done. Seeded data engine: $($SUDO cat "$MOUNT_DATA/wardsondb/.engine")"
        echo "  Must match the image's --storage-engine or WardSONDB refuses to start."
    else
        echo "Done."
        echo "WARNING: no .engine marker in the seeded data — the image will stamp its own"
        echo "  engine on first boot with no mismatch check. Confirm this data was written"
        echo "  by the same engine the image was built with."
    fi
else
    echo "No Phase 0 data specified (--phase0-data). Skipping WardSONDB seed."
    echo "First boot will enter Learning Mode."
fi

# Seed soul hash
if [ -n "$SOUL_HASH" ]; then
    echo "Writing soul hash to STATE..."
    echo "$SOUL_HASH" | $SUDO tee "$MOUNT_STATE/soul.sha256" > /dev/null
    echo "Done."
else
    echo "No soul hash specified (--soul-hash). First boot will allow Learning Mode."
fi

# Seed importable intelligence graphs (kg-native-identity)
if [ -n "$IMPORT_DIR" ]; then
    if [ -d "$IMPORT_DIR" ] && ls "$IMPORT_DIR"/*.graph.json >/dev/null 2>&1; then
        echo "Copying intelligence graphs into STATE imported-intelligence/..."
        $SUDO mkdir -p "$MOUNT_STATE/imported-intelligence"
        $SUDO cp "$IMPORT_DIR"/*.graph.json "$MOUNT_STATE/imported-intelligence/"
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
        $SUDO mkdir -p "$MOUNT_STATE/seed-knowledge"
        $SUDO cp "$SEED_DIR"/*.knowledge.json "$MOUNT_STATE/seed-knowledge/"
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
        $SUDO mkdir -p "$MOUNT_STATE/ca-certificates"
        $SUDO cp "$CA_DIR"/*.pem "$MOUNT_STATE/ca-certificates/" 2>/dev/null || true
        $SUDO cp "$CA_DIR"/*.crt "$MOUNT_STATE/ca-certificates/" 2>/dev/null || true
        echo "Done: $(ls "$CA_DIR"/*.pem "$CA_DIR"/*.crt 2>/dev/null | wc -l) file(s)."
    else
        echo "ERROR: --ca-dir '$CA_DIR' has no *.pem or *.crt files"
        exit 1
    fi
fi

# Create PKI directory (embra-trustd will generate CA on first run)
$SUDO mkdir -p "$MOUNT_STATE/pki"

sync

echo ""
echo "Seed complete. Partitions will be unmounted on exit."
