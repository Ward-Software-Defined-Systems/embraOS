# Imported Intelligence — authoring reference

This folder holds **importable intelligence graphs**: complete IDENTITY+SOUL
definitions in `.graph.json` form. During Learning Mode, after the operator
profile (Phase 1) is collected, embraOS scans its import directories and — if
candidate files exist — offers to seal one of these graphs as the
intelligence's identity and soul instead of building them conversationally.

This document is the canonical **authoring contract**: what a valid file
looks like and how to craft one. What the OS does with an import (sealing,
projection, rendering, the migration ceremony) is documented in
[`docs/IDENTITY-GRAPH.md`](../docs/IDENTITY-GRAPH.md).

## Where files are read from at runtime

| Location | Purpose |
|---|---|
| `/embra/state/imported-intelligence/` | Operator-provisioned (`scripts/seed-state.sh --import-dir <path>` before first boot). Wins filename collisions. |
| `/usr/share/embra/imported-intelligence/` | Read-only, baked into the OS image from this folder at build time. |
| `EMBRA_IMPORT_DIR` (env) | Dev-mode override — when set, it is the only directory scanned. |

Only files ending in `.graph.json` are scanned. This README is never baked
into the image.

## File format

```json
{
  "_comment": "optional, ignored",
  "name": "Embra",
  "nodes": [
    {"id": "embra", "type": "self", "text": "Embra, a continuity intelligence: ..."},
    {"_comment": "=== marker objects like this may appear anywhere in either array ==="},
    {"id": "truth_over_comfort", "type": "value", "text": "Truth over comfort."}
  ],
  "edges": [
    {"src": "embra", "dst": "truth_over_comfort", "relation": "holds_value"}
  ]
}
```

- **Node**: `{"id", "type", "text"}` — `id` is the stable graph key (snake_case
  by convention), `type` is a free-form category, `text` is the node's full
  content.
- **Edge**: `{"src", "dst", "relation"}` — directed, referencing node `id`s;
  `relation` is free-form.
- **`_comment` markers**: any object in either array that lacks `id` (nodes)
  or `src` (edges) is skipped. Use them as section headers.
- **`name`** (optional, top-level): the display name seeded into the system
  configuration at import. When absent, the `self` node's `id` is title-cased
  (`meridian` → `Meridian`). The display name lives *outside* the sealed
  content either way — a later rename never touches the seal.

## Validation — what the importer enforces

A file failing any rule is reported (with every violation listed) and
excluded from the import selector:

1. **Exactly one node with `type: "self"`** — the anchor the intelligence
   hangs from. This is the only universal structural requirement.
2. **No dangling references** — every edge `src`/`dst` names an existing
   node `id`.
3. **No duplicate edges** — the `(src, dst, relation)` triple is unique.
4. **Valid ids** — non-empty, ≤ 512 bytes, no leading `_`, no NUL, and
   **not prefixed `user_`** (that namespace is reserved for the operator
   subgraph embraOS generates locally from the Phase-1 profile).
5. Parseable JSON with `nodes` and `edges` arrays.

## Vocabulary is yours

Node `type` and edge `relation` vocabularies are **per-intelligence** — the
importer treats them as opaque strings and the OS stores, traverses, and
renders them faithfully. The two committed examples are deliberate contrasts:

| | `Embra_IDENTITY-SOUL.graph.json` | `Meridian_IDENTITY-SOUL.graph.json` |
|---|---|---|
| Derivation | Production Embra (from `Embra_IDENTITY.md` + `Embra_SOUL.md`) | Authored control fixture (QNM dynamical-specificity test) |
| Nodes / edges | 100 / 354 | 100 / 349 |
| Node types | 13 (trait, value, soul_line, behavior, principle, anti_pattern, …) | 13 entirely different (belief, craft_virtue, drive, failure_mode, horizon, …) |
| Relations | 10 | 54 |
| Topology | Hub-and-spoke from the self node | Mesh + ring motifs |

Where a relation you need matches one of the knowledge graph's brain-created
edge types — `enables`, `refines`, `contradicts`, `depends_on`, `related_to`
— prefer reusing it: those edges are first-class citizens of the existing
traversal and retrieval semantics. Everything else imports as a free-form
relation and traverses identically.

## Crafting guidance

- **Soul lines must be nodes, not implications.** The sealed graph *is* the
  soul: an inviolable line that only exists as connotation across other
  nodes is not enforceable. Give each hard constraint its own node (Embra
  uses `type: "soul_line"`) and wire the traits/values that uphold it.
- **~100 nodes is a healthy scale.** Big enough for real structure, small
  enough that the full node set renders into the system prompt (nodes are
  rendered grouped by type; edges are traversed, not rendered).
- **Density beats breadth.** Embra's v3 file grew from 22 seed nodes by
  adding eight enrichment categories — behavioral manifestations, principles
  and maxims, anti-patterns, structural concepts, relational dynamics, voice
  sub-traits, temporal concepts, meta-cognition — and cross-wiring them.
  Anti-pattern nodes connected by `contradicts` edges are as defining as
  the values they oppose.
- **Text is the payload.** Every node's `text` should stand alone as a
  complete statement; the prompt renderer shows node texts, not ids.
- **The graph is sealed whole.** Identity and soul travel together in one
  file and are SHA-256-sealed together at import — immutable thereafter,
  verified at every boot. Post-seal evolution happens *additively*: the
  running intelligence links memories and knowledge into identity nodes; it
  cannot edit them.

## Provenance

Files here are authored artifacts — treat them like source. The two examples
were drafted in the embraOS-QNM-Core project (Embra reviewed against the
production documents it derives from; Meridian reviewed by the operator as a
coherent counter-identity). Keep derivation notes in the top-level
`_comment`.
