//! The refreshes of the submission that comes next, begun while this one is
//! sent (`VICTORY_PIPELINE_PREPARE=1`; off by default).
//!
//! The thread that prepares submissions spends most of a frame on what reads
//! title memory and writes host copies: the write watch asked, the copies
//! refreshed, the steps. When it has taken the bytes this submission sends up
//! (`send::take`), what is left of it (the lists that record them, the
//! compare, the send, the flight in the line) does not touch a host copy's
//! bytes. Meanwhile the next submission of the same queue is in the line, the
//! title's call for it made: what it refreshes is in title memory as the
//! title left it. A thread of ours refreshes those copies now, as the next
//! submission's own preparing would a moment later, and that finds them
//! brought up to date (`Shadow::nothing_new`) and asks the kernel for what
//! the title wrote since.
//!
//! What makes it safe, in the order the invariants are tested:
//!
//! 1. The title's writes are seen before the refresh: the submission is in
//!    the line, so the title's call is behind it, and every write it made
//!    for it. Writes made after the refresh are found by the asking of the
//!    submission's own preparing, which comes later and has a higher number.
//! 2. What flights in the air write is left alone as always: the refresh is
//!    `Shadow::update`, which leaves ranges declared written by submissions
//!    in the air (the one being sent among them, declared before this
//!    starts) to their landing and to the stale-bytes step of the later
//!    preparing.
//! 3. Fences and signals are in order, and 4. only our thread of
//!    submissions gives lists to the queue, in order: this thread gives
//!    nothing to the queue and signals nothing; 5. the values signalled
//!    are the submission lock's, which this does not take.
//!
//! The preparing of the next submission waits for this to be done before it
//! asks or refreshes anything ([`join`]): the two are never at the same
//! copies at once. The asking is one that stands beside others
//! (`watch::ask_standing`).

use super::*;

/// Whether the next submission's refreshes are begun early.
///
/// (`VICTORY_PIPELINE_PREPARE` may change while the title runs, `VICTORY_AB`: [`offer`] reads this once, and a refresh
/// offered while it was on is joined by [`join`] after it went off.)
pub(super) fn on() -> bool {
    crate::queue::pipeline_prepare() && late() && relay_exact()
}

/// Whether something was ever offered: from then on [`join`] looks at the gate whether the switch is on or not.
static OFFERED_EVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Offered work, being done, and the wait for it to be over.
struct Gate<W> {
    state: Mutex<(Option<W>, bool)>,
    changed: std::sync::Condvar,
}

impl<W> Gate<W> {
    const fn new() -> Self {
        Gate { state: Mutex::new((None, false)), changed: std::sync::Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Option<W>, bool)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Offers `work`: refused (given back) if some is offered or being done.
    fn offer(&self, work: W) -> Result<(), W> {
        let mut state = self.lock();
        if state.0.is_some() || state.1 {
            return Err(work);
        }
        state.0 = Some(work);
        drop(state);
        self.changed.notify_all();
        Ok(())
    }

    /// The thread that does it: waits for work and takes it; it is being done
    /// until [`Gate::done`].
    fn take(&self) -> W {
        let mut state = self.lock();
        loop {
            if let Some(work) = state.0.take() {
                state.1 = true;
                return work;
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    fn done(&self) {
        self.lock().1 = false;
        self.changed.notify_all();
    }

    /// Waits until nothing is offered or being done; whether it had to.
    fn join(&self) -> bool {
        let mut state = self.lock();
        let waited = state.0.is_some() || state.1;
        while state.0.is_some() || state.1 {
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        waited
    }
}

/// What is to be refreshed: per host copy, the span, as the next submission's
/// own refreshing would make it.
type Spans = Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>;

static GATE: Gate<Spans> = Gate::new();

/// Since the last frame-rate line: offers made, offers with nothing to
/// refresh (no next submission known, or its copies are being given to the
/// sends), refreshes done, their spans, bytes asked, microseconds the thread
/// worked, and microseconds the next preparing waited for it.
static STATS: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];
const OFFERED: usize = 0;
const NOTHING: usize = 1;
const DONE: usize = 2;
const SPANS: usize = 3;
const BYTES: usize = 4;
const WORKED: usize = 5;
const WAITED: usize = 6;
const REFUSED: usize = 7;

/// The spans of the refreshes of the lists at `lists` (host lists, as the
/// addresses the line gave), as [`Batch::refresh_front`] makes them for one
/// part, but for the copies not in `excluding`.
///
/// # Safety
///
/// The addresses name host lists that nothing changes or takes while this
/// runs ([`crate::queue::take_ahead`]).
unsafe fn spans_of(lists: &[usize], excluding: &[usize]) -> Spans {
    let mut copies: ByCopy<std::sync::Arc<crate::shadow::Shadow>> = ByCopy::default();
    let mut touches = Vec::new();
    for &at in lists {
        // SAFETY: the caller's.
        let list = unsafe { &*(at as *const HostList) };
        let mut last = 0;
        for step in &list.deferred().copies {
            match step {
                Step::Refresh(shadow, at, len) => {
                    let key = std::sync::Arc::as_ptr(shadow) as usize;
                    if key != last {
                        copies.entry(key).or_insert_with(|| shadow.clone());
                        last = key;
                    }
                    touches.push(Touch::Refresh(key, *at, *at + *len));
                }
                Step::GpuWrite(shadow, ..) => touches.push(Touch::Write(std::sync::Arc::as_ptr(shadow) as usize)),
                _ => {}
            }
        }
    }
    front_spans_gap(touches.into_iter(), refresh_narrow_gap())
        .into_iter()
        .filter(|(key, ..)| !excluding.contains(key))
        .filter_map(|(key, from, to)| Some((copies.get(&key)?.clone(), from, to)))
        .collect()
}

/// After the submission's bytes are taken: offers the refreshes of the
/// submission that comes next, if our thread of submissions named it
/// ([`crate::queue::take_ahead`]). `excluding`: copies (by address) that this
/// submission still does something to, which are left alone.
pub(super) fn offer(excluding: &[usize]) {
    if !on() {
        return;
    }
    let Some(next) = crate::queue::take_ahead() else {
        STATS[NOTHING].fetch_add(1, Ordering::Relaxed);
        return;
    };
    // SAFETY: from `take_ahead`, on our thread of submissions, which is the
    // one that takes the entry out of the line.
    let spans = unsafe { spans_of(&next, excluding) };
    if spans.is_empty() {
        STATS[NOTHING].fetch_add(1, Ordering::Relaxed);
        return;
    }
    start();
    OFFERED_EVER.store(true, Ordering::Release);
    match GATE.offer(spans) {
        Ok(()) => {
            STATS[OFFERED].fetch_add(1, Ordering::Relaxed);
        }
        Err(_) => {
            STATS[REFUSED].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Whether [`join`] looks at the gate: while the switch is on, and once anything was offered whatever the switch says
/// now (it may change while the title runs, `VICTORY_AB`: what was offered while it was on is over before the next
/// preparing asks or refreshes the same copies).
fn must_join(on: bool, offered_ever: bool) -> bool {
    on || offered_ever
}

/// Before the preparing of a submission asks or refreshes anything: the
/// refreshes begun early are over.
pub(super) fn join() {
    if !must_join(on(), OFFERED_EVER.load(Ordering::Acquire)) {
        return;
    }
    let began = std::time::Instant::now();
    if GATE.join() {
        STATS[WAITED].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let made = std::thread::Builder::new().name("victory-prefetch".into()).spawn(|| {
            crate::cores::keep_on_fast_cores("victory-prefetch");
            crate::cores::prioritize("victory-prefetch", crate::cores::Role::Helper);
            loop {
                let spans = GATE.take();
                let began = std::time::Instant::now();
                let _span = crate::timeline::span("refreshes of the next submission, begun early");
                // (A refresh that panicked is the next preparing's to do.)
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(spans)));
                STATS[WORKED].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                GATE.done();
            }
        });
        if let Err(e) = made {
            tracing::warn!(error = %e, "no thread to begin the next submission's refreshes: they are made by its preparing");
        }
    });
}

/// The write watch asked about the copies' spans, and each brought up to date
/// unless it is known to hold nothing new, as `Batch::refresh_front` does.
fn run(mut spans: Spans) {
    // Per copy, what is asked about (once, as a submission's own asking is).
    let mut ranges: ByCopy<(usize, usize)> = ByCopy::default();
    for (shadow, from, to) in &spans {
        if shadow.asked_ahead() {
            let span = ranges.entry(std::sync::Arc::as_ptr(shadow) as usize).or_insert((*from, *to));
            *span = (span.0.min(*from), span.1.max(*to));
        }
    }
    let ranges: Vec<(usize, usize)> = ranges.into_values().collect();
    let once = crate::watch::once();
    let standing = (once && !ranges.is_empty()).then(|| crate::watch::ask_standing(&ranges, None));
    if !once && spans.len() > 1 {
        crate::watch::ask_ahead(&ranges);
    }
    const FEW: usize = 256;
    if spans.len() > FEW {
        spans.retain(|(shadow, from, to)| !shadow.nothing_new_noted(*from, *to - *from));
    }
    let many = spans.len() > FEW;
    if !many {
        spans.retain(|(shadow, from, to)| !shadow.nothing_new(*from, *to - *from));
    }
    STATS[DONE].fetch_add(1, Ordering::Relaxed);
    STATS[SPANS].fetch_add(spans.len() as u64, Ordering::Relaxed);
    STATS[BYTES].fetch_add(spans.iter().map(|s| (s.2 - s.1) as u64).sum(), Ordering::Relaxed);
    crate::shadow::wide_each(spans.len(), move |i| {
        let (shadow, from, to) = &spans[i];
        if many && shadow.nothing_new(*from, *to - *from) {
            return;
        }
        Batch::update(shadow, *from, *to);
    });
    match standing {
        Some(standing) => crate::watch::end_standing(standing),
        None => {
            if !once {
                crate::watch::ask_ahead_done();
            }
        }
    }
}

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64).collect();
    if !on() {
        return;
    }
    tracing::info!(
        "the next submission's refreshes begun early, per frame (VICTORY_PIPELINE_PREPARE): {:.1} offered, {:.1} with nothing to refresh, \
         {:.1} refused (the one before not over), {:.1} done of {:.0} spans and {:.1} MB asked; the thread worked {:.1} ms, the preparing \
         that came next waited {:.1} ms for it (so {:.1} ms of it went beside what the submission before was doing)",
        s[OFFERED] / frames,
        s[NOTHING] / frames,
        s[REFUSED] / frames,
        s[DONE] / frames,
        s[SPANS] / frames,
        s[BYTES] / 1e6 / frames,
        s[WORKED] / 1000.0 / frames,
        s[WAITED] / 1000.0 / frames,
        (s[WORKED] - s[WAITED]).max(0.0) / 1000.0 / frames
    );
}

#[cfg(test)]
mod copy_tests {
    use crate::shadow::Shadow;
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

    const PAGE: usize = 4096;
    const N: usize = 16;
    const MEM_WRITE_WATCH: u32 = 0x20_0000;

    /// The tests ask the write watch, which is the process's one.
    static ONE_AT_A_TIME: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn memory() -> usize {
        // SAFETY: memory of ours, released by the test.
        let at = unsafe { VirtualAlloc(std::ptr::null(), N * PAGE, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) } as usize;
        assert_ne!(at, 0);
        at
    }

    fn write(at: usize, offset: usize, byte: u8) {
        // SAFETY: inside the allocation.
        unsafe { ((at + offset) as *mut u8).write_volatile(byte) };
    }

    /// What `prefetch::run` does with spans, and what the preparing that
    /// follows does with its own: asked once as a standing asking, a range
    /// brought up to date unless it is known to hold nothing new.
    fn refresh(shadow: &Shadow, at: usize, spans: &[(usize, usize)]) {
        let ranges: Vec<(usize, usize)> = spans.iter().map(|&(a, b)| (at + a, at + b)).collect();
        let standing = crate::watch::ask_standing(&ranges, None);
        for &(from, to) in &ranges {
            if !shadow.nothing_new(from, to - from) {
                shadow.update(from, to - from);
            }
        }
        crate::watch::end_standing(standing);
    }

    /// Takes what goes up to the card as the take does, into the card's copy.
    fn take(shadow: &Shadow, held: &[u8], card: &mut [u8]) {
        for (a, b) in shadow.take_dirty() {
            card[a..b].copy_from_slice(&held[a..b]);
        }
    }

    /// The refresh begun early, the title writing before it, while it runs
    /// and after it, and the preparing's own refresh after: what goes up to
    /// the card is the title's memory, every byte, every time (nothing the
    /// title wrote is left behind by what the early refresh noted as done).
    #[test]
    fn what_the_title_writes_around_an_early_refresh_reaches_the_card() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let at = memory();
        let mut held = vec![0u8; N * PAGE];
        let mut card = vec![0u8; N * PAGE];
        let shadow = Shadow::unbacked(at as u64, held.as_mut_ptr(), (N * PAGE) as u64, true);
        let mut seed = 0x9e37_79b9_7f4a_7c15u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        // The first look takes it whole.
        refresh(&shadow, at, &[(0, N * PAGE)]);
        let held_ptr = held.as_ptr() as usize;
        let held_now = move || {
            // SAFETY: the vector lives to the end of the test and nothing else
            // writes it but the refreshes, which this reads after.
            unsafe { std::slice::from_raw_parts(held_ptr as *const u8, N * PAGE) }
        };
        take(&shadow, held_now(), &mut card);
        for round in 0..300 {
            // The title writes (the submission's data), some pages, some bytes.
            for _ in 0..(next() % 6) {
                let page = (next() % N as u64) as usize;
                let offset = (next() % PAGE as u64) as usize;
                write(at, page * PAGE + offset, (next() % 255) as u8 + 1);
            }
            // The early refresh, of some spans, beside more writes of the
            // title (its other threads) from this thread.
            let mut spans = Vec::new();
            for _ in 0..(next() % 4) {
                let from = (next() % N as u64) as usize;
                let to = (from + 1 + (next() % 4) as usize).min(N);
                spans.push((from * PAGE, to * PAGE));
            }
            let shadow2 = shadow.clone();
            let early = {
                let spans = spans.clone();
                std::thread::spawn(move || refresh(&shadow2, at, &spans))
            };
            for _ in 0..(next() % 4) {
                let page = (next() % N as u64) as usize;
                write(at, page * PAGE + (next() % PAGE as u64) as usize, (next() % 255) as u8 + 1);
            }
            early.join().unwrap();
            // The title writes again before the preparing begins.
            for _ in 0..(next() % 3) {
                let page = (next() % N as u64) as usize;
                write(at, page * PAGE + (next() % PAGE as u64) as usize, (next() % 255) as u8 + 1);
            }
            // The preparing's own: all the submission refers to.
            refresh(&shadow, at, &[(0, N * PAGE)]);
            take(&shadow, held_now(), &mut card);
            // SAFETY: the allocation; nothing writes it now.
            let title = unsafe { std::slice::from_raw_parts(at as *const u8, N * PAGE) };
            assert!(card == title, "a byte of the title's did not reach the card in round {round}");
        }
        // SAFETY: ours.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }

    /// What a submission in the air declares written is left alone by the
    /// early refresh as by any (the title's bytes there wait for its
    /// landing), and taken when the range is done with.
    #[test]
    fn a_range_shaders_may_write_is_left_alone_by_the_early_refresh() {
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let at = memory();
        let mut held = vec![0u8; N * PAGE];
        let shadow = Shadow::unbacked(at as u64, held.as_mut_ptr(), (N * PAGE) as u64, true);
        refresh(&shadow, at, &[(0, N * PAGE)]);
        let range = (at + 2 * PAGE, at + 3 * PAGE);
        shadow.writing(range.0, range.1 - range.0);
        write(at, 2 * PAGE + 5, 9);
        write(at, 4 * PAGE + 5, 8);
        refresh(&shadow, at, &[(0, N * PAGE)]);
        assert!(shadow.writes_pending_in(range.0, range.1));
        let side = |offset: usize| held[offset];
        assert_eq!(side(4 * PAGE + 5), 8, "outside the range it is copied");
        assert_eq!(side(2 * PAGE + 5), 0, "inside it waits for the landing");
        shadow.copy_back_done(&[range]);
        assert!(!shadow.writes_pending_in(range.0, range.1));
        refresh(&shadow, at, &[(0, N * PAGE)]);
        assert_eq!(held[2 * PAGE + 5], 9, "taken when the range is done with");
        // SAFETY: ours.
        unsafe { VirtualFree(at as *mut _, 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod tests {
    use super::Gate;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    #[test]
    fn a_join_looks_at_the_gate_after_the_switch_went_off_once_anything_was_offered() {
        assert!(super::must_join(true, false));
        assert!(super::must_join(false, true));
        assert!(super::must_join(true, true));
        assert!(!super::must_join(false, false));
    }

    /// The preparing that comes next is not begun before the work offered is
    /// done, and what is offered while some is not over is refused.
    #[test]
    fn the_next_preparing_waits_for_what_was_offered() {
        let gate: Arc<Gate<u32>> = Arc::new(Gate::new());
        let over = Arc::new(AtomicBool::new(false));
        let runs = Arc::new(AtomicUsize::new(0));
        // Nothing offered: nothing to wait for.
        assert!(!gate.join());
        let worker = {
            let (gate, over, runs) = (gate.clone(), over.clone(), runs.clone());
            std::thread::spawn(move || {
                for _ in 0..2 {
                    let _work = gate.take();
                    std::thread::sleep(Duration::from_millis(60));
                    runs.fetch_add(1, Ordering::SeqCst);
                    over.store(true, Ordering::SeqCst);
                    gate.done();
                }
            })
        };
        assert!(gate.offer(1).is_ok());
        // While it is being done (or offered) another is refused.
        std::thread::sleep(Duration::from_millis(15));
        assert_eq!(gate.offer(2), Err(2));
        assert!(gate.join(), "it had to wait");
        assert!(over.load(Ordering::SeqCst), "not before the work was over");
        assert_eq!(runs.load(Ordering::SeqCst), 1);
        // Over: the next offer is taken, and again waited for.
        over.store(false, Ordering::SeqCst);
        assert!(gate.offer(3).is_ok());
        gate.join();
        assert!(over.load(Ordering::SeqCst));
        assert_eq!(runs.load(Ordering::SeqCst), 2);
        worker.join().unwrap();
    }
}
