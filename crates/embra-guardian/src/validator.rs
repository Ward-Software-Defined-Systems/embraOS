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
// json/host/html_text/inject helpers. The validator rejects: std::*, unsafe,
// extern/FFI, mod, pub free items, `use` outside core/alloc/json/host/
// html_text/inject, include!/env!/asm! (raw or imported too), third-party
// crates. vec!/format! are fine; run must never panic (a panic becomes a
// tool error).
const GUARDIAN_NAME: &str = "example_tool";  // == marker above; ^[a-z][a-z0-9_]{2,39}$
const GUARDIAN_DESC: &str = "What this tool does and when to call it.";  // 1..=600 chars
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#;  // valid JSON, object root, <=8 KiB
// const GUARDIAN_CAPS: &[&str] = &["http_get"];  // optional: "http_get", "http_request" and/or "web_search"; omit for pure compute

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

/// The tool-name rule, `[a-z][a-z0-9_]{2,39}`, which the validator and the
/// scaffold's path jail (`safe_tool_dir`) both apply: one definition, so a
/// name the validator accepts is one the jail can never read as a path.
pub(crate) fn valid_tool_name(name: &str) -> bool {
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
    if !valid_tool_name(marker) {
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
            syn::Item::Fn(f) if f.sig.ident == "run" => {
                run_count += 1;
                check_run_sig(f)?;
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

// `macro_rules` is refused as a whole: a declared macro's body is tokens
// `syn` never parses into items, so `extern`, `mod`, `pub` or an ABI
// attribute inside it would reach the compiler unseen. No shipped or
// documented module declares one.
const DENIED_MACROS: &[&str] = &[
    "include", "include_str", "include_bytes", "env", "option_env",
    "asm", "global_asm", "concat_idents", "macro_rules",
];
const USE_ROOTS_OK: &[&str] =
    &["core", "alloc", "crate", "self", "super", "json", "host", "html_text", "inject"];

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
            let n = ident_name(&seg.ident);
            if DENIED_MACROS.contains(&n.as_str()) {
                self.flag(err_at("macro", format!("`{n}!` is not allowed"), m.span()));
            }
        }
        self.scan_tokens(m.tokens.clone());
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
        // A denied macro imported under another name is called by that
        // name, which `visit_macro` cannot know (`use core::include_str as
        // inc; inc!("/x")`, the verification of 2026-10-31), and a denied
        // path can be imported as well: every leaf of the tree is checked by
        // the name it imports, whatever it is renamed to.
        let mut leaves: Vec<Vec<String>> = vec![];
        use_leaves(&u.tree, &mut vec![], &mut leaves);
        for segs in &leaves {
            if let Some(last) = segs.last()
                && DENIED_MACROS.contains(&last.as_str())
            {
                self.flag(err_at(
                    "macro",
                    format!("`{last}` is not allowed, imported or called"),
                    u.span(),
                ));
            }
            if denied_path(segs) {
                self.flag(err_at(
                    "forbidden_path",
                    format!("path `{}` is not allowed", segs.join("::")),
                    u.span(),
                ));
            }
        }
        syn::visit::visit_item_use(self, u);
    }
    fn visit_path(&mut self, p: &'ast syn::Path) {
        let segs: Vec<String> =
            p.segments.iter().map(|s| ident_name(&s.ident)).collect();
        if denied_path(&segs) {
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
            let n = ident_name(&seg.ident);
            // `#[unsafe(no_mangle)]` is the edition-2024 spelling of an ABI
            // attribute: the last segment is `unsafe`, not the name inside
            // it, so the wrapper is refused outright (#12).
            if n == "unsafe" {
                self.flag(err_at(
                    "attribute",
                    "`#[unsafe(...)]` is not allowed (the scaffold owns ABI/runtime attrs)",
                    a.span(),
                ));
            }
            // `cfg_attr` wraps another attribute in tokens this walker
            // would never see as an attribute (`#[cfg_attr(all(), no_mangle)]`),
            // so the wrapper is refused with the names it could carry.
            if matches!(
                n.as_str(),
                "no_mangle" | "export_name" | "link" | "link_section"
                    | "panic_handler" | "global_allocator" | "no_std" | "feature"
                    | "cfg_attr"
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
    /// The tokens of a list attribute (`#[name(…)]`), which `syn` leaves
    /// unparsed: scanned like a macro's, so a denied macro or path inside
    /// one is refused as it is anywhere else.
    fn visit_meta_list(&mut self, m: &'ast syn::MetaList) {
        self.scan_tokens(m.tokens.clone());
        syn::visit::visit_meta_list(self, m);
    }
}

/// The path rule `visit_path` and the token scan share: `std::{process, fs,
/// net, os, env, thread, ffi, arch}`, `core::arch`, anything under
/// `proc_macro`, and a bare `std`.
fn denied_path(segs: &[String]) -> bool {
    let two = (segs.first().map(String::as_str), segs.get(1).map(String::as_str));
    matches!(
        two,
        (Some("std"), Some("process" | "fs" | "net" | "os" | "env" | "thread" | "ffi" | "arch"))
            | (Some("core"), Some("arch"))
    ) || segs.first().map(String::as_str) == Some("proc_macro")
        || (segs.first().map(String::as_str) == Some("std") && segs.len() == 1)
}

/// Every token of a stream, groups opened, in order.
fn flatten(tokens: proc_macro2::TokenStream, out: &mut Vec<proc_macro2::TokenTree>) {
    for tt in tokens {
        match tt {
            proc_macro2::TokenTree::Group(g) => flatten(g.stream(), out),
            other => out.push(other),
        }
    }
}

impl Deny {
    /// The tokens of a macro invocation or a list attribute, which `syn`
    /// leaves unparsed. The scan checks three things, each as the parsed
    /// form is checked: the ident `unsafe`; a path `denied_path` refuses;
    /// and a denied macro name followed by `!`, bare or as the last segment
    /// of a path (`core::include_str!`), the segment `visit_macro` reads
    /// (#12), every identifier read unraw'd (`ident_name`). It does not
    /// recognise items, so a declared macro's body is kept out by refusing
    /// `macro_rules!` itself.
    fn scan_tokens(&mut self, tokens: proc_macro2::TokenStream) {
        use proc_macro2::TokenTree;
        let mut flat = Vec::new();
        flatten(tokens, &mut flat);
        let is_colon = |tt: Option<&TokenTree>| matches!(tt, Some(TokenTree::Punct(p)) if p.as_char() == ':');
        let mut i = 0;
        while i < flat.len() {
            let TokenTree::Ident(id) = &flat[i] else {
                i += 1;
                continue;
            };
            let name = ident_name(id);
            if name == "unsafe" {
                self.flag(err_at("unsafe", "`unsafe` inside a macro invocation is not allowed", id.span()));
            }
            // A path: `ident (:: ident)*`.
            let mut segs = vec![name];
            let mut j = i;
            while is_colon(flat.get(j + 1)) && is_colon(flat.get(j + 2)) {
                match flat.get(j + 3) {
                    Some(TokenTree::Ident(next)) => {
                        segs.push(ident_name(next));
                        j += 3;
                    }
                    _ => break,
                }
            }
            // A denied macro is named by the last segment, followed by `!`:
            // `include_str!` and `core::include_str!` alike. Checked after
            // the path is collected, so a qualified name is not stepped over.
            if let Some(last) = segs.last()
                && DENIED_MACROS.contains(&last.as_str())
                && matches!(flat.get(j + 1), Some(TokenTree::Punct(p)) if p.as_char() == '!')
            {
                self.flag(err_at("macro", format!("`{last}!` is not allowed"), id.span()));
            }
            if denied_path(&segs) {
                self.flag(err_at(
                    "forbidden_path",
                    format!("path `{}` is not allowed", segs.join("::")),
                    id.span(),
                ));
            }
            i = j + 1;
        }
    }
}

fn use_root(tree: &syn::UseTree) -> Option<String> {
    match tree {
        syn::UseTree::Path(p) => Some(ident_name(&p.ident)),
        syn::UseTree::Name(n) => Some(ident_name(&n.ident)),
        syn::UseTree::Rename(r) => Some(ident_name(&r.ident)),
        syn::UseTree::Group(g) => g.items.iter().find_map(use_root),
        syn::UseTree::Glob(_) => None,
    }
}

/// Every leaf of a `use` tree as the path it imports, the original name
/// last: a rename imports that name under another, so `use core::{include_str
/// as inc, fmt::Write as _}` gives `core::include_str` and `core::fmt::Write`.
/// A glob names no leaf; what it brings in is checked where it is called.
fn use_leaves(tree: &syn::UseTree, prefix: &mut Vec<String>, out: &mut Vec<Vec<String>>) {
    match tree {
        syn::UseTree::Path(p) => {
            prefix.push(ident_name(&p.ident));
            use_leaves(&p.tree, prefix, out);
            prefix.pop();
        }
        syn::UseTree::Name(n) => {
            let mut segs = prefix.clone();
            segs.push(ident_name(&n.ident));
            out.push(segs);
        }
        syn::UseTree::Rename(r) => {
            let mut segs = prefix.clone();
            segs.push(ident_name(&r.ident));
            out.push(segs);
        }
        syn::UseTree::Group(g) => {
            for item in &g.items {
                use_leaves(item, prefix, out);
            }
        }
        syn::UseTree::Glob(_) => {}
    }
}

/// The name of an identifier with its raw prefix stripped. proc-macro2
/// prints the raw spelling as `r#include_str`, and the compiler resolves it
/// as `include_str` (checked with rustc 1.98.1 on 2026-10-31:
/// `r#include_str!("/etc/hostname")` read the file), so every comparison
/// with a denylist reads the identifier through this.
fn ident_name(id: &proc_macro2::Ident) -> String {
    let s = id.to_string();
    match s.strip_prefix("r#") {
        Some(plain) => plain.to_string(),
        None => s,
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
    fn inject_helper_use_and_call_allowed() {
        // `inject` is scaffold-shipped like `html_text`: a `use` and a
        // bare call both pass the denylist.
        let src = r##"
// guardian-tool: scrub
const GUARDIAN_NAME: &str = "scrub";
const GUARDIAN_DESC: &str = "Scrub.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"t":{"type":"string"}}}"#;
use inject::redact;
fn run(input: &str) -> String {
    let (clean, flagged) = redact(input);
    let seen = inject::flagged(input);
    let (scrubbed, found) = inject::scrub(input);
    let names = inject::scan(input);
    format!("{clean} {flagged} {seen} {scrubbed} {} {}", found.len(), names.len())
}
"##;
        validate(src, &[]).expect("inject use and call pass");
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
            // Inside a macro invocation, which `syn` does not parse (#12).
            ("unsafe", "fn run(input: &str) -> String { let _ = vec![unsafe { core::ptr::read(0 as *const u8) }]; String::new() }"),
            ("forbidden_path", "fn run(input: &str) -> String { let _ = vec![std::fs::read(\"/x\")]; String::new() }"),
            ("macro", "fn run(input: &str) -> String { format!(\"{}\", include_str!(\"/etc/passwd\")) }"),
            ("forbidden_path", "fn run(input: &str) -> String { let _ = format!(\"{}\", std::env::var(\"HOME\").unwrap()); String::new() }"),
            // The edition-2024 spelling of an ABI attribute.
            ("attribute", "#[unsafe(no_mangle)] fn run(input: &str) -> String { String::new() }"),
            ("attribute", "#[unsafe(export_name = \"galloc\")] fn run(input: &str) -> String { String::new() }"),
            // A path-qualified denied macro inside another invocation: the
            // token scan used to step past the name with the path
            // (the verification of 2026-10-24). `core` re-exports them all.
            ("macro", "fn run(input: &str) -> String { format!(\"{}\", core::include_str!(\"/embra/state/api_key_anthropic\")) }"),
            ("macro", "fn run(input: &str) -> String { format!(\"{}\", core::env!(\"GITHUB_TOKEN\")) }"),
            ("macro", "fn run(input: &str) -> String { format!(\"{:?}\", core::option_env!(\"HOME\")) }"),
            ("macro", "fn run(input: &str) -> String { let _ = vec![::core::include_bytes!(\"/x\")]; String::new() }"),
            // `cfg_attr` carries an attribute the walker never sees as one.
            ("attribute", "#[cfg_attr(all(), no_mangle)] fn run(input: &str) -> String { String::new() }"),
            ("attribute", "#[cfg_attr(all(), unsafe(no_mangle))] fn run(input: &str) -> String { String::new() }"),
            ("attribute", "#[cfg_attr(all(), export_name = include_str!(\"/x\"))] fn run(input: &str) -> String { String::new() }"),
            // A denied macro inside any other list attribute's tokens.
            ("macro", "#[doc(alias = include_str!(\"/x\"))] fn run(input: &str) -> String { String::new() }"),
            // A declared macro's body is never parsed into items; the
            // declaration itself is refused.
            ("macro", "macro_rules! smuggle { () => { extern \"C\" { fn syscall(); } } }\nfn run(input: &str) -> String { smuggle!(); String::new() }"),
            // A raw-identifier spelling resolves as the plain one and
            // compiled, reading the file (rustc 1.98.1, 2026-10-31); the
            // walker compares every name unraw'd.
            ("macro", "fn run(input: &str) -> String { r#include_str!(\"/etc/hostname\").into() }"),
            ("macro", "fn run(input: &str) -> String { format!(\"{}\", core::r#include_str!(\"/embra/state/api_key_anthropic\")) }"),
            ("macro", "fn run(input: &str) -> String { let _ = vec![::core::r#include_bytes!(\"/x\")]; String::new() }"),
            ("macro", "fn run(input: &str) -> String { r#global_asm!(\"nop\"); String::new() }"),
            ("forbidden_path", "fn run(input: &str) -> String { r#std::fs::read(\"/x\"); String::new() }"),
            ("attribute", "#[r#cfg_attr(all(), no_mangle)] fn run(input: &str) -> String { String::new() }"),
            ("attribute", "#[r#no_mangle] fn run(input: &str) -> String { String::new() }"),
            // A denied macro imported under another name is called by that
            // name (`inc!`), so the import itself is refused, at the top
            // level, in a group and inside a body; a denied path too.
            ("macro", "use core::include_str as inc;\nfn run(input: &str) -> String { inc!(\"/x\").into() }"),
            ("macro", "use core::{include_str as inc, fmt::Write as _};\nfn run(input: &str) -> String { inc!(\"/x\").into() }"),
            ("macro", "fn run(input: &str) -> String { use core::include_bytes as b; let _ = b!(\"/x\"); String::new() }"),
            ("macro", "use core::r#include_str as inc;\nfn run(input: &str) -> String { inc!(\"/x\").into() }"),
            ("forbidden_path", "use core::arch as a;\nfn run(input: &str) -> String { String::new() }"),
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

    /// The token scan refuses what a statement would refuse and nothing
    /// else: ordinary macro arguments, `alloc` and `core` paths, the
    /// scaffold's `host` and `json` helpers and a renamed import of an
    /// ordinary item (`use core::fmt::Write as _`) still pass.
    #[test]
    fn a_macro_with_ordinary_arguments_still_passes() {
        let src = "// guardian-tool: demo\nconst GUARDIAN_NAME: &str=\"demo\";\
                   const GUARDIAN_DESC: &str=\"d\";\
                   const GUARDIAN_SCHEMA: &str=r#\"{\"type\":\"object\"}\"#;\n\
                   use core::fmt::Write as _;\nuse alloc::{format, string::String as S};\n\
                   fn run(input: &str) -> String { let v = vec![1, 2]; let s = alloc::format!(\"{} {}\", v.len(), core::cmp::max(1, 2)); let _: S = S::new(); format!(\"{s} {input}\") }";
        let m = validate(src, &[]).expect("ordinary macro arguments pass");
        assert_eq!(m.name, "demo");
    }

    /// Ordinary attributes keep passing: the scan of a list attribute's
    /// tokens refuses what a statement would refuse and nothing else.
    #[test]
    fn an_ordinary_attribute_still_passes() {
        let src = "// guardian-tool: demo\nconst GUARDIAN_NAME: &str=\"demo\";\
                   const GUARDIAN_DESC: &str=\"d\";\
                   const GUARDIAN_SCHEMA: &str=r#\"{\"type\":\"object\"}\"#;\n\
                   #[inline] fn helper(n: usize) -> usize { n }\n\
                   #[allow(dead_code, clippy::needless_return)] fn unused() -> usize { return 1; }\n\
                   #[cfg(target_arch = \"wasm32\")] fn arch() -> &'static str { \"wasm32\" }\n\
                   #[doc = \"documented\"] fn run(input: &str) -> String { format!(\"{} {}\", helper(input.len()), unused()) }";
        let m = validate(src, &[]).expect("ordinary attributes pass");
        assert_eq!(m.name, "demo");
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
