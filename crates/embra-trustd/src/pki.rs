//! PKI management for embraOS.
//!
//! Generates a Root CA and service certificates for mTLS.
//! All PKI material is stored on the STATE partition.

use anyhow::{Context, Result};
use embra_boot::names::{CaSet, CA_NAMES_FILE, PRIVATE_V4, PRIVATE_V6};
use rcgen::{
    BasicConstraints, Certificate, CertificateParams, CidrSubnet, DistinguishedName, DnType, DnValue, ExtendedKeyUsagePurpose, GeneralSubtree, IsCa,
    KeyPair, KeyUsagePurpose, NameConstraints, SanType, SerialNumber,
};
use sha2::{Digest, Sha256};
use std::net::IpAddr;
use std::path::{Path, PathBuf};
use tracing::{debug, info, warn};

/// The common names the OS mints certificates for (#10): embra-web's, for
/// its listener on `:3345` (`crates/embra-web/src/tls.rs`), the one caller
/// of `GenerateCertificate`. The names and the addresses on a certificate
/// are this machine's own (`embra_boot::names::leaf_names`): the RPC fills
/// them in, refuses a listed one the machine does not hold
/// (`main.rs::requested`), and the Root CA's name constraints
/// ([`ca_params`]) keep its key from signing for a name or an address
/// outside `localhost`, the machine's name and the private ranges, whoever
/// holds the key.
pub const SERVICE_COMMON_NAMES: &[&str] = &["embra-web"];

/// How far before the clock's reading a certificate begins: a browser whose
/// clock is a little behind this machine's does not find it "not valid yet".
const DATED_BACK: time::Duration = time::Duration::hours(1);
const CA_VALID: time::Duration = time::Duration::days(3650);
const LEAF_VALID: time::Duration = time::Duration::days(365);

/// The Root CA's parameters, one definition for a fresh install
/// (`generate_ca`), for every reboot (`load_ca`, which signs the issuer
/// again from the stored key) and for a certificate issued again
/// (`reissue`): the issuer identity cannot drift between them (#41). The
/// validity window and the serial are the caller's.
///
/// The name constraints (#10) are the set's: none for an authority from
/// before them, and for every other one `localhost`, the default name, the
/// operator's name when there is one, and the loopback and private address
/// ranges. A public name or address is in no set.
fn ca_params(set: &CaSet) -> CertificateParams {
    let mut params = CertificateParams::default();
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, DnValue::Utf8String("embraOS Root CA".to_string()));
    dn.push(DnType::OrganizationName, DnValue::Utf8String("embraOS".to_string()));
    params.distinguished_name = dn;
    params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
    params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
    params.name_constraints = match set {
        CaSet::Unconstrained => None,
        CaSet::Private { .. } => Some(NameConstraints {
            permitted_subtrees: set
                .permitted_names()
                .into_iter()
                .map(GeneralSubtree::DnsName)
                .chain(PRIVATE_V4.iter().map(|(net, bits)| GeneralSubtree::IpAddress(CidrSubnet::from_addr_prefix(IpAddr::V4(*net), *bits))))
                .chain(PRIVATE_V6.iter().map(|(net, bits)| GeneralSubtree::IpAddress(CidrSubnet::from_addr_prefix(IpAddr::V6(*net), *bits))))
                .collect(),
            excluded_subtrees: vec![],
        }),
    };
    params
}

/// A serial of its own for every certificate: rcgen's default is derived
/// from the key alone, so two certificates of one key (the authority's,
/// issued again) would share it. Sixteen bytes of a hash over the key and
/// the moment, positive.
fn serial_for(public_key: &[u8], at: time::OffsetDateTime) -> ([u8; 16], SerialNumber) {
    let mut hash = Sha256::new();
    hash.update(public_key);
    hash.update(at.unix_timestamp_nanos().to_be_bytes());
    let digest = hash.finalize();
    let mut bytes = [0u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[0] &= 0x7f;
    (bytes, SerialNumber::from_slice(&bytes))
}

/// The DER of the first certificate in a PEM text.
fn first_certificate(pem: &[u8]) -> Option<Vec<u8>> {
    use rustls_pki_types::pem::PemObject;
    rustls_pki_types::CertificateDer::from_pem_slice(pem).ok().map(|der| der.to_vec())
}

/// Whether a certificate carries the name constraints extension: its
/// object identifier (2.5.29.30) as DER encodes it, looked for in the
/// certificate's bytes. trustd parses no X.509; the five bytes do not occur
/// by chance in a certificate of this size.
fn has_name_constraints(der: &[u8]) -> bool {
    const NAME_CONSTRAINTS_OID: [u8; 5] = [0x06, 0x03, 0x55, 0x1d, 0x1e];
    der.windows(NAME_CONSTRAINTS_OID.len()).any(|w| w == NAME_CONSTRAINTS_OID)
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

/// A certificate trustd minted, with what the caller needs to know of it.
pub struct Minted {
    pub cert_pem: Vec<u8>,
    pub key_pem: Vec<u8>,
    /// The serial, in hex.
    pub serial: String,
    /// When the certificate ends, in seconds after the epoch.
    pub expires_at: i64,
}

pub struct PKIManager {
    pki_dir: PathBuf,
    ca_cert: Option<Certificate>,
    ca_key: Option<KeyPair>,
    /// Stored separately for distribution — ca_cert.pem() is only available
    /// when the cert was just generated, so we persist the PEM.
    ca_cert_pem_bytes: Option<Vec<u8>>,
    /// What the certificate on disk permits its key to sign.
    set: CaSet,
}

impl PKIManager {
    pub fn new(pki_dir: PathBuf) -> Self {
        Self { pki_dir, ca_cert: None, ca_key: None, ca_cert_pem_bytes: None, set: CaSet::Private { hostname: None } }
    }

    /// What the authority's certificate permits.
    pub fn set(&self) -> &CaSet {
        &self.set
    }

    /// Initialize PKI — load the existing CA or generate a new one, then
    /// make sure the operator's name for the machine is one it may sign.
    pub fn init(&mut self, operator_hostname: Option<&str>) -> Result<()> {
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
        self.admit(operator_hostname)?;
        Ok(())
    }

    fn generate_ca(&mut self) -> Result<()> {
        let set = CaSet::Private { hostname: None };
        let key_pair = KeyPair::generate()?;
        let now = time::OffsetDateTime::now_utc();
        let mut params = ca_params(&set);
        // 10 year validity for the CA
        params.not_before = now - DATED_BACK;
        params.not_after = now + CA_VALID;
        params.serial_number = Some(serial_for(&key_pair.public_key_der(), now).1);
        let cert = params.self_signed(&key_pair)?;

        self.ca_cert_pem_bytes = Some(cert.pem().into_bytes());
        self.ca_cert = Some(cert);
        self.ca_key = Some(key_pair);
        self.set = set;

        info!("Root CA generated");
        Ok(())
    }

    fn load_ca(&mut self, cert_path: &Path, key_path: &Path) -> Result<()> {
        let key_pem = std::fs::read_to_string(key_path).context("Failed to read CA key")?;
        let key_pair = KeyPair::from_pem(&key_pem).context("Failed to parse CA key")?;
        let cert_pem = std::fs::read(cert_path).context("Failed to read CA cert")?;
        self.ca_key = Some(key_pair);

        // What the certificate permits is in the record beside it. An
        // authority from before the record has none: one without name
        // constraints signs anything and is left as it is; one with them is
        // limited to the names of QEMU's own network and is issued again,
        // once, with its key, for the names a machine on a network has.
        match CaSet::read(&self.pki_dir) {
            Some(set) => {
                self.set = set;
                self.ca_cert_pem_bytes = Some(cert_pem);
            }
            None => match first_certificate(&cert_pem) {
                Some(der) if !has_name_constraints(&der) => {
                    self.set = CaSet::Unconstrained;
                    self.ca_cert_pem_bytes = Some(cert_pem);
                    write_atomic(&self.pki_dir.join(CA_NAMES_FILE), self.set.to_text().as_bytes(), 0o644)?;
                    info!("The CA is from before the name constraints: it signs any name and stays as it is");
                }
                Some(_) => {
                    info!("The CA is limited to the names of QEMU's own network: its certificate is issued again, with its key, for localhost, the machine's name and the private address ranges");
                    self.reissue(CaSet::Private { hostname: None })?;
                }
                None => {
                    warn!("{} holds no certificate that can be read: issued again from its key", cert_path.display());
                    self.reissue(CaSet::Private { hostname: None })?;
                }
            },
        }

        // To sign new certs with a loaded CA, we reconstruct the CA params
        // and re-self-sign with the loaded key. This produces a Certificate
        // object usable as an issuer in signed_by(). rcgen 0.13 has no
        // load-a-certificate-as-issuer path; the one `ca_params` keeps the
        // issuer identity the same on a fresh install and on every reboot.
        if self.ca_cert.is_none() {
            let key = self.ca_key.as_ref().ok_or_else(|| anyhow::anyhow!("CA not initialized"))?;
            self.ca_cert = Some(ca_params(&self.set).self_signed(key).context("Failed to reconstruct CA certificate from loaded key")?);
        }

        info!("CA loaded from {}", self.pki_dir.display());
        Ok(())
    }

    /// The certificate, the key and the record of what the certificate
    /// permits, each written whole: to a temporary file beside its place,
    /// created with its final mode (the key is 0600 from its first byte,
    /// never at the umask and then restricted, #31), synced and renamed
    /// over. A boot that stops between the first two leaves one file at
    /// most, and `init` generates both again; one that stops before the
    /// record finds a certificate with constraints and no record, and
    /// issues it again.
    fn save_ca(&self, cert_path: &Path, key_path: &Path) -> Result<()> {
        if let (Some(cert_pem), Some(key)) = (&self.ca_cert_pem_bytes, &self.ca_key) {
            write_atomic(cert_path, cert_pem, 0o644)?;
            write_atomic(key_path, key.serialize_pem().as_bytes(), 0o600)?;
            write_atomic(&self.pki_dir.join(CA_NAMES_FILE), self.set.to_text().as_bytes(), 0o644)?;
            info!("CA saved to {}", self.pki_dir.display());
        }
        Ok(())
    }

    /// The authority's certificate issued again for another set, with the
    /// key it has: the same subject and the same key, so every certificate
    /// it signed still chains, and a serial of its own. The certificate
    /// first, then the record: a stop between the two is a certificate with
    /// constraints and no record, which the next start issues again.
    fn reissue(&mut self, set: CaSet) -> Result<()> {
        let key = self.ca_key.as_ref().ok_or_else(|| anyhow::anyhow!("CA not initialized"))?;
        let now = time::OffsetDateTime::now_utc();
        let mut params = ca_params(&set);
        params.not_before = now - DATED_BACK;
        params.not_after = now + CA_VALID;
        params.serial_number = Some(serial_for(&key.public_key_der(), now).1);
        let cert = params.self_signed(key).context("Failed to issue the CA certificate again")?;
        let pem = cert.pem().into_bytes();
        write_atomic(&self.pki_dir.join("ca.crt"), &pem, 0o644)?;
        write_atomic(&self.pki_dir.join(CA_NAMES_FILE), set.to_text().as_bytes(), 0o644)?;
        self.ca_cert_pem_bytes = Some(pem);
        self.ca_cert = Some(cert);
        self.set = set;
        Ok(())
    }

    /// The operator's name for the machine, made a name the authority may
    /// sign: a certificate issued again when its constraints do not admit
    /// it. `true` when it was. A browser that imported the certificate
    /// imports it again; one that holds the former one keeps the former
    /// limits. A name taken away later stays admitted: nothing is issued
    /// for a name that is no longer used.
    pub fn admit(&mut self, operator_hostname: Option<&str>) -> Result<bool> {
        let Some(name) = operator_hostname.filter(|n| embra_boot::net::hostname_ok(n)) else {
            return Ok(false);
        };
        if self.set.admits_name(name) {
            return Ok(false);
        }
        info!("The machine's name is now {name}: the CA certificate is issued again, with its key, to admit it");
        self.reissue(CaSet::Private { hostname: Some(name.to_string()) })?;
        Ok(true)
    }

    /// Generate a service certificate signed by the CA, with the names
    /// given. The names are this machine's own by the RPC's gate
    /// (`main.rs::requested`) and, under a constrained CA, by its name
    /// constraints; this function signs what it is given. The certificate
    /// begins an hour before the clock's reading and ends a year after it.
    pub fn generate_service_cert(&self, common_name: &str, san_dns: &[String], san_ip: &[IpAddr], is_server: bool, is_client: bool) -> Result<Minted> {
        let ca_key = self.ca_key.as_ref().ok_or_else(|| anyhow::anyhow!("CA not initialized"))?;
        let ca_cert = self.ca_cert.as_ref().ok_or_else(|| anyhow::anyhow!("CA cert not available"))?;

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
        sans.extend(san_ip.iter().map(|ip| SanType::IpAddress(*ip)));
        params.subject_alt_names = sans;

        let service_key = KeyPair::generate()?;
        let now = time::OffsetDateTime::now_utc();
        let not_after = now + LEAF_VALID;
        params.not_before = now - DATED_BACK;
        params.not_after = not_after;
        let (serial, number) = serial_for(&service_key.public_key_der(), now);
        params.serial_number = Some(number);
        let cert = params.signed_by(&service_key, ca_cert, ca_key)?;

        debug!("Generated certificate for {}", common_name);
        Ok(Minted {
            cert_pem: cert.pem().into_bytes(),
            key_pem: service_key.serialize_pem().into_bytes(),
            serial: serial.iter().map(|b| format!("{b:02x}")).collect(),
            expires_at: not_after.unix_timestamp(),
        })
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
    use rustls::client::danger::ServerCertVerifier;
    use rustls::pki_types::pem::PemObject;
    use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
    use std::sync::Arc;
    use std::time::Duration;

    fn temp_pki(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("embra-trustd-pki-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        dir
    }

    fn ips(list: &[&str]) -> Vec<IpAddr> {
        list.iter().map(|a| a.parse().unwrap()).collect()
    }

    fn names(list: &[&str]) -> Vec<String> {
        list.iter().map(|n| n.to_string()).collect()
    }

    /// A verifier that trusts one CA certificate and nothing else, as a
    /// browser that imported it does: the chain, the names, the dates and
    /// the CA's name constraints.
    fn trusting(ca_pem: &[u8]) -> Arc<rustls::client::WebPkiServerVerifier> {
        let mut roots = rustls::RootCertStore::empty();
        roots.add(CertificateDer::from_pem_slice(ca_pem).unwrap()).unwrap();
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        rustls::client::WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build().unwrap()
    }

    fn verifies(verifier: &rustls::client::WebPkiServerVerifier, leaf_pem: &[u8], name: &str, at: UnixTime) -> Result<(), rustls::Error> {
        let leaf = CertificateDer::from_pem_slice(leaf_pem).unwrap();
        verifier.verify_server_cert(&leaf, &[], &ServerName::try_from(name.to_string()).unwrap(), &[], at).map(|_| ())
    }

    /// The serial of a certificate, read from its DER: the first INTEGER of
    /// the to-be-signed part, after the version when there is one.
    fn serial_of(pem: &[u8]) -> Vec<u8> {
        fn tlv(b: &[u8]) -> (u8, &[u8], &[u8]) {
            let (len, header) = if b[1] < 0x80 {
                (usize::from(b[1]), 2)
            } else {
                let n = usize::from(b[1] & 0x7f);
                (b[2..2 + n].iter().fold(0usize, |a, x| a << 8 | usize::from(*x)), 2 + n)
            };
            (b[0], &b[header..header + len], &b[header + len..])
        }
        let der = first_certificate(pem).unwrap();
        let (_, certificate, _) = tlv(&der);
        let (_, to_be_signed, _) = tlv(certificate);
        let (tag, first, rest) = tlv(to_be_signed);
        if tag == 0xa0 { tlv(rest).1.to_vec() } else { first.to_vec() }
    }

    /// A CA as an instance from before this change has it: the key, the
    /// certificate and no record. `constrained` is the set of v0.23.0 and
    /// v0.24.0 (the names of QEMU's own network); without it, the CA of the
    /// releases before.
    fn ca_from_before(dir: &Path, constrained: bool) -> (Vec<u8>, Vec<u8>) {
        std::fs::create_dir_all(dir).unwrap();
        let key = KeyPair::generate().unwrap();
        let mut params = ca_params(&CaSet::Unconstrained);
        if constrained {
            params.name_constraints = Some(NameConstraints {
                permitted_subtrees: ["localhost", "embraos", "buildroot"]
                    .iter()
                    .map(|n| GeneralSubtree::DnsName(n.to_string()))
                    .chain(["127.0.0.1", "10.0.2.15"].iter().map(|a| GeneralSubtree::IpAddress(CidrSubnet::from_addr_prefix(a.parse().unwrap(), 32))))
                    .collect(),
                excluded_subtrees: vec![],
            });
        }
        params.not_before = time::OffsetDateTime::now_utc();
        params.not_after = time::OffsetDateTime::now_utc() + time::Duration::days(3650);
        let cert = params.self_signed(&key).unwrap();
        let (cert_pem, key_pem) = (cert.pem().into_bytes(), key.serialize_pem().into_bytes());
        std::fs::write(dir.join("ca.crt"), &cert_pem).unwrap();
        std::fs::write(dir.join("ca.key"), &key_pem).unwrap();
        (cert_pem, key_pem)
    }

    /// The key is 0600 from its creation, the three files are written whole
    /// (no temporary file stays behind), a second manager over the same
    /// directory loads what the first wrote byte for byte, and the loaded
    /// CA signs a service certificate (#31, #52).
    #[cfg(unix)]
    #[test]
    fn the_ca_key_is_private_from_its_first_byte_and_a_reload_keeps_the_pair() {
        use std::os::unix::fs::PermissionsExt;
        let dir = temp_pki("mode");
        let mut first = PKIManager::new(dir.clone());
        first.init(None).expect("a fresh CA");
        let key = dir.join("ca.key");
        let cert = dir.join("ca.crt");
        assert_eq!(std::fs::metadata(&key).unwrap().permissions().mode() & 0o777, 0o600);
        assert_eq!(std::fs::metadata(&cert).unwrap().permissions().mode() & 0o777, 0o644);
        assert_eq!(std::fs::read_to_string(dir.join(CA_NAMES_FILE)).unwrap(), "set=private.v1\n");
        let listed: Vec<String> = std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        assert!(listed.iter().all(|n| !n.contains(".tmp-")), "{listed:?}");
        let key_bytes = std::fs::read(&key).unwrap();
        let cert_bytes = std::fs::read(&cert).unwrap();

        let mut second = PKIManager::new(dir.clone());
        second.init(None).expect("the CA loads");
        assert_eq!(std::fs::read(&key).unwrap(), key_bytes);
        assert_eq!(std::fs::read(&cert).unwrap(), cert_bytes, "a CA with its record is never issued again by a reload");
        assert_eq!(second.ca_cert_pem().unwrap(), cert_bytes);
        let minted = second.generate_service_cert("embra-apid", &names(&["embra-apid"]), &ips(&["127.0.0.1"]), true, false).expect("the loaded CA signs");
        assert!(String::from_utf8_lossy(&minted.cert_pem).contains("BEGIN CERTIFICATE"));
        assert!(String::from_utf8_lossy(&minted.key_pem).contains("PRIVATE KEY"));
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
        let dir = temp_pki("verify");
        let mut pki = PKIManager::new(dir.clone());
        pki.init(None).expect("a fresh CA");
        let ca = CertificateDer::from_pem_slice(&pki.ca_cert_pem().unwrap()).unwrap();
        let mut roots = rustls::RootCertStore::empty();
        roots.add(ca).unwrap();
        let roots = Arc::new(roots);
        let provider = Arc::new(rustls::crypto::aws_lc_rs::default_provider());
        let servers = rustls::client::WebPkiServerVerifier::builder_with_provider(roots.clone(), provider.clone()).build().unwrap();
        let clients = rustls::server::WebPkiClientVerifier::builder_with_provider(roots, provider).build().unwrap();
        let now = UnixTime::now;
        let leaf = |pem: &[u8]| CertificateDer::from_pem_slice(pem).unwrap();
        let name = |n: &str| ServerName::try_from(n.to_string()).unwrap();

        let server_pem = pki.generate_service_cert("embra-web", &names(&["localhost", "embraos"]), &ips(&["127.0.0.1"]), true, false).unwrap().cert_pem;
        let server = leaf(&server_pem);
        for asked in ["localhost", "embraos", "127.0.0.1"] {
            let _ = servers.verify_server_cert(&server, &[], &name(asked), &[], now()).unwrap_or_else(|e| panic!("{asked}: {e}"));
        }
        assert!(servers.verify_server_cert(&server, &[], &name("other.host"), &[], now()).is_err(), "a name not asked for");
        assert!(clients.verify_client_cert(&server, &[], now()).is_err(), "a server certificate is not a client certificate");

        let client_pem = pki.generate_service_cert("embra-console", &[], &[], false, true).unwrap().cert_pem;
        let client = leaf(&client_pem);
        let _ = clients.verify_client_cert(&client, &[], now()).expect("the client usage");
        assert!(servers.verify_server_cert(&client, &[], &name("localhost"), &[], now()).is_err(), "a client certificate is not a server certificate");

        // Another CA's certificate with the same name does not chain.
        let other_dir = temp_pki("other-ca");
        let mut other = PKIManager::new(other_dir.clone());
        other.init(None).unwrap();
        let foreign_pem = other.generate_service_cert("embra-web", &names(&["localhost"]), &[], true, false).unwrap().cert_pem;
        assert!(servers.verify_server_cert(&leaf(&foreign_pem), &[], &name("localhost"), &[], now()).is_err(), "another CA");

        // The CA loaded on a reboot signs certificates the same roots verify.
        let mut reloaded = PKIManager::new(dir.clone());
        reloaded.init(None).expect("the CA loads");
        let again_pem = reloaded.generate_service_cert("embra-web", &names(&["embraos"]), &[], true, true).unwrap().cert_pem;
        let again = leaf(&again_pem);
        let _ = servers.verify_server_cert(&again, &[], &name("embraos"), &[], now()).expect("the reloaded CA chains");
        let _ = clients.verify_client_cert(&again, &[], now()).expect("both usages");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::remove_dir_all(&other_dir);
    }

    /// A Root CA made here permits `localhost`, the default name and the
    /// loopback and private address ranges (#10): a leaf its own key signs
    /// for a public name or address — minted straight through
    /// `generate_service_cert`, past the RPC's gate — does not verify under
    /// the root, and neither does any other name on that leaf: one name
    /// outside the set fails the whole certificate, which is why the names
    /// on a leaf are cut to the set before it is minted.
    #[test]
    fn a_new_ca_admits_private_addresses_and_no_public_one() {
        let dir = temp_pki("constrained");
        let mut pki = PKIManager::new(dir.clone());
        pki.init(None).expect("a fresh CA");
        let browser = trusting(&pki.ca_cert_pem().unwrap());
        let now = UnixTime::now;

        let inside = pki
            .generate_service_cert("embra-web", &names(&["localhost", "embraos"]), &ips(&["127.0.0.1", "::1", "10.0.2.15", "172.20.3.4", "192.168.1.9", "169.254.7.7", "fd00::9"]), true, false)
            .unwrap()
            .cert_pem;
        for asked in ["localhost", "embraos", "127.0.0.1", "::1", "10.0.2.15", "172.20.3.4", "192.168.1.9", "169.254.7.7", "fd00::9"] {
            verifies(&browser, &inside, asked, now()).unwrap_or_else(|e| panic!("{asked}: {e}"));
        }

        for (dns, addresses, asked) in [
            (vec!["evil.example"], vec![], "evil.example"),
            (vec![], vec!["203.0.113.7"], "203.0.113.7"),
            (vec![], vec!["2001:db8::7"], "2001:db8::7"),
            (vec!["axiom"], vec![], "axiom"),
        ] {
            let outside = pki.generate_service_cert("embra-web", &names(&dns), &ips(&addresses), true, false).expect("the key signs anything; the constraint is the root's").cert_pem;
            assert!(verifies(&browser, &outside, asked, now()).is_err(), "{asked} verifies under no root made here");
        }

        // One name outside the set fails every name on the certificate.
        let mixed = pki.generate_service_cert("embra-web", &names(&["localhost"]), &ips(&["127.0.0.1", "203.0.113.7"]), true, false).unwrap().cert_pem;
        assert!(verifies(&browser, &mixed, "localhost", now()).is_err());
        assert!(verifies(&browser, &mixed, "127.0.0.1", now()).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_constrained_ca_from_before_is_issued_again_once_with_its_key_and_a_new_serial() {
        let dir = temp_pki("from-before");
        let (old_cert, old_key) = ca_from_before(&dir, true);
        assert!(has_name_constraints(&first_certificate(&old_cert).unwrap()));

        let mut pki = PKIManager::new(dir.clone());
        pki.init(None).expect("the CA loads and is issued again");
        let new_cert = std::fs::read(dir.join("ca.crt")).unwrap();
        assert_ne!(new_cert, old_cert, "the certificate was issued again");
        assert_eq!(std::fs::read(dir.join("ca.key")).unwrap(), old_key, "with the key it had");
        assert_ne!(serial_of(&new_cert), serial_of(&old_cert), "and a serial of its own");
        assert_eq!(std::fs::read_to_string(dir.join(CA_NAMES_FILE)).unwrap(), "set=private.v1\n");
        assert_eq!(pki.ca_cert_pem().unwrap(), new_cert);

        // A leaf for a leased address: the new certificate admits it, the
        // former one, in a browser that still holds it, does not.
        let leaf = pki.generate_service_cert("embra-web", &names(&["localhost"]), &ips(&["127.0.0.1", "192.168.1.42"]), true, false).unwrap().cert_pem;
        let now = UnixTime::now;
        verifies(&trusting(&new_cert), &leaf, "192.168.1.42", now()).expect("under the certificate issued again");
        verifies(&trusting(&new_cert), &leaf, "localhost", now()).expect("under the certificate issued again");
        assert!(verifies(&trusting(&old_cert), &leaf, "localhost", now()).is_err(), "the former certificate keeps the former limits");

        // Once: the next start finds the record and changes nothing.
        let mut again = PKIManager::new(dir.clone());
        again.init(None).unwrap();
        assert_eq!(std::fs::read(dir.join("ca.crt")).unwrap(), new_cert);

        // A record that was lost is written again, with a certificate.
        std::fs::remove_file(dir.join(CA_NAMES_FILE)).unwrap();
        let mut third = PKIManager::new(dir.clone());
        third.init(None).unwrap();
        assert_ne!(std::fs::read(dir.join("ca.crt")).unwrap(), new_cert);
        assert_eq!(std::fs::read(dir.join("ca.key")).unwrap(), old_key);
        assert_eq!(std::fs::read_to_string(dir.join(CA_NAMES_FILE)).unwrap(), "set=private.v1\n");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_ca_without_constraints_is_left_alone() {
        let dir = temp_pki("unconstrained");
        let (old_cert, old_key) = ca_from_before(&dir, false);
        assert!(!has_name_constraints(&first_certificate(&old_cert).unwrap()));

        let mut pki = PKIManager::new(dir.clone());
        pki.init(Some("axiom")).expect("the CA loads");
        assert_eq!(std::fs::read(dir.join("ca.crt")).unwrap(), old_cert, "byte for byte, an operator's name or not");
        assert_eq!(std::fs::read(dir.join("ca.key")).unwrap(), old_key);
        assert_eq!(std::fs::read_to_string(dir.join(CA_NAMES_FILE)).unwrap(), "set=none\n");
        assert_eq!(pki.set(), &CaSet::Unconstrained);
        assert!(!pki.admit(Some("another")).unwrap());

        // It admits whatever the machine is called and holds.
        let leaf = pki.generate_service_cert("embra-web", &names(&["localhost", "node-7"]), &ips(&["192.168.1.42"]), true, false).unwrap().cert_pem;
        for asked in ["localhost", "node-7", "192.168.1.42"] {
            verifies(&trusting(&old_cert), &leaf, asked, UnixTime::now()).unwrap_or_else(|e| panic!("{asked}: {e}"));
        }
        // A file that is no certificate is replaced from the key.
        std::fs::write(dir.join("ca.crt"), b"not a certificate").unwrap();
        std::fs::remove_file(dir.join(CA_NAMES_FILE)).unwrap();
        let mut repaired = PKIManager::new(dir.clone());
        repaired.init(None).expect("issued again from the key");
        assert!(first_certificate(&std::fs::read(dir.join("ca.crt")).unwrap()).is_some());
        assert_eq!(std::fs::read(dir.join("ca.key")).unwrap(), old_key);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_operators_hostname_outside_the_set_issues_the_ca_again() {
        let dir = temp_pki("hostname");
        let mut pki = PKIManager::new(dir.clone());
        pki.init(None).expect("a fresh CA");
        let first = pki.ca_cert_pem().unwrap();
        let key = std::fs::read(dir.join("ca.key")).unwrap();
        // The default name and no name need nothing.
        assert!(!pki.admit(Some("embraos")).unwrap());
        assert!(!pki.admit(None).unwrap());
        assert!(!pki.admit(Some("not a name")).unwrap());
        assert_eq!(pki.ca_cert_pem().unwrap(), first);

        assert!(pki.admit(Some("axiom")).unwrap(), "a name the certificate does not permit");
        let second = pki.ca_cert_pem().unwrap();
        assert_ne!(second, first);
        assert_eq!(std::fs::read(dir.join("ca.crt")).unwrap(), second);
        assert_eq!(std::fs::read(dir.join("ca.key")).unwrap(), key);
        assert_ne!(serial_of(&second), serial_of(&first));
        assert_eq!(std::fs::read_to_string(dir.join(CA_NAMES_FILE)).unwrap(), "set=private.v1\nhostname=axiom\n");

        let leaf = pki.generate_service_cert("embra-web", &names(&["localhost", "axiom"]), &ips(&["127.0.0.1"]), true, false).unwrap().cert_pem;
        let now = UnixTime::now;
        verifies(&trusting(&second), &leaf, "axiom", now()).expect("the certificate issued again admits the name");
        assert!(verifies(&trusting(&first), &leaf, "axiom", now()).is_err(), "the former one does not");

        // The same name again, the default and none: nothing more is issued.
        assert!(!pki.admit(Some("axiom")).unwrap());
        assert!(!pki.admit(Some("AXIOM")).unwrap());
        assert!(!pki.admit(None).unwrap());
        // A start with the name finds it admitted; one with another issues again.
        let mut reloaded = PKIManager::new(dir.clone());
        reloaded.init(Some("axiom")).unwrap();
        assert_eq!(reloaded.ca_cert_pem().unwrap(), second);
        let mut renamed = PKIManager::new(dir.clone());
        renamed.init(Some("meridian")).unwrap();
        assert_ne!(renamed.ca_cert_pem().unwrap(), second);
        assert_eq!(renamed.set(), &CaSet::Private { hostname: Some("meridian".into()) });
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_leaf_is_dated_an_hour_back_and_says_when_it_ends() {
        let dir = temp_pki("dates");
        let mut pki = PKIManager::new(dir.clone());
        pki.init(None).expect("a fresh CA");
        let browser = trusting(&pki.ca_cert_pem().unwrap());
        let before = time::OffsetDateTime::now_utc().unix_timestamp();
        let minted = pki.generate_service_cert("embra-web", &names(&["localhost"]), &ips(&["127.0.0.1"]), true, false).unwrap();
        let after = time::OffsetDateTime::now_utc().unix_timestamp();
        let at = |secs: i64| UnixTime::since_unix_epoch(Duration::from_secs(secs as u64));

        // A browser whose clock is half an hour behind accepts it; two hours behind, not yet.
        verifies(&browser, &minted.cert_pem, "localhost", at(before - 1800)).expect("half an hour behind");
        assert!(verifies(&browser, &minted.cert_pem, "localhost", at(before - 7200)).is_err(), "two hours behind");
        // It ends a year on, and says so.
        let year = 365 * 86_400;
        assert!(minted.expires_at >= before + year && minted.expires_at <= after + year, "{}", minted.expires_at);
        verifies(&browser, &minted.cert_pem, "localhost", at(minted.expires_at - 60)).expect("a minute before its end");
        assert!(verifies(&browser, &minted.cert_pem, "localhost", at(minted.expires_at + 60)).is_err(), "a minute after its end");
        // Its serial is its own, in hex.
        let other = pki.generate_service_cert("embra-web", &names(&["localhost"]), &[], true, false).unwrap();
        assert_eq!(minted.serial.len(), 32);
        assert!(minted.serial.bytes().all(|b| b.is_ascii_hexdigit()));
        assert_ne!(minted.serial, other.serial);
        let _ = std::fs::remove_dir_all(&dir);
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
