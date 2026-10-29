# shellcheck shell=bash
# What the two image scripts share (#43): the pins and the checks that are
# the same whatever the architecture, defined once so a pinned hash or a
# security invariant cannot drift between build-image.sh (x86_64) and
# build-image-aarch64.sh. Sourced by both after their option parsing; it
# defines variables and functions and runs nothing.
#
# The callers set BUILDROOT_DIR before the post-Buildroot checks, and
# EMBED_MODEL_NAME where they stage the model.

# embra-guardian-v1 in-OS Rust toolchain pin (musl host + wasm32 std).
# Staged into vendor/rust-toolchain by Step 3.5 and installed at /opt/rust.
# Kept at the host compiler's version (rust-toolchain.toml) as a convention,
# but a separate setting: the brain loads a Guardian tool only when it was
# built by the version in /opt/rust/RUST_VERSION, so moving this pin leaves
# every existing tool unloaded on the next boot until the operator runs
# `/guardian rebuild --all`. When it moves, the test fixture
# crates/embra-guardian/tests/fixtures/probe.wasm is built again as well
# (tests/fixture_roundtrip.rs says how).
RUST_TOOLCHAIN_VERSION="${RUST_TOOLCHAIN_VERSION:-1.98.1}"
# The pinned sha256 of each toolchain tarball, beside the scripts (#33).
TOOLCHAIN_PINS="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/rust-toolchain.sha256"

# The embedding model (KG-02), pinned by sha256 like CI's Bootlin SDK: the
# payload is fetched, never committed, and a changed upstream file fails
# the build instead of silently shipping different weights. Verified
# 2026-09-08. Architecture-INDEPENDENT: identical bytes serve x86_64 and
# aarch64.
EMBED_ONNX_SHA="828e1496d7fabb79cfa4dcd84fa38625c0d3d21da474a00f08db0f559940cf35"
EMBED_TOKENIZER_SHA="d241a60d5e8f04cc1b2b3e9ef7a4921b27bf526d9f6050ab90f9267a1f9e5c66"

# verify_toolchain_tarball <tarball name without .tar.xz> <directory it was
# downloaded to>: the expected hash is the pin in the repository, never a
# checksum fetched beside the download (#33).
verify_toolchain_tarball() {
    local tb="$1" dir="$2" pin
    pin="$(grep -F -- "  ${tb}.tar.xz" "$TOOLCHAIN_PINS" | head -n1 || true)"
    [ -n "$pin" ] || {
        echo "ERROR: no pinned sha256 for ${tb}.tar.xz in $TOOLCHAIN_PINS" >&2
        echo "  The pin moves with RUST_TOOLCHAIN_VERSION: take xz_hash from the channel manifest" >&2
        echo "  ($RUST_DIST_BASE/channel-rust-$RUST_TOOLCHAIN_VERSION.toml) and add the line." >&2
        exit 1
    }
    ( cd "$dir" && printf '%s\n' "$pin" | sha256sum -c - ) \
        || { echo "ERROR: ${tb}.tar.xz does not match its pin in $TOOLCHAIN_PINS" >&2; exit 1; }
}

# A build that fails one of the checks below has produced an image already:
# Buildroot wrote it before the checks ran, to the place where run-qemu.sh,
# seed-state.sh and embraos-backup.sh look first. It is removed, so that a
# build that failed leaves nothing that boots.
reject_image() {
    rm -f "$BUILDROOT_DIR/output/images/embraos.img"
    echo "       The image this build produced was removed:" >&2
    echo "       $BUILDROOT_DIR/output/images/embraos.img" >&2
    exit 1
}

# The post-Buildroot checks of the root filesystem, the same on both
# architectures: the embedding model, the in-OS toolchain at the pinned
# version and that version only, and no SSH server.
assert_rootfs_invariants() {
    # Post-Buildroot assert. Both ways this package can fail are SILENT: a package
    # whose Config.in is not sourced from buildroot/Config.in has its defconfig
    # symbol dropped without warning, and <PKG>_NAME is a Buildroot-reserved
    # variable, so using it for the install directory redirects the payload to
    # .../models/embra-embedding-model. Either way the image builds fine and the
    # brain reports "model at: NOT FOUND" only at runtime. Fail here instead.
    EMBED_TARGET="$BUILDROOT_DIR/output/target/usr/share/embra/models/${EMBED_MODEL_NAME:-bge-small-en-v1.5}"
    for f in model.onnx tokenizer.json; do
        if [ ! -s "$EMBED_TARGET/$f" ]; then
            echo "ERROR: embedding model missing from the rootfs: $EMBED_TARGET/$f" >&2
            echo "       Semantic KG retrieval would silently fall back to lexical-only." >&2
            echo "       Check that buildroot/Config.in sources the package's Config.in," >&2
            echo "       that BR2_PACKAGE_EMBRA_EMBEDDING_MODEL=y survived into" >&2
            echo "       $BUILDROOT_DIR/.config, and that the .mk installs to" >&2
            echo "       \$(EMBRA_EMBEDDING_MODEL_DIRNAME) — never \$(EMBRA_EMBEDDING_MODEL_NAME)." >&2
            reject_image
        fi
    done
    echo "embedding model present in rootfs: $EMBED_TARGET"

    # Same check for the in-OS toolchain. The brain compares /opt/rust/RUST_VERSION
    # with the toolchain each Guardian tool was built by, so the rootfs must carry
    # the version this build pinned, and that version only: two libcore rlibs for
    # wasm32 mean an earlier toolchain's files survived in Buildroot's target dir.
    RUST_IN_TARGET="$BUILDROOT_DIR/output/target/opt/rust"
    if [ "$(cat "$RUST_IN_TARGET/RUST_VERSION" 2>/dev/null || true)" != "$RUST_TOOLCHAIN_VERSION" ] \
       || [ ! -x "$RUST_IN_TARGET/bin/cargo" ]; then
        echo "ERROR: in-OS Rust toolchain $RUST_TOOLCHAIN_VERSION missing from the rootfs: $RUST_IN_TARGET" >&2
        echo "       Guardian tools could not be built or rebuilt on this image." >&2
        reject_image
    fi
    RUST_CORE_COUNT=$(find "$RUST_IN_TARGET/lib/rustlib/wasm32-unknown-unknown/lib" \
        -name 'libcore-*.rlib' 2>/dev/null | wc -l)
    if [ "$RUST_CORE_COUNT" -ne 1 ]; then
        echo "ERROR: expected one wasm32 libcore in $RUST_IN_TARGET, found $RUST_CORE_COUNT" >&2
        echo "       Remove $RUST_IN_TARGET and rebuild." >&2
        reject_image
    fi
    echo "in-OS Rust toolchain present in rootfs: $RUST_TOOLCHAIN_VERSION"

    # No SSH server in the image (security invariant). The option is off in the
    # defconfig and post_build.sh removes what an earlier build left in the target
    # directory; this is the check that both did their work.
    for f in usr/sbin/sshd usr/libexec/sshd-session usr/libexec/sshd-auth usr/libexec/sftp-server; do
        if [ -e "$BUILDROOT_DIR/output/target/$f" ]; then
            echo "ERROR: SSH server file in the rootfs: $BUILDROOT_DIR/output/target/$f" >&2
            echo "       embraOS ships the SSH client only." >&2
            reject_image
        fi
    done
    echo "no SSH server in rootfs"
}
