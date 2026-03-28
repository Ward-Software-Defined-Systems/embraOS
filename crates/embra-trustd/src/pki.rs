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
        let mut params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, DnValue::Utf8String("embraOS Root CA".to_string()));
        dn.push(DnType::OrganizationName, DnValue::Utf8String("embraOS".to_string()));
        params.distinguished_name = dn;
        params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
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
        // object usable as an issuer in signed_by().
        let mut ca_params = CertificateParams::default();
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, DnValue::Utf8String("embraOS Root CA".to_string()));
        dn.push(DnType::OrganizationName, DnValue::Utf8String("embraOS".to_string()));
        ca_params.distinguished_name = dn;
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![
            KeyUsagePurpose::KeyCertSign,
            KeyUsagePurpose::CrlSign,
        ];
        let ca_cert = ca_params.self_signed(&key_pair)
            .context("Failed to reconstruct CA certificate from loaded key")?;

        self.ca_cert_pem_bytes = Some(cert_pem.into_bytes());
        self.ca_cert = Some(ca_cert);
        self.ca_key = Some(key_pair);

        info!("CA loaded from {}", self.pki_dir.display());
        Ok(())
    }

    fn save_ca(&self, cert_path: &Path, key_path: &Path) -> Result<()> {
        if let (Some(cert_pem), Some(key)) = (&self.ca_cert_pem_bytes, &self.ca_key) {
            std::fs::write(cert_path, cert_pem)?;
            std::fs::write(key_path, key.serialize_pem())?;
            // Restrict key file permissions
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                std::fs::set_permissions(key_path, std::fs::Permissions::from_mode(0o600))?;
            }
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
