//! Diagnostics: how long the card itself works on our submissions, by its own
//! timestamps (`VICTORY_CARD_SPANS=1`). The clock of the frame line
//! (`card_clock_line`) counts from the call that sends a submission to the
//! moment a thread of ours wakes behind its last signal: the time the system
//! takes to hand a submission to the card and to report it done is in it, 40
//! times a frame. Here two lists made once, of nothing but a timestamp, stand
//! around each submission.
//!
//! The stamps around our own passes (`pass_slot`) are in every build, not only
//! the diagnostics one: the runs that measure the card's time are of the release
//! exe too, and a line that only the other exe prints was never in their logs.

// (Without diagnostics the stamps around submissions and the title's own timestamps are not used.)
#![cfg_attr(not(feature = "diagnostics"), allow(dead_code))]

use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;
use windows::core::Interface;

use super::Host;

/// Submissions stamped before the first stamps are read and used again.
const SPANS: usize = 512;

/// The stamps of one queue's submissions.
pub(super) struct Spans {
    _allocator: ID3D12CommandAllocator,
    _heap: ID3D12QueryHeap,
    _back: ID3D12Resource,
    /// The mapped results, two a submission.
    data: usize,
    lists: Vec<ID3D12CommandList>,
    ticks_per_ms: f64,
    next: usize,
    sums: Sums,
    /// With `VICTORY_CARD_GAPS=1`: what the CPU did around each stamped submission, by slot; the slot given last;
    /// the card's clock against the PC's; and what the gaps add up to.
    calls: Vec<Calls>,
    given: usize,
    clock: Clock,
    gaps: Gaps,
}

// SAFETY: free-threaded objects and a mapping only read behind a lock.
unsafe impl Send for Spans {}

/// What the stamps read so far add up to.
#[derive(Debug, Default, Clone, Copy, PartialEq)]
struct Sums {
    /// Submissions, ticks the card worked on them, ticks between them.
    count: u64,
    busy: u64,
    between: u64,
    /// Ticks of the longest one, and the last one's end.
    longest: u64,
    last: u64,
}

impl Sums {
    /// A submission that ran from `start` to `end` (card ticks), after those
    /// before it. The start's stamp may be of before the last one's end: the
    /// part before that end is the last one's.
    fn add(&mut self, start: u64, end: u64) {
        if start == 0 || end < start {
            return;
        }
        let start = start.max(self.last);
        let took = end.saturating_sub(start);
        if self.last != 0 {
            self.between += start - self.last;
        }
        self.count += 1;
        self.busy += took;
        self.longest = self.longest.max(took);
        self.last = end.max(self.last);
    }
}

fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_SPANS").is_some_and(|v| v == "1" || v == "2") || gaps_on())
}

/// `VICTORY_CARD_GAPS=1`: what the card's idle between two brackets of stamps is made of, by the CPU's own times of the
/// calls of `ExecuteCommandLists` (implies the stamps of `VICTORY_CARD_SPANS=1`). See [`Gaps`].
pub(crate) fn gaps_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_GAPS").is_some_and(|v| v == "1"))
}

/// `VICTORY_CARD_SPANS=2`: besides the stamps of `1`, a stamp behind every list of a submission (`Host::lap_lists`).
fn laps_on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_SPANS").is_some_and(|v| v == "2"))
}

static SPANS_MADE: Mutex<Option<Option<Spans>>> = Mutex::new(None);

impl Host {
    /// The lists that go in front of and behind a submission about to be
    /// sent to the drawing queue, if stamps are asked for.
    pub(super) fn span_lists(&self) -> Option<[ID3D12CommandList; 2]> {
        if !on() {
            return None;
        }
        let mut made = SPANS_MADE.lock().unwrap_or_else(|e| e.into_inner());
        let spans = made
            .get_or_insert_with(|| {
                self.make_spans()
                    .inspect_err(|e| tracing::warn!(error = %e, "no stamps around submissions"))
                    .ok()
            })
            .as_mut()?;
        let i = spans.next;
        spans.next = (i + 1) % SPANS;
        // The stamps of the submission that had these lists, that many
        // submissions ago: long done.
        // SAFETY: inside the mapping; written by the card at whole words.
        let (start, end) = unsafe {
            let at = (spans.data as *mut u64).add(2 * i);
            let read = (at.read_volatile(), at.add(1).read_volatile());
            at.write_volatile(0);
            at.add(1).write_volatile(0);
            read
        };
        let before = spans.sums.last;
        spans.sums.add(start, end);
        if gaps_on() {
            let calls = std::mem::take(&mut spans.calls[i]);
            spans.read_gap(&self.queue, before, start, end, calls);
        }
        spans.given = i;
        Some([spans.lists[2 * i].clone(), spans.lists[2 * i + 1].clone()])
    }

    fn make_spans(&self) -> windows::core::Result<Spans> {
        let device = &self.device;
        // SAFETY: plain object creation; lists of one allocator recorded one
        // after the other.
        unsafe {
            let mut heap: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC {
                    Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP,
                    Count: 2 * SPANS as u32,
                    NodeMask: 0,
                },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let back = self.read_back_buffer(16 * SPANS as u64).ok_or_else(windows::core::Error::empty)?;
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data))?;
            std::ptr::write_bytes(data.cast::<u8>(), 0, 16 * SPANS);
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let mut lists = Vec::with_capacity(2 * SPANS);
            for i in 0..2 * SPANS as u32 {
                let list: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i);
                list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 1, &back, u64::from(i) * 8);
                list.Close()?;
                lists.push(list.cast()?);
            }
            let frequency = self.queue.GetTimestampFrequency()?;
            let (mut card, mut pc) = (0, 0);
            self.queue.GetClockCalibration(&mut card, &mut pc)?;
            Ok(Spans {
                _allocator: allocator,
                _heap: heap,
                _back: back,
                data: data as usize,
                lists,
                ticks_per_ms: frequency as f64 / 1000.0,
                next: 0,
                sums: Sums::default(),
                calls: vec![Calls::default(); SPANS],
                given: 0,
                clock: Clock { card0: card, pc0: pc, card, pc, reads: 0, card_frequency: frequency as f64, pc_frequency: pc_frequency() },
                gaps: Gaps::default(),
            })
        }
    }
}

/// What the CPU did around one stamped submission (`VICTORY_CARD_GAPS=1`), in ticks of the PC's counter: when the call
/// of `ExecuteCommandLists` that sent its lists was entered, and when it and the signals behind it had returned; the
/// same for the call that sent its sync list (0: no such call), the slot of the stamps of that plan's passes, and
/// whether the bytes of its sends crossed on a copy queue (the drawing queue stood behind a wait for them).
#[derive(Clone, Copy, Default)]
struct Calls {
    enter: u64,
    ret: u64,
    sync_enter: u64,
    sync_ret: u64,
    slot: Option<usize>,
    staged: bool,
}

/// The card's clock against the PC's: two calibrations (the first one made with the stamps, the latest one renewed
/// every few submissions) and the two frequencies. The rate between the calibrations is used once they are two
/// seconds apart, the nominal ratio before.
struct Clock {
    card0: u64,
    pc0: u64,
    card: u64,
    pc: u64,
    reads: u32,
    card_frequency: f64,
    pc_frequency: f64,
}

impl Clock {
    /// A fresh calibration every 64th read.
    fn refresh(&mut self, queue: &ID3D12CommandQueue) {
        self.reads = self.reads.wrapping_add(1);
        if self.reads % 64 != 1 {
            return;
        }
        let (mut card, mut pc) = (0u64, 0u64);
        // SAFETY: a plain query with valid out pointers.
        if unsafe { queue.GetClockCalibration(&mut card, &mut pc) }.is_ok() {
            self.card = card;
            self.pc = pc;
        }
    }

    /// PC ticks per tick of the card.
    fn rate(&self) -> f64 {
        let (dcard, dpc) = (self.card.saturating_sub(self.card0), self.pc.saturating_sub(self.pc0));
        if dpc as f64 > 2.0 * self.pc_frequency && dcard > 0 {
            dpc as f64 / dcard as f64
        } else {
            self.pc_frequency / self.card_frequency
        }
    }

    /// A moment of the card's clock as a moment of the PC's counter.
    fn pc(&self, card: u64) -> f64 {
        self.pc as f64 + (i128::from(card) - i128::from(self.card)) as f64 * self.rate()
    }
}

/// The PC counter's ticks per second.
fn pc_frequency() -> f64 {
    let mut ticks = 0;
    // SAFETY: a valid out pointer.
    unsafe { windows_sys::Win32::System::Performance::QueryPerformanceFrequency(&mut ticks) };
    ticks.max(1) as f64
}

/// A stretch `(a, b)` of the PC's counter in which the card had nothing to run, before work that a call of
/// `ExecuteCommandLists` entered at `e` and returned at `r` gave it: the time before the call was entered (the CPU had
/// not sent it yet), the time inside the call, and the time after it returned (the work was handed over and the card
/// had not begun it: idle with work queued). The three add up to `b - a`.
fn split_stretch(a: f64, b: f64, e: f64, r: f64) -> (f64, f64, f64) {
    let r = r.max(e);
    ((e.min(b) - a).max(0.0), (r.min(b) - e.max(a)).max(0.0), (b - r.max(a)).max(0.0))
}

/// What the card's idle between brackets (the stamps line's "idle between them") is made of, since the last
/// frame-rate line (`VICTORY_CARD_GAPS=1`), in ticks of the PC's counter. Between the back stamp of a submission and
/// the front stamp of the next there is the sync list of the next one (a list of ours: it sends the bytes the title
/// wrote up to the copies in the card's memory and hashes them), which the stamps count as idle; what is left is
/// told apart by what the CPU was doing at the time with the call that gave the card its next work.
#[derive(Default)]
struct Gaps {
    /// Submissions read, and those with a gap before them.
    submissions: u64,
    gapped: u64,
    /// The gaps, in all.
    total: f64,
    /// Of it: our sync lists running (how many, and how many with their bytes crossing on a copy queue).
    sync: f64,
    synced: u64,
    staged: u64,
    /// Of it: the card with nothing handed over, the CPU not yet in the call.
    nothing: f64,
    /// Of it: the CPU inside the call that gave it the work.
    in_call: f64,
    /// Of it: the work handed over and not begun.
    handed: f64,
    /// Of `handed`: what stood before a sync list whose bytes crossed on a copy queue.
    handed_staged: f64,
    /// Stretches of `handed` over 100 us, and the longest one.
    long: u64,
    longest: f64,
}

impl Gaps {
    /// A stretch `(a, b)` before work whose call was entered at `enter` and returned at `ret`; `long` ticks make a long one.
    fn stretch(&mut self, (a, b): (f64, f64), enter: u64, ret: u64, staged: bool, long: f64) {
        if b <= a {
            return;
        }
        let (nothing, in_call, handed) = split_stretch(a, b, enter as f64, ret as f64);
        self.nothing += nothing;
        self.in_call += in_call;
        self.handed += handed;
        if staged {
            self.handed_staged += handed;
        }
        if handed > long {
            self.long += 1;
        }
        self.longest = self.longest.max(handed);
    }
}

/// The six stamps of a plan's passes (its sync list's begin, after its sends, after its hash passes, the compare's
/// begin and end, after the sends' copies), if the slot is made.
fn pass_stamps(slot: usize) -> Option<[u64; PASS_STAMPS]> {
    let made = PASSES_MADE.lock().unwrap_or_else(|e| e.into_inner());
    let passes = made.as_ref()?.as_ref()?;
    // SAFETY: inside the mapping; written by the card at whole words; a slot is not used again for 4,096 plans.
    Some(unsafe {
        let at = (passes.data as *const u64).add(slot * PASS_STAMPS);
        std::array::from_fn(|i| at.add(i).read_volatile())
    })
}

impl Spans {
    /// The submission that ran from `start` to `end` (the card's ticks) after one that ended at `before`, and what the
    /// CPU did to send it (`VICTORY_CARD_GAPS=1`).
    fn read_gap(&mut self, queue: &ID3D12CommandQueue, before: u64, start: u64, end: u64, c: Calls) {
        if before == 0 || start == 0 || end < start || c.enter == 0 {
            return;
        }
        self.clock.refresh(queue);
        self.gaps.submissions += 1;
        let (p, f) = (self.clock.pc(before), self.clock.pc(start));
        if f <= p {
            return;
        }
        self.gaps.gapped += 1;
        self.gaps.total += f - p;
        let long = self.clock.pc_frequency / 10_000.0;
        let sync = c
            .slot
            .and_then(pass_stamps)
            .filter(|s| c.sync_enter != 0 && s[0] != 0 && s[2] >= s[0])
            .map(|s| (self.clock.pc(s[0]), self.clock.pc(s[2])));
        match sync {
            Some((from, to)) if from < f && to > p => {
                let (from, to) = (from.max(p), to.min(f));
                self.gaps.sync += to - from;
                self.gaps.synced += 1;
                self.gaps.staged += u64::from(c.staged);
                self.gaps.stretch((p, from), c.sync_enter, c.sync_ret, c.staged, long);
                self.gaps.stretch((to, f), c.enter, c.ret, false, long);
            }
            _ => self.gaps.stretch((p, f), c.enter, c.ret, false, long),
        }
    }

    /// The line of what the gaps were made of since the last one.
    fn gap_line(&mut self, frames: f64) {
        let g = std::mem::take(&mut self.gaps);
        let waits: Vec<u64> = WAITS.iter().map(|w| w.swap(0, Ordering::Relaxed)).collect();
        if g.submissions == 0 {
            return;
        }
        let hz = self.clock.pc_frequency;
        let ms = |t: f64| t / hz * 1000.0 / frames;
        let n = |c: u64| c as f64 / frames;
        let by = wait_site::NAMES
            .iter()
            .zip(&waits)
            .map(|(name, &w)| format!("{name} {:.1}", n(w)))
            .collect::<Vec<_>>()
            .join(", ");
        tracing::info!(
            "the card's idle between brackets, by what ran in it and what the CPU was doing (VICTORY_CARD_GAPS=1, the stamps against the PC's counter; {:.1} submissions a frame, {:.1} with a gap): {:.1} ms in all = our sync lists running {:.1} ms in {:.1} lists ({:.1} with their bytes crossing on a copy queue) + nothing handed over yet, the CPU not in the call {:.1} ms + the CPU inside the call {:.1} ms + handed over and not begun {:.1} ms (the idle with work queued: {:.1} stretches a frame over 100 us, the longest {:.0} us; {:.1} ms of it before a sync list behind the copy queue); GPU-side waits queued on the drawing queue per frame: {by}",
            n(g.submissions),
            n(g.gapped),
            ms(g.total),
            ms(g.sync),
            n(g.synced),
            n(g.staged),
            ms(g.nothing),
            ms(g.in_call),
            ms(g.handed),
            n(g.long),
            g.longest / hz * 1e6,
            ms(g.handed_staged),
        );
    }
}

/// Where a wait is put on the drawing queue (`VICTORY_CARD_GAPS=1` counts them: none stands between a submission's
/// lists unless one of these switches is on).
pub(crate) mod wait_site {
    /// The sync list behind the copy of its bytes on the copy queue (`VICTORY_SEND_COPYQ`).
    pub const STAGE: usize = 0;
    /// The landing's fetch behind the compute queue.
    pub const LANDING: usize = 1;
    /// A queue behind the other one (`VICTORY_ASYNC_COMPUTE`), or the compute queue behind a fetch.
    pub const LANES: usize = 2;
    /// The host queue behind a fence of a lane.
    pub const LANE_FENCE: usize = 3;
    pub const NAMES: [&str; 4] = ["copy queue stage", "landing fetch", "queue behind queue", "lane fence"];
}

static WAITS: [AtomicU64; 4] = [const { AtomicU64::new(0) }; 4];

/// A wait was queued at `site` of [`wait_site`].
pub(crate) fn wait_seen(site: usize) {
    if gaps_on() {
        WAITS[site].fetch_add(1, Ordering::Relaxed);
    }
}

impl Host {
    /// The CPU's side of the submission whose stamps `span_lists` gave out last (`VICTORY_CARD_GAPS=1`): the call of
    /// `ExecuteCommandLists` for its lists was entered at `enter` and it, with its signals, returned at `ret` (ticks
    /// of the PC's counter); `sync` is the call that sent its sync list: entered, returned, the slot of the plan's
    /// stamps, whether its bytes crossed on a copy queue.
    pub(super) fn span_called(&self, enter: u64, ret: u64, sync: Option<(u64, u64, usize, bool)>) {
        if !gaps_on() {
            return;
        }
        let mut made = SPANS_MADE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(Some(spans)) = made.as_mut() else {
            return;
        };
        let i = spans.given;
        spans.calls[i] = match sync {
            Some((sync_enter, sync_ret, slot, staged)) => Calls { enter, ret, sync_enter, sync_ret, slot: Some(slot), staged },
            None => Calls { enter, ret, ..Calls::default() },
        };
    }
}

/// Slots of stamps for the passes of plans ([`Host::pass_slot`]) before one is used again.
const PASS_SLOTS: usize = 4096;
/// Stamps of a slot: the sync list's begin, after its sends, after its hash passes; the compare list's begin and end;
/// and (with our own sends) after the copies of the sends, before their scatter and barriers.
const PASS_STAMPS: usize = 6;

/// The stamps of our own passes for copies in the card's memory
/// (`VICTORY_CARD_SPANS=1`): what the card spends on the sends up (copies and
/// the scatter of changed words), on the hash passes behind them, and on the
/// compare passes after a submission's lists (`Host::card_before`).
struct Passes {
    _heap: ID3D12QueryHeap,
    _back: ID3D12Resource,
    /// The mapped results, `PASS_STAMPS` a slot.
    data: usize,
    ticks_per_ms: f64,
    next: usize,
    /// Ticks of sends, hashes, compares, the copies of the sends alone, and the lists counted for each.
    ticks: [u64; 4],
    count: [u64; 4],
}

// SAFETY: free-threaded objects and a mapping only read behind a lock.
unsafe impl Send for Passes {}

static PASSES_MADE: Mutex<Option<Option<Passes>>> = Mutex::new(None);

/// Ticks between two stamps, if both were written.
fn between(a: u64, b: u64) -> Option<u64> {
    (a != 0 && b >= a).then(|| b - a)
}

impl Host {
    /// A slot of stamps for the plan being recorded, if asked for.
    pub(super) fn pass_slot(&self) -> Option<usize> {
        if !on() {
            return None;
        }
        let mut made = PASSES_MADE.lock().unwrap_or_else(|e| e.into_inner());
        let passes = made
            .get_or_insert_with(|| {
                self.make_passes()
                    .inspect_err(|e| tracing::warn!(error = %e, "no stamps around our passes"))
                    .ok()
            })
            .as_mut()?;
        let slot = passes.next;
        passes.next = (slot + 1) % PASS_SLOTS;
        // SAFETY: inside the mapping; the slot's last plan was read long ago.
        unsafe { std::ptr::write_bytes((passes.data as *mut u64).add(slot * PASS_STAMPS), 0, PASS_STAMPS) };
        Some(slot)
    }

    /// Stamp number `k` of `slot`, at this point of `list`.
    ///
    /// # Safety
    /// An open list of ours.
    pub(super) unsafe fn pass_stamp(&self, list: &ID3D12GraphicsCommandList, slot: Option<usize>, k: usize) {
        let Some(slot) = slot else {
            return;
        };
        let made = PASSES_MADE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(Some(passes)) = made.as_ref() else {
            return;
        };
        let at = (slot * PASS_STAMPS + k) as u32;
        // SAFETY: the caller's; the heap and the buffer live as long as the process.
        unsafe {
            list.EndQuery(&passes._heap, D3D12_QUERY_TYPE_TIMESTAMP, at);
            list.ResolveQueryData(&passes._heap, D3D12_QUERY_TYPE_TIMESTAMP, at, 1, &passes._back, u64::from(at) * 8);
        }
    }

    /// The plan that had `slot` has run: its stamps are added up.
    pub(super) fn pass_read(&self, slot: Option<usize>) {
        let Some(slot) = slot else {
            return;
        };
        let mut made = PASSES_MADE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(Some(passes)) = made.as_mut() else {
            return;
        };
        // SAFETY: inside the mapping; written by the card at whole words.
        let s: [u64; PASS_STAMPS] = unsafe {
            let at = (passes.data as *const u64).add(slot * PASS_STAMPS);
            std::array::from_fn(|i| at.add(i).read_volatile())
        };
        for (i, ticks) in [between(s[0], s[1]), between(s[1], s[2]), between(s[3], s[4]), between(s[0], s[5])]
            .into_iter()
            .enumerate()
        {
            if let Some(ticks) = ticks {
                passes.ticks[i] += ticks;
                passes.count[i] += 1;
            }
        }
    }

    fn make_passes(&self) -> windows::core::Result<Passes> {
        let device = &self.device;
        // SAFETY: plain object creation.
        unsafe {
            let n = PASS_SLOTS * PASS_STAMPS;
            let mut heap: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: n as u32, NodeMask: 0 },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let back = self.read_back_buffer(8 * n as u64).ok_or_else(windows::core::Error::empty)?;
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data))?;
            std::ptr::write_bytes(data.cast::<u8>(), 0, 8 * n);
            Ok(Passes {
                _heap: heap,
                _back: back,
                data: data as usize,
                ticks_per_ms: self.queue.GetTimestampFrequency()? as f64 / 1000.0,
                next: 0,
                ticks: [0; 4],
                count: [0; 4],
            })
        }
    }
}

/// What a list of a submission is, for [`Host::lap_lists`].
pub(super) mod lap {
    /// The stamp list of ours that opens the bracket of the submission.
    pub const FRONT: u8 = 0;
    /// The card steps list (constants copied on the card).
    pub const STEPS: u8 = 1;
    /// A title list's prelude (texture transitions and twin copies).
    pub const PRELUDE: u8 = 2;
    /// A title list of the drawing type.
    pub const TITLE: u8 = 3;
    /// A title list of the compute type.
    pub const TITLE_C: u8 = 4;
    /// The compare list behind the lists.
    pub const CHECK: u8 = 5;
    /// The stamp list that closes the bracket.
    pub const BEHIND: u8 = 6;
    /// Another stamp list of ours (the timeline's).
    pub const OTHER: u8 = 7;
    pub const KINDS: usize = 8;
    pub const NAMES: [&str; KINDS] = [
        "front stamp",
        "card steps lists",
        "preludes",
        "title lists (drawing type)",
        "title lists (compute type)",
        "compare lists",
        "back stamp",
        "timeline stamps",
    ];
}

/// Stamps of laps before one is used again: a submission uses one for each of its lists, and some thirty
/// submissions are in the air at most.
const LAP_SLOTS: usize = 4096;

/// One submission's laps: where its stamps are, what each list was, and whether it opened a later call of
/// `ExecuteCommandLists` (what the card did before it may be waiting for the call).
struct Pending {
    first: u64,
    kinds: Vec<u8>,
    starts: Vec<bool>,
}

/// The stamps behind each list of the submissions (`VICTORY_CARD_SPANS=2`): what the card spends on each kind of
/// list inside the bracket of the stamps of [`Spans`], without making the card run the commands one by one.
/// The time of a list is the time from the stamp behind the list before it to the one behind it: whatever the
/// card waited for before the list is in it.
struct Laps {
    _allocator: ID3D12CommandAllocator,
    _heap: ID3D12QueryHeap,
    _back: ID3D12Resource,
    data: usize,
    lists: Vec<ID3D12CommandList>,
    ticks_per_ms: f64,
    /// The number of stamps given out so far.
    head: u64,
    pending: std::collections::VecDeque<Pending>,
    /// Ticks, lists and the longest list by kind; [0] the lists that follow another in their call, [1] those that
    /// open a later call.
    ticks: [[u64; 2]; lap::KINDS],
    count: [[u64; 2]; lap::KINDS],
    longest: [u64; lap::KINDS],
    /// Submissions read, their brackets' ticks (front stamp to back stamp), and the calls they were sent in.
    submissions: u64,
    bracket: u64,
    calls: u64,
}

// SAFETY: free-threaded objects and a mapping only read behind a lock.
unsafe impl Send for Laps {}

static LAPS_MADE: Mutex<Option<Option<Laps>>> = Mutex::new(None);

impl Laps {
    /// The submissions that ran long ago: their stamps are read, added up, and cleared.
    fn read_old(&mut self) {
        while let Some(front) = self.pending.front() {
            let end = front.first + front.kinds.len() as u64;
            if self.head < end + (LAP_SLOTS / 2) as u64 {
                break;
            }
            let Some(p) = self.pending.pop_front() else {
                break;
            };
            // SAFETY: inside the mapping; written by the card at whole words.
            let s: Vec<u64> = (0..p.kinds.len())
                .map(|j| unsafe {
                    let w = (self.data as *mut u64).add((p.first as usize + j) % LAP_SLOTS);
                    let v = w.read_volatile();
                    w.write_volatile(0);
                    v
                })
                .collect();
            self.submissions += 1;
            self.calls += 1 + p.starts.iter().filter(|&&b| b).count() as u64;
            if let (Some(&a), Some(&b)) = (s.first(), s.last())
                && a != 0
                && b >= a
            {
                self.bracket += b - a;
            }
            for j in 1..s.len() {
                if s[j - 1] == 0 || s[j] < s[j - 1] {
                    continue;
                }
                let ticks = s[j] - s[j - 1];
                let (k, later) = (p.kinds[j] as usize % lap::KINDS, usize::from(p.starts[j]));
                self.ticks[k][later] += ticks;
                self.count[k][later] += 1;
                self.longest[k] = self.longest[k].max(ticks);
            }
        }
    }
}

impl Host {
    /// With `VICTORY_CARD_SPANS=2`: a list of nothing but a timestamp goes behind each of `lists` (those of a
    /// submission about to be sent, `kinds` what each is), and `firsts`, where the runs begin, follow. `parts` are
    /// the runs that begin a later call of `ExecuteCommandLists`. The stamps are read some submissions later.
    pub(super) fn lap_lists(
        &self,
        lists: &mut Vec<Option<ID3D12CommandList>>,
        firsts: &mut [usize],
        kinds: &[u8],
        parts: &[usize],
    ) {
        if !laps_on() || kinds.len() != lists.len() || lists.is_empty() || lists.len() > LAP_SLOTS / 8 {
            return;
        }
        let mut made = LAPS_MADE.lock().unwrap_or_else(|e| e.into_inner());
        let Some(laps) = made
            .get_or_insert_with(|| {
                self.make_laps().inspect_err(|e| tracing::warn!(error = %e, "no stamps behind the lists")).ok()
            })
            .as_mut()
        else {
            return;
        };
        laps.read_old();
        let n = lists.len();
        let mut starts = vec![false; n];
        for &p in parts {
            if let Some(&f) = firsts.get(p)
                && f < n
                && f > 0
            {
                starts[f] = true;
            }
        }
        let first = laps.head;
        let mut with = Vec::with_capacity(2 * n);
        for (i, list) in lists.drain(..).enumerate() {
            with.push(list);
            with.push(Some(laps.lists[(first as usize + i) % LAP_SLOTS].clone()));
        }
        *lists = with;
        for f in firsts.iter_mut() {
            *f *= 2;
        }
        laps.head += n as u64;
        laps.pending.push_back(Pending { first, kinds: kinds.to_vec(), starts });
    }

    fn make_laps(&self) -> windows::core::Result<Laps> {
        let device = &self.device;
        // SAFETY: plain object creation; lists of one allocator recorded one after the other.
        unsafe {
            let mut heap: Option<ID3D12QueryHeap> = None;
            device.CreateQueryHeap(
                &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: LAP_SLOTS as u32, NodeMask: 0 },
                &mut heap,
            )?;
            let heap = heap.ok_or_else(windows::core::Error::empty)?;
            let back = self.read_back_buffer(8 * LAP_SLOTS as u64).ok_or_else(windows::core::Error::empty)?;
            let mut data = std::ptr::null_mut();
            back.Map(0, None, Some(&mut data))?;
            std::ptr::write_bytes(data.cast::<u8>(), 0, 8 * LAP_SLOTS);
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let mut lists = Vec::with_capacity(LAP_SLOTS);
            for i in 0..LAP_SLOTS as u32 {
                let list: ID3D12GraphicsCommandList =
                    device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
                list.EndQuery(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i);
                list.ResolveQueryData(&heap, D3D12_QUERY_TYPE_TIMESTAMP, i, 1, &back, u64::from(i) * 8);
                list.Close()?;
                lists.push(list.cast()?);
            }
            Ok(Laps {
                _allocator: allocator,
                _heap: heap,
                _back: back,
                data: data as usize,
                lists,
                ticks_per_ms: self.queue.GetTimestampFrequency()? as f64 / 1000.0,
                head: 0,
                pending: Default::default(),
                ticks: Default::default(),
                count: Default::default(),
                longest: Default::default(),
                submissions: 0,
                bracket: 0,
                calls: 0,
            })
        }
    }
}

/// The GPU timestamps the title was given since the last frame-rate line:
/// how many, how often a later one was 1 ms or more after the one before
/// (written with another submission), and where the first few lie.
static STAMPED: Mutex<(u64, u64, u64, Vec<usize>)> = Mutex::new((0, 0, 0, Vec::new()));

/// The title's GPU timestamp at `at` was given the value `now` (ticks of 10
/// ns).
pub(crate) fn stamped(at: usize, now: u64) {
    if !on() {
        return;
    }
    let mut s = STAMPED.lock().unwrap_or_else(|e| e.into_inner());
    s.0 += 1;
    s.1 += u64::from(now.saturating_sub(s.2) > 100_000);
    s.2 = now;
    if s.3.len() < 64 && !s.3.contains(&at) {
        s.3.push(at);
        tracing::info!(address = format_args!("{at:#x}"), now, "a GPU timestamp of the title");
    }
}

/// The viewports' sizes seen (each said once): whether the title draws its
/// scene at sizes of its choice.
pub(crate) fn viewport(width: u32, height: u32) {
    if !on() || width < 1024 || height < 576 {
        return;
    }
    static SEEN: Mutex<Vec<(u32, u32)>> = Mutex::new(Vec::new());
    let mut seen = SEEN.lock().unwrap_or_else(|e| e.into_inner());
    if seen.len() < 200 && !seen.contains(&(width, height)) {
        seen.push((width, height));
        tracing::info!(width, height, "a viewport of the title");
    }
}

/// For the frame-rate line: what the stamps read since the last one say, per
/// frame (they are of some [`SPANS`] submissions ago).
pub(super) fn line(frames: f64) {
    {
        let mut s = STAMPED.lock().unwrap_or_else(|e| e.into_inner());
        if s.0 > 0 {
            tracing::info!(
                "GPU timestamps the title asked for per frame: {:.1}, in {:.1} groups written at one time",
                s.0 as f64 / frames,
                s.1 as f64 / frames
            );
        }
        (s.0, s.1) = (0, 0);
    }
    {
        let mut made = PASSES_MADE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(Some(p)) = made.as_mut() {
            let ms = |i: usize| p.ticks[i] as f64 / p.ticks_per_ms / frames;
            let n = |i: usize| p.count[i] as f64 / frames;
            tracing::info!(
                "our passes on the card per frame (VICTORY_CARD_SPANS=1, by its stamps): sends up (copies and the scatter of changed words) {:.2} ms in {:.1} lists (the copies alone {:.2} ms in {:.1}), hash passes {:.2} ms in {:.1} lists, compare passes {:.2} ms in {:.1} lists",
                ms(0),
                n(0),
                ms(3),
                n(3),
                ms(1),
                n(1),
                ms(2),
                n(2)
            );
            (p.ticks, p.count) = ([0; 4], [0; 4]);
        }
    }
    if on() {
        let counts = super::ours::take();
        let all: u64 = counts.iter().map(|c| c.1).sum();
        let by = counts.iter().map(|(name, n)| format!("{name} {:.0}", *n as f64 / frames)).collect::<Vec<_>>().join(", ");
        tracing::info!(
            "full waits of OUR recording per frame (each drains the queue; the title's own are on the line of waits for every earlier shader write): {:.0} in all: {by}",
            all as f64 / frames
        );
    }
    {
        let mut made = LAPS_MADE.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(Some(l)) = made.as_mut() {
            l.read_old();
            let ms = |t: u64| t as f64 / l.ticks_per_ms / frames;
            let n = |c: u64| c as f64 / frames;
            let part = |k: u8| {
                let k = k as usize;
                let (t, c) = (l.ticks[k][0] + l.ticks[k][1], l.count[k][0] + l.count[k][1]);
                format!(
                    "{} {:.2} ms in {:.1} (longest {:.2} ms, {:.1} us each; the first of a later call: {:.2} ms in {:.1})",
                    lap::NAMES[k],
                    ms(t),
                    n(c),
                    l.longest[k] as f64 / l.ticks_per_ms,
                    if c > 0 { t as f64 * 1000.0 / l.ticks_per_ms / c as f64 } else { 0.0 },
                    ms(l.ticks[k][1]),
                    n(l.count[k][1])
                )
            };
            let total: u64 = (0..lap::KINDS).map(|k| l.ticks[k][0] + l.ticks[k][1]).sum();
            tracing::info!(
                "the card's time by kind of list per frame (VICTORY_CARD_SPANS=2, a stamp behind each list; {:.1} submissions in {:.1} calls of ExecuteCommandLists; the bracket from the front stamp to the back stamp {:.2} ms, the lists in it add up to {:.2} ms): {}; {}; {}; {}; {}; {}",
                n(l.submissions),
                n(l.calls),
                ms(l.bracket),
                ms(total),
                part(lap::STEPS),
                part(lap::PRELUDE),
                part(lap::TITLE),
                part(lap::TITLE_C),
                part(lap::CHECK),
                part(lap::OTHER)
            );
            l.ticks = Default::default();
            l.count = Default::default();
            l.longest = Default::default();
            (l.submissions, l.bracket, l.calls) = (0, 0, 0);
        }
    }
    let mut made = SPANS_MADE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(spans)) = made.as_mut() else {
        return;
    };
    let last = spans.sums.last;
    let s = std::mem::replace(&mut spans.sums, Sums { last, ..Sums::default() });
    let ms = |ticks: u64| ticks as f64 / spans.ticks_per_ms / frames;
    tracing::info!(
        "the card's own stamps per frame: {:.1} submissions, working {:.1} ms, idle between them {:.1} ms, the longest {:.1} ms",
        s.count as f64 / frames,
        ms(s.busy),
        ms(s.between),
        s.longest as f64 / spans.ticks_per_ms
    );
    if gaps_on() {
        spans.gap_line(frames);
    }
}

#[cfg(test)]
mod tests {
    use super::{Clock, Gaps, Sums, between, split_stretch};

    #[test]
    fn a_stretch_is_told_apart_by_the_call_that_ended_it() {
        // The CPU had not entered the call when the card ran out: all of it is the CPU's.
        assert_eq!(split_stretch(100.0, 200.0, 250.0, 260.0), (100.0, 0.0, 0.0));
        // Entered before the card ran out, back after: the card waited inside the call.
        assert_eq!(split_stretch(100.0, 200.0, 50.0, 300.0), (0.0, 100.0, 0.0));
        // Entered inside the stretch, back inside it: three parts.
        assert_eq!(split_stretch(100.0, 200.0, 120.0, 150.0), (20.0, 30.0, 50.0));
        // Back before the card ran out: the work was handed over and the card did not begin it.
        assert_eq!(split_stretch(100.0, 200.0, 10.0, 20.0), (0.0, 0.0, 100.0));
        // A return stamp before the entry (clock noise) does not make a negative part.
        let (a, b, c) = split_stretch(100.0, 200.0, 150.0, 140.0);
        assert!(a >= 0.0 && b >= 0.0 && c >= 0.0);
        assert!((a + b + c - 100.0).abs() < 1e-9);
    }

    #[test]
    fn stretches_add_up_to_the_gap() {
        let mut g = Gaps::default();
        g.stretch((0.0, 1000.0), 400, 600, false, 100.0);
        g.stretch((2000.0, 2300.0), 0, 10, true, 100.0);
        g.stretch((5.0, 5.0), 0, 0, false, 100.0);
        assert!((g.nothing + g.in_call + g.handed - 1300.0).abs() < 1e-9);
        assert_eq!((g.nothing, g.in_call, g.handed), (400.0, 200.0, 700.0));
        assert_eq!((g.handed_staged, g.long, g.longest), (300.0, 2, 400.0));
    }

    #[test]
    fn the_cards_clock_is_the_pcs_by_the_rate_between_calibrations() {
        // Nominal: 1 GHz card, 10 MHz counter.
        let mut c = Clock { card0: 1_000, pc0: 500, card: 1_000, pc: 500, reads: 0, card_frequency: 1e9, pc_frequency: 1e7 };
        assert!((c.pc(1_000 + 1_000_000) - (500.0 + 10_000.0)).abs() < 1e-6);
        // Three seconds on, the card's clock ran a part in a thousand slow: the measured rate wins.
        c.card = c.card0 + 2_997_000_000;
        c.pc = c.pc0 + 30_000_000;
        assert!((c.rate() - 30_000_000.0 / 2_997_000_000.0).abs() < 1e-12);
        assert!((c.pc(c.card) - c.pc as f64).abs() < 1e-6);
    }

    #[test]
    fn submissions_add_up_without_counting_anything_twice() {
        let mut s = Sums::default();
        s.add(100, 150);
        s.add(140, 200); // began before the last one ended
        s.add(260, 270);
        assert_eq!((s.count, s.busy, s.between, s.longest, s.last), (3, 110, 60, 50, 270));
        // Never written, or nonsense: nothing.
        s.add(0, 0);
        s.add(300, 290);
        assert_eq!(s.count, 3);
        // Whole inside the last one.
        s.add(265, 268);
        assert_eq!((s.count, s.busy, s.between), (4, 110, 60));
    }

    #[test]
    fn pass_stamps_count_only_when_both_were_written() {
        assert_eq!(between(100, 150), Some(50));
        assert_eq!(between(0, 150), None);
        assert_eq!(between(150, 100), None);
        assert_eq!(between(100, 100), Some(0));
    }
}
