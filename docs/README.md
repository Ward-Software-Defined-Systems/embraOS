# embraOS Manual

The operating manual for embraOS. The project landing page is [../README.md](../README.md). The manual is in five directories, one per subject, and the project-level documents at the top.

## Project

- [ROADMAP.md](ROADMAP.md) — Phase 0–5 delivery status, sprint by sprint, the post-Sprint-5 increments and the code-review pit stop.
- [CHANGE-LOG.md](CHANGE-LOG.md) — Chronological merge log for `main`, anchored on tagged releases.
- [SYSTEM-DESIGN.md](SYSTEM-DESIGN.md) — The 7-layer continuity architecture, the four LLM providers, reasoning controls, prompt caching.
- [OPEN-PROBLEMS.md](OPEN-PROBLEMS.md) — Unresolved Phase 1–3 design questions tracked at the architecture level.

## Build — `BUILD/`

- [BUILD/QUICK-START.md](BUILD/QUICK-START.md) — Build the QEMU image from source (Ubuntu 24.04 / 26.04), the musl cross-toolchain, the build pipeline, the Config Wizard, and operational notes (build overrides, terminal size, image search order, clean first boot, port forwarding, backup & restore).
- [BUILD/AARCH64-BUILD.md](BUILD/AARCH64-BUILD.md) — Apple Silicon hosts: the aarch64 image via `build-image-aarch64.sh` and the named-volume Docker Buildroot flow, HVF-accelerated QEMU, the Mac backup and seeding wrappers.
- [BUILD/INTEL-MAC-BUILD.md](BUILD/INTEL-MAC-BUILD.md) — Intel Mac hosts: the x86_64 image through the same Docker Buildroot flow, HVF-accelerated QEMU, the Mac backup wrapper.

## Operations — `OPERATIONS/`

- [OPERATIONS/OPERATION.md](OPERATIONS/OPERATION.md) — What running it feels like, the session model, keyboard shortcuts, current limitations.
- [OPERATIONS/COMMAND-REFERENCE.md](OPERATIONS/COMMAND-REFERENCE.md) — Every slash command.
- [OPERATIONS/TOOL-REFERENCE.md](OPERATIONS/TOOL-REFERENCE.md) — All 118 built-in tools by category, plus workspace/GitHub/SSH safety notes.
- [OPERATIONS/LOCAL-AI-GATEWAY.md](OPERATIONS/LOCAL-AI-GATEWAY.md) — Routing every provider through WSDS Local AI Gateway with `/gateway`: what goes through, what stays direct, what the gateway records and blocks; under evaluation for vendoring.

## Local models — `LOCAL-MODELS/`

- [LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md](LOCAL-MODELS/RECOMMENDED-LOCAL-MODELS.md) — Vetted models and server configuration for the Ollama / LM Studio backends.
- [LOCAL-MODELS/NEMOTRON-VS-QWEN3.8-BENCHMARK.md](LOCAL-MODELS/NEMOTRON-VS-QWEN3.8-BENCHMARK.md) — A long-context benchmark on the Mac Studio: Nemotron 3.5 Super against Qwen3.8 27B, and Qwen3.8 at 4, 8 and 16 bits on Ollama and LM Studio.
- [LOCAL-MODELS/LOCAL_MODEL_SETTINGS/](LOCAL-MODELS/LOCAL_MODEL_SETTINGS/) — Importable server-side presets for the local backends: the LM Studio preset for Qwen3.8 27B.

## The two graphs — `MULTI-GRAPH/`

- [MULTI-GRAPH/KNOWLEDGE-GRAPH.md](MULTI-GRAPH/KNOWLEDGE-GRAPH.md) — Cross-session memory graph: auto-derived edges, density rationale, promotion at creation, link candidates, auto-enrichment, retrieval ranking, the in-OS semantic-similarity layer, the 12 `knowledge_*` tools.
- [MULTI-GRAPH/IDENTITY-GRAPH.md](MULTI-GRAPH/IDENTITY-GRAPH.md) — The KG-native identity: sealed IDENTITY+SOUL graph, projection + boot reconcile, prompt rendering, the Learning-Mode import, and the re-seal migration ceremony. Authoring contract: [`Imported_Intelligence/README.md`](../Imported_Intelligence/README.md).

## Guardian — `GUARDIAN/`

Dynamic tools and the replicant check.

- [GUARDIAN/REPLICANT-CHECK.md](GUARDIAN/REPLICANT-CHECK.md) — The soul-spec gate every dynamic tool passes before it compiles: how it works, both authoring paths, and how to test it.
- [GUARDIAN/GUARDIAN-TOOL-EXAMPLES.md](GUARDIAN/GUARDIAN-TOOL-EXAMPLES.md) — Paste-ready dynamic-tool modules (embra-guardian-v1).
- [GUARDIAN/GUARDIAN-ADVANCED-EXAMPLE.md](GUARDIAN/GUARDIAN-ADVANCED-EXAMPLE.md) — The shipped flagship: a worked, prompt-injection-hardened end-to-end Guardian tool.
- [GUARDIAN/GUARDIAN-HTTP-REQUEST-EXAMPLE.md](GUARDIAN/GUARDIAN-HTTP-REQUEST-EXAMPLE.md) — The second shipped tool: `http_request`, a guarded curl for pages, resources and API work; credentials host-side, private hosts by allowlist, `save_to` for a file into the workspace.
- [GUARDIAN/M365-SHARED-MAILBOX.md](GUARDIAN/M365-SHARED-MAILBOX.md) — One Microsoft 365 shared mailbox for `http_request`: an Entra application scoped with Exchange Online RBAC for Applications, the token stored with `/guardian secret`, and what the Graph requests look like.
- [GUARDIAN/BROWSER-SERVICE.md](GUARDIAN/BROWSER-SERVICE.md) — A headless browser beside the OS: Browserless behind Caddy in Docker Compose ([`GUARDIAN/BROWSER-SERVICE/`](GUARDIAN/BROWSER-SERVICE/)), reached with `http_request`; pages as text, screenshots and PDFs into the workspace with `save_to`.
- [GUARDIAN/GUARDIAN-KG-SCAN-EXAMPLE.md](GUARDIAN/GUARDIAN-KG-SCAN-EXAMPLE.md) — `kg_scan`, the first intelligence-proposed tool: scans a `knowledge_dump` JSONL for structural patterns, fed through `guardian_call`'s `data_file` bridge.

## Appendix

Design lineage, project provenance, and license live in [../README.md](../README.md).
