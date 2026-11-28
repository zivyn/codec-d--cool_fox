//! Pieces of shader-written copies that stay on the card (`VICTORY_LAND_OWN=1`, off by default; live in the A/B).
//!
//! A landing fetches each piece the compare flagged (64 KiB of a copy in the card's memory), compares it with the
//! copy's CPU side and merges what changed into title memory, on the landing threads: 41 MB and 52 ms of CPU a
//! frame on all threads in the cell, of which the title's CPU reads almost nothing (73 reads of GPU-written pages
//! in a run, through one routine). The bytes are then taken again by whatever refreshes a copy over the same
//! memory.
//!
//! With the switch, a piece that nothing but the title's CPU can read from title memory is not merged: its fetched
//! bytes are copied into a buffer of ours that the unit [`owed::keep`] owns, and the title's pages are guarded
//! (`PAGE_GUARD`). The next flight that writes the same piece only replaces those bytes (no guard to arm, nothing
//! merged); the piece lands, from the newest bytes, when a thread reads or writes one of its pages, the title's by
//! the guard page violation and ours through `crate::host::read` (`owed::land_range`), and the copy is eager from
//! then on if the title's thread was the one (`Shadow::title_read`) or if our own readers keep forcing it
//! (`VICTORY_LAND_OWN_FORCED`).
//!
//! Which pieces (`Shadow::own_class`): a copy in the card's memory of a buffer in a default heap, the title not
//! seen reading it, its pages private committed read-write memory (they can be guarded), and no other copy of title
//! memory covers a byte of the piece. That last is what makes it safe to leave title memory behind the card: every
//! other copy is brought up to date from title memory and would take the old bytes; a copy made later takes its
//! first refresh whole, from title memory, which lands what is owed there first.
//!
//! Why the order is right. Title memory plus what is owed is what title memory would hold if the piece had landed:
//! nothing reads title memory without landing the owed pieces under it first (the title's thread by the handler, ours
//! by `host::read`, the kernel's writes by `watch::writing`, a free or an unmap by `watch::title_memory_freed`), and
//! nothing writes it before. The copy's CPU side is not given the piece's bytes while it is owed, so the merge, when it
//! comes, compares the card's bytes with what the CPU side held when the first of the pieces still owed was flagged:
//! exactly the bytes shaders wrote since then. What the title wrote to the copy's bytes after the piece was fetched
//! (what went up to the card while it was in flight, and what is to go up) is left out of the merge (`owe_skips`,
//! noted when the piece was kept: the copy's own notes of sends are forgotten as later fetches are merged, which a
//! piece kept for many frames cannot wait for).
//!
//! The memory kept is bounded (`VICTORY_LAND_OWN_MB`, 384): over it, pieces land as before.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use super::Notes;
use super::owed::{self, Land};
use crate::shadow::{CLASSES, OwnClass, Shadow};

/// A fetched piece: its copy, the offsets in it, and where its bytes are in the fetch buffer.
pub(crate) type Fetched = (Arc<Shadow>, usize, usize, u64);

/// Whether pieces are left on the card (`VICTORY_LAND_OWN=1`; live in the A/B).
pub(crate) fn on() -> bool {
    crate::ab::live_flag!("VICTORY_LAND_OWN")
}

/// Whether a piece that other copies lie over, none of which ever took its bytes, stays on the card too
/// (`VICTORY_LAND_OWN_UNTAKEN=1`; live in the A/B; needs [`on`]).
pub(crate) fn untaken_on() -> bool {
    crate::ab::live_flag!("VICTORY_LAND_OWN_UNTAKEN")
}

/// The most bytes kept (`VICTORY_LAND_OWN_MB`).
fn most() -> u64 {
    static MOST: OnceLock<u64> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_LAND_OWN_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(384)
            << 20
    })
}

/// A piece's bytes kept for its landing. Buffers go back to a pool of our own: the same pieces are written by every
/// frame, so the same buffers serve.
pub(crate) struct Stored {
    bytes: Vec<u8>,
}

static POOL: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
/// Bytes held in [`Stored`] now.
static HELD: AtomicU64 = AtomicU64::new(0);
const PIECE: usize = 64 << 10;
const POOLED: usize = 4096;

impl Stored {
    /// A copy of `bytes`.
    pub(crate) fn of(bytes: &[u8]) -> Stored {
        let kept = POOL.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let mut v = match kept {
            Some(v) if v.capacity() >= bytes.len() => v,
            _ => Vec::with_capacity(bytes.len().max(PIECE)),
        };
        v.clear();
        v.extend_from_slice(bytes);
        HELD.fetch_add(v.len() as u64, Ordering::Relaxed);
        Stored { bytes: v }
    }
}

impl Drop for Stored {
    fn drop(&mut self) {
        HELD.fetch_sub(self.bytes.len() as u64, Ordering::Relaxed);
        let v = std::mem::take(&mut self.bytes);
        let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
        if pool.len() < POOLED {
            pool.push(v);
        }
    }
}

/// Bytes kept on the card's behalf now.
pub(crate) fn held() -> u64 {
    HELD.load(Ordering::Relaxed)
}

/// Since the last frame-rate line: bytes of pieces by class; pieces kept new, replaced, given back to be landed
/// now, refused for the memory cap; bytes copied out.
static STATS: [AtomicU64; CLASSES + 7] = [const { AtomicU64::new(0) }; CLASSES + 7];
const NEW: usize = CLASSES;
const REPLACED: usize = CLASSES + 1;
const GIVEN: usize = CLASSES + 2;
const CAPPED: usize = CLASSES + 3;
const COPIED: usize = CLASSES + 4;
const OFFERED: usize = CLASSES + 5;
/// Kept pieces landed at once because a copy over them had taken their bytes by the time the guard was armed.
const RECHECKED: usize = CLASSES + 6;

/// What lands a piece from `stored`: the merge `land_card_with` makes of a fetched piece, leaving out `skips`.
pub(crate) fn lander(
    shadow: Arc<Shadow>,
    lo: usize,
    hi: usize,
    stored: Stored,
    skips: Vec<(usize, usize)>,
) -> Land {
    Box::new(move || {
        let mut notes = Notes::default();
        let at = stored.bytes.as_ptr() as usize;
        // SAFETY: the stored bytes, which stay until this returns; the copy is held here; `u64::MAX`: no send is
        // newer than the fetch that `skips` stands for.
        unsafe { shadow.land_card_skipping(&[(lo, hi, at)], u64::MAX, &skips, &mut notes, true) };
        notes.flush();
    })
}

/// The closures a kept unit tells the copy with: the title (or something that is not ours) touched it, and one of our
/// own readers landed it.
pub(crate) fn tellers(
    shadow: &Arc<Shadow>,
) -> (Box<dyn Fn() + Send + Sync>, Box<dyn Fn() + Send + Sync>) {
    let (a, b) = (shadow.clone(), shadow.clone());
    (
        Box::new(move || a.note_title_read()),
        Box::new(move || b.note_own_forced()),
    )
}

/// Of the pieces a landing has fetched (`data`: where its fetch buffer is mapped; `after`: the order of the sends
/// the fetch ran behind), keeps those that may stay on the card and returns the rest, to be landed as before.
pub(crate) fn offer(pieces: Vec<Fetched>, data: usize, after: u64) -> Vec<Fetched> {
    if pieces.is_empty() {
        return pieces;
    }
    let index = crate::shadow::overlap_index();
    let over_too = untaken_on();
    let mut rest = Vec::with_capacity(pieces.len());
    // The pieces newly kept, to be looked at again once their guards are armed.
    let mut armed: Vec<((u64, usize), usize, usize, Arc<Shadow>)> = Vec::new();
    for piece in pieces {
        let (lo, hi, at) = (piece.1, piece.2, piece.3 as usize);
        let n = hi - lo;
        let from = piece.0.span().0 as usize + lo;
        let to = from + n;
        let key = (piece.0.id(), lo);
        STATS[OFFERED].fetch_add(1, Ordering::Relaxed);
        // A piece owed already stays so: whatever made it ownable holds.
        let pending = owed::kept_pending(key, from, to);
        let class = if pending {
            OwnClass::Ownable
        } else {
            piece.0.own_class(lo, hi, &index)
        };
        STATS[class as usize].fetch_add(n as u64, Ordering::Relaxed);
        if class != OwnClass::Ownable && !(class == OwnClass::OwnableOver && over_too) {
            rest.push(piece);
            continue;
        }
        if !pending && held() + n as u64 > most() {
            STATS[CAPPED].fetch_add(n as u64, Ordering::Relaxed);
            rest.push(piece);
            continue;
        }
        // SAFETY: the fetch buffer, mapped, with these bytes in it: the copy into it has run.
        let src = unsafe { std::slice::from_raw_parts((data + at) as *const u8, n) };
        let stored = Stored::of(src);
        STATS[COPIED].fetch_add(n as u64, Ordering::Relaxed);
        let skips = piece.0.owe_skips(lo, hi, after);
        let land = lander(piece.0.clone(), lo, hi, stored, skips);
        let (touched, forced) = tellers(&piece.0);
        match owed::keep(key, from, to, land, touched, forced) {
            Ok(owed::Kept::New) => {
                STATS[NEW].fetch_add(1, Ordering::Relaxed);
                armed.push((key, from, to, piece.0.clone()));
            }
            Ok(owed::Kept::Replaced) => {
                STATS[REPLACED].fetch_add(1, Ordering::Relaxed);
            }
            Err(land) => {
                // Could not be kept (its pages are not ours to guard, or another piece owed over them is being
                // landed): landed now, here, from the bytes just stored.
                STATS[GIVEN].fetch_add(1, Ordering::Relaxed);
                land();
            }
        }
    }
    recheck(armed);
    rest
}

/// Pieces kept just now, their guards armed: a copy made or refreshed over one while the piece was being decided,
/// before its guard stood, read the title's bytes from before and would not read them again (nothing noted a write).
/// If any copy over a piece has taken its bytes, the piece lands now, which notes the write for every copy and so
/// has that copy take them again. A copy that has not taken them reads them later, through the guard.
fn recheck(armed: Vec<((u64, usize), usize, usize, Arc<Shadow>)>) {
    if armed.is_empty() {
        return;
    }
    // (Made after the guards: a copy that is not in it was made after them, and reads through them.)
    let index = crate::shadow::overlap_index();
    for (key, from, to, shadow) in armed {
        let taken = index
            .others_over(shadow.id(), from as u64, to as u64)
            .iter()
            .any(|b| !b.untaken(from as u64, to as u64));
        if taken {
            STATS[RECHECKED].fetch_add(1, Ordering::Relaxed);
            owed::land_kept(key, from, to);
        }
    }
}

/// For the frame-rate line, over `frames` frames.
pub(crate) fn line(frames: f64) {
    let s: Vec<f64> = STATS
        .iter()
        .map(|s| s.swap(0, Ordering::Relaxed) as f64 / frames.max(1.0))
        .collect();
    if !on() && s.iter().all(|&v| v == 0.0) && owed::kept_count() == 0 {
        return;
    }
    let mb = |v: f64| v / (1 << 20) as f64;
    tracing::info!(
        "pieces left on the card per frame (VICTORY_LAND_OWN={}, VICTORY_LAND_OWN_UNTAKEN={}): {:.0} offered; kept new {:.1} (guards armed), replaced {:.1} (no guard, no merge), landed at once because they could not be kept {:.1}, landed at once because a copy over them had taken their bytes {:.1}; {:.1} MB copied out; by class of what was offered: ownable {:.1} MB, ownable under copies that never took it {:.1}, overlapped by a copy that took it {:.1}, seen read {:.1}, not private {:.1}, not default-heap {:.1}, forced often {:.1}, over the memory cap {:.1}; {} units and {:.1} MB held now (cap {} MB)",
        u8::from(on()),
        u8::from(untaken_on()),
        s[OFFERED],
        s[NEW],
        s[REPLACED],
        s[GIVEN],
        s[RECHECKED],
        mb(s[COPIED]),
        mb(s[0]),
        mb(s[6]),
        mb(s[1]),
        mb(s[2]),
        mb(s[3]),
        mb(s[4]),
        mb(s[5]),
        mb(s[CAPPED]),
        owed::kept_count(),
        mb(held() as f64),
        most() >> 20,
    );
}
