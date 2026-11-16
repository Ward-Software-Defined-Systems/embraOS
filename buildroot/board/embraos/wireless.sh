#!/bin/bash
# wireless.sh: what the build does for Wi-Fi, as functions. post_build.sh
# sources it and calls them on the target directory; image-common.sh sources
# it for the check of the modules. Run by itself, it calls one function:
#
#   wireless.sh <function> <arguments>
#
# The wireless drivers are the only kernel modules this image loads
# (linux.fragment says why). embrad loads the one a card asks for, from a
# list of the wireless drivers' device patterns that is written here, and the
# driver then asks for its firmware, which is completed here: Buildroot's
# linux-firmware package installs the files its own lists name, and the lists
# are behind the drivers (no ath11k at all, a third of iwlwifi missing).
#
# Every function reads the tree it is given and prints to standard output;
# none of them depends on the build host's own modinfo or kmod.

# The directories of a module tree that hold the 802.11 stack and its drivers.
EMBRAOS_WIRELESS_ROOTS="kernel/drivers/net/wireless/ kernel/net/wireless/ kernel/net/mac80211/"

# The linux-firmware directories copied whole: the drivers behind them ask
# for a range of versions (iwlwifi), for names made from the board (ath10k,
# ath11k, ath12k, brcmfmac with cypress) or for one of several revisions
# (the Realtek families), so no list of names taken from the modules is the
# set a card will ask for.
EMBRAOS_WIRELESS_FIRMWARE_DIRS="intel/iwlwifi ath9k_htc ath10k ath11k ath12k rtlwifi rtw88 rtw89 brcm cypress"

# A firmware name a module declares that is not the name its driver asks the
# kernel for, as `declared=asked`. rsi (a driver of the arm64 base
# configuration) declares its file without a directory and asks for it
# inside rsi/ (rsi_91x_hal.c), where linux-firmware has it. Without this the
# table below would report the driver as having none of its firmware and
# stop the aarch64 build. One pair a driver that is known to do so: a bare
# name that merely happens to be in some directory is not served.
EMBRAOS_FIRMWARE_ASKED_AS="rs9113_wlan_qspi.rps=rsi/rs9113_wlan_qspi.rps"

# embraos_wireless_modules <module tree>: the wireless drivers' modules by
# the names modules.alias gives them (underscores), one a line.
embraos_wireless_modules() {
    local moddir="${1%/}"
    (cd "$moddir" && find kernel/drivers/net/wireless -name '*.ko' 2>/dev/null) \
        | sed -e 's|.*/||' -e 's|\.ko$||' -e 's|-|_|g' | LC_ALL=C sort -u
}

# embraos_wireless_alias <module tree> <file>: the lines of modules.alias
# whose module is a wireless driver. embrad matches a device's modalias
# against these patterns and loads nothing that is not named here.
embraos_wireless_alias() {
    local moddir="${1%/}" out="$2" names
    # On one line: a line break inside an awk -v value is not portable.
    names="$(embraos_wireless_modules "$moddir" | tr '\n' ' ')"
    mkdir -p "$(dirname "$out")"
    if [ -z "${names// /}" ] || [ ! -f "$moddir/modules.alias" ]; then
        : > "$out"
        return 0
    fi
    awk -v names="$names" '
        BEGIN { n = split(names, list, " "); for (i = 1; i <= n; i++) want[list[i]] = 1 }
        $1 == "alias" && NF == 3 && ($3 in want)
    ' "$moddir/modules.alias" > "$out"
}

# embraos_modules_outside_wireless <module tree>: the modules that are
# neither part of the 802.11 stack and its drivers nor something one of
# them depends on (modules.dep, followed to its end), one a line. Nothing on
# a tree that carries wireless modules alone.
embraos_modules_outside_wireless() {
    local moddir="${1%/}"
    [ -f "$moddir/modules.dep" ] || return 0
    awk -v roots="$EMBRAOS_WIRELESS_ROOTS" '
        BEGIN { n = split(roots, root, " ") }
        {
            mod = $1; sub(/:$/, "", mod)
            all[mod] = 1
            deps[mod] = ""
            for (i = 2; i <= NF; i++) deps[mod] = deps[mod] " " $i
            for (i = 1; i <= n; i++) if (index(mod, root[i]) == 1) keep[mod] = 1
        }
        END {
            changed = 1
            while (changed) {
                changed = 0
                for (m in all) {
                    if (!(m in keep)) continue
                    k = split(deps[m], list, " ")
                    for (i = 1; i <= k; i++) if (!(list[i] in keep)) { keep[list[i]] = 1; changed = 1 }
                }
            }
            for (m in all) if (!(m in keep)) print m
        }
    ' "$moddir/modules.dep" | LC_ALL=C sort
}

# embraos_module_firmware <module file>: the firmware names a module
# declares (the `firmware=` entries of its .modinfo section), one a line.
embraos_module_firmware() {
    tr '\0' '\n' < "$1" | sed -n 's/^firmware=//p' | LC_ALL=C sort -u
}

# embraos_wireless_firmware <linux-firmware tree> <lib/firmware of the
# root filesystem> <module tree>: the Wi-Fi firmware Buildroot's package
# leaves out. The directories of EMBRAOS_WIRELESS_FIRMWARE_DIRS whole; every
# file a wireless module names that the tree has; then the tree's own links
# (WHENCE, `Link: name -> target`) for every target that is there now, the
# way Buildroot makes them inside its install step: iwlwifi asks for names
# at the top that are links into intel/iwlwifi/.
embraos_wireless_firmware() {
    local src="${1%/}" fw="${2%/}" moddir="${3%/}" d ko name link target
    [ -f "$src/WHENCE" ] || { echo "ERROR: $src is not a linux-firmware tree (no WHENCE)" >&2; return 1; }
    for d in $EMBRAOS_WIRELESS_FIRMWARE_DIRS; do
        [ -d "$src/$d" ] || continue
        mkdir -p "$fw/$d"
        cp -a "$src/$d/." "$fw/$d/"
    done
    (cd "$moddir" && find kernel/drivers/net/wireless -name '*.ko' 2>/dev/null) | while read -r ko; do
        embraos_module_firmware "$moddir/$ko"
    done | LC_ALL=C sort -u | while read -r name; do
        case "$name" in *'*'*|*'?'*|"") continue ;; esac
        if [ -f "$src/$name" ] && [ ! -e "$fw/$name" ]; then
            mkdir -p "$fw/$(dirname "$name")"
            cp -a "$src/$name" "$fw/$name"
        fi
    done
    sed -n -E 's/^Link: (.+) -> (.+)$/\1 \2/p' "$src/WHENCE" | while read -r link target; do
        [ -e "$fw/$link" ] && continue
        if [ -f "$(readlink -m "$fw/$(dirname "$link")/$target")" ]; then
            mkdir -p "$fw/$(dirname "$link")"
            ln -sf "$target" "$fw/$link"
        fi
    done
}

# embraos_firmware_name_present <lib/firmware> <name>: whether a declared
# name is served. A name with a pattern is a glob. An iwlwifi name carries
# the newest version the driver knows, which may be ahead of linux-firmware:
# the driver then asks for the versions below it, so any version of that
# device's file serves.
embraos_firmware_name_present() {
    local fw="${1%/}" name="$2" stem pair
    for pair in $EMBRAOS_FIRMWARE_ASKED_AS; do
        [ "${pair%%=*}" = "$name" ] && name="${pair#*=}"
    done
    case "$name" in
        *'*'*|*'?'*)
            compgen -G "$fw/$name" > /dev/null && return 0
            return 1
            ;;
    esac
    [ -e "$fw/$name" ] && return 0
    case "$name" in
        iwlwifi-*.ucode|intel/iwlwifi/iwlwifi-*.ucode)
            stem="${name%.ucode}"; stem="${stem%-*}"
            compgen -G "$fw/$stem-*.ucode" > /dev/null && return 0
            compgen -G "$fw/intel/iwlwifi/${stem##*/}-*.ucode" > /dev/null && return 0
            ;;
    esac
    return 1
}

# embraos_wireless_firmware_table <lib/firmware> <module tree>: a line a
# wireless module that declares firmware, `<module> <served>/<declared>`,
# and at its end the modules none of whose names is served. A module that
# declares none (ath12k) has no line. Returns 1 when a module is on that
# last list: its cards would be found, their driver loaded, and nothing
# would come up.
embraos_wireless_firmware_table() {
    local fw="${1%/}" moddir="${2%/}" ko names name served declared none=""
    while read -r ko; do
        names="$(embraos_module_firmware "$moddir/$ko")"
        [ -n "$names" ] || continue
        served=0; declared=0
        while read -r name; do
            declared=$((declared + 1))
            embraos_firmware_name_present "$fw" "$name" && served=$((served + 1))
        done <<< "$names"
        name="${ko##*/}"; name="${name%.ko}"
        echo "$name $served/$declared"
        [ "$served" -gt 0 ] || none="$none $name"
    done < <(cd "$moddir" && find kernel/drivers/net/wireless -name '*.ko' 2>/dev/null | LC_ALL=C sort)
    if [ -n "$none" ]; then
        echo "no firmware for:$none"
        return 1
    fi
    return 0
}

# Run by itself: one function and its arguments.
if [ "${BASH_SOURCE[0]}" = "$0" ] && [ $# -ge 1 ]; then
    set -euo pipefail
    fn="$1"; shift
    case "$fn" in
        embraos_wireless_modules|embraos_wireless_alias|embraos_modules_outside_wireless|embraos_module_firmware|embraos_wireless_firmware|embraos_firmware_name_present|embraos_wireless_firmware_table) "$fn" "$@" ;;
        *) echo "wireless.sh: no function $fn" >&2; exit 2 ;;
    esac
fi
