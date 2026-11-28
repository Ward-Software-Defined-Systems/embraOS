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
# The builder's own certificate authorities (Imported_CAs/): the check both
# image scripts run before anything is built, shared with post_build.sh, which
# bakes the folder, and with release-update.sh, which signs no release from it.
# shellcheck source=scripts/lib/imported-cas.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/imported-cas.sh"
# The build host's directories stay out of the programs and the root
# filesystem: the stand-ins rustc is handed, the records post_build.sh
# removes, and the functions the checks below read the result back with.
# shellcheck source=scripts/lib/build-paths.sh
source "$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/build-paths.sh"

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

# Intel's microcode bundle is pinned by its recipe: the version in
# buildroot/package/embra-intel-microcode/embra-intel-microcode.mk and the
# archive's sha256 in the hash file beside it, which Buildroot checks at the
# download (a wrong hash or a missing line stops the build). Buildroot's own
# intel-microcode recipe names one release per Buildroot version (2026.02.1:
# 20260210-rev1), so that package is not selected. assert_boot_initrds proves
# that the image carries the files and the licence of the bundle the recipe
# names.
INTEL_MICROCODE_RECIPE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/buildroot/package/embra-intel-microcode/embra-intel-microcode.mk"

# The model server of native models (experimental) is pinned by its recipe as
# well: the version in buildroot/package/embra-llama-cpp/embra-llama-cpp.mk
# and the archive's sha256 in the hash file beside it. Buildroot's own
# llama-cpp recipe names an older build, so that package is not selected.
# assert_native_model_runtime reads the root filesystem back against the
# build of the version the recipe names.
LLAMA_CPP_RECIPE="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/buildroot/package/embra-llama-cpp/embra-llama-cpp.mk"

# The Wi-Fi firmware of MediaTek's MT7922 is pinned by a recipe too:
# buildroot/package/embra-mt7922-firmware names a release of linux-firmware,
# and its hash file the sha256 of the card's two files and of MediaTek's
# licence text. The release Buildroot's own package carries has a file the
# kernel's driver refuses. assert_mt7921_power_tables proves that the image
# holds the pinned files and that the driver takes every power table of its
# family.
MT7922_FIRMWARE_PINS="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/buildroot/package/embra-mt7922-firmware/embra-mt7922-firmware.hash"

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

# intel_microcode_version: the bundle version the recipe names, from its one
# `EMBRA_INTEL_MICROCODE_VERSION = <version>` line. Nothing is printed and
# the status is 1 when the recipe has no line of that form, or more than one.
intel_microcode_version() {
    local v
    v="$(sed -n 's/^EMBRA_INTEL_MICROCODE_VERSION = \([0-9A-Za-z._-]\{1,\}\)$/\1/p' "$INTEL_MICROCODE_RECIPE" 2>/dev/null || true)"
    [ -n "$v" ] || return 1
    [ "$(printf '%s\n' "$v" | wc -l)" -eq 1 ] || return 1
    printf '%s\n' "$v"
}

# llama_cpp_version: the release of llama.cpp the recipe names, from its one
# `EMBRA_LLAMA_CPP_VERSION = <version>` line. Nothing is printed and the
# status is 1 when the recipe has no line of that form, or more than one.
llama_cpp_version() {
    local v
    v="$(sed -n 's/^EMBRA_LLAMA_CPP_VERSION = \([0-9A-Za-z._-]\{1,\}\)$/\1/p' "$LLAMA_CPP_RECIPE" 2>/dev/null || true)"
    [ -n "$v" ] || return 1
    [ "$(printf '%s\n' "$v" | wc -l)" -eq 1 ] || return 1
    printf '%s\n' "$v"
}

# clean_package_with_changed_recipe <buildroot dir> <package>: a package of
# this tree that Buildroot compiles from a tarball is built once. Buildroot
# rebuilds nothing whose options changed, so a recipe changed under
# buildroot/package/<package>/ never reaches a build directory that exists
# already: the image carries the build of the recipe before, and nothing
# says so. The package therefore has a stamp beside the build directories,
# the sha256 over its recipe files' names and bytes, and with another stamp
# its build directories are removed here, before make: the one of the
# version the recipe names, which is what `make <package>-dirclean` removes,
# and any a version before left. The build directory is all such a package
# keeps outside the target directory, which both image scripts repopulate.
# Called by both of them for the model server.
clean_package_with_changed_recipe() {
    local br="$1" pkg="$2" dir stamp have sum d
    dir="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/buildroot/package/$pkg"
    [ -d "$dir" ] || { echo "ERROR: no recipe directory $dir" >&2; exit 1; }
    stamp="$br/output/build/.embraos-recipe-$pkg"
    mkdir -p "$br/output/build"
    have="$(cd "$dir" && find . -type f | LC_ALL=C sort)"
    sum="$(cd "$dir" && printf '%s\n' "$have" | while IFS= read -r f; do
               [ -n "$f" ] || continue; printf '%s\n' "$f"; cat "$f"
           done | sha256sum | cut -d' ' -f1)"
    [ "$(cat "$stamp" 2>/dev/null || true)" = "$sum" ] && return 0
    for d in "$br/output/build/$pkg"-[0-9]*/; do
        [ -d "$d" ] || continue
        echo "recipe: buildroot/package/$pkg is not what ${d%/} was built from; removed, and $pkg builds again from its tarball"
        rm -rf "${d%/}"
    done
    printf '%s\n' "$sum" > "$stamp"
}

# clean_packages_with_changed_patches <buildroot dir> <patches dir>: Buildroot
# applies a package's patches once, when it extracts the package, so a patch
# added, changed or removed under buildroot/patches/<package>/
# (BR2_GLOBAL_PATCH_DIR) never reaches a build directory that exists already:
# the image is built without it and nothing says so. Each package with a
# patch directory therefore has a stamp beside the build directories, the
# sha256 over its patch files' names and bytes, and a package whose stamp
# differs is cleaned here, before make, to build again from its tarball with
# the patches of this checkout; so is a package whose patch directory is gone.
# A tree built before the stamps existed is taken at Buildroot's word: when
# the patches it recorded as applied (.applied_patches_list) carry the names
# this checkout has, the stamp is written and nothing is cleaned.
clean_packages_with_changed_patches() {
    local br="$1" patches="$2" stamps dir pkg have sum stamp built applied d
    stamps="$br/output/build"
    mkdir -p "$stamps"
    for dir in "$patches"/*/; do
        [ -d "$dir" ] || continue
        pkg="$(basename "$dir")"
        stamp="$stamps/.embraos-patches-$pkg"
        have="$(cd "$dir" && find . -type f | LC_ALL=C sort)"
        [ -n "$have" ] || [ -e "$stamp" ] || continue
        sum="$(cd "$dir" && printf '%s\n' "$have" | while IFS= read -r f; do
                   [ -n "$f" ] || continue; printf '%s\n' "$f"; cat "$f"
               done | sha256sum | cut -d' ' -f1)"
        [ "$(cat "$stamp" 2>/dev/null || true)" = "$sum" ] && continue
        built=""
        for d in "$stamps/$pkg"-[0-9]*/; do
            [ -d "$d" ] && { built="${d%/}"; break; }
        done
        if [ ! -e "$stamp" ] && [ -n "$built" ] && [ -f "$built/.applied_patches_list" ]; then
            applied="$(grep -F "/patches/$pkg/" "$built/.applied_patches_list" | sed 's|.*/|./|' | LC_ALL=C sort || true)"
            if [ "$applied" = "$have" ]; then
                printf '%s\n' "$sum" > "$stamp"
                continue
            fi
        fi
        # nothing built yet says nothing: the package is about to be built with these patches anyway
        [ -z "$built" ] || echo "patches: buildroot/patches/$pkg is not what $pkg was built with; the package builds again from its tarball"
        clean_buildroot_package "$br" "$pkg"
        printf '%s\n' "$sum" > "$stamp"
    done
    for stamp in "$stamps"/.embraos-patches-*; do
        [ -e "$stamp" ] || continue
        pkg="${stamp##*/.embraos-patches-}"
        [ -d "$patches/$pkg" ] && continue
        echo "patches: buildroot/patches/$pkg is gone; $pkg builds again from its tarball without them"
        clean_buildroot_package "$br" "$pkg"
        rm -f "$stamp"
    done
}

# clean_buildroot_package <buildroot dir> <package>: make <package>-dirclean,
# and the check that it worked. A package this configuration does not build
# has no such target and no build directory, which is fine; a build directory
# still there afterwards would keep the old patches, and stops the build.
clean_buildroot_package() {
    local br="$1" pkg="$2" d
    ( cd "$br" && make "${pkg}-dirclean" ) >/dev/null 2>&1 || true
    for d in "$br/output/build/$pkg"-[0-9]*/; do
        [ -d "$d" ] || continue
        echo "ERROR: $pkg was not cleaned (${d%/} is still there): run 'make ${pkg}-dirclean' in $br, then build again" >&2
        exit 1
    done
}

# remove_stale_package_dirs <buildroot dir> <package>...: a build directory
# is named <package>-<recipe version>, and `make <package>-dirclean` knows
# the version the recipe has now and no other. A directory of one of this
# tree's own packages that is still there after that dirclean carries a
# version its recipe had before: a copy of the release directory, gigabytes
# that no build reads again and none removes. It is removed and named.
# Called after the dirclean of the same packages, never before it, and
# never for a package Buildroot compiles: there the directory is the build.
remove_stale_package_dirs() {
    local br="$1" pkg d
    shift
    for pkg in "$@"; do
        for d in "$br/output/build/$pkg"-[0-9]*/; do
            [ -d "$d" ] || continue
            echo "build directory ${d%/} outlived the dirclean of $pkg (a former version of its recipe names it): removed"
            rm -rf "${d%/}"
        done
    done
    return 0
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
# contract). BUILD_ID is the commit, with -dirty when a tracked file differs
# from it. The index is refreshed before it is asked, as git's own scripts
# do it (require_clean_work_tree): git diff-index goes by the stat data the
# index caches for each file (owner, device, inode, times) and reads none, so
# an index that another view of the tree wrote calls every file changed. A
# Docker pass on a Mac leaves one: Buildroot asks its external tree for a
# version on every make (support/scripts/setlocalversion), and that refreshes
# this repository's index from inside the container, as root through the bind
# mount. The host pass after it stamped -dirty with no file changed
# (2026-11-13). The refresh reads the files whose stat data is off and keeps
# as changed what differs; where it cannot write the index the test is the
# plain one and errs towards -dirty, which a release refuses. A file git does
# not track is not seen: what a build bakes from Imported_Intelligence/ and
# Seed_Knowledge/ beside the commit's files is for release-update.sh to refuse.
write_release_file() {
    local triple="$1" engine="$2" version build_id dirty="" out
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
    [ -n "$version" ] || { echo "ERROR: no workspace version in Cargo.toml" >&2; exit 1; }
    build_id="$(git rev-parse --short=10 HEAD 2>/dev/null || echo unknown)"
    git update-index -q --refresh >/dev/null 2>&1 || true
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
    [ $# -eq 0 ] || echo "ERROR: $1" >&2
    rm -f "$BUILDROOT_DIR/output/images/embraos.img"
    echo "       The image this build produced was removed:" >&2
    echo "       $BUILDROOT_DIR/output/images/embraos.img" >&2
    exit 1
}

# The target directory holds the files of this configuration and nothing
# else. Buildroot never removes what a deselected package installed and never
# rebuilds a cached package whose options changed, so both image scripts
# remove output/target and every target install stamp before make and touch a
# marker; every package of the configuration then reinstalls into the fresh
# directory. This is the check that it happened: the marker is older than
# every install stamp, the files of the retired graphical experiment are
# absent by name, one modules directory, udevd exactly with eudev.
# (Buildroot's per-package .files-list.txt cannot serve here: its "before"
# snapshot is taken in a step that does not re-run, so a reinstalled package's
# list is empty.)
assert_target_is_clean() {
    local tgt="$BUILDROOT_DIR/output/target" cfg="$BUILDROOT_DIR/.config"
    local marker="$BUILDROOT_DIR/output/build/.embraos-target-reseeded" mods f stale
    [ -f "$marker" ] \
        || reject_image "the target directory was not repopulated before this build (no $marker): the image scripts remove output/target and the install stamps before make"
    stale="$(find "$BUILDROOT_DIR/output/build" -maxdepth 2 -name .stamp_target_installed ! -newer "$marker" -printf '%h\n' \
             | sed 's|.*/||' | sort | tr '\n' ' ')"
    [ -z "$stale" ] \
        || reject_image "not reinstalled after the target directory was repopulated: ${stale% } (a stale target directory)"
    # One modules directory, the pinned kernel's: a second one is an earlier
    # kernel's install that outlived its configuration.
    mods="$(ls "$tgt/lib/modules" 2>/dev/null | tr '\n' ' ')"
    [ "$mods" = "$LINUX_KERNEL_VERSION " ] \
        || reject_image "lib/modules holds '${mods% }' in the rootfs, expected the pinned kernel $LINUX_KERNEL_VERSION alone (a stale target directory)"
    # udevd exactly when eudev is built (the display console's input).
    if grep -q '^BR2_PACKAGE_EUDEV=y' "$cfg"; then
        [ -x "$tgt/sbin/udevd" ] || reject_image "/sbin/udevd is missing from the rootfs though BR2_PACKAGE_EUDEV=y"
    else
        [ ! -e "$tgt/sbin/udevd" ] || reject_image "/sbin/udevd is in the rootfs though eudev is not built (a stale target directory)"
    fi
    # The retired graphical experiment (2026-05), by name.
    for f in usr/bin/embra-desktop usr/bin/cage; do
        [ ! -e "$tgt/$f" ] || reject_image "/$f is in the rootfs: a file of the retired embra-desktop experiment (a stale target directory)"
    done
    if ls "$tgt"/usr/lib/libwlroots-* >/dev/null 2>&1; then
        reject_image "wlroots is in the rootfs: a file of the retired embra-desktop experiment (a stale target directory)"
    fi
    echo "target directory clean: repopulated for this build, one modules directory ($LINUX_KERNEL_VERSION)"
}

# The post-Buildroot checks of the root filesystem, the same on both
# architectures: the target directory holds this configuration's files and
# nothing else, the embedding model, the in-OS toolchain at the pinned
# version and that version only, no SSH server, and the slot's contents (the
# kernel, the initramfs, the release stamp, GRUB's slot line, exactly those
# under /boot, a squashfs that fits its slot and is xz), and no directory of
# the build host in any file. Takes the Rust triple of the build.
# assert_kernel_fragments_applied <triple>: every `CONFIG_X=y` and
# `CONFIG_X=m` line of the kernel fragments this architecture merges
# (board/embraos/linux.fragment for both, linux-<arch>.fragment beside it
# where one exists) stands as written in the built configuration, and every
# `# CONFIG_X is not set` line is unset. The
# merge drops a symbol whose dependency the base config lacks with one notice
# in a long log; this reads the result back. A dropped symbol is a missing
# subsystem line in the fragment (a driver is never built in above its
# subsystem), never a reason to relax the check.
assert_kernel_fragments_applied() {
    local triple="$1" arch="${1%%-*}" config fragments f line sym state missing="" count=0
    config="$BUILDROOT_DIR/output/build/linux-${LINUX_KERNEL_VERSION}/.config"
    [ -f "$config" ] || reject_image "the built kernel configuration is missing: $config (LINUX_KERNEL_VERSION=$LINUX_KERNEL_VERSION)"
    fragments="buildroot/board/embraos/linux.fragment"
    [ -f "buildroot/board/embraos/linux-${arch}.fragment" ] && fragments="$fragments buildroot/board/embraos/linux-${arch}.fragment"
    for f in $fragments; do
        while IFS= read -r line; do
            case "$line" in
                CONFIG_*=y|CONFIG_*=m)
                    count=$((count + 1))
                    if ! grep -q -x "$line" "$config"; then
                        # What the built configuration says instead: =y or =m, unset, or nothing at all.
                        state="$(grep -E "^(# )?${line%%=*}[ =]" "$config" | head -n1 | sed -e 's/.*=//' -e 's/# .* is not set/unset/' || true)"
                        missing="$missing ${line%%=*}(${state:-absent})"
                    fi
                    ;;
                "# CONFIG_"*" is not set")
                    sym="${line#\# }"; sym="${sym%% is not set}"
                    count=$((count + 1))
                    grep -q -E "^${sym}=" "$config" && missing="$missing ${sym}(set)"
                    ;;
            esac
        done < "$f"
    done
    [ -z "$missing" ] || reject_image "kernel fragment symbols not applied in the built configuration (a dependency the base config lacks: name the subsystem in the fragment):${missing}"
    echo "kernel fragments applied: $count symbols of ${fragments// / + } are in the built configuration"
    # The modules. The wireless drivers and the 802.11 stack under them are
    # modules on both architectures, the only ones the OS loads (embrad, from
    # /usr/share/embra/wireless.alias). On x86_64 nothing else may be one:
    # the ten the base config built as modules are built in by the fragments,
    # so every module in the rootfs is part of that stack or something it
    # depends on (modules.dep, followed to its end). The arm64 base config
    # keeps its own modules (1383 of them, 71 MB under lib/modules, on the
    # Apple Silicon build of 2026-11-09). Its own wireless drivers are on
    # embrad's list like the fragment's, and the build holds them to their
    # firmware the same way; nothing loads the others.
    local moddir="$BUILDROOT_DIR/output/target/lib/modules/$LINUX_KERNEL_VERSION" wireless
    # shellcheck source=/dev/null
    . buildroot/board/embraos/wireless.sh
    wireless="$(embraos_wireless_modules "$moddir" | wc -l)"
    [ "$wireless" -gt 0 ] || reject_image "the rootfs carries no wireless driver module under lib/modules/$LINUX_KERNEL_VERSION (the Wi-Fi block of board/embraos/linux.fragment)"
    if [ "$arch" = "x86_64" ]; then
        local others
        others="$(embraos_modules_outside_wireless "$moddir" | tr '\n' ' ')"
        [ -z "$others" ] || reject_image "the x86_64 rootfs carries kernel modules that are no wireless driver and nothing one depends on: ${others% } (a base-config module the fragments do not build in; nothing loads it)"
        echo "kernel modules: $(find "$moddir" -name '*.ko' | wc -l) on x86_64, each a wireless driver or something one depends on ($wireless drivers)"
    else
        echo "kernel modules: $wireless wireless drivers among the base configuration's modules"
    fi
}

# assert_firmware_families: the one symbol that selects every linux-firmware
# family (buildroot/package/embra-firmware/Config.in) names as many families
# as Buildroot's package offers. A Buildroot bump that adds a family stops
# here until the list is regenerated (the recipe is in that file).
assert_firmware_families() {
    local ours theirs
    ours="$(grep -c '^\s*select BR2_PACKAGE_LINUX_FIRMWARE_' buildroot/package/embra-firmware/Config.in || true)"
    theirs="$(grep -c '^config BR2_PACKAGE_LINUX_FIRMWARE_' "$BUILDROOT_DIR/package/linux-firmware/Config.in" || true)"
    [ "$ours" = "$theirs" ] && [ "$ours" -gt 0 ] \
        || reject_image "buildroot/package/embra-firmware/Config.in selects $ours linux-firmware families, Buildroot offers $theirs: regenerate the list (the recipe is in the file)"
    echo "firmware: every linux-firmware family selected ($ours of $theirs)"
}

# assert_firmware_present <target dir>: the blobs and their licences are on
# the rootfs, and the firmware names the built-in drivers declare
# (modules.builtin.modinfo, `<driver>.firmware=<file>`) resolve under
# /lib/firmware. The table is printed always, with the absent names; a
# driver names blobs for hardware newer than the linux-firmware snapshot
# (the kernel moves first), so a few absent names per driver are the
# normal state, and the fatal case is a driver of the mandatory set (the
# NICs that load their firmware at open and the probe-time drivers) with
# NONE of its names present: a family that is not installed at all, which
# is a driver that fails on its hardware with no message anyone sees.
assert_firmware_present() {
    local tgt="$1" fw="$1/lib/firmware" modinfo drivers drv names name have=0 lack=0 fatal=""
    [ -d "$fw" ] || reject_image "/lib/firmware is missing from the rootfs (BR2_PACKAGE_EMBRA_FIRMWARE_ALL)"
    [ -f "$tgt/usr/share/licenses/linux-firmware/WHENCE" ] \
        || reject_image "/usr/share/licenses/linux-firmware/WHENCE is missing from the rootfs: the firmware's licences travel beside the blobs (post_build.sh)"
    modinfo="$(ls "$tgt"/lib/modules/*/modules.builtin.modinfo 2>/dev/null | head -n1)"
    [ -n "$modinfo" ] || reject_image "lib/modules/<version>/modules.builtin.modinfo is missing from the rootfs"
    drivers="$(tr '\0' '\n' < "$modinfo" | sed -n 's/^\([a-z0-9_]*\)\.firmware=.*/\1/p' | sort -u | tr '\n' ' ')"
    for drv in $drivers; do
        names="$(tr '\0' '\n' < "$modinfo" | sed -n "s/^${drv}\.firmware=//p" | sort -u)"
        local drv_have=0 drv_lack=0
        local absent=""
        for name in $names; do
            if [ -e "$fw/$name" ] || [ -e "$fw/$name.xz" ] || [ -e "$fw/$name.zst" ]; then
                drv_have=$((drv_have + 1))
            else
                drv_lack=$((drv_lack + 1)); absent="$absent $name"
            fi
        done
        have=$((have + drv_have)); lack=$((lack + drv_lack))
        printf '  firmware %-16s %3d of %3d names present%s\n' "$drv" "$drv_have" $((drv_have + drv_lack)) "${absent:+ (absent:$absent)}"
        case "$drv" in
            bnx2|bnx2x|qed|i915|amdgpu|radeon|ice|cxgb4|r8169|r8152|tg3|e100)
                [ "$drv_have" -gt 0 ] || fatal="$fatal $drv"
                ;;
        esac
    done
    echo "firmware coverage: $have names present, $lack absent, over the drivers built in"
    [ -z "$fatal" ] || reject_image "a mandatory driver has none of its firmware under /lib/firmware (the family is not installed):${fatal}"
}

# firmware_u8 <file> <offset>, firmware_le32 <file> <offset>: one field of a
# firmware file as a number. Read byte by byte, so that the byte order of the
# host that builds the image does not enter into it.
firmware_u8() { od -An -tu1 -j "$2" -N1 "$1" | tr -d ' \n'; }
firmware_le32() {
    local bytes
    bytes="$(od -An -tu1 -j "$2" -N4 "$1")"
    # shellcheck disable=SC2086
    set -- $bytes
    [ $# -eq 4 ] || return 1
    echo $(($1 + ($2 << 8) + ($3 << 16) + ($4 << 24)))
}

# mt7921_power_table_walk <firmware file>: the kernel's mt7921_load_clc on a
# Wi-Fi firmware of its family, as far as it decides whether the firmware is
# taken (linux 6.18, drivers/net/wireless/mediatek/mt76/mt7921/mcu.c; the
# layouts are mt76_connac2_fw_trailer, mt76_connac2_fw_region and
# mt7921_clc). The file ends with a trailer of 36 bytes whose third byte
# counts the regions; before it stand that many region records of 40 bytes,
# each with its length at byte 20, its features at byte 24 and its type at
# byte 25. The region that is not downloaded (feature bit 6) and has type 2
# is the power table: a row of records that each begin with their own length
# in four bytes, a header of 16 bytes included. The driver refuses the whole
# firmware when a region runs past the file's data, when fewer than 16 bytes
# are left for a record, and when a record's length is under 16 or more than
# is left. One line says what was found; the status is 1 where the driver
# refuses. When the kernel pin moves, read that function again.
mt7921_power_table_walk() {
    local f="$1" size regions data offset=0 i at len feature kind base="" table=0 off=0 n=0 rec
    size="$(file_size "$f")"
    [ "${size:-0}" -ge 36 ] || { echo "shorter than a firmware's trailer"; return 1; }
    regions="$(firmware_u8 "$f" $((size - 34)))"
    data=$((size - 36 - regions * 40))
    [ "$data" -ge 0 ] || { echo "its $regions region records are longer than the file"; return 1; }
    for ((i = 0; i < regions; i++)); do
        at=$((size - 36 - (regions - i) * 40))
        len="$(firmware_le32 "$f" $((at + 20)))" || { echo "region $i could not be read"; return 1; }
        [ "$len" -le $((data - offset)) ] || { echo "region $i runs past the file's data"; return 1; }
        feature="$(firmware_u8 "$f" $((at + 24)))"
        kind="$(firmware_u8 "$f" $((at + 25)))"
        if [ $((feature & 64)) -ne 0 ] && [ "$kind" -eq 2 ]; then
            base="$offset"; table="$len"
            break
        fi
        offset=$((offset + len))
    done
    [ -n "$base" ] || { echo "no power table"; return 0; }
    while [ "$off" -lt "$table" ]; do
        [ $((table - off)) -ge 16 ] || { echo "after $n records $((table - off)) bytes are left, fewer than a record's header"; return 1; }
        rec="$(firmware_le32 "$f" $((base + off)))" || { echo "record $((n + 1)) could not be read"; return 1; }
        if [ "$rec" -lt 16 ] || [ "$rec" -gt $((table - off)) ]; then
            echo "record $((n + 1)) says it is $rec bytes long where $((table - off)) are left of the table"
            return 1
        fi
        n=$((n + 1)); off=$((off + rec))
    done
    echo "$n records fill the table's $table bytes"
}

# assert_mt7921_power_tables <target dir>: the kernel's mt7921 driver family
# (MT7920, MT7921, MT7922) reads a power table out of the Wi-Fi firmware it
# loads and refuses the firmware when a record runs past the table
# ("Invalid CLC record", then "hardware init failed"): the card is found,
# its driver is loaded, and no interface comes up. linux-firmware carried
# such a file for the MT7922 in two releases, 20251011 among them, the one
# Buildroot 2026.02 has, which is why that card's two files are a recipe of
# this tree. Here the card's two files are the ones the recipe pins,
# MediaTek's licence text on the rootfs is the one of the pinned release,
# and every firmware of the family the root filesystem holds is walked the
# way the driver walks it: the names the family's modules declare, and the
# pinned firmware whatever they declare. Modules of the family that
# declare no firmware are a rejection: the walk would go by nothing of
# theirs. Guard: the_build_walks_a_wifi_firmwares_power_table_as_the_driver_does
# (embra-boot), on stand-in files.
assert_mt7921_power_tables() {
    local tgt="$1" fw="$1/lib/firmware" ko modules=0 names="" name said walked="" pin have pinned=0 pinned_code=""
    # shellcheck source=buildroot/board/embraos/wireless.sh
    . "$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)/buildroot/board/embraos/wireless.sh"
    [ -f "$MT7922_FIRMWARE_PINS" ] || reject_image "the MT7922's firmware has no hash file ($MT7922_FIRMWARE_PINS)"
    while read -r pin name; do
        pinned=$((pinned + 1))
        case "$name" in WIFI_RAM_CODE_*) pinned_code="mediatek/$name" ;; esac
        have="$(sha256_of "$fw/mediatek/$name" 2>/dev/null || true)"
        [ "$have" = "$pin" ] \
            || reject_image "/lib/firmware/mediatek/$name is not the file buildroot/package/embra-mt7922-firmware pins (its sha256 is ${have:-none: the file is absent}): the recipe was not selected or sourced (buildroot/Config.in), or linux-firmware's own install ran after it"
    done < <(sed -n 's/^sha256  \([0-9a-f]\{64\}\)  \(WIFI_[A-Za-z0-9_]*\.bin\)$/\1 \2/p' "$MT7922_FIRMWARE_PINS")
    { [ "$pinned" -eq 2 ] && [ -n "$pinned_code" ]; } \
        || reject_image "$MT7922_FIRMWARE_PINS names $pinned firmware files; the card's two are the firmware (WIFI_RAM_CODE_…) and its ROM patch"
    pin="$(sed -n 's/^sha256  \([0-9a-f]\{64\}\)  LICENCE\.mediatek$/\1/p' "$MT7922_FIRMWARE_PINS")"
    have="$(sha256_of "$tgt/usr/share/licenses/linux-firmware/LICENCE.mediatek" 2>/dev/null || true)"
    { [ -n "$pin" ] && [ "$have" = "$pin" ]; } \
        || reject_image "MediaTek's licence text on the rootfs (/usr/share/licenses/linux-firmware/LICENCE.mediatek) is not the text of the release the MT7922's firmware is pinned to ($MT7922_FIRMWARE_PINS): the recipe has to install that release's text with the files"
    for ko in "$tgt"/lib/modules/*/kernel/drivers/net/wireless/mediatek/mt76/mt7921/*.ko; do
        [ -f "$ko" ] || continue
        modules=$((modules + 1))
        names="$names $(embraos_module_firmware "$ko" | grep '/WIFI_RAM_CODE_' | tr '\n' ' ' || true)"
    done
    if [ "$modules" -gt 0 ] && [ -z "${names// /}" ]; then
        reject_image "the mt7921 family's $modules modules on the rootfs declare no Wi-Fi firmware: the walk of their power tables has nothing to go by (embraos_module_firmware, board/embraos/wireless.sh)"
    fi
    for name in $(printf '%s\n' $names "$pinned_code" | LC_ALL=C sort -u); do
        [ -f "$fw/$name" ] || continue
        said="$(mt7921_power_table_walk "$fw/$name")" \
            || reject_image "/lib/firmware/$name carries a power table the kernel's mt7921 driver refuses ($said): a card that needs the file is found, its driver loaded, and no interface comes up. buildroot/package/embra-mt7922-firmware pins such a file from a later release of linux-firmware"
        walked="$walked ${name##*/}"
        echo "  firmware $name: $said"
    done
    echo "firmware: the driver takes the power table of each mt7921 family firmware on the rootfs ($(echo $walked | wc -w | tr -d ' ') walked); the MT7922's two files are the pinned ones, under the licence text the rootfs carries"
}

# assert_boot_initrds <target dir> <triple>: the two initrds post_build.sh
# wrote beside the repository's. ucode.cpio is a newc archive (070701) and,
# on x86_64, carries the two microcode files as the concatenations the
# kernel's early loader reads (GenuineIntel.bin from Intel's bundle at the
# version the recipe pins, AuthenticAMD.bin from linux-firmware's amd-ucode,
# and with the first the licence of that bundle on the rootfs); on another
# architecture it is the empty archive. firmware.cpio.gz is a gzip'd newc
# archive with every probe-time blob under lib/firmware.
# assert_cpio_parents_precede <cpio -tv listing> <archive name>: every
# directory on a member's path is a member of its own, listed before it. The
# kernel's cpio unpacker creates no parent directory and skips a file whose
# directory is not there yet without a word, so an archive of files alone
# unpacks to nothing and no boot message says so (the probe-time firmware
# archive did exactly that until 2026-11-09).
assert_cpio_parents_precede() {
    local missing
    missing="$(awk '{ name = ($1 ~ /^l/) ? $(NF-2) : $NF }
        /^d/ { dirs[name] = 1; next }
        { p = name; while (sub(/\/[^\/]*$/, "", p)) if (!(p in dirs)) print p }' "$1" | sort -u | head -n 3 | tr '\n' ' ')"
    [ -z "$missing" ] || reject_image "$2 has members under directories that are not members before them (${missing% }): the kernel's cpio unpacker creates no parent directory and skips such a file without a word"
}

assert_boot_initrds() {
    local tgt arch="${2%%-*}" magic tmp listing ucode_version ucode_bundle
    # Absolute: the archives are read from inside a scratch directory below.
    tgt="$(cd "$1" && pwd)"
    magic="$(head -c 6 "$tgt/boot/ucode.cpio")"
    [ "$magic" = "070701" ] || reject_image "/boot/ucode.cpio is not a newc cpio archive (starts with '$magic'; the early microcode loader reads newc, uncompressed)"
    tmp="$(mktemp -d)"
    (cd "$tmp" && cpio -id --quiet < "$tgt/boot/ucode.cpio" 2>/dev/null) || reject_image "/boot/ucode.cpio does not unpack"
    listing="$(mktemp)"
    cpio -tv --quiet < "$tgt/boot/ucode.cpio" > "$listing" 2>/dev/null
    assert_cpio_parents_precede "$listing" "/boot/ucode.cpio"
    if [ "$arch" = "x86_64" ]; then
        [ -s "$tmp/kernel/x86/microcode/GenuineIntel.bin" ] || reject_image "/boot/ucode.cpio lacks kernel/x86/microcode/GenuineIntel.bin (BR2_PACKAGE_EMBRA_INTEL_MICROCODE puts intel-ucode/ in the images directory)"
        [ -s "$tmp/kernel/x86/microcode/AuthenticAMD.bin" ] || reject_image "/boot/ucode.cpio lacks kernel/x86/microcode/AuthenticAMD.bin (linux-firmware's amd-ucode family)"
        # Intel's files and licence are the pinned bundle's and nothing else:
        # the comparison is with the build directory of the version the recipe
        # names, so a bundle Buildroot never built, an images directory another
        # bundle left behind and a licence from another one are each refused.
        ucode_version="$(intel_microcode_version)" \
            || reject_image "no single 'EMBRA_INTEL_MICROCODE_VERSION = <version>' line in $INTEL_MICROCODE_RECIPE: the build cannot tell which Intel microcode bundle the image is to carry"
        ucode_bundle="$BUILDROOT_DIR/output/build/embra-intel-microcode-$ucode_version"
        [ -f "$ucode_bundle/.stamp_images_installed" ] \
            || reject_image "Intel's microcode bundle $ucode_version was not built ($ucode_bundle/.stamp_images_installed is missing): check that the x86_64 defconfig selects BR2_PACKAGE_EMBRA_INTEL_MICROCODE and that buildroot/Config.in sources its Config.in"
        cat "$ucode_bundle"/intel-ucode/* | cmp -s - "$tmp/kernel/x86/microcode/GenuineIntel.bin" \
            || reject_image "/boot/ucode.cpio's GenuineIntel.bin is not the concatenation of the intel-ucode files of Intel's bundle $ucode_version ($ucode_bundle): the images directory holds another bundle's files"
        cat "$tgt"/lib/firmware/amd-ucode/microcode_amd*.bin | cmp -s - "$tmp/kernel/x86/microcode/AuthenticAMD.bin" \
            || reject_image "/boot/ucode.cpio's AuthenticAMD.bin is not the concatenation of /lib/firmware/amd-ucode/microcode_amd*.bin"
        [ -s "$tgt/usr/share/licenses/intel-microcode/license" ] \
            || reject_image "/usr/share/licenses/intel-microcode/license is missing from the rootfs: Intel's microcode is redistributed on the condition that its notice travels with it (the embra-intel-microcode recipe installs it)"
        cmp -s "$ucode_bundle/license" "$tgt/usr/share/licenses/intel-microcode/license" \
            || reject_image "/usr/share/licenses/intel-microcode/license is not the licence of Intel's bundle $ucode_version ($ucode_bundle/license)"
        echo "microcode: /boot/ucode.cpio carries GenuineIntel.bin ($(stat -c %s "$tmp/kernel/x86/microcode/GenuineIntel.bin") bytes, Intel's bundle $ucode_version) and AuthenticAMD.bin ($(stat -c %s "$tmp/kernel/x86/microcode/AuthenticAMD.bin") bytes); Intel's licence under /usr/share/licenses/intel-microcode"
    else
        [ -z "$(ls -A "$tmp")" ] || reject_image "/boot/ucode.cpio is not the empty archive on $arch"
        echo "microcode: /boot/ucode.cpio is the empty archive on $arch"
    fi
    rm -rf "$tmp"
    gzip -t "$tgt/boot/firmware.cpio.gz" || reject_image "/boot/firmware.cpio.gz is not a gzip file"
    local members
    gzip -dc "$tgt/boot/firmware.cpio.gz" | cpio -tv --quiet > "$listing" 2>/dev/null
    members="$(grep -c '^-.* lib/firmware/' "$listing" || true)"
    [ "$members" -gt 0 ] || reject_image "/boot/firmware.cpio.gz carries no lib/firmware file (post_build.sh builds it from the probe-time drivers' names)"
    assert_cpio_parents_precede "$listing" "/boot/firmware.cpio.gz"
    rm -f "$listing"
    assert_probe_firmware_archived "$tgt"
    echo "probe-time firmware: /boot/firmware.cpio.gz carries $members files with their directories ($(stat -c %s "$tgt/boot/firmware.cpio.gz") bytes)"
}

# assert_probe_firmware_archived <target dir>: every blob a probe-time driver
# names and the rootfs holds is a member of /boot/firmware.cpio.gz. The
# drivers are post_build.sh's PROBE_DRIVERS, read from its line so the two
# cannot drift; their names are the kernel's own list
# (modules.builtin.modinfo, `<driver>.firmware=<file>`). A graphics driver
# takes the display over from the firmware's framebuffer before it asks for
# its blobs, so one that is missing from the archive is a screen that stays
# dark on that hardware, from a build that looked whole.
assert_probe_firmware_archived() {
    local tgt="$1" post="buildroot/board/embraos/post_build.sh" drivers modinfo members drv name cand want=0 lack=0 missing=""
    drivers="$(sed -n 's/^PROBE_DRIVERS="\(.*\)"$/\1/p' "$post")"
    [ -n "$drivers" ] && [ "$(grep -c '^PROBE_DRIVERS=' "$post")" = 1 ] \
        || reject_image "no single PROBE_DRIVERS=\"...\" line in $post: the build cannot tell which drivers' firmware the boot archive is to carry"
    modinfo="$(ls "$tgt"/lib/modules/*/modules.builtin.modinfo 2>/dev/null | head -n1)"
    [ -n "$modinfo" ] || reject_image "lib/modules/<version>/modules.builtin.modinfo is missing from the rootfs"
    members="$(mktemp)"
    gzip -dc "$tgt/boot/firmware.cpio.gz" | cpio -t --quiet 2>/dev/null > "$members"
    for drv in $drivers; do
        while IFS= read -r name; do
            [ -n "$name" ] || continue
            for cand in "$name" "$name.xz" "$name.zst"; do
                if [ -f "$tgt/lib/firmware/$cand" ]; then
                    want=$((want + 1))
                    if ! grep -q -x -F "lib/firmware/$cand" "$members"; then
                        lack=$((lack + 1))
                        [ "$lack" -gt 5 ] || missing="$missing $cand"
                    fi
                    break
                fi
            done
        done <<EOF
$(tr '\0' '\n' < "$modinfo" | sed -n "s/^${drv}\.firmware=//p" | sort -u)
EOF
    done
    rm -f "$members"
    [ "$lack" = 0 ] || reject_image "/boot/firmware.cpio.gz lacks $lack of the firmware files that the probe-time drivers ($drivers) name and the rootfs holds (the first:$missing): a driver that probes before the rootfs is mounted would not find them (post_build.sh builds the archive)"
    echo "probe-time firmware: the $want files that ${drivers// /, } name and the rootfs holds are all in /boot/firmware.cpio.gz"
}

# license_version_line <file>: the text's version line, its third ("Version
# 2.0, November 26, 2026"): what the installer and the OS name the text by,
# beside its sha256. Empty when the third line is not one.
license_version_line() { sed -n '3s/^\(Version [0-9][^[:cntrl:]]*\)$/\1/p' "$1"; }

# assert_license_text <target dir>: the root filesystem carries the
# repository's LICENSE byte for byte, where the installer and the OS read it,
# and the text has the shape both read: its title on the first line and its
# version on the third. An image whose copy is missing or is another text is
# rejected: an acceptance names the text it was shown.
assert_license_text() {
    local tgt="$1" lic="usr/share/licenses/embraos/LICENSE" version
    [ -s LICENSE ] || reject_image "the repository's LICENSE is missing: the licence text travels with every image"
    [ "$(sed -n '1p' LICENSE)" = "embraOS PROPRIETARY LICENSE" ] \
        || reject_image "the first line of LICENSE is not \"embraOS PROPRIETARY LICENSE\": the installer and the OS show this file as the licence"
    version="$(license_version_line LICENSE)"
    [ -n "$version" ] || reject_image "the third line of LICENSE is not its version line (\"Version <number>, <date>\"): the installer and the OS name the text by it"
    [ -s "$tgt/$lic" ] || reject_image "/$lic is missing from the rootfs: the licence text travels with every image (post_build.sh installs it)"
    cmp -s LICENSE "$tgt/$lic" || reject_image "/$lic in the rootfs is not the repository's LICENSE: the installer and the OS would show another text"
    echo "licence: /$lic is the repository's LICENSE ($version; sha256 $(sha256_of LICENSE))"
}

# assert_display_console_launcher <target dir>: the display console's launcher
# is cog with this tree's patches (buildroot/patches/cog). embrad starts it
# with a failure action that one of them adds (--webprocess-failure=restart-home,
# crates/embrad/src/supervisor.rs): a cog built without it stops at that
# argument, the display console never comes up on any machine, and the build
# would say nothing. Each patch that leaves words in a program is looked for
# by them: the action in the launcher, the reason of a renderer's end in its
# core library, the mode set again in the DRM platform. An image without the
# launcher has nothing to check.
assert_display_console_launcher() {
    local tgt="$1" cog core drm
    cog="$tgt/usr/bin/cog"
    [ -e "$cog" ] || return 0
    core="$(ls "$tgt"/usr/lib/libcogcore.so.*.*.* 2>/dev/null | head -1)"
    drm="$tgt/usr/lib/cog/modules/libcogplatform-drm.so"
    grep -a -q -F -- 'restart-home' "$cog" \
        || reject_image "the display console's launcher (/usr/bin/cog) does not know the failure action embrad starts it with (restart-home, buildroot/patches/cog/0007): the display console would stop at its start on every machine"
    { [ -n "$core" ] && grep -a -q -F -- 'exceeded the memory limit' "$core"; } \
        || reject_image "the display console's launcher does not say why a renderer stopped (libcogcore, buildroot/patches/cog/0008): cog was built without this tree's patches"
    { [ -f "$drm" ] && grep -a -q -F -- 'the mode was set again' "$drm"; } \
        || reject_image "the display console's DRM platform does not set the mode again after a refused commit (libcogplatform-drm.so, buildroot/patches/cog/0009): cog was built without this tree's patches"
    echo "display console: the launcher is built with this tree's patches (the failure action restart-home, the reason of a renderer's end, the mode set again)"
}

assert_rootfs_invariants() {
    local triple="${1:-}"
    assert_target_is_clean
    [ -n "$triple" ] && assert_kernel_fragments_applied "$triple"
    assert_license_text "$BUILDROOT_DIR/output/target"
    assert_display_console_launcher "$BUILDROOT_DIR/output/target"
    assert_firmware_families
    assert_firmware_present "$BUILDROOT_DIR/output/target"
    assert_mt7921_power_tables "$BUILDROOT_DIR/output/target"
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
    # The kernel's banner names the image, never who built it: read from the
    # header the kernel's build wrote for it.
    [ "$(kernel_banner_builder "$BUILDROOT_DIR/output/build/linux-$LINUX_KERNEL_VERSION")" = "$KERNEL_BUILT_BY@$KERNEL_BUILT_BY" ] \
        || reject_image "the kernel's banner does not name $KERNEL_BUILT_BY@$KERNEL_BUILT_BY: its build was not handed KBUILD_BUILD_USER and KBUILD_BUILD_HOST (buildroot/external.mk)"
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
    # /boot holds exactly the slot's files: the kernel, the three initrds
    # GRUB loads (the microcode first and uncompressed, the probe-time
    # firmware, the repository's initramfs), the slot's menu line. Anything
    # else there is a leftover the squashfs would carry.
    local boot_files expected_boot
    boot_files="$(cd "$tgt/boot" && ls -A | sort | tr '\n' ' ')"
    expected_boot="$(printf '%s\n' "$kernel" embraos-slot.cfg initramfs.cpio.gz firmware.cpio.gz ucode.cpio | sort | tr '\n' ' ')"
    [ "$boot_files" = "$expected_boot" ] \
        || reject_image "/boot holds '${boot_files% }' in the rootfs, expected exactly: ${expected_boot% }"
    assert_boot_initrds "$tgt" "$triple"
    echo "slot contents present: /boot/$kernel, the three initrds, /etc/embraos-release ($stamped), /boot/embraos-slot.cfg, nothing else under /boot"

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
    assert_native_model_runtime "$tgt" "$triple"
    assert_imported_cas
    assert_rootfs_names_no_builder_dir

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

# assert_native_model_runtime <target directory> <triple>: the model server
# of native models (experimental) is in the root filesystem as its recipe
# installs it (buildroot/package/embra-llama-cpp), from the build of the
# version the recipe names. A package symbol Buildroot dropped is silent, and
# so is a library the server would look for at start and not find: embrad
# would report a model server that ends at once, on every machine. Checked:
# the server; the CPU backends, the ones the build made and no other, with
# the architecture's baseline among them; every library the server and its
# libraries name is in the root filesystem, none of them a TLS or a transfer
# library, and no run path is written into any; on x86_64, the server, its
# libraries and the two backends below AVX use no AVX register (a processor
# without AVX stops on the first such instruction); the library that loads a
# backend still writes the line embrad reads the backend's name from; the
# licence texts are the build's own.
assert_native_model_runtime() {
    local tgt="$1" triple="${2:-}" version src bin readelf objdump f name dyn n want have baseline lic count
    local -a files plain
    version="$(llama_cpp_version)" \
        || reject_image "the model server's recipe has no line of the form 'EMBRA_LLAMA_CPP_VERSION = <version>' ($LLAMA_CPP_RECIPE)"
    src="$BUILDROOT_DIR/output/build/embra-llama-cpp-$version"
    bin="$src/buildroot-build/bin"
    [ -x "$tgt/usr/bin/llama-server" ] \
        || reject_image "/usr/bin/llama-server is missing from the rootfs (BR2_PACKAGE_EMBRA_LLAMA_CPP in the defconfig; buildroot/Config.in sources the package)"
    [ -d "$bin" ] \
        || reject_image "the model server in the rootfs is not the build of the version its recipe names ($version): $bin is not there"
    readelf="$(ls "$BUILDROOT_DIR"/output/host/bin/*-buildroot-linux-musl-readelf 2>/dev/null | head -n 1)"
    objdump="$(ls "$BUILDROOT_DIR"/output/host/bin/*-buildroot-linux-musl-objdump 2>/dev/null | head -n 1)"
    { [ -n "$readelf" ] && [ -x "$readelf" ] && [ -n "$objdump" ] && [ -x "$objdump" ]; } \
        || reject_image "Buildroot's readelf and objdump are not under $BUILDROOT_DIR/output/host/bin: the model server's files cannot be read back"

    want="$(cd "$bin" && ls libggml-cpu-*.so 2>/dev/null | LC_ALL=C sort | tr '\n' ' ')"
    have="$(cd "$tgt/usr/lib/llama-cpp" 2>/dev/null && ls | LC_ALL=C sort | tr '\n' ' ')"
    { [ -n "$want" ] && [ "$have" = "$want" ]; } \
        || reject_image "/usr/lib/llama-cpp in the rootfs holds '${have% }'; the model server's build made '${want% }'"
    case "$triple" in
        x86_64-*) baseline="libggml-cpu-x64.so" ;;
        aarch64-*) baseline="libggml-cpu-armv8.0_1.so" ;;
        *) baseline="" ;;
    esac
    if [ -n "$baseline" ] && [ ! -f "$tgt/usr/lib/llama-cpp/$baseline" ]; then
        reject_image "/usr/lib/llama-cpp in the rootfs lacks $baseline, the CPU backend for a processor with no instruction set beyond the architecture's own"
    fi

    files=("$tgt/usr/bin/llama-server" "$tgt/usr/lib/libllama-server-impl.so")
    for f in "$tgt"/usr/lib/libggml-base.so.* "$tgt"/usr/lib/libggml.so.* "$tgt"/usr/lib/libllama.so.* \
             "$tgt"/usr/lib/libllama-common.so.* "$tgt"/usr/lib/libmtmd.so.*; do
        # the file, not the link its soname names
        if [ -f "$f" ] && [ ! -L "$f" ]; then files+=("$f"); fi
    done
    [ ${#files[@]} -eq 7 ] \
        || reject_image "the rootfs holds ${#files[@]} of the model server's 7 files outside its backends (the server and six libraries under /usr/lib)"
    plain=("${files[@]}")
    for f in "$tgt"/usr/lib/llama-cpp/*.so; do
        files+=("$f")
    done
    for f in "${files[@]}"; do
        dyn="$("$readelf" -d "$f" 2>/dev/null)" || reject_image "/${f#"$tgt"/} in the rootfs could not be read as a program or a library"
        if printf '%s\n' "$dyn" | grep -q -E '\((RPATH|RUNPATH)\)'; then
            reject_image "/${f#"$tgt"/} in the rootfs carries a run path: $(printf '%s\n' "$dyn" | grep -E '\((RPATH|RUNPATH)\)' | head -n 1 | sed 's/.*\[\(.*\)\].*/\1/') (CMAKE_SKIP_RPATH in the recipe)"
        fi
        while IFS= read -r name; do
            [ -n "$name" ] || continue
            case "$name" in
                libssl*|libcrypto*|libcurl*|libgnutls*)
                    reject_image "/${f#"$tgt"/} in the rootfs loads $name: the model server is built without TLS and without a download client" ;;
            esac
            { [ -e "$tgt/usr/lib/$name" ] || [ -e "$tgt/lib/$name" ]; } \
                || reject_image "/${f#"$tgt"/} in the rootfs loads $name, which is not in the rootfs: the model server would end at its start"
        done <<NEEDED
$(printf '%s\n' "$dyn" | sed -n 's/.*(NEEDED).*\[\(.*\)\].*/\1/p')
NEEDED
    done

    # embrad learns which CPU backend the loader takes on a processor from the
    # loader's own line, which the server's program writes when it is asked
    # (BACKEND_QUESTION and backend_of_answer, crates/embra-boot/src/native_model.rs).
    # A llama.cpp whose loader words the line otherwise would leave /native and
    # the status record without the backend's name, and nothing would say so.
    n=0
    for f in "$tgt"/usr/lib/libggml.so.*; do
        if [ -f "$f" ] && [ ! -L "$f" ] && grep -a -q 'loaded %s backend from %s' "$f"; then n=$((n + 1)); fi
    done
    [ "$n" = 1 ] \
        || reject_image "libggml.so in the rootfs does not carry the loader's line 'loaded %s backend from %s': embrad reads the CPU backend's name from it (crates/embra-boot/src/native_model.rs, backend_of_answer)"

    case "$triple" in
        x86_64-*)
            plain+=("$tgt/usr/lib/llama-cpp/libggml-cpu-x64.so")
            [ ! -f "$tgt/usr/lib/llama-cpp/libggml-cpu-sse42.so" ] || plain+=("$tgt/usr/lib/llama-cpp/libggml-cpu-sse42.so")
            for f in "${plain[@]}"; do
                n="$("$objdump" -d --no-show-raw-insn "$f" 2>/dev/null | grep -c -E '%[yz]mm[0-9]' || true)"
                [ "$n" = 0 ] \
                    || reject_image "/${f#"$tgt"/} in the rootfs uses AVX registers ($n instructions): a processor without AVX stops on the first of them"
            done
            ;;
    esac

    count=0
    for lic in "LICENSE:LICENSE" "vendor/cpp-httplib/LICENSE:LICENSE-cpp-httplib" "licenses/LICENSE-jsonhpp:LICENSE-nlohmann-json" \
               "vendor/hash/xxhash/LICENSE:LICENSE-xxhash" "vendor/hash/rotate-bits/LICENSE.md:LICENSE-rotate-bits"; do
        cmp -s "$src/${lic%%:*}" "$tgt/usr/share/licenses/llama.cpp/${lic##*:}" \
            || reject_image "/usr/share/licenses/llama.cpp/${lic##*:} in the rootfs is missing or is not ${lic%%:*} of llama.cpp $version: the licence texts travel with the binaries"
        count=$((count + 1))
    done
    have="$(ls "$tgt/usr/share/licenses/llama.cpp" | wc -l)"
    [ "$have" -eq "$count" ] \
        || reject_image "/usr/share/licenses/llama.cpp in the rootfs holds $have files; the recipe installs $count"
    echo "native models (experimental): llama-server of llama.cpp $version, $(printf '%s\n' $want | wc -l) CPU backends${baseline:+ with $baseline}, every library it loads in the rootfs, no TLS library, no run path, the loader's line that names a backend, $count licence texts"
}

# clean_packages_that_name_the_builder <buildroot dir>: a package whose file
# in the target directory of the last build names a directory of this host
# (packages_naming_builder_dirs, scripts/lib/build-paths.sh) was built before
# the compiler was handed a stand-in for Buildroot's directory, and Buildroot
# rebuilds nothing whose options changed: it is cleaned and builds again.
# Called by both image scripts before they remove the target directory, which
# is what it reads. WebKit is one of the three, and its build is the longest
# of the image: this happens once on a tree, and never in a container, whose
# directories name nobody.
clean_packages_that_name_the_builder() {
    local br="$1" root pkgs pkg
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    pkgs="$(packages_naming_builder_dirs "$root" "$br/output/target")" || {
        echo "ERROR: the target directory of the last build could not be read through ($br/output/target)" >&2
        exit 1
    }
    for pkg in $pkgs; do
        echo "build paths: $pkg was built with this host's directory in it; the package builds again from its tarball"
        clean_buildroot_package "$br" "$pkg"
    done
}

# relink_kernel_that_names_its_builder <buildroot dir>: a kernel built before
# its build was handed a name for the banner (buildroot/external.mk) carries
# the login and the host name of whoever built it, and Buildroot builds a
# built kernel no second time. Its build step is made to run again: the
# kernel's own build rewrites the banner's header and links, and compiles
# nothing twice. Called by both image scripts before make.
relink_kernel_that_names_its_builder() {
    local br="$1" tree who
    tree="$br/output/build/linux-$LINUX_KERNEL_VERSION"
    who="$(kernel_banner_builder "$tree")"
    [ -n "$who" ] || return 0
    [ "$who" != "$KERNEL_BUILT_BY@$KERNEL_BUILT_BY" ] || return 0
    echo "kernel: its banner names who built it; its build step runs again (the banner and a link, nothing is compiled twice)"
    ( cd "$br" && make linux-clean-for-rebuild ) >/dev/null 2>&1 || true
    if [ -e "$tree/.stamp_built" ]; then
        echo "ERROR: the kernel's build stamp is still there ($tree/.stamp_built): run 'make linux-clean-for-rebuild' in $br, then build again" >&2
        exit 1
    fi
}

# assert_rootfs_names_no_builder_dir: no file of the root filesystem names a
# directory of this build host (builder_needles, scripts/lib/build-paths.sh).
# Refused, each with the file and a text of it: a program of this tree built
# before rustc was handed the stand-ins (a --buildroot-only run over an older
# build), a record of the build that post_build.sh did not remove, a package
# Buildroot compiled before it was handed its own stand-in, and a package
# that newly keeps its build directory.
assert_rootfs_names_no_builder_dir() {
    local root tgt="$BUILDROOT_DIR/output/target" found f rel pkg
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    if [ -z "$(builder_needles "$root")" ]; then
        echo "build paths: nothing to look for in the root filesystem (every directory of this build is directly under /)"
        return 0
    fi
    found="$(files_naming_builder_dirs "$root" "$tgt")" \
        || reject_image "the root filesystem could not be read through for the directories of this build host"
    if [ -n "$found" ]; then
        echo "ERROR: a directory of this build host is named in the root filesystem:" >&2
        while IFS= read -r f; do
            [ -n "$f" ] || continue
            rel="${f#"$tgt"/}"
            printf '       /%s\n         %s\n' "$rel" "$(builder_strings "$root" "$f" | head -n 1)" >&2
            pkg="$(package_that_compiled "$rel" || true)"
            [ -z "$pkg" ] || echo "         ($pkg was built before the compiler was handed a stand-in: build again, and it is cleaned first)" >&2
        done <<LIST
$found
LIST
        echo "       A program of this tree: it was built before rustc was handed the stand-ins; run a full" >&2
        echo "       build (--storage-engine), not --buildroot-only. A record of the build that nothing reads" >&2
        echo "       at run time: remove_build_records (scripts/lib/build-paths.sh) removes it." >&2
        reject_image
    fi
    echo "build paths: none of this host's directories in the root filesystem"
}

# assert_imported_cas: the root filesystem trusts the certificate authorities
# of Imported_CAs/ and no other. /usr/share/embra/ca-certificates holds the
# folder's *.pem and *.crt files byte for byte and nothing else, and no such
# directory when the folder holds none: a file dropped in or taken out after
# Buildroot ran would leave an image that trusts something other than what
# the build's log said. The folder is checked once more with Buildroot's own
# openssl, so every certificate of an image has been parsed, whatever the
# host had at hand when the build began.
assert_imported_cas() {
    local root tgt="$BUILDROOT_DIR/output/target" openssl="$BUILDROOT_DIR/output/host/bin/openssl"
    local dest want have base
    root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
    dest="$tgt/$IMPORTED_CAS_ROOTFS_DIR"
    [ -x "$openssl" ] || openssl="$(command -v openssl || true)"
    imported_cas_check "$root" "$openssl" > /dev/null \
        || reject_image "$IMPORTED_CAS_FOLDER/ does not pass its check (the lines above)"
    want="$(imported_cas_list "$root" | tr '\n' ' ')"
    # `|| true`: without the directory `ls` fails, and under the callers'
    # pipefail that would end the build here without a word.
    have="$( (ls "$dest" 2>/dev/null || true) | LC_ALL=C sort | tr '\n' ' ')"
    [ "$want" = "$have" ] \
        || reject_image "/$IMPORTED_CAS_ROOTFS_DIR in the rootfs holds '${have% }' and $IMPORTED_CAS_FOLDER/ holds '${want% }': the folder changed after post_build.sh baked it"
    for base in $want; do
        cmp -s "$root/$IMPORTED_CAS_FOLDER/$base" "$dest/$base" \
            || reject_image "/$IMPORTED_CAS_ROOTFS_DIR/$base in the rootfs is not $IMPORTED_CAS_FOLDER/$base: the file changed after post_build.sh baked it"
    done
    if [ -z "$want" ]; then
        echo "imported CAs in the rootfs: none"
        return 0
    fi
    [ -n "$openssl" ] \
        || reject_image "no openssl to parse the certificates of $IMPORTED_CAS_FOLDER/ with (Buildroot's host openssl is missing, and the host has none)"
    echo "imported CAs in the rootfs: ${want% }"
    echo "  every machine installed from this image trusts them; scripts/release-update.sh signs no release from it"
}

# assert_image_layout <image>: the partitions carry the names of layout.sh in
# order, the first starts at sector 2048 and every start is a multiple of
# 2048 — checked on the final image, not trusted from genimage's `align` —
# and the image is the whole number of MiB layout.sh computes with the backup
# GPT on its last sector, not trusted from genimage's `size`.
# assert_slot_boot_menu <the ESP's grub.cfg, read out of the image>: the slot
# carries the boot menu (/usr/share/embra/grub.cfg in the target directory),
# and it is byte for byte the menu on the image's ESP and the one the
# installer medium takes with the ESP's tree (output/images/efi-part). An OS
# update puts the slot's menu on the ESP once the slot is the confirmed
# default (embra-update boot-menu), so the three are one file, or a machine
# that was updated boots another menu than one that was installed.
assert_slot_boot_menu() {
    local esp_menu="$1"
    local slot_menu="$BUILDROOT_DIR/output/target/usr/share/embra/grub.cfg"
    local staged="$BUILDROOT_DIR/output/images/efi-part/EFI/BOOT/grub.cfg"
    [ -s "$slot_menu" ] || {
        echo "ERROR: the root filesystem carries no boot menu at /usr/share/embra/grub.cfg (post_build.sh renders it into the slot)" >&2
        reject_image
    }
    [ -s "$esp_menu" ] || {
        echo "ERROR: the boot menu could not be read from the image's ESP" >&2
        reject_image
    }
    cmp -s "$slot_menu" "$esp_menu" || {
        echo "ERROR: the boot menu on the image's ESP is not the slot's /usr/share/embra/grub.cfg" >&2
        reject_image
    }
    cmp -s "$slot_menu" "$staged" || {
        echo "ERROR: the boot menu the installer medium takes ($staged) is not the slot's /usr/share/embra/grub.cfg" >&2
        reject_image
    }
    echo "boot menu: the slot's /usr/share/embra/grub.cfg is the ESP's and the installer medium's ($(wc -c < "$slot_menu" | tr -d ' ') bytes)"
}

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
    # The menu on the ESP, read out whole, is the slot's own copy.
    local esp_menu="$BUILDROOT_DIR/output/build/.embraos-esp-grub.cfg"
    "$mtype" -i "${img}@@$((esp_start * 512))" ::/EFI/BOOT/grub.cfg > "$esp_menu" 2>/dev/null || : > "$esp_menu"
    assert_slot_boot_menu "$esp_menu"
    echo "image layout: $EMBRAOS_PARTITION_NAMES, 1 MiB aligned, $EMBRAOS_IMAGE_SIZE_MIB MiB with the backup GPT on the last sector; the ESP carries the loader, grub.cfg and grubenv (slot A active)"
}

# write_vm_images <raw image> <out dir> <arch> <engine>: the same disk for the
# other hypervisors, converted from the raw GPT image after it passed the
# checks above, one folder per hypervisor under <out dir>; the raw image and
# the installer medium stay in <out dir> itself.
#   vmware/     embraos.ova (the OVF descriptor rendered from embraos.ovf.in
#               beside this file, the disk as a streamOptimized VMDK and the
#               SHA256 manifest, in that order: ESXi, vCenter, Fusion) and
#               embraos.vmdk (monolithicSparse: a disk for Fusion, Workstation
#               and VirtualBox).
#   hyper-v/    embraos.vhdx (dynamic: generation 2 with Secure Boot off).
#   parallels/  embraos.vmx beside a copy of embraos.vmdk. Parallels Desktop
#               runs no machine from a bare VMDK and opens no OVA; a VMware
#               machine it converts whole, so it is given the machine around
#               the disk, rendered from embraos.vmx.in beside this file (its
#               header has the reason for each setting, the rendered file the
#               two commands). The x86_64 file is the machine Parallels
#               converted on an Intel Mac; the aarch64 one names VMware's Arm
#               guest type and has not been converted (the template says what
#               stands against it on Apple silicon).
# The OVF descriptor and the VMX are rendered for the architecture: a template
# line that starts with @x86_64@ or @aarch64@ goes into that architecture's file
# alone, one that starts with @@ is the template's own comment. In the OVF x86_64
# declares VMware's paravirtual SCSI controller; the kernel builds that driver
# for x86 alone, so aarch64 declares an NVMe controller in its place, and a
# USB 3 controller for the keyboard and the pointer. The zero regions, the
# empty slot B and the unused DATA, are not stored, so each file is about the
# size of the data written. Needs qemu-img (Ubuntu: qemu-utils; macOS:
# Homebrew's qemu). Each converted disk must report the raw image's virtual
# size, a VMDK's capacity must be whole 64 KiB grains (VMware's sparse-extent
# format asks for it; a whole-MiB image gives it) and its embedded descriptor
# must name the file it is in; the VMX must name the VMDK that stands beside
# it and one guest type; the OVA's member order and manifest are checked after
# packing. A failure removes the staged file and stops the build. What an
# earlier build left goes first: the three files beside the raw image, from
# before the folders, and the Parallels pair, which is written again.
write_vm_images() {
    local raw="$1" out="$2" arch="$3" engine="$4"
    local name="embraos" template vmx_template version build_id raw_size other_arch
    # One machine for the OVF and the VMX: the vCPUs, the memory and VMware's
    # virtual hardware version.
    local cpus=4 memory_mb=4096 hw_version=20
    local vmware="$out/vmware" hyperv="$out/hyper-v" parallels="$out/parallels"
    command -v qemu-img >/dev/null 2>&1 || {
        echo "ERROR: qemu-img not found; the VM images need it (Ubuntu: apt-get install qemu-utils; macOS: brew install qemu)" >&2
        exit 1
    }
    # The architecture whose tagged template lines stay out of this descriptor.
    case "$arch" in
        x86_64)  other_arch="aarch64" ;;
        aarch64) other_arch="x86_64" ;;
        *) echo "ERROR: write_vm_images has no OVF hardware for the architecture '$arch' (x86_64 or aarch64)" >&2; exit 1 ;;
    esac
    template="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/embraos.ovf.in"
    [ -f "$template" ] || { echo "ERROR: $template is missing" >&2; exit 1; }
    vmx_template="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/embraos.vmx.in"
    [ -f "$vmx_template" ] || { echo "ERROR: $vmx_template is missing" >&2; exit 1; }
    version="$(sed -n 's/^version = "\(.*\)"/\1/p' Cargo.toml | head -n1)"
    build_id="$(git rev-parse --short=10 HEAD 2>/dev/null || echo unknown)"
    raw_size="$(file_size "$raw")"
    mkdir -p "$vmware" "$hyperv"
    # What an earlier build left: the three files beside the raw image, from
    # before the folders, and the Parallels pair, which is written again
    # further down, so a build that stops before that leaves no pair of
    # another build, or of the other architecture, behind.
    local earlier
    for earlier in "$out/${name}.vmdk" "$out/${name}.vhdx" "$out/${name}.ova"; do
        if [ -f "$earlier" ]; then
            rm -f "$earlier"
            echo "removed $earlier: an earlier build's, from before the folders"
        fi
    done
    rm -f "$parallels/${name}.vmx" "$parallels/${name}.vmdk"

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

    convert_checked vmdk "" "$vmware/${name}.vmdk"
    convert_checked vhdx "subformat=dynamic" "$hyperv/${name}.vhdx"

    # The machine Parallels imports: embraos.vmx beside a copy of the VMDK,
    # staged as a pair and moved across with the disk first, so the VMX never
    # stands without the disk it names.
    local pair_stage="$out/.parallels-stage" vmx="${name}.vmx" vmx_disk
    rm -rf "$pair_stage"
    mkdir -p "$pair_stage" "$parallels"
    cp "$vmware/${name}.vmdk" "$pair_stage/${name}.vmdk"
    # The template's own comments and the other architecture's lines go first,
    # then this architecture's tag, then the values.
    sed -e '/^@@/d' -e "/^@${other_arch}@/d" -e "s|^@${arch}@||" \
        -e "s|@DISK_FILE@|${name}.vmdk|g" -e "s|@NAME@|$name|g" \
        -e "s|@VERSION@|$version|g" -e "s|@BUILD_ID@|$build_id|g" \
        -e "s|@ARCH@|$arch|g" -e "s|@ENGINE@|$engine|g" \
        -e "s|@CPUS@|$cpus|g" -e "s|@MEMORY_MB@|$memory_mb|g" -e "s|@HW_VERSION@|$hw_version|g" \
        "$vmx_template" > "$pair_stage/$vmx"
    grep -q '@[A-Za-z0-9_]*@' "$pair_stage/$vmx" && {
        echo "ERROR: a VMX placeholder, architecture tag or template comment was not rendered" >&2
        rm -rf "$pair_stage"
        exit 1
    }
    # One guest type, this architecture's: a file with none, or with both,
    # is a template whose tags went wrong.
    [ "$(grep -c '^guestOS = ' "$pair_stage/$vmx")" = 1 ] || {
        echo "ERROR: $vmx does not name exactly one guest type for $arch" >&2
        rm -rf "$pair_stage"
        exit 1
    }
    # The same mistake as a VMDK that names another extent: a machine whose
    # disk line names a file that is not beside it converts to nothing.
    vmx_disk="$(sed -n 's/^sata0:0\.fileName = "\(.*\)"$/\1/p' "$pair_stage/$vmx")"
    { [ "$vmx_disk" = "${name}.vmdk" ] && [ -f "$pair_stage/$vmx_disk" ]; } || {
        echo "ERROR: $vmx names its disk '${vmx_disk:-?}', not the ${name}.vmdk beside it" >&2
        rm -rf "$pair_stage"
        exit 1
    }
    mv "$pair_stage/${name}.vmdk" "$parallels/${name}.vmdk"
    mv "$pair_stage/$vmx" "$parallels/$vmx"
    rm -rf "$pair_stage"

    local stage="$out/.ova-stage" disk="${name}-disk1.vmdk" ovf="${name}.ovf" mf="${name}.mf" disk_size
    rm -rf "$stage" "$vmware/${name}.ova" "$vmware/${name}.ova.partial"
    mkdir -p "$stage"
    convert_checked vmdk "subformat=streamOptimized" "$stage/$disk"
    disk_size="$(file_size "$stage/$disk")"
    # The template's own comments and the other architecture's lines go first,
    # then this architecture's tag, then the values.
    sed -e '/^@@/d' -e "/^@${other_arch}@/d" -e "s|^@${arch}@||" \
        -e "s|@DISK_FILE@|$disk|g" -e "s|@DISK_FILE_SIZE@|$disk_size|g" \
        -e "s|@CAPACITY@|$raw_size|g" -e "s|@NAME@|$name|g" \
        -e "s|@VERSION@|$version|g" -e "s|@BUILD_ID@|$build_id|g" \
        -e "s|@ARCH@|$arch|g" -e "s|@ENGINE@|$engine|g" \
        -e "s|@CPUS@|$cpus|g" -e "s|@MEMORY_MB@|$memory_mb|g" -e "s|@SYSTEM_TYPE@|vmx-$hw_version|g" \
        "$template" > "$stage/$ovf"
    grep -q '@[A-Za-z0-9_]*@' "$stage/$ovf" && { echo "ERROR: an OVF placeholder or architecture tag was not rendered" >&2; exit 1; }
    (
        cd "$stage" || exit 1
        {
            echo "SHA256($ovf)= $(sha256_of "$ovf")"
            echo "SHA256($disk)= $(sha256_of "$disk")"
        } > "$mf"
        # The OVF spec wants the descriptor first; the manifest after what it covers.
        # The stage is a folder of <out dir>, so the OVA's folder is its sibling.
        tar --format=ustar -cf "../vmware/${name}.ova.partial" "$ovf" "$disk" "$mf"
    )
    [ "$(tar -tf "$vmware/${name}.ova.partial" | tr '\n' ' ')" = "$ovf $disk $mf " ] || {
        echo "ERROR: the OVA's members are not ovf, disk, manifest in that order" >&2
        rm -rf "$stage" "$vmware/${name}.ova.partial"
        exit 1
    }
    mv "$vmware/${name}.ova.partial" "$vmware/${name}.ova"
    rm -rf "$stage"
    echo "VM images under $out: vmware/${name}.ova ($(( $(file_size "$vmware/${name}.ova") / 1048576 )) MiB) and vmware/${name}.vmdk ($(( $(file_size "$vmware/${name}.vmdk") / 1048576 )) MiB), hyper-v/${name}.vhdx ($(( $(file_size "$hyperv/${name}.vhdx") / 1048576 )) MiB), parallels/$vmx with a copy of ${name}.vmdk (virtual size $raw_size bytes)"
}

# GNU and BSD spellings of two small things the conversion needs.
file_size() { stat -c %s "$1" 2>/dev/null || stat -f %z "$1"; }
sha256_of() { (sha256sum "$1" 2>/dev/null || shasum -a 256 "$1") | cut -d' ' -f1; }

# write_installer_iso <buildroot dir> <images dir> <arch> <engine>: the
# installer medium beside the raw image, for the architecture of the build
# (x86_64 or aarch64: GRUB's platform, the loader's name and the serial
# console follow it, the rest is the same). A UEFI-bootable hybrid ISO: a
# virtual DVD boots it through El Torito, a USB stick written with dd through
# the GPT entry xorriso appends (the EFI System Partition, the same FAT
# image), both proven under OVMF on x86_64. On aarch64 the medium is the way
# onto the hypervisors of that architecture. It carries this build's kernel,
# the repository's
# initramfs (the same embra-init as the slot), rootfs.squashfs, the disk's
# ESP tree (what the installer copies onto the target's ESP),
# /embraos/install.manifest (install.v1: the release, the rootfs's size and
# sha256) and /LICENSE, the repository's licence, where it can be read before
# the medium is booted; it boots a second GRUB image: the disk's module list plus
# iso9660, part_msdos, search_fs_file and chain, with an early config that
# finds the medium by its manifest and loads /boot/grub/grub.cfg from it
# (board/embraos/grub-installer.cfg.in, rendered here). The FAT image is
# labelled EMBRAISO, never ESP: the menu's "Boot the installed disk" finds
# the disk's ESP by that label. Every tool is Buildroot's (host-xorriso,
# host-grub2, host-dosfstools, host-mtools): the same ISO on every
# workstation. The ISO is staged and moved whole; a failed check leaves none.
write_installer_iso() {
    local br="$1" out="$2" arch="$3" engine="$4"
    # What follows the architecture: GRUB's platform (the directory Buildroot
    # built its modules in and grub-mkimage's format), the loader's name on
    # the removable-media path, and the serial console post_image.sh gives
    # the disk's own menu.
    local grub_platform loader serial
    case "$arch" in
        x86_64)  grub_platform="x86_64-efi"; loader="BOOTX64.EFI";  serial="ttyS0" ;;
        aarch64) grub_platform="arm64-efi";  loader="BOOTAA64.EFI"; serial="ttyAMA0" ;;
        *) echo "ERROR: installer ISO: no loader for the architecture '$arch' (x86_64 or aarch64)" >&2; return 1 ;;
    esac
    local host="$br/output/host" images="$br/output/images" triple="${arch}-unknown-linux-musl"
    local xorriso="$host/bin/xorriso" mkimage="$host/bin/grub-mkimage" lint="$host/bin/grub-script-check"
    local mcopy="$host/bin/mcopy" mmd="$host/bin/mmd" mkfsfat
    # `|| true`: under the callers' `set -e -o pipefail` a listing that finds
    # nothing would end the build here, before the message below names what
    # is missing.
    mkfsfat="$(ls "$host/sbin/mkfs.fat" "$host/sbin/mkdosfs" 2>/dev/null | head -n1 || true)"
    local t
    for t in "$xorriso" "$mkimage" "$lint" "$mcopy" "$mmd" "${mkfsfat:-$host/sbin/mkfs.fat}"; do
        [ -x "$t" ] || { echo "ERROR: installer ISO: $t is missing (BR2_PACKAGE_HOST_XORRISO, _GRUB2, _MTOOLS, _DOSFSTOOLS)" >&2; return 1; }
    done
    local kernel="" k
    for k in Image bzImage; do
        [ -f "$images/$k" ] && { kernel="$k"; break; }
    done
    [ -n "$kernel" ] || { echo "ERROR: installer ISO: no kernel image in $images" >&2; return 1; }
    local grub_core
    grub_core="$(ls -d "$br"/output/build/grub2-*/build-"$grub_platform"/grub-core 2>/dev/null | head -n1 || true)"
    [ -d "$grub_core" ] || { echo "ERROR: installer ISO: GRUB's $grub_platform modules are not built under $br/output/build" >&2; return 1; }
    local modules
    modules="$(sed -n 's/^BR2_TARGET_GRUB2_BUILTIN_MODULES_EFI="\(.*\)"$/\1/p' "buildroot/configs/embraos_${arch}_defconfig")"
    [ -n "$modules" ] || { echo "ERROR: installer ISO: no BR2_TARGET_GRUB2_BUILTIN_MODULES_EFI in the $arch defconfig" >&2; return 1; }
    local release="target/$triple/release/embraos-release"
    [ -f "$release" ] || { echo "ERROR: installer ISO: $release is missing" >&2; return 1; }
    [ -f initramfs.cpio.gz ] || { echo "ERROR: installer ISO: initramfs.cpio.gz is missing (scripts/create_initramfs.sh)" >&2; return 1; }
    [ -d "$images/efi-part/EFI/BOOT" ] || { echo "ERROR: installer ISO: $images/efi-part/EFI/BOOT (the disk's ESP tree) is missing" >&2; return 1; }

    local stage="$out/.iso-stage" tree
    rm -rf "$stage"
    tree="$stage/tree"
    mkdir -p "$tree/boot/grub" "$tree/embraos/esp" "$tree/EFI/BOOT" "$stage/efi/EFI/BOOT"
    cp "$images/$kernel" "$tree/boot/$kernel"
    cp initramfs.cpio.gz "$tree/boot/initramfs.cpio.gz"
    # The two initrds beside it, as the disk's GRUB loads them: the microcode
    # (first, uncompressed) and the probe-time firmware (post_build.sh).
    cp "$br/output/target/boot/ucode.cpio" "$tree/boot/ucode.cpio"
    cp "$br/output/target/boot/firmware.cpio.gz" "$tree/boot/firmware.cpio.gz"
    cp "$images/rootfs.squashfs" "$tree/embraos/rootfs.squashfs"
    cp -r "$images/efi-part/EFI" "$tree/embraos/esp/EFI"
    # The licence at the medium's root: it can be read before the medium is
    # booted. The same file as the root filesystem's (assert_license_text).
    [ -s LICENSE ] || { echo "ERROR: installer ISO: the repository's LICENSE is missing" >&2; rm -rf "$stage"; return 1; }
    cp LICENSE "$tree/LICENSE"

    # The menu.
    local version
    version="$(sed -n 's/^VERSION=//p' "$release" | head -n1)"
    # The template's own comments (lines that start with @@) are not rendered.
    sed -e '/^@@/d' \
        -e "s|@KERNEL_IMAGE@|$kernel|g" -e "s|@SERIAL_CONSOLE@|$serial|g" -e "s|@VERSION@|$version|g" \
        -e "s|@LOADER@|$loader|g" \
        buildroot/board/embraos/grub-installer.cfg.in > "$tree/boot/grub/grub.cfg"
    if grep -q '@[A-Z_]*@' "$tree/boot/grub/grub.cfg"; then
        echo "ERROR: installer ISO: an unrendered placeholder in grub.cfg: $(grep -o '@[A-Z_]*@' "$tree/boot/grub/grub.cfg" | sort -u | tr '\n' ' ')" >&2
        rm -rf "$stage"; return 1
    fi
    "$lint" "$tree/boot/grub/grub.cfg" || { echo "ERROR: installer ISO: grub-script-check refused the menu" >&2; rm -rf "$stage"; return 1; }

    # The loader: the disk's modules and the medium's, an early config that
    # finds the medium by its manifest.
    printf 'search --no-floppy --file /embraos/install.manifest --set=root\nset prefix=($root)/boot/grub\n' > "$stage/early.cfg"
    "$lint" "$stage/early.cfg" || { rm -rf "$stage"; return 1; }
    # shellcheck disable=SC2086
    "$mkimage" -d "$grub_core" -O "$grub_platform" -p /boot/grub -c "$stage/early.cfg" -o "$stage/efi/EFI/BOOT/$loader" \
        $modules iso9660 part_msdos search_fs_file chain \
        || { echo "ERROR: installer ISO: grub-mkimage failed" >&2; rm -rf "$stage"; return 1; }
    cp "$stage/efi/EFI/BOOT/$loader" "$tree/EFI/BOOT/$loader"

    # The manifest and the sums.
    local sq_size sq_sha
    sq_size="$(stat -c %s "$tree/embraos/rootfs.squashfs")"
    sq_sha="$(sha256sum "$tree/embraos/rootfs.squashfs" | cut -d' ' -f1)"
    {
        echo "FORMAT=install.v1"
        grep -E '^(VERSION|BUILD_ID|ARCH|ENGINE|BUILT)=' "$release"
        echo "KERNEL=boot/$kernel"
        echo "ROOTFS=embraos/rootfs.squashfs"
        echo "ROOTFS_SIZE=$sq_size"
        echo "ROOTFS_SHA256=$sq_sha"
    } > "$tree/embraos/install.manifest"
    [ "$(sed -n 's/^ENGINE=//p' "$tree/embraos/install.manifest")" = "$engine" ] \
        || { echo "ERROR: installer ISO: the release stamp says ENGINE=$(sed -n 's/^ENGINE=//p' "$release"), the build $engine" >&2; rm -rf "$stage"; return 1; }
    cp "$release" "$tree/embraos/embraos-release"
    (cd "$tree" && find . -type f ! -name SHA256SUMS -print0 | LC_ALL=C sort -z | xargs -0 sha256sum > SHA256SUMS)

    # The El Torito EFI image: FAT, the loader alone, at least 8 MiB.
    local efi_bytes efi_mib
    efi_bytes="$(stat -c %s "$stage/efi/EFI/BOOT/$loader")"
    [ "$efi_bytes" -lt $((4 * 1024 * 1024)) ] || { echo "ERROR: installer ISO: the loader is $efi_bytes bytes; look at its module list" >&2; rm -rf "$stage"; return 1; }
    efi_mib=$(( (efi_bytes + 1024 * 1024 + 1024 * 1024 - 1) / (1024 * 1024) ))
    [ "$efi_mib" -ge 8 ] || efi_mib=8
    dd if=/dev/zero of="$stage/efiboot.img" bs=1M count="$efi_mib" status=none
    "$mkfsfat" -n EMBRAISO "$stage/efiboot.img" > /dev/null
    "$mmd" -i "$stage/efiboot.img" ::EFI ::EFI/BOOT
    "$mcopy" -i "$stage/efiboot.img" "$stage/efi/EFI/BOOT/$loader" ::EFI/BOOT/

    # The ISO: ISO 9660 level 3 (a file over 4 GiB would be allowed), Rock
    # Ridge and Joliet, the EFI image as the El Torito boot image and as GPT
    # partition 2 (type EFI System), the MBR a protective one.
    local iso="$stage/embraos.iso"
    "$xorriso" -as mkisofs -iso-level 3 -rational-rock -joliet -joliet-long \
        -volid EMBRAOS_INSTALL -appid "embraOS installer $version" \
        -eltorito-alt-boot -e --interval:appended_partition_2:all:: -no-emul-boot \
        -append_partition 2 0xef "$stage/efiboot.img" -appended_part_as_gpt \
        -o "$iso" "$tree" > "$stage/xorriso.log" 2>&1 \
        || { echo "ERROR: installer ISO: xorriso failed:" >&2; tail -n 5 "$stage/xorriso.log" >&2; rm -rf "$stage"; return 1; }

    # The checks: one UEFI El Torito image, a GPT, the manifest inside equal
    # to the one written, the squashfs's hash in it.
    "$xorriso" -indev "$iso" -report_el_torito plain 2>/dev/null | grep -q 'El Torito boot img.*UEFI' \
        || { echo "ERROR: installer ISO: no UEFI El Torito boot image in $iso" >&2; rm -rf "$stage"; return 1; }
    "$xorriso" -indev "$iso" -report_system_area plain 2>/dev/null | grep -q '^GPT' \
        || { echo "ERROR: installer ISO: no GPT in $iso (the USB stick path)" >&2; rm -rf "$stage"; return 1; }
    "$xorriso" -osirrox on -indev "$iso" -extract /embraos/install.manifest "$stage/manifest.read" > /dev/null 2>&1
    cmp -s "$stage/manifest.read" "$tree/embraos/install.manifest" \
        || { echo "ERROR: installer ISO: the manifest read back from $iso differs" >&2; rm -rf "$stage"; return 1; }
    grep -q "^ROOTFS_SHA256=$sq_sha\$" "$stage/manifest.read" || { rm -rf "$stage"; return 1; }
    "$xorriso" -osirrox on -indev "$iso" -extract /LICENSE "$stage/license.read" > /dev/null 2>&1
    cmp -s "$stage/license.read" LICENSE \
        || { echo "ERROR: installer ISO: /LICENSE read back from $iso is missing or is not the repository's LICENSE" >&2; rm -rf "$stage"; return 1; }
    mv "$iso" "$out/embraos.iso"
    echo "installer ISO in $out: embraos.iso ($(( $(file_size "$out/embraos.iso") / 1048576 )) MiB; a DVD through El Torito, a USB stick through its GPT; the loader $efi_bytes bytes in a $efi_mib MiB FAT)"
    rm -rf "$stage"
}
