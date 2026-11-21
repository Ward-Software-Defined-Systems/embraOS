# Validation status

What embraOS has been run on, and what has not been run yet. This is the one page that says so. The rest of the manual describes what the OS does, and the [change log](CHANGE-LOG.md) records what changed and when.

**As of:** 2026-11-21, after `v0.27.0-phase2`.

x86_64 under QEMU on Linux is the reference platform: what the manual describes has been run there, unless a row below marks it TBD. The tables list what has been run beyond that, and every item that is still TBD. An item marked TBD has not been run; it is not known to fail.

## The platforms

| Name on this page | What it is |
|---|---|
| QEMU | x86_64 with KVM on Ubuntu 24.04 and 26.04, under OVMF |
| QEMU on an Intel Mac | x86_64 with HVF |
| QEMU on Apple Silicon | aarch64 with HVF |
| ESXi | VMware ESXi 8.0 U2 |
| Hyper-V | a generation 2 VM with Secure Boot off |
| Parallels | Parallels Desktop 26.5 on an Intel Mac |
| Parallels on Apple Silicon | Parallels Desktop on an Apple Silicon Mac, an aarch64 VM |
| Fusion | VMware Fusion on Apple Silicon |
| Bare metal | a YouYeeToo X1 single-board computer (Intel Celeron N5105, 16 GB of memory, eMMC, a Realtek RTL8168h Ethernet port, Intel graphics on HDMI) |
| The laptop | a second machine that is not a VM: a Lenovo ThinkPad E550 (Intel Core i7-5500U, 8 GB of memory, a SATA SSD, an Intel Ethernet port, an Intel Wireless-AC 7265 card, Intel graphics on its own panel) |
| The stick | a third machine that is not a VM: an AWOW NY41S stick PC (Intel Celeron J4105, 8 GB of memory, eMMC, an Intel Dual Band Wireless-AC 3165 card, a Realtek RTL8168h Ethernet port, Intel graphics on HDMI) |
| The Pi | an aarch64 machine that is not a VM: a Raspberry Pi 5 (8 GB of memory, board revision 1.1, an SD card, a Linksys AE1000 USB Wi-Fi adapter on a Ralink RT3572), started by a UEFI firmware whose files are on the installed card's EFI partition |

## Images and boot

| What | Validated on | TBD |
|---|---|---|
| The disk image: UEFI, GRUB's menu, the two slots | QEMU; QEMU on an Intel Mac; QEMU on Apple Silicon | |
| The disk on another bus, another kind of network card | QEMU: AHCI, IDE, NVMe and virtio-scsi disks, a pvscsi disk on the direct kernel path; e1000e and vmxnet3 cards | |
| A bridged network card | QEMU on an Intel Mac and on Apple Silicon (`vmnet-bridged`) | |
| `vmware/embraos.ova` | ESXi, as imported: the pvscsi disk, the VMXNET3 adapter, the display as the console | the aarch64 OVA on an Arm hypervisor |
| `vmware/embraos.vmdk` | | Fusion and VirtualBox; the aarch64 VMDK |
| `hyper-v/embraos.vhdx` | Hyper-V | the aarch64 VHDX |
| `parallels/embraos.vmx` with its VMDK | Parallels | Parallels on Apple Silicon |
| A machine that is not a VM | Bare metal: the installed disk boots from eMMC, with the display on HDMI, a USB keyboard, a DHCP lease on the onboard port, and the CPU's microcode updated early. The laptop: the installed disk boots from its SATA SSD, with its own panel, keyboard and pointer, and a lease on its wired port and over Wi-Fi. The stick: the installed disk boots from its eMMC, with the display on HDMI, a keyboard and a mouse, and a lease over Wi-Fi. The Pi: the installed card boots through its UEFI firmware, with the display on HDMI, a keyboard and a mouse, a lease over Wi-Fi on its USB adapter, and a session; under that firmware the board's own Ethernet port and Wi-Fi card do not come up | every other machine |
| A core that is awake runs at the top of its frequency range (the `performance` governor and policy; before, a CPU without hardware P-states stayed at its lowest frequency) | Bare metal, a CPU with hardware P-states: every core's policy reads `performance`, and its energy preference with it. The stick, a CPU without them: every core's governor reads `performance` and its frequency 2.4 GHz, where it read 0.8 GHz, and the machine is fast again | |
| An Intel CPU sleeps by the kernel's own idle states for its model (Intel's idle driver; before, by the states its firmware declares) | | An Intel machine that is not a VM: the idle driver reads `intel_idle` |
| aarch64: a machine the Raspberry Pi's frequency driver or ACPI's CPPC driver serves has a frequency policy (both are built in; before, modules that nothing loaded) | | A Raspberry Pi started with a device tree, or an Arm machine whose ACPI tables declare `_CPC`: a policy exists and reads `performance` |
| An AMD machine's graphics are driven by the kernel's own driver (`amdgpu`, its firmware in the boot archive; before, the firmware's framebuffer alone) | | An AMD machine that is not a VM: a picture after boot with the driver bound, the display console in its mode; `nomodeset` back on the firmware's framebuffer |
| `/reboot halt`: the services stopped, STATE and DATA unmounted, the machine powered off; `/reboot now` unmounts both as well | QEMU; bare metal (the power-off, and the boot after it) | a machine that cannot power itself off; aarch64 |

## The installer (`embraos.iso`)

| What | Validated on | TBD |
|---|---|---|
| A clean install | QEMU (a DVD onto a virtio disk, a USB stick onto an AHCI disk); ESXi (the installed disk also boots behind the LSI Logic SAS and Parallel controllers); Hyper-V; Parallels; bare metal (a USB stick onto the eMMC, the prompts answered at the display) | a disk with 4096-byte sectors |
| The aarch64 medium | QEMU on Apple Silicon (a DVD onto virtio-blk and virtio-scsi disks, a USB stick onto an NVMe disk); Fusion; Parallels on Apple Silicon | every other Arm hypervisor |
| An unattended install | QEMU; QEMU on Apple Silicon | |
| A second install over an installed disk | QEMU | |
| A stick that attaches as UAS | QEMU, from the medium's kernel on (OVMF reads no such stick) | a UAS stick booted by a machine's firmware |
| The upgrade in place | QEMU (at the prompt and unattended, into either slot; the trial boot, its snapshot and the confirmation; the slot before it booted again after three failed tries); bare metal; the stick (from a USB stick, a medium built on an Intel Mac) | |
| A medium of the other storage engine is refused | Bare metal | |

## The display console

| What | Validated on | TBD |
|---|---|---|
| The page, the keyboard, the pointer, a click and the 1920x1080 mode | QEMU (standard VGA and virtio-gpu); ESXi; Hyper-V; Parallels; QEMU on an Intel Mac and on Apple Silicon (virtio-gpu); bare metal (HDMI); the stick (HDMI: the page, a keyboard, a mouse and the 1920x1080 mode); the laptop and Parallels on Apple Silicon (the page, the keyboard, the pointer and a click); the Pi (HDMI: the page, a keyboard, a mouse and a click) | |
| A key held down: a letter repeats, and a modifier is released with its key | QEMU (standard VGA); bare metal (a held modifier) | |
| A firmware framebuffer (`ramfb`): 800x600, and 1024x768 on request | QEMU; QEMU on Apple Silicon | |
| `embra.kioskmode=` | QEMU (standard VGA, virtio-gpu, a firmware framebuffer), GRUB's Diagnostics entry among the ways in | |
| The page's scale: 2 on a mode 2560 pixels wide, 1 below, and `embra.kioskscale=` for either | QEMU (standard VGA, at 1920x1080 and at 2560x1440) | bare metal |
| The menu: Copy, Paste, This console's URL | QEMU; QEMU on an Intel Mac and on Apple Silicon | |
| The dropdowns, and the notices for what needs a browser | QEMU (standard VGA); bare metal | |

## The network

| What | Validated on | TBD |
|---|---|---|
| A DHCP lease on every card: one card, several, a USB card, a card or a link that comes late, no card | QEMU; bare metal (the onboard port); the laptop (its wired port) | on a machine: a USB Ethernet adapter, and a cable plugged in after the boot |
| `/network static` on a network without DHCP | QEMU; bare metal; the laptop (an address of its own on a card) | |
| `/network hostname` | QEMU | the name in a router's list |
| The web console's certificate and its CA | QEMU, with curl as the client: by address and by name, a name set with `/network hostname`, an address that changes, the CA of an earlier release issued again once | a browser with the CA imported; a name a DNS server serves |
| The web console at the leased address | QEMU on an Intel Mac and on Apple Silicon (bridged); Hyper-V; Parallels; bare metal | |
| The web console's access token: the same at every boot, a new one with `/web-token rotate`, left out of a backup and kept by a restore | QEMU, with curl and the display console as the clients, the commands typed at the wizard; bare metal, with a browser (the same URL after a reboot, the sign-out at a rotation) | the phone's page |

## Wi-Fi

| What | Validated on | TBD |
|---|---|---|
| A scan, a join, a wrong passphrase, the join at the next boot, `embra.wifi=0` | QEMU, on the kernel's simulated radios with a stand-in access point; the laptop (a scan, a join with its lease, the join at the next boot); the stick (a join with its lease, the join at the next boot); the Pi (a join with its lease) | |
| A real card | QEMU: one USB adapter (Realtek RTL8852AU) handed to the VM with `EMBRA_USB_HOST`, as far as a scan; the laptop: its built-in Intel Wireless-AC 7265 (`iwlwifi`); the stick: its built-in Intel Dual Band Wireless-AC 3165 (`iwlwifi`); the Pi: its USB adapter (`rt2800usb`) | on a machine, a USB adapter that arrives as a driver disk, which embrad itself switches; every other card family; `EMBRA_USB_HOST` on a Mac |

## The clock

| What | Validated on | TBD |
|---|---|---|
| At boot: a hardware clock that lost its time, a time server that answers, one that does not, `embra.ntp=off`, no network card | QEMU; bare metal (a board whose clock battery is dead, booted with a network) | |
| The saved time, across power cuts | QEMU | bare metal: a boot of the board without a network |
| The checks while the OS runs: a slew, a step, an answer refused, a silent server, a late link, a lease's time server, the operator's servers, `/network time off` | QEMU, against a stand-in time server; bare metal (one check, slewed) | the other cases on a real network |

## Updates

| What | Validated on | TBD |
|---|---|---|
| A signed update with `/update apply`: the check, the download, the staged slot, the trial boot with its snapshot, the confirmation | QEMU (a test release); bare metal (the published releases: `v0.24.0-phase2`, then `v0.25.0-phase2` over it, then `v0.26.0-phase2`); the laptop (`v0.26.0-phase2`); Fusion (aarch64: `v0.26.0-phase2` over `v0.25.0-phase2`) | |
| The same from the boot menu's Update entry | QEMU | |
| A slot that does not come up: three tries, then the slot before it | QEMU | |
| A manifest that was changed, or signed with another key, is refused | QEMU | |
| The automatic check: the `update` block, the notice of a newer release, `/update auto` | QEMU; bare metal (the block, the notice); Fusion (the notice) | |
| `/update rollback`; `--tag`; `--allow-downgrade` | | on any machine |

## Backups

| What | Validated on | TBD |
|---|---|---|
| Snapshots and restores: `/backup`, and the boot menu's Backup and restore | QEMU; the stick | |
| A backup uploaded through the web console and restored | QEMU; bare metal; the stick (a backup taken on bare metal); the Pi (a backup taken on an x86_64 machine); one instance carried this way between bare metal, the stick and the Pi | |
| `/backup` and `/reboot` before a session | QEMU (at the wizard and in Learning Mode); bare metal (at the wizard) | Learning Mode on bare metal |
| `embraos-backup.sh` across image rebuilds, and its Mac wrapper | QEMU; QEMU on Apple Silicon | |
| `embraos-backup.sh`: the web console's access token and the saved clock left out of a backup, and the image's own kept at a restore | the script's `self-test`, on scratch directories | a backup and a restore of an image |

## Certificate authorities of your own

| What | Validated on | TBD |
|---|---|---|
| A CA in `Imported_CAs/` when the image is built | QEMU; bare metal | |
| A CA seeded with `seed-state.sh --ca-dir` | QEMU | |
| `/ca import` on a running machine | QEMU (by URL, and by an upload from a desktop browser); bare metal | an upload from the phone's page; at the first-boot wizard, against a gateway of your own |
| After the next boot, git and the OS updater trust the imported CA | QEMU (the merge at boot) | a git tool reaching a server behind the CA |

## The Local AI Gateway

| What | Validated on | TBD |
|---|---|---|
| Routing by prefix, the handshake, HTTPS with a seeded CA, a turn through the gateway for each provider, the DLP refusal, the setup flow and the wizard's question | QEMU | |
| HTTPS with the CA the image was built with | Bare metal | |
| A certificate refused on its dates, with the clock named | | a machine whose clock is behind |

## Guardian tools

| What | Validated on | TBD |
|---|---|---|
| The replicant check stopped while `/guardian-define` waits for it: nothing compiled, the model server's connection closed | QEMU, with a stand-in model server | `/guardian approve` stopped on a tool the intelligence proposed; `/guardian rebuild <name>` stopped while it waits |
| A tool of the operator's and one the intelligence proposes: validation, the replicant check and its refusal, the build, the sandbox | QEMU; QEMU on Apple Silicon | a live run of the rebuild gate, of a database error at the check and of a stored hash that no longer matches (unit tests cover the three) |
| The shipped tools: `web_search`, `http_request`, `websocket` | QEMU; bare metal (`websocket`) | |
| The browser service through `websocket` | QEMU (every call of a session); bare metal (one exchange, driven by the intelligence) | an intelligence that keeps one connection over several turns |
| A Microsoft 365 shared mailbox through `http_request` | QEMU | |

## Themes

| What | Validated on | TBD |
|---|---|---|
| The eight themes on the desktop page and the phone's page: the contrast of every text against what is painted behind it, measured on the page in each theme (the main view, the Tools tab and the picker; the chat, the command list and the picker), and the edge of every field | QEMU, in Chrome 156 | Safari on an iPhone; Firefox |
| What no rule of the stylesheet sets: the words the activity strip draws while its feed is away, a line the web console writes into the terminal itself, and the ring of every card of the picker on a page of another theme, each measured on the page in every theme | QEMU, in Chrome 156 | |
| The themes on the display console: each one drawn, a change followed within a few seconds, the stored theme at the first paint after a boot | QEMU; bare metal (HDMI) | |
| The picker (a press, a status poll while it is open, Escape, the keyboard's ring, Apply from a browser that observes) and `/theme` (the list, a name, `reset`, a word that is no theme, and a name at the first-boot wizard's first question) | QEMU, in Chrome 156; bare metal (the picker on the display console, with its mouse) | the picker on a phone |

## Turns and the model wait

| What | Validated on | TBD |
|---|---|---|
| A turn cut by a failed model call (a request refused, an error inside the answer, the model wait run out, the network card's link taken away before an answer and inside one): the error named, the turn saved with its marker, `/continue`, the next turn told what was cut | QEMU, with a stand-in model server on the LM Studio preset | a cut turn and `/continue` with a real key on Anthropic and on Gemini (the tests run both against a stand-in server) |
| A stop before the answer begins and inside a silent answer: the request's connection closed with the stop | QEMU, with a stand-in model server | Ollama and LM Studio: the server's own log showing that it left the request |
| The model wait: `/model-wait`, a call ended when the wait ran out | QEMU, with a wait of one minute | a local model whose answer begins after more than 15 minutes |
| A turn that was running at a restart: in its session after the boot, the next turn told | QEMU: `/reboot now`; a power cut one second into a turn's held model call, and two seconds after a turn's end, on the RocksDB engine and on the fjall engine | |
| The seal and a power cut: the soul is on the disk before its hash is stored, and the next boot verifies it | QEMU: a power cut a quarter of a second after the seal, on the RocksDB engine and on the fjall engine | |
| Learning Mode through a refused model call: the phase stays open | QEMU, with a stand-in model server | |
| The wait for the model said in minutes: the console's line and the phone's top bar, after a minute of a held call | QEMU, in Chrome 156 | |
| A console that attaches while a turn runs: told what runs, shown as running, its stop ends the turn, the reply arrives at the turn's end | QEMU, with two consoles on a stand-in model server | a phone that loses and regains its connection during a turn |

## First boot and identity

| What | Validated on | TBD |
|---|---|---|
| A console that leaves at a question of the first-boot wizard: its conversation ends there, no Learning Mode and no model call follow, and the next console starts the wizard | QEMU | |
| The configuration wizard, Learning Mode, the identity import | QEMU; QEMU on Apple Silicon | Learning Mode with a real model while two consoles are attached |
| The re-seal ceremony | a raw disk image on the build host | a machine installed from the ISO; a disk converted for a hypervisor |

## Builds

| What | Validated on | TBD |
|---|---|---|
| The x86_64 image | Ubuntu 24.04 and 26.04, the reference build; an Intel Mac, with Buildroot in Docker (with the wireless drivers in the image: its installer medium upgraded the stick, which joined its Wi-Fi network) | |
| The aarch64 image | Apple Silicon, with Buildroot in Docker: both storage engines | |
| The launchers under the bash 3.2 that macOS ships | An Intel Mac; Apple Silicon | |
| A build leaves none of its host's directories in the programs, in the web console's bundle or in the root filesystem, and the kernel's banner names the image | Ubuntu 24.04: an image build with both read-backs, its slot image unpacked and searched, and the image booted under QEMU | a build on a Mac (the host pass under bash 3.2 with the BSD tools) |
| `seed-state-mac.sh --update-key` | | on a Mac |

Which models have been vetted with each provider is in [LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md](LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md).
