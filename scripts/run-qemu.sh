#!/bin/bash
# Launch embraOS in QEMU with hardware acceleration (auto-detected)

set -euo pipefail

# Find image — check buildroot output first (always freshest), then output/images
if [ -n "${1:-}" ]; then
    IMAGE="$1"
elif [ -f "buildroot-src/output/images/embraos.img" ]; then
    IMAGE="buildroot-src/output/images/embraos.img"
elif [ -f "output/images/embraos.img" ]; then
    IMAGE="output/images/embraos.img"
else
    echo "ERROR: No disk image found."
    echo "Build it first with: ./scripts/build-image.sh"
    exit 1
fi

# Find kernel and initramfs alongside the image
IMAGE_DIR="$(dirname "$IMAGE")"
KERNEL="${IMAGE_DIR}/bzImage"
if [ ! -f "$KERNEL" ]; then
    # Fall back to other known locations
    for k in buildroot-src/output/images/bzImage output/images/bzImage; do
        if [ -f "$k" ]; then KERNEL="$k"; break; fi
    done
fi

INITRD="initramfs.cpio.gz"
if [ ! -f "$INITRD" ]; then
    echo "ERROR: initramfs.cpio.gz not found. Run ./scripts/create_initramfs.sh first."
    exit 1
fi

if [ ! -f "$KERNEL" ]; then
    echo "ERROR: bzImage not found. Build with ./scripts/build-image.sh first."
    exit 1
fi

MEMORY="2G"
CPUS="2"

# Auto-detect best acceleration. `-cpu host` is added on bare-metal hosts
# only: passes through host CPU features instead of QEMU's qemu64 default
# (x86-64-v1, missing SSSE3/SSE4 that some recent userspace assumes).
#
# Inside a hypervisor (Parallels, VMware, Hyper-V, KVM-on-KVM) we deliberately
# skip `-cpu host` and let QEMU use qemu64 — the L0 hypervisor advertises CPU
# features through CPUID that it can't actually emulate when the L1 guest
# tries to use them, which causes hard lockups of the L1 VM (we hit this on
# Parallels: kernel decompressed, jumped, used a passthrough feature on
# first instruction, the entire Ubuntu VM froze).
ACCEL=""
ACCEL_NAME="none (TCG software emulation)"
if [ "$(uname)" = "Darwin" ]; then
    ACCEL="-accel hvf -cpu host"
    ACCEL_NAME="HVF (macOS)"
elif [ -e /dev/kvm ] && [ -r /dev/kvm ] && [ -w /dev/kvm ]; then
    ACCEL="-accel kvm"
    ACCEL_NAME="KVM (Linux)"
    if command -v systemd-detect-virt >/dev/null 2>&1 \
        && [ "$(systemd-detect-virt 2>/dev/null)" = "none" ]; then
        ACCEL="$ACCEL -cpu host"
        ACCEL_NAME="KVM (Linux, -cpu host)"
    fi
fi

# Parallels guests fall back to TCG when nested virtualization is off in the
# host VM settings — boot then takes 5-10 minutes (Mesa3D + LLVM JIT + cage)
# and looks like a kernel hang. Warn loudly so the operator knows what to fix.
if [ -z "$ACCEL" ] && command -v systemd-detect-virt >/dev/null 2>&1 \
    && [ "$(systemd-detect-virt 2>/dev/null)" = "parallels" ]; then
    cat >&2 <<'WARN'

WARNING: Running in Parallels Desktop without /dev/kvm — falling back to
         TCG software emulation. Boot will be 5-10x slower and usually
         looks like a kernel hang at the bootloader.

         Enable hardware acceleration:
           1. Shut down this Ubuntu VM (not suspend).
           2. Parallels Desktop -> Configure -> Hardware -> CPU & Memory
              -> "Enable nested virtualization" (Pro/Business edition).
           3. Boot the VM and re-run.

WARN
fi

# embra-desktop graphical session selection.
# When EMBRA_DESKTOP=1, swap the serial-only TUI launch for a GTK-windowed
# 1280x720 graphical session backed by virtio-gpu (Mesa llvmpipe path) and
# virtio keyboard + tablet input. The serial line is redirected to a file
# so embrad's stdio still has somewhere to land after cage takes
# /dev/tty1. Use Ctrl-Alt-G to release the QEMU pointer grab.
DESKTOP_MODE="${EMBRA_DESKTOP:-0}"

# Detect host terminal size and pass to guest via kernel cmdline
HOST_COLS=$(stty size 2>/dev/null | awk '{print $2}')
HOST_ROWS=$(stty size 2>/dev/null | awk '{print $1}')
HOST_COLS=${HOST_COLS:-80}
HOST_ROWS=${HOST_ROWS:-24}

if [ "$DESKTOP_MODE" = "1" ]; then
    # Display backend — `EMBRA_DISPLAY` overrides; default tries the
    # most-likely-to-work options in sequence based on what's available.
    # gtk needs an X11/Wayland session reachable from the QEMU process;
    # sdl is more universal; vnc works headless (connect with any VNC
    # client at localhost:5900).
    EMBRA_DISPLAY="${EMBRA_DISPLAY:-auto}"
    if [ "$EMBRA_DISPLAY" = "auto" ]; then
        if [ -n "${WAYLAND_DISPLAY:-}" ] || [ -n "${DISPLAY:-}" ]; then
            EMBRA_DISPLAY="sdl"
        else
            EMBRA_DISPLAY="vnc"
        fi
    fi
    case "$EMBRA_DISPLAY" in
        gtk)
            DISPLAY_ARGS=(-display gtk,gl=off)
            DISPLAY_DESC="GTK 1280x720 (virtio-gpu, llvmpipe)"
            ;;
        sdl)
            DISPLAY_ARGS=(-display sdl,gl=off)
            DISPLAY_DESC="SDL 1280x720 (virtio-gpu, llvmpipe)"
            ;;
        vnc)
            DISPLAY_ARGS=(-vnc :0)
            DISPLAY_DESC="VNC at localhost:5900 (connect with any VNC viewer)"
            ;;
        spice)
            DISPLAY_ARGS=(-display spice-app,gl=off)
            DISPLAY_DESC="SPICE (spice-client launches)"
            ;;
        *)
            echo "ERROR: unknown EMBRA_DISPLAY=$EMBRA_DISPLAY (use gtk|sdl|vnc|spice|auto)" >&2
            exit 2
            ;;
    esac
    # `-vga none` strips QEMU's default Cirrus/std VGA card. Without it,
    # the guest sees TWO display devices (VGA + virtio-gpu) → SDL/GTK
    # opens two windows, /dev/dri/card0 ends up bound to VGA, cage opens
    # the wrong DRM device, and the kernel framebuffer console keeps
    # writing to the virtio-gpu surface that cage never claims. Symptom
    # was a "QEMU" window with kernel printk + a "QEMU - Press Ctrl-Alt-G"
    # window with stale init-time text, plus repeating vblank-wait timeout
    # WARNINGs from drm_fb_helper_damage_work. With -vga none the only
    # display device is virtio-gpu → /dev/dri/card0 → cage takes DRM
    # master cleanly.
    DISPLAY_ARGS+=(
        -vga none
        -device virtio-gpu-pci,xres=1280,yres=720
        -device virtio-keyboard-pci
        -device virtio-tablet-pci
    )
    SERIAL_ARGS=(-serial "file:/tmp/embra-serial.log")
    # `embra.desktop=1` flips embrad's supervisor to spawn
    # `cage -- /usr/bin/embra-desktop` in place of the serial-TTY
    # embra-console. cage is a wlroots-based kiosk compositor that
    # owns /dev/tty1 + DRM + libinput; embra-desktop runs as its
    # only fullscreen Wayland client.
    #
    # Two `console=` entries: kernel printk goes to BOTH the VGA text
    # framebuffer (visible on VNC) AND the serial line (captured to
    # /tmp/embra-serial.log on host).
    #
    # Order matters: the LAST `console=` becomes /dev/console for
    # userspace output. `console=tty0 console=ttyS0` puts ttyS0 last so
    # embrad's tracing logs land in the host serial log file (where the
    # operator can `tail -f`), while kernel boot messages remain visible
    # on VNC for boot-up confirmation.
    #
    # `quiet` is intentionally OMITTED so failures during
    # kernel/initramfs/embrad boot are visible.
    #
    # `vt.handoff=1` tells the kernel framebuffer console to release the
    # CRTC/scanout when a userspace DRM master (cage) takes over, instead
    # of fighting back with damage-update workqueue items (which produced
    # the `[CRTC:37:crtc-0] vblank wait timed out` WARNINGs we saw).
    # `vt.global_cursor_default=0` hides the kernel's blinking text cursor
    # so it doesn't flicker through cage's surface during the handoff.
    KERNEL_CMDLINE="root=/dev/vda2 ro console=tty0 console=ttyS0 vt.handoff=1 vt.global_cursor_default=0 embra.desktop=1"
    SERIAL_DESC="/tmp/embra-serial.log"
else
    DISPLAY_ARGS=(-nographic)
    SERIAL_ARGS=(-serial mon:stdio)
    KERNEL_CMDLINE="console=ttyS0 root=/dev/vda2 ro quiet embra.cols=$HOST_COLS embra.rows=$HOST_ROWS"
    DISPLAY_DESC="serial only (-nographic)"
    SERIAL_DESC="this terminal"
fi

echo "Starting embraOS in QEMU..."
echo "  Image: $IMAGE"
echo "  Kernel: $KERNEL"
echo "  Initrd: $INITRD"
echo "  Memory: $MEMORY"
echo "  CPUs: $CPUS"
echo "  Acceleration: $ACCEL_NAME"
echo "  Display: $DISPLAY_DESC"
echo "  Serial console: $SERIAL_DESC"
echo "  Port forwards: 50000→50000 (gRPC), 8443→8443 (REST)"
echo ""
if [ "$DESKTOP_MODE" = "1" ]; then
    case "$EMBRA_DISPLAY" in
        vnc)
            echo "Connect with: vncviewer localhost:5900   (or any VNC client)"
            echo "Then send SIGINT to exit QEMU."
            ;;
        *)
            echo "Close the QEMU window or send SIGINT to exit."
            echo "Live boot progress: tail -F /tmp/embra-serial.log    (in another terminal)"
            ;;
    esac
else
    echo "Press Ctrl-A X to exit QEMU"
fi
echo ""

# Pre-flight: tight host memory swap-thrashes the guest before init runs;
# warn when MemAvailable is below the requested -m plus 1 GiB QEMU overhead.
# (Bit us once on a 3 GiB Parallels VM — kernel never made it past early init.)
if [ -r /proc/meminfo ]; then
    MEM_AVAIL_KB=$(awk '/^MemAvailable:/ {print $2}' /proc/meminfo)
    MEM_REQ_GIB=${MEMORY%G}
    MEM_NEED_KB=$(( (MEM_REQ_GIB + 1) * 1024 * 1024 ))
    if [ -n "$MEM_AVAIL_KB" ] && [ "$MEM_AVAIL_KB" -lt "$MEM_NEED_KB" ]; then
        echo "WARNING: only $((MEM_AVAIL_KB / 1024)) MiB available on host." >&2
        echo "         QEMU requests ${MEMORY} + ~1 GiB overhead; boot may stall in swap." >&2
        echo "" >&2
    fi
fi

qemu-system-x86_64 \
    $ACCEL \
    -m "$MEMORY" \
    -smp "$CPUS" \
    -audio none \
    -drive file="$IMAGE",format=raw,if=virtio \
    -kernel "$KERNEL" \
    -initrd "$INITRD" \
    -append "$KERNEL_CMDLINE" \
    "${DISPLAY_ARGS[@]}" \
    "${SERIAL_ARGS[@]}" \
    -nic user,hostfwd=tcp::50000-:50000,hostfwd=tcp::8443-:8443 \
    -no-reboot
