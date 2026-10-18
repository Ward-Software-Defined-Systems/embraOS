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
3. **The user agent** — every hop carries `User-Agent: embraOS/<version>`,
   the workspace version (`embraOS/0.18.0-phase1` today), so it moves with
   a release; a `User-Agent` in the request's `headers` is sent instead.
4. **Redirects** followed for GET and HEAD only, at most three, every hop
   checked; the other methods get the 3xx back with its target under
   `redirect`. The whole call shares one budget: 10 s, or what a top-level
   `timeout_ms` asks, up to 60 s; the call's deadline follows it.
5. **The answer** — status, the URL that answered, the response headers
   (lowercase, no cookies, capped) and the body, for text, JSON, XML, NDJSON
   and form data; a binary body is refused by name unless it is saved
   (step 6). The envelope never carries a request header.
6. **A save, with `save_to`** — a 2xx body of any type goes to a file under
   `/embra/workspace`, whole or not at all: a body over 12 MiB is refused
   and nothing is written; an existing file is never overwritten; the media
   store (`MEDIA/`) and the Guardian build tree (`.guardian/`) are refused;
   a symlinked directory leads nowhere (the nearest existing ancestor is
   canonicalized before any directory is created); the write is atomic. The
   answer names the saved path and its size; nothing is read or scanned. A
   non-2xx answer comes back as text as usual, so an error page stays
   readable. The broker never resolves a path: the brain's workspace sink
   does (`crates/embra-brain/src/guardian/save.rs`).

## Setup

```text
/guardian egress allow gitlab.example.com                   # a private host, once
/guardian secret gitlab.example.com PRIVATE-TOKEN glpat-…   # a credential, once
/guardian secret                                            # the hosts and header names
```

## Input / output

Input (`url` required; everything else optional):

```json
{ "url": "https://gitlab.example.com/api/v4/projects/2/issues/17",
  "method": "GET", "accept": "application/json",
  "query": { "per_page": 5 }, "timeout_ms": 8000 }
```

```json
{ "url": "https://gitlab.example.com/api/v4/projects/2/issues/17/notes",
  "method": "POST", "json": { "body": "Posted by the intelligence." } }
```

Output:

```json
{ "status": 200, "url": "https://gitlab.example.com/api/v4/projects/2/issues/17",
  "content_type": "application/json",
  "headers": { "content-type": "application/json", "x-request-id": "…" },
  "body": "{\"iid\":17,…}", "injection_suspected": false }
```

`as` chooses the body's form: `auto` (the default) reduces an HTML page to
text and returns anything else as sent, `text` reduces whatever came back,
`raw` never reduces. A reduced page is scrubbed: injection directives and
the structural markers of an injected turn become `[redacted-directive]`
(`inject::scrub`). A body returned as sent is never rewritten, only scanned
(`inject::scan`): data must stay as it was sent. The scan reads the whole
body the host returned, the part past the tool's own cut included. A JSON
body (`application/json`, `application/x-ndjson`, `application/*+json`) is
read as JSON (`inject::scan_json`): an escape before a word does not hide it,
so `\n\nIgnore …` in an API body reads as `Ignore …`. Any other body, and a
reduced page, is decoded text, where a backslash is a character. A marker
split by a line break is not found.

`injection_suspected` is false unless the scan matched. When it matched,
`injection_signals` names each finding once:

- `directive: <verb> … <object>` (`directive: ignore … instructions`): an
  imperative aimed at the model's instructions;
- `marker: <marker>` (`marker: assistant:`): the frame of an injected turn;
- `mentions: system prompt`: the phrase only, not a directive. Pages about
  prompt injection carry it, and so does a repository whose commits talk
  about the system prompt.

A name is spelled from the scanner's own lists, never copied from the body,
so the field cannot carry injected words. A commits listing that mentions
the system prompt answers:

```json
{ "status": 200, "url": "https://gitlab.example.com/api/v4/projects/2/repository/commits",
  "content_type": "application/json", "headers": { "…": "…" },
  "body": "[{\"id\":\"5604bce…\",\"message\":\"… No tool description or system prompt changed.\\n\"},…]",
  "injection_suspected": true, "injection_signals": ["mentions: system prompt"] }
```

`truncated` names a cut: `{"body": N}` when the host cut the body at
`max_bytes` (N bytes kept; default 256 KiB, at most 1 MiB, ask again with a
larger `max_bytes`) or the tool at its own 768 KiB; `{"text": 65536}` when
the reduced text was cut. A body cut mid-way is not the answer, and the
model is told so. With `save_to` the hop reads up to the 12 MiB save cap: a
2xx body over it is refused whole, and a non-2xx answer comes back as text
cut at 1 MiB.

A refused request or a guard error answers `{"error": "…"}` in the guard's
words: the reason a private host was refused names the command that would
admit it, and a credential header in the input names `/guardian secret`.

### `save_to`

`save_to` names a file under `/embra/workspace` (relative to it, or absolute
under it) that takes the body in place of the answer: any content type, a
binary one included, a 2xx only, up to 12 MiB, never over an existing file.
A screenshot from a headless-browser service reached over HTTPS
([BROWSER-SERVICE.md](./BROWSER-SERVICE.md)):

```json
{ "url": "https://browser.example.lan/screenshot", "method": "POST",
  "json": { "url": "https://example.com/", "options": { "type": "png" } },
  "save_to": "shots/example.png" }
```

```json
{ "status": 200, "url": "https://browser.example.lan/screenshot",
  "content_type": "image/png", "headers": { "content-type": "image/png" },
  "saved": { "path": "/embra/workspace/shots/example.png", "bytes": 183221 } }
```

Nothing was read, so the answer carries no `body`, no `truncated` and no
`injection_*` field, and `as` is ignored. `image_view` then shows the
picture to the intelligence and to the operator (a long capture comes
back in tiles, four per call), and `file_offer` offers the file for
download. A body over the cap answers `{"error": "the body is
larger than the save cap of 12582912 bytes; nothing written"}`; an existing
target, a path outside the workspace, `MEDIA/…`, `.guardian/…` and a path
that names no file are refused in the sink's words before any request is
sent; HEAD with `save_to` is refused. A non-2xx answer is not saved: it
comes back as text under the response cap, the way an error page should.

## The module

```rust
// guardian-tool: http_request
const GUARDIAN_NAME: &str = "http_request";
const GUARDIAN_DESC: &str = "A guarded curl: one HTTP request (GET, HEAD, POST, PUT, PATCH, DELETE) with headers, query, body or json. Answers status, URL, headers, body (text, JSON, XML, NDJSON, form). HTML is reduced to text (as=auto|text) and scrubbed; injection_suspected is false unless the scan matched; injection_signals names each match. save_to=<workspace path> saves a 2xx body of any type (binary too, up to 12 MiB, never overwriting) and answers the path, for image_view or file_offer. Credentials: /guardian secret <host> <header> <value>, never the input; a private host needs /guardian egress allow <host>.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"url":{"type":"string"},"method":{"type":"string"},"headers":{"type":"object"},"query":{"type":"object"},"accept":{"type":"string"},"body":{"type":"string"},"json":{"description":"any JSON value, sent as the body with content-type application/json"},"as":{"type":"string","enum":["auto","text","raw"]},"save_to":{"type":"string"},"max_bytes":{"type":"integer"},"timeout_ms":{"type":"integer"}},"required":["url"]}"#;
const GUARDIAN_CAPS: &[&str] = &["http_request"];

// A page reduced to text is cut here, a body returned as sent there. The
// host caps a body at the caller's max_bytes (at most 1 MiB; a body saved
// with save_to lands in a file and never comes here); the output must stay
// under the sandbox's 2 MiB after JSON escaping.
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

    // A saved body: the host wrote it under the workspace (a 2xx only,
    // whole or nothing) and answers the path and the size. Nothing was
    // read, so there is nothing to cut or to scan.
    let saved = env.get("saved");
    if !saved.is_null() {
        out.push(("saved", saved.clone()));
        return json::stringify(&json::obj(out));
    }

    // A redirect the guard handed back (a POST's 3xx) has no body to read.
    let stopped = env.get("redirect").as_str().is_some();
    let reduce = !stopped && (mode == "text" || (mode == "auto" && is_html(content_type)));
    let mut truncated: Vec<(&str, json::Json)> = vec![];
    let (key, value, found) = if stopped {
        ("body", json::s(""), vec![])
    } else if reduce {
        // A page the model will read: reduced, then the scrubber rewrites
        // directives and the structural markers of an injected turn and
        // names what it found.
        let (clean, found) = inject::scrub(&html_text::to_text(body));
        let end = cut_at(&clean, TEXT_CAP);
        if let Some(n) = host_cut {
            truncated.push(("body", json::n(n as f64)));
        }
        if end < clean.len() {
            truncated.push(("text", json::n(TEXT_CAP as f64)));
        }
        ("text", json::s(&clean[..end]), found)
    } else {
        // Data: as sent, nothing rewritten; the names are the signal. A
        // JSON body is read as JSON, so an escape before a word hides
        // nothing; any other body is read as text.
        let end = cut_at(body, BODY_CAP);
        if end < body.len() {
            truncated.push(("body", json::n(BODY_CAP as f64)));
        } else if let Some(n) = host_cut {
            truncated.push(("body", json::n(n as f64)));
        }
        let found = if is_json(content_type) { inject::scan_json(body) } else { inject::scan(body) };
        ("body", json::s(&body[..end]), found)
    };
    out.push((key, value));
    if !truncated.is_empty() {
        out.push(("truncated", json::obj(truncated)));
    }
    // False unless the scan matched, and then the names say what it matched:
    // never a flag every body carries.
    out.push(("injection_suspected", json::b(!found.is_empty())));
    if !found.is_empty() {
        out.push(("injection_signals", json::arr(found.iter().map(|n| json::s(n)).collect())));
    }
    json::stringify(&json::obj(out))
}

fn is_html(content_type: &str) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    ct.starts_with("text/html") || ct.starts_with("application/xhtml+xml")
}

/// The JSON media types the host lets through: JSON, NDJSON and any
/// `application/*+json`.
fn is_json(content_type: &str) -> bool {
    let ct = content_type.trim().to_ascii_lowercase();
    let mt = ct.split(';').next().unwrap_or("").trim();
    mt == "application/json"
        || mt == "application/x-ndjson"
        || (mt.starts_with("application/") && mt.ends_with("+json"))
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
- **A save is the host's.** With `save_to` the tool forwards the path as
  written; the brain's sink resolves it inside the workspace jail, refuses
  the media store, the Guardian build tree, an existing target and a
  symlinked escape, and writes atomically through the one commit routine.
  The tool reads nothing of a saved body.
- **Never panics:** every accessor uses `unwrap_or`; a guard error yields
  `{"error":…}`. A panic would be a sandbox trap surfaced as a tool error.
  The cut of a body is on a character boundary.
- **Tested end to end** over a wasm fixture built from this module with the
  pinned toolchain (`crates/embra-guardian/tests/http_request_roundtrip.rs`),
  with a recording transport: a page reduced and scrubbed with its findings
  named, a POST's method, headers and JSON body as sent, a commits listing
  that mentions the system prompt named as a mention and a clean one with
  the flag false, a secret injected for its host and never for a
  redirect's, a redirect followed for GET and handed back for POST, a 404
  as an answer, a binary body refused, a private host by allowlist and
  loopback never, a body at the host's 1 MiB cap through the sandbox, a
  binary body saved to the workspace and answered as `saved` with no body
  and no scan, an error page under `save_to` answered as text with nothing
  saved. A
  sidecar beside the fixture holds the hash of every file the scaffold
  wrote, so a helper or prelude change without a rebuild fails the test.

## Try it

```text
/guardian status http_request     # → ready (built at boot; "shipped: yes")
# then ask in plain language, e.g.:
#   "use http_request to GET https://api.github.com/repos/rust-lang/rust and
#    tell me the star count"
#   "POST a note to issue 17 on our GitLab with http_request"  (after the
#    egress and secret setup above)
#   "take a screenshot of https://example.com/ with the browser service and
#    show me"  (BROWSER-SERVICE.md; save_to, then image_view)
#   "list the unread mail in the shared mailbox and summarize it"
#    (M365-SHARED-MAILBOX.md; the bearer from /guardian secret)
```

To change the tool, `/guardian-define` with your version of the module under
the same name; to be rid of it, `/guardian delete http_request`.
