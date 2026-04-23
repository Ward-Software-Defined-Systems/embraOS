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
# NOTE: Steps 1-3 (Rust cross-compilation + initramfs) work on macOS.
#       Step 4 (Buildroot) requires a Linux host. On Apple Silicon, use Docker:
#
#   ./scripts/build-image-aarch64.sh --storage-engine rocksdb   # outer call bakes engine
#
#   # Patch Buildroot configs for aarch64 (one-time per arch switch)
#   find buildroot/package -name '*.mk' -exec \
#     sed -i '' 's|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|g' {} +
#   sed -i '' 's/"bzImage"/"Image"/' buildroot/board/embraos/genimage.cfg
#
#   docker run --rm -v "$PWD":/work -w /work ubuntu:24.04 bash -c \
#     "apt-get update && apt-get install -y build-essential gcc g++ \
#      unzip bc cpio rsync wget python3 file git dosfstools && \
#      FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only"
#
#   (Docker defaults to linux/arm64 on Apple Silicon — host tools run natively,
#    Buildroot cross-compiles the kernel and packages for aarch64.)

set -euo pipefail

RUST_TARGET="aarch64-unknown-linux-musl"
BUILDROOT_DEFCONFIG="embraos_aarch64_defconfig"
# ARM64 kernel binary is "Image" (flat binary), not "bzImage" (x86 boot wrapper)
KERNEL_NAME="Image"

# macOS-compatible nproc
nproc_compat() {
    if command -v nproc &>/dev/null; then
        nproc
    else
        sysctl -n hw.ncpu
    fi
}

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
    # link time. See Troubleshooting in EMBRAOS_AARCH64_BUILD_GUIDE.md.
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

    echo "=== Step 1: Build Rust binaries (aarch64 musl static) ==="
    rustup target add "$RUST_TARGET"
    cargo build --release --target "$RUST_TARGET"

    echo "=== Step 2: Build WardSONDB (from separate repo) ==="
    WARDSONDB_DIR="${WARDSONDB_DIR:-../WardSONDB}"
    if [ -d "$WARDSONDB_DIR" ]; then
        (cd "$WARDSONDB_DIR" && cargo build --release --target "$RUST_TARGET")
        cp "$WARDSONDB_DIR/target/${RUST_TARGET}/release/wardsondb" \
           "target/${RUST_TARGET}/release/wardsondb"
    else
        echo "WARNING: WardSONDB directory not found at $WARDSONDB_DIR"
        echo "Set WARDSONDB_DIR to the WardSONDB repository path"
    fi

    echo "=== Step 3: Create initramfs ==="
    # create_initramfs.sh respects RUST_TARGET env var (aarch64-aware version).
    RUST_TARGET="$RUST_TARGET" ./scripts/create_initramfs.sh
fi

echo "=== Step 4: Buildroot ==="
# Buildroot requires a Linux host (compiles Linux kernel, uses Linux-specific tools)
if [ "$(uname)" = "Darwin" ]; then
    echo "ERROR: Buildroot cannot build natively on macOS."
    echo ""
    echo "Steps 1-3 (Rust cross-compilation + initramfs) completed successfully."
    echo "Storage engine '${EMBRA_STORAGE_ENGINE:-<unset>}' was baked into the embrad binary."
    echo ""
    echo "Before running Buildroot, patch the Buildroot configs for aarch64:"
    echo ""
    echo "  # .mk files: swap x86_64 target path for aarch64"
    echo "  find buildroot/package -name '*.mk' -exec \\"
    echo "    sed -i '' 's|x86_64-unknown-linux-musl|aarch64-unknown-linux-musl|g' {} +"
    echo ""
    echo "  # genimage.cfg: ARM64 kernel is 'Image', not 'bzImage'"
    echo "  sed -i '' 's/\"bzImage\"/\"Image\"/' buildroot/board/embraos/genimage.cfg"
    echo ""
    echo "Then build the disk image in Docker (no engine flag needed inside):"
    echo ""
    echo "  docker run --rm -v \"\$PWD\":/work -w /work ubuntu:24.04 bash -c \\"
    echo "    \"apt-get update && apt-get install -y build-essential gcc g++ \\"
    echo "     unzip bc cpio rsync wget python3 file git dosfstools && \\"
    echo "     FORCE_UNSAFE_CONFIGURE=1 ./scripts/build-image-aarch64.sh --buildroot-only\""
    echo ""
    echo "(Docker defaults to linux/arm64 on Apple Silicon — no Rosetta overhead.)"
    echo ""
    echo "Or run this script on an ARM64 Linux machine."
    exit 1
fi

BUILDROOT_DIR="${BUILDROOT_DIR:-buildroot-src}"
if [ ! -d "$BUILDROOT_DIR" ]; then
    git clone https://github.com/buildroot/buildroot.git "$BUILDROOT_DIR"
    (cd "$BUILDROOT_DIR" && git checkout 2024.02)  # Use a stable release
fi

# Clean stale Buildroot package caches so fresh binaries are picked up
# Includes embraOS packages AND upstream packages whose config may have changed
(cd "$BUILDROOT_DIR" && \
    for pkg in embrad embra-apid embra-trustd embra-brain embra-console wardsondb \
               git openssl libcurl openssh; do
        make "${pkg}-dirclean" 2>/dev/null || true
    done && \
    rm -f output/images/rootfs.squashfs output/images/embraos.img)

# Configure and build
(cd "$BUILDROOT_DIR" && \
    make BR2_EXTERNAL="$(pwd)/../buildroot" "$BUILDROOT_DEFCONFIG" && \
    make -j$(nproc_compat))

echo "=== Step 5: Copy outputs ==="
mkdir -p output/images
cp "$BUILDROOT_DIR/output/images/embraos.img" output/images/
cp "$BUILDROOT_DIR/output/images/${KERNEL_NAME}" output/images/

echo ""
echo "Build complete! (aarch64)"
echo "Run: ./scripts/run-qemu-aarch64.sh"
