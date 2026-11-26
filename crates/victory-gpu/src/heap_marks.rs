//! The heaps' records asked for at the title's submit
//! (`VICTORY_HEAP_SYNC_AT_SUBMIT=1`, off by default).
//!
//! Ported from Belobug/VIctory-Belobug-AMD 673be0e and a609a2f (renderer
//! port 7, its heaps' records part; there under `VICTORY_PORT_JOURNAL` with
//! the journal in parts, which is not taken here). There the submitting lane
//! waited 4.5-5.2 ms a frame for the heaps' records, and port 7 took 2.4 ms
//! off the lane. Here it waited 2.7-5.8 ms a frame (release run 233354).
//!
//! A round of the heap-sync thread (`crate::descriptor::sync_heaps`) is asked
//! for when the title submits ([`round_after`]), so it runs while the
//! submission waits in the line (12-15 submissions a frame are in it when one
//! is added), and the submitting thread waits for that round, or for a later
//! one if (a) a wait of its queue that was met in memory went since (the
//! queue's floor, `queue/behind.rs` `State::floors`) or (b) a step of ours
//! wrote, or may have written, a heap's title memory while preparing an
//! earlier submission ([`prep_wrote`]). A submission that waited for flights
//! to land begins a round of its own after the wait (`crate::host`).
//!
//! Why a submission's records are what they were: a round reads the title's
//! records as they are when it runs. The records a submission's lists use are
//! written by the title before its submit call, or, for a part behind a wait
//! for a memory value or a fence, before that wait is met. A round asked for
//! at the submit call (or, behind a wait, the first round to begin after the
//! line saw the wait met) begins after those writes: it sees all of them, as
//! the round begun at the preparing did. What changes: a record the title
//! writes for a submission after submitting it is left to a later round (the
//! console's card could miss it as well).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

/// Whether the switch is on.
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_HEAP_SYNC_AT_SUBMIT"))
}

/// With rounds `asked` asked for and `done` done (one runs at a time, in
/// order; round `k` begins only once `k - 1` is done and `k` has been asked
/// for): the number of a round that begins after now, and whether it has to
/// be asked for (it is `asked + 1` then). A round asked for and not begun
/// yet (`asked >= done + 2`: the one before it is not done) begins after
/// now; with `asked == done + 1` round `asked` may be running already.
pub(crate) fn round_after(asked: u64, done: u64) -> (u64, bool) {
    if asked >= done + 2 { (asked, false) } else { (asked + 1, true) }
}

thread_local! {
    /// On the submitting thread: the round of the heaps' records the
    /// submission being prepared needs (0: none known, a round is asked for
    /// at the preparing, as without the switch).
    static MARK: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// The line is about to do a submission that needs round `mark`.
pub(crate) fn set_mark(mark: u64) {
    MARK.set(mark);
}

/// The round every submission prepared from now on needs at least: one that
/// began after a step of a submission being prepared wrote, or may have
/// written, title memory of a heap of descriptors ([`prep_wrote`]).
static PREP_FLOOR: AtomicU64 = AtomicU64::new(0);

/// A step of the submission being prepared wrote, or may have written, title
/// memory of a heap of descriptors: every submission prepared after it waits
/// for a round begun after now.
pub(crate) fn prep_wrote() {
    PREP_FLOOR.fetch_max(crate::descriptor::round_mark(), Relaxed);
    STATS[PREP_WRITES].fetch_add(1, Relaxed);
}

/// The round the submission about to be prepared needs (its mark, at least
/// [`PREP_FLOOR`]); 0 if none is known (or the switch is off).
pub(crate) fn needed() -> u64 {
    if !on() {
        return 0;
    }
    match MARK.get() {
        0 => {
            STATS[NO_MARK].fetch_add(1, Relaxed);
            0
        }
        m => m.max(PREP_FLOOR.load(Relaxed)),
    }
}

/// Since the last frame line: rounds asked for at a submit; submissions
/// that found their round asked earlier; preparings that wrote a heap's
/// memory (or may have); nanoseconds the submitting thread waited for the
/// heaps' records, and how many times; preparings with no mark; those whose
/// round had to be asked for at the preparing.
pub(crate) static STATS: [AtomicU64; 7] = [const { AtomicU64::new(0) }; 7];
pub(crate) const ROUNDS_ASKED: usize = 0;
pub(crate) const EARLY_ROUNDS: usize = 1;
const PREP_WRITES: usize = 2;
const HEAPS_NS: usize = 3;
const HEAPS: usize = 4;
const NO_MARK: usize = 5;
pub(crate) const LATE_ROUNDS: usize = 6;

/// The submitting thread waited `ns` for the heaps' records.
pub(crate) fn heaps_waited(ns: u64) {
    STATS[HEAPS_NS].fetch_add(ns, Relaxed);
    STATS[HEAPS].fetch_add(1, Relaxed);
}

/// The frame line's, per frame.
pub(crate) fn line(frames: f64) {
    let s: [f64; 7] = std::array::from_fn(|i| STATS[i].swap(0, Relaxed) as f64);
    if !on() {
        return;
    }
    let f = frames.max(1.0);
    tracing::info!(
        "the heaps' records asked for at the title's submit, per frame (VICTORY_HEAP_SYNC_AT_SUBMIT=1): the submitting thread \
         waited {:.2} ms in {:.1} preparings; {:.1} rounds asked for at a submit, {:.1} preparings found theirs asked earlier, \
         {:.1} asked it at the preparing, {:.1} had no mark; {:.1} preparings wrote a heap's memory",
        s[HEAPS_NS] / 1e6 / f,
        s[HEAPS] / f,
        s[ROUNDS_ASKED] / f,
        s[EARLY_ROUNDS] / f,
        s[LATE_ROUNDS] / f,
        s[NO_MARK] / f,
        s[PREP_WRITES] / f,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A round asked for and not begun is one that begins after now; one
    /// that may be running is not.
    #[test]
    fn a_round_is_one_that_begins_after_now() {
        // Nothing asked: ask for round 1.
        assert_eq!(round_after(0, 0), (1, true));
        // Round 3 asked for, 2 done: 3 may be running, ask for 4.
        assert_eq!(round_after(3, 2), (4, true));
        // Rounds up to 5 asked for, 2 done: 3 runs or waits, 5 cannot have
        // begun.
        assert_eq!(round_after(5, 2), (5, false));
        assert_eq!(round_after(4, 2), (4, false));
        // Everything done: ask anew.
        assert_eq!(round_after(7, 7), (8, true));
    }

    /// A simulation of the rounds' thread: whatever the times of asking,
    /// the round a caller is given begins after its call.
    #[test]
    fn the_round_given_begins_after_the_call() {
        let (mut asked, mut done) = (0u64, 0u64);
        // When each round began (in calls), and each caller's (call, round).
        let mut began: std::collections::HashMap<u64, u64> = Default::default();
        let mut given: Vec<(u64, u64)> = Vec::new();
        let mut running: Option<u64> = None;
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        for call in 1..5000u64 {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            match seed % 3 {
                0 => {
                    let (round, ask) = round_after(asked, done);
                    if ask {
                        asked += 1;
                    }
                    given.push((call, round));
                }
                1 => {
                    if let Some(r) = running.take() {
                        done = r;
                    }
                }
                _ => {
                    if running.is_none() && asked > done {
                        running = Some(done + 1);
                        began.insert(done + 1, call);
                    }
                }
            }
        }
        for (call, round) in given {
            if let Some(&at) = began.get(&round) {
                assert!(at > call, "round {round} began at {at}, before the call at {call}");
            }
        }
    }
}
