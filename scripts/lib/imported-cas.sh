# shellcheck shell=bash
# Imported_CAs/: the certificate authorities a builder bakes into an image.
#
# An operator's private CA reaches a running machine through STATE's
# ca-certificates/ (scripts/seed-state.sh --ca-dir), which needs the raw disk
# image on the build host. A machine installed from the ISO, a hypervisor's
# converted disk and bare metal have no such step, so a build can carry the
# certificates itself: the *.pem and *.crt files of Imported_CAs/ in the
# repository root go read-only into the root filesystem, and embrad
# (crates/embrad/src/ca_bundle.rs) and the brain (crates/embra-brain/src/net.rs)
# trust them beside STATE's. Every machine installed from that build trusts
# them, which is why scripts/release-update.sh signs no release from one.
#
# Sourced by scripts/lib/image-common.sh (the two image scripts check the
# folder before anything is built, and the image is checked against it after
# Buildroot), by buildroot/board/embraos/post_build.sh (which bakes it) and by
# scripts/release-update.sh. It defines variables and functions and runs
# nothing. macOS runs it under /bin/bash 3.2 with the BSD tools.

# The folder, in the repository root. Its contents are not in git (.gitignore
# keeps the README alone): every push is public, and a private CA's
# certificate names the hosts behind it.
IMPORTED_CAS_FOLDER="Imported_CAs"
# Where the root filesystem carries them. The two readers name the same
# directory; guard the_image_s_ca_directory_is_the_one_the_build_bakes
# (embra-brain) reads this line and embrad's.
IMPORTED_CAS_ROOTFS_DIR="usr/share/embra/ca-certificates"
# The readers skip a larger file (MAX_CA_FILE_SIZE, ca_bundle.rs), so the
# build refuses one: what is baked is what is trusted.
IMPORTED_CAS_MAX_BYTES=262144

# imported_cas_list <repository root>: the names of the folder's *.pem and
# *.crt files, one a line, sorted. Nothing when the folder is absent or holds
# none.
imported_cas_list() {
    local dir="$1/$IMPORTED_CAS_FOLDER" f
    [ -d "$dir" ] || return 0
    for f in "$dir"/*.pem "$dir"/*.crt; do
        if [ -e "$f" ] || [ -L "$f" ]; then
            printf '%s\n' "${f##*/}"
        fi
    done | LC_ALL=C sort
}

# imported_cas_check <repository root> [<openssl>]: 0 when every file of the
# folder is fit to bake, with what will be baked listed on stdout; 1 with each
# reason on stderr. A file is fit when it is a PEM file of certificates and
# nothing else: no private key, no other PEM block, at most
# IMPORTED_CAS_MAX_BYTES, a name of letters, digits, dots, dashes and
# underscores, and, with an openssl at hand, certificates that parse. Anything
# else in the folder but the README and dotfiles stops the build as well: a
# file of another kind would not be baked, and saying nothing about it leaves
# its owner believing it was.
imported_cas_check() {
    local root="$1" openssl="${2:-}" dir f base size begins ends others problems=0 files=0 certs=0
    local tmp="" n block subject fp end flags text lines=""
    # The folder is walked in byte order whatever the builder's locale, so the
    # listing reads the same on every host.
    local LC_ALL=C
    dir="$root/$IMPORTED_CAS_FOLDER"
    if [ ! -d "$dir" ]; then
        echo "imported CAs: none (no $IMPORTED_CAS_FOLDER/ folder)"
        return 0
    fi
    for f in "$dir"/*; do
        if [ ! -e "$f" ] && [ ! -L "$f" ]; then continue; fi   # an empty folder leaves the pattern itself
        base="${f##*/}"
        case "$base" in
            README.md) continue ;;
            *.pem|*.crt) ;;
            *)
                echo "ERROR: $IMPORTED_CAS_FOLDER/$base is neither a *.pem nor a *.crt file and would not be baked: convert it to a PEM certificate (openssl x509 -inform der -in <file> -out <name>.pem) or take it out of the folder" >&2
                problems=$((problems + 1)); continue ;;
        esac
        case "$base" in
            *[!A-Za-z0-9._-]*)
                echo "ERROR: $IMPORTED_CAS_FOLDER/$base: a certificate file's name is letters, digits, dots, dashes and underscores" >&2
                problems=$((problems + 1)); continue ;;
        esac
        if [ ! -f "$f" ] || [ ! -r "$f" ]; then
            echo "ERROR: $IMPORTED_CAS_FOLDER/$base is not a readable file" >&2
            problems=$((problems + 1)); continue
        fi
        size=$(( $(wc -c < "$f") ))
        if [ "$size" -gt "$IMPORTED_CAS_MAX_BYTES" ]; then
            echo "ERROR: $IMPORTED_CAS_FOLDER/$base is $size bytes; the OS reads no certificate file over $IMPORTED_CAS_MAX_BYTES" >&2
            problems=$((problems + 1)); continue
        fi
        if LC_ALL=C grep -q 'PRIVATE KEY' "$f"; then
            echo "ERROR: $IMPORTED_CAS_FOLDER/$base carries a private key: a certificate authority's certificate alone belongs here, never its key" >&2
            problems=$((problems + 1)); continue
        fi
        begins="$(LC_ALL=C grep -c '^-----BEGIN CERTIFICATE-----' "$f" || true)"
        ends="$(LC_ALL=C grep -c '^-----END CERTIFICATE-----' "$f" || true)"
        others="$(LC_ALL=C grep '^-----BEGIN ' "$f" | LC_ALL=C grep -vc '^-----BEGIN CERTIFICATE-----' || true)"
        if [ "$begins" -lt 1 ] || [ "$begins" != "$ends" ]; then
            echo "ERROR: $IMPORTED_CAS_FOLDER/$base is not a PEM certificate file (BEGIN CERTIFICATE lines: $begins, END CERTIFICATE lines: $ends)" >&2
            problems=$((problems + 1)); continue
        fi
        if [ "$others" -gt 0 ]; then
            echo "ERROR: $IMPORTED_CAS_FOLDER/$base holds a PEM block that is not a certificate; the OS reads BEGIN CERTIFICATE blocks alone" >&2
            problems=$((problems + 1)); continue
        fi
        files=$((files + 1))
        if [ -z "$openssl" ]; then
            certs=$((certs + begins))
            lines="$lines  $base: $begins certificate(s), not parsed (no openssl at hand)
"
            continue
        fi
        # Each block is parsed on its own: openssl reads the first of a file.
        tmp="$(mktemp -d)"
        awk -v d="$tmp" '/^-----BEGIN CERTIFICATE-----/{n++; f=d"/"n".pem"} f{print > f} /^-----END CERTIFICATE-----/{if (f) close(f); f=""}' "$f"
        n=1
        while [ "$n" -le "$begins" ]; do
            block="$tmp/$n.pem"
            if ! subject="$("$openssl" x509 -in "$block" -noout -subject 2>/dev/null)"; then
                echo "ERROR: $IMPORTED_CAS_FOLDER/$base: certificate $n of $begins does not parse" >&2
                problems=$((problems + 1)); n=$((n + 1)); continue
            fi
            fp="$("$openssl" x509 -in "$block" -noout -fingerprint -sha256 2>/dev/null | sed 's/^[^=]*=//')"
            end="$("$openssl" x509 -in "$block" -noout -enddate 2>/dev/null | sed 's/^notAfter=//')"
            flags=""
            # Read whole and matched in the shell: a `grep -q` at the end of a
            # pipe leaves early, and under pipefail the writer's SIGPIPE would
            # read as "no match".
            text="$("$openssl" x509 -in "$block" -noout -text 2>/dev/null || true)"
            case "$text" in *CA:TRUE*) ;; *) flags="$flags; not a CA certificate" ;; esac
            "$openssl" x509 -in "$block" -noout -checkend 0 >/dev/null 2>&1 || flags="$flags; EXPIRED"
            certs=$((certs + 1))
            lines="$lines  $base: $(printf '%s' "$subject" | sed -e 's/^subject= *//' -e 's/^ *//'); sha256 $fp; expires $end$flags
"
            n=$((n + 1))
        done
        rm -rf "$tmp"
    done
    if [ "$problems" -gt 0 ]; then
        echo "ERROR: $IMPORTED_CAS_FOLDER/ has $problems file(s) that cannot be baked into the image (the lines above); nothing is built" >&2
        return 1
    fi
    if [ "$files" -eq 0 ]; then
        echo "imported CAs: none ($IMPORTED_CAS_FOLDER/ holds no *.pem or *.crt file)"
        return 0
    fi
    echo "imported CAs: $certs certificate(s) in $files file(s) of $IMPORTED_CAS_FOLDER/, baked into /$IMPORTED_CAS_ROOTFS_DIR and trusted by every machine installed from this build:"
    printf '%s' "$lines"
    return 0
}

# imported_cas_install <repository root> <target directory> [<openssl>]: the
# folder's certificate files, checked, copied into the target's
# IMPORTED_CAS_ROOTFS_DIR. The directory is written whole: what an earlier
# run left there goes first, and a folder without certificates leaves none.
imported_cas_install() {
    local root="$1" target="${2:-}" openssl="${3:-}" dest base names
    if [ -z "$target" ] || [ ! -d "$target" ]; then
        echo "ERROR: imported_cas_install: no target directory '$target'" >&2
        return 1
    fi
    imported_cas_check "$root" "$openssl" || return 1
    dest="$target/$IMPORTED_CAS_ROOTFS_DIR"
    rm -rf "$dest"
    names="$(imported_cas_list "$root")"
    [ -n "$names" ] || return 0
    mkdir -p "$dest"
    # The check above admits names without a space or a pattern character, so
    # the list splits on its newlines alone.
    for base in $names; do
        cp "$root/$IMPORTED_CAS_FOLDER/$base" "$dest/$base" || return 1
        chmod 0644 "$dest/$base" || return 1
    done
}
