//! Where the manifests come from: `--source`, else the operator's STATE
//! drop-in, else the GitHub releases of the public mirror. The rootfs is
//! fetched from the same base under the name the manifest gives, never from
//! a URL inside the manifest, so a mirror behind the operator's CA serves
//! the same signed manifest.

use std::net::Ipv4Addr;
use std::path::Path;

pub const DEFAULT_SOURCE: &str = "https://github.com/Ward-Software-Defined-Systems/embraOS/releases/latest/download";
pub const STATE_SOURCE_FILE: &str = "/embra/state/update-source";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    pub base: String,
}

/// `https://` anywhere; `http://` to a private address or localhost only
/// (a mirror on the operator's own network).
pub fn allowed(url: &str) -> Result<(), String> {
    if let Some(rest) = url.strip_prefix("https://") {
        return if rest.is_empty() { Err("an empty https URL".into()) } else { Ok(()) };
    }
    if let Some(rest) = url.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or("");
        let private = host == "localhost"
            || host.parse::<Ipv4Addr>().map(|ip| ip.is_private() || ip.is_loopback()).unwrap_or(false);
        return if private { Ok(()) } else { Err(format!("http:// is for a private address only, not {host}; use https://")) };
    }
    Err(format!("not a URL this updater fetches from: {url} (https://, or http:// to a private address)"))
}

/// The base, its trailing slash dropped; a `--tag` rewrites a GitHub
/// `releases/latest/download` base to that release's own.
pub fn resolve(flag: Option<&str>, tag: Option<&str>, state_file: &Path) -> Result<Source, String> {
    let mut base = match flag {
        Some(f) => f.trim().to_string(),
        None => match std::fs::read_to_string(state_file) {
            Ok(t) if !t.trim().is_empty() => t.trim().to_string(),
            _ => DEFAULT_SOURCE.to_string(),
        },
    };
    while base.ends_with('/') {
        base.pop();
    }
    allowed(&base)?;
    if let Some(t) = tag {
        if !t.starts_with('v') || t.contains('/') || t.contains("..") {
            return Err(format!("not a release tag: {t}"));
        }
        if let Some(repo) = base.strip_suffix("/releases/latest/download") {
            base = format!("{repo}/releases/download/{t}");
        }
    }
    Ok(Source { base })
}

impl Source {
    pub fn manifest_url(&self, arch: &str, engine: &str) -> String {
        format!("{}/embraos-{arch}-{engine}.manifest.json", self.base)
    }

    pub fn signature_url(&self, arch: &str, engine: &str) -> String {
        format!("{}.sig", self.manifest_url(arch, engine))
    }

    pub fn rootfs_url(&self, file: &str) -> String {
        format!("{}/{file}", self.base)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_state_drop_in_overrides_the_default_and_a_tag_rewrites_the_github_base() {
        let none = Path::new("/nonexistent/update-source");
        let s = resolve(None, None, none).unwrap();
        assert_eq!(s.base, DEFAULT_SOURCE);
        assert_eq!(s.manifest_url("x86_64", "fjall"), format!("{DEFAULT_SOURCE}/embraos-x86_64-fjall.manifest.json"));
        assert_eq!(s.signature_url("x86_64", "fjall"), format!("{DEFAULT_SOURCE}/embraos-x86_64-fjall.manifest.json.sig"));
        let s = resolve(None, Some("v0.24.0-phase2"), none).unwrap();
        assert_eq!(s.base, "https://github.com/Ward-Software-Defined-Systems/embraOS/releases/download/v0.24.0-phase2");
        assert_eq!(s.rootfs_url("f.squashfs"), format!("{}/f.squashfs", s.base));
        let file = std::env::temp_dir().join(format!("embra-update-source-{}", std::process::id()));
        std::fs::write(&file, "https://gitlab.ops.wsds/api/v4/projects/2/packages/generic/embraos/latest/\n").unwrap();
        let s = resolve(None, None, &file).unwrap();
        assert_eq!(s.base, "https://gitlab.ops.wsds/api/v4/projects/2/packages/generic/embraos/latest");
        let s = resolve(None, Some("v1.0.0"), &file).unwrap();
        assert_eq!(s.base, "https://gitlab.ops.wsds/api/v4/projects/2/packages/generic/embraos/latest", "a tag rewrites GitHub's base only");
        assert_eq!(resolve(Some("https://mirror.example/x/"), None, &file).unwrap().base, "https://mirror.example/x");
        assert!(resolve(None, Some("../x"), none).is_err());
        std::fs::remove_file(&file).unwrap();
    }

    #[test]
    fn an_http_source_is_accepted_for_a_private_host_only() {
        assert!(allowed("http://10.0.0.5:8080/updates").is_ok());
        assert!(allowed("http://192.168.1.9/u").is_ok());
        assert!(allowed("http://localhost:9000").is_ok());
        assert!(allowed("http://example.com/u").is_err());
        assert!(allowed("http://8.8.8.8/u").is_err());
        assert!(allowed("ftp://x").is_err());
        assert!(allowed("https://").is_err());
        assert!(allowed("https://example.com").is_ok());
    }
}
