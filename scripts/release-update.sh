#!/bin/bash
# Prepare the update artifacts of a release: the slot image, its signed
# manifest, and the upload lines.
#
#   ./scripts/release-update.sh --tag v0.24.0-phase2 --arch x86_64 --engine fjall --key ~/keys/update-signing.key
#   ./scripts/release-update.sh --self-test
#
# Writes into output/update/ (or --out):
#   embraos-<tag>-<arch>-<engine>.rootfs.squashfs   the slot image the build produced
#   embraos-<arch>-<engine>.manifest.json            update.v1: version, tag, build id, arch,
#                                                     engine, schema, kernel, the rootfs's size and
#                                                     sha256, when it was built, the notes URL
#   embraos-<arch>-<engine>.manifest.json.sig        a raw Ed25519 signature over the manifest
#                                                     bytes, base64, made with the operator's key
#   embraos-<tag>-x86_64-<engine>.iso                 the installer medium of the same build
#                                                     (x86_64 only; William, 2026-11-08: a release
#                                                     asset), with
#   embraos-<tag>-x86_64-<engine>.iso.sha256           its sha256sum line and
#   embraos-<tag>-x86_64-<engine>.iso.sha256.sig       the same Ed25519 signature over that line:
#                                                     `sha256sum -c` the first, verify the second
#                                                     against scripts/update-signing.pub; the OS
#                                                     never downloads the ISO (it is for a fresh
#                                                     install), so it carries no manifest
#
# The manifest's name carries no version: the OS fetches
# <source>/embraos-<arch>-<engine>.manifest.json from the release marked
# latest, verifies the signature against scripts/update-signing.pub (baked into
# the rootfs), and only then downloads the squashfs the manifest names. Every
# release uploads all four manifests (two architectures, two engines), or an
# instance of the missing pair sees "no manifest at this source".
#
# Uploading is the operator's step; the private key never enters the
# repository or CI. The tag must be the one the tree is at, the version the
# one CITATION.cff records, and the tree clean.

set -euo pipefail

ROOT="$(cd "$(dirname "$0")/.." && pwd)"
TAG=""; ARCH=""; ENGINE=""; KEY=""; ROOTFS=""; ISO=""; OUT="$ROOT/output/update"; SELF_TEST=0
while [ $# -gt 0 ]; do
    case "$1" in
        --tag) TAG="${2:-}"; shift 2 ;;
        --arch) ARCH="${2:-}"; shift 2 ;;
        --engine) ENGINE="${2:-}"; shift 2 ;;
        --key) KEY="${2:-}"; shift 2 ;;
        --rootfs) ROOTFS="${2:-}"; shift 2 ;;
        --iso) ISO="${2:-}"; shift 2 ;;
        --out) OUT="${2:-}"; shift 2 ;;
        --self-test) SELF_TEST=1; shift ;;
        *) echo "ERROR: unknown argument: $1" >&2; exit 2 ;;
    esac
done

# openssl with Ed25519 raw signing (-rawin): OpenSSL 1.1.1 or later; macOS's
# LibreSSL openssl has none, Homebrew's openssl@3 does.
OPENSSL="${OPENSSL:-openssl}"
if ! "$OPENSSL" version 2>/dev/null | grep -q '^OpenSSL [13]\.'; then
    echo "ERROR: $OPENSSL is not OpenSSL 1.1.1+ (Ed25519 with -rawin is needed)." >&2
    echo "  macOS: brew install openssl@3 && OPENSSL=\$(brew --prefix openssl@3)/bin/openssl $0 ..." >&2
    exit 1
fi

sign() {   # sign <key.pem> <file> → base64 signature on stdout
    "$OPENSSL" pkeyutl -sign -rawin -inkey "$1" -in "$2" | base64 | tr -d '\n'
}
sha256_line() {   # sha256_line <file in the current directory> → the sha256sum line (GNU or BSD host)
    sha256sum "$1" 2>/dev/null || shasum -a 256 "$1"
}
sha256_check() {   # sha256_check <sums file in the current directory>; 0 when every line checks
    sha256sum -c --quiet "$1" >/dev/null 2>&1 || shasum -a 256 -c --quiet "$1" >/dev/null 2>&1
}
verify() {   # verify <pub.pem> <file> <sig.b64>; 0 when one key verifies
    local pubs tmpsig rc=1
    tmpsig="$(mktemp)"
    base64 -d < "$3" > "$tmpsig"
    # Each PUBLIC KEY block of the file is tried.
    pubs="$(mktemp -d)"
    awk -v d="$pubs" '/BEGIN PUBLIC KEY/{n++; f=d"/"n".pem"} f{print > f} /END PUBLIC KEY/{f=""}' "$1"
    for k in "$pubs"/*.pem; do
        [ -f "$k" ] || continue
        if "$OPENSSL" pkeyutl -verify -rawin -pubin -inkey "$k" -in "$2" -sigfile "$tmpsig" >/dev/null 2>&1; then rc=0; break; fi
    done
    rm -rf "$tmpsig" "$pubs"
    return $rc
}

if [ "$SELF_TEST" -eq 1 ]; then
    T="$(mktemp -d)"
    "$OPENSSL" genpkey -algorithm ed25519 -out "$T/key.pem" 2>/dev/null
    "$OPENSSL" pkey -in "$T/key.pem" -pubout -out "$T/pub.pem" 2>/dev/null
    printf '{"format":"update.v1","version":"0.0.0-test"}' > "$T/manifest.json"
    sign "$T/key.pem" "$T/manifest.json" > "$T/manifest.json.sig"
    verify "$T/pub.pem" "$T/manifest.json" "$T/manifest.json.sig" || { echo "self-test: the signature did not verify" >&2; exit 1; }
    printf '{"format":"update.v1","version":"0.0.0-tampered"}' > "$T/manifest.json"
    if verify "$T/pub.pem" "$T/manifest.json" "$T/manifest.json.sig"; then echo "self-test: a tampered manifest verified" >&2; exit 1; fi
    # The ISO's checksum line and its signature, the way an operator checks them.
    head -c 65536 /dev/urandom > "$T/embraos-v0.0.0-test-x86_64-fjall.iso"
    (cd "$T" && sha256_line embraos-v0.0.0-test-x86_64-fjall.iso > embraos-v0.0.0-test-x86_64-fjall.iso.sha256)
    sign "$T/key.pem" "$T/embraos-v0.0.0-test-x86_64-fjall.iso.sha256" > "$T/embraos-v0.0.0-test-x86_64-fjall.iso.sha256.sig"
    (cd "$T" && sha256_check embraos-v0.0.0-test-x86_64-fjall.iso.sha256) || { echo "self-test: the ISO's checksum line did not check" >&2; exit 1; }
    verify "$T/pub.pem" "$T/embraos-v0.0.0-test-x86_64-fjall.iso.sha256" "$T/embraos-v0.0.0-test-x86_64-fjall.iso.sha256.sig" || { echo "self-test: the ISO's checksum signature did not verify" >&2; exit 1; }
    printf 'x' >> "$T/embraos-v0.0.0-test-x86_64-fjall.iso"
    if (cd "$T" && sha256_check embraos-v0.0.0-test-x86_64-fjall.iso.sha256) 2>/dev/null; then echo "self-test: a changed ISO passed its checksum" >&2; exit 1; fi
    rm -rf "$T"
    echo "self-test: a throwaway key signs, verifies, and refuses a tampered manifest; an ISO's checksum line signs, checks, and refuses a changed ISO"
    exit 0
fi

[ -n "$TAG" ] && [ -n "$ARCH" ] && [ -n "$ENGINE" ] && [ -n "$KEY" ] || {
    echo "Usage: $0 --tag <vX.Y.Z-phaseN> --arch <x86_64|aarch64> --engine <fjall|rocksdb> --key <private.pem> [--rootfs <squashfs>] [--iso <embraos.iso>] [--out <dir>]" >&2
    exit 2
}
case "$ARCH" in x86_64|aarch64) ;; *) echo "ERROR: --arch must be x86_64 or aarch64" >&2; exit 2 ;; esac
case "$ENGINE" in fjall|rocksdb) ;; *) echo "ERROR: --engine must be fjall or rocksdb" >&2; exit 2 ;; esac
[ -f "$KEY" ] || { echo "ERROR: no key at $KEY" >&2; exit 2; }

cd "$ROOT"
# The release metadata, the tag and the tree agree, or nothing is signed.
VERSION="$(sed -n 's/^version: "\(.*\)"/\1/p' CITATION.cff | head -n1)"
[ "$TAG" = "v$VERSION" ] || { echo "ERROR: --tag $TAG is not v$VERSION (CITATION.cff)" >&2; exit 1; }
AT_TAG="$(git describe --tags --exact-match 2>/dev/null || true)"
[ "$AT_TAG" = "$TAG" ] || { echo "ERROR: the tree is at '${AT_TAG:-no tag}', not $TAG" >&2; exit 1; }
git diff-index --quiet HEAD -- || { echo "ERROR: the tree has uncommitted changes" >&2; exit 1; }
SCHEMA="$(grep -oE 'CURRENT_SCHEMA_VERSION: u32 = [0-9]+' crates/embra-brain/src/migrations/mod.rs | grep -oE '[0-9]+$')"
[ -n "$SCHEMA" ] || { echo "ERROR: CURRENT_SCHEMA_VERSION not found" >&2; exit 1; }
KERNEL="$(sed -n 's/^LINUX_KERNEL_VERSION="\${LINUX_KERNEL_VERSION:-\(.*\)}"/\1/p' scripts/lib/image-common.sh | head -n1)"
BUILD_ID="$(git rev-parse --short=10 HEAD)"

if [ -z "$ROOTFS" ]; then
    for c in output/images/rootfs.squashfs buildroot-src/output/images/rootfs.squashfs; do
        [ -f "$c" ] && { ROOTFS="$c"; break; }
    done
fi
[ -n "$ROOTFS" ] && [ -f "$ROOTFS" ] || { echo "ERROR: no rootfs.squashfs (build the image, or --rootfs <path>)" >&2; exit 1; }

# The slot image names its own arch and engine; with unsquashfs present they
# are checked against the flags, and the slot's /boot is checked whole.
if command -v unsquashfs >/dev/null 2>&1; then
    REL="$(unsquashfs -cat "$ROOTFS" etc/embraos-release 2>/dev/null || true)"
    [ -n "$REL" ] || { echo "ERROR: $ROOTFS carries no /etc/embraos-release" >&2; exit 1; }
    for pair in "ARCH=$ARCH" "ENGINE=$ENGINE" "VERSION=$VERSION"; do
        echo "$REL" | grep -qx "$pair" || { echo "ERROR: $ROOTFS says $(echo "$REL" | grep "^${pair%%=*}=" ), not $pair" >&2; exit 1; }
    done
    for f in boot/initramfs.cpio.gz boot/embraos-slot.cfg; do
        unsquashfs -ls "$ROOTFS" "$f" >/dev/null 2>&1 || { echo "ERROR: $ROOTFS lacks /$f" >&2; exit 1; }
    done
else
    echo "WARNING: unsquashfs is not installed; the slot image is not inspected" >&2
fi

mkdir -p "$OUT"
FILE="embraos-${TAG}-${ARCH}-${ENGINE}.rootfs.squashfs"
MANIFEST="$OUT/embraos-${ARCH}-${ENGINE}.manifest.json"
cp "$ROOTFS" "$OUT/$FILE"
SIZE="$(stat -c %s "$OUT/$FILE" 2>/dev/null || stat -f %z "$OUT/$FILE")"
SHA="$( (sha256sum "$OUT/$FILE" 2>/dev/null || shasum -a 256 "$OUT/$FILE") | cut -d' ' -f1)"
BUILT="$(date -u +%Y-%m-%dT%H:%M:%SZ)"
REPO_URL="https://github.com/Ward-Software-Defined-Systems/embraOS"
printf '{"format":"update.v1","version":"%s","tag":"%s","build_id":"%s","arch":"%s","engine":"%s","schema_version":%s,"kernel":"%s","rootfs":{"file":"%s","url":"%s/releases/download/%s/%s","size":%s,"sha256":"%s"},"built":"%s","notes_url":"%s/releases/tag/%s"}\n' \
    "$VERSION" "$TAG" "$BUILD_ID" "$ARCH" "$ENGINE" "$SCHEMA" "$KERNEL" "$FILE" "$REPO_URL" "$TAG" "$FILE" "$SIZE" "$SHA" "$BUILT" "$REPO_URL" "$TAG" > "$MANIFEST"
sign "$KEY" "$MANIFEST" > "$MANIFEST.sig"
verify scripts/update-signing.pub "$MANIFEST" "$MANIFEST.sig" || {
    echo "ERROR: the signature does not verify against scripts/update-signing.pub — is the key's public half in it?" >&2
    exit 1
}
# The installer ISO of the same build, x86_64 only: copied under the
# release's name, its sha256sum line signed with the same key. Its embedded
# manifest is checked against the flags when an xorriso is at hand (the
# host's, or Buildroot's under buildroot-src/output/host/bin).
ISO_UPLOADS=""
if [ "$ARCH" = x86_64 ]; then
    if [ -z "$ISO" ]; then
        for c in output/images/embraos.iso; do
            [ -f "$c" ] && { ISO="$c"; break; }
        done
    fi
    [ -n "$ISO" ] && [ -f "$ISO" ] || { echo "ERROR: no embraos.iso (build the image, or --iso <path>): the installer medium is a release asset" >&2; exit 1; }
    XORRISO="$(command -v xorriso || ls buildroot-src/output/host/bin/xorriso 2>/dev/null | head -n1 || true)"
    if [ -n "$XORRISO" ]; then
        ISO_TMP="$(mktemp -d)"
        "$XORRISO" -osirrox on -indev "$ISO" -extract /embraos/install.manifest "$ISO_TMP/manifest" >/dev/null 2>&1 || true
        ISO_MANIFEST="$(cat "$ISO_TMP/manifest" 2>/dev/null || true)"
        rm -rf "$ISO_TMP"
        [ -n "$ISO_MANIFEST" ] || { echo "ERROR: $ISO carries no /embraos/install.manifest" >&2; exit 1; }
        for pair in "ARCH=$ARCH" "ENGINE=$ENGINE" "VERSION=$VERSION"; do
            echo "$ISO_MANIFEST" | grep -qx "$pair" || { echo "ERROR: $ISO says $(echo "$ISO_MANIFEST" | grep "^${pair%%=*}="), not $pair" >&2; exit 1; }
        done
    else
        echo "WARNING: no xorriso; the ISO's embedded manifest is not inspected" >&2
    fi
    ISO_FILE="embraos-${TAG}-${ARCH}-${ENGINE}.iso"
    cp "$ISO" "$OUT/$ISO_FILE"
    (cd "$OUT" && sha256_line "$ISO_FILE" > "$ISO_FILE.sha256")
    sign "$KEY" "$OUT/$ISO_FILE.sha256" > "$OUT/$ISO_FILE.sha256.sig"
    verify scripts/update-signing.pub "$OUT/$ISO_FILE.sha256" "$OUT/$ISO_FILE.sha256.sig" || {
        echo "ERROR: the ISO's checksum signature does not verify against scripts/update-signing.pub" >&2
        exit 1
    }
    ISO_SIZE="$(stat -c %s "$OUT/$ISO_FILE" 2>/dev/null || stat -f %z "$OUT/$ISO_FILE")"
    ISO_UPLOADS=" \"$OUT/$ISO_FILE\" \"$OUT/$ISO_FILE.sha256\" \"$OUT/$ISO_FILE.sha256.sig\""
fi

echo "written to $OUT:"
echo "  $FILE  ($SIZE bytes, sha256 $SHA)"
echo "  $(basename "$MANIFEST")  (schema $SCHEMA, kernel $KERNEL, build $BUILD_ID)"
echo "  $(basename "$MANIFEST").sig"
if [ -n "$ISO_UPLOADS" ]; then
    echo "  $ISO_FILE  ($ISO_SIZE bytes; the installer medium)"
    echo "  $ISO_FILE.sha256  and  $ISO_FILE.sha256.sig  (the checksum line and its signature)"
fi
echo ""
echo "Upload to the release (the operator's step):"
echo "  gh release upload $TAG \"$OUT/$FILE\" \"$MANIFEST\" \"$MANIFEST.sig\"$ISO_UPLOADS --clobber"
