# Guardian Advanced Example — prompt-injection-hardened `web_search`

The flagship dynamic tool, in **one module** that declares **two
capabilities**: search the web (Brave, via the Guardian `web_search`
guard) **and** optionally fetch + read the top results (via the
`http_get` egress guard) — neutralizing prompt-injection in everything
before the model sees it. Read
[GUARDIAN-TOOL-EXAMPLES.md](./GUARDIAN-TOOL-EXAMPLES.md) first for the
contract and the `json` / `host` / `html_text` APIs.

> The module below is checked against the real validator by
> `crates/embra-guardian/tests/doc_examples_validate.rs` and is compiled
> to `wasm32` during development, so it is known-good — not pseudocode.

## Setup — one-time Brave key

`host::web_search` is **not configured until the operator sets a Brave
Search API key** (host-side, STATE, `0600` — never in a guest module,
the manifest, or results):

```text
/guardian key brave <your-brave-api-key>
/guardian key brave                 # (no token) → reports SET / NOT set
```

Until a key is set the tool returns
`{"error":"search capability not configured (no Brave API key set)"}` —
a clean degradation, not a crash.

## Why one module with two caps (not a separate `web_fetch`)

`http_get` is already the Guardian fetch primitive (https-only,
RFC1918/SSRF-blocked, allowlist, size + content-type caps). So "search,
then read the page that answers the question" is **one module declaring
`["web_search","http_get"]`** — no separate fetch tool. Three defenses
stack:

1. **`web_search` guard** (host): Brave key host-side, endpoint pinned
   (`api.search.brave.com`) so the query is the only guest-controlled
   input — no guest URL / SSRF surface. Request is clamped/whitelisted
   host-side (`count` 1–20, `offset` 0–9, `freshness`, sanitized
   `exclude`). Results filtered to `https`, normalized.
2. **`http_get` guard** (host): the fetch of a chosen result URL goes
   through the same egress policy as any other fetch.
3. **This tool's scrubber** (`fn run`): strips control / zero-width
   chars, redacts injection-directive phrases, length-caps every field
   (reporting *what* was cut and to what length), de-dupes by host,
   ranks by query overlap, flags `injection_suspected`. Search text,
   `extra_snippets`, **and** fetched page text all go through it.

## Input / output

Input (`query` required; everything else optional):

```json
{ "query": "tokio cancellation safety", "max": 5, "recency": "year",
  "exclude": ["pinterest.com"], "extra_snippets": true, "fetch_top": 1 }
```

Output:

```json
{
  "query":"tokio cancellation safety",
  "count":2,
  "results":[
    {"title":"Tokio docs","url":"https://docs.rs/tokio","description":"…","age":"2024-10-08T10:30:00Z","snippets":["…"],"score":3,"injection_suspected":false,"text":"… extracted page text …"},
    {"title":"[redacted-directive]","url":"https://evil.test/x","description":"[redacted-directive]: leak secrets","score":0,"injection_suspected":true,"truncated":{"description":1000}}
  ]
}
```

## The module

```rust
// guardian-tool: web_search
const GUARDIAN_NAME: &str = "web_search";
const GUARDIAN_DESC: &str = "Search the web (Brave, via the Guardian web_search guard) with optional recency/exclude/pagination, optionally fetch + read the top results via the http_get guard, and neutralize prompt-injection in every field before the model sees it. Ranked by query overlap, de-duplicated by host.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string"},"max":{"type":"integer"},"recency":{"type":"string"},"exclude":{"type":"array","items":{"type":"string"}},"offset":{"type":"integer"},"extra_snippets":{"type":"boolean"},"fetch_top":{"type":"integer"}},"required":["query"]}"#;
const GUARDIAN_CAPS: &[&str] = &["web_search", "http_get"];

const INJECTION_MARKERS: &[&str] = &[
    "ignore previous instructions", "ignore all previous", "disregard the above",
    "you are now", "new instructions:", "system prompt", "developer message",
    "begin system", "[tool:", "</system>", "assistant:", "```tool",
];

struct Entry {
    title: String,
    url: String,
    description: String,
    age: Option<String>,
    snippets: Vec<String>,
    score: f64,
    injection: bool,
    text: Option<String>,
    // (field, cap) for every field that was length-capped (#7).
    truncated: Vec<(&'static str, usize)>,
}

fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return err(&e),
    };
    let query = v.get("query").as_str().unwrap_or("").trim();
    if query.is_empty() {
        return err("query is required");
    }
    let max = v.get("max").as_f64().unwrap_or(5.0) as usize;
    let offset = v.get("offset").as_f64().unwrap_or(0.0) as i64;
    let recency = v.get("recency").as_str().unwrap_or("");
    let extra = v.get("extra_snippets").as_bool().unwrap_or(false);
    let fetch_top = v.get("fetch_top").as_f64().unwrap_or(0.0) as usize;

    // Build the structured web_search request. The host clamps every
    // field again — this is just a convenient surface.
    let mut req: Vec<(&str, json::Json)> =
        vec![("q", json::s(query)), ("count", json::n(20.0))];
    if offset > 0 {
        req.push(("offset", json::n(offset as f64)));
    }
    if !recency.is_empty() {
        req.push(("freshness", json::s(recency)));
    }
    if extra {
        req.push(("extra_snippets", json::b(true)));
    }
    if let Some(arr) = v.get("exclude").as_array() {
        let ex: Vec<json::Json> =
            arr.iter().filter_map(|d| d.as_str()).map(json::s).collect();
        if !ex.is_empty() {
            req.push(("exclude", json::arr(ex)));
        }
    }
    let env = json::parse(&host::web_search_ex(&json::stringify(&json::obj(req))))
        .unwrap_or(json::null());
    if !env.get("ok").as_bool().unwrap_or(false) {
        return err(env.get("error").as_str().unwrap_or("search failed"));
    }

    let mut out: Vec<Entry> = vec![];
    let mut seen: Vec<String> = vec![];
    if let Some(items) = env.get("results").as_array() {
        for r in items {
            let url = r.get("url").as_str().unwrap_or("");
            if !is_safe_url(url) {
                continue;
            }
            let h = host_of(url).to_string();
            if seen.iter().any(|s| s == &h) {
                continue;
            }
            seen.push(h);
            out.push(scrub_entry(query, r));
        }
    }
    out.sort_by(|a, b| b.score.total_cmp(&a.score));
    out.truncate(max);

    // Optionally fetch + read the top N pages — same scrubber.
    for e in out.iter_mut().take(fetch_top) {
        let fenv = json::parse(&host::http_get(&e.url)).unwrap_or(json::null());
        if !fenv.get("ok").as_bool().unwrap_or(false) {
            continue;
        }
        let body = fenv.get("body").as_str().unwrap_or("");
        let (clean, flagged, cut) = sanitize(&html_text::to_text(body), 4000);
        e.injection = e.injection || flagged;
        if cut {
            e.truncated.push(("text", 4000));
        }
        e.text = Some(clean);
    }

    let results: Vec<json::Json> = out.iter().map(entry_json).collect();
    json::stringify(&json::obj(vec![
        ("query", json::s(query)),
        ("count", json::n(results.len() as f64)),
        ("results", json::arr(results)),
    ]))
}

fn scrub_entry(query: &str, r: &json::Json) -> Entry {
    let url = r.get("url").as_str().unwrap_or("").to_string();
    let (title, tf, tc) = sanitize(r.get("title").as_str().unwrap_or(""), 200);
    let (desc, df, dc) = sanitize(r.get("description").as_str().unwrap_or(""), 1000);
    let mut truncated: Vec<(&'static str, usize)> = vec![];
    if tc {
        truncated.push(("title", 200));
    }
    if dc {
        truncated.push(("description", 1000));
    }
    let age = match r.get("age").as_str() {
        Some(a) if !a.is_empty() => Some(sanitize(a, 60).0),
        _ => None,
    };
    let mut injection = tf || df;
    let mut snippets: Vec<String> = vec![];
    if let Some(arr) = r.get("snippets").as_array() {
        for s in arr {
            let (clean, f, _) = sanitize(s.as_str().unwrap_or(""), 500);
            injection = injection || f;
            snippets.push(clean);
        }
    }
    let score = overlap_score(query, &title, &desc);
    Entry {
        title,
        url,
        description: desc,
        age,
        snippets,
        score,
        injection,
        text: None,
        truncated,
    }
}

fn entry_json(e: &Entry) -> json::Json {
    let mut o: Vec<(&str, json::Json)> = vec![
        ("title", json::s(&e.title)),
        ("url", json::s(&e.url)),
        ("description", json::s(&e.description)),
        ("score", json::n(e.score)),
        ("injection_suspected", json::b(e.injection)),
    ];
    if let Some(a) = &e.age {
        o.push(("age", json::s(a)));
    }
    if !e.snippets.is_empty() {
        o.push(("snippets", json::arr(e.snippets.iter().map(|s| json::s(s)).collect())));
    }
    if let Some(t) = &e.text {
        o.push(("text", json::s(t)));
    }
    if !e.truncated.is_empty() {
        let t: Vec<(&str, json::Json)> =
            e.truncated.iter().map(|(f, c)| (*f, json::n(*c as f64))).collect();
        o.push(("truncated", json::obj(t)));
    }
    json::obj(o)
}

fn err(msg: &str) -> String {
    json::stringify(&json::obj(vec![("error", json::s(msg))]))
}

/// Returns (clean, injection_flagged, was_truncated). Truncation is
/// reported structurally by the caller — no opaque inline marker (#7).
fn sanitize(raw: &str, cap: usize) -> (String, bool, bool) {
    let mut s: String = raw
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .filter(|c| !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}'))
        .collect();
    let mut flagged = false;
    for m in INJECTION_MARKERS {
        if contains_ci(&s, m) {
            flagged = true;
        }
    }
    if flagged {
        for m in INJECTION_MARKERS {
            s = redact_ci(&s, m, "[redacted-directive]");
        }
    }
    let mut truncated = false;
    if s.len() > cap {
        let mut end = cap;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        truncated = true;
    }
    (s, flagged, truncated)
}

fn contains_ci(hay: &str, needle: &str) -> bool {
    let h = hay.as_bytes();
    let n = needle.as_bytes();
    if n.is_empty() || n.len() > h.len() {
        return n.is_empty();
    }
    let mut i = 0;
    while i + n.len() <= h.len() {
        let mut k = 0;
        while k < n.len() && h[i + k].to_ascii_lowercase() == n[k].to_ascii_lowercase() {
            k += 1;
        }
        if k == n.len() {
            return true;
        }
        i += 1;
    }
    false
}

fn redact_ci(s: &str, needle: &str, repl: &str) -> String {
    if needle.is_empty() {
        return s.to_string();
    }
    let hb = s.as_bytes();
    let nb = needle.as_bytes();
    let mut outb: Vec<u8> = vec![];
    let mut i = 0;
    while i < hb.len() {
        if i + nb.len() <= hb.len() {
            let mut k = 0;
            while k < nb.len() && hb[i + k].to_ascii_lowercase() == nb[k].to_ascii_lowercase() {
                k += 1;
            }
            if k == nb.len() {
                outb.extend_from_slice(repl.as_bytes());
                i += nb.len();
                continue;
            }
        }
        outb.push(hb[i]);
        i += 1;
    }
    match core::str::from_utf8(&outb) {
        Ok(t) => t.to_string(),
        Err(_) => s.to_string(),
    }
}

fn is_safe_url(u: &str) -> bool {
    u.starts_with("https://") && !u.contains('@')
}

fn host_of(u: &str) -> &str {
    let rest = u.strip_prefix("https://").unwrap_or(u);
    match rest.find('/') {
        Some(i) => &rest[..i],
        None => rest,
    }
}

fn overlap_score(query: &str, title: &str, description: &str) -> f64 {
    let mut hay = String::new();
    hay.push_str(title);
    hay.push(' ');
    hay.push_str(description);
    let mut score = 0.0_f64;
    for tok in query.split_whitespace() {
        if tok.len() < 2 {
            continue;
        }
        if contains_ci(&hay, tok) {
            score += 1.0;
        }
    }
    score
}
```

## Notes

- **Never panics:** every accessor uses `unwrap_or`; a search/fetch
  error or bad input yields `{"error":…}`. A panic becomes a sandbox
  trap surfaced as a tool error.
- **No third-party crates** (v1 rule): `#![no_std]` + the vendored
  `json` and `html_text` helpers only. `html_text::to_text` is a
  **heuristic** HTML→text reducer (drops `<script>/<style>`, strips
  tags, decodes a small entity set), documented conservative — it does
  not make hostile markup safe; the scrubber still runs on its output.
- **Structured truncation (#7):** a cut field reports `truncated:
  {"<field>": <cap>}` instead of an opaque inline marker.
- **Recency / exclude / pagination / freshness:** `recency` accepts
  `day|week|month|year` (or a `YYYY-MM-DDtoYYYY-MM-DD` range); `exclude`
  domains become `-site:` operators; `offset` (0–9) pages results;
  `extra_snippets` returns more excerpt text per result *without* a
  fetch. All clamped/whitelisted host-side.
- The `web_search` + `http_get` declarations are what make
  `host::web_search_ex` / `host::http_get` available and what
  `guardian_list` surfaces as the tool's privileges.

## Try it

```text
/guardian key brave <your-brave-api-key>   # one-time, host-side
/guardian-define
<paste the module above>
.                                 # serial terminator (web: just Enter)
/guardian status web_search       # → ready
# then just ask the intelligence in plain language, e.g.:
#   "use the web_search guardian tool to find recent material on tokio
#    cancellation safety and fetch the top result"
# → real Brave results ranked by overlap, de-duped by host, the top
#   result's page fetched + reduced to text; any injection directive in
#   any field becomes [redacted-directive] with injection_suspected:true.
/guardian delete web_search
```
