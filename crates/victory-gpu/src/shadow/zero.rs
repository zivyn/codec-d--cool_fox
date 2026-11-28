//! Zero copy (`VICTORY_ZEROCOPY`): the GPU reads the title's own bytes where the title's CPU writes them and no
//! shader writes them, with no copy in the card's memory to bring up to date.
//!
//! On the console the CPU and the GPU share one memory. Here a title buffer's bytes were copied into a buffer in the
//! card's memory before every submission that read them (what the title's CPU wrote since: found by write watching
//! and guards, compared, sent over the bus), 38 MB a frame in the world. For memory that shaders only read, the copy
//! adds nothing: a heap opened on the title's own pages ([`crate::inplace`]) and a buffer placed in it show the GPU
//! the same bytes, read over the bus when the shader needs them, and there is nothing to write-watch, guard, compare
//! or send.
//!
//! What qualifies is what the title itself declares. Its allocations carry the GPU's page protection
//! (`title_memory_gpu_readonly`): ranges the GPU may read and not write are the constant rings, dynamic vertex data and
//! structure the title's CPU fills every frame. Such a range is a **cell**: one placed buffer over all of it, which
//! every ask that lies inside the range ([`serve`]) is answered with, before any buffer's copy is looked for. An ask
//! that begins in the range and ends outside it is the buffer's copy's, as before (that copy is brought up to date
//! for the whole of that ask).
//!
//! Which declared ranges are worth giving: the card's copies show what is sent up. A range is watched
//! ([`observe`]: the bytes the sends into it, counted where the card copies are brought up to date) for
//! `VICTORY_ZEROCOPY_OBSERVE` frames (90), and given to the card if the title wrote at least
//! `VICTORY_ZEROCOPY_MIN_UP_KB` (16) KiB a frame into it: a range the title writes once and shaders read a thousand
//! times a frame stays a copy in the card's memory, where the reads are fast. `VICTORY_ZEROCOPY_OBSERVE=0` gives
//! every declared range at its first ask.
//!
//! Which memory: private memory committed whole (opened as a part of its allocation: the pages are pinned until the
//! cell goes) and, since the title's 32-bit heap and most of what it shows the GPU are view memory, the title's
//! physical pages: views of sections. A section made `SEC_RESERVE` cannot be opened; with `VICTORY_ZEROCOPY` `xmem`
//! makes them `SEC_COMMIT` (whole, so a host that cannot commit that much at once gets them reserved as before), and
//! the host opens the whole section once, as one heap that locks nothing and does not mind its views being unmapped,
//! remapped or protected. A cell over view memory is a buffer at the section's offset of the run of views that holds
//! the ask (cut to the declared range): the GPU reads the title's pages, wherever they are mapped, and a write
//! through any view of them is seen.
//!
//! Fallbacks: a range whose memory the host refuses (a page of private memory guarded just then, a section made
//! reserved, the host's own refusal) stays a copy, and is tried again after 2, 4, 8.. times 120 frames, six times in
//! all; view memory that is not one run of one section where the ask is, is the buffer's copy's for 120 frames. What was a copy first and becomes a cell is told to everything that remembers the answer
//! ([`super::changed`]), so views made of the copy are made again of the cell. Cells go when the title frees or
//! decommits the memory ([`crate::inplace::title_memory_freeing`]) and the free waits for the lists that hold them.
//!
//! What it assumes: memory the title's CPU rewrites while the GPU still reads it is the title's to fence, as on the
//! console (the fences the title waits on are signalled when the card has run the work, not when it was sent).
//! The copy kept the bytes as they were at submission, whatever the title did after.

use super::*;
use std::cell::RefCell;
use std::collections::HashMap;
use std::sync::Weak;

/// The cells, by start.
static CELLS: RwLock<BTreeMap<u64, Arc<Shadow>>> = RwLock::new(BTreeMap::new());

/// Changes when a cell comes or goes or the declared ranges change: what a thread remembers is of before.
static EPOCH: AtomicU64 = AtomicU64::new(1);

/// The declared ranges changed ([`super::title_memory_gpu_readonly`], [`super::title_memory_gpu_writable`]).
pub(super) fn bump() {
    EPOCH.fetch_add(1, Ordering::SeqCst);
}

/// What is decided from the switches.
pub(crate) struct Policy {
    /// Frames a declared range is watched for before it is given (0: given at its first ask).
    pub(crate) observe: u64,
    /// The bytes a frame the title must write into it, sent up, for it to be given.
    pub(crate) min_up: u64,
    /// The most a cell holds: a declared range beyond it is cut in cells of this many bytes.
    pub(crate) cell: u64,
    /// The least a title buffer of an upload heap is (`VICTORY_ZEROCOPY=2`).
    pub(crate) min_buffer: u64,
    /// The most such a buffer is.
    pub(crate) max_buffer: u64,
    /// `VICTORY_ZEROCOPY_SIZES`: the exact sizes of title buffers (any heap) that are placed in place whatever else
    /// is true of them, for buffers the log shows nothing was ever fetched from (a decision of the pilot, per run).
    pub(crate) sizes: Vec<u64>,
}

impl Policy {
    pub(crate) fn get() -> &'static Policy {
        static P: std::sync::OnceLock<Policy> = std::sync::OnceLock::new();
        P.get_or_init(|| {
            let var = |name: &str, default: u64| std::env::var(name).ok().and_then(|v| v.trim().parse::<u64>().ok()).unwrap_or(default);
            Policy {
                observe: var("VICTORY_ZEROCOPY_OBSERVE", 90),
                min_up: var("VICTORY_ZEROCOPY_MIN_UP_KB", 16) << 10,
                cell: var("VICTORY_ZEROCOPY_CELL_MB", 64).max(1) << 20,
                min_buffer: var("VICTORY_ZEROCOPY_MIN_KB", 64) << 10,
                max_buffer: var("VICTORY_ZEROCOPY_BUFFER_MB", 64) << 20,
                sizes: std::env::var("VICTORY_ZEROCOPY_SIZES")
                    .map(|v| {
                        v.split(',')
                            .filter_map(|n| {
                                let n = n.trim();
                                n.strip_prefix("0x").map_or_else(|| n.parse().ok(), |h| u64::from_str_radix(h, 16).ok())
                            })
                            .collect()
                    })
                    .unwrap_or_default(),
            }
        })
    }
}

/// Whether a title buffer of `len` bytes in an upload heap is placed (`VICTORY_ZEROCOPY=2`).
pub(super) fn wants_buffer(len: u64) -> bool {
    let p = Policy::get();
    crate::inplace::zero_level() >= 2 && len >= p.min_buffer && len <= p.max_buffer
}

/// Whether a title buffer of `len` bytes is named by `VICTORY_ZEROCOPY_SIZES`.
pub(super) fn listed(len: u64) -> bool {
    crate::inplace::zero_level() >= 1 && Policy::get().sizes.contains(&len)
}

/// What is known of a declared range, by its start.
#[derive(Default, Clone, Debug, PartialEq)]
pub(crate) struct Watch {
    /// Bytes sent up into it since the first.
    pub(crate) bytes: u64,
    /// The frame of the first and of the last send into it.
    pub(crate) first: u64,
    pub(crate) last: u64,
    /// Times the host or the memory refused it, and the frame to try again at.
    pub(crate) failed: u32,
    pub(crate) retry: u64,
    /// Given: a cell stands (or stood, and the memory changed: then the watch starts over).
    pub(crate) given: bool,
    /// Where the declared range ends.
    pub(crate) end: u64,
}

/// How many times a refused range is tried, at 120 frames times 2, 4, 8..
const TRIES: u32 = 6;

static WATCH: Mutex<Option<HashMap<u64, Watch>>> = Mutex::new(None);

fn watches() -> std::sync::MutexGuard<'static, Option<HashMap<u64, Watch>>> {
    WATCH.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether a range with this watch is given now (`frame`).
pub(crate) fn due(p: &Policy, w: &Watch, frame: u64) -> bool {
    if w.failed >= TRIES || frame < w.retry {
        return false;
    }
    // Given already: the range was seen to be written, and the cells of its other runs follow.
    if w.given || p.observe == 0 {
        return true;
    }
    let seen = w.last.saturating_sub(w.first) + 1;
    w.bytes > 0 && seen >= p.observe && w.bytes / seen >= p.min_up
}

/// The declared range that holds all of `[address, end)`.
fn containing(address: u64, end: u64) -> Option<(u64, u64)> {
    let all = GPU_READONLY.read().unwrap_or_else(|e| e.into_inner());
    let i = all.partition_point(|&(_, b)| b <= address);
    all.get(i).copied().filter(|&(a, b)| a <= address && end <= b)
}

/// The gap of undeclared memory around `address`, and the declared range after or before it: what an ask that lies
/// in none is told about itself ([`Hot`] remembers it).
fn gap(address: u64, end: u64) -> (u64, u64) {
    let all = GPU_READONLY.read().unwrap_or_else(|e| e.into_inner());
    let i = all.partition_point(|&(_, b)| b <= address);
    match all.get(i) {
        // Inside a range, and not all of the ask: the ask alone.
        Some(&(a, _)) if a < end => (address, end),
        Some(&(a, _)) => (if i == 0 { 0 } else { all[i - 1].1 }, a),
        None => (all.last().map_or(0, |r| r.1), u64::MAX),
    }
}

/// The cell (start, bytes) of the declared range `(a, b)` that holds `address`.
pub(crate) fn cell_of(range: (u64, u64), address: u64, cell: u64) -> (u64, u64) {
    let (a, b) = range;
    if b - a <= cell {
        return (a, b - a);
    }
    let start = (address / cell * cell).max(a);
    (start, (start + cell).min(b) - start)
}

thread_local! {
    static ASKS: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

/// An ask was answered by a cell: counted by the thread, put in the shared count every 1,024.
fn asked() {
    ASKS.with(|c| {
        let n = c.get() + 1;
        if n >= 1024 {
            crate::inplace::ZERO[crate::inplace::Z_ASKS].fetch_add(n, Ordering::Relaxed);
            c.set(0);
        } else {
            c.set(n);
        }
    });
}

/// What a thread remembers of the asks it was answered, until [`EPOCH`] moves: a cell, or "not now" until a frame.
struct Hot {
    lo: u64,
    hi: u64,
    epoch: u64,
    until: u64,
    cell: Option<Weak<Shadow>>,
}

thread_local! {
    static HOT: RefCell<Vec<Hot>> = const { RefCell::new(Vec::new()) };
}

const HOT_MOST: usize = 8;

fn remember(hot: Hot) {
    HOT.with(|h| {
        let mut h = h.borrow_mut();
        if let Some(i) = h.iter().position(|x| (x.lo, x.hi) == (hot.lo, hot.hi)) {
            h[i] = hot;
        } else if h.len() < HOT_MOST {
            h.push(hot);
        } else {
            let at = (hot.lo >> 6) as usize % HOT_MOST;
            h[at] = hot;
        }
    });
}

/// The answer of a cell to an ask inside a declared range of title memory, if there is one (or it can be made now); `None`:
/// the ask is the buffer's copy's (or a window's).
pub(super) fn serve(address: u64, len: u64) -> Option<Arc<Shadow>> {
    let end = address.checked_add(len.max(1))?;
    // Nothing declared near: two loads.
    if GPU_READONLY_BOUNDS[1].load(Ordering::Relaxed) <= address || GPU_READONLY_BOUNDS[0].load(Ordering::Relaxed) >= end {
        return None;
    }
    let epoch = EPOCH.load(Ordering::SeqCst);
    let frame = crate::frame::current();
    let hot = HOT.with(|h| {
        h.borrow().iter().find(|x| x.epoch == epoch && x.lo <= address && end <= x.hi).map(|x| (x.until, x.cell.clone()))
    });
    match hot {
        Some((_, Some(cell))) => {
            if let Some(shadow) = cell.upgrade() {
                asked();
                return Some(shadow);
            }
        }
        Some((until, None)) if frame < until => return None,
        _ => {}
    }
    serve_with(address, end, frame, Policy::get(), &|start, len, a, e| crate::inplace::place_zero_cell(start, len, a, e))
}

/// What opens a cell: the declared range `(start, bytes)` and the ask inside it; the placed buffer and the start and
/// bytes it shows.
pub(crate) type Place<'a> = &'a dyn Fn(u64, u64, u64, u64) -> Result<(crate::inplace::Placed, u64, u64), crate::inplace::Refusal>;

/// [`serve`], slowly: the ask `[address, end)`, at `frame`, under `p`; `place` opens the memory `(start, bytes)`.
pub(crate) fn serve_with(
    address: u64,
    end: u64,
    frame: u64,
    p: &Policy,
    place: Place,
) -> Option<Arc<Shadow>> {
    let epoch = EPOCH.load(Ordering::SeqCst);
    // The cell is there already.
    {
        let cells = CELLS.read().unwrap_or_else(|e| e.into_inner());
        if let Some((_, cell)) = cells.range(..=address).next_back().filter(|(s, c)| **s + c.len >= end) {
            asked();
            remember(Hot { lo: cell.start, hi: cell.start + cell.len, epoch, until: 0, cell: Some(Arc::downgrade(cell)) });
            return Some(cell.clone());
        }
    }
    let Some(range) = containing(address, end) else {
        // In no declared range (or across the edge of one): not now, and not until the ranges change.
        let (lo, hi) = gap(address, end);
        remember(Hot { lo, hi, epoch, until: u64::MAX, cell: None });
        return None;
    };
    let (start, len) = cell_of(range, address, p.cell);
    if end > start + len {
        // Across two cells of a long range: the buffer's copy's.
        remember(Hot { lo: address, hi: end, epoch, until: u64::MAX, cell: None });
        return None;
    }
    {
        let mut all = watches();
        let w = all.get_or_insert_with(Default::default).entry(range.0).or_default();
        w.end = range.1;
        if !due(p, w, frame) {
            // Watched, or refused for now: ask again in a few frames.
            let until = if w.failed > 0 { w.retry } else { frame + 30 };
            drop(all);
            remember(Hot { lo: start, hi: start + len, epoch, until, cell: None });
            return None;
        }
    }
    let placed = place(start, len, address, end);
    let mut all = watches();
    let w = all.get_or_insert_with(Default::default).entry(range.0).or_default();
    let (placed, start, len) = match placed {
        Ok(cell) => cell,
        // View memory that is not one run of one section here: the buffer's copy's, and looked at again later.
        Err(crate::inplace::Refusal::Scattered) => {
            drop(all);
            remember(Hot { lo: address, hi: end, epoch, until: frame + 120, cell: None });
            return None;
        }
        Err(crate::inplace::Refusal::Memory) => {
            w.failed += 1;
            w.retry = frame + (120u64 << w.failed.min(8));
            let until = w.retry;
            drop(all);
            remember(Hot { lo: start, hi: start + len, epoch, until, cell: None });
            return None;
        }
    };
    w.given = true;
    w.failed = 0;
    w.retry = 0;
    drop(all);
    let shadow = Arc::new(Shadow::placed(placed, start, len, true));
    {
        let mut cells = CELLS.write().unwrap_or_else(|e| e.into_inner());
        // Two threads asked at once: the first stands.
        if let Some((_, other)) = cells.range(..=address).next_back().filter(|(s, c)| **s + c.len >= end) {
            return Some(other.clone());
        }
        cells.insert(start, shadow.clone());
        moved(7);
        // What was made of the buffer's copy of these bytes is made again of the cell.
        changed(start, start + len);
    }
    bump();
    asked();
    tracing::info!(
        start = format_args!("{start:#x}"),
        mb = len >> 20,
        "zero copy: title memory the title declares read-only to the GPU is the card's own bytes from now on, no copy"
    );
    Some(shadow)
}

/// Bytes are about to be sent up into the copy of `[start, ..)`, `ranges` of offsets: counts what goes into declared
/// ranges that are not given yet.
pub(crate) fn observe(start: u64, ranges: &[(usize, usize)]) {
    let (lo, hi) = (GPU_READONLY_BOUNDS[0].load(Ordering::Relaxed), GPU_READONLY_BOUNDS[1].load(Ordering::Relaxed));
    if hi == 0 {
        return;
    }
    let mut seen: [(u64, u64, u64); 8] = [(0, 0, 0); 8];
    let mut n = 0;
    {
        let all = GPU_READONLY.read().unwrap_or_else(|e| e.into_inner());
        for &(a, b) in ranges {
            let (from, to) = (start + a as u64, start + b as u64);
            if to <= lo || from >= hi {
                continue;
            }
            let mut i = all.partition_point(|&(_, y)| y <= from);
            while let Some(&(ra, rb)) = all.get(i) {
                if ra >= to {
                    break;
                }
                let bytes = to.min(rb) - from.max(ra);
                match seen[..n].iter().position(|s| s.0 == ra) {
                    Some(k) => seen[k].1 += bytes,
                    None if n < seen.len() => {
                        seen[n] = (ra, bytes, rb);
                        n += 1;
                    }
                    None => {}
                }
                i += 1;
            }
        }
    }
    if n == 0 {
        return;
    }
    let frame = crate::frame::current();
    let mut all = watches();
    let all = all.get_or_insert_with(Default::default);
    for &(range, bytes, range_end) in &seen[..n] {
        let w = all.entry(range).or_default();
        w.end = range_end;
        if w.bytes == 0 {
            w.first = frame;
        }
        w.bytes += bytes;
        w.last = frame;
    }
}

/// Takes the cells that hold any of `[at, end)` out of the tables (the shadows go when the last list that holds them
/// does), and the watches of the ranges there.
pub(crate) fn let_go(at: u64, end: u64) -> Vec<Arc<Shadow>> {
    let mut gone = Vec::new();
    {
        let mut cells = CELLS.write().unwrap_or_else(|e| e.into_inner());
        let keys: Vec<u64> = cells.iter().filter(|(s, c)| **s < end && **s + c.len > at).map(|(s, _)| *s).collect();
        for key in keys {
            gone.extend(cells.remove(&key));
        }
    }
    if let Some(all) = watches().as_mut() {
        // (A range that stood there starts over.)
        all.retain(|&start, w| !(start < end && w.end.max(start + 1) > at));
    }
    if !gone.is_empty() {
        bump();
    }
    gone
}

/// The cells, for the census: (start, bytes, true).
pub(super) fn spans() -> Vec<(u64, u64, bool)> {
    CELLS.read().unwrap_or_else(|e| e.into_inner()).iter().map(|(s, c)| (*s, c.len, true)).collect()
}

/// What the ten-second line says of the ranges.
pub(super) fn report() -> String {
    let ranges: Vec<(u64, u64)> = GPU_READONLY.read().unwrap_or_else(|e| e.into_inner()).clone();
    let all = watches();
    let frame = crate::frame::current();
    let mut parts = Vec::new();
    for (a, b) in ranges.iter().take(8) {
        let w = all.as_ref().and_then(|w| w.get(a)).cloned().unwrap_or_default();
        let seen = w.last.saturating_sub(w.first) + 1;
        parts.push(format!(
            "{a:#x}+{} MB: {}",
            (b - a) >> 20,
            if w.given {
                format!("given, after {:.0} KB a frame sent up over {seen} frames: what no longer goes", w.bytes as f64 / 1024.0 / seen as f64)
            } else if w.failed > 0 {
                format!("refused {} times, again at frame {}", w.failed, w.retry)
            } else {
                format!("watched, {:.0} KB a frame sent up over {seen} frames (frame {frame})", w.bytes as f64 / 1024.0 / seen as f64)
            }
        ));
    }
    format!("declared read-only to the GPU: {} ranges{}{}", ranges.len(), if parts.is_empty() { "" } else { ": " }, parts.join("; "))
}

/// What is known of a declared range (tests).
#[cfg(test)]
pub(crate) fn watch_of(start: u64) -> Option<Watch> {
    watches().as_ref().and_then(|w| w.get(&start)).cloned()
}
