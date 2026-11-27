//! Levels on demand: host copies of title textures that take the card's
//! memory level by level, as the title holds the levels.
//!
//! The title places a texture's whole chain of levels in its address space
//! and holds only some of them: the smallest first, at the start of the
//! texture's memory. The memory of the larger ones is either not mapped,
//! or belongs to the next texture, which the title packs where they would
//! be (run 188). A host copy was a committed texture: every level of it
//! took the card's memory from the first upload on. Run 189's accounting
//! line counted 2.2 to 3.0 GB of 4.3 to 5.5 GB of host copies in levels
//! whose title memory was not mapped, on some 1,600 copies; with about
//! 2.2 to 3.1 GB of render targets that filled the card, and textures
//! that needed a new host copy waited for room (the main character's face
//! among them, runs 188 and 189).
//!
//! Here such a texture is a reserved one (`CreateReservedResource`): it has
//! every level, so level numbers and sizes are what they were, but no
//! memory. The rules:
//! - A level is given tiles of a heap of ours (`UpdateTileMappings`, on the
//!   queue) only in a submission-time step of a list that also writes all
//!   of that level: the upload of the texture (`texture.rs` `upload`, which
//!   copies every level from the staging buffer every time), or, before
//!   the card itself writes the texture (a copy, a resolve), an upload of
//!   every level with every level given (`texture.rs` `make_whole`). The
//!   step runs when the list is submitted, before the list is executed, so
//!   the mapping is ahead of the copy on the queue. Tiles hold what their
//!   last owner left; no level is shown before it is written whole.
//! - Which levels are held is said by one function, at the moment the
//!   staging buffer is filled (`Staging::held_levels`): the levels whose
//!   title memory is mapped and lies before the start of the next texture
//!   placed in this one's span. The staging buffer holds zeros for the
//!   others, and the step gives tiles by that same answer.
//! - The small levels the card packs into shared tiles are given together
//!   when any of them is held; the upload writes all of them (zeros for
//!   the ones not held).
//! - A shader must not reach a level without memory (it reads zeros: a
//!   black texture). The host view of a slot is made from the first level
//!   that is held and has tiles ([`show_from`], `descriptor/host.rs`
//!   `make`), and the slots of a texture are made again when that level
//!   changes (`HostHeap::texture_views_remade`).
//! - shortcut: tiles are not taken back while the host copy lives. A level
//!   the title lets go of, or that turns out to be the next texture's,
//!   keeps its tiles; it is hidden by the views and written with zeros at
//!   the next upload. Its tiles go back when the host copy does.
//!
//! Only for a texture the title places in memory and fills, two-dimensional,
//! one layer, more than one level, of [`floor`] bytes or more, whose memory
//! is not all its own when the host copy is made (`resource.rs` `hosted`);
//! every other texture is made as before. Off unless
//! `VICTORY_LEVELS_ON_DEMAND=1`.

use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

use crate::host::host;
use crate::resource::HostTexture;

/// The card's tile: 64 KiB.
const TILE: u64 = 64 << 10;

/// Milliseconds since the start, plus one: 0 is never, in the stamps of
/// [`Tiled`].
fn stamp_now() -> u64 {
    crate::host::coarse_ms() + 1
}

/// Tiles in one heap of the pool (64 MiB). The largest level given is a
/// 4096x4096 one of one byte a texel: 256 tiles.
const HEAP_TILES: u32 = 1024;

/// Since the start, or now: such textures alive; the bytes all their
/// levels would take; the bytes of the tiles given to them; the bytes of
/// the pool's heaps; such textures made in all; levels given; calls of
/// `UpdateTileMappings`; their microseconds; heaps given back to the card;
/// uploads that left a held level without tiles (the card had no room, or
/// no heap could be had); uploads recorded again for such a level; uploads
/// that held a level back for its size ([`Tiled::uploaded`]'s `cap`) and
/// the levels they held back; levels taken back that the title no longer
/// held; levels taken back from cold textures ([`crate::priority`]); the
/// bytes of tiles those two took back; hot textures given levels past the
/// cap of everyone; the bytes of shortfall hot textures wait with now.
pub(crate) static STATS: [AtomicU64; 18] = [const { AtomicU64::new(0) }; 18];

/// Whether textures are made this way: the switch
/// (`VICTORY_LEVELS_ON_DEMAND=1`; off by default), and a card that reads
/// zeros from, and drops writes to, levels without memory (tiled resources
/// tier 2).
pub(crate) fn wanted() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var_os("VICTORY_LEVELS_ON_DEMAND").is_some_and(|v| v == "1") {
            return false;
        }
        let Some(host) = host() else {
            return false;
        };
        let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
        // SAFETY: a plain query into a struct of its own size.
        let asked = unsafe {
            host.device.CheckFeatureSupport(
                D3D12_FEATURE_D3D12_OPTIONS,
                (&raw mut options).cast(),
                size_of_val(&options) as u32,
            )
        };
        let tier = options.TiledResourcesTier.0;
        let on = asked.is_ok() && tier >= 2;
        tracing::info!(
            tier,
            on,
            floor_kb = floor() >> 10,
            "host copies of title textures take the card's memory level by level (VICTORY_LEVELS_ON_DEMAND=1)"
        );
        on
    })
}

fn parse_floor(value: Option<&str>) -> u64 {
    value
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(1 << 20, |kb| kb.max(64).saturating_mul(1024))
}

/// The bytes from which a texture is made this way: a megabyte, or
/// `VICTORY_LEVELS_MIN_KB`. (The accounting of run 189: the levels not
/// held are 1.9 MB a copy on some 1,600 copies; a 512x512 texture has six
/// tiles at most, and most of the calls for nothing.)
pub(crate) fn floor() -> u64 {
    static BYTES: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *BYTES.get_or_init(|| parse_floor(std::env::var("VICTORY_LEVELS_MIN_KB").ok().as_deref()))
}

/// The size from which a texture is made this way while the card is short
/// of room ([`crate::resource::level_cap_for`]): 256 KiB.
pub(crate) const SQUEEZED_FLOOR: u64 = 256 << 10;

/// The bytes of the tiles given to textures now: the card's memory they
/// hold (`resource.rs` `room` counts it with the other host copies).
pub(crate) fn given_total() -> u64 {
    STATS[2].load(Ordering::Relaxed)
}

/// The bytes of the pool's heaps that hold no tile of a texture: the card's
/// memory in use by us that the next level given takes first (the heaps
/// stay until all of their tiles are back, and are carved into runs).
pub(crate) fn spare() -> u64 {
    STATS[3].load(Ordering::Relaxed).saturating_sub(STATS[2].load(Ordering::Relaxed))
}

/// One heap of the pool and its free runs of tiles (first tile, tiles), in
/// order, no two touching.
struct Heap {
    heap: ID3D12Heap,
    free: Vec<(u32, u32)>,
}

// SAFETY: a heap of the device, named only in queue and device calls, which
// any thread may make; its free runs are plain numbers behind `POOL`'s lock.
unsafe impl Send for Heap {}

impl Heap {
    fn free_tiles(&self) -> u32 {
        self.free.iter().map(|run| run.1).sum()
    }
}

/// The pool's heaps by place; a place is empty after its heap went back to
/// the card (a texture names its heaps by place).
static POOL: Mutex<Vec<Option<Heap>>> = Mutex::new(Vec::new());

/// `count` tiles of one heap, in as many runs as it takes, the lowest free
/// tiles first (single free tiles are used up before a long run is cut):
/// the heap, its place in the pool and the runs (first tile, tiles). A new
/// heap is made when no heap has that many free.
fn take(device: &ID3D12Device, count: u32) -> Option<(ID3D12Heap, usize, Vec<(u32, u32)>)> {
    if count == 0 || count > HEAP_TILES {
        return None;
    }
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let found = pool.iter().position(|h| h.as_ref().is_some_and(|h| h.free_tiles() >= count));
    let index = match found {
        Some(index) => index,
        None => {
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: u64::from(HEAP_TILES) * TILE,
                Properties: D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_NON_RT_DS_TEXTURES,
            };
            let mut made: Option<ID3D12Heap> = None;
            // SAFETY: plain creation; the out pointer is ours.
            if let Err(e) = unsafe { device.CreateHeap::<ID3D12Heap>(&raw const desc, &raw mut made) } {
                tracing::warn!(error = %e, "no heap for texture levels given on demand: those levels stay empty");
                return None;
            }
            let fresh = Heap {
                heap: made?,
                free: vec![(0, HEAP_TILES)],
            };
            STATS[3].fetch_add(u64::from(HEAP_TILES) * TILE, Ordering::Relaxed);
            match pool.iter().position(Option::is_none) {
                Some(index) => {
                    pool[index] = Some(fresh);
                    index
                }
                None => {
                    pool.push(Some(fresh));
                    pool.len() - 1
                }
            }
        }
    };
    let h = pool[index].as_mut()?;
    let mut runs = Vec::new();
    let mut left = count;
    while left > 0 {
        let &(start, tiles) = h.free.first()?;
        let part = tiles.min(left);
        runs.push((start, part));
        if part == tiles {
            h.free.remove(0);
        } else {
            h.free[0] = (start + part, tiles - part);
        }
        left -= part;
    }
    Some((h.heap.clone(), index, runs))
}

/// Gives the runs `runs` of the pool's heap at `index` back; runs that
/// touch become one. A heap with every tile free goes back to the card,
/// unless it is the pool's only one. (Called when the texture that held
/// the tiles is dropped: nothing on the card reads through its mappings
/// any more, as for any host texture that is dropped.)
fn give_back(index: usize, runs: &[(u32, u32)]) {
    let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(h)) = pool.get_mut(index) else {
        return;
    };
    h.free.extend_from_slice(runs);
    h.free.sort_unstable();
    let mut joined: Vec<(u32, u32)> = Vec::with_capacity(h.free.len());
    for &(from, tiles) in &h.free {
        match joined.last_mut() {
            Some(last) if last.0 + last.1 == from => last.1 += tiles,
            _ => joined.push((from, tiles)),
        }
    }
    h.free = joined;
    let all_free = h.free_tiles() == HEAP_TILES;
    if all_free && pool.iter().flatten().count() > 1 {
        pool[index] = None;
        STATS[3].fetch_sub(u64::from(HEAP_TILES) * TILE, Ordering::Relaxed);
        STATS[8].fetch_add(1, Ordering::Relaxed);
    }
}

/// What is given so far to one texture.
struct Given {
    /// Per level with tiles of its own: whether it has them.
    levels: Vec<bool>,
    /// Whether the small levels the card packs together have theirs.
    packed: bool,
    /// The runs taken: the subresource they map, the heap's place, first
    /// tile, tiles.
    runs: Vec<(u32, usize, u32, u32)>,
}

/// A host texture made without memory, and the tiles its levels have been
/// given.
pub(crate) struct Tiled {
    /// Tiles of each level that has tiles of its own (level 0 first).
    standard: Vec<u32>,
    /// The tiles of the levels the card packs together (0: there are none);
    /// they are the levels from `standard.len()` on.
    packed: u32,
    /// Levels in all.
    levels: u32,
    /// The bytes all its tiles would take.
    pub(crate) full: u64,
    given: Mutex<Given>,
    /// The first level a view of the texture may show: the first one that
    /// is held or was written by the card and has tiles (`u32::MAX`: none
    /// yet).
    shown: AtomicU32,
    /// The levels held at the last upload (a bit a level).
    held: AtomicU32,
    /// The levels the card itself wrote, in lists submitted so far.
    card: AtomicU32,
    /// The levels a recorded list of the card's own writes has already
    /// written with zeros first ([`Tiled::claim`]).
    claimed: AtomicU32,
    /// The levels being taken back ([`Tiled::retire`]): they keep their
    /// tiles until the views that show them have been made again, but are
    /// not shown, and not counted as having tiles by a view.
    retiring: AtomicU32,
    /// Milliseconds since the start, plus one, when a draw last read the
    /// texture through a table ([`Tiled::note_sampled`]), when an upload last
    /// came, and when tiles were last given (0: never).
    sampled: AtomicU64,
    touched: AtomicU64,
    given_at: AtomicU64,
    /// The cap (plus one; 0: not short) under which a held level was left
    /// without tiles, and when it was last asked again
    /// ([`Tiled::retry_hot`]).
    short: AtomicU64,
    short_at: AtomicU64,
}

impl Tiled {
    /// For `texture`, just made reserved with `levels` levels and one
    /// layer: how the card cuts it into tiles. `None` if the card will not
    /// say, or cuts it in a way this does not handle; the caller then makes
    /// the texture the old way.
    pub(crate) fn make(device: &ID3D12Device, texture: &ID3D12Resource, levels: u32) -> Option<Tiled> {
        if levels == 0 || levels > 16 {
            return None;
        }
        let mut total = 0u32;
        let mut packed = D3D12_PACKED_MIP_INFO::default();
        let mut shape = D3D12_TILE_SHAPE::default();
        let mut count = levels;
        let mut tilings = vec![D3D12_SUBRESOURCE_TILING::default(); levels as usize];
        // SAFETY: outputs of our own, `count` tilings of them.
        unsafe {
            device.GetResourceTiling(
                texture,
                Some(&raw mut total),
                Some(&raw mut packed),
                Some(&raw mut shape),
                Some(&raw mut count),
                0,
                tilings.as_mut_ptr(),
            );
        }
        let standard_levels = u32::from(packed.NumStandardMips);
        if total == 0 || standard_levels + u32::from(packed.NumPackedMips) != levels || standard_levels > count {
            return None;
        }
        let mut standard = Vec::with_capacity(standard_levels as usize);
        for tiling in &tilings[..standard_levels as usize] {
            let tiles = tiling
                .WidthInTiles
                .checked_mul(u32::from(tiling.HeightInTiles))
                .and_then(|n| n.checked_mul(u32::from(tiling.DepthInTiles)))
                .unwrap_or(0);
            if tiles == 0 || tiles > HEAP_TILES || tiling.StartTileIndexInOverallResource == D3D12_PACKED_TILE {
                return None;
            }
            standard.push(tiles);
        }
        let packed_tiles = if packed.NumPackedMips > 0 { packed.NumTilesForPackedMips } else { 0 };
        if (packed.NumPackedMips > 0 && packed_tiles == 0) || packed_tiles > HEAP_TILES {
            return None;
        }
        let full = u64::from(total) * TILE;
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(full, Ordering::Relaxed);
        STATS[4].fetch_add(1, Ordering::Relaxed);
        Some(Tiled {
            given: Mutex::new(Given {
                levels: vec![false; standard.len()],
                packed: false,
                runs: Vec::new(),
            }),
            standard,
            packed: packed_tiles,
            levels,
            full,
            shown: AtomicU32::new(u32::MAX),
            held: AtomicU32::new(0),
            card: AtomicU32::new(0),
            claimed: AtomicU32::new(0),
            retiring: AtomicU32::new(0),
            sampled: AtomicU64::new(0),
            touched: AtomicU64::new(0),
            given_at: AtomicU64::new(0),
            short: AtomicU64::new(0),
            short_at: AtomicU64::new(0),
        })
    }

    /// At the submission of a list that uploads every level of `texture`
    /// from its staging buffer, of which `held` are the title's now (the
    /// others are zeros there): tiles for the held ones that have none.
    /// `cap`: the most bytes of tiles a held level may be given now
    /// (`resource.rs` `level_cap`: `u64::MAX` while the card has room, less
    /// as it fills, 0 past its line). Held levels that take more get no
    /// tiles, nor does any when `cap` is 0: the list still brings the levels
    /// that have theirs up to date (which takes none of the card's memory),
    /// and its copies into the others go nowhere. A texture then shows its
    /// first level that has tiles ([`Tiled::first_shown`]): a blurred one,
    /// not a black one, while the card is short. (An upload of a host copy
    /// that exists passes the full card's gate in `draw.rs` on the ground
    /// that it takes no memory; with tiles given here regardless, the line
    /// would not bound these textures.)
    /// Returns whether the first level a view may show changed, and
    /// whether a level named is still without tiles (no room, or no heap
    /// could be had): the caller has the upload recorded again later, or
    /// the level would stay empty until the title writes the texture.
    pub(crate) fn uploaded(&self, texture: &ID3D12Resource, held: u32, cap: u64) -> (bool, bool) {
        match host() {
            Some(host) => self.uploaded_on(host.queue(), &host.device, texture, held, cap),
            None => (false, false),
        }
    }

    /// The levels of `named` that fit `cap` ([`Tiled::uploaded`]): those
    /// whose tiles take no more bytes, and the small ones the card packs
    /// together (one block of tiles) if `cap` is not 0.
    fn fitting(&self, named: u32, cap: u64) -> u32 {
        if cap == u64::MAX {
            return named;
        }
        let first_packed = self.standard.len() as u32;
        let mut fits = 0;
        for (level, &tiles) in self.standard.iter().enumerate().take(32) {
            if u64::from(tiles) * TILE <= cap {
                fits |= 1 << level;
            }
        }
        if cap >= TILE && first_packed < 32 {
            fits |= self.every_level() & !((1u32 << first_packed) - 1);
        }
        named & fits
    }

    /// [`Tiled::uploaded`] on a queue and a device.
    pub(crate) fn uploaded_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        held: u32,
        cap: u64,
    ) -> (bool, bool) {
        let held = held & self.every_level();
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        self.held.store(held, Ordering::Relaxed);
        self.touched.store(stamp_now(), Ordering::Relaxed);
        let card = self.card.load(Ordering::Relaxed);
        let named = held | card;
        // The levels the card itself writes are few and are not kept
        // waiting: a write of the card's into no memory is lost.
        let may = self.fitting(named, cap) | card;
        // A level on its way back that the title holds again, and the cap
        // takes, stays: this upload writes it.
        self.retiring.fetch_and(!(named & may), Ordering::Relaxed);
        let kept = self.without_locked(&given, named & !may);
        if kept != 0 {
            STATS[11].fetch_add(1, Ordering::Relaxed);
            STATS[12].fetch_add(u64::from(kept.count_ones()), Ordering::Relaxed);
        }
        let changed = self.give_locked(&mut given, queue, device, texture, named, may);
        let missing = self.without_locked(&given, named) != 0;
        self.short.store(if missing { cap.saturating_add(1) } else { 0 }, Ordering::Relaxed);
        (changed, missing)
    }

    /// At the submission of a list in which the card itself writes the
    /// levels `levels` of `texture` (after zeros, if they were new:
    /// [`Tiled::claim`]): tiles for those that have none. Returns whether
    /// the first level a view may show changed.
    /// Only the levels this list writes get tiles here: a held level that
    /// is waiting for room is written by no copy of this list, and tiles
    /// hold what their last owner left.
    pub(crate) fn card_wrote(&self, texture: &ID3D12Resource, levels: u32) -> bool {
        match host() {
            Some(host) => self.card_wrote_on(host.queue(), &host.device, texture, levels),
            None => false,
        }
    }

    /// [`Tiled::card_wrote`] on a queue and a device.
    pub(crate) fn card_wrote_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        levels: u32,
    ) -> bool {
        let levels = levels & self.every_level();
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        self.touched.store(stamp_now(), Ordering::Relaxed);
        let prev = self.card.load(Ordering::Relaxed);
        let changed = self.give_locked(
            &mut given,
            queue,
            device,
            texture,
            prev | levels | self.held.load(Ordering::Relaxed),
            levels,
        );
        // Only levels that actually received tiles (or already had them) saw
        // the card's write; a level that got no tiles saw its zeros and write
        // dropped, so neither `card` nor `claimed` should keep it.
        let without = self.without_locked(&given, levels);
        let backed = levels & !without;
        if backed != 0 {
            self.card.fetch_or(backed, Ordering::Relaxed);
        }
        if without != 0 {
            self.unclaim(without);
        }
        changed
    }

    /// The bits of the levels that get their tiles with level `level`:
    /// itself, or every level the card packs together if it is one of them.
    pub(crate) fn with_level(&self, level: u32) -> u32 {
        let first_packed = self.standard.len() as u32;
        if level < first_packed {
            1 << level
        } else if level < self.levels {
            self.every_level() & !((1u32 << first_packed) - 1)
        } else {
            0
        }
    }

    /// Of `levels`, the ones that have no tiles now and that no list
    /// recorded before has claimed: the caller's list writes them with
    /// zeros before the card's own write, so that what the write does not
    /// cover is not what the tiles' last owner left.
    pub(crate) fn claim(&self, levels: u32) -> u32 {
        let fresh = self.without(levels);
        fresh & !self.claimed.fetch_or(fresh, Ordering::Relaxed)
    }

    /// Gives up the claim on `levels` ([`Tiled::claim`]): the zeros of the
    /// list that claimed them never reached tiles of theirs (the list was
    /// reset without being submitted, the zeros could not be recorded, or
    /// no tiles could be had), so the next list that writes them sends its
    /// own.
    /// shortcut: a second list recorded between the claim and this sends
    /// no zeros; if it is the first whose write gets tiles, what that
    /// write does not cover is what the tiles' last owner left.
    pub(crate) fn unclaim(&self, levels: u32) {
        self.claimed.fetch_and(!levels, Ordering::Relaxed);
    }

    /// Of `levels` (a bit a level), the ones that have no tiles now.
    pub(crate) fn without(&self, levels: u32) -> u32 {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        self.without_locked(&given, levels)
    }

    fn without_locked(&self, given: &Given, levels: u32) -> u32 {
        let first_packed = self.standard.len() as u32;
        let mut without = 0;
        for level in 0..self.levels.min(32) {
            let has = if level < first_packed { given.levels[level as usize] } else { given.packed };
            if !has {
                without |= 1 << level;
            }
        }
        levels & without
    }

    /// Gives tiles to the levels of `texture` that both `named` and `may`
    /// name (a bit a level, level 0 the lowest) and that have none yet;
    /// the levels the card packs together get theirs when any of them is
    /// so named. `named`: the levels that are the title's or the card's
    /// own, by which the first level a view may show is told; `may`: the
    /// ones of them that may get tiles in this call. To be called only
    /// from a submission-time step of a list that writes every level of
    /// `may`, whole: the call is ahead of that list on `queue`, and tiles
    /// hold what their last owner left.
    /// Returns whether the first level a view may show changed
    /// ([`Tiled::first_shown`]): the caller has the texture's slots made
    /// again.
    #[cfg(test)]
    pub(crate) fn give_on(
        &self,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        named: u32,
        may: u32,
    ) -> bool {
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        self.give_locked(&mut given, queue, device, texture, named, may)
    }

    fn give_locked(
        &self,
        given: &mut Given,
        queue: &ID3D12CommandQueue,
        device: &ID3D12Device,
        texture: &ID3D12Resource,
        named: u32,
        may: u32,
    ) -> bool {
        let now = |level: u32| level < 32 && named & may & (1 << level) != 0;
        let first_packed = self.standard.len() as u32;
        // (subresource, tiles, the level if it has tiles of its own) to map now.
        let mut wanted: Vec<(u32, u32, Option<usize>)> = Vec::new();
        for level in 0..first_packed {
            if !given.levels[level as usize] && now(level) {
                wanted.push((level, self.standard[level as usize], Some(level as usize)));
            }
        }
        if self.packed > 0 && !given.packed && (first_packed..self.levels).any(now) {
            wanted.push((first_packed, self.packed, None));
        }
        for (subresource, tiles, level) in wanted {
            let Some((heap, index, runs)) = take(device, tiles) else {
                continue;
            };
            let at = D3D12_TILED_RESOURCE_COORDINATE {
                X: 0,
                Y: 0,
                Z: 0,
                Subresource: subresource,
            };
            let size = D3D12_TILE_REGION_SIZE {
                NumTiles: tiles,
                ..Default::default()
            };
            // One region, the subresource's tiles in order, on as many
            // ranges of the heap as the runs taken.
            let flags = vec![D3D12_TILE_RANGE_FLAG_NONE; runs.len()];
            let starts: Vec<u32> = runs.iter().map(|run| run.0).collect();
            let counts: Vec<u32> = runs.iter().map(|run| run.1).collect();
            let began = std::time::Instant::now();
            // SAFETY: a live reserved texture and a live heap of ours; one
            // region of `tiles` tiles from the subresource's first tile;
            // `runs.len()` ranges whose counts add up to `tiles`, in three
            // arrays of that length that live through the call.
            unsafe {
                queue.UpdateTileMappings(
                    texture,
                    1,
                    Some(&raw const at),
                    Some(&raw const size),
                    &heap,
                    runs.len() as u32,
                    Some(flags.as_ptr()),
                    Some(starts.as_ptr()),
                    Some(counts.as_ptr()),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
            }
            STATS[6].fetch_add(1, Ordering::Relaxed);
            STATS[7].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
            match level {
                Some(level) => given.levels[level] = true,
                None => given.packed = true,
            }
            for &(start, count) in &runs {
                given.runs.push((subresource, index, start, count));
            }
            self.given_at.store(stamp_now(), Ordering::Relaxed);
            STATS[2].fetch_add(u64::from(tiles) * TILE, Ordering::Relaxed);
            STATS[5].fetch_add(1, Ordering::Relaxed);
        }
        self.reshown(given, named)
    }

    /// The first level a view may show, from the levels named (the title's
    /// or the card's own) that have tiles and are not on their way back;
    /// whether it changed.
    fn reshown(&self, given: &Given, named: u32) -> bool {
        let first_packed = self.standard.len() as u32;
        let retiring = self.retiring.load(Ordering::Relaxed);
        let with_tiles = |level: u32| {
            retiring & (1 << level.min(31)) == 0
                && if level < first_packed { given.levels[level as usize] } else { given.packed }
        };
        let shown = (0..self.levels)
            .find(|&level| level < 32 && named & (1 << level) != 0 && with_tiles(level))
            .unwrap_or(u32::MAX);
        self.shown.swap(shown, Ordering::Relaxed) != shown
    }

    /// The first level a view of the texture may show; `None` while no
    /// level is both held and given (the views are then as the title made
    /// them, and read zeros).
    pub(crate) fn first_shown(&self) -> Option<u32> {
        let shown = self.shown.load(Ordering::Relaxed);
        (shown != u32::MAX).then_some(shown)
    }

    /// Whether level `level` has tiles.
    #[cfg(test)]
    fn has_tiles(&self, level: u32) -> bool {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        match given.levels.get(level as usize) {
            Some(&has) => has,
            None => level < self.levels && given.packed,
        }
    }

    /// Whether every level has tiles.
    #[cfg(test)]
    fn whole(&self) -> bool {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.levels.iter().all(|&has| has) && (self.packed == 0 || given.packed)
    }

    /// A bit for every level of the texture.
    pub(crate) fn every_level(&self) -> u32 {
        if self.levels >= 32 { u32::MAX } else { (1 << self.levels) - 1 }
    }

    /// The bytes of the tiles it has now.
    pub(crate) fn given_bytes(&self) -> u64 {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        given.runs.iter().map(|run| u64::from(run.3) * TILE).sum()
    }

    /// A list read the texture through a table: it is looked at.
    pub(crate) fn note_sampled(&self) {
        let now = stamp_now();
        if self.sampled.load(Ordering::Relaxed) != now {
            self.sampled.store(now, Ordering::Relaxed);
        }
    }

    /// How the texture stands ([`crate::priority::heat`]).
    pub(crate) fn heat(&self) -> crate::priority::Heat {
        let now = stamp_now();
        let sampled = self.sampled.load(Ordering::Relaxed);
        crate::priority::heat(
            crate::priority::mode(),
            (sampled != 0).then(|| now.saturating_sub(sampled)),
            now.saturating_sub(self.touched.load(Ordering::Relaxed)),
        )
    }

    /// Milliseconds since a list read it through a table, else since an
    /// upload came.
    fn idle_ms(&self, now: u64) -> u64 {
        let sampled = self.sampled.load(Ordering::Relaxed);
        now.saturating_sub(if sampled != 0 { sampled } else { self.touched.load(Ordering::Relaxed) })
    }

    /// The bytes of tiles the held levels that have none would take.
    pub(crate) fn deficit(&self) -> u64 {
        let held = self.held.load(Ordering::Relaxed);
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let first_packed = self.standard.len() as u32;
        let mut tiles = 0u64;
        for (level, &n) in self.standard.iter().enumerate() {
            if held & (1 << level) != 0 && !given.levels[level] {
                tiles += u64::from(n);
            }
        }
        if self.packed > 0 && !given.packed && held & self.packed_levels(first_packed) != 0 {
            tiles += u64::from(self.packed);
        }
        tiles * TILE
    }

    /// The bits of the levels the card packs together.
    fn packed_levels(&self, first_packed: u32) -> u32 {
        self.every_level() & !((1u32 << first_packed) - 1)
    }

    /// The levels that have tiles and are not on their way back.
    fn with_tiles_locked(&self, given: &Given) -> u32 {
        let first_packed = self.standard.len() as u32;
        let mut have = 0;
        for level in 0..first_packed {
            if given.levels[level as usize] {
                have |= 1 << level;
            }
        }
        if given.packed {
            have |= self.packed_levels(first_packed);
        }
        have & !self.retiring.load(Ordering::Relaxed)
    }

    /// The levels that have tiles although the title no longer holds them
    /// and the card did not write them: dead weight (hidden by the views,
    /// written with zeros at the next upload).
    pub(crate) fn dead(&self) -> u32 {
        let named = self.held.load(Ordering::Relaxed) | self.card.load(Ordering::Relaxed);
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let first_packed = self.standard.len() as u32;
        let mut dead = self.with_tiles_locked(&given) & !named;
        // The packed levels go together, and only when none is named.
        let packed = self.packed_levels(first_packed);
        if dead & packed != packed {
            dead &= !packed;
        }
        dead
    }

    /// The levels with tiles of more than `keep` bytes that the card did not
    /// write, and the bytes they hold.
    pub(crate) fn big_given(&self, keep: u64) -> (u32, u64) {
        let card = self.card.load(Ordering::Relaxed);
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let retiring = self.retiring.load(Ordering::Relaxed);
        let (mut mask, mut bytes) = (0u32, 0u64);
        for (level, &n) in self.standard.iter().enumerate() {
            let size = u64::from(n) * TILE;
            if given.levels[level] && size > keep && (card | retiring) & (1 << level) == 0 {
                mask |= 1 << level;
                bytes += size;
            }
        }
        (mask, bytes)
    }

    /// Begins taking back the levels of `levels` that have tiles: they are
    /// no longer shown from now on, so the views made again by the caller
    /// (`levels_changed`) do not read them, and [`Tiled::unmap`] takes their
    /// tiles once the lists that may still read them have run. Returns the
    /// levels begun and whether the first level a view may show changed.
    pub(crate) fn retire(&self, levels: u32) -> (u32, bool) {
        let given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let mask = levels & self.with_tiles_locked(&given);
        if mask == 0 {
            return (0, false);
        }
        self.retiring.fetch_or(mask, Ordering::Relaxed);
        let named = self.held.load(Ordering::Relaxed) | self.card.load(Ordering::Relaxed);
        (mask, self.reshown(&given, named))
    }

    /// Takes the tiles of the levels of `levels` that are still on their way
    /// back ([`Tiled::retire`]) out of `texture`: the mapping is made null on
    /// `queue`, ahead of the lists submitted after it, and the tiles go to
    /// the pool. A level put back by an upload since stays. Returns the bytes
    /// of tiles given back.
    pub(crate) fn unmap(&self, queue: &ID3D12CommandQueue, texture: &ID3D12Resource, levels: u32) -> u64 {
        let mut given = self.given.lock().unwrap_or_else(|e| e.into_inner());
        let mask = levels & self.retiring.load(Ordering::Relaxed);
        let first_packed = self.standard.len() as u32;
        // (subresource, tiles) of what has tiles and goes.
        let mut going: Vec<(u32, u32)> = Vec::new();
        for level in 0..first_packed {
            if mask & (1 << level) != 0 && given.levels[level as usize] {
                going.push((level, self.standard[level as usize]));
            }
        }
        if self.packed > 0 && given.packed && mask & self.packed_levels(first_packed) != 0 {
            going.push((first_packed, self.packed));
        }
        let mut freed = 0u64;
        for (subresource, tiles) in going {
            let at = D3D12_TILED_RESOURCE_COORDINATE {
                X: 0,
                Y: 0,
                Z: 0,
                Subresource: subresource,
            };
            let size = D3D12_TILE_REGION_SIZE {
                NumTiles: tiles,
                ..Default::default()
            };
            let flag = D3D12_TILE_RANGE_FLAG_NULL;
            let start = 0u32;
            // SAFETY: a live reserved texture; one region of the
            // subresource's `tiles` tiles, on one null range of that many
            // (no heap is named: every range is null).
            unsafe {
                queue.UpdateTileMappings(
                    texture,
                    1,
                    Some(&raw const at),
                    Some(&raw const size),
                    None::<&ID3D12Heap>,
                    1,
                    Some(&raw const flag),
                    Some(&raw const start),
                    Some(&raw const tiles),
                    D3D12_TILE_MAPPING_FLAG_NONE,
                );
            }
            STATS[6].fetch_add(1, Ordering::Relaxed);
            let mut by_heap: std::collections::BTreeMap<usize, Vec<(u32, u32)>> = std::collections::BTreeMap::new();
            given.runs.retain(|&(sub, index, first, count)| {
                if sub == subresource {
                    by_heap.entry(index).or_default().push((first, count));
                    false
                } else {
                    true
                }
            });
            for (index, runs) in by_heap {
                give_back(index, &runs);
            }
            if subresource < first_packed {
                given.levels[subresource as usize] = false;
            } else {
                given.packed = false;
            }
            freed += u64::from(tiles) * TILE;
        }
        STATS[2].fetch_sub(freed, Ordering::Relaxed);
        self.retiring.fetch_and(!mask, Ordering::Relaxed);
        // What the card writes into them next is zeroed first again.
        self.claimed.fetch_and(!mask, Ordering::Relaxed);
        freed
    }

    /// Marks the texture short of levels under `cap` (a cold texture whose
    /// large levels were taken back).
    fn set_short(&self, cap: u64) {
        self.short.store(cap.saturating_add(1), Ordering::Relaxed);
        self.short_at.store(stamp_now(), Ordering::Relaxed);
    }

    /// For a texture a draw reads through a table: whether it is hot, is short
    /// of levels under a cap `may` says the card takes more than now, and was
    /// not asked about in the last second. The caller uploads it again.
    pub(crate) fn retry_hot(&self, may: impl Fn(u64) -> bool) -> bool {
        let short = self.short.load(Ordering::Relaxed);
        if short == 0 {
            return false;
        }
        let now = stamp_now();
        let at = self.short_at.load(Ordering::Relaxed);
        if at != 0 && now.saturating_sub(at) < 1000 {
            return false;
        }
        self.short_at.store(now, Ordering::Relaxed);
        self.heat() == crate::priority::Heat::Hot && may(short - 1)
    }
}

impl Drop for Tiled {
    fn drop(&mut self) {
        let given = self.given.get_mut().unwrap_or_else(|e| e.into_inner());
        let mut bytes = 0;
        let mut by_heap: std::collections::BTreeMap<usize, Vec<(u32, u32)>> = std::collections::BTreeMap::new();
        for &(_, index, start, tiles) in &given.runs {
            by_heap.entry(index).or_default().push((start, tiles));
            bytes += u64::from(tiles) * TILE;
        }
        for (index, runs) in by_heap {
            give_back(index, &runs);
        }
        STATS[0].fetch_sub(1, Ordering::Relaxed);
        STATS[1].fetch_sub(self.full, Ordering::Relaxed);
        STATS[2].fetch_sub(bytes, Ordering::Relaxed);
    }
}

/// Keeps a two-dimensional shader view (2D or 2D array) from the levels
/// before `first` of a texture of `levels` levels: it starts at `first` if
/// it started before it, shows as many levels fewer (clamped to the
/// remaining levels), and reduces `ResourceMinLODClamp` by the same shift so
/// the effective minimum LOD stays at the same resource level.
pub(crate) fn show_from(desc: &mut D3D12_SHADER_RESOURCE_VIEW_DESC, first: u32, levels: u32) {
    if first >= levels {
        return;
    }
    let clamp = |most: &mut u32, mips: &mut u32, min_lod: &mut f32| {
        if first <= *most {
            return;
        }
        let fewer = first - *most;
        *most = first;
        if *mips != u32::MAX {
            *mips = mips.saturating_sub(fewer).clamp(1, levels - first);
        }
        if *min_lod > 0.0 {
            *min_lod = (*min_lod - fewer as f32).max(0.0);
        }
    };
    match desc.ViewDimension {
        D3D12_SRV_DIMENSION_TEXTURE2D => {
            // SAFETY: `ViewDimension` selects `Texture2D`.
            let view = unsafe { &mut desc.Anonymous.Texture2D };
            clamp(&mut view.MostDetailedMip, &mut view.MipLevels, &mut view.ResourceMinLODClamp);
        }
        D3D12_SRV_DIMENSION_TEXTURE2DARRAY => {
            // SAFETY: `ViewDimension` selects `Texture2DArray`.
            let view = unsafe { &mut desc.Anonymous.Texture2DArray };
            clamp(&mut view.MostDetailedMip, &mut view.MipLevels, &mut view.ResourceMinLODClamp);
        }
        _ => {}
    }
}

/// The textures made level by level that have had an upload, by where their
/// title memory starts: [`pass`] walks them. (Weak: a copy let go leaves.)
static TRACK: Mutex<std::collections::BTreeMap<u64, std::sync::Weak<HostTexture>>> =
    Mutex::new(std::collections::BTreeMap::new());

/// Where the next [`pass`] starts in [`TRACK`].
static CURSOR: AtomicU64 = AtomicU64::new(0);

/// Levels begun to be taken back ([`Tiled::retire`]): the submission begun
/// when they were, the time (stamp) from which their tiles may go, the
/// texture, the levels.
static RETIRING: Mutex<Vec<(u64, u64, std::sync::Arc<HostTexture>, u32)>> = Mutex::new(Vec::new());

/// How long the views that showed a level taken back have to be made again
/// (the next texture pass makes them) before its tiles go.
const RETIRE_DELAY_MS: u64 = 1000;

/// How long the title has to hold the same levels before the tiles of the
/// ones it let go are taken back: a title that moves a texture in its
/// streaming memory unmaps it before it maps it again.
const DEAD_GRACE_MS: u64 = 5000;

/// `texture`, whose title memory starts at `start`, has had an upload: [`pass`]
/// looks at it from now on.
pub(crate) fn track(start: u64, texture: &std::sync::Arc<HostTexture>) {
    let mut track = TRACK.lock().unwrap_or_else(|e| e.into_inner());
    if track.get(&start).is_some_and(|w| std::ptr::eq(w.as_ptr(), std::sync::Arc::as_ptr(texture))) {
        return;
    }
    track.insert(start, std::sync::Arc::downgrade(texture));
}

/// The bytes of tiles hot textures lack, as the last look at the textures
/// waiting for levels found ([`crate::priority::reclaim_need`]).
pub(crate) fn set_hot_want(bytes: u64) {
    STATS[17].store(bytes, Ordering::Relaxed);
}

/// Once for each texture pass (`draw.rs` `heap_textures`), with the card's
/// room (`resource::headroom_lately`): gives back what the policy of
/// [`crate::priority`] says. Bounded: the tiles of the levels begun earlier
/// whose time has come, then [`crate::priority::SCAN_A_PASS`] textures, in
/// turn, and no more than [`crate::priority::RECLAIM_PASS_BYTES`] of cold
/// textures' tiles. The levels the title no longer holds go whichever the
/// texture; a cold texture's large levels only while hot textures wait and the
/// card is short of room.
pub(crate) fn pass(headroom: i64) {
    use crate::priority::{
        Holder, KEEP_BYTES, RECLAIM_PASS_BYTES, SCAN_A_PASS, enabled, reclaim_need, reclaim_plan,
    };
    if !enabled() {
        return;
    }
    finish_retired();
    let need = reclaim_need(headroom, STATS[17].load(Ordering::Relaxed));
    let window = {
        let mut track = TRACK.lock().unwrap_or_else(|e| e.into_inner());
        let from = CURSOR.load(Ordering::Relaxed);
        let mut window = Vec::with_capacity(SCAN_A_PASS);
        let mut gone = Vec::new();
        let mut seen = 0;
        let mut last = from;
        for (&start, weak) in track.range(from..).take(SCAN_A_PASS) {
            seen += 1;
            last = start;
            match weak.upgrade() {
                Some(texture) => window.push((start, texture)),
                None => gone.push(start),
            }
        }
        for start in gone {
            track.remove(&start);
        }
        CURSOR.store(if seen < SCAN_A_PASS { 0 } else { last.saturating_add(1) }, Ordering::Relaxed);
        window
    };
    let now = stamp_now();
    let mut holders = Vec::new();
    for (start, texture) in &window {
        let Some(tiled) = texture.tiled.as_ref() else {
            continue;
        };
        if now.saturating_sub(tiled.touched.load(Ordering::Relaxed)) >= DEAD_GRACE_MS {
            let dead = tiled.dead();
            if dead != 0 {
                begin_retire(*start, texture, dead, false);
            }
        }
        if need > 0 {
            let (big, bytes) = tiled.big_given(KEEP_BYTES);
            if big != 0 {
                holders.push(Holder {
                    id: *start,
                    heat: tiled.heat(),
                    idle: tiled.idle_ms(now),
                    given_ago: now.saturating_sub(tiled.given_at.load(Ordering::Relaxed)),
                    big: bytes,
                });
            }
        }
    }
    if need == 0 {
        return;
    }
    let (chosen, _) = reclaim_plan(&mut holders, need, RECLAIM_PASS_BYTES);
    for id in chosen {
        let Ok(at) = window.binary_search_by_key(&id, |w| w.0) else {
            continue;
        };
        let texture = &window[at].1;
        if let Some(tiled) = texture.tiled.as_ref() {
            let (big, _) = tiled.big_given(KEEP_BYTES);
            begin_retire(id, texture, big, true);
        }
    }
}

/// Begins taking back `levels` of `texture` (title memory at `start`): not
/// shown from now on, tiles gone in [`finish_retired`]. A cold texture is
/// asked for again when the card has room (`levels_waiting`).
fn begin_retire(start: u64, texture: &std::sync::Arc<HostTexture>, levels: u32, cold: bool) {
    let Some(tiled) = texture.tiled.as_ref() else {
        return;
    };
    let (mask, changed) = tiled.retire(levels);
    if mask == 0 {
        return;
    }
    if changed {
        crate::descriptor::host::levels_changed(start);
    }
    STATS[if cold { 14 } else { 13 }].fetch_add(u64::from(mask.count_ones()), Ordering::Relaxed);
    if cold {
        tiled.set_short(crate::priority::KEEP_BYTES);
        crate::descriptor::host::levels_waiting(
            start,
            crate::priority::KEEP_BYTES,
            std::sync::Arc::downgrade(texture),
        );
    }
    RETIRING.lock().unwrap_or_else(|e| e.into_inner()).push((
        crate::host::submissions().0,
        stamp_now() + RETIRE_DELAY_MS,
        texture.clone(),
        mask,
    ));
}

/// Takes the tiles of the levels [`begin_retire`] began with, once the
/// submissions begun by then have finished and the views have had their time
/// to be made again.
fn finish_retired() {
    let ready: Vec<_> = {
        let mut retiring = RETIRING.lock().unwrap_or_else(|e| e.into_inner());
        if retiring.is_empty() {
            return;
        }
        let finished = crate::host::submissions().1;
        let now = stamp_now();
        let (ready, later): (Vec<_>, Vec<_>) =
            retiring.drain(..).partition(|(after, from, _, _)| *after <= finished && now >= *from);
        *retiring = later;
        ready
    };
    let Some(host) = host() else {
        return;
    };
    for (_, _, texture, mask) in ready {
        if let Some(tiled) = texture.tiled.as_ref() {
            let bytes = tiled.unmap(host.queue(), &texture.texture, mask);
            STATS[15].fetch_add(bytes, Ordering::Relaxed);
        }
    }
}

/// For the log: the textures [`pass`] looks at by heat: how many, the bytes of
/// tiles they hold, and of them the bytes in large levels.
fn census() -> [(u64, u64, u64); 3] {
    let all: Vec<_> = {
        let track = TRACK.lock().unwrap_or_else(|e| e.into_inner());
        track.values().filter_map(std::sync::Weak::upgrade).collect()
    };
    let mut out = [(0u64, 0u64, 0u64); 3];
    for texture in all {
        let Some(tiled) = texture.tiled.as_ref() else {
            continue;
        };
        let entry = &mut out[tiled.heat() as usize];
        entry.0 += 1;
        entry.1 += tiled.given_bytes();
        entry.2 += tiled.big_given(crate::priority::KEEP_BYTES).1;
    }
    out
}

/// For the log, with the texture queue's line: what this saves, and what
/// the mapping calls cost since the line before.
pub(crate) fn status() {
    static BEFORE: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    if s[4] == 0 {
        return;
    }
    let heat = if crate::priority::enabled() { census() } else { [(0, 0, 0); 3] };
    let calls = s[6] - BEFORE[0].swap(s[6], Ordering::Relaxed);
    let micros = s[7] - BEFORE[1].swap(s[7], Ordering::Relaxed);
    tracing::info!(
        "textures with levels on demand: {} alive ({} made so far), {} MB of the card given to {} levels in all, \
         {} MB of the card not allocated for levels the title does not hold ({} MB if every level had memory); \
         the pool's heaps {} MB ({} given back to the card so far); since the line before: {} calls of \
         UpdateTileMappings in {} microseconds; in all {} calls, {} microseconds; {} uploads left a held \
         level without memory (no room on the card, or no heap), {} uploads were recorded again for one;          {} uploads held {} levels back for their size, the card being short of room; \
         tile priority (VICTORY_TEXTURE_PRIORITY, {}): hot textures (read through a table within 10 s) {} with {} MB of tiles ({} MB in large levels), \
         not read through a table lately {} with {} MB ({} MB large), cold {} with {} MB ({} MB large); hot textures lack {} MB of tiles; \
         taken back so far: {} levels the title no longer held, {} levels of cold textures, {} MB in all; {} uploads of hot textures went past the cap of everyone",
        s[0],
        s[4],
        s[2] >> 20,
        s[5],
        s[1].saturating_sub(s[2]) >> 20,
        s[1] >> 20,
        s[3] >> 20,
        s[8],
        calls,
        micros,
        s[6],
        s[7],
        s[9],
        s[10],
        s[11],
        s[12],
        crate::priority::mode(),
        heat[0].0,
        heat[0].1 >> 20,
        heat[0].2 >> 20,
        heat[1].0,
        heat[1].1 >> 20,
        heat[1].2 >> 20,
        heat[2].0,
        heat[2].1 >> 20,
        heat[2].2 >> 20,
        s[17] >> 20,
        s[13],
        s[14],
        s[15] >> 20,
        s[16],
    );
}

/// On the card, without the title: what levels on demand relies on, with
/// the pool and the mapping call of this module on a device of the test's
/// own. One test, one device: the pool is one for the process, and its
/// heaps belong to the device that made them.
///
/// Run by name, alone, no game needed:
/// `cargo test -p victory-gpu --release -- --ignored levels_on_demand --nocapture`
///
/// It runs on the SOFTWARE adapter unless `VICTORY_TILES_TEST_CARD=1`: on
/// that adapter nothing reaches the card or its driver, so nothing it does
/// can reset the driver or touch the screen. Wherever it runs, the debug
/// layer is switched on if Windows has it (the optional feature "Graphics
/// Tools"), its messages are printed after every step, and the test stops
/// at the first error or lost device with the step's name, before
/// anything else is sent. On the card it does not run without the layer,
/// and no wait is longer than 1.5 s.
#[cfg(test)]
mod tests {
    use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    use windows::Win32::Graphics::Dxgi::Common::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::{Interface, s};

    use super::*;
    use crate::host::{compile, transition, warp_adapter};

    const SIDE: u32 = 1024;
    const LEVELS: u16 = 11;

    /// Two views of one texture read at the same place and level: the
    /// first as the layer makes it for a texture whose level 0 has no
    /// memory, the second as the title asked.
    const LOOK: &str = r#"
#define RS "DescriptorTable(SRV(t0, numDescriptors = 2)), UAV(u0), StaticSampler(s0, filter = FILTER_MIN_MAG_MIP_POINT)"
Texture2D<float4> clamped : register(t0);
Texture2D<float4> whole : register(t1);
SamplerState nearest : register(s0);
RWByteAddressBuffer words : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void look() {
    float4 a = clamped.SampleLevel(nearest, float2(0.3, 0.3), 0);
    float4 b = whole.SampleLevel(nearest, float2(0.3, 0.3), 0);
    words.Store(0, (uint)(a.x * 255.0 + 0.5));
    words.Store(4, (uint)(b.x * 255.0 + 0.5));
}
"#;

    /// Where each level of a texture lies in a buffer of all of them: its
    /// place, its rows, the bytes of a row that are the level's own, and
    /// the buffer's bytes.
    struct Laid {
        footprints: Vec<D3D12_PLACED_SUBRESOURCE_FOOTPRINT>,
        rows: Vec<u32>,
        wide: Vec<u64>,
        total: u64,
    }

    struct Rig {
        device: ID3D12Device,
        queue: ID3D12CommandQueue,
        allocator: ID3D12CommandAllocator,
        list: ID3D12GraphicsCommandList,
        fence: ID3D12Fence,
        event: HANDLE,
        submitted: u64,
        /// The debug layer's messages, if Windows has the layer.
        messages: Option<ID3D12InfoQueue>,
        /// What the test is doing, for the message when it stops.
        step: &'static str,
        /// On the card, not on the software adapter.
        card: bool,
    }

    impl Rig {
        /// The software adapter, or the default one with
        /// `VICTORY_TILES_TEST_CARD=1`; `None` without one, without tiled
        /// resources of tier 2, or on the card without the debug layer.
        fn new() -> Option<Rig> {
            // SAFETY: plain object creation on a device of our own.
            unsafe {
                let card = std::env::var_os("VICTORY_TILES_TEST_CARD").is_some_and(|v| v == "1");
                // The debug layer, before the device is made: a call that
                // is not well formed is then refused and named by the
                // runtime, not handed to the driver.
                let mut layer: Option<ID3D12Debug> = None;
                let layered = D3D12GetDebugInterface(&mut layer).is_ok() && layer.is_some();
                layer.iter().for_each(|l| l.EnableDebugLayer());
                if card && !layered {
                    eprintln!("no D3D12 debug layer (Windows optional feature \"Graphics Tools\"): not run on the card without it");
                    return None;
                }
                let mut device: Option<ID3D12Device> = None;
                if card {
                    D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                } else {
                    let adapter = warp_adapter().ok()?;
                    D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
                }
                let device = device?;
                let messages = device.cast::<ID3D12InfoQueue>().ok();
                let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS::default();
                device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .ok()?;
                eprintln!(
                    "levels on demand, {}: tiled resources tier {}, debug layer {}",
                    if card { "ON THE CARD" } else { "on the software adapter" },
                    options.TiledResourcesTier.0,
                    if messages.is_some() { "on" } else { "not installed" }
                );
                if options.TiledResourcesTier.0 < 2 {
                    return None;
                }
                let queue: ID3D12CommandQueue = device
                    .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                        Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                        ..Default::default()
                    })
                    .ok()?;
                let allocator: ID3D12CommandAllocator =
                    device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT).ok()?;
                let list: ID3D12GraphicsCommandList = device
                    .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                    .ok()?;
                let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
                let event = CreateEventW(None, false, false, None).ok()?;
                Some(Rig {
                    device,
                    queue,
                    allocator,
                    list,
                    fence,
                    event,
                    submitted: 0,
                    messages,
                    step: "making the device",
                    card,
                })
            }
        }

        /// Prints what the debug layer said since the last call; the first
        /// error among it (as `host.rs` `log_messages` reads them).
        fn said(&self) -> Option<String> {
            let queue = self.messages.as_ref()?;
            let mut first_error = None;
            // SAFETY: each message is read into a buffer of the size the
            // queue reports, then the queue is emptied.
            unsafe {
                for i in 0..queue.GetNumStoredMessages() {
                    let mut size = 0;
                    if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                        continue;
                    }
                    let mut buffer = vec![0u64; size.div_ceil(8)];
                    let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                    if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                        let m = &*message;
                        let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                        let text = String::from_utf8_lossy(text);
                        let text = text.trim_end_matches('\0');
                        eprintln!("D3D12 says (severity {}, id {}): {text}", m.Severity.0, m.ID.0);
                        if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                            first_error = Some(text.to_owned());
                        }
                    }
                }
                queue.ClearStoredMessages();
            }
            first_error
        }

        /// Stops the test, naming the step, if the device is lost or the
        /// debug layer reported an error: nothing more is sent after it.
        fn check(&self, what: &str) {
            let error = self.said();
            // SAFETY: a plain query.
            let removed = unsafe { self.device.GetDeviceRemovedReason() };
            assert!(removed.is_ok(), "{}, {what}: the device is lost: {removed:?}", self.step);
            assert!(error.is_none(), "{}, {what}: the debug layer reports an error: {}", self.step, error.unwrap_or_default());
        }

        /// Gives tiles as a submission would, and stops if the mapping
        /// call was refused.
        fn give(&self, tiled: &Tiled, texture: &ID3D12Resource, levels: u32) {
            tiled.give_on(&self.queue, &self.device, texture, levels, levels);
            self.check("tiles given");
        }

        /// Records with `record`, runs the list behind what is already on
        /// the queue (tile mappings among it), waits for the card and
        /// opens the list again.
        fn run(&mut self, record: impl FnOnce(&ID3D12GraphicsCommandList)) {
            record(&self.list);
            self.submitted += 1;
            // SAFETY: our list, queue and fence; the allocator is reset
            // only after the card finished everything recorded from it.
            unsafe {
                self.list.Close().unwrap();
                let run: ID3D12CommandList = self.list.cast().unwrap();
                self.queue.ExecuteCommandLists(&[Some(run)]);
                self.queue.Signal(&self.fence, self.submitted).unwrap();
                self.fence.SetEventOnCompletion(self.submitted, self.event).unwrap();
                // Never without end: a step is a few copies of a few
                // megabytes. On the card the limit stays under Windows' own
                // two seconds; the software adapter, which compiles its
                // shader on first use and resets nothing, gets longer.
                let limit = if self.card { 1500 } else { 20_000 };
                let waited = WaitForSingleObject(self.event, limit);
                self.check("a submission");
                assert!(
                    waited == WAIT_OBJECT_0,
                    "{}: a submission did not finish within {limit} ms; stopped before another is sent",
                    self.step
                );
                self.allocator.Reset().unwrap();
                self.list.Reset(&self.allocator, None).unwrap();
            }
        }

        fn buffer(&self, kind: D3D12_HEAP_TYPE, flags: D3D12_RESOURCE_FLAGS, state: D3D12_RESOURCE_STATES, bytes: u64) -> ID3D12Resource {
            let heap = D3D12_HEAP_PROPERTIES {
                Type: kind,
                ..Default::default()
            };
            let desc = D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                Width: bytes,
                Height: 1,
                DepthOrArraySize: 1,
                MipLevels: 1,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                Flags: flags,
                ..Default::default()
            };
            let mut out: Option<ID3D12Resource> = None;
            // SAFETY: plain buffer creation.
            unsafe {
                self.device
                    .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut out)
                    .unwrap();
            }
            out.unwrap()
        }

        /// A square texture description of `side` texels and `levels`
        /// levels; `reserved`: in the layout a texture without memory of
        /// its own must have.
        fn desc(format: DXGI_FORMAT, side: u32, levels: u16, reserved: bool) -> D3D12_RESOURCE_DESC {
            D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
                Width: u64::from(side),
                Height: side,
                DepthOrArraySize: 1,
                MipLevels: levels,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                Layout: if reserved { D3D12_TEXTURE_LAYOUT_64KB_UNDEFINED_SWIZZLE } else { D3D12_TEXTURE_LAYOUT_UNKNOWN },
                ..Default::default()
            }
        }

        /// A texture without memory, in the state copies into it need, and
        /// how the card cuts it.
        fn reserved(&self, desc: &D3D12_RESOURCE_DESC) -> (ID3D12Resource, Tiled) {
            let mut made: Option<ID3D12Resource> = None;
            // SAFETY: plain creation; a texture that is no target has no clear value.
            let asked = unsafe {
                self.device
                    .CreateReservedResource(desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made)
            };
            // The layer's reason first, if it refused.
            self.check("a texture without memory made");
            asked.expect("a reserved texture");
            let texture = made.expect("a reserved texture");
            let tiled = Tiled::make(&self.device, &texture, u32::from(desc.MipLevels)).expect("the card says how it cuts the texture");
            (texture, tiled)
        }

        fn footprints(&self, desc: &D3D12_RESOURCE_DESC) -> Laid {
            let levels = usize::from(desc.MipLevels);
            let mut footprints = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); levels];
            let mut rows = vec![0u32; levels];
            let mut wide = vec![0u64; levels];
            let mut total = 0u64;
            // SAFETY: outputs for `levels` subresources.
            unsafe {
                self.device.GetCopyableFootprints(
                    desc,
                    0,
                    levels as u32,
                    0,
                    Some(footprints.as_mut_ptr()),
                    Some(rows.as_mut_ptr()),
                    Some(wide.as_mut_ptr()),
                    Some(&mut total),
                );
            }
            Laid {
                footprints,
                rows,
                wide,
                total,
            }
        }

        /// An upload buffer of `total` bytes in which the bytes of each
        /// level are `byte(level)`.
        fn filled(&self, layout: &Laid, byte: impl Fn(usize) -> u8) -> ID3D12Resource {
            let (footprints, rows) = (&layout.footprints, &layout.rows);
            let upload = self.buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_GENERIC_READ, layout.total);
            // SAFETY: the buffer is `total` bytes; each level's rows lie inside it.
            unsafe {
                let mut data = std::ptr::null_mut();
                upload.Map(0, None, Some(&mut data)).unwrap();
                for (level, f) in footprints.iter().enumerate() {
                    let len = rows[level] as usize * f.Footprint.RowPitch as usize;
                    std::slice::from_raw_parts_mut(data.cast::<u8>().add(f.Offset as usize), len).fill(byte(level));
                }
                upload.Unmap(0, None);
            }
            upload
        }

        /// Copies every level of `texture` from `upload`, then every level
        /// back, and gives for each level the first sixteen bytes read of
        /// its first row and of the row and column half way (another tile
        /// of a level of several).
        fn round_trip(
            &mut self,
            texture: &ID3D12Resource,
            layout: &Laid,
            upload: Option<&ID3D12Resource>,
        ) -> Vec<[Vec<u8>; 2]> {
            let (footprints, rows) = (&layout.footprints, &layout.rows);
            let back = self.buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST, layout.total);
            self.run(|list| {
                // SAFETY: an open list; the texture and buffers outlive the run.
                unsafe {
                    if let Some(upload) = upload {
                        for (level, f) in footprints.iter().enumerate() {
                            list.CopyTextureRegion(&sub(texture, level as u32), 0, 0, 0, &placed(upload, *f), None);
                        }
                    }
                    list.ResourceBarrier(&[transition(texture, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                    for (level, f) in footprints.iter().enumerate() {
                        list.CopyTextureRegion(&placed(&back, *f), 0, 0, 0, &sub(texture, level as u32), None);
                    }
                    list.ResourceBarrier(&[transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST)]);
                }
            });
            let mut read = Vec::new();
            // SAFETY: a read-back buffer of `total` bytes the finished list copied into.
            unsafe {
                let mut data = std::ptr::null_mut();
                back.Map(0, None, Some(&mut data)).unwrap();
                for (level, f) in footprints.iter().enumerate() {
                    let pitch = f.Footprint.RowPitch as usize;
                    let len = rows[level] as usize * pitch;
                    let bytes = std::slice::from_raw_parts(data.cast::<u8>().cast_const().add(f.Offset as usize), len);
                    // The bytes of a row that are the level's own (its
                    // blocks or texels; the rest of the pitch is padding).
                    let own = layout.wide[level] as usize;
                    let wide = own.min(len).min(16);
                    let first = bytes[..wide].to_vec();
                    let middle = (rows[level] as usize / 2) * pitch + ((own / 2) & !15);
                    let far = if rows[level] >= 2 && own >= 32 && middle + wide <= len {
                        bytes[middle..middle + wide].to_vec()
                    } else {
                        first.clone()
                    };
                    read.push([first, far]);
                }
                back.Unmap(0, None);
            }
            read
        }
    }

    fn sub(texture: &ID3D12Resource, index: u32) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: a borrowed pointer the location never releases.
            pResource: unsafe { std::mem::transmute_copy(texture) },
            Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { SubresourceIndex: index },
        }
    }

    fn placed(buffer: &ID3D12Resource, footprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT) -> D3D12_TEXTURE_COPY_LOCATION {
        D3D12_TEXTURE_COPY_LOCATION {
            // SAFETY: as above.
            pResource: unsafe { std::mem::transmute_copy(buffer) },
            Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
            Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 { PlacedFootprint: footprint },
        }
    }

    fn all(bytes: &[Vec<u8>; 2], want: u8) -> bool {
        bytes.iter().all(|part| part.iter().all(|&b| b == want))
    }

    /// The levels from `first` on, a bit a level.
    fn from_level(first: u32) -> u32 {
        (u32::MAX << first) & ((1 << u32::from(LEVELS)) - 1)
    }

    /// (i) Levels arrive one by one: after each step every level with
    /// tiles reads back its own bytes and every level without reads zeros.
    fn levels_arrive(rig: &mut Rig, format: DXGI_FORMAT) {
        rig.step = if format == DXGI_FORMAT_BC7_UNORM { "step 1, levels arrive (BC7)" } else { "step 1, levels arrive (BC3)" };
        let desc = Rig::desc(format, SIDE, LEVELS, true);
        let (texture, tiled) = rig.reserved(&desc);
        // (The card packs the levels smaller than a tile into shared
        // tiles; the software adapter may give each its own.)
        eprintln!(
            "format {}: {} levels with tiles of their own ({:?} tiles), {} tiles for the packed ones",
            format.0,
            tiled.standard.len(),
            tiled.standard,
            tiled.packed
        );
        assert!(!rig.card || tiled.packed > 0, "on the card a 1024 texture has levels smaller than a tile");
        let layout = rig.footprints(&desc);
        let upload = rig.filled(&layout, |level| level as u8 + 1);
        for first in [3u32, 1, 0] {
            // Ahead of the list on the queue, as at a submission.
            rig.give(&tiled, &texture, from_level(first));
            let read = rig.round_trip(&texture, &layout, Some(&upload));
            for (level, bytes) in read.iter().enumerate() {
                let want = if tiled.has_tiles(level as u32) { level as u8 + 1 } else { 0 };
                assert!(all(bytes, want), "format {} from level {first}: level {level} reads {bytes:?}, wanted {want}", format.0);
            }
        }
        assert!(tiled.whole());
        assert_eq!(tiled.first_shown(), Some(0));
    }

    /// (ii) A copy on the card into a level without tiles is dropped; the
    /// same copy in a list that runs after the level was given lands.
    fn a_card_copy_lands_after_the_give(rig: &mut Rig) {
        rig.step = "step 2, a copy on the card into a level without tiles";
        let format = DXGI_FORMAT_BC7_UNORM;
        let desc = Rig::desc(format, SIDE, LEVELS, true);
        let (texture, tiled) = rig.reserved(&desc);
        let layout = rig.footprints(&desc);
        // The last level with tiles of its own, and a texture of its size
        // with every byte 0x5a to copy from.
        let level = tiled.standard.len() as u32 - 1;
        let source_desc = Rig::desc(format, SIDE >> level, 1, false);
        let source_layout = rig.footprints(&source_desc);
        let source_bytes = rig.filled(&source_layout, |_| 0x5a);
        let mut made: Option<ID3D12Resource> = None;
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        // SAFETY: plain texture creation.
        unsafe {
            rig.device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &source_desc, D3D12_RESOURCE_STATE_COPY_DEST, None, &mut made)
                .unwrap();
        }
        let source = made.unwrap();
        rig.run(|list| {
            // SAFETY: an open list and live resources.
            unsafe {
                list.CopyTextureRegion(&sub(&source, 0), 0, 0, 0, &placed(&source_bytes, source_layout.footprints[0]), None);
                list.ResourceBarrier(&[transition(&source, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                // No tiles yet: this copy goes nowhere.
                list.CopyTextureRegion(&sub(&texture, level), 0, 0, 0, &sub(&source, 0), None);
            }
        });
        let before = rig.round_trip(&texture, &layout, None);
        assert!(all(&before[level as usize], 0), "a copy into a level without tiles left {:?}", before[level as usize]);
        // As `texture.rs` `card_writes` has it: the give at the list's
        // submission, the copy in the list.
        assert_ne!(tiled.claim(tiled.with_level(level)), 0, "the level is new");
        rig.give(&tiled, &texture, tiled.with_level(level));
        rig.run(|list| {
            // SAFETY: as above.
            unsafe { list.CopyTextureRegion(&sub(&texture, level), 0, 0, 0, &sub(&source, 0), None) };
        });
        let after = rig.round_trip(&texture, &layout, None);
        assert!(all(&after[level as usize], 0x5a), "the copy after the give left {:?}", after[level as usize]);
        assert_eq!(tiled.claim(tiled.with_level(level)), 0, "claimed once");
    }

    /// (iii) Tiles a texture gave back go to the next one, which reads
    /// what was written into it and zeros where it has no tiles; what a
    /// level given and not written reads is printed (the reason no level
    /// is given outside a list that writes it).
    fn a_reused_tile_reads_as_written(rig: &mut Rig) {
        rig.step = "step 3, a reused tile";
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let layout = rig.footprints(&desc);
        let (first, first_tiles) = rig.reserved(&desc);
        let level = first_tiles.standard.len() as u32 - 1;
        rig.give(&first_tiles, &first, 1 << level);
        let upload = rig.filled(&layout, |_| 0xa5);
        let read = rig.round_trip(&first, &layout, Some(&upload));
        assert!(all(&read[level as usize], 0xa5));
        let runs = first_tiles.given.lock().unwrap().runs.clone();
        drop(first_tiles);
        drop(first);
        let (second, second_tiles) = rig.reserved(&desc);
        rig.give(&second_tiles, &second, 1 << level);
        let reused = second_tiles.given.lock().unwrap().runs == runs;
        let unwritten = rig.round_trip(&second, &layout, None);
        eprintln!(
            "a level given tiles and not written reads {:?} (its tiles are the first texture's: {reused})",
            &unwritten[level as usize][0][..4]
        );
        let upload = rig.filled(&layout, |_| 0x3c);
        let read = rig.round_trip(&second, &layout, Some(&upload));
        assert!(all(&read[level as usize], 0x3c), "the reused level reads {:?}", read[level as usize]);
        assert!(all(&read[0], 0), "level 0 has no tiles and reads {:?}", read[0]);
    }

    /// (iv) A level whose tiles come from several runs of the heap (single
    /// free tiles between taken ones) reads as written in every tile.
    fn a_level_on_several_ranges(rig: &mut Rig) {
        rig.step = "step 4, a level on several ranges of the heap";
        // Three single tiles; the first and the last go back, the middle
        // one stays taken.
        let singles: Vec<_> = (0..3).map(|_| take(&rig.device, 1).expect("a tile")).collect();
        give_back(singles[0].1, &singles[0].2);
        give_back(singles[2].1, &singles[2].2);
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let layout = rig.footprints(&desc);
        let (texture, tiled) = rig.reserved(&desc);
        // Level 1 of a 1024 texture: four tiles.
        assert!(tiled.standard.len() > 1 && tiled.standard[1] >= 2);
        rig.give(&tiled, &texture, 1 << 1);
        let runs = tiled.given.lock().unwrap().runs.len();
        eprintln!("level 1 ({} tiles) was mapped on {runs} ranges", tiled.standard[1]);
        assert!(runs >= 2, "the single free tiles were not used first");
        let upload = rig.filled(&layout, |_| 0x77);
        let read = rig.round_trip(&texture, &layout, Some(&upload));
        assert!(all(&read[1], 0x77), "level 1 reads {:?}", read[1]);
        give_back(singles[1].1, &singles[1].2);
    }

    /// (v) A view made by [`show_from`] of a texture whose level 0 has no
    /// tiles returns level 1's colour for a sample at level 0; the view as
    /// the title asked returns zero.
    fn the_view_starts_at_the_first_level_with_tiles(rig: &mut Rig) -> Option<()> {
        rig.step = "step 5, the view from the first level with tiles";
        let cs = compile(LOOK, s!("look"), s!("cs_5_1")).ok()?;
        let format = DXGI_FORMAT_R8G8B8A8_UNORM;
        let desc = Rig::desc(format, SIDE, LEVELS, true);
        let layout = rig.footprints(&desc);
        let (texture, tiled) = rig.reserved(&desc);
        // Every level but the largest; each level's bytes are sixteen
        // times its number plus one.
        rig.give(&tiled, &texture, from_level(1));
        assert_eq!(tiled.first_shown(), Some(1));
        let upload = rig.filled(&layout, |level| (level as u8 + 1) * 16);
        rig.round_trip(&texture, &layout, Some(&upload));
        // SAFETY: plain object creation; the views are made in a heap
        // that lives to the end of the function, of a live texture.
        let (root, pipeline, views, words, back) = unsafe {
            let root: ID3D12RootSignature = rig.device.CreateRootSignature(0, &cs[..]).ok()?;
            let pipeline: ID3D12PipelineState = rig
                .device
                .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                    pRootSignature: std::mem::transmute_copy(&root),
                    CS: D3D12_SHADER_BYTECODE {
                        pShaderBytecode: cs.as_ptr().cast(),
                        BytecodeLength: cs.len(),
                    },
                    ..Default::default()
                })
                .ok()?;
            let views: ID3D12DescriptorHeap = rig
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: 2,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            let as_asked = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
                Shader4ComponentMapping: 5768,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2D: D3D12_TEX2D_SRV {
                        MostDetailedMip: 0,
                        MipLevels: u32::MAX,
                        PlaneSlice: 0,
                        ResourceMinLODClamp: 0.0,
                    },
                },
            };
            let mut shown = as_asked;
            show_from(&mut shown, tiled.first_shown()?, u32::from(LEVELS));
            assert_eq!(shown.Anonymous.Texture2D.MostDetailedMip, 1);
            let step = rig.device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV) as usize;
            let mut at = views.GetCPUDescriptorHandleForHeapStart();
            rig.device.CreateShaderResourceView(&texture, Some(&raw const shown), at);
            at.ptr += step;
            rig.device.CreateShaderResourceView(&texture, Some(&raw const as_asked), at);
            let words = rig.buffer(
                D3D12_HEAP_TYPE_DEFAULT,
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                256,
            );
            let back = rig.buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_FLAG_NONE, D3D12_RESOURCE_STATE_COPY_DEST, 256);
            (root, pipeline, views, words, back)
        };
        // Before the one dispatch: the pipeline and both views are sound.
        rig.check("the pipeline and the views made");
        rig.run(|list| {
            // SAFETY: an open list and live objects.
            unsafe {
                list.ResourceBarrier(&[transition(&texture, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE)]);
                list.SetDescriptorHeaps(&[Some(views.clone())]);
                list.SetComputeRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetComputeRootDescriptorTable(0, views.GetGPUDescriptorHandleForHeapStart());
                list.SetComputeRootUnorderedAccessView(1, words.GetGPUVirtualAddress());
                list.Dispatch(1, 1, 1);
                list.ResourceBarrier(&[transition(&words, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE)]);
                list.CopyBufferRegion(&back, 0, &words, 0, 16);
            }
        });
        // SAFETY: a read-back buffer the finished list copied sixteen bytes into.
        let read = unsafe {
            let mut data = std::ptr::null_mut();
            let range = D3D12_RANGE { Begin: 0, End: 16 };
            back.Map(0, Some(&range), Some(&mut data)).unwrap();
            let read = std::slice::from_raw_parts(data.cast::<u32>(), 2).to_vec();
            back.Unmap(0, None);
            read
        };
        eprintln!("a sample at level 0: {} through the view from level 1, {} through the view as asked", read[0], read[1]);
        assert_eq!(read[0], 32, "the view from the first level with tiles reads level 1");
        assert_eq!(read[1], 0, "the view from level 0 reads a level without tiles");
        Some(())
    }

    fn a_full_card_keeps_new_levels_waiting(rig: &mut Rig) {
        rig.step = "step 6, no room on the card: held levels wait, and get their tiles when asked again";
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let (texture, tiled) = rig.reserved(&desc);
        let layout = rig.footprints(&desc);
        let upload = rig.filled(&layout, |level| level as u8 + 1);
        let read_as_given = |rig: &mut Rig, what: &str| {
            let read = rig.round_trip(&texture, &layout, Some(&upload));
            for (level, bytes) in read.iter().enumerate() {
                let want = if tiled.has_tiles(level as u32) { level as u8 + 1 } else { 0 };
                assert!(all(bytes, want), "{what}: level {level} reads {bytes:?}, wanted {want}");
            }
        };
        // No room: nothing is given, the held levels are said to wait,
        // nothing is shown, every level reads zeros.
        let (changed, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(3), 0);
        rig.check("an upload without room");
        assert!(!changed && waiting, "without room: changed {changed}, waiting {waiting}");
        assert_eq!(tiled.given_bytes(), 0, "without room no tile is given");
        assert_eq!(tiled.first_shown(), None);
        read_as_given(rig, "without room");
        // Asked again with room: the same levels get theirs.
        let (changed, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(3), u64::MAX);
        rig.check("the upload again, with room");
        assert!(changed && !waiting, "with room: changed {changed}, waiting {waiting}");
        assert!(tiled.has_tiles(3) && !tiled.has_tiles(2));
        assert_eq!(tiled.first_shown(), Some(3));
        read_as_given(rig, "with room");
        // More levels held and no room again: the ones that have tiles
        // are brought up to date, the new ones wait, the view stays.
        let before = tiled.given_bytes();
        let (changed, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(1), 0);
        rig.check("more levels held, without room");
        assert!(!changed && waiting, "more levels without room: changed {changed}, waiting {waiting}");
        assert_eq!(tiled.given_bytes(), before);
        assert_eq!(tiled.first_shown(), Some(3));
        read_as_given(rig, "more levels without room");
        // The card's own write is not kept waiting, and brings only its
        // own level: level 1 still waits although it is held.
        assert_eq!(tiled.claim(1 << 0), 1 << 0);
        let changed = tiled.card_wrote_on(&rig.queue, &rig.device, &texture, 1 << 0);
        rig.check("the card's own write, without room");
        assert!(changed && tiled.has_tiles(0) && !tiled.has_tiles(1) && !tiled.has_tiles(2));
        assert_eq!(tiled.first_shown(), Some(0));
        assert_eq!(tiled.claim(1 << 0), 0, "a level with tiles is not claimed again");
        // With room everything held gets its tiles.
        let (_, waiting) = tiled.uploaded_on(&rig.queue, &rig.device, &texture, from_level(1), u64::MAX);
        rig.check("the last upload, with room");
        assert!(!waiting && tiled.whole());
        read_as_given(rig, "everything given");
    }

    fn a_claim_given_up_is_made_again(rig: &mut Rig) {
        rig.step = "step 7, a claim whose zeros never landed is given up";
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let (_texture, tiled) = rig.reserved(&desc);
        let level = tiled.with_level(2);
        assert_eq!(tiled.claim(level), level, "the level is new");
        assert_eq!(tiled.claim(level), 0, "claimed once");
        // The list that claimed it was reset without being submitted.
        tiled.unclaim(level);
        assert_eq!(tiled.claim(level), level, "claimed again after it was given up");
        // The small levels the card packs together are claimed together.
        let small = tiled.with_level(u32::from(LEVELS) - 1);
        assert!(!rig.card || small.count_ones() > 1, "on the card a 1024 texture has several levels in one tile");
        assert_eq!(tiled.claim(1 << (u32::from(LEVELS) - 1)), 1 << (u32::from(LEVELS) - 1));
        tiled.unclaim(small);
        assert_eq!(tiled.claim(small), small);
    }

    fn a_retired_level_gives_its_tiles_back(rig: &mut Rig) {
        rig.step = "step 8, a level taken back";
        let desc = Rig::desc(DXGI_FORMAT_BC7_UNORM, SIDE, LEVELS, true);
        let (texture, tiled) = rig.reserved(&desc);
        rig.give(&tiled, &texture, tiled.every_level());
        assert!(tiled.whole());
        tiled.held.store(tiled.every_level(), Ordering::Relaxed);
        let before = tiled.given_bytes();
        let (begun, changed) = tiled.retire(1 << 0);
        assert_eq!((begun, changed), (1, true));
        assert_eq!(tiled.first_shown(), Some(1));
        // The tiles stay until the mapping is made null.
        assert_eq!(tiled.given_bytes(), before);
        let freed = tiled.unmap(&rig.queue, &texture, 1 << 0);
        rig.check("tiles taken back");
        assert_eq!(freed, u64::from(tiled.standard[0]) * TILE);
        assert_eq!(tiled.given_bytes(), before - freed);
        assert!(!tiled.has_tiles(0) && tiled.has_tiles(1));
        // Taken back for good only once: the level is given again on demand.
        assert_eq!(tiled.unmap(&rig.queue, &texture, 1 << 0), 0);
        rig.give(&tiled, &texture, 1 << 0);
        assert!(tiled.has_tiles(0));
    }

    /// On the software adapter unless `VICTORY_TILES_TEST_CARD=1`.
    #[test]
    #[ignore = "on the card: run it by name with --ignored --nocapture"]
    fn levels_on_demand_on_the_card() {
        let Some(mut rig) = Rig::new() else {
            eprintln!("no adapter with tiled resources of tier 2, or no debug layer for a run on the card; skipped");
            return;
        };
        rig.check("the device made");
        levels_arrive(&mut rig, DXGI_FORMAT_BC7_UNORM);
        levels_arrive(&mut rig, DXGI_FORMAT_BC3_UNORM);
        a_card_copy_lands_after_the_give(&mut rig);
        a_reused_tile_reads_as_written(&mut rig);
        a_level_on_several_ranges(&mut rig);
        a_full_card_keeps_new_levels_waiting(&mut rig);
        a_claim_given_up_is_made_again(&mut rig);
        a_retired_level_gives_its_tiles_back(&mut rig);
        if the_view_starts_at_the_first_level_with_tiles(&mut rig).is_none() {
            eprintln!("the view test's shader or objects could not be made; that part was skipped");
        }
        rig.step = "the end";
        rig.check("every step done");
        eprintln!(
            "all steps passed {}",
            if rig.card { "ON THE CARD" } else { "on the software adapter; VICTORY_TILES_TEST_CARD=1 is the run on the card" }
        );
        let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
        eprintln!(
            "{} calls of UpdateTileMappings took {} microseconds in all ({} a call); the pool's heaps {} MB, {} given back",
            s[6],
            s[7],
            s[7] / s[6].max(1),
            s[3] >> 20,
            s[8]
        );
    }

    /// A texture of four levels with tiles of their own (64, 16, 4 and 1
    /// tiles: 4 MiB, 1 MiB, 256 KiB and 64 KiB) and one packed tile.
    fn four_levels() -> Tiled {
        Tiled {
            standard: vec![64, 16, 4, 1],
            packed: 1,
            levels: 7,
            full: 0,
            given: Mutex::new(Given { levels: vec![false; 4], packed: false, runs: Vec::new() }),
            shown: AtomicU32::new(u32::MAX),
            held: AtomicU32::new(0),
            card: AtomicU32::new(0),
            claimed: AtomicU32::new(0),
            retiring: AtomicU32::new(0),
            sampled: AtomicU64::new(0),
            touched: AtomicU64::new(0),
            given_at: AtomicU64::new(0),
            short: AtomicU64::new(0),
            short_at: AtomicU64::new(0),
        }
    }

    /// `four_levels` with every level given its tiles, by hand.
    fn four_levels_given() -> Tiled {
        let tiled = four_levels();
        {
            let mut given = tiled.given.lock().unwrap();
            given.levels = vec![true; 4];
            given.packed = true;
        }
        tiled
    }

    #[test]
    fn levels_the_title_let_go_are_dead_weight_and_retiring_hides_them() {
        let tiled = four_levels_given();
        let every = tiled.every_level();
        tiled.held.store(every, Ordering::Relaxed);
        assert_eq!(tiled.dead(), 0, "all held: nothing is dead");
        {
            let given = tiled.given.lock().unwrap();
            tiled.reshown(&given, every);
        }
        assert_eq!(tiled.first_shown(), Some(0));
        // The title lets go of levels 0 and 1.
        tiled.held.store(every & !0b11, Ordering::Relaxed);
        assert_eq!(tiled.dead(), 0b11);
        // The card's own write of level 0 keeps it.
        tiled.card.store(0b1, Ordering::Relaxed);
        assert_eq!(tiled.dead(), 0b10);
        tiled.card.store(0, Ordering::Relaxed);
        // Retiring them: not shown at once, begun only once.
        let (begun, changed) = tiled.retire(0b11);
        assert_eq!((begun, changed), (0b11, true));
        assert_eq!(tiled.first_shown(), Some(2));
        assert_eq!(tiled.retire(0b11), (0, false), "not begun twice");
        assert_eq!(tiled.dead(), 0, "on their way back already");
        // The packed levels are dead together, and only when none is held.
        tiled.held.store(0b0000_1100, Ordering::Relaxed);
        assert_eq!(tiled.dead(), 0b111_0000);
        tiled.held.store(0b0100_1100, Ordering::Relaxed);
        assert_eq!(tiled.dead(), 0, "one packed level held keeps the block");
    }

    #[test]
    fn a_level_the_title_holds_again_is_not_hidden_by_a_pending_retire() {
        let tiled = four_levels_given();
        tiled.held.store(tiled.every_level(), Ordering::Relaxed);
        let (begun, _) = tiled.retire(0b1);
        assert_eq!(begun, 0b1);
        assert_eq!(tiled.retiring.load(Ordering::Relaxed), 0b1);
        // What `uploaded_on` does for a level it names and the cap takes.
        tiled.retiring.fetch_and(!(tiled.every_level() & tiled.fitting(tiled.every_level(), u64::MAX)), Ordering::Relaxed);
        assert_eq!(tiled.retiring.load(Ordering::Relaxed), 0);
        assert_eq!(tiled.big_given(256 << 10), (0b11, 5 << 20), "levels of 4 MiB and 1 MiB are the large ones");
    }

    #[test]
    fn the_shortfall_counts_the_tiles_of_the_held_levels_without_any() {
        let tiled = four_levels();
        tiled.held.store(0b1111, Ordering::Relaxed);
        assert_eq!(tiled.deficit(), (64 + 16 + 4 + 1) * TILE);
        tiled.given.lock().unwrap().levels[0] = true;
        assert_eq!(tiled.deficit(), (16 + 4 + 1) * TILE);
        // The packed block counts once, when any of its levels is held.
        tiled.held.store(0b0111_1111, Ordering::Relaxed);
        assert_eq!(tiled.deficit(), (16 + 4 + 1 + 1) * TILE);
        tiled.given.lock().unwrap().packed = true;
        assert_eq!(tiled.deficit(), (16 + 4 + 1) * TILE);
    }

    #[test]
    fn a_hot_texture_short_of_levels_is_asked_again_once_a_second_when_the_card_takes_more() {
        let tiled = four_levels();
        // Not short: never.
        assert!(!tiled.retry_hot(|_| true));
        tiled.set_short(256 << 10);
        // Just asked: not again yet.
        assert!(!tiled.retry_hot(|_| true));
        tiled.short_at.store(0, Ordering::Relaxed);
        // Not read through a table: not hot.
        assert!(!tiled.retry_hot(|_| true));
        tiled.short_at.store(0, Ordering::Relaxed);
        tiled.note_sampled();
        assert_eq!(tiled.heat(), crate::priority::Heat::Hot);
        // The card does not take more than the cap it was short under.
        assert!(!tiled.retry_hot(|cap| cap < 256 << 10));
        tiled.short_at.store(0, Ordering::Relaxed);
        assert!(tiled.retry_hot(|cap| cap == 256 << 10));
        assert!(!tiled.retry_hot(|_| true), "once a second");
    }

    #[test]
    fn levels_that_do_not_fit_the_cap_are_left_out() {
        let tiled = four_levels();
        let every = tiled.every_level();
        assert_eq!(every, 0b111_1111);
        // Room for anything: everything held.
        assert_eq!(tiled.fitting(every, u64::MAX), every);
        // 4 MiB: all four levels with tiles of their own and the packed ones.
        assert_eq!(tiled.fitting(every, 4 << 20), every);
        // 1 MiB: level 0 goes.
        assert_eq!(tiled.fitting(every, 1 << 20), every & !0b1);
        // 256 KiB: levels 0 and 1 go.
        assert_eq!(tiled.fitting(every, 256 << 10), every & !0b11);
        // One tile: only the single-tile level and the packed ones.
        assert_eq!(tiled.fitting(every, 64 << 10), 0b111_1000);
        // None at all: nothing.
        assert_eq!(tiled.fitting(every, 0), 0);
        // Only levels named are asked about.
        assert_eq!(tiled.fitting(0b0000_0110, 1 << 20), 0b0000_0110);
        assert_eq!(tiled.fitting(0b0000_0101, 1 << 20), 0b0000_0100);
    }

    #[test]
    fn floor_saturates_instead_of_wrapping() {
        assert_eq!(parse_floor(None), 1 << 20);
        assert_eq!(parse_floor(Some("0")), 64 << 10);
        assert_eq!(parse_floor(Some("2048")), 2048 << 10);
        assert_eq!(parse_floor(Some(&(1u64 << 54).to_string())), u64::MAX);
        assert_eq!(parse_floor(Some(&u64::MAX.to_string())), u64::MAX);
    }

    #[test]
    fn show_from_clamps_2d_and_2d_array_and_adjusts_min_lod() {
        let mut srv2d = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
            Shader4ComponentMapping: 5768,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 12,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 3.5,
                },
            },
        };
        show_from(&mut srv2d, 3, 11);
        // SAFETY: `ViewDimension` is `TEXTURE2D`.
        let v2d = unsafe { srv2d.Anonymous.Texture2D };
        assert_eq!(v2d.MostDetailedMip, 3);
        // Clamped to remaining levels (11 - 3 = 8), not 12 - 3 = 9.
        assert_eq!(v2d.MipLevels, 8);
        assert_eq!(v2d.ResourceMinLODClamp, 0.5);

        let mut srv_arr = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
            Shader4ComponentMapping: 5768,
            Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                    MostDetailedMip: 0,
                    MipLevels: 6,
                    FirstArraySlice: 0,
                    ArraySize: 1,
                    PlaneSlice: 0,
                    ResourceMinLODClamp: 1.5,
                },
            },
        };
        show_from(&mut srv_arr, 2, 11);
        // SAFETY: `ViewDimension` is `TEXTURE2DARRAY`.
        let varr = unsafe { srv_arr.Anonymous.Texture2DArray };
        assert_eq!(varr.MostDetailedMip, 2);
        assert_eq!(varr.MipLevels, 4);
        assert_eq!(varr.ResourceMinLODClamp, 0.0);
    }

    #[test]
    fn card_wrote_failure_does_not_poison_card_mask() {
        let Some(host) = host() else {
            return;
        };
        // A dummy live resource (never mapped because `take()` rejects `HEAP_TILES + 1`).
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = Rig::desc(DXGI_FORMAT_R8G8B8A8_UNORM, 64, 2, false);
        let mut tex: Option<ID3D12Resource> = None;
        // SAFETY: plain texture creation on the host device.
        unsafe {
            host.device
                .CreateCommittedResource(
                    &heap,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut tex,
                )
                .unwrap();
        }
        let tex = tex.unwrap();
        STATS[0].fetch_add(1, Ordering::Relaxed);
        STATS[1].fetch_add(TILE, Ordering::Relaxed);
        let mut tiled = Tiled {
            // Level 0 demands > HEAP_TILES so `take()` fails deterministically.
            standard: vec![HEAP_TILES + 1, 1],
            packed: 0,
            levels: 2,
            full: TILE,
            given: Mutex::new(Given {
                levels: vec![false, false],
                packed: false,
                runs: Vec::new(),
            }),
            shown: AtomicU32::new(u32::MAX),
            held: AtomicU32::new(0),
            card: AtomicU32::new(0),
            claimed: AtomicU32::new(0),
            retiring: AtomicU32::new(0),
            sampled: AtomicU64::new(0),
            touched: AtomicU64::new(0),
            given_at: AtomicU64::new(0),
            short: AtomicU64::new(0),
            short_at: AtomicU64::new(0),
        };
        assert_eq!(tiled.claim(1 << 0), 1 << 0);
        let changed = tiled.card_wrote_on(host.queue(), &host.device, &tex, 1 << 0);
        assert!(!changed);
        assert_eq!(tiled.card.load(Ordering::Relaxed), 0, "failed tile allocation must not poison `card`");
        assert_eq!(tiled.first_shown(), None);
        assert_eq!(tiled.claim(1 << 0), 1 << 0, "failed level must be unclaimed");
        // Even if level 0 later becomes allocatable, an upload with `room == false`
        // that does not hold level 0 must not treat level 0 as card-owned.
        tiled.standard[0] = 1;
        let (changed, waiting) = tiled.uploaded_on(host.queue(), &host.device, &tex, 1 << 1, 0);
        assert!(!changed && waiting);
        assert!(!tiled.has_tiles(0));
        assert_eq!(tiled.given_bytes(), 0);
    }
}
