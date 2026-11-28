//! The whole preparing of the next submission, begun while this one is sent
//! (`VICTORY_STAGE_A=1` with `VICTORY_STAGE_A_STEPS=1`; off by default): stage A of
//! a two-stage prepare.
//!
//! A submission's preparing is two stages. Stage A reads title memory and writes host
//! copies and title memory: the write watch asked, the title's bytes under flights'
//! writes, the copies refreshed, the steps (title buffer copies, fills, places,
//! declarations of what shaders write). Stage B is what the card sees: the snapshots,
//! the declarations, `card_before` (the take, the list that brings the card's copies up
//! to date), the heaps' records, the send, the flight in the line, the landing. The
//! thread of submissions did both for one submission before it began the next. Here the
//! thread `victory-front` does stage A of the next submission from the moment the one
//! before has taken what goes up to the card (and has put its places into the copies),
//! while the thread of submissions sends it; the thread of submissions then goes on with
//! the next at its snapshots ([`Host::submit`] takes what was done, [`take`]).
//!
//! What makes it safe, each a test or an argument:
//!
//! 1. **Order of the writes into title memory and the take.** The steps of the next
//!    submission write title memory and mark host copies for the next take
//!    (`Step::Copy`, `send_copied`). They begin only after the take of the one before
//!    (the offer is made from `card_before`'s callback, or after the places are in the
//!    copies when it has some), so no copy of the one before's sends holds a byte of
//!    the next one's. The take of the next submission is its own, later.
//! 2. **It is the next submission and nothing else.** The entry offered is the first
//!    of the whole line and is a submission of the queue being done; it has taken along
//!    what it can ([`crate::queue::prefold_next`]) and nothing more is taken into it
//!    (`prepared` in the line's entry), so the lists the thread prepared are the ones the thread of
//!    submissions runs, in the same order and parts ([`take`] looks, and stops the
//!    process if they are not: nothing done by the steps can be taken back). The line's
//!    order is the order of the steps: no submission of another queue goes between.
//! 3. **No waiting, no sweep, nothing of the lane's.** Anything that would make the
//!    thread of submissions wait (flights that write what is read, title bytes under
//!    flights' writes that cannot be put in beside them), plan steps for the card
//!    (`card_steps`), cut the submission (`cut_for`), sweep or take the submission lock
//!    declines the offer before any step is done: the thread of submissions does the
//!    whole stage A then, as before. The thread does not hold the lock of submissions,
//!    gives nothing to the queue, signals nothing and takes no `QUEUE_ORDER`: fences,
//!    signals and the order of the queue are the thread of submissions' alone.
//! 4. **What the title's steps would do on the other thread.** A step of ours that runs
//!    closures (`Step::Run`, `Step::Host`, `Step::Plane`) touches state of other
//!    threads' (descriptors, the heaps' round): such a submission is declined. What is
//!    left is refreshes, title buffer copies, fills, places, declarations and texture
//!    refreshes, which are what the thread of submissions did.
//! 5. **Flights.** What the one before declared written is in the line (`PendingFlight`,
//!    `VICTORY_PENDING_FLIGHT=1`, which this needs) before the offer, so what the
//!    thread asks of flights ([`late_blocked`], [`stale_under_flights`], `air`) includes
//!    it; flights only land from then on (what is written only shrinks), and the thread
//!    of submissions finds the same or less when it takes what was done.
//! 6. **Numbers.** The thread takes the submission's number (`STARTED`) before its
//!    steps; threads that would take one between (a submission that is waited for,
//!    `Host::execute`) wait until it is taken ([`quiesce`]), so numbers stay in the
//!    order of the lists.
//!
//! What is not done on the thread: the snapshots, the declarations of picked ranges, the
//! take (they are stage B), and the submissions that wait for flights.

use super::*;

/// Whether stage A of the next submission is done by the thread
/// ([`crate::queue::stage_front`]; it needs the flights' pending registration, and the one asking).
///
/// (May change while the title runs, `VICTORY_AB`: [`offer`] reads it once; [`join`] and [`take`] look at what
/// there is whatever it says.)
pub(super) fn on() -> bool {
    prefetch::on() && crate::queue::stage_front() && pending_flights() && crate::watch::once()
}

/// What was done: the lists it was done for (addresses, in order), where their parts begin, the number the
/// submission got, and the title buffer copies placed in the copies' references (done after the take).
pub(super) struct Front {
    lists: Vec<usize>,
    parts: Vec<usize>,
    number: u64,
    placed: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, Vec<u8>)>,
}

/// What the thread is asked: the lists (addresses, in order) and where their parts begin.
struct Asked {
    lists: Vec<usize>,
    parts: Vec<usize>,
}

static GATE: prefetch::Gate<Asked> = prefetch::Gate::new();

/// What the thread did, for [`take`].
static DONE: Mutex<Option<Front>> = Mutex::new(None);

/// Whether anything was ever offered: from then on [`join`] looks at the gate whatever the switch says.
static OFFERED_EVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Threads that are not the thread of submissions wait here while stage A of a submission is asked for and not
/// taken ([`quiesce`]).
struct Hold {
    held: Mutex<bool>,
    changed: std::sync::Condvar,
}

impl Hold {
    const fn new() -> Hold {
        Hold { held: Mutex::new(false), changed: std::sync::Condvar::new() }
    }

    fn set(&self, held: bool) {
        *self.held.lock().unwrap_or_else(|e| e.into_inner()) = held;
        self.changed.notify_all();
    }

    /// Waits until it is not held, at most `most`. Whether it was held at all.
    fn wait(&self, most: std::time::Duration) -> bool {
        let began = std::time::Instant::now();
        let mut held = self.held.lock().unwrap_or_else(|e| e.into_inner());
        let waited = *held;
        while *held {
            let left = most.saturating_sub(began.elapsed());
            if left.is_zero() {
                break;
            }
            held = self.changed.wait_timeout(held, left).unwrap_or_else(|e| e.into_inner()).0;
        }
        waited
    }
}

static HOLD: Hold = Hold::new();

thread_local! {
    /// This is the thread of submissions (it asked for stage A of a submission).
    static LANE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Since the last frame-rate line: offers, submissions done, those declined (and why), the microseconds the thread
/// worked, the thread of submissions waited at [`join`], and the submissions it took the work of.
static STATS: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];
const OFFERS: usize = 0;
const DONE_OK: usize = 1;
const DECLINED: usize = 2;
const WORKED: usize = 3;
const WAITED: usize = 4;
const TAKEN: usize = 5;
static WHY: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

fn declined(why: &'static str) {
    STATS[DECLINED].fetch_add(1, Ordering::Relaxed);
    let mut all = WHY.lock().unwrap_or_else(|e| e.into_inner());
    match all.iter_mut().find(|w| w.0 == why) {
        Some(w) => w.1 += 1,
        None => all.push((why, 1)),
    }
}

/// Whether the steps of `lists` are of the kinds the thread may do ([`self`]'s fourth point).
fn steps_ok<'a>(steps: impl Iterator<Item = &'a Step>) -> Result<(), &'static str> {
    for step in steps {
        match step {
            Step::Run(_) => return Err("a step of ours that may write anywhere"),
            Step::Host(..) | Step::Plane(..) => return Err("a step of ours that works on other threads' state"),
            Step::Copy(..) | Step::Fill(..) | Step::Refresh(..) | Step::GpuWrite(..) | Step::Texture(_) | Step::Placed(_) => {}
        }
    }
    Ok(())
}

/// Whether what `front` was prepared for is the lists at `runs` (addresses), in order, with the same parts.
fn same(front: &Front, runs: &[usize], parts: &[usize]) -> bool {
    front.parts == parts && front.lists == runs
}

/// After the submission being prepared has taken what goes up to the card and put its places into the copies:
/// asks the thread to do stage A of the submission that comes next. Whether it was asked (the thread of submissions
/// then takes it at its turn: [`take`]).
pub(super) fn offer() -> bool {
    if !on() {
        return false;
    }
    let Some((_, lists, parts)) = crate::queue::prefold_next() else {
        return false;
    };
    if lists.is_empty() {
        return false;
    }
    start();
    OFFERED_EVER.store(true, Ordering::Release);
    LANE.with(|l| l.set(true));
    HOLD.set(true);
    match GATE.offer(Asked { lists, parts }) {
        Ok(()) => {
            STATS[OFFERS].fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(_) => {
            HOLD.set(false);
            false
        }
    }
}

/// Before anything of a submission that was asked for is done on the thread of submissions, or a decision about it
/// (`Host::execute_parts`, the top of `Host::submit`): the thread is done with it.
pub(super) fn join() {
    if !OFFERED_EVER.load(Ordering::Acquire) {
        return;
    }
    let began = std::time::Instant::now();
    if GATE.join() {
        STATS[WAITED].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// At the top of a submission sent ahead, after [`join`]: what the thread did for it, if it did. Stops the process
/// if it was done for other lists than these: what its steps did cannot be taken back.
pub(super) fn take(runs: &[&HostList], parts: &[usize]) -> Option<Front> {
    if !OFFERED_EVER.load(Ordering::Acquire) {
        return None;
    }
    let front = DONE.lock().unwrap_or_else(|e| e.into_inner()).take();
    HOLD.set(false);
    let front = front?;
    let at: Vec<usize> = runs.iter().map(|&run| std::ptr::from_ref::<HostList>(run) as usize).collect();
    if !same(&front, &at, parts) {
        fail(format_args!(
            "stage A prepared {} lists in {} parts and the submission to run has {} in {} (their steps are done and cannot be undone)",
            front.lists.len(),
            front.parts.len() + 1,
            runs.len(),
            parts.len() + 1
        ));
    }
    STATS[TAKEN].fetch_add(1, Ordering::Relaxed);
    Some(front)
}

impl Front {
    pub(super) fn number(&self) -> u64 {
        self.number
    }

    pub(super) fn into_placed(self) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, Vec<u8>)> {
        self.placed
    }
}

/// Before a submission that is waited for takes its number and the submission lock: the threads of the title that
/// make one (`Host::execute`, an internal upload list) wait until stage A of the next submission of the line has
/// taken its number and been taken, so that numbers stay in the order of the lists. (At most 200 ms: nothing is waited
/// for forever.)
pub(super) fn quiesce() {
    if LANE.with(|l| l.get()) && crate::queue::line_on() {
        // (The thread of submissions does not wait for itself.)
        return;
    }
    if OFFERED_EVER.load(Ordering::Acquire) {
        HOLD.wait(std::time::Duration::from_millis(200));
    }
}

fn start() {
    static STARTED_ONCE: std::sync::Once = std::sync::Once::new();
    STARTED_ONCE.call_once(|| {
        let made = std::thread::Builder::new().name("victory-front".into()).spawn(|| {
            crate::cores::keep_on_fast_cores("victory-front");
            crate::cores::prioritize("victory-front", crate::cores::Role::Helper);
            loop {
                let asked = GATE.take();
                let began = std::time::Instant::now();
                let _span = crate::timeline::span("stage A of the next submission");
                let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| match host() {
                    Some(host) => host.front(&asked),
                    None => Err("no host"),
                }));
                match result {
                    Ok(Ok(front)) => {
                        *DONE.lock().unwrap_or_else(|e| e.into_inner()) = Some(front);
                        STATS[DONE_OK].fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Err(why)) => {
                        declined(why);
                        HOLD.set(false);
                        // The refreshes the offer after the take would have begun (the thread of submissions does the
                        // rest, as before).
                        // SAFETY: as in `Host::front`: the lists are the first of the line, held until `join`.
                        let spans = unsafe { prefetch::spans_of(&asked.lists, &[]) };
                        let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| prefetch::run(spans)));
                    }
                    // (Steps may have been done: nothing can be undone.)
                    Err(_) => fail(format_args!("stage A of a submission panicked")),
                }
                STATS[WORKED].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                GATE.done();
            }
        });
        if let Err(e) = made {
            tracing::warn!(error = %e, "no thread for stage A: the submissions are prepared by the thread that sends them");
        }
    });
}

impl Host {
    /// Stage A of the submission whose lists are `asked`, on the thread of stage A: the same as the first half of
    /// [`Host::submit`] makes of a submission that waits for nothing ([`self`]'s third and fourth points), or a
    /// reason why it is not done, given before any step is.
    fn front(&self, asked: &Asked) -> Result<Front, &'static str> {
        // SAFETY: the entry is the first of the line and nothing takes it, or changes what it holds, before the
        // thread of submissions has called [`join`] (`Host::execute_parts` begins with it): the host lists do not move.
        let runs: Vec<&HostList> = asked.lists.iter().map(|&at| unsafe { &*(at as *const HostList) }).collect();
        for list in &runs {
            steps_ok(list.deferred().copies.iter())?;
        }
        if !late() || !relay_exact() || !pending_flights() {
            return Err("the relay is not on as this needs");
        }
        if !refresh_front() {
            return Err("refreshes are not made ahead of the steps");
        }
        if split_blocked() != 0 {
            return Err("submissions are cut where they wait");
        }
        if upcheck::on() {
            return Err("the check of copies runs between submissions");
        }
        // Behind flights, as the thread of submissions would find it (no sweep, no settle).
        if !flying() {
            return Err("nothing in the air: the thread of submissions sweeps");
        }
        if late_free_present() {
            return Err("the present's own sweep");
        }
        if card_steps::cut_for(&runs).is_some() {
            return Err("the submission is cut for places");
        }
        {
            let written = line().written.clone();
            if blocked_by(&written, &runs, false).is_some() || late_blocked_counted(&runs, false).is_some() {
                return Err("it waits for flights it reads from");
            }
        }
        let mut batch = Batch::new(lean_steps());
        let ranges = ask_spans(&runs, batch.lean);
        let mut asking: Option<crate::watch::Standing> = None;
        ask_for_submission(&ranges, &mut asking);
        let end = |asking: &mut Option<crate::watch::Standing>| match asking.take() {
            Some(standing) => crate::watch::end_standing(standing),
            None => crate::watch::ask_once_end(),
        };
        // What the title wrote under flights' writes that cannot be put in beside them waits for them (the thread
        // of submissions settles): not here.
        if !stale_under_flights(&runs).is_empty() {
            end(&mut asking);
            return Err("title bytes under flights' writes wait for them");
        }
        // Nothing is declined from here on: the submission has its number, and its steps are done.
        let number = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        crate::shadow::preparing(number);
        let air = line().written.clone();
        batch.refresh_front(&runs, &asked.parts);
        for list in &runs {
            list.run_deferred(&mut batch, &air, |_| false);
        }
        batch.fill();
        batch.flush();
        end(&mut asking);
        Ok(Front { lists: asked.lists.clone(), parts: asked.parts.clone(), number, placed: std::mem::take(&mut batch.placed) })
    }
}

/// For the frame-rate line.
pub(super) fn frame_line(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64).collect();
    let why: Vec<(&'static str, u64)> = std::mem::take(&mut *WHY.lock().unwrap_or_else(|e| e.into_inner()));
    if !crate::queue::stage_front() {
        return;
    }
    let mut why = why;
    why.sort_by_key(|w| std::cmp::Reverse(w.1));
    let why: Vec<String> = why.iter().map(|(text, n)| format!("{:.2} {text}", *n as f64 / frames)).collect();
    tracing::info!(
        "stage A of the next submission beside the sending of this one (VICTORY_STAGE_A_STEPS), per frame: {:.1} asked for, {:.1} done, {:.1} taken by the \
         thread of submissions, {:.1} declined [{}]; the thread worked {:.1} ms, the thread of submissions waited {:.1} ms for it",
        s[OFFERS] / frames,
        s[DONE_OK] / frames,
        s[TAKEN] / frames,
        s[DECLINED] / frames,
        why.join("; "),
        s[WORKED] / 1000.0 / frames,
        s[WAITED] / 1000.0 / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_that_run_closures_decline_the_submission() {
        let clean = [Step::Copy(8, 16, 4), Step::Fill(1, 2, 3, None)];
        assert_eq!(steps_ok(clean.iter()), Ok(()));
        assert!(steps_ok([Step::Run(Box::new(|| {}))].iter()).is_err());
        assert!(steps_ok([Step::Host(Box::new(|| {}), (0, 0))].iter()).is_err());
        assert!(steps_ok([Step::Plane((0, 4), Box::new(|| {}))].iter()).is_err());
    }

    /// The lists the thread prepared must be those the thread of submissions runs, in the same order and parts.
    #[test]
    fn what_was_prepared_is_for_the_same_lists_in_the_same_parts() {
        let front = Front { lists: vec![0x10, 0x20, 0x30], parts: vec![2], number: 5, placed: Vec::new() };
        let (a, b, c) = (0x10usize, 0x20usize, 0x30usize);
        assert!(same(&front, &[a, b, c], &[2]));
        assert!(!same(&front, &[a, c, b], &[2]), "another order");
        assert!(!same(&front, &[a, b], &[2]), "fewer lists");
        assert!(!same(&front, &[a, b, c], &[1]), "another part");
        assert!(!same(&front, &[a, b, c], &[]), "no part");
    }

    /// Threads that wait for their number wait while stage A is asked for and not taken, at most the time given, and
    /// go on at once when it is not.
    #[test]
    fn threads_that_take_a_number_wait_for_stage_a_to_be_taken() {
        let hold = std::sync::Arc::new(Hold::new());
        assert!(!hold.wait(std::time::Duration::from_millis(500)), "nothing held: no wait");
        hold.set(true);
        let began = std::time::Instant::now();
        assert!(hold.wait(std::time::Duration::from_millis(30)), "held: it waited");
        assert!(began.elapsed() >= std::time::Duration::from_millis(25), "until the time given");
        let waiter = {
            let hold = hold.clone();
            std::thread::spawn(move || {
                let began = std::time::Instant::now();
                hold.wait(std::time::Duration::from_secs(10));
                began.elapsed()
            })
        };
        std::thread::sleep(std::time::Duration::from_millis(40));
        hold.set(false);
        assert!(waiter.join().unwrap() < std::time::Duration::from_secs(5), "released when taken");
    }
}
