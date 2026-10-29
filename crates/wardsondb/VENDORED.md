# The vendored fork

`crates/wardsondb` is a vendored fork of [WardSONDB](https://github.com/Ward-Software-Defined-Systems/WardSONDB), the document database embraOS ships. The fork was taken on 2026-07-17 and synced to upstream `main` at `1732213` on 2026-07-24, the base it diverges from. embraOS reaches it over REST on port 8090 and no embraOS crate depends on its library.

The crate keeps upstream's Apache-2.0 licence: `LICENSE` beside this file and `license = "Apache-2.0"` in `Cargo.toml`. The embraOS workspace's own `LICENSE` governs the rest of embraOS, not this crate.

## Divergences from `1732213`

Each is a decision of the embraOS maintainer, recorded in the commit named, and each is meant to be carried upstream.

- `Cargo.toml`: `axum-server = "0.8"` where upstream has `"0.7"` (2026-09-27). It removed `rustls-pemfile`, unmaintained, from the lockfile. At the next sync upstream's line is taken if it moved.
- `tests/integration_test.rs`: the integration tests keep their bound listener and hand it to tokio with `from_std` (2026-10-02). A listener dropped and bound again gave a parallel test the window to take the port.
- `src/engine/ttl.rs`, `src/server/ttl_worker.rs`: `drop_collection` removes `ttl:{name}`, and the worker's `run_one_policy` discards a policy whose collection is gone through `Storage::discard_ttl` (2026-10-02). A dropped collection's policy errored on the ghost every tick.
- `src/engine/storage.rs`, `src/engine/document.rs`: a document's writers hold one of 256 shards of a per-document lock from the read that decides the write to the commit; a bulk mutation takes the shards of a chunk in index order; `partial_update_document` merges and commits on one read (`4e24ce3`, 2026-10-29; the review's #64-1, #64-2 and #42-5).
- `src/server/routes/system.rs`, `src/server/routes/collections.rs`, `src/engine/ttl.rs`: `/_stats`, `/_metrics` and `/{collection}/storage` fail when a `_meta` read fails, and a stored TTL policy that does not parse is a 500 naming its collection (`f40b48b`, 2026-10-29; #64-3).
- `src/engine/document.rs`, `src/server/routes/mod.rs` and the route modules: a path parameter or a custom `_id` that is empty, `.`, `..`, or carries a path separator or a control character is refused with 400 before the engine (`89b1fab`, 2026-10-29; #25).
- `src/config.rs`, `src/main.rs`, `src/query/cancel.rs` and the scan loops of `src/engine/document.rs`, `src/query/executor.rs`, `src/query/distinct.rs`, `src/query/aggregate.rs`: `--bind <addr>`; the self-signed certificate dated 365 days from its generation; a read past `--query-timeout` stopped at its next checkpoint (`8f6d5fe`, 2026-10-29; #32).
- `src/query/aggregate.rs`, `src/engine/backend/mod.rs`, `src/engine/backend/fjall_backend.rs`, `src/engine/backend/rocksdb_backend.rs`, `src/engine/bitmap.rs`, `Cargo.toml`: a `$group`-first aggregate streams through its accumulators; one terminal-stage helper; one RocksDB table-and-compression helper; `$ne` subtracts without the clone; the engines are the cargo features `fjall` and `rocksdb`, default both, the engine still chosen at run time with `--storage-engine` (`a86a472`, 2026-10-29; #42).
- `Cargo.toml`, `API.md`, this file: the `license` field, the `--bind` row and this record (2026-10-29; #27).

## Syncing

A sync adopts whole upstream commits: `git -C ../WardSONDB show <sha> -- <files> | git apply --directory=crates/wardsondb`. Hunks of upstream's `Cargo.lock` are never applied: the workspace lock pins fjall 2.11.2, lsm-tree 2.10.4, value-log 1.9.0, rust-rocksdb 0.46.0, rust-librocksdb-sys 0.42.0+10.10.1, roaring 0.10.12 and tikv-jemallocator 0.6.1, the versions upstream tested, and the on-disk format of existing data depends on them. Equal blob hashes against the upstream tree prove the state of every file not listed above.
