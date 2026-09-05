//! In-process performance and allocation benchmarks for `fz`.
//!
//! This benchmark suite intentionally measures parser and matcher work after
//! fixture construction. `benches/perf.py` measures the separate CLI-process
//! and terminal costs that cannot be observed through an in-process allocator.

use fz::{has_match, match_positions, match_score, Candidates, Choices};
use rustybench::{AllocProfiler, Bencher, black_box};

#[global_allocator]
static ALLOC: AllocProfiler = AllocProfiler::system();

const SMALL_RECORDS: usize = 128;
const LARGE_RECORDS: usize = 100_000;
const NO_MATCH: &[u8] = b"QZXQZXQZX";
const PATH_EXACT: &[u8] = b"src/fz/benchmark/perf_case.py";
const PATH_LONGER: &[u8] = b"src/fz/benchmark/performance_snapshot.py";
const INTERACTIVE_QUERY_SEQUENCE: &[&[u8]] = &[
    b"src",
    b"src/f",
    b"src/fz/benc",
    // The user inserts `h` before `c` to correct `benc` to `bench`. This is
    // not a prefix refinement, so Choices must rescore the full candidate set.
    b"src/fz/bench",
    b"src/fz/benchmark",
];

fn mixed_word(index: usize, lane: u64) -> u64 {
    // Counter-based SplitMix64 output makes each path selection independent of
    // its neighbors; in particular, this does not use the repeating low bits
    // of an LCG to select path components.
    let mut value = (index as u64)
        .wrapping_add(0x9e37_79b9_7f4a_7c15)
        .wrapping_add(lane.wrapping_mul(0xbf58_476d_1ce4_e5b9));
    value = (value ^ (value >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    value = (value ^ (value >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    value ^ (value >> 31)
}

fn choose_component<'a>(values: &'a [&'a str], index: usize, lane: u64) -> &'a str {
    values[mixed_word(index, lane) as usize % values.len()]
}

fn append_path(input: &mut Vec<u8>, index: usize) {
    const ROOTS: &[&str] = &["src", "packages", "services", "tools", "tests", "docs", "vendor"];
    const AREAS: &[&str] = &["auth", "catalog", "editor", "index", "network", "storage", "terminal"];
    const STEMS: &[&str] = &["command", "config", "handler", "model", "parser", "report", "snapshot"];
    const EXTENSIONS: &[&str] = &["rs", "toml", "md", "json", "py"];
    input.extend_from_slice(choose_component(ROOTS, index, 0).as_bytes());
    input.push(b'/');
    input.extend_from_slice(choose_component(AREAS, index, 1).as_bytes());
    input.push(b'/');
    input.extend_from_slice(choose_component(AREAS, index, 2).as_bytes());
    input.push(b'_');
    input.extend_from_slice(format!("{:06x}", mixed_word(index, 3) & 0xff_ffff).as_bytes());
    input.push(b'/');
    input.extend_from_slice(choose_component(STEMS, index, 4).as_bytes());
    input.push(b'_');
    input.extend_from_slice(format!("{:04x}", mixed_word(index, 5) & 0xffff).as_bytes());
    input.push(b'.');
    input.extend_from_slice(choose_component(EXTENSIONS, index, 6).as_bytes());
    input.push(b'\n');
}

fn path_input(records: usize) -> Vec<u8> {
    let mut input = Vec::with_capacity(records * 48);
    if records != 0 {
        input.extend_from_slice(PATH_EXACT);
        input.push(b'\n');
    }
    if records > 1 {
        input.extend_from_slice(PATH_LONGER);
        input.push(b'\n');
    }
    for index in 2..records {
        append_path(&mut input, index);
    }
    input
}

fn dense_path_input(records: usize) -> Vec<u8> {
    let mut input = Vec::with_capacity(records * 48);
    for index in 0..records {
        // Every record matches `src/fz/bench`, exercising dense result storage,
        // sorting, and the caller's merge of its workers' result vectors.
        input.extend_from_slice(b"src/fz/benchmark/dense_case_");
        input.extend_from_slice(format!("{index:06x}").as_bytes());
        input.extend_from_slice(b".rs\n");
    }
    input
}

fn numeric_input(records: usize) -> Vec<u8> {
    let mut input = Vec::with_capacity(records * 6);
    for index in 0..records {
        // 7,919 is coprime with 100,000, so this is a deterministic permutation.
        input.extend_from_slice(format!("{:05}\n", (index * 7_919 + 31_415) % 100_000).as_bytes());
    }
    input
}

fn long_candidate(length: usize, id: usize) -> Vec<u8> {
    let mut candidate = format!("long-candidate-{id:03}/needle-{id:03}/").into_bytes();
    const FILL: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789/_-.";
    while candidate.len() < length {
        let remaining = length - candidate.len();
        candidate.extend_from_slice(&FILL[..remaining.min(FILL.len())]);
    }
    candidate
}

fn long_input(records: usize) -> Vec<u8> {
    const LENGTHS: &[usize] = &[1000, 1023, 1024, 1025, 1152, 1536];
    let mut input = Vec::with_capacity(records * 1_128);
    for index in 0..records {
        input.extend_from_slice(&long_candidate(LENGTHS[index % LENGTHS.len()], index));
        input.push(b'\n');
    }
    input
}

fn small_input() -> Vec<u8> {
    let mut input = path_input(SMALL_RECORDS);
    input.extend_from_slice(b"README.md\napp/models/order.py\nsrc/fz/benchmark/perf_case.py\n");
    input
}

fn candidates(input: Vec<u8>) -> Candidates {
    Candidates::from_input(input, b'\n')
}

#[rustybench::bench]
fn has_match_selective_path(bencher: Bencher) {
    let candidate = PATH_LONGER.to_vec();
    bencher.bench_local(|| black_box(has_match(black_box(b"src/fz/bench"), black_box(&candidate))));
}

#[rustybench::bench]
fn has_match_long_1024(bencher: Bencher) {
    let candidate = long_candidate(1024, 42);
    bencher.bench_local(|| black_box(has_match(black_box(b"needle-042/abcdefghijkl"), black_box(&candidate))));
}

#[rustybench::bench]
fn score_selective_path(bencher: Bencher) {
    let candidate = PATH_LONGER.to_vec();
    bencher.bench_local(|| black_box(match_score(black_box(b"src/fz/bench"), black_box(&candidate))));
}

#[rustybench::bench]
fn score_long_1024(bencher: Bencher) {
    let candidate = long_candidate(1024, 42);
    bencher.bench_local(|| black_box(match_score(black_box(b"needle-042/abcdefghijklmnop"), black_box(&candidate))));
}

#[rustybench::bench]
fn score_over_1024_bypass(bencher: Bencher) {
    let candidate = long_candidate(1025, 42);
    bencher.bench_local(|| black_box(match_score(black_box(b"needle"), black_box(&candidate))));
}

#[rustybench::bench]
fn positions_selective_path(bencher: Bencher) {
    let candidate = PATH_LONGER.to_vec();
    bencher.bench_local(|| black_box(match_positions(black_box(b"src/fz/bench"), black_box(&candidate))));
}

#[rustybench::bench]
fn positions_long_1024(bencher: Bencher) {
    let candidate = long_candidate(1024, 42);
    bencher.bench_local(|| black_box(match_positions(black_box(b"needle-042/abcdefghijklmnop"), black_box(&candidate))));
}

#[rustybench::bench(sample_count = 20, sample_size = 1)]
fn candidates_parse_small(bencher: Bencher) {
    let input = small_input();
    bencher
        .with_inputs(move || input.clone())
        .bench_local_values(|input| black_box(candidates(input)));
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn candidates_parse_numeric_100k(bencher: Bencher) {
    let input = numeric_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || input.clone())
        .bench_local_values(|input| black_box(candidates(input)));
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn candidates_parse_paths_100k(bencher: Bencher) {
    let input = path_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || input.clone())
        .bench_local_values(|input| black_box(candidates(input)));
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn candidates_parse_long_boundary(bencher: Bencher) {
    let input = long_input(SMALL_RECORDS);
    bencher
        .with_inputs(move || input.clone())
        .bench_local_values(|input| black_box(candidates(input)));
}

fn choices_from(input: &[u8]) -> Choices {
    Choices::from_candidates(candidates(input.to_vec()))
}

fn default_parallel_choices_from(input: &[u8]) -> Choices {
    let mut choices = choices_from(input);
    // The CLI's default Options value is zero, which permits up to four
    // workers; Choices itself intentionally defaults to one worker.
    choices.set_workers(0);
    choices
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn choices_search_numeric_100k_dense(bencher: Bencher) {
    let input = numeric_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || choices_from(&input))
        .bench_local_values(|mut choices| {
            choices.search(b"0");
            black_box((choices.available(), choices.get(0).map_or(0, <[u8]>::len)))
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn choices_search_numeric_100k_selective(bencher: Bencher) {
    let input = numeric_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || choices_from(&input))
        .bench_local_values(|mut choices| {
            choices.search(b"987");
            black_box((choices.available(), choices.get(0).map_or(0, <[u8]>::len)))
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn choices_search_paths_100k_dense(bencher: Bencher) {
    let input = path_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || choices_from(&input))
        .bench_local_values(|mut choices| {
            choices.search(b"src");
            black_box((choices.available(), choices.get(0).map_or(0, <[u8]>::len)))
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn choices_search_dense_paths_100k_default_parallel(bencher: Bencher) {
    let input = dense_path_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || default_parallel_choices_from(&input))
        .bench_local_values(|mut choices| {
            choices.search(b"src/fz/bench");
            black_box((choices.available(), choices.get(0).map_or(0, <[u8]>::len)))
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn choices_search_paths_100k_selective(bencher: Bencher) {
    let input = path_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || choices_from(&input))
        .bench_local_values(|mut choices| {
            choices.search(b"src/fz/bench");
            black_box((choices.available(), choices.get(0).map_or(0, <[u8]>::len)))
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn choices_search_paths_100k_no_match(bencher: Bencher) {
    let input = path_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || choices_from(&input))
        .bench_local_values(|mut choices| {
            choices.search(NO_MATCH);
            black_box(choices.available())
        });
}

#[rustybench::bench(sample_count = 10, sample_size = 1)]
fn choices_interactive_paths_100k_refinement_and_edit_default_parallel(bencher: Bencher) {
    let input = path_input(LARGE_RECORDS);
    bencher
        .with_inputs(move || default_parallel_choices_from(&input))
        .bench_local_values(|mut choices| {
            for &query in INTERACTIVE_QUERY_SEQUENCE {
                choices.search(black_box(query));
            }
            black_box((
                choices.available(),
                choices.selected().map_or(0, <[u8]>::len),
            ))
        });
}

#[rustybench::bench(sample_count = 20, sample_size = 1)]
fn choices_search_long_boundary(bencher: Bencher) {
    let input = long_input(SMALL_RECORDS);
    bencher
        .with_inputs(move || choices_from(&input))
        .bench_local_values(|mut choices| {
            choices.search(b"needle-042/abcdefghijklmnop");
            black_box((choices.available(), choices.get(0).map_or(0, <[u8]>::len)))
        });
}

fn main() {
    rustybench::main();
}
