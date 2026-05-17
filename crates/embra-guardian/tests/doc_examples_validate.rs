//! Guards `docs/GUARDIAN-TOOL-EXAMPLES.md`: every fenced ```rust module
//! in the operator doc must pass the real `embra_guardian::validate`
//! gate, so the published examples can never silently drift out of spec.

const DOC: &str = include_str!("../../../docs/GUARDIAN-TOOL-EXAMPLES.md");

#[test]
fn every_doc_module_passes_the_validator() {
    let mut count = 0usize;
    for seg in DOC.split("```rust").skip(1) {
        let code = seg.split("```").next().unwrap_or("").trim();
        if !code.contains("// guardian-tool:") {
            continue;
        }
        count += 1;
        let m = embra_guardian::validate(code, &[]).unwrap_or_else(|e| {
            panic!("doc example failed validation: {e}\n--- module ---\n{code}\n---")
        });
        if m.name == "http_fetch" {
            assert_eq!(
                m.caps,
                vec!["http_get".to_string()],
                "http_fetch must declare the http_get capability"
            );
        }
    }
    assert!(
        count >= 4,
        "expected >=4 guardian modules in docs/GUARDIAN-TOOL-EXAMPLES.md, found {count}"
    );
}
