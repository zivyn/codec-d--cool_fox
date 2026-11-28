//! What the threads that translate lists spend their time on, by operation (`VICTORY_LIST_OPS=1`, diagnostics builds): for
//! each of a few operations the translators do over and over for each draw, how many times a frame and how long all
//! of them took, inclusive (an operation inside another is in both: `bind` holds `tables`, which holds `make_listed`, ...)
//! and with the clock's own cost (some 25-30 ns for the two reads of it) in each. The line is in the log with the one of
//! `VICTORY_THIN`, every five seconds. Wall time of the threads, descheduled time in it.
//!
//! Off, an operation costs one load and one branch.

use std::cell::Cell;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::Instant;

/// The operations that are timed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(usize)]
pub(crate) enum Op {
    /// `descriptor::view_at`: the view a record names.
    ViewAt,
    /// `shadow::lookup`: the copy of a title buffer an address is in.
    Lookup,
    /// `resource::hosted`: the host side of a title texture.
    Hosted,
    /// `HostHeap::keep` and `keep_ref`: what a slot shows is kept.
    Keep,
    /// `HostHeap::make_listed`: a slot made from a record, within a recording.
    MakeListed,
    /// `make_twinned`: a view made in the host's heap (the driver's calls are in it).
    MakeTwinned,
    /// `draw::readable`: what a draw reads is brought up to date.
    Readable,
    /// `State::tables`: the descriptor tables of a draw.
    Tables,
    /// `State::bind`: everything a draw sets.
    Bind,
}

const N: usize = 9;
const NAMES: [&str; N] = [
    "view_at",
    "shadow lookup",
    "hosted",
    "keep",
    "make_listed",
    "make_twinned",
    "readable",
    "tables",
    "bind",
];

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    cfg!(feature = "diagnostics")
        && *ON.get_or_init(|| std::env::var_os("VICTORY_LIST_OPS").is_some_and(|v| v == "1"))
}

/// Calls and nanoseconds, all threads, since the last line.
static TOTAL: [(AtomicU64, AtomicU64); N] = [const { (AtomicU64::new(0), AtomicU64::new(0)) }; N];

thread_local! {
    /// This thread's, not yet in [`TOTAL`], and how many were added since they were last.
    static LOCAL: Cell<([(u64, u64); N], u32)> = const { Cell::new(([(0, 0); N], 0)) };
}

/// Calls to a thread's own tally before it is added to the total.
const FLUSH: u32 = 256;

/// Times an operation until it is dropped.
pub(crate) struct Timer(Option<(Op, Instant)>);

pub(crate) fn time(op: Op) -> Timer {
    Timer(on().then(|| (op, Instant::now())))
}

impl Drop for Timer {
    fn drop(&mut self) {
        if let Some((op, begun)) = self.0.take() {
            add(op, begun.elapsed().as_nanos() as u64);
        }
    }
}

fn add(op: Op, ns: u64) {
    let _ = LOCAL.try_with(|local| {
        let (mut tally, mut since) = local.get();
        let slot = &mut tally[op as usize];
        slot.0 += 1;
        slot.1 += ns;
        since += 1;
        if since >= FLUSH {
            for (total, mine) in TOTAL.iter().zip(&mut tally) {
                total.0.fetch_add(mine.0, Relaxed);
                total.1.fetch_add(mine.1, Relaxed);
                *mine = (0, 0);
            }
            since = 0;
        }
        local.set((tally, since));
    });
}

/// The text of the line: per frame, for each operation that ran, its calls, its time in milliseconds (all threads) and
/// the time of a call.
fn text(counts: &[(u64, u64); N], frames: f64) -> Option<String> {
    let parts: Vec<String> = NAMES
        .iter()
        .zip(counts)
        .filter(|(_, (calls, _))| *calls > 0)
        .map(|(name, &(calls, ns))| {
            format!(
                "{name} {:.0} calls {:.1} ms ({:.0} ns each)",
                calls as f64 / frames,
                ns as f64 / 1e6 / frames,
                ns as f64 / calls as f64
            )
        })
        .collect();
    (!parts.is_empty()).then(|| parts.join(", "))
}

/// For the frame-rate line, with `frames` frames since the last.
pub(crate) fn line(frames: f64) {
    if !on() || frames <= 0.0 {
        return;
    }
    let mut counts = [(0u64, 0u64); N];
    for (total, mine) in TOTAL.iter().zip(&mut counts) {
        *mine = (total.0.swap(0, Relaxed), total.1.swap(0, Relaxed));
    }
    if let Some(text) = text(&counts, frames) {
        tracing::info!(
            "list translation by operation per frame (VICTORY_LIST_OPS; each inside the next is in both, the clock's own 25-30 ns in each): {text}"
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_names_are_one_for_each_operation_in_order() {
        assert_eq!(NAMES[Op::ViewAt as usize], "view_at");
        assert_eq!(NAMES[Op::Bind as usize], "bind");
        assert_eq!(Op::Bind as usize + 1, N);
    }

    #[test]
    fn the_line_names_what_ran_per_frame_and_leaves_out_what_did_not() {
        let mut counts = [(0u64, 0u64); N];
        counts[Op::ViewAt as usize] = (40_000, 4_000_000);
        counts[Op::Bind as usize] = (4_000, 40_000_000);
        let text = text(&counts, 2.0).unwrap();
        assert_eq!(
            text,
            "view_at 20000 calls 2.0 ms (100 ns each), bind 2000 calls 20.0 ms (10000 ns each)"
        );
        assert_eq!(super::text(&[(0, 0); N], 2.0), None);
    }

    #[test]
    fn a_thread_adds_its_tally_to_the_total_every_so_often() {
        let before = TOTAL[Op::Keep as usize].0.load(Relaxed);
        std::thread::spawn(|| {
            for _ in 0..FLUSH {
                add(Op::Keep, 10);
            }
        })
        .join()
        .unwrap();
        assert!(TOTAL[Op::Keep as usize].0.load(Relaxed) >= before + u64::from(FLUSH));
        assert!(TOTAL[Op::Keep as usize].1.load(Relaxed) >= 10 * u64::from(FLUSH));
    }
}
