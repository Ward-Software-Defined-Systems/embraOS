# Routing the providers through WSDS Local AI Gateway

[WSDS Local AI Gateway](https://github.com/Ward-Software-Defined-Systems/Local-AI-Gateway) is a monitoring, inspection and data-loss-prevention reverse proxy for LLM traffic: a separate WSDS project that runs beside embraOS, never inside it. Its unified listener takes the standard Anthropic, Gemini and OpenAI-compatible paths and forwards each request, byte for byte and with the key the client sent, to the provider the path names. embraOS routes every provider request through it with one command, `/local-ai-gateway`, and back with another.

**Status.** Under evaluation (2026-10-25). The operator is evaluating the gateway as the companion of embraOS and for vendoring into this workspace, the way WardSONDB was vendored at `crates/wardsondb`. The unified listener and the cloud backends live on the gateway's `unified-router` branch; its `main` has the per-backend listeners for LM Studio and Ollama only. The live checks of this page are listed at the end; a row marked TBD has not been run.

## What the gateway does with a request

- **Forced routing by prefix.** The brain sends every request with the gateway's own prefix for the provider: `/anthropic/v1/messages`, `/gemini/v1beta/models/<id>:streamGenerateContent?alt=sse`, `/ollama/v1/chat/completions`, `/lmstudio/v1/models`. The gateway strips the prefix and forwards the rest of the path and the query to that backend, whatever the model is called (`crates/local-ai-gateway-proxy/src/route.rs`, `strip_provider_prefix`). Nothing is translated: Anthropic Messages, Gemini `generateContent` and the OpenAI-compatible chat reach their provider in their own format.
- **The key stays the brain's.** The gateway holds no provider keys. It forwards `x-api-key`, `x-goog-api-key` or the preset's bearer to the backend the prefix names and to nothing else, and redacts them in what it stores.
- **Recorded.** Every request is a row in the gateway's Requests tab: both bodies (redacted), the headers, the chunk timeline, tokens, time to first token, how it was routed, copy as curl. Cloud traffic is priced at list rates on the Cloud tab. Bodies are kept 7 days by default, in the gateway's SQLite database on the gateway host; the system prompt and the knowledge-graph context of every turn are in them.
- **Scanned.** Request bodies are scanned for secrets and personal data before they leave. On a cloud provider a secret **blocks** the request: the gateway answers 403 with the header `x-gateway-blocked: dlp` and a body in the provider's own error shape whose message is `request blocked by gateway DLP policy (rule <id>)`. embraOS shows that body as the turn's error (`HTTP 403: …`); the Security tab shows the finding with its JSON path into the request, such as `$.messages[3].content`. Headers are not scanned, so the probe and the key check, GETs without a body, are never blocked.
- **Streams pass through.** A 2xx body is forwarded chunk by chunk, never buffered; a client that hangs up drops the upstream stream, so `/stop` ends a generation through the gateway as it does direct. The gateway never follows a provider's redirect and sends none of its own on the proxy listeners.

## Setting it up

On the gateway host, in `config.toml` (`local-ai-gateway config init` writes one with every default):

- `[unified] listen` must name a non-loopback address, such as `"0.0.0.0:4000"`; the default `127.0.0.1:4000` is unreachable from the OS. The proxy listeners carry no authentication and speak plain HTTP; bind them to a network you trust.
- Each backend embraOS uses is `enabled`, and its `upstream` is the server that should answer: `[backends.anthropic]` and `[backends.gemini]` default to the providers' hosts; `[backends.ollama]` and `[backends.lmstudio]` default to `http://127.0.0.1:11434` and `http://127.0.0.1:1234` **on the gateway host**, not to the endpoint stored in embraOS.
- `[dlp.cloud] request_action` is `block` by default. The Security tab's test box shows what a text would trigger.

From an embraOS instance in QEMU, the host machine is `10.0.2.2`, and a name such as `axiom-app.ops.wsds` resolves through QEMU's forwarder at `10.0.2.3` when the host resolves it.

## `/local-ai-gateway`

| Command | Effect |
|---|---|
| `/local-ai-gateway` | Show the gateway: whether one is stored, its URL, and whether requests go through it |
| `/local-ai-gateway <url>` | Store the unified listener and turn it on. A bare host gets `http://` and `:4000`; `https://` keeps its implicit 443; a trailing `/` is dropped; a path is kept. A loopback host, a query or credentials in the URL are refused with the reason, and nothing is saved |
| `/local-ai-gateway on`, `/local-ai-gateway off` | Turn the stored gateway on or off; the URL is kept |
| `/local-ai-gateway reset` | Forget the gateway |

A change is saved to `SystemConfig.provider_gateway` (serde-additive; an instance from before the field reads as not configured) and takes effect on the next message: the provider is rebuilt from the configuration at every turn start. The health probe runs through the new route at once; `/status` and the web console's provider pill show its result, and the probe's `endpoint` names the host reached: `api.anthropic.com` direct, `axiom-app.ops.wsds:4000` through the gateway. `/provider` adds `Gateway: on (<url>)` or `Gateway: off (<url> kept)` to its status line. The first-boot wizard has no gateway step; set it after the first boot.

## What goes through, and what stays direct

| Provider | Prefix | Through the gateway while it is on |
|---|---|---|
| Anthropic | `/anthropic` | `POST /v1/messages` (every turn, the learning turns, the replicant check); `GET /v1/models/<id>` (the probe); `GET /v1/models` (the key check of `/provider --setup anthropic`) |
| Gemini | `/gemini` | `POST /v1beta/models/<id>:streamGenerateContent?alt=sse`; `POST` and `DELETE /v1beta/cachedContents…` (the explicit cache of Gemini 3.1 Pro); `GET /v1beta/models/<id>` (the probe); `GET /v1beta/models` (the key check); `POST /v1beta/interactions` (`image_generate`) |
| Ollama | `/ollama` | `POST /v1/chat/completions`; `GET /v1/models` (`/model` and the probe); `GET /api/ps` (the probe's context-window reading) |
| LM Studio | `/lmstudio` | `POST /v1/chat/completions`; `GET /v1/models` (`/model` and the probe) |

Ollama and LM Studio keep the endpoint stored in embraOS; the gateway's `[backends.<name>] upstream` is the server that answers while the gateway is on, and the stored endpoint is used again when it is off. `/model` lists the models the gateway's upstream serves.

Direct, whatever `/local-ai-gateway` says: the first-boot wizard's key check (no gateway is stored yet); the endpoint probe of `/provider --setup <ollama|lm_studio>`, which validates the endpoint the operator typed; the Guardian tools (`web_search`, `http_request`, `http_get`), which have their own egress rules (`/guardian egress`); the sentence-embedding model, which runs inside the OS and sends nothing.

## Caveats

- **The provider key reaches the gateway host in clear.** The proxy listeners speak plain HTTP. The brain says so when `/local-ai-gateway` stores an `http://` URL. An `https://` gateway behind a reverse proxy with a publicly trusted certificate passes the URL rule; the brain's provider clients trust their compiled-in public roots only, so a private CA does not work for them. TBD, untested.
- **Bodies are stored on the gateway host.** Redacted of what the DLP rules match, kept 7 days, in an unencrypted database. Treat the gateway's data directory as you treat embraOS's DATA.
- **A block is a failed turn.** The turn ends with the 403 and the rule's name; nothing was sent to the provider. A memory or a tool description that matches a rule blocks every turn until the finding is allowlisted on the gateway (`[[dlp.allowlist]]`) or the text changes. A block is never mistaken for a stale Gemini cache: the stale-cache retry fires only on a body that names a cached content. A blocked `cachedContents` creation makes the turn go uncached, and the stream request then names the rule.
- **A toggle is a new probe target.** The probe announces a standing failure once more after a toggle, says nothing on a healthy flip, and reads Ollama's context window again at its next run.
- **The gateway's own limits apply.** Requests are buffered in full with a 64 MiB cap (413 above it); open streams are cut five seconds into a gateway shutdown; the unified listener's `default_backend` never sees a request from embraOS, because every one carries a prefix.

## Live checks

Run on the operator's rebuilt image against the gateway at `axiom-app.ops.wsds:4000`, with its Requests tab open. A row reads `passed <date>` once it has been run; TBD until then.

| Check | Result |
|---|---|
| `/local-ai-gateway http://axiom-app.ops.wsds:4000`: the Info names the prefixes; `/provider` shows `Gateway: on`; `/status` names the gateway host; the probe's `GET /v1/models/<model>` is a row routed by prefix | TBD |
| An Anthropic turn through the gateway: a `POST /v1/messages` row with tokens, timing and spend; tool calls and `/stop` mid-stream as direct | TBD |
| Gemini 3.1 Pro: the `cachedContents` POST as `cloud_passthrough`, the `streamGenerateContent` row, no stale-cache retry in the `gemini::cache` log; Gemini 4 Argon: the probe through the gateway | TBD |
| Ollama: `/model` lists the gateway upstream's models; a turn is a `/ollama/v1/chat/completions` row; `/status` shows the context window from `/api/ps`. LM Studio with `/lmstudio` | TBD |
| DLP: a message carrying a fake `AKIA…` key pattern ends with `HTTP 403: … request blocked by gateway DLP policy (rule …)`; the Security tab shows the finding's path | TBD |
| `/provider --setup anthropic` with the gateway on: the key check's `GET /v1/models` is a row | TBD |
| `/local-ai-gateway off`: no new rows, `/status` names `api.anthropic.com`; `/local-ai-gateway on` again; `/local-ai-gateway reset` reads not configured | TBD |
| `image_generate` through `/gemini/v1beta/interactions` | TBD |

*Last updated: 2026-10-25.*
