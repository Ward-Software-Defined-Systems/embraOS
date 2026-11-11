# Updates and backups

How embraOS keeps itself whole across an OS update and how an operator takes a backup from inside the OS. The host-side script `scripts/embraos-backup.sh` stays the path for an image rebuild and for moving a backup off the machine ([BUILD/QUICK-START.md](../BUILD/QUICK-START.md)); everything below runs on the machine itself.

## The disk and the two OS slots

The image is a GPT aligned to 1 MiB with five partitions (`buildroot/board/embraos/layout.sh` names them, in order):

| Partition | Size | Content |
|---|---|---|
| `esp` | 64 MiB | The EFI System Partition: GRUB (`EFI/BOOT/BOOTX64.EFI` or `BOOTAA64.EFI`), `grub.cfg`, `grubenv` |
| `rootfs_a` | 2 GiB | OS slot A: one SquashFS with the kernel and the initramfs under `/boot`, the release stamp at `/etc/embraos-release` |
| `rootfs_b` | 2 GiB | OS slot B, empty until the first update |
| `state` | 256 MiB | STATE: the seal, the CA, the operator's settings and drop-ins |
| `data` | 4 GiB | DATA: the database, the workspace, and the backups under `backups/` |

The slots were 1 GiB until 2026-11-08 and are 2 GiB since the firmware for bare metal joined the image; a slot's size is fixed on the disk that carries it, and the updater refuses a release larger than the slot partition it would write, so a disk installed or deployed with the older layout takes no release that outgrows 1 GiB. DATA is the last partition, and it grows to the disk's end at boot when a gibibyte or more follows it: the installer leaves no gap, but a VM's virtual disk enlarged afterwards has one. `embra-update grow-data` runs before the services at every normal boot (embrad, the database stopped): sgdisk moves the backup GPT to the new end and recreates the entry from the same start, the kernel is told the new length (`BLKPG_RESIZE_PARTITION`, while the partition is mounted), and resize2fs grows ext4 online; one line on both consoles when it grew, nothing when there was nothing to do, and a failure is a line and a normal boot with the partition as it was. The disk is 8514 MiB, a whole number of MiB: 1 MiB before the first partition, the partitions, and 1 MiB that holds the backup GPT on the last sector (`layout.sh` computes the size; the build reads it back from the image). The converted disks in the folders beside the raw image, `vmware/embraos.ova` and `vmware/embraos.vmdk`, `hyper-v/embraos.vhdx` and `parallels/embraos.vmdk`, have the same capacity.

GRUB boots the active slot by default and shows its menu for three seconds: each slot that holds a release, a Diagnostics submenu, Update embraOS, Backup and restore, Reboot, Halt and Firmware setup. A slot without `/boot/embraos-slot.cfg` has no entry and no "Make slot … the default" or "Boot slot … once" line under Diagnostics: a fresh image lists slot A alone, and slot B appears after the first update. `grubenv` holds the A/B state (`slot_active`, a staged `slot_try` with its `tries_left`, a status per slot). A slot an update staged is booted with `embra.trial=1` while it has tries left; embrad confirms it once every service is healthy, and GRUB marks it failed and falls back to the previous slot when the tries run out.

## Backups

A backup is a snapshot of STATE and DATA: a `tar.gz` under `/embra/data/backups/<timestamp>_<label>/` with a `backup.json` record beside it (the label, who took it, the release, the database engine, the counts, the sizes, the archive's SHA-256, what was left out). It holds STATE whole but the halt reason and the next-boot marker, DATA's database directory whole, and DATA's workspace but the Guardian's build tree (its built artifacts are kept) and cargo home. Links stay links; special files are skipped and named in the record.

A snapshot runs only while the database is stopped, so a copy of its files is consistent: before the services start. Two ways to ask for one:

- **From a session:** `/backup create [label]` schedules it for the next boot and `/reboot now` boots; embrad takes the snapshot before WardSONDB starts and the boot continues into the session. `/backup status` shows what is scheduled.
- **From the boot menu:** Backup and restore → Back up STATE and DATA. embrad takes the snapshot, its progress on both consoles (the serial line and the display), and reboots to the menu.

The free-space rule: DATA must have the snapshot's size plus 256 MiB free, or the snapshot is refused with both figures. `/backup list` shows every backup, newest first, each complete one with its download route (`web: /api/backups/<name>`): the web console streams the archive to the browser behind the console's token, with no size cap. `/backup delete <name>` removes one. The automatic snapshots an update takes (labels `pre-<version>`) rotate: the newest two stay; a snapshot with any other label is never removed on its own.

The archives are outside the workspace and the model's tools are refused them (`file_read` and the rest answer that the backups carry the operator's STATE). The host script's backup of DATA leaves `backups/` out: each is a copy of STATE and DATA already.

## Restoring

A restore replaces STATE and DATA's database and workspace with a snapshot and keeps the other backups. It runs before the services start, like a snapshot:

- **From a session:** `/backup restore <name>` answers with the confirmation to type, `restore <name>`; anything else, a slash command included, cancels. Confirmed, the next boot restores and reboots: `/reboot now` when ready.
- **From the boot menu:** Backup and restore → Restore STATE and DATA. A numbered list on both consoles, the number asked twice and answered on whichever console the operator types on (the first line typed wins and is echoed on the other), anything else reboots without restoring.

A restore refuses an incomplete entry (a `.partial` directory a crash left, which `/backup delete` removes), an archive that does not match its record, and a snapshot whose database engine is not the running release's (a database of the other engine cannot be opened; `seed-state.sh --wipe data` on the host is the way to start over). The sealed identity comes back whole: STATE's `soul.sha256` and DATA's soul document are from one snapshot, so trustd's seal check passes at the next boot.

## Updates

An OS update is a new slot image written into the inactive slot and tried by GRUB; the running slot is untouched and is where GRUB returns if the new release does not come up. The operator decides: `/update apply` in a session, or Update embraOS in the boot menu. The intelligence has no tool for it.

### What a release publishes

`scripts/release-update.sh --tag <vX.Y.Z-phaseN> --arch <x86_64|aarch64> --engine <fjall|rocksdb> --key <private.pem>` prepares, per architecture and engine, three files under `output/update/` and prints their upload lines (uploading to the GitHub release is the operator's step; the private key never enters the repository or CI):

| File | Content |
|---|---|
| `embraos-<tag>-<arch>-<engine>.rootfs.squashfs` | The slot image the build produced: one SquashFS with the kernel and the initramfs under `/boot` and the release stamp at `/etc/embraos-release` |
| `embraos-<arch>-<engine>.manifest.json` | `update.v1`: the version, tag, build id, architecture, engine, schema version, kernel, the rootfs file's name, size and SHA-256, when it was built, the notes URL. The name carries no version: the OS fetches it from the release marked latest |
| `embraos-<arch>-<engine>.manifest.json.sig` | A raw Ed25519 signature over the manifest's bytes, base64, made with `openssl pkeyutl -sign -rawin` and the operator's key |

The script refuses a tag that is not the one the tree is at or not the version `CITATION.cff` records, a dirty tree, and a slot image whose stamp names another architecture or engine or whose `/boot` lacks one of the five files GRUB reads from a slot (the kernel, `ucode.cpio`, `firmware.cpio.gz`, `initramfs.cpio.gz`, `embraos-slot.cfg`); it verifies its own signature against `scripts/update-signing.pub` before it prints anything. It signs no release that would trust a certificate authority of the builder's own ([Imported_CAs/README.md](../../Imported_CAs/README.md)): a slot image that carries files under `/usr/share/embra/ca-certificates/`, a tree whose `Imported_CAs/` holds certificate files, and a slot image it cannot read for want of `unsquashfs` (Ubuntu: `squashfs-tools`; macOS: `brew install squashfs`) each stop it before anything is written, and `--allow-imported-cas` signs all the same, for a release meant for machines of one's own. With an xorriso at hand the ISO must be the medium of the same root filesystem (`ROOTFS_SHA256` in its manifest), so what was checked of the slot image holds for the installer medium. Every release uploads all four manifests (two architectures, two engines), or an instance of the missing pair reads "no manifest for … is published at this source". `--self-test` signs a fixture with a throwaway key.

### What the OS trusts

The OS verifies a manifest's signature against the keys in `/usr/share/embra/update-signing.pub` (the repository's `scripts/update-signing.pub`, baked into the image) and in the operator's drop-in `/embra/state/update-signing.pub` (`seed-state.sh --update-key <pem>`), before it reads anything of the manifest. The file may hold several keys: a new key goes in beside the old one release before the old one stops signing, so an instance on the previous release still verifies the next. Until a key is present the updater refuses every manifest. The key pair is made once with `openssl genpkey -algorithm ed25519 -out update-signing.key` and `openssl pkey -in update-signing.key -pubout`.

The signature binds the version, the tag, the build id, the architecture, the engine and the image's hash to the operator's key: the URL, a mirror, a DNS answer or a certificate cannot hand the OS an image it will boot. It does not protect against the signing host itself; rotation limits that window. A signed manifest for an older version is a downgrade and refused unless `--allow-downgrade` is given to the updater; one for another architecture or engine is refused before any download; the data's own engine marker is what the manifest is compared with.

### Where the manifest comes from

The default source is the GitHub releases of the public mirror, `https://github.com/Ward-Software-Defined-Systems/embraOS/releases/latest/download` (no API, no token: GitHub answers the fixed name with the latest non-draft, non-prerelease release). `/update source <url>` stores the operator's own base under `/embra/state/update-source` — `https://…`, or `http://` to a private address, such as a GitLab generic package registry behind the operator's CA (the updater trusts the CA drop-ins embrad merges for every service) — and `/update source reset` returns to the default. The image is always fetched from the same base under the name the manifest gives, never from a URL inside the manifest, so a mirror serves the same signed manifest. `embra-update check --tag <v…>` reads one release's own files on GitHub.

### Applying

`/update` reports the running release, the available one with its schema step and notes, and the verdict. `/update apply` runs the updater as a child of the brain, one at a time, its lines in the console:

1. The manifest is fetched and verified again, and the inactive slot must not be the slot GRUB may still fall back to (a staged slot untried, or a trial boot in progress).
2. The image is streamed into the inactive slot's partition, hashed as it is written, refused past its declared size, synced, read back and hashed again; both digests must equal the signed one. The slot's previous content is gone from the first byte, the one sacrifice of two slots; a half-written slot is simply unmarked and never booted.
3. `grubenv` is written: the slot staged with three tries. Nothing else moves; the running slot stays the default until the new one confirms.

`/reboot now` boots the staged slot (`embra.trial=1` on its kernel line). Before any service starts, the new release takes a snapshot of STATE and DATA (`pre-<version>`, kept to the newest two), so the previous release has a consistent copy to come back to; a snapshot it cannot take halts, which counts the try. Its brain then migrates the knowledge graph, and once every service is healthy embrad confirms the slot: active from now on. A slot that never confirms is tried three times and then marked failed, and GRUB boots the previous one. `/update status` shows both slots; `/update rollback` makes the other slot the default by hand (`/reboot now` boots it).

A rollback onto a knowledge graph the newer release migrated: the older brain refuses to start — the halt reason names both schemas and the `pre-` snapshot — and the boot menu's Restore entry puts that snapshot back. The Guardian's dynamic tools are rebuilt with `/guardian rebuild --all` when a release moves the in-OS toolchain ([COMMAND-REFERENCE.md](COMMAND-REFERENCE.md)).

A release also publishes the installer medium, one per architecture and engine: `embraos-<tag>-<arch>-<engine>.iso` (the x86_64 one is about 900 MB with the firmware), beside it `…iso.sha256`, its `sha256sum` line, and `…iso.sha256.sig`, the same Ed25519 signature over that line as the manifests carry, made with the operator's key (`scripts/release-update.sh` writes the three and names them in its upload line; with an xorriso at hand it refuses an ISO whose own manifest names another architecture, engine or version). The aarch64 medium is validated on Apple Silicon under QEMU and under VMware Fusion (2026-11-11, a build's own `embraos.iso`; [AARCH64-BUILD.md](../BUILD/AARCH64-BUILD.md)). To check a download: `sha256sum -c embraos-<tag>-<arch>-<engine>.iso.sha256`, and `openssl pkeyutl -verify -rawin -pubin -inkey <the key from scripts/update-signing.pub> -in …iso.sha256 -sigfile <(base64 -d …iso.sha256.sig)`. The OS never downloads the ISO; it is for a fresh install, and the images it writes update from the manifests like any other.

From the boot menu, Update embraOS boots the active slot with `embra.mode=update`: embrad brings the network up and runs the updater on both consoles (`Apply … to slot b? [y/N]`, answered on the serial line or at the display's keyboard), then reboots into the staged slot, or back to the menu with the reason.
