//! The card's memory against the system's budget (GPU1).
//!
//! On an 8 GB card the layer sits 0.1 to 0.6 GB past the budget the system
//! gives the process for the whole of a run (runs I8, I9: 7,07 to 7,67 GB in
//! use of 7,03 to 7,14 GB), and nothing in the layer says what the system
//! does about it: it never refuses an allocation, it moves allocations to
//! the PC's memory when it likes (least recently used first, by priority),
//! and no D3D12 call says which ones. This module has three parts:
//!
//! - [`log`], the line with the card's memory by category, how much of the
//!   overshoot is memory nothing reads ([`Report::idle`]: tile heaps with no
//!   tile in them, blocks of textures with no texture), and a count of the
//!   moments the card's own use fell while the PC's memory shared with it
//!   rose, which is how a move to the PC's memory shows from here
//!   ([`Watch`]).
//! - [`prioritize`], residency priorities (`VICTORY_RESIDENCY_PRIORITY=1`):
//!   what every frame reads (card copies of title buffers, blocks of
//!   textures, which hold the render targets) is asked to stay on the card
//!   after the tile heaps, which hold the levels textures read sparsely.
//! - the policy of [`trim_ready`] (`VICTORY_TILE_TRIM=1`), which `tiles`
//!   uses to give back heaps of tiles with no tile in them.
//! - [`census`] (VRAM2): the rest of the card's use by who asked for it (the
//!   blocks of textures by how full they are, committed textures by kind,
//!   device-local buffers by the line that made them), and what is left.
//! - [`os`] (VRAM1): what the system's video memory manager says it did with
//!   our allocations (the kernel's statistics: demoted bytes by priority
//!   class, allocations in a segment they do not prefer, evictions) and the
//!   budget change events, in the line after this one's.
//! - [`hold`] (`VICTORY_VRAM_HOLD=1`, off by default): the fix for a card
//!   that works 25 to 30 ms a frame longer in some processes than in others
//!   with the use 0.3 to 0.6 GB higher: priorities set on the allocations
//!   the frame reads (maximum for targets and depth, high for the rest of
//!   what is hot), the heaps with no tile given back, and a limit on the
//!   card's whole use ([`gross_over`]) that no free room in a block counts
//!   against.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

pub(crate) mod census;
mod os;

const MB: u64 = 1 << 20;

/// How much the system should keep of an allocation of ours on the card
/// when it must move some to the PC's memory.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Read by every frame: card copies of title buffers, blocks of textures.
    High,
    /// Read in part and sparsely: heaps of tiles.
    Low,
    /// Written and read by every frame and moved at a price nothing else
    /// has (a frame's render targets and depth, the twins of depth
    /// textures): the last the system should move.
    Max,
}

impl Class {
    fn value(self) -> D3D12_RESIDENCY_PRIORITY {
        match self {
            Class::High => D3D12_RESIDENCY_PRIORITY_HIGH,
            Class::Low => D3D12_RESIDENCY_PRIORITY_LOW,
            Class::Max => D3D12_RESIDENCY_PRIORITY_MAXIMUM,
        }
    }

    fn at(self) -> usize {
        match self {
            Class::High => 0,
            Class::Low => 1,
            Class::Max => 2,
        }
    }
}

/// Whether the hold is on (`VICTORY_VRAM_HOLD=1`; off by default): residency
/// priorities on every hot allocation ([`priorities`]), the trim of empty
/// heaps ([`trim_on`]) and the limit of the card's whole use ([`gross_over`]).
pub(crate) fn hold() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VRAM_HOLD").is_some_and(|v| v == "1"))
}

/// How far past the system's budget the card's whole use may stand before
/// the texture room says no, in bytes: `VICTORY_VRAM_OVER_MB` (0 or more),
/// else 400 MB with the hold on, else no limit.
///
/// The texture room counts the free parts of the blocks and heaps as room
/// (`resource::card_use`), which is only so for what fits in the pieces they
/// have left: a card whose use stands 0.5 to 0.8 GB past its budget with a
/// third of the blocks free is as full as it is.
pub(crate) fn gross_over() -> Option<u64> {
    static OVER: std::sync::OnceLock<Option<u64>> = std::sync::OnceLock::new();
    *OVER.get_or_init(|| {
        match std::env::var("VICTORY_VRAM_OVER_MB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
        {
            Some(mb) => Some(mb * MB),
            None => hold().then_some(400 * MB),
        }
    })
}

/// The room left to the card's use under budget plus [`gross_over`], in bytes
/// (negative: past it); `i64::MAX` with no limit or no budget.
pub(crate) fn gross_room(budget: u64, used: u64) -> i64 {
    room_under(budget, used, gross_over())
}

fn room_under(budget: u64, used: u64, over: Option<u64>) -> i64 {
    match over {
        Some(over) if budget > 0 => {
            i64::try_from(budget.saturating_add(over)).unwrap_or(i64::MAX)
                - i64::try_from(used).unwrap_or(i64::MAX)
        }
        _ => i64::MAX,
    }
}

/// Whether residency priorities are set (`VICTORY_RESIDENCY_PRIORITY=1`; off
/// by default until a run at the window spot shows what they do).
pub(crate) fn priorities() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        hold() || std::env::var_os("VICTORY_RESIDENCY_PRIORITY").is_some_and(|v| v == "1")
    })
}

/// Allocations given a priority and their bytes, by class; calls that failed.
static SET: [[AtomicU64; 2]; 3] = [const { [const { AtomicU64::new(0) }; 2] }; 3];
static SET_FAILED: AtomicU64 = AtomicU64::new(0);

/// Asks the system to treat `object`, `bytes` of the card's memory, as of
/// `class` when it chooses what to move to the PC's memory. A failed call
/// changes nothing (the priority stays the default one) and is counted.
pub(crate) fn prioritize(device: &ID3D12Device, object: &impl Interface, bytes: u64, class: Class) {
    if !priorities() {
        return;
    }
    let done = (|| -> windows::core::Result<()> {
        let device = device.cast::<ID3D12Device1>()?;
        let pageable = object.cast::<ID3D12Pageable>()?;
        let objects = [Some(pageable)];
        let priority = [class.value()];
        // SAFETY: one live object of this device, one priority.
        unsafe { device.SetResidencyPriority(1, objects.as_ptr(), priority.as_ptr()) }
    })();
    match done {
        Ok(()) => {
            SET[class.at()][0].fetch_add(1, Ordering::Relaxed);
            SET[class.at()][1].fetch_add(bytes, Ordering::Relaxed);
        }
        Err(e) => {
            if SET_FAILED.fetch_add(1, Ordering::Relaxed) == 0 {
                tracing::warn!(error = %e, "SetResidencyPriority failed (said once; counted in the vram line)");
            }
        }
    }
}

/// [`prioritize`] for a resource of ours that is not a heap (`resource`'s
/// size is asked of the device, only with the priorities on).
pub(crate) fn prioritize_resource(device: &ID3D12Device, resource: &ID3D12Resource, class: Class) {
    if !priorities() {
        return;
    }
    // SAFETY: plain queries of a live resource and its device.
    let bytes = unsafe {
        let desc = resource.GetDesc();
        device.GetResourceAllocationInfo(0, &[desc]).SizeInBytes
    };
    prioritize(device, resource, bytes, class);
}

/// What the system says (budget and use of the card's own memory, use of the
/// PC's memory shared with it) and what our own counters hold, in bytes.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Parts {
    pub(crate) budget: u64,
    pub(crate) used: u64,
    pub(crate) shared_used: u64,
    /// Heaps of tiles, and the bytes of tiles given out of them.
    pub(crate) tile_heaps: u64,
    pub(crate) tiles_given: u64,
    /// Blocks of textures, and the bytes of them no texture holds.
    pub(crate) blocks: u64,
    pub(crate) blocks_free: u64,
    /// Card copies of title buffers.
    pub(crate) card_copies: u64,
    /// Host copies of title memory in the PC's memory, and staging blocks.
    pub(crate) pc_known: u64,
}

/// What [`Parts`] come to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Report {
    /// Bytes in use past the budget (0 when under it).
    pub(crate) over: u64,
    /// Bytes of tile heaps and blocks of textures that hold nothing: the
    /// system moves these first at no cost, nothing reads them.
    pub(crate) idle: u64,
    /// The part of `over` that is not idle: what the system must move
    /// although something reads it (at most; it moves what it likes).
    pub(crate) hot_over: u64,
    /// Bytes in use that no counter of ours names (the driver's, the
    /// pipelines', the upscaler's, descriptor and scratch memory...).
    pub(crate) unattributed: u64,
    /// Bytes the card's use would have to fall by to leave `margin` under
    /// the budget with nothing read moved: 0 once the idle bytes cover it.
    pub(crate) to_fit: u64,
}

pub(crate) fn assess(p: &Parts, margin: u64) -> Report {
    let over = p.used.saturating_sub(p.budget);
    let idle = p.tile_heaps.saturating_sub(p.tiles_given) + p.blocks_free;
    let named = p.tile_heaps + p.blocks + p.card_copies;
    let target = p.budget.saturating_sub(margin);
    Report {
        over,
        idle,
        hot_over: over.saturating_sub(idle),
        unattributed: p.used.saturating_sub(named),
        to_fit: p.used.saturating_sub(target).saturating_sub(idle),
    }
}

/// Moments the card's use fell while the PC's shared use rose: since the line
/// before, `local` bytes less and `shared` more. A move of allocations to
/// the PC's memory shows so; so does anything of ours that frees card memory
/// while making PC memory in the same few seconds (the line before names
/// the counters, so a reader can tell).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Moved {
    pub(crate) local: u64,
    pub(crate) shared: u64,
}

/// Looks at two successive readings (`(used, shared_used)`).
pub(crate) fn watch(before: (u64, u64), now: (u64, u64)) -> Option<Moved> {
    let fell = before.0.saturating_sub(now.0);
    let rose = now.1.saturating_sub(before.1);
    (fell >= 64 * MB && rose >= 32 * MB).then_some(Moved {
        local: fell,
        shared: rose,
    })
}

/// Whether a heap of tiles that has had no tile in it since `since`
/// (milliseconds, and the number of submissions begun then) may go back to
/// the card: two seconds and one submission begun after it was freed, and
/// finished, have passed (the null mapping that emptied it is in the queue
/// ahead of that submission), and it is not the pool's only heap.
pub(crate) fn trim_ready(since: (u64, u64), now_ms: u64, finished: u64, heaps: usize) -> bool {
    heaps > 1 && now_ms.saturating_sub(since.0) >= 2000 && finished > since.1
}

/// Heaps by how much of them is free: all of it, 3/4 or more, 1/2 or more,
/// less (`free` and `all` in tiles, one pair per heap).
pub(crate) fn occupancy(heaps: impl Iterator<Item = (u32, u32)>) -> [u32; 4] {
    let mut out = [0u32; 4];
    for (free, all) in heaps {
        let at = if free >= all {
            0
        } else if free * 4 >= all * 3 {
            1
        } else if free * 2 >= all {
            2
        } else {
            3
        };
        out[at] += 1;
    }
    out
}

/// Heaps given back by the trim, and bytes.
pub(crate) static TRIMMED: [AtomicU64; 2] = [const { AtomicU64::new(0) }; 2];

/// Whether wholly free heaps of tiles go back to the card
/// (`VICTORY_TILE_TRIM=1`, or with the hold or the pool's compaction; off by default).
pub(crate) fn trim_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        hold()
            || crate::pool::compact_on()
            || std::env::var_os("VICTORY_TILE_TRIM").is_some_and(|v| v == "1")
    })
}

/// Total moves seen and the last reading.
static MOVES: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
static LAST: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

/// The line: the card's memory by category, with the numbers [`assess`]
/// derives, once for each memory line (every five seconds).
pub(crate) fn log() {
    // The barriers the title sent, over the frames since the line before.
    static FRAME: AtomicU64 = AtomicU64::new(0);
    let frame = crate::frame::current();
    let frames = frame.saturating_sub(FRAME.swap(frame, Ordering::Relaxed));
    crate::barrier::line(frames as f64);
    crate::list::runs_line(frames as f64);
    crate::pipeline::skipped_line(frames as f64);
    let Some(host) = crate::host::host() else {
        return;
    };
    let Some([own, shared]) = host.video_memory_groups() else {
        return;
    };
    let (heaps, given) = crate::tiles::bytes();
    let card_copies = crate::shadow::card_bytes();
    let blocks = crate::pool::texture_bytes();
    let parts = Parts {
        budget: own[0],
        used: own[1],
        shared_used: shared[1],
        tile_heaps: heaps,
        tiles_given: given,
        blocks,
        blocks_free: crate::pool::texture_room(),
        card_copies,
        // Host copies of title memory not on the card, and the staging blocks.
        pc_known: crate::shadow::live_bytes().saturating_sub(card_copies)
            + crate::pool::staging_bytes(),
    };
    let r = assess(&parts, 256 * MB);
    let now = (parts.used, parts.shared_used);
    let had = LAST[2].swap(1, Ordering::Relaxed) == 1;
    let before = (
        LAST[0].swap(now.0, Ordering::Relaxed),
        LAST[1].swap(now.1, Ordering::Relaxed),
    );
    let moved = if had { watch(before, now) } else { None };
    if let Some(m) = &moved {
        MOVES[0].fetch_add(1, Ordering::Relaxed);
        MOVES[1].fetch_add(m.local, Ordering::Relaxed);
        MOVES[2].fetch_add(m.shared, Ordering::Relaxed);
    }
    let heat = crate::resource::HOST_TEXTURES[1].load(Ordering::Relaxed);
    let filled = crate::resource::KINDS[1].load(Ordering::Relaxed);
    let [free, three, half, fewer] = crate::tiles::occupancy();
    let left = crate::shadow::left_in_pc();
    let copies = crate::texture::UPLOAD_COPIES
        .each_ref()
        .map(|c| c.load(Ordering::Relaxed));
    tracing::info!(
        "vram (VICTORY_RESIDENCY_PRIORITY={}, VICTORY_TILE_TRIM={}): the card {} MB in use of a budget of {} MB ({} over); \
         tile heaps {} MB ({} MB given as tiles; {} heaps wholly free, {} 3/4 free, {} half free, {} less), \
         blocks of textures {} MB ({} MB free; host textures alive {} MB, {} MB of them filled for the title, the rest targets and twins), \
         card copies of title buffers {} MB, not named by any counter of ours {} MB; \
         idle (read by nothing, the system moves it first) {} MB, so {} MB over that something reads, {} MB to fall to leave 256 MB under the budget; \
         the PC's memory shared with the card {} MB in use ({} MB host copies and staging we know of, {} not); \
         moves seen (the card's use fell 64 MB or more while the PC's rose 32 MB or more between lines): {} ({} MB, {} MB){}; \
         title buffers left in the PC's memory: {} by the cap of copies ({} MB), {} by the line of the card's use ({} MB);          texture upload copy commands (VICTORY_UPLOAD_HELD_ONLY): {} recorded, {} left out for levels the title does not hold ({} MB the bus did not carry);          priorities set: {} high ({} MB), {} low ({} MB), {} failed; tile heaps given back by the trim: {} ({} MB)",
        u8::from(priorities()),
        u8::from(trim_on()),
        parts.used / MB,
        parts.budget / MB,
        r.over / MB,
        parts.tile_heaps / MB,
        parts.tiles_given / MB,
        free,
        three,
        half,
        fewer,
        parts.blocks / MB,
        parts.blocks_free / MB,
        heat / MB,
        filled / MB,
        parts.card_copies / MB,
        r.unattributed / MB,
        r.idle / MB,
        r.hot_over / MB,
        r.to_fit / MB,
        parts.shared_used / MB,
        parts.pc_known / MB,
        parts.shared_used.saturating_sub(parts.pc_known) / MB,
        MOVES[0].load(Ordering::Relaxed),
        MOVES[1].load(Ordering::Relaxed) / MB,
        MOVES[2].load(Ordering::Relaxed) / MB,
        moved.map_or(String::new(), |m| format!(
            ", one just now: card -{} MB, PC +{} MB",
            m.local / MB,
            m.shared / MB
        )),
        left[0],
        left[1] / MB,
        left[2],
        left[3] / MB,
        copies[0],
        copies[1],
        copies[2] / MB,
        SET[0][0].load(Ordering::Relaxed),
        SET[0][1].load(Ordering::Relaxed) / MB,
        SET[1][0].load(Ordering::Relaxed),
        SET[1][1].load(Ordering::Relaxed) / MB,
        SET_FAILED.load(Ordering::Relaxed),
        TRIMMED[0].load(Ordering::Relaxed),
        TRIMMED[1].load(Ordering::Relaxed) / MB,
    );
    census::log(parts.used);
    os::start(host);
    os::log(
        parts.used,
        parts.shared_used,
        [
            SET[0][1].load(Ordering::Relaxed),
            SET[2][1].load(Ordering::Relaxed),
            SET[1][1].load(Ordering::Relaxed),
        ],
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parts() -> Parts {
        // The window spot of run I8, in MB: 7,322 in use of 7,028.
        Parts {
            budget: 7028 * MB,
            used: 7322 * MB,
            shared_used: 8341 * MB,
            tile_heaps: 1472 * MB,
            tiles_given: 878 * MB,
            blocks: 3072 * MB,
            blocks_free: 60 * MB,
            card_copies: 630 * MB,
            pc_known: 4000 * MB,
        }
    }

    #[test]
    fn what_is_idle_is_not_what_the_system_must_move_from_the_readers() {
        let r = assess(&parts(), 256 * MB);
        assert_eq!(r.over, 294 * MB);
        // 594 MB of heaps with no tile and 60 MB of free blocks.
        assert_eq!(r.idle, 654 * MB);
        assert_eq!(
            r.hot_over, 0,
            "the overshoot is covered by memory nothing reads"
        );
        // 7,322 less heaps, blocks and copies: 2,148 MB no counter names.
        assert_eq!(r.unattributed, 2148 * MB);
        // To stand 256 MB under the budget: 7,322 - 6,772 = 550, less the idle 654.
        assert_eq!(r.to_fit, 0);
    }

    #[test]
    fn past_what_is_idle_the_rest_is_read() {
        let mut p = parts();
        p.tiles_given = 1400 * MB;
        p.blocks_free = 0;
        let r = assess(&p, 256 * MB);
        assert_eq!(r.idle, 72 * MB);
        assert_eq!(r.hot_over, 222 * MB);
        assert_eq!(r.to_fit, 550 * MB - 72 * MB);
    }

    #[test]
    fn under_the_budget_nothing_is_over() {
        let mut p = parts();
        p.used = 6000 * MB;
        let r = assess(&p, 256 * MB);
        assert_eq!((r.over, r.hot_over, r.to_fit), (0, 0, 0));
        // Counters that add up to more than the system says give no negative.
        p.used = 1000 * MB;
        assert_eq!(assess(&p, 0).unattributed, 0);
    }

    #[test]
    fn a_move_to_the_pcs_memory_is_the_card_falling_while_the_pc_rises() {
        let at = |a: u64, b: u64| (a * MB, b * MB);
        assert_eq!(
            watch(at(7300, 8300), at(7100, 8400)),
            Some(Moved {
                local: 200 * MB,
                shared: 100 * MB
            })
        );
        // Falling alone is a free; rising alone is an allocation.
        assert_eq!(watch(at(7300, 8300), at(7100, 8300)), None);
        assert_eq!(watch(at(7300, 8300), at(7300, 8500)), None);
        // Small changes are noise.
        assert_eq!(watch(at(7300, 8300), at(7280, 8340)), None);
    }

    #[test]
    fn a_heap_goes_back_only_after_a_submission_begun_after_it_emptied_has_finished() {
        // Emptied at 10,000 ms with 40 submissions begun.
        let since = (10_000, 40);
        assert!(!trim_ready(since, 11_999, 41, 5), "two seconds not passed");
        assert!(
            !trim_ready(since, 12_000, 40, 5),
            "no later submission finished"
        );
        assert!(trim_ready(since, 12_000, 41, 5));
        assert!(
            !trim_ready(since, 20_000, 99, 1),
            "the pool's only heap stays"
        );
    }

    #[test]
    fn heaps_are_counted_by_how_free_they_are() {
        let heaps = [
            (1024, 1024),
            (900, 1024),
            (768, 1024),
            (512, 1024),
            (511, 1024),
            (0, 1024),
        ];
        assert_eq!(occupancy(heaps.into_iter()), [1, 2, 1, 2]);
        assert_eq!(occupancy(std::iter::empty()), [0; 4]);
    }

    #[test]
    fn classes_have_the_systems_values() {
        assert_eq!(Class::High.value(), D3D12_RESIDENCY_PRIORITY_HIGH);
        assert_eq!(Class::Low.value(), D3D12_RESIDENCY_PRIORITY_LOW);
        assert_eq!(Class::Max.value(), D3D12_RESIDENCY_PRIORITY_MAXIMUM);
        let at = [Class::High.at(), Class::Low.at(), Class::Max.at()];
        assert!(
            at.iter().all(|&i| i < SET.len()),
            "every class has a counter"
        );
        assert!(at[0] != at[1] && at[1] != at[2] && at[0] != at[2]);
        // (The system's values are unsigned numbers held in an i32.)
        let value = |c: D3D12_RESIDENCY_PRIORITY| c.0 as u32;
        assert!(value(D3D12_RESIDENCY_PRIORITY_LOW) < value(D3D12_RESIDENCY_PRIORITY_HIGH));
        assert!(value(D3D12_RESIDENCY_PRIORITY_HIGH) < value(D3D12_RESIDENCY_PRIORITY_MAXIMUM));
    }

    #[test]
    fn the_whole_use_may_stand_past_the_budget_by_the_limit_and_no_more() {
        // Budget 6,966 MB, use 7,777 MB (run I18F-1): 811 over; the limit 400 leaves 411 MB short.
        assert_eq!(
            room_under(6966 * MB, 7777 * MB, Some(400 * MB)),
            -(411 * MB as i64)
        );
        // The card of run I18F-3 (the fast process): 7,317 MB, 351 over, 49 MB of room left.
        assert_eq!(
            room_under(6966 * MB, 7317 * MB, Some(400 * MB)),
            49 * MB as i64
        );
        // Under budget plus limit: the room that is left.
        assert_eq!(
            room_under(6966 * MB, 7000 * MB, Some(400 * MB)),
            366 * MB as i64
        );
        // No limit, or no budget to hold it to: none.
        assert_eq!(room_under(6966 * MB, 9000 * MB, None), i64::MAX);
        assert_eq!(room_under(0, 9000 * MB, Some(256 * MB)), i64::MAX);
    }
}
