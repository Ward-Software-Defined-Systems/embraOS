//! Static validation of the pasted guest module — the Guardian's
//! compile-time gate. Parse-only (`syn`); the module is **never executed**
//! to learn its name/schema. Enforces the scaffold-wrapped contract +
//! a denylist, then the compile step (`build`) is the second gate.
//!
//! The paste is a sequence of Rust items (a leading `// guardian-tool:`
//! marker comment is trivia to `syn`):
//! ```ignore
//! // guardian-tool: web_search
//! const GUARDIAN_NAME:   &str     = "web_search";
//! const GUARDIAN_DESC:   &str     = "…";
//! const GUARDIAN_SCHEMA: &str     = r#"{"type":"object","properties":{}}"#;
//! const GUARDIAN_CAPS:   &[&str]  = &["http_get"];   // optional, [] default
//! fn run(input: &str) -> String { … }
//! // …private helpers…
//! ```
//! The scaffold owns `#![no_std]`, the allocator, the panic handler, the
//! ABI exports, `mod json`, and `mod host` — so the paste contains none
//! of that, and any `unsafe`/FFI/`std`/extra-dep here is a real attempt.

use serde_json::Value;
use syn::spanned::Spanned;
use syn::visit::Visit;

use crate::abi;

const MAX_DESC: usize = 600;
const MAX_SCHEMA: usize = 8 * 1024;

/// Copy-fill skeleton handed to the intelligence — inlined in the
/// `guardian_propose` tool description (so the model sees it before
/// drafting) and appended to validation-failure messages (so a failed
/// attempt converges in one redraft). Reduces the propose→validate-fail
/// loop the contract prose alone didn't prevent. This exact text is run
/// through [`validate`] in the tests, so the skeleton we hand out always
/// passes the gate; `embra-brain` asserts the propose description contains
/// it verbatim, so the two never drift.
pub const GUARDIAN_TEMPLATE: &str = r##"// guardian-tool: example_tool
// Paste only this shape (+ any private helper fns); the scaffold owns
// #![no_std], the allocator, the panic handler, the ABI, and the
// json/host/html_text helpers. The validator rejects: std::*, unsafe,
// extern/FFI, mod, pub free items, `use` outside core/alloc/json/host/
// html_text, include!/env!/asm!, third-party crates. vec!/format! are fine;
// run must never panic (a panic becomes a tool error).
const GUARDIAN_NAME: &str = "example_tool";  // == marker above; ^[a-z][a-z0-9_]{2,39}$
const GUARDIAN_DESC: &str = "What this tool does and when to call it.";  // 1..=600 chars
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#;  // valid JSON, object root, <=8 KiB
// const GUARDIAN_CAPS: &[&str] = &["http_get"];  // optional; "http_get" and/or "web_search"; omit for pure compute

fn run(input: &str) -> String {
    // `input` is the JSON args matching GUARDIAN_SCHEMA. Parse defensively,
    // do the work, return any String (JSON recommended).
    let args = match json::parse(input) {
        Ok(a) => a,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let text = args.get("text").as_str().unwrap_or("");
    // ...your logic here (with a declared cap, e.g. host::http_get("https://..."))...
    json::stringify(&json::obj(vec![("ok", json::b(true)), ("text", json::s(text))]))
}
"##;

/// A module that passed every static check. `input_schema` is normalized
/// (object root guaranteed, `properties` stamped if absent).
#[derive(Debug, Clone)]
pub struct ValidatedModule {
    pub name: String,
    pub description: String,
    pub input_schema: Value,
    pub caps: Vec<String>,
    pub source: String,
}

/// A single, actionable rejection: which rule, where, and a fix hint.
#[derive(Debug, Clone)]
pub struct ValidationError {
    pub rule: &'static str,
    pub message: String,
    pub line: Option<usize>,
    pub col: Option<usize>,
}

impl std::fmt::Display for ValidationError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match (self.line, self.col) {
            (Some(l), Some(c)) => {
                write!(f, "guardian-validate [{}] at {l}:{c}: {}", self.rule, self.message)
            }
            _ => write!(f, "guardian-validate [{}]: {}", self.rule, self.message),
        }
    }
}
impl std::error::Error for ValidationError {}

fn err(rule: &'static str, msg: impl Into<String>) -> ValidationError {
    ValidationError { rule, message: msg.into(), line: None, col: None }
}

fn err_at(rule: &'static str, msg: impl Into<String>, sp: proc_macro2::Span) -> ValidationError {
    let s = sp.start();
    ValidationError {
        rule,
        message: msg.into(),
        line: Some(s.line),
        col: Some(s.column + 1),
    }
}

fn ident_ok(name: &str) -> bool {
    let b = name.as_bytes();
    (3..=40).contains(&name.len())
        && b[0].is_ascii_lowercase()
        && b.iter().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || *c == b'_')
}

/// Validate a pasted module. `reserved` = built-in tool names the brain
/// passes in; the two meta-tool names are always reserved too.
pub fn validate(source: &str, reserved: &[&str]) -> Result<ValidatedModule, ValidationError> {
    // 1. marker comment (raw text — `syn` discards comments).
    let marker = source
        .lines()
        .map(str::trim)
        .find_map(|l| l.strip_prefix("// guardian-tool:"))
        .map(str::trim)
        .ok_or_else(|| {
            err("marker", "first line must be `// guardian-tool: <name>`")
        })?;
    if !ident_ok(marker) {
        return Err(err(
            "name",
            format!("tool name `{marker}` must match ^[a-z][a-z0-9_]{{2,39}}$"),
        ));
    }
    if marker == "guardian_call"
        || marker == "guardian_list"
        || reserved.contains(&marker)
    {
        return Err(err("name", format!("tool name `{marker}` is reserved")));
    }

    // 2. parse.
    let file = syn::parse_file(source).map_err(|e| {
        let s = e.span().start();
        ValidationError {
            rule: "parse",
            message: format!("not valid Rust: {e}"),
            line: Some(s.line),
            col: Some(s.column + 1),
        }
    })?;

    // 3. denylist walk (first violation wins, by source order).
    let mut deny = Deny { err: None };
    deny.visit_file(&file);
    if let Some(e) = deny.err {
        return Err(e);
    }

    // 4. structural contract.
    let mut name_lit = None;
    let mut desc_lit = None;
    let mut schema_lit = None;
    let mut caps_lits: Option<Vec<String>> = None;
    let mut run_count = 0usize;

    for item in &file.items {
        match item {
            syn::Item::Const(c) => {
                let id = c.ident.to_string();
                let span = c.span();
                match id.as_str() {
                    "GUARDIAN_NAME" => name_lit = Some((str_lit(&c.expr, "GUARDIAN_NAME", span)?, span)),
                    "GUARDIAN_DESC" => desc_lit = Some(str_lit(&c.expr, "GUARDIAN_DESC", span)?),
                    "GUARDIAN_SCHEMA" => schema_lit = Some(str_lit(&c.expr, "GUARDIAN_SCHEMA", span)?),
                    "GUARDIAN_CAPS" => caps_lits = Some(str_array(&c.expr, span)?),
                    _ => {}
                }
            }
            syn::Item::Fn(f) => {
                if f.sig.ident == "run" {
                    run_count += 1;
                    check_run_sig(f)?;
                }
            }
            _ => {}
        }
    }

    let (name, name_span) = name_lit
        .ok_or_else(|| err("contract", "missing `const GUARDIAN_NAME: &str`"))?;
    if name != marker {
        return Err(err_at(
            "contract",
            format!("GUARDIAN_NAME `{name}` must equal the marker `{marker}`"),
            name_span,
        ));
    }
    let description =
        desc_lit.ok_or_else(|| err("contract", "missing `const GUARDIAN_DESC: &str`"))?;
    if description.trim().is_empty() || description.len() > MAX_DESC {
        return Err(err("description", format!("description must be 1..={MAX_DESC} chars")));
    }
    let schema_str =
        schema_lit.ok_or_else(|| err("contract", "missing `const GUARDIAN_SCHEMA: &str`"))?;
    if schema_str.len() > MAX_SCHEMA {
        return Err(err("schema", format!("schema must be <= {MAX_SCHEMA} bytes")));
    }
    let input_schema = normalize_schema(&schema_str)?;

    if run_count == 0 {
        return Err(err("contract", "missing `fn run(input: &str) -> String`"));
    }
    if run_count > 1 {
        return Err(err("contract", "exactly one `fn run` is allowed"));
    }

    let mut caps = caps_lits.unwrap_or_default();
    caps.sort();
    caps.dedup();
    for c in &caps {
        if !abi::KNOWN_CAPS.contains(&c.as_str()) {
            return Err(err(
                "capability",
                format!("unknown capability `{c}` (known: {:?})", abi::KNOWN_CAPS),
            ));
        }
    }

    Ok(ValidatedModule {
        name,
        description,
        input_schema,
        caps,
        source: source.to_string(),
    })
}

fn str_lit(
    expr: &syn::Expr,
    what: &str,
    span: proc_macro2::Span,
) -> Result<String, ValidationError> {
    if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) = expr {
        Ok(s.value())
    } else {
        Err(err_at("contract", format!("`{what}` must be a string literal"), span))
    }
}

fn str_array(
    expr: &syn::Expr,
    span: proc_macro2::Span,
) -> Result<Vec<String>, ValidationError> {
    // Accept `&["a", "b"]` or `&[]`.
    let arr = match expr {
        syn::Expr::Reference(r) => &*r.expr,
        other => other,
    };
    if let syn::Expr::Array(a) = arr {
        let mut out = Vec::new();
        for el in &a.elems {
            if let syn::Expr::Lit(syn::ExprLit { lit: syn::Lit::Str(s), .. }) = el {
                out.push(s.value());
            } else {
                return Err(err_at("capability", "GUARDIAN_CAPS must be string literals", span));
            }
        }
        Ok(out)
    } else {
        Err(err_at("capability", "GUARDIAN_CAPS must be `&[&str]`", span))
    }
}

fn check_run_sig(f: &syn::ItemFn) -> Result<(), ValidationError> {
    let span = f.sig.span();
    if f.sig.unsafety.is_some() {
        return Err(err_at("unsafe", "`run` must not be `unsafe`", span));
    }
    if !matches!(f.vis, syn::Visibility::Inherited) {
        return Err(err_at("visibility", "`run` must not be `pub` (the scaffold exports it)", span));
    }
    if f.sig.inputs.len() != 1 {
        return Err(err_at("contract", "`run` must take exactly one `&str`", span));
    }
    let ok_in = matches!(
        f.sig.inputs.first(),
        Some(syn::FnArg::Typed(p)) if is_str_ref(&p.ty)
    );
    if !ok_in {
        return Err(err_at("contract", "`run`'s parameter must be `&str`", span));
    }
    match &f.sig.output {
        syn::ReturnType::Type(_, ty) if path_tail_is(ty, "String") => Ok(()),
        _ => Err(err_at("contract", "`run` must return `String`", span)),
    }
}

fn is_str_ref(ty: &syn::Type) -> bool {
    matches!(ty, syn::Type::Reference(r) if path_tail_is(&r.elem, "str"))
}

fn path_tail_is(ty: &syn::Type, want: &str) -> bool {
    matches!(ty, syn::Type::Path(p) if p.path.segments.last().is_some_and(|s| s.ident == want))
}

fn normalize_schema(s: &str) -> Result<Value, ValidationError> {
    let mut v: Value = serde_json::from_str(s)
        .map_err(|e| err("schema", format!("GUARDIAN_SCHEMA is not valid JSON: {e}")))?;
    let obj = v
        .as_object_mut()
        .ok_or_else(|| err("schema", "schema root must be a JSON object"))?;
    if obj.get("type").and_then(Value::as_str) != Some("object") {
        return Err(err("schema", r#"schema root must have "type":"object""#));
    }
    if !obj.get("properties").is_some_and(Value::is_object) {
        obj.insert("properties".into(), serde_json::json!({}));
    }
    Ok(v)
}

/// Denylist visitor. Records the first violation in source order.
struct Deny {
    err: Option<ValidationError>,
}

impl Deny {
    fn flag(&mut self, e: ValidationError) {
        if self.err.is_none() {
            self.err = Some(e);
        }
    }
}

const DENIED_MACROS: &[&str] = &[
    "include", "include_str", "include_bytes", "env", "option_env",
    "asm", "global_asm", "concat_idents",
];
const USE_ROOTS_OK: &[&str] =
    &["core", "alloc", "crate", "self", "super", "json", "host", "html_text"];

impl<'ast> Visit<'ast> for Deny {
    fn visit_item_extern_crate(&mut self, i: &'ast syn::ItemExternCrate) {
        self.flag(err_at("extern_crate", "`extern crate` is not allowed", i.span()));
    }
    fn visit_item_foreign_mod(&mut self, i: &'ast syn::ItemForeignMod) {
        self.flag(err_at("ffi", "`extern { … }` / raw FFI is not allowed", i.span()));
    }
    fn visit_item_mod(&mut self, i: &'ast syn::ItemMod) {
        self.flag(err_at(
            "module",
            "`mod` is not allowed; the scaffold provides `json`/`host`/`html_text`",
            i.span(),
        ));
    }
    fn visit_expr_unsafe(&mut self, i: &'ast syn::ExprUnsafe) {
        self.flag(err_at("unsafe", "`unsafe` is not allowed", i.span()));
    }
    fn visit_item_impl(&mut self, i: &'ast syn::ItemImpl) {
        if i.unsafety.is_some() {
            self.flag(err_at("unsafe", "`unsafe impl` is not allowed", i.span()));
        }
        syn::visit::visit_item_impl(self, i);
    }
    fn visit_item_fn(&mut self, i: &'ast syn::ItemFn) {
        if i.sig.unsafety.is_some() {
            self.flag(err_at("unsafe", "`unsafe fn` is not allowed", i.sig.span()));
        }
        if i.sig.ident != "run" && !matches!(i.vis, syn::Visibility::Inherited) {
            self.flag(err_at("visibility", "free items must not be `pub`", i.sig.span()));
        }
        syn::visit::visit_item_fn(self, i);
    }
    fn visit_macro(&mut self, m: &'ast syn::Macro) {
        if let Some(seg) = m.path.segments.last() {
            let n = seg.ident.to_string();
            if DENIED_MACROS.contains(&n.as_str()) {
                self.flag(err_at("macro", format!("`{n}!` is not allowed"), m.span()));
            }
        }
        syn::visit::visit_macro(self, m);
    }
    fn visit_item_use(&mut self, u: &'ast syn::ItemUse) {
        if let Some(r) = use_root(&u.tree)
            && !USE_ROOTS_OK.contains(&r.as_str())
        {
            self.flag(err_at(
                "use",
                format!("`use {r}::…` is not allowed (v1 permits no extra crates; \
                         only core/alloc + scaffold `json`/`host`/`html_text`)"),
                u.span(),
            ));
        }
        syn::visit::visit_item_use(self, u);
    }
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> =
            p.segments.iter().map(|s| s.ident.to_string()).collect();
        let two = (segs.first().map(String::as_str), segs.get(1).map(String::as_str));
        let denied = matches!(
            two,
            (Some("std"), Some("process" | "fs" | "net" | "os" | "env" | "thread" | "ffi" | "arch"))
                | (Some("core"), Some("arch"))
        ) || segs.first().map(String::as_str) == Some("proc_macro")
            || (segs.first().map(String::as_str) == Some("std") && segs.len() == 1);
        if denied {
            self.flag(err_at(
                "forbidden_path",
                format!("path `{}` is not allowed", segs.join("::")),
                p.span(),
            ));
        }
        syn::visit::visit_path(self, p);
    }
    fn visit_attribute(&mut self, a: &'ast syn::Attribute) {
        if let Some(seg) = a.path().segments.last() {
            let n = seg.ident.to_string();
            if matches!(
                n.as_str(),
                "no_mangle" | "export_name" | "link" | "link_section"
                    | "panic_handler" | "global_allocator" | "no_std" | "feature"
            ) {
                self.flag(err_at(
                    "attribute",
                    format!("`#[{n}]` is not allowed (the scaffold owns ABI/runtime attrs)"),
                    a.span(),
                ));
            }
        }
        syn::visit::visit_attribute(self, a);
    }
}

fn use_root(tree: &syn::UseTree) -> Option<String> {
    match tree {
        syn::UseTree::Path(p) => Some(p.ident.to_string()),
        syn::UseTree::Name(n) => Some(n.ident.to_string()),
        syn::UseTree::Rename(r) => Some(r.ident.to_string()),
        syn::UseTree::Group(g) => g.items.iter().find_map(use_root),
        syn::UseTree::Glob(_) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guardian_template_passes_the_gate() {
        // The skeleton we hand the intelligence must itself validate, or we'd
        // be teaching it to write modules the gate rejects.
        validate(GUARDIAN_TEMPLATE, &[])
            .expect("GUARDIAN_TEMPLATE must pass the validator");
    }

    const GOOD: &str = r##"
// guardian-tool: temp_delta
const GUARDIAN_NAME: &str = "temp_delta";
const GUARDIAN_DESC: &str = "Delta between two temperatures.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"a":{"type":"number"}}}"#;
const GUARDIAN_CAPS: &[&str] = &["http_get"];
fn run(input: &str) -> String {
    let _ = clamp(input.len());
    String::from("ok")
}
fn clamp(n: usize) -> usize { if n > 10 { 10 } else { n } }
"##;

    fn reject(src: &str) -> ValidationError {
        validate(src, &["git_status"]).expect_err("should reject")
    }

    #[test]
    fn good_module_validates() {
        let m = validate(GOOD, &["git_status"]).expect("should pass");
        assert_eq!(m.name, "temp_delta");
        assert_eq!(m.caps, vec!["http_get"]);
        assert_eq!(m.input_schema["type"], "object");
        assert!(m.input_schema["properties"].is_object());
    }

    #[test]
    fn missing_marker() {
        assert_eq!(reject("const GUARDIAN_NAME: &str = \"x\";").rule, "marker");
    }

    #[test]
    fn bad_name_and_reserved() {
        assert_eq!(reject("// guardian-tool: Bad-Name\n").rule, "name");
        let src = "// guardian-tool: git_status\n";
        assert_eq!(reject(src).rule, "name");
    }

    #[test]
    fn schema_must_be_object() {
        let src = r##"
// guardian-tool: demo
const GUARDIAN_NAME: &str = "demo";
const GUARDIAN_DESC: &str = "d";
const GUARDIAN_SCHEMA: &str = r#"[1,2,3]"#;
fn run(input: &str) -> String { String::new() }
"##;
        assert_eq!(reject(src).rule, "schema");
    }

    #[test]
    fn unknown_capability() {
        let src = GOOD.replace(r#"&["http_get"]"#, r#"&["spawn_proc"]"#);
        assert_eq!(reject(&src).rule, "capability");
    }

    #[test]
    fn html_text_helper_use_and_call_allowed() {
        // `html_text` is a scaffold-shipped module like `json`: both a
        // `use html_text::…;` and a bare `html_text::to_text(..)` call
        // must pass the denylist.
        let src = r##"
// guardian-tool: reader
const GUARDIAN_NAME: &str = "reader";
const GUARDIAN_DESC: &str = "Fetch + read.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"u":{"type":"string"}}}"#;
const GUARDIAN_CAPS: &[&str] = &["http_get"];
use html_text::to_text;
fn run(input: &str) -> String {
    let page = host::http_get(input);
    let _ = to_text(&page);
    html_text::to_text(&page)
}
"##;
        let m = validate(src, &[]).expect("html_text use + call must be allowed");
        assert_eq!(m.name, "reader");
    }

    #[test]
    fn denies_unsafe_ffi_std_macros_use_attrs() {
        let cases = [
            ("unsafe", "fn run(input: &str) -> String { unsafe { } String::new() }"),
            ("ffi", "extern \"C\" { fn syscall(); }\nfn run(input: &str) -> String { String::new() }"),
            ("forbidden_path", "fn run(input: &str) -> String { std::fs::read(\"/x\"); String::new() }"),
            ("use", "use serde::Serialize;\nfn run(input: &str) -> String { String::new() }"),
            ("macro", "fn run(input: &str) -> String { include_str!(\"/etc/passwd\").into() }"),
            ("module", "mod sneaky { }\nfn run(input: &str) -> String { String::new() }"),
            ("attribute", "#[no_mangle] fn run(input: &str) -> String { String::new() }"),
            ("extern_crate", "extern crate alloc;\nfn run(input: &str) -> String { String::new() }"),
        ];
        for (rule, body) in cases {
            let src = format!(
                "// guardian-tool: demo\nconst GUARDIAN_NAME: &str=\"demo\";\
                 const GUARDIAN_DESC: &str=\"d\";\
                 const GUARDIAN_SCHEMA: &str=r#\"{{\"type\":\"object\"}}\"#;\n{body}"
            );
            assert_eq!(reject(&src).rule, rule, "case `{rule}` body: {body}");
        }
    }

    #[test]
    fn run_must_be_str_to_string_and_singular() {
        let two = format!("{GOOD}\nfn run(x: &str) -> String {{ String::new() }}");
        assert_eq!(reject(&two).rule, "contract");
        let badsig = r##"
// guardian-tool: demo
const GUARDIAN_NAME: &str = "demo";
const GUARDIAN_DESC: &str = "d";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object"}"#;
fn run(n: usize) -> String { String::new() }
"##;
        assert_eq!(reject(badsig).rule, "contract");
    }
}
