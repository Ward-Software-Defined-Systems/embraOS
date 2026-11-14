// guardian-tool: websocket
const GUARDIAN_NAME: &str = "websocket";
const GUARDIAN_DESC: &str = "A WebSocket client: op=open (wss:// url, headers, protocols) answers an id; send (text, json or base64 binary); receive; close; list. Connections stay open between calls: 4 at most, closed after 10 min unused or 60 min. reply_id waits for the JSON frame with that top-level id (a CDP or JSON-RPC reply), answered alone; until=<text> answers frames through the first that carries it; wait_ms <= 60000. Frames come back as sent; injection_suspected is false unless the scan matched. Credentials: /guardian secret <host> <header> <value>, never the input; a private host: /guardian egress allow <host>.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"op":{"type":"string","enum":["open","send","receive","close","list"]},"url":{"type":"string","description":"open: a wss:// URL"},"headers":{"type":"object","description":"open: handshake headers; Authorization and Cookie come from /guardian secret"},"protocols":{"type":"array","items":{"type":"string"},"description":"open: subprotocols offered; the server must pick one"},"id":{"type":"string","description":"the connection, as open answered it"},"text":{"type":"string","description":"send: a text frame"},"json":{"description":"send: any JSON value, sent as a text frame"},"binary":{"type":"string","description":"send: a binary frame, base64"},"reply_id":{"description":"send or receive: wait for the text frame whose JSON has this top-level id and answer it alone; the other frames stay buffered"},"until":{"type":"string","description":"send or receive: wait for a text frame that contains this; frames are answered in order through it"},"wait_ms":{"type":"integer","description":"at most 60000; default 10000 when the call waits for a frame"},"max_frames":{"type":"integer","description":"default 20, at most 100"},"max_bytes":{"type":"integer","description":"JSON text of frames answered per call: default 65536, at most 1048576. A frame that does not fit is cut and gone; its truncated.needs_max_bytes is what answers such a frame whole"},"code":{"type":"integer","description":"close: default 1000"},"reason":{"type":"string","description":"close: at most 123 bytes"}},"required":["op"]}"#;
const GUARDIAN_CAPS: &[&str] = &["websocket"];
// 2026-11-14: the first version.

fn run(input: &str) -> String {
    let v = match json::parse(input) {
        Ok(v) => v,
        Err(e) => return err(&e),
    };
    let op = v.get("op").as_str().unwrap_or("");
    if !matches!(op, "open" | "send" | "receive" | "close" | "list") {
        return err("op must be open, send, receive, close or list");
    }
    // The operation goes to the host as the caller wrote it. The host keeps
    // the connection between calls (this instance is gone when the call
    // returns), runs the egress policy at open, adds the operator's secrets
    // to the handshake, refuses a frame that carries one, and serializes
    // "json" itself: a 64-bit integer in a command arrives exact, where
    // this module's numbers are floating point.
    let env = json::parse(&host::websocket(input)).unwrap_or(json::null());
    if !env.get("ok").as_bool().unwrap_or(false) {
        // A handshake the server refused carries its status and its words.
        let mut out: Vec<(&str, json::Json)> =
            vec![("error", json::s(env.get("error").as_str().unwrap_or("the operation failed")))];
        for key in ["status", "redirect", "body"] {
            if !env.get(key).is_null() {
                out.push((key, env.get(key).clone()));
            }
        }
        return json::stringify(&json::obj(out));
    }
    let mut out: Vec<(&str, json::Json)> = vec![];
    for key in [
        "id", "protocol", "headers", "connections", "sent", "frames", "matched", "timed_out", "buffered",
        "dropped", "closed",
    ] {
        if !env.get(key).is_null() {
            out.push((key, env.get(key).clone()));
        }
    }
    // Frames are data: answered as sent, nothing rewritten, and what a
    // text frame carries is named. A frame that is JSON is read as JSON,
    // so an escape before a word hides nothing; any other frame as text.
    if let Some(frames) = env.get("frames").as_array() {
        let mut found: Vec<String> = vec![];
        for frame in frames {
            if let Some(text) = frame.get("text").as_str() {
                let names = if is_json(text) { inject::scan_json(text) } else { inject::scan(text) };
                for name in names {
                    if !found.contains(&name) {
                        found.push(name);
                    }
                }
            }
        }
        // False unless the scan matched, and then the names say what it
        // matched: never a flag every frame carries.
        out.push(("injection_suspected", json::b(!found.is_empty())));
        if !found.is_empty() {
            out.push(("injection_signals", json::arr(found.iter().map(|n| json::s(n)).collect())));
        }
    }
    json::stringify(&json::obj(out))
}

/// A frame that is a JSON object or array, by its first character.
fn is_json(text: &str) -> bool {
    let start = text.trim_start();
    start.starts_with('{') || start.starts_with('[')
}

fn err(msg: &str) -> String {
    json::stringify(&json::obj(vec![("error", json::s(msg))]))
}
