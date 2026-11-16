# The vendored fork

`crates/wardsondb` is a vendored copy of [WardSONDB](https://github.com/Ward-Software-Defined-Systems/WardSONDB), the document database embraOS ships. The fork was taken on 2026-07-17 and synced to upstream `main` at `da13eec` on 2026-11-03, the base it is measured against. embraOS reaches it over REST on port 8090 and no embraOS crate depends on its library.

The crate keeps upstream's Apache-2.0 licence: `LICENSE` beside this file and `license = "Apache-2.0"` in `Cargo.toml`. The embraOS workspace's own `LICENSE` governs the rest of embraOS, not this crate.

## Divergences from `da13eec`

Two, in three commits, none carried upstream yet.

1. `src/engine/storage.rs` (2026-11-05, commit `d50bcac` on `phase2`): the engine marker is written whole or not at all — the name goes to `.engine.tmp`, fsynced, renamed over `.engine`, the directory fsynced — and an empty marker reads as absent and is written again. The plain `fs::write` left `.engine` created and empty when the machine stopped within the page cache's writeback window of the first start (embraOS's first headless boot test did exactly that), and the next start refused the directory with "created with engine ''". Guard `the_engine_marker_is_written_whole_and_an_empty_one_is_rewritten`.
2. `src/server/routes/query.rs` (2026-11-16, commits `7f07563` and `73c3bf1` on `phase2`), test code only, both in `a_read_past_its_timeout_is_stopped_at_its_next_checkpoint`. The first: the test's stand-in read gives its thread up once per look at its token, where it did so after every entry. A scan looks at the token every `CHECK_EVERY` entries, so a thousand yields lay between two looks. They take a millisecond on an idle machine and about nine seconds under sixty-four busy loops on sixteen cores: the read's first look then came about eight seconds after its one-second timeout, and the test's five-second wait for the read to stop had ended. The test failed on a loaded machine with nothing wrong in the code under it. The second: the test tells that read to end before it asserts anything, and gives the answer five seconds. Dropping the test's runtime waits for a read that is still running, so with the timeout's cancel taken out the last assertion fired and the run never ended, and with the timeout itself taken out the test waited without end; both now fail, after six seconds and after five.

Every other file has upstream's blob hash, except this one and the three upstream files the vendoring leaves out: `Cargo.lock` (the workspace lock governs), `CONTRIBUTING.md` and `.gitlab-ci.yml` (upstream's repository files). Each commit is carried upstream from the WardSONDB checkout with `git -C ../embraOS show <commit> -- crates/wardsondb/src | git apply -p3`, in the order `d50bcac`, `7f07563`, `73c3bf1`. The hunks are `cargo fmt` clean, and with the three applied to `da13eec` every file the two trees share has the same blob hash (replayed on a copy of upstream, 2026-11-16).

A divergence is a decision of the embraOS maintainer, recorded in its commit and carried upstream through an issue on the WardSONDB repository. The previous base was `1732213` (synced 2026-07-24). The nine commits that diverged from it between 2026-09-27 and 2026-10-29 were ported upstream 1:1 as [WardSONDB issue #1](https://github.com/Ward-Software-Defined-Systems/WardSONDB/issues/1), each upstream commit carrying a `Ported-From: embraOS <sha>` trailer, and the sync of 2026-11-03 brought upstream's follow-ups back: `cargo fmt` over the ported hunks, 65,536 write shards in place of 256, the bulk-insert duplicate check through one multi-get, timeout checkpoints in the index scans and the `$or`-union arms, two guard benches, and the changelog, README and `API.md` text for all of it. Upstream's CI gate runs `cargo fmt --check`; a hunk that is not formatted does not port 1:1.

| embraOS | upstream | subject |
|---|---|---|
| `fe0d63a` | `6b4441f` | axum-server 0.7 → 0.8 |
| `76509a3` | `4497a70` | a dropped collection takes its TTL policy with it |
| `8c30f2b` | `b0c9651` | the integration tests keep their bound port |
| `793c696` | `4b6ffbf` | a document's writers hold its shard from the read to the commit |
| `a99ae7d` | `651f6ee` | the observability endpoints report a failed meta read |
| `6f4058d` | `1df1f89` | a path segment with a separator or a control byte is refused |
| `2ab4fde` | `52bc0e0` | `--bind`, a certificate dated from today, reads stopped at a timeout |
| `b6272c1` | `784c008` | `$group`-first pipelines stream; the engines become cargo features |
| `60fe4fe` | `a48cd32` | `license = "Apache-2.0"` in the manifest (this file was not ported) |

## Syncing

A sync adopts whole upstream commits: `git -C ../WardSONDB show <sha> -- <files> | git apply --directory=crates/wardsondb`. Hunks of upstream's `Cargo.lock` are never applied: the workspace lock pins fjall 2.11.2, lsm-tree 2.10.4, value-log 1.9.0, rust-rocksdb 0.46.0, rust-librocksdb-sys 0.42.0+10.10.1, roaring 0.10.12 and tikv-jemallocator 0.6.1, the versions upstream tested, and the on-disk format of existing data depends on them. `CONTRIBUTING.md` and `.gitlab-ci.yml` are not vendored. Equal blob hashes against the upstream tree prove the state of every other file.
