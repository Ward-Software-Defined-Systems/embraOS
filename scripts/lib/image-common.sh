# shellcheck shell=bash
# What the two image scripts share (#43): the pins and the checks that are
# the same whatever the architecture, defined once so a pinned hash or a
# security invariant cannot drift between build-image.sh (x86_64) and
# build-image-aarch64.sh. Sourced by both after their option parsing; it
# defines variables and functions and runs nothing.
#
# The callers set BUILDROOT_DIR before the post-Buildroot checks, and
# EMBED_MODEL_NAME where they stage the model.

# The disk layout: the sizes genimage.cfg.in is rendered with and the GPT
# partition names, shared with post_image.sh (and read by embra-boot's test).
# shellcheck source=buildroot/board/embraos/layout.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/buildroot/board/embraos/layout.sh"
# shellcheck source=scripts/lib/disk-layout.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/disk-layout.sh"

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

# The Linux kernel both defconfigs pin: the 6.18 longterm line (2026-10-31;
# supported to the end of 2028). Buildroot 2026.02's own "latest" is the
# 6.19 series, end of life since 2026-04-22 and never fixed for
# CVE-2026-98432 (ipv4 TCP, CVSS 9.8) or CVE-2026-46300. The version moves
# on purpose, like the toolchain's, in three places that are checked against
# each other: this pin, `BR2_LINUX_KERNEL_CUSTOM_VERSION_VALUE` in both
# defconfigs (with the headers series `BR2_PACKAGE_HOST_LINUX_HEADERS_CUSTOM_<x_y>`
# of its first two digits), and the hash line in scripts/linux-kernel.sha256.
# Buildroot checks no hash for a custom kernel version (linux/linux.mk exempts
# the tarball unless the version is its latest), so `stage_kernel_tarball`
# verifies it here, against the line kernel.org signs in sha256sums.asc.
LINUX_KERNEL_VERSION="${LINUX_KERNEL_VERSION:-6.18.60}"
KERNEL_PINS="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)/linux-kernel.sha256"
KERNEL_MIRROR="${KERNEL_MIRROR:-https://cdn.kernel.org/pub/linux/kernel}"

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

# assert_kernel_pin_in_defconfig <defconfig>: the defconfig names the pinned
# kernel version and the headers series of its first two digits, so the pin
# and the defconfigs never drift apart.
assert_kernel_pin_in_defconfig() {
    local dc="$1" series
    series="${LINUX_KERNEL_VERSION%.*}"
    series="${series//./_}"
    grep -qxF -- "BR2_LINUX_KERNEL_CUSTOM_VERSION_VALUE=\"${LINUX_KERNEL_VERSION}\"" "$dc" || {
        echo "ERROR: $dc does not pin BR2_LINUX_KERNEL_CUSTOM_VERSION_VALUE=\"${LINUX_KERNEL_VERSION}\"" >&2
        echo "  LINUX_KERNEL_VERSION (scripts/lib/image-common.sh), both defconfigs and scripts/linux-kernel.sha256 move together." >&2
        exit 1
    }
    grep -qxF -- "BR2_PACKAGE_HOST_LINUX_HEADERS_CUSTOM_${series}=y" "$dc" || {
        echo "ERROR: $dc does not select the kernel headers series BR2_PACKAGE_HOST_LINUX_HEADERS_CUSTOM_${series}=y for ${LINUX_KERNEL_VERSION}" >&2
        exit 1
    }
}

# stage_kernel_tarball <buildroot dir>: the kernel tarball in Buildroot's
# download directory, verified against the pin — downloaded when absent,
# verified either way, so Buildroot finds it there and downloads nothing. A
# file that does not match its pin is removed and the build stops; the
# expected hash is the pin in the repository, never a checksum fetched
# beside the download.
stage_kernel_tarball() {
    local br="$1" tb="linux-${LINUX_KERNEL_VERSION}.tar.xz" dl pin site
    dl="$br/dl/linux"
    site="$KERNEL_MIRROR/v${LINUX_KERNEL_VERSION%%.*}.x"
    pin="$(grep -F -- "  ${tb}" "$KERNEL_PINS" | head -n1 || true)"
    [ -n "$pin" ] || {
        echo "ERROR: no pinned sha256 for ${tb} in $KERNEL_PINS" >&2
        echo "  The pin moves with LINUX_KERNEL_VERSION: take the line from kernel.org's signed" >&2
        echo "  sha256sums.asc ($site/sha256sums.asc) and add it." >&2
        exit 1
    }
    mkdir -p "$dl"
    if [ ! -f "$dl/$tb" ]; then
        echo "  downloading $tb"
        curl -fSL --retry 3 -o "$dl/$tb.part" "$site/$tb" \
            || { rm -f "$dl/$tb.part"; echo "ERROR: download of ${tb} failed" >&2; exit 1; }
        mv "$dl/$tb.part" "$dl/$tb"
    fi
    ( cd "$dl" && printf '%s\n' "$pin" | sha256sum -c - ) \
        || { rm -f "$dl/$tb"; echo "ERROR: ${tb} does not match its pin in $KERNEL_PINS; the file was removed" >&2; exit 1; }
}

# size_to_bytes <genimage size>: 64M, 1024M, 4G → bytes.
size_to_bytes() {
    local s="$1" n unit
    n="${s%[KMGkmg]}"
    unit="${s#"$n"}"
    case "$unit" in
        K|k) echo $((n * 1024)) ;;
        M|m) echo $((n * 1024 * 1024)) ;;
        G|g) echo $((n * 1024 * 1024 * 1024)) ;;
        "")  echo "$n" ;;
        *)   echo "ERROR: unknown size unit in '$s'" >&2; return 1 ;;
    esac
}

# write_release_file <rust triple> <storage engine>: the stamp of this build,
# written beside the binaries Buildroot picks up; post_build.sh installs it
# as /etc/embraos-release and derives GRUB's slot line from it. A
# --buildroot-only run reuses the last full build's file, so the stamp always
# describes the binaries in the image (the baked engine has the same
# contract). BUILD_ID is the commit, with -dirty when the tree had changes.
write_release_file() {
    local triple="$1" engine="$2" version build_id dirty="" out
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
    [ -n "$version" ] || { echo "ERROR: no workspace version in Cargo.toml" >&2; exit 1; }
    build_id="$(git rev-parse --short=10 HEAD 2>/dev/null || echo unknown)"
    git diff-index --quiet HEAD -- 2>/dev/null || dirty="-dirty"
    out="target/${triple}/release/embraos-release"
    mkdir -p "$(dirname "$out")"
    {
        echo "VERSION=${version}"
        echo "BUILD_ID=${build_id}${dirty}"
        echo "ARCH=${triple%%-*}"
        echo "ENGINE=${engine}"
        echo "BUILT=$(date -u +%Y-%m-%dT%H:%M:%SZ)"
    } > "$out"
    echo "release stamp ($out): $(tr '\n' ' ' < "$out")"
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
# version and that version only, no SSH server, and the slot's contents (the
# kernel, the initramfs, the release stamp, GRUB's slot line, a squashfs that
# fits its slot and is xz). Takes the Rust triple of the build.
assert_rootfs_invariants() {
    local triple="${1:-}"
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

    # The slot's contents. Each OS slot is one squashfs that carries its own
    # kernel (installed by Buildroot, BR2_LINUX_KERNEL_INSTALL_TARGET) and the
    # initramfs scripts/create_initramfs.sh wrote; GRUB loads both from the
    # squashfs. A stale initramfs — an older embra-init under
    # --buildroot-only — is caught by hashing the archive's init against the
    # binary that was built.
    local tgt="$BUILDROOT_DIR/output/target" kernel="" k
    for k in Image bzImage; do
        [ -f "$BUILDROOT_DIR/output/images/$k" ] && { kernel="$k"; break; }
    done
    [ -n "$kernel" ] || { echo "ERROR: no kernel image in $BUILDROOT_DIR/output/images" >&2; reject_image; }
    cmp -s "$tgt/boot/$kernel" "$BUILDROOT_DIR/output/images/$kernel" || {
        echo "ERROR: /boot/$kernel in the rootfs is not the kernel Buildroot built (BR2_LINUX_KERNEL_INSTALL_TARGET)" >&2
        reject_image
    }
    cmp -s "$tgt/boot/initramfs.cpio.gz" initramfs.cpio.gz || {
        echo "ERROR: /boot/initramfs.cpio.gz in the rootfs is not the repository's initramfs.cpio.gz" >&2
        reject_image
    }
    if [ -n "$triple" ] && [ -f "target/$triple/release/embra-init" ]; then
        local in_image built
        # create_initramfs.sh stores the member as `init`; an archive a BSD
        # cpio wrote before that (macOS) names it `./init`. Both are read.
        in_image="$(zcat "$tgt/boot/initramfs.cpio.gz" | cpio -i --quiet --to-stdout init ./init 2>/dev/null | sha256sum | cut -d' ' -f1)"
        built="$(sha256sum "target/$triple/release/embra-init" | cut -d' ' -f1)"
        [ "$in_image" = "$built" ] || {
            echo "ERROR: the initramfs carries another embra-init than the one built for $triple" >&2
            echo "       Run scripts/create_initramfs.sh (RUST_TARGET=$triple) and build again." >&2
            reject_image
        }
    fi
    local key stamped want
    for key in VERSION BUILD_ID ARCH ENGINE BUILT; do
        grep -q "^${key}=." "$tgt/etc/embraos-release" 2>/dev/null || {
            echo "ERROR: /etc/embraos-release in the rootfs lacks ${key} (post_build.sh installs write_release_file's stamp)" >&2
            reject_image
        }
    done
    stamped="$(sed -n 's/^VERSION=//p' "$tgt/etc/embraos-release")"
    want="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
    [ "$stamped" = "$want" ] || {
        echo "ERROR: /etc/embraos-release says VERSION=$stamped, the workspace is $want; run a full build" >&2
        reject_image
    }
    grep -q '^set slot_version="' "$tgt/boot/embraos-slot.cfg" 2>/dev/null || {
        echo "ERROR: /boot/embraos-slot.cfg is missing or malformed in the rootfs" >&2
        reject_image
    }
    echo "slot contents present: /boot/$kernel, /boot/initramfs.cpio.gz, /etc/embraos-release ($stamped), /boot/embraos-slot.cfg"

    # The network: the resolver is a link into the ephemeral tmpfs (the root
    # is read-only; embrad and the lease script write there), and the lease
    # script udhcpc runs is in place and executable.
    if [ "$(readlink "$tgt/etc/resolv.conf" 2>/dev/null)" != "/embra/ephemeral/resolv.conf" ]; then
        reject_image "/etc/resolv.conf in the rootfs is not the link to /embra/ephemeral/resolv.conf (post_build.sh)"
    fi
    [ -x "$tgt/usr/share/embra/udhcpc.script" ] \
        || reject_image "/usr/share/embra/udhcpc.script is missing or not executable in the rootfs (the overlay)"
    echo "network: /etc/resolv.conf -> /embra/ephemeral/resolv.conf, the udhcpc lease script present"
    # The updater: embrad runs it in a maintenance boot, the brain for the
    # operator's commands. A dropped package symbol is silent.
    [ -x "$tgt/usr/bin/embra-update" ] || {
        echo "ERROR: /usr/bin/embra-update is missing from the rootfs (BR2_PACKAGE_EMBRA_UPDATE)" >&2
        reject_image
    }
    cmp -s "$tgt/usr/share/embra/update-signing.pub" scripts/update-signing.pub || {
        echo "ERROR: /usr/share/embra/update-signing.pub in the rootfs is not scripts/update-signing.pub" >&2
        reject_image
    }
    if grep -q 'BEGIN PUBLIC KEY' scripts/update-signing.pub; then
        echo "release signing keys present in rootfs: $(grep -c 'BEGIN PUBLIC KEY' scripts/update-signing.pub)"
    else
        echo "release signing keys: none yet (scripts/update-signing.pub holds no key; the updater refuses every manifest until one is added)"
    fi

    # The squashfs fits its slot with the margin free (layout.sh), and it is
    # xz: GRUB 2.12's squash4 reads gzip, lzo and xz, never zstd.
    local sq="$BUILDROOT_DIR/output/images/rootfs.squashfs" sq_size slot_bytes limit
    sq_size="$(stat -c %s "$sq")"
    slot_bytes="$(size_to_bytes "$EMBRAOS_SLOT_SIZE")"
    limit=$((slot_bytes * (100 - EMBRAOS_SLOT_MARGIN_PCT) / 100))
    [ "$sq_size" -le "$limit" ] || {
        echo "ERROR: rootfs.squashfs is $sq_size bytes; a slot is $slot_bytes bytes and the margin leaves $limit" >&2
        echo "       Grow EMBRAOS_SLOT_SIZE in buildroot/board/embraos/layout.sh or shrink the rootfs." >&2
        reject_image
    }
    "$BUILDROOT_DIR/output/host/bin/unsquashfs" -s "$sq" 2>/dev/null | grep -q 'Compression xz' || {
        echo "ERROR: rootfs.squashfs is not xz-compressed; GRUB reads gzip, lzo and xz squashfs only" >&2
        reject_image
    }
    echo "rootfs.squashfs: $sq_size bytes of a $slot_bytes-byte slot, xz"
}

# assert_image_layout <image>: the partitions carry the names of layout.sh in
# order, the first starts at sector 2048 and every start is a multiple of
# 2048 — checked on the final image, not trusted from genimage's `align` —
# and the image is the whole number of MiB layout.sh computes with the backup
# GPT on its last sector, not trusted from genimage's `size`.
assert_image_layout() {
    local img="$1" expected rows row nr start name i=0
    read -r -a expected <<< "$EMBRAOS_PARTITION_NAMES"
    mapfile -t rows < <(partx -g -o NR,START,NAME -- "$img" 2>/dev/null)
    [ "${#rows[@]}" -eq "${#expected[@]}" ] || {
        echo "ERROR: $img has ${#rows[@]} partitions, expected ${#expected[@]} ($EMBRAOS_PARTITION_NAMES)" >&2
        reject_image
    }
    for row in "${rows[@]}"; do
        read -r nr start name <<< "$row"
        [ "$name" = "${expected[$i]}" ] || {
            echo "ERROR: partition $nr of $img is named '$name', expected '${expected[$i]}'" >&2
            reject_image
        }
        if [ "$i" -eq 0 ] && [ "$start" -ne 2048 ]; then
            echo "ERROR: partition 1 of $img starts at sector $start, expected 2048" >&2
            reject_image
        fi
        [ $((start % 2048)) -eq 0 ] || {
            echo "ERROR: partition $nr ($name) of $img starts at sector $start, not a multiple of 2048" >&2
            reject_image
        }
        i=$((i + 1))
    done
    embraos_check_whole_mib_gpt "$img" || reject_image
    # The ESP carries the loader, grub.cfg and grubenv, and grubenv names
    # slot A active: read through mtools at the partition's offset.
    local esp_start mdir mtype listing f
    read -r _ esp_start _ <<< "${rows[0]}"
    mdir="$BUILDROOT_DIR/output/host/bin/mdir"
    mtype="$BUILDROOT_DIR/output/host/bin/mtype"
    listing="$("$mdir" -i "${img}@@$((esp_start * 512))" -b ::/EFI/BOOT 2>/dev/null || true)"
    for f in grub.cfg grubenv; do
        echo "$listing" | grep -qi "/${f}\$" || {
            echo "ERROR: the ESP of $img lacks EFI/BOOT/${f}" >&2
            reject_image
        }
    done
    echo "$listing" | grep -qiE '/boot(x64|aa64)\.efi$' || {
        echo "ERROR: the ESP of $img lacks a GRUB loader under EFI/BOOT" >&2
        reject_image
    }
    "$mtype" -i "${img}@@$((esp_start * 512))" ::/EFI/BOOT/grubenv 2>/dev/null | grep -qx 'slot_active=a' || {
        echo "ERROR: grubenv on the ESP of $img does not name slot A active" >&2
        reject_image
    }
    echo "image layout: $EMBRAOS_PARTITION_NAMES, 1 MiB aligned, $EMBRAOS_IMAGE_SIZE_MIB MiB with the backup GPT on the last sector; the ESP carries the loader, grub.cfg and grubenv (slot A active)"
}

# write_vm_images <raw image> <out dir> <arch> <engine>: the same disk for the
# other hypervisors, converted from the raw GPT image after it passed the
# checks above. embraos.vmdk (monolithicSparse: Parallels, VMware Fusion and
# Workstation, VirtualBox), embraos.vhdx (dynamic: Hyper-V generation 2 with
# Secure Boot off) and embraos.ova (the OVF descriptor rendered from
# embraos.ovf.in beside this file, the disk as a streamOptimized VMDK and the
# SHA256 manifest, in that order: ESXi, vCenter, Fusion). The zero regions,
# the empty slot B and the unused DATA, are not stored, so each file is about
# the size of the data written. Needs qemu-img (Ubuntu: qemu-utils; macOS:
# Homebrew's qemu). Each converted disk must report the raw image's virtual
# size, a VMDK's capacity must be whole 64 KiB grains (VMware's sparse-extent
# format asks for it; a whole-MiB image gives it) and its embedded descriptor
# must name the file it is in; the OVA's member order and manifest are checked
# after packing. A failure removes the staged file and stops the build.
write_vm_images() {
    local raw="$1" out="$2" arch="$3" engine="$4"
    local name="embraos" template version build_id raw_size
    command -v qemu-img >/dev/null 2>&1 || {
        echo "ERROR: qemu-img not found; the VM images need it (Ubuntu: apt-get install qemu-utils; macOS: brew install qemu)" >&2
        exit 1
    }
    template="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/embraos.ovf.in"
    [ -f "$template" ] || { echo "ERROR: $template is missing" >&2; exit 1; }
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
    build_id="$(git rev-parse --short=10 HEAD 2>/dev/null || echo unknown)"
    raw_size="$(file_size "$raw")"
    mkdir -p "$out"

    # convert_checked <qemu format> <-o options or ""> <target>
    # The disk is converted under its final name inside a staging directory
    # and moved across once it checks out. The name matters: qemu-img writes
    # the file name it was given into a VMDK's embedded descriptor as the
    # extent's name, and VMware and Parallels open the extent by that name,
    # so a disk converted as <name>.partial and renamed is "invalid" to both
    # (QEMU reads the one-file layouts without the name, which is why the
    # size check alone did not catch it). vmdk_extent_name reads the name
    # back, and a VMDK whose descriptor names another file stops the build.
    local staging="$out/.convert-stage"
    vmdk_extent_name() {   # <vmdk> → the file the descriptor's first extent line names
        local names
        names="$(head -c 65536 "$1" | LC_ALL=C tr -cd '\12\40-\176' \
            | LC_ALL=C sed -n 's/^RW [0-9][0-9]* SPARSE "\([^"]*\)".*$/\1/p')"
        printf '%s\n' "${names%%$'\n'*}"
    }
    convert_checked() {
        local fmt="$1" opts="$2" target="$3" base work vsize extent
        base="$(basename "$target")"
        work="$staging/$base"
        rm -rf "$staging"
        mkdir -p "$staging"
        rm -f "$target"
        if [ -n "$opts" ]; then
            qemu-img convert -f raw -O "$fmt" -o "$opts" "$raw" "$work"
        else
            qemu-img convert -f raw -O "$fmt" "$raw" "$work"
        fi
        vsize="$(qemu-img info "$work" | sed -n 's/^virtual size: .*(\([0-9]*\) bytes)$/\1/p')"
        [ "$vsize" = "$raw_size" ] || {
            echo "ERROR: $target reports a virtual size of ${vsize:-?} bytes; the raw image has $raw_size" >&2
            rm -rf "$staging"
            exit 1
        }
        if [ "$fmt" = vmdk ]; then
            [ $((vsize % 65536)) -eq 0 ] || {
                echo "ERROR: $target has a capacity of $vsize bytes, not whole 64 KiB grains; the raw image is not a whole number of MiB" >&2
                rm -rf "$staging"
                exit 1
            }
            extent="$(vmdk_extent_name "$work")"
            [ "$extent" = "$base" ] || {
                echo "ERROR: the descriptor inside $target names its extent '${extent:-?}', not '$base'; VMware and Parallels would refuse the disk" >&2
                rm -rf "$staging"
                exit 1
            }
        fi
        mv "$work" "$target"
        rm -rf "$staging"
    }

    convert_checked vmdk "" "$out/${name}.vmdk"
    convert_checked vhdx "subformat=dynamic" "$out/${name}.vhdx"

    local stage="$out/.ova-stage" disk="${name}-disk1.vmdk" ovf="${name}.ovf" mf="${name}.mf" disk_size
    rm -rf "$stage" "$out/${name}.ova" "$out/${name}.ova.partial"
    mkdir -p "$stage"
    convert_checked vmdk "subformat=streamOptimized" "$stage/$disk"
    disk_size="$(file_size "$stage/$disk")"
    sed -e "s|@DISK_FILE@|$disk|g" -e "s|@DISK_FILE_SIZE@|$disk_size|g" \
        -e "s|@CAPACITY@|$raw_size|g" -e "s|@NAME@|$name|g" \
        -e "s|@VERSION@|$version|g" -e "s|@BUILD_ID@|$build_id|g" \
        -e "s|@ARCH@|$arch|g" -e "s|@ENGINE@|$engine|g" \
        -e "s|@CPUS@|4|g" -e "s|@MEMORY_MB@|4096|g" -e "s|@SYSTEM_TYPE@|vmx-20|g" \
        "$template" > "$stage/$ovf"
    grep -q '@[A-Z_]*@' "$stage/$ovf" && { echo "ERROR: an OVF placeholder was not rendered" >&2; exit 1; }
    (
        cd "$stage" || exit 1
        {
            echo "SHA256($ovf)= $(sha256_of "$ovf")"
            echo "SHA256($disk)= $(sha256_of "$disk")"
        } > "$mf"
        # The OVF spec wants the descriptor first; the manifest after what it covers.
        tar --format=ustar -cf "../${name}.ova.partial" "$ovf" "$disk" "$mf"
    )
    [ "$(tar -tf "$out/${name}.ova.partial" | tr '\n' ' ')" = "$ovf $disk $mf " ] || {
        echo "ERROR: the OVA's members are not ovf, disk, manifest in that order" >&2
        rm -rf "$stage" "$out/${name}.ova.partial"
        exit 1
    }
    mv "$out/${name}.ova.partial" "$out/${name}.ova"
    rm -rf "$stage"
    echo "VM images in $out: ${name}.vmdk ($(( $(file_size "$out/${name}.vmdk") / 1048576 )) MiB), ${name}.vhdx ($(( $(file_size "$out/${name}.vhdx") / 1048576 )) MiB), ${name}.ova ($(( $(file_size "$out/${name}.ova") / 1048576 )) MiB; virtual size $raw_size bytes)"
}

# GNU and BSD spellings of two small things the conversion needs.
file_size() { stat -c %s "$1" 2>/dev/null || stat -f %z "$1"; }
sha256_of() { (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1") | cut -d' ' -f1; }
