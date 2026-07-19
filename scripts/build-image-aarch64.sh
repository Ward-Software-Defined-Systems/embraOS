#!/bin/bash
# Full build: Rust binaries → initramfs → Buildroot → disk image (aarch64)
#
# Apple Silicon native build — produces an ARM64 embraOS image that runs
# under qemu-system-aarch64 with HVF hardware acceleration (near-native speed).
#
# Usage:
#   ./scripts/build-image-aarch64.sh --storage-engine <fjall|rocksdb>
#   ./scripts/build-image-aarch64.sh --buildroot-only
#
# --storage-engine selects the WardSONDB backend baked into the embrad binary.
# The choice is locked into the DATA partition on first boot via WardSONDB's
# .engine marker file — switching engines later requires wiping DATA.
#
# NOTE: Steps 0.5-3 (Leptos/WASM frontend + Rust cross-compilation + initramfs)
#       work on macOS. Step 0.5 needs the Trunk WASM bundler on the host:
#         rustup target add wasm32-unknown-unknown && cargo install trunk --locked
#       Step 3.5 (in-OS Rust toolchain) and Step 4 (Buildroot) require a Linux
#       host; on Apple Silicon, use Docker:
#
#   ./scripts/build-image-aarch64.sh --storage-engine rocksdb   # outer call bakes engine
#
#   # No per-arch patching needed: the Buildroot tree is arch-parameterized
#   # (external.mk EMBRAOS_RUST_TARGET + genimage.cfg.in). The aarch64
#   # defconfig (BR2_aarch64=y) drives both.
#
#   docker run --rm -v "$PWD":/work -v embraos-br-aarch64:/work/buildroot-src \
#     -w /work ubuntu:24.04 bash -c \
#     "apt-get update && apt-get install -y build-essential gcc g++ \
#      unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools && \
#      FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
#
#   (Docker defaults to linux/arm64 on Apple Silicon — host tools run natively,
#    Buildroot cross-compiles the kernel and packages for aarch64. The named
#    volume keeps the Buildroot tree on the Docker VM's native filesystem:
#    building it on the bind mount exhausts the macOS file provider's fd pool
#    — "Too many open files".)

set -euo pipefail

RUST_TARGET="aarch64-unknown-linux-musl"
BUILDROOT_DEFCONFIG="embraos_aarch64_defconfig"
# ARM64 kernel binary is "Image" (flat binary), not "bzImage" (x86 boot wrapper)
KERNEL_NAME="Image"

# embra-guardian-v1 in-OS Rust toolchain pin (musl host + wasm32 std).
# Staged into vendor/rust-toolchain by Step 3.5 and installed at /opt/rust.
RUST_TOOLCHAIN_VERSION="${RUST_TOOLCHAIN_VERSION:-1.94.1}"

# Buildroot release pin. Override at runtime:
#   BUILDROOT_VERSION=2024.02 ./scripts/build-image-aarch64.sh ...
BUILDROOT_VERSION="${BUILDROOT_VERSION:-2026.02.1}"

# macOS-compatible nproc
nproc_compat() {
    if command -v nproc &>/dev/null; then
        nproc
    else
        sysctl -n hw.ncpu
    fi
}

# Parallel build jobs for Buildroot. Defaults to all cores (canonical
# behavior — unchanged from the x86_64 build). Lower it on a memory-
# constrained host: Buildroot's GCC + musl toolchain bootstrap can OOM
# with many concurrent compilers in a small Docker VM (e.g. OrbStack's
# 4 GB default). Pass through Docker with:  docker run -e JOBS=2 ...
JOBS="${JOBS:-$(nproc_compat)}"

BUILDROOT_ONLY=false
STORAGE_ENGINE=""
while [ $# -gt 0 ]; do
    case "$1" in
        --buildroot-only)
            BUILDROOT_ONLY=true
            shift
            ;;
        --storage-engine)
            STORAGE_ENGINE="${2:-}"
            shift 2
            ;;
        *)
            echo "ERROR: unknown argument: $1" >&2
            echo "Usage: $0 --storage-engine <fjall|rocksdb> [--buildroot-only]" >&2
            exit 2
            ;;
    esac
done

if [ "$BUILDROOT_ONLY" = false ]; then
    if [ -z "$STORAGE_ENGINE" ]; then
        echo "ERROR: --storage-engine <fjall|rocksdb> is required" >&2
        exit 2
    fi
    case "$STORAGE_ENGINE" in
        fjall|rocksdb) ;;
        *)
            echo "ERROR: --storage-engine must be 'fjall' or 'rocksdb', got '$STORAGE_ENGINE'" >&2
            exit 2
            ;;
    esac
    export EMBRA_STORAGE_ENGINE="$STORAGE_ENGINE"
    echo "=== embraOS build: storage engine = $STORAGE_ENGINE (aarch64) ==="
elif [ -n "$STORAGE_ENGINE" ]; then
    echo "WARNING: --storage-engine ignored with --buildroot-only (Rust not rebuilt;" >&2
    echo "         engine taken from the previously-baked embrad binary)" >&2
fi

if [ "$BUILDROOT_ONLY" = false ]; then
    # Musl cross-toolchain. On macOS the toolchain comes from Homebrew's
    # musl-cross package (covers both C and C++); on Linux use the self-
    # contained musl.cc toolchain for both C and C++ compilation.
    MUSL_SYSROOT=""
    if [ "$(uname)" = "Darwin" ]; then
        # Homebrew musl-cross — auto-detect path (Apple Silicon vs Intel prefix)
        MUSL_CROSS_BIN=""
        MUSL_CROSS_ROOT=""
        for prefix in /opt/homebrew/Cellar/musl-cross /usr/local/Cellar/musl-cross; do
            for dir in "$prefix"/*/libexec/bin; do
                if [ -x "$dir/aarch64-linux-musl-gcc" ]; then
                    MUSL_CROSS_BIN="$dir"
                    MUSL_CROSS_ROOT="$(dirname "$dir")"
                    break 2
                fi
            done
        done
        if [ -z "$MUSL_CROSS_BIN" ]; then
            echo "ERROR: musl-cross aarch64 toolchain not found in Homebrew" >&2
            echo "  Install it with:  brew install filosottile/musl-cross/musl-cross" >&2
            exit 1
        fi
        export PATH="$MUSL_CROSS_BIN:$PATH"
        # Sysroot for bindgen/clang (needed by zstd-sys, rocksdb-sys, etc.)
        # Homebrew layout: libexec/aarch64-linux-musl/ is sibling of libexec/bin/
        if [ -d "$MUSL_CROSS_ROOT/aarch64-linux-musl" ]; then
            MUSL_SYSROOT="$MUSL_CROSS_ROOT/aarch64-linux-musl"
        fi
    else
        # Linux — expect the self-contained aarch64 musl toolchain from musl.cc
        MUSL_CROSS="${MUSL_CROSS:-/opt/aarch64-linux-musl-cross}"
        if [ ! -x "$MUSL_CROSS/bin/aarch64-linux-musl-gcc" ]; then
            echo "ERROR: musl cross-toolchain not found at $MUSL_CROSS" >&2
            echo "  Install it with:" >&2
            echo "    cd /tmp && curl -LO https://musl.cc/aarch64-linux-musl-cross.tgz" >&2
            echo "    sudo tar -xzf aarch64-linux-musl-cross.tgz -C /opt" >&2
            exit 1
        fi
        export PATH="$MUSL_CROSS/bin:$PATH"
        # musl.cc layout: sysroot is at <MUSL_CROSS>/aarch64-linux-musl
        if [ -d "$MUSL_CROSS/aarch64-linux-musl" ]; then
            MUSL_SYSROOT="$MUSL_CROSS/aarch64-linux-musl"
        fi
    fi

    export CC_aarch64_unknown_linux_musl=aarch64-linux-musl-gcc
    export CXX_aarch64_unknown_linux_musl=aarch64-linux-musl-g++
    export AR_aarch64_unknown_linux_musl=aarch64-linux-musl-ar
    export CARGO_TARGET_AARCH64_UNKNOWN_LINUX_MUSL_LINKER=aarch64-linux-musl-gcc

    # Plain AR/RANLIB for dependencies whose build.rs invokes configure/make
    # directly (e.g., tikv-jemalloc-sys). Without these, jemalloc's ./configure
    # picks up macOS's BSD ar, which produces empty archives that pass the build
    # step silently — you then hit undefined references to _rjem_malloc etc. at
    # link time. See Troubleshooting in docs/AARCH64-BUILD.md.
    export AR=aarch64-linux-musl-ar
    export RANLIB=aarch64-linux-musl-ranlib
    export RANLIB_aarch64_unknown_linux_musl=aarch64-linux-musl-ranlib

    # Tell bindgen/clang to use the musl sysroot instead of Xcode's macOS SDK.
    # Without this, crates like zstd-sys and rocksdb-sys fail with:
    #   fatal error: 'stddef.h' file not found
    # because clang picks up Xcode's stddef.h which pulls in macOS headers.
    if [ -n "$MUSL_SYSROOT" ]; then
        export BINDGEN_EXTRA_CLANG_ARGS_aarch64_unknown_linux_musl="--sysroot=${MUSL_SYSROOT} -I${MUSL_SYSROOT}/include"
        echo "  musl sysroot: $MUSL_SYSROOT"
    else
        echo "WARNING: could not locate musl sysroot — bindgen-based crates may fail" >&2
    fi

    # Build the Leptos/WASM frontend BEFORE the cargo cross-build so
    # embra-web can embed crates/embra-web-ui/dist via rust-embed. This
    # is host/CI-only (no Node in the image; assets ride inside the
    # static musl binary). wasm32 output is host-arch-agnostic — same
    # on x86_64 and aarch64 hosts.
    echo "=== Step 0.5: Build embra-web frontend (Trunk → WASM) ==="
    if ! command -v trunk &>/dev/null; then
        echo "ERROR: trunk not found (needed for the embra-web frontend)" >&2
        echo "  Install it with:" >&2
        echo "    rustup target add wasm32-unknown-unknown" >&2
        echo "    cargo install trunk --locked" >&2
        exit 1
    fi
    rustup target add wasm32-unknown-unknown
    (cd crates/embra-web-ui && trunk build --release)

    echo "=== Step 1: Build Rust binaries (aarch64 musl static) ==="
    rustup target add "$RUST_TARGET"
    cargo build --release --target "$RUST_TARGET"
    # wardsondb is a workspace member (crates/wardsondb, vendored 2026-07-17)
    # and builds in Step 1 with the other binaries. The former Step 2
    # sibling-repo build/copy is gone; Buildroot's wardsondb.mk picks the
    # binary up from target/${RUST_TARGET}/release/ as before.
    # (Step numbering keeps the gap deliberately — Steps 3/3.5/4/5 are
    # referenced by name across the build guides.)

    echo "=== Step 3: Create initramfs ==="
    # create_initramfs.sh respects RUST_TARGET env var (aarch64-aware version).
    RUST_TARGET="$RUST_TARGET" ./scripts/create_initramfs.sh
fi

# Step 3.5: stage the in-OS Rust toolchain for embra-guardian-v1.
# Unconditional w.r.t. --buildroot-only (Buildroot's local-SITE rsync
# needs vendor/rust-toolchain present before Step 4). SKIPPED on macOS:
# this aarch64 flow runs Step 4 in a linux/arm64 Docker container, and
# macOS lacks sha256sum/xz/curl by default — the Docker pass below stages
# it, and vendor/rust-toolchain rides the -v "$PWD":/work bind mount.
if [ "$(uname)" = "Darwin" ]; then
    echo "=== Step 3.5: in-OS Rust toolchain — deferred to the Docker Buildroot pass (macOS) ==="
else
    echo "=== Step 3.5: Stage in-OS Rust toolchain (embra-guardian-v1) ==="
    RUST_STAGE="$PWD/vendor/rust-toolchain"
    if [ -x "$RUST_STAGE/bin/cargo" ] && \
       [ "$(cat "$RUST_STAGE/RUST_VERSION" 2>/dev/null || true)" = "$RUST_TOOLCHAIN_VERSION" ]; then
        echo "in-OS Rust toolchain already staged ($RUST_TOOLCHAIN_VERSION) — skipping"
    else
        for tool in xz sha256sum curl; do
            command -v "$tool" &>/dev/null || {
                echo "ERROR: $tool is required to stage the in-OS Rust toolchain" >&2
                echo "  (Linux: apt-get install -y xz-utils curl coreutils)" >&2
                exit 1
            }
        done
        RUST_DIST_BASE="${RUST_DIST_BASE:-https://static.rust-lang.org/dist}"
        RUST_HOST="rust-${RUST_TOOLCHAIN_VERSION}-aarch64-unknown-linux-musl"
        RUST_WASM="rust-std-${RUST_TOOLCHAIN_VERSION}-wasm32-unknown-unknown"
        RUST_TMP="$(mktemp -d)"
        trap 'rm -rf "$RUST_TMP"' EXIT
        for tb in "$RUST_HOST" "$RUST_WASM"; do
            echo "  downloading ${tb}.tar.xz"
            curl -fSL --retry 3 -o "$RUST_TMP/${tb}.tar.xz" \
                "$RUST_DIST_BASE/${tb}.tar.xz" \
                || { echo "ERROR: download of ${tb}.tar.xz failed" >&2; exit 1; }
            curl -fSL --retry 3 -o "$RUST_TMP/${tb}.tar.xz.sha256" \
                "$RUST_DIST_BASE/${tb}.tar.xz.sha256" \
                || { echo "ERROR: download of ${tb}.tar.xz.sha256 failed" >&2; exit 1; }
            ( cd "$RUST_TMP" \
                && printf '%s  %s\n' "$(awk '{print $1}' "${tb}.tar.xz.sha256")" "${tb}.tar.xz" \
                   | sha256sum -c - ) \
                || { echo "ERROR: sha256 verification failed for ${tb}.tar.xz" >&2; exit 1; }
            tar -xf "$RUST_TMP/${tb}.tar.xz" -C "$RUST_TMP" \
                || { echo "ERROR: extract of ${tb}.tar.xz failed" >&2; exit 1; }
        done
        rm -rf "$RUST_STAGE"
        mkdir -p "$RUST_STAGE"
        "$RUST_TMP/$RUST_HOST/install.sh" --prefix="$RUST_STAGE" \
            --disable-ldconfig --without=rust-docs >/dev/null \
            || { echo "ERROR: Rust host install failed" >&2; exit 1; }
        "$RUST_TMP/$RUST_WASM/install.sh" --prefix="$RUST_STAGE" \
            --disable-ldconfig >/dev/null \
            || { echo "ERROR: wasm32 std install failed" >&2; exit 1; }
        rm -rf "$RUST_STAGE/share/doc" "$RUST_STAGE/share/man" \
               "$RUST_STAGE/lib/rustlib/src" 2>/dev/null || true
        echo "$RUST_TOOLCHAIN_VERSION" > "$RUST_STAGE/RUST_VERSION"
        rm -rf "$RUST_TMP"; trap - EXIT
        echo "  staged Rust $RUST_TOOLCHAIN_VERSION ($(du -sh "$RUST_STAGE" 2>/dev/null | cut -f1)) → $RUST_STAGE"
    fi
fi

echo "=== Step 4: Buildroot ==="
# Buildroot requires a Linux host (compiles Linux kernel, uses Linux-specific tools)
if [ "$(uname)" = "Darwin" ]; then
    echo "ERROR: Buildroot cannot build natively on macOS."
    echo ""
    echo "Steps 0.5-3 (frontend + Rust cross-compilation + initramfs) completed successfully."
    echo "Storage engine '${EMBRA_STORAGE_ENGINE:-<unset>}' was baked into the embrad binary."
    echo ""
    echo "The Buildroot tree is arch-parameterized (external.mk EMBRAOS_RUST_TARGET"
    echo "+ genimage.cfg.in) — no per-arch patching. Build the disk image in Docker"
    echo "(no engine flag needed inside):"
    echo ""
    echo "  docker run --rm -v \"\$PWD\":/work -v embraos-br-aarch64:/work/buildroot-src \\"
    echo "    -w /work ubuntu:24.04 bash -c \\"
    echo "    \"apt-get update && apt-get install -y build-essential gcc g++ \\"
    echo "     unzip bc cpio rsync wget curl xz-utils python3 file git dosfstools && \\"
    echo "     FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only\""
    echo ""
    echo "(Docker defaults to linux/arm64 on Apple Silicon — no Rosetta overhead."
    echo " The named volume keeps the Buildroot tree on the Docker VM's filesystem:"
    echo " building it on the bind mount exhausts macOS file-provider fds — EMFILE.)"
    echo ""
    echo "Or run this script on an ARM64 Linux machine."
    exit 1
fi

# fd preflight: raise the soft fd limit to the hard cap and report state.
# Docker containers (OrbStack: soft 20480 / hard 1048576) default well below
# what a full Buildroot build can demand. Per-process only — macOS-side
# virtiofs-provider fd exhaustion (EMFILE on bind-mount paths) is outside
# any in-container limit's reach.
FD_SOFT="$(ulimit -Sn)"
FD_HARD="$(ulimit -Hn)"
if [ "$FD_SOFT" != "unlimited" ]; then
    FD_RAISE="$FD_HARD"
    if [ "$FD_RAISE" = "unlimited" ]; then
        FD_RAISE=1048576
    fi
    if [ "$FD_SOFT" -lt "$FD_RAISE" ]; then
        ulimit -n "$FD_RAISE" 2>/dev/null \
            || echo "WARNING: could not raise fd soft limit (soft $FD_SOFT, hard $FD_HARD)" >&2
    fi
fi
echo "fd limits: soft $(ulimit -Sn) / hard $(ulimit -Hn); system-wide $(awk '{print $1 "/" $3}' /proc/sys/fs/file-nr 2>/dev/null || echo 'n/a')"
echo "Buildroot parallelism: -j${JOBS} (override: docker run -e JOBS=N ...)"

BUILDROOT_DIR="${BUILDROOT_DIR:-buildroot-src}"
# Clone when missing OR empty: if BUILDROOT_DIR is a mountpoint (e.g. a Docker
# named volume), the directory always exists — an empty one must still get the
# clone (git clones into an existing empty dir).
if [ ! -d "$BUILDROOT_DIR" ] || [ -z "$(ls -A "$BUILDROOT_DIR" 2>/dev/null)" ]; then
    git clone https://github.com/buildroot/buildroot.git "$BUILDROOT_DIR"
fi
# Ensure the clone is at the pinned version. Switching versions wipes
# output/ to avoid mixing stale Buildroot state across releases; refuse
# the switch if the user has local changes in $BUILDROOT_DIR.
(
    cd "$BUILDROOT_DIR"
    if ! git rev-parse --git-dir >/dev/null 2>&1; then
        echo "ERROR: $BUILDROOT_DIR exists but is not a git checkout (non-empty, no .git)." >&2
        echo "       Remove or empty it so this script can clone Buildroot fresh." >&2
        exit 1
    fi
    current=$(git describe --tags --exact-match 2>/dev/null || echo "")
    if [ "$current" != "$BUILDROOT_VERSION" ]; then
        if ! git diff-index --quiet HEAD --; then
            echo "ERROR: $BUILDROOT_DIR has local changes; refusing to switch to $BUILDROOT_VERSION." >&2
            echo "       Commit/stash them, or set BUILDROOT_VERSION=${current:-<your-version>} to pin to the current checkout." >&2
            exit 1
        fi
        echo "=== Buildroot: switching ${current:-unknown} -> $BUILDROOT_VERSION (wiping output/) ==="
        git fetch --tags
        git checkout "$BUILDROOT_VERSION"
        rm -rf output/
    fi
)

# Clean stale Buildroot package caches so fresh binaries are picked up
# Includes embraOS packages AND upstream packages whose config may have changed
(cd "$BUILDROOT_DIR" && \
    for pkg in embrad embra-apid embra-trustd embra-brain embra-console embra-web wardsondb \
               embra-rust-toolchain git openssl libcurl openssh; do
        make "${pkg}-dirclean" 2>/dev/null || true
    done && \
    rm -f output/images/rootfs.squashfs output/images/embraos.img)

# Configure and build
(cd "$BUILDROOT_DIR" && \
    make BR2_EXTERNAL="$(pwd)/../buildroot" "$BUILDROOT_DEFCONFIG" && \
    make -j"$JOBS")

echo "=== Step 5: Copy outputs ==="
mkdir -p output/images
cp "$BUILDROOT_DIR/output/images/embraos.img" output/images/
cp "$BUILDROOT_DIR/output/images/${KERNEL_NAME}" output/images/

echo ""
echo "Build complete! (aarch64)"
echo "Run: ./scripts/run-qemu-aarch64.sh"
