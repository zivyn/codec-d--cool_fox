//! What the title does with its shader-visible view heap, counted: the
//! highest slot it writes a view record to, which 4096-slot blocks of the heap
//! it touches at all (the console's heap has 1,245,184 slots, the host's
//! 1,000,000 at most on NVIDIA, tier 3 too), how many of those lie past the
//! slots the host mirrors, and how the draws read tables there.
//!
//! The numbers are cheap (a few relaxed atomics per record) and always kept;
//! [`Usage::line`] says them with the frame line, when they changed.

use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};

/// Slots in a block: the unit "distinct pages of the heap" counts in (the
/// title lays its per-frame table ring out in blocks of this size).
pub(crate) const BLOCK: u32 = 4096;

/// Words of the bitmaps: blocks of every slot the strides table covers
/// (2^21 slots).
const WORDS: usize = (1 << 21) / BLOCK as usize / 64;

/// Tables read in place a thread counts before it adds them
/// ([`Usage::table`]).
const BATCH: u32 = 1024;

thread_local! {
    static IN_PLACE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// What was seen of one view heap of the title.
pub(crate) struct Usage {
    /// One more than the highest slot a view record was written to (0: none).
    top: AtomicU32,
    /// The blocks a view record was written to.
    blocks: [AtomicU64; WORDS],
    /// View records written to slots the host mirrors, and past them, by
    /// calls and copies of the API (`Create*View`, `CopyDescriptors*`).
    api: [AtomicU64; 2],
    /// View records the title copied in as bytes (found by the heap sync)
    /// past the host's slots.
    bytes_past: AtomicU64,
    /// Tables a draw reads: read in place (every entry below the host's
    /// slots), made afresh from the title's records in dynamic slots because
    /// they reach past them.
    tables: [AtomicU64; 2],
    /// One more than the highest slot of a table's last entry a draw reads.
    table_top: AtomicU32,
    /// What [`Usage::line`] said last (a hash of the counters).
    said: AtomicU64,
}

impl Usage {
    pub(crate) const fn new() -> Self {
        Self {
            top: AtomicU32::new(0),
            blocks: [const { AtomicU64::new(0) }; WORDS],
            api: [const { AtomicU64::new(0) }; 2],
            bytes_past: AtomicU64::new(0),
            tables: [const { AtomicU64::new(0) }; 2],
            table_top: AtomicU32::new(0),
            said: AtomicU64::new(0),
        }
    }

    /// A view record written at `index` by a call of the API; `host`: how
    /// many slots the host mirrors.
    pub(crate) fn written(&self, index: u32, host: u32) {
        self.api[usize::from(index >= host)].fetch_add(1, Relaxed);
        self.block(index);
    }

    /// A view record the title copied in as bytes at `index`, past the
    /// host's slots (those below are written by the sync and counted when
    /// the host makes them).
    pub(crate) fn found_past(&self, index: u32) {
        self.bytes_past.fetch_add(1, Relaxed);
        self.block(index);
    }

    /// A view record found at `index` below the host's slots.
    pub(crate) fn found(&self, index: u32) {
        self.block(index);
    }

    fn block(&self, index: u32) {
        self.top.fetch_max(index.saturating_add(1), Relaxed);
        let block = (index / BLOCK) as usize;
        if let Some(word) = self.blocks.get(block / 64) {
            word.fetch_or(1 << (block % 64), Relaxed);
        }
    }

    /// A draw reads a table whose last entry is slot `last`, `snapshot`: made
    /// afresh in dynamic slots (it reaches past the host's), else read in
    /// place.
    ///
    /// Called for every table of every draw from several threads: tables read
    /// in place are counted a thread at a time and added in batches of
    /// [`BATCH`] (up to `BATCH - 1` of each thread are not in the number yet),
    /// and the highest slot is only written when it climbs.
    pub(crate) fn table(&self, last: u32, snapshot: bool) {
        if snapshot {
            self.tables[1].fetch_add(1, Relaxed);
        } else {
            let n = IN_PLACE.with(|n| {
                n.set(n.get() + 1);
                n.get()
            });
            if n >= BATCH {
                IN_PLACE.with(|n| n.set(0));
                self.tables[0].fetch_add(u64::from(n), Relaxed);
            }
        }
        let top = last.saturating_add(1);
        if top > self.table_top.load(Relaxed) {
            self.table_top.fetch_max(top, Relaxed);
        }
    }

    /// The numbers: highest slot written, blocks written, blocks written
    /// from `host` slots on (past the host's), records by the API below and
    /// past `host`, records copied as bytes past it, tables read in place and
    /// made afresh, the highest table slot.
    pub(crate) fn numbers(&self, host: u32) -> Numbers {
        let first = (host / BLOCK) as usize;
        let (mut all, mut past) = (0u32, 0u32);
        for (w, word) in self.blocks.iter().enumerate() {
            let bits = word.load(Relaxed);
            all += bits.count_ones();
            // Blocks from the one `host` lies in on (that one holds the
            // host's last slot and the first past it: counted past).
            let mask = match first.saturating_sub(w * 64) {
                0 => u64::MAX,
                n if n >= 64 => 0,
                n => u64::MAX << n,
            };
            past += (bits & mask).count_ones();
        }
        Numbers {
            top: self.top.load(Relaxed).saturating_sub(1),
            seen: self.top.load(Relaxed) != 0,
            blocks: all,
            blocks_past: past,
            api: [self.api[0].load(Relaxed), self.api[1].load(Relaxed)],
            bytes_past: self.bytes_past.load(Relaxed),
            tables: [self.tables[0].load(Relaxed), self.tables[1].load(Relaxed)],
            table_top: self.table_top.load(Relaxed).saturating_sub(1),
        }
    }

    /// Says [`Numbers`] (title heap of `title` slots, `host` of them
    /// mirrored, `pinned` past ones pinned) if they changed since the last
    /// time, once the title wrote any.
    pub(crate) fn line(&self, title: u32, host: u32, pinned: usize) {
        let n = self.numbers(host);
        if !n.seen {
            return;
        }
        let mut hash = 0xcbf2_9ce4_8422_2325u64;
        for v in [
            u64::from(n.top),
            u64::from(n.blocks),
            u64::from(n.blocks_past),
            n.api[0],
            n.api[1],
            n.bytes_past,
            n.tables[0],
            n.tables[1],
            u64::from(n.table_top),
            pinned as u64,
        ] {
            hash = (hash ^ v).wrapping_mul(0x100_0000_01b3);
        }
        if self.said.swap(hash, Relaxed) == hash {
            return;
        }
        tracing::info!(
            title,
            host,
            highest = n.top,
            blocks = n.blocks,
            blocks_past = n.blocks_past,
            of_blocks = title.div_ceil(BLOCK),
            pinned,
            "title view heap use: view records were written up to slot {} ({} of {} blocks of {BLOCK} slots touched, {} of them from the host's {host} slots on); records by calls: {} below the host's slots, {} past them (pinned: {pinned}); \
             {} copied in as bytes past them; draws read {} tables in place and {} made afresh in dynamic slots (the highest table entry: slot {})",
            n.top,
            n.blocks,
            title.div_ceil(BLOCK),
            n.blocks_past,
            n.api[0],
            n.api[1],
            n.bytes_past,
            n.tables[0],
            n.tables[1],
            n.table_top,
        );
    }
}

/// [`Usage::numbers`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Numbers {
    pub top: u32,
    pub seen: bool,
    pub blocks: u32,
    pub blocks_past: u32,
    pub api: [u64; 2],
    pub bytes_past: u64,
    pub tables: [u64; 2],
    pub table_top: u32,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The highest slot, the blocks touched and those from the host's slots
    /// on are counted whichever way a record came, and each block once.
    #[test]
    fn blocks_and_the_highest_slot_are_counted() {
        let u = Usage::new();
        assert!(!u.numbers(745_023).seen);
        u.written(10, 745_023);
        u.written(11, 745_023);
        u.written(BLOCK - 1, 745_023);
        u.found(3 * BLOCK);
        let n = u.numbers(745_023);
        assert!(n.seen);
        assert_eq!(
            (n.top, n.blocks, n.blocks_past, n.api),
            (3 * BLOCK, 2, 0, [3, 0])
        );
        // Past the host's slots: by a call, and as bytes.
        u.written(1_100_000, 745_023);
        u.found_past(1_100_001);
        u.found_past(1_245_183);
        let n = u.numbers(745_023);
        assert_eq!(n.top, 1_245_183);
        assert_eq!(n.api, [3, 1]);
        assert_eq!(n.bytes_past, 2);
        assert_eq!(n.blocks, 4);
        assert_eq!(
            n.blocks_past, 2,
            "blocks 268 and 303 lie past block 181, where the host's slots end"
        );
        assert_eq!(1_100_000 / BLOCK, 268);
        assert_eq!(1_245_183 / BLOCK, 303);
    }

    /// Blocks are counted from the block the host's slots end in: a block
    /// below it is not past, whatever word of the bitmap it is in.
    #[test]
    fn past_blocks_are_counted_across_words_of_the_bitmap() {
        let u = Usage::new();
        for block in [0u32, 63, 64, 65, 127, 128, 300] {
            u.found(block * BLOCK + 5);
        }
        assert_eq!(u.numbers(0).blocks_past, 7);
        assert_eq!(u.numbers(64 * BLOCK).blocks_past, 5, "from block 64 on");
        assert_eq!(u.numbers(64 * BLOCK + 1).blocks_past, 5);
        assert_eq!(u.numbers(65 * BLOCK).blocks_past, 4);
        assert_eq!(u.numbers(128 * BLOCK).blocks_past, 2);
        assert_eq!(u.numbers(301 * BLOCK).blocks_past, 0);
        assert_eq!(u.numbers(10_000_000).blocks_past, 0);
        // Slots past what the bitmap covers are still the highest seen.
        u.found(3_000_000);
        assert_eq!(u.numbers(0).top, 3_000_000);
        assert_eq!(u.numbers(0).blocks, 7);
    }

    /// Tables are counted by how they are read, and the highest entry kept.
    #[test]
    fn tables_are_counted_by_how_they_are_read() {
        let u = Usage::new();
        u.table(1_200_000, true);
        u.table(5, true);
        for _ in 0..BATCH + 3 {
            u.table(100, false);
        }
        let n = u.numbers(745_023);
        // (A batch of this thread's counts that another test left may fall
        // in too, so the number is one batch or two.)
        assert!(
            (u64::from(BATCH)..=2 * u64::from(BATCH)).contains(&n.tables[0]),
            "{n:?}"
        );
        assert_eq!((n.tables[1], n.table_top), (2, 1_200_000));
    }
}
