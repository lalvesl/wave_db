# WaveDB performance — latest run

- Commit: `6b4650b`
- When: 2026-06-05T02:57:57Z (unix 1780628277)
- Arch: `x86_64`
- Build: release (optimized)

| scenario          | records | payload (B) | elapsed (s) | records/s | MiB/s | disk B/rec |
| ----------------- | ------: | ----------: | ----------: | --------: | ----: | ---------: |
| in_memory_write   |   50000 |         128 |      1.3852 |     36096 |   4.4 |        0.0 |
| in_memory_write   |  200000 |         128 |     45.3383 |      4411 |   0.5 |        0.0 |
| in_memory_read    |  200000 |         128 |      0.0372 |   5375137 | 656.1 |        0.0 |
| durable_write_wal |    5000 |         128 |      8.4600 |       591 |   0.1 |      150.0 |
| journal_recovery  |    5000 |         128 |      0.1195 |     41839 |   5.1 |      150.0 |

The full time series is in [`history.jsonl`](history.jsonl). Regenerate with `cargo run -p wavedb-bench --release --bin record-perf`.
