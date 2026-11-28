//! The title's queues as two timelines, on the way to running its compute
//! queue on a queue of ours (`VICTORY_ASYNC_COMPUTE=1`, stage 1: nothing runs
//! anywhere else; `=2`, stage 2: the compute lane's submissions run on a
//! compute queue of the host, `crate::host::lanes`, which takes the lane and
//! the waits this module resolves from [`current`]).
//!
//! The title runs work on a graphics queue and a compute queue and orders
//! them with fences and memory values: the compute queue waits for what the
//! graphics queue signals or writes, and the graphics queue for what the
//! compute queue does. The host runs both on one queue, in the line's order
//! ([`super::behind`]), so every such wait is met by the order alone. To run
//! the compute work beside the graphics work, each submission has to know
//! which submissions of the other lane it waits for, and each lane needs its
//! own count of what has run and what is retired. This module keeps that,
//! from the line, in the order the line serves it:
//!
//! - [`Model::run`] numbers a submission among its lane's and says which
//!   submissions of the other lane it waits for ([`Plan::waits`]): the title's
//!   fence waits and memory-value waits met since the lane's last submission,
//!   each by the first signal or write that satisfies it.
//! - [`Fences`]: one host fence per lane, signalled after a lane's submission
//!   with its number and waited for by the submissions that depend on it. With
//!   one queue the wait is met by the queue's own order; a wait is only made
//!   for a number the host has queued the signal of ([`Model::can_wait`]), so
//!   a wrong resolution can cost a lookup and never a hang.
//! - [`Model::finish`]: the numbers a lane has finished, retired in the
//!   lane's order whatever order they finish in ([`Model::retired`]).
//!
//! The line says how often the lanes wait for each other (`line`), which is
//! what decides whether two queues could overlap anything.

use std::collections::{BTreeSet, HashMap};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, Ordering::Relaxed};

use super::This;

/// The most signals and writes kept for a fence or an address.
const KEEP: usize = 64;
/// The most waits kept for a lane that does not submit.
const ASKS_MOST: usize = 256;

/// One of the two timelines.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub(crate) enum Lane {
    Graphics,
    Compute,
}

impl Lane {
    /// The lane of a title queue of kind `kind` (0 graphics, 1 copy, 2
    /// compute): the compute queue has its own, the rest are the graphics one.
    pub(crate) fn of(kind: u32) -> Lane {
        if kind == 2 {
            Lane::Compute
        } else {
            Lane::Graphics
        }
    }

    pub(crate) fn index(self) -> usize {
        match self {
            Lane::Graphics => 0,
            Lane::Compute => 1,
        }
    }
}

/// A submission of a lane, by its number among the lane's (1, 2, ...; 0 is
/// "nothing yet").
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Dep {
    pub(crate) lane: Lane,
    pub(crate) number: u64,
}

/// A submission as the model numbered it.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub(crate) lane: Lane,
    pub(crate) number: u64,
    /// The submissions of the other lane it waits for (the latest of them).
    pub(crate) waits: Vec<Dep>,
}

/// What a lane's next submission stands behind.
enum Ask {
    Fence(usize, u64),
    Value(usize, Box<dyn Fn(u64) -> bool + Send>),
}

/// What the model counted since the last line.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(super) struct Stats {
    /// Submissions by lane.
    pub(super) runs: [u64; 2],
    /// Waits that name a submission of the other lane, by the waiting lane.
    pub(super) cross: [u64; 2],
    /// Waits met by the waiting lane's own order.
    pub(super) same: u64,
    /// Waits for a signal given before the lane had run anything.
    pub(super) nothing: u64,
    /// Waits no signal or write seen satisfies (the title waits for the CPU,
    /// or for what happened before the model was on).
    pub(super) unresolved: u64,
    /// Waits made on the host queue, and those left out because the host had
    /// not queued the signal they name.
    pub(super) made: u64,
    pub(super) refused: u64,
}

/// The two lanes' bookkeeping.
#[derive(Default)]
pub(crate) struct Model {
    /// Submissions numbered, by lane.
    ran: [u64; 2],
    /// Numbers whose signal on the lane's host fence is queued.
    signalled: [u64; 2],
    /// Numbers finished beyond the lane's retired one, and the retired one.
    finished: [BTreeSet<u64>; 2],
    retired: [u64; 2],
    /// Signals of fences (in the order they came): value and where.
    fences: HashMap<usize, Vec<(u64, Dep)>>,
    /// Writes of memory values by submissions: value and where.
    values: HashMap<usize, Vec<(u64, Dep)>>,
    /// Waits not yet put on a submission, by waiting lane.
    asks: [Vec<Ask>; 2],
    pub(super) stats: Stats,
}

impl Model {
    /// The queue of `lane` signals `fence` to `value`: after what the lane
    /// has run so far.
    pub(crate) fn signal(&mut self, lane: Lane, fence: usize, value: u64) {
        let dep = Dep {
            lane,
            number: self.ran[lane.index()],
        };
        let seen = self.fences.entry(fence).or_default();
        seen.push((value, dep));
        if seen.len() > KEEP {
            seen.remove(0);
        }
    }

    /// The queue of `lane` waits for `fence` to reach `value`: what it is
    /// given next stands behind it.
    pub(crate) fn wait_fence(&mut self, lane: Lane, fence: usize, value: u64) {
        self.ask(lane, Ask::Fence(fence, value));
    }

    /// The queue of `lane` waits for the memory at `address` to hold what
    /// `holds` accepts.
    pub(crate) fn wait_value(
        &mut self,
        lane: Lane,
        address: usize,
        holds: Box<dyn Fn(u64) -> bool + Send>,
    ) {
        self.ask(lane, Ask::Value(address, holds));
    }

    fn ask(&mut self, lane: Lane, ask: Ask) {
        let asks = &mut self.asks[lane.index()];
        if asks.len() >= ASKS_MOST {
            asks.remove(0);
        }
        asks.push(ask);
    }

    /// A submission of `lane` goes, with the values its lists write at the end
    /// (`writes`: address and value).
    pub(crate) fn run(&mut self, lane: Lane, writes: &[(usize, u64)]) -> Plan {
        let i = lane.index();
        self.ran[i] += 1;
        self.stats.runs[i] += 1;
        let number = self.ran[i];
        let mut latest: [u64; 2] = [0; 2];
        for ask in std::mem::take(&mut self.asks[i]) {
            let found = match &ask {
                Ask::Fence(fence, value) => self
                    .fences
                    .get(fence)
                    .and_then(|seen| seen.iter().find(|(at, _)| at >= value))
                    .map(|&(_, dep)| dep),
                Ask::Value(address, holds) => self
                    .values
                    .get(address)
                    .and_then(|seen| seen.iter().find(|(v, _)| holds(*v)))
                    .map(|&(_, dep)| dep),
            };
            match found {
                None => self.stats.unresolved += 1,
                Some(dep) if dep.lane == lane => self.stats.same += 1,
                Some(dep) if dep.number == 0 => self.stats.nothing += 1,
                Some(dep) => {
                    self.stats.cross[i] += 1;
                    latest[dep.lane.index()] = latest[dep.lane.index()].max(dep.number);
                }
            }
        }
        let other = 1 - i;
        let waits = if latest[other] > 0 {
            vec![Dep {
                lane: if other == 0 {
                    Lane::Graphics
                } else {
                    Lane::Compute
                },
                number: latest[other],
            }]
        } else {
            Vec::new()
        };
        for &(address, value) in writes {
            let seen = self.values.entry(address).or_default();
            seen.push((value, Dep { lane, number }));
            if seen.len() > KEEP {
                seen.remove(0);
            }
        }
        Plan {
            lane,
            number,
            waits,
        }
    }

    /// The host has queued the signal of `number` on `lane`'s fence.
    pub(super) fn signalled(&mut self, lane: Lane, number: u64) {
        let at = &mut self.signalled[lane.index()];
        *at = (*at).max(number);
    }

    /// Whether a wait for `dep` may be queued: its signal is.
    pub(super) fn can_wait(&mut self, dep: Dep) -> bool {
        let ok = dep.number <= self.signalled[dep.lane.index()];
        if ok {
            self.stats.made += 1;
        } else {
            self.stats.refused += 1;
        }
        ok
    }

    /// Submission `number` of `lane` has finished (in any order). (Stage 2:
    /// the lanes' retirement.)
    #[allow(dead_code)]
    pub(super) fn finish(&mut self, lane: Lane, number: u64) {
        let i = lane.index();
        if number <= self.retired[i] {
            return;
        }
        self.finished[i].insert(number);
        while self.finished[i].remove(&(self.retired[i] + 1)) {
            self.retired[i] += 1;
        }
    }

    /// The number up to which every submission of `lane` has finished.
    #[allow(dead_code)]
    pub(super) fn retired(&self, lane: Lane) -> u64 {
        self.retired[lane.index()]
    }
}

static MODEL: Mutex<Option<Model>> = Mutex::new(None);

fn with<T>(f: impl FnOnce(&mut Model) -> T) -> T {
    let mut model = MODEL.lock().unwrap_or_else(|e| e.into_inner());
    f(model.get_or_insert_with(Model::default))
}

/// What `VICTORY_ASYNC_COMPUTE` says: 0 off, 1 the line's compute work is
/// numbered and its waits resolved with the host queue given the lanes' fences
/// (stage 1: nothing runs elsewhere), 2 the compute lane's submissions run on a
/// queue of the host's own (stage 2, `crate::host::lanes`).
pub(crate) fn mode_of(text: Option<&str>) -> u8 {
    match text.map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    }
}

/// [`mode_of`] of the switch, read at each use (`VICTORY_AB`: `VICTORY_AB=10,VICTORY_ASYNC_COMPUTE=2` is stage 2 in
/// the phases that have its bit, off in the others; without a value, stage 1): a switch that comes on finds the
/// lanes as they are (waits it did not see are unresolved, counted, and never waited for), one that goes off leaves
/// fences and numbers that nothing reads. Needs the submit line.
pub(crate) fn mode() -> u8 {
    static SEEN: AtomicBool = AtomicBool::new(false);
    let mode = crate::ab::live_value!("VICTORY_ASYNC_COMPUTE", u8, mode_of);
    let mode = if mode > 0 && super::behind::on() { mode } else { 0 };
    if mode == 1 && !SEEN.swap(true, Relaxed) {
        tracing::info!(
            "VICTORY_ASYNC_COMPUTE: the title's compute queue is numbered as a timeline of its own (stage 1: one host queue)"
        );
    }
    mode
}

/// Whether the submit line is on (stage 2 needs it: the lists of the compute type only ever go through it).
pub(crate) fn line_on() -> bool {
    super::behind::on()
}

/// Whether the lanes are numbered at all (stages 1 and 2).
pub(super) fn on() -> bool {
    mode() > 0
}

/// The submission the line's thread is sending now, among the title's lanes
/// (stage 2): its lane and the submissions of the other lane it waits for, as
/// the title's waits resolve.
#[derive(Clone, Debug)]
pub(crate) struct Current {
    pub(crate) lane: Lane,
    pub(crate) waits: Vec<Dep>,
}

thread_local! {
    static CURRENT: std::cell::RefCell<Option<Current>> = const { std::cell::RefCell::new(None) };
}

/// [`Current`] on the line's thread, between [`begin`] and [`end`] of a
/// submission in stage 2.
pub(crate) fn current() -> Option<Current> {
    CURRENT.with(|c| c.borrow().clone())
}

fn lane_of(queue: usize) -> Lane {
    Lane::of(super::queue(queue as This).kind)
}

/// The line serves a signal of `queue`.
pub(super) fn signal(queue: usize, fence: usize, value: u64) {
    if on() {
        with(|m| m.signal(lane_of(queue), fence, value));
    }
}

/// The line serves a wait of `queue` for a fence.
pub(super) fn wait(queue: usize, fence: usize, value: u64) {
    if on() {
        with(|m| m.wait_fence(lane_of(queue), fence, value));
    }
}

/// The line serves a wait of `queue` for a memory value.
pub(super) fn value_wait(queue: usize, wait: &crate::list::ValueWait) {
    if on() {
        let (address, _) = wait.place();
        let wait = *wait;
        with(|m| {
            m.wait_value(
                lane_of(queue),
                address,
                Box::new(move |v| wait.holds_for(v)),
            )
        });
    }
}

/// A submission of `queue` is about to go, with the values its lists write.
/// The waits of it on the other lane are made on the host queue (stage 1), or
/// left to `crate::host::lanes` with [`current`] (stage 2).
pub(super) fn begin(
    host: &crate::host::Host,
    queue: usize,
    writes: &[(usize, u64)],
) -> Option<Plan> {
    let mode = mode();
    if mode == 0 {
        return None;
    }
    let plan = with(|m| m.run(lane_of(queue), writes));
    if mode == 2 {
        CURRENT.with(|c| {
            *c.borrow_mut() = Some(Current {
                lane: plan.lane,
                waits: plan.waits.clone(),
            });
        });
        return Some(plan);
    }
    let fences = fences(host)?;
    for dep in &plan.waits {
        if with(|m| m.can_wait(*dep)) {
            host.queue_wait(&fences[dep.lane.index()], dep.number);
        }
    }
    Some(plan)
}

/// The submission of `plan` has been given to the host queue: its lane's
/// fence is signalled with its number (stage 1), or the lanes are told where
/// it went (stage 2).
pub(super) fn end(host: &crate::host::Host, plan: Option<Plan>) {
    let Some(plan) = plan else {
        return;
    };
    if CURRENT.with(|c| c.borrow_mut().take()).is_some() {
        crate::host::lanes::planned(plan.lane, plan.number);
        with(|m| m.signalled(plan.lane, plan.number));
        return;
    }
    if let Some(fences) = fences(host) {
        host.queue_signal(&fences[plan.lane.index()], plan.number);
        with(|m| m.signalled(plan.lane, plan.number));
    }
}

#[cfg(windows)]
type Fence = windows::Win32::Graphics::Direct3D12::ID3D12Fence;

/// The lanes' fences, which any thread may signal and wait on.
#[cfg(windows)]
struct LaneFences([Fence; 2]);

// SAFETY: D3D12 fences are free-threaded.
#[cfg(windows)]
unsafe impl Send for LaneFences {}
// SAFETY: as above.
#[cfg(windows)]
unsafe impl Sync for LaneFences {}

/// One host fence per lane, made at the first use.
#[cfg(windows)]
fn fences(host: &crate::host::Host) -> Option<&'static [Fence; 2]> {
    static FENCES: std::sync::OnceLock<Option<LaneFences>> = std::sync::OnceLock::new();
    FENCES.get_or_init(|| make_fences(&host.device).ok().map(LaneFences)).as_ref().map(|f| &f.0)
}

/// Two fences for the lanes.
#[cfg(windows)]
fn make_fences(
    device: &windows::Win32::Graphics::Direct3D12::ID3D12Device,
) -> windows::core::Result<[Fence; 2]> {
    use windows::Win32::Graphics::Direct3D12::D3D12_FENCE_FLAG_NONE;
    // SAFETY: plain object creation.
    unsafe {
        Ok([
            device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
            device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
        ])
    }
}

/// For the frame-rate line: the lanes' submissions and how they wait for each
/// other, per frame.
pub(super) fn line(frames: f64) {
    if !on() {
        return;
    }
    let s = with(|m| std::mem::take(&mut m.stats));
    let f = |n: u64| n as f64 / frames;
    tracing::info!(
        "the title's queues as two timelines per frame (VICTORY_ASYNC_COMPUTE, {}): {:.1} submissions on the graphics lane, {:.1} on the compute lane; \
         compute waits for graphics {:.1} times, graphics waits for compute {:.1} times, {:.1} waits met by the lane's own order, {:.1} for a signal given before \
         anything ran, {:.1} not resolved; host waits made {:.1}, left out {:.1}",
        if crate::host::lanes::lists_compute() { "two host queues" } else { "one host queue" },
        f(s.runs[0]),
        f(s.runs[1]),
        f(s.cross[1]),
        f(s.cross[0]),
        f(s.same),
        f(s.nothing),
        f(s.unresolved),
        f(s.made),
        f(s.refused)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write(model: &mut Model, lane: Lane, address: usize, value: u64) -> Plan {
        model.run(lane, &[(address, value)])
    }

    #[test]
    fn the_title_queues_are_two_lanes() {
        assert_eq!(Lane::of(0), Lane::Graphics);
        assert_eq!(Lane::of(1), Lane::Graphics);
        assert_eq!(Lane::of(2), Lane::Compute);
        assert_eq!((Lane::Graphics.index(), Lane::Compute.index()), (0, 1));
    }

    #[test]
    fn submissions_are_numbered_among_their_lanes() {
        let mut m = Model::default();
        let numbers: Vec<(Lane, u64)> = [
            Lane::Graphics,
            Lane::Compute,
            Lane::Graphics,
            Lane::Graphics,
            Lane::Compute,
        ]
        .into_iter()
        .map(|lane| {
            let plan = m.run(lane, &[]);
            assert!(plan.waits.is_empty());
            (plan.lane, plan.number)
        })
        .collect();
        assert_eq!(
            numbers,
            [
                (Lane::Graphics, 1),
                (Lane::Compute, 1),
                (Lane::Graphics, 2),
                (Lane::Graphics, 3),
                (Lane::Compute, 2)
            ]
        );
        assert_eq!(m.stats.runs, [3, 2]);
    }

    /// The compute queue waits for what the graphics queue signals, also when
    /// the wait is given before the signal.
    #[test]
    fn a_fence_wait_names_the_submission_the_signal_came_after() {
        let mut m = Model::default();
        m.run(Lane::Graphics, &[]);
        m.run(Lane::Graphics, &[]);
        m.signal(Lane::Graphics, 7, 5);
        m.run(Lane::Graphics, &[]);
        m.wait_fence(Lane::Compute, 7, 5);
        let plan = m.run(Lane::Compute, &[]);
        assert_eq!(
            plan.waits,
            [Dep {
                lane: Lane::Graphics,
                number: 2
            }]
        );
        assert_eq!(m.stats.cross, [0, 1]);
        // Graphics waits for the compute lane's own signal.
        m.signal(Lane::Compute, 8, 1);
        m.wait_fence(Lane::Graphics, 8, 1);
        let plan = m.run(Lane::Graphics, &[]);
        assert_eq!(
            plan.waits,
            [Dep {
                lane: Lane::Compute,
                number: 1
            }]
        );
        assert_eq!(m.stats.cross, [1, 1]);
    }

    #[test]
    fn the_first_signal_that_reaches_the_value_is_the_one() {
        let mut m = Model::default();
        m.run(Lane::Graphics, &[]);
        m.signal(Lane::Graphics, 1, 1);
        m.run(Lane::Graphics, &[]);
        m.signal(Lane::Graphics, 1, 2);
        m.run(Lane::Graphics, &[]);
        m.signal(Lane::Graphics, 1, 3);
        for (value, number) in [(1, 1), (2, 2), (3, 3), (0, 1)] {
            m.wait_fence(Lane::Compute, 1, value);
            let plan = m.run(Lane::Compute, &[]);
            assert_eq!(
                plan.waits,
                [Dep {
                    lane: Lane::Graphics,
                    number
                }],
                "a wait for {value}"
            );
        }
        // Not signalled that far: unresolved, and no dependency.
        m.wait_fence(Lane::Compute, 1, 9);
        let plan = m.run(Lane::Compute, &[]);
        assert!(plan.waits.is_empty());
        assert_eq!(m.stats.unresolved, 1);
    }

    #[test]
    fn several_waits_of_a_submission_are_the_latest_of_them() {
        let mut m = Model::default();
        for (fence, value) in [(1, 1), (2, 1), (3, 1)] {
            m.run(Lane::Graphics, &[]);
            m.signal(Lane::Graphics, fence, value);
        }
        for fence in [2, 1, 3] {
            m.wait_fence(Lane::Compute, fence, 1);
        }
        let plan = m.run(Lane::Compute, &[]);
        assert_eq!(
            plan.waits,
            [Dep {
                lane: Lane::Graphics,
                number: 3
            }]
        );
        // The waits went with that submission: the next has none.
        assert!(m.run(Lane::Compute, &[]).waits.is_empty());
    }

    #[test]
    fn a_wait_for_the_lanes_own_work_or_for_nothing_is_no_dependency() {
        let mut m = Model::default();
        // A signal before anything ran on its lane.
        m.signal(Lane::Graphics, 1, 1);
        m.wait_fence(Lane::Compute, 1, 1);
        assert!(m.run(Lane::Compute, &[]).waits.is_empty());
        assert_eq!(m.stats.nothing, 1);
        // The compute lane waits for its own signal.
        m.signal(Lane::Compute, 2, 1);
        m.wait_fence(Lane::Compute, 2, 1);
        assert!(m.run(Lane::Compute, &[]).waits.is_empty());
        assert_eq!(m.stats.same, 1);
    }

    #[test]
    fn a_value_wait_names_the_submission_that_wrote_what_it_accepts() {
        let mut m = Model::default();
        write(&mut m, Lane::Graphics, 0x1000, 3);
        write(&mut m, Lane::Graphics, 0x1000, 7);
        write(&mut m, Lane::Graphics, 0x2000, 7);
        m.wait_value(Lane::Compute, 0x1000, Box::new(|v| v >= 5));
        let plan = m.run(Lane::Compute, &[]);
        assert_eq!(
            plan.waits,
            [Dep {
                lane: Lane::Graphics,
                number: 2
            }]
        );
        m.wait_value(Lane::Compute, 0x3000, Box::new(|_| true));
        assert!(m.run(Lane::Compute, &[]).waits.is_empty());
        assert_eq!(m.stats.unresolved, 1);
    }

    #[test]
    fn what_a_lane_finishes_out_of_order_is_retired_in_order() {
        let mut m = Model::default();
        for _ in 0..5 {
            m.run(Lane::Compute, &[]);
        }
        m.finish(Lane::Compute, 3);
        m.finish(Lane::Compute, 2);
        assert_eq!(m.retired(Lane::Compute), 0, "1 is not finished");
        m.finish(Lane::Compute, 1);
        assert_eq!(m.retired(Lane::Compute), 3);
        m.finish(Lane::Compute, 5);
        m.finish(Lane::Compute, 1);
        assert_eq!(m.retired(Lane::Compute), 3);
        m.finish(Lane::Compute, 4);
        assert_eq!(m.retired(Lane::Compute), 5);
        assert_eq!(m.retired(Lane::Graphics), 0, "the lanes are apart");
    }

    /// A wait is only made for a signal the host has queued.
    #[test]
    fn a_host_wait_is_made_only_for_a_signal_that_is_queued() {
        let mut m = Model::default();
        let dep = Dep {
            lane: Lane::Graphics,
            number: 2,
        };
        assert!(!m.can_wait(dep));
        m.signalled(Lane::Graphics, 1);
        assert!(!m.can_wait(dep));
        m.signalled(Lane::Graphics, 2);
        m.signalled(Lane::Graphics, 1);
        assert!(m.can_wait(dep));
        assert!(!m.can_wait(Dep {
            lane: Lane::Compute,
            number: 1
        }));
        assert_eq!((m.stats.made, m.stats.refused), (1, 3));
    }

    /// Random titles: whatever the waits and signals, a dependency names a
    /// submission of the other lane that has already been numbered, and a lane
    /// never depends on a later submission of its own.
    #[test]
    fn dependencies_only_name_submissions_that_exist() {
        let mut seed = 0x2545_f491_4f6c_dd1d_u64;
        let mut next = move |n: u64| {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed % n
        };
        for _ in 0..200 {
            let mut m = Model::default();
            let mut signalled = [0u64; 2];
            for _ in 0..200 {
                let lane = if next(2) == 0 {
                    Lane::Graphics
                } else {
                    Lane::Compute
                };
                match next(4) {
                    0 => m.signal(lane, next(3) as usize, next(6)),
                    1 => m.wait_fence(lane, next(3) as usize, next(6)),
                    2 => m.wait_value(lane, next(3) as usize, Box::new(move |v| v >= 2)),
                    _ => {
                        let plan = m.run(lane, &[(next(3) as usize, next(6))]);
                        for dep in &plan.waits {
                            assert_ne!(dep.lane, plan.lane);
                            assert!(dep.number >= 1 && dep.number <= m.ran[dep.lane.index()]);
                            // A wait goes out only once its signal is queued.
                            if m.can_wait(*dep) {
                                assert!(dep.number <= signalled[dep.lane.index()]);
                            }
                        }
                        // The host queues the signal after the submission.
                        signalled[lane.index()] = plan.number;
                        m.signalled(lane, plan.number);
                    }
                }
            }
        }
    }
}

/// The lane fences on the software adapter: a queue that waits for a lane's
/// number goes on when another queue signals it, and not before.
#[cfg(all(test, windows))]
mod warp_tests {
    use super::*;
    use windows::Win32::Foundation::WAIT_OBJECT_0;
    use windows::Win32::Graphics::Direct3D12::*;
    use windows::Win32::Graphics::Dxgi::*;
    use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject};
    use windows::core::Interface;

    fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        // SAFETY: plain DXGI queries and device creation on the software
        // adapter; a card is never opened.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                return None;
            }
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    fn queue(device: &ID3D12Device, kind: D3D12_COMMAND_LIST_TYPE) -> ID3D12CommandQueue {
        // SAFETY: plain object creation.
        unsafe {
            device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: kind,
                    ..Default::default()
                })
                .expect("a queue")
        }
    }

    /// Waits for `value` on `fence` up to `ms`.
    fn reached(fence: &ID3D12Fence, value: u64, ms: u32) -> bool {
        // SAFETY: an event of the test's own.
        unsafe {
            if fence.GetCompletedValue() >= value {
                return true;
            }
            let event = CreateEventW(None, false, false, None).expect("an event");
            fence
                .SetEventOnCompletion(value, event)
                .expect("an event on the fence");
            WaitForSingleObject(event, ms) == WAIT_OBJECT_0
        }
    }

    #[test]
    fn a_compute_queue_goes_on_when_the_graphics_queue_signals_its_number() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        let direct = queue(&device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        let compute = queue(&device, D3D12_COMMAND_LIST_TYPE_COMPUTE);
        let lanes = make_fences(&device).expect("lane fences");
        // SAFETY: our queues and fences.
        unsafe {
            let ran: ID3D12Fence = device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .expect("a fence");
            // The compute lane's submission 1 depends on graphics number 2.
            compute
                .Wait(&lanes[Lane::Graphics.index()], 2)
                .expect("a wait");
            compute
                .Signal(&lanes[Lane::Compute.index()], 1)
                .expect("a signal");
            compute.Signal(&ran, 1).expect("a signal");
            assert!(!reached(&ran, 1, 150), "it stands at the wait");
            assert_eq!(lanes[Lane::Compute.index()].GetCompletedValue(), 0);
            // Graphics number 1 is not enough, 2 is.
            direct
                .Signal(&lanes[Lane::Graphics.index()], 1)
                .expect("a signal");
            assert!(!reached(&ran, 1, 150), "number 1 does not release it");
            direct
                .Signal(&lanes[Lane::Graphics.index()], 2)
                .expect("a signal");
            assert!(reached(&ran, 1, 10_000), "number 2 does");
            assert_eq!(lanes[Lane::Compute.index()].GetCompletedValue(), 1);
            // The graphics queue waits for the compute lane's number 1: met.
            direct
                .Wait(&lanes[Lane::Compute.index()], 1)
                .expect("a wait");
            direct.Signal(&ran, 2).expect("a signal");
            assert!(reached(&ran, 2, 10_000));
        }
    }

    /// One queue, both lanes: a wait for a number the same queue signalled
    /// earlier is met by its order (what stage 1 relies on).
    #[test]
    fn on_one_queue_a_wait_for_an_earlier_signal_is_met_at_once() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        let direct = queue(&device, D3D12_COMMAND_LIST_TYPE_DIRECT);
        let lanes = make_fences(&device).expect("lane fences");
        // SAFETY: our queue and fences.
        unsafe {
            let ran: ID3D12Fence = device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .expect("a fence");
            for number in 1..=3u64 {
                direct
                    .Signal(&lanes[Lane::Graphics.index()], number)
                    .expect("a signal");
                direct
                    .Wait(&lanes[Lane::Graphics.index()], number)
                    .expect("a wait");
                direct.Signal(&ran, number).expect("a signal");
            }
            assert!(reached(&ran, 3, 10_000));
            assert_eq!(lanes[Lane::Graphics.index()].GetCompletedValue(), 3);
        }
    }
}
