//! `VICTORY_REMAP_BATCH=1`: a change of many pages of mapping (the title's
//! `CopyPageMappingsBatchX`, which sets a tiled heap to one page of zeros and
//! parts of it to the pages that stream in, a thousand calls and 4,000 pages
//! a second in the prologue's cutscene) is done by the groups of
//! [`super::map_aliases`] with what is noted and told of its pages once for a
//! group, not for each page.
//!
//! Where the time of an alias map goes (runs of the cutscene, `title memory
//! calls` lines, 100 s of it): 355,000 pages, 84 us a page on average, of
//! which the host's own calls under them (unmap, split, map) are a fifth: the
//! rest is what the runtime does about each page, in global locks. Each page
//! was told to the GPU side three times (`title_memory_unmapping` and
//! `title_memory_remapped`, each looking through the descriptor ranges, the
//! views and the guards' sets, and `title_memory_view` under the views' write
//! lock), noted under the write lock of what the title sees (`Shown`) twice,
//! and its own range took the readers' lock (`MAPPING`, write) and
//! `PHYSICAL` for itself: 12,600 times in 5 s, held 3.3 s of them, with every
//! reader of title memory in place turned away or waiting behind it. A heap
//! set to one page of zeros is a run of 64 KiB pages one after the other in
//! address: one range for what is told of all of them.
//!
//! With the switch on:
//! - the ranges of a call are done in groups under one `PHYSICAL` and one
//!   readers' lock each, whatever `VICTORY_MAP_THREADS` is (without the
//!   switch a single thread did them range by range). `VICTORY_ALIAS_FAST`'s
//!   skipping of pages that show what is asked works in this path too, so it
//!   no longer needs several threads;
//! - the pages of a group that are unmapped, and those that are mapped, are
//!   told to the GPU side by runs of consecutive addresses
//!   (`title_memory_unmapping`, `title_memory_remapped`), and their views
//!   all at once (`title_memory_views`: one write lock of the views, one
//!   look of the guards at each page shown); what the title sees is noted
//!   under one lock;
//! - the threads that map do not queue a batch nobody serves when there are
//!   none (`VICTORY_MAP_THREADS=1`: the caller does them all).
//!
//! What is told is what the pages one by one told, in the same order of
//! kinds (all that is gone before a host call, all that came after it), over
//! longer ranges: the calls take ranges (a view of several pages is told as
//! one anyway).

use super::*;

/// Whether the switch is on (read once).
pub(super) fn on() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("VICTORY_REMAP_BATCH").is_some_and(|v| v == "1"));
    #[cfg(test)]
    if BATCH.load(Ordering::Relaxed) == 2 {
        return true;
    }
    *ON
}

/// Tests turn it on for a while (2).
#[cfg(test)]
pub(super) static BATCH: std::sync::atomic::AtomicU8 = std::sync::atomic::AtomicU8::new(0);

/// The pages at `at` (each a page of [`PAGE`] bytes, any order, none twice)
/// as runs of pages one after the other: address and bytes.
pub(super) fn runs(mut at: Vec<usize>) -> Vec<(usize, usize)> {
    at.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for page in at {
        match out.last_mut() {
            Some((start, len)) if *start + *len == page => *len += PAGE,
            _ => out.push((page, PAGE)),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pages_one_after_the_other_are_a_run() {
        let a = 0x10_0000_0000usize;
        let at = |k: usize| a + k * PAGE;
        assert_eq!(runs(vec![]), vec![]);
        assert_eq!(runs(vec![at(5)]), vec![(at(5), PAGE)]);
        assert_eq!(
            runs(vec![at(3), at(1), at(2), at(7), at(9), at(8), at(0)]),
            vec![(at(0), 4 * PAGE), (at(7), 3 * PAGE)]
        );
        assert_eq!(runs(vec![at(1), at(3)]), vec![(at(1), PAGE), (at(3), PAGE)]);
    }
}
