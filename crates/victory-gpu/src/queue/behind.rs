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
    /// Does it. `forced`: its wait was not met.
    fn go(self, forced: bool);
}

struct Entry<W> {
    number: u64,
    queue: usize,
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
        state.items.push_back(Entry { number, queue, what });
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

    /// Does one entry: the oldest that is the first of its queue and free to
    /// run. Whether there was one. Only one thread calls this.
    fn step(&self, limit: u64) -> bool {
        // Each queue's first entry, oldest first.
        let mut heads: Vec<(u64, usize, Option<W::Ask>, bool)> = Vec::new();
        {
            let state = self.lock();
            for e in state.items.iter().take_while(|e| e.number < limit) {
                if !heads.iter().any(|h| h.1 == e.queue) {
                    heads.push((e.number, e.queue, e.what.ask(), e.what.shows()));
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
                // frames (`super::present`).
                let state = self.lock();
                if !super::forces() || !state.items.iter().any(|e| e.number < limit && e.what.shows()) {
                    return false;
                }
                (first, true)
            }
        };
        let entry = {
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
        let (shows, runs) = (entry.what.shows(), entry.what.runs());
        entry.what.go(forced);
        {
            let mut state = self.lock();
            state.running = None;
            state.presents -= usize::from(shows);
            state.runs -= usize::from(runs);
        }
        self.moved.notify_all();
        true
    }
}

/// What the title's queues put in the line.
enum Job {
    /// The queue waits for the fence (its address) to reach the value.
    Wait(usize, u64),
    /// A part of a submission waits for a memory value.
    Value(crate::list::ValueWait),
    /// A submission, or a part of one: the lists' host sides and the order
    /// they run in.
    Run(usize, Vec<crate::list::Detached>, Vec<usize>),
    /// A signal of the queue's.
    Signal(usize, u64),
    /// A frame is shown: the texture and the state its lists leave it in.
    Present(Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>, super::Shown),
}

// SAFETY: D3D12 objects are free-threaded; a job is touched by whoever put it
// in the line and then, once it is taken out, by our thread alone (the flights
// it becomes are handed to other threads the same way).
unsafe impl Send for Job {}

enum Ask {
    Wait(usize, u64),
    Value(crate::list::ValueWait),
}

impl Work for Job {
    type Ask = Ask;

    fn ask(&self) -> Option<Ask> {
        match self {
            &Job::Wait(fence, value) => Some(Ask::Wait(fence, value)),
            Job::Value(wait) => Some(Ask::Value(*wait)),
            _ => None,
        }
    }

    fn met(ask: &Ask) -> bool {
        match ask {
            &Ask::Wait(fence, value) => crate::fence::completed(fence as This) >= value,
            Ask::Value(wait) => super::value_met(wait),
        }
    }

    fn shows(&self) -> bool {
        matches!(self, Job::Present(..))
    }

    fn runs(&self) -> bool {
        matches!(self, Job::Run(..))
    }

    fn go(self, forced: bool) {
        let began = Instant::now();
        if forced {
            STATS[FORCED].fetch_add(1, Relaxed);
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Forced, 1);
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| tracing::warn!("a queue still stood at a wait at a present: it was not met"));
        }
        match self {
            Job::Wait(..) | Job::Value(..) => {}
            Job::Run(queue, lists, order) => {
                let _ = queue;
                #[cfg(feature = "diagnostics")]
                crate::capture::op(queue, || format!("RUN submission of {} lists (its thread's)", order.len()));
                match crate::host::host() {
                    Some(host) => host.execute_late(lists, order),
                    None => drop(lists),
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
            Job::Signal(fence, value) => {
                if !crate::host::late_signal(fence, value) {
                    crate::fence::set(fence as This, value);
                }
            }
            Job::Present(texture, shown) => {
                if let Some((texture, state)) = texture {
                    super::show(&texture.texture, state, shown);
                }
                QUEUED.fetch_sub(1, Relaxed);
            }
        }
        STATS[BUSY].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
}

static LANE: Lane<Job> = Lane::new();

/// Submissions and presents in the line that have not been given to the host
/// yet.
static QUEUED: AtomicU64 = AtomicU64::new(0);

/// Since the last frame-rate line: submissions put in the line, presents, the
/// times they found things in it (the sum of their numbers) and the most; what
/// threads of the title waited, in microseconds, for room in the line, at a
/// present, and for a drain, and the drains; what our thread worked; waits
/// not met at a present.
static STATS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];
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

/// How many submissions may be in the line when a submit call returns
/// (`VICTORY_SUBMIT_DEPTH`; 64: about two frames of the title's).
fn runs_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("VICTORY_SUBMIT_DEPTH").unwrap_or(64).max(1))
}

/// How many presents may be in the line when a present call returns
/// (`VICTORY_SUBMIT_FRAMES`; 1: the frame before is sent, this one may not be;
/// 0: both are).
fn frames_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| number("VICTORY_SUBMIT_FRAMES").unwrap_or(1))
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
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        if !std::env::var_os("VICTORY_ASYNC_SUBMIT").is_some_and(|v| v == "1") {
            return false;
        }
        if !crate::host::late_free_present() || !super::cuts() {
            tracing::warn!("VICTORY_ASYNC_SUBMIT=1 needs the relay and lists cut at their waits: submissions are prepared by the thread that makes them");
            return false;
        }
        let worker = || {
            use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
            // SAFETY: this thread's own priority.
            unsafe { SetThreadPriority(GetCurrentThread(), THREAD_PRIORITY_ABOVE_NORMAL) };
            crate::cores::keep_on_fast_cores("victory-submit");
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
    let runs = jobs.iter().filter(|j| j.runs()).count() as u64;
    let before = QUEUED.fetch_add(runs, Relaxed);
    STATS[RUNS].fetch_add(runs, Relaxed);
    STATS[DEPTH].fetch_add(before, Relaxed);
    STATS[DEEPEST].fetch_max(before + runs, Relaxed);
    LANE.push(queue, jobs);
    if before + runs > runs_most() as u64 {
        let began = Instant::now();
        LANE.wait_runs(runs_most());
        STATS[ROOM].fetch_add(began.elapsed().as_micros() as u64, Relaxed);
    }
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
pub(super) fn present(this: This, texture: Option<(std::sync::Arc<crate::resource::HostTexture>, u32)>, shown: super::Shown) {
    QUEUED.fetch_add(1, Relaxed);
    STATS[PRESENTS].fetch_add(1, Relaxed);
    LANE.push(this as usize, [Job::Present(texture, shown)]);
    let began = Instant::now();
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
         line, {:.1} ms at presents, {:.1} ms in {:.1} drains; waits not met at a present {:.2}",
        s[RUNS] / frames,
        s[PRESENTS] / frames,
        s[DEPTH] / s[RUNS].max(1.0),
        s[DEEPEST],
        ms(BUSY),
        ms(ROOM),
        ms(AT_PRESENT),
        ms(DRAINING),
        s[DRAINS] / frames,
        s[FORCED] / frames
    );
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
            matches!(self, T::Run(..) | T::Slow(..))
        }
        fn go(self, forced: bool) {
            match self {
                T::Wait(..) => {
                    if forced {
                        FORCED_HERE.with(|f| f.set(f.get() + 1));
                    }
                }
                T::Run(name, log) | T::Present(name, log) => log.lock().unwrap().push(name.into()),
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
