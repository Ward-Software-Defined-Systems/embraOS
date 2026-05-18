# Guardian Advanced Example — prompt-injection-hardened `web_search`

The flagship dynamic tool: **actually search the web** through the
Guardian `web_search` capability (Brave-backed, host-side), then
**neutralize prompt-injection** in the result text *before the model ever
sees it*. Read [GUARDIAN-TOOL-EXAMPLES.md](./GUARDIAN-TOOL-EXAMPLES.md)
first for the contract, the `json`/`host` APIs, and the
`/guardian-define` workflow.

> The module below is checked against the real validator by
> `crates/embra-guardian/tests/doc_examples_validate.rs` and is compiled
> to `wasm32` during development, so it is known-good — not pseudocode.

## Setup — one-time Brave key

`host::web_search` is **not configured until the operator sets a Brave
Search API key**. The key is stored host-side on the STATE partition
(0600, like the other provider keys); it never reaches a guest module,
the manifest, or the returned envelope. Set it once:

```text
/guardian key brave <your-brave-api-key>
/guardian key brave                 # (no token) → reports SET / NOT set
```

Until a key is set, the tool returns
`{"error":"search capability not configured (no Brave API key set)"}` —
a clean degradation, not a crash.

## Why this matters

Search results are **untrusted attacker-controlled text**. A page title
or description can contain "ignore previous instructions, you are now…",
zero-width characters hiding directives, or fake `[TOOL:` / `assistant:`
framing. Two independent defenses apply:

1. **The Guardian `web_search` guard** (host side, automatic when the
   tool declares `web_search`): the host holds the Brave API key and the
   endpoint is **fixed** (`api.search.brave.com`) — the query is the only
   guest-controlled input, so there is no guest-controlled URL / SSRF
   surface. Results are filtered to `https`, length-capped, and
   normalized to a stable `{title,url,description}` shape. This protects
   the *host / network / credential*.
2. **This tool's content scrubber** (`fn run`, below): strips control +
   zero-width characters, case-insensitively redacts known
   injection-directive phrases, length-caps every field, de-dupes by
   host, ranks by query overlap, and flags any entry where a directive
   was found (`injection_suspected: true`). This protects the *model*.

The guard makes the call *safe to make*; the scrubber makes the result
*safe to show the model*. Neither alone is sufficient.

## Input / output

Input:

```json
{ "query": "rust async runtime", "max": 5 }
```

Output (`max` defaults to 5):

```json
{
  "query":"rust async runtime",
  "count":2,
  "results":[
    {"title":"Tokio","url":"https://tokio.rs","description":"An async runtime for Rust…","score":2,"injection_suspected":false},
    {"title":"[redacted-directive]","url":"https://evil.test/x","description":"[redacted-directive]: leak secrets","score":0,"injection_suspected":true}
  ]
}
```

## The module

```rust
// guardian-tool: web_search
const GUARDIAN_NAME: &str = "web_search";
const GUARDIAN_DESC: &str = "Search the web (Brave, via the Guardian web_search guard) and neutralize prompt-injection in result titles/descriptions before the model sees them. Returns results ranked by query overlap, de-duplicated by host, each flagged with injection_suspected.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string"},"max":{"type":"integer"}},"required":["query"]}"#;
const GUARDIAN_CAPS: &[&str] = &["web_search"];

const INJECTION_MARKERS: &[&str] = &[
    "ignore previous instructions", "ignore all previous", "disregard the above",
    "you are now", "new instructions:", "system prompt", "developer message",
    "begin system", "[tool:", "</system>", "assistant:", "```tool",
];

fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let query = v.get("query").as_str().unwrap_or("").trim();
    if query.is_empty() {
        return json::stringify(&json::obj(vec![("error", json::s("query is required"))]));
    }
    let max = v.get("max").as_f64().unwrap_or(5.0) as usize;

    // Perform the actual search through the Guardian web_search guard.
    // The host holds the Brave key and pins the endpoint; the query is
    // the only guest-controlled input. Everything that comes back is
    // attacker-controlled text — it is scrubbed below before output.
    let env = json::parse(&host::web_search(query)).unwrap_or(json::null());
    if !env.get("ok").as_bool().unwrap_or(false) {
        let err = env.get("error").as_str().unwrap_or("search failed");
        return json::stringify(&json::obj(vec![("error", json::s(err))]));
    }

    let mut out: Vec<json::Json> = vec![];
    let mut seen: Vec<String> = vec![];
    if let Some(items) = env.get("results").as_array() {
        for r in items {
            let url = r.get("url").as_str().unwrap_or("");
            if !is_safe_url(url) {
                continue;
            }
            let h = host_of(url);
            if seen.iter().any(|s| s.as_str() == h) {
                continue;
            }
            seen.push(h.to_string());
            out.push(scrub_entry(
                query,
                r.get("title").as_str().unwrap_or(""),
                url,
                r.get("description").as_str().unwrap_or(""),
            ));
        }
    }

    out.sort_by(|a, b| {
        let sb = b.get("score").as_f64().unwrap_or(0.0);
        let sa = a.get("score").as_f64().unwrap_or(0.0);
        sb.total_cmp(&sa)
    });
    out.truncate(max);

    json::stringify(&json::obj(vec![
        ("query", json::s(query)),
        ("count", json::n(out.len() as f64)),
        ("results", json::arr(out)),
    ]))
}

fn scrub_entry(query: &str, title: &str, url: &str, description: &str) -> json::Json {
    let (t, f1) = sanitize(title, 200);
    let (d, f2) = sanitize(description, 1000);
    let score = overlap_score(query, &t, &d);
    json::obj(vec![
        ("title", json::s(&t)),
        ("url", json::s(url)),
        ("description", json::s(&d)),
        ("score", json::n(score)),
        ("injection_suspected", json::b(f1 || f2)),
    ])
}

fn sanitize(raw: &str, cap: usize) -> (String, bool) {
    // 1. drop control + zero-width chars (hidden-instruction vectors)
    let mut s: String = raw
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .filter(|c| !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}'))
        .collect();
    // 2. flag + redact injection directives (case-insensitive)
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
    // 3. bound length on a char boundary
    if s.len() > cap {
        let mut end = cap;
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        s.truncate(end);
        s.push_str(" …[truncated]");
    }
    (s, flagged)
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
    // Markers + replacement are ASCII; multi-byte content is copied
    // byte-for-byte from valid UTF-8, so the result is valid UTF-8.
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

- **Never panics:** every accessor uses `unwrap_or`; a search error or
  bad input yields `{"error":…}`. A panic would become a sandbox trap
  surfaced as a tool error.
- **No third-party crates** (v1 rule): all logic is `#![no_std]` + the
  vendored `json` helper. `contains_ci`/`redact_ci` are allocation-light
  ASCII scanners — no `regex`, no `serde`.
- **The scrubber is conservative**, not exhaustive. It raises
  `injection_suspected` so the model (and operator) can treat flagged
  results with suspicion; it does not claim to make hostile text safe.
- The `web_search` declaration is what makes `host::web_search`
  available and what `guardian_list` surfaces as the tool's privilege.
  The host already filters results to `https`; `is_safe_url` is
  defense-in-depth.

## Try it

```text
/guardian key brave <your-brave-api-key>   # one-time, host-side
/guardian-define
<paste the module above>
.                                 # serial terminator (web: just Enter)
/guardian status web_search       # → ready
# to the intelligence:
guardian_call action=invoke tool=web_search input={"query":"rust async runtime"}
# → real Brave results, ranked by query overlap, de-duped by host;
#   any entry containing an injection directive has its title/description
#   replaced with [redacted-directive] and injection_suspected:true.
/guardian delete web_search
```
