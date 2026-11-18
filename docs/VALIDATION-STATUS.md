# Validation status

What embraOS has been run on, and what has not been run yet. This is the one page that says so. The rest of the manual describes what the OS does, and the [change log](CHANGE-LOG.md) records what changed and when.

**As of:** 2026-11-18, `v0.26.0-phase2`.

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
| Fusion | VMware Fusion on Apple Silicon |
| Bare metal | one machine: a YouYeeToo X1 single-board computer (Intel Celeron N5105, 16 GB of memory, eMMC, a Realtek RTL8168h Ethernet port, Intel graphics on HDMI) |

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
| A machine that is not a VM | Bare metal: the installed disk boots from eMMC, with the display on HDMI, a USB keyboard, a DHCP lease on the onboard port, and the CPU's microcode updated early | every other machine; an aarch64 machine |
| `/reboot halt`: the services stopped, STATE and DATA unmounted, the machine powered off; `/reboot now` unmounts both as well | QEMU; bare metal (the power-off, and the boot after it) | a machine that cannot power itself off; aarch64 |

## The installer (`embraos.iso`)

| What | Validated on | TBD |
|---|---|---|
| A clean install | QEMU (a DVD onto a virtio disk, a USB stick onto an AHCI disk); ESXi (the installed disk also boots behind the LSI Logic SAS and Parallel controllers); Hyper-V; Parallels; bare metal (a USB stick onto the eMMC, the prompts answered at the display) | a disk with 4096-byte sectors |
| The aarch64 medium | QEMU on Apple Silicon (a DVD onto virtio-blk and virtio-scsi disks, a USB stick onto an NVMe disk); Fusion | every other Arm hypervisor |
| An unattended install | QEMU; QEMU on Apple Silicon | |
| A second install over an installed disk | QEMU | |
| A stick that attaches as UAS | QEMU, from the medium's kernel on (OVMF reads no such stick) | a UAS stick booted by a machine's firmware |
| The upgrade in place | QEMU (at the prompt and unattended, into either slot; the trial boot, its snapshot and the confirmation; the slot before it booted again after three failed tries); bare metal | |
| A medium of the other storage engine is refused | Bare metal | |

## The display console

| What | Validated on | TBD |
|---|---|---|
| The page, the keyboard, the pointer, a click and the 1920x1080 mode | QEMU (standard VGA and virtio-gpu); ESXi; Hyper-V; QEMU on an Intel Mac and on Apple Silicon (virtio-gpu); bare metal (HDMI) | the pointer and a click on Parallels, where the page, the mode and the keyboard work |
| A key held down: a letter repeats, and a modifier is released with its key | QEMU (standard VGA); bare metal (a held modifier) | |
| A firmware framebuffer (`ramfb`): 800x600, and 1024x768 on request | QEMU; QEMU on Apple Silicon | |
| `embra.kioskmode=` | QEMU (standard VGA, virtio-gpu, a firmware framebuffer), GRUB's Diagnostics entry among the ways in | |
| The menu: Copy, Paste, This console's URL | QEMU; QEMU on an Intel Mac and on Apple Silicon | |
| The dropdowns, and the notices for what needs a browser | QEMU (standard VGA); bare metal | |

## The network

| What | Validated on | TBD |
|---|---|---|
| A DHCP lease on every card: one card, several, a USB card, a card or a link that comes late, no card | QEMU; bare metal (the onboard port) | on a machine: a USB Ethernet adapter, and a cable plugged in after the boot |
| `/network static` on a network without DHCP | QEMU; bare metal | |
| `/network hostname` | QEMU | the name in a router's list |
| The web console's certificate and its CA | QEMU, with curl as the client: by address and by name, a name set with `/network hostname`, an address that changes, the CA of an earlier release issued again once | a browser with the CA imported; a name a DNS server serves |
| The web console at the leased address | QEMU on an Intel Mac and on Apple Silicon (bridged); Hyper-V; Parallels; bare metal | |
| The web console's access token: the same at every boot, a new one with `/web-token rotate`, left out of a backup and kept by a restore | QEMU, with curl and the display console as the clients, the commands typed at the wizard; bare metal, with a browser (the same URL after a reboot, the sign-out at a rotation) | the phone's page |

## Wi-Fi

| What | Validated on | TBD |
|---|---|---|
| A scan, a join, a wrong passphrase, the join at the next boot, `embra.wifi=0` | QEMU, on the kernel's simulated radios with a stand-in access point | a join on a real card |
| A real card | QEMU: one USB adapter (Realtek RTL8852AU) handed to the VM with `EMBRA_USB_HOST`, as far as a scan | Wi-Fi on any machine, where embrad itself switches an adapter that arrives as a driver disk; every other card family; a card on an Arm machine; `EMBRA_USB_HOST` on a Mac |

## The clock

| What | Validated on | TBD |
|---|---|---|
| At boot: a hardware clock that lost its time, a time server that answers, one that does not, `embra.ntp=off`, no network card | QEMU; bare metal (a board whose clock battery is dead, booted with a network) | |
| The saved time, across power cuts | QEMU | bare metal: a boot of the board without a network |
| The checks while the OS runs: a slew, a step, an answer refused, a silent server, a late link, a lease's time server, the operator's servers, `/network time off` | QEMU, against a stand-in time server; bare metal (one check, slewed) | the other cases on a real network |

## Updates

| What | Validated on | TBD |
|---|---|---|
| A signed update with `/update apply`: the check, the download, the staged slot, the trial boot with its snapshot, the confirmation | QEMU (a test release); bare metal (the published releases: `v0.24.0-phase2`, then `v0.25.0-phase2` over it) | an aarch64 machine or VM |
| The same from the boot menu's Update entry | QEMU | |
| A slot that does not come up: three tries, then the slot before it | QEMU | |
| A manifest that was changed, or signed with another key, is refused | QEMU | |
| The automatic check: the `update` block, the notice of a newer release, `/update auto` | QEMU; bare metal (the block) | |
| `/update rollback`; `--tag`; `--allow-downgrade` | | on any machine |

## Backups

| What | Validated on | TBD |
|---|---|---|
| Snapshots and restores: `/backup`, and the boot menu's Backup and restore | QEMU | |
| A backup uploaded through the web console and restored | QEMU; bare metal | |
| `/backup` and `/reboot` before a session | QEMU (at the wizard and in Learning Mode); bare metal (at the wizard) | Learning Mode on bare metal |
| `embraos-backup.sh` across image rebuilds, and its Mac wrapper | QEMU; QEMU on Apple Silicon | |

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
| A tool of the operator's and one the intelligence proposes: validation, the replicant check and its refusal, the build, the sandbox | QEMU; QEMU on Apple Silicon | a live run of the rebuild gate, of a database error at the check and of a stored hash that no longer matches (unit tests cover the three) |
| The shipped tools: `web_search`, `http_request`, `websocket` | QEMU; bare metal (`websocket`) | |
| The browser service through `websocket` | QEMU (every call of a session); bare metal (one exchange, driven by the intelligence) | an intelligence that keeps one connection over several turns |
| A Microsoft 365 shared mailbox through `http_request` | QEMU | |

## First boot and identity

| What | Validated on | TBD |
|---|---|---|
| The configuration wizard, Learning Mode, the identity import | QEMU; QEMU on Apple Silicon | Learning Mode with a real model while two consoles are attached |
| The re-seal ceremony | a raw disk image on the build host | a machine installed from the ISO; a disk converted for a hypervisor |

## Builds

| What | Validated on | TBD |
|---|---|---|
| The x86_64 image | Ubuntu 24.04 and 26.04, the reference build; an Intel Mac, with Buildroot in Docker | on an Intel Mac, a build since the wireless drivers joined the image |
| The aarch64 image | Apple Silicon, with Buildroot in Docker: both storage engines | |
| The launchers under the bash 3.2 that macOS ships | An Intel Mac; Apple Silicon | |
| `seed-state-mac.sh --update-key` | | on a Mac |

Which models have been vetted with each provider is in [LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md](LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md).
