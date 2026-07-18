# WardSONDB API Documentation

> **Version:** 0.1.0 (Phase 1-3: MVP + Aggregation + TLS + Indexes + TTL + Auth + $collect + $distinct + Prometheus + Bitmap Scan Accelerator)

## Getting Started

### Prerequisites

**File descriptor limit (ulimit):** WardSONDB requires `ulimit -n` to be at least **4096** for production use. macOS defaults to 256 and Linux to 1024 — both are too low. The underlying storage engine (fjall) opens file handles for each SST segment and will crash with "Too many open files" past ~900K documents.

**Recommended:** `ulimit -n 65536`

```bash
# Raise the file descriptor limit before launching (pick a backend)
ulimit -n 65536 && wardsondb --storage-engine rocksdb

# With TLS enabled
ulimit -n 65536 && wardsondb --storage-engine rocksdb --tls
```

WardSONDB will attempt to auto-raise the file descriptor limit on startup if possible. If the limit remains below 4096, it logs a **WARNING** at startup.

### Building

```bash
cargo build --release
```

The binary is at `target/release/wardsondb`.

### Launching

```bash
wardsondb [OPTIONS]
```

### CLI Options

| Flag | Short | Default | Description |
|------|-------|---------|-------------|
| `--port <PORT>` | `-p` | `8080` | Listen port |
| `--data-dir <PATH>` | `-d` | `./data` | Data directory (created automatically) |
| `--storage-engine <ENGINE>` | | *required* | Storage backend: `rocksdb` or `fjall`. Required on every launch (no default). Locked per data directory via a `.engine` marker file. |
| `--log-level <LEVEL>` | `-l` | `info` | Log level: `trace`, `debug`, `info`, `warn`, `error` |
| `--log-file <PATH>` | | `wardsondb.log` | Log file path. Written via a non-blocking appender; if the path can't be opened the server warns and runs without file logging |
| `--verbose` | `-v` | `false` | Enable per-request logging on both sinks (terminal and file). Off by default — always-on request logs grow without bound over long uptimes |
| `--tls` | | `false` | Enable TLS (HTTPS) |
| `--tls-cert <PATH>` | | | Path to PEM certificate file |
| `--tls-key <PATH>` | | | Path to PEM private key file |
| `--ttl-interval <SECS>` | | `60` | TTL cleanup interval in seconds |
| `--api-key <KEY>` | | | API key for authentication (repeatable) |
| `--api-key-file <PATH>` | | | File with API keys (one per line, # comments) |
| `--metrics-public` | | `false` | Make `/_metrics` publicly accessible (bypasses auth) |
| `--query-timeout <SECS>` | | `30` | Read timeout in seconds for query, aggregate, distinct, and get-by-id (0 = no timeout) |
| `--max-query-limit <N>` | | `100000` | Maximum query `limit`; larger requests are clamped silently |
| `--max-body-mb <N>` | | `64` | Maximum HTTP request body size in MiB; oversized requests get 413 `DOCUMENT_TOO_LARGE` |
| `--cache-size-mb <N>` | | `64` | Block cache size in MiB (read cache for all partitions) |
| `--write-buffer-mb <N>` | | `64` | Total write buffer budget in MiB across all partitions |
| `--memtable-mb <N>` | | `8` | Max memtable size per partition in MiB before flush |
| `--flush-workers <N>` | | `2` | Background threads for flushing memtables to disk |
| `--compaction-workers <N>` | | `2` | Background threads for LSM-tree compaction |
| `--bitmap-fields <CSV>` | | `""` | Comma-separated fields for bitmap scan accelerator (skip auto-detection) |
| `--bitmap-max-cardinality <N>` | | `1000` | Max distinct values per bitmap column before disabling |
| `--bitmap-sample-size <N>` | | `10000` | Number of inserts to sample for auto-detection |
| `--bitmap-memory-mb <N>` | | `0` | Bitmap memory budget in MiB (`0` = auto: min(4096, 10% system RAM)) |
| `--no-bitmap` | | `false` | Disable the bitmap scan accelerator entirely |
| `--help` | `-h` | | Print help |
| `--version` | `-V` | | Print version |

The `--help` output also displays a **FILE DESCRIPTORS** section at the bottom, showing the current `ulimit -n` value and whether it meets the minimum requirement of 4096.

### Examples

Every invocation below passes `--storage-engine rocksdb`; swap in
`--storage-engine fjall` to use the alternative backend.

```bash
# Minimal — listen on port 8080, store data in ./data
wardsondb --storage-engine rocksdb

# Custom port and data directory
wardsondb --storage-engine rocksdb --port 3000 --data-dir /var/lib/wardsondb

# Debug logging with per-request logs in the terminal
wardsondb --storage-engine rocksdb --log-level debug --verbose

# Production — info level, logs to a specific file
wardsondb --storage-engine rocksdb --data-dir /var/lib/wardsondb --log-file /var/log/wardsondb.log

# TLS with auto-generated self-signed certificate
wardsondb --storage-engine rocksdb --tls

# TLS with custom certificate
wardsondb --storage-engine rocksdb --tls --tls-cert /etc/ssl/wardsondb.crt --tls-key /etc/ssl/wardsondb.key

# Bitmap scan accelerator with explicit fields
wardsondb --storage-engine rocksdb --bitmap-fields "event_type,severity,network.action"

# Bitmap with custom cardinality cap
wardsondb --storage-engine rocksdb --bitmap-fields "event_type,severity" --bitmap-max-cardinality 500

# Bitmap with explicit memory budget (2 GiB)
wardsondb --storage-engine rocksdb --bitmap-fields "event_type,severity" --bitmap-memory-mb 2048

# Disable bitmap scan accelerator
wardsondb --storage-engine rocksdb --no-bitmap
```

### Logging Behavior

- **Terminal** shows startup messages, stats banners (every 10s), and warnings/errors.
- **Log file** (`wardsondb.log` by default) records everything including per-request logs.
- Use `--verbose` to also show per-request logs in the terminal.

### TLS / HTTPS

Pass `--tls` to enable HTTPS. Two modes:

**Auto-generated self-signed certificate:**
```bash
wardsondb --storage-engine rocksdb --tls
```
- Generates a self-signed certificate and key on first run
- Stored in `<data-dir>/tls/cert.pem` and `<data-dir>/tls/key.pem`
- Reused on subsequent runs if the files already exist
- SANs: `localhost`, `127.0.0.1`, `0.0.0.0`
- Valid for 365 days
- Use `curl -k` or `--insecure` with self-signed certs

**Custom certificate:**
```bash
wardsondb --storage-engine rocksdb --tls --tls-cert /path/to/cert.pem --tls-key /path/to/key.pem
```

Without `--tls`, the server runs plain HTTP (default behavior).

---

## Response Format

All endpoints return a JSON envelope:

**Success:**
```json
{
  "ok": true,
  "data": { ... },
  "meta": {
    "request_id": "019...",
    "duration_ms": 2.4
  }
}
```

**Success (query with diagnostics):**
```json
{
  "ok": true,
  "data": [ ... ],
  "meta": {
    "duration_ms": 12.1,
    "total_count": 1542,
    "returned_count": 50,
    "docs_scanned": 1542
  }
}
```

**Error:**
```json
{
  "ok": false,
  "error": {
    "code": "DOCUMENT_NOT_FOUND",
    "message": "Document not found: abc123"
  },
  "meta": {}
}
```

Every response includes an `X-Request-Id` header (UUIDv7).

---

## System Endpoints

### GET / — Server Info

Returns server name, version, uptime, and data directory.

```bash
curl http://localhost:8080/
```

```json
{
  "ok": true,
  "data": {
    "name": "WardSONDB",
    "version": "0.1.0",
    "uptime_seconds": 42,
    "data_directory": "./data"
  },
  "meta": {}
}
```

### GET /_health — Health Check

Returns 200 if the server is running. Includes `write_pressure` to indicate whether the server is under heavy compaction/write load. If the storage engine has suffered a fatal flush or compaction failure (poisoned), it returns `"status": "degraded"` with a `"warning"` field.

```bash
curl http://localhost:8080/_health
```

**Healthy response (normal pressure):**
```json
{
  "ok": true,
  "data": {
    "status": "healthy",
    "write_pressure": "normal",
    "scan_accelerator_ready": true
  },
  "meta": {}
}
```

**Healthy response (high write pressure):**
```json
{
  "ok": true,
  "data": {
    "status": "healthy",
    "write_pressure": "high",
    "scan_accelerator_ready": true
  },
  "meta": {}
}
```

| Field | Values | Description |
|-------|--------|-------------|
| `status` | `"healthy"`, `"degraded"` | `"degraded"` only when storage engine is poisoned |
| `write_pressure` | `"normal"`, `"high"` | `"high"` when average request latency exceeds 5000ms in recent intervals. Clients should defer non-essential queries. Returns to `"normal"` when latency drops below 500ms for 3 consecutive intervals. |
| `scan_accelerator_ready` | `true`, `false` | Whether the bitmap scan accelerator is initialized and ready for queries |

**Degraded response (storage engine poisoned):**
```json
{
  "ok": true,
  "data": {
    "status": "degraded",
    "write_pressure": "normal",
    "scan_accelerator_ready": true,
    "warning": "Storage engine is poisoned: flush/compaction failure. Writes rejected, reads may continue. Restart required."
  },
  "meta": {}
}
```

### GET /_stats — Server Statistics

Returns collection count, total documents, uptime, and lifetime operation counters.

```bash
curl http://localhost:8080/_stats
```

```json
{
  "ok": true,
  "data": {
    "collection_count": 3,
    "total_documents": 15240,
    "uptime_seconds": 3600,
    "storage_poisoned": false,
    "memory_config": {
      "cache_size_mb": 64,
      "max_write_buffer_mb": 64,
      "max_memtable_mb": 8,
      "flush_workers": 2,
      "compaction_workers": 2
    },
    "lifetime": {
      "requests": 45000,
      "inserts": 15000,
      "queries": 29500,
      "deletes": 500
    },
    "scan_accelerator": {
      "ready": true,
      "total_positions": 15240,
      "memory_bytes": 98304,
      "memory_budget_bytes": 4294967296,
      "over_budget": false,
      "bitmap_columns": [
        {"field": "event_type", "cardinality": 5, "memory_bytes": 65536},
        {"field": "severity", "cardinality": 4, "memory_bytes": 32768}
      ]
    }
  },
  "meta": {}
}
```

---

## Collections

### GET /_collections — List Collections

```bash
curl http://localhost:8080/_collections
```

```json
{
  "ok": true,
  "data": [
    { "name": "events", "doc_count": 1000, "indexes": [] },
    { "name": "users", "doc_count": 50, "indexes": [] }
  ],
  "meta": {}
}
```

### POST /_collections — Create Collection

```bash
curl -X POST http://localhost:8080/_collections \
  -H "Content-Type: application/json" \
  -d '{"name": "events"}'
```

**Response (201 Created):**
```json
{
  "ok": true,
  "data": { "name": "events", "doc_count": 0, "indexes": [] },
  "meta": {}
}
```

**Collection name rules:**
- Cannot be empty
- Cannot start with `_` (underscore)
- Cannot be a reserved name (`_collections`, `_health`, `_stats`)
- Characters allowed: `a-z`, `A-Z`, `0-9`, `_`, `-`, `.`, `#`, `$`

**Errors:**
- `409 COLLECTION_EXISTS` — collection with that name already exists
- `400 INVALID_DOCUMENT` — invalid collection name

### GET /{collection} — Collection Info

```bash
curl http://localhost:8080/events
```

```json
{
  "ok": true,
  "data": { "name": "events", "doc_count": 1000, "indexes": [] },
  "meta": {}
}
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`

### DELETE /{collection} — Drop Collection

Deletes all documents in the collection.

```bash
curl -X DELETE http://localhost:8080/events
```

```json
{
  "ok": true,
  "data": { "dropped": true, "name": "events" },
  "meta": {}
}
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`

---

## Documents

### POST /{collection}/docs — Insert Document

Insert a JSON document. If the request body includes an `_id` field with a non-empty string value, it is used as the document ID. Otherwise, a UUIDv7 is auto-generated. Other system fields are added automatically: `_rev`, `_created_at`, `_updated_at`, and `_received_at` (server-side ingest timestamp, immutable on updates).

**Custom `_id` rules:** must be a non-empty string, max 512 bytes, must not start with `_`, must not contain null bytes.

```bash
curl -X POST http://localhost:8080/events/docs \
  -H "Content-Type: application/json" \
  -d '{
    "event_type": "firewall",
    "network": {
      "src_ip": "192.168.1.100",
      "dst_ip": "10.0.0.1",
      "dst_port": 443
    },
    "severity": "high"
  }'
```

**Insert with custom ID:**
```bash
curl -X POST http://localhost:8080/events/docs \
  -H "Content-Type: application/json" \
  -d '{
    "_id": "evt-firewall-2026-03-25-001",
    "event_type": "firewall",
    "network": {"src_ip": "192.168.1.100", "action": "block"}
  }'
```

**Response (201 Created):**
```json
{
  "ok": true,
  "data": {
    "_id": "0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a",
    "_rev": 1,
    "_created_at": "2026-03-09T14:30:00.000Z",
    "_updated_at": "2026-03-09T14:30:00.000Z",
    "event_type": "firewall",
    "network": {
      "src_ip": "192.168.1.100",
      "dst_ip": "10.0.0.1",
      "dst_port": 443
    },
    "severity": "high"
  },
  "meta": {}
}
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `400 INVALID_DOCUMENT` — malformed JSON or invalid custom `_id`
- `409 DOCUMENT_CONFLICT` — a document with the provided `_id` already exists

### POST /{collection}/docs/_bulk — Bulk Insert

Insert multiple documents (maximum 10,000 per request). Uses **partial success** semantics: each document is validated individually, and invalid documents are skipped with per-document errors. All valid documents are committed atomically in a single transaction.

Documents may include a custom `_id` field. If provided, it must be a unique non-empty string. Duplicate `_id` values (within the batch or against existing documents) are skipped with per-document errors.

```bash
curl -X POST http://localhost:8080/events/docs/_bulk \
  -H "Content-Type: application/json" \
  -d '{
    "documents": [
      {"event_type": "dns", "query": "example.com"},
      {"event_type": "dhcp", "mac": "AA:BB:CC:DD:EE:FF"},
      {"event_type": "firewall", "action": "block"}
    ]
  }'
```

**Response (201 Created):**
```json
{
  "ok": true,
  "data": {
    "inserted": 3,
    "errors": []
  },
  "meta": {}
}
```

**Partial success example** (mix of valid and invalid documents):
```bash
curl -X POST http://localhost:8080/events/docs/_bulk \
  -H "Content-Type: application/json" \
  -d '{
    "documents": [
      {"event_type": "firewall", "severity": "high"},
      "not an object",
      {"event_type": "dns", "query": "example.com"},
      42
    ]
  }'
```

```json
{
  "ok": true,
  "data": {
    "inserted": 2,
    "errors": [
      "Document 1: must be a JSON object",
      "Document 3: must be a JSON object"
    ]
  },
  "meta": {}
}
```

**Error handling behavior:**
- Each document is validated independently (must be a JSON object, must be under 16 MB)
- Invalid documents are skipped with an error message referencing their index in the array
- All valid documents are written in a single atomic transaction
- If the transaction itself fails (e.g., storage error), all valid documents fail together
- The `inserted` count reflects only successfully written documents

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `400 INVALID_DOCUMENT` — malformed JSON body (the outer request, not individual docs)

### GET /{collection}/docs/{id} — Get Document by ID

```bash
curl http://localhost:8080/events/docs/0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a
```

```json
{
  "ok": true,
  "data": {
    "_id": "0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a",
    "_rev": 1,
    "_created_at": "2026-03-09T14:30:00.000Z",
    "_updated_at": "2026-03-09T14:30:00.000Z",
    "event_type": "firewall",
    "severity": "high"
  },
  "meta": {}
}
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `404 DOCUMENT_NOT_FOUND`

### PUT /{collection}/docs/{id} — Replace Document

Full replacement of the document body. System fields (`_id`, `_created_at`, `_received_at`) are preserved. `_rev` is incremented. `_updated_at` is refreshed.

Include `_rev` in the body to enable optimistic concurrency — if it doesn't match the stored revision, the update is rejected.

```bash
curl -X PUT http://localhost:8080/events/docs/0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a \
  -H "Content-Type: application/json" \
  -d '{
    "_rev": 1,
    "event_type": "firewall",
    "severity": "critical",
    "resolved": false
  }'
```

```json
{
  "ok": true,
  "data": {
    "_id": "0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a",
    "_rev": 2,
    "_created_at": "2026-03-09T14:30:00.000Z",
    "_updated_at": "2026-03-09T14:35:00.000Z",
    "event_type": "firewall",
    "severity": "critical",
    "resolved": false
  },
  "meta": {}
}
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `404 DOCUMENT_NOT_FOUND`
- `409 DOCUMENT_CONFLICT` — `_rev` mismatch
- `400 INVALID_DOCUMENT` — malformed JSON

### PATCH /{collection}/docs/{id} — Partial Update

Applies a [JSON Merge Patch (RFC 7396)](https://tools.ietf.org/html/rfc7396). Only specified fields are modified. Set a field to `null` to remove it.

```bash
curl -X PATCH http://localhost:8080/events/docs/0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a \
  -H "Content-Type: application/json" \
  -d '{
    "severity": "low",
    "resolved": true
  }'
```

```json
{
  "ok": true,
  "data": {
    "_id": "0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a",
    "_rev": 3,
    "_created_at": "2026-03-09T14:30:00.000Z",
    "_updated_at": "2026-03-09T14:40:00.000Z",
    "event_type": "firewall",
    "severity": "low",
    "resolved": true
  },
  "meta": {}
}
```

**Removing a field:**
```bash
curl -X PATCH http://localhost:8080/events/docs/0195e3a1-... \
  -H "Content-Type: application/json" \
  -d '{"resolved": null}'
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `404 DOCUMENT_NOT_FOUND`
- `400 INVALID_DOCUMENT` — malformed JSON

### DELETE /{collection}/docs/{id} — Delete Document

```bash
curl -X DELETE http://localhost:8080/events/docs/0195e3a1-2b3c-7d4e-8f5a-6b7c8d9e0f1a
```

```json
{
  "ok": true,
  "data": { "deleted": true },
  "meta": {}
}
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `404 DOCUMENT_NOT_FOUND`

---

## Querying

### POST /{collection}/query — Query Documents

Query documents using a filter DSL with sort, pagination, projection, and count support.

```bash
curl -X POST http://localhost:8080/events/query \
  -H "Content-Type: application/json" \
  -d '{
    "filter": { "event_type": "firewall" },
    "sort": [{"_created_at": "desc"}],
    "limit": 50,
    "offset": 0
  }'
```

**Response:**
```json
{
  "ok": true,
  "data": [
    {
      "_id": "0195e3a1-...",
      "_rev": 1,
      "event_type": "firewall",
      "severity": "high"
    }
  ],
  "meta": {
    "duration_ms": 5.23,
    "total_count": 142,
    "returned_count": 50,
    "docs_scanned": 10000
  }
}
```

### Query Request Body

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `filter` | object | `null` | Filter DSL (see below). Omit or `null` to match all documents. |
| `sort` | array or object | `[]` | Array of single-field `{"field": direction}` objects (priority in array order), or a single single-field object. Directions: `"asc"`, `"desc"`, `1`, `-1`. |
| `limit` | integer | `100` | Maximum documents to return (capped at `--max-query-limit`, default 100,000). |
| `offset` | integer | `0` | Number of documents to skip (for pagination). |
| `fields` | array | `null` | Projection — list of field names to include. `_id` is always included. |
| `count_only` | boolean | `false` | If `true`, return only the count, not the documents. |
| `cursor` | string | `null` | Opaque pagination token from a previous response's `meta.next_cursor`. Mutually exclusive with `offset`. See **Cursor Pagination**. |

### Filter Operators

#### Comparison Operators

```json
// Implicit equality
{"status": "active"}

// Explicit equality
{"status": {"$eq": "active"}}

// Not equal
{"status": {"$ne": "deleted"}}

// Greater than / greater or equal
{"score": {"$gt": 90}}
{"score": {"$gte": 90}}

// Less than / less or equal
{"price": {"$lt": 100}}
{"count": {"$lte": 5}}

// Value in array
{"status": {"$in": ["active", "pending"]}}

// Value not in array
{"role": {"$nin": ["banned", "suspended"]}}

// Field exists or is missing
{"email": {"$exists": true}}
{"deleted_at": {"$exists": false}}

// Regex match
{"name": {"$regex": "^John"}}

// Array contains value
{"tags": {"$contains": "important"}}
```

**Range comparisons are type-bracketed:** `$gt`/`$gte`/`$lt`/`$lte` only match values of the operand's own comparable type — a number operand matches numbers, a string operand strings, a boolean operand booleans (`{"$gt": false}` matches `true`). Operands of type `null`, array, or object match nothing, even against values of the same type. Indexed and in-memory evaluation agree on this. (Cross-type *ordering* exists too, but only for sorting — see [Sorting](#sorting).)

#### Logical Operators

```json
// AND — explicit
{"$and": [
  {"status": "active"},
  {"score": {"$gte": 90}}
]}

// AND — implicit (multiple top-level keys)
{
  "status": "active",
  "score": {"$gte": 90}
}

// OR
{"$or": [
  {"status": "active"},
  {"role": "admin"}
]}

// NOT
{"$not": {"status": "deleted"}}
```

#### Dot Notation (Nested Fields)

Access nested object fields using dot notation:

```json
{"network.src_ip": "192.168.1.100"}
{"network.dst_port": {"$in": [80, 443, 8080]}}
{"enrichment.geo.country": "US"}
```

### Sorting

Sort by one or more fields. Each sort entry is an object with a single key (field name) and a direction: `"asc"`, `"desc"`, `1`, or `-1`. Field priority follows array order. A single-field sort may also be written as a bare object (`{"sort": {"price": "desc"}}`) — the same two shapes the aggregate `$sort` stage accepts.

Dot notation is supported for sorting on nested fields:

```json
{"sort": [{"network.dst_port": "desc"}, {"_created_at": "asc"}]}
```

Validation is strict: an unrecognized direction value, a sort entry with more than one field, or an empty-object sort is rejected with `400 INVALID_QUERY`. Multi-field specs must use the array form — a flat object with several fields is rejected because JSON object key order is not preserved after parsing.

Documents missing a sort field sort to the **beginning** in ascending order, **end** in descending order.

**Cross-type ordering:** when a sort field holds values of different JSON types, they order `null < false < true < numbers < strings < arrays < objects` (ascending; reversed descending). Arrays and objects order among themselves by their serialized JSON text. This is the same order the index key encoding defines, so indexed and in-memory sorts agree. A missing field is **not** the same as `null`: missing sorts before every present value ascending (`null` is the smallest *present* value).

### Projection

Return only specific fields (always includes `_id`):

```json
{
  "filter": {"event_type": "firewall"},
  "fields": ["event_type", "network.src_ip", "severity"]
}
```

### Cursor Pagination

For walking large result sets, prefer cursors over `offset` — offset pagination re-walks all skipped index entries on every page (and, on paths with a post-filter or sort, re-loads the skipped documents too, since a match can only be counted after evaluation), while a cursor resumes directly from the previous position. Bare index pages skip offset entries at the key level, and `$in`/`$or`-union/bitmap pages at the id level (see **Windowed pages** below) — which softens but doesn't remove the cost of deep offsets.

When a query has more matching documents than `limit`, the response includes an opaque token in `meta.next_cursor` (alongside `meta.has_more: true`). Echo it back in the `cursor` field of the next request, keeping the same collection, filter, and sort:

```json
{
  "filter": {"event_type": "firewall"},
  "sort": [{"received_at": "desc"}],
  "limit": 500,
  "cursor": "eyJ2IjoxLCJmIjo..."
}
```

**Response (last page has no `next_cursor`):**
```json
{
  "ok": true,
  "data": [ ... ],
  "meta": {
    "returned_count": 500,
    "has_more": true,
    "next_cursor": "eyJ2IjoxLCJmIjo..."
  }
}
```

Rules and semantics:

- `cursor` is **mutually exclusive with `offset`**, and cannot be combined with `count_only` or `"limit": 0` (all `400 INVALID_QUERY`).
- A cursor is bound to its **collection and sort specification** (fields + directions). Reusing it with a different sort or collection returns `400 INVALID_QUERY`. The **filter is deliberately not bound**: the cursor is purely positional, so you may narrow or widen the filter mid-walk and pagination stays well-defined.
- Pages are **strictly-after**: each page contains the matching documents positioned after the last document of the previous page in the total order (sort fields, then an `_id` tiebreak in the direction of the last sort field). Documents deleted between pages are skipped without repeats or gaps among survivors; a document whose sort value is updated may move behind the cursor (not seen again) or ahead of it (seen on a later page) — inherent to keyset pagination.
- `next_cursor` is emitted for sorted queries, cursor-resumed queries, and no-sort full scans (which stream in `_id` order, so cursor walks of a whole collection work with no sort at all). Index- or bitmap-served queries **without** a sort don't emit one — pass an explicit sort (e.g. `[{"_id": "asc"}]`) to paginate those.
- One exception on emission: an `index_sorted` plan whose index has extra fields *after* the sort fields returns exact `has_more` but no `next_cursor` (its within-tie order can't be resumed safely). Extend the sort to cover the index tail, or create an index that ends at the sort fields.
- A second exception: a page whose boundary document's sort values encode to a token larger than 4 KiB (very long strings, large arrays/objects) also returns exact `has_more` with no `next_cursor` — the server never emits a token it would itself reject on replay. Use `offset` pagination for such data, or sort on a smaller field.
- Cursor-resumed pages served by an index seek or `_id` seek omit `meta.total_count` (they never see the full match set), as does any **early-exited filtered page** (see **Streaming scans** below) — in every case `total_count` is omitted only when `has_more` is `true`; a response with `has_more` absent/false always carries the exact count.
- `limit` is clamped to `--max-query-limit` per page, as usual.

### Count Only

Get just the count of matching documents without returning them:

```json
{
  "filter": {"event_type": "firewall"},
  "count_only": true
}
```

**Response:**
```json
{
  "ok": true,
  "data": { "count": 142 },
  "meta": {
    "duration_ms": 3.1,
    "total_count": 142,
    "docs_scanned": 10000
  }
}
```

**Unfiltered counts are O(1):** with no `filter`, the count is served from the collection's document counter (maintained on every insert/delete path, seeded by a full count at startup) instead of scanning — `scan_strategy: "doc_counter"`, `docs_scanned: 0`, regardless of collection size. Filtered counts use the index fast paths where possible (see below) or a scan. Every `count_only` response reports how it counted in `scan_strategy` (`doc_counter`, `index_eq`, `index_in`, `index_range`, `compound_eq`, `compound_range`, `bitmap`, `or_union`, or `full_scan`).

**Windowed pages:** when an index or bitmap scan needs no post-filter, sort, or cursor, only the requested `offset`/`limit` window of documents is loaded. On this path `total_count` is the index-entry (or bitmap) count and `docs_scanned` reports the window actually fetched; for single-condition and compound-index plans the total comes from a keys-only count and only the window's ids are ever read (the count and the window scan are two reads instants apart — the same snapshot-gap class as the rest of this path). A document deleted between the index read and the load shortens the page rather than shifting it, exactly like the count fast paths. Offset tiling with a constant filter still covers every document exactly once.

**Streaming scans:** scans that must evaluate a filter per document stream instead of materializing. An **unsorted** filtered page stops as soon as the page plus one probe row is full: `has_more` stays exact, page contents and offset tiling are unchanged (same deterministic scan order), and `meta.total_count` is included only when the scan ran to natural exhaustion — i.e. it is omitted exactly when `has_more` is `true`. Clients that need the count of a multi-page result use `count_only`, which always evaluates fully and stays exact. **Sorted** pages keep exact `total_count` (every match is still evaluated; only the requested window stays resident). Unfiltered full-scan pages skip `offset` entries without parsing them and take `total_count` from the collection's document counter. Throughout, `meta.docs_scanned` reports the documents actually loaded and parsed by the scan, so it shrinks when a page early-exits.

### Early Termination (Sorted Index Scan)

When a query uses a compound index that covers both the filter and sort fields, WardSONDB can return results without loading the entire match set. This is indicated by `scan_strategy: "index_sorted"` in the response meta.

```json
{
  "ok": true,
  "data": [...],
  "meta": {
    "duration_ms": 2.1,
    "returned_count": 50,
    "docs_scanned": 50,
    "index_used": "idx_type_time",
    "scan_strategy": "index_sorted",
    "has_more": true
  }
}
```

**Key differences from regular queries:**
- `total_count` is omitted (null) — computing it would negate the performance gain
- `has_more: true` indicates more matching documents exist beyond the returned set
- `scan_strategy` identifies the optimization used

**When it activates:** Requires a compound index whose fields start with the equality filter field(s), followed by **all** sort fields in order with a uniform direction (all `asc` or all `desc`). For example, index `["event_type", "received_at"]` + query `event_type=firewall` sorted by `received_at desc`; or index `["event_type", "severity", "received_at"]` sorted by `[{"severity": "asc"}, {"received_at": "asc"}]`. Extra index fields *after* the sort fields are allowed (they only affect within-tie order; such plans don't emit `next_cursor` — see **Cursor Pagination**). Mixed sort directions fall back to an in-memory sort.

### Compound Range Scan

When a query combines equality on one field with a range on another, and a compound index covers both fields in order, WardSONDB uses a compound range scan. This is indicated by `scan_strategy: "compound_range"` in the response meta.

```json
{
  "ok": true,
  "data": [...],
  "meta": {
    "duration_ms": 3.5,
    "returned_count": 50,
    "total_count": 1200,
    "docs_scanned": 1200,
    "index_used": "idx_type_time",
    "scan_strategy": "compound_range"
  }
}
```

**Example:** With index `["event_type", "received_at"]`, the query `event_type = "firewall" AND received_at >= "2026-03-12T14:00:00Z"` seeks to the `firewall` prefix and range-scans only the matching time window — instead of scanning all 3M firewall docs.

**When it activates:** Requires a compound index where leading field(s) match equality conditions and the next field matches the range condition. Supports `$gt`, `$gte`, `$lt`, `$lte` (including dual-bound ranges like `$gte` + `$lt`). Uncovered conditions become a post-filter.

**count_only optimization:** When `count_only: true` and no post-filter is needed, counts index keys directly with `docs_scanned: 0`.

### Bitmap Scan Accelerator

For queries on low-cardinality fields (e.g., `event_type`, `severity`, `network.action`), WardSONDB can use Roaring Bitmaps to skip full-collection JSON deserialization. This is indicated by `scan_strategy: "bitmap"` in the response meta.

```json
{
  "ok": true,
  "data": [...],
  "meta": {
    "duration_ms": 15.2,
    "returned_count": 50,
    "total_count": 420000,
    "docs_scanned": 50,
    "scan_strategy": "bitmap"
  }
}
```

**Bitmap count_only** — when all filter fields have bitmap columns and `count_only: true`, the count is computed entirely from bitmaps with zero document reads:

```json
{
  "ok": true,
  "data": { "count": 420000 },
  "meta": {
    "duration_ms": 0.3,
    "total_count": 420000,
    "docs_scanned": 0,
    "scan_strategy": "bitmap"
  }
}
```

**Bitmap-accelerated aggregation** — `$group` by a bitmap field with only `$count` accumulators returns results with zero doc reads (`scan_strategy: "bitmap_aggregate"`). A `$match` + `$group` pipeline where both fields have bitmaps uses `scan_strategy: "bitmap_filtered_aggregate"`.

**When it activates:**
- The bitmap scan accelerator must be enabled and ready (not `--no-bitmap`)
- The filtered field(s) must have bitmap columns (configured via `--bitmap-fields` or auto-detected)
- The field's cardinality must be below `--bitmap-max-cardinality` (default: 1000)
- For `count_only` queries: bitmap is preferred over indexes when all filter fields have bitmap columns (~2500x faster than index counting at scale)
- For document-returning queries: indexes take priority over bitmaps

**Supported filter operators:** `$eq`, `$ne`, `$in`, `$exists`, `$and`, `$or`. For `$and` filters mixing bitmap and non-bitmap fields, the bitmap narrows the candidate set and a residual post-filter applies to the reduced set.

**Sort caveat:** Bitmap scans do not provide sort order. For sort + limit queries where a compound index exists, `IndexSorted` is preferred.

### Query Examples

**Find high-severity firewall events from a specific IP:**
```bash
curl -X POST http://localhost:8080/events/query \
  -H "Content-Type: application/json" \
  -d '{
    "filter": {
      "$and": [
        {"event_type": "firewall"},
        {"severity": {"$in": ["high", "critical"]}},
        {"network.src_ip": "192.168.1.100"}
      ]
    },
    "sort": [{"_created_at": "desc"}],
    "limit": 20
  }'
```

**Find documents where a field exists:**
```bash
curl -X POST http://localhost:8080/events/query \
  -H "Content-Type: application/json" \
  -d '{
    "filter": {"network.dst_port": {"$exists": true}},
    "fields": ["event_type", "network.dst_port"],
    "limit": 10
  }'
```

**Count blocked firewall events:**
```bash
curl -X POST http://localhost:8080/events/query \
  -H "Content-Type: application/json" \
  -d '{
    "filter": {
      "event_type": "firewall",
      "network.action": "block"
    },
    "count_only": true
  }'
```

---

## Aggregation

### POST /{collection}/aggregate — Aggregation Pipeline

Execute a multi-stage aggregation pipeline (maximum 100 stages). Each stage transforms the document set sequentially.

```bash
curl -X POST http://localhost:8080/events/aggregate \
  -H "Content-Type: application/json" \
  -d '{
    "pipeline": [
      {"$match": {"network.action": "block"}},
      {"$group": {"_id": "network.src_ip", "count": {"$count": {}}, "last_seen": {"$max": "received_at"}}},
      {"$sort": {"count": "desc"}},
      {"$limit": 10}
    ]
  }'
```

**Response:**
```json
{
  "ok": true,
  "data": [
    {"_id": "79.124.58.250", "count": 1034, "last_seen": "2026-03-09T15:42:31Z"},
    {"_id": "185.220.101.34", "count": 892, "last_seen": "2026-03-09T15:41:00Z"}
  ],
  "meta": {
    "duration_ms": 45.2,
    "docs_scanned": 50000,
    "groups": 142,
    "index_used": "idx_network_action"
  }
}
```

### Pipeline Stages

| Stage | Description |
|-------|-------------|
| `$match` | Filter documents (same DSL as query filter) |
| `$group` | Group by field(s) and compute accumulators |
| `$sort` | Sort results by field(s) |
| `$limit` | Limit number of results |
| `$skip` | Skip N results |

Stages execute sequentially — the output of one stage feeds the input of the next. Place `$match` first for best performance (reduces documents before grouping).

**Index-accelerated aggregation:** When a pipeline starts with a `$match` stage and the filter targets an indexed field, the aggregation engine uses the index to narrow the scan set instead of performing a full collection scan. The response `meta` includes `index_used` when an index was used.

### $group Stage

```json
{
  "$group": {
    "_id": "<group-key>",
    "field_name": {"<accumulator>": "<field-path>"}
  }
}
```

**Group key (`_id`) options:**

```json
// Group by a single field
"_id": "event_type"

// Group by a nested field (dot notation)
"_id": "network.src_ip"

// Multi-field grouping
"_id": {"type": "event_type", "action": "network.action"}

// Group all documents into one result
"_id": null
```

**Accumulators:**

| Accumulator | Value | Description |
|-------------|-------|-------------|
| `$count` | `{}` | Count documents in group |
| `$sum` | `"field.path"` | Sum numeric values |
| `$avg` | `"field.path"` | Average of numeric values |
| `$min` | `"field.path"` | Minimum value by the cross-type sort order (see [Sorting](#sorting)) |
| `$max` | `"field.path"` | Maximum value by the cross-type sort order (see [Sorting](#sorting)) |
| `$collect` | `"field.path"` | Collect unique values into an array sorted by the cross-type sort order (capped at 1000) |

### $sort Stage (in aggregation)

Sort results by one or more fields. Accepts the same shapes as the `/query` endpoint's `sort` field — a single-field object, or an array of single-field objects for multi-field sorts (priority in array order). Directions: `"asc"`/`"desc"` or `1`/`-1`:

```json
{"$sort": {"count": "desc"}}
{"$sort": [{"count": -1}, {"name": 1}]}
```

A flat object with several fields (`{"count": -1, "name": 1}`) is rejected with `400 INVALID_PIPELINE`: JSON object key order is not preserved after parsing, so field priority would be undefined — use the array form.

### Aggregation Examples

**Top blocked IPs (SIEM dashboard):**
```bash
curl -X POST http://localhost:8080/events/aggregate \
  -H "Content-Type: application/json" \
  -d '{
    "pipeline": [
      {"$match": {"network.action": "block", "received_at": {"$gte": "2026-03-09T00:00:00Z"}}},
      {"$group": {"_id": "network.src_ip", "count": {"$count": {}}, "last_seen": {"$max": "received_at"}}},
      {"$sort": {"count": "desc"}},
      {"$limit": 10}
    ]
  }'
```

**Event type breakdown with severity stats:**
```bash
curl -X POST http://localhost:8080/events/aggregate \
  -H "Content-Type: application/json" \
  -d '{
    "pipeline": [
      {"$group": {
        "_id": "event_type",
        "count": {"$count": {}},
        "avg_severity": {"$avg": "severity"},
        "max_severity": {"$max": "severity"}
      }},
      {"$sort": {"count": "desc"}}
    ]
  }'
```

**Multi-field grouping (type + action breakdown):**
```bash
curl -X POST http://localhost:8080/events/aggregate \
  -H "Content-Type: application/json" \
  -d '{
    "pipeline": [
      {"$group": {
        "_id": {"type": "event_type", "action": "network.action"},
        "count": {"$count": {}}
      }},
      {"$sort": {"count": "desc"}},
      {"$limit": 20}
    ]
  }'
```

**Global totals (all docs in one group):**
```bash
curl -X POST http://localhost:8080/events/aggregate \
  -H "Content-Type: application/json" \
  -d '{
    "pipeline": [
      {"$group": {
        "_id": null,
        "total_events": {"$count": {}},
        "total_severity": {"$sum": "severity"},
        "avg_severity": {"$avg": "severity"}
      }}
    ]
  }'
```

**$collect example — collect unique destination ports per source IP:**
```bash
curl -X POST http://localhost:8080/events/aggregate \
  -H "Content-Type: application/json" \
  -d '{
    "pipeline": [
      {"$group": {
        "_id": "network.src_ip",
        "ports": {"$collect": "network.dst_port"},
        "count": {"$count": {}}
      }},
      {"$sort": {"count": "desc"}},
      {"$limit": 10}
    ]
  }'
```

**$collect response:**
```json
{
  "ok": true,
  "data": [
    {"_id": "10.0.0.1", "ports": [22, 80, 443], "count": 150}
  ]
}
```

When the unique value cap (1000) is reached, the `$collect` result becomes an object with `_truncated: true`:
```json
{"ports": {"values": [...], "_truncated": true}}
```

**Errors:**
- `400 INVALID_PIPELINE` — empty pipeline, unknown stage, malformed $group spec, unknown accumulator
- `404 COLLECTION_NOT_FOUND`

---

## Distinct Values

### POST /{collection}/distinct — Get Unique Field Values

Return distinct values for a field, optionally filtered.

```bash
curl -X POST http://localhost:8080/events/distinct \
  -H "Content-Type: application/json" \
  -d '{"field": "event_type", "filter": {"severity": "high"}, "limit": 1000}'
```

**Request Body:**

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `field` | string | (required) | Field path to collect unique values from (dot notation supported) |
| `filter` | object | `null` | Optional filter (same DSL as query) |
| `limit` | integer | `1000` | Max unique values to return |

**Response:**
```json
{
  "ok": true,
  "data": {
    "field": "event_type",
    "values": ["dns", "firewall", "ids"],
    "count": 3,
    "truncated": false
  },
  "meta": {"duration_ms": 12.3, "docs_scanned": 50000, "index_used": "idx_event_type"}
}
```

When the field is indexed and no filter is specified, the query uses an **index-only scan** (`docs_scanned: 0`).

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `400 INVALID_QUERY` — malformed filter

---

## Indexes

### POST /{collection}/indexes — Create Index

Create a secondary index on one or more field paths. Backfills existing documents.

**Single-field index** — use `field` (string):

```bash
curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_event_type", "field": "event_type"}'
```

**Compound index** — use `fields` (array of strings):

```bash
curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_type_time", "fields": ["event_type", "received_at"]}'
```

Provide either `field` (single field) or `fields` (compound index), not both. If `field` is provided, it is treated as a single-element `fields` array internally.

**Response (201 Created):**
```json
{
  "ok": true,
  "data": {
    "name": "idx_event_type",
    "collection": "events",
    "fields": ["event_type"],
    "created_at": "2026-03-09T14:30:00Z"
  },
  "meta": {}
}
```

The response always includes `fields` as an array, regardless of whether the index was created with `field` or `fields`.

**Compound index response example:**
```json
{
  "ok": true,
  "data": {
    "name": "idx_type_time",
    "collection": "events",
    "fields": ["event_type", "received_at"],
    "created_at": "2026-03-09T14:30:00Z"
  },
  "meta": {}
}
```

**Selective indexing:** Documents missing any of the indexed fields are skipped — no index entry is created for them.

**Index name rules:** alphanumeric characters, underscores, and hyphens only.

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `409 INDEX_EXISTS` — the index name is taken, or a single-field index on
  that exact field already exists. A compound index sharing the leading
  field does NOT conflict with a new single-field index: compound indexes
  only contain documents carrying **all** their component fields, so they
  never serve single-field lookups — the single-field index is the real one
- `400 INVALID_INDEX` — empty name, empty field/fields, or both `field` and `fields` provided

### GET /{collection}/indexes — List Indexes

```bash
curl http://localhost:8080/events/indexes
```

```json
{
  "ok": true,
  "data": [
    {"name": "idx_event_type", "collection": "events", "fields": ["event_type"], "created_at": "..."},
    {"name": "idx_src_ip", "collection": "events", "fields": ["network.src_ip"], "created_at": "..."},
    {"name": "idx_type_time", "collection": "events", "fields": ["event_type", "received_at"], "created_at": "..."}
  ],
  "meta": {}
}
```

### DELETE /{collection}/indexes/{name} — Drop Index

```bash
curl -X DELETE http://localhost:8080/events/indexes/idx_event_type
```

```json
{
  "ok": true,
  "data": {"dropped": true, "name": "idx_event_type"},
  "meta": {}
}
```

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `404 INDEX_NOT_FOUND`

### Query Acceleration

When a query filter includes an indexed field, the query planner automatically uses the index:

```bash
# This query uses idx_event_type instead of scanning all documents
curl -X POST http://localhost:8080/events/query \
  -H "Content-Type: application/json" \
  -d '{"filter": {"event_type": "firewall"}, "count_only": true}'
```

The response includes `meta.index_used` when an index is used:
```json
{
  "ok": true,
  "data": {"count": 5000},
  "meta": {"duration_ms": 1.2, "total_count": 5000, "docs_scanned": 0, "index_used": "idx_event_type"}
}
```

**Supported index operations:** `$eq` (implicit and explicit), `$in`, `$gt`, `$gte`, `$lt`, `$lte`.

**`$or` index union (`scan_strategy: "or_union"`):** a `$or` whose arms are each individually servable by an index (single-field conditions on indexed fields, or `$and` arms coverable by a compound/single-field index) runs one index lookup per arm and unions the results instead of scanning the collection. Results are identical to the full-scan evaluation — same documents, same order, same pages — with `docs_scanned` reduced to the documents actually loaded (the page window on bare pages; the candidates hydrated before the page filled on residual pages) and `index_used` listing the arm indexes (`+`-joined). One arm that no index can serve (an unindexed field, a nested `$or`, `$regex`/`$not`) disables the union and the query full-scans as before.

**Compound filters:** In an `AND` filter, if one field is indexed, the index narrows the candidate set and the remaining conditions are applied as a post-filter.

**Optimized `count_only`:** When `count_only: true` and the filter is a single indexed field condition with no post-filter, the count is computed directly from the index keys — zero document deserialization (`docs_scanned: 0`).

### Recommended SIEM Indexes

```bash
# Time-range queries
curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_received_at", "field": "received_at"}'

# Event type filtering
curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_event_type", "field": "event_type"}'

# Network fields
curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_network_action", "field": "network.action"}'

curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_src_ip", "field": "network.src_ip"}'

curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_dst_ip", "field": "network.dst_ip"}'

curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_dst_port", "field": "network.dst_port"}'

# Compound index for type + time queries (e.g., SIEM dashboards filtering by event type within a time range)
curl -X POST http://localhost:8080/events/indexes \
  -H "Content-Type: application/json" \
  -d '{"name": "idx_type_time", "fields": ["event_type", "received_at"]}'
```

---

## Bulk Operations

### POST /{collection}/docs/_delete_by_query — Delete by Query

Delete all documents matching a filter.

```bash
curl -X POST http://localhost:8080/events/docs/_delete_by_query \
  -H "Content-Type: application/json" \
  -d '{"filter": {"received_at": {"$lt": "2026-03-01T00:00:00Z"}}}'
```

**Response:**
```json
{
  "ok": true,
  "data": {"deleted": 15000},
  "meta": {"duration_ms": 120.5}
}
```

Uses the same filter DSL as `/query`. All matching documents and their index entries are removed in a single transaction.

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `400 INVALID_QUERY` — malformed filter

### POST /{collection}/docs/_update_by_query — Update by Query

Update all documents matching a filter using `$set` operations. Supports dot-notation for nested fields (intermediate objects are created automatically).

```bash
curl -X POST http://localhost:8080/events/docs/_update_by_query \
  -H "Content-Type: application/json" \
  -d '{
    "filter": {"network.src_ip": "1.2.3.4"},
    "update": {
      "$set": {
        "enrichment.src.geo_country": "US",
        "enrichment.src.abuse_score": 42
      }
    }
  }'
```

**Response:**
```json
{
  "ok": true,
  "data": {"updated": 350},
  "meta": {"duration_ms": 85.3}
}
```

Each updated document gets `_rev` incremented and `_updated_at` refreshed. Index entries are updated in the same transaction.

**Errors:**
- `404 COLLECTION_NOT_FOUND`
- `400 INVALID_QUERY` — malformed filter or missing `$set`

---

## TTL / Auto-Expiry

Per-collection retention policy with background cleanup. The server runs a cleanup task at the configured interval (default 60s, adjustable via `--ttl-interval`).

### PUT /{collection}/ttl — Set TTL Policy

```bash
curl -X PUT http://localhost:8080/events/ttl \
  -H "Content-Type: application/json" \
  -d '{"retention_days": 30, "field": "_created_at"}'
```

| Field | Type | Default | Description |
|-------|------|---------|-------------|
| `retention_days` | integer | (required) | Documents older than this are deleted |
| `field` | string | `_created_at` | ISO 8601 timestamp field to evaluate |

**Response:**
```json
{
  "ok": true,
  "data": {"retention_days": 30, "field": "_created_at", "enabled": true},
  "meta": {}
}
```

### GET /{collection}/ttl — Get TTL Policy

```bash
curl http://localhost:8080/events/ttl
```

### DELETE /{collection}/ttl — Remove TTL Policy

```bash
curl -X DELETE http://localhost:8080/events/ttl
```

TTL status is included in `GET /_stats` (active policies count, last cleanup timestamp).

---

## Authentication

API key authentication is opt-in. When API keys are configured, all endpoints except `GET /_health` require a valid key.

### Configuration

```bash
# Single key
wardsondb --storage-engine rocksdb --api-key "my-secret-key"

# Multiple keys
wardsondb --storage-engine rocksdb --api-key "key1" --api-key "key2"

# Key file (one key per line, # for comments)
wardsondb --storage-engine rocksdb --api-key-file /etc/wardsondb/keys.txt
```

### Usage

Include the API key in requests via either header:

```bash
# Authorization: Bearer
curl -H "Authorization: Bearer my-secret-key" http://localhost:8080/_stats

# X-API-Key
curl -H "X-API-Key: my-secret-key" http://localhost:8080/_stats
```

### Behavior

- No keys configured → open access (current default)
- Keys configured → all endpoints require auth except `GET /_health`
- `GET /_metrics` follows auth policy unless `--metrics-public` is set
- Invalid/missing key returns `401 UNAUTHORIZED`

---

## Storage Info

### GET /{collection}/storage — Collection Storage Details

```bash
curl http://localhost:8080/events/storage
```

```json
{
  "ok": true,
  "data": {
    "name": "events",
    "doc_count": 2118648,
    "index_count": 8,
    "indexes": ["idx_event_type", "idx_src_ip"],
    "oldest_doc": "2026-03-09T23:49:57+00:00",
    "newest_doc": "2026-03-10T14:36:09+00:00",
    "ttl": {"retention_days": 30, "field": "_created_at", "enabled": true},
    "scan_accelerator": {
      "total_positions": 2118648,
      "bitmap_columns": [
        {"field": "event_type", "cardinality": 9, "memory_bytes": 1835008},
        {"field": "severity", "cardinality": 4, "memory_bytes": 524288}
      ]
    }
  },
  "meta": {}
}
```

`oldest_doc` and `newest_doc` are derived from UUIDv7 key timestamps (O(1) lookup, no full scan). The `scan_accelerator` section is only present when the accelerator is ready.

---

## Prometheus Metrics

### GET /_metrics — Prometheus Exposition Format

```bash
curl http://localhost:8080/_metrics
```

Returns metrics in Prometheus text format (`Content-Type: text/plain; version=0.0.4`):

```
# HELP wardsondb_uptime_seconds Server uptime in seconds
# TYPE wardsondb_uptime_seconds gauge
wardsondb_uptime_seconds 3600
# HELP wardsondb_documents_total Total documents across all collections
# TYPE wardsondb_documents_total gauge
wardsondb_documents_total 2118648
# HELP wardsondb_collections_total Total number of collections
# TYPE wardsondb_collections_total gauge
wardsondb_collections_total 3
# HELP wardsondb_requests_total Lifetime request count
# TYPE wardsondb_requests_total counter
wardsondb_requests_total 81367
# HELP wardsondb_inserts_total Lifetime insert count
# TYPE wardsondb_inserts_total counter
wardsondb_inserts_total 417710
# HELP wardsondb_queries_total Lifetime query count
# TYPE wardsondb_queries_total counter
wardsondb_queries_total 63200
# HELP wardsondb_deletes_total Lifetime delete count
# TYPE wardsondb_deletes_total counter
wardsondb_deletes_total 500
# HELP wardsondb_storage_poisoned Whether the storage engine is poisoned (0 or 1)
# TYPE wardsondb_storage_poisoned gauge
wardsondb_storage_poisoned 0
# HELP wardsondb_ttl_active_policies Number of active TTL policies
# TYPE wardsondb_ttl_active_policies gauge
wardsondb_ttl_active_policies 1
```

By default follows auth policy. Use `--metrics-public` to allow unauthenticated access for Prometheus scrapers.

---

## Error Codes

| HTTP Status | Code | Description |
|-------------|------|-------------|
| 400 | `INVALID_DOCUMENT` | Malformed JSON body or invalid input |
| 400 | `INVALID_QUERY` | Malformed query DSL |
| 400 | `INVALID_PIPELINE` | Malformed aggregation pipeline |
| 400 | `INVALID_INDEX` | Bad index definition |
| 401 | `UNAUTHORIZED` | Missing or invalid API key (when auth is configured) |
| 400 | `SCHEMA_VIOLATION` | Document fails JSON Schema validation |
| 404 | `COLLECTION_NOT_FOUND` | Collection does not exist |
| 404 | `DOCUMENT_NOT_FOUND` | Document ID not found in collection |
| 404 | `INDEX_NOT_FOUND` | Index does not exist |
| 409 | `COLLECTION_EXISTS` | Collection with that name already exists |
| 409 | `INDEX_EXISTS` | Index with that name already exists |
| 409 | `DOCUMENT_CONFLICT` | Revision mismatch on update |
| 408 | `QUERY_TIMEOUT` | Read exceeded `--query-timeout` |
| 413 | `DOCUMENT_TOO_LARGE` | Document exceeds the 16 MB per-document limit, or the request body exceeds `--max-body-mb` |
| 500 | `INTERNAL_ERROR` | Unexpected server error |
| 503 | `STORAGE_POISONED` | Storage engine suffered a fatal flush/compaction failure. Writes rejected, reads may continue. Restart required. |

---

## Performance

Benchmarked against 3.45 million production SIEM events on Mac Studio (M4 Max, 128GB RAM, 1.8TB SSD), release build.

| Operation | Time | Notes |
|-----------|------|-------|
| Bitmap aggregate: count by event_type | 0.096ms | 0 docs scanned (bitmap_aggregate) |
| Bitmap count: severity = 6 (no index) | 0.17ms | 0 docs scanned (bitmap) |
| Compound range: type + time ≥ 6h (32K matches) | 5.5ms | 0 docs scanned (compound_range) |
| Indexed equality + sort + limit 50 | 9.5ms | 50 docs scanned (index_sorted) |
| Indexed count (3M matches) | 432ms | 0 docs scanned (index_eq) |
| Compound EQ: type + action (2.9M matches) | 485ms | 0 docs scanned (compound_eq) |
| Get by ID | <1ms | Direct key lookup |
| Single doc insert | ~13 µs | 76,000+/sec throughput |
| Bulk insert (500 docs) | ~1.8ms | 278,000+ docs/sec throughput |
| Unindexed full scan (3.45M docs) | 5-15 sec | Create indexes or enable bitmaps |

All numbers measured against 3.45 million production SIEM events. Run `cargo bench` for reproducible synthetic benchmarks.

Run benchmarks yourself:
```bash
cargo bench
```

---

## Technical Notes

### Concurrency

The database uses `parking_lot` for internal synchronization (RwLock, Mutex). Unlike the standard library locks, `parking_lot` locks do not poison on panic, which avoids cascading failures if a single request panics. This means the database remains available even after an unexpected panic in a handler.

### fjall Memory Configuration

The storage engine (fjall) is configured with the following memory limits, all configurable via CLI flags:

| Parameter | Default | CLI Flag | Description |
|-----------|---------|----------|-------------|
| `cache_size` | 64 MiB | `--cache-size-mb` | Unified block + blob cache shared across all partitions |
| `max_write_buffer_size` | 64 MiB | `--write-buffer-mb` | Total write buffer cap across all partitions |
| `max_memtable_size` | 8 MiB | `--memtable-mb` | Maximum memtable size per partition before flush |
| `flush_workers` | 2 | `--flush-workers` | Background threads for flushing memtables to disk |
| `compaction_workers` | 2 | `--compaction-workers` | Background threads for LSM-tree compaction |

For high-memory systems handling millions of documents, see the Memory Tuning section in the README for recommended production values.

These values are reported in the `GET /_stats` response under `memory_config`. If the storage engine encounters a fatal error during flush or compaction, it enters a **poisoned** state: new writes are rejected (returning `503 STORAGE_POISONED`), but reads may continue to serve from cached data. A server restart is required to recover.

---

## Limits & Security

| Resource | Limit | Notes |
|----------|-------|-------|
| Query `limit` | 100,000 | Silently clamped; default 100; configurable via `--max-query-limit` |
| Request body | 64 MiB | Configurable via `--max-body-mb`; oversized requests get 413 `DOCUMENT_TOO_LARGE` |
| Bulk insert | 10,000 documents | Returns 400 if exceeded |
| Pipeline stages | 100 | Returns 400 if exceeded |
| Filter nesting depth | 20 | `$and`/`$or`/`$not` nesting |
| Filter branch count | 1,000 | Max children per `$and`/`$or` |
| Dot-notation depth | 20 | Max segments in `a.b.c...` paths |
| Regex pattern length | 1,024 chars | Regex uses Rust `regex` crate (linear-time, no backtracking) |
| Document size | 16 MB | Per document |
| Query timeout | 30 seconds | Applies to query, aggregate, distinct, and get-by-id; configurable via `--query-timeout` (0 = no timeout) |
| `$collect` accumulator | 1,000 unique values | Returns `{"values": [...], "_truncated": true}` if cap is reached |
| API key comparison | Constant-time | Uses `subtle` crate to prevent timing attacks |

---

## Planned Features (not yet implemented)

| Feature | Endpoint / Config | Priority | Notes |
|---------|-------------------|----------|-------|
| Streaming (NDJSON) | TBD | Medium | Push large result sets over one response; cursor pagination (shipped — see **Cursor Pagination**) is the building block |
| Query explain | `POST /{collection}/query?explain` | Medium | Show scan strategy and index usage |
| Schema validation | `PUT /{collection}/schema` | Lower | Optional JSON Schema on inserts/updates |

---

## Quick Start Example

A complete workflow from starting the server to querying data:

```bash
# 1. Start the server
wardsondb --storage-engine rocksdb --port 8080

# 2. Create a collection
curl -X POST http://localhost:8080/_collections \
  -H "Content-Type: application/json" \
  -d '{"name": "events"}'

# 3. Insert some documents
curl -X POST http://localhost:8080/events/docs \
  -H "Content-Type: application/json" \
  -d '{"event_type": "firewall", "network": {"src_ip": "10.0.0.1", "action": "block"}, "severity": "high"}'

curl -X POST http://localhost:8080/events/docs \
  -H "Content-Type: application/json" \
  -d '{"event_type": "dns", "query": "example.com", "severity": "low"}'

curl -X POST http://localhost:8080/events/docs \
  -H "Content-Type: application/json" \
  -d '{"event_type": "firewall", "network": {"src_ip": "10.0.0.2", "action": "allow"}, "severity": "low"}'

# 4. Query for high-severity firewall events
curl -X POST http://localhost:8080/events/query \
  -H "Content-Type: application/json" \
  -d '{"filter": {"event_type": "firewall", "severity": "high"}}'

# 5. Update a document (partial)
curl -X PATCH http://localhost:8080/events/docs/<ID_FROM_STEP_3> \
  -H "Content-Type: application/json" \
  -d '{"resolved": true}'

# 6. Check server stats
curl http://localhost:8080/_stats

# 7. Drop the collection when done
curl -X DELETE http://localhost:8080/events
```
