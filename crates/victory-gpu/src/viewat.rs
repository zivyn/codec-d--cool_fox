//! A thread's own table of the views the records of descriptors name, so that the list threads do not take the one lock
//! of the table of views ([`crate::descriptor`]) for each entry of each table of each draw (`VICTORY_VIEW_AT_CACHE=1`).
//!
//! `descriptor::view_at(i, generation)` is read some 47,000 times a frame by the eight threads that translate lists,
//! each time through a reader-writer lock whose count of readers is one word they all change (their own cores take it
//! from each other, whatever the lock's work), and written by the title a few times a frame. What an entry names does not
//! change while its generation is the one in a record: only an entry that is freed (the title released its resource:
//! the generation goes up, and the views that name it are stale) or one that is made another view when it is used
//! again. Each of those is written to a short log under the lock that makes it ([`Log::changed`]); a thread that
//! looks, first takes in what was logged since its last look and drops those entries from its table, or all of it if
//! it was more than the log holds behind.
//!
//! The entries are by index (a slot of 1,024 for the index's hash), the generation is part of what a slot answers for.

use crate::descriptor::View;
use std::sync::atomic::{
    AtomicU64,
    Ordering::{Acquire, Relaxed, Release},
};

/// What the log holds: the changes that a thread can be behind by.
const LOG: usize = 256;
/// A thread's slots.
const SLOTS: usize = 1024;

/// The entries of the table of views that were freed or made another view, newest `LOG` of them.
pub(crate) struct Log {
    /// How many changes were logged since the start.
    seq: AtomicU64,
    /// Change number `n`: its low 32 bits, then the entry's index.
    entries: [AtomicU64; LOG],
}

impl Log {
    pub(crate) const fn new() -> Log {
        Log {
            seq: AtomicU64::new(0),
            entries: [const { AtomicU64::new(0) }; LOG],
        }
    }

    /// Entry `i` was freed or made another view. Only one thread at a time may call this (the table's lock, held for
    /// writing, does); the entry's new state must be in place before it is called.
    pub(crate) fn changed(&self, i: u32) {
        let seq = self.seq.load(Relaxed) + 1;
        self.entries[seq as usize % LOG].store((seq & 0xFFFF_FFFF) << 32 | u64::from(i), Release);
        self.seq.store(seq, Release);
    }
}

/// The views one thread has read from the table, by the index of their entry.
pub(crate) struct Near {
    /// The change last taken in.
    seen: u64,
    /// Index, generation and the view of each slot.
    slots: Vec<(u32, u32, Option<View>)>,
}

impl Near {
    pub(crate) const fn new() -> Near {
        Near {
            seen: 0,
            slots: Vec::new(),
        }
    }

    fn slot(i: u32) -> usize {
        (i.wrapping_mul(0x9E37_79B1) >> 22) as usize % SLOTS
    }

    /// Takes in the changes logged since the last look.
    fn sync(&mut self, log: &Log) {
        let now = log.seq.load(Acquire);
        if now == self.seen {
            return;
        }
        if self.slots.is_empty() {
            self.seen = now;
            return;
        }
        if now - self.seen > LOG as u64 {
            self.slots.clear();
            self.seen = now;
            return;
        }
        for seq in self.seen + 1..=now {
            let logged = log.entries[seq as usize % LOG].load(Acquire);
            if logged >> 32 != seq & 0xFFFF_FFFF {
                // Written over by a later change meanwhile (or not yet written): all of it goes.
                self.slots.clear();
                break;
            }
            let i = logged as u32;
            if let Some(slot) = self.slots.get_mut(Self::slot(i))
                && slot.0 == i
            {
                slot.2 = None;
            }
        }
        self.seen = now;
    }

    /// The view of entry `i` at `generation` this thread read before, if no change to the entry was logged since. Call
    /// it before reading the table: what changes after it is taken in at the next call.
    pub(crate) fn get(&mut self, log: &Log, i: u32, generation: u32) -> Option<View> {
        self.sync(log);
        match self.slots.get(Self::slot(i)) {
            Some((at, was, Some(view))) if *at == i && *was == generation => Some(view.clone()),
            _ => None,
        }
    }

    /// `view` was read from the table for entry `i` at `generation`, after the last [`Near::get`].
    pub(crate) fn put(&mut self, i: u32, generation: u32, view: &View) {
        if self.slots.is_empty() {
            self.slots.resize(SLOTS, (0, 0, None));
        }
        let at = Self::slot(i);
        self.slots[at] = (i, generation, Some(view.clone()));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn view(n: u32) -> View {
        View::Constant(u64::from(n) * 256, 64)
    }

    #[test]
    fn a_view_read_is_answered_again_for_its_generation_only() {
        let (log, mut near) = (Log::new(), Near::new());
        assert_eq!(near.get(&log, 7, 3), None);
        near.put(7, 3, &view(1));
        assert_eq!(near.get(&log, 7, 3), Some(view(1)));
        // Another generation of the entry is another question (and a stale one is never answered).
        assert_eq!(near.get(&log, 7, 4), None);
        assert_eq!(near.get(&log, 8, 3), None);
    }

    #[test]
    fn a_change_to_an_entry_drops_that_entry_and_no_other() {
        let (log, mut near) = (Log::new(), Near::new());
        // Two entries that do not share a slot.
        let (a, b) = (
            5u32,
            (6..64).find(|&b| Near::slot(b) != Near::slot(5)).unwrap(),
        );
        assert_eq!(near.get(&log, a, 1), None);
        near.put(a, 1, &view(1));
        near.put(b, 1, &view(2));
        log.changed(a);
        assert_eq!(near.get(&log, a, 1), None);
        assert_eq!(near.get(&log, b, 1), Some(view(2)));
        // Read again after the change: answered again, and until the next change.
        near.put(a, 2, &view(3));
        assert_eq!(near.get(&log, a, 2), Some(view(3)));
        log.changed(a);
        assert_eq!(near.get(&log, a, 2), None);
        // A change to an entry the thread never read costs it nothing.
        log.changed(b + 1000);
        assert_eq!(near.get(&log, b, 1), Some(view(2)));
    }

    #[test]
    fn a_thread_that_fell_behind_by_more_than_the_log_holds_begins_again() {
        let (log, mut near) = (Log::new(), Near::new());
        let _ = near.get(&log, 1, 1);
        near.put(1, 1, &view(1));
        for k in 0..(LOG as u32 + 10) {
            log.changed(10_000 + k);
        }
        assert_eq!(
            near.get(&log, 1, 1),
            None,
            "it cannot tell what it missed: all of it goes"
        );
        near.put(1, 1, &view(1));
        assert_eq!(near.get(&log, 1, 1), Some(view(1)));
        // Fewer than the log holds behind: only what was named goes.
        for k in 0..(LOG as u32 - 1) {
            log.changed(20_000 + k);
        }
        assert_eq!(near.get(&log, 1, 1), Some(view(1)));
        log.changed(1);
        assert_eq!(near.get(&log, 1, 1), None);
    }

    /// Whatever is said of an entry while another thread changes it: a thread never answers with a view that was
    /// replaced before its `get` began.
    #[test]
    fn a_reader_never_answers_with_a_view_replaced_before_it_asked() {
        use std::sync::{Arc, RwLock};
        let table = Arc::new(RwLock::new(1u32));
        let log = Arc::new(Log::new());
        let writes = Arc::new(AtomicU64::new(0));
        let writer = {
            let (table, log, writes) = (table.clone(), log.clone(), writes.clone());
            std::thread::spawn(move || {
                for n in 2..2000u32 {
                    let mut t = table.write().unwrap();
                    *t = n;
                    log.changed(9);
                    drop(t);
                    writes.store(u64::from(n), Release);
                }
            })
        };
        let mut near = Near::new();
        while writes.load(Acquire) < 1999 {
            // What the table held when the ask began is the least the answer may be.
            let floor = *table.read().unwrap();
            let got = match near.get(&log, 9, 1) {
                Some(v) => v,
                None => {
                    let n = *table.read().unwrap();
                    let v = view(n);
                    near.put(9, 1, &v);
                    v
                }
            };
            assert!(got >= view(floor), "{got:?} is older than {floor}");
        }
        writer.join().unwrap();
    }
}
