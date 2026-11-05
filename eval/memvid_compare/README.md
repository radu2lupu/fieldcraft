# Memvid vs Fieldcraft retrieval harness

Plan: `docs/plans/memvid-comparison.md`. Results: `RESULTS.md`. Standalone cargo project (not a
workspace member). Snapshot, caches and outputs live on the Dev SSD and are never committed.

```sh
export CARGO_TARGET_DIR="/Volumes/Development SSD/fieldcraft-memvid-eval/target"
cd eval/memvid_compare
cargo test --release && cargo clippy --release --all-targets
. ~/.fieldcraft/secrets.env          # Gemini key; only `embed`/`eval` on a cache miss need it
B="$CARGO_TARGET_DIR/release/memvid_compare"
O="/Volumes/Development SSD/fieldcraft-memvid-eval/out"
cargo build --release
$B labels                            # recovered label set vs the Jev README
$B embed                             # query vectors -> query_vectors.jsonl (cached)
/usr/bin/time -l $B build --lift-cap # batch-ingest all lessons into lessons.mv2 (~15 min)
$B sqlite-build                      # same data in a minimal SQLite+FTS5 file (size comparison)
/usr/bin/time -l $B eval --out "$O"  # all 7 arms, bootstrap CIs (~5 min after open)
/usr/bin/time -l $B qbench --engine mv --n 300   # cold open, p50/p95, RSS
/usr/bin/time -l $B qbench --engine fc --n 300
/usr/bin/time -l $B load-only        # RSS baseline of the corpus copy
```

`MEMVID_EVAL_DIR` overrides the data directory. `eval --skip-mv` runs the Fieldcraft arms only.
Per-query ranks go to `$O/per_query_ranks.jsonl`, tables to `$O/summary.md`.

## Navigation arm

Plan `docs/plans/navigation-arm.md`, results `eval/nav_compare_RESULTS.md`. Needs the Keychain item
`fieldcraft.jev-api-key`; outputs and the request cache go to `/Volumes/Development SSD/fieldcraft-nav-eval`.

```sh
export CARGO_TARGET_DIR="/Volumes/Development SSD/fieldcraft-nav-eval/target"
cargo build --release
$CARGO_TARGET_DIR/release/nav_eval run --limit 50          # pilot: evenly spaced sample, prints cost extrapolation
$CARGO_TARGET_DIR/release/nav_eval run --set jev --max-usd 14.5   # 865 queries (about $13); reruns are free (cached)
```
