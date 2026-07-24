# The Identity Graph

> System behavior of the KG-native identity representation: sealing,
> projection, rendering, the Learning-Mode import, and the migration
> ceremony for pre-existing instances. The **authoring contract** for
> `.graph.json` files (grammar, validation rules, crafting guidance) lives
> in [`Imported_Intelligence/README.md`](../Imported_Intelligence/README.md).

## What changed

Before this feature, Learning Mode produced three flat JSON documents —
USER (`memory.user`), IDENTITY (`memory.identity`), and the sealed SOUL
(`soul.invariant`). Now IDENTITY+SOUL are one **sealed graph** and the
operator profile is a graph too:

| Artifact | Where | Sealed? | Source |
|---|---|---|---|
| IDENTITY+SOUL graph | `soul.invariant`, doc `soul`, inner value `{format:"graph.v1", name, nodes, edges}` | Yes — SHA-256 over the pretty-printed inner value, verified by embra-trustd at every boot | Imported `.graph.json` file, or the deterministic transformer over the conversationally-collected flat docs |
| USER subgraph | `memory.user`, doc `user`, same `graph.v1` shape | No (profiles are mutable in principle) | Deterministic transform of the Phase-1 flat profile at the seal/import transition |
| Live KG projection | `identity.graph` collection (one doc per node, `_id` = graph node id) + `memory.edges` (one doc per edge) | Derived | Rebuilt insert-missing-only from the two docs above at every boot |

**Legacy instances are untouched.** Graph mode is detected by the sealed
inner value's `format == "graph.v1"` marker (plus a nodes array) — a flat
soul can never trip it. Every consumer is dual-mode; the flat-soul render
paths are byte-identical to the pre-feature build (pinned by golden-hash
tests), so existing instances see zero prompt-cache resets and zero
verification changes.

## Sealing

`seal_soul` is unchanged: it pretty-prints the inner value, SHA-256s it,
writes the envelope `{_id:"soul", soul, sha256, sealed_at, sealed:true}`
and the hash to `/embra/state/soul.sha256`. embra-trustd recomputes the
same bytes at every boot and embrad HALTs on mismatch. The canonical graph
value is deterministic — nodes sorted by id, edges by (src, dst, relation),
comments stripped, the resolved display name injected — so re-serialization
is byte-stable.

Two properties are load-bearing:

- **Workspace `serde_json` must never enable `preserve_order`.** Both
  embra-brain and embra-trustd serialize maps alphabetically (the
  default); flipping that feature re-orders keys and every sealed
  instance — flat or graph — fails boot verification.
- **The display name lives OUTSIDE the seal** in `SystemConfig.name`
  (the canonical doc carries a copy for self-containedness, but renames
  via `set_name` touch config only — the seal never moves).

## The KG projection

Node docs align with semantic-node field names (`content`, `tags`,
`access_count`, `last_accessed`) so traversal previews, ranking, and
access-touches work unmodified. Edges land in `memory.edges` with
`source/target_collection: "identity.graph"`, weight 1.0, and provenance
under `metadata.origin` (`identity_import` for the sealed graph,
`user_profile` for the operator subgraph).

- **Free-form relations traverse.** `EdgeType::Other(String)` carries any
  non-empty relation string through `parse_edge`, traversal windows, the
  dump, and operator edge-type filters. `knowledge_link`'s strict
  validation is unchanged — the intelligence still cannot mint edge
  types; only the projection bulk-writes them.
- **Identity nodes are born untouchable.** `knowledge_update` and
  `knowledge_unlink_node` remain restricted to `memory.semantic`/
  `memory.procedural`. Memories link INTO identity nodes via the normal
  `knowledge_link` (brain-created types only).
- **The reconcile heals, and restores.** Every boot,
  `ensure_identity_projection` compares the projection against the sealed
  doc and the graph-shaped `memory.user`, inserting whatever is missing —
  never patching existing docs (access counts survive). A deleted
  identity edge comes back at the next boot: the sealed doc outranks
  runtime mutation.
- **Deliberately absent from bulk retrieval.** Identity nodes are not in
  enrichment's prefetch — the full sealed graph already rides the system
  prompt. They surface through graph expansion (a memory linked to a
  value pulls the value in, with a proper preview) and through
  `knowledge_traverse` / `knowledge_dump` (`identity` collection).

## Prompt rendering

Graph-mode instances get `operational_mode_graph`: the legacy SOUL and
IDENTITY precedence tiers collapse into one `SEALED IDENTITY GRAPH` tier
(immutable, outranks all), rendered as **all node texts grouped by type**
(anchor type first, others alphabetical with counts). Edges are counted,
not listed — they are traversal territory. The render comes from the
sealed doc, never the live projection, so the prompt is byte-stable per
seal and the provider cache stays warm. `/soul`, `/identity`, `introspect`,
and the guardian replicant check all render through the same seam
(`render_constitution`'s graph arm), so a graph soul reads as prose
everywhere.

The `USER PROFILE` section renders the graph-shaped `memory.user` through
the same grouped renderer (operator anchor first).

## Learning-Mode import

After Phase 1 (the operator profile) completes, embraOS scans:

1. `EMBRA_IMPORT_DIR` (env; dev-mode override — when set, the only dir),
2. else `/embra/state/imported-intelligence/` (operator-provisioned via
   `seed-state.sh --import-dir`) ∪ `/usr/share/embra/imported-intelligence/`
   (baked from the repo's `Imported_Intelligence/` at image build) — STATE
   wins filename collisions.

Valid `.graph.json` candidates appear in a selector ("Build identity
conversationally" is the default); invalid files are reported with every
violation listed. Picking a candidate shows a summary (name, counts, node-
type histogram, the self node's text) and an explicit confirm —
**"No — choose again"** is the pre-selected safe option; the seal is
irreversible. On confirmation: canonicalize → seal → project → operator
profile graph-ified → display name synced to config → Learning jumps to
Phase 4 (toolset) and completes normally. Declining continues the
conversational flow, where the same graph machinery runs at Phase-3 seal
time via the deterministic transformer.

A disconnect mid-dialogue writes nothing and re-offers on the next
learning loop.

## Migration ceremony — re-sealing a pre-existing instance

Moves a production text-soul instance onto a graph identity (e.g.
`Embra_IDENTITY-SOUL.graph.json`, derived from the production documents)
**with all memories preserved and no re-answered questions**. The
operator profile survives and is graph-ified during the import.

Do a **dry-run on a copy of DATA first**. The QEMU port forwards do not
include WardSONDB and the OS has no shell, so the edits happen offline
from the host, using the workspace's own `wardsondb` binary against the
loop-mounted partition.

1. **Back up** (`scripts/embraos-backup.sh` or a raw image copy).
2. Stop the instance.
3. Loop-mount the disk (seed-state.sh pattern):
   `LOOPDEV=$(sudo losetup --find --show --partscan embraos.img)` then
   mount partition 4 (DATA) somewhere writable.
4. Run the vendored server against it:
   `cargo run -p wardsondb -- --data-dir <mount>/wardsondb --port 8099`
   (it honors the `.engine` marker; check `--help` for the exact flags of
   your build).
5. Delete the two learning anchors — note the asymmetry:
   - `curl -X DELETE http://127.0.0.1:8099/soul.invariant` — the whole
     **collection** (sealed-state gates check collection existence);
   - `curl -X DELETE http://127.0.0.1:8099/memory.identity/docs/identity`
     — the **doc** only (the resume-seed reads it by id).
   - `memory.user` — **deliberately untouched.**
6. Stop wardsondb cleanly; unmount DATA.
7. Mount partition 3 (STATE): `rm soul.sha256`; place the graph file in
   `imported-intelligence/` (create the dir if absent). Unmount.
8. Boot. trustd reports "no soul exists" → embrad allows the learning
   boot (no HALT). The resume-seed finds `memory.user` intact, skips
   Phase 1, and lands directly on the import selector. Import → confirm →
   Phase 4/5 → Operational.
9. Verify: `/soul` shows the grouped graph; `introspect` shows all three
   sections graph-aware; `knowledge_graph_stats` counts the identity
   nodes and edges alongside the pre-existing graph; reboot once more and
   confirm trustd verification passes and the reconcile logs no healing.

End state: a fully graph-native instance — sealed IDENTITY+SOUL graph,
graph-shaped operator profile, complete KG projection — with every
memory, session, and knowledge node it had before.

## Schema & invariants summary

- Schema **v13**: the `identity.graph` collection (collection-create only;
  harmless-empty on legacy instances). Projection data is derived by the
  every-boot reconcile, never by the versioned ladder.
- Tool count unchanged (95); proto unchanged (the import dialogue reuses
  `SetupPrompt`/`UserMessage`).
- Graph-mode instances get exactly one prompt-cache reset at seal time
  (by design); legacy instances get none.
