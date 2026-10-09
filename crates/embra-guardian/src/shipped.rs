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

/// Every shipped tool, in install order.
pub const SHIPPED: &[ShippedTool] = &[WEB_SEARCH];

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
];

/// The hashes of every version ever shipped under `name`, oldest first.
pub fn known_sha256s(name: &str) -> &'static [&'static str] {
    match name {
        "web_search" => KNOWN_WEB_SEARCH_SHA256,
        _ => &[],
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
    fn the_shipped_web_search_source_is_the_doc_example() {
        let doc = include_str!("../../../docs/GUARDIAN-ADVANCED-EXAMPLE.md");
        assert_eq!(doc_module(doc, "web_search"), WEB_SEARCH.stored_source());
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
    }

    #[test]
    fn the_known_hashes_end_with_the_current_source() {
        let known = known_sha256s("web_search");
        assert_eq!(known.last().copied(), Some(WEB_SEARCH.source_sha256().as_str()));
        assert!(known.iter().all(|h| h.len() == 64 && h.bytes().all(|b| b.is_ascii_hexdigit())));
        let mut uniq = known.to_vec();
        uniq.sort_unstable();
        uniq.dedup();
        assert_eq!(uniq.len(), known.len(), "a hash repeats");
        assert!(known_sha256s("other").is_empty());
    }
}
