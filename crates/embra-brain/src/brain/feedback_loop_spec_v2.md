# embraOS Feedback Loop — Self-Evaluation Protocol

**Spec version:** v2.2 (operational backbone — steps only)

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

### 1.6 — Session Extract: Promote Learnings

```
session_extract <name>    // for every session since last feedback loop
```

### 1.7 — Memory Dedup: Clean

```
memory_dedup
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
recall <key terms from each modified entry>
```

### 4.2 — Second Pass: Present S2/S3 for Approval

### 4.3 — Update Protocol

---

## Step 5: Record

### 5.1 — Session Summary

```
session_summarize <feedback-loop-session-name>
```

### 5.2 — Findings Record

```
remember Feedback Loop Run <date>: <count> sessions reviewed, <count> memory entries scanned. Alignment confirmed in: <list>. Tensions found: <count> (S0: <n>, S1: <n>, S2: <n>, S3: <n>). Actions taken: <summary>. #feedback-loop #evaluation
```

### 5.3 — Promote Findings to Knowledge Graph

```
knowledge_promote <findings_entry_id> | semantic | evaluation
knowledge_promote <practice_entry_id> | procedural | <procedure_json>
knowledge_promote <practice_entry_id> | semantic | practice
knowledge_promote <protocol_update_entry_id> | semantic | practice
knowledge_promote <rewrite_entry_id> | semantic | <category>
```
