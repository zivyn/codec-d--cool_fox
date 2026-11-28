//! The title's compute queue on a compute queue of the host
//! (`VICTORY_ASYNC_COMPUTE=2`, stage 2; `=1` is stage 1, the bookkeeping of
//! `crate::queue::timelines`, and nothing else).
//!
//! The host gives everything the title submits, graphics and async compute, to
//! one queue. The title overlaps its compute queue with its graphics queue on
//! the console, so the card works the two one after the other where it could
//! work them side by side. With the switch the submissions of the title's
//! compute queue go to a host queue of type compute and the rest of the host
//! goes on as it was, on its own.
//!
//! The pieces, and why each is there:
//!
//! - Lists of the compute type for the title's compute lists
//!   ([`want_compute`]). A queue runs the lists of its own type only (a
//!   compute list on the drawing queue removes the device), and the host list
//!   of a title list is made before anyone knows where it goes, so the type
//!   *is* the queue: with the switch (`=2` in the variable, or in
//!   `VICTORY_AB`, for the whole process) every submission of compute lists goes
//!   to Q2 and every other one to Q1, the host's queue that there always was.
//!   What the host adds to a submission of Q2 (the lists that send up to the
//!   card's copies, the compare, the card's own steps, the preludes) is of the
//!   compute type too. What a compute queue may not do goes to Q1 around the
//!   hand-off: the transitions that name a state of drawing (the first move of a
//!   texture out of the state a drawing list left it in) are made on Q1 in a
//!   list of their own, ahead of the submission, and the compute queue waits for
//!   it ([`Route::waits`]); the states a list of the compute type moves its
//!   textures between are the non-drawing ones (`compute_legal`).
//! - Two queues, each with a fence of its own ([`Lanes::fences`]) that it
//!   signals behind each submission with the value the host numbers the
//!   submission by, and a sequencer ([`Lanes::order`], a copy queue that does no
//!   work): for each submission it waits for the fence of the queue the
//!   submission went to and then signals the host's one fence (`Host::fence`)
//!   with the same value, in the order the submissions were given. The host's
//!   fence then still says "everything up to this value has run", whichever
//!   queue ran it, so the flights, their landing, the retirement of what they
//!   used (`bury`), and every wait of the CPU for the card go on exactly as with
//!   one queue. (Two queues signalling the one fence would go back and forth
//!   over it.) A flight lands only when both queues have got to it, so what a
//!   compute flight wrote is in title memory before any later flight's landing
//!   and before the title is told (fences, values) that it is done.
//! - What a submission waits for on the other queue ([`Book::waits_for`]), with
//!   the switch's overlap (`=2` in the phases that have it): the submissions of
//!   the other lane the title's own waits resolved to (stage 1), the title
//!   queue's previous submission if that ran on the other host queue, and the
//!   submissions in the air on the other queue that it shares anything with
//!   ([`Footprint`]): what one reads or writes of title memory, whole host copies
//!   in the card's memory (the lists that send up to a copy and compare it move
//!   it between states, and share its hashes) and whole textures (their states
//!   are the host's to track). With nothing in common nothing waits, and that is
//!   where the overlap is. In the phases without it (an A/B in the process, or
//!   `=1`) the queues keep to one order: each submission waits for everything
//!   in the air on the other queue, which is what one queue gave. A wait is only
//!   ever queued for a value whose signal is already queued, so two queues
//!   cannot wait for each other. The switch may flip at any submission: the
//!   queues and the sequencer stay, and the first submission after a flip to
//!   serial waits for what the overlap left in the air.
//! - The landing thread's fetch of pieces of the card's copies ([`fetching`]) is ordered against the compute queue too:
//!   it waits for the submissions of Q2 in the air that write what it reads, and the submissions of Q2 that come
//!   after it and write it wait for it. With one queue the fetch sat in the queue's order; with two it would read a
//!   copy while a dispatch of Q2 wrote it and merge the half-written bytes into title memory.
//! - Not with stage A's thread (`front`): it prepares a submission's card steps
//!   before the queue is known.

use std::collections::VecDeque;

use super::*;
use crate::queue::timelines::{self, Lane as Title};

/// Whether the variable (or `VICTORY_AB`'s switch with the value) says `=2`.
fn asked() -> bool {
    static ASKED: OnceLock<bool> = OnceLock::new();
    *ASKED.get_or_init(|| {
        let env = std::env::var("VICTORY_ASYNC_COMPUTE").ok();
        timelines::mode_of(env.as_deref()) == 2
            || crate::ab::value_b("VICTORY_ASYNC_COMPUTE")
                .is_some_and(|v| timelines::mode_of(Some(v)) == 2)
    })
}

/// Whether the host's queues are Q1 and Q2 for the whole process: `=2` was asked, and what it needs is there (the
/// submit line, the relay, a device that makes a compute queue). Then the title's compute lists are of the compute
/// type, and every submission is joined to the host's fence by the sequencer.
pub(crate) fn lists_compute() -> bool {
    if !asked() {
        return false;
    }
    let ready = timelines::line_on() && relay() && host().and_then(lanes).is_some();
    static WARNED: std::sync::Once = std::sync::Once::new();
    if !ready && host().is_some() {
        WARNED.call_once(|| {
            tracing::warn!("VICTORY_ASYNC_COMPUTE=2 needs the submit line (VICTORY_THIN), the relay and a compute queue: one queue as before")
        });
    }
    ready
}

/// Where a list of the title's kind `kind` is made: the compute type for the title's compute lists ([`lists_compute`]).
pub(crate) fn want_compute(kind: u32) -> bool {
    kind == 2 && lists_compute()
}

// ---------------------------------------------------------------------
// What a submission touches, and what has to wait for what (no D3D12).

/// A key, and a range in the key's units: key 0 is title memory (bytes), one
/// with [`TEXTURE`] set is a texture, any other is a host copy in the card's
/// memory by its number plus one (pieces).
type Run = (u64, u64, u64);

/// The key of a texture (its address, below this bit).
const TEXTURE: u64 = 1 << 63;

/// Sorts runs and joins those that touch or overlap, within a key.
fn normalize(runs: &mut Vec<Run>) {
    runs.retain(|r| r.1 < r.2);
    runs.sort_unstable();
    let mut at = 0;
    for i in 1..runs.len() {
        let next = runs[i];
        let last = &mut runs[at];
        if next.0 == last.0 && next.1 <= last.2 {
            last.2 = last.2.max(next.2);
        } else {
            at += 1;
            runs[at] = next;
        }
    }
    if !runs.is_empty() {
        runs.truncate(at + 1);
    }
}

/// Whether two normalized lists of runs share a unit.
fn overlaps(a: &[Run], b: &[Run]) -> bool {
    let (mut i, mut j) = (0, 0);
    while i < a.len() && j < b.len() {
        let (x, y) = (a[i], b[j]);
        if x.0 != y.0 {
            if x.0 < y.0 {
                i += 1;
            } else {
                j += 1;
            }
        } else if x.2 <= y.1 {
            i += 1;
        } else if y.2 <= x.1 {
            j += 1;
        } else {
            return true;
        }
    }
    false
}

/// What a submission reads and writes that another submission, running at the
/// same time, must not read or write the other way. Reads of one with reads of
/// the other are no conflict.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(super) struct Footprint {
    reads: Vec<Run>,
    writes: Vec<Run>,
    /// A step of ours that may write title memory anywhere.
    any: bool,
}

impl Footprint {
    pub(super) fn read(&mut self, from: usize, to: usize) {
        self.reads.push((0, from as u64, to as u64));
    }

    pub(super) fn write(&mut self, from: usize, to: usize) {
        self.writes.push((0, from as u64, to as u64));
    }

    /// Pieces `[first, end)` of copy `id` in the card's memory are only
    /// compared.
    pub(super) fn card_read(&mut self, id: u64, first: u32, end: u32) {
        self.reads.push((id + 1, u64::from(first), u64::from(end)));
    }

    /// Pieces `[first, end)` of copy `id` are sent to, hashed, or compared and
    /// hashed again.
    pub(super) fn card_write(&mut self, id: u64, first: u32, end: u32) {
        self.writes.push((id + 1, u64::from(first), u64::from(end)));
    }

    /// A texture (by its host copy's address) is used: whole, and exclusively (its states are changed by
    /// the lists that name it).
    pub(super) fn texture(&mut self, at: usize) {
        self.writes.push((TEXTURE | (at as u64 & !TEXTURE), 0, 1));
    }

    /// What `more` reads and writes, too.
    pub(super) fn merge(&mut self, more: Footprint) {
        self.reads.extend(more.reads);
        self.writes.extend(more.writes);
        self.any |= more.any;
    }

    /// Ready to be compared: runs sorted and joined.
    pub(super) fn sealed(mut self) -> Self {
        normalize(&mut self.reads);
        normalize(&mut self.writes);
        self
    }

    /// Whether the two cannot run at the same time. Both sealed.
    pub(super) fn conflicts(&self, other: &Footprint) -> bool {
        self.any
            || other.any
            || overlaps(&self.writes, &other.reads)
            || overlaps(&self.writes, &other.writes)
            || overlaps(&self.reads, &other.writes)
    }
}

/// What the host counted since the last line.
#[derive(Default, Clone, Debug, PartialEq, Eq)]
pub(super) struct Stats {
    /// Submissions sent, by queue.
    sent: [u64; 2],
    /// Waits made on the other queue: for what the title waited for, for the
    /// title queue's previous submission, for what both touch, and for
    /// everything in the air (the phases without the overlap).
    waits: [u64; 4],
    /// Submissions that waited for nothing on the other queue.
    free: u64,
    /// Hand-offs: lists on Q1 that move textures out of the states of drawing
    /// for a submission of Q2, and the barriers in them.
    hands: [u64; 2],
}

/// The two queues as the host numbers them: what was signalled on each, what
/// is in the air, and where the title's submissions went.
#[derive(Default)]
pub(super) struct Book {
    /// The last value queued as a signal on each queue's fence.
    signalled: [u64; 2],
    /// The submissions in the air, per queue, by value.
    flying: [Vec<(u64, Footprint)>; 2],
    /// By title lane: the queue and value of its last submission.
    last: [Option<(usize, u64)>; 2],
    /// By title lane: number, queue and value of the submissions the title's
    /// waits may name.
    planned: [VecDeque<(u64, usize, u64)>; 2],
    /// The fetches of pieces of the card's copies not known to be done ([`Book::fetching`]).
    fetches: Vec<Fetch>,
    stats: Stats,
}

/// A fence of the host's, held in the book.
struct Held(ID3D12Fence);

// SAFETY: D3D12 fences are free-threaded.
unsafe impl Send for Held {}

/// A fetch of pieces of the card's copies: what it reads (whole copies), and the value its fence is signalled with
/// behind it.
struct Fetch {
    reads: Footprint,
    fence: Held,
    value: u64,
}

impl Fetch {
    fn done(&self) -> bool {
        // SAFETY: a plain query of a fence of ours.
        unsafe { self.fence.0.GetCompletedValue() >= self.value }
    }
}

/// The most submissions the title's waits may name back.
const PLANNED: usize = 512;

impl Book {
    /// A fetch of pieces of the copies `copies` (numbers, whole) is about to be given to a queue, with `value`
    /// as the signal of its `fence` behind it. Returns the value of Q2's fence it must wait for, 0 for none: the
    /// newest submission of Q2 in the air (`completed`: what Q2's fence reports now) that writes any of them, or
    /// may write anywhere. And the fetch is noted from here until its fence reaches `value`: a submission of Q2
    /// that comes after and writes what it reads waits for it ([`Book::fetches_for`]). Both are done under the
    /// one lock, so a submission and a fetch that meet are ordered one way or the other.
    pub(super) fn fetching(&mut self, copies: &[u64], completed: u64, fence: &ID3D12Fence, value: u64) -> u64 {
        let mut reads = Footprint::default();
        for &id in copies {
            reads.card_read(id, 0, u32::MAX);
        }
        let reads = reads.sealed();
        let want = self.flying[1]
            .iter()
            .rev()
            .find(|(v, other)| *v > completed && reads.conflicts(other))
            .map_or(0, |(v, _)| *v);
        self.fetches.retain(|f| !f.done());
        self.fetches.push(Fetch { reads, fence: Held(fence.clone()), value });
        want
    }

    /// The fetches not done yet that a submission with the footprint `fp` must wait for: (fence, value).
    pub(super) fn fetches_for(&mut self, fp: &Footprint) -> Vec<(ID3D12Fence, u64)> {
        self.fetches.retain(|f| !f.done());
        self.fetches.iter().filter(|f| fp.conflicts(&f.reads)).map(|f| (f.fence.0.clone(), f.value)).collect()
    }
}

impl Book {
    /// What a submission that goes to queue `target` (0 or 1), of title lane
    /// `title`, waits for on the other queue: the value on the other queue's
    /// fence, 0 for none. `deps`: the title's own waits as (title lane,
    /// number); `completed`: what each queue's fence reports now (what is done
    /// is not waited for). `fp` sealed. `serial`: the queues keep to one order.
    pub(super) fn waits_for(
        &mut self,
        target: usize,
        title: usize,
        deps: &[(usize, u64)],
        fp: &Footprint,
        completed: [u64; 2],
        serial: bool,
    ) -> u64 {
        let other = 1 - target;
        for q in 0..2 {
            self.flying[q].retain(|(v, _)| *v > completed[q]);
        }
        if serial {
            // One order, as one queue gave it: everything queued on the other one is done first.
            let want = self.signalled[other].max(completed[other]);
            if want > completed[other] {
                self.stats.waits[3] += 1;
                return want;
            }
            self.stats.free += 1;
            return 0;
        }
        let mut want = 0u64;
        for &(lane, number) in deps {
            let v = match self.planned[lane].iter().rev().find(|p| p.0 == number) {
                Some(&(_, queue, v)) => {
                    if queue == other {
                        v
                    } else {
                        0
                    }
                }
                // Not known any more: everything queued there.
                None => self.signalled[other],
            };
            if v > completed[other] && v > want {
                want = v;
                self.stats.waits[0] += 1;
            }
        }
        if let Some((queue, v)) = self.last[title]
            && queue == other
            && v > completed[other]
            && v > want
        {
            want = v;
            self.stats.waits[1] += 1;
        }
        // The newest first: the first that conflicts is the one to wait for.
        for (v, other_fp) in self.flying[other].iter().rev() {
            if *v <= want {
                break;
            }
            if fp.conflicts(other_fp) {
                want = *v;
                self.stats.waits[2] += 1;
                break;
            }
        }
        if want == 0 {
            self.stats.free += 1;
        }
        want
    }

    /// A submission of queue `target` is about to be given to its queue with `value` as its signal: it counts as in
    /// the air from here on, for the fetches that look at what is ([`Book::fetching`]) and for the next
    /// submission's waits. (The signal follows under the lock that numbers them: [`Book::sent`] finds it here.)
    pub(super) fn announce(&mut self, target: usize, value: u64, fp: &Footprint) {
        self.note(target, value, fp.clone());
    }

    /// `value` is in the air on `target` with `fp`: once (a value a submission announced and did not send, because
    /// it did not go after all, is taken again by the next one).
    fn note(&mut self, target: usize, value: u64, fp: Footprint) {
        match self.flying[target].iter_mut().rev().find(|(v, _)| *v == value) {
            Some(entry) => entry.1 = fp,
            None => self.flying[target].push((value, fp)),
        }
    }

    /// A submission went to queue `target` with `value` as its signal.
    pub(super) fn sent(&mut self, target: usize, title: usize, value: u64, fp: Footprint) {
        self.signalled[target] = self.signalled[target].max(value);
        self.note(target, value, fp);
        self.last[title] = Some((target, value));
        self.stats.sent[target] += 1;
    }

    /// The title's submission `number` of lane `title` has been given out
    /// (all of its parts): the title's waits name it from here on.
    pub(super) fn planned(&mut self, title: usize, number: u64) {
        if let Some((queue, value)) = self.last[title] {
            let planned = &mut self.planned[title];
            if planned.len() >= PLANNED {
                planned.pop_front();
            }
            planned.push_back((number, queue, value));
        }
    }
}

static BOOK: Mutex<Option<Book>> = Mutex::new(None);

fn book<T>(f: impl FnOnce(&mut Book) -> T) -> T {
    let mut book = BOOK.lock().unwrap_or_else(|e| e.into_inner());
    f(book.get_or_insert_with(Book::default))
}

/// The title's submission `number` of `lane` has been given out
/// (`crate::queue::timelines::end`).
pub(crate) fn planned(lane: Title, number: u64) {
    book(|b| b.planned(lane.index(), number));
}

/// For the frame-rate line.
pub(crate) fn line(frames: f64) {
    if !lists_compute() {
        return;
    }
    let s = book(|b| std::mem::take(&mut b.stats));
    let f = |n: u64| n as f64 / frames;
    tracing::info!(
        "the compute lane on a queue of its own per frame (VICTORY_ASYNC_COMPUTE=2, {}): {:.1} submissions sent to the compute queue, {:.1} to the drawing queue; \
         waits on the other queue: for what the title waited for {:.1}, for the title queue's last submission {:.1}, for what both touch {:.1}, for everything in the air {:.1}, none {:.1}; \
         hand-offs of textures out of the states of drawing: {:.2} lists with {:.2} barriers",
        if timelines::mode() == 2 {
            "overlapping"
        } else {
            "one order"
        },
        f(s.sent[1]),
        f(s.sent[0]),
        f(s.waits[0]),
        f(s.waits[1]),
        f(s.waits[2]),
        f(s.waits[3]),
        f(s.free),
        f(s.hands[0]),
        f(s.hands[1])
    );
}

// ---------------------------------------------------------------------
// The queues.

/// Q2 and what the two queues are joined by.
pub(super) struct Lanes {
    compute: ID3D12CommandQueue,
    order: ID3D12CommandQueue,
    /// Signalled by Q1 and Q2 behind their submissions (with the host's number of
    /// the submission), and by Q1 behind a hand-off (with its own count).
    fences: [ID3D12Fence; 3],
}

// SAFETY: D3D12 queues and fences are free-threaded.
unsafe impl Send for Lanes {}
// SAFETY: as above.
unsafe impl Sync for Lanes {}

fn lanes(host: &Host) -> Option<&'static Lanes> {
    static LANES: OnceLock<Option<Lanes>> = OnceLock::new();
    LANES
        .get_or_init(|| match make(&host.device) {
            Ok(lanes) => {
                tracing::info!(
                    "VICTORY_ASYNC_COMPUTE=2: the title's compute lists are of the compute type and run on a compute queue of the host, joined to the host's fence by a sequencer queue"
                );
                Some(lanes)
            }
            Err(e) => {
                tracing::warn!(error = %e, "no compute queue for VICTORY_ASYNC_COMPUTE=2: everything stays on one queue");
                None
            }
        })
        .as_ref()
}

fn make(device: &ID3D12Device) -> windows::core::Result<Lanes> {
    // SAFETY: plain object creation.
    unsafe {
        let queue = |kind| {
            device.CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                Type: kind,
                ..Default::default()
            })
        };
        Ok(Lanes {
            compute: queue(D3D12_COMMAND_LIST_TYPE_COMPUTE)?,
            order: queue(D3D12_COMMAND_LIST_TYPE_COPY)?,
            fences: [
                device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
                device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
                device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
            ],
        })
    }
}

/// The lists that make hand-offs, and when each is free again.
struct Hands {
    free: Vec<Prelude>,
    /// By the count the list signalled.
    busy: VecDeque<(u64, Prelude)>,
    count: u64,
}

// SAFETY: D3D12 objects are free-threaded; a list is recorded by the thread that holds the submission lock
// and not touched again until its count has been reached.
unsafe impl Send for Hands {}

static HANDS: Mutex<Hands> = Mutex::new(Hands {
    free: Vec::new(),
    busy: VecDeque::new(),
    count: 0,
});

// ---------------------------------------------------------------------
// What goes where.

/// What the steps of `runs`, and the textures they name, read and write.
fn steps_footprint(runs: &[&HostList]) -> Footprint {
    let mut fp = Footprint::default();
    for list in runs {
        for step in &list.deferred().copies {
            match step {
                &Step::Copy(dst, src, len) => {
                    fp.write(dst, dst + len);
                    fp.read(src, src + len);
                }
                &Step::Fill(_, src, len, _) => fp.read(src, src + len),
                Step::Refresh(shadow, address, len) => {
                    fp.read(*address, *address + *len);
                    fp.card_read(shadow.id(), 0, u32::MAX);
                }
                Step::GpuWrite(shadow, address, len) => {
                    fp.write(*address, *address + *len);
                    fp.card_write(shadow.id(), 0, u32::MAX);
                }
                &Step::Host(_, (from, to), _) => fp.read(from, to),
                Step::Run(_) => fp.any = true,
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    fp.read(at, at + len);
                }
                &Step::Plane((at, len), _) => fp.read(at, at + len),
                // (A copy done on the card between host copies: one or two a frame, whichever resources they are.)
                Step::Placed(_) => fp.any = true,
            }
        }
        for key in list.states.lock().unwrap_or_else(|e| e.into_inner()).keys() {
            fp.texture(*key);
        }
        for twin in list
            .twins
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .values()
        {
            fp.texture(std::sync::Arc::as_ptr(&twin.depth) as usize);
            fp.texture(std::sync::Arc::as_ptr(&twin.twin.colour) as usize);
        }
    }
    fp
}

/// A submission on its way to a queue.
pub(super) struct Route {
    /// 0: Q1, the host's queue; 1: Q2.
    target: usize,
    /// The title lane (0 graphics, 1 compute).
    title: usize,
    /// The queues keep to one order (the phases without the overlap).
    serial: bool,
    deps: Vec<(usize, u64)>,
    fp: Footprint,
    /// What the lists' preludes ask of Q1 ahead of the submission.
    hand: Vec<D3D12_RESOURCE_BARRIER>,
    waited: bool,
    /// The value the submission's signal will have (the host's number).
    value: u64,
}

impl Route {
    /// For a submission of `runs` whose signal will be `value`: `None` without the switch (or without what it
    /// needs): the submission goes as it always did. With it every submission has a route: its lists' type says
    /// its queue.
    pub(super) fn begin(host: &Host, runs: &[&HostList], value: u64) -> Option<Route> {
        if !lists_compute() {
            return None;
        }
        lanes(host)?;
        let compute = runs.iter().filter(|list| list.compute).count();
        if compute != 0 && compute != runs.len() {
            fail(format_args!(
                "a submission holds {compute} lists of the compute type and {} of the drawing type",
                runs.len() - compute
            ));
        }
        let target = usize::from(compute != 0);
        let overlap = timelines::mode() == 2;
        let current = if overlap { timelines::current() } else { None };
        let title = current.as_ref().map_or(target, |c| c.lane.index());
        let deps = current.map_or_else(Vec::new, |c| {
            c.waits.iter().map(|d| (d.lane.index(), d.number)).collect()
        });
        Some(Route {
            target,
            title,
            serial: !overlap,
            deps,
            fp: steps_footprint(runs),
            hand: Vec::new(),
            waited: false,
            value,
        })
    }

    /// Whether the submission goes to Q2: its lists, and everything the host
    /// adds for it, are of the compute type.
    pub(super) fn compute(&self) -> bool {
        self.target == 1
    }

    /// A list the host added to the submission (the card's own steps).
    pub(super) fn add_list(&mut self, list: &HostList) {
        self.fp.merge(steps_footprint(&[list]));
    }

    /// What the lists' preludes ask to be made on Q1 first.
    pub(super) fn hand(&mut self, barriers: Vec<D3D12_RESOURCE_BARRIER>) {
        self.hand = barriers;
    }

    /// The queue the submission goes to.
    pub(super) fn queue<'a>(&self, host: &'a Host) -> &'a ID3D12CommandQueue {
        match (self.target, lanes(host)) {
            (1, Some(lanes)) => &lanes.compute,
            _ => &host.queue,
        }
    }

    /// What the card's plan for the submission touches of the host copies in
    /// its memory, and what it declares written of title memory: the copies
    /// sent up to (`dirty`), hashed and compared (`declared`), and only
    /// compared (`looked`). A copy is one resource with one state at a time
    /// (the lists that send up to it move it between states), so it is taken
    /// whole: nothing that uses it runs on the other queue at the same time
    /// as what changes it.
    pub(super) fn card(
        &mut self,
        dirty: &[(std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>)],
        declared: &[Declared],
        looked: &[&crate::picked::Range],
    ) {
        for (shadow, _) in dirty {
            self.fp.card_write(shadow.id(), 0, u32::MAX);
        }
        for (shadow, ranges) in declared {
            self.fp.card_write(shadow.id(), 0, u32::MAX);
            for &(from, to) in ranges {
                self.fp.write(from, to);
            }
        }
        for l in looked {
            self.fp.card_read(l.0.id(), 0, u32::MAX);
        }
    }

    /// Before anything of the submission is given to its queue: the queue
    /// waits for what the other queue must have done first, and for the
    /// hand-off. Once.
    pub(super) fn waits(&mut self, host: &Host) {
        if self.waited {
            return;
        }
        self.waited = true;
        let Some(lanes) = lanes(host) else {
            return;
        };
        let fp = std::mem::take(&mut self.fp).sealed();
        // SAFETY: plain queries of fences of ours.
        let completed = [0, 1].map(|q| unsafe { lanes.fences[q].GetCompletedValue() });
        let other = 1 - self.target;
        let want = book(|b| {
            b.waits_for(
                self.target,
                self.title,
                &self.deps,
                &fp,
                completed,
                self.serial,
            )
        });
        self.fp = fp;
        let queue = self.queue(host);
        if want > 0 {
            // SAFETY: our queue and fence; the value was queued as a signal.
            if let Err(e) = unsafe { queue.Wait(&lanes.fences[other], want) } {
                fail(format_args!(
                    "a queue could not be told to wait for the other ({e})"
                ));
            }
        }
        if !self.hand.is_empty() {
            let barriers = std::mem::take(&mut self.hand);
            if self.target == 1
                && let Err(e) = hand_off(host, lanes, &barriers)
            {
                fail(format_args!(
                    "the textures could not be handed to the compute queue ({e})"
                ));
            }
        }
        if self.target == 1 {
            // In the air from here (after the hand-off: a fetch that waits for this submission is queued behind
            // every wait this one made on Q1), and behind the fetches of the card's copies it would write.
            let fetches = book(|b| {
                b.announce(1, self.value, &self.fp);
                b.fetches_for(&self.fp)
            });
            for (fence, value) in fetches {
                // SAFETY: our queue; a fence of a fetch that signals it behind its list.
                if let Err(e) = unsafe { queue.Wait(&fence, value) } {
                    fail(format_args!("the compute queue could not be told to wait for a fetch ({e})"));
                }
            }
        }
    }

    /// Behind the submission's lists: the queue's fence takes the
    /// submission's value, and the sequencer gives it to the host's fence once
    /// the queue has got there. In place of the host queue's signal of the
    /// host's fence.
    pub(super) fn signal(&mut self, host: &Host, value: u64) -> windows::core::Result<()> {
        self.waits(host);
        let Some(lanes) = lanes(host) else {
            // SAFETY: our queue and fence.
            return unsafe { host.queue.Signal(&host.fence, value) };
        };
        let fence = &lanes.fences[self.target];
        // SAFETY: our queues and fences, behind the lock that numbers the signals.
        unsafe {
            self.queue(host).Signal(fence, value)?;
            lanes.order.Wait(fence, value)?;
            lanes.order.Signal(&host.fence, value)?;
        }
        let fp = std::mem::take(&mut self.fp).sealed();
        book(|b| b.sent(self.target, self.title, value, fp));
        Ok(())
    }
}

/// For the landing thread's fetch of pieces of the card's copies ([`Host::relay_card_back`]): `copies` are the
/// numbers of the copies it reads, `fence` the fetch's own, signalled with `value` behind its list. Returns the
/// fence of Q2 and the value the queue that runs the fetch must wait for before its list, or `None`: nothing of Q2
/// in the air writes them (or there is no Q2, the switch is off). From here on the submissions of Q2 that write the
/// copies wait for `fence` to reach `value`.
pub(super) fn fetching(host: &Host, copies: &[u64], fence: &ID3D12Fence, value: u64) -> Option<(&'static ID3D12Fence, u64)> {
    if !lists_compute() {
        return None;
    }
    let lanes = lanes(host)?;
    // SAFETY: a plain query of a fence of ours.
    let completed = unsafe { lanes.fences[1].GetCompletedValue() };
    let want = book(|b| b.fetching(copies, completed, fence, value));
    (want > 0).then_some((&lanes.fences[1], want))
}

/// Moves textures out of the states of drawing on Q1, in a list of its own,
/// ahead of what the compute queue is about to do: the compute queue waits
/// for it.
fn hand_off(
    host: &Host,
    lanes: &Lanes,
    barriers: &[D3D12_RESOURCE_BARRIER],
) -> windows::core::Result<()> {
    let (list, count) = {
        let mut hands = HANDS.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: a plain query.
        let done = unsafe { lanes.fences[2].GetCompletedValue() };
        while hands.busy.front().is_some_and(|(count, _)| *count <= done) {
            if let Some((_, list)) = hands.busy.pop_front() {
                hands.free.push(list);
            }
        }
        let list = match hands.free.pop() {
            Some(list) => list,
            None => host
                .make_prelude()
                .ok_or_else(windows::core::Error::empty)?,
        };
        hands.count += 1;
        (list, hands.count)
    };
    let (allocator, commands) = &list;
    // SAFETY: the list is not executing (its count was reached); the textures are held by the lists that name them.
    unsafe {
        allocator.Reset()?;
        commands.Reset(allocator, None)?;
        commands.ResourceBarrier(barriers);
        commands.Close()?;
        host.queue.ExecuteCommandLists(&[Some(commands.cast()?)]);
        host.queue.Signal(&lanes.fences[2], count)?;
        lanes.compute.Wait(&lanes.fences[2], count)?;
    }
    book(|b| {
        b.stats.hands[0] += 1;
        b.stats.hands[1] += barriers.len() as u64;
    });
    HANDS
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .busy
        .push_back((count, list));
    Ok(())
}

#[cfg(test)]
mod model_tests {
    use super::*;

    /// A tiny deterministic generator.
    pub(super) struct Dice(u64);

    impl Dice {
        pub(super) fn new(seed: u64) -> Dice {
            Dice(seed)
        }

        pub(super) fn next(&mut self, n: u64) -> u64 {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            self.0 % n
        }
    }

    fn fp(reads: &[(u64, u64)], writes: &[(u64, u64)]) -> Footprint {
        let mut f = Footprint::default();
        for &(a, b) in reads {
            f.read(a as usize, b as usize);
        }
        for &(a, b) in writes {
            f.write(a as usize, b as usize);
        }
        f.sealed()
    }

    #[test]
    fn reads_alone_never_conflict_and_a_write_conflicts_with_what_it_touches() {
        let a = fp(&[(0, 100)], &[]);
        let b = fp(&[(50, 150)], &[]);
        assert!(!a.conflicts(&b));
        let w = fp(&[], &[(90, 95)]);
        assert!(a.conflicts(&w) && w.conflicts(&a), "a write inside a read");
        assert!(!b.conflicts(&fp(&[], &[(0, 50)])), "next to it");
        assert!(b.conflicts(&fp(&[], &[(149, 200)])), "its last byte");
        let ww = fp(&[], &[(10, 20)]);
        assert!(ww.conflicts(&fp(&[], &[(19, 30)])), "two writes");
        assert!(!ww.conflicts(&fp(&[], &[(20, 30)])));
        let any = Footprint {
            any: true,
            ..Footprint::default()
        };
        assert!(
            any.conflicts(&Footprint::default()),
            "a step that may write anywhere"
        );
    }

    #[test]
    fn pieces_of_a_copy_conflict_only_in_the_same_copy() {
        let mut a = Footprint::default();
        a.card_write(7, 0, 4);
        let mut b = Footprint::default();
        b.card_read(7, 3, 5);
        let mut c = Footprint::default();
        c.card_write(8, 0, 4);
        let (a, b, c) = (a.sealed(), b.sealed(), c.sealed());
        assert!(a.conflicts(&b));
        assert!(!a.conflicts(&c), "another copy");
        // Pieces and bytes are not the same unit.
        let bytes = fp(&[], &[(0, 4)]);
        assert!(!a.conflicts(&bytes));
    }

    /// The runs of a random footprint against a brute-force picture of what it covers.
    #[test]
    fn normalizing_keeps_what_is_covered() {
        let mut dice = Dice(0x9e37_79b9_7f4a_7c15);
        for _ in 0..300 {
            let mut runs: Vec<Run> = Vec::new();
            for _ in 0..dice.next(12) {
                let key = dice.next(3);
                let a = dice.next(60);
                runs.push((key, a, a + dice.next(20)));
            }
            let mut covered = [[false; 80]; 3];
            for &(k, a, b) in &runs {
                for x in a..b {
                    covered[k as usize][x as usize] = true;
                }
            }
            normalize(&mut runs);
            let mut after = [[false; 80]; 3];
            for &(k, a, b) in &runs {
                assert!(a < b);
                for x in a..b {
                    assert!(!after[k as usize][x as usize], "no unit twice");
                    after[k as usize][x as usize] = true;
                }
            }
            assert_eq!(covered, after);
            assert!(
                runs.windows(2).all(|w| w[0].0 < w[1].0 || w[0].2 < w[1].1),
                "joined"
            );
        }
    }

    #[test]
    fn overlap_agrees_with_a_brute_force_look() {
        let mut dice = Dice(0x2545_f491_4f6c_dd1d);
        let made = |dice: &mut Dice| {
            let mut runs: Vec<Run> = Vec::new();
            for _ in 0..dice.next(6) {
                let a = dice.next(100);
                runs.push((dice.next(2), a, a + 1 + dice.next(10)));
            }
            normalize(&mut runs);
            runs
        };
        for _ in 0..2000 {
            let (a, b) = (made(&mut dice), made(&mut dice));
            let brute = a
                .iter()
                .any(|x| b.iter().any(|y| x.0 == y.0 && x.1 < y.2 && y.1 < x.2));
            assert_eq!(overlaps(&a, &b), brute, "{a:?} {b:?}");
        }
    }

    /// A submission announced before it is given to its queue is in the air once, announced or sent, and the next
    /// submission's waits see it from the announcement.
    #[test]
    fn a_submission_announced_and_then_sent_is_in_the_air_once() {
        let mut b = Book::default();
        let written = fp(&[], &[(0, 10)]);
        b.announce(1, 5, &written);
        b.announce(1, 5, &written);
        assert_eq!((b.flying[1].len(), b.signalled[1]), (1, 0), "in the air, not signalled yet");
        // A submission of the other queue that shares bytes with it waits for it.
        assert_eq!(b.waits_for(0, 0, &[], &fp(&[(5, 6)], &[]), [0, 0], false), 5);
        b.sent(1, 1, 5, written);
        assert_eq!((b.flying[1].len(), b.signalled[1]), (1, 5));
    }

    #[test]
    fn a_wait_is_for_what_the_title_waited_for_the_last_of_its_queue_and_what_is_shared() {
        let mut b = Book::default();
        // Graphics (Q1) value 5 wrote [0,10); title lane 0 number 1.
        b.sent(0, 0, 5, fp(&[], &[(0, 10)]));
        b.planned(0, 1);
        b.sent(0, 0, 6, fp(&[], &[(100, 110)]));
        b.planned(0, 2);
        let mine = fp(&[], &[(500, 510)]);
        // Nothing in common and nothing waited for: free.
        assert_eq!(b.waits_for(1, 1, &[], &mine, [0, 0], false), 0);
        assert_eq!(b.stats.free, 1);
        // The title waited for graphics number 1: value 5.
        assert_eq!(b.waits_for(1, 1, &[(0, 1)], &mine, [0, 0], false), 5);
        // Shared memory with the newest in the air: value 6.
        assert_eq!(
            b.waits_for(1, 1, &[], &fp(&[(105, 106)], &[]), [0, 0], false),
            6
        );
        // The older one only.
        assert_eq!(
            b.waits_for(1, 1, &[], &fp(&[(5, 6)], &[]), [0, 0], false),
            5
        );
        // What the other queue has done is not waited for.
        assert_eq!(
            b.waits_for(1, 1, &[(0, 2)], &fp(&[(105, 106)], &[]), [6, 0], false),
            0
        );
        assert_eq!(b.stats.waits, [1, 0, 2, 0]);
    }

    #[test]
    fn a_title_queue_stays_in_order_across_host_queues() {
        let mut b = Book::default();
        b.sent(1, 1, 3, Footprint::default().sealed());
        // The next of the title's compute queue goes to Q1: after value 3 on Q2.
        assert_eq!(
            b.waits_for(0, 1, &[], &Footprint::default().sealed(), [0, 0], false),
            3
        );
        assert_eq!(b.stats.waits[1], 1);
        // On its own queue nothing is waited for.
        let mut b = Book::default();
        b.sent(1, 1, 3, Footprint::default().sealed());
        assert_eq!(
            b.waits_for(1, 1, &[], &Footprint::default().sealed(), [0, 0], false),
            0
        );
    }

    #[test]
    fn a_name_that_is_not_known_any_more_waits_for_everything_queued() {
        let mut b = Book::default();
        for v in 1..=3u64 {
            b.sent(0, 0, v, Footprint::default().sealed());
        }
        assert_eq!(
            b.waits_for(
                1,
                1,
                &[(0, 99)],
                &Footprint::default().sealed(),
                [0, 0],
                false
            ),
            3
        );
    }

    // The queues as the card runs them, to see that what the host asks for
    // can be met and what it orders is ordered.

    #[derive(Clone, Copy, Debug)]
    enum Cmd {
        Wait(usize, u64),
        Signal(usize, u64),
        Work(usize),
    }

    /// Fences 0 and 1 are the queues', 2 is the host's. Queue 2 is the
    /// sequencer.
    #[derive(Debug, PartialEq, Eq)]
    enum Ended {
        Done(Vec<Cmd2>),
        Stalled,
    }

    /// What ran, in order, for the checks.
    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    enum Cmd2 {
        Work(usize),
        Host(u64),
    }

    /// Runs the queues like a card: each in its order, a wait standing until
    /// its fence has the value. A choice of which queue goes next from `dice`.
    fn run(queues: &[Vec<Cmd>; 3], dice: &mut Dice) -> Ended {
        let mut at = [0usize; 3];
        let mut fence = [0u64; 3];
        let mut log = Vec::new();
        loop {
            let ready: Vec<usize> = (0..3)
                .filter(|&q| match queues[q].get(at[q]) {
                    None => false,
                    Some(Cmd::Wait(f, v)) => fence[*f] >= *v,
                    Some(_) => true,
                })
                .collect();
            if ready.is_empty() {
                return if (0..3).all(|q| at[q] == queues[q].len()) {
                    Ended::Done(log)
                } else {
                    Ended::Stalled
                };
            }
            let q = ready[dice.next(ready.len() as u64) as usize];
            match queues[q][at[q]] {
                Cmd::Wait(..) => {}
                Cmd::Signal(f, v) => {
                    fence[f] = v;
                    if f == 2 {
                        log.push(Cmd2::Host(v));
                    }
                }
                Cmd::Work(id) => log.push(Cmd2::Work(id)),
            }
            at[q] += 1;
        }
    }

    /// A random title: submissions on its two lanes with random memory, some
    /// eligible for Q2, some waiting for submissions of the other lane, as the
    /// host would send them. Returns the queues' commands and what each
    /// submission touched.
    fn host_sends(
        dice: &mut Dice,
        submissions: usize,
        flip: bool,
    ) -> (
        [Vec<Cmd>; 3],
        Vec<(usize, Footprint, Vec<usize>, usize, bool)>,
    ) {
        let mut queues: [Vec<Cmd>; 3] = [Vec::new(), Vec::new(), Vec::new()];
        let mut book = Book::default();
        let mut sent = Vec::new();
        // The completed values: nothing, so that every wait is made.
        let mut numbers = [0u64; 2];
        let mut value = 0u64;
        for id in 0..submissions {
            let title = dice.next(2) as usize;
            numbers[title] += 1;
            let eligible = title == 1 && dice.next(4) != 0;
            let target = usize::from(eligible);
            let mut f = Footprint::default();
            for _ in 0..dice.next(4) {
                let a = dice.next(8) * 100;
                if dice.next(2) == 0 {
                    f.read(a as usize, a as usize + 50);
                } else {
                    f.write(a as usize, a as usize + 50);
                }
            }
            let f = f.sealed();
            // The title waits for an earlier submission of the other lane now and then.
            let mut deps = Vec::new();
            let mut must = Vec::new();
            if numbers[1 - title] > 0 && dice.next(3) == 0 {
                let n = 1 + dice.next(numbers[1 - title]);
                deps.push((1 - title, n));
                // Which submission was number n of that lane?
                if let Some(at) =
                    sent.iter()
                        .position(|s: &(usize, Footprint, Vec<usize>, usize, bool)| {
                            s.3 == (1 - title) * 1_000_000 + n as usize
                        })
                {
                    must.push(at);
                }
            }
            // The switch flips now and then: this submission keeps to one order, or does not.
            let serial = flip && dice.next(3) == 0;
            let want = book.waits_for(target, title, &deps, &f, [0, 0], serial);
            assert!(
                want <= book.signalled[1 - target],
                "a wait for what was never queued"
            );
            value += 1;
            if want > 0 {
                queues[target].push(Cmd::Wait(1 - target, want));
            }
            queues[target].push(Cmd::Work(id));
            queues[target].push(Cmd::Signal(target, value));
            queues[2].push(Cmd::Wait(target, value));
            queues[2].push(Cmd::Signal(2, value));
            book.sent(target, title, value, f.clone());
            book.planned(title, numbers[title]);
            sent.push((
                target,
                f,
                must,
                title * 1_000_000 + numbers[title] as usize,
                serial,
            ));
        }
        (queues, sent)
    }

    #[test]
    fn whatever_the_title_waits_for_the_queues_never_stand_for_each_other() {
        let mut dice = Dice(0x1234_5678_9abc_def1);
        for round in 0..400 {
            let (queues, _) = host_sends(&mut dice, 5 + (round % 40), round % 2 == 1);
            for _ in 0..3 {
                assert!(
                    matches!(run(&queues, &mut dice), Ended::Done(_)),
                    "round {round}: {queues:?}"
                );
            }
        }
    }

    #[test]
    fn a_wait_for_a_value_nobody_signals_is_seen_as_a_stall() {
        let queues: [Vec<Cmd>; 3] = [
            vec![Cmd::Wait(1, 5), Cmd::Work(0)],
            vec![Cmd::Work(1), Cmd::Signal(1, 4)],
            Vec::new(),
        ];
        assert_eq!(run(&queues, &mut Dice(7)), Ended::Stalled);
        // Two queues that wait for each other.
        let queues: [Vec<Cmd>; 3] = [
            vec![Cmd::Wait(1, 1), Cmd::Signal(0, 1)],
            vec![Cmd::Wait(0, 1), Cmd::Signal(1, 1)],
            Vec::new(),
        ];
        assert_eq!(run(&queues, &mut Dice(8)), Ended::Stalled);
    }

    /// The host's fence is signalled in the order the submissions were given,
    /// whatever order the queues finish them in; and what conflicts, or was
    /// named by the title, runs one after the other.
    #[test]
    fn conflicting_work_runs_in_order_and_the_hosts_fence_goes_up_by_one_thing() {
        let mut dice = Dice(0x0dd_ba11_f00d);
        for round in 0..600 {
            let n = 5 + (round % 50);
            // (Every other round the switch flips between the overlap and one order along the way.)
            let (queues, sent) = host_sends(&mut dice, n, round % 2 == 1);
            for _ in 0..4 {
                let Ended::Done(log) = run(&queues, &mut dice) else {
                    panic!("round {round} stalled");
                };
                let place = |id: usize| log.iter().position(|c| *c == Cmd2::Work(id)).unwrap();
                let hosts: Vec<u64> = log
                    .iter()
                    .filter_map(|c| {
                        if let Cmd2::Host(v) = c {
                            Some(*v)
                        } else {
                            None
                        }
                    })
                    .collect();
                assert!(
                    hosts.windows(2).all(|w| w[0] < w[1]),
                    "the host's fence only goes up"
                );
                for j in 0..sent.len() {
                    for i in 0..j {
                        let (qi, fi, _, ti) =
                            (&sent[i].0, &sent[i].1, &sent[i].2, sent[i].3 / 1_000_000);
                        let (qj, fj, _, tj) =
                            (&sent[j].0, &sent[j].1, &sent[j].2, sent[j].3 / 1_000_000);
                        // The same queue is in order by itself; in one order, everything before is.
                        if qi != qj && (fi.conflicts(fj) || (ti == tj) || sent[j].4) {
                            assert!(place(i) < place(j), "round {round}: {i} before {j}");
                        }
                    }
                }
                // What the title named runs before the one that waited.
                for (j, s) in sent.iter().enumerate() {
                    for &i in &s.2 {
                        if sent[i].0 != s.0 {
                            assert!(
                                place(i) < place(j),
                                "round {round}: the title's wait, {i} before {j}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// The title's fences and memory values through the stage-1 model into the book and the queues: random signals
    /// and waits on both lanes, each submission numbered among its lane's and waiting, on the other host queue, for
    /// what the model resolved. Nothing stands for another, and what a submission was resolved to wait for runs first.
    #[test]
    fn the_titles_random_fences_and_values_become_waits_that_are_met_and_ordered() {
        use crate::queue::timelines::{Lane, Model};
        let mut dice = Dice(0x5851_f42d_4c95_7f2d);
        let mut resolved = 0;
        for round in 0..300 {
            let mut model = Model::default();
            let mut book = Book::default();
            let mut queues: [Vec<Cmd>; 3] = [Vec::new(), Vec::new(), Vec::new()];
            // By work number: the queue it went to; and (named, waiting) pairs of work.
            let mut went: Vec<usize> = Vec::new();
            let mut by_lane: std::collections::HashMap<(usize, u64), usize> =
                std::collections::HashMap::new();
            let mut named: Vec<(usize, usize)> = Vec::new();
            let mut value = 0u64;
            for _ in 0..(10 + round % 50) {
                let title = dice.next(2) as usize;
                let lane = if title == 0 {
                    Lane::Graphics
                } else {
                    Lane::Compute
                };
                let (fence, at, seen) =
                    (dice.next(3) as usize, dice.next(6), dice.next(3) as usize);
                match dice.next(5) {
                    0 => model.signal(lane, fence, at),
                    1 => model.wait_fence(lane, fence, at),
                    2 => model.wait_value(lane, seen, Box::new(|v| v >= 2)),
                    _ => {
                        let plan = model.run(lane, &[(seen, at)]);
                        let deps: Vec<(usize, u64)> = plan
                            .waits
                            .iter()
                            .map(|d| (d.lane.index(), d.number))
                            .collect();
                        let target = usize::from(title == 1 && dice.next(4) != 0);
                        let mut f = Footprint::default();
                        if dice.next(2) == 0 {
                            let a = dice.next(6) * 100;
                            f.write(a as usize, a as usize + 50);
                        }
                        let f = f.sealed();
                        let want = book.waits_for(target, title, &deps, &f, [0, 0], false);
                        assert!(
                            want <= book.signalled[1 - target],
                            "a wait for what was never queued"
                        );
                        let id = went.len();
                        value += 1;
                        if want > 0 {
                            queues[target].push(Cmd::Wait(1 - target, want));
                        }
                        queues[target].push(Cmd::Work(id));
                        queues[target].push(Cmd::Signal(target, value));
                        queues[2].push(Cmd::Wait(target, value));
                        queues[2].push(Cmd::Signal(2, value));
                        for &(lane, number) in &deps {
                            resolved += 1;
                            let first = by_lane[&(lane, number)];
                            named.push((first, id));
                        }
                        book.sent(target, title, value, f);
                        book.planned(title, plan.number);
                        by_lane.insert((title, plan.number), id);
                        went.push(target);
                    }
                }
            }
            for _ in 0..3 {
                let Ended::Done(log) = run(&queues, &mut dice) else {
                    panic!("round {round} stalled: {queues:?}");
                };
                let place = |id: usize| log.iter().position(|c| *c == Cmd2::Work(id)).unwrap();
                for &(first, waiting) in &named {
                    // (On one queue it is the queue's own order.)
                    assert!(
                        place(first) < place(waiting),
                        "round {round}: {first} named by {waiting}"
                    );
                }
            }
        }
        assert!(
            resolved > 100,
            "the random titles resolved only {resolved} waits"
        );
    }

    #[test]
    fn in_one_order_everything_in_the_air_on_the_other_queue_is_waited_for() {
        let mut b = Book::default();
        b.sent(0, 0, 3, fp(&[], &[(0, 10)]));
        b.sent(0, 0, 5, fp(&[], &[(100, 110)]));
        let apart = fp(&[(500, 510)], &[]);
        // Nothing in common, and still: the newest, as one queue would have it.
        assert_eq!(b.waits_for(1, 1, &[], &apart, [0, 0], true), 5);
        // Only what is not done.
        assert_eq!(b.waits_for(1, 1, &[], &apart, [4, 0], true), 5);
        assert_eq!(b.waits_for(1, 1, &[], &apart, [5, 0], true), 0);
        assert_eq!(b.stats.waits, [0, 0, 0, 2]);
        // The same from the other side.
        b.sent(1, 1, 6, Footprint::default().sealed());
        assert_eq!(b.waits_for(0, 0, &[], &apart, [5, 0], true), 6);
    }

    #[test]
    fn a_texture_is_used_by_one_queue_at_a_time() {
        let mut a = Footprint::default();
        a.texture(0x1000);
        let mut b = Footprint::default();
        b.texture(0x1000);
        let mut c = Footprint::default();
        c.texture(0x2000);
        let (a, b, c) = (a.sealed(), b.sealed(), c.sealed());
        assert!(
            a.conflicts(&b),
            "two uses of the same texture, even two reads"
        );
        assert!(!a.conflicts(&c));
        // Not a copy in the card's memory, not title memory.
        let mut copy = Footprint::default();
        copy.card_write(0x1000 - 1, 0, u32::MAX);
        assert!(!a.conflicts(&copy.sealed()));
        assert!(!a.conflicts(&fp(&[], &[(0x1000, 0x1001)])));
    }

    #[test]
    fn a_compute_list_keeps_to_the_states_a_compute_queue_may_use() {
        const RENDER_TARGET: u32 = 0x4;
        const UNORDERED: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const DEPTH_READ: u32 = 0x20;
        const NON_PIXEL: u32 = 0x40;
        const PIXEL: u32 = 0x80;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        // What a title reads a texture as: in a compute list, the non-pixel state.
        assert_eq!(compute_legal(NON_PIXEL | PIXEL), NON_PIXEL);
        assert_eq!(compute_legal(PIXEL), NON_PIXEL);
        assert_eq!(compute_legal(DEPTH_READ | NON_PIXEL | PIXEL), NON_PIXEL);
        assert_eq!(compute_legal(DEPTH_READ), NON_PIXEL);
        // What it may keep.
        for state in [
            0,
            UNORDERED,
            NON_PIXEL,
            COPY_DEST,
            COPY_SOURCE,
            UNORDERED | NON_PIXEL,
            COPY_SOURCE | NON_PIXEL,
        ] {
            assert_eq!(compute_legal(state), state);
        }
        // Whatever the state is, what comes out is legal and not nothing unless it was nothing.
        for state in 0..0x4000u32 {
            let legal = compute_legal(state);
            assert_eq!(legal & DRAWING_STATES, 0, "{state:#x}");
            assert_eq!(legal == 0, state == 0, "{state:#x}");
        }
        for drawing in [
            RENDER_TARGET,
            DEPTH_WRITE,
            DEPTH_READ,
            PIXEL,
            0x100,
            0x1000,
            0x2000,
            0x100_0000,
        ] {
            assert_ne!(DRAWING_STATES & drawing, 0, "{drawing:#x}");
        }
    }

    #[test]
    fn what_is_done_is_not_waited_for() {
        let mut b = Book::default();
        b.sent(0, 0, 4, fp(&[], &[(0, 10)]));
        let mine = fp(&[(5, 6)], &[]);
        assert_eq!(b.waits_for(1, 1, &[], &mine, [3, 0], false), 4);
        assert_eq!(
            b.waits_for(1, 1, &[], &mine, [4, 0], false),
            0,
            "the card has it"
        );
        assert!(b.flying[0].is_empty(), "and it is no longer in the air");
    }
}

#[cfg(all(test, windows))]
mod warp;
