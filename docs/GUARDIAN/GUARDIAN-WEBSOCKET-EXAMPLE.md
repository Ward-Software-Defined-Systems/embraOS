# Guardian Example — the shipped `websocket`

A WebSocket client whose connections stay open between calls. One module
that declares **one** capability, `websocket`: open a `wss://` connection,
send text, JSON or binary frames, read what arrives, close. It is for the
services that speak over a socket and end their session when it closes: a
browser's DevTools endpoint (CDP), a JSON-RPC service, an event stream.
Read [GUARDIAN-TOOL-EXAMPLES.md](./GUARDIAN-TOOL-EXAMPLES.md) first for the
contract and the `json` / `host` / `inject` APIs; the `host::websocket` API
is described there.

**It ships with embraOS**, like [`web_search`](./GUARDIAN-ADVANCED-EXAMPLE.md)
and [`http_request`](./GUARDIAN-HTTP-REQUEST-EXAMPLE.md): the module below is
the shipped source (`crates/embra-guardian/src/shipped/websocket.rs`, the same
bytes as this page, pinned by a test), installed at boot and built in the
background; `/guardian list` shows it as `websocket (shipped)`. It needs no
key.

## Why the connection is the host's

A Guardian tool is a fresh instance on every call and keeps nothing, and
`http_request` speaks one request and its answer. A WebSocket outlasts
both: the service keeps its state for as long as the socket is open, and
a session that ends with its socket has to span several calls. So the
socket is not the tool's. The brain holds it between calls
(`crates/embra-brain/src/guardian/ws.rs`): one task a connection reads
the whole time, answers a ping and keeps what arrives in a bounded
buffer. `open` answers an id, and every later call names it. A tool
reaches the connections it opened and no other tool's. Nothing survives a
restart of the brain, and an id from before one names nothing.

## What the host does

The tool hands each operation to the host as the caller wrote it; the
policy lives on the host:

1. **The egress guard, at `open`** — the rules of `http_request`: `wss://`
   only (a plain `ws://` is refused as plain `http://` is), no userinfo, a
   public address passes, a private one only for a host on the operator's
   list (`/guardian egress allow <host>`), loopback and link-local never.
   The addresses the guard classified are the ones the host dials: the
   name is not resolved a second time.
2. **The operator's secrets, on the handshake** — a header stored with
   `/guardian secret <host> <header> <value>` is added for that host and
   port and no other. The input may not set `Authorization`, `Cookie` or
   any other credential header, and the headers the handshake owns
   (`Host`, `Upgrade`, `Connection`, `Sec-WebSocket-*`) are dropped. A
   subprotocol is offered with `protocols`; no extension is offered. The
   handshake carries `User-Agent: embraOS/<version>`; a `User-Agent` in
   `headers` is sent instead.
3. **A stored secret never leaves in an input** — a URL, a header, a
   subprotocol or a frame that carries one of the operator's stored
   secrets is refused before anything is sent.
4. **TLS** — the public roots and the operator's certificate authorities,
   read at every call: a certificate authority imported with `/ca import`
   is in force at the next `open`, with no restart. The certificate is
   verified against the URL's host.
5. **Bounds:**

   | Bound | Value |
   |---|---|
   | Connections open at once | 4 a tool, 8 in all |
   | Unused (no call touched it) | closed after 10 minutes |
   | Lifetime | closed after 60 minutes |
   | The handshake | 10 seconds; a redirect is named, not followed |
   | A frame sent | 256 KiB |
   | A message received | 1 MiB of it is kept, and a cut frame says its full size; a message over 4 MiB closes the connection (code 1009) |
   | Buffered while no call reads | 1024 frames or 4 MiB; the oldest are dropped and counted (`dropped`) |
   | Answered per call | `max_frames` 20 (at most 100), `max_bytes` 64 KiB of JSON text (at most 1 MiB) |
   | A wait | `wait_ms`, 10 seconds when a call waits for a frame, at most 60 |

   The figures are the project's estimates, not measured limits of a
   machine.
6. **One budget a call** — ten seconds for the I/O and the wait. An
   operator's stop (`/stop`, Esc) ends a wait at once; a stopped call takes
   no frame, and a connection it opened is closed.
7. **The journal** — one line when a connection opens and one when it
   closes: the tool, the id, the host and port; at the close also how long
   it was open, the frame counts, the close code and who ended it (the
   server, the tool, the OS, the transport). A close reason is logged only
   in the OS's own words; a server's and a tool's are answered to the tool
   and stay out of the journal. Never a frame, a header, the URL's path or
   its query.

## Setup

```text
/guardian egress allow <host>                           # a host on a private address
/guardian secret <host> Authorization Bearer <token>    # when the service wants a credential
/ca import <url | uploads/<file>>                       # when its certificate is the operator's own CA
```

## Input / output

`op` is `open`, `send`, `receive`, `close` or `list`.

```json
{"op": "open", "url": "wss://service.example/socket", "protocols": ["v1"]}
```

answers `{"id": "ws-3f9a1c2e", "protocol": "v1", "headers": {…}}`, or
`{"error": …, "status": 401, "body": "…"}` when the server answered the
handshake with something else.

```json
{"op": "send", "id": "ws-3f9a1c2e", "json": {"id": 1, "method": "ping"}, "reply_id": 1}
```

sends one frame: `text`, `json` or `binary` (base64). The host serializes
`json`: its keys go out in alphabetical order and a 64-bit integer arrives
exact; a service that wants its text byte for byte is sent `text`. With
`reply_id`, `until` or `wait_ms` the call reads as well.

- **`reply_id`** waits for the text frame whose JSON has that top-level
  `id` and answers it alone; the frames around it stay buffered. A command
  and its reply are one call, whatever events arrive between them. A
  number and a string name the same id.
- **`until`** waits for a text frame that contains the text and answers
  the frames in order through it: for an event, or a stream's last line.
  When it has not arrived by the end of the wait, the frames that are
  there are answered with `matched: false`.
- Without either, `receive` answers the frames in arrival order.

A read answers `frames` (`{"text": …}` or `{"binary": "<base64>", "bytes":
n}`), `matched`, `timed_out`, `buffered` (frames still waiting), `dropped`,
`closed` (`{code, reason}` once the connection ended) and the injection
scan below. `close` ends a connection; `list` names the tool's open ones.

A frame that was cut carries `truncated: {"bytes": n, "needs_max_bytes":
m}`: `n` is the message's size as it arrived, `m` the `max_bytes` a call
has to ask to be answered such a frame whole. The two differ because
`max_bytes` counts JSON text, where a quote, a backslash or a line break
takes two bytes. `m` is absent when no call can be answered the frame
whole: its JSON text is over 1 MiB, or the connection kept a part of it.

### A CDP exchange

Each line is the `input` of `guardian_call` with `tool: "websocket"`. The
reply of each command names the next one's ids.

```json
{"op": "open", "url": "wss://browser.example.lan/chromium"}
{"op": "send", "id": "<id>", "json": {"id": 1, "method": "Target.createTarget", "params": {"url": "about:blank"}}, "reply_id": 1}
{"op": "send", "id": "<id>", "json": {"id": 2, "method": "Target.attachToTarget", "params": {"targetId": "<T>", "flatten": true}}, "reply_id": 2}
{"op": "send", "id": "<id>", "json": {"id": 3, "sessionId": "<S>", "method": "Page.enable"}, "reply_id": 3}
{"op": "send", "id": "<id>", "json": {"id": 4, "sessionId": "<S>", "method": "Page.navigate", "params": {"url": "https://example.com/"}}, "reply_id": 4}
{"op": "receive", "id": "<id>", "until": "Page.loadEventFired", "wait_ms": 30000}
{"op": "send", "id": "<id>", "json": {"id": 5, "sessionId": "<S>", "method": "Runtime.evaluate", "params": {"expression": "document.title", "returnByValue": true}}, "reply_id": 5}
{"op": "close", "id": "<id>"}
```

Run against the browser service of [BROWSER-SERVICE.md](./BROWSER-SERVICE.md):
**TBD**.

## The module

```rust
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
```

## Notes

- **Frames are data.** They are answered as sent and never rewritten: a
  frame may be a protocol's own JSON, and a rewritten one would break it.
  The text frames are scanned (`inject::scan_json` for a frame that is
  JSON, `inject::scan` for any other) and `injection_suspected` is false
  unless the scan matched; `injection_signals` then names what it matched.
  What a page says, read through a browser's `Runtime.evaluate`, is the
  page's text and not an instruction.
- **Frames are taken once.** A call that reads frames removes them from
  the buffer. A call the operator stops after it took frames loses them,
  and two calls on one connection each get what they took.
- **A large result.** An answer carries `max_bytes` of JSON text, 64 KiB
  unless the call asks more. A frame that does not fit is answered cut and
  is gone from the buffer, as a body over `http_request`'s `max_bytes` is
  cut: the head of a long text is there to read, and a command whose
  reply has to arrive whole is sent with a larger `max_bytes` (the cut
  frame's `needs_max_bytes` names it) or asks the service for less. A
  message is kept up to 1 MiB whatever a call asks. A screenshot or a PDF
  goes through the service's REST route with `http_request` and `save_to`
  ([BROWSER-SERVICE.md](./BROWSER-SERVICE.md)), not through a frame.
- **The operator's hold.** `/guardian connections` lists the open
  connections and `/guardian connections close <id>` (or `all`) ends them.
  `/guardian egress deny <host>` and the removal of a host's secret close
  the connections to that host; `/guardian delete` and a rebuild close the
  tool's.

## Try it

```text
/guardian list          # websocket (shipped) …
# then ask in plain language, e.g.:
#   "open a WebSocket to wss://echo.example/ and send it hello"
#   "drive the browser service over CDP: open example.com and tell me its
#    title"  (after the egress, secret and CA setup above)
```
