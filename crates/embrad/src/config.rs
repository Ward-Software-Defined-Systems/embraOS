//! Boot configuration for embrad.
//!
//! Reads configuration from environment variables and STATE partition.

/// Paths used by embrad
pub const STATE_DIR: &str = "/embra/state";
pub const DATA_DIR: &str = "/embra/data";
pub const EPHEMERAL_DIR: &str = "/embra/ephemeral";

pub const SOUL_HASH_FILE: &str = "/embra/state/soul.sha256";
pub const PKI_DIR: &str = "/embra/state/pki";
pub const HALT_REASON_FILE: &str = "/embra/state/halt_reason";

/// Service binary paths (in the immutable rootfs)
pub const WARDSONDB_BIN: &str = "/usr/bin/wardsondb";
pub const TRUSTD_BIN: &str = "/usr/bin/embra-trustd";
pub const APID_BIN: &str = "/usr/bin/embra-apid";
pub const BRAIN_BIN: &str = "/usr/bin/embra-brain";
pub const CONSOLE_BIN: &str = "/usr/bin/embra-console";
