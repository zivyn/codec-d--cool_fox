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
//!
//! # Stage A (`VICTORY_STAGE_A=1`): begun before the steps, with a copy owned by one stage at a time
//!
//! The offer after the take leaves the steps, the snapshots and the card's copies of a
//! submission (the larger part of what is beside it) without anything to go beside them. With
//! the switch the thread is offered the next submission's spans earlier, as soon as the
//! planning of this one is made ([`offer_early`]), and the lane's steps of this one, its
//! snapshots and its own asking and refreshing go on at the same time. What makes that safe is
//! that a host copy has one owner at a time:
//!
//! - the thread owns the copies that this submission does nothing to ([`InUse`]: no step of
//!   its lists, nor of the planning's own list, refreshes, declares or places on the copy, and
//!   none of them writes title memory where the span lies, and no step of ours runs that may
//!   write anywhere) from the offer to [`join_early`], which the lane calls before it
//!   declares what its shaders write, takes what goes up to the card or does anything with
//!   the card's side of a copy;
//! - the lane owns all the others until the take, and gives them to the thread at the offer
//!   after the take, as before ([`offer`]): what the early offer left out of the spans is what
//!   this one offers (a span the thread did is not offered twice);
//! - the thread asks and refreshes as the other offer does (a standing asking, `Batch::update`),
//!   which leaves alone what flights in the air declared written, and what the title or the
//!   lane writes while it works has a higher number than what it read, so the next asking
//!   (the lane's own, for this submission, which comes later) finds it. The thread gives
//!   nothing to the queue and takes no lock of the submission, so fences, signals and
//!   `QUEUE_ORDER` are still the lane's alone.
//!
//! What is not done on the thread: the title's bytes under flights' writes (they wait for the
//! flights, and the flight of this very submission is not in the line yet at the early
//! offer), the steps (they write title memory in their order), the planning.

use super::*;

/// Whether the next submission's refreshes are begun early.
///
/// (`VICTORY_PIPELINE_PREPARE` may change while the title runs, `VICTORY_AB`: [`offer`] reads this once, and a refresh
/// offered while it was on is joined by [`join`] after it went off.)
pub(super) fn on() -> bool {
    crate::queue::pipeline_prepare() && late() && relay_exact()
}

/// Whether the next submission's refreshes may begin once a submission's card
/// copies are taken: `own` (the bytes were taken into copies of the send's own),
/// `nothing_sent` (no copy has bytes to go up). Without own copies the list
/// that brings the card up to date reads the copies' CPU side when the card
/// runs it, and the refreshes of the next submission write there meanwhile:
/// the card would get a mix of two frames' bytes.
pub(super) fn may_offer(own: bool, nothing_sent: bool) -> bool {
    own || nothing_sent
}

/// Whether something was ever offered: from then on [`join`] looks at the gate whether the switch is on or not.
static OFFERED_EVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Offered work, being done, and the wait for it to be over.
pub(super) struct Gate<W> {
    state: Mutex<(Option<W>, bool)>,
    changed: std::sync::Condvar,
}

impl<W> Gate<W> {
    pub(super) const fn new() -> Self {
        Gate { state: Mutex::new((None, false)), changed: std::sync::Condvar::new() }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, (Option<W>, bool)> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Offers `work`: refused (given back) if some is offered or being done.
    pub(super) fn offer(&self, work: W) -> Result<(), W> {
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
    pub(super) fn take(&self) -> W {
        let mut state = self.lock();
        loop {
            if let Some(work) = state.0.take() {
                state.1 = true;
                return work;
            }
            state = self.changed.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    pub(super) fn done(&self) {
        self.lock().1 = false;
        self.changed.notify_all();
    }

    /// Waits until nothing is offered or being done; whether it had to.
    pub(super) fn join(&self) -> bool {
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
pub(super) type Spans = Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>;

/// What the thread is given: whether it is the early offer ([`offer_early`]), and the spans.
type Offered = (bool, Spans);

static GATE: Gate<Offered> = Gate::new();

thread_local! {
    /// On the lane, between the early offer and the one after the take: the spans of the next
    /// submission that the early offer did not give away (all of them if it gave none, the next
    /// submission having been asked for once).
    static PLAN: std::cell::RefCell<Option<Spans>> = const { std::cell::RefCell::new(None) };
}

/// Since the last frame-rate line: offers made, offers with nothing to
/// refresh (no next submission known, or its copies are being given to the
/// sends), refreshes done, their spans, bytes asked, microseconds the thread
/// worked, and microseconds the next preparing waited for it.
static STATS: [std::sync::atomic::AtomicU64; 19] = [const { std::sync::atomic::AtomicU64::new(0) }; 19];
const OFFERED: usize = 0;
const NOTHING: usize = 1;
const DONE: usize = 2;
const SPANS: usize = 3;
const BYTES: usize = 4;
const WORKED: usize = 5;
const WAITED: usize = 6;
const REFUSED: usize = 7;
/// Early offers made, spans given in them, spans kept for the take (a step of the submission is at their copy),
/// early offers not made because a step of ours may write anywhere, microseconds the lane waited at [`join_early`],
/// and microseconds the thread worked on early offers.
const EARLY: usize = 8;
const EARLY_SPANS: usize = 9;
const EARLY_KEPT: usize = 10;
const EARLY_VETO: usize = 11;
const EARLY_WAITED: usize = 12;
const EARLY_WORKED: usize = 13;
/// Offers of the entry after the next ([`offer_deep`], `VICTORY_STAGE_A_DEEP`) made, spans given in them, spans kept back
/// (a step of the submission being prepared is at their copy), offers not made because a step of ours may write
/// anywhere, and offers refused (the gate was not free).
const DEEP: usize = 14;
const DEEP_SPANS: usize = 15;
const DEEP_KEPT: usize = 16;
const DEEP_VETO: usize = 17;
const DEEP_REFUSED: usize = 18;

/// The spans of the refreshes of the lists at `lists` (host lists, as the
/// addresses the line gave), as [`Batch::refresh_front`] makes them for one
/// part, but for the copies not in `excluding`.
///
/// # Safety
///
/// The addresses name host lists that nothing changes or takes while this
/// runs ([`crate::queue::take_ahead`]).
pub(super) unsafe fn spans_of(lists: &[usize], excluding: &[usize]) -> Spans {
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

/// What a submission's steps still do to host copies and title memory between its planning
/// and the end of its snapshots, where the next submission's refreshes may not be
/// ([`offer_early`]).
#[derive(Default, Debug, PartialEq)]
pub(super) struct InUse {
    /// The copies (by address, in order) that a step refreshes, declares written or places on.
    copies: Vec<usize>,
    /// The title memory that a step writes or places, in order, joined.
    ranges: Vec<(usize, usize)>,
    /// A step of ours that may write title memory anywhere ([`Step::Run`]).
    unknown: bool,
}

impl InUse {
    /// What `lists` (the submission's, and the planning's own list) do.
    pub(super) fn of(lists: &[&HostList]) -> InUse {
        let mut used = InUse::default();
        let mut written = Vec::new();
        for list in lists {
            used.note_all(list.deferred().copies.iter(), &mut written);
        }
        used.finish(written)
    }

    fn note_all<'a>(&mut self, steps: impl Iterator<Item = &'a Step>, written: &mut Vec<(usize, usize)>) {
        let mut last = 0;
        for step in steps {
            match step {
                Step::Refresh(shadow, ..) | Step::GpuWrite(shadow, ..) => {
                    // (Most come several times on end.)
                    let key = std::sync::Arc::as_ptr(shadow) as usize;
                    if key != last {
                        self.copies.push(key);
                        last = key;
                    }
                }
                Step::Placed(p) => {
                    self.copies.push(std::sync::Arc::as_ptr(&p.shadow) as usize);
                    written.push((p.dst, p.dst + p.len));
                    written.push((p.src, p.src + p.len));
                }
                &Step::Copy(dst, _, len) => written.push((dst, dst + len)),
                Step::Run(_) => self.unknown = true,
                // (A fill reads title memory and writes the list's own upload memory; a host step, a texture and a
                // plane write memory that is no copy's.)
                Step::Fill(..) | Step::Host(..) | Step::Texture(_) | Step::Plane(..) => {}
            }
        }
    }

    fn finish(mut self, written: Vec<(usize, usize)>) -> InUse {
        self.copies.sort_unstable();
        self.copies.dedup();
        join_ranges(&mut self.ranges, written);
        self
    }

    /// Whether the thread may begin a span of a copy now.
    fn leaves(&self, shadow: &std::sync::Arc<crate::shadow::Shadow>, from: usize, to: usize) -> bool {
        self.copies.binary_search(&(std::sync::Arc::as_ptr(shadow) as usize)).is_err() && !touches(&self.ranges, from, to)
    }
}

/// `spans` as what the thread may begin now and what waits for the take.
fn split_spans(spans: Spans, used: &InUse) -> (Spans, Spans) {
    spans.into_iter().partition(|(shadow, from, to)| used.leaves(shadow, *from, *to))
}

/// Whether the early offer is made (`VICTORY_STAGE_A=1`, with the asking that stands beside others).
///
/// (May change while the title runs, `VICTORY_AB`: [`offer_early`] reads it once for a submission, [`join_early`] and
/// [`join`] look at the gate whatever it says.)
fn early_on() -> bool {
    on() && crate::queue::stage_a() && crate::watch::once()
}

/// In the planning of a submission, after the steps that the card is given are chosen and before the lane waits
/// for flights, asks or refreshes anything: offers the refreshes of the next submission that touch no copy this one
/// does something to ([`InUse`]) to the thread ([`crate::queue::take_ahead`] names the next one); the rest is kept
/// for [`offer`], after the take. Whether the thread was given any.
///
/// The lane calls [`join_early`] before it declares what its shaders write.
pub(super) fn offer_early(runs: &[&HostList], own: Option<&HostList>) -> bool {
    PLAN.with(|p| p.borrow_mut().take());
    if !early_on() {
        return false;
    }
    let Some(next) = crate::queue::take_ahead() else {
        return false;
    };
    // SAFETY: from `take_ahead`, on our thread of submissions, which is the
    // one that takes the entry out of the line.
    let spans = unsafe { spans_of(&next, &[]) };
    if spans.is_empty() {
        return false;
    }
    let mut lists: Vec<&HostList> = runs.to_vec();
    lists.extend(own);
    let used = InUse::of(&lists);
    if used.unknown {
        // A step of ours may write title memory where the thread reads it, and ends the askings that stand.
        STATS[EARLY_VETO].fetch_add(1, Ordering::Relaxed);
        PLAN.with(|p| *p.borrow_mut() = Some(spans));
        return false;
    }
    let (now, later) = split_spans(spans, &used);
    STATS[EARLY_KEPT].fetch_add(later.len() as u64, Ordering::Relaxed);
    let mut offered = false;
    let mut left = later;
    if !now.is_empty() {
        start();
        OFFERED_EVER.store(true, Ordering::Release);
        let count = now.len() as u64;
        match GATE.offer((true, now)) {
            Ok(()) => {
                STATS[EARLY].fetch_add(1, Ordering::Relaxed);
                STATS[EARLY_SPANS].fetch_add(count, Ordering::Relaxed);
                offered = true;
            }
            Err((_, now)) => {
                // (Not over, which the top of the preparing waits for: not reached.)
                STATS[REFUSED].fetch_add(1, Ordering::Relaxed);
                left.extend(now);
            }
        }
    }
    PLAN.with(|p| *p.borrow_mut() = Some(left));
    offered
}

/// The thread of stage A, once it has the number of the submission whose lists are `runs`: offers the refreshes of the
/// entry after it (`next`: its host lists, as the addresses the line gave) that touch no copy this submission does
/// something to ([`InUse`]), to the thread of the refreshes, beside this submission's asking, refreshing and steps
/// (`VICTORY_STAGE_A_DEEP=1`). Whether any was offered.
///
/// Ownership is as for [`offer_early`], one entry deeper: a host copy is in one stage's hands at a time.
///
/// - What this submission refreshes, declares written or places on, and the title memory its steps write, is not
///   given (the copies named in [`InUse`]): the thread of stage A is the only one to touch those until the lane's take
///   of this submission, and the lane's take is behind [`join`], which waits for this offer's work too (the lane joins
///   the thread of stage A first, so an offer made while it worked is there to be waited for).
/// - A step of ours that may write anywhere ends the askings that stand and the offer is not made ([`InUse::unknown`]).
/// - The refreshes are those of the early offer (a standing asking beside the thread's own, `Batch::update`): they
///   leave alone what flights in the air declared written, the previous submission's own among them (it is registered
///   before stage A of this one is offered), and what the title writes while they run has a higher number than what
///   they read, so the asking of the entry's own preparing, which comes later, finds it.
/// - What the take of this submission sends is taken after the join, so a copy refreshed here for the entry after it
///   holds, at that take, bytes of title memory newer than the submission's: only copies no list of this submission
///   refreshes (so none of its lists reads them through the card's copy as of its own frame, `InUse`).
///
/// # Safety (of the addresses)
///
/// `next` names host lists of an entry of the line, which only the thread of submissions takes out of it, and it does
/// not before this submission has been taken by it, after [`join`] and the thread of stage A being done.
pub(super) fn offer_deep(runs: &[&HostList], next: &[usize]) -> bool {
    if !early_on() {
        return false;
    }
    // SAFETY: as above.
    let spans = unsafe { spans_of(next, &[]) };
    if spans.is_empty() {
        return false;
    }
    let used = InUse::of(runs);
    if used.unknown {
        STATS[DEEP_VETO].fetch_add(1, Ordering::Relaxed);
        return false;
    }
    let (now, later) = split_spans(spans, &used);
    STATS[DEEP_KEPT].fetch_add(later.len() as u64, Ordering::Relaxed);
    if now.is_empty() {
        return false;
    }
    start();
    OFFERED_EVER.store(true, Ordering::Release);
    let count = now.len() as u64;
    match GATE.offer((true, now)) {
        Ok(()) => {
            STATS[DEEP].fetch_add(1, Ordering::Relaxed);
            STATS[DEEP_SPANS].fetch_add(count, Ordering::Relaxed);
            true
        }
        Err(_) => {
            STATS[DEEP_REFUSED].fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

/// Before the lane declares what its shaders write, and takes what goes up to the card: what was
/// offered early is over, so that every copy is the lane's again.
pub(super) fn join_early() {
    if !OFFERED_EVER.load(Ordering::Acquire) {
        return;
    }
    let began = std::time::Instant::now();
    if GATE.join() {
        STATS[EARLY_WAITED].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// After the submission's bytes are taken: offers the refreshes of the
/// submission that comes next, if our thread of submissions named it
/// ([`crate::queue::take_ahead`]), or what is left of them after the early offer
/// ([`offer_early`]). `excluding`: copies (by address) that this
/// submission still does something to, which are left alone.
pub(super) fn offer(excluding: &[usize]) {
    if !on() {
        return;
    }
    let spans: Spans = match PLAN.with(|p| p.borrow_mut().take()) {
        Some(left) => left
            .into_iter()
            .filter(|(shadow, ..)| !excluding.contains(&(std::sync::Arc::as_ptr(shadow) as usize)))
            .collect(),
        None => {
            let Some(next) = crate::queue::take_ahead() else {
                STATS[NOTHING].fetch_add(1, Ordering::Relaxed);
                return;
            };
            // SAFETY: from `take_ahead`, on our thread of submissions, which is the
            // one that takes the entry out of the line.
            unsafe { spans_of(&next, excluding) }
        }
    };
    if spans.is_empty() {
        STATS[NOTHING].fetch_add(1, Ordering::Relaxed);
        return;
    }
    start();
    OFFERED_EVER.store(true, Ordering::Release);
    match GATE.offer((false, spans)) {
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
                let (early, spans) = GATE.take();
                let began = std::time::Instant::now();
                let _span = crate::timeline::span(if early {
                    "refreshes of the next submission, begun before the steps"
                } else {
                    "refreshes of the next submission, begun early"
                });
                // (A refresh that panicked is the next preparing's to do.)
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run(spans)));
                let spent = began.elapsed().as_micros() as u64;
                STATS[WORKED].fetch_add(spent, Ordering::Relaxed);
                if early {
                    STATS[EARLY_WORKED].fetch_add(spent, Ordering::Relaxed);
                }
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
pub(super) fn run(mut spans: Spans) {
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
    let copies = spans.iter().map(|s| std::sync::Arc::as_ptr(&s.0) as usize).collect::<Vec<_>>();
    wide_each_by_copy(copies.into_iter(), move |i| {
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
    if super::relax::deep() {
        tracing::info!(
            "stage A, the refreshes of the entry after the next begun beside the next's steps (VICTORY_STAGE_A_DEEP), per frame: {:.1} offered with {:.1} spans, {:.1} spans kept back (a step of the next submission is at their copy), {:.2} not offered because a step of ours may write anywhere, {:.2} refused (the thread was not free); the thread worked {:.1} ms in all on early offers, the lane waited {:.1} ms for it at the join",
            s[DEEP] / frames,
            s[DEEP_SPANS] / frames,
            s[DEEP_KEPT] / frames,
            s[DEEP_VETO] / frames,
            s[DEEP_REFUSED] / frames,
            s[EARLY_WORKED] / 1000.0 / frames,
            s[WAITED] / 1000.0 / frames
        );
    }
    if crate::queue::stage_a() {
        tracing::info!(
            "stage A, the refreshes begun before the steps (VICTORY_STAGE_A), per frame: {:.1} offered with {:.1} spans, {:.1} spans kept for \
             the take (a step of the submission is at their copy), {:.2} not offered because a step of ours may write anywhere; the thread \
             worked {:.1} ms on them, the lane waited {:.1} ms for them before it declared its shaders' writes",
            s[EARLY] / frames,
            s[EARLY_SPANS] / frames,
            s[EARLY_KEPT] / frames,
            s[EARLY_VETO] / frames,
            s[EARLY_WORKED] / 1000.0 / frames,
            s[EARLY_WAITED] / 1000.0 / frames
        );
    }
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

    /// A copy as the lane has it: the title's memory, the copy's CPU side (leaked: the test ends with the process), and
    /// what the card holds of it.
    struct Held {
        at: usize,
        shadow: std::sync::Arc<Shadow>,
        side: usize,
        card: Vec<u8>,
    }

    impl Held {
        fn new() -> Held {
            let at = memory();
            let side: &'static mut [u8] = Box::leak(vec![0u8; N * PAGE].into_boxed_slice());
            let shadow = Shadow::unbacked(at as u64, side.as_mut_ptr(), (N * PAGE) as u64, true);
            Held { at, shadow, side: side.as_ptr() as usize, card: vec![0u8; N * PAGE] }
        }

        fn side(&self) -> &[u8] {
            // SAFETY: leaked, the length of the allocation; only refreshes write it and the test reads it between them.
            unsafe { std::slice::from_raw_parts(self.side as *const u8, N * PAGE) }
        }

        fn title(&self) -> &[u8] {
            // SAFETY: the allocation, read when nothing writes it.
            unsafe { std::slice::from_raw_parts(self.at as *const u8, N * PAGE) }
        }

        fn take(&mut self) {
            for (a, b) in self.shadow.take_dirty() {
                let held = self.side;
                // SAFETY: as `side`.
                let held = unsafe { std::slice::from_raw_parts(held as *const u8, N * PAGE) };
                self.card[a..b].copy_from_slice(&held[a..b]);
            }
        }
    }

    /// The thread of stage A and the lane at the same time, with the title's own threads writing all through: what
    /// the lane's submission does something to is the lane's (what it refreshes, what it writes, what it declares),
    /// the thread is given the others, the lane takes what goes up to the card once the thread is joined, and the
    /// thread is given the rest after the take, as `offer_early` and `offer` do. After the lane's next preparing
    /// asks and refreshes, every byte of every copy the card holds is the title's, 500 rounds of random writes.
    #[test]
    fn two_stages_beside_the_lane_leave_no_stale_byte_for_the_card() {
        use super::{InUse, split_spans};
        use crate::host::Step;
        use std::sync::atomic::{AtomicBool, Ordering};
        const COPIES: usize = 6;
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let mut copies: Vec<Held> = (0..COPIES).map(|_| Held::new()).collect();
        let mut seed = 0x1234_5678_9abc_def1u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for c in copies.iter_mut() {
            refresh(&c.shadow, c.at, &[(0, N * PAGE)]);
            c.take();
        }
        let (mut early_spans, mut late_spans) = (0usize, 0usize);
        for round in 0..500 {
            // The title writes for the submission and the next.
            for _ in 0..(next() % 8) {
                let c = (next() % COPIES as u64) as usize;
                write(copies[c].at, (next() % (N * PAGE) as u64) as usize, (next() % 255) as u8 + 1);
            }
            // What the lane's submission does: refreshes some copies, copies into a page of one, declares a range of one.
            let mut steps: Vec<Step> = Vec::new();
            let mut named = Vec::new();
            for c in 0..COPIES {
                if next() % 3 == 0 {
                    steps.push(Step::Refresh(copies[c].shadow.clone(), copies[c].at, N * PAGE));
                    named.push(c);
                }
            }
            let copy_to = (next() % 3 == 0).then(|| ((next() % COPIES as u64) as usize, (next() % N as u64) as usize));
            if let Some((c, page)) = copy_to {
                let at = copies[c].at + page * PAGE;
                steps.push(Step::Copy(at, at, PAGE));
            }
            let declare = named.first().map(|&c| (c, (next() % N as u64) as usize));
            if let Some((c, _)) = declare {
                steps.push(Step::GpuWrite(copies[c].shadow.clone(), copies[c].at, PAGE));
            }
            let mut used = InUse::default();
            let mut written = Vec::new();
            used.note_all(steps.iter(), &mut written);
            let used = used.finish(written);
            assert!(!used.unknown);
            // The next submission refreshes a span of each copy.
            let spans: super::Spans = (0..COPIES)
                .map(|c| {
                    let from = (next() % N as u64) as usize;
                    let to = (from + 1 + (next() % 6) as usize).min(N);
                    (copies[c].shadow.clone(), copies[c].at + from * PAGE, copies[c].at + to * PAGE)
                })
                .collect();
            let (now, later) = split_spans(spans, &used);
            for (shadow, from, to) in &now {
                assert!(used.leaves(shadow, *from, *to), "a span of a copy the submission does something to was given away");
            }
            for (shadow, from, to) in &later {
                assert!(!used.leaves(shadow, *from, *to), "a span of a free copy was kept");
            }
            (early_spans, late_spans) = (early_spans + now.len(), late_spans + later.len());
            // The title's other threads, writing while all this goes on.
            let stop = std::sync::Arc::new(AtomicBool::new(false));
            let writer = {
                let stop = stop.clone();
                let targets: Vec<usize> = copies.iter().map(|c| c.at).collect();
                let mut s = next() | 1;
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        s ^= s << 13;
                        s ^= s >> 7;
                        s ^= s << 17;
                        write(targets[(s % COPIES as u64) as usize], ((s >> 8) % (N * PAGE) as u64) as usize, (s >> 40) as u8 | 1);
                        std::thread::yield_now();
                    }
                })
            };
            // Stage A, beside the lane's asking, refreshing and steps.
            let stage = std::thread::spawn(move || super::run(now));
            for &c in &named {
                refresh(&copies[c].shadow, copies[c].at, &[(0, N * PAGE)]);
            }
            if let Some((c, page)) = copy_to {
                let at = copies[c].at + page * PAGE;
                for i in (0..PAGE).step_by(64) {
                    write(at, i + 3, (round % 200) as u8 + 1);
                }
                crate::watch::wrote(at, PAGE);
            }
            let mut ranges = Vec::new();
            if let Some((c, page)) = declare {
                let range = (copies[c].at + page * PAGE, copies[c].at + (page + 1) * PAGE);
                copies[c].shadow.writing(range.0, range.1 - range.0);
                ranges.push((c, range));
            }
            // The lane is at the place before its shaders' writes are declared and the take: the thread is over.
            stage.join().unwrap();
            for c in copies.iter_mut() {
                c.take();
            }
            // After the take, the rest is given to the thread; the lane sends meanwhile.
            let rest = std::thread::spawn(move || super::run(later));
            std::thread::yield_now();
            rest.join().unwrap();
            // The flight lands, before the next preparing refreshes the copy again.
            for &(c, range) in &ranges {
                copies[c].shadow.copy_back_done(&[range]);
            }
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
            // The next preparing's own asking and refresh, and its take: nothing stale is left.
            for c in copies.iter_mut() {
                refresh(&c.shadow, c.at, &[(0, N * PAGE)]);
                c.take();
            }
            for (i, c) in copies.iter().enumerate() {
                assert!(c.card == c.title(), "a byte of copy {i} that the card holds is not the title's after round {round}");
                assert!(c.side() == c.title(), "a byte of copy {i}'s CPU side is not the title's after round {round}");
            }
        }
        assert!(early_spans > 0 && late_spans > 0, "the stress had both kinds of span: {early_spans} early, {late_spans} late");
        for c in &copies {
            // SAFETY: ours.
            unsafe { VirtualFree(c.at as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// STAGE3: the stress above with the steps stage A's thread may now do (`VICTORY_STAGE_A_RELAX`) and the entry after
    /// the next refreshed beside the next's steps (`VICTORY_STAGE_A_DEEP`). Two entries a round, E1 and E2: each has
    /// refreshes, a title buffer copy, a declaration, `Host` steps of every kind (some of them kinds nothing lets
    /// through), movie planes and now and then a `Run`; the bits are drawn each round. What `relax::Seen` lets through
    /// is done by a thread (the thread of stage A) while E2's untouched copies are refreshed beside it by a second
    /// (the deep offer, `split_spans` of E1's `InUse`); what it does not is done by the lane. Then the lane's take, the
    /// rest of E2's spans to the thread, E2's own steps, E2's take, the landing of both, and the next preparing's own
    /// refresh, with the title's other threads writing all through: every byte the card holds and every byte of every
    /// CPU side is the title's, every step ran once and the steps of E1 before those of E2, the lists prepared are the
    /// lists run. 500 rounds.
    #[test]
    fn relaxed_steps_and_the_deep_offer_leave_no_stale_byte_and_run_every_step_once_in_order() {
        use super::{InUse, split_spans};
        use crate::host::Step;
        use crate::host::relax::{self, Class, Kind};
        use std::sync::atomic::{AtomicBool, Ordering};
        use std::sync::{Arc, Mutex};
        const COPIES: usize = 6;

        // What a thread does for an entry's steps (the model of `Batch::run_deferred`: in order, the title buffer copy noted
        // to the journal while the asking stands, a declaration, a closure that logs where it ran).
        enum M {
            Refresh(Arc<Shadow>, usize),
            Copy(usize, u8),
            Declare(Arc<Shadow>, usize),
            Host(usize, usize),
            Run(usize, u8),
        }
        type Log = Arc<Mutex<Vec<(usize, usize, usize)>>>;
        fn do_steps(steps: Vec<M>, bases: Vec<usize>, log: Log, round: usize) -> Vec<(Arc<Shadow>, (usize, usize))> {
            let ranges: Vec<(usize, usize)> = bases.iter().map(|&at| (at, at + N * PAGE)).collect();
            let standing = crate::watch::ask_standing(&ranges, None);
            let mut declared = Vec::new();
            for step in steps {
                match step {
                    M::Refresh(shadow, at) => refresh(&shadow, at, &[(0, N * PAGE)]),
                    M::Copy(dst, value) => {
                        for i in (0..PAGE).step_by(64) {
                            write(dst, i + 3, value);
                        }
                        crate::watch::wrote(dst, PAGE);
                    }
                    M::Declare(shadow, at) => {
                        shadow.writing(at, PAGE);
                        declared.push((shadow, (at, at + PAGE)));
                    }
                    M::Host(entry, idx) => log.lock().unwrap().push((round, entry, idx)),
                    M::Run(at, value) => {
                        // Writes title memory where nobody was told, and ends every asking that stands.
                        write(at, 7, value);
                        crate::watch::end_standing_all();
                        crate::watch::ask_once_end();
                    }
                }
            }
            crate::watch::end_standing(standing);
            declared
        }

        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let mut copies: Vec<Held> = (0..COPIES).map(|_| Held::new()).collect();
        let mut seed = 0x5eed_0003_dee9_1234u64;
        let mut next = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        for c in copies.iter_mut() {
            refresh(&c.shadow, c.at, &[(0, N * PAGE)]);
            c.take();
        }
        let soft = [Kind::SlotFlip, Kind::LevelsChanged, Kind::BudgetNotes, Kind::UploadNoted, Kind::HeldNoted, Kind::Diagnostics, Kind::RectVertices];
        let hard = [Kind::TilesGiven, Kind::TilesClaimed, Kind::MemoryLanded];
        let log: Log = Arc::default();
        let (mut by_thread, mut by_lane, mut deep_spans, mut kept_spans) = (0usize, 0usize, 0usize, 0usize);
        for round in 0..500 {
            // The title writes for both entries.
            for _ in 0..(next() % 8) {
                let c = (next() % COPIES as u64) as usize;
                write(copies[c].at, (next() % (N * PAGE) as u64) as usize, (next() % 255) as u8 + 1);
            }
            let bits = (next() % 8) as u32;
            let mut real: [Vec<Step>; 2] = [Vec::new(), Vec::new()];
            let mut model: [Vec<M>; 2] = [Vec::new(), Vec::new()];
            let mut expected: Vec<(usize, usize, usize)> = Vec::new();
            for e in 0..2 {
                let mut hosts = 0usize;
                for c in 0..COPIES {
                    if next() % 3 == 0 {
                        real[e].push(Step::Refresh(copies[c].shadow.clone(), copies[c].at, N * PAGE));
                        model[e].push(M::Refresh(copies[c].shadow.clone(), copies[c].at));
                    }
                }
                if next() % 3 == 0 {
                    let c = (next() % COPIES as u64) as usize;
                    let at = copies[c].at + (next() % N as u64) as usize * PAGE;
                    real[e].push(Step::Copy(at, at, PAGE));
                    model[e].push(M::Copy(at, (round % 200) as u8 + 1));
                }
                if next() % 3 == 0 {
                    let c = (next() % COPIES as u64) as usize;
                    let at = copies[c].at + (next() % N as u64) as usize * PAGE;
                    real[e].push(Step::GpuWrite(copies[c].shadow.clone(), at, PAGE));
                    model[e].push(M::Declare(copies[c].shadow.clone(), at));
                }
                for _ in 0..(next() % 4) {
                    let kind = if next() % 6 == 0 { hard[(next() % 3) as usize] } else { soft[(next() % 7) as usize] };
                    let range = if kind == Kind::RectVertices { (0x4000_0000, 0x4000_0100) } else { (0, 0) };
                    real[e].push(Step::Host(Box::new(|| {}), range, kind));
                    model[e].push(M::Host(e, hosts));
                    expected.push((round, e, hosts));
                    hosts += 1;
                }
                if next() % 8 == 0 {
                    real[e].push(Step::Plane((0x5000_0000, 64), Box::new(|| {})));
                    model[e].push(M::Host(e, hosts));
                    expected.push((round, e, hosts));
                    hosts += 1;
                }
                if next() % 14 == 0 {
                    let c = (next() % COPIES as u64) as usize;
                    real[e].push(Step::Run(Box::new(|| {})));
                    model[e].push(M::Run(copies[c].at + (next() % N as u64) as usize * PAGE, (round % 200) as u8 + 1));
                }
            }
            // E1: what the thread may do of it, by the bits of the round. (Judged twice, the second time from the kinds.)
            let mut seen = relax::Seen::default();
            seen.steps(real[0].iter());
            let never = real[0].iter().any(|s| match s {
                Step::Run(_) => true,
                Step::Host(_, _, kind) => matches!(kind.class(), Class::Unproven(_)),
                _ => false,
            });
            assert_eq!(seen.hard.is_some(), never, "round {round}: the scan and the kinds agree on what is never done");
            let accepted = seen.hard.is_none() && seen.need & !bits == 0;
            // The lists prepared are the lists run: the thread's and the lane's ids of E1's lists, in order and parts.
            let ids: Vec<usize> = (0..3).map(|i| round * 16 + i).collect();
            assert!(crate::host::front::same_lists(&ids, &[2], &ids, &[2]));
            assert!(!crate::host::front::same_lists(&ids, &[2], &ids[..2], &[2]), "E2's lists are not E1's");
            // E2's spans: a span of each copy.
            let spans: super::Spans = (0..COPIES)
                .map(|c| {
                    let from = (next() % N as u64) as usize;
                    let to = (from + 1 + (next() % 6) as usize).min(N);
                    (copies[c].shadow.clone(), copies[c].at + from * PAGE, copies[c].at + to * PAGE)
                })
                .collect();
            let mut used = InUse::default();
            let mut written = Vec::new();
            used.note_all(real[0].iter(), &mut written);
            let used = used.finish(written);
            let (now, later) = if used.unknown { (Vec::new(), spans) } else { split_spans(spans, &used) };
            for (shadow, from, to) in &now {
                assert!(used.leaves(shadow, *from, *to), "round {round}: a span of a copy E1 does something to was given away");
            }
            (deep_spans, kept_spans) = (deep_spans + now.len(), kept_spans + later.len());
            let stop = Arc::new(AtomicBool::new(false));
            let writer = {
                let stop = stop.clone();
                let targets: Vec<usize> = copies.iter().map(|c| c.at).collect();
                let mut s = next() | 1;
                std::thread::spawn(move || {
                    while !stop.load(Ordering::Relaxed) {
                        s ^= s << 13;
                        s ^= s >> 7;
                        s ^= s << 17;
                        write(targets[(s % COPIES as u64) as usize], ((s >> 8) % (N * PAGE) as u64) as usize, (s >> 40) as u8 | 1);
                        std::thread::yield_now();
                    }
                })
            };
            let bases: Vec<usize> = copies.iter().map(|c| c.at).collect();
            let first = std::mem::take(&mut model[0]);
            let second = std::mem::take(&mut model[1]);
            // E1 on the thread or on the lane, with E2's free spans refreshed beside it as the deep offer (or, with the
            // lane, the early offer) does.
            let beside = std::thread::spawn(move || super::run(now));
            let mut declared = if accepted {
                by_thread += 1;
                let (bases, log) = (bases.clone(), log.clone());
                std::thread::spawn(move || do_steps(first, bases, log, round)).join().unwrap()
            } else {
                by_lane += 1;
                do_steps(first, bases.clone(), log.clone(), round)
            };
            // The lane joins both before it takes (the thread of stage A first, then the thread of the refreshes).
            beside.join().unwrap();
            for c in copies.iter_mut() {
                c.take();
            }
            // After the take: the rest of E2's spans to the thread (joined before E2's own preparing asks or refreshes),
            // and E2's own preparing.
            std::thread::spawn(move || super::run(later)).join().unwrap();
            declared.extend(do_steps(second, bases, log.clone(), round));
            for c in copies.iter_mut() {
                c.take();
            }
            // Both flights land before the next preparing refreshes the copies again.
            for (shadow, range) in &declared {
                shadow.copy_back_done(&[*range]);
            }
            stop.store(true, Ordering::Relaxed);
            writer.join().unwrap();
            for c in copies.iter_mut() {
                refresh(&c.shadow, c.at, &[(0, N * PAGE)]);
                c.take();
            }
            for (i, c) in copies.iter().enumerate() {
                assert!(c.card == c.title(), "round {round}: a byte of copy {i} that the card holds is not the title's");
                assert!(c.side() == c.title(), "round {round}: a byte of copy {i}'s CPU side is not the title's");
            }
            // Every step ran once, E1's before E2's.
            assert_eq!(*log.lock().unwrap(), expected, "round {round}: steps run once each, in the order of the entries");
            log.lock().unwrap().clear();
        }
        assert!(by_thread > 40 && by_lane > 40, "both paths were taken: {by_thread} on the thread, {by_lane} on the lane");
        assert!(deep_spans > 0 && kept_spans > 0, "the deep offer had both kinds of span: {deep_spans} given, {kept_spans} kept");
        for c in &copies {
            // SAFETY: ours.
            unsafe { VirtualFree(c.at as *mut _, 0, MEM_RELEASE) };
        }
    }

    /// A range the submission declares written is left alone by the thread that is given the copy after the take,
    /// until the flight has landed, whichever of the two stages refreshes it.
    #[test]
    fn a_range_declared_by_the_submission_waits_for_its_landing_whichever_stage_refreshes_it() {
        use crate::host::Step;
        let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let mut copy = Held::new();
        refresh(&copy.shadow, copy.at, &[(0, N * PAGE)]);
        copy.take();
        let range = (copy.at + 2 * PAGE, copy.at + 3 * PAGE);
        // The submission names the copy (its steps declare the range): the thread is not given it early.
        let steps = [Step::GpuWrite(copy.shadow.clone(), range.0, PAGE)];
        let mut used = super::InUse::default();
        let mut written = Vec::new();
        used.note_all(steps.iter(), &mut written);
        let used = used.finish(written);
        let spans: super::Spans = vec![(copy.shadow.clone(), copy.at, copy.at + N * PAGE)];
        let (now, later) = super::split_spans(spans, &used);
        assert!(now.is_empty(), "the lane's until the take");
        copy.shadow.writing(range.0, range.1 - range.0);
        write(copy.at, 2 * PAGE + 5, 9);
        write(copy.at, 4 * PAGE + 5, 8);
        copy.take();
        // After the take the thread is given it; the flight is not landed.
        std::thread::spawn(move || super::run(later)).join().unwrap();
        assert!(copy.shadow.writes_pending_in(range.0, range.1));
        assert_eq!(copy.side()[4 * PAGE + 5], 8, "outside the range it is copied");
        assert_eq!(copy.side()[2 * PAGE + 5], 0, "inside it waits for the landing");
        copy.shadow.copy_back_done(&[range]);
        refresh(&copy.shadow, copy.at, &[(0, N * PAGE)]);
        copy.take();
        assert_eq!(copy.side()[2 * PAGE + 5], 9, "taken when the flight has landed");
        assert!(copy.card == copy.title());
        // SAFETY: ours.
        unsafe { VirtualFree(copy.at as *mut _, 0, MEM_RELEASE) };
    }
}

#[cfg(test)]
mod tests {
    use super::Gate;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    use std::time::Duration;

    /// Not offered while the card's list copies the CPU side of copies whose
    /// bytes were not taken (`VICTORY_SEND_OWN=0`, or the taking failed).
    #[test]
    fn nothing_is_offered_while_the_card_is_to_read_the_cpu_side() {
        assert!(super::may_offer(true, false), "taken into the send's own copies");
        assert!(super::may_offer(true, true));
        assert!(super::may_offer(false, true), "nothing goes up");
        assert!(!super::may_offer(false, false), "the list reads the CPU side later");
    }

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

    fn copy(start: u64) -> Arc<crate::shadow::Shadow> {
        let side: &'static mut [u8] = Box::leak(vec![0u8; 16 * 4096].into_boxed_slice());
        crate::shadow::Shadow::unbacked(start, side.as_mut_ptr(), 16 * 4096, false)
    }

    fn in_use(steps: &[crate::host::Step]) -> super::InUse {
        let mut used = super::InUse::default();
        let mut written = Vec::new();
        used.note_all(steps.iter(), &mut written);
        used.finish(written)
    }

    /// A copy that a step refreshes, declares written or places on is the lane's until the take, and so is any span
    /// where a step writes title memory; copies that only fills and host steps look at are not.
    #[test]
    fn what_the_steps_name_or_write_is_not_given_to_the_thread() {
        use crate::host::Step;
        let (a, b, c, d) = (copy(0x1000_0000), copy(0x2000_0000), copy(0x3000_0000), copy(0x4000_0000));
        let used = in_use(&[
            Step::Refresh(a.clone(), 0x1000_0000, 4096),
            Step::Refresh(a.clone(), 0x1000_1000, 4096),
            Step::GpuWrite(b.clone(), 0x2000_0000, 4096),
            Step::Copy(0x3000_2000, 0x9000_0000, 4096),
            Step::Fill(0x5000_0000, 0x4000_0000, 4096, None),
            Step::Host(Box::new(|| {}), (0x4000_0000, 0x4000_1000), crate::host::relax::Kind::RectVertices),
        ]);
        assert!(!used.unknown);
        assert!(!used.leaves(&a, 0x1000_8000, 0x1000_9000), "named by a refresh: the whole copy");
        assert!(!used.leaves(&b, 0x2000_8000, 0x2000_9000), "named by a declaration: the whole copy");
        assert!(!used.leaves(&c, 0x3000_2000, 0x3000_3000), "where a title copy writes");
        assert!(!used.leaves(&c, 0x3000_2800, 0x3000_3800), "where a title copy writes, in part");
        assert!(used.leaves(&c, 0x3000_0000, 0x3000_1000), "the rest of its copy is free");
        assert!(used.leaves(&c, 0x3000_3000, 0x3000_4000));
        assert!(used.leaves(&d, 0x4000_0000, 0x4000_1000), "only read by a fill and a host step");
    }

    /// A step of ours that may write title memory anywhere keeps the early offer from being made.
    #[test]
    fn a_step_that_may_write_anywhere_stops_the_early_offer() {
        use crate::host::Step;
        let used = in_use(&[Step::Run(Box::new(|| {}))]);
        assert!(used.unknown);
        assert!(!in_use(&[]).unknown);
    }

    /// The spans come back as two lists in their order: what the thread may begin and what waits for the take.
    #[test]
    fn the_spans_are_split_by_what_the_submission_does() {
        use crate::host::Step;
        let (a, c, d) = (copy(0x1000_0000), copy(0x3000_0000), copy(0x4000_0000));
        let used = in_use(&[Step::Refresh(a.clone(), 0x1000_0000, 4096), Step::Copy(0x3000_2000, 0x9000_0000, 4096)]);
        let spans = vec![
            (a.clone(), 0x1000_0000, 0x1000_4000),
            (c.clone(), 0x3000_0000, 0x3000_1000),
            (c.clone(), 0x3000_2000, 0x3000_3000),
            (d.clone(), 0x4000_0000, 0x4000_8000),
        ];
        let (now, later) = super::split_spans(spans, &used);
        let key = |s: &(Arc<crate::shadow::Shadow>, usize, usize)| (s.1, s.2);
        assert_eq!(now.iter().map(key).collect::<Vec<_>>(), vec![(0x3000_0000, 0x3000_1000), (0x4000_0000, 0x4000_8000)]);
        assert_eq!(later.iter().map(key).collect::<Vec<_>>(), vec![(0x1000_0000, 0x1000_4000), (0x3000_2000, 0x3000_3000)]);
    }

    /// What is offered early is over before what is offered after the take can be: the lane waits for it ahead of
    /// declaring its shaders' writes (`join_early`), and an offer made while it is not over is refused, so a copy is
    /// never in two stages' hands at once, and the two are done in the order they were offered.
    #[test]
    fn what_was_offered_before_the_steps_is_over_before_the_offer_after_the_take_is_taken() {
        let gate: Arc<Gate<(bool, u32)>> = Arc::new(Gate::new());
        let order = Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker = {
            let (gate, order) = (gate.clone(), order.clone());
            std::thread::spawn(move || {
                for _ in 0..2 {
                    let work = gate.take();
                    std::thread::sleep(Duration::from_millis(40));
                    order.lock().unwrap().push(work);
                    gate.done();
                }
            })
        };
        assert!(gate.offer((true, 1)).is_ok());
        assert_eq!(gate.offer((false, 2)), Err((false, 2)), "not while the early one is not over");
        assert!(gate.join(), "the lane waited for the thread");
        assert_eq!(*order.lock().unwrap(), vec![(true, 1)], "over before the lane goes on");
        assert!(gate.offer((false, 2)).is_ok());
        gate.join();
        assert_eq!(*order.lock().unwrap(), vec![(true, 1), (false, 2)]);
        worker.join().unwrap();
    }
}
