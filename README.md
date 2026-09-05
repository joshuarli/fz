# fz

A small Rust port of John Hawthorn's **fzy**. Candidates arrive
on stdin, the interface uses `/dev/tty`, and Enter writes one selection to stdout.
With no matches, Enter writes the query instead. Escape or Ctrl-C/D/G cancels
with status 1 and no stdout. Successful selection and noninteractive filtering
return status 0, including filters with no matches.

```sh
cargo build --release
find . -type f | target/release/fz
printf 'src/main.rs\nsrc/lib.rs\n' | target/release/fz -e mr -s
```

The checked-in `rust-toolchain.toml` selects the compiler. The normal release
profile matches `~/d/e`: `opt-level = 3`, fat LTO, one codegen unit, aborting
panics, and stripped symbols. It uses the prebuilt standard library.

Terminal access uses `rustix` with terminal, event, and runtime support. The
benchmark-only dependency is `rustybench`, including its allocation profiler.

Options retain fzy's `-l`/`--lines`, `-p`/`--prompt`, `-q`/`--query`,
`-e`/`--show-matches`, `-t`/`--tty`, `-s`/`--show-scores`, `-0`/`--read-null`,
`-i`/`--show-info`, help, and version. `-j`/`--workers` sets a worker ceiling;
`-j1` keeps searches on the calling thread. Large searches use up to four workers
by default, and small searches stay on the caller to avoid thread startup cost. Benchmark mode, configuration,
shell integration, previews, and multiple selection are outside this port.

Up/down or Ctrl-P/N (also Ctrl-K/J) select, wrapping at the ends. Left/right,
Home/End, and Ctrl-A/E move within the query. Backspace deletes a code point,
Ctrl-W deletes the preceding word, and Ctrl-U deletes everything before the
cursor. Tab completes the selected candidate; PageUp/PageDown move by a page.

Matching follows the upstream byte-oriented algorithm, including its asymmetric
ASCII case handling: lowercase query letters accept either case; uppercase query
letters require uppercase candidates. UTF-8 query editing moves by code point;
matching does not perform Unicode normalization or case folding. Empty candidate
records are skipped, newline input preserves carriage returns, and NUL-delimited
input can contain newlines. Output always ends records with a newline.

Scores reward consecutive letters, word starts, slashes, CamelCase, and dots, and
penalize gaps. Equal scores retain input order. Empty queries have score `-inf`,
exact matches `inf`, and candidates longer than 1024 bytes remain eligible but
have score `-inf` and no highlighted positions. Queries are bounded to 4096 bytes
in the interactive editor.

## Verification

```sh
cargo test
cargo tree
wc -c target/release/fz
```

The Rust tests port upstream matching, choices, and property tests. The acceptance
suite uses Python 3's standard library for PTYs and a small ANSI screen reader;
Python is a test tool only. Differential checks compare stdout, scores, ordering,
exit status, and interactive selections against a separately built fzy binary.

To build the oracle without writing to the upstream checkout:

```sh
mkdir -p target/upstream
cp -R ~/d/fzy/src ~/d/fzy/test ~/d/fzy/deps ~/d/fzy/Makefile target/upstream/
make -C target/upstream fzy test
FZY_ORACLE="$PWD/target/upstream/fzy" cargo test
```

The behavioral reference is upstream commit
`34b88869d022e861da4846c4463aea3ddfb3ff30`. Its algorithm and ported tests are
MIT licensed, copyright John Hawthorn; the notice is retained in `LICENSE`.

Runtime compatibility is verified on macOS ARM64 with the pinned toolchain.
Linux runtime behavior has not been verified here. The upstream checkout was
left untouched; its copied C suite passed all 32 tests.

The initial core, CLI, and acceptance suites were confirmed failing before
production implementation. Three later terminal regressions (page endpoints,
scroll margin, and info-row cleanup) were added after review fixes; their
pre-fix behavior was subsequently checked in an isolated copy. The ARM scoring
rounding regression was reproduced in a failing Rust test before its fix.
