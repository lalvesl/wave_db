# wavedb-bench results

Version-controlled performance trail for the WaveDB storage engine. The point of
committing these files is to make performance regressions visible in code review
and `git log`, the same way criterion tracks deltas locally — except here the
numbers live in the repo, not in `target/`.

## Files

- **`history.jsonl`** — append-only time series. One JSON object per recorder
  run (timestamp, git commit, arch, and every measured sample). Never rewritten,
  so the history is auditable. Diff it across commits to see how a change moved
  the numbers.
- **`latest.md`** — regenerated each run: a readable snapshot of the most recent
  numbers and the commit they were measured at.

## Recording a baseline

```sh
cargo run -p wavedb-bench --release --bin record-perf
git add crates/wavedb-bench/results
git commit -m "perf: record baseline @ <topic>"
```

Always record with `--release`; a debug build is flagged in the output and the
numbers are not representative.

## Scenarios

| scenario            | what it measures                                                                                            |
| ------------------- | ----------------------------------------------------------------------------------------------------------- |
| `in_memory_write`   | hash-mapped page-table insert rate (routing + packing + rebalances), no fsync — the engine's hot write loop |
| `in_memory_read`    | point-lookup rate over a prefilled table                                                                    |
| `durable_write_wal` | full WAL commit rate (journal append + **fsync** + apply) — the real durable write rate                     |
| `journal_recovery`  | time to re-open a node and replay the journal back into the data file after a simulated crash               |

`durable_write_wal` is fsync-bound and therefore the most hardware- and
filesystem-sensitive number; compare runs taken on the same machine.

## Statistical micro-benchmarks

For rigorous, noise-controlled per-operation timings (with confidence intervals),
run the criterion harness — it shares the exact same workloads:

```sh
cargo bench -p wavedb-bench
```

Criterion writes its reports to `target/criterion/` (git-ignored).
