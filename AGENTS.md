`fz` is a minimal Rust port of John Hawthorn's `fzy` for macOS and Linux: stdin candidates → interactive fuzzy filtering on `/dev/tty` → one selection on stdout. Upstream is the behavioral oracle (checkout at `~/d/fzy`, read-only).

## Scope

- Preserve fzy's observable behavior: scoring, ranking, key bindings, exit status, stdin/stdout contract. Do not add fzf-style features, configuration, shell integration, previews, or multi-select.
- Simplify the implementation freely; never change observable semantics. Do not transliterate the C.
- Dependencies stay minimal and low-level: `rustix` for terminal/event access, `libc` on Linux for the signal mask. Ask before adding any other, including dev-dependencies.
- Keep the upstream MIT notice in `LICENSE`.

## Layout

- `src/score.rs` — matching, scoring, highlight positions.
- `src/lib.rs` — `Choices`: candidate storage, ranking, and the scoped worker search (`-j`, capped at four by default).
- `src/terminal.rs` — raw mode, key decoding, rendering, resize handling.
- `src/main.rs` — option parsing and startup.
- `tests/` — Rust ports of the upstream suites plus CLI/candidate tests; `acceptance.py` drives PTYs and differentially checks against upstream fzy.
- `benches/` — Rustybench benchmarks, release-process comparisons, and the enforced performance budget.

## Verification

- `cargo test` for the core; set `FZY_ORACLE` to a built upstream `fzy` to run the differential tests (otherwise they skip). Build steps are in `README.md`.
- Bug fixes start with a failing test that reproduces the bug.
- Score ordering depends on float contraction: macOS Clang uses FMA, Linux GCC does not. Do not "fix" one-ULP ordering differences without checking both.
- Release builds are for final verification and performance work. Check `cargo tree` and the stripped binary size when dependencies or the release profile change.
