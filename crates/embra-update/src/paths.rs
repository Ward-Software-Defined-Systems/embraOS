//! Where things are in the OS. The snapshot logic takes its roots as a
//! value (`snapshot::Roots`), so a test runs it under a temporary tree.

/// STATE and DATA are mounted under this directory.
pub const EMBRA_ROOT: &str = "/embra";
/// The backups, on DATA and outside the workspace (the brain's read jail
/// refuses the directory: an archive carries STATE's secrets).
pub const BACKUPS_DIR: &str = "/embra/data/backups";
/// The database's directory under DATA, copied whole.
pub const DATABASE_DIR: &str = "wardsondb";
/// The workspace under DATA (bind-mounted at /embra/workspace in the OS).
pub const WORKSPACE_DIR: &str = "workspace";
/// WardSONDB's engine marker under its directory.
pub const ENGINE_MARKER: &str = ".engine";
/// The Guardian's build tree under the workspace: left out, except the
/// built artifacts boot reconcile loads.
pub const GUARDIAN_TARGET: &str = ".guardian/target";
pub const GUARDIAN_ARTIFACTS: &str = ".guardian/target/wasm32-unknown-unknown/release";
pub const GUARDIAN_CARGO_HOME: &str = ".guardian/cargo-home";
/// STATE files a snapshot leaves out: a halt's reason, the next-boot marker,
/// and the last time the machine saved its clock, which belongs to the
/// machine and not to what a snapshot holds.
pub const STATE_EXCLUDED: &[&str] = &["halt_reason", "next-boot", "lost+found", embra_boot::clock::LAST_KNOWN_NAME];
/// STATE entries a restore leaves in place while it empties the rest: the
/// filesystem's own directory, and the saved clock (a restore must not take
/// a board whose clock battery is dead back to the image's build time).
pub const STATE_KEPT_BY_RESTORE: &[&str] = &["lost+found", embra_boot::clock::LAST_KNOWN_NAME];
/// The database's loopback address; a snapshot refuses while it answers.
pub const DB_ADDR: &str = "127.0.0.1:8090";
/// What stays free on DATA beside a snapshot: the database's compaction and
/// a Guardian build need room while the archive exists.
pub const RESERVE_BYTES: u64 = 256 * 1024 * 1024;
/// Automatic snapshots (the ones taken before a release first runs) kept.
pub const KEEP_AUTOMATIC: usize = 2;
pub const AUTOMATIC_PREFIX: &str = "pre-";
pub const ARCHIVE: &str = "backup.tar.gz";
pub const RECORD: &str = "backup.json";
pub const PARTIAL_SUFFIX: &str = ".partial";
/// `taken_by` of a backup that came from outside (`import`): it is never
/// rotated and never stands in for a snapshot this machine took.
pub const UPLOADED: &str = "uploaded";
pub const FORMAT: &str = "backup.v1";
