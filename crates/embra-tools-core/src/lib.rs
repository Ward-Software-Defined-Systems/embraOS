//! Shared types for the embra tool registry.
//!
//! This crate deliberately avoids any dependency on `embra-brain` so that
//! the proc-macro's test fixtures and downstream adapters (future local /
//! QNM Brain implementations) can reference these types without pulling
//! in the full brain surface. `DispatchContext` and `ToolDescriptor` live
//! in `embra-brain` because they reference `WardsonDbClient`.

pub use serde_json::Value as JsonValue;

use serde::{Deserialize, Serialize};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};

pub type BoxFut<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

#[derive(Debug, thiserror::Error)]
pub enum DispatchError {
    #[error("unknown tool: {0}")]
    Unknown(String),
    #[error("input deserialization failed for tool {tool}: {source}")]
    BadInput {
        tool: String,
        #[source]
        source: serde_json::Error,
    },
    #[error("{0}")]
    Handler(String),
    #[error("tool '{tool}' exceeded {limit_secs}s global execution timeout")]
    Timeout { tool: String, limit_secs: u64 },
}

/// Structured result of one tool dispatch.
///
/// Every tool's inherent `run()` may return either `Result<String, _>` (the
/// 103 pre-media tools — the `#[embra_tool]` macro maps it through
/// `From<String>`) or `Result<ToolOutput, _>` when it has images to hand the
/// model (`image_view`, `image_generate`). `text` is what the dispatcher
/// size-caps (`apply_max_size`); `images` deliberately bypass that cap —
/// byte-truncating a base64 payload would corrupt it silently — and are
/// bounded by count (`MAX_TOOL_RESULT_IMAGES`) in the dispatcher instead.
#[derive(Debug, Clone, Default)]
pub struct ToolOutput {
    pub text: String,
    pub images: Vec<ToolImage>,
    /// Display-only references: images the tool put in the MEDIA store
    /// that the OPERATOR should see (the loop emits a `MediaRef` frame per
    /// entry) but that are NOT handed to the model (e.g. `image_generate`
    /// with `return_image=false`). Images in `images` with a `media_ref`
    /// are announced the same way — don't list them here too.
    pub media_refs: Vec<MediaRefMeta>,
    /// Display-only references to workspace files offered to the operator
    /// for download (`file_offer`): the loop emits a `FileRef` frame per
    /// entry and persists it on the assistant turn. Nothing of it reaches
    /// the model beyond `text`.
    pub files: Vec<FileRefMeta>,
}

impl ToolOutput {
    pub fn text(text: impl Into<String>) -> Self {
        ToolOutput {
            text: text.into(),
            images: Vec::new(),
            media_refs: Vec::new(),
            files: Vec::new(),
        }
    }

    pub fn with_file(mut self, file: FileRefMeta) -> Self {
        self.files.push(file);
        self
    }

    pub fn with_image(mut self, image: ToolImage) -> Self {
        self.images.push(image);
        self
    }

    pub fn with_media_ref(mut self, media_ref: MediaRefMeta) -> Self {
        self.media_refs.push(media_ref);
        self
    }
}

impl From<String> for ToolOutput {
    fn from(text: String) -> Self {
        ToolOutput::text(text)
    }
}

/// One image a tool returns alongside its text. `data` is the raw encoded
/// file (PNG/JPEG/GIF/WebP — already normalized by the media ingest path),
/// never base64: providers encode at wire time. `media_ref` is set when the
/// bytes also live in the MEDIA store, so the tool loop can emit the
/// operator-facing `MediaRef` frame (tools have no stream handle).
#[derive(Clone)]
pub struct ToolImage {
    pub media_type: String,
    pub data: Vec<u8>,
    pub width: u32,
    pub height: u32,
    pub name: String,
    pub media_ref: Option<MediaRefMeta>,
}

impl std::fmt::Debug for ToolImage {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ToolImage")
            .field("media_type", &self.media_type)
            .field("bytes", &self.data.len())
            .field("width", &self.width)
            .field("height", &self.height)
            .field("name", &self.name)
            .field("media_ref", &self.media_ref)
            .finish()
    }
}

/// Display metadata for an image in the MEDIA store — the wire-neutral
/// twin of the proto `MediaRef` (embra-brain converts). `origin` is
/// `attached` | `generated` | `viewed`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MediaRefMeta {
    pub id: String,
    pub media_type: String,
    pub width: u32,
    pub height: u32,
    pub byte_size: u64,
    pub name: String,
    pub origin: String,
    pub path: String,
    #[serde(default)]
    pub caption: String,
}

/// Display metadata for a workspace file offered for download — the
/// wire-neutral twin of the proto `FileRef` (embra-brain converts).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRefMeta {
    pub path: String,
    pub name: String,
    pub byte_size: u64,
    pub media_type: String,
    #[serde(default)]
    pub caption: String,
}

/// One tool invocation recorded in the current turn's trace.
///
/// `input_preview` and `result_preview` are bounded (≤2 KiB, byte-capped
/// char-boundary-safe, in the embra-brain populator) so the trace stays
/// small even for chatty tools.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceEntry {
    pub tool_name: String,
    pub tool_use_id: String,
    pub input_preview: String,
    /// RFC3339 UTC timestamp when dispatch started.
    pub started_at: String,
    pub elapsed_ms: u64,
    pub is_error: bool,
    pub result_preview: String,
}

/// In-memory trace of tool calls made within one user turn.
pub type TurnTrace = VecDeque<TraceEntry>;

/// Shared handle to a [`TurnTrace`]. Interior mutability via `Arc<Mutex>`
/// avoids propagating `&mut` through the `fn` handler signature on
/// `ToolDescriptor::handler`.
pub type TurnTraceHandle = Arc<Mutex<TurnTrace>>;

/// Construct an empty trace handle with a reasonable default capacity.
pub fn new_turn_trace_handle() -> TurnTraceHandle {
    Arc::new(Mutex::new(VecDeque::with_capacity(32)))
}

/// The `User-Agent` every HTTP request a tool sends carries, in the
/// `product/version` form: the Guardian
/// broker's clients (`http_get`, `http_request`, Brave `web_search`) and the
/// brain's GitLab and GitHub clients. The version is the workspace's
/// (`[workspace.package] version`, which every crate inherits), so the
/// string moves with a release and nowhere else. An `http_request` input
/// may name its own `user-agent` header; that value replaces this one for
/// that request. Guard `the_user_agent_names_embraos_and_the_workspace_version`.
pub const USER_AGENT: &str = concat!("embraOS/", env!("CARGO_PKG_VERSION"));

#[cfg(test)]
mod user_agent_tests {
    use super::USER_AGENT;

    /// The version under `[workspace.package]` in the workspace manifest.
    fn workspace_version() -> String {
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(|p| p.parent())
            .expect("workspace root");
        let text = std::fs::read_to_string(root.join("Cargo.toml")).expect("the workspace manifest");
        let mut in_package = false;
        for line in text.lines() {
            let line = line.trim();
            if line.starts_with('[') {
                in_package = line == "[workspace.package]";
                continue;
            }
            if in_package
                && let Some(rest) = line.strip_prefix("version")
                && let Some(value) = rest.trim_start().strip_prefix('=')
            {
                return value.trim().trim_matches('"').to_string();
            }
        }
        panic!("no [workspace.package] version in the workspace manifest");
    }

    #[test]
    fn the_user_agent_names_embraos_and_the_workspace_version() {
        assert_eq!(USER_AGENT, format!("embraOS/{}", workspace_version()));
        // A header value is printable ASCII without a space: a version that
        // broke this would fail both Guardian clients at construction.
        assert!(USER_AGENT.bytes().all(|b| b.is_ascii_graphic()), "{USER_AGENT:?}");
    }
}

#[cfg(test)]
mod output_tests {
    use super::*;

    #[test]
    fn a_text_only_output_carries_no_files_and_with_file_adds_one() {
        let out = ToolOutput::text("done");
        assert!(out.files.is_empty());
        assert!(out.images.is_empty());
        assert!(out.media_refs.is_empty());
        let out = out.with_file(FileRefMeta {
            path: "/embra/workspace/report.md".into(),
            name: "report.md".into(),
            byte_size: 10,
            media_type: "text/markdown".into(),
            caption: "the report".into(),
        });
        assert_eq!(out.files.len(), 1);
        assert_eq!(out.files[0].name, "report.md");
        let from_string: ToolOutput = String::from("x").into();
        assert!(from_string.files.is_empty());
    }
}

// ── Stored secrets, matched by value ───────────────────────────────────────

/// The operator's stored secrets, held so a text that is about to leave the
/// OS can be checked for one. Each secret is kept in the forms it could take
/// on the wire (as stored, percent-encoded, base64 in both alphabets), and a
/// match answers the KIND of secret, never its value. The brain fills one
/// from its configuration and STATE before an outbound dispatch; the
/// Guardian broker checks every request against it before the transport.
#[derive(Default)]
pub struct SecretSet {
    entries: Vec<SecretEntry>,
}

struct SecretEntry {
    kind: &'static str,
    forms: Vec<String>,
}

/// A value shorter than this is not treated as a secret: it would match
/// ordinary text.
pub const SECRET_MIN_BYTES: usize = 8;

impl SecretSet {
    pub fn new() -> Self {
        Self::default()
    }

    /// Add one secret under its kind (a short phrase such as "the GitHub
    /// token", which the refusal names). A value under `SECRET_MIN_BYTES`
    /// is ignored. A header value of the form `Bearer <token>` adds the bare
    /// token as well.
    pub fn add(&mut self, kind: &'static str, value: &str) {
        let value = value.trim();
        if value.len() < SECRET_MIN_BYTES {
            return;
        }
        let mut forms: Vec<String> = Vec::new();
        for v in std::iter::once(value).chain(bare_bearer_token(value)) {
            for form in [
                v.to_string(),
                percent_encode(v),
                base64_standard_unpadded(v.as_bytes()),
                base64_url_unpadded(v.as_bytes()),
            ] {
                if form.len() >= SECRET_MIN_BYTES && !forms.contains(&form) {
                    forms.push(form);
                }
            }
        }
        self.entries.push(SecretEntry { kind, forms });
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// The kind of the first stored secret that appears in `text` in any of
    /// its forms. The value itself is never returned.
    pub fn found_in(&self, text: &str) -> Option<&'static str> {
        self.entries
            .iter()
            .find(|e| e.forms.iter().any(|f| text.contains(f.as_str())))
            .map(|e| e.kind)
    }

    /// `text` with every occurrence of every stored secret, in any of its
    /// forms, replaced by `[<kind> redacted]`; `None` when none appears, so
    /// a caller keeps its text as it is.
    pub fn redact(&self, text: &str) -> Option<String> {
        let mut out: Option<String> = None;
        for e in &self.entries {
            for form in &e.forms {
                let current = out.as_deref().unwrap_or(text);
                if current.contains(form.as_str()) {
                    out = Some(current.replace(form.as_str(), &format!("[{} redacted]", e.kind)));
                }
            }
        }
        out
    }
}

fn bare_bearer_token(value: &str) -> Option<&str> {
    let (scheme, token) = value.split_once(char::is_whitespace)?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then(|| token.trim())
        .filter(|t| !t.is_empty())
}

/// RFC 3986 percent-encoding of every byte but the unreserved characters.
pub fn percent_encode(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// Standard base64 (RFC 4648 §4) without padding: the padded form has it
/// as a prefix, so the one form matches both.
pub fn base64_standard_unpadded(bytes: &[u8]) -> String {
    base64_with(
        bytes,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/",
    )
}

/// URL-safe base64 (RFC 4648 §5) without padding.
pub fn base64_url_unpadded(bytes: &[u8]) -> String {
    base64_with(
        bytes,
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789-_",
    )
}

fn base64_with(bytes: &[u8], alphabet: &[u8; 64]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = (u32::from(chunk[0]) << 16)
            | (u32::from(chunk.get(1).copied().unwrap_or(0)) << 8)
            | u32::from(chunk.get(2).copied().unwrap_or(0));
        out.push(alphabet[(n >> 18) as usize & 63] as char);
        out.push(alphabet[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(alphabet[(n >> 6) as usize & 63] as char);
        }
        if chunk.len() > 2 {
            out.push(alphabet[n as usize & 63] as char);
        }
    }
    out
}

#[cfg(test)]
mod secret_set_tests {
    use super::*;

    #[test]
    fn the_encoders_match_the_rfc_vectors() {
        // RFC 4648 §10.
        for (plain, b64) in [
            ("", ""),
            ("f", "Zg"),
            ("fo", "Zm8"),
            ("foo", "Zm9v"),
            ("foob", "Zm9vYg"),
            ("fooba", "Zm9vYmE"),
            ("foobar", "Zm9vYmFy"),
        ] {
            assert_eq!(base64_standard_unpadded(plain.as_bytes()), b64, "{plain:?}");
        }
        assert_eq!(base64_standard_unpadded(&[0xfb, 0xff, 0xbf]), "+/+/");
        assert_eq!(base64_url_unpadded(&[0xfb, 0xff, 0xbf]), "-_-_");
        // RFC 3986 §2.3: the unreserved characters pass, everything else is
        // encoded in upper-case hex.
        assert_eq!(percent_encode("AZaz09-._~"), "AZaz09-._~");
        assert_eq!(percent_encode("a b/c?d=e&f+g"), "a%20b%2Fc%3Fd%3De%26f%2Bg");
        assert_eq!(percent_encode("é"), "%C3%A9");
    }

    #[test]
    fn a_secret_is_found_in_each_of_its_forms_and_answered_by_kind() {
        let mut set = SecretSet::new();
        let token = "ghp_abc/DEF+ghi0123456789";
        set.add("the GitHub token", token);
        for text in [
            format!("posting {token} here"),
            format!("https://h/x?t={}", percent_encode(token)),
            format!("{{\"k\":\"{}\"}}", base64_standard_unpadded(token.as_bytes())),
            format!("{}=", base64_standard_unpadded(token.as_bytes())),
            base64_url_unpadded(token.as_bytes()),
        ] {
            let kind = set.found_in(&text);
            assert_eq!(kind, Some("the GitHub token"), "{text}");
            assert!(!kind.unwrap().contains(token), "the kind is not the value");
        }
        assert_eq!(set.found_in("nothing of the sort"), None);
        assert_eq!(set.found_in("ghp_abc/DEF"), None, "a prefix is not the secret");
    }

    #[test]
    fn a_bearer_header_adds_its_bare_token() {
        let mut set = SecretSet::new();
        set.add("a /guardian secret", "Bearer eyJhbGciOiJIUzI1NiJ9.payload");
        assert_eq!(set.found_in("token=eyJhbGciOiJIUzI1NiJ9.payload"), Some("a /guardian secret"));
        assert_eq!(set.found_in("Bearer eyJhbGciOiJIUzI1NiJ9.payload"), Some("a /guardian secret"));
    }

    /// Redaction replaces every form of every secret by its kind and
    /// leaves a text without one as it is (`None`).
    #[test]
    fn redaction_replaces_every_form_by_its_kind_and_leaves_clean_text_alone() {
        let mut set = SecretSet::new();
        set.add("the GitHub token", "ghp_0123456789abcdefghij");
        set.add("a git host token", "glpat-0123456789abcdef");
        let text = "remote: https://oauth2:glpat-0123456789abcdef@gitlab.example/x.git\nghp_0123456789abcdefghij and Z2hwXzAxMjM0NTY3ODlhYmNkZWZnaGlq";
        let out = set.redact(text).unwrap();
        assert_eq!(
            out,
            "remote: https://oauth2:[a git host token redacted]@gitlab.example/x.git\n[the GitHub token redacted] and [the GitHub token redacted]"
        );
        assert!(set.found_in(&out).is_none());
        assert!(set.redact("nothing stored appears here").is_none());
    }

    #[test]
    fn a_short_value_is_not_a_secret_and_an_empty_set_matches_nothing() {
        let mut set = SecretSet::new();
        set.add("the provider API key", "k");
        set.add("the provider API key", "1234567");
        set.add("the provider API key", "   ");
        assert!(set.is_empty());
        assert_eq!(set.found_in("k 1234567"), None);
        assert_eq!(SecretSet::new().found_in("anything at all"), None);
        set.add("the provider API key", "12345678");
        assert!(!set.is_empty());
        assert_eq!(set.found_in("x12345678y"), Some("the provider API key"));
    }
}
