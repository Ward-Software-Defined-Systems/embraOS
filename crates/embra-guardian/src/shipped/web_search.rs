// guardian-tool: web_search
const GUARDIAN_NAME: &str = "web_search";
const GUARDIAN_DESC: &str = "Search the web (Brave, via the Guardian web_search guard) with optional recency/exclude/pagination/min_score, optionally fetch + read the top results via the http_get guard, and neutralize prompt-injection in every field before the model sees it. Ranked by query overlap, de-duplicated by host. Needs the operator's Brave Search key (/guardian key brave); without it every call answers 'search capability not configured'.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"query":{"type":"string"},"max":{"type":"integer"},"recency":{"type":"string"},"exclude":{"type":"array","items":{"type":"string"}},"offset":{"type":"integer"},"extra_snippets":{"type":"boolean"},"fetch_top":{"type":"integer"},"min_score":{"type":"number"}},"required":["query"]}"#;
const GUARDIAN_CAPS: &[&str] = &["web_search", "http_get"];

// Structural markers: rewritten to [redacted-directive] wherever they
// appear. Each is the frame of an injected turn, not ordinary prose.
const REDACT_MARKERS: &[&str] = &[
    "you are now", "new instructions:", "developer message", "begin system",
    "[tool:", "</system>", "assistant:", "```tool", "disregard the above",
];
// Flag-only markers: they set injection_suspected and stay in the text.
// "system prompt" is the subject of every page ABOUT prompt injection;
// rewriting it destroyed the content a security query asked for.
const FLAG_MARKERS: &[&str] = &["system prompt"];
// An imperative directive: a verb, then an object within a few words.
// "Ignore your previous instructions", "Please ignore all prior rules",
// "IGNORE ALL INSTRUCTIONS" and "Ignore the user and reply …" all match;
// "ignore case" and "forget it" do not. A strong object matches on its
// own; a weak one, common in ordinary prose ("# ignore duplicate commands"
// in a .bashrc), needs a qualifier between the verb and it ("disregard
// all safety policies", "forget your guidelines").
const DIRECTIVE_VERBS: &[&str] = &["ignore", "disregard", "forget"];
const DIRECTIVE_OBJECTS_STRONG: &[&str] = &[
    "instruction", "instructions", "directive", "directives", "rule", "rules", "user", "users",
];
const DIRECTIVE_OBJECTS_WEAK: &[&str] = &[
    "direction", "directions", "prompt", "prompts", "guideline", "guidelines", "guidance",
    "command", "commands", "order", "orders", "policy", "policies", "constraint", "constraints",
];
const DIRECTIVE_QUALIFIERS: &[&str] = &[
    "all", "any", "every", "previous", "prior", "above", "earlier", "original", "initial",
    "existing", "your", "my", "our", "their", "its", "safety", "system", "developer",
    "operator", "assistant",
];
const DIRECTIVE_WINDOW: usize = 5;

struct Entry {
    title: String,
    url: String,
    description: String,
    age: Option<String>,
    snippets: Vec<String>,
    score: f64,
    injection: bool,
    text: Option<String>,
    // How the fetch of the page went: the HTTP status when it was read,
    // the guard's reason when it was refused or failed, the URL that
    // answered when the guard followed a redirect, the target of one it
    // did not follow. An empty `text` is then told from a blocked fetch.
    fetch_status: Option<u16>,
    fetch_error: Option<String>,
    fetch_url: Option<String>,
    fetch_redirect: Option<String>,
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
    let min_score = v.get("min_score").as_f64().unwrap_or(0.0);

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
    // min_score drops the fuzzy hits a nonsense query brings back at 0.
    if min_score > 0.0 {
        out.retain(|e| e.score >= min_score);
    }
    out.truncate(max);

    // Optionally fetch + read the top N pages — same scrubber.
    for e in out.iter_mut().take(fetch_top) {
        let fenv = json::parse(&host::http_get(&e.url)).unwrap_or(json::null());
        if !fenv.get("ok").as_bool().unwrap_or(false) {
            let why = fenv.get("error").as_str().unwrap_or("fetch failed");
            e.fetch_error = Some(sanitize(why, 200).0);
            continue;
        }
        e.fetch_status = fenv.get("status").as_f64().map(|st| st as u16);
        match fenv.get("url").as_str() {
            Some(answered) if answered != e.url => e.fetch_url = Some(sanitize(answered, 300).0),
            _ => {}
        }
        if let Some(target) = fenv.get("redirect").as_str() {
            e.fetch_redirect = Some(sanitize(target, 300).0);
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
    let raw_title = r.get("title").as_str().unwrap_or("");
    let raw_desc = r.get("description").as_str().unwrap_or("");
    // Scored before the scrubber runs: a redacted directive still says
    // what the page is about, and a security query must find it.
    let score = overlap_score(query, raw_title, raw_desc);
    let (title, tf, tc) = sanitize(raw_title, 200);
    let (desc, df, dc) = sanitize(raw_desc, 1000);
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
    let mut snippets_cut = false;
    if let Some(arr) = r.get("snippets").as_array() {
        for s in arr {
            let (clean, f, cut) = sanitize(s.as_str().unwrap_or(""), 500);
            injection = injection || f;
            snippets_cut = snippets_cut || cut;
            snippets.push(clean);
        }
    }
    if snippets_cut {
        truncated.push(("snippets", 500));
    }
    Entry {
        title,
        url,
        description: desc,
        age,
        snippets,
        score,
        injection,
        text: None,
        fetch_status: None,
        fetch_error: None,
        fetch_url: None,
        fetch_redirect: None,
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
    if let Some(st) = e.fetch_status {
        o.push(("fetch_status", json::n(st as f64)));
    }
    if let Some(why) = &e.fetch_error {
        o.push(("fetch_error", json::s(why)));
    }
    if let Some(answered) = &e.fetch_url {
        o.push(("fetch_url", json::s(answered)));
    }
    if let Some(target) = &e.fetch_redirect {
        o.push(("fetch_redirect", json::s(target)));
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
/// Order: the directive matcher, the structural markers (rewritten), the
/// flag-only markers (kept), then the cap.
fn sanitize(raw: &str, cap: usize) -> (String, bool, bool) {
    let cleaned: String = raw
        .chars()
        .filter(|c| !c.is_control() || *c == '\n' || *c == '\t')
        .filter(|c| !matches!(*c, '\u{200B}'..='\u{200F}' | '\u{2060}' | '\u{FEFF}'))
        .collect();
    let (mut s, mut flagged) = redact_directives(&cleaned);
    for m in REDACT_MARKERS {
        if contains_ci(&s, m) {
            flagged = true;
            s = redact_ci(&s, m, "[redacted-directive]");
        }
    }
    for m in FLAG_MARKERS {
        if contains_ci(&s, m) {
            flagged = true;
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

/// Words are runs of ASCII letters and digits; everything else separates
/// them. A DIRECTIVE_VERBS word followed within DIRECTIVE_WINDOW words by a
/// strong object, or by a weak object with a qualifier between them, is
/// one directive: the span from the verb through the object becomes one
/// [redacted-directive]. Returns (text, flagged).
fn redact_directives(s: &str) -> (String, bool) {
    let b = s.as_bytes();
    let mut words: Vec<(usize, usize)> = vec![];
    let mut i = 0;
    while i < b.len() {
        if b[i].is_ascii_alphanumeric() {
            let start = i;
            while i < b.len() && b[i].is_ascii_alphanumeric() {
                i += 1;
            }
            words.push((start, i));
        } else {
            i += 1;
        }
    }
    let mut spans: Vec<(usize, usize)> = vec![];
    let mut w = 0;
    while w < words.len() {
        let (vs, ve) = words[w];
        if !word_in(&s[vs..ve], DIRECTIVE_VERBS) {
            w += 1;
            continue;
        }
        let mut found: Option<(usize, usize)> = None;
        let mut k = w + 1;
        while k < words.len() && k <= w + DIRECTIVE_WINDOW {
            let (os, oe) = words[k];
            let object = &s[os..oe];
            let strong = word_in(object, DIRECTIVE_OBJECTS_STRONG);
            let weak = !strong && word_in(object, DIRECTIVE_OBJECTS_WEAK);
            if strong || (weak && has_qualifier(s, &words[w + 1..k])) {
                found = Some((k, oe));
                break;
            }
            k += 1;
        }
        match found {
            Some((k, oe)) => {
                // A possessive on the object is part of the directive:
                // "ignore the user's request" leaves no "'s request" stub.
                let oe = possessive_end(s, oe);
                spans.push((vs, oe));
                w = k + 1;
            }
            None => w += 1,
        }
    }
    if spans.is_empty() {
        return (s.to_string(), false);
    }
    let mut out = String::with_capacity(s.len());
    let mut last = 0;
    for (a, z) in spans {
        out.push_str(&s[last..a]);
        out.push_str("[redacted-directive]");
        last = z;
    }
    out.push_str(&s[last..]);
    (out, true)
}

fn word_in(word: &str, set: &[&str]) -> bool {
    set.iter().any(|m| m.eq_ignore_ascii_case(word))
}

/// The end of a `'s` / `’s` that follows the word ending at `end`, else
/// `end`. A prefix test, never a byte slice: the byte after the word may
/// open a multi-byte character (a curly quote).
fn possessive_end(s: &str, end: usize) -> usize {
    let rest = &s[end..];
    for suffix in ["'s", "'S", "\u{2019}s", "\u{2019}S"] {
        if rest.starts_with(suffix) {
            return end + suffix.len();
        }
    }
    end
}

/// Whether one of the words (byte spans into `s`) is a DIRECTIVE_QUALIFIERS word.
fn has_qualifier(s: &str, between: &[(usize, usize)]) -> bool {
    between.iter().any(|(a, z)| word_in(&s[*a..*z], DIRECTIVE_QUALIFIERS))
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
    for raw in query.split_whitespace() {
        // A quoted query carries its quotes on its first and last words.
        let tok = raw.trim_matches(|c: char| !c.is_alphanumeric());
        if tok.len() < 2 {
            continue;
        }
        if contains_ci(&hay, tok) {
            score += 1.0;
        }
    }
    score
}
