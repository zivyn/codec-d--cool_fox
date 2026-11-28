//! Where the host copies of title memory lie, for one question: does any other copy cover this title memory?
//!
//! A piece of a copy that shaders write may be left on the card (`crate::land::own`) only if no other host copy of
//! title memory covers its bytes: any other copy is brought up to date from title memory, which would not hold the
//! piece's new bytes. The copies are in three places (title buffers' copies, windows of memory outside buffers, copies
//! of a buffer view's own bytes) and each frame the landing asks about a thousand pieces, so the spans are taken once
//! per landing, sorted, with for each prefix the furthest end and the furthest end of any other copy: one binary
//! search answers.

use std::sync::Arc;

use super::{EXACT, LOOSE, Shadow};

/// The spans of every copy made, by start.
pub(crate) struct OverlapIndex {
    /// (start, end, copy number), by start.
    spans: Vec<(u64, u64, u64)>,
    /// For each prefix of `spans`: the furthest end and its copy, and the furthest end of any other copy.
    best: Vec<(u64, u64, u64)>,
    /// The copies of `spans`, in the same order (empty for an index made of spans alone, in the tests).
    copies: Vec<Arc<Shadow>>,
}

/// The index of the copies there are now. A copy made after is taken whole by its first refresh, from title memory, which
/// lands what is owed there first: it is not missed.
pub(crate) fn overlap_index() -> OverlapIndex {
    let mut copies: Vec<(u64, u64, Arc<Shadow>)> = Vec::new();
    {
        let map = super::buffers_read();
        copies.extend(map.iter().filter_map(|(key, entry)| {
            entry.shadow.as_ref().map(|s| (key.0, entry.end, s.clone()))
        }));
    }
    {
        let loose = LOOSE.read().unwrap_or_else(|e| e.into_inner());
        copies.extend(
            loose
                .iter()
                .map(|(&(start, len), s)| (start, start + len, s.clone())),
        );
    }
    {
        let exact = EXACT.lock().unwrap_or_else(|e| e.into_inner());
        copies.extend(
            exact
                .1
                .iter()
                .map(|(&(start, len), (s, _))| (start, start + len, s.clone())),
        );
    }
    copies.sort_by_key(|c| (c.0, c.1, c.2.id()));
    let spans: Vec<(u64, u64, u64)> = copies.iter().map(|c| (c.0, c.1, c.2.id())).collect();
    let mut index = OverlapIndex::of(spans);
    index.copies = copies.into_iter().map(|c| c.2).collect();
    index
}

impl OverlapIndex {
    /// The index of `spans` (start, end, copy number).
    pub(crate) fn of(mut spans: Vec<(u64, u64, u64)>) -> OverlapIndex {
        spans.sort_by_key(|s| (s.0, s.1, s.2));
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
        OverlapIndex {
            spans,
            best,
            copies: Vec::new(),
        }
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

    /// An index of these copies (the tests'; the real one is [`overlap_index`]).
    #[cfg(test)]
    pub(crate) fn of_copies(copies: &[Arc<Shadow>]) -> OverlapIndex {
        let mut sorted: Vec<Arc<Shadow>> = copies.to_vec();
        sorted.sort_by_key(|c| (c.span().0, c.span().0 + c.span().1, c.id()));
        let spans = sorted
            .iter()
            .map(|c| (c.span().0, c.span().0 + c.span().1, c.id()))
            .collect();
        let mut index = OverlapIndex::of(spans);
        index.copies = sorted;
        index
    }

    /// The copies other than number `id` that cover a byte of `[from, to)` (none, for an index of spans alone).
    pub(crate) fn others_over(&self, id: u64, from: u64, to: u64) -> Vec<Arc<Shadow>> {
        let n = self.spans.partition_point(|s| s.0 < to);
        let mut found = Vec::new();
        for i in (0..n).rev() {
            // Nothing before this reaches `from`.
            if self.best[i].0 <= from {
                break;
            }
            let (_, end, other) = self.spans[i];
            if other != id
                && end > from
                && let Some(copy) = self.copies.get(i)
            {
                found.push(copy.clone());
            }
        }
        found
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

    #[test]
    fn the_copies_over_a_range_are_named_without_the_one_asking() {
        use crate::shadow::Shadow;
        let mut sides = [vec![0u8; 100], vec![0u8; 100], vec![0u8; 100]];
        let [s0, s1, s2] = &mut sides;
        let a = Shadow::unbacked(0x7000_0000, s0.as_mut_ptr(), 100, false);
        let b = Shadow::unbacked(0x7000_0050, s1.as_mut_ptr(), 100, false);
        let c = Shadow::unbacked(0x7000_1000, s2.as_mut_ptr(), 100, false);
        let index = OverlapIndex::of_copies(&[a.clone(), b.clone(), c.clone()]);
        let ids = |found: Vec<std::sync::Arc<Shadow>>| {
            let mut ids: Vec<u64> = found.iter().map(|s| s.id()).collect();
            ids.sort_unstable();
            ids
        };
        assert_eq!(
            ids(index.others_over(a.id(), 0x7000_0000, 0x7000_0064)),
            vec![b.id()]
        );
        assert_eq!(
            ids(index.others_over(b.id(), 0x7000_0000, 0x7000_0064)),
            vec![a.id()]
        );
        assert_eq!(
            ids(index.others_over(0, 0x7000_0000, 0x7000_00ff)),
            vec![a.id(), b.id()]
        );
        assert!(
            index
                .others_over(c.id(), 0x7000_1000, 0x7000_1064)
                .is_empty()
        );
        assert!(index.others_over(0, 0x7000_0200, 0x7000_0300).is_empty());
    }
}
