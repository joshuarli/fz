# Measured comparison and acceptance

The accepted comparison was measured at 2026-09-05T02:46:30Z on Darwin 27.0.0,
ARM64, with eight logical CPUs, the pinned Rust toolchain, and the normal
release profile. Upstream fzy is the unmodified copied build of commit
`34b88869d022e861da4846c4463aea3ddfb3ff30`. Each process case has one warmup
and five measured repetitions. Complete filter output is checked against fzy
before timing; interactive measurements wait for the query's count and ranked
rows, then verify terminal restoration after cancellation.

`python3 benches/perf.py --check-ratio 1.1 --json
target/perf/accepted-comparison.json` passed all 158 recorded fz/fzy median
ratios: non-interactive wall time, child CPU, and peak RSS; interactive redraw
latency; and interactive current and peak RSS. The highest ratio was
1.08490566 for small-input dense filtering with `-j1` peak RSS. Ratios below 1
favor fz.

The report at `target/perf/accepted-comparison.json` retains raw samples,
fixture digests, host details, and measured binary hashes:

- fz: `d9b4919672397290ff12858d60dba2705f45dc6f6119bef7da87ca75eb0de2a2`
- fzy: `5c35ee65f3e3fa9b04dce30be3854686a5713fdbe7b472119e262ef020cd97ef`

`benches/budget-macos-aarch64.json` is the accepted same-host Rustybench
budget. It derives time ceilings as `ceil(observed_median_ns * 1.10)` from
`target/perf/rustybench-final-observed.json` and holds allocation ceilings at
the observed values. Check a same-host candidate with:

```sh
cargo run --manifest-path ~/d/rustybench/Cargo.toml --release -- \
  check benches/budget-macos-aarch64.json target/perf/rustybench-accepted-candidate.json
```

Rustybench's allocation profiler is thread-local, so its default-parallel
`Choices` rows omit allocations made by fz's scoped workers. The process gate's
current and peak RSS measurements cover the complete release process.
