# embraOS Manual

The operating manual for embraOS. The project landing page is [../README.md](../README.md).

## Project

- [ROADMAP.md](ROADMAP.md) — Phase 0–5 delivery status + the embra-guardian-v1 branch.
- [CHANGE-LOG.md](CHANGE-LOG.md) — Chronological merge log for `main`, anchored on tagged releases.

## Getting Started

- [QUICK-START.md](QUICK-START.md) — Build the QEMU image from source (Ubuntu 24.04 / 26.04), the musl cross-toolchain, the build pipeline, the Config Wizard, and operational notes (terminal size, image search order, clean first boot, port forwarding, backup & restore).
- [OPERATION.md](OPERATION.md) — What running it feels like, the session model, keyboard shortcuts, current limitations.

## Reference

- [COMMAND-REFERENCE.md](COMMAND-REFERENCE.md) — Every slash command.
- [TOOL-REFERENCE.md](TOOL-REFERENCE.md) — All 94 built-in tools by category, plus workspace/GitHub/SSH safety notes.
- [RECOMMENDED-LOCAL-MODELS.md](RECOMMENDED-LOCAL-MODELS.md) — Per-family model guidance for the Ollama / LM Studio backends.

## Internals

- [SYSTEM-DESIGN.md](SYSTEM-DESIGN.md) — The 7-layer continuity architecture, the four LLM providers, reasoning controls, prompt caching.
- [KNOWLEDGE-GRAPH.md](KNOWLEDGE-GRAPH.md) — Cross-session memory graph: auto-derived edges, density rationale, promotion path, auto-enrichment, retrieval ranking, the 10 `knowledge_*` tools.
- [OPEN-PROBLEMS.md](OPEN-PROBLEMS.md) — Unresolved Phase 1–3 design questions tracked at the architecture level.

## Guardian — Dynamic Tools & the Replicant Check

- [REPLICANT-CHECK.md](REPLICANT-CHECK.md) — The soul-spec gate every dynamic tool passes before it compiles: how it works, both authoring paths, and how to test it.
- [GUARDIAN-TOOL-EXAMPLES.md](GUARDIAN-TOOL-EXAMPLES.md) — Paste-ready dynamic-tool modules (embra-guardian-v1).
- [GUARDIAN-ADVANCED-EXAMPLE.md](GUARDIAN-ADVANCED-EXAMPLE.md) — A worked, prompt-injection-hardened end-to-end Guardian tool.
- [GUARDIAN-KG-SCAN-EXAMPLE.md](GUARDIAN-KG-SCAN-EXAMPLE.md) — `kg_scan`, the first intelligence-proposed tool: scans a `knowledge_dump` JSONL for structural patterns, fed through `guardian_call`'s `data_file` bridge.

## Appendix

Design lineage, project provenance, and license live in [../README.md](../README.md).
