//! The one HTTPS client of the updater: blocking, with the compiled-in
//! roots and every certificate of the operator's CA bundle (`SSL_CERT_FILE`,
//! which embrad merges from the STATE drop-ins and exports to every service),
//! reqwest's default redirects (GitHub's release assets are two hops, the
//! second cross-origin), a connect timeout, TCP keepalive and no total
//! timeout (the blocking client has no per-read one; a slot image is many
//! minutes at a slow link). A status other than 200 is a failure that
//! names it, never silence. A model's file is the one body asked for from a
//! byte on (`open_from`): 206 with the range asked for, or 200 with the
//! whole from a server that takes no range.

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

/// What a GET from a byte on came to.
pub enum Opened {
    /// 206: the body starts at the byte asked for and runs to the end of a
    /// file of `total` bytes.
    Rest { body: reqwest::blocking::Response, total: u64 },
    /// 200: the file from its first byte, whatever was asked (a server that
    /// takes no range answers so), with its length when the server said one.
    Whole { body: reqwest::blocking::Response, length: Option<u64> },
}

/// `bytes <start>-<end>/<total>`: the three numbers of a `Content-Range`.
fn content_range(value: &str) -> Option<(u64, u64, u64)> {
    let (range, total) = value.trim().strip_prefix("bytes ")?.split_once('/')?;
    let (start, end) = range.split_once('-')?;
    let (start, end, total): (u64, u64, u64) = (start.trim().parse().ok()?, end.trim().parse().ok()?, total.trim().parse().ok()?);
    (start <= end && end < total).then_some((start, end, total))
}

/// Open a GET of `url` from byte `from` on (`Range: bytes=<from>-`; none for
/// 0), the status checked first. A 206 has to be the rest of the file from
/// that byte: any other range is an error, never bytes written at the wrong
/// place. The error names `url` as it was given and no address a redirect
/// led to (a signed one, on a file host).
pub fn open_from(client: &Client, url: &str, from: u64) -> Result<Opened, String> {
    let mut request = client.get(url);
    if from > 0 {
        request = request.header(reqwest::header::RANGE, format!("bytes={from}-"));
    }
    let resp = request.send().map_err(|e| format!("GET {url}: {}", error_chain(&e.without_url())))?;
    let status = resp.status();
    if status == reqwest::StatusCode::OK {
        let length = resp.content_length();
        return Ok(Opened::Whole { body: resp, length });
    }
    if status == reqwest::StatusCode::PARTIAL_CONTENT {
        let header = resp.headers().get(reqwest::header::CONTENT_RANGE).and_then(|v| v.to_str().ok()).unwrap_or("").to_string();
        return match content_range(&header) {
            Some((start, end, total)) if start == from && end + 1 == total => Ok(Opened::Rest { body: resp, total }),
            _ => Err(format!("GET {url} from byte {from} → HTTP 206 with the range {header:?}, which is not the rest of the file")),
        };
    }
    Err(format!("GET {url} → HTTP {}: {}", status.as_u16(), status.canonical_reason().unwrap_or("")))
}

/// A scripted loopback server for the tests of this crate.
#[cfg(test)]
pub mod test_support {
    use std::io::{Read, Write};
    use std::sync::mpsc;

    /// One answer: a status line, headers, a body, and how much of the body
    /// is sent before the connection closes.
    pub struct Answer {
        status: &'static str,
        headers: Vec<String>,
        body: Vec<u8>,
        declared: Option<usize>,
        sent: usize,
    }

    impl Answer {
        pub fn new(status: &'static str, body: &[u8]) -> Self {
            Answer { status, headers: Vec::new(), body: body.to_vec(), declared: Some(body.len()), sent: body.len() }
        }

        pub fn header(mut self, line: String) -> Self {
            self.headers.push(line);
            self
        }

        /// The connection closes after this many bytes of the body.
        pub fn cut_after(mut self, sent: usize) -> Self {
            self.sent = sent.min(self.body.len());
            self
        }

        /// No Content-Length: the body is what comes before the connection
        /// closes.
        pub fn without_length(mut self) -> Self {
            self.declared = None;
            self
        }

        /// A Content-Length other than the body's.
        pub fn declaring(mut self, length: usize) -> Self {
            self.declared = Some(length);
            self
        }
    }

    pub type Step = Box<dyn FnOnce(&str) -> Answer + Send>;

    /// One step of a script: the answer to a request, from its head.
    pub fn step(answer: impl FnOnce(&str) -> Answer + Send + 'static) -> Step {
        Box::new(answer)
    }

    /// The byte a request's `Range: bytes=<n>-` asks from.
    pub fn range_start(head: &str) -> Option<u64> {
        head.lines().find_map(|line| {
            let line = line.to_ascii_lowercase();
            line.strip_prefix("range: bytes=").and_then(|range| range.trim_end_matches('-').parse().ok())
        })
    }

    /// Answer one connection per entry of `script`, each from the request's
    /// head, then listen no more. Returns the URL, and the heads as they
    /// came (a head is reported before its answer is written).
    pub fn serve(script: Vec<Step>) -> (String, mpsc::Receiver<String>) {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let (seen, heads) = mpsc::channel();
        std::thread::spawn(move || {
            for answer in script {
                let Ok((mut stream, _)) = listener.accept() else { return };
                let mut head = Vec::new();
                let mut buf = [0u8; 1024];
                while !head.windows(4).any(|w| w == b"\r\n\r\n") {
                    match stream.read(&mut buf) {
                        Ok(0) | Err(_) => break,
                        Ok(n) => head.extend_from_slice(&buf[..n]),
                    }
                }
                let head = String::from_utf8_lossy(&head).into_owned();
                let answer = answer(&head);
                let _ = seen.send(head);
                let mut out = format!("{}\r\n", answer.status);
                for header in &answer.headers {
                    out.push_str(header);
                    out.push_str("\r\n");
                }
                if let Some(length) = answer.declared {
                    out.push_str(&format!("content-length: {length}\r\n"));
                }
                out.push_str("connection: close\r\n\r\n");
                let _ = stream.write_all(out.as_bytes());
                let _ = stream.write_all(&answer.body[..answer.sent]);
                let _ = stream.flush();
            }
        });
        (format!("http://{addr}/model.gguf"), heads)
    }
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

    #[test]
    fn a_content_range_is_three_numbers_that_fit() {
        assert_eq!(content_range("bytes 5-9/10"), Some((5, 9, 10)));
        assert_eq!(content_range(" bytes 0-0/1 "), Some((0, 0, 1)));
        for not_one in ["", "bytes", "5-9/10", "bytes 5-9", "bytes 9-5/10", "bytes 5-10/10", "bytes 5-9/*", "bytes */10", "bytes -5-9/10", "items 5-9/10"] {
            assert_eq!(content_range(not_one), None, "{not_one:?}");
        }
    }

    #[test]
    fn a_get_from_a_byte_on_is_the_rest_of_the_file_or_the_whole_or_an_error() {
        use test_support::{range_start, serve, step, Answer};
        let client = client().unwrap();
        let part = || Answer::new("HTTP/1.1 206 Partial Content", b"56789");
        let (url, heads) = serve(vec![
            step(move |_| part().header("content-range: bytes 5-9/10".into())),
            step(|_| Answer::new("HTTP/1.1 200 OK", b"0123456789")),
            step(|_| Answer::new("HTTP/1.1 200 OK", b"0123456789").without_length()),
            step(move |_| part().header("content-range: bytes 4-8/10".into())),
            step(move |_| part().header("content-range: bytes 5-8/10".into())),
            step(move |_| part()),
            step(|_| Answer::new("HTTP/1.1 416 Range Not Satisfiable", b"")),
        ]);
        let Ok(Opened::Rest { mut body, total }) = open_from(&client, &url, 5) else {
            panic!("a 206 with the range asked for is the rest of the file");
        };
        assert_eq!(total, 10);
        let mut rest = String::new();
        body.read_to_string(&mut rest).unwrap();
        assert_eq!(rest, "56789");
        // A server that takes no range answers 200 with the whole, to a ranged request too.
        assert!(matches!(open_from(&client, &url, 5), Ok(Opened::Whole { length: Some(10), .. })));
        assert!(matches!(open_from(&client, &url, 0), Ok(Opened::Whole { length: None, .. })));
        for (from, part) in [(5, "bytes 4-8/10"), (5, "bytes 5-8/10"), (5, "the range \"\""), (5, "HTTP 416")] {
            let Err(reason) = open_from(&client, &url, from) else { panic!("{part}: taken") };
            assert!(reason.contains(part) && reason.contains(&url), "{reason}");
        }
        let heads: Vec<String> = heads.try_iter().collect();
        let asked: Vec<Option<u64>> = heads.iter().map(|h| range_start(h)).collect();
        assert_eq!(asked, [Some(5), Some(5), None, Some(5), Some(5), Some(5), Some(5)]);
        assert!(heads[0].to_ascii_lowercase().contains("user-agent: embraos/"), "{}", heads[0]);
    }
}
