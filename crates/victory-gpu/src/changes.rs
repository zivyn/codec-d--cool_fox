//! A log of the recent changes of what asks about title memory are answered by, each with the address range it
//! changed (`VICTORY_RANGE_VALID=1`, see `crate::shadow`).
//!
//! What a cache keeps of an answer holds as long as nothing that could change the answer has changed: with one number for
//! everything that moved (`shadow::world`, 1.2 to 1.7 times a frame) every thread made its views and its asks again after
//! each move, though a move is of one buffer, window or copy of a view's own bytes. Here a cache stamps what it keeps with
//! how many changes were logged when it was made, and asks whether any of the changes logged since lies in the address
//! range the answer depends on.
//!
//! The log keeps the last [`SLOTS`] changes; a stamp older than that is "changed" (the cache makes it again). A change is
//! logged by whoever makes it, under the lock that makes it, and for a let-go of what nobody holds before it looks at who
//! holds it (the order `crate::asked` relies on).

use std::sync::atomic::{AtomicU64, Ordering};

const SLOTS: usize = 64;

struct Slot {
    /// The number of the change in this slot (1, 2, ...), 0 for none.
    seq: AtomicU64,
    lo: AtomicU64,
    hi: AtomicU64,
}

pub(crate) struct ChangeLog {
    /// Changes logged so far.
    next: AtomicU64,
    slots: [Slot; SLOTS],
}

impl ChangeLog {
    pub(crate) const fn new() -> Self {
        ChangeLog {
            next: AtomicU64::new(0),
            slots: [const { Slot { seq: AtomicU64::new(0), lo: AtomicU64::new(0), hi: AtomicU64::new(0) } }; SLOTS],
        }
    }

    /// What a cache stamps with: read it before the ask whose answer it keeps.
    pub(crate) fn stamp(&self) -> u64 {
        self.next.load(Ordering::SeqCst)
    }

    /// A change of what is answered for addresses `[lo, hi)`.
    pub(crate) fn log(&self, lo: u64, hi: u64) {
        let n = self.next.fetch_add(1, Ordering::SeqCst) + 1;
        let slot = &self.slots[n as usize % SLOTS];
        // (Not the number of the change that was here until the range is: a reader that finds another number takes
        // the change for one it cannot tell, which it counts as a change of its range.)
        slot.seq.store(0, Ordering::SeqCst);
        slot.lo.store(lo, Ordering::SeqCst);
        slot.hi.store(hi, Ordering::SeqCst);
        slot.seq.store(n, Ordering::SeqCst);
    }

    /// Whether a change logged after `stamp` lies in `[lo, hi)` (or cannot be told: logged just now, or too long ago).
    pub(crate) fn changed_since(&self, stamp: u64, lo: u64, hi: u64) -> bool {
        let now = self.next.load(Ordering::SeqCst);
        if now == stamp {
            return false;
        }
        if now < stamp || now - stamp > SLOTS as u64 {
            return true;
        }
        (stamp + 1..=now).any(|n| {
            let slot = &self.slots[n as usize % SLOTS];
            if slot.seq.load(Ordering::SeqCst) != n {
                return true;
            }
            let (from, to) = (slot.lo.load(Ordering::SeqCst), slot.hi.load(Ordering::SeqCst));
            // (Read as a whole: if the slot was taken by a later change meanwhile, it is not told.)
            slot.seq.load(Ordering::SeqCst) != n || (lo < to && from < hi)
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_a_change_that_overlaps_the_range_changes_it() {
        let log = ChangeLog::new();
        let before = log.stamp();
        assert!(!log.changed_since(before, 100, 200));
        log.log(500, 600);
        // Outside: before it, after it, touching its ends.
        assert!(!log.changed_since(before, 100, 200));
        assert!(!log.changed_since(before, 600, 700));
        assert!(!log.changed_since(before, 400, 500));
        // Inside, over, across an end.
        assert!(log.changed_since(before, 550, 560));
        assert!(log.changed_since(before, 0, 1000));
        assert!(log.changed_since(before, 599, 700));
        assert!(log.changed_since(before, 450, 501));
        // What was stamped after is not changed by it.
        assert!(!log.changed_since(log.stamp(), 550, 560));
        // Several changes since the stamp: any of them.
        let was = log.stamp();
        log.log(10, 20);
        log.log(900, 950);
        assert!(log.changed_since(was, 15, 16));
        assert!(log.changed_since(was, 940, 960));
        assert!(!log.changed_since(was, 20, 900));
    }

    #[test]
    fn a_stamp_older_than_the_log_is_changed() {
        let log = ChangeLog::new();
        let was = log.stamp();
        for k in 0..SLOTS as u64 {
            log.log(k * 10, k * 10 + 1);
        }
        // All of them are still there: the range of the first ones tells.
        assert!(!log.changed_since(was, 10_000, 10_001));
        assert!(log.changed_since(was, 0, 1));
        log.log(5000, 5001);
        // One too many: nothing can be told.
        assert!(log.changed_since(was, 10_000, 10_001));
        // A recent stamp is as good as before.
        let recent = log.stamp();
        log.log(7, 8);
        assert!(!log.changed_since(recent, 10_000, 10_001));
        assert!(log.changed_since(recent, 7, 8));
    }

    /// Threads that log changes while another logs and asks: a change logged after a stamp is never missed for its own
    /// range (it may be said of others'), and nothing panics.
    #[test]
    fn a_change_is_never_missed_while_threads_log() {
        let log = ChangeLog::new();
        let stop = std::sync::atomic::AtomicBool::new(false);
        std::thread::scope(|s| {
            for t in 0..2u64 {
                let (log, stop) = (&log, &stop);
                s.spawn(move || {
                    let mut i = 0u64;
                    while !stop.load(Ordering::SeqCst) {
                        let k = (i + t) % 3;
                        log.log(k * 1000, k * 1000 + 1000);
                        i += 1;
                        std::thread::yield_now();
                    }
                });
            }
            let (log, stop) = (&log, &stop);
            s.spawn(move || {
                for i in 0..5000u64 {
                    let stamp = log.stamp();
                    let (lo, hi) = (10_000 + (i % 7) * 100, 10_000 + (i % 7) * 100 + 50);
                    log.log(lo, hi);
                    assert!(log.changed_since(stamp, lo + 10, lo + 20), "change {i}");
                }
                stop.store(true, Ordering::SeqCst);
            });
        });
        // Quiet: ranges of the three of them are changed since the beginning, one nobody logged is told only for a stamp
        // too old to tell.
        let now = log.stamp();
        log.log(40_000, 41_000);
        assert!(!log.changed_since(now, 5000, 6000));
        assert!(log.changed_since(now, 40_500, 40_600));
    }
}
