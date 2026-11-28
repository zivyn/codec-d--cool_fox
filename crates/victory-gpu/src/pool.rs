//! Host memory for the textures the title fills, and for their staging
//! buffers, cut from blocks made once.
//!
//! Each such texture had an allocation of its own and its staging buffer
//! another. On an RX 7900 XTX an allocation takes about 310 microseconds
//! whatever its size, on one thread (`texture::allocation_costs`), and a
//! texture cut from a block 6: the pass that makes the textures shaders
//! pick by index (`draw.rs` `heap_textures`) got through 350-500 of the
//! 24,000 queued when the world loads in each of its 250 ms turns, some
//! forty seconds of a world without its textures.
//! `VICTORY_TEXTURE_POOL=0` makes every one an allocation of its own again.
//!
//! A piece given back is used again only two seconds later (nothing the card
//! was told before that is still running: Windows resets a card that takes
//! that long over one submission), and by then no list names it (the owners' rule, as for allocations of their own): a
//! texture's piece comes back when the last reference to the host texture
//! goes (an object of ours the texture holds says so), a staging buffer's
//! when the last list that recorded its copy is reset.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::ffi::c_void;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, AtomicUsize, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::{GUID, HRESULT, IUnknown, Interface};

/// Bytes of a block; pieces of more than half of one are not pooled (with
/// blocks made ahead, [`ahead`]: of more than one).
const BLOCK: u64 = 64 << 20;
/// Blocks with nothing in them that are kept.
const SPARE: usize = 2;
/// The same while the card has room ([`room`]): 4 GiB of them.
const SPARE_KEPT: usize = 64;
/// The same of blocks of staging buffers, which are the PC's memory: run
/// crash-1 held 5.2 GB of them.
const STAGING_KEPT: usize = 8;
/// How many are kept now.
static SPARES: AtomicUsize = AtomicUsize::new(SPARE);

/// Empty blocks of staging that may be held past the spare ones for a while
/// ([`staging_hold_ms`]): 2 GiB of them.
const HOLD_MOST: usize = 32;

/// How long an empty block of staging past the spare ones is held before it is
/// let go, in milliseconds (`VICTORY_STAGING_HOLD_MS`; 0, off: at once).
///
/// While the card stands over 85 % of its budget (every scene on a 4060 of 8 GB:
/// 7.75-7.95 of 7.19 GB) only [`SPARE`] empty blocks stay, and a scene entry that
/// sends up 1-2 GB of texture data a frame needs those blocks back within
/// seconds: 9-28 blocks of 64 MiB were let go and made again in every 5 s of
/// the minute of entry (run NAV2-a, the store: 0.6-1.8 GB of fresh upload
/// memory per 5 s, against 0-0.6 GB in a steady scene), each one a driver call
/// and 16,384 first-touch page faults of the 42-64k faults a frame the entry
/// shows (the process's kernel time 123 -> 497 ms a frame). Held, the blocks are
/// there for the next burst. Staging is the PC's memory, not the card's:
/// nothing is held under memory pressure ([`crate::residency::pressured`]) or
/// in the lean levels ([`crate::lean`]), and never more than [`HOLD_MOST`]
/// past the spare ones.
fn staging_hold_ms(strict: bool) -> u64 {
    if strict || crate::lean::level() >= 1 {
        return 0;
    }
    crate::ab::live_value!("VICTORY_STAGING_HOLD_MS", u64, |v| v
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or(0))
}

/// Sets how many empty blocks stay, by the card's memory (budget and use):
/// [`SPARE_KEPT`] below 85 % ([`crate::host::keep_freed`]), else [`SPARE`].
/// Asked once a second.
pub(crate) fn room(memory: Option<[u64; 4]>) {
    let pressure = crate::residency::pressured();
    let limit = spares(memory, crate::host::keep_freed() && !pressure);
    SPARES.store(limit, Ordering::Relaxed);
    let tick = now();
    // A full pool can become idle while another subsystem needs the VRAM.
    // A busy allocator observes the new limit on its next allocation or tick.
    if let Ok(mut pool) = TEXTURES.try_lock() {
        pool.reclaim(tick, limit, true);
    }
    if let Ok(mut pool) = STAGING.try_lock() {
        // (`VICTORY_STAGING_SPARE`, `VICTORY_RAM_LEAN`: fewer.)
        pool.most = crate::lean::staging_spare(STAGING_KEPT);
        let critical = crate::residency::critical_commit();
        pool.hold = staging_hold_ms(critical || pressure);
        let staging_limit = if critical {
            0
        } else if pressure {
            1
        } else {
            limit
        };
        pool.reclaim(tick, staging_limit, true);
    }
}

fn spares(memory: Option<[u64; 4]>, keep: bool) -> usize {
    match memory {
        Some([budget, used, ..]) if keep && u128::from(used) * 100 < u128::from(budget) * 85 => SPARE_KEPT,
        _ => SPARE,
    }
}

/// Milliseconds until a piece given back is used again. (It was three
/// frames, a tenth of a second while the title shows a loading screen at 30
/// a second, with up to 18 submissions in the air.)
const REST: u64 = 2500;

fn now() -> u64 {
    crate::host::now_ms()
}

pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_POOL").is_none_or(|v| v != "0"))
}

/// How many blocks of textures are made ahead, so that one has been there
/// for a while when its first piece is taken (`VICTORY_RIPE_BLOCKS`, 12;
/// 0: none, and new blocks are used at once, as before).
///
/// A page fault of lists already on the card, in a texture made the same
/// tenth of a second as the first piece of a block made for it, lost the
/// device in runs n08 and n11 (2026-11-25): the title gives a slot that
/// those lists read by index a new texture. Why the card does not have the
/// new block's addresses yet is not known (n11 waited for `MakeResident`;
/// `indirect::tests::new_blocks_under_readers` never loses it). A block that
/// every submission begun before its making has left behind ([`Ripe`]) has
/// no such lists: pieces are cut from those first.
fn ahead() -> usize {
    static AHEAD: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *AHEAD.get_or_init(|| std::env::var("VICTORY_RIPE_BLOCKS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(12))
}

/// Submissions begun and finished ([`crate::host::submissions`]), and how
/// many blocks to make ahead (0: every block is ripe at once).
#[derive(Clone, Copy)]
struct Ripe {
    started: u64,
    finished: u64,
    ahead: usize,
}

impl Ripe {
    const ALL: Ripe = Ripe { started: 0, finished: 0, ahead: 0 };

    fn now() -> Ripe {
        let (started, finished) = crate::host::submissions();
        // Not more than are kept ([`room`]): they would go and be made again.
        Ripe { started, finished, ahead: ahead().min(SPARES.load(Ordering::Relaxed)) }
    }

    fn is<B>(self, block: &Block<B>) -> bool {
        self.ahead == 0 || block.made <= self.finished
    }
}

/// Pieces cut from blocks that were not ripe, for want of a ripe one.
static UNRIPE: AtomicU64 = AtomicU64::new(0);

/// The free ranges of a block: start and length, never two that touch, and
/// their bytes in all.
struct Ranges {
    map: BTreeMap<u64, u64>,
    free: u64,
}

impl Ranges {
    fn new(size: u64) -> Ranges {
        Ranges { map: BTreeMap::from([(0, size)]), free: size }
    }

    /// Where the lowest place for `len` bytes at a multiple of `align` is, if
    /// there is one: what [`Ranges::take`] would take, without taking it.
    fn place(&self, len: u64, align: u64) -> Option<(u64, u64, u64)> {
        self.map.iter().find_map(|(&start, &size)| {
            let at = start.next_multiple_of(align);
            (at + len <= start + size).then_some((start, size, at))
        })
    }

    fn fits(&self, len: u64, align: u64) -> bool {
        self.place(len, align).is_some()
    }

    /// The lowest place for `len` bytes at a multiple of `align`.
    fn take(&mut self, len: u64, align: u64) -> Option<u64> {
        let (start, size, at) = self.place(len, align)?;
        self.map.remove(&start);
        if at > start {
            self.map.insert(start, at - start);
        }
        if at + len < start + size {
            self.map.insert(at + len, start + size - at - len);
        }
        self.free -= len;
        Some(at)
    }

    fn give(&mut self, mut start: u64, mut len: u64) {
        self.free += len;
        if let Some((&before, &size)) = self.map.range(..start).next_back()
            && before + size == start
        {
            self.map.remove(&before);
            (start, len) = (before, len + size);
        }
        if let Some(size) = self.map.remove(&(start + len)) {
            len += size;
        }
        self.map.insert(start, len);
    }

    fn whole(&self, size: u64) -> bool {
        self.map.get(&0) == Some(&size)
    }

    /// The bytes free in all, and the longest free range.
    fn free(&self) -> u64 {
        self.free
    }

    fn longest(&self) -> u64 {
        self.map.values().copied().max().unwrap_or(0)
    }
}

/// A part of a block.
#[derive(Clone, Copy, Debug, PartialEq)]
pub(crate) struct Piece {
    block: usize,
    start: u64,
    len: u64,
}

struct Block<B> {
    memory: B,
    free: Ranges,
    /// The end of the highest piece ever taken: above it the memory is as
    /// it was made (zeros).
    used_to: u64,
    /// The newest submission begun when it was made ([`Ripe`]).
    made: u64,
    /// When that was (ms).
    #[cfg_attr(not(feature = "diagnostics"), allow(dead_code))]
    since: u64,
    /// When a look found it empty (ms; 0: not seen empty since a piece was
    /// last taken from it), for [`Pool::hold`].
    empty_at: u64,
    /// When its emptying began (ms; 0: not being emptied): no piece is cut
    /// from it meanwhile ([`Pool::plan_drain`]).
    draining: u64,
    /// No emptying of it is begun before this (ms): it was tried and still
    /// held pieces that stayed.
    cool: u64,
}

struct Pool<B> {
    /// `None`: a block let go, its place to be used again.
    blocks: Vec<Option<Block<B>>>,
    /// Pieces given back, and when each may be used again.
    resting: VecDeque<(u64, Piece)>,
    /// The empty blocks it was last told to keep, and the most it keeps.
    spare: usize,
    most: usize,
    retry_growth_at: u64,
    /// How long (ms) an empty block past `spare` is held before it is let go
    /// (0: it goes at once).
    hold: u64,
    /// A new piece is cut from the fullest block that has room for it
    /// (`VICTORY_POOL_FIT`), not from the first.
    best: bool,
    /// When the pool last had to make a block (ms): the compaction waits
    /// until none was made for [`QUIET_MS`], so that it does not work while
    /// the textures of a world are still arriving.
    grown_at: u64,
    /// The pieces that stand for a texture of ours, by the texture's address:
    /// the block it lies in and its bytes ([`texture`]).
    owners: BTreeMap<usize, (usize, u64)>,
}

impl<B: Clone> Pool<B> {
    const fn new(most: usize) -> Pool<B> {
        Pool {
            blocks: Vec::new(),
            resting: VecDeque::new(),
            spare: 0,
            most,
            retry_growth_at: 0,
            hold: 0,
            best: false,
            grown_at: 0,
            owners: BTreeMap::new(),
        }
    }

    /// A piece of `len` bytes and the memory it lies in, and whether that
    /// part was ever used before; `make` makes a block when none has room.
    fn take(&mut self, len: u64, align: u64, now: u64, make: impl FnMut() -> Option<B>) -> Option<(Piece, B, bool)> {
        self.take_ripe(len, align, now, Ripe::ALL, make)
    }

    /// The same from a block that is ripe, if one has room; `make` makes one
    /// ahead too, when few are empty.
    fn take_ripe(
        &mut self,
        len: u64,
        align: u64,
        now: u64,
        ripe: Ripe,
        mut make: impl FnMut() -> Option<B>,
    ) -> Option<(Piece, B, bool)> {
        // With blocks made ahead, what would be an allocation of its own (and
        // new when the card first reads it) lies in one of those too.
        if len == 0 || len > if ripe.ahead > 0 { BLOCK } else { BLOCK / 2 } {
            return None;
        }
        self.rested(now);
        let mut found = None;
        if self.best {
            // The block with the least free that still has room: blocks fill
            // up one after the other, and those with a few pieces left in
            // them are the ones that empty.
            let fullest = self
                .blocks
                .iter()
                .enumerate()
                .filter_map(|(i, b)| Some((i, b.as_ref()?)))
                .filter(|(_, b)| b.draining == 0 && ripe.is(b) && b.free.fits(len, align))
                .min_by_key(|(i, b)| (b.free.free(), *i))
                .map(|(i, _)| i);
            if let Some(i) = fullest {
                found = self.blocks[i].as_mut().and_then(|b| b.free.take(len, align)).map(|start| (i, start));
            }
        } else {
            for (i, block) in self.blocks.iter_mut().enumerate() {
                if let Some(block) = block
                    && block.draining == 0
                    && ripe.is(block)
                    && let Some(start) = block.free.take(len, align)
                {
                    found = Some((i, start));
                    break;
                }
            }
        }
        if found.is_none() && ripe.ahead > 0 {
            // The one that has been there the longest.
            let mut unripe: Vec<(u64, usize)> = self
                .blocks
                .iter()
                .enumerate()
                .filter_map(|(i, b)| Some((b.as_ref().filter(|b| b.draining == 0)?.made, i)))
                .collect();
            unripe.sort_unstable();
            found = unripe.into_iter().find_map(|(_, i)| Some((i, self.blocks[i].as_mut()?.free.take(len, align)?)));
        }
        // A failed 64 MiB block must not trigger another driver call for each
        // small texture. Existing free ranges remain usable during backoff.
        let (i, start) = match found {
            Some(found) => found,
            None => {
                let memory = self.grow(now, &mut make)?;
                self.grown_at = now;
                let i = self.add(memory, ripe.started);
                (i, self.blocks[i].as_mut()?.free.take(len, align)?)
            }
        };
        if ripe.ahead > 0 {
            if !ripe.is(self.blocks[i].as_ref()?) {
                let n = UNRIPE.fetch_add(1, Ordering::Relaxed) + 1;
                if n.is_power_of_two() {
                    tracing::info!("textures cut from blocks made while lists ran that still run: {n}");
                }
            }
            let empty = self.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
            if empty < ripe.ahead
                && let Some(memory) = self.grow(now, &mut make)
            {
                self.grown_at = now;
                self.add(memory, ripe.started);
            }
        }
        let block = self.blocks[i].as_mut()?;
        let again = start < block.used_to;
        block.used_to = block.used_to.max(start + len);
        block.empty_at = 0;
        Some((Piece { block: i, start, len }, block.memory.clone(), again))
    }

    fn grow(&mut self, now: u64, make: &mut impl FnMut() -> Option<B>) -> Option<B> {
        if now < self.retry_growth_at {
            return None;
        }
        let block = make();
        if block.is_none() {
            self.retry_growth_at = now.saturating_add(500);
        }
        block
    }

    fn add(&mut self, memory: B, made: u64) -> usize {
        let i = match self.blocks.iter().position(Option::is_none) {
            Some(i) => i,
            None => {
                self.blocks.push(None);
                self.blocks.len() - 1
            }
        };
        self.blocks[i] =
            Some(Block {
                memory,
                free: Ranges::new(BLOCK),
                used_to: 0,
                made,
                since: crate::host::now_ms(),
                empty_at: 0,
                draining: 0,
                cool: 0,
            });
        i
    }

    fn give(&mut self, piece: Piece, now: u64) {
        self.resting.push_back((now + REST, piece));
    }

    /// Makes the pieces that rested long enough free, and lets go of empty
    /// blocks past [`SPARE`].
    fn rested(&mut self, now: u64) {
        self.reclaim(now, SPARES.load(Ordering::Relaxed), false);
    }

    fn reclaim(&mut self, now: u64, limit: usize, sweep: bool) {
        let mut any = false;
        while let Some(&(ready, piece)) = self.resting.front()
            && ready <= now
        {
            self.resting.pop_front();
            if let Some(block) = self.blocks[piece.block].as_mut() {
                block.free.give(piece.start, piece.len);
                any |= block.free.whole(BLOCK);
            }
        }
        // (Without a piece given back too, once there is less room.)
        let most = limit.min(self.most);
        let previous = std::mem::replace(&mut self.spare, most);
        if sweep || any || most < previous {
            let mut spare = 0;
            for block in &mut self.blocks {
                let Some(b) = block else { continue };
                if b.draining != 0 && b.free.whole(BLOCK) {
                    // Emptied: it goes like any empty block past the spare ones.
                    b.draining = 0;
                    COMPACTED[2].fetch_add(1, Ordering::Relaxed);
                }
                if !b.free.whole(BLOCK) {
                    b.empty_at = 0;
                    continue;
                }
                if b.empty_at == 0 {
                    b.empty_at = now.max(1);
                }
                spare += 1;
                // Past the spare ones a block goes when it has been empty for
                // the hold (at once without one, or when far past them).
                if spare > most
                    && (self.hold == 0 || now.saturating_sub(b.empty_at) >= self.hold || spare > most + HOLD_MOST)
                {
                    *block = None;
                }
            }
        }
    }

    fn blocks(&self) -> usize {
        self.blocks.iter().flatten().count()
    }

    /// Chooses blocks to empty, and marks them: no piece is cut from them
    /// from now on, and [`Pool::victims`] names the textures that lie in them.
    /// Returns how many blocks are being emptied.
    ///
    /// The free bytes of the blocks that stay (the empty ones among them) are
    /// the room for what is moved out and for what comes; each block emptied
    /// takes `BLOCK` of that room away, so the most that can go is what lies
    /// over `slack`. The blocks with the fewest bytes in use go first: they
    /// are the cheapest to empty. A block that has been tried for
    /// [`DRAIN_MS`] and still holds pieces is left alone for [`COOL_MS`]
    /// (some texture there cannot be moved).
    fn plan_drain(&mut self, now: u64, slack: u64, most: usize) -> usize {
        for b in self.blocks.iter_mut().flatten() {
            if b.draining != 0 && now.saturating_sub(b.draining) >= DRAIN_MS && !b.free.whole(BLOCK) {
                b.draining = 0;
                b.cool = now + COOL_MS;
                COMPACTED[3].fetch_add(1, Ordering::Relaxed);
            }
        }
        let draining = self.blocks.iter().flatten().filter(|b| b.draining != 0).count();
        if draining >= most || now.saturating_sub(self.grown_at) < QUIET_MS {
            return draining;
        }
        // The room in the blocks that stay, the empty ones too.
        let room: u64 = self.blocks.iter().flatten().filter(|b| b.draining == 0).map(|b| b.free.free()).sum();
        let live_out: u64 = self.blocks.iter().flatten().filter(|b| b.draining != 0).map(|b| BLOCK - b.free.free()).sum();
        let mut left = room.saturating_sub(live_out);
        let mut candidates: Vec<(u64, usize)> = self
            .blocks
            .iter()
            .enumerate()
            .filter_map(|(i, b)| {
                let b = b.as_ref()?;
                (b.draining == 0 && b.cool <= now && !b.free.whole(BLOCK)).then_some((BLOCK - b.free.free(), i))
            })
            .collect();
        candidates.sort_unstable();
        let mut now_draining = draining;
        for (_, i) in candidates {
            // Taking this block out takes its room away and puts its pieces in the rest.
            let after = left.saturating_sub(BLOCK);
            if now_draining >= most || after < slack || left < BLOCK {
                break;
            }
            if let Some(b) = self.blocks[i].as_mut() {
                b.draining = now.max(1);
            }
            COMPACTED[0].fetch_add(1, Ordering::Relaxed);
            left = after;
            now_draining += 1;
        }
        now_draining
    }

    /// The addresses of the textures that lie in blocks being emptied
    /// ([`Pool::plan_drain`]).
    fn victims(&self) -> BTreeSet<usize> {
        self.owners
            .iter()
            .filter(|(_, (block, _))| self.blocks.get(*block).and_then(Option::as_ref).is_some_and(|b| b.draining != 0))
            .map(|(&raw, _)| raw)
            .collect()
    }

    /// The pool by how full its blocks are, for the line.
    fn report(&self) -> BlocksReport {
        let mut out = BlocksReport::default();
        for b in self.blocks.iter().flatten() {
            let free = b.free.free();
            out.blocks += 1;
            out.free += free;
            out.longest = out.longest.max(b.free.longest());
            out.draining += u32::from(b.draining != 0);
            let at = if free >= BLOCK {
                0
            } else if free * 4 >= BLOCK * 3 {
                1
            } else if free * 2 >= BLOCK {
                2
            } else if free * 4 >= BLOCK {
                3
            } else {
                4
            };
            out.by_free[at] += 1;
            out.ranges += b.free.map.len() as u64;
        }
        out
    }
}

/// How long the pool must have made no block before the compaction begins
/// to empty one (ms).
const QUIET_MS: u64 = 10_000;
/// How long a block is being emptied before it is given up for a while (ms).
const DRAIN_MS: u64 = 60_000;
/// How long a block that was given up is left alone (ms).
const COOL_MS: u64 = 120_000;

/// What the pool of texture blocks looks like: blocks, the bytes free in
/// them, how many are wholly free, three quarters, a half, a quarter free
/// and less; the free ranges in all and the longest; blocks being emptied.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct BlocksReport {
    pub(crate) blocks: u32,
    pub(crate) free: u64,
    pub(crate) by_free: [u32; 5],
    pub(crate) ranges: u64,
    pub(crate) longest: u64,
    pub(crate) draining: u32,
}

/// Blocks chosen to be emptied, textures moved out of them (made again
/// elsewhere), the bytes of those, blocks that got empty, blocks given up.
pub(crate) static COMPACTED: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

/// A mapped upload buffer's memory.
#[derive(Clone)]
struct Upload(ID3D12Resource, *mut u8);
// SAFETY: the mapping is written by the owners of its pieces, each its own.
unsafe impl Send for Upload {}

static TEXTURES: Mutex<Pool<ID3D12Heap>> = Mutex::new(Pool::new(SPARE_KEPT));
static STAGING: Mutex<Pool<Upload>> = Mutex::new(Pool::new(STAGING_KEPT));

fn lock<T>(m: &'static Mutex<T>) -> std::sync::MutexGuard<'static, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Bytes of the pieces textures hold now.
static HELD: AtomicU64 = AtomicU64::new(0);

/// Bytes of the texture blocks no texture holds: the card's memory in use
/// by us that new textures take first.
pub(crate) fn texture_room() -> u64 {
    (lock(&TEXTURES).blocks() as u64 * BLOCK).saturating_sub(HELD.load(Ordering::Relaxed))
}

/// Whether the blocks of textures are kept dense (`VICTORY_POOL_COMPACT=1`;
/// off by default): textures are cut from the fullest block with room, and
/// while the card is under pressure the blocks with the fewest bytes in use
/// are emptied, their textures made again in the others
/// ([`Pool::plan_drain`], `resource::compact_pool`).
///
/// Textures that came and went left the blocks half empty: in run ARCH2R-a
/// of 3,328 MB of blocks 1,813 MB were free at the window spot, spread over
/// 52 blocks of 64 MB, and no block empties while one of its pieces lives.
pub(crate) fn compact_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_POOL_COMPACT").is_some_and(|v| v == "1"))
}

/// Whether a piece is cut from the fullest block that has room for it
/// (`VICTORY_POOL_FIT=best`, or the compaction): the blocks that stay
/// half empty then are the ones that were never filled, not every one.
pub(crate) fn fit_best() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| compact_on() || std::env::var_os("VICTORY_POOL_FIT").is_some_and(|v| v == "best"))
}

/// The bytes the blocks of textures may keep free past the empty spare ones
/// ([`SPARE`]) before the compaction empties a block: `VICTORY_POOL_SLACK_MB`,
/// 256 by default (room for the textures that come while it works).
fn slack() -> u64 {
    static MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MB.get_or_init(|| std::env::var("VICTORY_POOL_SLACK_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256)) << 20
}

/// How many bytes of textures the compaction makes again each second:
/// `VICTORY_POOL_COMPACT_MB`, 48 by default (each is read from title memory
/// and sent up again).
pub(crate) fn compact_bytes() -> u64 {
    static MB: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MB.get_or_init(|| std::env::var("VICTORY_POOL_COMPACT_MB").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(48)) << 20
}

/// The most blocks being emptied at once.
const DRAIN_AT_ONCE: usize = 4;

/// With the compaction on and the card under pressure (the empty blocks kept
/// are [`SPARE`] or fewer): decides which blocks to empty, and says which
/// textures lie in them, by address. `None`: nothing to do now.
pub(crate) fn compact_step(now: u64) -> Option<BTreeSet<usize>> {
    if !compact_on() || SPARES.load(Ordering::Relaxed) > SPARE {
        return None;
    }
    let mut pool = lock(&TEXTURES);
    pool.plan_drain(now, slack(), DRAIN_AT_ONCE);
    let victims = pool.victims();
    (!victims.is_empty()).then_some(victims)
}

/// The blocks of textures by how full they are (the census line).
pub(crate) fn blocks_report() -> BlocksReport {
    lock(&TEXTURES).report()
}

/// Bytes of the pieces textures hold now.
pub(crate) fn held() -> u64 {
    HELD.load(Ordering::Relaxed)
}

/// Bytes of the blocks of textures, and of the blocks of staging buffers (the
/// PC's memory), for the vram line.
pub(crate) fn texture_bytes() -> u64 {
    lock(&TEXTURES).blocks() as u64 * BLOCK
}

pub(crate) fn staging_bytes() -> u64 {
    lock(&STAGING).blocks() as u64 * BLOCK
}

/// For the memory line: the blocks of staging buffers, and of them how many
/// are empty (kept for the next ask).
#[cfg(feature = "diagnostics")]
pub(crate) fn staging_empty() -> [u64; 2] {
    let pool = lock(&STAGING);
    let empty = pool.blocks.iter().flatten().filter(|b| b.free.whole(BLOCK)).count();
    [pool.blocks() as u64, empty as u64]
}

/// For the memory line: blocks of textures and of staging buffers, in MiB.
#[cfg(feature = "diagnostics")]
pub(crate) fn memory() -> [u64; 2] {
    [lock(&TEXTURES).blocks() as u64 * (BLOCK >> 20), lock(&STAGING).blocks() as u64 * (BLOCK >> 20)]
}

/// Part of a block of mapped upload memory, back in the pool when dropped.
pub(crate) struct StagingPiece(Piece);

impl Drop for StagingPiece {
    fn drop(&mut self) {
        lock(&STAGING).give(self.0, now());
    }
}

/// `bytes` of upload memory, all zeros: the buffer it lies in, where in
/// that buffer, and its mapping. `None`: not pooled.
pub(crate) fn staging(host: &crate::host::Host, bytes: u64) -> Option<(StagingPiece, ID3D12Resource, u64, *mut u8)> {
    if !on() {
        return None;
    }
    // Copies from a buffer begin at multiples of 512.
    let bytes = bytes.next_multiple_of(512);
    let (piece, memory, again) = lock(&STAGING).take(bytes, 512, now(), || {
        // Only gate speculative bindless prefetch on staging-pool growth
        // headroom; table-bound textures and decoration bakes are sampled
        // immediately and would otherwise read uninitialized/reused VRAM.
        let speculative = crate::texture::PICKING.get().is_some() && !crate::texture::BAKING.get();
        if speculative && !crate::residency::allow_staging_growth(false) {
            return None;
        }
        let (buffer, data) = host.upload_buffer(BLOCK)?;
        crate::host::name(&buffer, || "staging of textures (a block)".into());
        Some(Upload(buffer, data))
    })?;
    let owned = StagingPiece(piece);
    // SAFETY: the piece lies in the block's mapping.
    let data = unsafe { memory.1.add(piece.start as usize) };
    if again {
        // What is not detiled into it (an unmapped texture) uploads zeros,
        // as from a buffer of its own.
        // SAFETY: the piece is ours alone.
        unsafe { std::ptr::write_bytes(data, 0, bytes as usize) };
    }
    Some((owned, memory.0, piece.start, data))
}

/// An object the host texture holds and releases when it goes: its piece
/// is free then.
#[repr(C)]
struct Reclaim {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    piece: Piece,
    /// The texture's address, as [`Pool::owners`] has it.
    raw: usize,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}

static RECLAIM: Methods = Methods([query as *const (), add_ref as *const (), release as *const ()]);

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> HRESULT {
    const E_NOINTERFACE: HRESULT = HRESULT(0x8000_4002_u32 as i32);
    // SAFETY: COM's arguments.
    unsafe {
        if out.is_null() {
            return HRESULT(0x8000_4003_u32 as i32);
        }
        if iid.as_ref().is_some_and(|iid| *iid == IUnknown::IID) {
            add_ref(this);
            *out = this;
            return HRESULT(0);
        }
        *out = std::ptr::null_mut();
    }
    E_NOINTERFACE
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe { (*this.cast::<Reclaim>()).references.fetch_add(1, Ordering::Relaxed) + 1 }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Reclaim>()).references.fetch_sub(1, Ordering::AcqRel) - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Reclaim>());
            HELD.fetch_sub(gone.piece.len, Ordering::Relaxed);
            let mut pool = lock(&TEXTURES);
            pool.owners.remove(&gone.raw);
            pool.give(gone.piece, now());
        }
        left
    }
}

/// Textures made in the last second ([`fresh`]), and when.
static RECENT: Mutex<VecDeque<(usize, u64)>> = Mutex::new(VecDeque::new());

/// Whether `texture` was made in a block in the last second.
pub(crate) fn fresh(texture: &ID3D12Resource) -> bool {
    let raw = texture.as_raw() as usize;
    let since = now().saturating_sub(1000);
    RECENT.lock().unwrap_or_else(|e| e.into_inner()).iter().rev().take_while(|r| r.1 >= since).any(|r| r.0 == raw)
}

fn made_now(texture: &ID3D12Resource) {
    let now = now();
    let mut recent = RECENT.lock().unwrap_or_else(|e| e.into_inner());
    while recent.front().is_some_and(|r| r.1 + 1000 < now) {
        recent.pop_front();
    }
    recent.push_back((texture.as_raw() as usize, now));
}

/// Makes `texture` give `piece` back when the last reference to it goes.
fn give_back_with(texture: &ID3D12Resource, piece: Piece) {
    const KEY: GUID = GUID::from_u128(0x6f0e2f0c_5a59_4b1e_9a3f_7f4d2c9be713);
    let raw = texture.as_raw() as usize;
    lock(&TEXTURES).owners.insert(raw, (piece.block, piece.len));
    let made = Box::into_raw(Box::new(Reclaim { methods: &RECLAIM.0, references: AtomicU32::new(1), piece, raw }));
    // SAFETY: a COM object of ours with one reference, which this takes and
    // drops at the end; the texture takes its own.
    unsafe {
        let object = IUnknown::from_raw(made.cast());
        if texture.SetPrivateDataInterface(&KEY, &object).is_err() {
            // Never given back: the piece stays taken, nothing is reused
            // under a texture that may be alive.
            std::mem::forget(object);
        }
    }
}

/// A texture of `desc` (no target, depth or unordered access) in a block;
/// `None`: not pooled, or the host refused.
///
/// # Safety
/// `desc` is a valid description.
pub(crate) unsafe fn texture(
    device: &ID3D12Device,
    desc: *const D3D12_RESOURCE_DESC,
    state: D3D12_RESOURCE_STATES,
) -> Option<ID3D12Resource> {
    if !on() {
        return None;
    }
    // Small textures lie at multiples of 4 KiB where the host allows it
    // (the top mip is 64 KiB at most): 64 KiB each otherwise.
    // SAFETY: the caller's description.
    let mut d = unsafe { *desc };
    d.Alignment = u64::from(D3D12_SMALL_RESOURCE_PLACEMENT_ALIGNMENT);
    // SAFETY: plain queries.
    let mut info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    if info.SizeInBytes == u64::MAX || info.Alignment != d.Alignment {
        d.Alignment = 0;
        info = unsafe { device.GetResourceAllocationInfo(0, &[d]) };
    }
    if info.SizeInBytes == u64::MAX {
        return None;
    }
    #[cfg_attr(not(feature = "diagnostics"), allow(unused_variables))]
    let (piece, heap, again) = {
        let mut pool = lock(&TEXTURES);
        pool.best = fit_best();
        pool
    }
    .take_ripe(info.SizeInBytes, info.Alignment.max(1), now(), Ripe::now(), || {
        let desc = D3D12_HEAP_DESC {
            SizeInBytes: BLOCK,
            Properties: D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
            Alignment: 0,
            Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES | crate::host::not_resident(),
        };
        let mut heap: Option<ID3D12Heap> = None;
        // SAFETY: plain creation.
        if let Err(e) = unsafe { device.CreateHeap(&desc, &mut heap) } {
            crate::residency::allocation_error(&e);
            tracing::warn!(error = %e, "no block of texture memory");
        }
        if let Some(h) = &heap
            && let Err(e) = crate::host::resident(device, h).and_then(|()| crate::host::mapped(device))
        {
            tracing::warn!(error = %e, "a block of texture memory is not resident");
            return None;
        }
        if let Some(h) = &heap {
            // Blocks hold the render targets and the textures made whole.
            crate::vram::prioritize(device, h, BLOCK, crate::vram::Class::High);
        }
        heap
    })?;
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: the piece is no one else's; the heap lives while a piece of
    // it is taken.
    let result = unsafe { device.CreatePlacedResource(&heap, piece.start, &d, state, None, &mut made) };
    match (result, made) {
        (Ok(()), Some(made)) => {
            HELD.fetch_add(piece.len, Ordering::Relaxed);
            give_back_with(&made, piece);
            made_now(&made);
            #[cfg(feature = "diagnostics")]
            {
                crate::book::made(&made, || "a texture in a block".into());
                crate::book::block(&heap, &made, piece.start, BLOCK);
                crate::book::note(made.as_raw() as usize, || {
                    let block = lock(&TEXTURES).blocks.get(piece.block).and_then(Option::as_ref).map(|b| (b.made, b.since));
                    let (submission, since) = block.unwrap_or_default();
                    format!(
                        "; piece {:#x}+{:#x} of block {} {:#x} made {:.1} s before it at submission {submission}, used before: {again}, made at submissions {:?}",
                        piece.start,
                        piece.len,
                        piece.block,
                        heap.as_raw() as usize,
                        now().saturating_sub(since) as f64 / 1e3,
                        crate::host::submissions()
                    )
                });
            }
            Some(made)
        }
        _ => {
            lock(&TEXTURES).give(piece, now());
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ranges_are_cut_aligned_and_join_again() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.take(100, 64), Some(0));
        assert_eq!(r.take(100, 64), Some(128));
        assert_eq!(r.take(28, 4), Some(100));
        assert_eq!(r.take(2000, 4), None);
        r.give(128, 100);
        r.give(0, 100);
        assert!(!r.whole(1024));
        r.give(100, 28);
        assert!(r.whole(1024));
    }

    #[test]
    fn empty_blocks_stay_while_the_card_has_room() {
        assert_eq!(spares(Some([100, 84, 0, 0]), true), SPARE_KEPT);
        assert_eq!(spares(Some([100, 85, 0, 0]), true), SPARE);
        assert_eq!(spares(Some([100, 10, 0, 0]), false), SPARE);
        assert_eq!(spares(None, true), SPARE);
    }

    #[test]
    fn a_failed_block_is_not_retried_for_each_small_upload() {
        let mut pool: Pool<u32> = Pool::new(2);
        let calls = std::cell::Cell::new(0);
        let refuse = || { calls.set(calls.get() + 1); None };
        for size in [704008, 179720, 179720, 24080, 48648, 524288, 89616, 351760] {
            assert!(pool.take(size, 512, 12, refuse).is_none());
        }
        assert_eq!(calls.get(), 1);
        assert!(pool.take(24080, 512, 1012, || Some(1)).is_some());
    }

    #[test]
    fn budget_tick_reclaims_idle_blocks_without_reusing_live_or_resting_pieces() {
        let mut pool = Pool::new(STAGING_KEPT);
        let (held, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(1)).unwrap();
        let (resting, _, _) = pool.take(BLOCK / 2, 1, 0, || Some(2)).unwrap();
        pool.give(resting, 0);
        for i in 0..12 {
            pool.add(i, 0);
        }
        pool.reclaim(0, SPARE_KEPT, true);
        assert_eq!(pool.blocks(), STAGING_KEPT + 1);
        pool.reclaim(REST - 1, SPARE, true);
        assert_eq!(pool.blocks(), SPARE + 1);
        assert_eq!(pool.resting.len(), 1);
        pool.reclaim(REST, 0, true);
        assert_eq!(pool.blocks(), 1);
        assert!(pool.resting.is_empty());
        pool.give(held, REST);
        pool.reclaim(2 * REST, 0, true);
        assert_eq!(pool.blocks(), 0);
    }

    /// With a hold, the empty blocks past the spare ones stay until they have
    /// been empty that long; without one they go at the first look.
    #[test]
    fn empty_blocks_past_the_spare_ones_are_held_for_the_hold() {
        let mut pool: Pool<u32> = Pool::new(STAGING_KEPT);
        pool.hold = 5000;
        for i in 0..6 {
            pool.add(i, 0);
        }
        // First seen empty at 1000.
        pool.reclaim(1000, 2, true);
        assert_eq!(pool.blocks(), 6);
        pool.reclaim(5999, 2, true);
        assert_eq!(pool.blocks(), 6);
        pool.reclaim(6000, 2, true);
        assert_eq!(pool.blocks(), 2);
        // The same without a hold: at the first look.
        let mut plain: Pool<u32> = Pool::new(STAGING_KEPT);
        for i in 0..6 {
            plain.add(i, 0);
        }
        plain.reclaim(1000, 2, true);
        assert_eq!(plain.blocks(), 2);
    }

    /// A block that is used begins its hold again when it is empty again; one in use is never let go.
    #[test]
    fn a_held_block_that_is_used_begins_its_hold_again() {
        let mut pool: Pool<u32> = Pool::new(STAGING_KEPT);
        pool.hold = 5000;
        for i in 0..3 {
            pool.add(i, 0);
        }
        // All three seen empty at 1000: one is the spare one, two are held.
        pool.reclaim(1000, 1, true);
        assert_eq!(pool.blocks(), 3);
        // A piece is taken from the first block (the lowest is used first).
        let (piece, _, _) = pool.take(1024, 512, 4000, || None).unwrap();
        assert_eq!(piece.block, 0);
        // Held since 1000 for 5000 ms: the third goes, the second is the spare one, the first is in use.
        pool.reclaim(7000, 1, true);
        assert_eq!(pool.blocks(), 2);
        // The piece comes back (it rests REST ms): the first block is empty at 9500 and is the first
        // to be counted, so it is the spare one, and the second (empty since 1000) goes.
        pool.give(piece, 7000);
        pool.reclaim(7000 + REST, 1, true);
        assert_eq!(pool.blocks(), 1);
        // A new block is held from its first look.
        pool.add(1, 0);
        pool.reclaim(10_000, 1, true);
        assert_eq!(pool.blocks(), 2);
        pool.reclaim(14_999, 1, true);
        assert_eq!(pool.blocks(), 2);
        pool.reclaim(15_000, 1, true);
        assert_eq!(pool.blocks(), 1);
    }

    /// Far past the spare ones nothing is held: the hold has a cap.
    #[test]
    fn the_hold_has_a_cap() {
        let mut pool: Pool<u32> = Pool::new(STAGING_KEPT);
        pool.hold = 60_000;
        for i in 0..(HOLD_MOST + 10) as u32 {
            pool.add(i, 0);
        }
        pool.reclaim(1000, 2, true);
        assert_eq!(pool.blocks(), 2 + HOLD_MOST);
    }

    /// Under memory pressure nothing is held, whatever the switch says.
    #[test]
    fn the_hold_is_off_under_pressure() {
        assert_eq!(staging_hold_ms(true), 0);
    }

    #[test]
    fn pieces_never_overlap_and_rest_before_they_are_used_again() {
        let mut pool: Pool<u32> = Pool::new(SPARE);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let mut taken: Vec<Piece> = Vec::new();
        for i in 0..500u64 {
            let (p, _, again) = pool.take(65536 * (1 + i % 7), 65536, 0, make).unwrap();
            assert!(!again);
            assert!(taken.iter().all(|q| q.block != p.block || p.start + p.len <= q.start || q.start + q.len <= p.start));
            taken.push(p);
        }
        let blocks = pool.blocks();
        assert!(blocks >= 2);
        // Given back at 10: not used again before 10 + REST.
        let first = taken[0];
        pool.give(first, 10);
        let (p, _, _) = pool.take(first.len, 65536, 10 + REST - 1, make).unwrap();
        assert_ne!((p.block, p.start), (first.block, first.start));
        let (q, _, again) = pool.take(first.len, 65536, 10 + REST, make).unwrap();
        assert_eq!((q.block, q.start, again), (first.block, first.start, true));
        // Everything back: the empty blocks past the spare ones go.
        for p in taken.drain(1..).chain([p, q]) {
            pool.give(p, 20);
        }
        pool.rested(20 + REST);
        assert_eq!(pool.blocks(), blocks.min(SPARE));
        // Too big for a block, and a block that cannot be made.
        assert!(pool.take(BLOCK, 65536, 30, make).is_none());
        let mut none: Pool<u32> = Pool::new(SPARE);
        assert!(none.take(65536, 65536, 0, || None).is_none());
    }

    #[test]
    fn pieces_come_from_blocks_older_than_the_lists_that_run() {
        let mut pool: Pool<u64> = Pool::new(SPARE_KEPT);
        let made = std::cell::Cell::new(0);
        let make = || {
            made.set(made.get() + 1);
            Some(made.get())
        };
        let at = |started, finished| Ripe { started, finished, ahead: 2 };
        let take = |pool: &mut Pool<u64>, ripe| pool.take_ripe(BLOCK / 2, 65536, 0, ripe, make).unwrap().1;
        // Nothing runs: at once, and two are made ahead, one a time.
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 2);
        assert_eq!(take(&mut pool, at(0, 0)), 1);
        assert_eq!(made.get(), 3);
        // 4 is made while submission 5 runs.
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(made.get(), 4);
        assert_eq!(take(&mut pool, at(5, 4)), 2);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        assert_eq!(take(&mut pool, at(6, 4)), 3);
        // None is ripe: the oldest; it is ripe once 5 has finished.
        assert_eq!(take(&mut pool, at(7, 4)), 4);
        assert_eq!(take(&mut pool, at(7, 5)), 4);
        // A whole block's worth too.
        assert_eq!(pool.take_ripe(BLOCK, 65536, 0, at(8, 7), make).unwrap().1, 5);
        // Off: the first with room, whatever its age, none ahead.
        let n = made.get();
        assert_eq!(pool.take(BLOCK / 2, 65536, 0, make).unwrap().1, 6);
        assert_eq!(made.get(), n);
        assert!(pool.take(BLOCK, 65536, 0, make).is_none());
    }

    /// A pooled texture's piece comes back when its last reference goes, not
    /// before; its staging piece begins as zeros.
    #[test]
    fn a_texture_gives_its_piece_back_when_it_goes() {
        let Some(host) = crate::host::host() else { return };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: 256,
            Height: 256,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            ..Default::default()
        };
        // SAFETY: a valid description.
        let Some(t) = (unsafe { texture(&host.device, &desc, D3D12_RESOURCE_STATE_COMMON) }) else { return };
        assert_eq!(crate::host::references(&t), 1, "what the texture holds is no reference to it");
        let resting = || lock(&TEXTURES).resting.len();
        let before = resting();
        let other = t.clone();
        drop(t);
        assert_eq!(resting(), before);
        drop(other);
        assert_eq!(resting(), before + 1);
        // SAFETY: the piece's mapping, 4096 bytes of it ours.
        unsafe {
            let (piece, _, at, data) = staging(host, 4096).unwrap();
            assert!(at.is_multiple_of(512));
            std::ptr::write_bytes(data, 0xAB, 4096);
            drop(piece);
            lock(&STAGING).rested(u64::MAX);
            let (_piece, _, _, data) = staging(host, 4096).unwrap();
            assert!((0..4096).all(|i| *data.add(i) == 0));
            // SAFETY: plain query.
            assert!(host.device.GetDeviceRemovedReason().is_ok());
        }
    }

    const MB: u64 = 1 << 20;
    /// A time long after the pool last made a block.
    const T0: u64 = 100_000;

    /// A pool of `n` empty blocks with `live[i]` bytes taken in block `i` (one piece each, by hand).
    fn pool_with(live: &[u64]) -> Pool<u32> {
        let mut pool: Pool<u32> = Pool::new(SPARE_KEPT);
        for (i, &bytes) in live.iter().enumerate() {
            let at = pool.add(i as u32, 0);
            if bytes > 0 {
                pool.blocks[at].as_mut().unwrap().free.take(bytes, 1).unwrap();
            }
        }
        pool
    }

    #[test]
    fn free_bytes_follow_what_is_taken_and_given() {
        let mut r = Ranges::new(1024);
        assert_eq!(r.free(), 1024);
        let a = r.take(100, 64).unwrap();
        let b = r.take(200, 64).unwrap();
        // The bytes between the pieces that alignment left stay free.
        assert_eq!(r.free(), 1024 - 300);
        r.give(a, 100);
        assert_eq!(r.free(), 1024 - 200);
        r.give(b, 200);
        assert_eq!(r.free(), 1024);
        assert!(r.whole(1024));
        assert_eq!(r.longest(), 1024);
    }

    #[test]
    fn a_piece_is_cut_from_the_fullest_block_that_has_room_for_it() {
        let mut pool = pool_with(&[40 * MB, 54 * MB, 0]);
        let make = || None::<u32>;
        // The first block with room is block 0; the fullest with room is block 1.
        pool.best = false;
        let (first, _, _) = pool.take(8 * MB, 1, 0, make).unwrap();
        assert_eq!(first.block, 0);
        pool.best = true;
        let (best, _, _) = pool.take(8 * MB, 1, 0, make).unwrap();
        assert_eq!(best.block, 1);
        // What no fuller block has room for goes to the next fullest, and an empty block comes last.
        let (more, _, _) = pool.take(16 * MB, 1, 0, make).unwrap();
        assert_eq!(more.block, 0);
        let (large, _, _) = pool.take(30 * MB, 1, 0, make).unwrap();
        assert_eq!(large.block, 2);
    }

    #[test]
    fn nothing_is_cut_from_a_block_being_emptied() {
        let mut pool = pool_with(&[10 * MB, 10 * MB]);
        pool.blocks[0].as_mut().unwrap().draining = 5;
        let (piece, _, _) = pool.take(MB, 1, 0, || None::<u32>).unwrap();
        assert_eq!(piece.block, 1);
        pool.best = true;
        let (piece, _, _) = pool.take(MB, 1, 0, || None::<u32>).unwrap();
        assert_eq!(piece.block, 1);
        // With the other one full a new block is made, not block 0 used.
        let rest = pool.blocks[1].as_ref().unwrap().free.free();
        pool.blocks[1].as_mut().unwrap().free.take(rest, 1).unwrap();
        let (piece, _, _) = pool.take(MB, 1, 0, || Some(7)).unwrap();
        assert_eq!(piece.block, 2);
    }

    #[test]
    fn the_sparsest_blocks_are_emptied_while_the_rest_have_room_for_them_and_the_slack() {
        // 8, 8, 40, 40, 40, 40 MB in use: 208 MB free in all.
        let live = [8 * MB, 8 * MB, 40 * MB, 40 * MB, 40 * MB, 40 * MB];
        let mut pool = pool_with(&live);
        for (i, &bytes) in live.iter().enumerate() {
            pool.owners.insert(0x1000 + i, (i, bytes));
        }
        // With 64 MB of slack two blocks go: 208 - 128 = 80 free are left, of them 16 are to be filled
        // with what comes out, and a third block would leave 16.
        assert_eq!(pool.plan_drain(T0, 64 * MB, 4), 2);
        assert!(pool.blocks[0].as_ref().unwrap().draining != 0);
        assert!(pool.blocks[1].as_ref().unwrap().draining != 0);
        assert!(pool.blocks[2].as_ref().unwrap().draining == 0);
        assert_eq!(pool.victims(), BTreeSet::from([0x1000, 0x1001]));
        // Asked again: the same two, and no third.
        assert_eq!(pool.plan_drain(T0 + 1000, 64 * MB, 4), 2);
        // Without slack a third one goes; at most `most` are at it.
        assert_eq!(pool.plan_drain(T0 + 2000, 0, 3), 3);
        assert_eq!(pool.plan_drain(T0 + 2000, 0, 3), 3);
    }

    #[test]
    fn nothing_is_emptied_while_blocks_are_still_being_made() {
        let mut pool = pool_with(&[8 * MB, 8 * MB, 8 * MB, 8 * MB]);
        // A block was made when a texture found no room, a moment ago.
        pool.grown_at = T0 - QUIET_MS + 1;
        assert_eq!(pool.plan_drain(T0, 0, 4), 0);
        assert_eq!(pool.plan_drain(T0 + 1, 0, 4), 3);
    }

    #[test]
    fn nothing_is_emptied_when_little_is_free() {
        // Four blocks 60 MB in use: 16 MB free in all.
        let mut pool = pool_with(&[60 * MB; 4]);
        assert_eq!(pool.plan_drain(T0, 0, 4), 0);
        // Plenty free, but the slack asked for is more than there is to spare.
        let mut pool = pool_with(&[8 * MB, 8 * MB]);
        assert_eq!(pool.plan_drain(T0, 100 * MB, 4), 0);
        // Empty blocks are not emptied.
        let mut pool = pool_with(&[0, 0, 0]);
        assert_eq!(pool.plan_drain(T0, 0, 4), 0);
    }

    #[test]
    fn a_block_that_got_empty_is_done_and_one_that_did_not_is_left_alone_for_a_while() {
        let mut pool = pool_with(&[8 * MB, 8 * MB, 8 * MB, 8 * MB]);
        let before = COMPACTED[3].load(Ordering::Relaxed);
        assert_eq!(pool.plan_drain(T0, 0, 1), 1);
        assert_eq!(pool.blocks[0].as_ref().unwrap().draining, T0);
        // Its piece went (taken out by hand, as the texture's release gives it back): it is done.
        let b = pool.blocks[0].as_mut().unwrap();
        b.free.give(0, 8 * MB);
        pool.reclaim(T0 + 1000, SPARE_KEPT, true);
        assert_eq!(pool.blocks[0].as_ref().unwrap().draining, 0);
        // The next block is at it for too long: given up, and not tried again for a while.
        assert_eq!(pool.plan_drain(T0 + 2000, 0, 1), 1);
        let tried = pool.blocks.iter().position(|b| b.as_ref().is_some_and(|b| b.draining != 0)).unwrap();
        assert_eq!(pool.plan_drain(T0 + 2000 + DRAIN_MS, 0, 1), 1, "another block is tried");
        let now_trying = pool.blocks.iter().position(|b| b.as_ref().is_some_and(|b| b.draining != 0)).unwrap();
        assert_ne!(tried, now_trying);
        assert!(pool.blocks[tried].as_ref().unwrap().cool >= T0 + 2000 + DRAIN_MS + COOL_MS);
        assert!(COMPACTED[3].load(Ordering::Relaxed) > before);
    }

    #[test]
    fn the_report_counts_blocks_by_how_free_they_are() {
        let pool = pool_with(&[0, 8 * MB, 24 * MB, 40 * MB, 60 * MB]);
        let r = pool.report();
        assert_eq!(r.blocks, 5);
        assert_eq!(r.by_free, [1, 1, 1, 1, 1]);
        assert_eq!(r.free, 5 * BLOCK - (8 + 24 + 40 + 60) * MB);
        assert_eq!(r.longest, BLOCK);
        assert_eq!(r.draining, 0);
    }
}
