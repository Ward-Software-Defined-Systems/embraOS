# embraOS Feedback Loop — Self-Evaluation Protocol

**Spec version:** v2.3 (operational backbone — steps only)

---

## Step 1: Gather

### 1.1 — Introspect: Load Evaluation Criteria

```
introspect soul
introspect identity
introspect user
```

### 1.2 — Session Summaries: Overview

```
session_list
session_summarize <name>    // for each session since last feedback loop
```

### 1.3 — Session Transcripts: Initial Review

```
session_read <name> [range]
```

### 1.4 — Session Search: Targeted Discovery

```
session_search "<query>"    // for each query in the search set
```

### 1.5 — Session Re-read: Search-Informed Review

```
session_read <name> [range]
```

### 1.6 — Session Extract: Save Learnings

```
session_extract <name>    // for every session since last feedback loop
```

### 1.7 — Knowledge Audit: Clean

```
knowledge_audit    // duplicate, orphaned, superseded and contradicting nodes
knowledge_merge <source node> | <target node>    // for each duplicate pair the operator agrees to; dry_run first
```

### 1.8 — Memory Scan: Inventory

```
memory_scan
memory_scan #<tag>    // for key tags: #soul, #identity, #architecture, #personal, #operational
```

### 1.9 — Memory Recall: Targeted Retrieval

```
recall embraOS
recall continuity
recall soul
recall priorities
recall personal
recall infrastructure
```

---

## Step 2: Evaluate

### 2.1 — Alignment Assessment

### 2.2 — Tension and Drift Assessment

### 2.3 — Evaluation Dimensions

---

## Step 3: Reconcile

### 3.1 — Decision Framework

### 3.2 — Action Definitions

### 3.3 — Reconciliation Plan Format

### 3.4 — Governance Boundary

---

## Step 4: Execute

### 4.1 — First Pass: Auto-Execute S0/S1

```
Accept        // no memory of its own: named in the 5.2 findings record
Reclassify    knowledge_update <collection>:<id> | {"tags": [...]} or {"category": "<category>"}
Rewrite       knowledge_update <collection>:<id> | {"content": "<rewritten>"}    // in place, links kept; forget + remember only for an entry that has no node
Remove        forget <entry or node id>    // S2/S3 only, after approval: the entry, its node and their edges
Add practice  remember <practice, one line> #operational-practice    // category: pattern; procedure: <procedure_json> when it has steps
recall <key terms from each modified entry>
```

### 4.2 — Second Pass: Present S2/S3 for Approval

### 4.3 — Update Protocol

```
remember <protocol update, one line> #feedback-loop-protocol    // category: decision
```

---

## Step 5: Record

### 5.1 — Session Summary

```
session_summarize <feedback-loop-session-name>
```

### 5.2 — Findings Record

```
remember Feedback Loop Run <date>: <count> sessions reviewed, <count> memory entries scanned. Alignment confirmed in: <list>. Tensions found: <count> (S0: <n>, S1: <n>, S2: <n>, S3: <n>). Accepted: <each accepted tension, named>. Actions taken: <summary>. #feedback-loop #evaluation    // category: observation
```

### 5.3 — Link What Was Saved

```
knowledge_link <node saved in this run> | <edge_type> | <related node> | <weight>    // for each related node its remember listed
```
