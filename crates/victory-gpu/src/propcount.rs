//! Counters for card-side propagation and lazy landing (`VICTORY_PROP_COUNT=1`, a diagnostics switch: it changes
//! nothing the title or the card sees, it only counts and logs a line with each frame-rate line).
//!
//! The question: of the bytes that go GPU -> title memory (a landing merges what shaders wrote into the title's
//! memory on the CPU) and title memory -> host copies (a refresh reads title memory, compares and stores it into the
//! CPU side of a copy and sends it up to the card), how many are a round trip that nobody needed? Bytes that shaders
//! wrote in a copy A, landed, and then taken by the refresh of another copy B that covers the same title memory, with
//! no thread of the title reading them in between, could be one copy on the card (A to B) and no landing.
//!
//! What is counted, per frame:
//! - the pieces a landing fetches, by what a lazy landing would need of their copy (`Shadow::own_class`): ownable (a
//!   copy in the card's memory of a buffer in a default heap, private committed title memory, no other copy of title
//!   memory over the piece, the title never seen reading the copy), overlapped by another copy, seen read, not
//!   private memory, not a default-heap buffer, or kept eager for being forced often;
//! - the bytes merged into title memory, and the landing's CPU from the landing stamps (merge jobs, guards, notes);
//! - the bytes refreshes take into copies ([`taking`], one count per `Shadow::take`), with their time, and of them
//!   the 4 KiB pages a landing put into title memory in the last two frames: by the copy's own landing (its echo,
//!   which the copy compares and leaves) or by another copy's landing (the round trip), and of the second those of
//!   a copy the title was not seen to read (the title's CPU, by the probe or by touching an owed piece: run with
//!   `VICTORY_READ_PROBE=1` to learn it);
//! - which pairs of copies (lander, taker) move the most.
//!
//! "Without any CPU read in between" is as good as `Shadow::title_read` is (the read probe arms a few pages; a
//! touch of an owed piece says for sure): the figure is an upper bound of what a propagation can save, and the
//! ownable column is the lower bound of what lazy landing can leave alone.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

use crate::shadow::{CLASSES, Shadow};

/// Whether the counters run (`VICTORY_PROP_COUNT=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PROP_COUNT").is_some_and(|v| v == "1"))
}

const KERNEL_PAGE: usize = 4096;
/// A landing counts as lately for a refresh within this many frames.
const LATELY: u64 = 2;

/// Bytes of the pieces landings fetched by class, then their number by class.
static FETCHED: [AtomicU64; 2 * CLASSES] = [const { AtomicU64::new(0) }; 2 * CLASSES];
/// Bytes merged into title memory, pieces with a new byte.
static MERGED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
/// Takes, bytes, microseconds; of the bytes: pages landed lately, by the copy's own landing, by another copy's, by
/// another copy's of a copy not seen read; microseconds of the takes with another copy's landing in them (by share).
static TAKES: [AtomicU64; 8] = [const { AtomicU64::new(0) }; 8];
const T_N: usize = 0;
const T_BYTES: usize = 1;
const T_US: usize = 2;
const T_LATELY: usize = 3;
const T_OWN: usize = 4;
const T_CROSS: usize = 5;
const T_CROSS_UNREAD: usize = 6;
const T_CROSS_US: usize = 7;

/// What landings put into which pages lately, and by which pair of copies.
#[derive(Default)]
struct Landed {
    /// 4 KiB page number -> (frame, copy that landed it, whether the title was seen to read that copy).
    pages: HashMap<usize, (u64, u64, bool)>,
    /// Copy number -> (start, length) in title memory.
    spans: HashMap<u64, (u64, u64)>,
    /// (lander, taker) -> bytes.
    pairs: HashMap<(u64, u64), u64>,
}

static LANDED: Mutex<Option<Landed>> = Mutex::new(None);

fn landed_map() -> std::sync::MutexGuard<'static, Option<Landed>> {
    LANDED.lock().unwrap_or_else(|e| e.into_inner())
}

/// The pieces a landing is about to fetch or merge.
pub(crate) fn fetched(pieces: &[(Arc<Shadow>, usize, usize, u64)]) {
    if !on() || pieces.is_empty() {
        return;
    }
    let index = crate::shadow::overlap_index();
    for (shadow, lo, hi, _) in pieces {
        let class = shadow.own_class(*lo, *hi, &index) as usize;
        FETCHED[class].fetch_add((hi - lo) as u64, Ordering::Relaxed);
        FETCHED[CLASSES + class].fetch_add(1, Ordering::Relaxed);
    }
}

/// `merged` bytes of the piece at offsets `[lo, hi)` of `shadow` were put into title memory.
pub(crate) fn landed(shadow: &Shadow, lo: usize, hi: usize, merged: u64) {
    if !on() || merged == 0 {
        return;
    }
    MERGED[0].fetch_add(merged, Ordering::Relaxed);
    MERGED[1].fetch_add(1, Ordering::Relaxed);
    let (start, len) = shadow.span();
    let (from, to) = (start as usize + lo, start as usize + hi);
    let frame = crate::frame::current();
    let read = shadow.title_read();
    let id = shadow.id();
    let mut guard = landed_map();
    let map = guard.get_or_insert_with(Landed::default);
    map.spans.insert(id, (start, len));
    for page in from / KERNEL_PAGE..to.div_ceil(KERNEL_PAGE) {
        map.pages.insert(page, (frame, id, read));
    }
}

/// A refresh taking title memory `[from, to)` into a copy: counted, and timed until it is dropped.
pub(crate) struct Taking {
    began: Instant,
    bytes: u64,
    cross: u64,
}

pub(crate) fn taking(shadow: &Shadow, from: usize, to: usize) -> Option<Taking> {
    if !on() || to <= from {
        return None;
    }
    let now = crate::frame::current();
    let me = shadow.id();
    let (mut lately, mut own, mut cross, mut unread) = (0u64, 0u64, 0u64, 0u64);
    {
        let mut guard = landed_map();
        if let Some(map) = guard.as_mut() {
            let (start, len) = shadow.span();
            let mut mine = false;
            for page in from / KERNEL_PAGE..to.div_ceil(KERNEL_PAGE) {
                let Some(&(frame, by, read)) = map.pages.get(&page) else {
                    continue;
                };
                if now.saturating_sub(frame) > LATELY {
                    continue;
                }
                let n =
                    (((page + 1) * KERNEL_PAGE).min(to) - (page * KERNEL_PAGE).max(from)) as u64;
                lately += n;
                if by == me {
                    own += n;
                } else {
                    cross += n;
                    if !read {
                        unread += n;
                    }
                    *map.pairs.entry((by, me)).or_default() += n;
                    mine = true;
                }
            }
            if mine {
                map.spans.entry(me).or_insert((start, len));
            }
        }
    }
    TAKES[T_N].fetch_add(1, Ordering::Relaxed);
    TAKES[T_BYTES].fetch_add((to - from) as u64, Ordering::Relaxed);
    TAKES[T_LATELY].fetch_add(lately, Ordering::Relaxed);
    TAKES[T_OWN].fetch_add(own, Ordering::Relaxed);
    TAKES[T_CROSS].fetch_add(cross, Ordering::Relaxed);
    TAKES[T_CROSS_UNREAD].fetch_add(unread, Ordering::Relaxed);
    Some(Taking {
        began: Instant::now(),
        bytes: (to - from) as u64,
        cross,
    })
}

impl Drop for Taking {
    fn drop(&mut self) {
        let us = self.began.elapsed().as_micros() as u64;
        TAKES[T_US].fetch_add(us, Ordering::Relaxed);
        if self.cross > 0 && self.bytes > 0 {
            TAKES[T_CROSS_US].fetch_add(
                us * self.cross.min(self.bytes) / self.bytes,
                Ordering::Relaxed,
            );
        }
    }
}

/// For the frame-rate line, over `frames` frames; `land` is the landing's counters (`crate::land::stat`), not yet
/// taken.
pub(crate) fn line(frames: f64, land: &[u64]) {
    if !on() {
        return;
    }
    let take = |a: &AtomicU64| a.swap(0, Ordering::Relaxed) as f64 / frames.max(1.0);
    let mb = |v: f64| v / (1 << 20) as f64;
    let f: Vec<f64> = FETCHED.iter().map(take).collect();
    let total: f64 = f[..CLASSES].iter().sum();
    let pieces: f64 = f[CLASSES..].iter().sum();
    let merged = (take(&MERGED[0]), take(&MERGED[1]));
    let t: Vec<f64> = TAKES.iter().map(take).collect();
    let us = |i: usize| land.get(i).copied().unwrap_or(0) as f64 / 1000.0 / frames.max(1.0);
    let landing_ms = us(crate::land::stat::CARD_ALL) + us(crate::land::stat::NOTES);
    let share = |part: f64, whole: f64| if whole > 0.0 { part / whole } else { 0.0 };
    // (The order of `OwnClass`: ownable, overlapped, read, not private, not gpu-only, forced, ownable under copies
    // that never took the bytes.)
    tracing::info!(
        "card propagation counters per frame (VICTORY_PROP_COUNT=1): landings fetched {:.0} pieces, {:.1} MB: ownable {:.1} MB ({:.0} pieces; a copy in the card's memory of a default-heap buffer, private memory, no other copy over it, title not seen reading it), ownable under copies that never took the bytes {:.1} MB ({:.0} pieces), overlapped by a copy that took them {:.1}, seen read {:.1}, not private memory {:.1}, not a default-heap buffer {:.1}, kept eager for forced reads {:.1}; merged into title memory {:.1} MB in {:.0} pieces; landing CPU on all threads {:.1} ms (merge jobs {:.1}, guards and notes {:.1}), of it the ownable share about {:.1} ms and with the copies that never took the bytes {:.1} ms",
        pieces,
        mb(total),
        mb(f[0]),
        f[CLASSES],
        mb(f[6]),
        f[CLASSES + 6],
        mb(f[1]),
        mb(f[2]),
        mb(f[3]),
        mb(f[4]),
        mb(f[5]),
        mb(merged.0),
        merged.1,
        landing_ms,
        us(crate::land::stat::CARD_ALL),
        us(crate::land::stat::NOTES),
        landing_ms * share(f[0], total),
        landing_ms * share(f[0] + f[6], total),
    );
    tracing::info!(
        "card propagation counters per frame: refreshes took {:.0} runs, {:.1} MB, {:.1} ms on all threads; pages a landing put into title memory in the last {} frames: {:.1} MB, of them {:.1} MB by the taking copy's own landing (its echo) and {:.1} MB by another copy's landing (landed to be taken again by a copy it did not come from; {:.1} MB of those from copies the title was not seen to read, which is the round trip nobody read: about {:.1} ms of refresh by share and {:.1} ms of landing by share of the fetched bytes)",
        t[T_N],
        mb(t[T_BYTES]),
        t[T_US] / 1000.0,
        LATELY,
        mb(t[T_LATELY]),
        mb(t[T_OWN]),
        mb(t[T_CROSS]),
        mb(t[T_CROSS_UNREAD]),
        t[T_CROSS_US] / 1000.0 * share(t[T_CROSS_UNREAD], t[T_CROSS]),
        landing_ms * share(t[T_CROSS_UNREAD], total),
    );
    // The pairs that moved the most, and the map made small again.
    let mut guard = landed_map();
    if let Some(map) = guard.as_mut() {
        let now = crate::frame::current();
        map.pages
            .retain(|_, v| now.saturating_sub(v.0) <= LATELY + 1);
        let mut pairs: Vec<((u64, u64), u64)> = map.pairs.drain().collect();
        pairs.sort_by_key(|p| std::cmp::Reverse(p.1));
        let name = |id: u64, map: &Landed| match map.spans.get(&id) {
            Some(&(start, len)) => format!("{start:#x}+{len:#x}"),
            None => format!("copy {id}"),
        };
        for ((lander, taker), bytes) in pairs.into_iter().take(6) {
            tracing::info!(
                "  landed by {} and taken by {}: {:.2} MB a frame",
                name(lander, map),
                name(taker, map),
                mb(bytes as f64 / frames.max(1.0))
            );
        }
        if map.pages.len() > 8 << 20 {
            map.pages.clear();
        }
    }
}
