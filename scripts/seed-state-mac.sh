#!/bin/bash
# seed-state-mac.sh — macOS wrapper for seed-state.sh
#
# Runs the Linux seeding script inside a privileged Docker container with loop
# device access for mounting disk image partitions. macOS has neither losetup
# nor ext4, so this is the only way to seed an image from the Mac.
#
# Same interface as seed-state.sh — host paths given to the image and to
# --phase0-data / --import-dir / --seed-dir / --ca-dir are translated to
# container paths automatically (bind-mounted read-only if they live outside
# the project root).
#
# Usage:
#   ./scripts/seed-state-mac.sh --ca-dir /path/to/dir-with-rootCA.pem
#   ./scripts/seed-state-mac.sh output/images/embraos.img --seed-dir Seed_Knowledge
#   ./scripts/seed-state-mac.sh --dry-run --ca-dir ~/certs      # print, run nothing
#
# Requires: Docker (OrbStack or Docker Desktop). No apt packages are installed
# in the container — partition geometry comes from partx and mounting from
# mount(8), both in the ubuntu:24.04 base image — so this also runs offline.
#
# Environment:
#   EMBRAOS_IMAGE   Disk image path (overridden by the argument/--image)
#
# NOTE: written for macOS's bash 3.2 — no associative arrays, no ${var,,},
# no mapfile. Keep it that way.

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd -P)"
EMBRAOS_ROOT="$(dirname "$SCRIPT_DIR")"
DOCKER_IMAGE="ubuntu:24.04"

RED='\033[0;31m'
NC='\033[0m'

die() { echo -e "${RED}[ERROR]${NC} $*" >&2; exit 1; }

usage() {
    cat <<'EOF'
seed-state-mac.sh — macOS wrapper for seed-state.sh

Runs the Linux seeding script inside a privileged Docker container with loop
device access. Same options as seed-state.sh; host paths are translated to
container paths automatically.

Usage:
  ./scripts/seed-state-mac.sh [<image>] [options]

Options:
  --image <path>          Disk image to seed (same as the positional argument)
  --phase0-data <dir>     Copy <dir>/wardsondb/ into DATA
  --soul-hash <hash>      Write <hash> to STATE/soul.sha256
  --import-dir <dir>      Copy *.graph.json into STATE/imported-intelligence/
  --seed-dir <dir>        Copy *.knowledge.json into STATE/seed-knowledge/
  --ca-dir <dir>          Copy *.pem / *.crt into STATE/ca-certificates/
  --dry-run               Print the docker command and exit (no Docker needed)
  -h, --help              Show this help

Environment:
  EMBRAOS_IMAGE   Disk image path (overridden by the argument/--image)

The VM must be stopped before seeding.
Docker (OrbStack or Docker Desktop) must be running.
EOF
}

# --- Argument parsing (same grammar as seed-state.sh, plus --dry-run) ----

IMAGE_ARG=""
PHASE0_DATA=""
SOUL_HASH=""
IMPORT_DIR=""
SEED_DIR=""
CA_DIR=""
DRY_RUN=0

need() { [ "$1" -ge 2 ] || die "$2 requires a value"; }

while [ $# -gt 0 ]; do
    case "$1" in
        --image)       need $# --image;       IMAGE_ARG="$2";   shift 2 ;;
        --phase0-data) need $# --phase0-data; PHASE0_DATA="$2"; shift 2 ;;
        --soul-hash)   need $# --soul-hash;   SOUL_HASH="$2";   shift 2 ;;
        --import-dir)  need $# --import-dir;  IMPORT_DIR="$2";  shift 2 ;;
        --seed-dir)    need $# --seed-dir;    SEED_DIR="$2";    shift 2 ;;
        --ca-dir)      need $# --ca-dir;      CA_DIR="$2";      shift 2 ;;
        --dry-run)     DRY_RUN=1; shift ;;
        -h|--help)     usage; exit 0 ;;
        -*)            echo "Unknown option: $1" >&2; echo >&2; usage >&2; exit 1 ;;
        *)             [ -z "$IMAGE_ARG" ] || die "unexpected extra argument: $1"
                       IMAGE_ARG="$1"; shift ;;
    esac
done

# --- Image resolution (host side) ---------------------------------------
# Kept in sync by hand with seed-state.sh, run-qemu.sh, run-qemu-aarch64.sh and
# embraos-backup.sh, which all carry a copy of this precedence.
# EMBRAOS_IMAGE is consumed here and never forwarded with -e: a macOS host path
# is meaningless inside the container.
if [ -n "$IMAGE_ARG" ]; then
    IMAGE="$IMAGE_ARG"
elif [ -n "${EMBRAOS_IMAGE:-}" ]; then
    IMAGE="$EMBRAOS_IMAGE"
elif [ -f "${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img"
elif [ -f "${EMBRAOS_ROOT}/output/images/embraos.img" ]; then
    IMAGE="${EMBRAOS_ROOT}/output/images/embraos.img"
else
    die "No disk image found.
  Searched: ${EMBRAOS_ROOT}/buildroot-src/output/images/embraos.img
            ${EMBRAOS_ROOT}/output/images/embraos.img
  Pass one explicitly or set EMBRAOS_IMAGE. Build with ./scripts/build-image-aarch64.sh"
fi

# --- Inner-script compatibility -----------------------------------------
# The container executes /work/scripts/seed-state.sh out of the bind-mounted
# repo, so this wrapper and the script it drives can drift apart — only one of
# the two copied to a host, a different branch checked out, an older clone. A
# pre-rewrite seed-state.sh consumes "--image" as the image path and then dies
# with "Unknown option: /work/..."; it would also mount via losetup --partscan,
# which does not work inside a container. Fail with a message that says so.
INNER_SCRIPT="${EMBRAOS_ROOT}/scripts/seed-state.sh"
[ -f "$INNER_SCRIPT" ] || die "not found: $INNER_SCRIPT"
if ! grep -q -- '--image)' "$INNER_SCRIPT" 2>/dev/null ||
   ! grep -q 'partx -g -o START,SECTORS' "$INNER_SCRIPT" 2>/dev/null; then
    die "scripts/seed-state.sh on this host predates the container-safe rewrite.
  It does not understand --image (you would see 'Unknown option: /work/...')
  and mounts via losetup --partscan, which does not work inside a container.
  Copy the current scripts/seed-state.sh to this host and retry —
  both files must come from the same revision."
fi

abspath_dir()  { (cd "$1" && pwd -P); }
abspath_file() { echo "$(cd "$(dirname "$1")" && pwd -P)/$(basename "$1")"; }

# --- Host-side VM guard -------------------------------------------------
# Must live here as well as in seed-state.sh: a container cannot see host
# processes, so the in-script check is a no-op on this path. Same image-aware
# comparison — a VM running a different image does not block this seed — and
# the same fail-closed fallback when a QEMU's image can't be determined.

resolve_path() {
    readlink -f "$1" 2>/dev/null || abspath_file "$1"
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
        case "$path" in /*) ;; *) path="${EMBRAOS_ROOT}/${path}" ;; esac

        if [ "$(resolve_path "$path")" = "$target" ]; then
            die "QEMU (pid $pid) is running with this image
  $target
  Stop the VM before seeding to avoid corrupting it
  Run: Ctrl-A X in the QEMU console, or kill the QEMU process"
        fi
    done

    [ "$unknown" -eq 0 ] || die "a QEMU process is running but its disk image could not be determined
  Refusing to seed rather than risk corrupting a live image"
}

check_no_vm_using_image

# --- Path translation ---------------------------------------------------

# Prefix test written as a case so a sibling directory (/repo-backup vs /repo)
# takes the bind-mount branch instead of being mangled into /work/-backup.
under_root() {
    case "$1" in
        "$EMBRAOS_ROOT"|"$EMBRAOS_ROOT"/*) return 0 ;;
        *) return 1 ;;
    esac
}

# docker -v is colon-delimited and has no escape for a colon in a host path.
no_colon() {
    case "$1" in
        *:*) die "path contains ':', which docker -v cannot express: $1" ;;
    esac
}

no_colon "$EMBRAOS_ROOT"

DOCKER_ARGS=(run --rm --privileged)
# Full if, not `[ -t 0 ] && DOCKER_ARGS+=(-it)`: as a bare statement that
# returns non-zero when stdin is not a TTY and set -e would kill the script —
# exactly the piped/CI case the check exists to handle.
if [ -t 0 ] && [ -t 1 ]; then
    DOCKER_ARGS+=(-it)
fi
DOCKER_ARGS+=(-v "${EMBRAOS_ROOT}:/work" -w /work)
DOCKER_ARGS+=(-e EMBRAOS_ROOT=/work -e DEBIAN_FRONTEND=noninteractive)

SEED_ARGS=()
MAP=""

# Fixed container mount points chosen by flag, not by basename: two flags
# pointing at different directories that share a basename would otherwise
# collide on one container path.
add_dir() {   # add_dir <flag> <hostpath> <mountname>
    local flag="$1" hp="$2" name="$3" abs rel cpath note
    [ -n "$hp" ] || return 0
    [ -d "$hp" ] || die "$flag: not a directory: $hp"
    abs=$(abspath_dir "$hp")
    no_colon "$abs"
    note=""
    if under_root "$abs"; then
        rel="${abs#"$EMBRAOS_ROOT"}"      # inner quotes are required — without
        rel="${rel#/}"                    # them the pattern is glob-expanded
        if [ -n "$rel" ]; then cpath="/work/$rel"; else cpath="/work"; fi
    else
        cpath="/mnt/$name"
        DOCKER_ARGS+=(-v "${abs}:${cpath}:ro")
        note="   (read-only bind mount)"
    fi
    SEED_ARGS+=("$flag" "$cpath")
    MAP="${MAP}
  ${flag}
      ${abs}
   →  ${cpath}${note}"
}

# Image: read-write. Bind its directory and keep the basename.
IMG_ABS=$(abspath_file "$IMAGE")
no_colon "$IMG_ABS"
[ -f "$IMG_ABS" ] || die "image not found: $IMG_ABS"
if under_root "$IMG_ABS"; then
    IMG_CPATH="/work/${IMG_ABS#"$EMBRAOS_ROOT"/}"
    IMG_NOTE=""
else
    DOCKER_ARGS+=(-v "$(dirname "$IMG_ABS"):/mnt/image")
    IMG_CPATH="/mnt/image/$(basename "$IMG_ABS")"
    IMG_NOTE="   (bind mount, read-write)"
fi
SEED_ARGS+=(--image "$IMG_CPATH")
MAP="  --image
      ${IMG_ABS}
   →  ${IMG_CPATH}${IMG_NOTE}"

add_dir --phase0-data "$PHASE0_DATA" phase0
add_dir --import-dir  "$IMPORT_DIR"  import
add_dir --seed-dir    "$SEED_DIR"    seed
add_dir --ca-dir      "$CA_DIR"      ca

# A literal, not a path — no translation.
if [ -n "$SOUL_HASH" ]; then
    SEED_ARGS+=(--soul-hash "$SOUL_HASH")
fi

# Printed before running because Docker Desktop's file-sharing allowlist fails
# SILENTLY: a bind mount of a path outside /Users, /Volumes, /private or /tmp
# yields an empty directory in the container, and the operator then sees
# "--ca-dir '/mnt/ca' has no *.pem or *.crt files" — a message that blames the
# certificates. (OrbStack has no allowlist.)
echo "Path mapping (host → container):"
echo "$MAP"
echo ""

# The self-heal never fires against ubuntu:24.04 — partx is in util-linux,
# which is Priority:required. It is here so a slimmer base image still works.
INNER='set -e
command -v partx >/dev/null 2>&1 || { apt-get update -qq && apt-get install -y -qq util-linux; }
exec /work/scripts/seed-state.sh "$@"'

if [ "$DRY_RUN" -eq 1 ]; then
    echo "Dry run — would execute:"
    printf 'docker'
    printf ' %q' "${DOCKER_ARGS[@]}" "$DOCKER_IMAGE" bash -c "$INNER" -- "${SEED_ARGS[@]}"
    printf '\n'
    exit 0
fi

command -v docker >/dev/null 2>&1 || die "Docker not found. Install OrbStack or Docker Desktop."
docker info >/dev/null 2>&1 || die "Docker is not running. Start OrbStack or Docker Desktop."

# `bash -c "$INNER" -- "$@"` sets $0 to "--" and $1 to the first real argument,
# preserving argv exactly — paths with spaces survive.
docker "${DOCKER_ARGS[@]}" "$DOCKER_IMAGE" bash -c "$INNER" -- "${SEED_ARGS[@]}"
