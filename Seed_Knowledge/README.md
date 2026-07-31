# Seed Knowledge Packs (`knowledge.v1`)

Curated packs of knowledge-graph nodes and edges that the brain loads into
the **live** knowledge collections (`memory.semantic` / `memory.procedural`
+ `memory.edges`) on every boot. Two packs ship committed: `embraos-kg`
teaches an instance how its own memory works, and `embraos-guardian` is
the Guardian tool-authoring reference (the sandbox contract, the vendored
guest APIs, and the pitfalls where general Rust training conflicts with
the scoped `no_std` environment); operators can ship their own packs the
same way. This is the `Imported_Intelligence/` pattern applied to
knowledge instead of identity — packs are **mutable knowledge**, never
sealed, and never touch `identity.graph`.

This folder's `*.knowledge.json` files are baked read-only into the OS
image (`post_build.sh` → `/usr/share/embra/seed-knowledge/`). This README
is host-side documentation and is not baked.

## Where packs load from

| Source | Path | Notes |
|---|---|---|
| Rootfs (baked) | `/usr/share/embra/seed-knowledge/` | the committed packs |
| STATE (drop-in) | `/embra/state/seed-knowledge/` | operator packs; **wins filename collisions** with the rootfs |
| Dev override | `EMBRA_SEED_DIR` env | when set, the **only** directory scanned |

`scripts/seed-state.sh --seed-dir /path/to/packs` copies packs into STATE
before first boot. Files must end in `.knowledge.json` and stay under
4 MiB. Two files whose packs share a `name` conflate the loader's
fast-path counts — the first file (alphabetical) wins and the other is
skipped with a boot-journal warning.

## The ensure-present contract (read this before editing a live instance)

The boot reconcile checks **presence by node `_id` only**. Consequences:

- **Edits stick.** `knowledge_update` on a seeded node is never overwritten
  — the reconcile inserts missing docs, it never patches existing ones.
- **Deletions resurrect.** Deleting (or `knowledge_merge`-ing away) a node
  whose id is still listed in a pack brings it back at the next boot. To
  remove seeded knowledge permanently, revise the pack.
- **Revisions ship as new ids.** A released pack that changes a claim
  should introduce the new text under a NEW id and drop the old id — the
  old node is then no longer pack-listed, so an operator deletion sticks
  (and existing instances keep the old node until the operator removes or
  edits it).

Seeded nodes are otherwise **ordinary graph citizens**: retrieval,
enrichment, traversal, `knowledge_audit`, `knowledge_merge`, and
`knowledge_update` treat them exactly like promoted knowledge. Their
provenance is visible (`origin: "knowledge_seed"` + `pack` on the node;
`metadata.origin`/`metadata.pack` on edges) and counted by
`knowledge_graph_stats`.

## Format

```json
{
  "format": "knowledge.v1",
  "name": "my-pack",
  "description": "optional one-liner",
  "nodes": [
    { "_comment": "objects without an 'id' are comment markers" },
    {
      "id": "seed_example_fact",
      "kind": "semantic",
      "category": "fact",
      "content": "One self-contained claim, written to be used in conversation.",
      "tags": ["lowercase", "single-word"]
    },
    {
      "id": "seed_example_proc",
      "kind": "procedural",
      "title": "Short verb phrase",
      "description": "What this procedure achieves.",
      "steps": ["First step", "Second step"],
      "outcomes": { "success": "…", "failure": "…" },
      "tags": ["topic"]
    }
  ],
  "edges": [
    { "src": "seed_example_fact", "dst": "seed_example_proc", "relation": "enables", "weight": 0.9 }
  ]
}
```

Field rules (validation collects **every** violation at once and reports
them in the boot journal; an invalid pack is skipped, never partially
applied):

- `format` must be `"knowledge.v1"`; `name` non-empty (it namespaces the
  loader's bookkeeping — keep it stable across releases of the same pack).
- **Node ids**: non-empty, ≤ 512 bytes, no leading `_`, no NUL, unique
  within the pack; the `user_` prefix is reserved for the operator
  profile. A `seed_<pack>_<topic>` convention is recommended (not
  enforced) — ids share the `_id` namespace with server-minted UUIDs and
  identity slugs.
- `kind: "semantic"` needs `category` (one of `fact`, `preference`,
  `decision`, `observation`, `pattern`) and non-empty `content`.
- `kind: "procedural"` needs non-empty `title` and `description`;
  optional `steps` (array of strings, written 1-based into the structured
  shape) and `outcomes` (both `success` and `failure`, or neither).
- `tags` is required on every node (an empty array is allowed). Lowercase,
  single-word, reusing the instance's established vocabulary — exact
  lowercase match is what makes seeded knowledge retrievable.
- **Edges** reference in-pack ids only; `relation` is free-form
  (the built-in vocabulary — `enables`, `contradicts`, `refines`,
  `depends_on`, `related_to` — is recommended so the edges read uniformly
  next to brain-authored ones); `weight` optional, default `1.0`, must be
  in `(0.0, 1.0]`; duplicate `(src, dst, relation)` triples are rejected.

## What the loader does with a pack

Per boot, per pack: two filtered counts + one spot-probe on the healthy
path; on mismatch, an insert-missing walk writes absent nodes (and gives
each **freshly inserted** node one auto-edge derivation pass, wiring its
tags into the operator's existing knowledge), then probe-inserts absent
edges. Everything is warn-don't-fail — a broken pack never blocks boot.

Every committed pack in this folder is parsed and validated by the test
`committed_seed_packs_validate` (`crates/embra-brain/src/knowledge/seed.rs`),
so `cargo test` fails before an invalid pack can ship in an image.
