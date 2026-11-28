//! Diagnostics: how long the card itself works on our submissions, by its own
//! timestamps (`VICTORY_CARD_SPANS=1`). The clock of the frame line
//! (`card_clock_line`) counts from the call that sends a submission to the
//! moment a thread of ours wakes behind its last signal: the time the system
//! takes to hand a submission to the card and to report it done is in it, 40
//! times a frame. Here two lists made once, of nothing but a timestamp, stand
//! around each submission.

use std::sync::Mutex;

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
    *ON.get_or_init(|| std::env::var_os("VICTORY_CARD_SPANS").is_some_and(|v| v == "1"))
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
        spans.sums.add(start, end);
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
            Ok(Spans {
                _allocator: allocator,
                _heap: heap,
                _back: back,
                data: data as usize,
                lists,
                ticks_per_ms: self.queue.GetTimestampFrequency()? as f64 / 1000.0,
                next: 0,
                sums: Sums::default(),
            })
        }
    }
}

/// Slots of stamps for the passes of plans ([`Host::pass_slot`]) before one is used again.
const PASS_SLOTS: usize = 4096;
/// Stamps of a slot: the sync list's begin, after its sends, its end; the compare list's begin and end.
const PASS_STAMPS: usize = 5;

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
    /// Ticks of sends, hashes, compares, and the lists counted for each.
    ticks: [u64; 3],
    count: [u64; 3],
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
        for (i, ticks) in [between(s[0], s[1]), between(s[1], s[2]), between(s[3], s[4])].into_iter().enumerate() {
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
                ticks: [0; 3],
                count: [0; 3],
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
                "our passes on the card per frame (VICTORY_CARD_SPANS=1, by its stamps): sends up (copies and the scatter of changed words) {:.2} ms in {:.1} lists, hash passes {:.2} ms in {:.1} lists, compare passes {:.2} ms in {:.1} lists",
                ms(0),
                n(0),
                ms(1),
                n(1),
                ms(2),
                n(2)
            );
            (p.ticks, p.count) = ([0; 3], [0; 3]);
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
}

#[cfg(test)]
mod tests {
    use super::{Sums, between};

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
