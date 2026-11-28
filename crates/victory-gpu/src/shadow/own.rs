//! What a piece of a copy needs to be left on the card (`crate::land::own`): who else may read its bytes from title
//! memory, and what it owes the copy's CPU side.

use super::{OverlapIndex, Shadow};
use std::sync::atomic::Ordering;

/// What a fetched piece is to a lazy landing, in the order the counters print.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum OwnClass {
    /// May be left on the card: nobody but the title's CPU can read its bytes from title memory, and the title was not
    /// seen to.
    Ownable = 0,
    /// Another copy of title memory covers some of its bytes: that copy is refreshed from title memory.
    Overlapped = 1,
    /// The title's CPU was seen to read the copy.
    Read = 2,
    /// Its title memory is not private, committed, read-write memory (a view of pages, memory the title protects): it
    /// cannot be guarded.
    NotPrivate = 3,
    /// Not a copy in the card's memory of a buffer in a default heap: the title's CPU writes such buffers.
    NotGpuOnly = 4,
    /// Our own readers forced its pieces to land so often that keeping them is not worth the guards.
    Forced = 5,
}

/// How many classes there are.
pub(crate) const CLASSES: usize = 6;

/// How many landings our own readers (a refresh, a copy of the title's) may force of a copy's kept pieces before it
/// lands eagerly for good (`VICTORY_LAND_OWN_FORCED`, 8).
fn forced_limit() -> u32 {
    static MOST: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_LAND_OWN_FORCED")
            .ok()
            .and_then(|v| v.trim().parse().ok())
            .unwrap_or(8)
    })
}

impl Shadow {
    /// What the piece at offsets `[lo, hi)` of this copy is to a lazy landing. `index`: the copies there are
    /// ([`super::overlap_index`]).
    pub(crate) fn own_class(&self, lo: usize, hi: usize, index: &OverlapIndex) -> OwnClass {
        if !self.unwatched_landing() {
            return OwnClass::NotGpuOnly;
        }
        if self.title_read() {
            return OwnClass::Read;
        }
        if self.own_forced.load(Ordering::Relaxed) >= forced_limit() {
            return OwnClass::Forced;
        }
        let (from, to) = (self.start + lo as u64, self.start + hi as u64);
        if index.others(self.id, from, to) {
            return OwnClass::Overlapped;
        }
        let (a, b) = (
            from as usize / 4096 * 4096,
            (to as usize).next_multiple_of(4096),
        );
        if !crate::inplace::private_whole(a, b - a) {
            return OwnClass::NotPrivate;
        }
        OwnClass::Ownable
    }

    /// A reader of ours (not the title's) made a kept piece of this copy land.
    pub(crate) fn note_own_forced(&self) {
        self.own_forced.fetch_add(1, Ordering::Relaxed);
    }

    /// How many landings of this copy our readers forced.
    #[cfg(test)]
    pub(crate) fn own_forced_count(&self) -> u32 {
        self.own_forced.load(Ordering::Relaxed)
    }

    /// The offsets of `[lo, hi)` that title bytes hold now that the copy's CPU side has and the card may not: what
    /// is to go up, and what went up after the send numbered `fetched_after`. A piece landed later is not put over
    /// them (`Shadow::land_card_skipping`).
    pub(crate) fn owe_skips(
        &self,
        lo: usize,
        hi: usize,
        fetched_after: u64,
    ) -> Vec<(usize, usize)> {
        let Some(card) = &self.card else {
            return Vec::new();
        };
        let dirty = card.dirty.lock().unwrap_or_else(|e| e.into_inner());
        let sent = card.sent.lock().unwrap_or_else(|e| e.into_inner());
        let mut skip: Vec<(usize, usize)> = dirty
            .iter()
            .copied()
            .filter(|&(a, b)| a < hi && b > lo)
            .collect();
        sent.near(fetched_after, lo, hi, &mut skip);
        skip
    }
}
