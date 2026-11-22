#!/bin/bash
# Post-build: prepare rootfs before SquashFS creation

# A failed step stops the build, and an unset or empty target directory
# stops it here: every `rm -rf` below is rooted on it (#33).
set -euo pipefail

BOARD_DIR="$(dirname "$0")"
TARGET_DIR="${1:?post_build.sh needs the target directory as its argument}"
# The Rust target triple (BR2_ROOTFS_POST_SCRIPT_ARGS in both defconfigs): the
# release stamp is read from beside the binaries Buildroot picks up, so it
# describes the build that produced them, under --buildroot-only as well.
RUST_TARGET="${2:-}"
[ -n "$RUST_TARGET" ] || {
    echo "ERROR: post_build.sh needs the Rust target as its second argument (BR2_ROOTFS_POST_SCRIPT_ARGS)" >&2
    exit 1
}
REPO_ROOT="${BOARD_DIR}/../../.."

# Remove unnecessary files for minimal rootfs
rm -rf "${TARGET_DIR}/usr/share/man"
rm -rf "${TARGET_DIR}/usr/share/doc"

# Create mount points
mkdir -p "${TARGET_DIR}/embra/state"
mkdir -p "${TARGET_DIR}/embra/data"
mkdir -p "${TARGET_DIR}/embra/ephemeral"
mkdir -p "${TARGET_DIR}/embra/boot"
mkdir -p "${TARGET_DIR}/mnt/initramfs"
mkdir -p "${TARGET_DIR}/mnt/root"
mkdir -p "${TARGET_DIR}/tmp"
mkdir -p "${TARGET_DIR}/run"
mkdir -p "${TARGET_DIR}/dev"
mkdir -p "${TARGET_DIR}/proc"
mkdir -p "${TARGET_DIR}/sys"

# Each OS slot carries its own kernel and initramfs under /boot: the kernel
# is installed by Buildroot (BR2_LINUX_KERNEL_INSTALL_TARGET; bzImage on
# x86_64, Image on aarch64, never named here), the initramfs is the one
# scripts/create_initramfs.sh wrote in the repository root. GRUB loads both
# from the slot's squashfs.
KERNELS=0
for k in bzImage Image; do
    [ -f "${TARGET_DIR}/boot/${k}" ] && KERNELS=$((KERNELS + 1))
done
[ "$KERNELS" -eq 1 ] || {
    echo "ERROR: expected exactly one kernel under ${TARGET_DIR}/boot (BR2_LINUX_KERNEL_INSTALL_TARGET), found ${KERNELS}" >&2
    exit 1
}
[ -f "${REPO_ROOT}/initramfs.cpio.gz" ] || {
    echo "ERROR: ${REPO_ROOT}/initramfs.cpio.gz is missing — run scripts/create_initramfs.sh" >&2
    exit 1
}
install -m 0644 "${REPO_ROOT}/initramfs.cpio.gz" "${TARGET_DIR}/boot/initramfs.cpio.gz"

# The firmware's licences beside the blobs: several vendors require the text
# with the binaries, and Buildroot installs none of it. WHENCE names every
# file's licence; the LICENCE/LICENSE files are the texts. From the
# package's source tree, which Buildroot hands over as BUILD_DIR.
FW_SRC="$(ls -d "${BUILD_DIR}"/linux-firmware-*/ 2>/dev/null | head -n1)"
[ -n "$FW_SRC" ] && [ -f "${FW_SRC}/WHENCE" ] || {
    echo "ERROR: linux-firmware's source tree is not under ${BUILD_DIR} (BR2_PACKAGE_EMBRA_FIRMWARE_ALL selects it); its licences cannot be installed" >&2
    exit 1
}
LIC="${TARGET_DIR}/usr/share/licenses/linux-firmware"
rm -rf "$LIC"; mkdir -p "$LIC"
cp "${FW_SRC}/WHENCE" "$LIC/"
find "$FW_SRC" -maxdepth 1 \( -name 'LICEN[CS]E*' -o -name 'GPL-*' -o -name 'COPYING' -o -name 'LICENSE' \) -type f -exec cp {} "$LIC/" \;
echo "post_build.sh: $(ls "$LIC" | wc -l) licence files of linux-firmware under /usr/share/licenses/linux-firmware"

# The probe-time firmware initrd: every file the built-in drivers that ask
# at probe name (modules.builtin.modinfo, `<driver>.firmware=<file>`), as a
# gzip'd newc archive GRUB loads beside the initramfs, so a driver that
# probes before the rootfs is mounted finds its blob in the initramfs's
# /lib/firmware. The open-time families (the NICs' blobs at ifup) stay on
# the rootfs alone. The set is the drivers whose probe wants the blob:
# i915 (GuC, HuC, DMC), amdgpu and radeon (a Radeon's own blobs: either
# driver takes the display over from the firmware's framebuffer and does
# not come up without them), the E800's DDP package, qed, cxgb4. The image
# build reads this list from the line below and refuses an archive that
# lacks a blob of theirs the rootfs holds (assert_probe_firmware_archived
# in scripts/lib/image-common.sh): keep the line's form.
MODINFO="$(ls "${TARGET_DIR}"/lib/modules/*/modules.builtin.modinfo | head -n1)"
FW_STAGE="$(mktemp -d)"
PROBE_DRIVERS="i915 amdgpu radeon ice qed cxgb4"
for drv in $PROBE_DRIVERS; do
    tr '\0' '\n' < "$MODINFO" | sed -n "s/^${drv}\.firmware=//p" | sort -u | while read -r name; do
        for cand in "$name" "$name.xz" "$name.zst"; do
            if [ -f "${TARGET_DIR}/lib/firmware/$cand" ]; then
                mkdir -p "${FW_STAGE}/lib/firmware/$(dirname "$cand")"
                cp "${TARGET_DIR}/lib/firmware/$cand" "${FW_STAGE}/lib/firmware/$cand"
                break
            fi
        done
    done
done
# The directories are members too, each before what it holds (the sort puts a
# path before the paths under it): the kernel's cpio unpacker creates no parent
# directory and skips a file whose directory is not there yet without a word,
# so an archive of files alone unpacks to nothing. assert_boot_initrds checks
# the order.
(cd "$FW_STAGE" && find lib | LC_ALL=C sort | cpio -o -H newc --quiet 2>/dev/null | gzip -9 > "${TARGET_DIR}/boot/firmware.cpio.gz")
echo "post_build.sh: /boot/firmware.cpio.gz holds $(find "$FW_STAGE" -type f | wc -l) probe-time files of ${PROBE_DRIVERS// /, } ($(stat -c %s "${TARGET_DIR}/boot/firmware.cpio.gz") bytes)"
rm -rf "$FW_STAGE"

# The early microcode initrd: the kernel's early loader reads the first
# initrd, uncompressed newc, for kernel/x86/microcode/{GenuineIntel,
# AuthenticAMD}.bin, each the concatenation of the vendor's files. Intel's
# come from the bundle package/embra-intel-microcode pins: its recipe writes
# them into the images directory (BINARIES_DIR), the directory whole, and
# keeps them off the rootfs; AMD's from linux-firmware's amd-ucode family. On
# another architecture the archive is empty, so GRUB's line is the same
# everywhere.
UC_STAGE="$(mktemp -d)"
case "$RUST_TARGET" in
    x86_64-*)
        mkdir -p "${UC_STAGE}/kernel/x86/microcode"
        [ -d "${BINARIES_DIR}/intel-ucode" ] || { echo "ERROR: ${BINARIES_DIR}/intel-ucode is missing (BR2_PACKAGE_EMBRA_INTEL_MICROCODE)" >&2; exit 1; }
        # Intel's licence permits the binary redistribution on the condition
        # that its notice travels with the distribution: the recipe installs
        # the text beside the firmware's licences, from the bundle the files
        # come from, and a rootfs without it stops here.
        [ -s "${TARGET_DIR}/usr/share/licenses/intel-microcode/license" ] || { echo "ERROR: /usr/share/licenses/intel-microcode/license is missing from the target (the embra-intel-microcode recipe installs it)" >&2; exit 1; }
        cat "${BINARIES_DIR}"/intel-ucode/* > "${UC_STAGE}/kernel/x86/microcode/GenuineIntel.bin"
        ls "${TARGET_DIR}"/lib/firmware/amd-ucode/microcode_amd*.bin > /dev/null 2>&1 || { echo "ERROR: /lib/firmware/amd-ucode/microcode_amd*.bin is missing (linux-firmware's amd-ucode family)" >&2; exit 1; }
        cat "${TARGET_DIR}"/lib/firmware/amd-ucode/microcode_amd*.bin > "${UC_STAGE}/kernel/x86/microcode/AuthenticAMD.bin"
        (cd "$UC_STAGE" && printf 'kernel\nkernel/x86\nkernel/x86/microcode\nkernel/x86/microcode/GenuineIntel.bin\nkernel/x86/microcode/AuthenticAMD.bin\n' | cpio -o -H newc --quiet > "${TARGET_DIR}/boot/ucode.cpio")
        ;;
    *)
        (cd "$UC_STAGE" && printf '' | cpio -o -H newc --quiet > "${TARGET_DIR}/boot/ucode.cpio")
        ;;
esac
echo "post_build.sh: /boot/ucode.cpio written ($(stat -c %s "${TARGET_DIR}/boot/ucode.cpio") bytes)"
rm -rf "$UC_STAGE"

# The release stamp (scripts/lib/image-common.sh::write_release_file): what
# this slot is, read by embrad, embra-update and the status surfaces.
RELEASE_FILE="${REPO_ROOT}/target/${RUST_TARGET}/release/embraos-release"
[ -f "$RELEASE_FILE" ] || {
    echo "ERROR: ${RELEASE_FILE} is missing — a full build writes it beside the binaries" >&2
    exit 1
}
mkdir -p "${TARGET_DIR}/etc"
install -m 0644 "$RELEASE_FILE" "${TARGET_DIR}/etc/embraos-release"

# What GRUB shows for this slot in its menu: one `set` line it can `source`
# from the slot's squashfs. A value that could break out of the quotes is
# refused rather than escaped.
VERSION=$(sed -n 's/^VERSION=//p' "$RELEASE_FILE" | head -n1)
BUILD_ID=$(sed -n 's/^BUILD_ID=//p' "$RELEASE_FILE" | head -n1)
BUILT=$(sed -n 's/^BUILT=//p' "$RELEASE_FILE" | head -n1)
for v in "$VERSION" "$BUILD_ID" "$BUILT"; do
    [ -n "$v" ] || { echo "ERROR: ${RELEASE_FILE} lacks VERSION, BUILD_ID or BUILT" >&2; exit 1; }
    case "$v" in
        *['"$\\']*|*$'\n'*)
            echo "ERROR: a release value carries a quote, a dollar, a backslash or a newline: ${v}" >&2
            exit 1 ;;
    esac
done
printf 'set slot_version="%s (%s, %s)"\n' "$VERSION" "$BUILD_ID" "${BUILT%%T*}" \
    > "${TARGET_DIR}/boot/embraos-slot.cfg"

# The release signing keys the updater trusts (scripts/update-signing.pub;
# the operator's own go to STATE's update-signing.pub). The file may hold
# no key yet, and then no manifest verifies.
install -D -m 0644 "${REPO_ROOT}/scripts/update-signing.pub" "${TARGET_DIR}/usr/share/embra/update-signing.pub"

# The resolver: /etc/resolv.conf on the read-only root is a link into the
# ephemeral tmpfs, where embrad writes the resolvers of the card with the
# best route (crates/embrad/src/net.rs), from the records the udhcpc lease
# script keeps for each card (rootfs_overlay/usr/share/embra/udhcpc.script).
rm -f "${TARGET_DIR}/etc/resolv.conf"
ln -s /embra/ephemeral/resolv.conf "${TARGET_DIR}/etc/resolv.conf"

# Wi-Fi (wireless.sh has the functions and the reasons). The wireless
# drivers are modules, and embrad loads the one a card asks for from this
# list: the lines of modules.alias whose module is a wireless driver, and
# no other. Their firmware: Buildroot's package installs what its own lists
# name, which leaves out ath11k whole and a third of iwlwifi; the rest comes
# from the same linux-firmware tree, with the tree's links. The table that
# follows fails the build on a driver none of whose firmware names is
# served: its card would be found and never come up.
. "${BOARD_DIR}/wireless.sh"
WL_MODDIR="$(ls -d "${TARGET_DIR}"/lib/modules/*/ 2>/dev/null | head -n1)"
[ -n "$WL_MODDIR" ] || { echo "ERROR: no module tree under ${TARGET_DIR}/lib/modules" >&2; exit 1; }
WL_ALIAS="${TARGET_DIR}/usr/share/embra/wireless.alias"
embraos_wireless_alias "$WL_MODDIR" "$WL_ALIAS"
[ -s "$WL_ALIAS" ] || {
    echo "ERROR: /usr/share/embra/wireless.alias is empty: the kernel built no wireless driver as a module (board/embraos/linux.fragment, the Wi-Fi block)" >&2
    exit 1
}
embraos_wireless_firmware "$FW_SRC" "${TARGET_DIR}/lib/firmware" "$WL_MODDIR"
WL_TABLE="$(embraos_wireless_firmware_table "${TARGET_DIR}/lib/firmware" "$WL_MODDIR")" || {
    echo "$WL_TABLE" | tail -n1 >&2
    echo "ERROR: a wireless driver has none of the firmware it names under /lib/firmware (the line above; linux-firmware's tree is ${FW_SRC})" >&2
    exit 1
}
echo "post_build.sh: Wi-Fi: $(wc -l < "$WL_ALIAS") device patterns of $(awk '{print $3}' "$WL_ALIAS" | LC_ALL=C sort -u | wc -l) drivers in /usr/share/embra/wireless.alias; firmware is served for each of the $(echo "$WL_TABLE" | wc -l) modules that name any"
# The supplicant, the regulatory database and the mode-switch tool are
# packages of both defconfigs.
for f in usr/sbin/wpa_supplicant usr/sbin/usb_modeswitch lib/firmware/regulatory.db lib/firmware/regulatory.db.p7s; do
    [ -e "${TARGET_DIR}/$f" ] || {
        echo "ERROR: /$f is missing from the root filesystem (BR2_PACKAGE_WPA_SUPPLICANT, BR2_PACKAGE_USB_MODESWITCH or BR2_PACKAGE_WIRELESS_REGDB in the defconfig)" >&2
        exit 1
    }
done
[ -d "${TARGET_DIR}/usr/share/usb_modeswitch" ] || { echo "ERROR: /usr/share/usb_modeswitch is missing (BR2_PACKAGE_USB_MODESWITCH_DATA)" >&2; exit 1; }
# What the two recipes install beside the programs, and nothing here uses.
# The supplicant's example configuration joins any open network; embrad
# renders the configuration it runs with (crates/embra-boot/src/wifi.rs),
# and no file of that name may be there for a supplicant started by hand to
# find. Its two ifupdown hooks have no ifupdown to call them. The
# mode-switch tool's udev rule, its dispatcher and the Tcl shell the
# dispatcher is written for: embrad calls the tool itself, with the data
# file of the device (crates/embrad/src/wifi.rs), whether udev runs or not.
rm -f "${TARGET_DIR}/etc/wpa_supplicant.conf" \
      "${TARGET_DIR}/etc/network/if-up.d/wpasupplicant" "${TARGET_DIR}/etc/network/if-down.d/wpasupplicant" \
      "${TARGET_DIR}/lib/udev/rules.d/40-usb_modeswitch.rules" "${TARGET_DIR}/lib/udev/usb_modeswitch" \
      "${TARGET_DIR}/usr/sbin/usb_modeswitch_dispatcher" "${TARGET_DIR}/etc/usb_modeswitch.conf" \
      "${TARGET_DIR}/usr/bin/jimsh"
rm -f "${TARGET_DIR}"/usr/lib/libjim.so*
rm -rf "${TARGET_DIR}/etc/usb_modeswitch.d" "${TARGET_DIR}/var/lib/usb_modeswitch"

# The machine's name: /etc/hostname and /etc/hosts are links into the
# ephemeral tmpfs as well. embrad names the machine (the operator's name,
# the one a DHCP server assigned, or embraos: crates/embrad/src/net.rs),
# sets the kernel's hostname and writes both files before any service
# starts. Buildroot's own two files say "buildroot" and nothing reads them.
rm -f "${TARGET_DIR}/etc/hostname" "${TARGET_DIR}/etc/hosts"
ln -s /embra/ephemeral/hostname "${TARGET_DIR}/etc/hostname"
ln -s /embra/ephemeral/hosts "${TARGET_DIR}/etc/hosts"
[ -x "${TARGET_DIR}/usr/share/embra/udhcpc.script" ] \
    || { echo "ERROR: the overlay's /usr/share/embra/udhcpc.script is missing or not executable" >&2; exit 1; }

# Workspace mount point (embrad bind-mounts /embra/data/workspace here at boot)
mkdir -p "${TARGET_DIR}/embra/workspace"

# Importable intelligence graphs (kg-native-identity): bake the repo's
# Imported_Intelligence/*.graph.json examples read-only into the rootfs so
# every fresh boot can offer the Learning-Mode import with zero
# provisioning. STATE's imported-intelligence/ wins filename collisions.
# The folder's README.md is host-side authoring documentation — .graph.json
# only, by construction.
if ls "${REPO_ROOT}/Imported_Intelligence"/*.graph.json >/dev/null 2>&1; then
    mkdir -p "${TARGET_DIR}/usr/share/embra/imported-intelligence"
    cp "${REPO_ROOT}/Imported_Intelligence"/*.graph.json \
       "${TARGET_DIR}/usr/share/embra/imported-intelligence/"
fi

# The builder's own certificate authorities: bake the *.pem and *.crt files
# of the repo's Imported_CAs/ read-only into the rootfs, where embrad and the
# brain trust them beside STATE's ca-certificates/. A machine installed from
# the ISO, a hypervisor's disk or bare metal has no seed-state.sh step, so a
# CA it needs travels in the image. The folder's contents are not in git.
# imported_cas_install checks the folder first (a private key, a file that
# is no PEM certificate and one the readers would skip each stop the build)
# and writes the directory whole; with Buildroot's own openssl every
# certificate is parsed.
# shellcheck source=scripts/lib/imported-cas.sh
source "${REPO_ROOT}/scripts/lib/imported-cas.sh"
IMPORTED_CAS_OPENSSL="${HOST_DIR:-}/bin/openssl"
[ -x "$IMPORTED_CAS_OPENSSL" ] || IMPORTED_CAS_OPENSSL="$(command -v openssl || true)"
imported_cas_install "${REPO_ROOT}" "${TARGET_DIR}" "$IMPORTED_CAS_OPENSSL"

# Seed knowledge packs (knowledge.v1): bake the repo's committed
# Seed_Knowledge/*.knowledge.json packs read-only into the rootfs — the
# brain's boot reconcile ensures their nodes/edges exist in the live KG on
# every boot. A pack the image ships loads from the image; a STATE copy of
# the same pack is skipped and named (STATE's seed-knowledge/ is for the
# operator's own packs; `knowledge/seed.rs::scan_dirs`). The folder's
# README.md is host-side authoring documentation.
if ls "${REPO_ROOT}/Seed_Knowledge"/*.knowledge.json >/dev/null 2>&1; then
    mkdir -p "${TARGET_DIR}/usr/share/embra/seed-knowledge"
    cp "${REPO_ROOT}/Seed_Knowledge"/*.knowledge.json \
       "${TARGET_DIR}/usr/share/embra/seed-knowledge/"
fi

# No SSH server in the image (security invariant, see the defconfig).
# With the server option off a fresh build tree never gets these files. A
# tree that was built while it was on still has them: Buildroot's target
# directory persists across builds, and an option that is switched off
# removes nothing. They go here, on every build. The client (ssh, scp, sftp,
# ssh-keygen, ssh-copy-id) and its configuration stay.
rm -f "${TARGET_DIR}/usr/sbin/sshd" \
      "${TARGET_DIR}/usr/libexec/sshd-session" \
      "${TARGET_DIR}/usr/libexec/sshd-auth" \
      "${TARGET_DIR}/usr/libexec/sftp-server" \
      "${TARGET_DIR}/etc/ssh/sshd_config"

# The records of the build that the root filesystem would carry and nothing
# reads at run time: the in-OS toolchain's install log and manifests, ICU's
# build tools and makefile fragments, libxslt's and libstdc++'s helpers. Each
# names the directories the build ran in (scripts/lib/build-paths.sh).
# shellcheck source=scripts/lib/build-paths.sh
source "${REPO_ROOT}/scripts/lib/build-paths.sh"
remove_build_records "${TARGET_DIR}"

# udev's files, for a build that carries it (eudev comes with the display
# console's input): its network-naming rule would rename a card after embrad
# has started a DHCP client on it under the kernel's name (crates/embrad/src/
# net.rs), and its init scripts are for SysV init, which this image has not.
# Removed on every build, whether or not eudev is built.
rm -f "${TARGET_DIR}/lib/udev/rules.d/80-net-name-slot.rules" \
      "${TARGET_DIR}/etc/init.d/S10udevd" \
      "${TARGET_DIR}/etc/init.d/S30dbus"
# OpenSSH's install leaves the server's moduli beside the client's config.
rm -f "${TARGET_DIR}/etc/ssh/moduli"

# Defense-in-depth: lock the root account.
# The Buildroot skeleton leaves /etc/shadow with an empty root password,
# which means anyone with shell access can become root without credentials.
# embraOS has no login paths today (nothing starts a getty, and there is no
# SSH server in the image), so an empty-password root is not currently
# exploitable —
# but file_read is unrestricted and `/etc/shadow` is readable, so agent
# compromise via prompt injection (flagged in Sprint 3 sweep #11) would hand
# over a useful credential for free. Locking it removes that value while
# leaving the account structure intact so future tooling (su, su-exec) can
# still reason about UID 0. See Embra_Debug #11.
if [ -f "${TARGET_DIR}/etc/shadow" ]; then
    sed -i 's/^root:[^:]*:/root:*:/' "${TARGET_DIR}/etc/shadow"
fi
