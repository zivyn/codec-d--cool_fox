//! Where the host copies of title memory lie, for one question: does any other copy cover this title memory?
//!
//! A piece of a copy that shaders write may be left on the card (`crate::land::own`) only if no other host copy of
//! title memory covers its bytes: any other copy is brought up to date from title memory, which would not hold the
//! piece's new bytes. The copies are in three places (title buffers' copies, windows of memory outside buffers, copies
//! of a buffer view's own bytes) and each frame the landing asks about a thousand pieces, so the spans are taken once
//! per landing, sorted, with for each prefix the furthest end and the furthest end of any other copy: one binary
//! search answers.

use super::{EXACT, LOOSE};

/// The spans of every copy made, by start.
pub(crate) struct OverlapIndex {
    /// (start, end, copy number), by start.
    spans: Vec<(u64, u64, u64)>,
    /// For each prefix of `spans`: the furthest end and its copy, and the furthest end of any other copy.
    best: Vec<(u64, u64, u64)>,
}

/// The index of the copies there are now. A copy made after is taken whole by its first refresh, from title memory, which
/// lands what is owed there first: it is not missed.
pub(crate) fn overlap_index() -> OverlapIndex {
    let mut spans: Vec<(u64, u64, u64)> = Vec::new();
    {
        let map = super::buffers_read();
        spans.extend(
            map.iter().filter_map(|(key, entry)| {
                entry.shadow.as_ref().map(|s| (key.0, entry.end, s.id()))
            }),
        );
    }
    {
        let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        spans.extend(
            loose
                .iter()
                .map(|(&(start, len), s)| (start, start + len, s.id())),
        );
    }
    {
        let exact = EXACT.lock().unwrap_or_else(|e| e.into_inner());
        spans.extend(
            exact
                .1
                .iter()
                .map(|(&(start, len), (s, _))| (start, start + len, s.id())),
        );
    }
    OverlapIndex::of(spans)
}

impl OverlapIndex {
    /// The index of `spans` (start, end, copy number).
    pub(crate) fn of(mut spans: Vec<(u64, u64, u64)>) -> OverlapIndex {
        spans.sort_unstable();
        let mut best = Vec::with_capacity(spans.len());
        let (mut first, mut owner, mut second) = (0u64, 0u64, 0u64);
        for &(_, end, id) in &spans {
            if best.is_empty() {
                (first, owner, second) = (end, id, 0);
            } else if id == owner {
                first = first.max(end);
            } else if end > first {
                second = second.max(first);
                (first, owner) = (end, id);
            } else {
                second = second.max(end);
            }
            best.push((first, owner, second));
        }
        OverlapIndex { spans, best }
    }

    /// Whether a copy other than number `id` covers any byte of `[from, to)` of title memory.
    pub(crate) fn others(&self, id: u64, from: u64, to: u64) -> bool {
        let n = self.spans.partition_point(|s| s.0 < to);
        let Some(&(first, owner, second)) = n.checked_sub(1).and_then(|i| self.best.get(i)) else {
            return false;
        };
        if owner != id {
            first > from
        } else {
            second > from
        }
    }

    /// How many copies are in it.
    pub(crate) fn len(&self) -> usize {
        self.spans.len()
    }
}

#[cfg(test)]
mod tests {
    use super::OverlapIndex;

    /// Brute force: any span of another copy that meets `[from, to)`.
    fn meets(spans: &[(u64, u64, u64)], id: u64, from: u64, to: u64) -> bool {
        spans
            .iter()
            .any(|&(a, b, other)| other != id && a < to && b > from)
    }

    #[test]
    fn the_index_answers_as_a_scan_does() {
        let mut seed = 0x2545_f491_4f6c_dd1du64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for round in 0..60u64 {
            let count = 1 + (next() % 40) as usize;
            let spans: Vec<(u64, u64, u64)> = (0..count as u64)
                .map(|id| {
                    let start = next() % 1000;
                    let len = 1 + next() % if round % 3 == 0 { 600 } else { 40 };
                    (start, start + len, id + 1)
                })
                .collect();
            let index = OverlapIndex::of(spans.clone());
            assert_eq!(index.len(), count);
            for _ in 0..200 {
                let from = next() % 1100;
                let to = from + 1 + next() % 90;
                let id = 1 + next() % (count as u64 + 1);
                assert_eq!(
                    index.others(id, from, to),
                    meets(&spans, id, from, to),
                    "round {round}, copy {id}, [{from}, {to}) in {spans:?}"
                );
            }
        }
    }

    #[test]
    fn a_copy_alone_has_no_other_over_it() {
        let index = OverlapIndex::of(vec![(100, 200, 1)]);
        assert!(!index.others(1, 100, 200));
        assert!(index.others(2, 150, 160));
        assert!(!index.others(2, 200, 300));
        assert!(!index.others(2, 0, 100));
        assert!(!OverlapIndex::of(Vec::new()).others(1, 0, 10));
    }
}
