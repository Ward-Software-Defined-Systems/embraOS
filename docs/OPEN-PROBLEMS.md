# embraOS — Open Problems

Unresolved design questions tracked at the architecture level rather than as code comments. Each is something that will need a decision during Phase 1–3 implementation. Implementation bugs go in the Embra_Debug tracker — this list is design-state tracking, not defects.

Extracted from `ARCHITECTURE.md` — `### Architectural Tensions (Known Open Problems)` — on 2026-05-23. Wording verbatim. A dated note under an entry says what has happened since; the entry itself is left as it was written.

---

## Module trust escalation

Operator-authored modules start sandboxed. How do they earn broader access? A trust ladder is needed: sandbox → internal-only → governed-egress → full-egress. Each step requires governance approval plus operational history. The ladder design is not yet specified.

**2026-10-03.** The first rungs exist for dynamic tools (`embra-guardian`). A tool runs in a `wasmtime` sandbox with no ambient authority and reaches out only through the host imports it declares: `http_get` (https, public addresses only) and `web_search` (`KNOWN_CAPS` in `crates/embra-guardian/src/abi.rs`; the rules in `caps.rs`). A declaration is granted when the tool passes the replicant check and, for a proposal of the intelligence, the operator's `/guardian approve`; the one tool the image ships (`web_search`) holds its declarations by the project's review, at install. That is approval per definition, not a ladder: nothing is earned from operating history, and there is no internal-only rung — a private or loopback address is refused outright. The ladder is still not specified.

## Resource contention: LLM vs modules

Local LLM inference is resource-intensive. Module containers also need CPU and memory. embrad needs to arbitrate. The Continuity Engine should reason about "more inference capacity" vs "more module capacity" as a scheduling decision — but the scheduling policy is not yet designed.

## Governance latency

Every governed operation goes through embra-guardian. If governance evaluation involves LLM reasoning, this adds latency to the hot path. Proposed dual-path: deterministic rule-engine for hot-path governance (fast), LLM-based evaluation for complex or novel requests (slow but thorough). The threshold between paths is not yet defined.

**2026-10-03.** Dynamic tools settled on the dual path, split by time instead of by request. The evaluation that needs a model, the replicant check, runs when a tool is defined, proposed or rebuilt (`run_replicant_check` in `crates/embra-brain/src/guardian/mod.rs`) and never when it is called. A call meets deterministic limits only: the sandbox's epoch timeout and memory cap, and the capability broker's rules. The threshold between the paths is still undefined for an operation that is governed at the moment it runs, as a proxied MCP call would be.

## WardSONDB as single point of failure

WardSONDB is a core OS service. If it fails, the brain can't read state and the feedback loop halts. Mitigations: WAL-based crash recovery (fjall's built-in durability), read replica for continuity during recovery, snapshot-based restore as last resort. The replica architecture is not yet designed.

**2026-10-03.** Two of the three mitigations exist. Crash recovery is the storage engine's — fjall or rocksdb, chosen when the image is built (`--storage-engine`) — and `embrad` restarts the database with backoff under a burst budget (`crates/embrad/src/supervisor.rs`). Snapshot restore is `scripts/embraos-backup.sh` (`backup`, `restore`, `list`, `verify` of STATE and DATA), a file-level copy taken from the host with the VM stopped. There is still no replica, and no design for one.

## Bare metal vs K8s isolation parity

In bare metal mode, module containers share the same kernel as embraOS. In K8s mode, modules run in a separate namespace with network policies. Bare metal needs stronger containerd-level isolation (seccomp, AppArmor/SELinux profiles, user namespaces) to match K8s-level isolation. The seccomp/AppArmor profiles are not yet written.

## Module image provenance

If module source originates inside the OS (operator-authored via Guardian, or — under future governance design — brain-proposed), the provenance chain must be auditable end-to-end: source code → `modules.source` → sandboxed build → image signing → governance review → allowlist → deploy. Each step must be logged and verifiable. The sandboxed build pipeline is not yet designed.

**2026-10-03.** Part of the chain exists for dynamic tools, and the source this entry calls future — a proposal of the intelligence — has existed since `guardian_propose`. The record of a tool (`ToolDoc` in `crates/embra-guardian/src/store.rs`, collection `guardian.tools`) keeps its source and the SHA-256 of it, the verdict of the replicant check with the model that judged and the time, the toolchain version it was built with, and the tail of the build log. A tool has no third-party dependency, so its build runs no code but the compiler's. Not kept: a record of the operator's approval (building a proposal is the approval), a hash or a signature of the built artifact, and a log of each step that can be verified afterwards. Modules as images, with signing and an allowlist, are still not designed.

## Does the auto-derived edge layer still earn its cost?

Opened 2026-09-08, when retrieval's graph-expansion step was deleted. `same_session`, `temporal` and `tag_overlap` account for **405,429 of production's 408,046 edges (99.36%)**, growing ~1.7x faster than the node layer (edges +44.5% against nodes +26% over 19 days), and 21% of sampled nodes now exceed the 500-edge auto window — up from 10.8% a month earlier.

That layer's headline justification was per-turn depth-2 expansion during retrieval: density was the substrate expansion needed to find adjacent nodes. Measurement retired that argument. Expansion reached a 1,000-node slab (42% of the graph) that was 97.7–99.6% auto-reached, cost ~96% of retrieval latency, and contributed **nothing** to the injected top-20 on any query tested.

What remains is `knowledge_traverse`, which the intelligence invokes deliberately rather than on every turn — and the control case is instructive: identity nodes carry no auto edges, and a traversal from one is **100% meaningful**. The graph is useful exactly where the auto layer does not drown it.

Options, none taken: decay `same_session`/`temporal` weights with age and prune below a floor; stop double-writing auto edges; cap per-node auto degree at write time; or accept the growth as the cost of a traversal tool used a few times a session. The decision needs a measurement of what `knowledge_traverse` actually returns in practice, which does not exist yet. Related: the WardSONDB traverse endpoint stays deferred precisely because the step it would accelerate was the one deleted (`embraOS-Phase1-Implementation/Sprint 6/KG-DEFERRED-ITEMS-DECISION.md`).

**2026-10-03.** The proportion holds: 430,271 of 433,325 edges are automatic (99.3%, the backup of 2026-10-02). `remember` now promotes a memory when it is created, so every memory derives twice — once for its entry and once for its node, which carries the same text, tags and session. That was already the practice (1,264 of 1,294 entries were promoted); it is the rule now. It adds an option to the list above: derive for the node alone. A memory would then add about half of what it adds today. The price is in retrieval, whose session step walks `same_session` edges from the session's entries (`retrieval.rs`, step 2) and would have to be examined again.
