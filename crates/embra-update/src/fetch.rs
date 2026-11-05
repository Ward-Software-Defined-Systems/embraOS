//! The one HTTPS client of the updater: blocking, with the compiled-in
//! roots and every certificate of the operator's CA bundle (`SSL_CERT_FILE`,
//! which embrad merges from the STATE drop-ins and exports to every service),
//! reqwest's default redirects (GitHub's release assets are two hops, the
//! second cross-origin), a connect timeout, TCP keepalive and no total
//! timeout (the blocking client has no per-read one; a slot image is many
//! minutes at a slow link). A status other than 200 is a failure that
//! names it, never silence.

use std::io::Read;
use std::time::Duration;

use reqwest::blocking::Client;

const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);
const KEEPALIVE: Duration = Duration::from_secs(30);
/// The manifest and its signature are small; a body past this is refused.
pub const SMALL_MAX: u64 = 64 * 1024;

pub fn client() -> Result<Client, String> {
    let mut builder = Client::builder()
        .user_agent(embra_tools_core::USER_AGENT)
        .connect_timeout(CONNECT_TIMEOUT)
        .tcp_keepalive(KEEPALIVE)
        .timeout(None);
    if let Ok(path) = std::env::var("SSL_CERT_FILE") {
        match std::fs::read(&path) {
            Ok(pem) => match reqwest::Certificate::from_pem_bundle(&pem) {
                Ok(certs) => {
                    for c in certs {
                        builder = builder.add_root_certificate(c);
                    }
                }
                Err(e) => eprintln!("warning: the CA bundle {path} does not parse ({e}); the compiled-in roots alone are trusted"),
            },
            Err(e) => eprintln!("warning: the CA bundle {path} could not be read ({e}); the compiled-in roots alone are trusted"),
        }
    }
    builder.build().map_err(|e| format!("the HTTPS client could not be built: {}", error_chain(&e)))
}

/// The error and its causes, joined.
pub fn error_chain(e: &dyn std::error::Error) -> String {
    let mut parts = vec![e.to_string()];
    let mut cur = e.source();
    while let Some(s) = cur {
        let text = s.to_string();
        if parts.last() != Some(&text) {
            parts.push(text);
        }
        cur = s.source();
    }
    parts.join(": ")
}

/// A small body whole, under `max` bytes.
pub fn get_small(client: &Client, url: &str, max: u64) -> Result<Vec<u8>, String> {
    let resp = client.get(url).send().map_err(|e| format!("GET {url}: {}", error_chain(&e)))?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        return Err(format!("GET {url} → HTTP {}: {}", status.as_u16(), status.canonical_reason().unwrap_or("")));
    }
    let mut body = Vec::new();
    let mut taken = resp.take(max + 1);
    taken.read_to_end(&mut body).map_err(|e| format!("GET {url}: reading the body: {e}"))?;
    if body.len() as u64 > max {
        return Err(format!("GET {url}: the body is over {max} bytes"));
    }
    Ok(body)
}

/// Open a GET whose body the caller streams; the status checked first.
pub fn open(client: &Client, url: &str) -> Result<reqwest::blocking::Response, String> {
    let resp = client.get(url).send().map_err(|e| format!("GET {url}: {}", error_chain(&e)))?;
    let status = resp.status();
    if status != reqwest::StatusCode::OK {
        return Err(format!("GET {url} → HTTP {}: {}", status.as_u16(), status.canonical_reason().unwrap_or("")));
    }
    Ok(resp)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    /// One response from a loopback listener, then the status and the body.
    fn serve_once(status_line: &str, body: &[u8]) -> String {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let status_line = status_line.to_string();
        let body = body.to_vec();
        std::thread::spawn(move || {
            if let Ok((mut s, _)) = listener.accept() {
                let mut req = [0u8; 4096];
                let _ = s.read(&mut req);
                let _ = s.write_all(format!("{status_line}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n", body.len()).as_bytes());
                let _ = s.write_all(&body);
            }
        });
        format!("http://{addr}/embraos-x86_64-fjall.manifest.json")
    }

    #[test]
    fn a_status_other_than_200_is_the_failure_and_never_an_empty_body() {
        let client = client().unwrap();
        let url = serve_once("HTTP/1.1 404 Not Found", b"nope");
        let err = get_small(&client, &url, SMALL_MAX).unwrap_err();
        assert!(err.contains("HTTP 404"), "{err}");
        let url = serve_once("HTTP/1.1 200 OK", b"{\"ok\":true}");
        assert_eq!(get_small(&client, &url, SMALL_MAX).unwrap(), b"{\"ok\":true}");
        let url = serve_once("HTTP/1.1 200 OK", &[b'x'; 100]);
        assert!(get_small(&client, &url, 50).unwrap_err().contains("over 50 bytes"));
    }
}
