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
