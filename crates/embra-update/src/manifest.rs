//! A release manifest (`update.v1`): what a release is and the slot image it
//! names, trusted only once an Ed25519 signature over its bytes verifies
//! under one of the operator's keys. Parsed after the signature, never
//! before.

use base64::Engine as _;
use embra_boot::Release;
use ring::signature::{ED25519, UnparsedPublicKey};
use serde::Deserialize;

pub const FORMAT: &str = "update.v1";

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct Manifest {
    pub format: String,
    pub version: String,
    pub tag: String,
    #[serde(default)]
    pub build_id: String,
    pub arch: String,
    pub engine: String,
    pub schema_version: u32,
    #[serde(default)]
    pub kernel: String,
    pub rootfs: RootfsEntry,
    #[serde(default)]
    pub built: String,
    #[serde(default)]
    pub notes_url: String,
}

#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct RootfsEntry {
    pub file: String,
    #[serde(default)]
    pub url: String,
    pub size: u64,
    pub sha256: String,
}

/// Verify `sig_b64` over `bytes` under any of `keys`, then parse. The
/// signature is checked first: nothing of an unsigned manifest is read.
pub fn verify(bytes: &[u8], sig_b64: &str, keys: &[[u8; 32]]) -> Result<Manifest, String> {
    if keys.is_empty() {
        return Err("no release signing key is installed (/usr/share/embra/update-signing.pub, or the operator's /embra/state/update-signing.pub)".into());
    }
    let sig = base64::engine::general_purpose::STANDARD
        .decode(sig_b64.trim())
        .map_err(|e| format!("the signature is not base64: {e}"))?;
    if sig.len() != 64 {
        return Err(format!("the signature is {} bytes, not the 64 of Ed25519", sig.len()));
    }
    let verified = keys.iter().enumerate().find(|(_, k)| UnparsedPublicKey::new(&ED25519, k).verify(bytes, &sig).is_ok());
    let Some((index, _)) = verified else {
        return Err(format!("the manifest's signature verifies under none of the {} installed key(s)", keys.len()));
    };
    let m: Manifest = serde_json::from_slice(bytes).map_err(|e| format!("the manifest does not parse: {e}"))?;
    if m.format != FORMAT {
        return Err(format!("the manifest is a {} document, not {FORMAT}", m.format));
    }
    if m.rootfs.sha256.len() != 64 || !m.rootfs.sha256.chars().all(|c| c.is_ascii_hexdigit()) {
        return Err("the manifest's rootfs sha256 is not 64 hex characters".into());
    }
    let _ = index;
    Ok(m)
}

/// `MAJOR.MINOR.PATCH` of a version, the suffix after `-` dropped.
pub fn version_triple(v: &str) -> Option<(u64, u64, u64)> {
    let core = v.split('-').next()?;
    let mut parts = core.split('.').map(|p| p.parse::<u64>().ok());
    let a = parts.next()??;
    let b = parts.next()??;
    let c = parts.next()??;
    parts.next().is_none().then_some((a, b, c))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Verdict {
    /// The manifest's version is above the running one.
    Newer,
    /// The same version and the same build.
    SameBuild,
    /// The same version, another build id.
    SameVersionOtherBuild,
    /// The manifest's version is below the running one.
    Older,
    /// Another architecture or engine, named.
    Mismatch(String),
}

/// The manifest against the running release; DATA's engine marker (what the
/// data is) wins over the release's baked engine when present.
pub fn compare(release: &Release, data_engine: Option<&str>, m: &Manifest) -> Verdict {
    if m.arch != release.arch {
        return Verdict::Mismatch(format!("the manifest is for {}, this machine is {}", m.arch, release.arch));
    }
    let engine = data_engine.unwrap_or(release.engine.as_str());
    if m.engine != engine {
        return Verdict::Mismatch(format!("the manifest is built for the {} engine, this machine runs {engine}", m.engine));
    }
    version_verdict(&m.version, &m.build_id, &release.version, &release.build_id)
}

/// A release (`version`, `build_id`) against the one that is installed: the
/// `MAJOR.MINOR.PATCH` triples, then the build ids. The update and the
/// installer's upgrade in place share it; neither architecture nor engine
/// is looked at here.
pub fn version_verdict(version: &str, build_id: &str, installed_version: &str, installed_build_id: &str) -> Verdict {
    match (version_triple(version), version_triple(installed_version)) {
        (Some(theirs), Some(ours)) if theirs > ours => Verdict::Newer,
        (Some(theirs), Some(ours)) if theirs < ours => Verdict::Older,
        (Some(_), Some(_)) => {
            if build_id == installed_build_id {
                Verdict::SameBuild
            } else {
                Verdict::SameVersionOtherBuild
            }
        }
        _ => {
            if version == installed_version && build_id == installed_build_id {
                Verdict::SameBuild
            } else {
                Verdict::Mismatch(format!("a version that does not compare: {version} against {installed_version}"))
            }
        }
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use ring::signature::{Ed25519KeyPair, KeyPair};

    /// A throwaway key pair; `(key, public 32 bytes)`.
    pub fn key_pair() -> (Ed25519KeyPair, [u8; 32]) {
        let rng = ring::rand::SystemRandom::new();
        let doc = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let kp = Ed25519KeyPair::from_pkcs8(doc.as_ref()).unwrap();
        let mut public = [0u8; 32];
        public.copy_from_slice(kp.public_key().as_ref());
        (kp, public)
    }

    pub fn sign_b64(kp: &Ed25519KeyPair, bytes: &[u8]) -> String {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD.encode(kp.sign(bytes).as_ref())
    }

    pub fn sample(version: &str, build_id: &str, arch: &str, engine: &str) -> String {
        format!(
            r#"{{"format":"update.v1","version":"{version}","tag":"v{version}","build_id":"{build_id}","arch":"{arch}","engine":"{engine}","schema_version":15,"kernel":"6.18.60","rootfs":{{"file":"embraos-v{version}-{arch}-{engine}.rootfs.squashfs","url":"","size":512442368,"sha256":"{}"}},"built":"2026-11-05T10:12:00Z","notes_url":"https://example/notes"}}"#,
            "ab".repeat(32)
        )
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::*;
    use super::*;

    fn release(version: &str, build_id: &str, engine: &str) -> Release {
        Release::parse(&format!("VERSION={version}\nBUILD_ID={build_id}\nARCH=x86_64\nENGINE={engine}\nBUILT=now\n")).unwrap()
    }

    #[test]
    fn a_manifest_verifies_under_its_key_and_is_refused_under_another() {
        let (kp, public) = key_pair();
        let (_, other) = key_pair();
        let bytes = sample("0.24.0-phase2", "abc", "x86_64", "fjall").into_bytes();
        let sig = sign_b64(&kp, &bytes);
        let m = verify(&bytes, &sig, &[public]).unwrap();
        assert_eq!(m.version, "0.24.0-phase2");
        assert_eq!(m.rootfs.size, 512442368);
        assert!(verify(&bytes, &sig, &[other, public]).is_ok(), "any of several keys");
        let err = verify(&bytes, &sig, &[other]).unwrap_err();
        assert!(err.contains("none of the 1 installed key"));
        let err = verify(&bytes, &sig, &[]).unwrap_err();
        assert!(err.contains("no release signing key"));
    }

    #[test]
    fn a_byte_changed_in_the_manifest_fails_the_signature() {
        let (kp, public) = key_pair();
        let bytes = sample("0.24.0-phase2", "abc", "x86_64", "fjall").into_bytes();
        let sig = sign_b64(&kp, &bytes);
        let mut tampered = bytes.clone();
        tampered[20] ^= 1;
        assert!(verify(&tampered, &sig, &[public]).is_err());
        assert!(verify(&bytes, "not base64!", &[public]).unwrap_err().contains("not base64"));
        assert!(verify(&bytes, "AAAA", &[public]).unwrap_err().contains("not the 64"));
    }

    #[test]
    fn a_signed_document_of_another_format_or_a_bad_hash_is_refused_after_the_signature() {
        let (kp, public) = key_pair();
        let bytes = br#"{"format":"update.v2"}"#.to_vec();
        let sig = sign_b64(&kp, &bytes);
        assert!(verify(&bytes, &sig, &[public]).unwrap_err().contains("does not parse") || verify(&bytes, &sig, &[public]).is_err());
        let text = sample("0.24.0-phase2", "abc", "x86_64", "fjall").replace(&"ab".repeat(32), "zz");
        let bytes = text.into_bytes();
        let sig = sign_b64(&kp, &bytes);
        assert!(verify(&bytes, &sig, &[public]).unwrap_err().contains("sha256"));
    }

    #[test]
    fn the_comparison_reads_the_triple_and_reports_the_suffix() {
        let m: Manifest = serde_json::from_str(&sample("0.24.0-phase2", "abc", "x86_64", "fjall")).unwrap();
        assert_eq!(compare(&release("0.23.0-phase1", "old", "fjall"), None, &m), Verdict::Newer);
        assert_eq!(compare(&release("0.24.0-phase2", "abc", "fjall"), None, &m), Verdict::SameBuild);
        assert_eq!(compare(&release("0.24.0-phase2", "def", "fjall"), None, &m), Verdict::SameVersionOtherBuild);
        assert_eq!(compare(&release("0.25.0-phase2", "x", "fjall"), None, &m), Verdict::Older);
        assert_eq!(compare(&release("0.24.0-phase1", "abc", "fjall"), None, &m), Verdict::SameBuild, "the suffix does not order");
        assert_eq!(version_triple("1.2.3-phase9"), Some((1, 2, 3)));
        assert_eq!(version_triple("1.2"), None);
        assert_eq!(version_triple("a.b.c"), None);
    }

    #[test]
    fn another_arch_or_engine_is_a_mismatch_whatever_the_version() {
        let m: Manifest = serde_json::from_str(&sample("9.0.0", "abc", "x86_64", "fjall")).unwrap();
        assert!(matches!(compare(&release("0.1.0", "x", "rocksdb"), None, &m), Verdict::Mismatch(s) if s.contains("rocksdb")));
        assert!(matches!(compare(&release("0.1.0", "x", "fjall"), Some("rocksdb"), &m), Verdict::Mismatch(s) if s.contains("rocksdb")), "the data's engine wins");
        let m: Manifest = serde_json::from_str(&sample("9.0.0", "abc", "aarch64", "fjall")).unwrap();
        assert!(matches!(compare(&release("0.1.0", "x", "fjall"), None, &m), Verdict::Mismatch(s) if s.contains("aarch64")));
    }
}
