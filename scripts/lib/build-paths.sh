# shellcheck shell=bash
# build-paths.sh: the build host's directories stay out of what an image
# build produces.
#
# A Rust program carries the path of every source file a panic message or a
# log line can name: under cargo's registry for a dependency, under the
# target directory for generated code, under the checkout for a crate the
# frontend takes by path. On a build host those lie in the builder's home
# directory, so each program named the builder's login and where the
# checkout sits some hundreds of times, and the web console's bundle with
# them. Both image scripts therefore hand rustc a stand-in for each of these
# directories (`--remap-path-prefix`) and read the programs back before
# Buildroot runs.
#
# The root filesystem carries records of the build as well, which nothing
# reads at run time: post_build.sh removes them (remove_build_records). Three
# packages Buildroot compiles would name its own directory, and are handed a
# stand-in or patched (BUILD_PATHS_BY_PACKAGE). The image scripts read the
# whole target directory back. The kernel's banner names who built it, and is
# told a name of the image's own (KERNEL_BUILT_BY).
#
# Sourced by scripts/lib/image-common.sh and by
# buildroot/board/embraos/post_build.sh; it defines variables and functions
# and runs nothing. Run by itself, it calls one function:
#
#   build-paths.sh <function> <arguments>
#
# macOS runs the first half under /bin/bash 3.2 with the BSD tools.

# builder_dirs <checkout>: the directories of this build host that a build
# reads sources from or writes to, each with the name the programs carry in
# its place, one pair a line with a tab between them: the builder's home, the
# checkout, the target directory when the environment names another, cargo's
# home and rustup's. A directory is listed as the environment names it and,
# where a link is on its way, as the kernel names it too: cargo hands rustc
# its own home as the environment has it and the working directory resolved.
# The order is part of the contract: rustc applies the LAST prefix that
# matches, so the home comes first and what usually lies inside it after.
builder_dirs() {
    local root="$1" home="${HOME:-}"
    builder_dir "$home" /builder
    builder_dir "$root" /embraOS
    builder_dir "${CARGO_TARGET_DIR:-}" /embraOS/target
    builder_dir "${CARGO_HOME:-${home:+$home/.cargo}}" /cargo
    builder_dir "${RUSTUP_HOME:-${home:+$home/.rustup}}" /rustup
}

# builder_dir <directory> <stand-in>: one pair of builder_dirs, twice when the
# directory's resolved name is another. An unset directory and / give none.
builder_dir() {
    local dir="$1" to="$2" real
    [ -n "$dir" ] || return 0
    [ "${dir#/}" != "$dir" ] || dir="$PWD/$dir"
    while [ "$dir" != "/" ] && [ "${dir%/}" != "$dir" ]; do dir="${dir%/}"; done
    [ "$dir" != "/" ] || return 0
    printf '%s\t%s\n' "$dir" "$to"
    real="$(cd "$dir" 2>/dev/null && pwd -P)" || real=""
    if [ -n "$real" ] && [ "$real" != "/" ] && [ "$real" != "$dir" ]; then
        printf '%s\t%s\n' "$real" "$to"
    fi
}

# export_builder_path_remaps <checkout>: CARGO_ENCODED_RUSTFLAGS with one
# --remap-path-prefix for every pair of builder_dirs, behind the flags the
# builder's environment had (CARGO_ENCODED_RUSTFLAGS, else RUSTFLAGS word by
# word, as cargo reads them). The encoded form, because a directory may hold
# a space; it is also the first of the places cargo takes flags from, so no
# other one drops the remaps. Cargo reads the environment in place of a
# configuration file's `rustflags`, not beside them: a builder who keeps
# flags there for this target names them in RUSTFLAGS for an image build.
# trunk's cargo (the web bundle) and the cross-build both inherit it.
export_builder_path_remaps() {
    local root="$1" sep flags="" word dir to
    local -a words
    sep="$(printf '\037')"
    if [ -n "${CARGO_ENCODED_RUSTFLAGS:-}" ]; then
        flags="$CARGO_ENCODED_RUSTFLAGS"
    else
        words=()
        IFS=' ' read -r -a words <<EOF
$(printf '%s' "${RUSTFLAGS:-}" | tr '\t\n' '  ')
EOF
        for word in ${words[@]+"${words[@]}"}; do
            flags="${flags:+$flags$sep}$word"
        done
    fi
    while IFS="$(printf '\t')" read -r dir to; do
        [ -n "$dir" ] || continue
        flags="${flags:+$flags$sep}--remap-path-prefix=$dir=$to"
    done <<EOF
$(builder_dirs "$root")
EOF
    export CARGO_ENCODED_RUSTFLAGS="$flags"
}

# builder_needles <checkout>: what the checks look for, one directory a line
# with its closing slash: the directories of builder_dirs that lie in no
# other of them. A directory directly under / is not looked for: /root and a
# container's /work name nobody, and a root filesystem is full of such names
# (the remap covers them all the same). The closing slash keeps
# /home/ann from matching /home/anna.
builder_needles() {
    local root="$1" dirs dir other inside
    dirs="$(builder_dirs "$root" | cut -f1)"
    printf '%s\n' "$dirs" | while IFS= read -r dir; do
        [ -n "$dir" ] || continue
        # two components or more: something is left after the first one goes
        [ "${dir#/*/}" != "$dir" ] || continue
        inside=0
        while IFS= read -r other; do
            [ -n "$other" ] || continue
            [ "$other" != "$dir" ] || continue
            [ "${other#/*/}" != "$other" ] || continue
            if [ "${dir#"$other"/}" != "$dir" ]; then inside=1; fi
        done <<EOF
$dirs
EOF
        [ "$inside" = 0 ] || continue
        printf '%s/\n' "$dir"
    done | LC_ALL=C sort -u
}

# files_naming_builder_dirs <checkout> <file or directory>...: the files at
# and under the given paths that name one of builder_needles, one a line;
# nothing when there is nothing to look for. Status 2 when a file could not
# be read: a check that skipped a file has not checked.
files_naming_builder_dirs() {
    local root="$1" needle out rc=0
    local -a args
    shift
    args=()
    while IFS= read -r needle; do
        [ -n "$needle" ] || continue
        args+=(-e "$needle")
    done <<EOF
$(builder_needles "$root")
EOF
    [ ${#args[@]} -gt 0 ] || return 0
    out="$(LC_ALL=C grep -r -l -a -F "${args[@]}" -- "$@")" || rc=$?
    if [ "$rc" -gt 1 ]; then
        echo "ERROR: not every file could be read under: $*" >&2
        return 2
    fi
    [ -z "$out" ] || printf '%s\n' "$out"
}

# builder_strings <checkout> <file>: the texts of one file that name one of
# builder_needles, one a line, cut at 200 characters.
builder_strings() {
    local root="$1" file="$2" needle
    local -a args
    args=()
    while IFS= read -r needle; do
        [ -n "$needle" ] || continue
        args+=(-e "$needle")
    done <<EOF
$(builder_needles "$root")
EOF
    [ ${#args[@]} -gt 0 ] || return 0
    LC_ALL=C tr -c '[:print:]' '\n' < "$file" | LC_ALL=C grep -F "${args[@]}" | cut -c1-200 || true
}

# assert_programs_name_no_builder_dir <checkout> <directory of the built
# programs> <directory of the web bundle>: every program cargo linked (the
# executable files directly in its release directory: the services, the
# database, the initramfs's init) and every file of the bundle embra-web
# embeds names none of builder_needles; the build stops on one that does,
# with the file and a text of it. Called by both image scripts after the
# cross-build, where the host that ran cargo still knows its own directories
# (a Mac's are unknown inside the container the Buildroot pass runs in).
assert_programs_name_no_builder_dir() {
    local root="$1" release="$2" bundle="$3" found f looked
    local -a programs
    programs=()
    while IFS= read -r f; do
        [ -n "$f" ] || continue
        programs+=("$f")
    done <<EOF
$(find "$release" -maxdepth 1 -type f -perm -u+x 2>/dev/null | LC_ALL=C sort || true)
EOF
    [ ${#programs[@]} -gt 0 ] || { echo "ERROR: no program under $release to read back" >&2; exit 1; }
    [ -d "$bundle" ] || { echo "ERROR: no web bundle at $bundle to read back" >&2; exit 1; }
    looked="$(builder_needles "$root" | grep -c . || true)"
    if [ "$looked" -eq 0 ]; then
        echo "build paths: nothing to look for in the programs (every directory of this build is directly under /)"
        return 0
    fi
    found="$(files_naming_builder_dirs "$root" "${programs[@]}" "$bundle")" || exit 1
    if [ -n "$found" ]; then
        echo "ERROR: a directory of this build host is in what was built:" >&2
        printf '%s\n' "$found" | while IFS= read -r f; do
            [ -n "$f" ] || continue
            printf '       %s\n         %s\n' "$f" "$(builder_strings "$root" "$f" | head -n 1)" >&2
        done
        echo "       rustc was handed a stand-in for each of these directories (scripts/lib/build-paths.sh)," >&2
        echo "       so this is a text a crate keeps of its own build directory, or flags that did not" >&2
        echo "       reach rustc (cargo takes them from CARGO_ENCODED_RUSTFLAGS)." >&2
        exit 1
    fi
    echo "build paths: none of this host's directories in the ${#programs[@]} programs under $release or in the web bundle"
}

# remove_build_records <target directory>: what a root filesystem carries of
# its own build and nothing reads at run time, each of them naming the
# directories the build ran in. Called by post_build.sh on every build.
remove_build_records() {
    local tgt="${1:-}"
    [ -n "$tgt" ] && [ -d "$tgt" ] || { echo "ERROR: remove_build_records needs the target directory" >&2; return 1; }
    # The in-OS toolchain's installer leaves a log of every command it ran
    # and a manifest of every file a component put down, with the staging
    # directory's full path in each line, and an uninstaller that reads
    # them. A read-only root filesystem uninstalls nothing.
    rm -f "$tgt/opt/rust/lib/rustlib/install.log" \
          "$tgt/opt/rust/lib/rustlib/uninstall.sh" \
          "$tgt"/opt/rust/lib/rustlib/manifest-*
    # ICU: the makefile fragments for building a data package against it,
    # which name the cross-compiler, and the two build tools that Buildroot's
    # own list of ICU's tools (ICU_REMOVE_DEV_FILES) does not reach, with the
    # two libraries nothing else loads (libicutu keeps the cross-compiler's
    # path; libicutest loads libicutu).
    rm -rf "$tgt/usr/lib/icu"
    rm -f "$tgt/usr/bin/icuexportdata" "$tgt/usr/bin/pkgdata" \
          "$tgt"/usr/lib/libicutu.so* "$tgt"/usr/lib/libicutest.so*
    # libxslt's script for a build against it, and the debugger's loader
    # for libstdc++'s pretty-printers: both name the toolchain's directory.
    rm -f "$tgt/usr/lib/xsltConf.sh" "$tgt"/usr/lib/libstdc++.so.*-gdb.py
}

# What Buildroot itself would compile its own directory into, as a shell
# pattern under the target directory and the package behind it: sqlite's
# shell and WebKit name their sources (`__FILE__`), libpsl the list its
# table was made from. buildroot/external.mk hands the compiler a stand-in
# for the first two and buildroot/patches/libpsl empties the third's name. A
# package built before that still says where it was built, and Buildroot
# rebuilds nothing whose options changed: packages_naming_builder_dirs finds
# such a package in the target directory the last build left, and the image
# scripts clean it before they repopulate that directory
# (clean_packages_that_name_the_builder, image-common.sh).
BUILD_PATHS_BY_PACKAGE=("usr/bin/sqlite3:sqlite" "usr/lib/libpsl.so.*:libpsl" "usr/lib/libWPEWebKit-*.so.*:wpewebkit")

# package_that_compiled <path under the target directory>: the package of
# BUILD_PATHS_BY_PACKAGE the file belongs to; status 1 for any other file.
package_that_compiled() {
    local rel="$1" entry
    for entry in "${BUILD_PATHS_BY_PACKAGE[@]}"; do
        # shellcheck disable=SC2254  # a pattern, on purpose
        case "$rel" in ${entry%%:*}) printf '%s\n' "${entry##*:}"; return 0 ;; esac
    done
    return 1
}

# packages_naming_builder_dirs <checkout> <target directory>: the packages of
# BUILD_PATHS_BY_PACKAGE whose file in that directory names one of
# builder_needles, one a line. Nothing without the directory (a first build)
# and nothing where there is nothing to look for (a container).
packages_naming_builder_dirs() {
    local root="$1" tgt="$2" entry pattern f found
    [ -d "$tgt" ] || return 0
    for entry in "${BUILD_PATHS_BY_PACKAGE[@]}"; do
        pattern="${entry%%:*}"
        # shellcheck disable=SC2231  # the pattern expands, on purpose
        for f in "$tgt"/$pattern; do
            [ -f "$f" ] || continue
            found="$(files_naming_builder_dirs "$root" "$f")" || return 2
            [ -n "$found" ] || continue
            printf '%s\n' "${entry##*:}"
            break
        done
    done
}

# The name the kernel's banner carries in place of the builder's login and of
# the build host's name: buildroot/external.mk hands the kernel's build both
# (KBUILD_BUILD_USER, KBUILD_BUILD_HOST).
KERNEL_BUILT_BY="embraos"

# kernel_banner_builder <built kernel tree>: `<login>@<host>` as the kernel's
# own header records them for its banner (include/generated/compile.h);
# nothing for a tree without the header (a kernel not built yet).
kernel_banner_builder() {
    local header="$1/include/generated/compile.h" by host
    [ -f "$header" ] || return 0
    by="$(sed -n 's/^#define LINUX_COMPILE_BY[[:space:]]*"\(.*\)"$/\1/p' "$header" | head -n 1)"
    host="$(sed -n 's/^#define LINUX_COMPILE_HOST[[:space:]]*"\(.*\)"$/\1/p' "$header" | head -n 1)"
    printf '%s@%s\n' "$by" "$host"
}

# Run by itself: one function and its arguments.
if [ "${BASH_SOURCE[0]}" = "$0" ] && [ $# -ge 1 ]; then
    set -euo pipefail
    fn="$1"; shift
    case "$fn" in
        builder_dirs|builder_needles|files_naming_builder_dirs|builder_strings|assert_programs_name_no_builder_dir|remove_build_records|package_that_compiled|packages_naming_builder_dirs|kernel_banner_builder)
            "$fn" "$@" ;;
        export_builder_path_remaps)
            "$fn" "$@"; printf '%s\n' "$CARGO_ENCODED_RUSTFLAGS" | tr '\037' '\n' ;;
        *) echo "build-paths.sh: no function $fn" >&2; exit 2 ;;
    esac
fi
