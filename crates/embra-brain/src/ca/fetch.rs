//! Where the `/ca` command reads a certificate from: a URL, or a file in
//! the workspace.
//!
//! A URL is fetched by the one client of the brain that verifies no
//! certificate (`net::unverified`): the certificate authority that would
//! verify the host is the thing being fetched. Nothing read here is trusted
//! for coming from here. The command shows what it read with its SHA-256,
//! and imports only the certificate whose SHA-256 the operator gives.

use std::path::Path;
use std::time::Duration;

use super::cert::MAX_SOURCE_BYTES;

/// The whole fetch: connect, answer and body.
const FETCH_TIMEOUT: Duration = Duration::from_secs(15);

/// What a `/ca import` names.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Source {
    Url(reqwest::Url),
    /// A path in the workspace, as typed.
    File(String),
}

impl Source {
    /// `http://` or `https://` is a URL, anything else a workspace path. A
    /// URL with a user name or a password is refused: the connection is
    /// not verified, so they would go to whoever answers.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let scheme = text.get(..8).unwrap_or(text).to_ascii_lowercase();
        if !scheme.starts_with("http://") && !scheme.starts_with("https://") {
            return Ok(Self::File(text.to_string()));
        }
        let url = reqwest::Url::parse(text).map_err(|e| format!("{text} is not a URL ({e})."))?;
        if !url.username().is_empty() || url.password().is_some() {
            return Err(
                "Credentials in the URL are refused: the connection is not verified, and they would go to whoever answers."
                    .to_string(),
            );
        }
        if url.host_str().is_none() {
            return Err(format!("{text} names no host."));
        }
        Ok(Self::Url(url))
    }
}

/// How a source was read: what the reply says of it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum How {
    /// Over https, the server's certificate not verified.
    Unverified,
    /// Over plain http.
    Plain,
    /// A file in the workspace.
    Workspace,
}

/// The bytes of a source, and where they came from as the reply and the
/// log name it: the host and the port of a URL (never its path or query,
/// which may carry a token), the path of a file.
#[derive(Debug)]
pub(crate) struct Read {
    pub(crate) from: String,
    pub(crate) how: How,
    pub(crate) bytes: Vec<u8>,
}

/// Reads a source. An error is a sentence for the operator.
pub(crate) async fn read(source: &Source, workspace: &Path) -> Result<Read, String> {
    match source {
        Source::Url(url) => fetch(url).await,
        Source::File(path) => file(path, workspace).await,
    }
}

/// The host and the port of a URL.
fn host_of(url: &reqwest::Url) -> String {
    let host = url.host_str().unwrap_or("the host");
    match url.port() {
        Some(port) => format!("{host}:{port}"),
        None => host.to_string(),
    }
}

async fn fetch(url: &reqwest::Url) -> Result<Read, String> {
    let from = host_of(url);
    let how = if url.scheme() == "https" { How::Unverified } else { How::Plain };
    let client = crate::net::unverified(FETCH_TIMEOUT);
    let mut response = client
        .get(url.clone())
        .send()
        .await
        .map_err(|e| format!("{from} did not answer: {}.", crate::net::error_chain(&e)))?;
    let status = response.status();
    if status.is_redirection() {
        return Err(format!(
            "{from} answered with a redirect (HTTP {}), which is not followed.",
            status.as_u16()
        ));
    }
    if status != reqwest::StatusCode::OK {
        return Err(format!("{from} answered HTTP {}.", status.as_u16()));
    }
    let mut bytes = Vec::new();
    loop {
        let chunk = response
            .chunk()
            .await
            .map_err(|e| format!("The answer of {from} broke off: {}.", crate::net::error_chain(&e)))?;
        let Some(chunk) = chunk else { break };
        if bytes.len() + chunk.len() > MAX_SOURCE_BYTES {
            return Err(format!(
                "{from} answered more than {MAX_SOURCE_BYTES} bytes; a certificate file holds at most that."
            ));
        }
        bytes.extend_from_slice(&chunk);
    }
    Ok(Read { from, how, bytes })
}

/// A file in the workspace, through the resolver the downloads use: inside
/// the workspace as written and after every link is followed, a regular
/// file, under the cap. Nothing of a refused file's content is said.
async fn file(path: &str, workspace: &Path) -> Result<Read, String> {
    use crate::media::offer::{resolve_offer_in, OfferError};
    let offered = resolve_offer_in(workspace, path, MAX_SOURCE_BYTES as u64).await.map_err(|e| match e {
        OfferError::NotFound(shown) => {
            format!("{shown}: no such file in the workspace (an upload lands in uploads/).")
        }
        OfferError::TooLarge(bytes, cap) => {
            format!("{path} holds {bytes} bytes; a certificate file holds at most {cap}.")
        }
        OfferError::Denied(why) => format!("{path} is refused: {why}."),
        OfferError::NotAFile(shown) => format!("{shown} is not a regular file."),
        OfferError::Io(why) => format!("{path} could not be read: {why}."),
    })?;
    let bytes = tokio::fs::read(&offered.path)
        .await
        .map_err(|e| format!("{} could not be read: {e}.", offered.rel))?;
    Ok(Read { from: offered.rel, how: How::Workspace, bytes })
}

#[cfg(test)]
mod tests {
    use super::*;
    use wiremock::matchers::{method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn a_source_is_a_url_or_a_workspace_path() {
        assert_eq!(Source::parse(" uploads/Local AI Gateway ca.pem ").unwrap(), Source::File("uploads/Local AI Gateway ca.pem".into()));
        assert_eq!(Source::parse("/embra/workspace/uploads/ca.pem").unwrap(), Source::File("/embra/workspace/uploads/ca.pem".into()));
        assert_eq!(Source::parse("ftp://host/ca.pem").unwrap(), Source::File("ftp://host/ca.pem".into()), "no other scheme is a URL");
        for url in ["https://gateway.example:8787/api/tls/ca", "HTTP://10.0.0.5/ca.pem", "https://[fd00::1]:4000/ca.pem?x=1"] {
            assert!(matches!(Source::parse(url), Ok(Source::Url(_))), "{url}");
        }
        assert!(Source::parse("https://").is_err());
    }

    /// The fetch is not verified, so nothing that authenticates the operator
    /// goes into the URL.
    #[test]
    fn credentials_in_the_url_are_refused() {
        for url in ["https://admin:secret@gateway.example/ca.pem", "http://token@gateway.example/ca.pem", "https://:pw@h/ca"] {
            let refusal = Source::parse(url).unwrap_err();
            assert!(refusal.starts_with("Credentials in the URL are refused"), "{url}: {refusal}");
            assert!(!refusal.contains("secret") && !refusal.contains("token"), "the refusal repeats none of it: {refusal}");
        }
    }

    #[test]
    fn the_host_is_named_and_never_the_path_or_the_query() {
        let url = reqwest::Url::parse("https://gateway.example:8787/api/tls/ca?token=abc").unwrap();
        assert_eq!(host_of(&url), "gateway.example:8787");
        assert_eq!(host_of(&reqwest::Url::parse("http://10.0.0.5/ca.pem").unwrap()), "10.0.0.5");
    }

    #[tokio::test]
    async fn a_redirect_is_reported_and_not_followed() {
        let server = MockServer::start().await;
        let elsewhere = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/ca.pem"))
            .respond_with(ResponseTemplate::new(302).insert_header("location", format!("{}/ca.pem", elsewhere.uri())))
            .mount(&server)
            .await;
        let source = Source::parse(&format!("{}/ca.pem", server.uri())).unwrap();
        let refusal = read(&source, Path::new("/nonexistent")).await.unwrap_err();
        assert!(refusal.contains("answered with a redirect (HTTP 302), which is not followed"), "{refusal}");
        assert!(elsewhere.received_requests().await.unwrap().is_empty(), "the other host saw nothing");
    }

    #[tokio::test]
    async fn an_answer_over_the_cap_or_one_that_is_not_200_is_refused() {
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/big"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'A'; MAX_SOURCE_BYTES + 1]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/fits"))
            .respond_with(ResponseTemplate::new(200).set_body_bytes(vec![b'A'; MAX_SOURCE_BYTES]))
            .mount(&server)
            .await;
        Mock::given(method("GET"))
            .and(path("/gone"))
            .respond_with(ResponseTemplate::new(401).set_body_string("token required"))
            .mount(&server)
            .await;
        let at = |p: &str| Source::parse(&format!("{}{p}", server.uri())).unwrap();
        let nowhere = Path::new("/nonexistent");
        let big = read(&at("/big"), nowhere).await.unwrap_err();
        assert!(big.contains(&format!("answered more than {MAX_SOURCE_BYTES} bytes")), "{big}");
        let fits = read(&at("/fits"), nowhere).await.unwrap();
        assert_eq!((fits.bytes.len(), fits.how), (MAX_SOURCE_BYTES, How::Plain));
        assert_eq!(fits.from, server.address().to_string(), "the host and the port");
        let gone = read(&at("/gone"), nowhere).await.unwrap_err();
        assert!(gone.ends_with("answered HTTP 401.") && !gone.contains("token required"), "{gone}");
    }

    /// A workspace file is read through the downloads' resolver: a path that
    /// leaves the workspace, by its text or by a link, is refused, and the
    /// refusal says nothing of what the file holds.
    #[tokio::test]
    async fn a_workspace_link_that_leaves_the_workspace_is_refused_and_nothing_of_the_file_is_shown() {
        let root = std::env::temp_dir().join(format!("brain-ca-fetch-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let workspace = root.join("workspace");
        std::fs::create_dir_all(workspace.join("uploads")).unwrap();
        std::fs::write(root.join("outside.pem"), "TOP SECRET CONTENT").unwrap();
        std::fs::write(workspace.join("uploads/ca.pem"), "inside").unwrap();
        std::os::unix::fs::symlink(root.join("outside.pem"), workspace.join("uploads/link.pem")).unwrap();

        let inside = read(&Source::File("uploads/ca.pem".into()), &workspace).await.unwrap();
        assert_eq!((inside.bytes.as_slice(), inside.how, inside.from.as_str()), (b"inside".as_slice(), How::Workspace, "uploads/ca.pem"));
        let absolute = format!("{}/uploads/ca.pem", workspace.display());
        assert_eq!(read(&Source::File(absolute), &workspace).await.unwrap().bytes, b"inside");

        for outside in ["uploads/link.pem", "../outside.pem", "uploads/../../outside.pem", "/etc/hostname"] {
            let refusal = read(&Source::File(outside.into()), &workspace).await.unwrap_err();
            assert!(refusal.contains("is refused"), "{outside}: {refusal}");
            assert!(!refusal.contains("TOP SECRET"), "{refusal}");
        }
        let missing = read(&Source::File("uploads/none.pem".into()), &workspace).await.unwrap_err();
        assert!(missing.contains("no such file in the workspace"), "{missing}");
        let directory = read(&Source::File("uploads".into()), &workspace).await.unwrap_err();
        assert!(directory.contains("is not a regular file"), "{directory}");
        std::fs::write(workspace.join("uploads/big.pem"), vec![b'A'; MAX_SOURCE_BYTES + 1]).unwrap();
        let big = read(&Source::File("uploads/big.pem".into()), &workspace).await.unwrap_err();
        assert!(big.contains(&format!("holds {} bytes", MAX_SOURCE_BYTES + 1)), "{big}");
        std::fs::remove_dir_all(&root).ok();
    }
}
