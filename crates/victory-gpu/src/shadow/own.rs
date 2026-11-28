//! What a piece of a copy needs to be left on the card (`crate::land::own`): who else may read its bytes from title
//! memory, and what it owes the copy's CPU side.

use super::{OverlapIndex, PAGE, Shadow};
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
    /// As [`OwnClass::Ownable`], but other copies of title memory lie over it, none of which ever took a byte of it
    /// (`Shadow::untaken`): their first refresh of those bytes reads title memory, which lands the piece first. Kept
    /// only with `VICTORY_LAND_OWN_UNTAKEN`, and checked again after the guard is armed.
    OwnableOver = 6,
}

/// How many classes there are.
pub(crate) const CLASSES: usize = 7;

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
        let mut over = false;
        if index.others(self.id, from, to) {
            let others = index.others_over(self.id, from, to);
            // (An index of spans alone names no copy: not known to be untaken.)
            if others.is_empty() || others.iter().any(|b| !b.untaken(from, to)) {
                return OwnClass::Overlapped;
            }
            over = true;
        }
        let (a, b) = (
            from as usize / 4096 * 4096,
            (to as usize).next_multiple_of(4096),
        );
        if !crate::inplace::private_whole(a, b - a) {
            return OwnClass::NotPrivate;
        }
        if over {
            OwnClass::OwnableOver
        } else {
            OwnClass::Ownable
        }
    }

    /// Whether this copy has taken no byte of title memory `[from, to)` into itself, and reads title memory again
    /// before it relies on any of them: what a copy over a kept piece must be, for the piece to stay on the card. Its
    /// next refresh of the bytes reads them through `crate::host::read`, which lands what is owed under them first.
    ///
    /// Not so (and `false`): a copy placed in title memory (the card reads the bytes itself), a copy shaders write
    /// (its landing writes the same bytes), a page of it taken (`copied`), a range taken whole (`ranged`, `quiet`).
    /// A copy in memory the kernel does not watch takes its range whole at every refresh and is as untaken as one
    /// that was never asked for, unless its quiet memo says it stopped.
    pub(crate) fn untaken(&self, from: u64, to: u64) -> bool {
        if self.place.is_some() || self.written.load(Ordering::Relaxed) {
            return false;
        }
        let (s, e) = (
            from.max(self.start) as usize,
            to.min(self.start + self.len) as usize,
        );
        if s >= e {
            return true;
        }
        if self
            .quiet
            .lock()
            .unwrap_or_else(|x| x.into_inner())
            .is_some_and(|(a, b, _)| a < e && b > s)
        {
            return false;
        }
        if self
            .ranged
            .lock()
            .unwrap_or_else(|x| x.into_inner())
            .iter()
            .any(|r| r.2 != 0 && r.0 < e && r.1 > s)
        {
            return false;
        }
        let base = self.start as usize / PAGE;
        let copied = self.copied.lock().unwrap_or_else(|x| x.into_inner());
        !(s / PAGE..=(e - 1) / PAGE).any(|page| copied.get(page - base).is_some_and(|&at| at != 0))
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    const BLOCK: usize = 64 << 10;

    /// A copy of title memory `[start, start + len)` with no host buffer (its CPU side is `side`'s).
    fn copy(start: usize, len: usize, side: &mut Vec<u8>) -> Arc<Shadow> {
        side.resize(len, 0);
        Shadow::unbacked(start as u64, side.as_mut_ptr(), len as u64, true)
    }

    /// A copy that nothing took is untaken; one that took a block, a range or is shaders' to write is not, and a
    /// copy placed in title memory never is (the card reads the bytes itself).
    #[test]
    fn what_a_copy_took_is_what_it_must_read_again() {
        let (mut side, start) = (Vec::new(), 0x6000_0000usize);
        let b = copy(start, 4 * BLOCK, &mut side);
        let (from, to) = (start as u64, (start + 4 * BLOCK) as u64);
        assert!(b.untaken(from, to));
        // Bytes outside the copy are nothing to it.
        assert!(b.untaken(to + 10, to + 100));
        b.copied.lock().unwrap()[1] = 7;
        assert!(!b.untaken(from, to));
        assert!(
            b.untaken(from, from + BLOCK as u64),
            "the first block was never taken"
        );
        assert!(!b.untaken(from + BLOCK as u64 + 5, from + BLOCK as u64 + 6));
        b.copied.lock().unwrap()[1] = 0;
        assert!(b.untaken(from, to));
        b.ranged.lock().unwrap()[0] = (start + 100, start + 200, 5);
        assert!(!b.untaken(from + 150, from + 160));
        assert!(b.untaken(from + 300, from + 400));
        b.ranged.lock().unwrap()[0] = (0, 0, 0);
        *b.quiet.lock().unwrap() = Some((start, start + BLOCK, 3));
        assert!(!b.untaken(from, from + 8));
        *b.quiet.lock().unwrap() = None;
        assert!(b.untaken(from, to));
        b.written.store(true, Ordering::Relaxed);
        assert!(
            !b.untaken(from, to),
            "shaders write it: its landing writes the same bytes"
        );
    }

    /// A piece under no other copy is ownable; under copies that never took it, ownable over them; under a copy
    /// that took it, overlapped; a copy the title was seen to read is not ownable at all.
    #[test]
    fn a_piece_is_classed_by_the_copies_over_it() {
        // SAFETY: memory of ours, released below.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                4 * BLOCK,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        let (mut a_side, mut b_side) = (Vec::new(), Vec::new());
        let a = copy(at, 4 * BLOCK, &mut a_side);
        a.gpu_only.store(true, Ordering::Release);
        let alone = OverlapIndex::of_copies(&[a.clone()]);
        assert_eq!(a.own_class(0, BLOCK, &alone), OwnClass::Ownable);
        // Another copy over the second block only.
        let b = copy(at + BLOCK, BLOCK, &mut b_side);
        let both = OverlapIndex::of_copies(&[a.clone(), b.clone()]);
        assert_eq!(
            a.own_class(0, BLOCK, &both),
            OwnClass::Ownable,
            "the first block has no other copy"
        );
        assert_eq!(
            a.own_class(BLOCK, 2 * BLOCK, &both),
            OwnClass::OwnableOver,
            "b never took it"
        );
        b.copied.lock().unwrap()[0] = 9;
        assert_eq!(
            a.own_class(BLOCK, 2 * BLOCK, &both),
            OwnClass::Overlapped,
            "b took it"
        );
        // An index of spans alone names no copy: overlapped, whatever it says.
        let spans = OverlapIndex::of(vec![(
            at as u64 + BLOCK as u64,
            at as u64 + 2 * BLOCK as u64,
            77,
        )]);
        assert_eq!(a.own_class(BLOCK, 2 * BLOCK, &spans), OwnClass::Overlapped);
        // Not a buffer the title's CPU cannot write; seen read; forced often.
        let c = copy(at + 2 * BLOCK, BLOCK, &mut b_side);
        assert_eq!(c.own_class(0, BLOCK, &alone), OwnClass::NotGpuOnly);
        a.note_title_read();
        assert_eq!(a.own_class(0, BLOCK, &alone), OwnClass::Read);
        // SAFETY: the allocation made above.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
        // Memory that is not private: not guardable (nothing there is committed at all).
        let e = copy(0x1000, BLOCK, &mut b_side);
        e.gpu_only.store(true, Ordering::Release);
        assert_eq!(
            e.own_class(0, BLOCK, &OverlapIndex::of_copies(&[e.clone()])),
            OwnClass::NotPrivate
        );
    }
}
