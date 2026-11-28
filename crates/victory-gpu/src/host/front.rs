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
//!    it, and so are the destinations of the copies it does on the card
//!    (`Line::pending_copies`: until it is a flight in the line, the thread would refresh
//!    another host copy of them from title memory the copy has not reached yet);
//!    flights only land from then on (what is written only shrinks), and the thread
//!    of submissions finds the same or less when it takes what was done.
//! 6. **Numbers.** The thread takes the submission's number (`STARTED`) before its
//!    steps; threads that would take one between (a submission that is waited for,
//!    `Host::execute`) wait until it is taken ([`lock_quiet`]: decided under the submission
//!    lock, which is where the thread of submissions takes and offers), so numbers stay in
//!    the order of the lists.
//! 7. **A submission that is cut is not offered.** While the first half of a submission cut in two is
//!    sent, the line's first entry is not the next one (the second half is on the stack): nothing is
//!    offered then ([`split_pending`]), or the lists the thread prepared would not be those that come next.
//!
//! 8. **What more it may do (`VICTORY_STAGE_A_RELAX`, [`relax`]).** Point 4 is the default: steps that run closures
//!    decline the submission. Each kind of `Host` step says what it is ([`relax::Kind`]) and what it does to what
//!    others read; the ones that only note something for later (with the submission's number, which the thread takes
//!    before its steps), read title memory into the list's own scratch, or fill a movie plane, are let through by a
//!    bit each; the ones that call the host queue or write what the send of the submission before reads, and
//!    `Step::Run`, never are. Bits 8 and 16 make the thread wait, at most 150 ms in all and before any step, for the flights
//!    the submission reads from or whose declared bytes it must refresh, where the thread of submissions would have
//!    planned the reads for the card or waited: a wait that is not over gives the submission back, as before. Every
//!    decline names what it needs ([`Declined`]), and the frame line counts by need and by kind of step.
//! 9. **The entry after the next (`VICTORY_STAGE_A_DEEP`).** With the next submission's number taken the thread offers
//!    the refreshes of the entry after it (named by the lane when it offered the next: [`crate::queue::deeper_next`])
//!    to the thread of the refreshes, for the copies the next submission does nothing to (`prefetch::offer_deep`).
//!    The lane joins this thread first and the other second, before it takes anything of the next submission.
//!
//! What is not done on the thread: the snapshots, the declarations of picked ranges, the
//! take (they are stage B), and the submissions that wait for flights unless a bit says wait.

use super::*;

/// Whether stage A of the next submission is done by the thread
/// ([`crate::queue::stage_front`]; it needs the flights' pending registration, and the one asking).
///
/// (May change while the title runs, `VICTORY_AB`: [`offer`] reads it once; [`join`] and [`take`] look at what
/// there is whatever it says.)
pub(super) fn on() -> bool {
    // (Not with the compute lane on a queue of its own, `lanes`: the thread would prepare the card's steps before it is
    // known which queue they are for.)
    !lanes::lists_compute() && prefetch::on() && crate::queue::stage_front() && pending_flights() && crate::watch::once()
}

/// What was done: the lists it was done for (addresses, in order), where their parts begin, the number the
/// submission got, and the title buffer copies placed in the copies' references (done after the take).
pub(super) struct Front {
    lists: Vec<usize>,
    parts: Vec<usize>,
    number: u64,
    placed: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, Vec<u8>)>,
}

/// What the thread is asked: the lists (addresses, in order) and where their parts begin, and with
/// `VICTORY_STAGE_A_DEEP` the lists of the entry after this one, if it is a submission of the same queue
/// whose lists are known.
struct Asked {
    lists: Vec<usize>,
    parts: Vec<usize>,
    deeper: Option<Vec<usize>>,
}

static GATE: prefetch::Gate<Asked> = prefetch::Gate::new();

/// What the thread did, for [`take`].
static DONE: Mutex<Option<Front>> = Mutex::new(None);

/// Whether anything was ever offered: from then on [`join`] looks at the gate whatever the switch says.
static OFFERED_EVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Threads that are not the thread of submissions wait here while stage A of a submission is asked for and not
/// taken ([`lock_quiet`]).
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

impl Hold {
    /// Whether stage A of a submission is asked for and not taken.
    fn is_held(&self) -> bool {
        *self.held.lock().unwrap_or_else(|e| e.into_inner())
    }
}

thread_local! {
    /// On the thread of submissions: how many halves of cut submissions are being sent (the second half waits on the stack).
    static SPLITS: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
}

/// While it lives, the thread of submissions sends the first half of a submission that was cut (`Host::execute_parts`):
/// [`offer`] asks for nothing, as the line's first entry is not the submission that comes next after this half (the
/// second half is), and the thread's work for another would be stopped for by [`take`] after its steps were done.
pub(super) struct SplitPending(());

pub(super) fn split_pending() -> SplitPending {
    SPLITS.with(|s| s.set(s.get() + 1));
    SplitPending(())
}

impl Drop for SplitPending {
    fn drop(&mut self) {
        SPLITS.with(|s| s.set(s.get().saturating_sub(1)));
    }
}

/// Whether the half of a cut submission is being sent on this thread.
fn split_pending_here() -> bool {
    SPLITS.with(|s| s.get() > 0)
}

thread_local! {
    /// This is the thread of submissions (it asked for stage A of a submission).
    static LANE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Since the last frame-rate line: offers, submissions done, those declined (and why), the microseconds the thread
/// worked, the thread of submissions waited at [`join`], and the submissions it took the work of.
static STATS: [std::sync::atomic::AtomicU64; 12] = [const { std::sync::atomic::AtomicU64::new(0) }; 12];
const OFFERS: usize = 0;
const DONE_OK: usize = 1;
const DECLINED: usize = 2;
const WORKED: usize = 3;
const WAITED: usize = 4;
const TAKEN: usize = 5;
/// Waits for flights on the thread (`VICTORY_STAGE_A_RELAX` bits 8 and 16): done, given up on, the microseconds
/// waited, and the offers that had steps the bits let through (counted done or not).
const WAITS_OK: usize = 6;
const WAITS_GAVE_UP: usize = 7;
const WAITS_US: usize = 8;
const RELAXED: usize = 9;
/// Offers given to the thread with the entry after the next named (`VICTORY_STAGE_A_DEEP`), and those that named none.
const DEEP_ASKED: usize = 10;
const DEEP_NONE: usize = 11;
static WHY: Mutex<Vec<(String, u64)>> = Mutex::new(Vec::new());
/// Declined offers by what they would have needed of `VICTORY_STAGE_A_RELAX` (bits) and by kind of step.
static NEEDS: Mutex<Vec<(u32, u64)>> = Mutex::new(Vec::new());
static KINDS: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

/// Why the thread gave a submission back before any step was done, and what of `VICTORY_STAGE_A_RELAX` it would have
/// needed besides (what was found out before the decision: not all of it is always looked at).
pub(super) struct Declined {
    why: String,
    need: u32,
    kinds: Vec<&'static str>,
}

impl From<&'static str> for Declined {
    fn from(why: &'static str) -> Declined {
        Declined { why: why.to_string(), need: 0, kinds: Vec::new() }
    }
}

fn declined(why: Declined) {
    STATS[DECLINED].fetch_add(1, Ordering::Relaxed);
    {
        let mut all = WHY.lock().unwrap_or_else(|e| e.into_inner());
        match all.iter_mut().find(|w| w.0 == why.why) {
            Some(w) => w.1 += 1,
            None => all.push((why.why, 1)),
        }
    }
    if why.need != 0 {
        let mut all = NEEDS.lock().unwrap_or_else(|e| e.into_inner());
        match all.iter_mut().find(|n| n.0 == why.need) {
            Some(n) => n.1 += 1,
            None => all.push((why.need, 1)),
        }
    }
    if !why.kinds.is_empty() {
        let mut all = KINDS.lock().unwrap_or_else(|e| e.into_inner());
        for kind in why.kinds {
            match all.iter_mut().find(|k| k.0 == kind) {
                Some(k) => k.1 += 1,
                None => all.push((kind, 1)),
            }
        }
    }
}

/// Whether what `front` was prepared for is the lists at `runs` (addresses), in order, with the same parts.
fn same(front: &Front, runs: &[usize], parts: &[usize]) -> bool {
    same_lists(&front.lists, &front.parts, runs, parts)
}

/// [`same`] for the lists and parts themselves.
pub(super) fn same_lists(prepared: &[usize], prepared_parts: &[usize], runs: &[usize], parts: &[usize]) -> bool {
    prepared_parts == parts && prepared == runs
}

/// After the submission being prepared has taken what goes up to the card and put its places into the copies:
/// asks the thread to do stage A of the submission that comes next. Whether it was asked (the thread of submissions
/// then takes it at its turn: [`take`]).
pub(super) fn offer() -> bool {
    if !on() || split_pending_here() {
        return false;
    }
    let Some((_, lists, parts)) = crate::queue::prefold_next() else {
        return false;
    };
    if lists.is_empty() {
        return false;
    }
    // (The entry after the next, named now: when the thread runs, the line may have moved on.)
    let deeper = if relax::deep() { crate::queue::deeper_next() } else { None };
    if relax::deep() {
        STATS[if deeper.is_some() { DEEP_ASKED } else { DEEP_NONE }].fetch_add(1, Ordering::Relaxed);
    }
    start();
    OFFERED_EVER.store(true, Ordering::Release);
    LANE.with(|l| l.set(true));
    HOLD.set(true);
    match GATE.offer(Asked { lists, parts, deeper }) {
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

/// The submission lock for a submission that is waited for and takes its number under it: the threads of the title
/// that make one (`Host::execute`, an internal upload list) get it only when stage A of the next submission of the
/// line is not asked for, so that numbers stay in the order of the lists. (At most 200 ms: nothing is waited for
/// forever.) `lock` takes the submission lock.
///
/// Whether stage A is asked for is decided with the lock held: the thread of submissions takes a submission and offers
/// the next under it, and a thread that looked before the lock could see "not asked for" in between and get the
/// lock after the offer (rev2). With stage A asked for, the lock is let go while it is waited for, or the thread of
/// submissions could not take what was done.
pub(super) fn lock_quiet<G>(lock: impl FnMut() -> G) -> G {
    // (The thread of submissions does not wait for itself.)
    let exempt = (LANE.with(|l| l.get()) && crate::queue::line_on()) || !OFFERED_EVER.load(Ordering::Acquire);
    acquire(&HOLD, exempt, std::time::Duration::from_millis(200), lock)
}

fn acquire<G>(hold: &Hold, exempt: bool, most: std::time::Duration, mut lock: impl FnMut() -> G) -> G {
    let began = std::time::Instant::now();
    loop {
        let guard = lock();
        if exempt || !hold.is_held() {
            return guard;
        }
        let left = most.saturating_sub(began.elapsed());
        if left.is_zero() {
            return guard;
        }
        drop(guard);
        hold.wait(left);
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
                    None => Err("no host".into()),
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
    fn front(&self, asked: &Asked) -> Result<Front, Declined> {
        // SAFETY: the entry is the first of the line and nothing takes it, or changes what it holds, before the
        // thread of submissions has called [`join`] (`Host::execute_parts` begins with it): the host lists do not move.
        let runs: Vec<&HostList> = asked.lists.iter().map(|&at| unsafe { &*(at as *const HostList) }).collect();
        // What it may do beyond the first version: read once, and what is decided is for this submission alone.
        let bits = relax::bits();
        let mut seen = relax::Seen::default();
        for list in &runs {
            seen.steps(list.deferred().copies.iter());
        }
        if let Some(why) = seen.hard {
            return Err(Declined { why: format!("a step that no bit lets through: {why}"), need: 0, kinds: Vec::new() });
        }
        if !late() || !relay_exact() || !pending_flights() {
            return Err("the relay is not on as this needs".into());
        }
        if !refresh_front() {
            return Err("refreshes are not made ahead of the steps".into());
        }
        if split_blocked() != 0 {
            return Err("submissions are cut where they wait".into());
        }
        if upcheck::on() {
            return Err("the check of copies runs between submissions".into());
        }
        // Behind flights, as the thread of submissions would find it (no sweep, no settle).
        if !flying() {
            return Err("nothing in the air: the thread of submissions sweeps".into());
        }
        if late_free_present() {
            return Err("the present's own sweep".into());
        }
        if card_steps::cut_for(&runs).is_some() {
            return Err("the submission is cut for places".into());
        }
        // Flights that write what it reads: the thread of submissions plans the reads for the card or waits for them.
        let mut need = seen.need;
        let mut waits: Option<&'static str> = None;
        {
            let (written, copies) = {
                let line = line();
                (line.written.clone(), card_steps::copies(&line))
            };
            if !written.is_empty() && (blocked_by(&written, &runs, false).is_some() || card_steps::copied_read(&copies, &runs)) {
                waits = Some(relax::what_waits(&written, &copies, &runs));
                need |= relax::WAIT_READS;
            }
        }
        // (All its waits for flights together are bounded: the threads that wait for a number are held back for 200 ms.)
        let waiting = std::time::Instant::now();
        let left = || relax::TOTAL_WAIT.saturating_sub(waiting.elapsed());
        let missing = need & !bits;
        if missing != 0 {
            let why = match waits {
                Some(what) if missing == relax::WAIT_READS => format!("it waits for flights it reads from: {what}"),
                _ => {
                    let what = waits.map(|w| format!(" (flights: {w})")).unwrap_or_default();
                    format!("it needs these of VICTORY_STAGE_A_RELAX: {}{what}", relax::describe(missing))
                }
            };
            return Err(Declined { why, need, kinds: seen.kinds });
        }
        if need & !relax::WAIT_READS != 0 {
            STATS[RELAXED].fetch_add(1, Ordering::Relaxed);
        }
        if waits.is_some() {
            // Landed, as `relay_wait` for the reads does in the thread of submissions (it takes no lock of ours).
            let began = std::time::Instant::now();
            let landed = relax::wait_flights(
                |line| blocked_by(&line.written, &runs, false).is_none() && !card_steps::copied_read(&card_steps::copies(line), &runs),
                left(),
            );
            STATS[WAITS_US].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
            if !landed {
                STATS[WAITS_GAVE_UP].fetch_add(1, Ordering::Relaxed);
                return Err(Declined { why: "flights it reads from did not land in time".into(), need, kinds: seen.kinds });
            }
            STATS[WAITS_OK].fetch_add(1, Ordering::Relaxed);
            // (The line moved while it waited.)
            if !flying() {
                return Err("nothing in the air after the wait: the thread of submissions sweeps".into());
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
        // What the title wrote under flights' writes that cannot be put in beside them waits for them: the thread of
        // submissions settles, or with bit 16 the thread waits for the flights that declared them, as `settle_stale`
        // does (three times at most: what is left after that waits as before).
        let mut stale = stale_under_flights(&runs);
        if !stale.is_empty() {
            need |= relax::WAIT_STALE;
            if bits & relax::WAIT_STALE == 0 || card_steps::stage() == 0 {
                end(&mut asking);
                let why = if bits & relax::WAIT_STALE == 0 {
                    "title bytes under flights' writes wait for them (VICTORY_STAGE_A_RELAX bit 16)"
                } else {
                    "title bytes under flights' writes wait for them (the card's steps are off)"
                };
                return Err(Declined { why: why.into(), need, kinds: seen.kinds });
            }
            for _ in 0..3 {
                let began = std::time::Instant::now();
                let landed = relax::wait_flights(
                    |_| stale.iter().all(|(shadow, from, to)| !shadow.writes_pending_in(*from, *to)),
                    left(),
                );
                STATS[WAITS_US].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                if !landed {
                    break;
                }
                // What they landed is title memory written: asked and looked at again.
                ask_for_submission(&ranges, &mut asking);
                stale = stale_under_flights(&runs);
                if stale.is_empty() {
                    break;
                }
            }
            if !stale.is_empty() {
                STATS[WAITS_GAVE_UP].fetch_add(1, Ordering::Relaxed);
                end(&mut asking);
                return Err(Declined { why: "title bytes under flights' writes were still there after waiting".into(), need, kinds: seen.kinds });
            }
            STATS[WAITS_OK].fetch_add(1, Ordering::Relaxed);
        }
        // Nothing is declined from here on: the submission has its number, and its steps are done.
        let number = STARTED.fetch_add(1, Ordering::AcqRel) + 1;
        crate::shadow::preparing(number);
        // The entry after this one has the refreshes of the copies this submission does nothing to begun now, beside
        // this one's asking, refreshes and steps (`VICTORY_STAGE_A_DEEP`): the lane joins them before it asks, refreshes or
        // takes anything for this submission, and so no copy is in two stages' hands.
        if let Some(next) = &asked.deeper {
            prefetch::offer_deep(&runs, next);
        }
        let air = line().written.clone();
        batch.refresh_front(&runs, &asked.parts);
        for list in &runs {
            list.run_deferred(&mut batch, &air, |_| false, None);
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
    let why: Vec<(String, u64)> = std::mem::take(&mut *WHY.lock().unwrap_or_else(|e| e.into_inner()));
    let needs: Vec<(u32, u64)> = std::mem::take(&mut *NEEDS.lock().unwrap_or_else(|e| e.into_inner()));
    let kinds: Vec<(&'static str, u64)> = std::mem::take(&mut *KINDS.lock().unwrap_or_else(|e| e.into_inner()));
    if !crate::queue::stage_front() {
        return;
    }
    let mut why = why;
    why.sort_by_key(|w| std::cmp::Reverse(w.1));
    let why: Vec<String> = why.iter().map(|(text, n)| format!("{:.2} {text}", *n as f64 / frames)).collect();
    tracing::info!(
        "stage A of the next submission beside the sending of this one (VICTORY_STAGE_A_STEPS), per frame: {:.1} asked for, {:.1} done, {:.1} taken by the thread of submissions, {:.1} declined [{}]; the thread worked {:.1} ms, the thread of submissions waited {:.1} ms for it",
        s[OFFERS] / frames,
        s[DONE_OK] / frames,
        s[TAKEN] / frames,
        s[DECLINED] / frames,
        why.join("; "),
        s[WORKED] / 1000.0 / frames,
        s[WAITED] / 1000.0 / frames
    );
    // What the declines say about what to relax next: what each declined offer would have needed of
    // VICTORY_STAGE_A_RELAX (all of it that was looked at), and the kinds of step that need a bit.
    let mut needs = needs;
    needs.sort_by_key(|n| std::cmp::Reverse(n.1));
    let needs: Vec<String> = needs.iter().map(|(mask, n)| format!("{:.2} need [{}]", *n as f64 / frames, relax::describe(*mask))).collect();
    let mut kinds = kinds;
    kinds.sort_by_key(|k| std::cmp::Reverse(k.1));
    let kinds: Vec<String> = kinds.iter().map(|(name, n)| format!("{:.2} {name}", *n as f64 / frames)).collect();
    tracing::info!(
        "stage A, what to relax next (VICTORY_STAGE_A_RELAX={}, VICTORY_STAGE_A_DEEP={}), per frame: the declined offers by what they would need {{{}}}; by kind of step they hold {{{}}}; {:.2} done with steps the bits let through; waits for flights on the thread: {:.2} done, {:.2} given up, {:.1} ms; offers with the entry after the next named {:.2}, with none {:.2}",
        relax::bits(),
        u8::from(relax::deep()),
        needs.join("; "),
        kinds.join("; "),
        s[RELAXED] / frames,
        s[WAITS_OK] / frames,
        s[WAITS_GAVE_UP] / frames,
        s[WAITS_US] / 1000.0 / frames,
        s[DEEP_ASKED] / frames,
        s[DEEP_NONE] / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_that_run_closures_decline_the_submission() {
        let clean = [Step::Copy(8, 16, 4), Step::Fill(1, 2, 3, None)];
        let look = |steps: &[Step]| {
            let mut seen = relax::Seen::default();
            seen.steps(steps.iter());
            seen
        };
        assert!(look(&clean).hard.is_none() && look(&clean).need == 0);
        assert!(look(&[Step::Run(Box::new(|| {}))]).hard.is_some(), "a title copy that may write anywhere: never");
        // Host steps that note for later and plane fills are let through by a bit each, not by default.
        let host = Step::Host(Box::new(|| {}), (0, 0), relax::Kind::SlotFlip);
        assert_eq!(look(std::slice::from_ref(&host)).need, relax::BOOKKEEPING);
        assert_eq!(look(&[Step::Plane((0, 4), Box::new(|| {}))]).need, relax::PLANES);
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

    /// While the first half of a cut submission is sent nothing is offered, and the count nests.
    #[test]
    fn nothing_is_offered_while_the_first_half_of_a_cut_submission_is_sent() {
        assert!(!split_pending_here());
        let outer = split_pending();
        assert!(split_pending_here());
        {
            let _inner = split_pending();
            assert!(split_pending_here());
        }
        assert!(split_pending_here(), "the outer half is still being sent");
        // Another thread is not the thread of submissions.
        assert!(!std::thread::spawn(split_pending_here).join().unwrap());
        assert!(!offer(), "nothing offered");
        drop(outer);
        assert!(!split_pending_here());
    }

    /// A thread that looked at the hold before it had the lock may find stage A asked for once it has it (the thread of
    /// submissions offered between): it lets the lock go, waits, and does not return with the lock while stage A is asked
    /// for; nor does it wait beyond the time given.
    #[test]
    fn a_thread_that_waits_for_its_number_decides_under_the_lock() {
        let hold = std::sync::Arc::new(Hold::new());
        let lock = std::sync::Arc::new(Mutex::new(0u32));
        let ms = std::time::Duration::from_millis;
        // Not asked for: the lock, at once.
        drop(acquire(&hold, false, ms(500), || lock.lock().unwrap()));
        // Asked for, and taken while the thread waits: it returns with the lock after, never before. The lock can be
        // had by others (the thread of submissions) meanwhile.
        hold.set(true);
        let waiter = {
            let (hold, lock) = (hold.clone(), lock.clone());
            std::thread::spawn(move || {
                let guard = acquire(&hold, false, ms(10_000), || lock.lock().unwrap());
                let quiet = !hold.is_held();
                drop(guard);
                quiet
            })
        };
        std::thread::sleep(ms(60));
        assert!(lock.try_lock().is_ok(), "the waiting thread does not hold the lock");
        assert!(!waiter.is_finished(), "still waiting for stage A to be taken");
        hold.set(false);
        assert!(waiter.join().unwrap(), "stage A was not asked for when the lock was returned");
        // The thread of submissions is not held; nor is a thread that waited the time given.
        hold.set(true);
        drop(acquire(&hold, true, ms(500), || lock.lock().unwrap()));
        let began = std::time::Instant::now();
        drop(acquire(&hold, false, ms(40), || lock.lock().unwrap()));
        assert!(began.elapsed() < ms(2000));
        hold.set(false);
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
