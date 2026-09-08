# embraOS — Open Problems

Unresolved design questions tracked at the architecture level rather than as code comments. Each is something that will need a decision during Phase 1–3 implementation. Implementation bugs go in the Embra_Debug tracker — this list is design-state tracking, not defects.

Extracted from `ARCHITECTURE.md` — `### Architectural Tensions (Known Open Problems)` — on 2026-05-23. Wording verbatim.

---

## Module trust escalation

Operator-authored modules start sandboxed. How do they earn broader access? A trust ladder is needed: sandbox → internal-only → governed-egress → full-egress. Each step requires governance approval plus operational history. The ladder design is not yet specified.

## Resource contention: LLM vs modules

Local LLM inference is resource-intensive. Module containers also need CPU and memory. embrad needs to arbitrate. The Continuity Engine should reason about "more inference capacity" vs "more module capacity" as a scheduling decision — but the scheduling policy is not yet designed.

## Governance latency

Every governed operation goes through embra-guardian. If governance evaluation involves LLM reasoning, this adds latency to the hot path. Proposed dual-path: deterministic rule-engine for hot-path governance (fast), LLM-based evaluation for complex or novel requests (slow but thorough). The threshold between paths is not yet defined.

## WardSONDB as single point of failure

WardSONDB is a core OS service. If it fails, the brain can't read state and the feedback loop halts. Mitigations: WAL-based crash recovery (fjall's built-in durability), read replica for continuity during recovery, snapshot-based restore as last resort. The replica architecture is not yet designed.

## Bare metal vs K8s isolation parity

In bare metal mode, module containers share the same kernel as embraOS. In K8s mode, modules run in a separate namespace with network policies. Bare metal needs stronger containerd-level isolation (seccomp, AppArmor/SELinux profiles, user namespaces) to match K8s-level isolation. The seccomp/AppArmor profiles are not yet written.

## Module image provenance

If module source originates inside the OS (operator-authored via Guardian, or — under future governance design — brain-proposed), the provenance chain must be auditable end-to-end: source code → `modules.source` → sandboxed build → image signing → governance review → allowlist → deploy. Each step must be logged and verifiable. The sandboxed build pipeline is not yet designed.

## Does the auto-derived edge layer still earn its cost?

Opened 2026-09-08, when retrieval's graph-expansion step was deleted. `same_session`, `temporal` and `tag_overlap` account for **405,429 of production's 408,046 edges (99.36%)**, growing ~1.7x faster than the node layer (edges +44.5% against nodes +26% over 19 days), and 21% of sampled nodes now exceed the 500-edge auto window — up from 10.8% a month earlier.

That layer's headline justification was per-turn depth-2 expansion during retrieval: density was the substrate expansion needed to find adjacent nodes. Measurement retired that argument. Expansion reached a 1,000-node slab (42% of the graph) that was 97.7–99.6% auto-reached, cost ~96% of retrieval latency, and contributed **nothing** to the injected top-20 on any query tested.

What remains is `knowledge_traverse`, which the intelligence invokes deliberately rather than on every turn — and the control case is instructive: identity nodes carry no auto edges, and a traversal from one is **100% meaningful**. The graph is useful exactly where the auto layer does not drown it.

Options, none taken: decay `same_session`/`temporal` weights with age and prune below a floor; stop double-writing auto edges; cap per-node auto degree at write time; or accept the growth as the cost of a traversal tool used a few times a session. The decision needs a measurement of what `knowledge_traverse` actually returns in practice, which does not exist yet. Related: the WardSONDB traverse endpoint stays deferred precisely because the step it would accelerate was the one deleted (`embraOS-Phase1-Implementation/Sprint 6/KG-DEFERRED-ITEMS-DECISION.md`).
