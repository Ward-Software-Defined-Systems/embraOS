//! PKI management for embraOS.
//!
//! Generates a Root CA and service certificates for mTLS.
//! All PKI material is stored on the STATE partition.

use anyhow::{Result, Context};
use rcgen::{
    CertificateParams, DistinguishedName, DnType, DnValue,
    KeyPair, KeyUsagePurpose, ExtendedKeyUsagePurpose, IsCa,
    BasicConstraints, SanType, Certificate,
};
use std::path::{Path, PathBuf};
use tracing::{info, debug};

/// The Root CA's parameters, one definition for a fresh install
/// (`generate_ca`) and for every reboot (`load_ca`, which re-signs the
/// issuer from the stored key): the issuer identity cannot drift between
/// the two (#41). The validity window is the generator's.
fn ca_params() -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, DnValue::Utf8String("embraOS Root CA".to_string()));
    dn.push(DnType::OrganizationName, DnValue::Utf8String("embraOS".to_string()));
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params
}

/// Writes `bytes` to `path` whole: a temporary file beside it, created
/// exclusively with `mode`, written, synced and renamed into place.
fn write_atomic(path: &Path, bytes: &[u8], mode: u32) -> Result<()> {
    use std::io::Write;
    let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("file");
    let tmp = path.with_file_name(format!(".{name}.tmp-{}", std::process::id()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(mode);
    }
    #[cfg(not(unix))]
    let _ = mode;
    let result = (|| -> Result<()> {
        let mut f = options.open(&tmp).with_context(|| format!("create {}", tmp.display()))?;
        f.write_all(bytes)?;
        f.sync_all()?;
        std::fs::rename(&tmp, path).with_context(|| format!("rename {} into place", tmp.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

pub struct PKIManager {
    pki_dir: PathBuf,
    ca_cert: Option<Certificate>,
    ca_key: Option<KeyPair>,
    /// Stored separately for distribution — ca_cert.pem() is only available
    /// when the cert was just generated, so we persist the PEM.
    ca_cert_pem_bytes: Option<Vec<u8>>,
}

impl PKIManager {
    pub fn new(pki_dir: PathBuf) -> Self {
        Self {
            pki_dir,
            ca_cert: None,
            ca_key: None,
            ca_cert_pem_bytes: None,
        }
    }

    /// Initialize PKI — load existing CA or generate new one.
    pub fn init(&mut self) -> Result<()> {
        std::fs::create_dir_all(&self.pki_dir)?;

        let ca_cert_path = self.pki_dir.join("ca.crt");
        let ca_key_path = self.pki_dir.join("ca.key");

        if ca_cert_path.exists() && ca_key_path.exists() {
            info!("Loading existing CA from {}", self.pki_dir.display());
            self.load_ca(&ca_cert_path, &ca_key_path)?;
        } else {
            info!("Generating new Root CA");
            self.generate_ca()?;
            self.save_ca(&ca_cert_path, &ca_key_path)?;
        }

        Ok(())
    }

    fn generate_ca(&mut self) -> Result<()> {
        let mut params = ca_params();
        // 10 year validity for the CA
        params.not_before = time::OffsetDateTime::now_utc();
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(3650);

        let key_pair = KeyPair::generate()?;
        let cert = params.self_signed(&key_pair)?;

        self.ca_cert_pem_bytes = Some(cert.pem().into_bytes());
        self.ca_cert = Some(cert);
        self.ca_key = Some(key_pair);

        info!("Root CA generated");
        Ok(())
    }

    fn load_ca(&mut self, cert_path: &Path, key_path: &Path) -> Result<()> {
        let key_pem = std::fs::read_to_string(key_path)
            .context("Failed to read CA key")?;
        let key_pair = KeyPair::from_pem(&key_pem)
            .context("Failed to parse CA key")?;

        let cert_pem = std::fs::read_to_string(cert_path)
            .context("Failed to read CA cert")?;

        // To sign new certs with a loaded CA, we reconstruct the CA params
        // and re-self-sign with the loaded key. This produces a Certificate
        // object usable as an issuer in signed_by(). rcgen 0.13 has no
        // load-a-certificate-as-issuer path; the one `ca_params` keeps the
        // issuer identity the same on a fresh install and on every reboot.
        let ca_cert = ca_params().self_signed(&key_pair)
            .context("Failed to reconstruct CA certificate from loaded key")?;

        self.ca_cert_pem_bytes = Some(cert_pem.into_bytes());
        self.ca_cert = Some(ca_cert);
        self.ca_key = Some(key_pair);

        info!("CA loaded from {}", self.pki_dir.display());
        Ok(())
    }

    /// The certificate and the key, each written whole: to a temporary file
    /// beside its place, created with its final mode (the key is 0600 from
    /// its first byte, never at the umask and then restricted, #31), synced
    /// and renamed over. A boot that stops between the two leaves one file
    /// at most, and `init` generates both again.
    fn save_ca(&self, cert_path: &Path, key_path: &Path) -> Result<()> {
        if let (Some(cert_pem), Some(key)) = (&self.ca_cert_pem_bytes, &self.ca_key) {
            write_atomic(cert_path, cert_pem, 0o644)?;
            write_atomic(key_path, key.serialize_pem().as_bytes(), 0o600)?;
            info!("CA saved to {}", self.pki_dir.display());
        }
        Ok(())
    }

    /// Generate a service certificate signed by the CA.
    pub fn generate_service_cert(
        &self,
        common_name: &str,
        san_dns: &[String],
        san_ip: &[String],
        is_server: bool,
        is_client: bool,
    ) -> Result<(Vec<u8>, Vec<u8>)> {
        let ca_key = self.ca_key.as_ref()
            .ok_or_else(|| anyhow::anyhow!("CA not initialized"))?;
        let ca_cert = self.ca_cert.as_ref()
            .ok_or_else(|| anyhow::anyhow!("CA cert not available"))?;

        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, DnValue::Utf8String(common_name.to_string()));
        dn.push(DnType::OrganizationName, DnValue::Utf8String("embraOS".to_string()));
        params.distinguished_name = dn;
        params.is_ca = IsCa::NoCa;

        // Set key usages based on role
        let mut extended_usages = Vec::new();
        if is_server {
            extended_usages.push(ExtendedKeyUsagePurpose::ServerAuth);
        }
        if is_client {
            extended_usages.push(ExtendedKeyUsagePurpose::ClientAuth);
        }
        params.extended_key_usages = extended_usages;

        // SANs
        let mut sans = Vec::new();
        for dns in san_dns {
            sans.push(SanType::DnsName(dns.clone().try_into()?));
        }
        for ip_str in san_ip {
            if let Ok(ip) = ip_str.parse::<std::net::IpAddr>() {
                sans.push(SanType::IpAddress(ip));
            }
        }
        params.subject_alt_names = sans;

        // 1 year validity
        params.not_before = time::OffsetDateTime::now_utc();
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(365);

        let service_key = KeyPair::generate()?;
        let cert = params.signed_by(&service_key, ca_cert, ca_key)?;

        let cert_pem = cert.pem().into_bytes();
        let key_pem = service_key.serialize_pem().into_bytes();

        debug!("Generated certificate for {}", common_name);
        Ok((cert_pem, key_pem))
    }

    /// Get the CA certificate PEM for distribution to clients.
    pub fn ca_cert_pem(&self) -> Result<Vec<u8>> {
        if let Some(pem) = &self.ca_cert_pem_bytes {
            Ok(pem.clone())
        } else {
            // Fall back to reading from disk
            let path = self.pki_dir.join("ca.crt");
            Ok(std::fs::read(&path).context("Failed to read CA cert")?)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_pki(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-trustd-pki-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    /// The key is 0600 from its creation, the pair is written whole (no
    /// temporary file stays behind), a second manager over the same
    /// directory loads what the first wrote byte for byte, and the loaded
    /// CA signs a service certificate (#31, #52).
    #[cfg(unix)]
    #[test]
    fn the_ca_key_is_private_from_its_first_byte_and_a_reload_keeps_the_pair() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_pki("mode");
        let mut first = PKIManager::new(dir.clone());
        first.init().expect("a fresh CA");
        let key = dir.join("ca.key");
        let cert = dir.join("ca.crt");
        assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&cert).unwrap().permissions().mode() & 0o777, 0o644);
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert!(names.iter().all(|n| !n.contains(".tmp-")), "{names:?}");
        let key_bytes = std::fs::read(&key).unwrap();
        let cert_bytes = std::fs::read(&cert).unwrap();

        let mut second = PKIManager::new(dir.clone());
        second.init().expect("the CA loads");
        assert_eq!(std::fs::read(&key).unwrap(), key_bytes);
        assert_eq!(std::fs::read(&cert).unwrap(), cert_bytes);
        assert_eq!(second.ca_cert_pem().unwrap(), cert_bytes);
        let (leaf, leaf_key) = second
            .generate_service_cert("embra-apid", &["embra-apid".to_string()], &["127.0.0.1".to_string()], true, false)
            .expect("the loaded CA signs");
        assert!(String::from_utf8_lossy(&leaf).contains("BEGIN CERTIFICATE"));
        assert!(String::from_utf8_lossy(&leaf_key).contains("PRIVATE KEY"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// What trustd mints verifies the way a client verifies it (#31; #52
    /// item 5): a server certificate chains to the first-boot `ca.crt` and
    /// carries the names asked for and the server usage; a client
    /// certificate carries the client usage and not the server one; a name
    /// that was not asked for does not verify; a certificate of another CA
    /// does not chain; the CA loaded on a reboot signs the same chain.
    #[test]
    fn a_minted_certificate_chains_to_the_ca_and_carries_its_names_and_usage() {
        use rustls::client::danger::ServerCertVerifier;
        use rustls::pki_types::pem::PemObject;
        use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
        use std::sync::Arc;

        let dir = temp_pki("verify");
        let mut pki = PKIManager::new(dir.clone());
        pki.init().expect("a fresh CA");
        let ca = CertificateDer::from_pem_slice(&pki.ca_cert_pem().unwrap()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).unwrap();
        let roots = Arc::new(roots);
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let servers = rustls::client::WebPkiServerVerifier::builder_with_provider(roots.clone(), provider.clone())
            .build()
            .unwrap();
        let clients = rustls::server::WebPkiClientVerifier::builder_with_provider(roots, provider)
            .build()
            .unwrap();
        // The clock is read at each verification: a certificate minted after a
        // captured instant is not yet valid at that instant (its not_before is
        // the minting second), and the suite under load crosses the second.
        let now = UnixTime::now;
        let leaf = |pem: &[u8]| CertificateDer::from_pem_slice(pem).unwrap();
        let name = |n: &str| ServerName::try_from(n.to_string()).unwrap();

        let (server_pem, _) = pki
            .generate_service_cert("embra-web", &["localhost".into(), "embraos".into()], &["127.0.0.1".into()], true, false)
            .unwrap();
        let server = leaf(&server_pem);
        for asked in ["localhost", "embraos", "127.0.0.1"] {
            let _ = servers
                .verify_server_cert(&server, &[], &name(asked), &[], now())
                .unwrap_or_else(|e| panic!("{asked}: {e}"));
        }
        assert!(servers.verify_server_cert(&server, &[], &name("other.host"), &[], now()).is_err(), "a name not asked for");
        assert!(clients.verify_client_cert(&server, &[], now()).is_err(), "a server certificate is not a client certificate");

        let (client_pem, _) = pki.generate_service_cert("embra-console", &[], &[], false, true).unwrap();
        let client = leaf(&client_pem);
        let _ = clients.verify_client_cert(&client, &[], now()).expect("the client usage");
        assert!(
            servers.verify_server_cert(&client, &[], &name("localhost"), &[], now()).is_err(),
            "a client certificate is not a server certificate"
        );

        // Another CA's certificate with the same name does not chain.
        let other_dir = temp_pki("other-ca");
        let mut other = PKIManager::new(other_dir.clone());
        other.init().unwrap();
        let (foreign_pem, _) = other.generate_service_cert("embra-web", &["localhost".into()], &[], true, false).unwrap();
        assert!(servers.verify_server_cert(&leaf(&foreign_pem), &[], &name("localhost"), &[], now()).is_err(), "another CA");

        // The CA loaded on a reboot signs certificates the same roots verify.
        let mut reloaded = PKIManager::new(dir.clone());
        reloaded.init().expect("the CA loads");
        let (again_pem, _) = reloaded.generate_service_cert("embra-apid", &["embra-apid".into()], &[], true, true).unwrap();
        let again = leaf(&again_pem);
        let _ = servers.verify_server_cert(&again, &[], &name("embra-apid"), &[], now()).expect("the reloaded CA chains");
        let _ = clients.verify_client_cert(&again, &[], now()).expect("both usages");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    /// `write_atomic` leaves the target untouched when it cannot write: a
    /// temporary file that cannot be created is no write at all.
    #[test]
    fn write_atomic_leaves_the_target_alone_when_it_cannot_write() {
        let dir = temp_pki("atomic");
        std::fs::create_dir_all(&dir).unwrap();
        let target = dir.join("ca.crt");
        std::fs::write(&target, b"before").unwrap();
        let err = write_atomic(&dir.join("missing-dir").join("ca.crt"), b"after", 0o644).unwrap_err();
        assert!(err.to_string().contains("create"), "{err}");
        assert_eq!(std::fs::read(&target).unwrap(), b"before");
        write_atomic(&target, b"after", 0o644).unwrap();
        assert_eq!(std::fs::read(&target).unwrap(), b"after");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
