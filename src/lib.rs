mod score;
pub use score::{has_match, match_score, match_positions};

fn rank_into(needle: &[u8], candidates: &[Vec<u8>], results: &mut Vec<(usize, f64)>) {
    results.clear();
    let mut scorer = score::ScoreWorkspace::new();
    results.extend(candidates.iter().enumerate().filter_map(|(index, candidate)| {
        has_match(needle, candidate).then(|| (index, scorer.score(needle, candidate)))
    }));
    // Index makes ties deterministic without allocating a stable-sort buffer.
    results.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
}

/// Eligible candidate indices in descending score order, retaining input order
/// for ties. Candidate bytes are never copied while searching or sorting.
pub fn rank(needle: &[u8], candidates: &[Vec<u8>]) -> Vec<(usize, f64)> {
    let mut results = Vec::new();
    rank_into(needle, candidates, &mut results);
    results
}

/// A search resets selection to the first result. Before the first search, no
/// result is available. Navigation wraps, and empty results retain index zero.
pub struct Choices {
    candidates: Candidates,
    all: bool,
    query: Vec<u8>,
    workers: usize,
    results: Vec<(usize, f64)>,
    selection: usize,
}

impl Choices {
    pub fn new(candidates: Vec<Vec<u8>>) -> Self {
        Self::from_candidates(Candidates::from_records(candidates))
    }

    pub fn from_candidates(candidates: Candidates) -> Self {
        Self { candidates, results: Vec::new(), selection: 0, all: false, query: Vec::new(), workers: 1 }
    }

    /// Set a worker ceiling. Zero uses available CPUs; at most four workers
    /// participate, and small searches stay on the caller to avoid startup cost.
    pub fn set_workers(&mut self, workers: usize) {
        self.workers = if workers == 0 {
            std::thread::available_parallelism().map(usize::from).unwrap_or(1)
        } else { workers }.clamp(1, 4);
    }

    pub fn search(&mut self, needle: &[u8]) {
        self.selection = 0;
        let refine = !self.query.is_empty() && needle.starts_with(&self.query);
        self.query.clear();
        self.query.extend_from_slice(needle);
        self.all = needle.is_empty();
        // The empty query is already in input order; no scoring, sorting, or
        // result allocation is needed for the initial interactive screen.
        if self.all { self.results.clear(); return; }
        if refine {
            let mut scorer = score::ScoreWorkspace::new();
            // Extending a subsequence query can only remove eligible matches.
            // Editing or deleting its earlier bytes requires a full search.
            self.results.retain_mut(|(index, score)| {
                let candidate = self.candidates.get(*index).unwrap();
                if has_match(needle, candidate) {
                    *score = scorer.score(needle, candidate);
                    true
                } else { false }
            });
        } else {
            self.results.clear();
            // Reserve once. Growing dense results geometrically retains old
            // allocator pages and inflates peak RSS even after realloc frees them.
            self.results.reserve_exact(self.candidates.len());
            let work = self.candidates.bytes.len().saturating_mul(needle.len());
            if self.workers > 1 && self.candidates.len() >= 4096 && work >= 2_000_000 {
                let chunk = self.candidates.len().div_ceil(self.workers);
                // Workers read disjoint candidate ranges and own only their
                // share of the result capacity, unlike fzy's N-sized per-worker
                // buffers. The caller participates and performs one final sort.
                std::thread::scope(|scope| {
                    let mut handles = Vec::with_capacity(self.workers - 1);
                    let candidates = &self.candidates;
                    for start in (chunk..candidates.len()).step_by(chunk) {
                        let end = (start + chunk).min(candidates.len());
                        handles.push(scope.spawn(move || {
                            let mut results = Vec::with_capacity(end - start);
                            score_range(needle, candidates, start, end, &mut results);
                            results
                        }));
                    }
                    score_range(needle, candidates, 0, chunk, &mut self.results);
                    for handle in handles { self.results.extend(handle.join().unwrap()); }
                });
            } else {
                score_range(needle, &self.candidates, 0, self.candidates.len(), &mut self.results);
            }
        }
        self.results.sort_unstable_by(|a, b| b.1.total_cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
    }

    pub fn next(&mut self) {
        if self.available() > 0 {
            self.selection = (self.selection + 1) % self.available();
        }
    }

    pub fn prev(&mut self) {
        if self.available() > 0 {
            self.selection = (self.selection + self.available() - 1) % self.available();
        }
    }

    pub fn get(&self, index: usize) -> Option<&[u8]> {
        let candidate = if self.all { index } else { self.results.get(index)?.0 };
        self.candidates.get(candidate)
    }

    pub fn getscore(&self, index: usize) -> Option<f64> {
        if self.all { (index < self.candidates.len()).then_some(f64::NEG_INFINITY) }
        else { self.results.get(index).map(|&(_, score)| score) }
    }

    pub fn selected(&self) -> Option<&[u8]> { self.get(self.selection) }
    pub fn len(&self) -> usize { self.candidates.len() }
    pub fn is_empty(&self) -> bool { self.candidates.is_empty() }
    pub fn available(&self) -> usize { if self.all { self.candidates.len() } else { self.results.len() } }
    pub fn selection(&self) -> usize { self.selection }
}

fn score_range(needle: &[u8], candidates: &Candidates, start: usize, end: usize, results: &mut Vec<(usize, f64)>) {
    let mut scorer = score::ScoreWorkspace::new();
    for index in start..end {
        let candidate = candidates.get(index).unwrap();
        if has_match(needle, candidate) {
            results.push((index, scorer.score(needle, candidate)));
        }
    }
}

/// Candidate bytes share one allocation. Adjacent offsets delimit records, so
/// indexing costs one machine word per candidate, with no allocation per line.
/// Input records retain their separators; only the offset index is allocated.
/// Runs of empty records are skipped when indexing and trimmed when borrowing.
pub struct Candidates {
    bytes: Vec<u8>,
    offsets: Vec<usize>,
    delimiter: Option<u8>,
}

impl Candidates {
    pub fn from_input(mut bytes: Vec<u8>, delimiter: u8) -> Self {
        let mut offsets = Vec::with_capacity(bytes.len() / 48 + 1);
        let mut start = 0;
        while start < bytes.len() {
            let end = start + score::find_either(&bytes[start..], delimiter, 0)
                .unwrap_or(bytes.len() - start);
            if start < end { offsets.push(start); }
            if end == bytes.len() { break; }
            if delimiter != 0 && bytes[end] == 0 {
                bytes.truncate(end);
                break;
            }
            start = end + 1;
        }
        offsets.push(bytes.len());
        offsets.shrink_to_fit();
        bytes.shrink_to_fit();
        Self { bytes, offsets, delimiter: Some(delimiter) }
    }

    fn from_records(records: Vec<Vec<u8>>) -> Self {
        let mut bytes = Vec::with_capacity(records.iter().map(Vec::len).sum());
        let mut offsets = Vec::with_capacity(records.len() + 1);
        for record in records {
            offsets.push(bytes.len());
            bytes.extend_from_slice(&record);
        }
        offsets.push(bytes.len());
        Self { bytes, offsets, delimiter: None }
    }

    pub fn len(&self) -> usize { self.offsets.len() - 1 }
    pub fn is_empty(&self) -> bool { self.len() == 0 }
    pub fn get(&self, index: usize) -> Option<&[u8]> {
        let mut end = *self.offsets.get(index.checked_add(1)?)?;
        let start = self.offsets[index];
        if let Some(delimiter) = self.delimiter {
            while end > start && self.bytes[end - 1] == delimiter { end -= 1; }
        }
        Some(&self.bytes[start..end])
    }
}
