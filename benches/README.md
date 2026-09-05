# Benchmarks

`bench.rs` measures the in-process contract with Rustybench. It reports wall
time and `AllocProfiler` allocation totals and maxima for `has_match`, scoring,
highlight positions, packed candidate parsing, and `Choices` searches. The
fixtures cover small input, 100,000 short numeric records, 100,000 varied
source-tree paths, and candidates immediately below and above fzy's 1024-byte
scoring boundary.

```sh
cargo bench --bench bench -- --format json
```

Use Rustybench's baseline runner when allocating an accepted time and heap
budget. Its comparison includes `alloc_bytes`, `max_alloc_bytes`, and
allocations per operation, so a checked-in host-specific baseline makes an
allocation regression visible with the timing regression.

```sh
cargo run --manifest-path ~/d/rustybench/Cargo.toml --release -- \
  baseline --root "$PWD" --baseline benches/local-baseline.json -- \
  cargo bench --bench bench
```

`perf.py` is intentionally separate. An in-process allocator cannot measure a
spawned release CLI, upstream fzy's worker pool, child peak RSS, or terminal
redraw latency. It prepares deterministic inputs before process timing,
compares complete output byte-for-byte before accepting timings, then reports
medians of wall time, child user-plus-system CPU time, and `wait4` peak RSS.
On macOS `ru_maxrss` is bytes; on Linux it is normalized from KiB to bytes.

```sh
python3 benches/perf.py --smoke
python3 benches/perf.py --quick
python3 benches/perf.py
python3 -m unittest benches/test_perf.py
```

The normal invocation uses one unmeasured warmup and five repetitions;
`--quick` uses three repetitions while retaining every workload. Override the
warmup count with `--warmup N`. `--smoke` uses only a tiny fixture, no default
warmup, and a separate `smoke/` fixture directory, so it is safe immediately
before a normal run. Both the default upstream worker setting and `-j1` are
kept in distinct output rows. The interactive workload uses a PTY, prepares
fzy-scored expected counts and top-three rows before timing, and only completes
each sample after that query's prompt, info row, and ranked rows are visible.
It samples resident memory after the final query and requires Ctrl-C
cancellation to restore the PTY termios state. Earlier prompt-only interactive
JSON/baselines are invalid and should be discarded. Use `--json
target/benchmarks/perf.json` to retain raw process samples with the median
report. `test_perf.py` includes a controlled fzy-style stale prompt/old-results
transition, so a prompt-only completion cannot return unnoticed.
