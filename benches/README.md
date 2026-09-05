# Benchmarks

See [RESULTS.md](RESULTS.md) for the measured fzy comparison and accepted macOS budget.

`bench.rs` measures the in-process contract with Rustybench. It reports wall
time and `AllocProfiler` allocation totals and maxima for `has_match`, scoring,
highlight positions, packed candidate parsing, and `Choices` searches. The
fixtures cover small input, 100,000 short numeric records, 100,000 varied
source-tree paths, a dense 100,000-path default-parallel search, an interactive
refinement and middle-edit sequence, and candidates immediately below and above
fzy's 1024-byte scoring boundary.

```sh
cargo bench --bench bench -- --format json
```

The default-parallel benchmarks call `Choices::set_workers(0)`, matching the
CLI default that uses available CPUs capped at four. `Choices` otherwise starts
with one worker. The interactive sequence uses the same default-parallel
configuration: it refines `src` into `src/fz/benc`, inserts `h` before `c` to
make `src/fz/bench`, then refines once more. The middle insertion cannot reuse
the previous candidate set, so it exercises the full-search path that editor
typing takes after a cursor edit.

`AllocProfiler` reports allocations from the Rustybench-controlled benchmark
thread. Rustybench deliberately keeps its tally thread-local and does not
initialize a tally for threads spawned inside a benchmark. As a result, the
default-parallel rows include their worker work in wall time, but their
`alloc_count`, `alloc_bytes`, `max_alloc_count`, and `max_alloc_bytes` omit the
allocations made by `Choices`' scoped workers. Treat those four fields as
caller-thread diagnostics for parallel rows, not process allocation or peak RSS
budgets. The serial rows are fully visible to the profiler; use `perf.py` for
release-process CPU and RSS measurements.

A future Rustybench all-thread allocation mode would need to be an explicit
opt-in: a benchmark scope would register each spawned worker and aggregate its
tally after the workers join. It would also need to define whether a maximum is
the sum of simultaneous live allocations across workers. Rustybench has no such
API today, and this suite does not treat its parallel allocation fields as a
memory budget until it does. `perf.py` RSS remains necessary for the release
process even if that API is added.

## Baseline and strict-budget workflow

Rustybench's `baseline` command writes a schema-1 candidate report. Capture a
candidate on the same host, release profile, fixture inventory, allocator, and
worker configuration as the budget it will be compared with.

```sh
cargo run --manifest-path ~/d/rustybench/Cargo.toml --release -- \
  baseline --root "$PWD" --baseline target/perf/rustybench-accepted-candidate.json -- \
  cargo bench --bench bench
```

The accepted `benches/budget-macos-aarch64.json` applies to its recorded
eight-CPU macOS/aarch64 host. Its time ceilings are
`ceil(observed_median_ns * 1.10)` and its allocation ceilings equal the
observed values from `target/perf/rustybench-final-observed.json`. The stored
`observed_median_ns` values and `budget_basis` metadata make that derivation
auditable.

```sh
cargo run --manifest-path ~/d/rustybench/Cargo.toml --release -- \
  diff benches/budget-macos-aarch64.json target/perf/rustybench-accepted-candidate.json
```

Check a new candidate from the same host, release profile, fixture inventory,
allocator, and worker configuration with:

```sh
cargo run --manifest-path ~/d/rustybench/Cargo.toml --release -- \
  check benches/budget-macos-aarch64.json target/perf/rustybench-accepted-candidate.json
```

`check` accepts only schema-1 reports with exactly the same unique benchmark
names. It treats each budget value as an absolute ceiling and fails a candidate
whose `median_ns`, `alloc_count`, `alloc_bytes`, `max_alloc_count`, or
`max_alloc_bytes` exceeds it. It does not apply percentage tolerances or permit
missing or additional benchmark rows. The parallel `Choices` allocation values
remain caller-thread-only because Rustybench's thread-local profiler does not
see fz's workers. The process RSS gate below covers the complete release
process for those rows.

`perf.py` is intentionally separate. An in-process allocator cannot measure a
spawned release CLI, upstream fzy's worker pool, child peak RSS, or terminal
redraw latency. It prepares deterministic inputs before process timing,
compares complete output byte-for-byte before accepting timings, then reports
medians of wall time, child user-plus-system CPU time, and `wait4` peak RSS.
On macOS `ru_maxrss` is bytes; on Linux it is normalized from KiB to bytes.

```sh
python3 benches/perf.py --smoke
python3 benches/perf.py --quick
python3 benches/perf.py --check-ratio 1.1 --json target/perf/accepted-comparison.json
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
report, host details, fixture digests, and hashes of the measured binaries.
`test_perf.py` includes a controlled fzy-style stale prompt/old-results
transition, so a prompt-only completion cannot return unnoticed.

`--check-ratio LIMIT` is opt-in and leaves the default reporting behavior
unchanged. `LIMIT` must be finite and positive. It fails when an fz/fzy median
exceeds the limit for non-interactive wall time, child CPU time, or peak RSS;
or for interactive redraw latency, current RSS when both sides are available,
or process-lifetime peak RSS. The accepted macOS report passed at `1.1`.
