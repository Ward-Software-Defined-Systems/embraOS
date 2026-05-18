# Guardian Tool Examples (embra-guardian-v1)

Paste-ready dynamic-tool modules for testing and as a starting point for
the intelligence. Every fenced `rust` module below is checked against the
real validator by `crates/embra-guardian/tests/doc_examples_validate.rs`,
so what's here is exactly what the gate accepts.

> Status: **experimental** (branch `embra-guardian-v1`). The in-OS
> toolchain (`/opt/rust`) must be present — build the image with
> `BR2_PACKAGE_EMBRA_RUST_TOOLCHAIN=y` (default in the embraOS defconfig).

## How it works

1. In the console (serial **or** web) type `/guardian-define`.
2. Paste a module (the fenced blocks below).
3. Submit: a lone `.` on its own line (serial) or paste-and-Enter (web).
4. It is validated synchronously, then compiled to a `wasm32` sandbox in
   the background. Poll with `/guardian status <name>`.
5. Once `ready`, just ask the intelligence to use the tool, in plain
   language, by its name (e.g. "use the sum guardian tool to add 2 and
   40"). It invokes the dynamic tool itself through internal meta-tools
   — you never type tool-call syntax.
6. Manage: `/guardian list`, `/guardian show <name>`,
   `/guardian delete <name>`.

## The contract

You paste **only** these items — the scaffold owns everything else
(`#![no_std]`, the allocator, the panic handler, the ABI exports, and the
`json` / `host` modules):

- A first marker line: `// guardian-tool: <name>`
  (`name` is `^[a-z][a-z0-9_]{2,40}$`, ≥3 chars, and must not collide
  with a built-in tool name nor `guardian_call`/`guardian_list` — the
  validator rejects with a precise message if it does).
- `const GUARDIAN_NAME: &str = "<name>";` (must equal the marker)
- `const GUARDIAN_DESC: &str = "…";`
- `const GUARDIAN_SCHEMA: &str = r#"{ "type":"object", "properties":{…} }"#;`
- *(optional)* `const GUARDIAN_CAPS: &[&str] = &["http_get"];`
  (v1 capabilities: `"http_get"`, `"web_search"` — declare only what the
  tool uses; the validator rejects an undeclared `host::` reference)
- Exactly one `fn run(input: &str) -> String` (not `pub`, not `unsafe`).
- Any number of **private** helper `fn`s.

Forbidden in the paste (validator-enforced): `unsafe`, `extern`/FFI,
`std::*` / `core::arch` / `proc_macro`, `use` of anything outside
`core`/`alloc`/`json`/`host`/`html_text`, `include!`/`env!`/`asm!`-style macros,
`mod`, `pub` free items, `#[no_mangle]`/`#[link]`/runtime attrs, and any
third-party crate dependency (v1 guests are dependency-free). `vec![]`
and `format!` are available.

`run` must never panic — a panic becomes a sandbox trap reported as a
tool error. Parse defensively (`unwrap_or`, return an `{"error":…}`
object).

### Provided `json` API (vendored, zero-dep)

```text
json::parse(&str) -> Result<Json, String>
Json::get(key: &str) -> &Json     // object field; &Null if missing
Json::idx(i: usize)  -> &Json     // array element; &Null if out of range
Json::as_str()   -> Option<&str>
Json::as_f64()   -> Option<f64>
Json::as_bool()  -> Option<bool>
Json::as_array() -> Option<&[Json]>
Json::is_null()  -> bool
json::stringify(&Json) -> String
builders: json::s(&str)  json::n(f64)  json::b(bool)  json::null()
          json::arr(Vec<Json>)  json::obj(Vec<(&str, Json)>)
```

### Provided `host` API (each fn appears only when its cap is declared)

`host::http_get` — when `GUARDIAN_CAPS` includes `"http_get"`:

```text
host::http_get(url: &str) -> String   // returns a JSON envelope string:
//   {"ok":true,"status":<u16>,"url":"…","content_type":"…","body":"…"}
//   {"ok":false,"error":"…"}
// The Guardian enforces: https-only, RFC1918/loopback/CGNAT/IPv6 +
// DNS-resolved-IP SSRF block, optional domain allowlist, 10s timeout,
// 256 KiB body cap, text/* | application/json content-types. Audited.
```

`host::web_search` — when `GUARDIAN_CAPS` includes `"web_search"`:

```text
host::web_search(query: &str) -> String
host::web_search_ex(request_json: &str) -> String   // structured form
// Envelope:
//   {"ok":true,"query":"…","count":<n>,"results":[
//      {"title":"…","url":"https://…","description":"…",
//       "age":"<provider date, when present>",
//       "snippets":["<extra excerpts, when requested>", …]}, …],
//    "infobox":{…}}        // entity card, present ONLY for entity-type
//                          // queries; best-effort / provider-defined;
//                          // omitted when absent
//   {"ok":false,"error":"…"}
//
// `web_search(q)` = a bare query. `web_search_ex(json)` takes a JSON
// object — all fields clamped / whitelisted host-side:
//   {"q":"…",                       // required
//    "count": 1..=20,               // default 10
//    "offset": 0..=9,               // page (default 0)
//    "freshness": "day"|"week"|"month"|"year"
//                 |"YYYY-MM-DDtoYYYY-MM-DD",
//    "exclude": ["host", …],        // become -site: operators in q
//    "extra_snippets": true|false}  // more excerpt text, no fetch
//
// Backed by Brave Search; the host holds the API key (never reaches the
// guest) and the endpoint is fixed (api.search.brave.com) so there is no
// guest-controlled URL / SSRF surface. Results filtered to https,
// length-capped, normalized. `age` and `infobox` are best-effort
// (provider-defined, omitted when absent; `infobox` only for entity
// queries). (Brave's `summarizer` web-response key is only an opaque
// pointer to a deprecated separate endpoint, so it is intentionally not
// surfaced.) Text/infobox are attacker-controlled — the tool MUST
// injection-scrub them (see GUARDIAN-ADVANCED-EXAMPLE.md). The key is
// set by the operator: `/guardian key brave <token>`; if unset the
// envelope is {"ok":false,"error":"search capability not configured…"}.
```

### Provided `html_text` API (always available, zero-dep)

```text
html_text::to_text(html: &str) -> String
// Heuristic HTML→text reducer for #![no_std] guests: drops
// <script>/<style> bodies, strips tags, decodes a minimal entity set,
// collapses whitespace. NOT a parser and NOT a sanitizer — pair it with
// http_get to make a fetched page model-readable, then injection-scrub
// the result (it is attacker-controlled). See GUARDIAN-ADVANCED-EXAMPLE.md.
```

## Minimal template

```rust
// guardian-tool: my_tool
const GUARDIAN_NAME: &str = "my_tool";
const GUARDIAN_DESC: &str = "One-line description of what this tool does.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"input":{"type":"string"}}}"#;
fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let _ = v.get("input").as_str().unwrap_or("");
    json::stringify(&json::obj(vec![("ok", json::b(true))]))
}
```

## Example 1 — `echo` (pure compute)

`{"message":"hi"}` → `{"echo":"hi"}`

```rust
// guardian-tool: echo
const GUARDIAN_NAME: &str = "echo";
const GUARDIAN_DESC: &str = "Echo the input message back under an \"echo\" key.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"message":{"type":"string"}},"required":["message"]}"#;
fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let msg = v.get("message").as_str().unwrap_or("");
    json::stringify(&json::obj(vec![("echo", json::s(msg))]))
}
```

## Example 2 — `sum` (numbers)

`{"a":2,"b":40}` → `{"sum":42}`

```rust
// guardian-tool: sum
const GUARDIAN_NAME: &str = "sum";
const GUARDIAN_DESC: &str = "Add two numbers a + b.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"a":{"type":"number"},"b":{"type":"number"}},"required":["a","b"]}"#;
fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let a = v.get("a").as_f64().unwrap_or(0.0);
    let b = v.get("b").as_f64().unwrap_or(0.0);
    json::stringify(&json::obj(vec![("sum", json::n(a + b))]))
}
```

## Example 3 — `word_count` (string processing + a private helper)

`{"text":"one two three"}` → `{"words":3,"chars":13,"lines":1}`

```rust
// guardian-tool: word_count
const GUARDIAN_NAME: &str = "word_count";
const GUARDIAN_DESC: &str = "Count words, characters, and lines in `text`.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"text":{"type":"string"}},"required":["text"]}"#;
fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let text = v.get("text").as_str().unwrap_or("");
    json::stringify(&json::obj(vec![
        ("words", json::n(count_words(text) as f64)),
        ("chars", json::n(text.chars().count() as f64)),
        ("lines", json::n(text.lines().count().max(1) as f64)),
    ]))
}
fn count_words(s: &str) -> usize {
    s.split_whitespace().filter(|w| !w.is_empty()).count()
}
```

## Example 4 — `http_fetch` (capability: Guardian-mediated network)

Declares the `http_get` capability. `{"url":"https://example.com","max":200}`
→ `{"status":200,"bytes":1256,"snippet":"…"}`

```rust
// guardian-tool: http_fetch
const GUARDIAN_NAME: &str = "http_fetch";
const GUARDIAN_DESC: &str = "Fetch an https URL via the Guardian egress guard; returns status and a body snippet.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"url":{"type":"string"},"max":{"type":"integer"}},"required":["url"]}"#;
const GUARDIAN_CAPS: &[&str] = &["http_get"];
fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let url = v.get("url").as_str().unwrap_or("");
    if url.is_empty() {
        return json::stringify(&json::obj(vec![("error", json::s("missing 'url'"))]));
    }
    let max = v.get("max").as_f64().unwrap_or(280.0) as usize;
    // The Guardian validates + fetches; we get back a JSON envelope string.
    let raw = host::http_get(url);
    let env = match json::parse(&raw) {
        Ok(v) => v,
        Err(_) => return raw,
    };
    if env.get("ok").as_bool().unwrap_or(false) {
        let body = env.get("body").as_str().unwrap_or("");
        let snippet: String = body.chars().take(max).collect();
        json::stringify(&json::obj(vec![
            ("status", json::n(env.get("status").as_f64().unwrap_or(0.0))),
            ("bytes", json::n(body.len() as f64)),
            ("snippet", json::s(&snippet)),
        ]))
    } else {
        json::stringify(&json::obj(vec![
            ("error", json::s(env.get("error").as_str().unwrap_or("fetch failed"))),
        ]))
    }
}
```

## Quick test sequence

```text
/guardian-define
<paste Example 2 (sum)>
.                                  ← serial terminator (web: just Enter)
/guardian status sum               ← repeat until: ready
# then just ask the intelligence in plain language, e.g.:
#   "use the sum guardian tool to add 2 and 40"   → it answers 42
/guardian list
/guardian delete sum
```
