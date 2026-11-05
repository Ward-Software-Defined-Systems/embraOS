#!/usr/bin/env python3
"""Draw assets/system-architecture.svg and .png: the system after the Phase 2 core wave, from QEMU and the firmware up to the UIs.

The figure is plain SVG written by this script and rasterized at 2x with resvg
(cargo install resvg), so it renders on a build host without a browser. Run
from the repository root:

    python3 assets/system-architecture.py

Every port, path, flag and figure in the drawing is the one the code binds or
the scripts pass (docs/SYSTEM-DESIGN.md has the tables); the start order is
embrad's register_services. Fonts: DejaVu Sans / DejaVu Sans Mono, the banner's.
"""
import os
import subprocess

OUT = os.path.dirname(os.path.abspath(__file__))
W, H = 1200, 1104
SCALE = 2

# ---------------------------------------------------------------- palette --
BG = "#0a0a0f"
BAND_FILL, BAND_EDGE = "rgba(255,255,255,0.025)", "rgba(255,255,255,0.07)"
BOX = "#14131a"
TEXT, DETAIL, DIM = "#e6e4df", "#aaa8b3", "#6a6a7a"
ARROW = "#8d8a96"
UI, WEB, APID, BRAIN, GUARD = "#ff8c28", "#f59e0b", "#d4a056", "#ef4444", "#c2410c"
DB, TRUST, SLAB, KERNEL, QEMU = "#b8860b", "#a06432", "#9a7a5a", "#6a6a7a", "#5a5a6a"
SANS = "DejaVu Sans, sans-serif"
MONO = "DejaVu Sans Mono, monospace"

parts = []
def emit(s): parts.append(s)

def esc(s):
    return s.replace("&", "&amp;").replace("<", "&lt;").replace(">", "&gt;")

def text(x, y, s, size=10.2, fill=DETAIL, weight="normal", anchor="start", family=SANS, spacing=None, opacity=1.0):
    ls = f' letter-spacing="{spacing}"' if spacing else ""
    op = f' opacity="{opacity}"' if opacity != 1.0 else ""
    emit(f'<text x="{x}" y="{y}" font-family="{family}" font-size="{size}" font-weight="{weight}" fill="{fill}" text-anchor="{anchor}"{ls}{op}>{esc(s)}</text>')

def rect(x, y, w, h, fill, stroke, rx=8, sw=1.2, dash=None, opacity=1.0):
    d = f' stroke-dasharray="{dash}"' if dash else ""
    op = f' opacity="{opacity}"' if opacity != 1.0 else ""
    emit(f'<rect x="{x}" y="{y}" width="{w}" height="{h}" rx="{rx}" fill="{fill}" stroke="{stroke}" stroke-width="{sw}"{d}{op}/>')

def band(x, y, w, h, label):
    rect(x, y, w, h, BAND_FILL, BAND_EDGE, rx=12, sw=1)
    text(x + 14, y + 17, label.upper(), size=9.2, fill=DIM, spacing=1.6)

def badge(x, y, n, color):
    emit(f'<circle cx="{x}" cy="{y}" r="7.5" fill="{BG}" stroke="{color}" stroke-width="1.2"/>')
    text(x, y + 3.4, str(n), size=9, fill=color, weight="bold", anchor="middle")

def box(x, y, w, h, title, lines, color, order=None, port=None, dash=None, mono_title=False, size=10.2, lead=12.2):
    rect(x, y, w, h, BOX, color, dash=dash)
    tx = x + 12
    if order is not None:
        badge(x + 14, y + 15, order, color)
        tx = x + 28
    text(tx, y + 19, title, size=12.5, fill=TEXT, weight="bold")
    if port:
        text(x + w - 12, y + 19, port, size=10.4, fill=color, anchor="end", family=MONO)
    for i, line in enumerate(lines):
        text(x + 12, y + 35 + i * lead, line, size=size)

def arrow(points, label=None, label_at=None, dash=None, anchor="start", label_dy=-4, label2=None):
    """points: list of (x, y); the head sits on the last point. label_at: (x, y) of the text."""
    d = f' stroke-dasharray="{dash}"' if dash else ""
    pts = " ".join(f"{x},{y}" for x, y in points)
    emit(f'<polyline points="{pts}" fill="none" stroke="{ARROW}" stroke-width="1.3" marker-end="url(#head)"{d}/>')
    if label and label_at:
        lx, ly = label_at
        text(lx, ly + label_dy, label, size=9.6, fill="#bdbac4", anchor=anchor)
        if label2:
            text(lx, ly + label_dy + 11.5, label2, size=9.6, fill="#bdbac4", anchor=anchor)

# ------------------------------------------------------------------ canvas --
emit(f'<svg xmlns="http://www.w3.org/2000/svg" viewBox="0 0 {W} {H}" width="{W}" height="{H}" role="img" aria-label="embraOS system architecture: QEMU, the UEFI firmware and GRUB with the two OS slots, the Linux kernel and embra-init, embrad as PID 1, the six supervised services, and the web and serial UIs on the host side">')
emit('<defs>'
     '<radialGradient id="glow" cx="50%" cy="0%" r="70%"><stop offset="0" stop-color="#2a1a10" stop-opacity="0.9"/><stop offset="1" stop-color="#0a0a0f" stop-opacity="0"/></radialGradient>'
     f'<marker id="head" viewBox="0 0 10 10" refX="9" refY="5" markerWidth="7" markerHeight="7" orient="auto-start-reverse"><path d="M 0 0 L 10 5 L 0 10 z" fill="{ARROW}"/></marker>'
     '</defs>')
emit(f'<rect width="{W}" height="{H}" fill="{BG}"/>')
emit(f'<rect width="{W}" height="{H}" fill="url(#glow)"/>')

# ------------------------------------------------------------------- title --
text(40, 32, "embraOS — system architecture", size=15, fill="#d4a056", weight="bold", spacing=0.5)
text(356, 32, "Phase 2 · one Buildroot tree builds x86_64 and aarch64", size=10.5, fill=DIM)
# legend
lx = 744
emit(f'<line x1="{lx}" y1="28" x2="{lx+26}" y2="28" stroke="{ARROW}" stroke-width="1.3" marker-end="url(#head)"/>')
text(lx + 32, 32, "gRPC · REST · PTY inside the OS", size=9.4, fill=DIM)
lx2 = 946
emit(f'<line x1="{lx2}" y1="28" x2="{lx2+26}" y2="28" stroke="{ARROW}" stroke-width="1.3" stroke-dasharray="2 3" marker-end="url(#head)"/>')
text(lx2 + 32, 32, "outbound HTTPS", size=9.4, fill=DIM)
badge(1090, 28, 6, DIM)
text(1104, 32, "start order", size=9.4, fill=DIM)

# ------------------------------------------------------------------ UI band --
band(40, 50, 1120, 100, "UIs · the host side")
box(60, 78, 200, 64, "Web console · desktop", ["xterm.js over /ws/terminal", "sidebar, activity strip, Tools tab", "Leptos/WASM app at /embraOS"], UI)
box(280, 78, 200, 64, "Web console · mobile", ["chat mode over /ws/chat", "a viewport of 768 px or less", "same URL, same token"], UI)
box(570, 78, 240, 64, "Serial TUI", ["EMBRA_TUI=1 · no image rebuild", "the QEMU host's terminal, the serial line", "embra-web is not started"], UI)
text(884, 90, "Any machine with the boot URL:", size=9.8, fill=DETAIL)
text(884, 103, "https://<host>:3345/embraOS?token=…", size=9.6, fill=WEB, family=MONO)
text(884, 116, "a new 32-hex token every boot, printed in the", size=9.8, fill=DETAIL)
text(884, 129, "boot banner only; the first visit sets a cookie", size=9.8, fill=DETAIL)

# ------------------------------------------------------------ services band --
band(40, 172, 1120, 512, "Services · one static musl binary each · supervised by embrad")
# row A
box(60, 200, 420, 72, "embra-web", [
    "web console server · TLS certificate minted by trustd at every start",
    "/ws/terminal PTY bridge · /ws/chat · /ws/activity · /ws/tools · /api/*",
    "serves embra-web-ui (Leptos/WASM) at /embraOS · gated by the boot token",
], WEB, order=5, port="HTTPS :3345")
box(520, 200, 340, 72, "embra-console", [
    "conversational TUI (ratatui over crossterm)",
    "the PTY child of embra-web, or on the serial line",
    "reaches the brain through apid and nothing else",
], WEB, order=6)
# row B
box(200, 308, 520, 56, "embra-apid", [
    "gateway: every brain RPC proxied · REST /health, /version, /status (the provider probe)",
    "streams WatchActivity (names and numbers) and WatchToolCalls (content, 64 KiB per side)",
], APID, order=3, port="gRPC :50000 · REST :8443")
# row C
box(60, 400, 680, 124, "embra-brain", [
    "headless AI runtime: providers (Anthropic, Gemini,",
    "Ollama, LM Studio) · 119 tools · sessions · Learning Mode",
    "knowledge graph + in-OS embeddings (tract · bge-small)",
    "proactive: reminders, cron · media (images in and out)",
    "tools write under /embra/workspace only · read everything",
    "but the operator's secrets · a stored secret never leaves",
    "SSH to private only · prompt and manifest fixed between turns",
], BRAIN, order=4, port="gRPC :50002", size=9.9, lead=12)
box(420, 428, 308, 86, "embra-guardian · in-process", [
    "syn validator · wasmtime sandbox, a fresh instance per call",
    "capability broker: http_get, http_request, web_search —",
    "egress policy and secrets host-side · replicant check +",
    "operator approval · built by the in-OS Rust toolchain",
], GUARD, size=9.6, lead=11.6)
box(880, 400, 280, 136, "Outside the OS", [
    "reached through the NIC (DHCP, or 10.0.2.15):",
    "AI providers · Anthropic, Gemini,",
    "Ollama, LM Studio (local or remote)",
    "Brave Search (web_search)",
    "GitHub · GitLab (gh_*, gl_*, git)",
    "GitHub Releases · the signed OS update",
    "Guardian egress: https only, address",
    "classes, /guardian egress allow <host>",
], DIM, dash="3 3", size=9.8, lead=12)
# row D
box(60, 560, 320, 98, "wardsondb", [
    "JSON document database · a vendored fork",
    "engine fjall | rocksdb, fixed in DATA at first boot",
    "soul · memory · knowledge graph · sessions ·",
    "schedules · Guardian tool definitions",
], DB, order=1, port="REST :8090")
box(450, 560, 290, 98, "embra-trustd", [
    "soul SHA-256 verification against",
    "/embra/state/soul.sha256 · a mismatch halts",
    "PKI: root CA 10 y · service certs 1 y",
    "GenerateCertificate for embra-web",
], TRUST, order=2, port="gRPC :50001")
# right column
box(880, 200, 280, 188, "Also in the workspace", [
    "embra-web-ui · Leptos/WASM, built with Trunk,",
    "embedded in embra-web, runs in the browser",
    "embra-common · tonic codegen of proto/",
    "(common, trust, brain, apid)",
    "embra-tools-core · embra-tool-macro",
    "the tool registry (#[embra_tool])",
    "embra-init · the initramfs /init (below)",
    "embra-boot · the boot contract: the kernel line,",
    "partitions by GPT name, grubenv, the release stamp",
    "embra-update · snapshots and restores of STATE and",
    "DATA · the signed release into the other slot",
    "wardsondb · vendored fork, reached over REST,",
    "never a library dependency of a crate here",
], KERNEL, size=9.6, lead=11.4)
box(880, 560, 280, 98, "Writable mounts · the rootfs is read-only", [
    "/embra/state ← STATE: soul.sha256, keys (0600), CA",
    "drop-ins, imported-intelligence/, update-source",
    "/embra/data ← DATA: WardSONDB · backups/",
    "/embra/workspace: a bind mount by embrad,",
    "the one place the tools write",
], SLAB, size=9.6, lead=12)

# ------------------------------------------------------------------ arrows --
# UIs → embra-web / embra-console
arrow([(160, 142), (160, 199)], "HTTPS + WS :3345 · the boot token", (170, 174))
arrow([(380, 142), (380, 199)], "QEMU hostfwd on every host interface", (392, 174))
arrow([(690, 142), (690, 199)], "ttyS0 | ttyAMA0 · QEMU -serial mon:stdio", (702, 174))
# web ↔ console over the PTY
arrow([(480, 236), (519, 236)], "PTY", (500, 230), anchor="middle")
# web / console → apid
arrow([(300, 272), (300, 307)], "gRPC :50000 · chat, sessions, stop, media, the activity and tool-call feeds", (312, 293))
arrow([(690, 272), (690, 307)], "gRPC :50000 · Converse", (702, 293))
# apid → brain
arrow([(300, 364), (300, 399)], "gRPC :50002 · every brain RPC", (312, 385))
# apid → trustd, routed right of the brain
arrow([(720, 336), (800, 336), (800, 609), (741, 609)], "gRPC :50001", (806, 548), anchor="start", label_dy=0)
# brain → wardsondb
arrow([(230, 524), (230, 559)], "REST :8090 · soul, memory, knowledge graph, sessions, schedules, tool definitions", (242, 545))
# trustd → wardsondb
arrow([(450, 636), (381, 636)], "REST :8090", (415, 630), anchor="middle")
text(415, 649, "reads the soul", size=9.0, fill="#bdbac4", anchor="middle")
# web → trustd (certificate), along the left margin and under wardsondb
arrow([(60, 250), (48, 250), (48, 672), (560, 672), (560, 659)], "GenerateCertificate :50001 at every embra-web start", (300, 668), anchor="middle", label_dy=-4)
# brain → outside
arrow([(740, 450), (879, 450)], "HTTPS out", (810, 445), anchor="middle", dash="2 3")
text(810, 462, "providers · tools · Guardian", size=9.0, fill="#bdbac4", anchor="middle")

# ------------------------------------------------------------------ slabs --
def slab(y, h, title, color, lines, order_note=None):
    rect(60, y, 1100, h, BOX, color, rx=8)
    text(72, y + 19, title, size=12.5, fill=TEXT, weight="bold")
    for i, line in enumerate(lines):
        text(72, y + 35 + i * 12.6, line, size=10, fill=DETAIL)

slab(712, 82, "embrad · PID 1 · init and service supervisor", SLAB, [
    "starts the services in order ① → ⑥ · soul gate: VerifySoul through trustd — a mismatch halts the boot, a missing soul is a first run (Learning Mode) · 5 s reconciliation loop",
    "any exit is restarted, 1 s · 2ⁿ up to 30 s, ten in a burst halts the service · bind-mounts /embra/workspace · mints the web token and prints the boot URL · operator CA drop-ins",
    "a trial boot (embra.trial=1) snapshots STATE and DATA before any service and is confirmed on the ESP once every service is healthy · embra.mode=update|backup|restore runs embra-update",
    "eth0: a DHCP lease through udhcpc (the lease script applies it, the client renews it), the static 10.0.2.15/24 via 10.0.2.2 after 10 s without one · /etc/resolv.conf → /embra/ephemeral",
])
slab(818, 58, "Linux kernel 6.18 LTS · Buildroot 2026.02.1 · initramfs /init = embra-init", KERNEL, [
    "virtio · AHCI · NVMe · virtio-scsi · pvscsi · e1000e · vmxnet3 · Hyper-V · SquashFS (xz) · ext4 · fbcon on the firmware's framebuffer · console=tty0 console=ttyS0 | ttyAMA0 · embra.slot=a|b",
    "embra-init (sync, no tokio): mounts proc, sys, dev → / from the slot's partition by GPT name (rootfs_a | rootfs_b), read-only → /embra/state from state → /embra/data from data → chroot → exec /sbin/embrad",
])
slab(900, 70, "UEFI firmware · GRUB 2.12 on the EFI System Partition · the A/B state in grubenv", KERNEL, [
    "OVMF | AAVMF → EFI/BOOT/BOOTX64.EFI | BOOTAA64.EFI, every module builtin · grub.cfg: the menu — slot A, slot B, Diagnostics, Update, Backup and restore, Reboot, Halt, Firmware setup",
    "grubenv: slot_active · slot_try · tries_left 3-2-1-0 · a status per slot — a staged slot boots with embra.trial=1 while tries last, failed at zero; embrad confirms, embra-update stages or rolls back",
    "the slot's kernel and initramfs loaded from its SquashFS (squash4 + xzio) · the host's EMBRA_* knobs arrive as one SMBIOS type 11 string the menu appends to the kernel line",
])
slab(994, 84, "QEMU · the host", QEMU, [
    "qemu-system-x86_64 -accel kvm (Linux) · qemu-system-aarch64 -accel hvf (Apple Silicon) · -m 4G · -smp N · OVMF on pflash (EMBRA_BOOT=uefi, default) | -kernel + -initrd (direct) · -nographic -serial mon:stdio",
    "-nic user: hostfwd 3345 → every host interface (the web console) · 50000 and 8443 → the host's loopback only (gRPC, REST) · QEMU's DHCP leases 10.0.2.15 · every other port answers inside the guest only",
    "embraos.img · GPT aligned to 1 MiB: esp vfat 64 MiB (GRUB, grub.cfg, grubenv) · rootfs_a | rootfs_b 1 GiB each, one xz SquashFS with its kernel and initramfs · state ext4 256 MiB · data ext4 4 GiB",
])
# embrad → trustd (the soul gate)
arrow([(660, 712), (660, 659)], "VerifySoul at boot", (668, 690), label_dy=0)
# slab connectors on the right
arrow([(1120, 712), (1120, 685)], "starts ① → ⑥ · restarts any exit", (1108, 703), anchor="end", label_dy=0)
arrow([(1120, 818), (1120, 795)], "exec /sbin/embrad after chroot", (1108, 811), anchor="end", label_dy=0)
arrow([(1120, 900), (1120, 877)], "linux + initrd from the slot's SquashFS", (1108, 893), anchor="end", label_dy=0)
arrow([(1120, 994), (1120, 971)], "pflash: the firmware · the virtio disk", (1108, 987), anchor="end", label_dy=0)

# ------------------------------------------------------------------ footer --
text(60, H - 12, "Every port, path and flag is the one the code binds or the scripts pass · detail: docs/SYSTEM-DESIGN.md", size=9.4, fill=DIM)
text(1160, H - 12, "2026-11", size=9.4, fill=DIM, anchor="end")
emit('</svg>')

svg_path = os.path.join(OUT, "system-architecture.svg")
png_path = os.path.join(OUT, "system-architecture.png")
with open(svg_path, "w", encoding="utf-8") as f:
    f.write("\n".join(parts) + "\n")
subprocess.run(["resvg", "--zoom", str(SCALE), svg_path, png_path], check=True)
print(f"wrote {svg_path} and {png_path} ({W*SCALE}x{H*SCALE})")
