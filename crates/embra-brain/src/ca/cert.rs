//! A certificate as the `/ca` command reads it: the certificates of a
//! source (PEM, or one certificate in DER), each one's SHA-256 fingerprint,
//! the few fields an operator tells certificates apart by, and the PEM the
//! command writes.
//!
//! The PEM parser is `rustls-pki-types`', the one `reqwest` reads a bundle
//! with (`Certificate::from_pem_bundle`) and the one the web console reads
//! its own chain with, so a source is split here as the brain's trust
//! loader splits a file. The fields are read by a small DER walker of this
//! file's own, for display alone: nothing is trusted or refused on what it
//! reads, and a certificate it cannot read is shown by its fingerprint.
//! Whether a certificate can be a root is the root store's verdict
//! ([`as_root`]).

use rustls_pki_types::pem::{self, PemObject, SectionKind};
use sha2::{Digest, Sha256};

/// The most bytes a source may hold: embrad's cap on a drop-in file
/// (`ca_bundle.rs`, `MAX_CA_FILE_SIZE`) and the build's
/// (`IMPORTED_CAS_MAX_BYTES`).
pub(crate) const MAX_SOURCE_BYTES: usize = 256 * 1024;
/// The most certificates one source may hold.
pub(crate) const MAX_CERTIFICATES: usize = 16;

/// Why a source gives no certificates. Each reads as what the source does,
/// after its name: "`<source>` carries a private key: ...".
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SourceError {
    Empty,
    TooLarge { bytes: usize },
    PrivateKey,
    Pem(&'static str),
    NoCertificate,
    TooMany { found: usize },
}

impl std::fmt::Display for SourceError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Empty => write!(f, "is empty"),
            Self::TooLarge { bytes } => {
                write!(f, "holds {bytes} bytes; a certificate file holds at most {MAX_SOURCE_BYTES}")
            }
            Self::PrivateKey => write!(
                f,
                "carries a private key: a certificate authority's certificate alone is imported, never its key"
            ),
            Self::Pem(what) => write!(f, "holds PEM that does not parse: {what}"),
            Self::NoCertificate => write!(
                f,
                "holds no certificate: a PEM CERTIFICATE block, or one certificate in DER, is what is read"
            ),
            Self::TooMany { found } => {
                write!(f, "holds {found} certificates; one source holds at most {MAX_CERTIFICATES}")
            }
        }
    }
}

/// Where `needle` starts in `haystack`.
fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

/// What is wrong with PEM that does not parse.
fn pem_fault(e: pem::Error) -> &'static str {
    match e {
        pem::Error::MissingSectionEnd { .. } => "a block has no END line",
        pem::Error::IllegalSectionStart { .. } => "a BEGIN line is malformed",
        pem::Error::Base64Decode(_) => "a block is not base64",
        _ => "it is not PEM",
    }
}

/// The certificates of a file in a certificate directory, split as the
/// trust loader has always split one (`reqwest::Certificate::from_pem_bundle`
/// runs this parser): every CERTIFICATE block in order, any other block
/// passed over, and a block that does not parse refusing the whole file.
/// No cap and no refusal of a key: a file an operator seeded keeps loading
/// as it did. A source the command is given is read by
/// [`certificates_of`], which is stricter.
pub(crate) fn file_certificates(file: &[u8]) -> Result<Vec<Vec<u8>>, &'static str> {
    rustls_pki_types::CertificateDer::pem_slice_iter(file)
        .map(|block| block.map(|der| der.to_vec()).map_err(pem_fault))
        .collect()
}

/// The certificates of a source, each as DER, in the source's order. PEM is
/// split by the parser the trust loader uses: text outside the blocks and
/// blocks of another kind (a revocation list, a request) are passed over,
/// and a private key refuses the whole source. A source without PEM armor
/// is one certificate in DER.
pub(crate) fn certificates_of(source: &[u8]) -> Result<Vec<Vec<u8>>, SourceError> {
    if source.iter().all(u8::is_ascii_whitespace) {
        return Err(SourceError::Empty);
    }
    if source.len() > MAX_SOURCE_BYTES {
        return Err(SourceError::TooLarge { bytes: source.len() });
    }
    if find(source, b"-----BEGIN ").is_none() {
        // One certificate in DER: a SEQUENCE that is the whole source.
        return match take(source, SEQUENCE) {
            Some((_, [])) => Ok(vec![source.to_vec()]),
            _ => Err(SourceError::NoCertificate),
        };
    }
    // Every private-key label ends this way, the ones the parser does not
    // know among them (an encrypted key, an OpenSSH key).
    if find(source, b"PRIVATE KEY-----").is_some() {
        return Err(SourceError::PrivateKey);
    }
    let mut certificates = Vec::new();
    for section in <(SectionKind, Vec<u8>)>::pem_slice_iter(source) {
        let (kind, der) = section.map_err(|e| SourceError::Pem(pem_fault(e)))?;
        match kind {
            SectionKind::Certificate => certificates.push(der),
            SectionKind::RsaPrivateKey | SectionKind::PrivateKey | SectionKind::EcPrivateKey => {
                return Err(SourceError::PrivateKey);
            }
            _ => {}
        }
    }
    if certificates.is_empty() {
        return Err(SourceError::NoCertificate);
    }
    if certificates.len() > MAX_CERTIFICATES {
        return Err(SourceError::TooMany { found: certificates.len() });
    }
    Ok(certificates)
}

/// The root store's verdict on one certificate: the certificate as a client
/// takes it, or why no client would. `reqwest` checks nothing when it wraps
/// the bytes; the store checks when a client is built, so a throwaway
/// client is built here, as the trust loader does for every drop-in.
pub(crate) fn as_root(der: &[u8]) -> Result<reqwest::Certificate, String> {
    let certificate = reqwest::Certificate::from_der(der).map_err(|e| e.to_string())?;
    reqwest::Client::builder()
        .add_root_certificate(certificate.clone())
        .build()
        .map(|_| certificate)
        .map_err(|e| {
            use std::error::Error as _;
            e.source().map_or_else(|| e.to_string(), |cause| cause.to_string())
        })
}

/// A certificate's SHA-256 over its DER: what selects and authenticates a
/// certificate in the `/ca` command.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) struct Fingerprint([u8; 32]);

impl Fingerprint {
    pub(crate) fn of(der: &[u8]) -> Self {
        Self(Sha256::digest(der).into())
    }

    /// Sixty-four hex digits, with or without colons between the pairs, in
    /// either case, with or without a leading `sha256:`.
    pub(crate) fn parse(text: &str) -> Option<Self> {
        let text = text.trim();
        let text = match text.get(..7) {
            Some(prefix) if prefix.eq_ignore_ascii_case("sha256:") => text[7..].trim_start(),
            _ => text,
        };
        let digits: String = text.chars().filter(|c| *c != ':').collect();
        if digits.len() != 64 {
            return None;
        }
        let mut bytes = [0u8; 32];
        hex::decode_to_slice(&digits, &mut bytes).ok()?;
        Some(Self(bytes))
    }

    /// As `openssl x509 -fingerprint -sha256` prints it: thirty-two pairs
    /// in upper case with colons between them.
    pub(crate) fn shown(&self) -> String {
        self.0.iter().map(|b| format!("{b:02X}")).collect::<Vec<_>>().join(":")
    }

    /// Sixty-four lower-case hex digits.
    pub(crate) fn hex(&self) -> String {
        hex::encode(self.0)
    }
}

/// One certificate as the command writes it: a `CERTIFICATE` block of
/// 64-column base64 and nothing else. Every reader of the certificate
/// directories takes it: the brain's loader, embrad's bundle (which merges
/// a file's whole text) and the build's check of `Imported_CAs/`.
pub(crate) fn canonical_pem(der: &[u8]) -> String {
    use base64::Engine as _;
    let encoded = base64::engine::general_purpose::STANDARD.encode(der);
    let mut pem = String::with_capacity(encoded.len() + encoded.len() / 64 + 64);
    pem.push_str("-----BEGIN CERTIFICATE-----\n");
    for line in encoded.as_bytes().chunks(64) {
        pem.push_str(&String::from_utf8_lossy(line));
        pem.push('\n');
    }
    pem.push_str("-----END CERTIFICATE-----\n");
    pem
}

/// The most characters one attribute's value is shown with.
const VALUE_SHOWN: usize = 64;
/// The most characters a whole name is shown with.
const NAME_SHOWN: usize = 200;

/// Text of a certificate as it is put on a terminal: no control or
/// direction-changing characters, and at most `max` characters. The text
/// is the sender's.
pub(crate) fn shown(text: &str, max: usize) -> String {
    let unseen = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}')
    };
    let mut out: String = text.chars().filter(|c| !unseen(*c)).take(max).collect();
    if text.chars().filter(|c| !unseen(*c)).count() > max {
        out.push('…');
    }
    out
}

/// What the walker reads of a certificate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct CertInfo {
    pub(crate) subject: String,
    pub(crate) issuer: String,
    /// Seconds since the epoch.
    pub(crate) not_before: i64,
    pub(crate) not_after: i64,
    /// Its basic constraints say it is a certificate authority.
    pub(crate) is_ca: bool,
    /// Its issuer's name is its own.
    pub(crate) self_issued: bool,
    /// The subject's common name, else its organization: what a file of it
    /// is named by.
    pub(crate) name_hint: Option<String>,
}

const SEQUENCE: u8 = 0x30;
const SET: u8 = 0x31;
const INTEGER: u8 = 0x02;
const BOOLEAN: u8 = 0x01;
const OCTET_STRING: u8 = 0x04;
const OID: u8 = 0x06;
const UTC_TIME: u8 = 0x17;
const GENERALIZED_TIME: u8 = 0x18;
/// `[0] EXPLICIT`, the version of a certificate that has one.
const VERSION: u8 = 0xA0;
/// `[3] EXPLICIT`, the extensions.
const EXTENSIONS: u8 = 0xA3;
/// 2.5.29.19, `basicConstraints`.
const BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1D, 0x13];
/// 2.5.4.3 and 2.5.4.10.
const COMMON_NAME: &[u8] = &[0x55, 0x04, 0x03];
const ORGANIZATION: &[u8] = &[0x55, 0x04, 0x0A];

/// The DER element at the front of `input`: its tag, its contents, and what
/// follows it. Definite lengths only, as DER has them; a length that runs
/// past the input, a length of more than four bytes and a tag of more than
/// one byte are no element.
fn element(input: &[u8]) -> Option<(u8, &[u8], &[u8])> {
    let (&tag, rest) = input.split_first()?;
    if tag & 0x1F == 0x1F {
        return None;
    }
    let (&first, rest) = rest.split_first()?;
    let (length, rest) = if first & 0x80 == 0 {
        (usize::from(first), rest)
    } else {
        let count = usize::from(first & 0x7F);
        if count == 0 || count > 4 {
            return None;
        }
        let (bytes, rest) = rest.split_at_checked(count)?;
        (bytes.iter().fold(0usize, |length, b| (length << 8) | usize::from(*b)), rest)
    };
    let (body, rest) = rest.split_at_checked(length)?;
    Some((tag, body, rest))
}

/// The contents of the element at the front of `input` when it has `tag`,
/// and what follows it.
fn take(input: &[u8], tag: u8) -> Option<(&[u8], &[u8])> {
    let (found, body, rest) = element(input)?;
    (found == tag).then_some((body, rest))
}

impl CertInfo {
    /// The fields of one certificate, or nothing when the bytes are not the
    /// structure a certificate has. Never a panic: every step is a checked
    /// slice.
    pub(crate) fn read(der: &[u8]) -> Option<Self> {
        let (certificate, after) = take(der, SEQUENCE)?;
        if !after.is_empty() {
            return None;
        }
        let (tbs, _) = take(certificate, SEQUENCE)?;
        let rest = match element(tbs)? {
            (VERSION, _, rest) => rest,
            _ => tbs,
        };
        let (_serial, rest) = take(rest, INTEGER)?;
        let (_signature, rest) = take(rest, SEQUENCE)?;
        let (issuer, rest) = take(rest, SEQUENCE)?;
        let (validity, rest) = take(rest, SEQUENCE)?;
        let (subject, rest) = take(rest, SEQUENCE)?;
        let (_key, mut rest) = take(rest, SEQUENCE)?;
        let mut is_ca = false;
        while !rest.is_empty() {
            let (tag, body, after) = element(rest)?;
            if tag == EXTENSIONS {
                is_ca = says_ca(body)?;
            }
            rest = after;
        }
        let (not_before, validity) = time(validity)?;
        let (not_after, validity) = time(validity)?;
        if !validity.is_empty() {
            return None;
        }
        let (subject_text, name_hint) = name(subject)?;
        let (issuer_text, _) = name(issuer)?;
        Some(Self {
            subject: subject_text,
            issuer: issuer_text,
            not_before,
            not_after,
            is_ca,
            self_issued: subject == issuer,
            name_hint,
        })
    }
}

/// Whether the extensions carry basic constraints that say `cA`.
fn says_ca(extensions: &[u8]) -> Option<bool> {
    let (mut list, after) = take(extensions, SEQUENCE)?;
    if !after.is_empty() {
        return None;
    }
    while !list.is_empty() {
        let (extension, after) = take(list, SEQUENCE)?;
        list = after;
        let (oid, rest) = take(extension, OID)?;
        // `critical BOOLEAN DEFAULT FALSE` is there or not.
        let rest = match element(rest)? {
            (BOOLEAN, _, rest) => rest,
            _ => rest,
        };
        let (value, _) = take(rest, OCTET_STRING)?;
        if oid == BASIC_CONSTRAINTS {
            let (constraints, _) = take(value, SEQUENCE)?;
            return Some(matches!(element(constraints), Some((BOOLEAN, flag, _)) if flag.first().is_some_and(|b| *b != 0)));
        }
    }
    Some(false)
}

/// A `Time` at the front of `input`, as seconds since the epoch, and what
/// follows it: `YYMMDDHHMMSSZ` (the century by RFC 5280's rule) or
/// `YYYYMMDDHHMMSSZ`.
fn time(input: &[u8]) -> Option<(i64, &[u8])> {
    let (tag, body, rest) = element(input)?;
    let text = std::str::from_utf8(body).ok()?;
    let digits = text.strip_suffix('Z')?;
    if !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let (year, rest_digits) = match (tag, digits.len()) {
        (UTC_TIME, 12) => {
            let yy: i32 = digits.get(..2)?.parse().ok()?;
            (if yy >= 50 { 1900 + yy } else { 2000 + yy }, digits.get(2..)?)
        }
        (GENERALIZED_TIME, 14) => (digits.get(..4)?.parse().ok()?, digits.get(4..)?),
        _ => return None,
    };
    let field = |at: usize| -> Option<u32> { rest_digits.get(at..at + 2)?.parse().ok() };
    let when = chrono::NaiveDate::from_ymd_opt(year, field(0)?, field(2)?)?
        .and_hms_opt(field(4)?, field(6)?, field(8)?)?;
    Some((when.and_utc().timestamp(), rest))
}

/// A `Name` as text, attribute by attribute in the certificate's order
/// (`C=US, O=Example, CN=Example Root`), and the name a file of the
/// certificate is called by: the common name, else the organization.
fn name(mut rdns: &[u8]) -> Option<(String, Option<String>)> {
    let mut parts = Vec::new();
    let (mut common_name, mut organization) = (None, None);
    while !rdns.is_empty() {
        let (mut set, after) = take(rdns, SET)?;
        rdns = after;
        let mut attributes = Vec::new();
        while !set.is_empty() {
            let (attribute, after) = take(set, SEQUENCE)?;
            set = after;
            let (oid, value) = take(attribute, OID)?;
            let (tag, bytes, after) = element(value)?;
            if !after.is_empty() {
                return None;
            }
            let text = shown(&string_of(tag, bytes), VALUE_SHOWN);
            if oid == COMMON_NAME && common_name.is_none() {
                common_name = Some(text.clone());
            } else if oid == ORGANIZATION && organization.is_none() {
                organization = Some(text.clone());
            }
            attributes.push(format!("{}={}", label_of(oid), escaped(&text)));
        }
        parts.push(attributes.join("+"));
    }
    Some((shown(&parts.join(", "), NAME_SHOWN), common_name.or(organization)))
}

/// A value with the characters that separate one attribute from the next
/// escaped (RFC 4514), so a value cannot read as a second attribute.
fn escaped(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for c in value.chars() {
        if matches!(c, ',' | '+' | '=' | '"' | '\\' | '<' | '>' | ';') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// The text of a string value, whatever string type it is in. Lenient on
/// purpose: a name string the type's own alphabet forbids is still shown.
fn string_of(tag: u8, bytes: &[u8]) -> String {
    match tag {
        // BMPString: UTF-16, big end first.
        0x1E => char::decode_utf16(bytes.chunks(2).map(|pair| match pair {
            [high, low] => u16::from_be_bytes([*high, *low]),
            _ => 0xFFFD,
        }))
        .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
        .collect(),
        // UniversalString: UTF-32, big end first.
        0x1C => bytes
            .chunks(4)
            .map(|quad| match quad {
                [a, b, c, d] => char::from_u32(u32::from_be_bytes([*a, *b, *c, *d])),
                _ => None,
            })
            .map(|c| c.unwrap_or(char::REPLACEMENT_CHARACTER))
            .collect(),
        // TeletexString, read as Latin-1 as every reader does.
        0x14 => bytes.iter().map(|b| char::from(*b)).collect(),
        // UTF8String, PrintableString, IA5String and the rest.
        _ => String::from_utf8_lossy(bytes).into_owned(),
    }
}

/// The short name of an attribute type, or its number.
fn label_of(oid: &[u8]) -> String {
    let short = match oid {
        [0x55, 0x04, 0x03] => "CN",
        [0x55, 0x04, 0x05] => "serialNumber",
        [0x55, 0x04, 0x06] => "C",
        [0x55, 0x04, 0x07] => "L",
        [0x55, 0x04, 0x08] => "ST",
        [0x55, 0x04, 0x09] => "street",
        [0x55, 0x04, 0x0A] => "O",
        [0x55, 0x04, 0x0B] => "OU",
        [0x2A, 0x86, 0x48, 0x86, 0xF7, 0x0D, 0x01, 0x09, 0x01] => "emailAddress",
        [0x09, 0x92, 0x26, 0x89, 0x93, 0xF2, 0x2C, 0x64, 0x01, 0x19] => "DC",
        _ => return dotted(oid).unwrap_or_else(|| format!("#{}", hex::encode(oid))),
    };
    short.to_string()
}

/// An object identifier in its dotted form.
fn dotted(oid: &[u8]) -> Option<String> {
    let mut arcs: Vec<u64> = Vec::new();
    let mut value: u64 = 0;
    let mut pending = false;
    for byte in oid {
        value = value.checked_mul(128)?.checked_add(u64::from(byte & 0x7F))?;
        pending = byte & 0x80 != 0;
        if !pending {
            if arcs.is_empty() {
                let first = (value / 40).min(2);
                arcs.push(first);
                arcs.push(value - first * 40);
            } else {
                arcs.push(value);
            }
            value = 0;
        }
    }
    if pending || arcs.is_empty() {
        return None;
    }
    Some(arcs.iter().map(u64::to_string).collect::<Vec<_>>().join("."))
}

/// A time as the command shows it: the day and the time, in UTC.
pub(crate) fn utc(seconds: i64) -> String {
    chrono::DateTime::from_timestamp(seconds, 0)
        .map_or_else(|| format!("{seconds} s"), |t| t.format("%Y-%m-%d %H:%M:%S UTC").to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, DnValue, IsCa, KeyPair};

    /// A certificate made with `openssl req -x509` on the build host (OpenSSL
    /// 3.0.13; the key was thrown away), and what `openssl x509 -noout
    /// -fingerprint -sha256 -subject -dates -ext basicConstraints` printed
    /// for it.
    const FIXTURE: &str = "-----BEGIN CERTIFICATE-----
MIICAzCCAamgAwIBAgICICYwCgYIKoZIzj0EAwIwWDELMAkGA1UEBhMCVVMxFTAT
BgNVBAoMDGVtYnJhT1MgVGVzdDERMA8GA1UECwwIRml4dHVyZXMxHzAdBgNVBAMM
FmVtYnJhT1MgdGVzdCByb290IDIwMjYwHhcNMjYxMTE0MTY0NzI3WhcNMzYxMTEx
MTY0NzI3WjBYMQswCQYDVQQGEwJVUzEVMBMGA1UECgwMZW1icmFPUyBUZXN0MREw
DwYDVQQLDAhGaXh0dXJlczEfMB0GA1UEAwwWZW1icmFPUyB0ZXN0IHJvb3QgMjAy
NjBZMBMGByqGSM49AgEGCCqGSM49AwEHA0IABIvcLA/CfdAdBdyVq9X9stFXawpI
ZrF8bQGjaHOJKoqKyerNkQ8E48AS4FOf+3qjSDPLuXJPcmyKZJH1taf3UJWjYzBh
MB0GA1UdDgQWBBSYC31iOY0ZI1K4evyjhOaIZI3sKDAfBgNVHSMEGDAWgBSYC31i
OY0ZI1K4evyjhOaIZI3sKDAPBgNVHRMBAf8EBTADAQH/MA4GA1UdDwEB/wQEAwIB
BjAKBggqhkjOPQQDAgNIADBFAiEAvUycSS//PYPSMVlqGcUw7shhU+7TxyNMS47B
A4VxDNMCIE+MW3vHLOk/mxfUo/327DXbTp2YINc24U5tO78fOWbQ
-----END CERTIFICATE-----
";
    const FIXTURE_FINGERPRINT: &str =
        "14:31:2F:40:56:6E:33:29:3C:45:10:51:54:8D:7A:63:A5:71:75:74:DC:77:7A:C5:AB:2D:A4:11:76:FC:46:22";

    fn fixture_der() -> Vec<u8> {
        certificates_of(FIXTURE.as_bytes()).unwrap().remove(0)
    }

    /// A CA and a certificate under it, as DER.
    fn minted() -> (Vec<u8>, Vec<u8>) {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        // A name of the test's own, in the test's order (the library's
        // default name has a common name already).
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params.distinguished_name.push(DnType::OrganizationName, DnValue::PrintableString("Example Org".try_into().unwrap()));
        ca_params.distinguished_name.push(DnType::CommonName, DnValue::Utf8String("Example Root".to_string()));
        ca_params.not_before = rcgen::date_time_ymd(2026, 1, 2);
        ca_params.not_after = rcgen::date_time_ymd(2036, 3, 4);
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["gateway.example".to_string()]).unwrap();
        leaf_params.distinguished_name = DistinguishedName::new();
        leaf_params.distinguished_name.push(DnType::CommonName, DnValue::Utf8String("gateway.example".to_string()));
        leaf_params.not_before = rcgen::date_time_ymd(2049, 12, 31);
        leaf_params.not_after = rcgen::date_time_ymd(2050, 1, 1);
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
        (ca.der().to_vec(), leaf.der().to_vec())
    }

    /// The fingerprint is SHA-256 over the DER, shown as OpenSSL shows it
    /// (and as `local-ai-gateway tls show` does), and the fields are the
    /// ones OpenSSL printed for the same certificate.
    #[test]
    fn the_fingerprint_is_sha256_over_the_der_in_the_form_openssl_prints() {
        let der = fixture_der();
        let fingerprint = Fingerprint::of(&der);
        assert_eq!(fingerprint.shown(), FIXTURE_FINGERPRINT);
        assert_eq!(fingerprint.shown().len(), 95);
        assert_eq!(fingerprint.hex(), "14312f40566e33293c451051548d7a63a5717574dc777ac5ab2da41176fc4622");

        let info = CertInfo::read(&der).expect("the walker reads an OpenSSL certificate");
        assert_eq!(info.subject, "C=US, O=embraOS Test, OU=Fixtures, CN=embraOS test root 2026");
        assert_eq!(info.issuer, info.subject);
        assert_eq!(utc(info.not_before), "2026-11-14 16:47:27 UTC");
        assert_eq!(utc(info.not_after), "2036-11-11 16:47:27 UTC");
        assert!(info.is_ca && info.self_issued);
        assert_eq!(info.name_hint.as_deref(), Some("embraOS test root 2026"));
    }

    #[test]
    fn a_fingerprint_is_read_with_or_without_colons_in_either_case() {
        let want = Fingerprint::of(&fixture_der());
        let hex = want.hex();
        for text in [
            FIXTURE_FINGERPRINT.to_string(),
            FIXTURE_FINGERPRINT.to_ascii_lowercase(),
            hex.clone(),
            hex.to_ascii_uppercase(),
            format!("sha256:{hex}"),
            format!("SHA256:{FIXTURE_FINGERPRINT}"),
            format!("  sha256:{FIXTURE_FINGERPRINT}  "),
            format!("sha256: {hex}"),
        ] {
            assert_eq!(Fingerprint::parse(&text), Some(want), "{text}");
        }
        for text in ["", "sha256:", &hex[..63], &format!("{hex}0"), &format!("sha1:{hex}"), &hex.replace('1', "g"), "é"] {
            assert_eq!(Fingerprint::parse(text), None, "{text}");
        }
    }

    /// A certificate from another library, with the string types and the two
    /// time forms OpenSSL's did not have: a PrintableString beside a
    /// UTF8String, and a date past 2049, which is a GeneralizedTime.
    #[test]
    fn a_certificate_made_by_rcgen_reads_back_its_subject_issuer_dates_and_ca_flag() {
        let (ca, leaf) = minted();
        let ca_info = CertInfo::read(&ca).unwrap();
        assert_eq!(ca_info.subject, "O=Example Org, CN=Example Root");
        assert_eq!(ca_info.issuer, ca_info.subject);
        assert_eq!(utc(ca_info.not_before), "2026-01-02 00:00:00 UTC");
        assert_eq!(utc(ca_info.not_after), "2036-03-04 00:00:00 UTC");
        assert!(ca_info.is_ca && ca_info.self_issued);
        assert_eq!(ca_info.name_hint.as_deref(), Some("Example Root"));

        let leaf_info = CertInfo::read(&leaf).unwrap();
        assert_eq!(leaf_info.subject, "CN=gateway.example");
        assert_eq!(leaf_info.issuer, "O=Example Org, CN=Example Root");
        assert_eq!(utc(leaf_info.not_before), "2049-12-31 00:00:00 UTC");
        assert_eq!(utc(leaf_info.not_after), "2050-01-01 00:00:00 UTC");
        assert!(!leaf_info.is_ca && !leaf_info.self_issued);
        assert_ne!(Fingerprint::of(&ca), Fingerprint::of(&leaf));
    }

    /// The walker is strict on structure and never panics: every proper
    /// prefix of a certificate is refused, and a certificate with any one
    /// byte changed is read or refused.
    #[test]
    fn a_certificate_cut_at_any_byte_is_refused_and_never_panics() {
        for der in [fixture_der(), minted().0, minted().1] {
            for cut in 0..der.len() {
                assert_eq!(CertInfo::read(&der[..cut]), None, "a certificate cut at {cut} of {}", der.len());
            }
            let mut longer = der.clone();
            longer.push(0);
            assert_eq!(CertInfo::read(&longer), None, "a byte after the certificate");
            for at in 0..der.len() {
                for flip in [0x01u8, 0x80, 0xFF] {
                    let mut changed = der.clone();
                    if let Some(byte) = changed.get_mut(at) {
                        *byte ^= flip;
                    }
                    let _ = CertInfo::read(&changed);
                }
            }
        }
        assert_eq!(CertInfo::read(&[]), None);
        assert_eq!(CertInfo::read(&[0x30, 0x84, 0xFF, 0xFF, 0xFF, 0xFF]), None, "a length past the input");
        assert_eq!(CertInfo::read(&[0x30, 0x85, 1, 1, 1, 1, 1]), None, "a length of five bytes");
    }

    /// A certificate's text is the sender's: control characters and
    /// direction overrides never reach the terminal, a long value is cut,
    /// and a value cannot pass for a second attribute.
    #[test]
    fn a_subject_is_shown_without_control_characters_and_cut() {
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(Vec::<String>::new()).unwrap();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(
            DnType::OrganizationName,
            DnValue::Utf8String("Evil, CN=Good\u{1b}[31m\u{7}\r\n\u{202E}".to_string()),
        );
        params.distinguished_name.push(DnType::CommonName, DnValue::Utf8String("x".repeat(300)));
        let der = params.self_signed(&key).unwrap().der().to_vec();
        let info = CertInfo::read(&der).unwrap();
        assert!(info.subject.starts_with("O=Evil\\, CN\\=Good[31m, CN=xxx"), "{}", info.subject);
        assert!(!info.subject.chars().any(|c| c.is_control() || c == '\u{202E}'), "{:?}", info.subject);
        assert_eq!(info.subject.matches('x').count(), VALUE_SHOWN, "the value is cut: {}", info.subject);
        assert!(info.subject.ends_with('…'));
        assert_eq!(info.name_hint.as_deref().map(|n| n.chars().count()), Some(VALUE_SHOWN + 1));

        assert_eq!(shown("a\u{0}b\u{9}c", 10), "abc");
        assert_eq!(shown("abcdef", 3), "abc…");
        assert_eq!(shown("abc", 3), "abc");
    }

    /// A source is split as the trust loader splits a file: every
    /// CERTIFICATE block in order, text between them passed over, and one
    /// DER certificate without armor taken whole. What is not a certificate
    /// source says why.
    #[test]
    fn a_source_gives_its_certificates_or_says_why_not() {
        let (ca, leaf) = minted();
        let bundle = format!("# the chain\n{}some text\n{}", canonical_pem(&leaf), canonical_pem(&ca));
        assert_eq!(certificates_of(bundle.as_bytes()).unwrap(), vec![leaf.clone(), ca.clone()]);
        assert_eq!(certificates_of(&ca).unwrap(), vec![ca.clone()], "one certificate in DER");
        assert_eq!(certificates_of(FIXTURE.replace('\n', "\r\n").as_bytes()).unwrap(), vec![fixture_der()], "CRLF line ends");

        assert_eq!(certificates_of(b""), Err(SourceError::Empty));
        assert_eq!(certificates_of(b" \n\t"), Err(SourceError::Empty));
        assert_eq!(certificates_of(b"<html>not found</html>"), Err(SourceError::NoCertificate));
        assert_eq!(certificates_of(&ca[..ca.len() - 1]), Err(SourceError::NoCertificate), "a DER cut short");
        assert_eq!(
            certificates_of(b"-----BEGIN PUBLIC KEY-----\nAAAA\n-----END PUBLIC KEY-----\n"),
            Err(SourceError::NoCertificate)
        );
        for label in ["PRIVATE KEY", "RSA PRIVATE KEY", "EC PRIVATE KEY", "ENCRYPTED PRIVATE KEY", "OPENSSH PRIVATE KEY"] {
            let with_key = format!("{}-----BEGIN {label}-----\nAAAA\n-----END {label}-----\n", canonical_pem(&ca));
            assert_eq!(certificates_of(with_key.as_bytes()), Err(SourceError::PrivateKey), "{label}");
        }
        assert_eq!(
            certificates_of(b"-----BEGIN CERTIFICATE-----\nAAAA\n"),
            Err(SourceError::Pem("a block has no END line"))
        );
        let many = canonical_pem(&ca).repeat(MAX_CERTIFICATES + 1);
        assert_eq!(certificates_of(many.as_bytes()), Err(SourceError::TooMany { found: MAX_CERTIFICATES + 1 }));
        assert_eq!(certificates_of(canonical_pem(&ca).repeat(MAX_CERTIFICATES).as_bytes()).unwrap().len(), MAX_CERTIFICATES);
        let large = vec![b'A'; MAX_SOURCE_BYTES + 1];
        assert_eq!(certificates_of(&large), Err(SourceError::TooLarge { bytes: MAX_SOURCE_BYTES + 1 }));
        assert_eq!(
            SourceError::PrivateKey.to_string(),
            "carries a private key: a certificate authority's certificate alone is imported, never its key"
        );
    }

    /// The command writes a certificate as OpenSSL writes one: the block,
    /// 64 columns, nothing around it. The brain's loader reads that file as
    /// the one certificate it is.
    #[test]
    fn what_is_written_is_one_certificate_in_canonical_pem() {
        let der = fixture_der();
        let pem = canonical_pem(&der);
        assert_eq!(pem, FIXTURE, "byte for byte what OpenSSL wrote");
        assert_eq!(certificates_of(pem.as_bytes()).unwrap(), vec![der.clone()]);
        assert_eq!(reqwest::Certificate::from_pem_bundle(pem.as_bytes()).unwrap().len(), 1);
        let (ca, _) = minted();
        let pem = canonical_pem(&ca);
        assert!(pem.starts_with("-----BEGIN CERTIFICATE-----\n") && pem.ends_with("\n-----END CERTIFICATE-----\n"));
        assert_eq!(pem.matches("-----BEGIN ").count(), 1);
        assert!(pem.lines().all(|line| line.len() <= 64), "{pem}");
        assert_eq!(certificates_of(pem.as_bytes()).unwrap(), vec![ca]);
    }

    /// Whether a certificate can be a root is the root store's verdict, the
    /// one the trust loader acts on: a certificate passes, and DER that is
    /// a well-formed element and no certificate does not.
    #[test]
    fn the_root_store_says_whether_a_certificate_can_be_a_root() {
        let (ca, leaf) = minted();
        assert!(as_root(&fixture_der()).is_ok());
        assert!(as_root(&ca).is_ok());
        assert!(as_root(&leaf).is_ok(), "the store takes a certificate that is no CA as an anchor");
        let not_a_certificate = [0x30, 0x03, 0x02, 0x01, 0x00];
        assert_eq!(certificates_of(&not_a_certificate).unwrap().len(), 1, "a DER element is taken as a source");
        assert!(as_root(&not_a_certificate).is_err(), "and the store refuses it");
        assert_eq!(CertInfo::read(&not_a_certificate), None);
    }
}
