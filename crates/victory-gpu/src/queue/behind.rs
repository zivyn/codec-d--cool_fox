//! Submissions prepared and sent by a thread of ours, in order, instead of by
//! the thread of the title that submits them (`VICTORY_ASYNC_SUBMIT=1`).
//!
//! The idea and the argument are fxceless/victory-fork's, commit f85101b6
//! (GPL-3.0-or-later, as this): a profile had the title's render thread inside
//! `queue::execute` for 202 of 292 ms of a frame while its recording threads
//! and the card stood idle, and the title does nothing with the result of that
//! work: it only has to be done before the lists run, in order. (Here, at
//! 77-79 ms a frame: 58 ms of it in the submit calls, the card idle for 37 ms
//! behind them.) What differs from that commit: one line for all queues, kept
//! in the order of the title's calls; the present stands in it too; nobody but
//! our thread runs what is in it.
//!
//! With the switch a submission's lists are taken from the title's lists (as
//! they are for every flight, `crate::list::detach`) and put in the [`Lane`],
//! with what they wait for, the queues' fence waits and signals, and the
//! presents. [`Lane::serve`], on a thread of ours, takes from it whatever is
//! free to run, the oldest first; within a queue the order is the title's,
//! always. What tells the title that work is done (fence values,
//! bottom-of-pipe writes) is told by the flights as before, when the card has
//! run them: later than the title's call by more than before, never earlier.
//!
//! What the title may do between its submit call and the moment our thread
//! prepares the lists, and why that is all right:
//!
//! - Reset or release the lists and allocators: their host sides, upload
//!   memory and draw state were taken out of them.
//! - Write upload memory, descriptors, constants for later lists: those of
//!   lists submitted are the card's to read until a fence behind them is
//!   reached, on the console as here; no fence behind them is reached before
//!   the lists have run ([`signal`] goes into the line behind them).
//! - Signal, wait, present on the queue: in the line behind them.
//! - Write a fence or a value in memory by the CPU: at once; our thread looks
//!   at what it stands at again ([`wake`], and every millisecond).
//! - Map pages (`CopyPageMappingsBatchX`): at once, as before, where the lists
//!   on the card may be reading them too; the bytes of uploads were read when
//!   the lists were recorded.
//! - Ask for something that has to see earlier submissions done (a read-back,
//!   a fence waited for without an event): [`drain`] first.
//! - Release a texture or a view's memory: what is retired waits for what is
//!   in the line too ([`queued`]).
//!
//! The line is bounded: a present returns when the one before it was shown
//! ([`frames_most`]), and a submission when at most [`runs_most`] are in the
//! line.

use std::collections::VecDeque;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::sync::{Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

use super::This;

/// One thing in the line: what it stands at, if it does, and what it does.
pub(super) trait Work: Send {
    /// What has to be met before it goes.
    type Ask;
    fn ask(&self) -> Option<Self::Ask>;
    fn met(ask: &Self::Ask) -> bool;
    /// Whether it is a present: everything before it goes first, met or not.
    fn shows(&self) -> bool;
    /// Whether it is a submission.
    fn runs(&self) -> bool;
    /// Whether it stands for other work, which takes its place in the line
    /// once it is free to go ([`Work::unfold`]).
    fn folded(&self) -> bool {
        false
    }
    fn unfold(self) -> Vec<Self>
    where
        Self: Sized,
    {
        vec![self]
    }
    /// Waits until a folded one is free to go.
    fn settle(_: &Self::Ask) {}
    /// Takes `next`, what its queue was given behind it, along to be done with
    /// it; or gives it back. Whether it is a submission that was taken.
    fn take_along(&mut self, next: Self) -> Result<bool, Self>
    where
        Self: Sized,
    {
        Err(next)
    }
    /// What it was taking along stops: nothing more of its queue is in the line.
    fn none_left(&self) {}
    /// Asked as it comes into the line: the number of a round of the heaps'
    /// records that began after it will have seen what the title wrote
    /// before its call (0: none was asked).
    fn round(&self) -> u64 {
        0
    }
    /// What the entry that is done after the one being done now is, as the
    /// addresses of its host lists in the order they run, if it is a
    /// submission whose lists are known ([`pipeline_prepare`]).
    fn peek(&self) -> Option<Vec<usize>> {
        None
    }
    /// Where the parts of a submission that stands for several begin in what [`Work::peek`] says (none for one).
    fn parts(&self) -> Vec<usize> {
        Vec::new()
    }
    /// Does it. `forced`: its wait was not met.
    fn go(self, forced: bool);
}

struct Entry<W> {
    number: u64,
    queue: usize,
    /// What [`Work::round`] said when it came in.
    round: u64,
    /// Runs counted for it that came into it ahead of its turn ([`Lane::prefold`]): they are not entries any more.
    along: usize,
    /// It took along what it could ahead of its turn and is prepared as it is: nothing more is taken into it.
    prepared: bool,
    what: W,
}

struct State<W> {
    /// In the order of the title's calls, whatever queue they were made on.
    items: VecDeque<Entry<W>>,
    /// The number the next entry gets.
    next: u64,
    /// The queue whose entry is being done now (it is no longer in `items`;
    /// what that queue is given stays behind it all the same).
    running: Option<usize>,
    /// Times a look at the line was asked for, and the last of them that a
    /// whole look has followed.
    asked: u64,
    served: u64,
    /// Presents and submissions in the line or being done.
    presents: usize,
    runs: usize,
}

thread_local! {
    /// On our thread, while it does an entry: the round of the heaps' records
    /// asked for it (and for what was taken along with it), 0 if none.
    static ROUND: std::cell::Cell<u64> = const { std::cell::Cell::new(0) };
}

thread_local! {
    /// On our thread, while it does an entry (a thread of the title that submits by itself is not the thread of the line).
    static DOING: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

thread_local! {
    /// On our thread, while it does an entry: the host lists of the entry
    /// that comes next of its queue, as addresses, if it is a submission.
    static AHEAD: std::cell::RefCell<Option<Vec<usize>>> = const { std::cell::RefCell::new(None) };
}

/// The host lists of the submission that comes next after the one being
/// prepared (taken: asked once), as addresses valid until our thread has
/// done with the entry it is at: nothing but our thread takes an entry out
/// of the line or changes what it holds.
pub(super) fn take_ahead() -> Option<Vec<usize>> {
    AHEAD.with(|a| a.borrow_mut().take())
}

/// Puts back what [`take_ahead`] took (a submission prepared in two halves
/// gives it to the second).
pub(super) fn restore_ahead(ahead: Option<Vec<usize>>) {
    AHEAD.with(|a| *a.borrow_mut() = ahead);
}

/// Whether the refreshes of the submission that comes next are begun, by a
/// thread of ours, while the one being prepared is sent
/// (`VICTORY_PIPELINE_PREPARE=1`; off by default): see `crate::host`.
pub(super) fn pipeline_prepare() -> bool {
    #[cfg(test)]
    if FORCE_PIPELINE.load(Relaxed) {
        return true;
    }
    // (May change while the title runs, `VICTORY_AB`: what it asks of the item at hand is read once, and the refreshes
    // begun stay joined, `crate::host::prefetch::join`.) Stage A is the same thread, begun earlier.
    let asked = crate::ab::live_flag!("VICTORY_PIPELINE_PREPARE");
    asked || stage_a()
}

/// Whether the next submission's refreshes are offered before the steps of the one being prepared, for the copies it
/// does nothing to (`VICTORY_STAGE_A=1`; off by default; it takes the refreshes of [`pipeline_prepare`] with it):
/// see `crate::host::prefetch`.
pub(super) fn stage_a() -> bool {
    #[cfg(test)]
    if FORCE_STAGE_A.load(Relaxed) {
        return true;
    }
    // (May change while the title runs, `VICTORY_AB`: read once for an item at hand, and what was offered stays joined.)
    crate::ab::live_flag!("VICTORY_STAGE_A")
}

/// Whether the next submission's whole preparing before its lists go to the card is done by a thread of ours while the
/// one before is sent (`VICTORY_STAGE_A_STEPS=1`, with [`stage_a`]; off by default): see `crate::host::front`.
pub(super) fn stage_front() -> bool {
    crate::ab::live_flag!("VICTORY_STAGE_A_STEPS")
}

/// While our thread does an entry: the first of the line, if it is a submission of the same queue, takes along what
/// it can (as it would when its turn came) and is named: its number, its host lists and where its parts begin.
/// Nothing more is taken into it after this ([`Lane::prefold`]).
pub(super) fn prefold() -> Option<(u64, Vec<usize>, Vec<usize>)> {
    if !DOING.get() {
        return None;
    }
    LANE.prefold()
}

/// Tests: stage A's question answers yes.
#[cfg(test)]
static FORCE_STAGE_A: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Tests: the pipeline's questions answer yes.
#[cfg(test)]
static FORCE_PIPELINE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// The round of the heaps' records asked for ahead for what our thread does
/// now ([`Work::round`]); 0 if none, or if not on our thread.
pub(super) fn round() -> u64 {
    ROUND.get()
}

/// Whether a round of the heaps' records is asked for as a submission comes
/// into the line (`VICTORY_HEAPS_EARLY=1`; off by default): it is made while
/// the thread prepares what is before it, and the submission's own preparing
/// waits for it at its end as for one begun then. Not for one that waited
/// for flights ([`crate::host`] begins another after the wait).
pub(super) fn heaps_early() -> bool {
    crate::ab::live_flag!("VICTORY_HEAPS_EARLY")
}

/// See the module's text.
pub(super) struct Lane<W: Work> {
    state: Mutex<State<W>>,
    /// Something to look at.
    work: Condvar,
    /// Something was done, or a look at the line ended.
    moved: Condvar,
}

impl<W: Work> Lane<W> {
    pub(super) const fn new() -> Self {
        Lane {
            state: Mutex::new(State {
                items: VecDeque::new(),
                next: 0,
                running: None,
                asked: 0,
                served: 0,
                presents: 0,
                runs: 0,
            }),
            work: Condvar::new(),
            moved: Condvar::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, State<W>> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn put(state: &mut State<W>, queue: usize, what: W) {
        state.presents += usize::from(what.shows());
        state.runs += usize::from(what.runs());
        let number = state.next;
        state.next += 1;
        let round = what.round();
        state.items.push_back(Entry { number, queue, round, along: 0, prepared: false, what });
    }

    /// Adds to the end of the line, and has it looked at.
    pub(super) fn push(&self, queue: usize, what: impl IntoIterator<Item = W>) {
        let mut state = self.lock();
        for what in what {
            Self::put(&mut state, queue, what);
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Adds `what` if `queue` has anything in the line or being done (or
    /// `always`); else does `now`, with nothing of the queue's able to come
    /// in between.
    pub(super) fn push_behind(&self, queue: usize, always: bool, what: W, now: impl FnOnce()) {
        let mut state = self.lock();
        if always || state.running == Some(queue) || state.items.iter().any(|e| e.queue == queue) {
            Self::put(&mut state, queue, what);
        } else {
            now();
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Has the line looked at: something an entry stands at may have moved.
    pub(super) fn wake(&self) {
        let mut state = self.lock();
        if state.items.is_empty() {
            return;
        }
        state.asked += 1;
        drop(state);
        self.work.notify_one();
    }

    /// Waits until everything in the line that is free to run has been done
    /// (what comes in after this call may not be).
    pub(super) fn drain(&self) {
        let folded: Vec<W::Ask> =
            self.lock().items.iter().filter(|e| e.what.folded()).filter_map(|e| e.what.ask()).collect();
        folded.iter().for_each(W::settle);
        let mut state = self.lock();
        if state.items.is_empty() && state.running.is_none() {
            return;
        }
        state.asked += 1;
        let ticket = state.asked;
        self.work.notify_one();
        while state.served < ticket {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits until at most `most` presents are in the line.
    pub(super) fn wait_presents(&self, most: usize) {
        let mut state = self.lock();
        while state.presents > most {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Waits until at most `most` submissions are in the line, or what is
    /// left of it stands at something.
    pub(super) fn wait_runs(&self, most: usize) {
        let mut state = self.lock();
        while state.runs > most && state.served < state.asked {
            state = self.moved.wait(state).unwrap_or_else(|e| e.into_inner());
        }
    }

    /// Looks at the line for as long as the process runs.
    fn serve(&self) -> ! {
        loop {
            let (asked, limit) = {
                let mut state = self.lock();
                while state.asked == state.served {
                    if state.items.is_empty() {
                        state = self.work.wait(state).unwrap_or_else(|e| e.into_inner());
                    } else {
                        // It stands at something the CPU may write without
                        // a call of ours.
                        let (back, out) =
                            self.work.wait_timeout(state, Duration::from_millis(1)).unwrap_or_else(|e| e.into_inner());
                        state = back;
                        if out.timed_out() {
                            break;
                        }
                    }
                }
                (state.asked, state.next)
            };
            self.look(limit);
            let mut state = self.lock();
            state.served = state.served.max(asked);
            drop(state);
            self.moved.notify_all();
        }
    }

    /// Does what is free to run of the entries before number `limit`.
    pub(super) fn look(&self, limit: u64) {
        while self.step(limit) {}
    }

    /// The parts of `entry`, which stands for them, in place `at`.
    fn unfold(state: &mut State<W>, at: usize, entry: Entry<W>) {
        let (number, queue, round, runs) = (entry.number, entry.queue, entry.round, usize::from(entry.what.runs()));
        let parts = entry.what.unfold();
        state.runs -= runs;
        state.runs += parts.iter().filter(|p| p.runs()).count();
        for (i, what) in parts.into_iter().enumerate() {
            state.items.insert(at + i, Entry { number, queue, round, along: 0, prepared: false, what });
        }
    }

    /// What follows `entry` in its queue, if that can go with it: taken along, as
    /// [`Work::take_along`] says. How many submissions that was.
    fn gather(&self, entry: &mut Entry<W>, limit: u64) -> usize {
        let mut added = 0;
        while entry.what.runs() {
            let next = {
                let mut state = self.lock();
                let at = state.items.iter().position(|e| e.queue == entry.queue && e.number < limit);
                at.and_then(|at| Some((at, state.items.remove(at)?)))
            };
            let Some((at, next)) = next else {
                entry.what.none_left();
                break;
            };
            // (Back where it was: parts of one submission share a number.)
            if next.what.folded() {
                let free = next.what.ask().is_none_or(|ask| W::met(&ask));
                let mut state = self.lock();
                if free {
                    Self::unfold(&mut state, at, next);
                    continue;
                }
                state.items.insert(at, next);
                break;
            }
            // (One that took others along ahead of its turn is not taken along itself: its count would be lost.)
            if next.along > 0 || next.prepared {
                self.lock().items.insert(at, next);
                break;
            }
            let Entry { number, queue, round, along, prepared, what } = next;
            match entry.what.take_along(what) {
                Ok(run) => {
                    added += usize::from(run);
                    entry.round = entry.round.max(round);
                }
                Err(what) => {
                    self.lock().items.insert(at, Entry { number, queue, round, along, prepared, what });
                    break;
                }
            }
        }
        added
    }

    /// While the entry being done is, the first of the line is a submission of the same queue: it takes along what
    /// it can, as it will when its turn comes (what comes in by then is not taken: it is marked `prepared`), so that
    /// what it is is known: its number, the host lists it will run (as [`Work::peek`] says) and where its parts begin.
    /// It stays at its place.
    pub(super) fn prefold(&self) -> Option<(u64, Vec<usize>, Vec<usize>)> {
        let (mut entry, limit) = {
            let mut state = self.lock();
            let queue = state.running?;
            let first = state.items.front()?;
            if first.queue != queue || first.what.folded() || !first.what.runs() || first.what.peek().is_none() {
                return None;
            }
            (state.items.pop_front()?, state.next)
        };
        // (The queue stays `running`: a thread that gives it something meanwhile puts it in the line.)
        if !entry.prepared {
            entry.along += self.gather(&mut entry, limit);
            entry.prepared = true;
        }
        let found = entry.what.peek().map(|lists| (entry.number, lists, entry.what.parts()));
        self.lock().items.push_front(entry);
        found
    }

    /// Does one entry: the oldest that is the first of its queue and free to
    /// run. Whether there was one. Only one thread calls this.
    fn step(&self, limit: u64) -> bool {
        // Each queue's first entry, oldest first.
        let mut heads: Vec<(u64, usize, Option<W::Ask>, bool)> = Vec::new();
        // Whether one of them stands for others not known yet.
        let mut folded = false;
        {
            let state = self.lock();
            for e in state.items.iter().take_while(|e| e.number < limit) {
                if !heads.iter().any(|h| h.1 == e.queue) {
                    heads.push((e.number, e.queue, e.what.ask(), e.what.shows()));
                    folded |= e.what.folded();
                }
            }
        }
        let Some(&(first, ..)) = heads.first() else {
            return false;
        };
        let ready = heads.iter().find(|(number, _, ask, shows)| match ask {
            Some(ask) => W::met(ask),
            // A present comes behind everything given before it.
            None => !shows || *number == first || !super::forces(),
        });
        let (number, forced) = match ready {
            Some(head) => (head.0, false),
            None => {
                // Nothing is free to run. With a present in the line the
                // oldest goes on regardless: a wait is not kept across
                // frames (`super::present`). (Not while there is work whose
                // parts are not known: it may be what is waited for.)
                if folded {
                    return false;
                }
                let state = self.lock();
                if !super::forces() || !state.items.iter().any(|e| e.number < limit && e.what.shows()) {
                    return false;
                }
                (first, true)
            }
        };
        let mut entry = {
            let mut state = self.lock();
            let Some(at) = state.items.iter().position(|e| e.number == number) else {
                return false;
            };
            let Some(entry) = state.items.remove(at) else {
                return false;
            };
            state.running = Some(entry.queue);
            entry
        };
        let (shows, mut runs) = (entry.what.shows(), usize::from(entry.what.runs()) + entry.along);
        if entry.what.folded() {
            // Its parts take its place: behind everything before it, before
            // everything after it (they share its number; the first of them
            // is found first).
            let mut state = self.lock();
            state.running = None;
            let at = state.items.iter().position(|e| e.number > entry.number).unwrap_or(state.items.len());
            Self::unfold(&mut state, at, entry);
            drop(state);
            self.moved.notify_all();
            return true;
        }
        // What follows it in its queue, if that can go with it. (Nobody else
        // takes anything out, and what comes in goes to the end.) Not for what
        // was taken along ahead of its turn, and is being prepared as it is
        // (`VICTORY_STAGE_A`).
        if !forced && !entry.prepared {
            runs += self.gather(&mut entry, limit);
        }
        ROUND.set(entry.round);
        let ahead = if pipeline_prepare() {
            // (Its place does not change before our thread is done with this
            // one; only titles' threads add to the end.)
            let state = self.lock();
            state.items.iter().find(|e| e.queue == entry.queue).and_then(|e| e.what.peek())
        } else {
            None
        };
        AHEAD.with(|a| *a.borrow_mut() = ahead);
        DOING.set(true);
        entry.what.go(forced);
        DOING.set(false);
        AHEAD.with(|a| a.borrow_mut().take());
        ROUND.set(0);
        {
            let mut state = self.lock();
            state.running = None;
            state.presents -= usize::from(shows);
            state.runs -= runs;
        }
        self.moved.notify_all();
        true
    }
}

type Recordings = std::sync::Arc<[std::sync::Arc<crate::list::thin::Recording>]>;

/// What the title's queues put in the line.
enum Job {
    /// The queue waits for the fence (its address) to reach the value.
    Wait(usize, u64),
    /// A part of a submission waits for a memory value.
    Value(crate::list::ValueWait),
    /// A submission, or a part of one: the lists' host sides and the order
    /// they run in.
    Run(usize, Vec<crate::list::Detached>, Vec<usize>),
    /// A submission whose lists are being translated ([`crate::list::thin`]):
    /// the parts of it are known when they are.
    Lists(usize, Recordings),
    /// Parts taken along with a submission ([`crate::host::joins`]): where they
    /// begin in the order, and the signals given behind them.
    Joined(usize, Vec<crate::list::Detached>, Vec<usize>, Vec<usize>, Vec<(usize, u64)>),
    /// A signal of the queue's.
    Signal(usize, u64),
    /// A frame is shown: the base texture (and optional overlay plane) and the states their lists leave them in.
    Present(
        Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>,
        Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>,
        super::Shown,
        // When the title's call put it in the line.
        Instant,
    ),
}

// SAFETY: D3D12 objects are free-threaded; a job is touched by whoever put it
// in the line and then, once it is taken out, by our thread alone (the flights
// it becomes are handed to other threads the same way).
unsafe impl Send for Job {}

enum Ask {
    Wait(usize, u64),
    Value(crate::list::ValueWait),
    Lists(Recordings),
}

impl Work for Job {
    type Ask = Ask;

    fn ask(&self) -> Option<Ask> {
        match self {
            &Job::Wait(fence, value) => Some(Ask::Wait(fence, value)),
            Job::Value(wait) => Some(Ask::Value(*wait)),
            Job::Lists(_, lists) => Some(Ask::Lists(lists.clone())),
            _ => None,
        }
    }

    fn folded(&self) -> bool {
        matches!(self, Job::Lists(..))
    }

    fn unfold(self) -> Vec<Self> {
        let Job::Lists(queue, recordings) = self else {
            return vec![self];
        };
        let lists: Vec<This> = recordings.iter().map(|r| r.back()).collect();
        let jobs = jobs_of(queue as This, &lists);
        // It stood for one submission not begun.
        let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
        QUEUED.fetch_add(runs, Relaxed);
        QUEUED.fetch_sub(1, Relaxed);
        STATS[RUNS].fetch_add(runs, Relaxed);
        jobs
    }

    fn settle(ask: &Ask) {
        if let Ask::Lists(recordings) = ask {
            recordings.iter().for_each(|r| r.wait());
        }
    }

    fn met(ask: &Ask) -> bool {
        match ask {
            &Ask::Wait(fence, value) => crate::fence::completed(fence as This) >= value,
            Ask::Value(wait) => super::value_met(wait),
            Ask::Lists(recordings) => recordings.iter().all(|r| r.done()),
        }
    }

    fn shows(&self) -> bool {
        matches!(self, Job::Present(..))
    }

    fn runs(&self) -> bool {
        matches!(self, Job::Run(..) | Job::Joined(..) | Job::Lists(..))
    }

    fn round(&self) -> u64 {
        if self.runs() && heaps_early() { crate::descriptor::ask_round() } else { 0 }
    }

    fn none_left(&self) {
        stopped(STOP_EMPTY);
    }

    fn peek(&self) -> Option<Vec<usize>> {
        match self {
            Job::Run(_, lists, order) | Job::Joined(_, lists, order, ..) => {
                Some(order.iter().map(|&i| std::ptr::from_ref(lists[i].host()) as usize).collect())
            }
            _ => None,
        }
    }

    fn parts(&self) -> Vec<usize> {
        match self {
            Job::Joined(_, _, _, parts, _) => parts.clone(),
            _ => Vec::new(),
        }
    }

    fn take_along(&mut self, next: Job) -> Result<bool, Job> {
        let (most, most_lists) = crate::host::joins();
        let most = ramp_parts(most);
        if most < 2 {
            stopped(STOP_PARTS);
            return Err(next);
        }
        // Nothing but waits and signals that a submission may follow.
        let (count, lists, signals) = match self {
            Job::Run(_, lists, order) => (1, (&*lists, &*order), &[][..]),
            Job::Joined(_, lists, order, parts, signals) => (1 + parts.len(), (&*lists, &*order), &signals[..]),
            _ => return Err(next),
        };
        let hosts = || lists.1.iter().map(|&i| lists.0[i].host());
        match &next {
            Job::Value(wait) => {
                let met = wait.met()
                    || crate::host::late_met_known(wait) == Some(true)
                    || hosts().any(|list| crate::host::will_meet(list, wait));
                // (Counted by cause; with VICTORY_JOIN_WAITS a write along that covers the place meets it.)
                if !met && !crate::host::precise::value_wait(wait, hosts()) {
                    stopped(STOP_VALUE);
                    return Err(next);
                }
            }
            &Job::Wait(fence, value) => {
                if crate::fence::completed(fence as This) < value
                    && !signals.iter().any(|&(f, v)| f == fence && v >= value)
                    && !crate::host::precise::fence_wait(fence, value)
                {
                    stopped(STOP_FENCE);
                    return Err(next);
                }
            }
            Job::Signal(..) => {}
            Job::Run(_, more, order) => {
                let runs: Vec<&crate::host::HostList> = order.iter().map(|&i| more[i].host()).collect();
                let why = if count >= most {
                    Some(STOP_PARTS)
                } else if lists.1.len() + runs.len() > most_lists {
                    Some(STOP_LISTS)
                } else {
                    use crate::host::NoJoin;
                    let refusal = crate::host::join_refusal(hosts(), &runs);
                    if let Some(no) = refusal {
                        crate::host::precise::refused(no, hosts(), &runs);
                    }
                    refusal.map(|no| match no {
                        NoJoin::CopyOrRun => STOP_STEP,
                        NoJoin::Written => STOP_WRITTEN,
                        NoJoin::WideOnly => STOP_WIDE,
                    })
                };
                if let Some(why) = why {
                    stopped(why);
                    return Err(next);
                }
            }
            Job::Present(..) => {
                stopped(STOP_PRESENT);
                return Err(next);
            }
            Job::Joined(..) | Job::Lists(..) => {
                stopped(STOP_OTHER);
                return Err(next);
            }
        }
        let taken = std::mem::replace(self, Job::Wait(0, 0));
        let (queue, mut lists, mut order, mut parts, mut signals) = match taken {
            Job::Run(queue, lists, order) => (queue, lists, order, Vec::new(), Vec::new()),
            Job::Joined(queue, lists, order, parts, signals) => (queue, lists, order, parts, signals),
            other => {
                *self = other;
                return Err(next);
            }
        };
        let run = matches!(next, Job::Run(..));
        match next {
            Job::Run(_, more, then) => {
                parts.push(order.len());
                order.extend(then.into_iter().map(|i| i + lists.len()));
                lists.extend(more);
                STATS[JOINED].fetch_add(1, Relaxed);
            }
            Job::Signal(fence, value) => signals.push((fence, value)),
            _ => {}
        }
        *self = Job::Joined(queue, lists, order, parts, signals);
        Ok(run)
    }

    fn go(self, forced: bool) {
        let began = Instant::now();
        let _doing = crate::timeline::span(match self {
            Job::Run(..) => "the line: a submission",
            Job::Joined(..) => "the line: submissions joined",
            Job::Present(..) => "the line: a present",
            Job::Signal(..) => "the line: a signal",
            _ => "the line: a wait that is met",
        });
        if forced {
            STATS[FORCED].fetch_add(1, Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Forced, 1);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a queue still stood at a wait at a present: it was not met"));
        }
        match self {
            // (A folded one is unfolded, never done.)
            Job::Wait(..) | Job::Value(..) | Job::Lists(..) => {}
            Job::Run(queue, lists, order) => {
                let _ = queue;
                ramp_grow();
                #[cfg(feature = "diagnostics")]
                crate::capture::op(queue, || format!("RUN submission of {} lists (its thread's)", order.len()));
                match crate::host::host() {
                    Some(host) => host.execute_late(lists, order),
                    None => drop(lists),
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
            Job::Joined(queue, lists, order, parts, signals) => {
                let _ = queue;
                ramp_grow();
                #[cfg(feature = "diagnostics")]
                crate::capture::op(queue, || {
                    format!("RUN {} parts of submissions in one, {} lists (its thread's)", 1 + parts.len(), order.len())
                });
                match crate::host::host() {
                    Some(host) => host.execute_parts(lists, order, &parts),
                    None => drop(lists),
                }
                // (A part that had no list is one all the same.)
                QUEUED.fetch_sub(1 + parts.len() as u64, Relaxed);
                for (fence, value) in signals {
                    if !crate::host::late_signal(fence, value) {
                        crate::fence::set(fence as This, value);
                    }
                }
            }
            Job::Signal(fence, value) => {
                if !crate::host::late_signal(fence, value) {
                    crate::fence::set(fence as This, value);
                }
            }
            Job::Present(texture, overlay, shown, called) => {
                ramp_reset();
                STATS[LAG].fetch_add(called.elapsed().as_micros() as u64, Relaxed);
                if let Some((texture, state)) = texture {
                    // (Where its lists were not translated when the title
                    // presented: the state they left it in.)
                    let state = if state == u32::MAX { texture.state.load(Relaxed) } else { state };
                    let over = overlay.as_ref().map(|(ov, ov_state)| {
                        let st = if *ov_state == u32::MAX { ov.state.load(Relaxed) } else { *ov_state };
                        (&ov.texture, st)
                    });
                    super::show_planes(&texture.texture, state, over, shown);
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
        }
        STATS[BUSY].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

static LANE: Lane<Job> = Lane::new();

/// Since the last frame-rate line: what ended a submission's taking of the ones behind it along (each submission
/// that was prepared with others in reach ends in one of these): see the constants.
static STOPS: [AtomicU64; 11] = [const { AtomicU64::new(0) }; 11];
/// A wait for a memory value that is not met, and not known to be met by the lists taken along or the flights.
const STOP_VALUE: usize = 0;
/// A wait for a fence that nothing taken along signals.
const STOP_FENCE: usize = 1;
/// The most parts (`VICTORY_JOIN_PARTS`, or the ramp's) were taken.
const STOP_PARTS: usize = 2;
/// The most lists (`VICTORY_JOIN_LISTS`) would be passed.
const STOP_LISTS: usize = 3;
/// The next part copies title memory or has a step of ours.
const STOP_STEP: usize = 4;
/// The next part reads memory that the lists taken along write.
const STOP_WRITTEN: usize = 5;
/// The next part reads memory only a range of 4 MiB or more, declared writable by the lists taken along, covers
/// (what `VICTORY_JOIN_WIDE_MB` would join; counted in diagnostics builds).
const STOP_WIDE: usize = 6;
/// A present.
const STOP_PRESENT: usize = 7;
/// A call whose lists are not translated yet, or parts taken along already.
const STOP_OTHER: usize = 8;
/// Nothing more of the queue was in the line.
const STOP_EMPTY: usize = 9;

fn stopped(why: usize) {
    STOPS[why].fetch_add(1, Relaxed);
}

/// Submissions and presents in the line that have not been given to the host
/// yet.
static QUEUED: AtomicU64 = AtomicU64::new(0);

/// Since the last frame-rate line: submissions put in the line, presents, the
/// times they found things in it (the sum of their numbers) and the most; what
/// threads of the title waited, in microseconds, for room in the line, at a
/// present, and for a drain, and the drains; what our thread worked; waits
/// not met at a present.
static STATS: [AtomicU64; 13] = [const { AtomicU64::new(0) }; 13];
const RUNS: usize = 0;
const PRESENTS: usize = 1;
const DEPTH: usize = 2;
const DEEPEST: usize = 3;
const ROOM: usize = 4;
const AT_PRESENT: usize = 5;
const DRAINING: usize = 6;
const DRAINS: usize = 7;
const BUSY: usize = 8;
const FORCED: usize = 9;
/// Parts prepared together with the one before them.
const JOINED: usize = 10;
/// Submissions that took fewer parts along than they might have
/// ([`ramp_parts`]).
const RAMPED: usize = 11;
/// Microseconds from the title's present call to the moment our thread did
/// it: how far behind the title the line is (the title may be one frame ahead,
/// `VICTORY_SUBMIT_FRAMES`).
const LAG: usize = 12;

/// How many parts the next submission of the line may take along
/// (`VICTORY_JOIN_RAMP=1`; off by default): after a present only its own,
/// then twice as many for each submission done, up to [`crate::host::joins`]
/// (`VICTORY_JOIN_RAMP_FIRST=<n>`: the first's).
///
/// Why: after the frame's last submission the card stood idle 30 ms (a
/// profile of the window spot: 131.7 to 162.1 ms) while the thread prepared
/// one submission of the next frame's first parts, joined from what was in
/// the line, for 35 ms. Joined parts are all sent when the last of them is
/// prepared; with the card idle, the first part sent alone puts it to work
/// 30 ms sooner, and the rest is prepared while it runs. Later in the frame
/// the card has work in the air and the joins are whole.
static RAMP: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(usize::MAX);

fn ramp_first() -> Option<usize> {
    // 0: no ramp. (May change while the title runs, `VICTORY_AB`: `RAMP` is set again at each present, and a value left
    // from before only lets a submission take what the ramp would, up to `most`.)
    fn first(text: Option<&str>) -> usize {
        if text.is_some_and(|v| v == "1") { number("VICTORY_JOIN_RAMP_FIRST").unwrap_or(1).max(1) } else { 0 }
    }
    match crate::ab::live_value!("VICTORY_JOIN_RAMP", usize, first) {
        0 => None,
        n => Some(n),
    }
}

/// The parts a submission may take with a ramp at `ramp`, of `most`.
fn ramped(ramp: usize, most: usize) -> usize {
    ramp.min(most)
}

/// The ramp after a submission went.
fn ramp_next(ramp: usize) -> usize {
    ramp.saturating_mul(2)
}

/// `most` parts, or fewer while the ramp is on and low.
fn ramp_parts(most: usize) -> usize {
    if ramp_first().is_none() {
        return most;
    }
    let ramp = ramped(RAMP.load(Relaxed), most);
    if ramp < most {
        STATS[RAMPED].fetch_add(1, Relaxed);
    }
    ramp
}

/// A present went: the next submissions begin the ramp again.
fn ramp_reset() {
    if let Some(first) = ramp_first() {
        RAMP.store(first, Relaxed);
    }
}

/// A submission went: the next may take twice as many.
fn ramp_grow() {
    if ramp_first().is_some() {
        RAMP.store(ramp_next(RAMP.load(Relaxed)), Relaxed);
    }
}

/// How many submissions may be in the line when a submit call returns
/// (`VICTORY_SUBMIT_DEPTH`; 64: about two frames of the title's).
fn runs_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("VICTORY_SUBMIT_DEPTH").unwrap_or(64).max(1))
}

/// How many presents may be in the line when a present call returns
/// (`VICTORY_SUBMIT_FRAMES`; 1: the frame before is sent, this one may not be;
/// 0: both are).
///
/// (May change while the title runs, `VICTORY_AB=..,VICTORY_SUBMIT_FRAMES=2`: a bound that a present compares the
/// line with when it comes and while it waits; nothing is kept by it, so the next present meets the new one, and
/// a line that holds more than the new bound is waited down by the presents that follow.)
fn frames_most() -> usize {
    crate::ab::live_value!("VICTORY_SUBMIT_FRAMES", usize, frames_of)
}

/// [`frames_most`] as the text of the variable says (none: 1).
fn frames_of(text: Option<&str>) -> usize {
    text.and_then(|v| v.trim().parse().ok()).unwrap_or(1)
}

fn number(name: &str) -> Option<usize> {
    std::env::var(name).ok()?.trim().parse().ok()
}

thread_local! {
    /// This is the thread that does what is in the line.
    static OURS: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

/// Whether submissions are prepared by a thread of ours
/// (`VICTORY_ASYNC_SUBMIT=1`; needs flights with the present sent ahead, as
/// the relay has them, and lists cut at their waits).
pub(super) fn on() -> bool {
    // Diagnostics: on and off while the title runs (`crate::ab`).
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_ASYNC_SUBMIT") {
        return on && started();
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        (std::env::var_os("VICTORY_ASYNC_SUBMIT").is_some_and(|v| v == "1") || crate::list::thin::wanted()) && started()
    })
}

/// The line is about to be left for the way without it: everything in it is
/// done first. (At a present, by the title's thread that submits.)
#[cfg(feature = "diagnostics")]
pub(super) fn leave() {
    LANE.wait_presents(0);
    LANE.drain();
}

/// Starts the thread, once: whether there is one.
fn started() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if !crate::host::late_free_present() || !super::cuts() {
            tracing::warn!("VICTORY_ASYNC_SUBMIT=1 needs the relay and lists cut at their waits: submissions are prepared by the thread that makes them");
            return false;
        }
        let worker = || {
            use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
            // SAFETY: this thread's own priority.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
            crate::cores::keep_on_fast_cores("victory-submit");
            crate::threadcpu::register("submit");
            crate::host::waits::this_is_the_lane();
            OURS.set(true);
            let ended = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| LANE.serve()));
            if ended.is_err() {
                // Nothing would ever be sent again, and the title's thread
                // would stand at its next present for good.
                crate::host::fail(format_args!("the thread that prepares and sends submissions panicked"));
            }
        };
        match std::thread::Builder::new().name("victory-submit".into()).spawn(worker) {
            Ok(_) => {
                tracing::info!(
                    frames = frames_most(),
                    runs = runs_most(),
                    "submissions are prepared and sent by a thread of ours, not by the thread that submits (VICTORY_ASYNC_SUBMIT=1)"
                );
                true
            }
            Err(e) => {
                tracing::warn!(error = %e, "no thread to prepare submissions: the threads that make them do");
                false
            }
        }
    })
}

/// Submissions in the line that the host has not begun
/// (`crate::host::retire` waits for them too).
pub(super) fn queued() -> u64 {
    QUEUED.load(Relaxed)
}

/// Something a queue may stand at moved.
pub(super) fn wake() {
    LANE.wake();
}

/// This thread does work the line waits for.
pub(super) fn no_drain_here() {
    OURS.set(true);
}

/// Before something that has to see the submissions made so far sent: they
/// are, as far as they are free to run. Not on our own thread, which is
/// inside one of them.
pub(super) fn drain() {
    if OURS.get() {
        return;
    }
    let began = Instant::now();
    LANE.drain();
    STATS[DRAINING].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    STATS[DRAINS].fetch_add(1, Relaxed);
}

/// A submission of `lists` to queue `this`.
pub(super) fn submit(this: This, lists: &[This]) {
    let queue = this as usize;
    if crate::list::thin::on() {
        let recordings = crate::list::thin::submitted(lists);
        if recordings.is_empty() {
            return;
        }
        let before = QUEUED.fetch_add(1, Relaxed);
        STATS[DEPTH].fetch_add(before, Relaxed);
        STATS[DEEPEST].fetch_max(before + 1, Relaxed);
        LANE.push(queue, [Job::Lists(queue, recordings)]);
        room(before + 1);
        return;
    }
    let jobs = jobs_of(this, lists);
    let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
    let before = QUEUED.fetch_add(runs, Relaxed);
    STATS[RUNS].fetch_add(runs, Relaxed);
    STATS[DEPTH].fetch_add(before, Relaxed);
    STATS[DEEPEST].fetch_max(before + runs, Relaxed);
    LANE.push(queue, jobs);
    room(before + runs);
}

/// Waits for room in the line, with `queued` submissions in it.
fn room(queued: u64) {
    if queued > runs_most() as u64 {
        let began = Instant::now();
        let _waiting = crate::timeline::span("a submission waits for room in the line");
        LANE.wait_runs(runs_most());
        STATS[ROOM].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

/// What a submission of `lists` is in the line.
fn jobs_of(this: This, lists: &[This]) -> Vec<Job> {
    let queue = this as usize;
    // In one piece up to the first list that waits for a memory value, as
    // `super::submit_cut` runs them; the rest part by part.
    let first = lists.iter().position(|&l| crate::list::cut_list(l)).unwrap_or(lists.len());
    #[cfg(feature = "diagnostics")]
    crate::capture::op(queue, || {
        let named: Vec<String> = lists
            .iter()
            .map(|&l| match crate::list::host_list(l) {
                Some(h) => format!("{:#x}", std::ptr::from_ref(h) as usize),
                None => format!("title {l:?}"),
            })
            .collect();
        format!(
            "SUBMIT queue kind {} lists {named:?} to the line (the first {first} in one piece)",
            super::queue(this).kind
        )
    });
    let mut jobs: Vec<Job> = Vec::new();
    if first > 0 {
        let (taken, order) = crate::list::detach(&lists[..first]);
        jobs.push(Job::Run(queue, taken, order));
    }
    if first < lists.len() {
        for (waits, taken, order) in crate::list::detach_parts(&lists[first..]) {
            jobs.extend(waits.into_iter().map(Job::Value));
            jobs.push(Job::Run(queue, taken, order));
        }
    }
    jobs
}

/// A signal of queue `this`: behind what it has in the line; if that is
/// nothing, behind the flights in the air, or now.
pub(super) fn signal(this: This, fence: This, value: u64) {
    let fence = fence as usize;
    LANE.push_behind(this as usize, false, Job::Signal(fence, value), || {
        if !crate::host::late_signal(fence, value) {
            crate::fence::set(fence as This, value);
        }
    });
}

/// Queue `this` waits for `fence`: nothing if it has nothing in the line and
/// the fence is there already.
pub(super) fn wait(this: This, fence: This, value: u64) {
    let always = crate::fence::completed(fence) < value;
    LANE.push_behind(this as usize, always, Job::Wait(fence as usize, value), || {});
}

/// A present on queue `this` of `texture`, in the state the lists recorded so
/// far leave it in.
pub(super) fn present(
    this: This,
    texture: Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>,
    overlay: Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>,
    shown: super::Shown,
) {
    QUEUED.fetch_add(1, Relaxed);
    STATS[PRESENTS].fetch_add(1, Relaxed);
    LANE.push(this as usize, [Job::Present(texture, overlay, shown, Instant::now())]);
    let began = Instant::now();
    let _waiting = crate::timeline::span("a present waits for the one before it to be sent");
    LANE.wait_presents(frames_most());
    STATS[AT_PRESENT].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
}

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Relaxed) as f64).collect();
    let ms = |i: usize| s[i] / 1000.0 / frames;
    tracing::info!(
        "submissions prepared by a thread of ours, per frame (VICTORY_ASYNC_SUBMIT): {:.1} in {:.1} presents, {:.1} in the line \
         when one was added (at most {:.0}); that thread worked {:.1} ms; threads of the title waited {:.1} ms for room in the \
         line, {:.1} ms at presents, {:.1} ms in {:.1} drains; waits not met at a present {:.2}; {:.1} of the \
         submissions were prepared together with the one before them (VICTORY_JOIN_PARTS); {:.1} attempts to join were cut short to fewer parts by the ramp after a present (VICTORY_JOIN_RAMP)",
        s[RUNS] / frames,
        s[PRESENTS] / frames,
        s[DEPTH] / s[RUNS].max(1.0),
        s[DEEPEST],
        ms(BUSY),
        ms(ROOM),
        ms(AT_PRESENT),
        ms(DRAINING),
        s[DRAINS] / frames,
        s[FORCED] / frames,
        s[JOINED] / frames,
        s[RAMPED] / frames
    );
    let t: Vec<f64> = STOPS.iter().map(|s| s.swap(0, Relaxed) as f64 / frames).collect();
    tracing::info!(
        "what ended the joining of parts, per frame (VICTORY_JOIN_PARTS, VICTORY_JOIN_WIDE_MB): a wait for a value not met {:.1}, a wait for a fence          nothing along signals {:.1}, the most parts {:.1}, the most lists {:.1}, a copy or a step of ours in the next part {:.1}, the next part reads          what the parts along write {:.1} (of them {:.1} only because of a range of 4 MiB or more declared writable), a present {:.1}, a call not          translated yet or parts along {:.1}, nothing more in the line {:.1}",
        t[STOP_VALUE],
        t[STOP_FENCE],
        t[STOP_PARTS],
        t[STOP_LISTS],
        t[STOP_STEP],
        t[STOP_WRITTEN] + t[STOP_WIDE],
        t[STOP_WIDE],
        t[STOP_PRESENT],
        t[STOP_OTHER],
        t[STOP_EMPTY]
    );
    if s[PRESENTS] > 0.0 {
        tracing::info!(
            "the line behind the title, per present: {:.1} ms from the title's present call to the present done by our thread              (VICTORY_SUBMIT_FRAMES lets the title be that many frames ahead)",
            s[LAG] / 1000.0 / s[PRESENTS]
        );
    }
    crate::host::precise::line(frames);
    if let Some([splits, before, lists]) = crate::host::take_blocked_splits() {
        tracing::info!(
            "joined submissions that waited for flights, per frame (VICTORY_SPLIT_BLOCKED): {:.2} with parts in front of the wait \
             ({:.1} lists in those parts of {:.1} in all; with =1 they are prepared and sent before the wait, with =2 only counted)",
            splits as f64 / frames,
            before as f64 / frames,
            lists as f64 / frames
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    type Log = Arc<Mutex<Vec<String>>>;

    /// A job of a test: what it stands at, and what it does.
    enum T {
        /// Stands until the fence has the value.
        Wait(usize, u64),
        Run(&'static str, Log),
        /// Runs once the gate is open: a submission that takes long.
        Slow(&'static str, Log, Arc<(Mutex<bool>, Condvar)>),
        Signal(usize, u64, Log),
        Present(&'static str, Log),
        /// Takes others of its kind, waits that are met, and signals along.
        Group(Vec<&'static str>, Log, Vec<(usize, u64)>),
        /// A submission that was given a round of the heaps' records as it
        /// came in; takes others of its kind along.
        Round(&'static str, u64, Log),
        /// A submission that says what it is to the one before it in its
        /// queue ([`Work::peek`]) and logs what the one before it saw.
        Peek(&'static str, Log),
    }

    impl Work for T {
        type Ask = (usize, u64);
        fn ask(&self) -> Option<(usize, u64)> {
            match self {
                &T::Wait(fence, value) => Some((fence, value)),
                _ => None,
            }
        }
        fn met(ask: &(usize, u64)) -> bool {
            crate::fence::completed(ask.0 as This) >= ask.1
        }
        fn shows(&self) -> bool {
            matches!(self, T::Present(..))
        }
        fn runs(&self) -> bool {
            matches!(self, T::Run(..) | T::Slow(..) | T::Group(..) | T::Round(..) | T::Peek(..))
        }
        fn peek(&self) -> Option<Vec<usize>> {
            match self {
                T::Peek(name, _) => Some(vec![name.len(), name.as_bytes()[0] as usize]),
                T::Group(names, ..) => Some(names.iter().map(|n| n.len()).collect()),
                _ => None,
            }
        }
        fn round(&self) -> u64 {
            match self {
                &T::Round(_, round, _) => round,
                _ => 0,
            }
        }
        fn take_along(&mut self, next: T) -> Result<bool, T> {
            if let T::Round(..) = self {
                return if matches!(next, T::Round(..)) { Ok(true) } else { Err(next) };
            }
            let T::Group(names, _, signals) = self else {
                return Err(next);
            };
            match next {
                T::Group(more, ..) if names.len() < 3 => {
                    names.extend(more);
                    Ok(true)
                }
                T::Wait(f, v) if crate::fence::completed(f as This) >= v || signals.contains(&(f, v)) => Ok(false),
                T::Signal(f, v, _) => {
                    signals.push((f, v));
                    Ok(false)
                }
                other => Err(other),
            }
        }
        fn go(self, forced: bool) {
            match self {
                T::Wait(..) => {
                    if forced {
                        FORCED_HERE.with(|f| f.set(f.get() + 1));
                    }
                }
                T::Run(name, log) | T::Present(name, log) => log.lock().unwrap().push(name.into()),
                T::Round(name, _, log) => log.lock().unwrap().push(format!("{name} round {}", round())),
                T::Peek(name, log) => {
                    let next = take_ahead();
                    log.lock().unwrap().push(format!("{name} saw {next:?}"));
                    // Asked once: it is gone.
                    assert!(take_ahead().is_none());
                }
                T::Slow(name, log, gate) => {
                    let mut open = gate.0.lock().unwrap();
                    while !*open {
                        open = gate.1.wait(open).unwrap();
                    }
                    log.lock().unwrap().push(name.into());
                }
                T::Signal(fence, value, log) => {
                    log.lock().unwrap().push(format!("signal {value}"));
                    crate::fence::set(fence as This, value);
                }
                T::Group(names, log, signals) => {
                    log.lock().unwrap().push(names.join("+"));
                    for (fence, value) in signals {
                        crate::fence::set(fence as This, value);
                    }
                }
            }
        }
    }

    thread_local! {
        static FORCED_HERE: std::cell::Cell<u32> = const { std::cell::Cell::new(0) };
    }

    fn fence() -> usize {
        let mut f = std::ptr::null();
        assert_eq!(crate::fence::create(std::ptr::null(), 0, 0, &crate::fence::FENCE_IIDS[0], &mut f), super::super::S_OK);
        f as usize
    }

    fn value(f: usize) -> u64 {
        crate::fence::completed(f as This)
    }

    fn look(lane: &Lane<T>) {
        let limit = lane.lock().next;
        lane.look(limit);
    }

    /// A queue's order is kept, a wait for another queue's signal holds only
    /// what is behind it on its own queue, and the oldest goes first.
    #[test]
    fn queues_keep_their_order_and_wait_for_each_others_fences() {
        let lane = Lane::new();
        let log: Log = Default::default();
        let f = fence();
        let run = |name| T::Run(name, log.clone());
        // The compute queue waits for what the graphics queue signals later.
        lane.push(2, [T::Wait(f, 1), run("compute")]);
        lane.push(1, [run("draw 1")]);
        lane.push_behind(1, false, T::Signal(f, 1, log.clone()), || panic!("the queue is not empty"));
        lane.push(1, [run("draw 2")]);
        look(&lane);
        assert_eq!(*log.lock().unwrap(), ["draw 1", "signal 1", "compute", "draw 2"]);
        assert_eq!(value(f), 1);
        // Nothing in the line: a signal is at once.
        let mut done = false;
        lane.push_behind(1, false, T::Signal(f, 2, log.clone()), || done = true);
        assert!(done);
    }

    /// After a present a submission takes one part (or the first's), then two,
    /// four, up to what joins allow; a ramp not begun lets all be joined.
    #[test]
    fn the_ramp_after_a_present_doubles_up_to_the_joins() {
        let (mut ramp, most) = (1usize, 8usize);
        let mut sent = Vec::new();
        for _ in 0..6 {
            sent.push(ramped(ramp, most));
            ramp = ramp_next(ramp);
        }
        assert_eq!(sent, [1, 2, 4, 8, 8, 8]);
        assert_eq!(ramped(usize::MAX, 8), 8);
        assert_eq!(ramp_next(usize::MAX), usize::MAX);
        // A first of 3 goes 3, 6, then all that are allowed.
        let (mut ramp, mut sent) = (3usize, Vec::new());
        for _ in 0..3 {
            sent.push(ramped(ramp, 8));
            ramp = ramp_next(ramp);
        }
        assert_eq!(sent, [3, 6, 8]);
    }

    /// A submission is done with the round of the heaps' records asked for the
    /// last of what it took along (they are in the order of the line, so the
    /// latest ask is the one that began after all of them), and the round is
    /// forgotten when it is done.
    #[test]
    fn a_submission_is_done_with_the_round_asked_for_what_it_took_along() {
        let lane = Lane::new();
        let log: Log = Default::default();
        lane.push(1, [T::Round("a", 5, log.clone()), T::Round("b", 9, log.clone())]);
        look(&lane);
        lane.push(2, [T::Round("c", 3, log.clone())]);
        look(&lane);
        assert_eq!(*log.lock().unwrap(), ["a round 9", "c round 3"]);
        assert_eq!(round(), 0);
    }

    /// What a submission is told of the one that comes next is the next of ITS
    /// queue, whatever other queues hold, nothing of what is not a submission
    /// it can name, and it is told once.
    #[test]
    fn a_submission_sees_the_next_of_its_own_queue_and_only_once() {
        FORCE_PIPELINE.store(true, Relaxed);
        let lane = Lane::new();
        let log: Log = Default::default();
        lane.push(1, [T::Peek("a", log.clone()), T::Peek("bb", log.clone()), T::Run("plain", log.clone())]);
        lane.push(2, [T::Peek("cc", log.clone())]);
        look(&lane);
        // `a` sees `bb`, the next of queue 1 (not `cc`, which queue 2 holds);
        // `bb` is followed by a submission that names nothing; `cc` is last.
        assert_eq!(
            *log.lock().unwrap(),
            [format!("a saw {:?}", Some(vec![2usize, b'b' as usize])), "bb saw None".to_string(), "plain".into(), "cc saw None".into()]
        );
        // A split submission: the first half gives what it was told to the
        // second.
        restore_ahead(Some(vec![7]));
        let held = take_ahead();
        assert!(take_ahead().is_none());
        restore_ahead(held);
        assert_eq!(take_ahead(), Some(vec![7]));
    }

    /// While one entry is done, the first of the line takes along what it can ahead of its turn and says what it holds;
    /// when its turn comes nothing more is taken into it (what a wait that is met since would have let it take stays),
    /// and the counts come out even.
    #[test]
    fn the_first_of_the_line_takes_along_ahead_of_its_turn_and_no_more_after() {
        let lane = Lane::new();
        let log: Log = Default::default();
        let f = fence();
        let part = |name| T::Group(vec![name], log.clone(), Vec::new());
        lane.push(1, [part("a"), part("b"), T::Wait(f, 1), part("c")]);
        // Nothing is being done: nothing is prefolded.
        assert!(lane.prefold().is_none());
        lane.lock().running = Some(1);
        // (The entry of the test's own is not in the line: `a` stands for it.)
        let (number, lists, parts) = lane.prefold().expect("a submission of the queue being done is first");
        assert_eq!(number, 0);
        assert_eq!(lists, vec![1, 1], "a and b");
        assert!(parts.is_empty());
        // Asked again, it is the same, and takes nothing more.
        let again = lane.prefold().unwrap();
        assert_eq!(again.1, vec![1, 1]);
        lane.lock().running = None;
        // The wait is met by now: without the mark `a+b` would take it, and `c` too.
        crate::fence::set(f as This, 1);
        look(&lane);
        assert_eq!(*log.lock().unwrap(), ["a+b".to_string(), "c".to_string()]);
        let state = lane.lock();
        assert_eq!((state.runs, state.items.len()), (0, 0), "the counts come out even");
    }

    /// A submission takes what follows it in its queue along, up to what it
    /// cannot take, which stays at its place; the counts come out even.
    #[test]
    fn submissions_take_those_behind_them_along() {
        let lane = Lane::new();
        let log: Log = Default::default();
        let f = fence();
        let part = |name| T::Group(vec![name], log.clone(), Vec::new());
        lane.push(1, [part("a"), T::Wait(f, 0), part("b")]);
        lane.push(2, [part("other queue"), T::Wait(f, 1), part("behind a's signal")]);
        lane.push(1, [T::Signal(f, 1, log.clone()), T::Wait(f, 1), part("c"), part("d")]);
        lane.push(1, [T::Wait(f, 9), part("held"), T::Present("present", log.clone()), part("next frame")]);
        // What came after the look was asked for is not touched.
        let limit = lane.lock().next;
        lane.push(1, [part("later")]);
        lane.look(limit);
        assert_eq!(
            *log.lock().unwrap(),
            ["a+b+c", "other queue+behind a's signal", "d", "held", "present", "next frame"],
            "{:?}",
            lane.lock().items.len()
        );
        look(&lane);
        assert_eq!(log.lock().unwrap().last().unwrap(), "later");
        let state = lane.lock();
        assert_eq!((state.presents, state.runs, state.items.len()), (0, 0, 0));
    }

    /// A wait that nothing meets holds its queue until a present, which
    /// comes behind everything given before it and before what comes after.
    #[test]
    fn a_present_goes_behind_everything_before_it_met_or_not() {
        let lane = Lane::new();
        let log: Log = Default::default();
        let f = fence();
        let run = |name| T::Run(name, log.clone());
        lane.push(2, [T::Wait(f, 9), run("held")]);
        lane.push(1, [run("draw")]);
        look(&lane);
        assert_eq!(*log.lock().unwrap(), ["draw"], "the wait holds its queue and no other");
        lane.push(1, [T::Present("present", log.clone())]);
        lane.push(1, [run("next frame")]);
        lane.push(2, [T::Wait(f, 10), run("held again")]);
        look(&lane);
        assert_eq!(*log.lock().unwrap(), ["draw", "held", "present", "next frame"]);
        assert_eq!(FORCED_HERE.get(), 1);
        // The second wait is met by a signal of the CPU.
        crate::fence::set(f as This, 10);
        look(&lane);
        assert_eq!(log.lock().unwrap().last().unwrap(), "held again");
        assert_eq!(FORCED_HERE.get(), 1);
        let state = lane.lock();
        assert_eq!((state.presents, state.runs, state.items.len()), (0, 0, 0));
    }

    /// The bound of presents (`VICTORY_SUBMIT_FRAMES`) is read at each present: a larger one lets the title go on at once,
    /// a smaller one holds it until the line is down to it, and nothing else is kept between two presents.
    #[test]
    fn a_bound_of_presents_that_changes_is_met_by_the_next_present() {
        assert_eq!([None, Some("0"), Some(" 2 "), Some("x")].map(frames_of), [1, 0, 2, 1]);
        let lane: &'static Lane<T> = Box::leak(Box::new(Lane::new()));
        std::thread::spawn(|| lane.serve());
        let log: Log = Default::default();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        lane.push(1, [T::Slow("slow", log.clone(), gate.clone())]);
        lane.push(1, [T::Present("present 1", log.clone())]);
        lane.push(1, [T::Present("present 2", log.clone())]);
        // Two presents in the line behind a submission that stands: a bound of 2 lets this return at once.
        lane.wait_presents(2);
        assert!(log.lock().unwrap().is_empty());
        // The bound goes down to 1 while the line is full: the present that comes waits until the first is sent.
        let waiter = std::thread::spawn(move || lane.wait_presents(1));
        std::thread::sleep(Duration::from_millis(20));
        assert!(!waiter.is_finished(), "held while two presents are in the line");
        *gate.0.lock().unwrap() = true;
        gate.1.notify_all();
        waiter.join().unwrap();
        // And 0 until the line is empty of presents.
        lane.wait_presents(0);
        assert_eq!(*log.lock().unwrap(), ["slow", "present 1", "present 2"]);
        assert_eq!(lane.lock().presents, 0);
    }

    /// With the thread: a fence behind a submission is not reached before the
    /// submission has been done, presents hold the title back a frame later,
    /// and a drain comes back when everything was done.
    #[test]
    fn the_title_goes_on_and_is_told_in_order() {
        let lane: &'static Lane<T> = Box::leak(Box::new(Lane::new()));
        std::thread::spawn(|| lane.serve());
        let log: Log = Default::default();
        let f = fence();
        let gate = Arc::new((Mutex::new(false), Condvar::new()));
        lane.push(1, [T::Slow("slow", log.clone(), gate.clone())]);
        lane.push_behind(1, false, T::Signal(f, 1, log.clone()), || panic!("behind the submission"));
        lane.push(1, [T::Present("present 1", log.clone())]);
        // One present may be in the line: this returns with it there.
        lane.wait_presents(1);
        assert_eq!(value(f), 0);
        assert!(log.lock().unwrap().is_empty());
        // A fence behind a submission being done (no longer in the line).
        while lane.lock().running.is_none() {
            std::thread::yield_now();
        }
        lane.push(1, [T::Run("second", log.clone())]);
        lane.push(1, [T::Present("present 2", log.clone())]);
        let opener = {
            let gate = gate.clone();
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(20));
                *gate.0.lock().unwrap() = true;
                gate.1.notify_all();
            })
        };
        lane.wait_presents(1);
        assert!(value(f) == 1, "the first frame was done");
        lane.drain();
        assert_eq!(*log.lock().unwrap(), ["slow", "signal 1", "present 1", "second", "present 2"]);
        opener.join().unwrap();
        // A wait the CPU meets without telling anybody; a drain does not
        // stand at it.
        lane.push(1, [T::Wait(f, 5), T::Run("after the wait", log.clone())]);
        lane.drain();
        assert_eq!(log.lock().unwrap().len(), 5);
        lane.wait_runs(0);
        crate::fence::set(f as This, 5);
        while log.lock().unwrap().len() < 6 {
            std::thread::yield_now();
        }
    }
}
