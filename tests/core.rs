// Derived from the following MIT-licensed upstream fzy test files:
//   - test/test_match.c
//   - test/test_choices.c
//   - test/test_properties.c
//
// Copyright (c) 2014 John Hawthorn
// SPDX-License-Identifier: MIT
//
// The pure Rust API replaces upstream's mutable C output buffers with owned
// positions, and exposes its choices state directly so the original observable
// search, ordering, and navigation behavior remains testable.

use fz::{has_match, match_positions, match_score, rank, Choices};

const SCORE_TOLERANCE: f64 = 0.000_001;
const SCORE_GAP_LEADING: f64 = -0.005;
const SCORE_GAP_TRAILING: f64 = -0.005;
const SCORE_GAP_INNER: f64 = -0.01;
const SCORE_MATCH_CONSECUTIVE: f64 = 1.0;
const SCORE_MATCH_SLASH: f64 = 0.9;
const SCORE_MATCH_CAPITAL: f64 = 0.7;
const SCORE_MATCH_DOT: f64 = 0.6;

fn assert_score_eq(expected: f64, actual: f64) {
    if expected.is_infinite() {
        assert_eq!(expected, actual);
    } else {
        assert!(
            (expected - actual).abs() <= SCORE_TOLERANCE,
            "expected {expected}, got {actual}",
        );
    }
}

fn candidates(values: &[&[u8]]) -> Vec<Vec<u8>> {
    values.iter().map(|value| value.to_vec()).collect()
}

fn ranked_indices(results: &[(usize, f64)]) -> Vec<usize> {
    results.iter().map(|(index, _)| *index).collect()
}

#[test]
fn has_match_accepts_exact_and_partial_subsequences() {
    assert!(has_match(b"a", b"a"));
    assert!(has_match(b"a", b"ab"));
    assert!(has_match(b"a", b"ba"));
    assert!(has_match(b"abc", b"a|b|c"));
}

#[test]
fn has_match_rejects_non_matches() {
    assert!(!has_match(b"a", b""));
    assert!(!has_match(b"a", b"b"));
    assert!(!has_match(b"ass", b"tags"));
}

#[test]
fn has_match_empty_query_always_matches() {
    assert!(has_match(b"", b""));
    assert!(has_match(b"", b"a"));
}

#[test]
fn has_match_preserves_fzys_asymmetric_ascii_case_rule() {
    assert!(has_match(b"f", b"foo"));
    assert!(has_match(b"f", b"Foo"));
    assert!(has_match(b"F", b"Foo"));
    assert!(!has_match(b"F", b"foo"));
}

#[test]
fn score_prefers_starts_of_words() {
    assert!(match_score(b"amor", b"app/models/order") > match_score(b"amor", b"app/models/zrder"));
}

#[test]
fn score_prefers_consecutive_letters() {
    assert!(match_score(b"amo", b"app/m/foo") < match_score(b"amo", b"app/models/foo"));
}

#[test]
fn score_prefers_contiguous_letters_over_a_period() {
    assert!(match_score(b"gemfil", b"Gemfile.lock") < match_score(b"gemfil", b"Gemfile"));
}

#[test]
fn score_prefers_shorter_matches() {
    assert!(match_score(b"abce", b"abcdef") > match_score(b"abce", b"abc de"));
    assert!(match_score(b"abc", b"    a b c ") > match_score(b"abc", b" a  b  c "));
    assert!(match_score(b"abc", b" a b c    ") > match_score(b"abc", b" a  b  c "));
}

#[test]
fn score_prefers_shorter_candidates() {
    assert!(match_score(b"test", b"tests") > match_score(b"test", b"testing"));
}

#[test]
fn score_prefers_the_start_of_the_candidate() {
    assert!(match_score(b"test", b"testing") > match_score(b"test", b"/testing"));
}

#[test]
fn score_exact_match_is_positive_infinity() {
    assert_score_eq(f64::INFINITY, match_score(b"abc", b"abc"));
    assert_score_eq(f64::INFINITY, match_score(b"aBc", b"abC"));
}

#[test]
fn score_empty_query_is_negative_infinity() {
    assert_score_eq(f64::NEG_INFINITY, match_score(b"", b""));
    assert_score_eq(f64::NEG_INFINITY, match_score(b"", b"a"));
    assert_score_eq(f64::NEG_INFINITY, match_score(b"", b"bb"));
}

#[test]
fn score_case_folds_independently_of_match_eligibility() {
    // `has_match` retains fzy's asymmetric `strcasechr` rule, but scoring
    // preserves the upstream DP's independent ASCII folding behavior.
    assert!(!has_match(b"aBc", b"abC"));
    assert_score_eq(f64::INFINITY, match_score(b"aBc", b"abC"));
    assert_score_eq(f64::NEG_INFINITY, match_score(b"abc", b"def"));
}

#[test]
fn score_applies_gap_penalties() {
    assert_score_eq(SCORE_GAP_LEADING, match_score(b"a", b"*a"));
    assert_score_eq(SCORE_GAP_LEADING * 2.0, match_score(b"a", b"*ba"));
    assert_score_eq(
        SCORE_GAP_LEADING * 2.0 + SCORE_GAP_TRAILING,
        match_score(b"a", b"**a*"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING * 2.0 + SCORE_GAP_TRAILING * 2.0,
        match_score(b"a", b"**a**"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING * 2.0 + SCORE_MATCH_CONSECUTIVE + SCORE_GAP_TRAILING * 2.0,
        match_score(b"aa", b"**aa**"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING + SCORE_GAP_LEADING + SCORE_GAP_INNER + SCORE_GAP_TRAILING + SCORE_GAP_TRAILING,
        match_score(b"aa", b"**a*a**"),
    );
}

#[test]
fn score_rewards_consecutive_letters() {
    assert_score_eq(
        SCORE_GAP_LEADING + SCORE_MATCH_CONSECUTIVE,
        match_score(b"aa", b"*aa"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING + SCORE_MATCH_CONSECUTIVE * 2.0,
        match_score(b"aaa", b"*aaa"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING + SCORE_GAP_INNER + SCORE_MATCH_CONSECUTIVE,
        match_score(b"aaa", b"*a*aa"),
    );
}

#[test]
fn score_rewards_letters_after_a_slash() {
    assert_score_eq(SCORE_GAP_LEADING + SCORE_MATCH_SLASH, match_score(b"a", b"/a"));
    assert_score_eq(
        SCORE_GAP_LEADING * 2.0 + SCORE_MATCH_SLASH,
        match_score(b"a", b"*/a"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING * 2.0 + SCORE_MATCH_SLASH + SCORE_MATCH_CONSECUTIVE,
        match_score(b"aa", b"a/aa"),
    );
}

#[test]
fn score_rewards_capitals() {
    assert_score_eq(SCORE_GAP_LEADING + SCORE_MATCH_CAPITAL, match_score(b"a", b"bA"));
    assert_score_eq(
        SCORE_GAP_LEADING * 2.0 + SCORE_MATCH_CAPITAL,
        match_score(b"a", b"baA"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING * 2.0 + SCORE_MATCH_CAPITAL + SCORE_MATCH_CONSECUTIVE,
        match_score(b"aa", b"baAa"),
    );
}

#[test]
fn score_rewards_letters_after_a_dot() {
    assert_score_eq(SCORE_GAP_LEADING + SCORE_MATCH_DOT, match_score(b"a", b".a"));
    assert_score_eq(
        SCORE_GAP_LEADING * 3.0 + SCORE_MATCH_DOT,
        match_score(b"a", b"*a.a"),
    );
    assert_score_eq(
        SCORE_GAP_LEADING + SCORE_GAP_INNER + SCORE_MATCH_DOT,
        match_score(b"a", b"*a.a"),
    );
}

#[test]
fn score_and_positions_reject_candidates_longer_than_the_upstream_limit() {
    let long = vec![b'a'; 4095];

    assert_score_eq(f64::NEG_INFINITY, match_score(b"aa", &long));
    assert_score_eq(f64::NEG_INFINITY, match_score(&long, b"aa"));
    assert_score_eq(f64::NEG_INFINITY, match_score(&long, &long));
    assert_eq!(match_positions(&long, &long), None);
}

#[test]
fn positions_return_the_optimal_consecutive_match() {
    assert_eq!(match_positions(b"amo", b"app/models/foo"), Some(vec![0, 4, 5]));
}

#[test]
fn positions_prefer_the_start_of_a_word() {
    assert_eq!(
        match_positions(b"amor", b"app/models/order"),
        Some(vec![0, 4, 11, 12]),
    );
}

#[test]
fn positions_return_unbonused_matches() {
    assert_eq!(match_positions(b"as", b"tags"), Some(vec![1, 3]));
    assert_eq!(match_positions(b"as", b"examples.txt"), Some(vec![2, 7]));
}

#[test]
fn positions_choose_the_latest_equal_scoring_word_starts() {
    assert_eq!(match_positions(b"abc", b"a/a/b/c/c"), Some(vec![2, 4, 6]));
}

#[test]
fn positions_for_an_exact_match_cover_every_byte() {
    assert_eq!(match_positions(b"foo", b"foo"), Some(vec![0, 1, 2]));
}

#[test]
fn positions_distinguish_empty_matches_from_non_matches() {
    assert_eq!(match_positions(b"", b"candidate"), Some(Vec::new()));
    assert_eq!(match_positions(b"z", b"candidate"), None);
}

#[test]
fn rank_of_an_empty_candidate_list_is_empty() {
    assert!(rank(b"", &[]).is_empty());
}

// The native upstream ARM build contracts the initial gap multiplication and
// word bonus into an FMA. Rounding that twice changes ordering even though the
// six-decimal printed scores are identical. Keep the smallest differential case.
#[cfg(all(target_os = "macos", target_arch = "aarch64"))]
#[test]
fn rank_preserves_native_fzy_rounding_for_nearly_equal_scores() {
    let values = candidates(&[b"_.e1cdaX230/YZ-b", b"b_.e1cdaX230/YZ-"]);
    assert_eq!(ranked_indices(&rank(b"Y-", &values)), vec![1, 0]);
}

#[cfg(target_os = "linux")]
#[test]
fn rank_preserves_gcc_fzy_rounding_for_nearly_equal_scores() {
    let values = candidates(&[b"_.e1cdaX230/YZ-b", b"b_.e1cdaX230/YZ-"]);
    assert_eq!(ranked_indices(&rank(b"Y-", &values)), vec![0, 1]);
}

#[test]
fn rank_filters_by_eligibility_orders_by_score_and_keeps_ties_stable() {
    let candidates = candidates(&[b"tags", b"test"]);

    let ranked = rank(b"ts", &candidates);
    assert_eq!(ranked_indices(&ranked), vec![1, 0]);
    assert!(ranked[0].1 > ranked[1].1);

    let tied = rank(b"", &candidates);
    assert_eq!(ranked_indices(&tied), vec![0, 1]);
    assert!(tied.iter().all(|(_, score)| *score == f64::NEG_INFINITY));
}

#[test]
fn choices_empty_stays_empty_when_navigated() {
    let mut choices = Choices::new(Vec::new());

    assert_eq!(choices.len(), 0);
    assert_eq!(choices.available(), 0);
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.get(0), None);
    assert_eq!(choices.selected(), None);

    choices.prev();
    assert_eq!(choices.selection(), 0);
    choices.next();
    assert_eq!(choices.selection(), 0);
}

#[test]
fn choices_with_one_candidate_searches_and_navigates() {
    let mut choices = Choices::new(candidates(&[b"tags"]));

    choices.search(b"");
    assert_eq!(choices.available(), 1);
    assert_eq!(choices.selection(), 0);

    choices.search(b"t");
    assert_eq!(choices.available(), 1);
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.get(0), Some(b"tags".as_slice()));
    assert_eq!(choices.get(1), None);
    assert_eq!(choices.selected(), Some(b"tags".as_slice()));

    choices.prev();
    assert_eq!(choices.selection(), 0);
    choices.next();
    assert_eq!(choices.selection(), 0);
}

#[test]
fn choices_filters_sorts_and_wraps_navigation() {
    let mut choices = Choices::new(candidates(&[b"tags", b"test"]));

    choices.search(b"");
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.available(), 2);
    assert_eq!(ranked_indices(&rank(b"", &candidates(&[b"tags", b"test"]))), vec![0, 1]);

    choices.next();
    assert_eq!(choices.selection(), 1);
    choices.next();
    assert_eq!(choices.selection(), 0);
    choices.prev();
    assert_eq!(choices.selection(), 1);
    choices.prev();
    assert_eq!(choices.selection(), 0);

    choices.search(b"te");
    assert_eq!(choices.available(), 1);
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.get(0), Some(b"test".as_slice()));
    choices.next();
    assert_eq!(choices.selection(), 0);
    choices.prev();
    assert_eq!(choices.selection(), 0);

    choices.search(b"foobar");
    assert_eq!(choices.available(), 0);
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.get(0), None);
    assert_eq!(choices.selected(), None);

    choices.search(b"ts");
    assert_eq!(choices.available(), 2);
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.get(0), Some(b"test".as_slice()));
    assert_eq!(choices.get(1), Some(b"tags".as_slice()));
    assert_eq!(ranked_indices(&rank(b"ts", &candidates(&[b"tags", b"test"]))), vec![1, 0]);
}

#[test]
fn choices_have_no_results_before_a_search() {
    let choices = Choices::new(candidates(&[b"test"]));

    assert_eq!(choices.available(), 0);
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.len(), 1);
    assert_eq!(choices.get(0), None);
    assert_eq!(choices.selected(), None);
}

#[test]
fn choices_accept_utf8_candidates_without_panicking() {
    let candidate = "Edmund Husserl - Méditations cartésiennes - Introduction a la phénoménologie.pdf".as_bytes();
    let mut choices = Choices::new(candidates(&[candidate]));

    choices.search(b"e");

    assert_eq!(choices.available(), 1);
    assert_eq!(choices.get(0), Some(candidate));
}

#[test]
fn choices_large_input_finds_and_ranks_all_subsequence_matches() {
    let candidates: Vec<Vec<u8>> = (0..100_000)
        .map(|number| number.to_string().into_bytes())
        .collect();
    let mut choices = Choices::new(candidates);

    choices.search(b"12");

    assert_eq!(choices.available(), 8146);
    assert_eq!(choices.get(0), Some(b"12".as_slice()));
    assert_eq!(choices.selected(), Some(b"12".as_slice()));
}

// A fixed xorshift sequence replaces upstream's theft-generated C strings.
// The generator stops a case at NUL, matching the original C string domain.
fn next_property_u64(state: &mut u64) -> u64 {
    *state ^= *state << 13;
    *state ^= *state >> 7;
    *state ^= *state << 17;
    *state
}

fn next_property_string(state: &mut u64) -> Vec<u8> {
    let length = (next_property_u64(state) % 128 + 1) as usize;
    let mut value = Vec::with_capacity(length);

    for _ in 0..length {
        let byte = next_property_u64(state) as u8;
        if byte == 0 {
            break;
        }
        value.push(byte);
    }

    value
}

// The upstream random generator has a low probability of yielding a pair of
// nonempty strings in a subsequence relation. Keep that stream above, then
// supplement it with bounded cases that are guaranteed to exercise scoring
// and backtracking across ASCII and arbitrary non-NUL bytes.
fn next_constructive_property_case(state: &mut u64) -> (Vec<u8>, Vec<u8>) {
    const ASCII_BYTES: &[u8] = b"aAbBcCzZ/-_. 09";

    let length = (next_property_u64(state) % 16 + 1) as usize;
    let mut haystack = Vec::with_capacity(length);
    let mut needle = Vec::new();

    for _ in 0..length {
        let random = next_property_u64(state);
        let byte = if random & 1 == 0 {
            ASCII_BYTES[(random as usize / 2) % ASCII_BYTES.len()]
        } else {
            (random as u8).max(1)
        };
        haystack.push(byte);

        if random & 4 != 0 {
            needle.push(byte);
        }
    }

    if needle.is_empty() {
        needle.push(haystack[(next_property_u64(state) as usize) % haystack.len()]);
    }

    (needle, haystack)
}

#[test]
fn property_nonempty_eligible_matches_have_a_score() {
    let mut state = 0xa600_d16b_175e_ed_u64;

    for case in 0..100_000 {
        let needle = next_property_string(&mut state);
        let haystack = next_property_string(&mut state);

        if !needle.is_empty() && has_match(&needle, &haystack) {
            assert!(
                match_score(&needle, &haystack) != f64::NEG_INFINITY,
                "case {case}: needle={needle:?}, haystack={haystack:?}",
            );
        }
    }

    for case in 0..10_000 {
        let (needle, haystack) = next_constructive_property_case(&mut state);
        assert!(has_match(&needle, &haystack));
        assert!(
            match_score(&needle, &haystack) != f64::NEG_INFINITY,
            "constructive case {case}: needle={needle:?}, haystack={haystack:?}",
        );
    }
}

#[test]
fn property_positions_are_increasing_and_identify_the_eligible_bytes() {
    let mut state = 0xa600_d16b_175e_ed_u64;

    for case in 0..100_000 {
        let needle = next_property_string(&mut state);
        let haystack = next_property_string(&mut state);

        if needle.is_empty() || !has_match(&needle, &haystack) {
            continue;
        }

        let positions = match_positions(&needle, &haystack).unwrap_or_else(|| {
            panic!("case {case}: eligible needle={needle:?}, haystack={haystack:?}")
        });
        assert_eq!(
            positions.len(),
            needle.len(),
            "case {case}: needle={needle:?}, haystack={haystack:?}",
        );

        for window in positions.windows(2) {
            assert!(
                window[0] < window[1],
                "case {case}: positions={positions:?}, needle={needle:?}, haystack={haystack:?}",
            );
        }

        for (needle_byte, position) in needle.iter().zip(positions) {
            assert!(
                needle_byte.eq_ignore_ascii_case(&haystack[position]),
                "case {case}: needle={needle:?}, haystack={haystack:?}, position={position}",
            );
        }
    }

    for case in 0..10_000 {
        let (needle, haystack) = next_constructive_property_case(&mut state);
        let positions = match_positions(&needle, &haystack).unwrap_or_else(|| {
            panic!("constructive case {case}: needle={needle:?}, haystack={haystack:?}")
        });
        assert_eq!(positions.len(), needle.len());

        for window in positions.windows(2) {
            assert!(window[0] < window[1]);
        }
        for (needle_byte, position) in needle.iter().zip(positions) {
            assert!(needle_byte.eq_ignore_ascii_case(&haystack[position]));
        }
    }
}
