# Validation status

What embraOS has been run on, and what has not been run yet. This is the one page that says so. The rest of the manual describes what the OS does, and the [change log](CHANGE-LOG.md) records what changed and when.

**As of:** 2026-11-24, `v0.28.0-phase2`.

x86_64 under QEMU on Linux is the reference platform: what the manual describes has been run there, unless a row below marks it TBD or is about hardware a VM does not have: such a row names the machines it ran on. The tables list what has been run beyond that, and every item that is still TBD. An item marked TBD has not been run; it is not known to fail.

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
| The laptop | a second machine that is not a VM: a Lenovo ThinkPad E550 (Intel Core i7-5500U, 8 GB of memory, a SATA SSD, an Intel Ethernet port, an Intel Wireless-AC 7265 card, Intel graphics on its own panel, and a second graphics card without a display of its own, an AMD Radeon of the Southern Islands family) |
| The stick | a third machine that is not a VM: an AWOW NY41S stick PC (Intel Celeron J4105, 8 GB of memory, eMMC, an Intel Dual Band Wireless-AC 3165 card, a Realtek RTL8168h Ethernet port, Intel graphics on HDMI) |
| The Beelink | a fourth x86_64 machine that is not a VM, the first with an AMD processor: a Beelink SER mini PC (AMD Ryzen 5 5560U, 16 GB of memory, an NVMe SSD, a Realtek RTL8168h Ethernet port, an Intel Wi-Fi 6 AX200 card, AMD Radeon graphics on HDMI) |
| The Supermicro | a fifth x86_64 machine that is not a VM, the first server board: a Supermicro X11SSW-F (Intel Xeon E3-1230 v5 with four cores and eight threads, 16 GB of memory, a RAID volume of three 20 TB SAS disks behind an LSI SAS2008 controller, two Intel I210 Ethernet ports, no graphics card: the picture is its management controller's, an ASPEED AST2400 on VGA; no sound device and no TPM) |
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
| A machine that is not a VM | Bare metal: the installed disk boots from eMMC, with the display on HDMI, a USB keyboard, a DHCP lease on the onboard port, and the CPU's microcode updated early. The laptop: the installed disk boots from its SATA SSD, with its own panel, keyboard and pointer, and a lease on its wired port and over Wi-Fi. The stick: the installed disk boots from its eMMC, with the display on HDMI, a keyboard and a mouse, and a lease over Wi-Fi. The Beelink: the installed disk boots from its NVMe SSD, with the display on HDMI, a keyboard and a mouse, a lease on its wired port and over Wi-Fi at the same time, and the CPU's microcode (AMD's) updated early. The Supermicro: the installed disk boots from a RAID volume of 30 TB on its SAS controller (`mpt3sas`), with the display on the VGA output of its management controller (`ast`), a keyboard and a mouse through a KVM switch, and a lease on one of its two onboard ports (`igb`). The Pi: the installed card boots through its UEFI firmware, with the display on HDMI, a keyboard and a mouse, a lease over Wi-Fi on its USB adapter, and a session; under that firmware the board's own Ethernet port and Wi-Fi card do not come up, and its fan does not run | every other machine |
| A core that is awake runs at the top of its frequency range (the `performance` governor and policy; before, a CPU without hardware P-states stayed at its lowest frequency) | Bare metal, a CPU with hardware P-states: every core's policy reads `performance`, and its energy preference with it. The stick, a CPU without them: every core's governor reads `performance` and its frequency 2.4 GHz, where it read 0.8 GHz, and the machine is fast again. The laptop, another CPU without them: the governor reads `performance` and the frequency 2.9 GHz of its 3.0. The Beelink, an AMD CPU under AMD's own driver: the policy reads `performance`, and its energy preference with it. The Supermicro, a Xeon with hardware P-states: each of its eight policies reads `performance`, and the energy preference with them | |
| An Intel CPU's idle states come through Intel's idle driver: the kernel's own for a model the driver has a table for, the firmware's for any other (before, the firmware's on every machine) | The stick, the laptop and the Supermicro, models the driver has a table for: the idle driver reads `intel_idle`, with the kernel's states for the model (eight on the stick, where it read `acpi_idle` and four; nine on the laptop; seven on the Supermicro). Bare metal, a model it has no table for: the idle driver reads `intel_idle`, with the states its firmware declares | |
| aarch64: a machine served by one of the five frequency drivers the base configuration builds as modules has a frequency policy (the Raspberry Pi's, ACPI's CPPC, Allwinner's, NXP's for the i.MX 8M and MediaTek's are built in; before, modules that nothing loaded) | | A Raspberry Pi started with a device tree, an Arm machine whose ACPI tables declare `_CPC`, or a board of one of the three chip families: a policy exists and reads `performance` |
| An AMD machine's graphics are driven by the kernel's own driver (`amdgpu`, its firmware in the boot archive; before, the firmware's framebuffer alone) | The Beelink: the driver bound with its firmware loaded, a picture after boot, and the display console in its 1920x1080 mode at 60 Hz on HDMI (into a video switcher) | `nomodeset` back on the firmware's framebuffer |
| A Radeon from before `amdgpu` is driven by the kernel's `radeon` driver (its firmware in the boot archive; before, no driver took such a card) | The laptop, on its second card: the driver bound, with the card's firmware loaded | A machine with the display on such a card: a picture after boot and the display console in its mode |
| A laptop's second graphics card that drives no display is powered off by its driver (the kernel's `VGA_SWITCHEROO`; before, the `radeon` driver left such a card running) | The laptop, whose second card the `radeon` driver has: the kernel finds the firmware's switching method, the card's runtime status reads `suspended`, and the display console is on the panel | a laptop whose second card is `amdgpu`'s |
| A firmware's own OEM string stays off the kernel line: the boot menu reads the launcher's knobs from the SMBIOS OEM string, and takes the `embra.*` words of it alone (before, the whole string, so a board's own text reached the kernel as words it did not know) | QEMU, by the kernel's own record of its command line: the string a server board's firmware has (on the menu before the change it was on the line, with the kernel's notice of unknown parameters; after it, neither), a placeholder, a string that mixes knobs with other words (the knobs alone, in their order), the launcher's own knobs whole, a display mode among them taken for the firmware's framebuffer too, and a machine without the table | a machine whose firmware has such a string; the installer medium's menu |
| `/reboot halt`: the services stopped, STATE and DATA unmounted, the machine powered off; `/reboot now` unmounts both as well | QEMU; bare metal (the power-off, and the boot after it); the Pi (aarch64) | a machine that cannot power itself off |

## The installer (`embraos.iso`)

| What | Validated on | TBD |
|---|---|---|
| A clean install | QEMU (a DVD onto a virtio disk, a USB stick onto an AHCI disk); ESXi (the installed disk also boots behind the LSI Logic SAS and Parallel controllers); Hyper-V; Parallels; bare metal (a USB stick onto the eMMC, the prompts answered at the display); the Beelink (a USB stick onto its NVMe disk); the Supermicro (a USB stick onto a RAID volume of 30 TB behind its SAS controller) | a disk with 4096-byte sectors |
| The aarch64 medium | QEMU on Apple Silicon (a DVD onto virtio-blk and virtio-scsi disks, a USB stick onto an NVMe disk); Fusion; Parallels on Apple Silicon | every other Arm hypervisor |
| An unattended install | QEMU; QEMU on Apple Silicon | |
| A second install over an installed disk | QEMU | |
| A stick that attaches as UAS | QEMU, from the medium's kernel on (OVMF reads no such stick) | a UAS stick booted by a machine's firmware |
| The upgrade in place | QEMU (at the prompt and unattended, into either slot; the trial boot, its snapshot and the confirmation; the slot before it booted again after three failed tries); bare metal; the stick (from a USB stick, a medium built on an Intel Mac); the laptop and the Beelink (from a USB stick) | |
| A medium of the other storage engine is refused | Bare metal | |

## The display console

| What | Validated on | TBD |
|---|---|---|
| The page, the keyboard, the pointer, a click and the 1920x1080 mode | QEMU (standard VGA and virtio-gpu); ESXi; Hyper-V; Parallels; QEMU on an Intel Mac and on Apple Silicon (virtio-gpu); bare metal (HDMI); the stick (HDMI: the page, a keyboard, a mouse and the 1920x1080 mode); the laptop and Parallels on Apple Silicon (the page, the keyboard, the pointer and a click); the Pi (HDMI: the page, a keyboard, a mouse and a click); the Beelink (HDMI, into a video switcher: the page, a keyboard, a mouse and the 1920x1080 mode); the Supermicro (the VGA output of its management controller, a card without a graphics processor: the page, a keyboard and a mouse through a KVM switch) | |
| A key held down: a letter repeats, and a modifier is released with its key | QEMU (standard VGA); bare metal (a held modifier) | |
| A firmware framebuffer (`ramfb`): 800x600, and 1024x768 on request | QEMU; QEMU on Apple Silicon | |
| More than one graphics card: the page on the card that has a display connected | QEMU (the firmware's framebuffer beside a virtio-gpu card, each in turn without a display); the laptop (a second card with a driver and without a display: the page on the panel) | |
| `embra.kioskmode=` | QEMU (standard VGA, virtio-gpu, a firmware framebuffer), GRUB's Diagnostics entry among the ways in | |
| The page's scale: 2 on a mode 2560 pixels wide, 1 below, and `embra.kioskscale=` for either | QEMU (standard VGA, at 1920x1080 and at 2560x1440) | bare metal |
| The menu: Copy, Paste, This console's URL | QEMU; QEMU on an Intel Mac and on Apple Silicon | |
| The dropdowns, and the notices for what needs a browser | QEMU (standard VGA); bare metal | |

## The network

| What | Validated on | TBD |
|---|---|---|
| A DHCP lease on every card: one card, several, a USB card, a card or a link that comes late, no card | QEMU; bare metal (the onboard port); the laptop (its wired port); the Beelink (its wired port and its Wi-Fi card, each with a lease at the same time); the Supermicro (one of its two onboard ports, the other without a cable) | on a machine: a USB Ethernet adapter, and a cable plugged in after the boot |
| `/network static` on a network without DHCP | QEMU; bare metal; the laptop (an address of its own on a card) | |
| `/network hostname` | QEMU | the name in a router's list |
| The web console's certificate and its CA | QEMU, with curl as the client: by address and by name, a name set with `/network hostname`, an address that changes, the CA of an earlier release issued again once | a browser with the CA imported; a name a DNS server serves |
| The web console at the leased address | QEMU on an Intel Mac and on Apple Silicon (bridged); Hyper-V; Parallels; bare metal | |
| The web console's access token: the same at every boot, a new one with `/web-token rotate`, left out of a backup and kept by a restore | QEMU, with curl and the display console as the clients, the commands typed at the wizard; bare metal, with a browser (the same URL after a reboot, the sign-out at a rotation) | the phone's page |

## Wi-Fi

| What | Validated on | TBD |
|---|---|---|
| A scan, a join, a wrong passphrase, the join at the next boot, `embra.wifi=0` | QEMU, on the kernel's simulated radios with a stand-in access point; the laptop (a scan, a join with its lease, the join at the next boot); the stick (a join with its lease, the join at the next boot); the Pi (a join with its lease); the Beelink (a join with its lease) | |
| A real card | QEMU: one USB adapter (Realtek RTL8852AU) handed to the VM with `EMBRA_USB_HOST`, as far as a scan; the laptop: its built-in Intel Wireless-AC 7265 (`iwlwifi`); the stick: its built-in Intel Dual Band Wireless-AC 3165 (`iwlwifi`); the Pi: its USB adapter (`rt2800usb`); the Beelink: its built-in Intel Wi-Fi 6 AX200 (`iwlwifi`) | on a machine, a USB adapter that arrives as a driver disk, which embrad itself switches; every other card family; `EMBRA_USB_HOST` on a Mac |

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
| A backup uploaded through the web console and restored | QEMU; bare metal; the stick (a backup taken on bare metal); the Pi (a backup taken on an x86_64 machine); one instance carried this way between bare metal, the stick, the Pi, the Beelink and the Supermicro | |
| `/backup` and `/reboot` before a session | QEMU (at the wizard and in Learning Mode); bare metal (at the wizard) | Learning Mode on bare metal |
| `embraos-backup.sh` across image rebuilds, and its Mac wrapper | QEMU; QEMU on Apple Silicon | |
| `embraos-backup.sh`: the web console's access token and the saved clock left out of a backup, and the image's own kept at a restore | the script's `self-test`, on scratch directories | a backup and a restore of an image |
| `embraos-backup.sh`: the OS's own snapshots and the native models' files left out of a backup, at DATA's root alone (a workspace directory of either name is copied), and the image's own of both kept at a restore | the script's `self-test`, on scratch directories | a backup and a restore of an image |

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

## Image generation

The rows of a provider's own API are run with a key. The rows of the OS run against a stand-in for that API, which refuses what the provider's reference does not list and answers with a picture of the dimensions the reference gives: they show what the OS sends and what it does with a picture, and not what the provider answers: the format of a picture and its size in bytes are the stand-in's.

| What | Validated on | TBD |
|---|---|---|
| A picture from Gemini's image endpoint | The Supermicro, with a Gemini key: a prompt alone on `gemini-nano-banana-2.1` (a JPEG of 848×1264); `2K` at 16:9 and at 21:9 (3168×1344); `512` on `gemini-3.1-flash-image` (512×512); a reference image as an edit source, and two references; `gemini-3-pro-image` at `1K` and `2K`, and `gemini-3.1-flash-lite-image` and `gemini-2.5-flash-image` at `1K`, the last answering with a PNG though the request asks for a JPEG, stored as `gen-<id>.png`; a size the configured model does not render refused before a request | three or four reference images |
| `4K` from Gemini's image endpoint: a picture that comes back larger than the media store's 12 MiB is refused and nothing is written, and the call is still paid for | The Supermicro: a flat two-colour poster at 1:1 stored (7.8 MiB); a detailed photograph larger than 12 MiB on each of the three models that render `4K`, refused each time (six calls, 12.2 to 14.2 MiB: at 16:9 and 21:9 on `gemini-nano-banana-2.1`, at 21:9 on `gemini-3.1-flash-image`, three times at 21:9 on `gemini-3-pro-image`) | |
| `image_generate` to the stored picture: a call with a prompt alone asks the default model for a JPEG and names no size; `2K`, `4K` and a ratio as asked; `512` on the one model that renders it; a reference image sent with the prompt; the picture stored as `gen-<id>.jpg` at the size that came back (to the stand-in's 6336×2688), served by the web console as a JPEG and handed to the model; a size the configured model does not render refused before a request, with the model's own sizes and the models that render it named, and `512px` refused as no value of the API; a call that still names `output_format` answered; `/image-provider` with the default and the five models, `model`, a model that does not exist, `clear` | QEMU, with a stand-in for Gemini's endpoint behind the gateway's route | |
| A picture from Higgsfield's API | | each of the four models (`xai/grok-imagine-image-2.0`, `higgsfield-ai/soul/v2/standard`, `ideogram/v4.0`, `z-image/turbo`) with a prompt alone, and with a size and a ratio on each; how long a picture takes against the wait; a request still running at the deadline, and a stop during a wait (the cancel and what is charged); a reference image on `xai/grok-imagine-image-2.0` and one on `ideogram/v4.0`, through Higgsfield's storage; a refusal of the key, of the balance and of a model the account lacks; the host the pictures come from |
| `image_generate` on Higgsfield to the stored picture: `/image-provider higgsfield`, a key in Gemini's shape refused and a key id and secret stored, a model of Gemini's refused with the way to it; a call with a prompt alone sends the prompt alone to `xai/grok-imagine-image-2.0` with the key and an idempotency key, reads the status at the advised pace, fetches the picture without the key and stores it as it came (`gen-<id>.png`); each model with a size and a ratio (`2K` sent as `2k` with 16:9 on Grok, `1080p` with 3:4 on SOUL, 4:5 and no size on Ideogram, `2K` with 21:9 on Z-Image), stored as PNG or JPEG as each came; two references on `xai/grok-imagine-image-2.0` and one on `ideogram/v4.0`, each put in the stand-in's storage through a slot of its own, with the slot's headers and without the key, and named in the request by its public URL; nine refusals before a request (a size, a ratio, a prompt's length, a reference, a second reference), none of which reached the stand-in; a stop during a wait, which sent one cancel at once; a request still running at the deadline, canceled once, the answer saying it had started and is charged if it completes; with the provider gateway on, `/image-provider` saying that Higgsfield is reached directly, and the call going to Higgsfield's address and not the gateway's; back to Gemini, the Higgsfield model cleared and said so; the key removed; the key's secret in nothing the OS sent back | QEMU, with a stand-in for Higgsfield's API | |

## Native models (experimental)

Every row is the 4B (`qwen3.5-4b`) on the machine's processor, unless it names the 9B or the 27B. The rows of the Beelink are with the files of an earlier catalogue. In the QEMU runs the model server is the recipe's, built in a Buildroot output of its own and put into the root filesystem of the last image; on the Beelink and the Supermicro it is the one an image build made.

| What | Validated on | TBD |
|---|---|---|
| The model server as the recipe builds it: started inside the image's root filesystem, the CPU backend chosen for the processor (the host's with AVX2, and two processor models without AVX), the listener on loopback alone | QEMU. Parallels on Apple Silicon: the server started in the aarch64 image, its listener on loopback, the 4B and the 9B each loaded with its image file and answering `native_ask` calls of the instance's intelligence | |
| The backend named: embrad asks the server's program which CPU backend its loader takes on the processor, and `/native`, the status record (`backend=`) and embrad's log name it; asked once a boot, named at once for a server started after `/native off` and `/native use`, and absent from the record while no server runs | QEMU: `haswell` on the host's processor, `sse42` on a processor model without AVX (and `x64` on one without SSE4.2, the question alone). Parallels on Apple Silicon (aarch64): `armv8.2_2`. The Supermicro (a Xeon E3 v5, with AVX2 and without AVX-512): `haswell` | |
| An image build with the model server's recipe, and its read-back of the root filesystem | The x86_64 image on an Intel Mac and the aarch64 image on Apple Silicon, with Buildroot in Docker | |
| A model's file from the source: the download with its progress, a cancel, the rest fetched after a check of what had arrived, the file verified against the catalogue; the 9B refused on a DATA of 4 GiB; a removal; a second job refused while one runs; a download that goes on when its console leaves | QEMU. The Beelink: the 9B's file downloaded and verified, on a DATA that is the rest of its disk. The Supermicro: both files of the 4B and of the 9B downloaded and verified | a download through a certificate authority of the operator's own |
| A model's file by upload: through the page's `/native` form on a machine at the first-boot wizard (the picker, the progress, the file installed); a file of the catalogue's size with other content refused and nothing kept; the refusals before the first byte | QEMU, in Chrome 156 (the form) and with curl (the refusals) | the display console's notice; Safari and Firefox; a phone |
| A model's image file from the source: fetched alone for a model that has its model file, verified against the catalogue, the model server started again with it, and `/native` saying that it reads images where it said text alone | QEMU | |
| A model's image file by upload: the upload's size telling which of the model's two files it is, refused for the room on a full DATA, installed and listed beside a model file that is not there | QEMU, the route itself (`PUT /api/models/<name>`) | through the page's form in a browser |
| The catalogue's own files, from the source: on an instance that held an earlier catalogue's files of the 4B, `/native` says that they are another release's and the model server is not started; an install removes them, downloads both files from `William-WSDS`, verifies them, and the model server comes back reading images; an image through `native_ask` on those files; the 9B's two files the same way, an earlier catalogue's image file of it removed first, and an image read by it | QEMU | |
| The image files as F16, from the source: on an instance that held the BF16 image files of the three models, `/native` says of each that its image file is not installed, and the model kept loaded reads text alone; an image through `native_ask` is refused with the way on; an install removes the earlier image file, fetches the F16 one alone and verifies it, and the model server comes back reading images; the 4B, the 9B and the 27B each read an image through it | QEMU | |
| Qwen3.8 27B: its model file by the upload route (17 GB, checked against the catalogue) and its image file from the source; loaded with 30 GiB and its note on memory; `/effort` with its three levels, a level of another provider refused; a `native_ask` that thinks at the level set, with the reasoning apart; an image read | QEMU | its model file from the source; a session it leads; a machine that is not a VM |
| The model server kept by embrad: loaded at the wizard's answer and at `/native use`, ended at `/native off` with its memory free again, loaded again for another context window, after a reboot and after a restore; a message sent while the model loads, answered after the load | QEMU, with 12 GiB and eight processors | |
| Memory running out: with 3 GiB the kernel ends the model server each time it loads, embrad leaves it after the third, the database, the brain and the web console stay up, and the turn, `/native` and the provider pill say why; with 5 GiB the model loads at the default window | QEMU | a model server ended for memory after it had answered, and one that was left alone asked for again with `/native use` (unit tests cover both) |
| The first boot on the native answer: the notice, the model question, the file downloaded at the wizard, a console that leaves during the download and one that returns, the summary; Learning Mode's first and last phase led by the model with thinking on, the import of the shipped identity, the seal; the web console's own terminal, attached beside the console that answered, got the model's greeting and was in the session after the seal | QEMU | the identity written with a native model through every phase of Learning Mode, without an import |
| A session on the model: the whole tool list read, `time` and `remember` called with valid arguments and answered from, a stop inside an answer, thinking off and on (`/effort`), a request over a window of 4,096 tokens refused with both sizes, `/native off` and `/native remove` refused while it leads, an operator's Guardian tool refused for the replicant check that is not run, the session after a reboot | QEMU | the 9B; a session on a processor without AVX |
| `native_ask` from a session another provider leads: a plain question, named tools and the whole tool list offered (the call reported and not run), a system text with a limit on the answer, a name that is no tool, a stop during a call, no model loaded; the switch between providers that leaves the model loaded | QEMU, with a stand-in for the other provider (every item). The Beelink, on the 4B and on the 9B: calls by the intelligence of its instance, which a hosted model leads (`deepseek-v4.1-flash:cloud`, Ollama Cloud through the Ollama preset): a plain question, a system text, named tools (the call reported and not run), a limit on the answer, a name that is no tool, thinking on; the whole tool list offered ended at the call's limit on both models, with its sentence | |
| `native_ask` from a session the native model itself leads: the model calls it when asked to, the answer comes from a context of its own with the note that says so, and the turn's own prompt is back from the model server's cache at the next step | QEMU | the whole tool list offered in such a call |
| An image through `native_ask`: a file named by its path read by the model (the 4B, the 9B and the 27B each), which names the text and the shape in it; the count of images in the answer; an argument the tool does not have refused by its name; a path the tools do not read refused | QEMU. Parallels on Apple Silicon (aarch64, four processors): an image described by the 4B in three calls, each inside the tool's limit, at five to seven minutes a call. The Supermicro (four cores): an image described by the 4B, read in two minutes, and by the 9B, read in a little over three; the same request again does not read the image again | |
| What a native model is told of itself in a `native_ask` call: asked who made it, the 4B names its maker | QEMU | |
| An image in a session the native model leads: attached, sent with the message, and the text in it given back | QEMU | |
| A machine that is short of memory for a model: the wizard's question with what each model takes and what the machine has; with 5 GiB a pick whose load is more than is free is told so and asked once more, and the same pick starts its install; with 12 GiB the note at a window of 131,072 tokens and none at the default; with 16 GiB the 27B asked for, which fits at no window: the note says so, with what the smallest window takes, where it had said that a smaller window takes less | QEMU | a machine that is not a VM |
| A `native_ask` call that its limit ends: the whole tool list offered to the 27B; three calls each ended at the limit and said how many tokens of the request the model server had read, each going on from what the one before had read; the fourth took the prompt from the server's cache and asked for the right tool | QEMU | |
| A backup and a restore with a model on DATA: the snapshot without the model, the model in place and loaded after the restore | QEMU | an update staged with a model on DATA |
| A release from before native models started on a configuration that names one: the provider taken for Anthropic, the turn refused for the missing key; back on the newer release, the native settings as they were | QEMU, with the programs of `v0.28.0-phase2` | |
| A machine that is not a VM | The Beelink, its instance upgraded in place by the installer medium of the image an Intel Mac built: the 4B and the 9B each installed and loaded at the default window (the 9B with the machine's 12.6 GiB nearly full, and its model server never ended), and `native_ask` called by its intelligence; on a later build, whose image files were BF16, the catalogue's own 4B loaded with its image file, and an image described by it through `native_ask`; on the build with the F16 image files, the 4B reading text alone at the first boot and saying so, the install bringing the F16 image file in place of the BF16 one, and the same image described in the same time. The Supermicro, with four cores and 16 GB: the 4B and the 9B each loaded at the default window with its image file and `native_ask` called by its intelligence; the 4B reads about 20 tokens a second and writes about 4, the 9B reads about 11 and writes a little over 2; a call that offered the whole tool list was ended by the tool's limit on each (the 4B had read 9,806 of its 24,248 tokens, the 9B 5,710); the 9B took 6.9 GB when loaded, the figure of its catalogue entry; the model server was never ended | a session the model leads, on the Beelink; a model loaded and a session, on each of the other four |

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
| A build leaves none of its host's directories in the programs, in the web console's bundle or in the root filesystem, and the kernel's banner names the image | Ubuntu 24.04: an image build with both read-backs, its slot image unpacked and searched, and the image booted under QEMU. A Mac: the aarch64 image built with the same checks, and booted on the Pi | |
| `seed-state-mac.sh --update-key` | | on a Mac |

Which models have been vetted with each provider is in [LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md](LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md).
