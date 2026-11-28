//! A whole copy of a buffer nobody's CPU writes takes only what differs
//! (`VICTORY_QUIET_COMPARE`).
//!
//! A copy in the card's memory of a buffer in a default heap (the title's CPU
//! cannot write it: `Shadow::mark_gpu_only`) skips its whole refreshes while
//! nothing is told written there (`Shadow::quiet_skip`). The note that says so
//! is void when anything is announced in one of the copy's 64 KiB blocks,
//! and in some processes it is void all the time (a frame line shows the
//! 21 MB working buffer of the upscaler "copied whole" 1.0x a frame and sent
//! up 21 MB a frame, in 8 of 25 windowed runs, where the others copy it
//! 0.0x): the refresh takes the buffer whole from title memory and the card
//! is sent all of it, 3 ms of the bus on the drawing queue and 4 to 6 ms of
//! threads, for bytes that are what the card wrote and its landing put in
//! title memory and in the copy's CPU side.
//!
//! Here the refresh reads title memory in windows and compares each 4 KiB
//! block with what the copy's CPU side holds; only the blocks that differ are
//! stored and noted for the card (`Shadow::card_changed`). A block that is the
//! same is a block the card holds already: the CPU side is what was last sent
//! or landed, outside the ranges shaders may be writing (a refresh never
//! looks there). Where title memory cannot be read (pages the runtime is
//! remapping) the window is taken whole, as before.

use std::cell::RefCell;

/// Since the last frame-rate line: bytes of whole copies taken by comparing, and those of them that differed
/// (and were stored and noted for the card).
static BYTES: [std::sync::atomic::AtomicU64; 2] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// `looked` bytes were compared, `taken` of them differed.
pub(crate) fn note(looked: usize, taken: usize) {
    BYTES[0].fetch_add(looked as u64, std::sync::atomic::Ordering::Relaxed);
    BYTES[1].fetch_add(taken as u64, std::sync::atomic::Ordering::Relaxed);
}

/// Since the last frame-rate line, of the long ranges `Shadow::quiet_skip` was asked about: refused for want of a note,
/// for a note that does not hold the range, for a write announced in its blocks since, for a window that differed,
/// and those left out.
static SKIPS: [std::sync::atomic::AtomicU64; 5] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// What came of a look at a long range for the skip: `why` is the place in [`SKIPS`] (4: left out).
pub(crate) fn skip_refused(why: usize) {
    SKIPS[why.min(4)].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// For the frame-rate line, over `frames` frames.
pub(crate) fn line(frames: f64) {
    let [none, short, stamped, differed, left] =
        [0, 1, 2, 3, 4].map(|i| SKIPS[i].swap(0, std::sync::atomic::Ordering::Relaxed));
    if none + short + stamped + differed + left > 0 {
        tracing::info!(
            "whole refreshes of long ranges of buffers no CPU writes, per frame: {:.2} left out, {:.2} taken for want of a note, {:.2} for a note that does not hold the range, {:.2} for a write announced in its blocks since the note, {:.2} for a window that differed",
            left as f64 / frames,
            none as f64 / frames,
            short as f64 / frames,
            stamped as f64 / frames,
            differed as f64 / frames
        );
    }
    let [looked, taken] = [0, 1].map(|i| BYTES[i].swap(0, std::sync::atomic::Ordering::Relaxed));
    if looked > 0 {
        tracing::info!(
            "whole copies of buffers no CPU writes, by comparing (VICTORY_QUIET_COMPARE) per frame: {:.2} MB looked at, {:.2} MB of them differed and went up",
            looked as f64 / 1e6 / frames,
            taken as f64 / 1e6 / frames
        );
    }
}

/// The unit that is compared and noted.
const BLOCK: usize = 4096;

/// The unit of work of a thread.
pub(crate) const PART: usize = 2 << 20;

/// Title memory is read this much at a time (it stays in the cache of the
/// thread that compares it).
const WINDOW: usize = 256 << 10;

/// Whether whole copies of such buffers take only what differs. May change
/// while the title runs (`VICTORY_AB=10,VICTORY_QUIET_COMPARE`): each whole
/// copy decides alone, and both ways leave the copy's CPU side holding title
/// memory.
pub(crate) fn on() -> bool {
    crate::ab::live_flag!("VICTORY_QUIET_COMPARE")
}

/// The runs of whole blocks of `held` that differ from `title` (the same
/// length), as offsets, in order, joined where they touch. The last block
/// may be short.
pub(crate) fn differing_blocks(held: &[u8], title: &[u8], out: &mut Vec<(usize, usize)>) {
    debug_assert_eq!(held.len(), title.len());
    let mut at = 0;
    while at < held.len() {
        let end = (at + BLOCK).min(held.len());
        if held[at..end] != title[at..end] {
            push_run(out, at, end);
        }
        at = end;
    }
}

/// `[from, to)` after the runs in `out`, joined with the last if it touches.
fn push_run(out: &mut Vec<(usize, usize)>, from: usize, to: usize) {
    match out.last_mut() {
        Some(last) if last.1 == from => last.1 = to,
        _ => out.push((from, to)),
    }
}

/// Takes title memory `[from, to)` into the copy's CPU side, `held` being the
/// copy's memory at `from`: only the blocks that differ are stored. Adds
/// their runs to `runs`, as offsets in the copy (`offset` is that of `from`),
/// in order and joined.
///
/// `read(address, buffer)` reads title memory, and says if all of it was
/// there; a window it could not read is taken whole by `take_whole`.
///
/// # Safety
/// `held` points to `to - from` bytes that nothing else reads or writes now.
pub(crate) unsafe fn take_differing(
    read: &dyn Fn(usize, &mut [u8]) -> bool,
    take_whole: &dyn Fn(usize, *mut u8, usize),
    held: *mut u8,
    from: usize,
    to: usize,
    offset: usize,
    runs: &mut Vec<(usize, usize)>,
) {
    thread_local! {
        static TITLE: RefCell<Vec<u8>> = const { RefCell::new(Vec::new()) };
    }
    TITLE.with_borrow_mut(|title| {
        let mut at = from;
        let mut found = Vec::new();
        while at < to {
            let n = WINDOW.min(to - at);
            title.resize(n, 0);
            // SAFETY: the caller's.
            let mine = unsafe { std::slice::from_raw_parts_mut(held.add(at - from), n) };
            if read(at, &mut title[..n]) {
                found.clear();
                differing_blocks(mine, &title[..n], &mut found);
                for &(a, b) in &found {
                    mine[a..b].copy_from_slice(&title[a..b]);
                    push_run(runs, offset + at - from + a, offset + at - from + b);
                }
            } else {
                take_whole(at, mine.as_mut_ptr(), n);
                push_run(runs, offset + at - from, offset + at - from + n);
            }
            at += n;
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(len: usize, seed: u64) -> Vec<u8> {
        let mut s = seed | 1;
        (0..len)
            .map(|_| {
                s ^= s << 13;
                s ^= s >> 7;
                s ^= s << 17;
                (s >> 24) as u8
            })
            .collect()
    }

    /// Reads from real memory, which is what the test's title memory is.
    fn read(address: usize, buffer: &mut [u8]) -> bool {
        // SAFETY: the tests give addresses of live vectors.
        unsafe {
            std::ptr::copy_nonoverlapping(address as *const u8, buffer.as_mut_ptr(), buffer.len())
        };
        true
    }

    fn whole(address: usize, to: *mut u8, n: usize) {
        // SAFETY: as above.
        unsafe { std::ptr::copy_nonoverlapping(address as *const u8, to, n) };
    }

    #[test]
    fn only_the_blocks_that_differ_are_found_and_joined() {
        let held = pattern(5 * BLOCK + 100, 7);
        let mut title = held.clone();
        let mut out = Vec::new();
        differing_blocks(&held, &title, &mut out);
        assert!(out.is_empty());
        title[BLOCK + 5] ^= 1;
        title[2 * BLOCK] ^= 1;
        title[4 * BLOCK + 99] ^= 1;
        title[5 * BLOCK + 99] ^= 1;
        differing_blocks(&held, &title, &mut out);
        // Blocks 1 and 2 touch; 4 and the short last one touch; 3 is the same.
        assert_eq!(out, vec![(BLOCK, 3 * BLOCK), (4 * BLOCK, 5 * BLOCK + 100)]);
    }

    #[test]
    fn what_differs_is_taken_and_the_rest_is_left_alone_over_windows_and_parts() {
        // Longer than two windows, the end not on a block.
        let len = 2 * WINDOW + 3 * BLOCK + 17;
        for seed in 1..40u64 {
            let title = pattern(len, seed);
            let mut held = title.clone();
            let rolls = pattern(12, seed + 100);
            // Change some blocks of the copy, some across the window's end.
            let mut changed = Vec::new();
            for r in rolls {
                let block = (r as usize * 131 + seed as usize * 7) % len.div_ceil(BLOCK);
                held[block * BLOCK] = title[block * BLOCK] ^ 0x5a;
                changed.push(block);
            }
            let before = held.clone();
            let mut runs = Vec::new();
            // SAFETY: the vector's own memory.
            unsafe {
                take_differing(
                    &read,
                    &whole,
                    held.as_mut_ptr(),
                    title.as_ptr() as usize,
                    title.as_ptr() as usize + len,
                    1000,
                    &mut runs,
                )
            };
            assert_eq!(held, title, "the copy holds title memory");
            // The runs are the blocks changed, as offsets in the copy (1000 on), in order, joined.
            let mut want = Vec::new();
            let mut blocks = changed.clone();
            blocks.sort_unstable();
            blocks.dedup();
            for b in blocks {
                push_run(
                    &mut want,
                    1000 + b * BLOCK,
                    1000 + ((b + 1) * BLOCK).min(len),
                );
            }
            assert_eq!(runs, want, "seed {seed}");
            // Nothing outside the runs was written.
            for (i, (&a, &b)) in before.iter().zip(&held).enumerate() {
                if a != b {
                    assert!(
                        runs.iter().any(|r| (r.0..r.1).contains(&(1000 + i))),
                        "byte {i} changed outside the runs"
                    );
                }
            }
        }
    }

    #[test]
    fn a_window_that_cannot_be_read_is_taken_whole() {
        let len = 3 * BLOCK;
        let title = pattern(len, 3);
        let mut held = title.clone();
        let mut runs = Vec::new();
        let none = |_: usize, _: &mut [u8]| false;
        // SAFETY: the vector's own memory.
        unsafe {
            take_differing(
                &none,
                &whole,
                held.as_mut_ptr(),
                title.as_ptr() as usize,
                title.as_ptr() as usize + len,
                0,
                &mut runs,
            )
        };
        assert_eq!(runs, vec![(0, len)]);
        assert_eq!(held, title);
    }

    #[test]
    fn a_title_memory_that_is_the_copy_gives_no_run() {
        let title = pattern(WINDOW + 10, 9);
        let mut held = title.clone();
        let mut runs = Vec::new();
        // SAFETY: the vector's own memory.
        unsafe {
            take_differing(
                &read,
                &whole,
                held.as_mut_ptr(),
                title.as_ptr() as usize,
                title.as_ptr() as usize + title.len(),
                0,
                &mut runs,
            )
        };
        assert!(runs.is_empty());
    }
}
