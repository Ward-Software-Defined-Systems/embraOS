# A browser service beside the OS

embraOS ships no browser. Buildroot has no Chromium for musl, a browser on
the live OS would run untrusted JavaScript the egress guard never sees, and
the VM has 4 GB. The browser runs as a service on your network instead, and
Embra reaches it with the shipped `http_request`
([GUARDIAN-HTTP-REQUEST-EXAMPLE.md](./GUARDIAN-HTTP-REQUEST-EXAMPLE.md)):
rendered pages as text, screenshots and PDFs into the workspace with
`save_to`, scripted interaction in one request. The files under
[`BROWSER-SERVICE/`](./BROWSER-SERVICE/) run it with Docker Compose.

**Validated:** 2026-10-17. Browserless `v2.57.1` behind Caddy `2.11.7` with
Docker Compose on an Ubuntu 24.04 host, Caddy's internal CA seeded into
STATE; from a QEMU image, every use and refusal on this page, the 60 s
budget, and the full-page measurements under *A screenshot*. The tiles of
`image_view` named there: validated 2026-10-18 in three rounds on the same
captures (seven tiles of 800×2366–2383 px for a 16,642 px page, the cuts
on blank lines and in the gap beside a figure, `part=2`, the refusals, the
mobile UI, the recapture with `scrollPage`).

## What runs

Two containers, one published port:

| Container | Image | Published | Role |
|---|---|---|---|
| `browserless` | `ghcr.io/browserless/chromium:v2.57.1` | nothing | headless Chromium with a REST API on `:3000`, reachable from `caddy` only |
| `caddy` | `caddy:2.11.7` | `443` | TLS for `BROWSER_HOST`, proxied to `browserless:3000` |

The Guardian guard admits https only, for a private host too (`check_url`,
`crates/embra-guardian/src/caps.rs`); Caddy is that https. Browserless is
licensed "SSPL-1.0 OR Browserless Commercial License"; read it before a
commercial deployment. Its REST routes (`/content`, `/screenshot`, `/pdf`,
`/function`, `/scrape`) are in the open-source image's source tree; the
running container lists what it serves at `https://<BROWSER_HOST>/docs`,
the check before relying on a route.

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
name (Caddy issues a certificate for an IP). QEMU's user-mode network
forwards DNS to the host's resolver.

## The certificate

The OS trusts the operator's CA drop-ins in `/embra/state/ca-certificates/`
on the Guardian transport
([QUICK-START.md — Self-hosted git servers (private CA)](../BUILD/QUICK-START.md#self-hosted-git-servers-private-ca)).
Two ways, one `tls` line in `caddy/Caddyfile`:

- **Your own CA.** Issue a certificate for `BROWSER_HOST`, mount it at
  `/certs` (the commented volume in the compose) and switch the `tls` line
  to the two files. When that CA's root is already in STATE, nothing on the
  OS changes.
- **Caddy's internal CA** (`tls internal`, the default). Caddy signs a
  certificate for `BROWSER_HOST` with a root it keeps in the `caddy_data`
  volume. Copy the root into the OS, with the VM stopped:

  ```text
  docker compose exec caddy cat /data/caddy/pki/authorities/local/root.crt > ca/browser-root.crt
  ./scripts/seed-state.sh --ca-dir ca
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
`text/html`.

## Tell the OS

```text
/guardian egress allow browser.example.lan
/guardian secret browser.example.lan Authorization "Bearer <BROWSERLESS_TOKEN>"
```

The first admits the private host; the second stores the bearer the host
adds to every `http_request` to that host
([COMMAND-REFERENCE.md](../OPERATIONS/COMMAND-REFERENCE.md)). The token never enters a
tool's input, the turn trace or the session history. Browserless also takes
`?token=`; do not use it, a URL is logged.

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

## Limits

- One `http_request` has a 10 s budget; `"timeout_ms": 45000` raises it, up
  to 60 s. Browserless's own `TIMEOUT` is 60 s in the compose.
- A page as text is cut at 64 KiB; a saved file at 12 MiB. A save is a 2xx
  only, whole or nothing, never over an existing file, never under `MEDIA/`
  or `.guardian/`.
- The guard checks the service host and injects the bearer there. The URLs
  the browser then visits, and the ESM imports a `/function` script pulls
  over HTTP, are governed by the container's network alone, not by
  `/guardian egress`.
- Sessions do not persist between calls: `/function` is scripted
  interaction, not a browsing session. A persistent session is a Phase 3
  question ([ROADMAP.md](../ROADMAP.md), the MCP server modules under the
  governance proxy).
