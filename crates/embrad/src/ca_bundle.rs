//! Operator CA trust for the git/OpenSSL path.
//!
//! The rootfs CA bundle (`/etc/ssl/certs/ca-certificates.crt`) lives on
//! read-only SquashFS and cannot be extended at runtime. Operators who run
//! git servers behind a private CA (e.g. a self-hosted GitLab with an mkcert
//! root) drop the CA PEM into `/embra/state/ca-certificates/`; at boot we
//! merge those certs with the stock bundle into a file on the ephemeral
//! tmpfs and export `GIT_SSL_CAINFO` + `SSL_CERT_FILE` before any service
//! spawns, so every child process (embra-brain and its git subprocesses)
//! inherits them.
//!
//! INVARIANT: the exported bundle must always be a strict superset of the
//! stock roots. `GIT_SSL_CAINFO`/`SSL_CERT_FILE` REPLACE the default trust
//! store rather than append — pointing them at an extras-only file would
//! break TLS to github.com and every other public host. If the base bundle
//! cannot be read, the feature aborts and no env is exported.

use tracing::{debug, info, warn};

/// Stock bundle baked into the rootfs by Buildroot's ca-certificates package.
const CA_BASE_BUNDLE: &str = "/etc/ssl/certs/ca-certificates.crt";

/// Operator drop-in directory on the writable STATE partition.
const CA_DROPIN_DIR: &str = "/embra/state/ca-certificates";

/// Merged bundle, rebuilt every boot (ephemeral is a boot-wiped tmpfs).
const CA_MERGED_BUNDLE: &str = "/embra/ephemeral/ca-bundle.crt";

/// Dev override: when set, this directory is scanned INSTEAD of the STATE
/// drop-in dir (mirrors `EMBRA_SEED_DIR` / `EMBRA_IMPORT_DIR` semantics).
const CA_DIR_ENV: &str = "EMBRA_CA_DIR";

/// Per-file size cap. A root CA PEM is ~2 KB; this rejects accidental junk
/// (a dropped tarball, a log file) without being fussy about chains.
const MAX_CA_FILE_SIZE: u64 = 256 * 1024;

const PEM_BEGIN: &str = "-----BEGIN CERTIFICATE-----";
const PEM_END: &str = "-----END CERTIFICATE-----";

/// Count well-formed BEGIN/END CERTIFICATE pairs. This is deliberately not
/// an ASN.1 parse — git/OpenSSL do the real validation; we only filter files
/// that plainly aren't PEM certificate material.
fn pem_cert_count(content: &str) -> usize {
    let mut count = 0;
    let mut rest = content;
    while let Some(begin) = rest.find(PEM_BEGIN) {
        let after_begin = &rest[begin + PEM_BEGIN.len()..];
        match after_begin.find(PEM_END) {
            Some(end) => {
                count += 1;
                rest = &after_begin[end + PEM_END.len()..];
            }
            None => break,
        }
    }
    count
}

/// Accept/reject decision for one candidate drop-in file, separated from the
/// filesystem walk so it is unit-testable. Returns a rejection reason.
fn check_ca_file(size: u64, content: &str) -> Result<usize, String> {
    if size > MAX_CA_FILE_SIZE {
        return Err(format!("{} bytes exceeds the {} byte cap", size, MAX_CA_FILE_SIZE));
    }
    let certs = pem_cert_count(content);
    if certs == 0 {
        return Err("no BEGIN/END CERTIFICATE pair found".to_string());
    }
    Ok(certs)
}

/// Merge the stock bundle with operator certs. Extras must be pre-sorted by
/// filename (the caller sorts) so the bundle bytes are deterministic across
/// boots. Exactly one newline separates blocks regardless of input trailing
/// whitespace.
fn merge_bundle(base: &str, extras: &[(String, String)]) -> String {
    let mut out = String::with_capacity(
        base.len() + extras.iter().map(|(_, c)| c.len() + 1).sum::<usize>(),
    );
    out.push_str(base.trim_end_matches('\n'));
    out.push('\n');
    for (_, content) in extras {
        out.push_str(content.trim_end_matches('\n'));
        out.push('\n');
    }
    out
}

/// Collect valid operator certs from `dir`, sorted by filename. Invalid
/// files are warned about and skipped — a bad drop-in must never poison the
/// bundle or block boot.
fn collect_operator_certs(dir: &std::path::Path) -> Vec<(String, String)> {
    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Vec::new(), // missing dir = feature unused
    };
    let mut certs: Vec<(String, String)> = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        let name = entry.file_name().to_string_lossy().to_string();
        let is_cert_ext = matches!(
            path.extension().and_then(|e| e.to_str()),
            Some("pem") | Some("crt")
        );
        if !path.is_file() || !is_cert_ext {
            continue;
        }
        let size = entry.metadata().map(|m| m.len()).unwrap_or(u64::MAX);
        let content = if size <= MAX_CA_FILE_SIZE {
            std::fs::read_to_string(&path).unwrap_or_default()
        } else {
            String::new()
        };
        match check_ca_file(size, &content) {
            Ok(n) => {
                debug!("Operator CA file accepted: {} ({} cert(s))", name, n);
                certs.push((name, content));
            }
            Err(reason) => {
                warn!("Operator CA file skipped: {}: {}", name, reason);
            }
        }
    }
    certs.sort_by(|a, b| a.0.cmp(&b.0));
    certs
}

/// Build the merged CA bundle and export `GIT_SSL_CAINFO` + `SSL_CERT_FILE`.
///
/// Called once from `register_services()` before any service spawns (the TZ
/// precedent) — children inherit the env, and the reconciliation loop's
/// respawns inherit it too. No operator certs → no bundle, no env, behavior
/// byte-identical to before this feature existed.
pub fn setup_operator_ca_trust() {
    let dropin_dir = std::env::var(CA_DIR_ENV)
        .ok()
        .filter(|d| !d.is_empty())
        .unwrap_or_else(|| CA_DROPIN_DIR.to_string());
    let extras = collect_operator_certs(std::path::Path::new(&dropin_dir));
    if extras.is_empty() {
        debug!("No operator CA certificates in {}; using stock trust store", dropin_dir);
        return;
    }

    let base = match std::fs::read_to_string(CA_BASE_BUNDLE) {
        Ok(b) if !b.is_empty() => b,
        Ok(_) | Err(_) => {
            // Never export an extras-only bundle — see the module INVARIANT.
            warn!(
                "Operator CA trust DISABLED: cannot read base bundle {} — \
                 exporting operator certs alone would break public-host TLS",
                CA_BASE_BUNDLE
            );
            return;
        }
    };

    let merged = merge_bundle(&base, &extras);
    if let Err(e) = std::fs::write(CA_MERGED_BUNDLE, &merged) {
        warn!("Operator CA trust DISABLED: cannot write {}: {}", CA_MERGED_BUNDLE, e);
        return;
    }

    // SAFETY: called once at startup before services are spawned
    unsafe {
        std::env::set_var("GIT_SSL_CAINFO", CA_MERGED_BUNDLE);
        std::env::set_var("SSL_CERT_FILE", CA_MERGED_BUNDLE);
    }
    let total_certs: usize = extras.iter().map(|(_, c)| pem_cert_count(c)).sum();
    let names: Vec<&str> = extras.iter().map(|(n, _)| n.as_str()).collect();
    info!(
        "Operator CA trust: merged {} cert(s) from {} file(s) [{}] into {}; \
         GIT_SSL_CAINFO + SSL_CERT_FILE exported",
        total_certs,
        extras.len(),
        names.join(", "),
        CA_MERGED_BUNDLE
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const CERT_A: &str = "-----BEGIN CERTIFICATE-----\nAAAA\n-----END CERTIFICATE-----\n";
    const CERT_B: &str = "-----BEGIN CERTIFICATE-----\nBBBB\n-----END CERTIFICATE-----";

    #[test]
    fn pem_cert_count_counts_pairs() {
        assert_eq!(pem_cert_count(CERT_A), 1);
        assert_eq!(pem_cert_count(&format!("{}{}", CERT_A, CERT_B)), 2);
        assert_eq!(pem_cert_count("-----BEGIN CERTIFICATE-----\nAAAA\n"), 0);
        assert_eq!(pem_cert_count(""), 0);
        assert_eq!(pem_cert_count("not a certificate"), 0);
    }

    #[test]
    fn check_ca_file_rejects_oversize_and_garbage() {
        assert!(check_ca_file(MAX_CA_FILE_SIZE + 1, CERT_A).is_err());
        assert!(check_ca_file(10, "garbage").is_err());
        assert_eq!(check_ca_file(CERT_A.len() as u64, CERT_A), Ok(1));
    }

    #[test]
    fn merge_bundle_single_newline_separation() {
        let base = "BASE-CERTS\n\n";
        let extras = vec![
            ("a.pem".to_string(), CERT_A.to_string()),
            ("b.crt".to_string(), CERT_B.to_string()),
        ];
        let merged = merge_bundle(base, &extras);
        assert_eq!(
            merged,
            format!("BASE-CERTS\n{}\n{}\n", CERT_A.trim_end_matches('\n'), CERT_B)
        );
    }

    #[test]
    fn merge_bundle_empty_extras_is_base_normalized() {
        assert_eq!(merge_bundle("BASE\n", &[]), "BASE\n");
        assert_eq!(merge_bundle("BASE", &[]), "BASE\n");
    }

    #[test]
    fn merge_bundle_is_deterministic() {
        let extras = vec![("a.pem".to_string(), CERT_A.to_string())];
        assert_eq!(merge_bundle("BASE", &extras), merge_bundle("BASE", &extras));
    }

    #[test]
    fn collect_operator_certs_skips_invalid_keeps_valid_sorted() {
        let dir = std::env::temp_dir().join(format!("embrad-ca-test-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("b-valid.pem"), CERT_B).unwrap();
        std::fs::write(dir.join("a-valid.crt"), CERT_A).unwrap();
        std::fs::write(dir.join("garbage.pem"), "not a cert").unwrap();
        std::fs::write(dir.join("ignored.txt"), CERT_A).unwrap();
        let certs = collect_operator_certs(&dir);
        std::fs::remove_dir_all(&dir).ok();
        let names: Vec<&str> = certs.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["a-valid.crt", "b-valid.pem"]);
    }

    #[test]
    fn collect_operator_certs_missing_dir_is_empty() {
        assert!(collect_operator_certs(std::path::Path::new("/nonexistent/embrad-ca")).is_empty());
    }
}
