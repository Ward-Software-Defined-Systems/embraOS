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
//! A builder's own CAs come the same way from a second directory, the
//! image's: the `*.pem` and `*.crt` files of `Imported_CAs/` at build time,
//! baked read-only into `/usr/share/embra/ca-certificates/`
//! (`scripts/lib/imported-cas.sh`). A machine installed from the ISO, a
//! hypervisor's disk or bare metal has no step that fills STATE, so a CA it
//! needs travels in the image. Both directories are merged, the image's
//! first.
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

/// The certificate authorities the image was built with (`Imported_CAs/`,
/// baked by `post_build.sh`): read-only, the same on every machine of the
/// build. The build's `IMPORTED_CAS_ROOTFS_DIR` and the brain's
/// `net::IMAGE_CA_DIR` name the same directory (the brain's guard
/// `the_image_s_ca_directory_is_the_one_the_build_bakes_and_embrad_merges`
/// reads all three).
const CA_IMAGE_DIR: &str = "/usr/share/embra/ca-certificates";

/// Merged bundle, rebuilt every boot (ephemeral is a boot-wiped tmpfs).
const CA_MERGED_BUNDLE: &str = "/embra/ephemeral/ca-bundle.crt";

/// Dev override: when set, this directory is scanned INSTEAD of the image's
/// and the STATE drop-in dir (mirrors `EMBRA_SEED_DIR` / `EMBRA_IMPORT_DIR`
/// semantics).
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

/// Where the extra certificates come from, in merge order, each with the
/// word the log names it by: the image's, then the operator's on STATE. The
/// development override, when set and not empty, replaces both.
fn ca_sources(dev_override: Option<String>) -> Vec<(&'static str, String)> {
    match dev_override.filter(|d| !d.is_empty()) {
        Some(dir) => vec![(CA_DIR_ENV, dir)],
        None => vec![("image", CA_IMAGE_DIR.to_string()), ("state", CA_DROPIN_DIR.to_string())],
    }
}

/// The certificates of every source, in the sources' order and by filename
/// within one. Each file is named `<source>/<file>`, so the log line says
/// which the image brought and which the operator dropped in, and a name
/// both carry stays two entries.
fn collect_from(sources: &[(&'static str, String)]) -> Vec<(String, String)> {
    let mut all = Vec::new();
    for (label, dir) in sources {
        for (name, content) in collect_operator_certs(std::path::Path::new(dir)) {
            all.push((format!("{label}/{name}"), content));
        }
    }
    all
}

/// The bundle to export, or none: the module's INVARIANT as a function.
/// `Some` only when the stock base bundle was read and is not empty and
/// there are operator certificates to add; the merge then starts with the
/// base, so the exported store is a strict superset of the stock roots.
/// An unreadable or empty base with extras present is `None`: nothing is
/// exported, because an extras-only store would refuse every public host.
fn plan_bundle(base: Option<&str>, extras: &[(String, String)]) -> Option<String> {
    let base = base.filter(|b| !b.is_empty())?;
    if extras.is_empty() {
        return None;
    }
    Some(merge_bundle(base, extras))
}

/// Build the merged CA bundle and export `GIT_SSL_CAINFO` + `SSL_CERT_FILE`.
///
/// Called once from `register_services()` before any service spawns (the TZ
/// precedent) — children inherit the env, and the reconciliation loop's
/// respawns inherit it too. No operator certs → no bundle, no env, behavior
/// byte-identical to before this feature existed.
pub fn setup_operator_ca_trust() {
    let sources = ca_sources(std::env::var(CA_DIR_ENV).ok());
    let extras = collect_from(&sources);
    if extras.is_empty() {
        let dirs: Vec<&str> = sources.iter().map(|(_, d)| d.as_str()).collect();
        debug!("No CA certificates in {}; using stock trust store", dirs.join(" or "));
        return;
    }

    let base = std::fs::read_to_string(CA_BASE_BUNDLE).ok();
    let Some(merged) = plan_bundle(base.as_deref(), &extras) else {
        // Never export an extras-only bundle — see the module INVARIANT.
        warn!(
            "Operator CA trust DISABLED: cannot read base bundle {} — \
             exporting operator certs alone would break public-host TLS",
            CA_BASE_BUNDLE
        );
        return;
    };
    if let Err(e) = std::fs::write(CA_MERGED_BUNDLE, &merged) {
        warn!("Operator CA trust DISABLED: cannot write {}: {}", CA_MERGED_BUNDLE, e);
        return;
    }

    // SAFETY: one thread runs — `supervisor::prepare_environment` calls this
    // from `main` before the tokio runtime is built — so no other thread
    // reads the environment meanwhile.
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

    /// The INVARIANT at the decision: no base, an empty base or no extras
    /// export nothing; a real base with extras exports a superset that
    /// starts with the base (#52).
    #[test]
    fn plan_bundle_exports_a_superset_of_the_base_or_nothing() {
        let extras = vec![("op.crt".to_string(), CERT_A.to_string())];
        assert_eq!(plan_bundle(None, &extras), None);
        assert_eq!(plan_bundle(Some(""), &extras), None);
        assert_eq!(plan_bundle(Some("ROOTS"), &[]), None);
        let merged = plan_bundle(Some("ROOTS\n"), &extras).expect("a bundle");
        assert!(merged.starts_with("ROOTS"), "{merged}");
        assert!(merged.contains(CERT_A.trim()), "{merged}");
        assert_eq!(merged, merge_bundle("ROOTS\n", &extras));
    }

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

    /// The image's directory and then STATE's; the development override,
    /// when it names a directory, stands for both.
    #[test]
    fn the_sources_are_the_image_then_state_and_the_override_replaces_both() {
        let both = vec![("image", CA_IMAGE_DIR.to_string()), ("state", CA_DROPIN_DIR.to_string())];
        assert_eq!(ca_sources(None), both);
        assert_eq!(ca_sources(Some(String::new())), both, "an empty override is no override");
        assert_eq!(ca_sources(Some("/tmp/dev-ca".into())), vec![(CA_DIR_ENV, "/tmp/dev-ca".to_string())]);
        assert_eq!(CA_IMAGE_DIR, "/usr/share/embra/ca-certificates");
        assert_eq!(CA_DROPIN_DIR, "/embra/state/ca-certificates");
    }

    /// A build's certificates and the operator's reach one bundle: the
    /// image's first whatever the file names, each entry named by its
    /// source, a name both directories carry kept twice, and a source that
    /// is not there adding nothing.
    #[test]
    fn the_image_s_certificates_are_merged_before_the_operator_s() {
        let root = std::env::temp_dir().join(format!("embrad-ca-sources-{}", std::process::id()));
        let (image, state) = (root.join("image"), root.join("state"));
        std::fs::create_dir_all(&image).unwrap();
        std::fs::create_dir_all(&state).unwrap();
        std::fs::write(image.join("z-build.pem"), CERT_A).unwrap();
        std::fs::write(image.join("shared.crt"), CERT_A).unwrap();
        std::fs::write(image.join("garbage.pem"), "not a cert").unwrap();
        std::fs::write(state.join("a-operator.pem"), CERT_B).unwrap();
        std::fs::write(state.join("shared.crt"), CERT_B).unwrap();
        let s = |p: &std::path::Path| p.to_string_lossy().into_owned();
        let sources = vec![("image", s(&image)), ("state", s(&state))];
        let found = collect_from(&sources);
        let names: Vec<&str> = found.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(names, vec!["image/shared.crt", "image/z-build.pem", "state/a-operator.pem", "state/shared.crt"]);
        let merged = plan_bundle(Some("ROOTS\n"), &found).expect("a bundle");
        assert!(merged.starts_with("ROOTS\n"), "the stock roots first: {merged}");
        assert_eq!(merged.matches(CERT_A.trim()).count(), 2, "both of the image's files");
        assert_eq!(merged.matches(CERT_B.trim()).count(), 2, "both of the operator's");
        assert!(merged.find(CERT_A.trim()).unwrap() < merged.find(CERT_B.trim()).unwrap(), "the image's before the operator's");

        let only_image = collect_from(&[("image", s(&image)), ("state", s(&root.join("absent")))]);
        assert_eq!(only_image.len(), 2, "an install with nothing on STATE trusts the image's alone");
        let only_state = collect_from(&[("image", s(&root.join("absent"))), ("state", s(&state))]);
        assert_eq!(only_state.len(), 2, "an image built with none trusts the operator's alone");
        assert!(collect_from(&[("image", s(&root.join("absent"))), ("state", s(&root.join("absent")))]).is_empty());
        std::fs::remove_dir_all(&root).ok();
    }
}
