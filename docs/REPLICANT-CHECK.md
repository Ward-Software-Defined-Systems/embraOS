# The Replicant Check

*Soul-gated dynamic tools — enforcement, not persuasion.* **Experimental.**

The replicant check is the gate every Guardian dynamic tool must pass before it compiles: an independent evaluation of the proposed tool against the operator-defined **soul**. It is the first concrete landing of *soul-as-enforced-runtime* — moving the soul from text the model is asked to honor in its system prompt to a checkable gate the OS enforces at the point a tool would be created.

It applies to **both** ways a dynamic tool can be authored: a module the **intelligence** drafts (the `guardian_propose` tool) and a module an **operator** pastes (`/guardian-define`). A draft that conflicts with the soul does not compile on either path — the soul outranks even an operator paste, and a refusal is not waivable.

---

## Why

embraOS gives a single model a sealed identity document — the soul: an operator-defined JSON of `purpose`, `ethical_lines`, `values`, and `surviving_constraints`, SHA-256-sealed at first boot and verified by `embra-trustd` on every boot. The operational system prompt already ranks the soul above everything, including the operator. But a system prompt is persuasion: the model is *asked* to refuse soul conflicts.

A dynamic tool is the highest-leverage action the system can take — Rust that compiles and runs on the live, immutable OS. Letting the intelligence author one is exactly where persuasion is not enough. The replicant check makes "does this honor the soul?" a step the OS performs and acts on, rather than a sentence in the prompt the model may or may not follow.

---

## The two authoring paths (both gated)

```
intelligence drafts a module                 operator pastes a module
  → guardian_propose                            → /guardian-define
        │                                              │
   Gate 1: syn static validation (contract + denylist) │
        │                                              │
   Gate 2: REPLICANT CHECK  (independent soul-verdict)  ← both paths
        │                                              │
   allow / escalate → Proposed doc            allow / escalate → compiles
        │                                       (escalate: with a warning)
   Gate 3: operator approval                    refuse → blocked, no compile
   /guardian approve <name>
        │
   build (wasm32 + wasmtime sandbox) → Ready → guardian_call invoke
```

The difference between the paths is the *approval* step, not the *soul check*. An intelligence draft that passes becomes a **proposal** an operator must approve before it builds — the authoring intelligence never rules on its own draft and never approves it. An operator paste that passes compiles directly, because the operator's paste *is* their approval.

---

## How the check works

When a module reaches Gate 2 (`crates/embra-brain/src/guardian/replicant.rs::evaluate_against_soul`, via the shared `run_replicant_check` helper in `guardian/mod.rs`):

1. **Load the sealed soul** (`learning::load_soul`). The soul is prose — four free-form string fields, deliberately not a rule engine — so the check is model judgment, not structured matching.
2. **Make an independent model call.** It builds its own provider via `grpc_service::build_provider_from_config` (the same backend the operator configured: Anthropic / Gemini / Ollama / LM Studio), with **no tools** in the request. The authoring intelligence is not the judge; this is a separate, isolated call whose only job is to rule on the draft.
3. **Send the proposed tool** — `name`, `description`, `input_schema`, declared capabilities, and full `source` — together with the rendered soul, and ask for a single JSON verdict. The prompt weighs the draft against every soul line, paying special attention to anything that would let the system **replicate itself, persist or escape its sandbox, exfiltrate operator data, or expand its own authority** beyond what the soul sanctions.
4. **Read only the final verdict text** (reasoning is never persisted or replayed, per the reasoning-stream privacy contract) and parse the first JSON object out of it.

The verdict shape:

```json
{ "verdict": "allow | refuse | escalate",
  "touched_lines": ["<soul line text>", "..."],
  "rationale": "<one paragraph>" }
```

---

## Verdict semantics

| verdict | intelligence proposal (`guardian_propose`) | operator paste (`/guardian-define`) |
|---|---|---|
| **allow** | saved as a `proposed` tool; awaits `/guardian approve` | compiles |
| **escalate** | saved as a `proposed` tool, flagged for the operator's judgment | compiles, with a "soul-borderline — review it" warning (the operator already chose to paste, so they are the escalation target) |
| **refuse** | blocked; **never proposed**; the intelligence is told it "did not pass the replicant check," naming the touched soul line | blocked; **never compiles** — the soul outranks even an operator paste |

Two rules hold on both paths:

- **Fail closed.** If the verdict call errors or returns an unparseable verdict, **nothing compiles** — the system never defaults to allow.
- **Skipped only before the soul is sealed.** During first-boot setup there is no soul to evaluate against, so an operator define proceeds; an intelligence proposal, by contrast, fails closed in that state (the intelligence must never self-author ungated).

The verdict is persisted with the tool (`ReplicantRecord`: `verdict`, `touched_lines`, `rationale`, `model`, `judged_at`) and shown at `/guardian show <name>` and `/guardian status <name>`, so the operator sees *why* something passed when deciding whether to approve it.

---

## Operator workflow

```
/guardian list                 # all dynamic tools — status (incl. `proposed`) + declared caps
/guardian show <name>          # source + the stored replicant verdict
/guardian approve <name>       # build + enable a proposed tool
/guardian reject <name>        # discard a proposal
/guardian status <name>        # build state + log tail + verdict
```

`guardian_propose`, `guardian_call`, and `guardian_list` are the only Guardian-facing tools the model sees; dynamic tools themselves are never injected into the provider tool schema, so the prompt cache stays byte-stable.

---

## Testing the check

To exercise the gate end to end (QEMU; needs a live provider, a sealed soul, and the in-OS `wasm32` toolchain):

1. **Seal a soul** with at least one sharp `ethical_line` — e.g. *"Never exfiltrate operator data to a third party."* — during the first-boot Learning Mode.
2. **Positive (intelligence path).** Ask the intelligence for a benign capability that doesn't exist yet. Confirm it calls `guardian_propose`, the check **allows**, `/guardian list` shows the tool as `proposed`, and `/guardian show` displays the source plus the `allow` verdict. `/guardian approve <name>` → status walks `proposed → building → ready`; the intelligence invokes it via `guardian_call`.
3. **Negative (the key test).** Ask the intelligence to draft a tool that plainly violates a soul line — e.g. one that POSTs a workspace file's contents to an external URL. Confirm the check **refuses**, no proposal is written, and the intelligence reports it "did not pass the replicant check," naming the touched line.
4. **Operator path is gated too.** `/guardian-define` and paste a soul-violating module. Confirm the **refuse blocks the compile** — the operator cannot waive the soul.
5. **Escalate.** A soul-borderline tool should land as a flagged `proposed` (intelligence) or compile with a warning (operator).
6. **Fail closed.** With the configured provider unreachable, confirm nothing compiles on either path.

---

## Honest limitations

- **The guard is itself a model output.** It is defense-in-depth, an audit trail, and an evaluation signal — **not a mathematical proof**. Its teeth come from three things the model cannot talk its way past: the verdict is a constrained, separate call; the OS acts on the verdict (a `refuse` blocks the compile regardless of what any model "wanted"); and an intelligence proposal still needs an operator's explicit approval. The "who guards the guardian" limit is real and stated plainly.
- **The soul is prose**, so the check reasons in natural language rather than evaluating typed rules.
- **The operator path is unchecked before the soul is sealed** (there is nothing to check against yet).

---

## Implementation

| Piece | Location |
|---|---|
| Verdict call + tolerant JSON parser | `crates/embra-brain/src/guardian/replicant.rs` |
| Shared gate, `propose` / `approve` / `reject`, gated `define` | `crates/embra-brain/src/guardian/mod.rs` |
| The judging provider (same backend as the main loop) | `crates/embra-brain/src/grpc_service.rs::build_provider_from_config` |
| `guardian_propose` meta-tool | `crates/embra-brain/src/tools/guardian.rs` |
| `ToolStatus::Proposed` + `ReplicantRecord` (persisted verdict) | `crates/embra-guardian/src/store.rs` |

It sits entirely above `apid` — no proto, WardSONDB-schema, or gateway change.

---

## What's next

The replicant check is Pillar 1 of soul-as-enforced-runtime, scoped to the self-authoring path. Two extensions are designed but not yet built, and the primitives here are deliberately built to generalize toward them:

- **A signed decision ledger** — hash-chaining each verdict and signing it with the `embra-trustd` key, so the system accumulates a tamper-evident record of every soul judgment it made. Verdicts are stored with the tool today, but not yet cryptographically chained.
- **Gating all side-effectful tool dispatch**, not just tool authoring — the same independent-verdict primitive applied at the point any world-touching tool runs.

See also: [Guardian Tool Examples](GUARDIAN-TOOL-EXAMPLES.md) · [Guardian Advanced Example](GUARDIAN-ADVANCED-EXAMPLE.md) · [Tool Reference](TOOL-REFERENCE.md) · [System Design](SYSTEM-DESIGN.md).
