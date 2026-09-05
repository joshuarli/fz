Port `fzy` to an **extremely minimal Rust binary** - `fz`. The upstream source is available read-only at `~/d/fzy`; treat it as the behavioral oracle.

Respect `rust-toolchain.toml`.

Goals:

* Preserve the essence and behavior of `fzy`, not `fzf` feature creep.
* Prefer **zero dependencies**. If unavoidable, allow only exceptionally lean, low-level dependencies such as `libc`; justify every dependency. No clap, crossterm, rayon, regex, serde, async, etc.
* Small, direct, idiomatic Rust. Avoid frameworks, abstraction layers, unnecessary modules, allocation, and concurrency.
* Preserve upstream licensing/attribution where required.

Work **strictly test-first**:

1. Study `~/d/fzy`, especially its tests, scoring algorithm, terminal behavior, stdin/stdout contract, and exit semantics.
2. Port the upstream test suite to Rust **before implementing production code**. Confirm tests fail for the expected missing behavior.
3. Implement incrementally via red → green → refactor until the Rust implementation reaches test parity with upstream.
4. Add differential tests that run equivalent cases against `~/d/fzy/build/fzy` (build upstream if necessary) and the Rust binary.
5. Cover interactive behavior with PTY/integration tests where practical.

Keep the scope ruthless: stdin candidates → interactive fuzzy filtering → single selection on stdout; navigation, editing, cancel/exit behavior, and fzy-compatible scoring/ranking. Do not add configuration, shell integration, previewing, multi-select, or unrelated features.

Optimize release builds for size and inspect the final dependency tree and binary size. The finished project should feel like **fzy rewritten by someone trying to delete everything that is not essential**.

Do not merely transliterate the C. Understand its invariants, reproduce its behavior, and simplify where Rust makes the implementation cleaner without changing observable semantics.
