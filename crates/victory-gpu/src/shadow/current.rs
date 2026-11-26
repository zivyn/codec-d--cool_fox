//! Which parts of a copy its kept snapshot holds as they are now
//! ([`super::keeps`]), so that a submission snapshots only the rest.

/// The unit it is tracked in: 4 KiB of title memory (what the write watch
/// reports), cut at multiples of 4 KiB of title addresses.
pub(super) const UNIT: usize = 4096;

/// For a copy of `len` bytes of title memory from address `start` on: per
/// unit, whether the snapshot's bytes there equal the copy's. A unit is
/// current once a submission took all of it, and stops being current when
/// something changes the copy there: a refresh from title memory, shader
/// writes found after a submission, the bytes of a placed copy. A copy
/// that is not tracked (never, or given back) has no units.
#[derive(Default)]
pub(super) struct Current {
    start: usize,
    len: usize,
    units: Vec<bool>,
}

impl Current {
    /// Starts tracking a copy, nothing current, if it is not tracked yet.
    pub(super) fn track(&mut self, start: usize, len: usize) {
        if self.units.is_empty() {
            *self = Self {
                start,
                len,
                units: vec![false; (start + len.max(1) - 1) / UNIT - start / UNIT + 1],
            };
        }
    }

    /// Forgets all: nothing is current.
    pub(super) fn clear(&mut self) {
        self.units.clear();
    }

    /// The units that hold offsets `[from, to)` (`from < to`).
    fn units(&self, from: usize, to: usize) -> std::ops::Range<usize> {
        let first = self.start / UNIT;
        (self.start + from) / UNIT - first..(self.start + to - 1) / UNIT - first + 1
    }

    /// The offsets unit `unit` holds.
    fn unit(&self, unit: usize) -> (usize, usize) {
        let first = self.start / UNIT;
        let lo = ((first + unit) * UNIT).saturating_sub(self.start);
        (lo, ((first + unit + 1) * UNIT - self.start).min(self.len))
    }

    /// A submission gives shaders offsets `[from, to)`: what it must take of
    /// them into the snapshot (runs, in order). A unit it takes whole is
    /// current from now on; of a unit it takes in part only those bytes are
    /// taken (the rest may belong to a range declared earlier, whose bytes
    /// stay as they were taken), and it is taken again next time. With
    /// `kept`, the runs it need not take are put there.
    pub(super) fn plan(
        &mut self,
        from: usize,
        to: usize,
        mut kept: Option<&mut Vec<(usize, usize)>>,
    ) -> Vec<(usize, usize)> {
        let mut taken = Vec::new();
        if from >= to || self.units.is_empty() {
            return taken;
        }
        for i in self.units(from, to) {
            let (lo, hi) = self.unit(i);
            let whole = from <= lo && hi <= to;
            let (lo, hi) = (lo.max(from), hi.min(to));
            if self.units[i] {
                if let Some(kept) = kept.as_deref_mut() {
                    push(kept, lo, hi);
                }
                continue;
            }
            push(&mut taken, lo, hi);
            self.units[i] = whole;
        }
        taken
    }

    /// The copy's bytes at offsets `[from, to)` change: the snapshot is not
    /// current there any more.
    pub(super) fn changed(&mut self, from: usize, to: usize) {
        let to = to.min(self.len);
        if from >= to || self.units.is_empty() {
            return;
        }
        let units = self.units(from, to);
        self.units[units].fill(false);
    }
}

/// `[lo, hi)` after `runs`, joined to its last run if they touch.
fn push(runs: &mut Vec<(usize, usize)>, lo: usize, hi: usize) {
    match runs.last_mut() {
        Some(last) if last.1 == lo => last.1 = hi,
        _ => runs.push((lo, hi)),
    }
}

/// The units of title memory that `now` and `old` (the same length, the
/// first byte at title address `address`, offset `at` of its copy) differ
/// in, as runs of offsets cut at unit edges, added to `out`.
pub(super) fn differing_units(
    address: usize,
    now: &[u8],
    old: &[u8],
    at: usize,
    out: &mut Vec<(usize, usize)>,
) {
    let mut i = 0;
    while i < now.len() {
        let n = (UNIT - (address + i) % UNIT).min(now.len() - i);
        if now[i..i + n] != old[i..i + n] {
            push(out, at + i, at + i + n);
        }
        i += n;
    }
}

#[cfg(test)]
mod tests {
    use super::{Current, UNIT, differing_units};

    fn tracked(start: usize, len: usize) -> Current {
        let mut c = Current::default();
        c.track(start, len);
        c
    }

    /// A copy that starts anywhere in a unit: the first submission takes
    /// what it declares, the second only what changed in between.
    #[test]
    fn only_units_not_current_are_taken() {
        for start in [0usize, 1, UNIT - 1, UNIT, 5 * UNIT + 123] {
            let len = 10 * UNIT + 77;
            let mut c = tracked(start, len);
            // Everything declared: all of it taken, in one run.
            assert_eq!(c.plan(0, len, None), vec![(0, len)], "{start}");
            // Declared again: nothing to take, and `kept` says so.
            let mut kept = Vec::new();
            assert_eq!(c.plan(0, len, Some(&mut kept)), vec![], "{start}");
            assert_eq!(kept, vec![(0, len)], "{start}");
            // The title wrote one byte at offset 3 * UNIT (its unit edges
            // depend on `start`): that unit alone is taken again.
            c.changed(3 * UNIT, 3 * UNIT + 1);
            let (lo, hi) = {
                let unit = (start + 3 * UNIT) / UNIT * UNIT;
                (unit.saturating_sub(start), (unit + UNIT - start).min(len))
            };
            assert_eq!(c.plan(0, len, None), vec![(lo, hi)], "{start}");
            assert_eq!(c.plan(0, len, None), vec![], "{start}");
        }
    }

    /// Changed bytes spanning units take all of them, and two units apart
    /// are two runs.
    #[test]
    fn changes_are_taken_by_unit_in_runs() {
        let mut c = tracked(0, 20 * UNIT);
        c.plan(0, 20 * UNIT, None);
        c.changed(2 * UNIT + 5, 4 * UNIT + 1);
        c.changed(9 * UNIT, 9 * UNIT + 1);
        c.changed(19 * UNIT, 99 * UNIT);
        assert_eq!(
            c.plan(0, 20 * UNIT, None),
            vec![
                (2 * UNIT, 5 * UNIT),
                (9 * UNIT, 10 * UNIT),
                (19 * UNIT, 20 * UNIT)
            ]
        );
    }

    /// A unit a range holds in part is taken in part, every time, and is
    /// never taken to be current; two ranges that cover it together do not
    /// make it current either.
    #[test]
    fn a_unit_held_in_part_is_not_current() {
        let mut c = tracked(0, 4 * UNIT);
        assert_eq!(
            c.plan(UNIT / 2, 2 * UNIT + 10, None),
            vec![(UNIT / 2, 2 * UNIT + 10)]
        );
        // The whole unit 1 was in it: current. Units 0 and 2 were held in part.
        assert_eq!(
            c.plan(UNIT / 2, 2 * UNIT + 10, None),
            vec![(UNIT / 2, UNIT), (2 * UNIT, 2 * UNIT + 10)]
        );
        assert_eq!(c.plan(0, UNIT / 2, None), vec![(0, UNIT / 2)]);
        assert_eq!(c.plan(UNIT / 2, UNIT, None), vec![(UNIT / 2, UNIT)]);
    }

    /// The last unit of a copy ends at the copy's end, and nothing is taken
    /// beyond it.
    #[test]
    fn the_last_unit_is_cut_at_the_copy_end() {
        let mut c = tracked(UNIT - 8, UNIT + 100);
        assert_eq!(c.plan(0, UNIT + 100, None), vec![(0, UNIT + 100)]);
        c.changed(UNIT + 99, UNIT + 100);
        assert_eq!(c.plan(0, UNIT + 100, None), vec![(UNIT + 8, UNIT + 100)]);
    }

    /// Given back, nothing is current; an untracked copy takes nothing here
    /// (the caller takes it whole).
    #[test]
    fn cleared_or_untracked_copies_have_nothing_current() {
        let mut c = tracked(0, 3 * UNIT);
        c.plan(0, 3 * UNIT, None);
        c.clear();
        c.changed(0, UNIT);
        assert_eq!(c.plan(0, UNIT, None), vec![]);
        c.track(0, 3 * UNIT);
        assert_eq!(c.plan(0, 3 * UNIT, None), vec![(0, 3 * UNIT)]);
    }

    /// The units found different are exactly those with a differing byte,
    /// whatever the address the slice starts at.
    #[test]
    fn differing_units_are_exactly_those_with_a_difference() {
        let len = 6 * UNIT + 9;
        let old: Vec<u8> = (0..len).map(|i| (i * 3) as u8).collect();
        for address in [0usize, 5, UNIT - 1, 3 * UNIT + 17] {
            let mut now = old.clone();
            for at in [0usize, UNIT + 1, 3 * UNIT, len - 1] {
                now[at] ^= 1;
            }
            let mut out = Vec::new();
            differing_units(address, &now, &old, 100, &mut out);
            // The same, unit by unit.
            let mut wanted: Vec<(usize, usize)> = Vec::new();
            let mut i = 0;
            while i < len {
                let n = (UNIT - (address + i) % UNIT).min(len - i);
                if now[i..i + n] != old[i..i + n] {
                    match wanted.last_mut() {
                        Some(l) if l.1 == 100 + i => l.1 += n,
                        _ => wanted.push((100 + i, 100 + i + n)),
                    }
                }
                i += n;
            }
            assert_eq!(out, wanted, "{address}");
            assert!(!out.is_empty());
        }
    }
}
