# Guardian Example — `kg_scan` (the first intelligence-proposed tool)

`kg_scan` scans a JSONL dump of the knowledge graph for four structural
patterns. It is the first Guardian tool authored by the intelligence
itself: drafted on a production instance via `guardian_propose`,
`syn`-validated, judged against the sealed soul by the replicant check
(fail-closed), and compiled only after operator approval — the full gated
self-authoring path ([REPLICANT-CHECK.md](./REPLICANT-CHECK.md)). The
intelligence iterated it v1 → v5 in one day (2026-06-03): v1's
temporal-gating matcher was a stub; v5 added the Crockford-Base32
timestamp decode with hyphen handling and corrected the
sovereignty-shield matcher.

The module below is committed **byte-faithful to the v5 artifact**. The
prose around it corrects three of the authoring intelligence's own
misconceptions instead of editing its code:

1. **It declares no capabilities.** The tool's original README claimed
   `http_get` + `web_search`; the module has no `GUARDIAN_CAPS` const and
   makes no `host::` calls. It is pure compute.
2. **`data` is content, not a path.** The guest runs sandboxed in
   `wasmtime` with no filesystem — `fn run` iterates `data.lines()`
   directly. The original README passed file *paths* in `data`, which
   never worked. `guardian_call`'s `data_file` bridge (below) is what
   feeds a dump file in.
3. **embraOS `_id`s are UUIDv7 hex, not ULIDs.** The module's comments
   and decoder assume ULIDs; the decode is still order-correct on UUIDv7
   — see the timestamp note below.

> Like every module in these docs, this one is checked against the real
> validator by `crates/embra-guardian/tests/doc_examples_validate.rs` —
> what's here is exactly what the gate accepts. Read
> [GUARDIAN-TOOL-EXAMPLES.md](./GUARDIAN-TOOL-EXAMPLES.md) first for the
> contract and the `json` helper API.

## The four patterns

| Pattern | Detects | Edge types used |
|---|---|---|
| `sovereignty_shield` | A node with at least one incoming `enables` edge and zero outgoing edges — enabled by others while depending on nothing. | `enables` |
| `double_helix` | Two nodes that `enables`-link each other in both directions — a mutually dependent pair. | `enables` |
| `focal_trick` | A node that is the target of both a `refines` edge and a `contradicts` edge — a contested focal point. | `refines`, `contradicts` |
| `temporal_gating` | A `depends_on` edge whose source node is newer than its target, verified by decoding both `_id` timestamps. | `depends_on` |

Counting is node-centric — a node with N qualifying edges counts once.
Matchers inspect direct adjacency only; there is no transitive traversal.

## Producing a dump — `knowledge_dump`

`kg_scan` reads the JSONL format the built-in `knowledge_dump` tool
writes to `/embra/workspace/KG_DUMPS/kg-dump-<utc>.jsonl`:

- **Line 1** is a `{"type":"meta",...}` header (dump provenance).
  `kg_scan` keys on `type` and skips unknown record types.
- **Node lines** — `{"type":"node","_id":"…","collection":"memory.semantic","data":{…}}`.
  The matchers read only `type`/`_id`/`collection`; slim dumps
  (`include_payload=false`) omit `data`.
- **Edge lines** — the stored edge doc spread at top level plus
  `"type":"edge"`. The matchers read `source_collection`/`source_id`/
  `target_collection`/`target_id`/`edge_type`; `weight`, `metadata`, and
  `created_at` ride along unused.

Scan the **slim profile**, not a full dump (limits below):

```text
knowledge_dump {
  "collections": ["semantic", "procedural", "edges"],
  "edge_types": ["enables", "contradicts", "refines", "depends_on"],
  "include_payload": false
}
```

This restricts the edge pass to the four brain-created types the matchers
use (the auto-derived `same_session`/`temporal`/`tag_overlap` bulk is
noise to this scanner) and drops node payloads the matchers never read.
One subtlety: `temporal_gating` requires the *target node record* to be
present in the dump, so a `depends_on` edge pointing at a raw episodic
entry will not fire under this profile — add `"entries"` to `collections`
if your graph links `depends_on` edges to unpromoted entries.

## Feeding the dump to the guest — `data_file`

The guest cannot open files. `guardian_call` takes an optional
`data_file`: a path inside `/embra/workspace` that the brain reads
host-side (2 MiB cap) and injects as the `input.data` string before
dispatch — the dump content never transits the model context. The wasm
sandbox is unchanged; the guest still just receives its input string.

```text
guardian_call {
  "action": "invoke",
  "tool": "kg_scan",
  "data_file": "/embra/workspace/KG_DUMPS/kg-dump-<utc>.jsonl",
  "input": { "action": "scan" }
}
```

`input.data` must not be set when `data_file` is — one source for `data`.

## Actions

| Action | Args | Returns |
|---|---|---|
| `help` | — | Pattern list with descriptions (no data needed) |
| `scan` | `data` | All four detectors: per-pattern `count` + up to 10 `examples` |
| `find_isomorphs` | `data`, `pattern` | Every match for one named pattern |
| `explain` | `pattern` | The description for one named pattern (no data needed) |

## Timestamp decoding on UUIDv7 ids

The module decodes `_id` timestamps as ULIDs (Crockford Base32, first 10
non-hyphen chars). embraOS `_id`s are actually UUIDv7 — hyphenated hex.
The decode still orders correctly: the first 10 hex chars cover the top
40 bits of the 48-bit millisecond timestamp, and hex digits map
monotonically into the module's Crockford table (`0-9` → `0x00-0x09`,
`a-f` → `0x0A-0x0F`), so decoded values order exactly as the timestamps
do. The dropped low 8 bits mean pairs within ~256 ms read as equal —
`temporal_gating` requires strictly newer, so near-simultaneous pairs
are skipped, never misreported.

## Limits

- **Guest budget:** 5 s epoch deadline, 8 MiB bump arena with a no-op
  `dealloc` (JSON parsing allocates a multiple of the input size), and
  matchers that are O(nodes × edges) and worse. Scan slim,
  edge-type-filtered dumps; keep bridged data near 1 MiB.
- **Bridge cap:** `data_file` refuses files over 2 MiB with a pointer to
  the slim profile. A full production dump (all collections, full
  payloads) is a backup/export artifact, not a scan input.
- **Retention:** dumps accumulate in `/embra/workspace/KG_DUMPS/` with no
  rotation — the intelligence removes stale ones with `file_delete`.

## The module

```rust
// guardian-tool: kg_scan
// Structural pattern scanner for the knowledge graph.
// Detects four isomorphic patterns: Sovereignty Shield, Double Helix,
// Focal Trick, and Temporal Gating.
const GUARDIAN_NAME: &str = "kg_scan";
const GUARDIAN_DESC: &str = "Scan a knowledge-graph JSONL dump for four structural isomorphs: Sovereignty Shield (node with only incoming enables, zero outgoing), Double Helix (bidirectional enables pair), Focal Trick (node both refined_by and contradicted_by), and Temporal Gating (depends_on edge where source is newer than target, verified via ULID timestamp decode). Actions: help, scan, find_isomorphs, explain.";
const GUARDIAN_SCHEMA: &str = r#"{"type":"object","properties":{"action":{"type":"string","enum":["help","scan","find_isomorphs","explain"]},"data":{"type":"string","description":"JSONL dump of nodes and edges, one JSON object per line. Required for scan and find_isomorphs."},"pattern":{"type":"string","enum":["sovereignty_shield","double_helix","focal_trick","temporal_gating"],"description":"Pattern name for find_isomorphs or explain."}},"required":["action"]}"#;

// Crockford Base32 decode table (128-byte ASCII lookup).
// Valid chars: 0-9 A-H J-K M-N P-Z (excludes I L O U).
// I/L alias to 1, O aliases to 0. All others map to 0xFF (invalid).
static CROCKFORD: [u8; 128] = [
    0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,
    0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,
    0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,
    0x00,0x01,0x02,0x03,0x04,0x05,0x06,0x07,0x08,0x09,0xFF,0xFF,0xFF,0xFF,0xFF,0xFF,
    0xFF,0x0A,0x0B,0x0C,0x0D,0x0E,0x0F,0x10,0x11,0x01,0x12,0x13,0x01,0x14,0x15,0x00,
    0x16,0x17,0x18,0x19,0x1A,0xFF,0x1B,0x1C,0x1D,0x1E,0x1F,0xFF,0xFF,0xFF,0xFF,0xFF,
    0xFF,0x0A,0x0B,0x0C,0x0D,0x0E,0x0F,0x10,0x11,0x01,0x12,0x13,0x01,0x14,0x15,0x00,
    0x16,0x17,0x18,0x19,0x1A,0xFF,0x1B,0x1C,0x1D,0x1E,0x1F,0xFF,0xFF,0xFF,0xFF,0xFF,
];

fn json_str<'a>(obj: &'a json::Json, key: &str) -> &'a str {
    obj.get(key).as_str().unwrap_or("")
}

fn json_obj(s: &str) -> json::Json {
    match json::parse(s) {
        Ok(o) => o,
        Err(_) => json::Json::Null,
    }
}

/// Decode the timestamp from a ULID _id (first 10 Crockford chars = 48-bit Unix-ms).
/// Returns None if the id is too short or contains invalid Crockford chars.
fn ulid_timestamp(id: &str) -> Option<u64> {
    let mut ts: u64 = 0;
    let mut count: u32 = 0;
    for c in id.chars().filter(|c| *c != '-') {
        if count >= 10 {
            break;
        }
        let b = c as usize;
        if b >= 128 {
            return None;
        }
        let val = CROCKFORD[b];
        if val == 0xFF {
            return None;
        }
        ts = (ts << 5) | (val as u64);
        count += 1;
    }
    if count < 10 {
        return None;
    }
    // Shift right 2: 10 chars × 5 bits = 50 bits, top 48 are timestamp
    Some(ts >> 2)
}

// ── Pattern matchers ──

fn sovereignty_shield_match(node: &json::Json, _nodes: &[&json::Json], edges: &[&json::Json]) -> bool {
    let nid = json_str(node, "_id");
    let ncoll = json_str(node, "collection");
    if nid.is_empty() || ncoll.is_empty() {
        return false;
    }
    let mut has_incoming_enables = false;
    let mut has_outgoing = false;
    for e in edges {
        let etype = json_str(e, "edge_type");
        let sid = json_str(e, "source_id");
        let scoll = json_str(e, "source_collection");
        let tid = json_str(e, "target_id");
        let tcoll = json_str(e, "target_collection");
        if tid == nid && tcoll == ncoll && etype == "enables" {
            has_incoming_enables = true;
        }
        if sid == nid && scoll == ncoll {
            has_outgoing = true;
        }
    }
    has_incoming_enables && !has_outgoing
}

fn double_helix_match(node: &json::Json, _nodes: &[&json::Json], edges: &[&json::Json]) -> bool {
    let nid = json_str(node, "_id");
    let ncoll = json_str(node, "collection");
    if nid.is_empty() || ncoll.is_empty() {
        return false;
    }
    for e in edges {
        let etype = json_str(e, "edge_type");
        if etype != "enables" {
            continue;
        }
        let sid = json_str(e, "source_id");
        let scoll = json_str(e, "source_collection");
        let tid = json_str(e, "target_id");
        let tcoll = json_str(e, "target_collection");
        if sid == nid && scoll == ncoll {
            // Check for reverse edge
            for e2 in edges {
                let e2type = json_str(e2, "edge_type");
                if e2type != "enables" {
                    continue;
                }
                let s2 = json_str(e2, "source_id");
                let sc2 = json_str(e2, "source_collection");
                let t2 = json_str(e2, "target_id");
                let tc2 = json_str(e2, "target_collection");
                if s2 == tid && sc2 == tcoll && t2 == nid && tc2 == ncoll {
                    return true;
                }
            }
        }
    }
    false
}

fn focal_trick_match(node: &json::Json, _nodes: &[&json::Json], edges: &[&json::Json]) -> bool {
    let nid = json_str(node, "_id");
    let ncoll = json_str(node, "collection");
    if nid.is_empty() || ncoll.is_empty() {
        return false;
    }
    let mut refined = false;
    let mut contradicted = false;
    for e in edges {
        let etype = json_str(e, "edge_type");
        let tid = json_str(e, "target_id");
        let tcoll = json_str(e, "target_collection");
        if tid == nid && tcoll == ncoll {
            if etype == "refines" {
                refined = true;
            }
            if etype == "contradicts" {
                contradicted = true;
            }
        }
    }
    refined && contradicted
}

fn temporal_gating_match(node: &json::Json, nodes: &[&json::Json], edges: &[&json::Json]) -> bool {
    let nid = json_str(node, "_id");
    let ncoll = json_str(node, "collection");
    if nid.is_empty() || ncoll.is_empty() {
        return false;
    }
    let source_ts = match ulid_timestamp(nid) {
        Some(ts) => ts,
        None => return false,
    };
    for e in edges {
        let etype = json_str(e, "edge_type");
        if etype != "depends_on" {
            continue;
        }
        let sid = json_str(e, "source_id");
        let scoll = json_str(e, "source_collection");
        let tid = json_str(e, "target_id");
        let tcoll = json_str(e, "target_collection");
        if sid == nid && scoll == ncoll {
            // Find target node and compare timestamps
            for t in nodes {
                let tnid = json_str(t, "_id");
                let tncoll = json_str(t, "collection");
                if tnid == tid && tncoll == tcoll {
                    if let Some(target_ts) = ulid_timestamp(tnid) {
                        if source_ts > target_ts {
                            return true;
                        }
                    }
                    break;
                }
            }
        }
    }
    false
}

// ── Pattern registry ──

struct Pattern {
    name: &'static str,
    desc: &'static str,
    matcher: fn(&json::Json, &[&json::Json], &[&json::Json]) -> bool,
}

static PATTERNS: &[Pattern] = &[
    Pattern {
        name: "sovereignty_shield",
        desc: "A node with only incoming enables edges and zero outgoing edges — a protected core that depends on nothing. The ψ boundary.",
        matcher: sovereignty_shield_match,
    },
    Pattern {
        name: "double_helix",
        desc: "Two nodes that mutually enable each other (bidirectional enables) — a co-constitutive pair where neither exists without the other.",
        matcher: double_helix_match,
    },
    Pattern {
        name: "focal_trick",
        desc: "A node that is both refined_by AND contradicted_by other nodes — a contested focal point, the observer paradox.",
        matcher: focal_trick_match,
    },
    Pattern {
        name: "temporal_gating",
        desc: "A depends_on edge where the source node is newer than the target node (verified via ULID timestamp decode) — a forward dependency on something older, the sub-epoch boundary.",
        matcher: temporal_gating_match,
    },
];

fn find_pattern(name: &str) -> Option<&'static Pattern> {
    for p in PATTERNS {
        if p.name == name {
            return Some(p);
        }
    }
    None
}

// ── Actions ──

fn action_help() -> String {
    let mut out = String::from("kg_scan — Knowledge Graph Structural Pattern Scanner\n\nPatterns:\n");
    for p in PATTERNS {
        out.push_str(&format!("  {} — {}\n", p.name, p.desc));
    }
    out.push_str("\nActions:\n");
    out.push_str("  help           — this message\n");
    out.push_str("  scan           — run all four detectors, return counts + examples\n");
    out.push_str("  find_isomorphs — deep-dive a single pattern, return all matches\n");
    out.push_str("  explain        — return the description for a named pattern\n");
    out
}

fn action_explain(pattern_name: &str) -> String {
    match find_pattern(pattern_name) {
        Some(p) => format!("{}: {}", p.name, p.desc),
        None => format!("Unknown pattern: {}. Use 'help' to list available patterns.", pattern_name),
    }
}

fn action_scan(data: &str) -> String {
    let mut nodes: Vec<json::Json> = Vec::new();
    let mut edges: Vec<json::Json> = Vec::new();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj = json_obj(line);
        let typ = json_str(&obj, "type");
        if typ == "node" {
            nodes.push(obj);
        } else if typ == "edge" {
            edges.push(obj);
        }
    }

    let node_refs: Vec<&json::Json> = nodes.iter().collect();
    let edge_refs: Vec<&json::Json> = edges.iter().collect();

    let mut results: Vec<json::Json> = Vec::new();
    for p in PATTERNS {
        let mut matches: Vec<&json::Json> = Vec::new();
        for n in &node_refs {
            if (p.matcher)(n, &node_refs, &edge_refs) {
                matches.push(n);
            }
        }
        let mut examples: Vec<json::Json> = Vec::new();
        let limit = if matches.len() < 10 { matches.len() } else { 10 };
        for i in 0..limit {
            let m = matches[i];
            examples.push(json::obj(vec![
                ("_id", json::s(json_str(m, "_id"))),
                ("collection", json::s(json_str(m, "collection"))),
            ]));
        }
        results.push(json::obj(vec![
            ("pattern", json::s(p.name)),
            ("count", json::n(matches.len() as f64)),
            ("examples", json::arr(examples)),
        ]));
    }
    json::stringify(&json::obj(vec![("results", json::arr(results))]))
}

fn action_find_isomorphs(data: &str, pattern_name: &str) -> String {
    let p = match find_pattern(pattern_name) {
        Some(p) => p,
        None => return format!("Unknown pattern: {}", pattern_name),
    };

    let mut nodes: Vec<json::Json> = Vec::new();
    let mut edges: Vec<json::Json> = Vec::new();
    for line in data.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let obj = json_obj(line);
        let typ = json_str(&obj, "type");
        if typ == "node" {
            nodes.push(obj);
        } else if typ == "edge" {
            edges.push(obj);
        }
    }

    let node_refs: Vec<&json::Json> = nodes.iter().collect();
    let edge_refs: Vec<&json::Json> = edges.iter().collect();

    let mut matches: Vec<json::Json> = Vec::new();
    for n in &node_refs {
        if (p.matcher)(n, &node_refs, &edge_refs) {
            matches.push(json::obj(vec![
                ("_id", json::s(json_str(n, "_id"))),
                ("collection", json::s(json_str(n, "collection"))),
            ]));
        }
    }
    json::stringify(&json::obj(vec![
        ("pattern", json::s(p.name)),
        ("count", json::n(matches.len() as f64)),
        ("matches", json::arr(matches)),
    ]))
}

fn run(input: &str) -> String {
    let args = match json::parse(input) {
        Ok(a) => a,
        Err(e) => return json::stringify(&json::obj(vec![("error", json::s(&e))])),
    };
    let action = args.get("action").as_str().unwrap_or("help");
    match action {
        "help" => action_help(),
        "explain" => {
            let pattern = args.get("pattern").as_str().unwrap_or("");
            action_explain(pattern)
        }
        "scan" => {
            let data = args.get("data").as_str().unwrap_or("");
            if data.is_empty() {
                return json::stringify(&json::obj(vec![("error", json::s("data field is required for scan"))]));
            }
            action_scan(data)
        }
        "find_isomorphs" => {
            let data = args.get("data").as_str().unwrap_or("");
            let pattern = args.get("pattern").as_str().unwrap_or("");
            if data.is_empty() {
                return json::stringify(&json::obj(vec![("error", json::s("data field is required for find_isomorphs"))]));
            }
            if pattern.is_empty() {
                return json::stringify(&json::obj(vec![("error", json::s("pattern field is required for find_isomorphs"))]));
            }
            action_find_isomorphs(data, pattern)
        }
        _ => json::stringify(&json::obj(vec![("error", json::s("unknown action"))])),
    }
}
```

## Try it

```text
/guardian-define
<paste the module above>
.                                  ← serial terminator (web: just Enter)
/guardian status kg_scan           ← repeat until: ready
# then in plain language, e.g.:
#   "run knowledge_dump with the slim scan profile, then use the kg_scan
#    guardian tool on the dump file and summarize what fired"
# the intelligence chains knowledge_dump → guardian_call {data_file: …}
# itself; help/explain need no dump at all.
```

On an instance where the intelligence has already authored its own
`kg_scan` (it exists on the machine this example came from),
`/guardian-define` hits a name conflict — `/guardian delete kg_scan`
first, or rename the marker + `GUARDIAN_NAME` pair in your paste.
