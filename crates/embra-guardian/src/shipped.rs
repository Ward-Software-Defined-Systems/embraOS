//! Tools that ship with embraOS, defined at boot by the brain.
//!
//! A shipped tool is project-reviewed: its module passes the validator
//! like any paste and is installed without the replicant check (at boot
//! there is no config and no provider to judge with). The brain installs
//! one that is absent, updates one it installed and the operator never
//! edited when the shipped source moves, adopts an operator's copy of a
//! version that was shipped, and leaves an edited or unrelated tool of the
//! same name alone; `/guardian delete` of a shipped name records a
//! decline (`embra-brain/src/guardian/mod.rs::ensure_shipped_tools`).
//! Each source is its doc example, byte for byte: the tests here pin it.

/// A tool the image carries, by name and module source.
pub struct ShippedTool {
    pub name: &'static str,
    pub source: &'static str,
}

impl ShippedTool {
    /// The hash a stored document carries for this source: the module as
    /// an operator paste is stored, trimmed (`store::sha256_hex`).
    pub fn source_sha256(&self) -> String {
        crate::store::sha256_hex(self.source.trim())
    }

    /// The source as the brain stores it.
    pub fn stored_source(&self) -> &'static str {
        self.source.trim()
    }
}

/// The flagship: prompt-injection-hardened web search over the Brave guard
/// (`docs/GUARDIAN-ADVANCED-EXAMPLE.md`). Inert until the operator sets a
/// key with `/guardian key brave`.
pub const WEB_SEARCH: ShippedTool = ShippedTool {
    name: "web_search",
    source: include_str!("shipped/web_search.rs"),
};

/// A guarded curl: one HTTP request with headers, query pairs and a body,
/// for pages, resources and API work (`docs/GUARDIAN-HTTP-REQUEST-EXAMPLE.md`).
/// Needs no key: credentials come from `/guardian secret`, private hosts
/// from `/guardian egress allow`.
pub const HTTP_REQUEST: ShippedTool = ShippedTool {
    name: "http_request",
    source: include_str!("shipped/http_request.rs"),
};

/// Every shipped tool, in install order.
pub const SHIPPED: &[ShippedTool] = &[WEB_SEARCH, HTTP_REQUEST];

/// Every version of the `web_search` module the project has shipped as
/// the doc example, oldest first; the last is the current source. A stored
/// tool whose source hash is one of these is a copy of a shipped version:
/// the brain adopts it and brings it to the current source.
pub const KNOWN_WEB_SEARCH_SHA256: &[&str] = &[
    // 6e25103 (2026-05-17), the first example
    "d05121c55b8a977e9dd4d274a8b5626e50c4a14990988506b219b484d987e787",
    // 3da1387 (2026-05-17), the real Brave-backed capability
    "1c201d4d802bed7a19971ff331034babe6e1883d383be75b5b83757cea952fab",
    // 50bd020 (2026-05-17) through 2026-10-09: JSON request, age, html_text, fetch_top
    "8736930de5de53de04f64bcbd9315a6a65b200a86a9e6072aca253cd2df44b72",
    // 2026-10-09 (Embra#17): the widened redactor, flag-only "system prompt", min_score
    "e5ea961920f40dbeef580df521a8903f91cea1b64de2299db7962faa3f1a5958",
    // 2026-10-09 (Embra#17, the rerun): weak objects need a qualifier; fetch_status, fetch_error
    "dca702dfcdd5a789e960b1068bd583c22eac75670eb6ee650833202ce4f3bf8f",
    // 2026-10-09 (Embra#17, the second rerun): score before redaction, the snippets marker, fetch_url and fetch_redirect
    "37b205bde4f9f1f1c3754ce7fcf347cab4a799393a061dfd32fdaa4ae35a0131",
    // 2026-10-09 (Embra#17, the third rerun): a possessive rides the directive; query words trimmed of punctuation
    "7d9bec236041453bff992ef00e7bf57190a7bb78a61bb79053bd70d8aa74fedc",
    // 2026-10-09: the scrubber moved to the vendored `inject` helper, shared with http_request
    "efa59644ff899c5d5e6f3c702e3e31b946a0e5f56c8ca17d617254799cc63299",
];

/// The hashes of every version ever shipped under `name`, oldest first.
pub fn known_sha256s(name: &str) -> &'static [&'static str] {
    match name {
        "web_search" => KNOWN_WEB_SEARCH_SHA256,
        "http_request" => KNOWN_HTTP_REQUEST_SHA256,
        _ => &[],
    }
}

/// Every version of the `http_request` module the project has shipped,
/// oldest first; the last is the current source.
pub const KNOWN_HTTP_REQUEST_SHA256: &[&str] = &[
    // 2026-10-09: the first version
    "178c62efb322d2a85cccd82a0c5d9b2689748e5dcfe85ec2f56d1983a00d5568",
];

/// The doc page a shipped tool's module is mirrored on, byte for byte.
#[cfg(test)]
fn doc_of(name: &str) -> &'static str {
    match name {
        "web_search" => include_str!("../../../docs/GUARDIAN-ADVANCED-EXAMPLE.md"),
        "http_request" => include_str!("../../../docs/GUARDIAN-HTTP-REQUEST-EXAMPLE.md"),
        other => panic!("no doc for the shipped tool {other}"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The doc's module block, extracted the way `tests/doc_examples_validate.rs` does.
    fn doc_module(doc: &str, name: &str) -> String {
        let marker = format!("// guardian-tool: {name}");
        let mut in_rust = false;
        let mut buf = String::new();
        for line in doc.lines() {
            let trimmed = line.trim();
            if !in_rust {
                if trimmed == "```rust" {
                    in_rust = true;
                    buf.clear();
                }
                continue;
            }
            if trimmed == "```" {
                in_rust = false;
                if buf.contains(&marker) {
                    return buf.trim().to_string();
                }
                buf.clear();
            } else {
                buf.push_str(line);
                buf.push('\n');
            }
        }
        panic!("no `{marker}` block in the doc");
    }

    #[test]
    fn every_shipped_source_is_its_doc_example() {
        for t in SHIPPED {
            assert_eq!(doc_module(doc_of(t.name), t.name), t.stored_source(), "{}", t.name);
        }
    }

    #[test]
    fn every_shipped_source_passes_the_validator() {
        for t in SHIPPED {
            let m = crate::validate(t.source, &[]).unwrap_or_else(|e| panic!("{}: {e}", t.name));
            assert_eq!(m.name, t.name);
        }
        let m = crate::validate(WEB_SEARCH.source, &[]).unwrap();
        assert_eq!(m.caps, vec!["http_get".to_string(), "web_search".to_string()]);
        assert!(m.description.contains("/guardian key brave"), "the description names the key");
        let m = crate::validate(HTTP_REQUEST.source, &[]).unwrap();
        assert_eq!(m.caps, vec!["http_request".to_string()]);
        assert!(
            m.description.contains("/guardian secret") && m.description.contains("/guardian egress allow"),
            "the description names the two commands"
        );
        assert!(m.input_schema["properties"]["json"].is_object(), "{}", m.input_schema);
    }

    #[test]
    fn every_known_hash_list_ends_with_its_current_source() {
        for t in SHIPPED {
            let known = known_sha256s(t.name);
            assert_eq!(known.last().copied(), Some(t.source_sha256().as_str()), "{}", t.name);
            assert!(known.iter().all(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())));
            let mut uniq = known.to_vec();
            uniq.sort_unstable();
            uniq.dedup();
            assert_eq!(uniq.len(), known.len(), "a hash repeats for {}", t.name);
        }
        assert!(known_sha256s("other").is_empty());
        let names: Vec<&str> = SHIPPED.iter().map(|t| t.name).collect();
        assert_eq!(names, ["web_search", "http_request"]);
    }
}
