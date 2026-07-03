//! Guards `docs/GUARDIAN-TOOL-EXAMPLES.md`: every fenced ```rust module
//! in the operator doc must pass the real `embra_guardian::validate`
//! gate, so the published examples can never silently drift out of spec.

const DOCS: &[&str] = &[
    include_str!("../../../docs/GUARDIAN-TOOL-EXAMPLES.md"),
    include_str!("../../../docs/GUARDIAN-ADVANCED-EXAMPLE.md"),
    include_str!("../../../docs/GUARDIAN-KG-SCAN-EXAMPLE.md"),
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
                    // Capability-declaring examples must keep declaring
                    // exactly the caps their `host::` calls require, or
                    // the doc has drifted from the validator's KNOWN_CAPS
                    // rule. `m.caps` is sorted+deduped by the validator,
                    // so the flagship's `["web_search","http_get"]`
                    // surfaces as the alphabetical `["http_get",
                    // "web_search"]`.
                    let expect_caps: &[&str] = match m.name.as_str() {
                        "http_fetch" => &["http_get"],
                        "web_search" => &["http_get", "web_search"],
                        _ => &[],
                    };
                    assert_eq!(
                        m.caps,
                        expect_caps.iter().map(|s| s.to_string()).collect::<Vec<_>>(),
                        "{} must declare exactly {expect_caps:?}",
                        m.name
                    );
                }
                buf.clear();
            } else {
                buf.push_str(line);
                buf.push('\n');
            }
        }
    }
    assert!(
        count >= 6,
        "expected >=6 guardian modules across the docs/ examples, found {count}"
    );
}
