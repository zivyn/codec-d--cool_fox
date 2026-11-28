//! The write watch asked about a submission's ranges on a thread of ours while the submitting thread plans, waits for
//! flights and sweeps (on unless `VICTORY_ASK_ASIDE=0`).
//!
//! A submission sent ahead begins with the steps it hands to the card (`card_steps`, 6 to 8 ms a frame, 2 to 4 ms in
//! each of the few submissions that have any), the wait for the flights it reads from and the once-a-second sweep,
//! and only then asks the kernel which pages of the ranges its lists refresh were written (5.7 ms a frame, 1 to 3 ms
//! in the same big submissions). The two do not depend on each other: the ask reads the kernel's bits and notes in
//! the journal, the planning reads the line of flights and writes a list of its own. Here the ask is made by the
//! thread below, from the ranges the lists name, and the submission joins it where it used to ask.
//!
//! What makes it as safe as asking later:
//!
//! 1. The title's writes for the submission are seen: it is in the line when it is taken, so the title's call, and
//!    every write it made for it, came before the ask, which is made after that. Writes made after the ask are found
//!    by the next one, with a higher number; a copy that reads them now holds them under the older number and takes
//!    them again then (more, never less).
//! 2. Flights that land title memory are not between the ask and the refreshes: a submission that waited for flights
//!    (`late_blocked`, the sweep's settle) asks again after the wait, as it did, and what was asked here is not used
//!    (`settled`). The wait is the only thing of its own that writes title memory before the refreshes.
//! 3. The ranges are those of the lists named by the title; the list of steps for the card that the planning makes
//!    names a few more copies, and those are asked after the join (`watch::ask_more`, which holds the standing asking
//!    and adds to it). A range the asking did not hold is asked by the refresh that reads it.
//! 4. One asking is made at a time: the thread is offered work only when it has none, and the submission waits for
//!    it before it takes another ([`Offered`] waits when dropped).

use super::*;

/// Whether the ask is made beside the planning.
///
/// (`VICTORY_ASK_ASIDE` may change while the title runs, `VICTORY_AB`: a submission reads it once, offers or not, and
/// joins what it offered; the asking that results is the one a submission makes either way.)
pub(super) fn on() -> bool {
    crate::ab::live_value!("VICTORY_ASK_ASIDE", bool, crate::ab::unless_zero)
}

/// Whether a submission offers its asking to the thread: only with flights in the air (`flying`, asked last: with none
/// there is nothing to go beside), and never for a submission stage A prepared (`staged`: its asking was made, and what is
/// offered here is not joined before the lane's `ask_once_end`, so it would end the asking of the stage after it and
/// stand over its pages: rev2).
pub(super) fn wanted(late: bool, staged: bool, flying: impl FnOnce() -> bool) -> bool {
    late && !staged && on() && crate::watch::once() && !ask_tokens() && flying()
}

/// What is offered to a thread and whether it is at it.
struct State {
    work: Option<Vec<(usize, usize)>>,
    busy: bool,
}

/// A thread that asks, and what is offered to it. The lane has one; the thread of stage A has another
/// (`VICTORY_STAGE_A_ASK_ASIDE`: its asking beside its planning), so that the two never meet at one.
struct Chan {
    state: Mutex<State>,
    changed: std::sync::Condvar,
    alive: std::sync::atomic::AtomicBool,
    started: std::sync::Once,
    name: &'static str,
    /// Whether the thread runs at the priority of the threads of stage A (`VICTORY_FRONT_HIGH`).
    front: bool,
}

impl Chan {
    const fn new(name: &'static str, front: bool) -> Chan {
        Chan {
            state: Mutex::new(State { work: None, busy: false }),
            changed: std::sync::Condvar::new(),
            alive: std::sync::atomic::AtomicBool::new(false),
            started: std::sync::Once::new(),
            name,
            front,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }
}

static LANE: Chan = Chan::new("victory-ask-aside", false);
static FRONT: Chan = Chan::new("victory-front-ask", true);

/// Since the last frame-rate line: asks offered, asks made again after a wait for flights, ranges asked after the
/// join, microseconds the thread worked and microseconds the submissions waited for it; then the same for the thread of
/// stage A's own (offered, worked, waited).
static STATS: [std::sync::atomic::AtomicU64; 8] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 8];
const OFFERED: usize = 0;
const REDONE: usize = 1;
const MORE: usize = 2;
const WORKED: usize = 3;
const WAITED: usize = 4;
const FRONT_OFFERED: usize = 5;
const FRONT_WORKED: usize = 6;
const FRONT_WAITED: usize = 7;

#[cfg(test)]
fn lock() -> std::sync::MutexGuard<'static, State> {
    LANE.lock()
}

/// The work done: the thread is not busy any more, whatever came of it.
struct Done(&'static Chan);

impl Drop for Done {
    fn drop(&mut self) {
        self.0.lock().busy = false;
        self.0.changed.notify_all();
    }
}

fn start(chan: &'static Chan) {
    chan.started.call_once(|| {
        let made = std::thread::Builder::new().name(chan.name.into()).spawn(move || {
            crate::cores::keep_helper_on_fast_cores(chan.name);
            crate::cores::prioritize(chan.name, crate::cores::Role::Helper);
            loop {
                let ranges = {
                    let mut state = chan.lock();
                    loop {
                        if let Some(ranges) = state.work.take() {
                            state.busy = true;
                            break ranges;
                        }
                        state = chan.changed.wait(state).unwrap_or_else(|e| e.into_inner());
                    }
                };
                if chan.front {
                    crate::sched::front_sync();
                }
                let began = std::time::Instant::now();
                {
                    let _done = Done(chan);
                    let _span = crate::timeline::span("the write watch asked, beside the planning");
                    // (An asking that panicked is made again by the submission, which finds none standing.)
                    let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| crate::watch::ask_once(&ranges)));
                }
                STATS[if chan.front { FRONT_WORKED } else { WORKED }].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
            }
        });
        match made {
            Ok(_) => chan.alive.store(true, Ordering::Release),
            Err(e) => tracing::warn!(error = %e, "no thread to ask the write watch beside the planning: the submissions ask for themselves"),
        }
    });
}

/// An asking that is being made, or was, which the holder waits for when it drops this.
pub(super) struct Offered(&'static Chan);

fn offer(chan: &'static Chan, ranges: Vec<(usize, usize)>) -> Option<Offered> {
    start(chan);
    if !chan.alive.load(Ordering::Acquire) {
        return None;
    }
    let mut state = chan.lock();
    if state.work.is_some() || state.busy {
        return None;
    }
    state.work = Some(ranges);
    drop(state);
    chan.changed.notify_all();
    STATS[if chan.front { FRONT_OFFERED } else { OFFERED }].fetch_add(1, Ordering::Relaxed);
    Some(Offered(chan))
}

/// Offers the asking about `ranges` to the thread; `None` if it has none to give (it is still at the one before, or
/// there is no thread).
pub(super) fn begin(ranges: Vec<(usize, usize)>) -> Option<Offered> {
    offer(&LANE, ranges)
}

/// [`begin`] for the thread of stage A (`VICTORY_STAGE_A_ASK_ASIDE`), on the thread of its own.
pub(super) fn begin_front(ranges: Vec<(usize, usize)>) -> Option<Offered> {
    offer(&FRONT, ranges)
}

/// Whether the thread of stage A asks beside its planning (`VICTORY_STAGE_A_ASK_ASIDE=1`, with the one asking and not the
/// standing askings of `VICTORY_ASK_TOKENS`).
///
/// (May change while the title runs, `VICTORY_AB`: `Host::front` reads it once for a submission and joins what it offered.)
pub(super) fn front_wanted() -> bool {
    crate::ab::live_flag!("VICTORY_STAGE_A_ASK_ASIDE") && crate::watch::once() && !ask_tokens()
}

impl Offered {
    /// Waits until the asking is made.
    pub(super) fn join(self) {
        drop(self);
    }
}

impl Drop for Offered {
    fn drop(&mut self) {
        let chan = self.0;
        let began = std::time::Instant::now();
        let mut state = chan.lock();
        while state.work.is_some() || state.busy {
            state = chan.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
        drop(state);
        STATS[if chan.front { FRONT_WAITED } else { WAITED }].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// An asking that was made is not used, a wait for flights came between: counted.
pub(super) fn redone() {
    STATS[REDONE].fetch_add(1, Ordering::Relaxed);
}

/// Ranges asked after the join: those the planning added.
pub(super) fn more(ranges: usize) {
    STATS[MORE].fetch_add(ranges as u64, Ordering::Relaxed);
}

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let s: Vec<f64> = STATS[..FRONT_OFFERED]
        .iter()
        .map(|s| s.swap(0, Ordering::Relaxed) as f64)
        .collect();
    if !on() && s[OFFERED] == 0.0 {
        return;
    }
    tracing::info!(
        "the write watch asked beside the planning, per frame (VICTORY_ASK_ASIDE): {:.1} offered, {:.1} made again after a wait for flights, {:.1} ranges \
         asked after it; the thread worked {:.1} ms, the submissions waited {:.1} ms for it (so {:.1} ms of the asking went beside what they did)",
        s[OFFERED] / frames,
        s[REDONE] / frames,
        s[MORE] / frames,
        s[WORKED] / 1000.0 / frames,
        s[WAITED] / 1000.0 / frames,
        (s[WORKED] - s[WAITED]).max(0.0) / 1000.0 / frames
    );
}

/// For the frame-rate line: the thread of stage A's own asking beside its planning (`VICTORY_STAGE_A_ASK_ASIDE`).
pub(super) fn front_line(frames: f64) {
    // (Read, not taken: [`line`] has the others.)
    let s = |n: usize| STATS[n].swap(0, Ordering::Relaxed) as f64;
    let (offered, worked, waited) = (s(FRONT_OFFERED), s(FRONT_WORKED), s(FRONT_WAITED));
    if !front_wanted() && offered == 0.0 {
        return;
    }
    tracing::info!(
        "stage A, the write watch asked beside its planning, per frame (VICTORY_STAGE_A_ASK_ASIDE): {:.2} offered; the thread worked {:.1} ms, the thread of stage A waited {:.1} ms for it (so {:.1} ms of the asking went beside the planning)",
        offered / frames,
        worked / 1000.0 / frames,
        waited / 1000.0 / frames,
        (worked - waited).max(0.0) / 1000.0 / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The two askers (`LANE` and `FRONT`) are one pair of threads for the whole process, and the tests that offer
    /// to them look at their state after the holders are dropped: tests that run side by side offer to the same
    /// threads and see each other's work. The tests that do take this lock for as long as they run.
    static ASKERS: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn askers() -> std::sync::MutexGuard<'static, ()> {
        ASKERS.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// A submission that stage A prepared is never offered, whatever else says yes; one that is not follows the switches.
    #[test]
    fn a_submission_stage_a_prepared_does_not_offer_its_asking() {
        let never = || -> bool { panic!("asked of flights for a submission that is not offered") };
        assert!(!wanted(true, true, never), "staged");
        assert!(!wanted(false, false, never), "not sent ahead");
        assert!(!wanted(true, true, || true));
        assert_eq!(wanted(true, false, || true), on() && crate::watch::once() && !ask_tokens());
        assert!(!wanted(true, false, || false), "nothing in the air");
    }

    /// The thread of stage A asks on a thread of its own: while the lane's asking is offered, the other can be offered too (a
    /// single one would refuse it), and each is over when its holder has waited.
    #[test]
    fn the_thread_of_stage_a_has_an_asking_thread_of_its_own() {
        let _one = askers();
        let mut plain = vec![0u8; 4096];
        let at = plain.as_mut_ptr() as usize;
        let mut both = 0;
        for _ in 0..30 {
            let lane = begin(vec![(at, at + 4096)]);
            let front = begin_front(vec![(at, at + 2048)]);
            // (Each is idle again when the loop comes back, so each is offered, if the thread is alive.)
            assert_eq!(front.is_some(), FRONT.alive.load(Ordering::Acquire), "the thread of stage A's own is free");
            both += usize::from(lane.is_some() && front.is_some());
            drop(front);
            drop(lane);
            let state = lock();
            assert!(state.work.is_none() && !state.busy);
            drop(state);
            let state = FRONT.lock();
            assert!(state.work.is_none() && !state.busy);
        }
        assert!(both > 0 || !FRONT.alive.load(Ordering::Acquire) || !LANE.alive.load(Ordering::Acquire));
    }

    /// What is offered is made before the holder's drop returns, and a second is refused while the first is not over.
    #[test]
    fn what_is_offered_is_asked_before_the_wait_is_over() {
        let _one = askers();
        // Nothing of ours is in the memory: ranges that are not write-watched are left alone, the asking just ends.
        let mut plain = vec![0u8; 4096];
        let at = plain.as_mut_ptr() as usize;
        let mut made = 0;
        for _ in 0..20 {
            if let Some(offered) = begin(vec![(at, at + 4096), (at, at + 2048)]) {
                made += 1;
                // Offered again at once: refused, or taken if the thread was fast enough.
                let second = begin(vec![(at, at + 100)]);
                offered.join();
                drop(second);
                let state = lock();
                assert!(state.work.is_none() && !state.busy);
            } else {
                std::thread::yield_now();
            }
        }
        assert!(
            made > 0 || !LANE.alive.load(Ordering::Acquire),
            "a thread that is alive takes what is offered when it is idle"
        );
    }
}
