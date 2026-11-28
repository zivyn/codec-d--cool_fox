//! What the lane waits for, per frame: the time of the thread that prepares submissions (`victory-submit`) that it
//! spent blocked, by the thing it was blocked on, as one line of the frame-rate meter.
//!
//! The timeline (`VICTORY_TIMELINE`) sees the waits that go through `WaitOnAddress` (a `Mutex`, a `Condvar`) and not those
//! of a `RwLock` (the journal's, a copy's `merging`), and the lines of the write trackers count the waits of all
//! threads, the title's faults among them. This counts the lane's own, whatever the lock, at the places where it can
//! block: the claim on a range being asked about, the journal, the guards' stripes, the locks of a copy it puts the
//! title's bytes into, and the joins of the work it gave to other threads.
//!
//! Only blocked time is counted: a lock taken at once, or a join with nothing left to wait for, costs one failed
//! `try_lock` or one look at a counter.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering};

/// What the lane waits for.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Wait {
    /// The claim on a range of title memory that another asking is at ([`crate::watch`]'s `Asking`).
    Hold,
    /// The journal of what the title wrote (a `RwLock`).
    Journal,
    /// A stripe of the guards.
    Guards,
    /// The locks of a copy the title's bytes are put into under shader writes (`Shadow::merging`, `before`): a landing
    /// of that copy holds them.
    CopyLocks,
    /// The end of the refreshes of copies the lane gave to the kept threads (`refresh_front`, a flush).
    JoinRefresh,
    /// The end of the pages compared on the threads kept for landing (`land::each_under`).
    JoinUnder,
    /// The end of the snapshots given to the kept threads.
    JoinSnapshots,
    /// The end of any other work given to the kept threads.
    JoinOther,
    /// Flights: a settle, or the relay's landing of them.
    Flights,
    /// The heaps' records.
    Heaps,
}

const KINDS: usize = 10;

const NAMES: [&str; KINDS] = [
    "a range another asking holds",
    "the journal",
    "the guards",
    "a copy's locks under shader writes",
    "the refreshes on the kept threads",
    "the pages compared under flights",
    "the snapshots",
    "other work on the kept threads",
    "flights",
    "the heaps' records",
];

/// Since the last line, by [`Wait`]: nanoseconds, and how many.
static NS: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];
static COUNT: [AtomicU64; KINDS] = [const { AtomicU64::new(0) }; KINDS];

thread_local! {
    /// This is the lane.
    static LANE: Cell<bool> = const { Cell::new(false) };
    /// What a join of work given to the kept threads is counted as, by whoever gave it.
    static JOIN: Cell<Wait> = const { Cell::new(Wait::JoinOther) };
}

/// The calling thread is the lane.
pub(crate) fn this_is_the_lane() {
    LANE.set(true);
}

/// Whether the calling thread is the lane.
pub(crate) fn lane() -> bool {
    LANE.get()
}

/// The lane waited `ns` for `what`; nothing if the calling thread is another.
pub(crate) fn note(what: Wait, ns: u64) {
    if lane() {
        NS[what as usize].fetch_add(ns, Ordering::Relaxed);
        COUNT[what as usize].fetch_add(1, Ordering::Relaxed);
    }
}

/// `f`, its time counted as the lane's wait for `what` (when the lane is the caller).
pub(crate) fn timed<T>(what: Wait, f: impl FnOnce() -> T) -> T {
    if !lane() {
        return f();
    }
    let began = std::time::Instant::now();
    let got = f();
    note(what, began.elapsed().as_nanos() as u64);
    got
}

/// The calling thread's joins are counted as `what` until this is dropped.
pub(crate) struct Joining(Wait);

pub(crate) fn joining(what: Wait) -> Joining {
    Joining(JOIN.replace(what))
}

impl Drop for Joining {
    fn drop(&mut self) {
        JOIN.set(self.0);
    }
}

/// The lane waited `ns` at a join of work it gave to the kept threads.
pub(crate) fn joined(ns: u64) {
    if lane() {
        note(JOIN.get(), ns);
    }
}

/// For the frame-rate line: per frame, the lane's waits that took anything.
pub(crate) fn line(frames: f64) {
    let ns: Vec<f64> = NS.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64).collect();
    let count: Vec<f64> = COUNT.iter().map(|n| n.swap(0, Ordering::Relaxed) as f64).collect();
    let all: f64 = ns.iter().sum();
    if all == 0.0 {
        return;
    }
    let parts: Vec<String> = (0..KINDS)
        .filter(|&i| count[i] > 0.0)
        .map(|i| format!("{} {:.2} ms ({:.0})", NAMES[i], ns[i] / 1e6 / frames, count[i] / frames))
        .collect();
    tracing::info!(
        "the lane waited per frame, by what it was blocked on (ms, waits): {:.2} ms in all: {}",
        all / 1e6 / frames,
        parts.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread that is not the lane counts nothing; the lane counts what it waits, under the class the joiner set.
    #[test]
    fn only_the_lane_is_counted_and_a_join_goes_by_who_gave_the_work() {
        std::thread::spawn(|| {
            let before = NS[Wait::Guards as usize].load(Ordering::Relaxed);
            note(Wait::Guards, 5);
            assert_eq!(NS[Wait::Guards as usize].load(Ordering::Relaxed), before);
        })
        .join()
        .unwrap();
        std::thread::spawn(|| {
            this_is_the_lane();
            let (a, b) = (NS[Wait::JoinRefresh as usize].load(Ordering::Relaxed), NS[Wait::JoinOther as usize].load(Ordering::Relaxed));
            {
                let _giving = joining(Wait::JoinRefresh);
                joined(7);
                {
                    let _nested = joining(Wait::JoinUnder);
                    joined(1);
                }
                joined(3);
            }
            joined(11);
            let after = |w: Wait| NS[w as usize].load(Ordering::Relaxed);
            assert!(after(Wait::JoinRefresh) >= a + 10);
            assert!(after(Wait::JoinOther) >= b + 11);
            let got = timed(Wait::Heaps, || 42);
            assert_eq!(got, 42);
        })
        .join()
        .unwrap();
    }
}
