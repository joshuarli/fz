use fz::{Candidates, Choices};

#[test]
fn packed_candidates_preserve_record_bytes_and_skip_empty_records() {
    for (input, delimiter, expected) in [
        (b"\nfoo\n\nbar\n".as_slice(), b'\n', vec![b"foo".as_slice(), b"bar"]),
        (b"\nfoo\nbar", b'\n', vec![b"foo".as_slice(), b"bar"]),
        (b"\0foo\nbar\0\0baz\0", 0, vec![b"foo\nbar".as_slice(), b"baz"]),
        (b"foo\nbar\0ignored\nbaz", b'\n', vec![b"foo".as_slice(), b"bar"]),
        (b"\xff\ncarriage\r\n", b'\n', vec![b"\xff".as_slice(), b"carriage\r"]),
        (b"\n\n", b'\n', vec![]),
        (b"", b'\n', vec![]),
    ] {
        let candidates = Candidates::from_input(input.to_vec(), delimiter);
        assert_eq!(candidates.len(), expected.len());
        assert_eq!(candidates.is_empty(), expected.is_empty());
        for (i, &bytes) in expected.iter().enumerate() {
            assert_eq!(candidates.get(i), Some(bytes));
        }
        assert_eq!(candidates.get(expected.len()), None);
        let mut choices = Choices::from_candidates(candidates);
        assert_eq!(choices.available(), 0);
        choices.search(b"");
        for (i, &bytes) in expected.iter().enumerate() {
            assert_eq!(choices.get(i), Some(bytes));
            assert_eq!(choices.getscore(i), Some(f64::NEG_INFINITY));
        }
    }
}

#[test]
fn empty_search_after_filter_restores_original_order_and_selection() {
    let mut choices = Choices::from_candidates(Candidates::from_input(b"tags\ntest\n".to_vec(), b'\n'));
    choices.search(b"ts");
    assert_eq!(choices.get(0), Some(b"test".as_slice()));
    assert!(choices.getscore(0).unwrap() > choices.getscore(1).unwrap());
    choices.next();
    choices.search(b"");
    assert_eq!(choices.selection(), 0);
    assert_eq!(choices.available(), 2);
    assert_eq!(choices.get(0), Some(b"tags".as_slice()));
    choices.prev();
    assert_eq!(choices.selected(), Some(b"test".as_slice()));
    choices.search(b"nonexistent");
    assert_eq!(choices.available(), 0);
    assert_eq!(choices.getscore(0), None);
}

#[test]
fn query_edits_produce_the_same_results_as_independent_searches() {
    let data = b"tags\ntest\nTagSet\nTeSt\nother\n";
    let mut edited = Choices::from_candidates(Candidates::from_input(data.to_vec(), b'\n'));
    for query in [b"t".as_slice(), b"te", b"tes", b"", b"ta", b"tags", b"tagss", b"ta", b"T", b"te"] {
        edited.search(query);
        let mut fresh = Choices::from_candidates(Candidates::from_input(data.to_vec(), b'\n'));
        fresh.search(query);
        assert_eq!(edited.available(), fresh.available(), "query {query:?}");
        for index in 0..fresh.available() {
            assert_eq!(edited.get(index), fresh.get(index), "query {query:?}");
            assert_eq!(edited.getscore(index), fresh.getscore(index), "query {query:?}");
        }
        assert_eq!(edited.selection(), 0);
        edited.next();
    }
}

#[test]
fn worker_limits_preserve_scores_order_and_navigation() {
    let input: Vec<u8> = (0..10_000).flat_map(|i| {
        format!("src/long_project_name/component_{i:05}/item.rs\n").into_bytes()
    }).collect();
    let mut serial = Choices::from_candidates(Candidates::from_input(input.clone(), b'\n'));
    let mut parallel = Choices::from_candidates(Candidates::from_input(input, b'\n'));
    serial.set_workers(1);
    parallel.set_workers(4);
    for query in [b"".as_slice(), b"component", b"component99", b"item", b"nonexistent"] {
        serial.search(query);
        parallel.search(query);
        assert_eq!(serial.available(), parallel.available());
        for index in 0..serial.available() {
            assert_eq!(serial.get(index), parallel.get(index), "query {query:?}");
            assert_eq!(serial.getscore(index), parallel.getscore(index), "query {query:?}");
        }
        serial.prev();
        parallel.prev();
        assert_eq!(serial.selected(), parallel.selected());
    }
}
