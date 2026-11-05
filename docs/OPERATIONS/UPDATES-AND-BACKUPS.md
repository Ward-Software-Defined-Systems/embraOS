# Updates and backups

How embraOS keeps itself whole across an OS update and how an operator takes a backup from inside the OS. The host-side script `scripts/embraos-backup.sh` stays the path for an image rebuild and for moving a backup off the machine ([BUILD/QUICK-START.md](../BUILD/QUICK-START.md)); everything below runs on the machine itself.

## The disk and the two OS slots

The image is a GPT aligned to 1 MiB with five partitions (`buildroot/board/embraos/layout.sh` names them, in order):

| Partition | Size | Content |
|---|---|---|
| `esp` | 64 MiB | The EFI System Partition: GRUB (`EFI/BOOT/BOOTX64.EFI` or `BOOTAA64.EFI`), `grub.cfg`, `grubenv` |
| `rootfs_a` | 1 GiB | OS slot A: one SquashFS with the kernel and the initramfs under `/boot`, the release stamp at `/etc/embraos-release` |
| `rootfs_b` | 1 GiB | OS slot B, empty until the first update |
| `state` | 256 MiB | STATE: the seal, the CA, the operator's settings and drop-ins |
| `data` | 4 GiB | DATA: the database, the workspace, and the backups under `backups/` |

GRUB boots the active slot by default and shows its menu for three seconds: each slot, a Diagnostics submenu, Update embraOS, Backup and restore, Reboot, Halt and Firmware setup. `grubenv` holds the A/B state (`slot_active`, a staged `slot_try` with its `tries_left`, a status per slot). A slot an update staged is booted with `embra.trial=1` while it has tries left; embrad confirms it once every service is healthy, and GRUB marks it failed and falls back to the previous slot when the tries run out.

## Backups

A backup is a snapshot of STATE and DATA: a `tar.gz` under `/embra/data/backups/<timestamp>_<label>/` with a `backup.json` record beside it (the label, who took it, the release, the database engine, the counts, the sizes, the archive's SHA-256, what was left out). It holds STATE whole but the halt reason and the next-boot marker, DATA's database directory whole, and DATA's workspace but the Guardian's build tree (its built artifacts are kept) and cargo home. Links stay links; special files are skipped and named in the record.

A snapshot runs only while the database is stopped, so a copy of its files is consistent: before the services start. Two ways to ask for one:

- **From a session:** `/backup create [label]` schedules it for the next boot and `/reboot now` boots; embrad takes the snapshot before WardSONDB starts and the boot continues into the session. `/backup status` shows what is scheduled.
- **From the boot menu:** Backup and restore → Back up STATE and DATA. embrad takes the snapshot on the console and reboots to the menu.

The free-space rule: DATA must have the snapshot's size plus 256 MiB free, or the snapshot is refused with both figures. `/backup list` shows every backup, newest first, each complete one with its download route (`web: /api/backups/<name>`): the web console streams the archive to the browser behind the console's token, with no size cap. `/backup delete <name>` removes one. The automatic snapshots an update takes (labels `pre-<version>`) rotate: the newest two stay; a snapshot with any other label is never removed on its own.

The archives are outside the workspace and the model's tools are refused them (`file_read` and the rest answer that the backups carry the operator's STATE). The host script's backup of DATA leaves `backups/` out: each is a copy of STATE and DATA already.

## Restoring

A restore replaces STATE and DATA's database and workspace with a snapshot and keeps the other backups. It runs before the services start, like a snapshot:

- **From a session:** `/backup restore <name>` answers with the confirmation to type, `restore <name>`; anything else, a slash command included, cancels. Confirmed, the next boot restores and reboots: `/reboot now` when ready.
- **From the boot menu:** Backup and restore → Restore STATE and DATA. A numbered list on the console, the number asked twice, anything else reboots without restoring.

A restore refuses an incomplete entry (a `.partial` directory a crash left, which `/backup delete` removes), an archive that does not match its record, and a snapshot whose database engine is not the running release's (a database of the other engine cannot be opened; `seed-state.sh --wipe data` on the host is the way to start over). The sealed identity comes back whole: STATE's `soul.sha256` and DATA's soul document are from one snapshot, so trustd's seal check passes at the next boot.

## Updates

The signed-release update into the inactive slot is the next wave of Phase 2; this section is written with it.
