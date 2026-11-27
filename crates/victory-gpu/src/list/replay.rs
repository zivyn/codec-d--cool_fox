//! What a list reuse could save, counted (`VICTORY_REPLAY_COUNT=1`; nothing is reused).
//!
//! The title records some 200 lists a frame, most of them as they were the frame before. Every list
//! is hashed as it is recorded (its calls, their arguments, and the state of the copies the title's
//! addresses are found on, [`crate::shadow::world`]) and the hashes of a frame are kept for the next,
//! so that a run says how many lists and draws came again as they were, at three strengths:
//!
//! - the calls alone (same operations in the same order: only arguments differ),
//! - the calls and arguments but the root views' addresses and the upload blocks' (those of memory
//!   made for a list by `allocate_upload` differ with each recording),
//! - all of them (but the upload blocks' own addresses, which are never the same).
//!
//! And for each draw the same of the calls up to it (a list that is the same as before up to a draw
//! could continue a translation from there), and of the tables the draws make fresh (`draw::State::tables`).
//! Costs a few multiplications a call and a lock a list; for runs that price the idea, not for play.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};

/// Whether it is asked for.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REPLAY_COUNT").is_some_and(|v| v == "1"))
}

#[inline]
fn mix(h: u64, w: u64) -> u64 {
    (h.rotate_left(5) ^ w).wrapping_mul(0x517c_c1b7_2722_0a95)
}

/// The hashes of one list, as it is recorded.
#[derive(Default)]
pub(super) struct Hashes {
    /// Calls only; calls and arguments but views' addresses; all but upload blocks' addresses.
    h: [u64; 3],
    /// The three hashes of the calls up to each draw (or dispatch).
    draws: Vec<[u64; 3]>,
}

impl Hashes {
    pub(super) fn clear(&mut self) {
        self.h = [0; 3];
        self.draws.clear();
    }

    /// A call `op` with `args` (its words, and those of what they point at). `address`: it names a place that
    /// differs from list to list (a root view); `block`: an upload block (never the same).
    pub(super) fn add(&mut self, op: u64, args: &[u64], address: bool, block: bool, draw: bool) {
        self.h[0] = mix(self.h[0], op);
        self.h[1] = mix(self.h[1], op);
        self.h[2] = mix(self.h[2], op);
        if !block {
            for &w in args {
                if !address {
                    self.h[1] = mix(self.h[1], w);
                }
                self.h[2] = mix(self.h[2], w);
            }
        }
        if draw {
            self.draws.push(self.h);
        }
    }
}

#[derive(Default)]
struct Sets {
    /// Per level: lists, and the calls up to draws.
    lists: [HashSet<u64>; 3],
    prefixes: [HashSet<u64>; 3],
    /// Lists by their full hash, with the world they were recorded in.
    worlds: std::collections::HashMap<u64, u64>,
    tables: HashSet<u64>,
}

struct Frames {
    frame: u64,
    world: u64,
    cur: Sets,
    prev: Sets,
}

static FRAMES: Mutex<Option<Frames>> = Mutex::new(None);

/// Per frame: lists, draws; lists the same as the frame before at the three levels; the same again in this
/// frame; draws in lists that were the same (full); draws whose calls up to them were the same (three levels);
/// lists the same in full and the copies' state too; tables made, and those the frame before had; changes of
/// the copies' state.
static COUNT: [AtomicU64; 16] = [const { AtomicU64::new(0) }; 16];

fn rotate(f: &mut Option<Frames>, frame: u64, world: u64) -> &mut Frames {
    let f = f.get_or_insert_with(|| Frames { frame, world, cur: Sets::default(), prev: Sets::default() });
    if f.frame != frame {
        let next = std::mem::take(&mut f.cur);
        f.prev = if f.frame + 1 == frame { next } else { Sets::default() };
        f.frame = frame;
        COUNT[15].fetch_add(world.saturating_sub(f.world), Relaxed);
        f.world = world;
    }
    f
}

/// A list was recorded to its end.
pub(super) fn done(h: &mut Hashes) {
    let world = crate::shadow::world();
    let mut guard = FRAMES.lock().unwrap_or_else(|e| e.into_inner());
    let f = rotate(&mut guard, crate::frame::current(), world);
    COUNT[0].fetch_add(1, Relaxed);
    COUNT[1].fetch_add(h.draws.len() as u64, Relaxed);
    let full = h.h[2];
    for level in 0..3 {
        if f.prev.lists[level].contains(&h.h[level]) {
            COUNT[2 + level].fetch_add(1, Relaxed);
        }
    }
    if f.cur.lists[2].contains(&full) {
        COUNT[5].fetch_add(1, Relaxed);
    }
    if f.prev.lists[2].contains(&full) {
        COUNT[6].fetch_add(h.draws.len() as u64, Relaxed);
        if f.prev.worlds.get(&full) == Some(&world) {
            COUNT[13].fetch_add(1, Relaxed);
        }
    }
    for d in &h.draws {
        for level in 0..3 {
            if f.prev.prefixes[level].contains(&d[level]) {
                COUNT[7 + level].fetch_add(1, Relaxed);
            }
            f.cur.prefixes[level].insert(d[level]);
        }
    }
    for level in 0..3 {
        f.cur.lists[level].insert(h.h[level]);
    }
    f.cur.worlds.insert(full, world);
    h.clear();
}

/// A table a draw made fresh, by its key (the table, its entries, a hash of their records).
pub(crate) fn table(key: u64) {
    let world = crate::shadow::world();
    let mut guard = FRAMES.lock().unwrap_or_else(|e| e.into_inner());
    let f = rotate(&mut guard, crate::frame::current(), world);
    COUNT[10].fetch_add(1, Relaxed);
    if f.prev.tables.contains(&key) {
        COUNT[11].fetch_add(1, Relaxed);
    }
    f.cur.tables.insert(key);
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    if !on() {
        return;
    }
    let c: Vec<f64> = COUNT.iter().map(|c| c.swap(0, Relaxed) as f64).collect();
    if c[0] == 0.0 {
        return;
    }
    let pct = |a: f64, b: f64| if b > 0.0 { 100.0 * a / b } else { 0.0 };
    tracing::info!(
        "lists reuse priced per frame (VICTORY_REPLAY_COUNT): {:.0} lists, {:.0} draws; lists as in the frame before: same calls {:.0}%, + arguments but root views' addresses {:.0}%, + those {:.0}% ({:.0}% also with the copies' state as it was); same list again in the frame {:.0}%; draws in lists the same in full {:.0}%; draws whose calls up to them were the same: calls {:.0}%, + arguments but views {:.0}%, all {:.0}%; tables made {:.0}, {:.0}% as in the frame before; the copies' state changed {:.1} times",
        c[0] / frames,
        c[1] / frames,
        pct(c[2], c[0]),
        pct(c[3], c[0]),
        pct(c[4], c[0]),
        pct(c[13], c[0]),
        pct(c[5], c[0]),
        pct(c[6], c[1]),
        pct(c[7], c[1]),
        pct(c[8], c[1]),
        pct(c[9], c[1]),
        c[10] / frames,
        pct(c[11], c[10]),
        c[15] / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(args: &[u64], view: u64) -> Hashes {
        let mut h = Hashes::default();
        h.add(1, &[7], false, false, false);
        h.add(2, &[view], true, false, false);
        h.add(3, &[0xdead], false, true, false);
        h.add(4, args, false, false, true);
        h
    }

    #[test]
    fn a_list_is_the_same_at_the_level_of_what_differs() {
        let a = list(&[1, 2], 100);
        // Another view address: the same but for the full hash.
        let b = list(&[1, 2], 200);
        assert_eq!(a.h[0], b.h[0]);
        assert_eq!(a.h[1], b.h[1]);
        assert_ne!(a.h[2], b.h[2]);
        // Another argument: only the calls are the same.
        let c = list(&[1, 3], 100);
        assert_eq!(a.h[0], c.h[0]);
        assert_ne!(a.h[1], c.h[1]);
        assert_ne!(a.h[2], c.h[2]);
        // Another order of calls.
        let mut d = Hashes::default();
        d.add(2, &[100], true, false, false);
        d.add(1, &[7], false, false, false);
        assert_ne!(d.h[0], a.h[0]);
    }

    #[test]
    fn the_calls_up_to_a_draw_are_kept_for_each_draw() {
        let mut h = Hashes::default();
        h.add(1, &[1], false, false, true);
        h.add(1, &[2], false, false, true);
        let mut g = Hashes::default();
        g.add(1, &[1], false, false, true);
        g.add(1, &[3], false, false, true);
        assert_eq!(h.draws[0], g.draws[0]);
        assert_ne!(h.draws[1], g.draws[1]);
        h.clear();
        assert!(h.draws.is_empty() && h.h == [0; 3]);
    }
}
