//! The title's write faults on guarded view memory, taken in fewer and cheaper
//! trips (all off unless asked for, and live in the A/B):
//!
//! - `VICTORY_FAULT_AROUND=<n>`: a fault opens, with the block that faulted,
//!   up to `n - 1` blocks beside it that are read-only by our hand and that
//!   the title wrote in this frame or the one before ([`plan`]), in one change of
//!   protection ([`runs_of`]). `1` only batches the blocks an opening has
//!   (a kernel write's range); `0` and unset: one fault, one block, as before.
//! - `VICTORY_FAULT_SPIN=<microseconds>`: a title thread that faults on a
//!   stripe another thread holds looks for it again for that long before it
//!   sleeps on it ([`spin_for`]).
//! - `VICTORY_ALIAS_BATCH=1`: the other views of a run of blocks are made
//!   read-only by one change of protection for each run of them that lies
//!   side by side (`super::cover_batched`).
//!
//! Why. A frame of the world has some 316 write faults on guarded blocks, and
//! each costs a change of protection (24 us on average, with all the
//! process's page faults and protection changes queueing for the same kernel
//! locks), a trip through the guards' stripes (190 us of waiting on average
//! for those that had to wait, 210 a frame) and our handler's own work. The
//! blocks that fault are nearly the same ones every frame, in runs: the title
//! writes the constants and the records of its lists in arrays. A block opened
//! beside the one that faulted is told as written at the next asking whether
//! it was written or not (it costs a copy of 64 KiB when it was not), and is
//! guarded again then, so nothing is missed.

use std::cell::Cell;
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The switches, for a test that sets them for the thread it runs on.
#[cfg(test)]
#[derive(Clone, Copy)]
pub(crate) struct Lean {
    pub(crate) around: usize,
    pub(crate) spin_us: u64,
    pub(crate) batch: bool,
}

#[cfg(test)]
impl Lean {
    pub(crate) const OFF: Lean = Lean {
        around: 0,
        spin_us: 0,
        batch: false,
    };
}

#[cfg(test)]
thread_local! {
    /// What the switches are for the faults and askings of this thread.
    pub(crate) static LEAN_IN_TEST: Cell<Lean> = const { Cell::new(Lean::OFF) };
}

#[cfg(test)]
thread_local! {
    /// The memory (block ranges, past the last) a test's threads own: a fault opens beside its block only blocks of
    /// these (none given: any). The tests of the guards run side by side in one process, their views lie next to each
    /// other, each thread works in its own mode of the guards (stripes, the one lock) and the block bits are
    /// global: a fault that opened the hot blocks beside its own, in another test's view, would do so under locks that
    /// test's threads do not hold, and race with them (a block made read-only and noted, or opened, by both: noted
    /// guarded and clean while writable, so that the title's writes are not told). In the title all threads share
    /// one mode (changed only through the gate), so there the blocks beside are always under the same locks.
    pub(crate) static OWN_IN_TEST: std::cell::RefCell<Vec<(usize, usize)>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether a fault may open `block` beside its own (in a test: only memory the test owns).
fn may_open(block: usize) -> bool {
    #[cfg(test)]
    {
        OWN_IN_TEST.with(|own| {
            let own = own.borrow();
            own.is_empty() || own.iter().any(|&(first, past)| (first..past).contains(&block))
        })
    }
    #[cfg(not(test))]
    {
        let _ = block;
        true
    }
}

/// `VICTORY_FAULT_AROUND`: the most blocks a fault opens (0: one).
pub(super) fn around() -> usize {
    #[cfg(test)]
    {
        LEAN_IN_TEST.get().around
    }
    #[cfg(not(test))]
    {
        crate::ab::live_value!("VICTORY_FAULT_AROUND", usize, |v| {
            v.and_then(|s| s.trim().parse::<usize>().ok())
                .unwrap_or(0)
                .min(super::UNIT)
        })
    }
}

/// `VICTORY_FAULT_SPIN`: microseconds a faulting thread looks for a stripe before it sleeps on it.
pub(super) fn spin_us() -> u64 {
    #[cfg(test)]
    {
        LEAN_IN_TEST.get().spin_us
    }
    #[cfg(not(test))]
    {
        crate::ab::live_value!("VICTORY_FAULT_SPIN", u64, |v| {
            v.and_then(|s| s.trim().parse::<u64>().ok())
                .unwrap_or(0)
                .min(1000)
        })
    }
}

/// `VICTORY_ALIAS_BATCH`: the other views of a run of blocks are guarded in one call for each run of them.
pub(super) fn alias_batch() -> bool {
    #[cfg(test)]
    {
        LEAN_IN_TEST.get().batch
    }
    #[cfg(not(test))]
    {
        crate::ab::live_flag!("VICTORY_ALIAS_BATCH")
    }
}

thread_local! {
    /// Whether this thread is in the write-fault handler.
    static FAULTING: Cell<bool> = const { Cell::new(false) };
}

/// This thread is in the fault handler until it is dropped.
pub(super) struct Faulting(bool);

impl Faulting {
    pub(super) fn enter() -> Faulting {
        Faulting(FAULTING.replace(true))
    }
}

impl Drop for Faulting {
    fn drop(&mut self) {
        FAULTING.set(self.0);
    }
}

/// Whether this thread is in the fault handler.
pub(super) fn faulting() -> bool {
    FAULTING.get()
}

/// The stripe `lock`, if it is free within [`spin_us`] of looking for it again and again; none if the thread is
/// not in the fault handler, or the time is up (it sleeps on the lock then, as before).
pub(super) fn spin_for(lock: &'static Mutex<()>) -> Option<MutexGuard<'static, ()>> {
    if !faulting() {
        return None;
    }
    let us = spin_us();
    if us == 0 {
        return None;
    }
    let began = Instant::now();
    let limit = Duration::from_micros(us);
    loop {
        for _ in 0..32 {
            std::hint::spin_loop();
        }
        if let Ok(guard) = lock.try_lock() {
            super::faultstat::spun(true, began.elapsed());
            return Some(guard);
        }
        if began.elapsed() >= limit {
            super::faultstat::spun(false, began.elapsed());
            return None;
        }
    }
}

/// Slots of [`HEAT`]: a block's is its number modulo this (blocks that share one only make each other seem hotter).
const SLOTS: usize = 1 << 18;

/// Per block, the frame a write fault last opened it (or one beside it did, [`kept`]) and how many frames in a row it
/// was kept so: the frame, plus one, in the upper 24 bits (0: never), the streak in the lower 8.
static HEAT: [AtomicU32; SLOTS] = [const { AtomicU32::new(0) }; SLOTS];

/// How many frames in a row a block may be kept hot by the faults of those beside it, without a fault of its own.
const STREAK_MAX: u32 = 3;
const FRAME_MASK: u32 = 0x00FF_FFFF;

fn pack(frame: u64, streak: u32) -> u32 {
    ((frame.wrapping_add(1) as u32 & FRAME_MASK) << 8) | streak.min(0xFF)
}

/// How many frames before `frame` the entry was made (none: never).
fn age_of(entry: u32, frame: u64) -> Option<u32> {
    if entry == 0 {
        return None;
    }
    let now = frame.wrapping_add(1) as u32 & FRAME_MASK;
    Some(now.wrapping_sub(entry >> 8) & FRAME_MASK)
}

fn slot(block: usize) -> &'static AtomicU32 {
    &HEAT[block % SLOTS]
}

/// A write fault opened `block` in frame `frame`: how many frames before that something was noted of it (none:
/// never).
pub(super) fn faulted(block: usize, frame: u64) -> Option<u32> {
    age_of(slot(block).swap(pack(frame, 0), Relaxed), frame)
}

/// How many frames before `frame` `block` was opened by a fault of its own or kept hot (none: never).
pub(super) fn age(block: usize, frame: u64) -> Option<u32> {
    age_of(slot(block).load(Relaxed), frame)
}

/// Whether `block` was opened in this frame or the one before.
pub(super) fn hot(block: usize, frame: u64) -> bool {
    age(block, frame).is_some_and(|age| age <= 1)
}

/// `block` was opened in frame `frame` with the one that faulted. It stays hot for [`STREAK_MAX`] frames more at
/// most, without a fault of its own: a block the title no longer writes is not opened for ever.
pub(super) fn kept(block: usize, frame: u64) {
    let slot = slot(block);
    let entry = slot.load(Relaxed);
    if age_of(entry, frame).is_some_and(|age| age <= 1) && entry & 0xFF < STREAK_MAX {
        slot.store(pack(frame, (entry & 0xFF) + 1), Relaxed);
    }
}

/// The blocks `[first, past)` a fault on `block` opens: itself, and the hot ones that are read-only by our hand
/// beside it (after it first), `around() - 1` of them at most.
pub(super) fn plan(block: usize, frame: u64) -> (usize, usize) {
    let n = around();
    // (A block that is not read-only by our hand, another thread opened it: nothing beside it is the title's to be
    // opened with it.)
    if n < 2 || !super::PROTECTED.any(block, block + 1) {
        return (block, block + 1);
    }
    let mut extra = n - 1;
    let (mut first, mut past) = (block, block + 1);
    while extra > 0 && may_open(past) && super::PROTECTED.any(past, past + 1) && hot(past, frame) {
        past += 1;
        extra -= 1;
    }
    while extra > 0 && first > 0 && may_open(first - 1) && super::PROTECTED.any(first - 1, first) && hot(first - 1, frame)
    {
        first -= 1;
        extra -= 1;
    }
    (first, past)
}

/// `blocks` as runs of blocks side by side, `(first, past)`, in order (a block named twice is one).
pub(super) fn runs_of(blocks: &[usize]) -> Vec<(usize, usize)> {
    let mut sorted = blocks.to_vec();
    sorted.sort_unstable();
    sorted.dedup();
    let mut runs: Vec<(usize, usize)> = Vec::new();
    for block in sorted {
        match runs.last_mut() {
            Some(run) if run.1 == block => run.1 += 1,
            _ => runs.push((block, block + 1)),
        }
    }
    runs
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_side_by_side_are_one_run_and_a_block_named_twice_is_one() {
        assert!(runs_of(&[]).is_empty());
        assert_eq!(
            runs_of(&[7, 5, 6, 9, 5, 12, 11]),
            [(5, 8), (9, 10), (11, 13)]
        );
    }

    #[test]
    fn a_block_is_hot_this_frame_and_the_next_and_kept_for_a_few_frames_without_a_fault() {
        // Blocks no memory has.
        let block = 0x7000_0123usize;
        assert_eq!(age(block, 100), None);
        assert!(!hot(block, 100));
        assert_eq!(faulted(block, 100), None);
        assert_eq!(age(block, 100), Some(0));
        assert!(hot(block, 100) && hot(block, 101) && !hot(block, 102));
        // Opened with a neighbour in the next frames: kept up to the streak, then it cools.
        let mut frame = 101;
        let mut kept_frames = 0u32;
        while hot(block, frame) && kept_frames < 20 {
            kept(block, frame);
            frame += 1;
            kept_frames += 1;
        }
        assert_eq!(
            kept_frames,
            STREAK_MAX + 1,
            "kept {STREAK_MAX} frames beyond the one it was hot in"
        );
        assert!(!hot(block, frame));
        // A fault of its own makes it hot again, with the age of the last note.
        assert_eq!(faulted(block, frame + 5), Some(7));
        assert!(hot(block, frame + 5));
    }

    #[test]
    fn an_empty_entry_and_a_wrapped_frame_counter_have_ages() {
        assert_eq!(age_of(0, 5), None);
        assert_eq!(age_of(pack(5, 0), 5), Some(0));
        assert_eq!(age_of(pack(5, 2), 8), Some(3));
        // The counter in the entry has 24 bits.
        let near_top = u64::from(FRAME_MASK) - 1;
        assert_eq!(age_of(pack(near_top, 0), near_top + 3), Some(3));
    }
}
