# Guardian Example — the shipped `http_request`

A guarded curl. One module that declares **one** capability, `http_request`:
a structured HTTP request through the Guardian guard — any of GET, HEAD,
POST, PUT, PATCH and DELETE, with headers, query pairs and a body or a JSON
value — for a page, a resource, or real API work. Read
[GUARDIAN-TOOL-EXAMPLES.md](./GUARDIAN-TOOL-EXAMPLES.md) first for the
contract and the `json` / `host` / `html_text` / `inject` APIs; the
`host::http_request` API is described there.

**It ships with embraOS**, like [`web_search`](./GUARDIAN-ADVANCED-EXAMPLE.md):
the module below is the shipped source (`crates/embra-guardian/src/shipped/http_request.rs`,
the same bytes as this page, pinned by a test), installed at boot and built in
the background; `/guardian list` shows it as `http_request (shipped)`. It
needs no key.

## What the host does on every call

The tool hands the request to the host as the caller wrote it; the policy
lives on the host:

1. **The egress guard, on every hop** — https only, no userinfo, the optional
   domain allowlist, and the address class: a public address passes; a
   private one only when the operator listed the host with
   `/guardian egress allow <host>` (a name with its subdomains, or an IP
   literal); loopback, link-local, multicast and reserved addresses never.
2. **The operator's secrets, host-side** — `/guardian secret <host> <header>
   <value>` stores a credential; the host adds it to a hop whose host matches
   and to no other, so a redirect to another host never carries it. A tool
   may not set `Authorization`, `Proxy-Authorization` or `Cookie` itself, and
   a token never enters a tool's input, the turn trace or the session history.
3. **Redirects** followed for GET and HEAD only, at most three, every hop
   checked; the other methods get the 3xx back with its target under
   `redirect`. The whole call shares one budget (10 s).
4. **The answer** — status, the URL that answered, the response headers
   (lowercase, no cookies, capped) and the body, for text, JSON, XML, NDJSON
   and form data; a binary body is refused by name. The envelope never
   carries a request header.

## Setup

```text
/guardian egress allow gitlab.ops.wsds                   # a private host, once
/guardian secret gitlab.ops.wsds PRIVATE-TOKEN glpat-…   # a credential, once
/guardian secret                                         # the hosts and header names
```

## Input / output

Input (`url` required; everything else optional):

```json
{ "url": "https://gitlab.ops.wsds/api/v4/projects/2/issues/17",
  "method": "GET", "accept": "application/json",
  "query": { "per_page": 5 }, "timeout_ms": 8000 }
```

```json
{ "url": "https://gitlab.ops.wsds/api/v4/projects/2/issues/17/notes",
  "method": "POST", "json": { "body": "Posted by the intelligence." } }
```

Output:

```json
{ "status": 200, "url": "https://gitlab.ops.wsds/api/v4/projects/2/issues/17",
  "content_type": "application/json",
  "headers": { "content-type": "application/json", "x-request-id": "…" },
  "body": "{\"iid\":17,…}", "injection_suspected": false }
```

`as` chooses the body's form: `auto` (the default) reduces an HTML page to
text and returns anything else as sent, `text` reduces whatever came back,
`raw` never reduces. A reduced page is scrubbed: injection directives and
the structural markers of an injected turn become `[redacted-directive]`
(`inject::redact`), and `injection_suspected` says whether anything was
found. A body returned as sent is left intact and only flagged
(`inject::flagged`): data must stay as it was sent.

`truncated` names a cut: `{"body": N}` when the host cut the body at
`max_bytes` (N bytes kept; default 256 KiB, at most 1 MiB, ask again with a
larger `max_bytes`) or the tool at its own 768 KiB; `{"text": 65536}` when
the reduced text was cut. A body cut mid-way is not the answer, and the
model is told so.

A refused request or a guard error answers `{"error": "…"}` in the guard's
words: the reason a private host was refused names the command that would
admit it, and a credential header in the input names `/guardian secret`.

## The module

```rust
// guardian-tool: http_request
const GUARDIAN_NAME: &str = "http_request";
const GUARDIAN_DESC: &str = "A guarded curl: one HTTP request (GET, HEAD, POST, PUT, PATCH, DELETE) with headers, query pairs and a body or JSON value, for pages, resources and API work. Answers status, the answering URL, the response headers and the body (text, JSON, XML, NDJSON, form data; binary refused). An HTML page is reduced to text (as=auto|text) and scrubbed of injection directives; other bodies come back as sent, only flagged. Needs no key: a credential comes from the operator's store (/guardian secret <host> <header> <value>), never from the input; a private host needs /guardian egress allow <host>.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"url":{"type":"string"},"method":{"type":"string"},"headers":{"type":"object"},"query":{"type":"object"},"accept":{"type":"string"},"body":{"type":"string"},"json":{"description":"any JSON value, sent as the body with content-type application/json"},"as":{"type":"string","enum":["auto","text","raw"]},"max_bytes":{"type":"integer"},"timeout_ms":{"type":"integer"}},"required":["url"]}"#;
const GUARDIAN_CAPS: &[&str] = &["http_request"];

// A page reduced to text is cut here, a body returned as sent there. The
// host caps a body at the caller's max_bytes (at most 1 MiB); the output
// must stay under the sandbox's 2 MiB after JSON escaping.
const TEXT_CAP: usize = 64 * 1024;
const BODY_CAP: usize = 768 * 1024;

fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return err(&e),
    };
    let url = v.get("url").as_str().unwrap_or("").trim();
    if url.is_empty() {
        return err("url is required");
    }
    let mode = v.get("as").as_str().unwrap_or("auto");
    if !matches!(mode, "auto" | "text" | "raw") {
        return err("as must be auto, text or raw");
    }
    // The request goes to the host as the caller wrote it. The host vets
    // every field (method, headers, query, body, caps), runs the egress
    // policy on every hop, adds the operator's secrets, and never echoes
    // a request header.
    let env = json::parse(&host::http_request(input)).unwrap_or(json::null());
    if !env.get("ok").as_bool().unwrap_or(false) {
        return err(env.get("error").as_str().unwrap_or("request failed"));
    }
    let content_type = env.get("content_type").as_str().unwrap_or("");
    let body = env.get("body").as_str().unwrap_or("");
    let host_cut = env.get("truncated_at").as_f64().map(|n| n as usize);

    let mut out: Vec<(&str, json::Json)> = vec![
        ("status", json::n(env.get("status").as_f64().unwrap_or(0.0))),
        ("url", json::s(env.get("url").as_str().unwrap_or(url))),
    ];
    if let Some(hops) = env.get("redirects").as_f64() {
        out.push(("redirects", json::n(hops)));
    }
    if let Some(target) = env.get("redirect").as_str() {
        out.push(("redirect", json::s(target)));
    }
    out.push(("content_type", json::s(content_type)));
    out.push(("headers", env.get("headers").clone()));

    // A redirect the guard handed back (a POST's 3xx) has no body to read.
    let stopped = env.get("redirect").as_str().is_some();
    let reduce = !stopped && (mode == "text" || (mode == "auto" && is_html(content_type)));
    let mut truncated: Vec<(&str, json::Json)> = vec![];
    let (key, value, flagged) = if stopped {
        ("body", json::s(""), false)
    } else if reduce {
        // A page the model will read: reduced, then the scrubber rewrites
        // directives and the structural markers of an injected turn.
        let (clean, hit) = inject::redact(&html_text::to_text(body));
        let end = cut_at(&clean, TEXT_CAP);
        if let Some(n) = host_cut {
            truncated.push(("body", json::n(n as f64)));
        }
        if end < clean.len() {
            truncated.push(("text", json::n(TEXT_CAP as f64)));
        }
        ("text", json::s(&clean[..end]), hit)
    } else {
        // Data: as sent, nothing rewritten; the flag is the signal.
        let end = cut_at(body, BODY_CAP);
        if end < body.len() {
            truncated.push(("body", json::n(BODY_CAP as f64)));
        } else if let Some(n) = host_cut {
            truncated.push(("body", json::n(n as f64)));
        }
        ("body", json::s(&body[..end]), inject::flagged(body))
    };
    out.push((key, value));
    if !truncated.is_empty() {
        out.push(("truncated", json::obj(truncated)));
    }
    out.push(("injection_suspected", json::b(flagged)));
    json::stringify(&json::obj(out))
}

fn is_html(content_type: &str) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    ct.starts_with("text/html") || ct.starts_with("application/xhtml+xml")
}

/// Where to cut `s` so that at most `limit` bytes remain, on a character
/// boundary. Never a byte slice inside a multi-byte character.
fn cut_at(s: &str, limit: usize) -> usize {
    if s.len() <= limit {
        return s.len();
    }
    let mut end = limit;
    while end > 0 && !s.is_char_boundary(end) {
        end -= 1;
    }
    end
}

fn err(msg: &str) -> String {
    json::stringify(&json::obj(vec![("error", json::s(msg))]))
}
```

## Notes

- **The tool adds nothing to the request.** Every field the caller gives is
  vetted by the host (`parse_http_request` in `crates/embra-guardian/src/caps.rs`):
  the method, header names and values (a credential header is refused with
  the command that stores one; the transport's own headers are dropped),
  query pairs (appended form-encoded), the body (256 KiB) or a JSON value,
  and the caps.
- **Never panics:** every accessor uses `unwrap_or`; a guard error yields
  `{"error":…}`. A panic would be a sandbox trap surfaced as a tool error.
  The cut of a body is on a character boundary.
- **Tested end to end** over a wasm fixture built from this module with the
  pinned toolchain (`crates/embra-guardian/tests/http_request_roundtrip.rs`),
  with a recording transport: a page reduced and scrubbed, a POST's method,
  headers and JSON body as sent, a secret injected for its host and never
  for a redirect's, a redirect followed for GET and handed back for POST,
  a 404 as an answer, a binary body refused, a private host by allowlist
  and loopback never, a body at the host's 1 MiB cap through the sandbox.

## Try it

```text
/guardian status http_request     # → ready (built at boot; "shipped: yes")
# then ask in plain language, e.g.:
#   "use http_request to GET https://api.github.com/repos/rust-lang/rust and
#    tell me the star count"
#   "POST a note to issue 17 on our GitLab with http_request"  (after the
#    egress and secret setup above)
```

To change the tool, `/guardian-define` with your version of the module under
the same name; to be rid of it, `/guardian delete http_request`.
