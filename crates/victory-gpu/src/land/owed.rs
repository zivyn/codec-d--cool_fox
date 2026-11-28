//! Lazy landing (`VICTORY_LAND_LAZY=1`, off by default): the title is told
//! before everything the card wrote is in title memory.
//!
//! The tail of a frame is the landing of its last flights: the card has run
//! them, the title's next frame waits for the signal behind them (11 ms of
//! the K6 timeline). Most of what such a landing puts into title memory is
//! in buffers that only shaders use, which the title's CPU does not read. So
//! the landing puts in at once what it must (the copies the title was seen to
//! read), gives the rest to the owed set, and the title is told. A piece owed
//! is a range of title pages made `PAGE_GUARD`: the first access to any of its
//! pages, a read or a write, by any thread, raises a guard page violation, and
//! the handler of `crate::watch` ([`fault`]) gives it to the thread that
//! faulted: if nobody has the unit, that thread lands it then (the landing is
//! its own work, which needs no other thread to go on), and goes on; if another
//! thread is landing it, it waits for that one, which holds none of the
//! faulting thread's locks. The relay thread lands the rest when the title has
//! been told ([`run_pending`]), and the next landing begins with what is still
//! owed ([`finish_all`]), so that no later signal is told before it.
//!
//! The reads of our own threads go through `crate::host::read`, which lands
//! what it is about to read ([`land_range`]) before it reads; a thread that
//! holds a lock of a copy (the one the landing takes shared) lands what it will
//! read beforehand. The kernel's accesses to such pages would fail instead of
//! raising an exception: the runtime tells of the kernel's writes beforehand
//! ([`land_range`] in `crate::watch::writing`), and a copy of ours that reads
//! them for the kernel does not exist for buffers of default heaps.
//!
//! One hole is left, and cannot be closed in process on private memory: the
//! guard of a page goes with the first access to it, the lander's own
//! included, and protection is the process's, not the thread's (a second view
//! of the pages, which would let the lander write them with the guard still
//! on, exists only for memory that is a mapped view, which is not armed). A
//! thread that reads that very page between the lander's first access to it
//! and the end of its merge (a 4 KiB page, a microsecond or two) reads the
//! bytes from before the landing. Every other page of the unit is waited
//! for, and the copies the title was seen to read are landed at once.
//!
//! What it takes: the pages are private memory, committed, read-write; the
//! copy is that of a buffer in a default heap; the landing is the tail's.

use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, RwLock};

use windows_sys::Win32::System::Memory::{PAGE_GUARD, PAGE_READWRITE, VirtualProtect};

/// The unit of protection.
pub(crate) const PAGE: usize = 4096;

/// Whether the tail's landing is lazy (`VICTORY_LAND_LAZY=1`).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAND_LAZY").is_some_and(|v| v == "1"))
}

/// Whether what a unit landed is compared with what was fetched
/// (`VICTORY_OWED_CHECK=1`).
pub(crate) fn check_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_OWED_CHECK").is_some_and(|v| v == "1"))
}

const PENDING: u8 = 0;
const LANDING: u8 = 1;
const DONE: u8 = 2;

thread_local! {
    /// How many units this thread is landing: its own accesses to owed pages
    /// are the landing's, and go on.
    static LANDING_DEPTH: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    /// Which units this thread is landing (the innermost last): only its
    /// accesses to the pages of these are the landing's.
    static LANDING_UNITS: std::cell::RefCell<Vec<*const Unit>> = const { std::cell::RefCell::new(Vec::new()) };
}

/// Whether this thread is landing `unit` (further out or not).
fn landing_here(unit: &Unit) -> bool {
    LANDING_UNITS.with_borrow(|units| units.contains(&std::ptr::from_ref(unit)))
}

/// What lands a unit: run once, by whoever has the unit.
pub(crate) type Land = Box<dyn FnOnce() + Send>;

/// A range of pages owed: armed, and landed by one thread.
pub(crate) struct Unit {
    from: usize,
    to: usize,
    state: AtomicU8,
    /// Whether its pages were made `PAGE_GUARD` (or are being): for
    /// [`finished_lately`].
    armed: std::sync::atomic::AtomicBool,
    land: Mutex<Option<Land>>,
    /// Told when a thread other than the lander touches the unit (the copies
    /// of its pieces are then ones the title reads).
    touched: Box<dyn Fn() + Send + Sync>,
}

/// The units not done with, until the next landing begins.
static UNITS: RwLock<Vec<Arc<Unit>>> = RwLock::new(Vec::new());
/// How many are not done.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);
/// The page ranges (from, to) of units that were done with and forgotten by
/// [`finish_all`] less than [`GRACE`] ago, and when. A thread that touched an
/// owed page raised its guard page violation before the unit was done and
/// reaches [`fault`] after it: the guard is spent, and the access only has to
/// go on.
static FINISHED: Mutex<Vec<(usize, usize, std::time::Instant)>> = Mutex::new(Vec::new());
/// How long [`FINISHED`] remembers a unit: the dispatch of an exception takes
/// microseconds, a preempted thread's a few time slices.
const GRACE: std::time::Duration = std::time::Duration::from_millis(250);
static DONE_CV: (Mutex<()>, Condvar) = (Mutex::new(()), Condvar::new());

/// Since the last frame-rate line: units, their bytes, touches that landed one
/// (and the microseconds they took), waits for another thread's landing, units
/// landed after the title was told and the microseconds that took.
pub(crate) static STATS: [std::sync::atomic::AtomicU64; 10] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 10];
pub(crate) const UNITS_N: usize = 0;
pub(crate) const BYTES: usize = 1;
pub(crate) const TOUCHES: usize = 2;
pub(crate) const TOUCH_US: usize = 3;
pub(crate) const WAITS: usize = 4;
pub(crate) const BACKGROUND_N: usize = 5;
pub(crate) const BACKGROUND_US: usize = 6;
pub(crate) const UNARMED: usize = 7;
/// With [`check_on`]: bytes of landed pieces compared with what was fetched, and
/// the dwords that differ (the title's own writes after the landing, and
/// what the landing left out for them, are there too).
pub(crate) const CHECKED: usize = 8;
pub(crate) const DIFFERING: usize = 9;

/// The page ranges (from, to; whole pages) that cover `ranges` (from, to),
/// where those that share a page are one: a page of two units would be unguarded
/// by the landing of one while the other is still owed.
pub(crate) fn groups(ranges: &[(usize, usize)]) -> Vec<(usize, usize)> {
    let mut sorted = ranges.to_vec();
    sorted.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::new();
    for (from, to) in sorted {
        let (a, b) = (from / PAGE * PAGE, to.next_multiple_of(PAGE));
        match out.last_mut() {
            Some(last) if a <= last.1 => last.1 = last.1.max(b),
            _ => out.push((a, b)),
        }
    }
    out
}

/// With [`check_on`]: the title memory `[at, at + len)` that a piece landed
/// is compared with the bytes it was fetched as (`now`), by dwords.
pub(crate) fn verify(at: usize, now: *const u8, len: usize) {
    let mut title = vec![0u8; len];
    if !crate::host::read_into(at as *const u8, &mut title) {
        return;
    }
    // SAFETY: the fetched bytes, `len` of them, which stay until the unit is done.
    let fetched = unsafe { std::slice::from_raw_parts(now, len) };
    let differing = title
        .chunks_exact(4)
        .zip(fetched.chunks_exact(4))
        .filter(|(a, b)| a != b)
        .count();
    STATS[CHECKED].fetch_add(len as u64, Ordering::Relaxed);
    STATS[DIFFERING].fetch_add(differing as u64, Ordering::Relaxed);
}

/// Arms `units` (page range, what lands it, what to do when something other
/// than the lander touches it): private, committed, read-write memory is made
/// `PAGE_GUARD`. Those that cannot be armed come back, to be landed now.
pub(crate) fn register(units: Vec<(usize, usize, Land, Box<dyn Fn() + Send + Sync>)>) -> Vec<Land> {
    let mut refused = Vec::new();
    let mut armed = Vec::with_capacity(units.len());
    for (from, to, land, touched) in units {
        // Registered before the memory is guarded: a touch right after finds it.
        let unit = Arc::new(Unit {
            from,
            to,
            state: AtomicU8::new(PENDING),
            armed: std::sync::atomic::AtomicBool::new(true),
            land: Mutex::new(Some(land)),
            touched,
        });
        UNITS
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(unit.clone());
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        if crate::inplace::private_whole(from, to - from) && guard(from, to - from, true) {
            STATS[UNITS_N].fetch_add(1, Ordering::Relaxed);
            STATS[BYTES].fetch_add((to - from) as u64, Ordering::Relaxed);
            armed.push(unit);
            continue;
        }
        // Not armed: given back, as done.
        unit.armed.store(false, Ordering::SeqCst);
        STATS[UNARMED].fetch_add(1, Ordering::Relaxed);
        let land = unit.land.lock().unwrap_or_else(|e| e.into_inner()).take();
        unit.state.store(DONE, Ordering::SeqCst);
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
        refused.extend(land);
    }
    let _ = armed;
    refused
}

/// `PAGE_GUARD` on or off the pages `[at, at + len)`.
fn guard(at: usize, len: usize, on: bool) -> bool {
    let mut old = 0;
    let to = if on {
        PAGE_READWRITE | PAGE_GUARD
    } else {
        PAGE_READWRITE
    };
    // SAFETY: private memory of the title, its protection changes only.
    unsafe { VirtualProtect(at as *const _, len, to, &mut old) != 0 }
}

/// Lands `unit` if nobody has: unguards its pages, runs what lands it. If
/// another thread has it, waits until that one is done. No lock is held here.
fn ensure(unit: &Unit) {
    match unit
        .state
        .compare_exchange(PENDING, LANDING, Ordering::SeqCst, Ordering::SeqCst)
    {
        Ok(_) => {
            // The pages stay guarded while it lands: a page a thread other
            // than the lander touches that the lander has not is waited for,
            // one the lander has is landed (the guard goes with the first
            // access to a page; the lander's own accesses go on, below).
            let land = unit.land.lock().unwrap_or_else(|e| e.into_inner()).take();
            LANDING_DEPTH.set(LANDING_DEPTH.get() + 1);
            LANDING_UNITS.with_borrow_mut(|units| units.push(std::ptr::from_ref(unit)));
            if let Some(land) = land {
                land();
            }
            LANDING_UNITS.with_borrow_mut(|units| units.pop());
            LANDING_DEPTH.set(LANDING_DEPTH.get() - 1);
            // The guards of the pages it did not touch.
            guard(unit.from, unit.to - unit.from, false);
            unit.state.store(DONE, Ordering::SeqCst);
            ACTIVE.fetch_sub(1, Ordering::SeqCst);
            let _held = DONE_CV.0.lock().unwrap_or_else(|e| e.into_inner());
            DONE_CV.1.notify_all();
        }
        Err(DONE) => {}
        // A thread that is landing a unit does not wait for another's (two
        // that touched each other's would wait for ever): its pieces lie in
        // pages of its own.
        Err(_) if LANDING_DEPTH.get() > 0 => {}
        Err(_) => {
            STATS[WAITS].fetch_add(1, Ordering::Relaxed);
            let mut held = DONE_CV.0.lock().unwrap_or_else(|e| e.into_inner());
            while unit.state.load(Ordering::SeqCst) != DONE {
                held = DONE_CV
                    .1
                    .wait_timeout(held, std::time::Duration::from_millis(2))
                    .unwrap_or_else(|e| e.into_inner())
                    .0;
            }
        }
    }
}

fn units_in(at: usize, len: usize) -> Vec<Arc<Unit>> {
    let end = at.saturating_add(len);
    UNITS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|u| u.from < end && at < u.to && u.state.load(Ordering::SeqCst) != DONE)
        .cloned()
        .collect()
}

/// For the handler of `crate::watch`, on a guard page violation at `at`:
/// whether the page is one owed (it is landed, and the access goes on).
pub(crate) fn fault(at: usize) -> bool {
    if ACTIVE.load(Ordering::SeqCst) == 0 {
        return false;
    }
    let units = units_in(at, 1);
    let Some(unit) = units.first() else {
        return false;
    };
    // The landing's own access to the pages of the unit it is landing. A page
    // of another unit that is owed is landed first (nested: the guard went
    // with this access, and anyone's later one would find it unlanded).
    if landing_here(unit) {
        return true;
    }
    let began = std::time::Instant::now();
    (unit.touched)();
    ensure(unit);
    STATS[TOUCHES].fetch_add(1, Ordering::Relaxed);
    STATS[TOUCH_US].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    true
}

/// For the handler of `crate::watch`, on a guard page violation at `at` that
/// no unit owed and nothing else armed: whether the page belongs to a unit
/// that is done (or was a moment ago). The violation was raised while it was
/// owed, and the unit landed before the handler got here: the guard went with
/// the access, and the memory is the landing's, so the access goes on.
pub(crate) fn finished_lately(at: usize) -> bool {
    let within = |from: usize, to: usize| from <= at && at < to;
    // (Looked at in this order: `finish_all` notes a unit in `FINISHED`
    // before it takes it from `UNITS`.)
    if UNITS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|u| {
            within(u.from, u.to)
                && u.state.load(Ordering::SeqCst) == DONE
                && u.armed.load(Ordering::SeqCst)
        })
    {
        return true;
    }
    let now = std::time::Instant::now();
    FINISHED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .any(|&(from, to, when)| within(from, to) && now.duration_since(when) < GRACE)
}

/// Whether any unit is owed.
pub(crate) fn active() -> bool {
    ACTIVE.load(Ordering::SeqCst) != 0
}

/// Before our own read or the kernel's write of title memory `[at, at + len)`:
/// what of it is owed is landed.
pub(crate) fn land_range(at: usize, len: usize) {
    if ACTIVE.load(Ordering::SeqCst) == 0 {
        return;
    }
    for unit in units_in(at, len) {
        ensure(&unit);
    }
}

/// What is owed and not landed is landed, by this thread and those of the
/// landing pool: after the title was told.
pub(crate) fn run_pending() {
    if ACTIVE.load(Ordering::SeqCst) == 0 {
        return;
    }
    let began = std::time::Instant::now();
    let units: Vec<Arc<Unit>> = UNITS
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .filter(|u| u.state.load(Ordering::SeqCst) == PENDING)
        .cloned()
        .collect();
    super::each(units.len(), &|i| {
        ensure(&units[i]);
        STATS[BACKGROUND_N].fetch_add(1, Ordering::Relaxed);
    });
    STATS[BACKGROUND_US].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Everything owed is landed, whoever is landing it, and forgotten: before a
/// landing begins, so that no signal is told ahead of what an earlier one owes.
pub(crate) fn finish_all() {
    let units: Vec<Arc<Unit>> = {
        let all = UNITS.read().unwrap_or_else(|e| e.into_inner());
        if all.is_empty() {
            drop(all);
            // (What was forgotten long ago is let go of.)
            let mut finished = FINISHED.lock().unwrap_or_else(|e| e.into_inner());
            if !finished.is_empty() {
                let now = std::time::Instant::now();
                finished.retain(|f| now.duration_since(f.2) < GRACE);
            }
            return;
        }
        all.clone()
    };
    for unit in &units {
        ensure(unit);
    }
    let mut all = UNITS.write().unwrap_or_else(|e| e.into_inner());
    {
        let now = std::time::Instant::now();
        let mut finished = FINISHED.lock().unwrap_or_else(|e| e.into_inner());
        finished.retain(|f| now.duration_since(f.2) < GRACE);
        finished.extend(
            all.iter()
                .filter(|u| {
                    u.state.load(Ordering::SeqCst) == DONE && u.armed.load(Ordering::SeqCst)
                })
                .map(|u| (u.from, u.to, now)),
        );
    }
    all.retain(|u| u.state.load(Ordering::SeqCst) != DONE);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, VirtualAlloc, VirtualFree,
    };

    /// The tests share the owed set and the process's handler.
    static ONE_AT_A_TIME: Mutex<()> = Mutex::new(());

    fn memory(pages: usize) -> usize {
        crate::watch::install();
        // SAFETY: memory of ours, released by the tests.
        let at = unsafe {
            VirtualAlloc(
                std::ptr::null(),
                pages * PAGE,
                MEM_RESERVE | MEM_COMMIT,
                PAGE_READWRITE,
            )
        } as usize;
        assert_ne!(at, 0);
        at
    }

    fn release(at: usize) {
        // SAFETY: ours.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    fn quiet() -> Box<dyn Fn() + Send + Sync> {
        Box::new(|| {})
    }

    /// Pages that share a page are one unit; those apart are two.
    #[test]
    fn ranges_that_share_a_page_are_one_unit() {
        let p = PAGE;
        assert_eq!(
            groups(&[(10, 20), (p - 5, p + 5), (3 * p, 3 * p + 1), (4 * p, 5 * p)]),
            vec![(0, 2 * p), (3 * p, 5 * p)]
        );
        assert_eq!(
            groups(&[(0, p), (p, 2 * p)]),
            vec![(0, 2 * p)],
            "touching pages are one"
        );
        assert!(groups(&[]).is_empty());
    }

    /// A read of an owed page by a thread that knows nothing of it lands the
    /// unit first and sees what the landing put there; the other pages are
    /// let go; a write after it is ordered after the landing.
    #[test]
    fn a_read_or_a_write_of_an_owed_page_lands_it_first() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let at = memory(4);
        let (from, to) = (at + PAGE, at + 3 * PAGE);
        let touched = Arc::new(AtomicBool::new(false));
        let seen = touched.clone();
        let refused = register(vec![(
            from,
            to,
            Box::new(move || {
                // SAFETY: the pages, unguarded by `ensure` before this runs.
                unsafe { std::ptr::write_bytes(from as *mut u8, 0x77, to - from) };
            }),
            Box::new(move || seen.store(true, Ordering::SeqCst)),
        )]);
        assert!(refused.is_empty());
        let read = std::thread::spawn(move || {
            // The title's thread: a read of the second page of the unit.
            // SAFETY: inside the allocation.
            unsafe { ((at + 2 * PAGE + 7) as *const u8).read_volatile() }
        })
        .join()
        .unwrap();
        assert_eq!(read, 0x77, "never the bytes from before the landing");
        assert!(touched.load(Ordering::SeqCst));
        // The first page of the unit was let go with the rest: no fault, 0x77.
        // SAFETY: inside the allocation.
        assert_eq!(unsafe { *((at + PAGE) as *const u8) }, 0x77);
        // A page beside it was never owed.
        // SAFETY: as above.
        assert_eq!(unsafe { *(at as *const u8) }, 0);
        finish_all();
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);

        // A write: after the landing, so it stands.
        let at2 = memory(2);
        register(vec![(
            at2,
            at2 + PAGE,
            Box::new(move || {
                // SAFETY: as above.
                unsafe { std::ptr::write_bytes(at2 as *mut u8, 0x11, PAGE) };
            }),
            quiet(),
        )]);
        std::thread::spawn(move || {
            // SAFETY: inside the allocation.
            unsafe { ((at2 + 100) as *mut u8).write_volatile(5) };
        })
        .join()
        .unwrap();
        // SAFETY: as above.
        let (kept, landed) = unsafe { (*((at2 + 100) as *const u8), *((at2 + 101) as *const u8)) };
        assert_eq!(
            (kept, landed),
            (5, 0x11),
            "the title's write stands over the landing's bytes"
        );
        finish_all();
        release(at);
        release(at2);
    }

    /// Our own reads (`land_range`, which `crate::host::read` calls) land what
    /// they read first, and a read of what was not owed does not touch the set.
    #[test]
    fn our_own_reads_land_the_block_first() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let at = memory(3);
        let (from, to) = (at, at + 2 * PAGE);
        register(vec![(
            from,
            to,
            Box::new(move || {
                // SAFETY: the unguarded pages.
                unsafe { std::ptr::write_bytes(from as *mut u8, 0x42, to - from) };
            }),
            quiet(),
        )]);
        let mut buffer = vec![0u8; 64];
        // Elsewhere: nothing landed.
        assert!(crate::host::read_into(
            (at + 2 * PAGE) as *const u8,
            &mut buffer
        ));
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 1);
        // In the unit: landed, then read.
        assert!(crate::host::read_into(
            (at + PAGE + 10) as *const u8,
            &mut buffer
        ));
        assert!(buffer.iter().all(|&b| b == 0x42), "{buffer:?}");
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
        finish_all();
        release(at);
    }

    /// While one thread lands a unit, a thread that touches it waits for that
    /// one, holding nothing the lander needs; and the next landing's
    /// `finish_all` returns only when it is done.
    #[test]
    fn a_touch_waits_for_the_thread_landing_and_finish_all_for_both() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let at = memory(2);
        let landed = Arc::new(AtomicBool::new(false));
        let flag = landed.clone();
        let (began, wait) = std::sync::mpsc::channel();
        let began = Mutex::new(began);
        register(vec![(
            at,
            at + PAGE,
            Box::new(move || {
                began.lock().unwrap().send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
                // SAFETY: the unguarded page.
                unsafe { std::ptr::write_bytes(at as *mut u8, 0x33, PAGE) };
                flag.store(true, Ordering::SeqCst);
            }),
            quiet(),
        )]);
        // The relay's thread, landing it.
        let relay = std::thread::spawn(run_pending);
        wait.recv().unwrap();
        // The title's thread, holding a lock of its own, touches it meanwhile.
        let title_lock = Arc::new(Mutex::new(()));
        let held = title_lock.clone();
        let flag = landed.clone();
        let title = std::thread::spawn(move || {
            let _mine = held.lock().unwrap();
            // SAFETY: inside the allocation.
            let byte = unsafe { ((at + 9) as *const u8).read_volatile() };
            (byte, flag.load(Ordering::SeqCst))
        });
        let (byte, was_landed) = title.join().unwrap();
        assert_eq!(
            (byte, was_landed),
            (0x33, true),
            "it waited for the landing"
        );
        relay.join().unwrap();
        // The next landing begins with what is owed: nothing is, now.
        finish_all();
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
        // A second unit still owed is landed by `finish_all`, before it returns.
        let landed = Arc::new(AtomicBool::new(false));
        let flag = landed.clone();
        register(vec![(
            at + PAGE,
            at + 2 * PAGE,
            Box::new(move || flag.store(true, Ordering::SeqCst)),
            quiet(),
        )]);
        finish_all();
        assert!(landed.load(Ordering::SeqCst));
        release(at);
    }

    /// A thread that holds a copy's lock alone and touches an owed page of it
    /// does not wait for itself: the landing it does in the handler goes on
    /// without the lock after a while, and the thread goes on.
    #[test]
    fn a_thread_holding_the_copys_lock_does_not_wait_for_itself() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        assert!(crate::safecopy::on());
        let at = memory(2);
        let mut side = vec![0u8; PAGE];
        let copy = crate::shadow::Shadow::unbacked(at as u64, side.as_mut_ptr(), PAGE as u64, true);
        let fetched = vec![0x55u8; PAGE];
        let now = fetched.as_ptr() as usize;
        let land_copy = copy.clone();
        register(vec![(
            at,
            at + PAGE,
            Box::new(move || {
                let mut notes = crate::land::Notes::default();
                // SAFETY: the vector of fetched bytes, which outlives the test's thread.
                unsafe { land_copy.land_card_with(&[(0, PAGE, now)], 0, &mut notes, true) };
                notes.flush();
            }),
            quiet(),
        )]);
        let held = copy.clone();
        let began = std::time::Instant::now();
        let byte = std::thread::spawn(move || {
            let _alone = held.hold_merging();
            // SAFETY: inside the allocation.
            unsafe { ((at + 33) as *const u8).read_volatile() }
        })
        .join()
        .unwrap();
        assert_eq!(byte, 0x55, "landed, though the lock could not be had");
        assert!(began.elapsed() < std::time::Duration::from_secs(10));
        finish_all();
        release(at);
    }

    /// A thread touches an owed page; the unit lands (another thread's
    /// landing, in the test a handler placed before ours) after the guard
    /// page violation was raised and before our handler looks at it. The
    /// guard went with the touch and the landing is done: the handler takes
    /// it and the access goes on, where it used to be passed on, unhandled,
    /// and ended the process.
    #[test]
    fn a_unit_that_lands_between_the_touch_and_the_handler_is_handled() {
        use windows_sys::Win32::System::Diagnostics::Debug::{
            AddVectoredExceptionHandler, EXCEPTION_CONTINUE_SEARCH, EXCEPTION_POINTERS,
            RemoveVectoredExceptionHandler,
        };
        static PAGE_OF: AtomicUsize = AtomicUsize::new(0);
        static FIRED: AtomicBool = AtomicBool::new(false);
        /// Ahead of the handler of `crate::watch`: for the first violation on
        /// the test's page, the unit lands before that one is looked at.
        unsafe extern "system" fn lands_first(info: *mut EXCEPTION_POINTERS) -> i32 {
            // SAFETY: the system passes the exception being dispatched.
            let r = unsafe { &*(*info).ExceptionRecord };
            if r.ExceptionCode == 0x8000_0001u32 as i32
                && r.NumberParameters >= 2
                && r.ExceptionInformation[1] & !(PAGE - 1) == PAGE_OF.load(Ordering::SeqCst)
                && !FIRED.swap(true, Ordering::SeqCst)
            {
                finish_all();
            }
            EXCEPTION_CONTINUE_SEARCH
        }
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        // (Memory of tests before, at the same addresses.)
        FINISHED.lock().unwrap_or_else(|e| e.into_inner()).clear();
        let at = memory(3);
        let (from, to) = (at, at + 2 * PAGE);
        register(vec![(
            from,
            to,
            Box::new(move || {
                // SAFETY: the pages, unguarded by `ensure` before this runs.
                unsafe { std::ptr::write_bytes(from as *mut u8, 0x77, to - from) };
            }),
            quiet(),
        )]);
        PAGE_OF.store(at + PAGE, Ordering::SeqCst);
        FIRED.store(false, Ordering::SeqCst);
        // SAFETY: a handler that only lands the test's unit, removed below.
        let handler = unsafe { AddVectoredExceptionHandler(1, Some(lands_first)) };
        assert!(!handler.is_null());
        let read = std::thread::spawn(move || {
            // SAFETY: inside the allocation.
            unsafe { ((at + PAGE + 7) as *const u8).read_volatile() }
        })
        .join()
        .unwrap();
        // SAFETY: the one added above.
        unsafe { RemoveVectoredExceptionHandler(handler) };
        assert!(
            FIRED.load(Ordering::SeqCst),
            "the unit landed before the handler looked"
        );
        assert_eq!(read, 0x77, "the access went on over the landed bytes");
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
        // A unit that is done is remembered for a while, and then it is not.
        assert!(finished_lately(at + PAGE + 7));
        assert!(
            !finished_lately(at + 2 * PAGE),
            "a page beside it was never owed"
        );
        std::thread::sleep(GRACE + std::time::Duration::from_millis(50));
        finish_all();
        assert!(!finished_lately(at + PAGE + 7));
        release(at);
    }

    /// The title frees, unmaps or maps again memory that holds an owed unit:
    /// the unit lands first, while it is the title's memory, not after, over
    /// whatever is there next.
    #[test]
    fn memory_the_title_frees_or_unmaps_lands_what_is_owed_first() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let owed = |at: usize| {
            let landed = Arc::new(AtomicBool::new(false));
            let flag = landed.clone();
            register(vec![(
                at + PAGE,
                at + 2 * PAGE,
                Box::new(move || {
                    // SAFETY: the unguarded page.
                    unsafe { std::ptr::write_bytes((at + PAGE) as *mut u8, 0x66, PAGE) };
                    flag.store(true, Ordering::SeqCst);
                }),
                quiet(),
            )]);
            assert!(!landed.load(Ordering::SeqCst));
            landed
        };
        let at = memory(3);
        let landed = owed(at);
        crate::watch::title_memory_freed(at, 3 * PAGE);
        assert!(landed.load(Ordering::SeqCst), "freed with its length");
        release(at);
        // No length: the whole allocation.
        let at = memory(3);
        let landed = owed(at);
        crate::watch::title_memory_freed(at, 0);
        assert!(landed.load(Ordering::SeqCst), "freed with no length");
        release(at);
        let at = memory(3);
        let landed = owed(at);
        crate::watch::title_memory_unmapping(at + PAGE, PAGE);
        assert!(landed.load(Ordering::SeqCst), "unmapped");
        release(at);
        let at = memory(3);
        let landed = owed(at);
        crate::watch::title_memory_remapped(at, 2 * PAGE);
        assert!(landed.load(Ordering::SeqCst), "mapped again");
        release(at);
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
        finish_all();
    }

    /// A thread that lands a unit and touches a page of another unit that is
    /// owed does not go on over unlanded memory: that unit is landed first
    /// (nested), and what the thread reads is what it landed. The pass through
    /// the guard is for the pages of the unit the thread is landing.
    #[test]
    fn a_landing_thread_touching_another_owed_unit_lands_that_one_first() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let at = memory(4);
        let seen = Arc::new(std::sync::atomic::AtomicU8::new(0xFF));
        let other_landed = Arc::new(AtomicBool::new(false));
        let (b_from, b_flag) = (at + 2 * PAGE, other_landed.clone());
        let (read, flag) = (seen.clone(), other_landed.clone());
        register(vec![
            (
                at,
                at + PAGE,
                Box::new(move || {
                    // A page of the other unit, read by the landing of this one.
                    // SAFETY: inside the allocation.
                    let byte = unsafe { ((b_from + 5) as *const u8).read_volatile() };
                    read.store(byte, Ordering::SeqCst);
                    assert!(
                        flag.load(Ordering::SeqCst),
                        "the other unit landed before the byte was read"
                    );
                    // SAFETY: the unit's own page.
                    unsafe { std::ptr::write_bytes(at as *mut u8, 0x11, PAGE) };
                }),
                quiet(),
            ),
            (
                b_from,
                b_from + PAGE,
                Box::new(move || {
                    // SAFETY: the other unit's page.
                    unsafe { std::ptr::write_bytes(b_from as *mut u8, 0x22, PAGE) };
                    b_flag.store(true, Ordering::SeqCst);
                }),
                quiet(),
            ),
        ]);
        land_range(at, PAGE);
        assert_eq!(seen.load(Ordering::SeqCst), 0x22);
        assert!(other_landed.load(Ordering::SeqCst));
        // Nothing is left owed twice or not at all.
        finish_all();
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
        // SAFETY: inside the allocation.
        assert_eq!(unsafe { *(at as *const u8) }, 0x11);
        release(at);
    }

    /// While a unit is landed page by page, a thread that touches a page the
    /// lander has not reached waits for the landing (the page stays guarded
    /// until the lander touches it) and reads what was landed.
    #[test]
    fn pages_the_lander_has_not_reached_stay_guarded_until_it_has() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let at = memory(3);
        let (began, wait) = std::sync::mpsc::channel();
        let began = Mutex::new(began);
        register(vec![(
            at,
            at + 2 * PAGE,
            Box::new(move || {
                // SAFETY: the unit's pages.
                unsafe { std::ptr::write_bytes(at as *mut u8, 0x44, PAGE) };
                began.lock().unwrap().send(()).unwrap();
                std::thread::sleep(std::time::Duration::from_millis(150));
                // SAFETY: as above.
                unsafe { std::ptr::write_bytes((at + PAGE) as *mut u8, 0x44, PAGE) };
            }),
            quiet(),
        )]);
        let lander = std::thread::spawn(run_pending);
        wait.recv().unwrap();
        let began = std::time::Instant::now();
        let byte = std::thread::spawn(move || {
            // SAFETY: inside the allocation: the second page, not yet reached.
            unsafe { ((at + PAGE + 9) as *const u8).read_volatile() }
        })
        .join()
        .unwrap();
        assert_eq!(byte, 0x44, "never the bytes from before the landing");
        assert!(
            began.elapsed() >= std::time::Duration::from_millis(50),
            "it waited for the lander"
        );
        lander.join().unwrap();
        finish_all();
        release(at);
    }

    /// The verifier counts the dwords of a landed piece that differ from what
    /// was fetched.
    #[test]
    fn the_verifier_counts_what_differs() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        assert!(crate::safecopy::on());
        let at = memory(1);
        let mut fetched = vec![9u8; 64];
        // SAFETY: inside the allocation.
        unsafe { std::ptr::write_bytes(at as *mut u8, 9, 64) };
        let (checked, differing) = (
            STATS[CHECKED].load(Ordering::Relaxed),
            STATS[DIFFERING].load(Ordering::Relaxed),
        );
        verify(at, fetched.as_ptr(), 64);
        assert_eq!(STATS[CHECKED].load(Ordering::Relaxed) - checked, 64);
        assert_eq!(STATS[DIFFERING].load(Ordering::Relaxed), differing);
        fetched[8] = 1;
        fetched[40] = 2;
        verify(at, fetched.as_ptr(), 64);
        assert_eq!(STATS[DIFFERING].load(Ordering::Relaxed) - differing, 2);
        release(at);
    }

    /// Memory that is not private read-write memory is not armed: given back.
    #[test]
    fn what_cannot_be_armed_comes_back() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let refused = register(vec![(PAGE, 2 * PAGE, Box::new(|| {}), quiet())]);
        assert_eq!(refused.len(), 1);
        assert_eq!(ACTIVE.load(Ordering::SeqCst), 0);
        finish_all();
    }
}
