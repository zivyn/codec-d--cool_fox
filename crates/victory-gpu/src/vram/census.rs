//! What holds the card's memory, by who asked for it (VRAM2).
//!
//! The vram line names three counters (tile heaps, blocks of textures, card
//! copies of title buffers) and says "not named by any counter of ours" of
//! the rest: 2.1 to 2.6 GB at the window spot. This line names more of it:
//!
//! - the blocks of textures by how full they are (a block holds no piece
//!   while anything lives in it: 1,813 MB of 3,328 were free at the spot of
//!   run ARCH2R-a, in 52 blocks), the free ranges and the longest, and what
//!   the compaction did ([`crate::pool`]);
//! - the host textures made as committed resources, by what they are
//!   (render targets, depth, unordered access with the colour twins of
//!   depth, the rest) and the bytes of the title's filled textures that
//!   lie outside the blocks ([`crate::resource::ALIVE_KINDS`]);
//! - the device-local buffers (`Host::uav_buffer`, `Host::mapped_card_buffer`)
//!   by the line of code that asked for them, alive now and made so far, the
//!   biggest first: card copies of title buffers, the card side of windows,
//!   the scratch of lists, the buffers the stand-ins convert through;
//! - what is left of the system's own count for the process: the driver's
//!   (pipelines, command memory, descriptor heaps), which no call of ours
//!   sizes.
//!
//! Counted from the objects' own lifetimes (an object of ours that a buffer
//! holds says so when the last reference to it goes, as `crate::sites` does
//! for the PC's memory), so that nothing here is kept right by hand.

use std::collections::HashMap;
use std::ffi::c_void;
use std::panic::Location;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Graphics::Direct3D12::{ID3D12Object, ID3D12Resource};
use windows::core::{GUID, HRESULT, IUnknown, Interface};

use super::MB;

/// A buffer in the card's own memory (`Host::uav_buffer`).
pub(crate) const CARD: u8 = 0;
/// A buffer in the card's memory that the CPU maps (`Host::mapped_card_buffer`).
pub(crate) const MAPPED: u8 = 1;

/// What the system gives a committed buffer: whole 64 KiB.
const GRAIN: u64 = 64 << 10;

/// What one line of code made.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Site {
    pub(crate) file: &'static str,
    pub(crate) line: u32,
    pub(crate) kind: u8,
    pub(crate) alive: u64,
    pub(crate) alive_bytes: u64,
    pub(crate) made: u64,
    pub(crate) made_bytes: u64,
}

#[derive(Default)]
struct Book {
    index: HashMap<(&'static str, u32, u8), usize>,
    sites: Vec<Site>,
}

static BOOK: OnceLock<Mutex<Book>> = OnceLock::new();

fn book() -> std::sync::MutexGuard<'static, Book> {
    BOOK.get_or_init(Mutex::default)
        .lock()
        .unwrap_or_else(|e| e.into_inner())
}

/// What the system takes of the card for a committed buffer of `size` bytes.
pub(crate) fn charged(size: u64) -> u64 {
    size.max(1).next_multiple_of(GRAIN)
}

/// The buffer `resource` of `size` bytes was made for `kind` at `at`.
pub(crate) fn made(resource: &ID3D12Resource, kind: u8, size: u64, at: &'static Location<'static>) {
    let bytes = charged(size);
    let slot = {
        let mut book = book();
        let book = &mut *book;
        let slot = *book
            .index
            .entry((at.file(), at.line(), kind))
            .or_insert_with(|| {
                book.sites.push(Site {
                    file: at.file(),
                    line: at.line(),
                    kind,
                    alive: 0,
                    alive_bytes: 0,
                    made: 0,
                    made_bytes: 0,
                });
                book.sites.len() - 1
            });
        let site = &mut book.sites[slot];
        site.alive += 1;
        site.alive_bytes += bytes;
        site.made += 1;
        site.made_bytes += bytes;
        slot
    };
    watch(resource, slot, bytes);
}

/// Every site with something alive, the biggest first.
pub(crate) fn alive() -> Vec<Site> {
    let mut sites: Vec<Site> = book()
        .sites
        .iter()
        .copied()
        .filter(|s| s.alive > 0)
        .collect();
    sites.sort_by_key(|s| std::cmp::Reverse(s.alive_bytes));
    sites
}

/// What the book says is alive: buffers and their bytes, and made so far.
pub(crate) fn totals() -> [u64; 4] {
    let mut t = [0u64; 4];
    for s in &book().sites {
        t[0] += s.alive;
        t[1] += s.alive_bytes;
        t[2] += s.made;
        t[3] += s.made_bytes;
    }
    t
}

/// What the buffer's last release gives back.
#[repr(C)]
struct Notice {
    methods: &'static [*const (); 3],
    references: AtomicU32,
    slot: usize,
    bytes: u64,
}

struct Methods([*const (); 3]);
// SAFETY: function addresses only.
unsafe impl Sync for Methods {}
static NOTICE: Methods = Methods([
    query as *const (),
    add_ref as *const (),
    release as *const (),
]);

unsafe extern "system" fn query(
    this: *mut c_void,
    iid: *const GUID,
    out: *mut *mut c_void,
) -> HRESULT {
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
    HRESULT(0x8000_4002_u32 as i32)
}

unsafe extern "system" fn add_ref(this: *mut c_void) -> u32 {
    // SAFETY: one of ours, alive while a reference to it is.
    unsafe {
        (*this.cast::<Notice>())
            .references
            .fetch_add(1, Ordering::Relaxed)
            + 1
    }
}

unsafe extern "system" fn release(this: *mut c_void) -> u32 {
    // SAFETY: as above; the last reference frees the box it was made as.
    unsafe {
        let left = (*this.cast::<Notice>())
            .references
            .fetch_sub(1, Ordering::AcqRel)
            - 1;
        if left == 0 {
            let gone = Box::from_raw(this.cast::<Notice>());
            if let Some(site) = book().sites.get_mut(gone.slot) {
                site.alive = site.alive.saturating_sub(1);
                site.alive_bytes = site.alive_bytes.saturating_sub(gone.bytes);
            }
        }
        left
    }
}

/// Has the book told when `resource` goes.
fn watch(resource: &ID3D12Resource, slot: usize, bytes: u64) {
    const KEY: GUID = GUID::from_u128(0x3b8d1f42_77a0_4c35_9e6d_a4125c0be9f8);
    let Ok(object) = resource.cast::<ID3D12Object>() else {
        return;
    };
    let notice = Box::into_raw(Box::new(Notice {
        methods: &NOTICE.0,
        references: AtomicU32::new(1),
        slot,
        bytes,
    }));
    // SAFETY: a COM object of ours with one reference, which this drops; the
    // buffer takes its own. (One that is refused is never counted out.)
    unsafe {
        let notice = IUnknown::from_raw(notice.cast());
        if object.SetPrivateDataInterface(&KEY, &notice).is_err() {
            std::mem::forget(notice);
        }
    }
}

/// The part of the system's count that the counters of ours do not name.
pub(crate) struct Named {
    pub(crate) blocks: u64,
    pub(crate) tile_heaps: u64,
    pub(crate) committed_textures: u64,
    pub(crate) buffers: u64,
}

impl Named {
    pub(crate) fn sum(&self) -> u64 {
        self.blocks + self.tile_heaps + self.committed_textures + self.buffers
    }
}

/// `path` as the file's name alone: the line says where, not in whose tree.
fn short(path: &str) -> &str {
    path.rsplit(['/', '\\']).next().unwrap_or(path)
}

/// The census line, and the figures it names (for the tests): once for each
/// memory line.
pub(crate) fn log(used: u64) {
    let pool = crate::pool::blocks_report();
    let held = crate::pool::held();
    let blocks = crate::pool::texture_bytes();
    let (heaps, given) = crate::tiles::bytes();
    let kinds = crate::resource::alive_kinds();
    // Filled textures beyond the pieces the blocks hold: the pool turned some away, and a piece in a block
    // takes less (4 KiB alignment) than the host says a texture takes (64 KiB).
    let filled = crate::resource::KINDS[1].load(Ordering::Relaxed);
    let outside = filled.saturating_sub(held);
    let committed = kinds
        .iter()
        .map(|k| k.1)
        .sum::<u64>()
        .saturating_sub(filled);
    let totals = totals();
    let named = Named {
        blocks,
        tile_heaps: heaps,
        committed_textures: committed + outside,
        buffers: totals[1],
    };
    let rest = used.saturating_sub(named.sum());
    let c = crate::pool::COMPACTED
        .each_ref()
        .map(|c| c.load(Ordering::Relaxed));
    let sites = alive();
    let biggest: Vec<String> = sites
        .iter()
        .take(14)
        .map(|s| {
            format!(
                "{}:{} {} {} alive {} MB ({} made, {} MB)",
                short(s.file),
                s.line,
                if s.kind == MAPPED { "mapped" } else { "card" },
                s.alive,
                s.alive_bytes / MB,
                s.made,
                s.made_bytes / MB
            )
        })
        .collect();
    tracing::info!(
        "vram census (VICTORY_POOL_COMPACT={}, VICTORY_POOL_FIT={}): the system counts {} MB; named {} MB: \
         blocks of textures {} MB ({} MB held, of {} MB of filled textures alive, {} MB free: {} blocks wholly free, \
         {} three quarters, {} half, {} a quarter, {} less; {} free ranges, the longest {} MB; {} being emptied); \
         compaction: {} blocks chosen, {} textures made again ({} MB), {} blocks emptied, {} given up; \
         tile heaps {} MB ({} MB given); \
         committed host textures {} MB (render targets {} of {} MB, depth {} of {} MB, unordered access and colour twins {} of {} MB, \
         the rest {} of {} MB, of which {} MB are filled textures beyond what the blocks hold: made outside them, and the small alignment the blocks save); \
         device-local buffers {} MB in {} buffers ({} made so far, {} MB), by the line that made them: [{}]; \
         not named {} MB (the driver's pipelines, command memory and descriptor heaps, and the system's own)",
        u8::from(crate::pool::compact_on()),
        u8::from(crate::pool::fit_best()),
        used / MB,
        named.sum() / MB,
        blocks / MB,
        held / MB,
        filled / MB,
        pool.free / MB,
        pool.by_free[0],
        pool.by_free[1],
        pool.by_free[2],
        pool.by_free[3],
        pool.by_free[4],
        pool.ranges,
        pool.longest / MB,
        pool.draining,
        c[0],
        c[1],
        c[4] / MB,
        c[2],
        c[3],
        heaps / MB,
        given / MB,
        named.committed_textures / MB,
        kinds[0].0,
        kinds[0].1 / MB,
        kinds[1].0,
        kinds[1].1 / MB,
        kinds[2].0,
        kinds[2].1 / MB,
        kinds[3].0,
        kinds[3].1 / MB,
        outside / MB,
        named.buffers / MB,
        totals[0],
        totals[2],
        totals[3] / MB,
        biggest.join("; "),
        rest / MB,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_committed_buffer_takes_whole_grains() {
        assert_eq!(charged(0), GRAIN);
        assert_eq!(charged(1), GRAIN);
        assert_eq!(charged(256), GRAIN);
        assert_eq!(charged(GRAIN), GRAIN);
        assert_eq!(charged(GRAIN + 1), 2 * GRAIN);
        assert_eq!(charged(4 << 20), 4 << 20);
    }

    #[test]
    fn what_is_named_is_the_sum_of_the_parts() {
        let n = Named {
            blocks: 3328 * MB,
            tile_heaps: 1216 * MB,
            committed_textures: 1318 * MB,
            buffers: 900 * MB,
        };
        assert_eq!(n.sum(), 6762 * MB);
    }

    #[test]
    fn the_file_is_named_without_its_tree() {
        assert_eq!(short("crates/victory-gpu/src/shadow.rs"), "shadow.rs");
        assert_eq!(short("C:\\work\\victory\\host.rs"), "host.rs");
        assert_eq!(short("lib.rs"), "lib.rs");
    }

    /// A buffer counts at its site while it lives and is counted out when the last reference goes.
    #[test]
    fn a_buffer_is_alive_at_its_site_until_its_last_reference_goes() {
        let Some(host) = crate::host::host() else {
            return;
        };
        let Some(buffer) = host.uav_buffer(100 << 10) else {
            return;
        };
        // The site of the call above is this file; find it by its kind and bytes.
        let mine = |s: &Site| short(s.file) == "census.rs" && s.alive_bytes >= charged(100 << 10);
        let at = alive().into_iter().find(mine);
        let at = at.expect("the buffer is in the book");
        assert_eq!(at.kind, CARD);
        assert!(at.alive >= 1 && at.made >= 1);
        let other = buffer.clone();
        drop(buffer);
        assert!(alive().into_iter().any(|s| mine(&s)), "a reference is left");
        drop(other);
        assert!(!alive().into_iter().any(|s| mine(&s)), "the last one went");
    }
}
