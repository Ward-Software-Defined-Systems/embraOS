# Changelog

All notable changes to WardSONDB are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/). WardSONDB does not
yet cut versioned releases, so everything sits under **Unreleased** until the
first tagged version.

## [Unreleased]

### Changed

Behavior changes an existing client could observe, most significant first.

- **Streaming scans: `total_count` is omitted on early-exited filtered
  pages.** Unsorted filtered pages now stop scanning once the page plus one
  probe row is full, so `meta.total_count` appears only when the scan ran to
  natural exhaustion — omitted exactly when `has_more` is `true` (single-page
  results, final pages, `count_only`, sorted queries, and unfiltered pages
  all keep exact counts; unfiltered full-scan pages serve theirs from the
  document counter). Page contents, ordering, offset tiling, and `has_more`
  exactness are unchanged.
- **`meta.docs_scanned` now reports documents actually loaded and parsed.**
  Early-exited pages report the (smaller) number of documents visited before
  the page filled; unfiltered full-scan pages skip `offset` entries without
  parsing them and report only the page window; the three paths that
  previously reported the raw candidate count (indexed, compound-range, and
  `$or`-union materializing pages) now match the rest. `distinct` similarly
  reports documents visited (it stops once `limit` distinct values are
  found) instead of always the collection size.
- **Bitmap-accelerated queries no longer cross-match values of different
  types.** Value bitmaps are now keyed with type tags: an accelerated
  equality scan on the string `"123"` previously also matched documents
  holding the number `123` (and the string `"__null__"` matched `null`);
  bitmap-aggregate `$group` keys now come back with their original types
  instead of being re-guessed (a string `"123"` group returned the number
  `123`). Persisted bitmap snapshots carry a format version — the first
  start after upgrading rebuilds the accelerator from storage once.
- **Bitmap accelerator restart and rebuild correctness.** A loaded snapshot
  (up to one persist-interval stale) is now reconciled against the document
  counters and rebuilt on any disagreement, instead of silently dropping
  post-persist documents from results; live rebuilds queue concurrent
  writes and drain them before serving instead of racing the position
  counter they were resetting.
- **Mid-scan storage errors now fail the request instead of silently
  truncating results.** Index lookups (including `$in`, which previously
  skipped errored values), and the key scans behind `DELETE` collection/index
  operations, surface engine iteration errors as 500s; a collection drop can
  no longer commit a truncated key removal. Unreachable in healthy
  deployments — this only changes what a failing disk looks like.
- **Numeric `-1` sort direction on `/query` now sorts descending.** It was
  silently accepted and sorted ASCENDING. Clients calibrated to the buggy
  order will see reversed results with no error.
- **Mixed-type sorts use one total cross-type order** — `null < false < true
  < numbers < strings < arrays < objects`, identical to the index key
  encoding (arrays/objects order among themselves by serialized JSON text;
  `-0.0` sorts before `0.0`). Previously values of different types compared
  "equal", which could return HTTP 500 on mixed-type sorts (total-order
  panic), silently skip documents on cursor walks, and make
  `$min`/`$max`/`$collect` pick arbitrary results on mixed-type groups.
  Missing fields still sort before all present values ascending and remain
  distinct from present `null`.
- **Indexed range queries are type-bracketed to match in-memory filtering.**
  `$gt`/`$gte`/`$lt`/`$lte` with an open bound no longer leak other types'
  values out of the index (an indexed `{"$gt": 5}` previously returned
  strings, arrays, and objects; `{"$gt": null}` returned every non-null
  document), and range operators with `null`/array/object operands now match
  nothing on every path — indexed results, counts, and full scans agree.
- **Strict sort-spec validation (new 400s):** a flat-object sort with more
  than one field, an unrecognized direction value, or an empty-object sort is
  rejected with `400 INVALID_QUERY` on both `/query` and the `$sort` stage.
- **`has_more` now appears on offset queries** and is exact on materializing
  paths.
- **Windowed bare pages:** index/bitmap page reads with no residual filter,
  sort, or cursor load only the requested window — `total_count` reports the
  full index/bitmap count while `docs_scanned` is the window size.
- **`scan_strategy` is populated on every `count_only` response**
  (`doc_counter`, `index_eq`, `index_in`, `index_range`, `compound_eq`,
  `compound_range`, `bitmap`, `full_scan`).
- **`$in` counts with duplicate values got smaller** — duplicate values were
  double-counted; they are now deduplicated (a bug fix, but visible to
  clients that relied on the inflated numbers).
- **Request logging is opt-in:** per-request lines land in the log file only
  with `--verbose`; default logging is lifecycle/warn/error only.
- **Bitmap auto-detection no longer creates columns.** It logs a
  recommendation instead; `--bitmap-fields` is the only activation path
  (auto-created columns could silently miss pre-detection documents and
  return short query results).
- **Oversize cursors are omitted rather than emitted.** A page whose boundary
  document's sort values encode past the 4 KiB token cap returns exact
  `has_more` with no `next_cursor` (previously the emitted token was rejected
  with a 400 on replay, making the next page unreachable).
- **Aggregate `$group`/`$collect` serialization failures now return 500**
  instead of silently collapsing the failing groups onto an empty-string key.

### Added

- **Opaque cursor pagination:** `cursor` request field and `meta.next_cursor`,
  fingerprint-bound to the collection and sort spec; exact `has_more`;
  index-seek resume on `index_sorted` plans and no-sort walks.
- **`--max-body-mb`** (default 64 MiB) replaces the previous 2 MiB framework
  default; overruns return `413 DOCUMENT_TOO_LARGE`.
- **`--bitmap-sample-size`** now takes effect (it was a documented no-op).
- **Sort spec shapes:** array form and single-key object form, with
  `asc`/`desc`/`1`/`-1` directions, accepted uniformly by `/query` and
  `$sort`.
- **`$or` index-union planning (`scan_strategy: "or_union"`):** a `$or` whose
  arms are each individually servable by an index runs one index lookup per
  arm and unions the results instead of scanning the collection. Results are
  identical to full-scan evaluation (same documents, order, pages, and
  counts — arm overlap is deduplicated); one unindexable arm falls back to
  the full scan as before. A partially bitmap-covered `$or` — which
  previously fell back to a full scan — now uses the index union when its
  arms are indexed.

### Fixed

- **RocksDB deployments no longer accumulate unbounded RSS on Linux.**
  RocksDB's C++ allocations went through glibc malloc (the process-wide
  jemalloc `#[global_allocator]` covers Rust allocations only), hitting
  ptmalloc2's per-thread arena retention under sustained ingest +
  compaction: a live soak ratcheted to 81 GB RSS (~40 GB/day) with ~10 GB
  actually live. `rust-rocksdb` now builds with its `jemalloc` feature,
  which exports jemalloc as the process-wide C allocator so RocksDB's
  allocations release properly (RocksDB LOG header now reads
  `Jemalloc supported: 1`). fjall deployments (pure Rust) were never
  affected.
- **Queries on a missing collection return `404 COLLECTION_NOT_FOUND` on
  every plan shape.** The existence check previously ran only on the
  full-scan path, so `/query`, `/aggregate`, and `/distinct` requests served
  by the bitmap or index-only fast paths answered `200` with empty results
  for collections that don't exist (e.g. just-dropped ones).
- **Single-field queries are no longer served from compound indexes.**
  With no single-field index, an equality/`$in`/range filter on a field was
  answered from an arbitrary compound index leading with it — but compound
  indexes exclude documents missing any of their other component fields, so
  such queries silently dropped those documents (and WHICH compound index
  answered was arbitrary per process, so results could differ across
  restarts). These filters now fall through to the bitmap accelerator or a
  full scan; create a real single-field index for hot paths — doing so is
  no longer misdetected as a duplicate of the compound index.
- **Bitmap-accelerated answers are now collection-scoped.** The accelerator
  shares one position space across all collections, and fully-covered
  filters leaked that: `count_only` could report more matches than the
  collection holds (other collections' documents sharing the value were
  counted), windowed pages reported the cross-collection total and returned
  empty pages with `has_more: true` past the collection's real matches, and
  bitmap-served `$group` counts summed every collection. Single-collection
  deployments were unaffected. Persisted bitmap snapshots move to format v3
  (per-collection membership); the first start after upgrading rebuilds the
  accelerator from storage once.
- **Dropping a collection no longer disables the bitmap accelerator.** The
  drop now surgically removes only the dropped collection's bitmap data —
  every other collection stays accelerated (previously ALL acceleration was
  cleared and nothing re-enabled it until restart). `/_stats` gains
  `scan_accelerator.positions_by_collection` for verifying the scoping.
- Bitmap-accelerated `$or` with partial column coverage returned the
  **intersection** of the covered arms instead of the union; mixed-coverage
  `$or` now falls back to a full scan (fully-covered `$or` keeps
  acceleration).
- Index-only aggregation undercounted collections containing custom `_id`s.
- `GET /_stats` could deadlock the process against concurrent bitmap-column
  writes (recursive read-lock).
- Unseeded/stomped document counters: `count_only` fast-path counts stay
  exact across collection-creation races and restarts.
- Giant `offset` values return an empty page instead of overflowing the page
  arithmetic (debug panic / wrapped page in release).
- Storage write-batch staging failures (missing column family, engine
  mismatch) return a 500 `BackendError` instead of panicking the process.

### Security

- **Dependency updates resolving all 16 open security advisories** (9 high /
  3 moderate / 4 low): `openssl` 0.10.75 → 0.10.81 (8 advisories; dev-only
  dependency), `rustls-webpki` 0.103.9 → 0.103.13 (4; `--tls` mode),
  `aws-lc-sys` 0.38.0 → 0.42.0 (2; `--tls` mode), `lz4_flex` 0.11.5 → 0.11.6
  (1; fjall block decompression), `rand` 0.8.5 → 0.8.7 (1; dev-only). No
  WardSONDB API or behavior changes; the storage-engine crates (fjall,
  RocksDB) are unchanged.

### Performance

- `$regex` filters compile once at parse time: the regex-scan benchmark
  dropped 644.8 ms → 27.1 ms (−95.8%).
- Windowed bare pages: deep-offset index page reads dropped
  327.8 ms → 15.2 ms (−95.5%).
- Unfiltered `count_only` is served from document counters: ~335 ms → 0.04 ms
  on a ~100k-doc collection.
- Startup seeding and count fast paths count keys inside the backend without
  materializing entries — no more transient whole-collection RAM spike before
  the listener binds.
- Query/aggregate/distinct responses move result documents instead of
  re-serializing deep clones.
- `$or` over indexed fields no longer full-scans: 398.2 ms → 17.4 ms on the
  two-arm 100k-doc benchmark (~23×); count-only unions count deduped index
  keys with zero document loads.
