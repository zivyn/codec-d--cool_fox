//! A probe for the title's CPU reading buffers that only shaders touch
//! (`VICTORY_READ_PROBE=1`, off by default), for the copies that
//! `crate::land::quiet` leaves out of landings (`VICTORY_LAND_QUIET=1`).
//!
//! A few pages of such a buffer, picked all over it, are made
//! `PAGE_READONLY | PAGE_GUARD` where they are read-only already (the write
//! guards of `crate::watch`; a page that is not is left alone, and kernel
//! reads and writes of it are never risked). The first access of any kind
//! to such a page, a read or a write, raises a guard page violation once
//! and takes the guard off; the handler of `crate::watch` passes it to
//! [`fault`], which notes it and lets the access go on. A write that follows
//! faults as a write to a guarded block does, as before.
//!
//! The faults are noted in a ring (nothing is allocated or locked in the
//! handler) and looked at later ([`drain`]): which thread it was. A thread
//! of ours (`victory-*`) or DirectStorage's does not count; any other is the
//! title's (or a driver's), and the buffer lands again from then on, once,
//! whole, so that what it read stale is right from that frame. The first of
//! such faults is logged with the buffer, the instruction and its module.
//!
//! Pages are armed a few at a time as landings come, and one armed in the
//! place of another lets that one go. Our own threads read these pages
//! too (refreshes, merges), so most of the time a page is armed is a short
//! one: the counts say how many arms there were, and how many ended in the
//! title's thread.

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Win32::System::Memory::{PAGE_GUARD, PAGE_READONLY, VirtualProtect};

const PAGE: usize = 4096;
/// Pages armed at one time, by a hash of their number; one that is armed
/// where another is, lets the other go first.
const SLOTS: usize = 4096;
static ARMED: [AtomicUsize; SLOTS] = [const { AtomicUsize::new(0) }; SLOTS];

/// Faults noted by the handler, not yet looked at: page, instruction, thread
/// and whether it was a write (`thread << 1 | write`), and the number of the
/// note (written last).
const RING: usize = 1024;
static RING_AT: [[AtomicUsize; 4]; RING] = [const { [const { AtomicUsize::new(0) }; 4] }; RING];
static HEAD: AtomicUsize = AtomicUsize::new(0);

/// Pages armed, not armable (not read-only, or not there), let go for
/// another; hits by title threads (reads, writes), by ours, by
/// DirectStorage's, by threads gone; notes lost to a full ring.
static STATS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
const ARMED_N: usize = 0;
const UNARMABLE: usize = 1;
const LET_GO: usize = 2;
/// Then, by who touched it: title reads and writes, ours, DirectStorage's.
const GONE: usize = 9;
const LOST: usize = 10;

/// Whether the probe is on.
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_READ_PROBE").is_some_and(|v| v == "1"))
}

fn slot_of(page: usize) -> usize {
    ((page as u64 / PAGE as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 52) as usize % SLOTS
}

/// Arms a few pages of `[start, end)`, now and then (every eighth call).
pub(crate) fn arm(start: usize, end: usize) {
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let n = CALLS.fetch_add(1, Ordering::Relaxed);
    if !n.is_multiple_of(8) {
        return;
    }
    let first = start.next_multiple_of(PAGE) / PAGE;
    let last = end / PAGE;
    if last <= first {
        return;
    }
    for k in 0..2u64 {
        let h = (n.wrapping_mul(2) + k + start as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15) >> 20;
        arm_page((first + h as usize % (last - first)) * PAGE);
    }
}

fn arm_page(page: usize) {
    let slot = &ARMED[slot_of(page)];
    let held = slot.load(Ordering::SeqCst);
    if held == page {
        return;
    }
    if held != 0 {
        // The guard goes first: a fault before the slot is cleared finds it.
        disarm(held);
        if slot
            .compare_exchange(held, 0, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
        {
            STATS[LET_GO].fetch_add(1, Ordering::Relaxed);
        }
    }
    // Asked first: a page that is not read-only is not touched, not even
    // for the moment it takes to put it back.
    if protection(page) != PAGE_READONLY {
        STATS[UNARMABLE].fetch_add(1, Ordering::Relaxed);
        return;
    }
    // The slot first: a fault right after the protection finds it.
    if slot
        .compare_exchange(0, page, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return;
    }
    let mut old = 0;
    // SAFETY: a page of title memory; only its protection changes, and only
    // if it was read-only.
    let done =
        unsafe { VirtualProtect(page as *const _, PAGE, PAGE_READONLY | PAGE_GUARD, &mut old) }
            != 0;
    if done && old == PAGE_READONLY {
        STATS[ARMED_N].fetch_add(1, Ordering::Relaxed);
        return;
    }
    if done && old == PAGE_READONLY | PAGE_GUARD {
        // Armed already (the slot holds it).
        return;
    }
    if done {
        // Not ours to guard: as it was.
        let mut guarded = 0;
        // SAFETY: as above.
        unsafe { VirtualProtect(page as *const _, PAGE, old, &mut guarded) };
    }
    STATS[UNARMABLE].fetch_add(1, Ordering::Relaxed);
    let _ = slot.compare_exchange(page, 0, Ordering::SeqCst, Ordering::SeqCst);
}

/// The protection of the page at `at` now.
fn protection(at: usize) -> u32 {
    use windows_sys::Win32::System::Memory::{MEMORY_BASIC_INFORMATION, VirtualQuery};
    // SAFETY: plain data filled in by the query.
    let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
    // SAFETY: a valid out pointer of the right size.
    let n = unsafe { VirtualQuery(at as *const _, &mut info, size_of_val(&info)) };
    if n == 0 { 0 } else { info.Protect }
}

/// Takes the guard off a page armed, if it is still on.
fn disarm(page: usize) {
    if protection(page) & PAGE_GUARD == 0 {
        return;
    }
    let mut old = 0;
    // SAFETY: as in `arm_page`.
    if unsafe { VirtualProtect(page as *const _, PAGE, PAGE_READONLY, &mut old) } != 0
        && old & PAGE_GUARD == 0
    {
        // Another protection (a guard of the writes, opened since): as it was.
        // SAFETY: as above.
        unsafe { VirtualProtect(page as *const _, PAGE, old, &mut old) };
    }
}

/// For the handler of `crate::watch`, on a guard page violation: an access
/// of the instruction at `rip` to `at`. Whether the page was armed here (the
/// access goes on); else it is not ours.
pub(crate) fn fault(at: usize, rip: usize, write: bool) -> bool {
    let page = at & !(PAGE - 1);
    let slot = &ARMED[slot_of(page)];
    if slot
        .compare_exchange(page, 0, Ordering::SeqCst, Ordering::SeqCst)
        .is_err()
    {
        return false;
    }
    let i = HEAD.fetch_add(1, Ordering::SeqCst);
    let at = &RING_AT[i % RING];
    at[0].store(page, Ordering::Relaxed);
    at[1].store(rip, Ordering::Relaxed);
    // SAFETY: a plain read of the thread block.
    let tid = unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() } as usize;
    at[2].store(tid << 1 | usize::from(write), Ordering::Relaxed);
    at[3].store(i + 1, Ordering::SeqCst);
    true
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Kind {
    Title,
    Ours,
    DirectStorage,
    Gone,
}

/// What `drain` has seen.
struct Seen {
    next: usize,
    kinds: HashMap<usize, Kind>,
    /// The pages the title's threads touched, in order, with the
    /// instruction.
    titles: Vec<(usize, usize)>,
}

fn seen() -> &'static Mutex<Seen> {
    static SEEN: OnceLock<Mutex<Seen>> = OnceLock::new();
    SEEN.get_or_init(|| {
        Mutex::new(Seen {
            next: 0,
            kinds: HashMap::new(),
            titles: Vec::new(),
        })
    })
}

fn kind_of(tid: usize) -> Kind {
    use windows_sys::Win32::Foundation::{CloseHandle, LocalFree};
    use windows_sys::Win32::System::Threading::{
        GetThreadDescription, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
    };
    // SAFETY: a handle opened here and closed; the name is the system's,
    // freed as asked.
    unsafe {
        let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid as u32);
        if thread.is_null() {
            return Kind::Gone;
        }
        let mut name = std::ptr::null_mut();
        let mut text = String::new();
        if GetThreadDescription(thread, &mut name) >= 0 && !name.is_null() {
            let len = (0..).take_while(|&i| *name.add(i) != 0).count();
            text = String::from_utf16_lossy(std::slice::from_raw_parts(name, len));
            LocalFree(name.cast());
        }
        CloseHandle(thread);
        if text.starts_with("victory") {
            Kind::Ours
        } else if text.starts_with("dstorage") {
            Kind::DirectStorage
        } else {
            Kind::Title
        }
    }
}

/// The module and offset an address is in, for the log.
fn module_of(at: usize) -> String {
    use windows_sys::Win32::System::LibraryLoader::{
        GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
        GetModuleFileNameW, GetModuleHandleExW,
    };
    let mut module = std::ptr::null_mut();
    // SAFETY: the system fills in a handle that is not counted; the name
    // goes into room of our own.
    unsafe {
        let flags =
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT;
        if GetModuleHandleExW(flags, at as *const u16, &mut module) == 0 {
            return format!("{at:#x}");
        }
        let mut name = [0u16; 260];
        let n = GetModuleFileNameW(module, name.as_mut_ptr(), name.len() as u32) as usize;
        let name = String::from_utf16_lossy(&name[..n.min(260)]);
        let name = name.rsplit(['\\', '/']).next().unwrap_or("");
        format!("{name}+{:#x}", at - module as usize)
    }
}

/// Looks at the faults noted since the last time: counted by who, those of
/// the title's threads kept and logged (the first few). Cheap when there are
/// none.
pub(crate) fn drain() {
    let head = HEAD.load(Ordering::SeqCst);
    let Ok(mut seen) = seen().try_lock() else {
        return;
    };
    if seen.next == head {
        return;
    }
    let from = head.saturating_sub(RING).max(seen.next);
    STATS[LOST].fetch_add((from - seen.next) as u64, Ordering::Relaxed);
    let mut i = from;
    while i < head {
        let at = &RING_AT[i % RING];
        if at[3].load(Ordering::SeqCst) != i + 1 {
            // Being written: the next time.
            break;
        }
        let (page, rip, who) = (
            at[0].load(Ordering::Relaxed),
            at[1].load(Ordering::Relaxed),
            at[2].load(Ordering::Relaxed),
        );
        let (tid, write) = (who >> 1, who & 1 != 0);
        let kind = *seen.kinds.entry(tid).or_insert_with(|| kind_of(tid));
        let w = usize::from(write);
        let n = match kind {
            Kind::Title => 3 + w,
            Kind::Ours => 5 + w,
            Kind::DirectStorage => 7 + w,
            Kind::Gone => GONE,
        };
        STATS[n].fetch_add(1, Ordering::Relaxed);
        if kind == Kind::Title && seen.titles.len() < 64 {
            seen.titles.push((page, rip));
            if seen.titles.len() <= 16 {
                tracing::warn!(
                    page = format_args!("{page:#x}"),
                    write,
                    thread = tid,
                    instruction = %module_of(rip),
                    "the title's own thread touched a page of a buffer only shaders write (VICTORY_READ_PROBE)"
                );
            }
        }
        i += 1;
    }
    seen.next = i;
}

/// The first page of `[start, end)` that a thread of the title touched
/// while it was armed, if any (as of the last [`drain`]).
pub(crate) fn touched_in(start: usize, end: usize) -> Option<usize> {
    let seen = seen().lock().unwrap_or_else(|e| e.into_inner());
    seen.titles
        .iter()
        .map(|t| t.0)
        .find(|&page| page >= start && page < end)
}

/// For the frame-rate line, since the start.
pub(crate) fn log() {
    if !on() {
        return;
    }
    drain();
    let s: Vec<u64> = STATS.iter().map(|s| s.load(Ordering::Relaxed)).collect();
    tracing::info!(
        "read probe (VICTORY_READ_PROBE=1, since the start): {} pages armed, {} not armable, {} let go for others; touched by the title's threads {} (reads) + {} \
         (writes), by ours {} + {}, by DirectStorage's {} + {}; {} by threads gone, {} notes lost",
        s[ARMED_N],
        s[UNARMABLE],
        s[LET_GO],
        s[3],
        s[4],
        s[5],
        s[6],
        s[7],
        s[8],
        s[GONE],
        s[LOST],
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree,
    };

    /// The tests share the probe's table and ring.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    /// A page of read-only memory is armed, noted at its first touch
    /// (given to `fault` as the handler does) and let go; a page that is not
    /// read-only is left as it was, and a fault on a page not armed is not
    /// ours.
    #[test]
    fn a_read_only_page_is_armed_and_its_touch_noted_once() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: memory of our own, released at the end.
        unsafe {
            let base = VirtualAlloc(
                std::ptr::null(),
                4 * PAGE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            ) as usize;
            assert_ne!(base, 0);
            let mut old = 0;
            // Not read-only: as it was, and nothing armed there.
            arm_page(base);
            assert_eq!(protection(base), PAGE_READWRITE);
            VirtualProtect((base + PAGE) as *const _, PAGE, PAGE_READONLY, &mut old);
            arm_page(base + PAGE);
            assert_eq!(protection(base + PAGE), PAGE_READONLY | PAGE_GUARD);
            assert!(
                !fault(base + 8, 0x1234, false),
                "a page not armed is not ours"
            );
            // The touch: noted once.
            assert!(fault(base + PAGE + 100, 0x5678, true));
            assert!(!fault(base + PAGE + 100, 0x5678, true), "only once");
            // Another page in the place of the first lets it go, as it was.
            VirtualProtect((base + 2 * PAGE) as *const _, PAGE, PAGE_READONLY, &mut old);
            arm_page(base + 2 * PAGE);
            assert_eq!(protection(base + 2 * PAGE), PAGE_READONLY | PAGE_GUARD);
            disarm(base + 2 * PAGE);
            assert_eq!(protection(base + 2 * PAGE), PAGE_READONLY);
            assert!(
                fault(base + 2 * PAGE, 1, false),
                "the slot still holds it until its fault or its replacement"
            );
            VirtualFree(base as *mut _, 0, MEM_RELEASE);
        }
    }

    /// The first access of a thread to an armed page goes through the
    /// handler of the process (as `crate::watch` installs it) and is
    /// counted once.
    #[test]
    fn the_handler_passes_an_armed_pages_touch_on() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        crate::watch::install();
        // SAFETY: memory of our own, released at the end.
        unsafe {
            let base = VirtualAlloc(
                std::ptr::null(),
                PAGE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            ) as usize;
            assert_ne!(base, 0);
            (base as *mut u8).write(7);
            let mut old = 0;
            VirtualProtect(base as *const _, PAGE, PAGE_READONLY, &mut old);
            arm_page(base);
            assert_eq!(protection(base), PAGE_READONLY | PAGE_GUARD);
            let before = HEAD.load(Ordering::SeqCst);
            assert_eq!((base as *const u8).read_volatile(), 7, "the read goes on");
            assert_eq!(protection(base), PAGE_READONLY, "the guard is gone");
            assert_eq!(HEAD.load(Ordering::SeqCst), before + 1);
            assert_eq!((base as *const u8).read_volatile(), 7);
            assert_eq!(HEAD.load(Ordering::SeqCst), before + 1, "once");
            VirtualFree(base as *mut _, 0, MEM_RELEASE);
        }
    }
}
