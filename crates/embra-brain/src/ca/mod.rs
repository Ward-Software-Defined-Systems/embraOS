//! Certificate authorities on a running machine: the `/ca` command.
//!
//! Beside the public roots a machine trusts the certificate authorities its
//! image was built with (`/usr/share/embra/ca-certificates/`, read-only)
//! and the ones in STATE (`/embra/state/ca-certificates/`). STATE's
//! directory used to be filled from outside the OS alone (`seed-state.sh
//! --ca-dir`), so a machine installed from the ISO had no way to trust the
//! operator's own authority: the provider gateway's, a self-hosted GitLab's.
//!
//! `/ca import <source>` reads a source ([`fetch`]) and shows each
//! certificate with its SHA-256; nothing is written. `/ca import <source>
//! sha256:<fingerprint>` reads it again and writes the one certificate with
//! that fingerprint, as canonical PEM made from the bytes that were hashed.
//! The fingerprint is the whole trust: a URL is fetched without
//! verification, the workspace is where the tools write, and a
//! certificate's own text is its sender's. The brain's roots are then read
//! again ([`crate::net::RootStore::reload`]), so the next client built
//! trusts the certificate. git and the OS updater read the bundle embrad
//! made at boot ([`BOOT_BUNDLE`]) and trust it after the next one.

pub(crate) mod cert;
pub(crate) mod fetch;

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use cert::{CertInfo, Fingerprint};
use fetch::{How, Read, Source};

/// The bundle embrad merges at boot for git and the OS updater
/// (`embrad/src/ca_bundle.rs`, `CA_MERGED_BUNDLE`): the stock roots and the
/// certificates of the two directories as they stood then. Absent when
/// there were none.
pub(crate) const BOOT_BUNDLE: &str = "/embra/ephemeral/ca-bundle.crt";

pub(crate) const USAGE: &str = "Usage: /ca [list] | /ca import <url | uploads/<file>> [sha256:<fingerprint>] | /ca remove <file>\n\
  /ca                                        The certificate authorities this machine trusts beside the public ones\n\
  /ca import <source>                        Read a certificate from a URL or a file in the workspace and show it; nothing is written\n\
  /ca import <source> sha256:<fingerprint>   Import the certificate with that SHA-256: the brain trusts it at once, git and the OS updater after the next boot\n\
  /ca remove <file>                          Remove one of STATE's certificates";

/// What a run of the command answers.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Reply {
    pub(crate) text: String,
    /// The command did not do what was asked.
    pub(crate) refused: bool,
}

impl Reply {
    fn said(text: impl Into<String>) -> Self {
        Self { text: text.into(), refused: false }
    }

    fn refusal(text: impl Into<String>) -> Self {
        Self { text: text.into(), refused: true }
    }
}

/// What the command's arguments ask for.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    List,
    Usage,
    Import { source: String, fingerprint: Option<String> },
    Remove(String),
}

/// The arguments of `/ca`. An import's source is everything before the
/// fingerprint, since an uploaded file's name may hold spaces; the
/// fingerprint is what follows a last ` sha256:`, or a last word that is
/// one.
pub(crate) fn parse(args: &str) -> Action {
    let args = args.trim();
    let (word, rest) = match args.split_once(char::is_whitespace) {
        Some((word, rest)) => (word, rest.trim()),
        None => (args, ""),
    };
    match word {
        "" | "list" if rest.is_empty() => Action::List,
        "import" if !rest.is_empty() => {
            let (source, fingerprint) = split_fingerprint(rest);
            if source.is_empty() {
                Action::Usage
            } else {
                Action::Import { source: source.to_string(), fingerprint: fingerprint.map(str::to_string) }
            }
        }
        "remove" if !rest.is_empty() => Action::Remove(rest.to_string()),
        _ => Action::Usage,
    }
}

fn split_fingerprint(rest: &str) -> (&str, Option<&str>) {
    // ASCII lowering keeps every byte offset.
    if let Some(at) = rest.to_ascii_lowercase().rfind(" sha256:") {
        return (rest[..at].trim(), Some(rest[at + 1..].trim()));
    }
    if let Some((head, last)) = rest.rsplit_once(char::is_whitespace)
        && Fingerprint::parse(last).is_some()
    {
        return (head.trim(), Some(last));
    }
    (rest, None)
}

/// One certificate of a file or of a source.
struct Entry {
    der: Vec<u8>,
    fingerprint: Fingerprint,
    info: Option<CertInfo>,
    /// Why the root store would not take it, when it would not.
    refused: Option<String>,
}

impl Entry {
    fn of(der: Vec<u8>) -> Self {
        Self {
            fingerprint: Fingerprint::of(&der),
            info: CertInfo::read(&der),
            refused: cert::as_root(&der).err(),
            der,
        }
    }

    fn subject(&self) -> &str {
        match &self.info {
            Some(info) if !info.subject.is_empty() => &info.subject,
            Some(_) => "(an empty subject)",
            None => "(its fields could not be read)",
        }
    }

    /// The lines the command shows of a certificate, two spaces in.
    fn described(&self, now: i64) -> Vec<String> {
        let mut lines = Vec::new();
        match &self.info {
            Some(info) => {
                lines.push(format!("  subject  {}", self.subject()));
                let issuer = if info.self_issued { "(itself)" } else { info.issuer.as_str() };
                lines.push(format!("  issuer   {issuer}"));
                let dates = if now < info.not_before {
                    ", not valid yet by this machine's clock"
                } else if now > info.not_after {
                    ", expired by this machine's clock"
                } else {
                    ""
                };
                lines.push(format!("  valid    {} to {}{dates}", cert::utc(info.not_before), cert::utc(info.not_after)));
                let ca = if info.is_ca {
                    "yes"
                } else {
                    "no: it is not a certificate authority's certificate (a server's own certificate is trusted for as long as the server presents this one)"
                };
                lines.push(format!("  CA       {ca}"));
            }
            None => lines.push(format!("  subject  {}", self.subject())),
        }
        lines.push(format!("  SHA-256  {}", self.fingerprint.shown()));
        if let Some(why) = &self.refused {
            lines.push(format!("  refused  the root store does not take it: {why}"));
        }
        lines
    }
}

/// Which directory a certificate file is in.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Origin {
    Image,
    State,
}

impl Origin {
    fn label(self) -> &'static str {
        match self {
            Self::Image => "image",
            Self::State => "state",
        }
    }
}

/// One file of a certificate directory.
struct CaFile {
    origin: Origin,
    name: String,
    entries: Vec<Entry>,
    /// Why the file gives no certificates, when it gives none.
    problem: Option<&'static str>,
}

impl CaFile {
    fn shown(&self) -> String {
        format!("{}/{}", self.origin.label(), self.name)
    }
}

/// Where the certificate files are, and what a run of the command needs
/// beside them. The process's is [`CaEnv::in_force`]; a test names its own.
pub(crate) struct CaEnv<'a> {
    /// The image's directory, read-only; none under the development
    /// override.
    pub(crate) image: Option<PathBuf>,
    /// STATE's directory: the one the command writes.
    pub(crate) state: PathBuf,
    pub(crate) boot_bundle: PathBuf,
    pub(crate) workspace: PathBuf,
    pub(crate) store: &'a crate::net::RootStore,
}

impl CaEnv<'static> {
    /// The directories in force and the process's roots.
    pub(crate) fn in_force() -> Self {
        let mut dirs = crate::net::dirs_in_force();
        let state = dirs.pop().unwrap_or_else(|| PathBuf::from(crate::net::OPERATOR_CA_DIR));
        Self {
            image: dirs.pop(),
            state,
            boot_bundle: PathBuf::from(BOOT_BUNDLE),
            workspace: PathBuf::from(crate::tools::engineering::WORKSPACE_ROOT),
            store: crate::net::process_roots(),
        }
    }
}

impl CaEnv<'_> {
    /// The directories, in the trust loader's order.
    fn dirs(&self) -> Vec<PathBuf> {
        self.image.iter().cloned().chain([self.state.clone()]).collect()
    }

    /// Reads the roots again and asks the provider probe to run: what was
    /// refused for its issuer a moment ago answers now.
    fn reload(&self) {
        self.store.reload(&self.dirs());
        crate::provider::health::request_probe();
    }

    /// The certificate files of both directories, the image's first, by
    /// name within one: the files the trust loader reads.
    fn files(&self) -> Vec<CaFile> {
        let mut files = Vec::new();
        let dirs = self.image.iter().map(|dir| (Origin::Image, dir)).chain([(Origin::State, &self.state)]);
        for (origin, dir) in dirs {
            let Ok(entries) = std::fs::read_dir(dir) else { continue };
            let mut names: Vec<String> = entries
                .flatten()
                .filter(|e| e.path().is_file())
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .filter(|name| has_certificate_extension(name))
                .collect();
            names.sort();
            for name in names {
                let (entries, problem) = match std::fs::read(dir.join(&name)).map(|bytes| cert::file_certificates(&bytes)) {
                    Ok(Ok(ders)) if ders.is_empty() => (Vec::new(), Some("it holds no certificate")),
                    Ok(Ok(ders)) => (ders.into_iter().map(Entry::of).collect(), None),
                    Ok(Err(fault)) => (Vec::new(), Some(fault)),
                    Err(_) => (Vec::new(), Some("it could not be read")),
                };
                files.push(CaFile { origin, name, entries, problem });
            }
        }
        files
    }

    /// The file that holds a certificate, as the listing names it.
    fn holder_of(&self, fingerprint: &Fingerprint) -> Option<String> {
        self.files()
            .into_iter()
            .find(|file| file.entries.iter().any(|e| e.fingerprint == *fingerprint))
            .map(|file| file.shown())
    }

    /// The certificates of the bundle git and the OS updater read this boot.
    fn boot_bundle_holds(&self) -> HashSet<Fingerprint> {
        std::fs::read(&self.boot_bundle)
            .ok()
            .and_then(|bytes| cert::file_certificates(&bytes).ok())
            .map(|ders| ders.iter().map(|der| Fingerprint::of(der)).collect())
            .unwrap_or_default()
    }

    /// `/ca`, `/ca list`.
    pub(crate) fn list(&self) -> String {
        let files = self.files();
        if files.is_empty() {
            return "No certificate authority beside the public ones: the image carries none and STATE holds none.\n\
                    /ca import <url | uploads/<file>> reads one and shows it."
                .to_string();
        }
        let roots = self.store.current(|| self.dirs());
        let bundle = self.boot_bundle_holds();
        let now = chrono::Utc::now().timestamp();
        let mut out = vec!["Certificate authorities this machine trusts beside the public ones:".to_string()];
        for file in &files {
            out.push(String::new());
            let whose = match file.origin {
                Origin::Image => "came with the image, read-only",
                Origin::State => "on this machine's STATE",
            };
            out.push(format!("{} ({whose})", file.shown()));
            if let Some(problem) = file.problem {
                out.push(format!("  not read: {problem}"));
            }
            for entry in &file.entries {
                out.extend(entry.described(now));
                let brain = if roots.trusts(&entry.fingerprint) {
                    "now"
                } else if entry.refused.is_some() {
                    "no"
                } else {
                    "not yet (the directories were not read since it came)"
                };
                let boot = if bundle.contains(&entry.fingerprint) { "this boot" } else { "from the next boot" };
                out.push(format!("  trusted  the brain: {brain}; git and the OS updater: {boot}"));
            }
        }
        out.join("\n")
    }

    /// `/ca import`, on a source that was read.
    pub(crate) fn import(&self, read: &Read, wanted: Option<Fingerprint>, typed: &str) -> Reply {
        let entries: Vec<Entry> = match cert::certificates_of(&read.bytes) {
            Ok(ders) => ders.into_iter().map(Entry::of).collect(),
            Err(why) => return Reply::refusal(format!("{} {why}. Nothing was written.", read.from)),
        };
        let Some(wanted) = wanted else {
            return Reply::said(shown_for_confirmation(read, &entries, typed));
        };
        let Some(entry) = entries.iter().find(|e| e.fingerprint == wanted) else {
            let held: Vec<String> =
                entries.iter().map(|e| format!("  {}  {}", e.fingerprint.shown(), e.subject())).collect();
            return Reply::refusal(format!(
                "No certificate of {} has SHA-256 {}. It holds:\n{}\nNothing was written.",
                read.from,
                wanted.shown(),
                held.join("\n")
            ));
        };
        if let Some(why) = &entry.refused {
            return Reply::refusal(format!(
                "The certificate with SHA-256 {} cannot be a root: {why}. Nothing was written.",
                wanted.shown()
            ));
        }
        if let Some(holder) = self.holder_of(&wanted) {
            // A file put there by hand since the last reading is a root
            // from here on as well.
            self.reload();
            return Reply::said(format!(
                "Already trusted: {holder} holds this certificate ({}, SHA-256 {}). Nothing was written.",
                entry.subject(),
                wanted.shown()
            ));
        }
        let name = file_name_for(entry);
        match write_new(&self.state, &name, &cert::canonical_pem(&entry.der)) {
            Err(WriteError::Exists) => Reply::refusal(format!(
                "state/{name} holds another certificate. /ca remove {name} first, if that one is to go. Nothing was written."
            )),
            Err(WriteError::Io(e)) => {
                Reply::refusal(format!("state/{name} could not be written: {e}. Nothing was written."))
            }
            Ok(()) => {
                self.reload();
                tracing::info!(
                    target: "ca",
                    file = %name,
                    sha256 = %wanted.hex(),
                    from = %read.from,
                    "certificate authority imported"
                );
                Reply::said(format!(
                    "Imported: state/{name} ({}, SHA-256 {}).\n\
                     The brain trusts it from its next request: the setup's gateway step, /local-ai-gateway, the providers, the GitLab and GitHub tools, http_get and http_request. A request in flight keeps the roots it started with.\n\
                     git and the OS updater trust it after the next boot (/reboot now).",
                    entry.subject(),
                    wanted.shown()
                ))
            }
        }
    }

    /// `/ca remove <file>`: one of STATE's files, never the image's.
    pub(crate) fn remove(&self, name: &str) -> Reply {
        let typed = cert::shown(name.trim(), 120);
        let name = name.trim();
        let name = name.strip_prefix("state/").unwrap_or(name);
        let images = |name: &str| self.image.as_ref().is_some_and(|dir| dir.join(name).is_file());
        if let Some(of_image) = name.strip_prefix("image/")
            && is_plain_name(of_image)
            && images(of_image)
        {
            return Reply::refusal(image_refusal(of_image));
        }
        if !is_plain_name(name) || !has_certificate_extension(name) {
            return Reply::refusal(format!(
                "{typed} is not the name of one of STATE's certificate files; /ca list names them. Nothing was removed."
            ));
        }
        let path = self.state.join(name);
        if !path.is_file() {
            return Reply::refusal(if images(name) {
                image_refusal(name)
            } else {
                format!("STATE holds no certificate file {name}; /ca list names them. Nothing was removed.")
            });
        }
        let held: Vec<(Fingerprint, String)> = std::fs::read(&path)
            .ok()
            .and_then(|bytes| cert::file_certificates(&bytes).ok())
            .unwrap_or_default()
            .into_iter()
            .map(Entry::of)
            .map(|e| (e.fingerprint, e.subject().to_string()))
            .collect();
        if let Err(e) = std::fs::remove_file(&path) {
            return Reply::refusal(format!("state/{name} could not be removed: {e}. Nothing was removed."));
        }
        sync_dir(&self.state);
        self.reload();
        tracing::info!(target: "ca", file = %name, "certificate authority removed");
        let mut text = format!("Removed: state/{name}");
        if let Some((fingerprint, subject)) = held.first() {
            text.push_str(&format!(" ({subject}, SHA-256 {})", fingerprint.shown()));
        }
        text.push_str(
            ".\nThe brain no longer trusts it from its next request. git and the OS updater trust it until the next boot.",
        );
        for (fingerprint, _) in &held {
            if let Some(holder) = self.holder_of(fingerprint) {
                text.push_str(&format!("\nThe same certificate is still in {holder}, and stays trusted."));
            }
        }
        Reply::said(text)
    }
}

fn image_refusal(name: &str) -> String {
    format!(
        "image/{name} came with the image: it goes with an image built without it (Imported_CAs/ on the build host). Nothing was removed."
    )
}

/// The reply to an import without a fingerprint: what the source holds,
/// what to compare, and the line that imports each certificate.
fn shown_for_confirmation(read: &Read, entries: &[Entry], typed: &str) -> String {
    let how = match read.how {
        How::Unverified => {
            " over https, without verifying its certificate (the path and any query went out unverified)"
        }
        How::Plain => " over plain http (the path and any query went out in the clear)",
        How::Workspace => " (a file in the workspace)",
    };
    let count = match entries.len() {
        1 => "1 certificate".to_string(),
        n => format!("{n} certificates"),
    };
    let now = chrono::Utc::now().timestamp();
    let mut out = vec![format!("Read from {}{how}: {count}. Nothing is written yet.", read.from)];
    for entry in entries {
        out.push(String::new());
        out.extend(entry.described(now));
        out.push(format!("  import   /ca import {typed} sha256:{}", entry.fingerprint.shown()));
    }
    out.push(String::new());
    out.push(
        "Compare the SHA-256 with the one the certificate's owner shows (WSDS Local AI Gateway: local-ai-gateway tls show). \
         The subject is what the certificate says of itself. When they match, run its import line."
            .to_string(),
    );
    out.join("\n")
}

/// The loader's and embrad's extensions.
fn has_certificate_extension(name: &str) -> bool {
    matches!(Path::new(name).extension().and_then(|e| e.to_str()), Some("pem") | Some("crt"))
}

/// One path component, and nothing that is not a file's own name.
fn is_plain_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 255
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && !name.chars().any(char::is_control)
}

/// The name a certificate is written under: its common name (else its
/// organization) in lower-case letters, digits and dashes, and the first
/// eight digits of its fingerprint. Within the build's rule for a name in
/// `Imported_CAs/` (letters, digits, dots, dashes, underscores).
fn file_name_for(entry: &Entry) -> String {
    let hint = entry.info.as_ref().and_then(|info| info.name_hint.as_deref()).unwrap_or("");
    let mut slug = String::new();
    for c in hint.chars() {
        if slug.len() >= 40 {
            break;
        }
        if c.is_ascii_alphanumeric() {
            slug.push(c.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    let slug = slug.trim_matches('-');
    let slug = if slug.is_empty() { "ca" } else { slug };
    format!("{slug}-{}.pem", &entry.fingerprint.hex()[..8])
}

enum WriteError {
    /// A file of that name is there.
    Exists,
    Io(std::io::Error),
}

/// Writes a new file whole and never over another: the text goes to a
/// temporary file beside it, synced, and is then linked under its name,
/// which fails when the name is taken.
fn write_new(dir: &Path, name: &str, text: &str) -> Result<(), WriteError> {
    use std::io::Write as _;
    std::fs::create_dir_all(dir).map_err(WriteError::Io)?;
    let temporary = dir.join(format!(".{name}.{}.tmp", std::process::id()));
    let written = (|| {
        let mut file = std::fs::File::create(&temporary)?;
        file.write_all(text.as_bytes())?;
        file.sync_all()
    })();
    let linked = written.and_then(|()| std::fs::hard_link(&temporary, dir.join(name)));
    let _ = std::fs::remove_file(&temporary);
    match linked {
        Ok(()) => {
            sync_dir(dir);
            Ok(())
        }
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => Err(WriteError::Exists),
        Err(e) => Err(WriteError::Io(e)),
    }
}

/// The directory's own entry reaches the disk: a name written or removed
/// survives a power cut.
fn sync_dir(dir: &Path) {
    if let Ok(handle) = std::fs::File::open(dir) {
        let _ = handle.sync_all();
    }
}

/// The whole command: its arguments to its reply.
pub(crate) async fn answer(args: &str, env: &CaEnv<'_>) -> Reply {
    match parse(args) {
        Action::Usage => Reply::said(USAGE),
        Action::List => Reply::said(env.list()),
        Action::Remove(name) => env.remove(&name),
        Action::Import { source, fingerprint } => {
            let wanted = match fingerprint.as_deref() {
                None => None,
                Some(text) => match Fingerprint::parse(text) {
                    Some(fingerprint) => Some(fingerprint),
                    None => {
                        return Reply::refusal(format!(
                            "{} is not a SHA-256 fingerprint: 64 hex digits, with or without colons between the pairs. Nothing was written.",
                            cert::shown(text, 120)
                        ));
                    }
                },
            };
            let read = match Source::parse(&source) {
                Ok(source) => fetch::read(&source, &env.workspace).await,
                Err(why) => Err(why),
            };
            match read {
                Ok(read) => env.import(&read, wanted, &source),
                Err(why) => Reply::refusal(format!("{why} Nothing was written.")),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::net::RootStore;
    use rcgen::{BasicConstraints, CertificateParams, DistinguishedName, DnType, DnValue, ExtendedKeyUsagePurpose, IsCa, KeyPair, KeyUsagePurpose};
    use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
    use std::sync::Arc;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    /// A certificate authority and, under it, a certificate for 127.0.0.1
    /// with its key.
    struct Minted {
        ca_der: Vec<u8>,
        ca_pem: String,
        leaf: CertificateDer<'static>,
        leaf_key: Vec<u8>,
    }

    fn mint(common_name: &str) -> Minted {
        let ca_key = KeyPair::generate().unwrap();
        let mut ca_params = CertificateParams::new(Vec::<String>::new()).unwrap();
        ca_params.is_ca = IsCa::Ca(BasicConstraints::Unconstrained);
        ca_params.key_usages = vec![KeyUsagePurpose::KeyCertSign, KeyUsagePurpose::CrlSign];
        ca_params.distinguished_name = DistinguishedName::new();
        ca_params.distinguished_name.push(DnType::CommonName, DnValue::Utf8String(common_name.to_string()));
        let ca = ca_params.self_signed(&ca_key).unwrap();
        let leaf_key = KeyPair::generate().unwrap();
        let mut leaf_params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        leaf_params.extended_key_usages = vec![ExtendedKeyUsagePurpose::ServerAuth];
        let leaf = leaf_params.signed_by(&leaf_key, &ca, &ca_key).unwrap();
        Minted { ca_der: ca.der().to_vec(), ca_pem: ca.pem(), leaf: leaf.der().clone(), leaf_key: leaf_key.serialize_der() }
    }

    /// A scratch machine: the two directories, a workspace, no boot bundle,
    /// and roots of its own.
    struct Scratch {
        root: PathBuf,
        store: RootStore,
    }

    impl Scratch {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!("brain-ca-{}-{name}", std::process::id()));
            let _ = std::fs::remove_dir_all(&root);
            for dir in ["image", "state", "workspace/uploads"] {
                std::fs::create_dir_all(root.join(dir)).unwrap();
            }
            Self { root, store: RootStore::new() }
        }

        fn env(&self) -> CaEnv<'_> {
            CaEnv {
                image: Some(self.root.join("image")),
                state: self.root.join("state"),
                boot_bundle: self.root.join("ca-bundle.crt"),
                workspace: self.root.join("workspace"),
                store: &self.store,
            }
        }

        fn upload(&self, name: &str, bytes: &[u8]) {
            std::fs::write(self.root.join("workspace/uploads").join(name), bytes).unwrap();
        }

        fn state_files(&self) -> Vec<String> {
            let mut names: Vec<String> = std::fs::read_dir(self.root.join("state"))
                .unwrap()
                .flatten()
                .map(|e| e.file_name().to_string_lossy().into_owned())
                .collect();
            names.sort();
            names
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// A TLS listener on the loopback presenting `leaf`: for `connections`
    /// connections it answers `response` to a handshake that completes.
    async fn tls_server(leaf: CertificateDer<'static>, key: Vec<u8>, connections: usize, response: Vec<u8>) -> std::net::SocketAddr {
        let config = rustls::ServerConfig::builder_with_provider(Arc::new(rustls::crypto::ring::default_provider()))
            .with_safe_default_protocol_versions()
            .unwrap()
            .with_no_client_auth()
            .with_single_cert(vec![leaf], PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(key)))
            .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(config));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            for _ in 0..connections {
                let Ok((tcp, _)) = listener.accept().await else { return };
                let Ok(mut tls) = acceptor.accept(tcp).await else { continue };
                let mut head = vec![0u8; 4096];
                let _ = tls.read(&mut head).await;
                let _ = tls.write_all(&response).await;
                tls.shutdown().await.ok();
            }
        });
        addr
    }

    fn http_200(headers: &str, body: &[u8]) -> Vec<u8> {
        let mut response =
            format!("HTTP/1.1 200 OK\r\n{headers}content-length: {}\r\nconnection: close\r\n\r\n", body.len()).into_bytes();
        response.extend_from_slice(body);
        response
    }

    #[test]
    fn a_source_with_spaces_is_taken_whole_before_the_fingerprint() {
        let fp = "14:31:2F:40:56:6E:33:29:3C:45:10:51:54:8D:7A:63:A5:71:75:74:DC:77:7A:C5:AB:2D:A4:11:76:FC:46:22";
        let import = |source: &str, fingerprint: Option<&str>| Action::Import {
            source: source.to_string(),
            fingerprint: fingerprint.map(str::to_string),
        };
        assert_eq!(parse(""), Action::List);
        assert_eq!(parse("  list "), Action::List);
        assert_eq!(parse("list all"), Action::Usage);
        assert_eq!(parse("help"), Action::Usage);
        assert_eq!(parse("import"), Action::Usage);
        assert_eq!(parse("remove"), Action::Usage);
        assert_eq!(parse("import uploads/ca.pem"), import("uploads/ca.pem", None));
        assert_eq!(
            parse("import uploads/Local AI Gateway ca.pem"),
            import("uploads/Local AI Gateway ca.pem", None),
            "an upload's name keeps its spaces"
        );
        let with_prefix = format!("sha256:{fp}");
        assert_eq!(
            parse(&format!("import uploads/Local AI Gateway ca.pem {with_prefix}")),
            import("uploads/Local AI Gateway ca.pem", Some(&with_prefix))
        );
        assert_eq!(
            parse(&format!("import https://gw.example:8787/ca.pem  SHA256:{fp}")),
            import("https://gw.example:8787/ca.pem", Some(&format!("SHA256:{fp}")))
        );
        assert_eq!(
            parse(&format!("import uploads/ca.pem {fp}")),
            import("uploads/ca.pem", Some(fp)),
            "a last word that is a fingerprint is one without the prefix"
        );
        assert_eq!(parse("import uploads/ca.pem sha256:zz"), import("uploads/ca.pem", Some("sha256:zz")), "what follows the prefix is judged later");
        assert_eq!(parse("import sha256:aa"), import("sha256:aa", None), "a fingerprint alone names no source");
        assert_eq!(parse("remove  state/gateway-ca-14312f40.pem "), Action::Remove("state/gateway-ca-14312f40.pem".into()));
    }

    /// An import without a fingerprint shows and writes nothing; with
    /// another certificate's fingerprint, or with one that is no
    /// fingerprint, it is refused and writes nothing.
    #[tokio::test]
    async fn an_import_without_a_fingerprint_writes_nothing() {
        let scratch = Scratch::new("show");
        let minted = mint("Gateway CA");
        let other = mint("Other CA");
        scratch.upload("gateway ca.pem", format!("{}{}", minted.ca_pem, other.ca_pem).as_bytes());
        let env = scratch.env();
        let fingerprint = Fingerprint::of(&minted.ca_der);
        assert_eq!(scratch.store.current(|| env.dirs()).generation, 1);

        let shown = answer("import uploads/gateway ca.pem", &env).await;
        assert!(!shown.refused, "{}", shown.text);
        assert!(
            shown.text.starts_with("Read from uploads/gateway ca.pem (a file in the workspace): 2 certificates. Nothing is written yet."),
            "{}",
            shown.text
        );
        assert!(shown.text.contains("  subject  CN=Gateway CA\n  issuer   (itself)\n"), "{}", shown.text);
        assert!(shown.text.contains("  CA       yes\n"), "{}", shown.text);
        assert!(
            shown.text.contains(&format!("  import   /ca import uploads/gateway ca.pem sha256:{}", fingerprint.shown())),
            "the line that imports it, with the source as it was typed: {}",
            shown.text
        );
        assert!(shown.text.contains("local-ai-gateway tls show"), "{}", shown.text);
        assert!(scratch.state_files().is_empty(), "nothing is written");
        assert_eq!(scratch.store.current(Vec::new).generation, 1, "and the roots are not read again");

        let stranger = Fingerprint::of(&mint("Stranger").ca_der);
        let wrong = answer(&format!("import uploads/gateway ca.pem sha256:{}", stranger.hex()), &env).await;
        assert!(wrong.refused);
        assert!(wrong.text.starts_with(&format!("No certificate of uploads/gateway ca.pem has SHA-256 {}. It holds:\n", stranger.shown())), "{}", wrong.text);
        assert!(wrong.text.contains(&format!("  {}  CN=Gateway CA\n", fingerprint.shown())), "{}", wrong.text);
        assert!(wrong.text.ends_with("Nothing was written."), "{}", wrong.text);

        let not_one = answer("import uploads/gateway ca.pem sha256:abc", &env).await;
        assert!(not_one.refused && not_one.text.starts_with("sha256:abc is not a SHA-256 fingerprint"), "{}", not_one.text);
        assert!(scratch.state_files().is_empty());
    }

    /// The import writes the one certificate the fingerprint names, as
    /// canonical PEM under a name from its subject, and the roots are read
    /// again: a client built from them trusts it. The same import again
    /// names the file and writes nothing.
    #[tokio::test]
    async fn an_import_writes_the_certificate_with_the_fingerprint_and_the_roots_are_read_again() {
        let scratch = Scratch::new("import");
        let minted = mint("WSDS Local AI Gateway CA");
        let other = mint("Other CA");
        scratch.upload("ca.pem", format!("some text\n{}{}", other.ca_pem, minted.ca_pem).as_bytes());
        let env = scratch.env();
        let fingerprint = Fingerprint::of(&minted.ca_der);
        assert!(!scratch.store.current(|| env.dirs()).trusts(&fingerprint));

        let imported = answer(&format!("import uploads/ca.pem sha256:{}", fingerprint.shown()), &env).await;
        assert!(!imported.refused, "{}", imported.text);
        let name = format!("wsds-local-ai-gateway-ca-{}.pem", &fingerprint.hex()[..8]);
        assert!(
            imported.text.starts_with(&format!("Imported: state/{name} (CN=WSDS Local AI Gateway CA, SHA-256 {}).", fingerprint.shown())),
            "{}",
            imported.text
        );
        assert!(imported.text.contains("The brain trusts it from its next request"), "{}", imported.text);
        assert!(imported.text.contains("git and the OS updater trust it after the next boot"), "{}", imported.text);
        assert_eq!(scratch.state_files(), vec![name.clone()], "one file, and no temporary one left");
        let written = std::fs::read_to_string(scratch.root.join("state").join(&name)).unwrap();
        assert_eq!(written, cert::canonical_pem(&minted.ca_der), "the one certificate, and no text of the source's");

        let roots = scratch.store.current(Vec::new);
        assert_eq!(roots.generation, 2, "the roots were read again");
        assert!(roots.trusts(&fingerprint));
        assert!(!roots.trusts(&Fingerprint::of(&other.ca_der)), "the source's other certificate is no root");

        let again = answer(&format!("import uploads/ca.pem {}", fingerprint.hex()), &env).await;
        assert!(!again.refused);
        assert!(again.text.starts_with(&format!("Already trusted: state/{name} holds this certificate")), "{}", again.text);
        assert_eq!(scratch.state_files(), vec![name]);
    }

    /// A certificate the image already carries is named and not written to
    /// STATE as well.
    #[tokio::test]
    async fn a_certificate_already_trusted_is_named_and_not_written_twice() {
        let scratch = Scratch::new("held");
        let minted = mint("Build CA");
        std::fs::write(scratch.root.join("image/build.crt"), &minted.ca_pem).unwrap();
        scratch.upload("ca.pem", minted.ca_pem.as_bytes());
        let fingerprint = Fingerprint::of(&minted.ca_der);
        let held = answer(&format!("import uploads/ca.pem sha256:{}", fingerprint.hex()), &scratch.env()).await;
        assert!(!held.refused);
        assert!(held.text.starts_with("Already trusted: image/build.crt holds this certificate (CN=Build CA"), "{}", held.text);
        assert!(scratch.state_files().is_empty());
    }

    /// A file of the name the import would take, holding something else, is
    /// never overwritten.
    #[tokio::test]
    async fn a_name_that_holds_another_certificate_is_not_overwritten() {
        let scratch = Scratch::new("taken");
        let minted = mint("Gateway CA");
        scratch.upload("ca.pem", minted.ca_pem.as_bytes());
        let fingerprint = Fingerprint::of(&minted.ca_der);
        let name = format!("gateway-ca-{}.pem", &fingerprint.hex()[..8]);
        let other = mint("Other CA");
        std::fs::write(scratch.root.join("state").join(&name), &other.ca_pem).unwrap();

        let refused = answer(&format!("import uploads/ca.pem sha256:{}", fingerprint.hex()), &scratch.env()).await;
        assert!(refused.refused);
        assert_eq!(
            refused.text,
            format!("state/{name} holds another certificate. /ca remove {name} first, if that one is to go. Nothing was written.")
        );
        assert_eq!(std::fs::read_to_string(scratch.root.join("state").join(&name)).unwrap(), other.ca_pem);
        assert_eq!(scratch.state_files(), vec![name], "no temporary file is left either");
    }

    #[tokio::test]
    async fn a_source_with_a_private_key_or_without_a_certificate_is_refused() {
        let scratch = Scratch::new("refused");
        let minted = mint("Gateway CA");
        scratch.upload("with-key.pem", format!("{}-----BEGIN PRIVATE KEY-----\nAAAA\n-----END PRIVATE KEY-----\n", minted.ca_pem).as_bytes());
        scratch.upload("page.pem", b"<html>sign in</html>");
        let env = scratch.env();
        let with_key = answer("import uploads/with-key.pem", &env).await;
        assert!(with_key.refused);
        assert_eq!(
            with_key.text,
            "uploads/with-key.pem carries a private key: a certificate authority's certificate alone is imported, never its key. Nothing was written."
        );
        let page = answer(&format!("import uploads/page.pem sha256:{}", Fingerprint::of(&minted.ca_der).hex()), &env).await;
        assert!(page.refused && page.text.starts_with("uploads/page.pem holds no certificate"), "{}", page.text);
        let missing = answer("import uploads/none.pem", &env).await;
        assert!(missing.refused && missing.text.ends_with("Nothing was written."), "{}", missing.text);
        let credentials = answer("import https://user:pw@127.0.0.1:9/ca.pem", &env).await;
        assert!(credentials.refused && credentials.text.starts_with("Credentials in the URL are refused"), "{}", credentials.text);
        assert!(scratch.state_files().is_empty());
    }

    /// What the command writes, each reader of the directory takes: the
    /// brain's loader, embrad's merge (a `.pem` or `.crt` under its size
    /// cap with one BEGIN/END pair) and the build's check of a file in
    /// `Imported_CAs/` (a name of letters, digits, dots, dashes and
    /// underscores; one certificate block, no other block, no key).
    #[tokio::test]
    async fn the_written_file_passes_the_brains_loader_embrads_rule_and_the_builds_rule() {
        let scratch = Scratch::new("readers");
        // A subject that is nothing a file name may hold.
        let minted = mint("Ünïcode / CA: \"Gateway\" & Söhne … 2026");
        scratch.upload("ca.pem", minted.ca_pem.as_bytes());
        let fingerprint = Fingerprint::of(&minted.ca_der);
        let imported = answer(&format!("import uploads/ca.pem sha256:{}", fingerprint.hex()), &scratch.env()).await;
        assert!(!imported.refused, "{}", imported.text);
        let names = scratch.state_files();
        let [name] = names.as_slice() else { panic!("one file: {names:?}") };
        assert_eq!(*name, format!("n-code-ca-gateway-s-hne-2026-{}.pem", &fingerprint.hex()[..8]));
        assert!(name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')), "the build's name rule: {name}");
        assert!(has_certificate_extension(name), "embrad's and the loader's extension");
        let text = std::fs::read_to_string(scratch.root.join("state").join(name)).unwrap();
        assert!(text.len() < cert::MAX_SOURCE_BYTES, "under embrad's cap");
        assert_eq!(text.matches("-----BEGIN CERTIFICATE-----").count(), 1, "embrad counts one pair");
        assert_eq!(text.matches("-----END CERTIFICATE-----").count(), 1);
        assert_eq!(text.matches("-----BEGIN ").count(), 1, "the build refuses another block");
        assert!(!text.contains("PRIVATE KEY"), "and a key");
        assert_eq!(reqwest::Certificate::from_pem_bundle(text.as_bytes()).unwrap().len(), 1, "the brain's loader");
        assert_eq!(cert::file_certificates(text.as_bytes()).unwrap(), vec![minted.ca_der.clone()]);

        // A subject with nothing to name a file by.
        let nameless = Entry { der: Vec::new(), fingerprint, info: None, refused: None };
        assert_eq!(file_name_for(&nameless), format!("ca-{}.pem", &fingerprint.hex()[..8]));
    }

    /// A removal takes one of STATE's files and reads the roots again; the
    /// image's files, a path and a name that is no certificate file are
    /// refused, and nothing outside the directory is touched.
    #[tokio::test]
    async fn a_removal_takes_a_state_file_and_never_the_images() {
        let scratch = Scratch::new("remove");
        let minted = mint("Gateway CA");
        let fingerprint = Fingerprint::of(&minted.ca_der);
        std::fs::write(scratch.root.join("image/build.crt"), &minted.ca_pem).unwrap();
        std::fs::write(scratch.root.join("state/gateway.pem"), &minted.ca_pem).unwrap();
        std::fs::write(scratch.root.join("state/notes.txt"), "keep").unwrap();
        std::fs::write(scratch.root.join("outside.pem"), "keep").unwrap();
        let env = scratch.env();
        assert!(scratch.store.current(|| env.dirs()).trusts(&fingerprint));

        for (name, says) in [
            ("build.crt", "image/build.crt came with the image"),
            ("image/build.crt", "image/build.crt came with the image"),
            ("../outside.pem", "is not the name of one of STATE's certificate files"),
            ("state/../../outside.pem", "is not the name of one of STATE's certificate files"),
            ("notes.txt", "is not the name of one of STATE's certificate files"),
            ("absent.pem", "STATE holds no certificate file absent.pem"),
        ] {
            let refused = answer(&format!("remove {name}"), &env).await;
            assert!(refused.refused && refused.text.contains(says), "{name}: {}", refused.text);
            assert!(refused.text.ends_with("Nothing was removed."), "{}", refused.text);
        }
        assert_eq!(scratch.state_files(), vec!["gateway.pem".to_string(), "notes.txt".to_string()]);
        assert!(scratch.root.join("outside.pem").is_file() && scratch.root.join("image/build.crt").is_file());
        assert_eq!(scratch.store.current(Vec::new).generation, 1, "a refusal reads nothing again");

        let removed = answer("remove state/gateway.pem", &env).await;
        assert!(!removed.refused, "{}", removed.text);
        assert!(removed.text.starts_with(&format!("Removed: state/gateway.pem (CN=Gateway CA, SHA-256 {}).", fingerprint.shown())), "{}", removed.text);
        assert!(removed.text.contains("git and the OS updater trust it until the next boot"), "{}", removed.text);
        assert!(removed.text.ends_with("The same certificate is still in image/build.crt, and stays trusted."), "{}", removed.text);
        assert_eq!(scratch.state_files(), vec!["notes.txt".to_string()]);
        assert_eq!(scratch.store.current(Vec::new).generation, 2);

        std::fs::remove_file(scratch.root.join("image/build.crt")).unwrap();
        std::fs::write(scratch.root.join("state/gateway.pem"), &minted.ca_pem).unwrap();
        let gone = answer("remove gateway.pem", &env).await;
        assert!(!gone.text.contains("still in"), "{}", gone.text);
        assert!(!scratch.store.current(Vec::new).trusts(&fingerprint), "no root after the reload");
    }

    /// The listing names each certificate of both directories, the image's
    /// first, and who trusts it: the brain by the roots in force, git and
    /// the OS updater by the bundle of this boot.
    #[tokio::test]
    async fn the_listing_names_each_certificate_and_who_trusts_it() {
        let scratch = Scratch::new("list");
        let env = scratch.env();
        let empty = answer("list", &env).await;
        assert!(empty.text.starts_with("No certificate authority beside the public ones"), "{}", empty.text);

        let build = mint("Build CA");
        let gateway = mint("Gateway CA");
        std::fs::write(scratch.root.join("image/build.crt"), &build.ca_pem).unwrap();
        std::fs::write(scratch.root.join("state/broken.pem"), "-----BEGIN CERTIFICATE-----\nAAAA\n").unwrap();
        std::fs::write(scratch.root.join("state/ignored.txt"), &gateway.ca_pem).unwrap();
        // The bundle of this boot: the image's certificate was there at boot.
        std::fs::write(scratch.root.join("ca-bundle.crt"), &build.ca_pem).unwrap();
        scratch.store.reload(&env.dirs());
        scratch.upload("ca.pem", gateway.ca_pem.as_bytes());
        let imported = answer(&format!("import uploads/ca.pem sha256:{}", Fingerprint::of(&gateway.ca_der).hex()), &env).await;
        assert!(!imported.refused, "{}", imported.text);

        let listing = answer("", &env).await.text;
        let gateway_file = format!("gateway-ca-{}.pem", &Fingerprint::of(&gateway.ca_der).hex()[..8]);
        let image_at = listing.find("image/build.crt (came with the image, read-only)").expect(&listing);
        let state_at = listing.find(&format!("state/{gateway_file} (on this machine's STATE)")).expect(&listing);
        assert!(image_at < state_at, "the image's first: {listing}");
        assert!(listing.contains("state/broken.pem (on this machine's STATE)\n  not read: a block has no END line"), "{listing}");
        assert!(!listing.contains("ignored.txt"), "{listing}");
        assert!(
            listing.contains(&format!("  SHA-256  {}\n  trusted  the brain: now; git and the OS updater: this boot", Fingerprint::of(&build.ca_der).shown())),
            "{listing}"
        );
        assert!(
            listing.contains(&format!("  SHA-256  {}\n  trusted  the brain: now; git and the OS updater: from the next boot", Fingerprint::of(&gateway.ca_der).shown())),
            "{listing}"
        );
        assert!(listing.contains("  subject  CN=Gateway CA\n  issuer   (itself)\n  valid    "), "{listing}");
    }

    /// A certificate that is no certificate authority is imported and said
    /// to be none. What the brain's roots do with it: a server that presents
    /// exactly that self-signed certificate is trusted, and a server's
    /// certificate that an authority issued is no anchor for itself.
    #[tokio::test]
    async fn a_certificate_that_is_not_a_ca_is_flagged_and_what_the_roots_do_with_it_is_pinned() {
        let scratch = Scratch::new("leaf");
        let env = scratch.env();
        // A self-signed server certificate, as a small service makes one.
        let key = KeyPair::generate().unwrap();
        let mut params = CertificateParams::new(vec!["127.0.0.1".to_string()]).unwrap();
        params.distinguished_name = DistinguishedName::new();
        params.distinguished_name.push(DnType::CommonName, DnValue::Utf8String("service.example".to_string()));
        let own = params.self_signed(&key).unwrap();
        scratch.upload("own.pem", own.pem().as_bytes());
        let fingerprint = Fingerprint::of(own.der());

        let shown = answer("import uploads/own.pem", &env).await;
        assert!(shown.text.contains("  CA       no: it is not a certificate authority's certificate"), "{}", shown.text);
        let imported = answer(&format!("import uploads/own.pem sha256:{}", fingerprint.hex()), &env).await;
        assert!(!imported.refused, "{}", imported.text);
        let roots = scratch.store.current(Vec::new);
        assert!(roots.trusts(&fingerprint));

        let ask = |addr: std::net::SocketAddr, certs: Vec<reqwest::Certificate>| async move {
            let mut builder = reqwest::Client::builder().timeout(std::time::Duration::from_secs(10));
            for cert in certs {
                builder = builder.add_root_certificate(cert);
            }
            builder.build().unwrap().get(format!("https://{addr}/")).send().await
        };
        let itself = tls_server(own.der().clone(), key.serialize_der(), 1, http_200("", b"ok")).await;
        assert_eq!(ask(itself, roots.certs.clone()).await.unwrap().status(), 200, "the server that presents it is trusted");

        // A certificate an authority issued, imported by itself: no anchor.
        let issued = mint("Some CA");
        let leaf_alone = cert::as_root(&issued.leaf).expect("the store takes it");
        let behind_ca = tls_server(issued.leaf.clone(), issued.leaf_key.clone(), 1, http_200("", b"ok")).await;
        let refused = ask(behind_ca, vec![leaf_alone]).await.unwrap_err();
        assert!(crate::net::error_chain(&refused).contains("UnknownIssuer"), "{}", crate::net::error_chain(&refused));
    }

    /// The whole of it, in one process: a gateway behind an authority
    /// nobody trusts is refused for its issuer; its authority's certificate
    /// is fetched from a listener whose own certificate nobody trusts
    /// either, shown, and imported by its fingerprint; the gateway then
    /// identifies itself, to a client built per call and to the long-lived
    /// one alike, while a client built before the import still refuses.
    /// The one test on the process's roots, which it puts back.
    #[tokio::test]
    async fn a_gateway_refused_for_its_issuer_identifies_itself_after_its_ca_is_imported() {
        let scratch = Scratch::new("e2e");
        let env = CaEnv { store: crate::net::process_roots(), ..scratch.env() };
        let gateway_ca = mint("WSDS Local AI Gateway CA");
        let fingerprint = Fingerprint::of(&gateway_ca.ca_der);
        let gateway = tls_server(
            gateway_ca.leaf.clone(),
            gateway_ca.leaf_key.clone(),
            6,
            http_200("x-local-ai-gateway: 0.9.0\r\n", b"{}"),
        )
        .await;
        let gateway_url = format!("https://{gateway}");

        let refused = crate::provider::gateway::identify(&gateway_url).await.unwrap_err();
        assert!(refused.contains("UnknownIssuer"), "{refused}");
        let before = crate::net::shared();

        // The authority's certificate, served by a listener behind another
        // authority: the fetch verifies nothing.
        let elsewhere = mint("Somebody Else");
        let source = tls_server(elsewhere.leaf.clone(), elsewhere.leaf_key.clone(), 2, http_200("", gateway_ca.ca_pem.as_bytes())).await;
        let url = format!("https://{source}/tls/ca.pem?token=not-in-the-reply");

        let shown = answer(&format!("import {url}"), &env).await;
        assert!(!shown.refused, "{}", shown.text);
        assert!(
            shown.text.starts_with(&format!(
                "Read from {source} over https, without verifying its certificate (the path and any query went out unverified): 1 certificate. Nothing is written yet."
            )),
            "{}",
            shown.text
        );
        assert!(shown.text.contains(&format!("  SHA-256  {}", fingerprint.shown())), "{}", shown.text);
        assert!(scratch.state_files().is_empty());
        assert!(crate::provider::gateway::identify(&gateway_url).await.is_err(), "showing trusts nothing");

        let imported = answer(&format!("import {url} sha256:{}", fingerprint.shown()), &env).await;
        assert!(!imported.refused, "{}", imported.text);
        assert!(!imported.text.contains("not-in-the-reply"), "the reply names the host alone: {}", imported.text);

        assert_eq!(crate::provider::gateway::identify(&gateway_url).await.as_deref(), Ok("0.9.0"), "a client built per call");
        let after = crate::net::shared();
        assert_eq!(after.get(format!("{gateway_url}/v1/models")).send().await.unwrap().status(), 200, "the long-lived client");
        assert!(
            before.get(format!("{gateway_url}/v1/models")).send().await.is_err(),
            "a client kept from before the import still refuses: why a provider keeps none"
        );

        let removed = answer(&format!("remove gateway-ca-{}.pem", &fingerprint.hex()[..8]), &env).await;
        assert!(removed.refused, "the file is named by its subject: {}", removed.text);
        let name = format!("wsds-local-ai-gateway-ca-{}.pem", &fingerprint.hex()[..8]);
        let removed = answer(&format!("remove {name}"), &env).await;
        assert!(!removed.refused, "{}", removed.text);
        assert!(crate::provider::gateway::identify(&gateway_url).await.is_err(), "removed, and no root after the reload");

        // The process's roots as every other test expects them.
        crate::net::process_roots().reload(&crate::net::dirs_in_force());
    }
}
