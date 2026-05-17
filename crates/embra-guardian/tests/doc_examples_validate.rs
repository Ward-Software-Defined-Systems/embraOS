//! Guards `docs/GUARDIAN-TOOL-EXAMPLES.md`: every fenced ```rust module
//! in the operator doc must pass the real `embra_guardian::validate`
//! gate, so the published examples can never silently drift out of spec.

const DOCS: &[&str] = &[
    include_str!("../../../docs/GUARDIAN-TOOL-EXAMPLES.md"),
    include_str!("../../../docs/GUARDIAN-ADVANCED-EXAMPLE.md"),
];

#[test]
fn every_doc_module_passes_the_validator() {
    let mut count = 0usize;
    for doc in DOCS {
        // Markdown fence rules: a ```rust block closes only on a line
        // whose trimmed content is exactly ``` — NOT on a ``` that
        // appears inside a string literal on some other line (e.g. the
        // web_search module's own "```tool" injection marker).
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
                let code = buf.trim();
                if code.contains("// guardian-tool:") {
                    count += 1;
                    let m = embra_guardian::validate(code, &[]).unwrap_or_else(|e| {
                        panic!("doc example failed validation: {e}\n--- module ---\n{code}\n---")
                    });
                    if m.name == "http_fetch" || m.name == "web_search" {
                        assert_eq!(
                            m.caps,
                            vec!["http_get".to_string()],
                            "{} must declare the http_get capability",
                            m.name
                        );
                    }
                }
                buf.clear();
            } else {
                buf.push_str(line);
                buf.push('\n');
            }
        }
    }
    assert!(
        count >= 5,
        "expected >=5 guardian modules across the docs/ examples, found {count}"
    );
}
