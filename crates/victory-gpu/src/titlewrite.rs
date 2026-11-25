//! The title's writes into guarded view memory that do not go through a
//! fault that opens the block (`crate::watch`): its bytes are put in through
//! our own view of the same pages (`crate::inplace`), which no guard covers,
//! and the block is told as written (`crate::watch::put_beside`). The block
//! stays read-only, so neither this write nor the block's next taking changes
//! a protection.
//!
//! A change of protection is some 5 us in the game's process, one at a time
//! whoever asks; a frame had 590 write faults, and made as many blocks
//! read-only again (run faults-1). What faults is, by the time spent in it,
//! for a third the title's `memcpy`, and for most of the rest single stores
//! into objects that lie in memory some buffer's copy covers: a flag, a
//! reference count, the link of a free list.
//!
//! - [`import`]: the title's `memcpy`, `memmove` and `memset` are ours
//!   (`VICTORY_COPY_BESIDE=1`; off unless asked for). They look at the bits
//!   of the blocks they write and go to the runtime's own, but for the parts
//!   that lie in guarded blocks.
//! - [`store`]: a store that faults is done here, and the thread goes on
//!   behind it (`VICTORY_STORE_BESIDE=1`; off unless asked for). Nothing of
//!   the title's code is known ahead: the instruction is read where it
//!   faulted, and one that is not understood opens the block as before. So
//!   does a block's fifth store since it was taken, and one at a place in the
//!   code most of whose blocks come to that ([`SITES`]): code that fills a
//!   block would fault at every store.

mod decode;

use crate::watch::Beside;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering::Relaxed};

/// Diagnostics, since the start: copies of the title's that put bytes beside
/// the guards, blocks they did so in, blocks they opened instead; stores done
/// beside the guards, faulting stores not understood, and blocks opened
/// because too many came.
pub(crate) static DONE: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

fn add(what: usize) {
    DONE[what].fetch_add(1, Relaxed);
}

/// A switch that is off unless its variable is `1` (or `VICTORY_AB` names it:
/// on and off while the title runs).
macro_rules! asked_for {
    ($name:literal) => {{
        static ASKED: std::sync::OnceLock<Result<bool, u32>> = std::sync::OnceLock::new();
        let asked = ASKED.get_or_init(|| {
            #[cfg(feature = "diagnostics")]
            if let Some(bit) = crate::ab::bit($name) {
                return Err(bit);
            }
            Ok(std::env::var_os($name).is_some_and(|v| v == "1"))
        });
        match *asked {
            Ok(on) => on,
            #[cfg(feature = "diagnostics")]
            Err(bit) => crate::ab::on(bit),
            #[cfg(not(feature = "diagnostics"))]
            Err(_) => false,
        }
    }};
}

fn copies_on() -> bool {
    #[cfg(test)]
    if ON_IN_TEST.get() {
        return true;
    }
    asked_for!("VICTORY_COPY_BESIDE")
}

fn stores_on() -> bool {
    #[cfg(test)]
    if ON_IN_TEST.get() {
        return true;
    }
    asked_for!("VICTORY_STORE_BESIDE")
}

#[cfg(test)]
thread_local! {
    /// Both, for this thread's writes.
    pub(crate) static ON_IN_TEST: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// The runtime's own `memcpy`, `memmove` and `memset`.
static REAL: [AtomicUsize; 3] = [const { AtomicUsize::new(0) }; 3];

type Copy = unsafe extern "system-unwind" fn(*mut u8, *const u8, usize) -> *mut u8;
type Set = unsafe extern "system-unwind" fn(*mut u8, i32, usize) -> *mut u8;

/// What the title's import of `name`, which the host has at `address`, is
/// bound to.
pub fn import(name: &str, address: usize) -> usize {
    let (which, ours) = match name {
        "memcpy" => (0, copy as *const () as usize),
        "memmove" => (1, shift as *const () as usize),
        "memset" => (2, set as *const () as usize),
        _ => return address,
    };
    #[cfg(feature = "diagnostics")]
    let on = crate::ab::bit("VICTORY_COPY_BESIDE").is_some() || copies_on();
    #[cfg(not(feature = "diagnostics"))]
    let on = copies_on();
    if !on {
        return address;
    }
    REAL[which].store(address, Relaxed);
    ours
}

#[cfg(test)]
pub(crate) fn copy_for_test(real: usize) -> usize {
    REAL[0].store(real, Relaxed);
    copy as *const () as usize
}

/// The most a block takes of copies beside its guard before it is opened; a
/// store counts as [`A_STORE`].
pub(crate) const MOST: u32 = 32;
const A_STORE: u32 = 8;

unsafe extern "system-unwind" fn copy(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: the title's copy, as it asked for it, by the function it asked.
    unsafe { copied(to, from, n, REAL[0].load(Relaxed)) }
}

unsafe extern "system-unwind" fn shift(to: *mut u8, from: *const u8, n: usize) -> *mut u8 {
    // SAFETY: as above.
    unsafe { copied(to, from, n, REAL[1].load(Relaxed)) }
}

/// # Safety
/// `real` copies as `memcpy` (or `memmove`) does, `to`, `from` and `n` are
/// the title's.
#[inline]
unsafe fn copied(to: *mut u8, from: *const u8, n: usize, real: usize) -> *mut u8 {
    // SAFETY: the host's function, noted by `import` before the title ran.
    let real: Copy = unsafe { std::mem::transmute(real) };
    if n == 0 || !crate::watch::guards_any(to as usize, n) || !copies_on() || overlap(to as usize, from as usize, n) {
        // SAFETY: by the caller.
        return unsafe { real(to, from, n) };
    }
    in_pieces(to as usize, n, |at, offset, len| {
        // SAFETY: a part of what the title asked for; or the same bytes at
        // our view of the pages.
        unsafe { real(at as *mut u8, from.add(offset), len) };
    });
    to
}

unsafe extern "system-unwind" fn set(to: *mut u8, byte: i32, n: usize) -> *mut u8 {
    // SAFETY: as in `copied`.
    let real: Set = unsafe { std::mem::transmute(REAL[2].load(Relaxed)) };
    if n == 0 || !crate::watch::guards_any(to as usize, n) || !copies_on() {
        // SAFETY: the title's, as it asked for it.
        return unsafe { real(to, byte, n) };
    }
    in_pieces(to as usize, n, |at, _, len| {
        // SAFETY: as in `copied`.
        unsafe { real(at as *mut u8, byte, len) };
    });
    to
}

/// Whether the two ranges meet, if only in a block (of which only one might
/// be written at another address).
fn overlap(to: usize, from: usize, n: usize) -> bool {
    const BLOCK: usize = crate::watch::PAGE;
    let (first, last) = (to / BLOCK * BLOCK, to.saturating_add(n).next_multiple_of(BLOCK));
    from < last && first < from.saturating_add(n)
}

/// Does `[to, to + n)` block by block: `put(at, offset, len)` with the
/// address to write at, the title's or ours.
fn in_pieces(to: usize, n: usize, put: impl Fn(usize, usize, usize)) {
    const BLOCK: usize = crate::watch::PAGE;
    add(0);
    let mut offset = 0;
    while offset < n {
        let at = to + offset;
        let len = (BLOCK - at % BLOCK).min(n - offset);
        if crate::watch::guards_any(at, len) {
            match crate::watch::put_beside(at / BLOCK, 1, |delta| put(at.wrapping_add(delta), offset, len)) {
                Beside::Done => {
                    add(1);
                    offset += len;
                    continue;
                }
                // Opened with no fault: the next copies here are plain ones.
                _ => crate::watch::opened_for_writing(at),
            }
            add(2);
        }
        put(at, offset, len);
        offset += len;
    }
}

/// Places of the title's code whose stores faulted, by a hash of the address:
/// the stores done beside the guards (the low half) and the blocks that had
/// too many of them (the high half).
static SITES: [AtomicU64; 1 << 10] = [const { AtomicU64::new(0) }; 1 << 10];

fn site(rip: usize) -> &'static AtomicU64 {
    &SITES[(rip as u64).wrapping_mul(0x9e37_79b9_7f4a_7c15) as usize >> 54 & 1023]
}

/// Whether a site's stores come one to a block, as far as is known.
fn sparse(counts: u64) -> bool {
    let (done, full) = (counts & 0xffff_ffff, counts >> 32);
    full < 4 || full * 12 < done
}

/// Does the store that faulted at `at`, a byte of a guarded block, beside the
/// guards; `false` if it is to open the block (and nothing was done).
pub(crate) fn store(context: &mut windows_sys::Win32::System::Diagnostics::Debug::CONTEXT, at: usize) -> bool {
    if !stores_on() {
        return false;
    }
    let rip = context.Rip as usize;
    let site = site(rip);
    let counts = site.load(Relaxed);
    if !sparse(counts) {
        // Looked at anew some time.
        if site.fetch_add(1 << 32, Relaxed) >> 32 > 4096 {
            site.store(0, Relaxed);
        }
        return false;
    }
    let mut code = [0u8; 15];
    // (To the end of the page if the next is not there.)
    let left = 4096 - rip % 4096;
    let there = crate::host::read_into(rip as *const u8, &mut code) || {
        let n = left.min(15);
        n < 15 && crate::host::read_into(rip as *const u8, &mut code[..n])
    };
    let Some(store) = there.then(|| decode::store(&code, context)).flatten() else {
        add(4);
        return false;
    };
    const BLOCK: usize = crate::watch::PAGE;
    let end = store.at.wrapping_add(store.size);
    if !(store.at..end).contains(&at) || store.at / BLOCK != (end - 1) / BLOCK {
        add(4);
        return false;
    }
    let mut done = false;
    let put = crate::watch::put_beside(store.at / BLOCK, A_STORE, |delta| {
        // SAFETY: the bytes the title is storing to, at our view of them.
        done = unsafe { store.run(context, store.at.wrapping_add(delta)) };
    });
    if put != Beside::Done {
        if put == Beside::Full {
            site.fetch_add(1 << 32, Relaxed);
            add(5);
        }
        return false;
    }
    if !done {
        // (A locked one that is not aligned: told as written for nothing.)
        add(4);
        return false;
    }
    context.Rip += store.len as u64;
    site.fetch_add(1, Relaxed);
    add(3);
    #[cfg(feature = "diagnostics")]
    crate::faultbook::note(crate::faultbook::What::Stored, rip, at / BLOCK, 0);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sites_that_fill_blocks_are_told_from_others() {
        assert!(sparse(0) && sparse(1000) && sparse(3 << 32));
        assert!(!sparse(4 << 32 | 16) && sparse(4 << 32 | 60));
    }

    #[test]
    fn copies_that_meet_are_left_to_the_runtime() {
        const B: usize = crate::watch::PAGE;
        assert!(overlap(10 * B + 5, 10 * B + 900, 8), "in one block");
        assert!(!overlap(10 * B + 5, 12 * B, 8) && !overlap(10 * B, 9 * B, B));
        assert!(overlap(10 * B, 9 * B, B + 1));
    }
}
