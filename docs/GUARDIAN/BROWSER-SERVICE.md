# A browser service beside the OS

embraOS gives the intelligence no browser. The browser engine in the image
(WPE WebKit under cog) is the display console: the web console on the
machine's own display ([SYSTEM-DESIGN.md](../SYSTEM-DESIGN.md)). Buildroot
has no Chromium for musl, a browser for the open web on the live OS would run
untrusted JavaScript the egress guard never sees, and the VM has 4 GB. The
browser runs as a service on your network instead, and
Embra reaches it with two shipped tools. `http_request`
([GUARDIAN-HTTP-REQUEST-EXAMPLE.md](./GUARDIAN-HTTP-REQUEST-EXAMPLE.md))
speaks one request and its answer: rendered pages as text, screenshots and
PDFs into the workspace with `save_to`, scripted interaction in one
request. `websocket`
([GUARDIAN-WEBSOCKET-EXAMPLE.md](./GUARDIAN-WEBSOCKET-EXAMPLE.md)) holds a
connection to the service's DevTools endpoint open between tool calls: a
browser session over several calls. The files under
[`BROWSER-SERVICE/`](./BROWSER-SERVICE/) run it with Docker Compose.

**Validated:** 2026-10-17. Browserless `v2.57.1` behind Caddy `2.11.7` with
Docker Compose on an Ubuntu 24.04 host, Caddy's internal CA seeded into
STATE; from a QEMU image, every use and refusal on this page, the 60 s
budget, and the full-page measurements under *A screenshot*. The tiles of
`image_view` named there: validated 2026-10-18 in three rounds on the same
captures (seven tiles of 800×2366–2383 px for a 16,642 px page, the cuts
on blank lines and in the gap beside a figure, `part=2`, the refusals, the
mobile UI, the recapture with `scrollPage`).

**The session over several calls:** 2026-11-14, from the build host and
not from a booted machine. The shipped `websocket` module, the Guardian
guard, the brain's connection broker and its TLS on the service's root ran
in one test process (`a_devtools_session_against_a_live_browser_service`,
`crates/embra-brain/src/guardian/ws.rs`; its comment has the command)
against a second instance of this compose: its own project and token,
published on the Docker host's bridge address alone, `BROWSER_HOST` an IP
literal with the `default_sni` line of *Start it*. Every answer, refusal
and figure under *A session over several calls* and in the last three
*Limits* is from those runs, and so is the bare upgrade under *Check it
from the host*. Driven by the intelligence on a booted machine, across
turns, with `/guardian connections`, `/guardian egress deny` and a stop
during a wait: **TBD**.

## What runs

Two containers, one published port:

| Container | Image | Published | Role |
|---|---|---|---|
| `browserless` | `ghcr.io/browserless/chromium:v2.57.1` | nothing | headless Chromium with a REST API on `:3000`, reachable from `caddy` only |
| `caddy` | `caddy:2.11.7` | `443` | TLS for `BROWSER_HOST`, proxied to `browserless:3000` |

The Guardian guard admits https and wss only, for a private host too
(`check_destination`, `crates/embra-guardian/src/caps.rs`); Caddy is that
TLS, for the REST routes and the DevTools WebSocket alike. Browserless is
licensed "SSPL-1.0 OR Browserless Commercial License"; read it before a
commercial deployment. Its REST routes (`/content`, `/screenshot`, `/pdf`,
`/function`, `/scrape`) are in the open-source image's source tree; the
running container lists what it serves at `https://<BROWSER_HOST>/docs`,
the check before relying on a route. Its DevTools endpoint is the
WebSocket route `/chromium`.

## Start it

```text
cd docs/GUARDIAN/BROWSER-SERVICE
cp .env.example .env            # BROWSER_HOST, BROWSERLESS_TOKEN (openssl rand -hex 16)
docker compose up -d
```

`.env` holds the token (`TOKEN`, enforced on every route by
`STRICT_TOKEN_USE`) and the name. The compose keeps `CONCURRENT` at 2,
`QUEUED` at 4, `TIMEOUT` at 60 s and `HEALTH` on; `ALLOW_GET` and
`ALLOW_FILE_PROTOCOL` stay off. Chrome needs `shm_size: 2g`.

`BROWSER_HOST` must resolve, from inside the VM, to the Docker host's
private address: a name in your DNS, or the IP literal itself as the site
name. Caddy issues a certificate for an IP, and serves it only with one
more line in the global options of `caddy/Caddyfile`, `default_sni
{$BROWSER_HOST}`: a client that reaches a site by an IP literal sends no
server name, and without the line Caddy finds no certificate for it
(found 2026-11-14 with an IP literal as the site: `curl` got a TLS
`internal error` alert, and a WebSocket `open` answered `TLS with
<address> failed: received fatal alert: InternalError`). QEMU's
user-mode network forwards DNS to the host's resolver.

## The certificate

The OS trusts the operator's CA drop-ins in `/embra/state/ca-certificates/`
on the Guardian transport (`/ca import` adds one on a running machine,
`seed-state.sh --ca-dir` on a stopped raw image), and the certificate
authorities an image was built with from `Imported_CAs/`
([QUICK-START.md — Self-hosted git servers (private CA)](../BUILD/QUICK-START.md#self-hosted-git-servers-private-ca)).
Two ways, one `tls` line in `caddy/Caddyfile`:

- **Your own CA.** Issue a certificate for `BROWSER_HOST`, mount it at
  `/certs` (the commented volume in the compose) and switch the `tls` line
  to the two files. When that CA's root is already in STATE, nothing on the
  OS changes.
- **Caddy's internal CA** (`tls internal`, the default). Caddy signs a
  certificate for `BROWSER_HOST` with a root it keeps in the `caddy_data`
  volume. Copy the root into the OS, with the VM stopped (in
  `docs/GUARDIAN/BROWSER-SERVICE`, where the compose runs; git ignores
  `*.crt`):

  ```text
  mkdir -p ca
  docker compose exec caddy cat /data/caddy/pki/authorities/local/root.crt > ca/browser-root.crt
  ../../../scripts/seed-state.sh --ca-dir ca
  ```

  `skip_install_trust` in the Caddyfile keeps Caddy out of the Docker
  host's own trust store.

## Check it from the host

```text
curl --cacert ca/browser-root.crt -H "Authorization: Bearer $BROWSERLESS_TOKEN" \
  https://$BROWSER_HOST/docs | head -c 300
curl --cacert ca/browser-root.crt -H "Authorization: Bearer $BROWSERLESS_TOKEN" \
  -H 'Content-Type: application/json' -d '{"url":"https://example.com/"}' \
  https://$BROWSER_HOST/content | head -c 300
```

The first lists the routes; the second answers the rendered page as
`text/html`. The DevTools endpoint is checked with a bare upgrade:

```text
curl --cacert ca/browser-root.crt --http1.1 -i -N --max-time 5 \
  -H "Authorization: Bearer $BROWSERLESS_TOKEN" \
  -H 'Connection: Upgrade' -H 'Upgrade: websocket' \
  -H 'Sec-WebSocket-Version: 13' -H 'Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==' \
  https://$BROWSER_HOST/chromium | head -3
```

It answers `HTTP/1.1 101 Switching Protocols` and waits (`--max-time` ends
it); without the header, `HTTP/1.1 401 Unauthorized`.

## Tell the OS

```text
/guardian egress allow browser.example.lan
/guardian secret browser.example.lan Authorization Bearer <BROWSERLESS_TOKEN>
```

The first admits the private host; the second stores the bearer the host
adds to every `http_request` to that host and to the handshake of every
WebSocket connection there
([COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md)). The token never enters a
tool's input, the turn trace or the session history. Browserless also takes
`?token=`; do not use it, a URL is logged. A service on another port is
stored as `<host>:<port>`.

## Use it

Ask in plain language; these are the inputs Embra sends:

- **A page as text** (`/content`):
  `{"url":"https://browser.example.lan/content","method":"POST","json":{"url":"https://example.com/"}}`.
  The rendered DOM comes back `text/html`, reduced to text and scrubbed like
  any page; `injection_suspected` is false unless the scan matched.
  `gotoOptions`, `waitForSelector` and `waitForTimeout` inside `json` wait
  for a slow page.
- **A script in one request** (`/function`):
  `{"url":"https://browser.example.lan/function","method":"POST","json":{"code":"export default async ({ page, context }) => { await page.goto(context.url); await page.click('#more'); return { data: await page.title(), type: 'text/plain' }; }","context":{"url":"https://example.com/"}}}`.
  The function runs against a fresh page and returns `{data, type}`; the
  session closes after it. Nothing survives between two calls.
- **A screenshot** (`/screenshot`):
  `{"url":"https://browser.example.lan/screenshot","method":"POST","json":{"url":"https://example.com/","options":{"type":"png"}},"save_to":"shots/example.png"}`.
  The PNG lands under `/embra/workspace/shots/`; `image_view` on that path
  shows it to Embra and to you. `image_view` keeps the long edge at 2576 px
  and the file under 1.5 MiB. A `fullPage` capture of a long page, with a
  long edge over 5152 px, comes back in tiles at full scale, four per call,
  the header naming each tile's coordinates; `part=2, 3, …` shows the next
  group. An image over 64 megapixels is refused before decode. Measured
  2026-10-17: a capture 4,385 px tall is shown whole and stays readable
  (1.7×); one 8,000 px tall shown whole was not (3.1×), and is four tiles
  now, readable down to the reference markers (2026-10-18). A `fullPage`
  capture shows only what had loaded: lazy-loaded pictures below the fold
  came out as empty frames and scroll-triggered fades as blank cards. The
  top-level `"scrollPage": true` together with
  `"gotoOptions": {"waitUntil": "networkidle0"}` scrolls the page and
  waits before the capture; on a long article every lazy-loaded picture
  was present (2026-10-18, the two options together). Text is still read
  through `/content`; the viewport or a top-level `selector` suit a single
  picture.
- **A PDF** (`/pdf`):
  `{"url":"https://browser.example.lan/pdf","method":"POST","json":{"url":"https://example.com/","options":{"printBackground":true,"format":"A4"}},"save_to":"files/example.pdf"}`,
  then `file_offer` on that path for a download in the web console. Nothing
  reads a PDF for the model.

### A session over several calls

Each route above runs in a browser of its own that is gone when the
request ends. A session that spans several tool calls (a login, a form, a
page read again after a click) runs over the DevTools endpoint with the
shipped `websocket`, and the connection is the session: Browserless starts
a browser when the connection opens and ends it when the connection
closes, and the brain keeps the connection open between calls
([GUARDIAN-WEBSOCKET-EXAMPLE.md](./GUARDIAN-WEBSOCKET-EXAMPLE.md)).

Ask in plain language; these are the inputs Embra sends, each the `input`
of one `guardian_call` to `websocket`. `<id>` is what `open` answered,
`<T>` and `<S>` what the two `Target` commands answered:

```json
{"op": "open", "url": "wss://browser.example.lan/chromium?timeout=300000"}
{"op": "send", "id": "<id>", "json": {"id": 1, "method": "Target.createTarget", "params": {"url": "about:blank"}}, "reply_id": 1}
{"op": "send", "id": "<id>", "json": {"id": 2, "method": "Target.attachToTarget", "params": {"targetId": "<T>", "flatten": true}}, "reply_id": 2}
{"op": "send", "id": "<id>", "json": {"id": 3, "sessionId": "<S>", "method": "Page.enable"}, "reply_id": 3}
{"op": "send", "id": "<id>", "json": {"id": 4, "sessionId": "<S>", "method": "Page.navigate", "params": {"url": "https://example.com/"}}, "reply_id": 4}
{"op": "receive", "id": "<id>", "until": "Page.loadEventFired", "wait_ms": 30000}
{"op": "send", "id": "<id>", "json": {"id": 5, "sessionId": "<S>", "method": "Runtime.evaluate", "params": {"expression": "document.title", "returnByValue": true}}, "reply_id": 5}
{"op": "close", "id": "<id>"}
```

- **The ids.** `Target.createTarget` answers
  `{"id":1,"result":{"targetId":"…"}}` and `Target.attachToTarget` with
  `flatten` `{"id":2,"result":{"sessionId":"…"}}`; a command to the page
  carries that `sessionId` beside its `id`, and so does its reply.
- **A command and its reply are one call.** `reply_id` answers the reply
  alone, in 1 to 210 ms on those runs. The session's events stay in the
  brain until a call reads them: `until` answered the seven frames through
  `Page.loadEventFired`, and after a second navigation with `Network`
  enabled, four seconds without a call left 21 frames waiting, read
  afterwards in the order they arrived.
- **What a page says is the page's.** `Runtime.evaluate` answered
  `{"id":5,"result":{"result":{"type":"string","value":"Example Domain"}},"sessionId":"…"}`.
  The tool scans every text frame and rewrites none;
  `injection_suspected` is false unless the scan matched.
- **A long reply.** An answer carries 64 KiB of JSON text unless the call
  asks more with `max_bytes`, 1 MiB at most. A reply of 200,104 bytes came
  back cut, with `"truncated":{"bytes":200104,"needs_max_bytes":320122}`,
  and the same command sent with `max_bytes` above that figure came back
  whole. A reply of 1.5 MB is over what a connection keeps: 1 MiB of it
  came back, marked. A screenshot or a PDF goes through `/screenshot` and
  `/pdf` with `save_to`, above.
- **The credential.** The bearer stored for the host rides the handshake.
  An `open` without it answered `{"error":"the server answered the
  handshake with HTTP 401, not a WebSocket","status":401,"body":"Bad or
  missing authentication."}`.

## Limits

- One `http_request` has a 10 s budget; `"timeout_ms": 45000` raises it, up
  to 60 s. Browserless's own `TIMEOUT` is 60 s in the compose.
- A page as text is cut at 64 KiB; a saved file at 12 MiB. A save is a 2xx
  only, whole or nothing, never over an existing file, never under `MEDIA/`
  or `.guardian/`.
- The guard checks the service host and injects the bearer there. The URLs
  the browser then visits (a `Page.navigate` of a DevTools session among
  them), and the ESM imports a `/function` script pulls over HTTP, are
  governed by the container's network alone, not by `/guardian egress`.
- A `/function` request is scripted interaction in a browser that ends
  with the request. A session over several calls is a WebSocket
  connection, and lasts as long as it. MCP server modules under the
  governance proxy stay Phase 3 ([ROADMAP.md](../ROADMAP.md)).
- Browserless's `TIMEOUT` (60 s in the compose) ends a session unless the
  URL carries `?timeout=<ms>`: a connection opened with `?timeout=300000`
  answered a command 85 s later, and one opened beside it without the
  parameter had been closed by the service by then (close code 1013,
  `Request has timed out`). On the OS side a connection is closed after
  10 minutes without a call and after 60 minutes.
- An open connection holds one of the service's `CONCURRENT` slots (2 in
  the compose) until it closes; the service queues what is over them.
  With both held, a third `open` got no answer to its handshake and was
  given up after 10 s (`… did not complete the handshake within 10 s`).
  `/guardian connections` lists what holds the slots and closes it.
- A new connection is a new browser: a page, a cookie or a login of one
  connection is not in the next. `Target.getTargets` on a connection
  opened after the first had closed listed one `about:blank` page of its
  own.
