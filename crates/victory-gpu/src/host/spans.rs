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

/// Query/list pairs kept in the ring; reuse is guarded by their actual fence.
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
    /// Last submission fence using each query/list/readback pair; 0 is unused.
    last_fence: [u64; SPANS],
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
    /// Sampling attempts skipped instead of waiting for a slot or lost device.
    skipped: u64,
}

/// Whether a slot has an earlier result that must be read before reuse.
#[derive(Debug, PartialEq, Eq)]
enum SlotRead {
    Unused,
    Completed,
}

/// Reserves a slot without waiting. The existing queue signal for `target`
/// must follow the lists using it. A failed reservation leaves its old fence
/// intact, so a later attempt cannot mistake pending results for an unused slot.
fn reserve_slot(last_fence: &mut u64, completed: u64, target: u64) -> Option<SlotRead> {
    if completed == u64::MAX || target == 0 || target == u64::MAX || (*last_fence != 0 && completed < *last_fence) {
        return None;
    }
    let read = if *last_fence == 0 { SlotRead::Unused } else { SlotRead::Completed };
    *last_fence = target;
    Some(read)
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
    /// sent to the drawing queue, if stamps are asked for. The caller holds
    /// the submission lock and has reserved `target` on this host's fence;
    /// its signal follows the returned lists in the existing submission.
    pub(super) fn span_lists(&self, target: u64) -> Option<[ID3D12CommandList; 2]> {
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
        // SAFETY: a plain query of the fence that follows this ring's lists.
        let completed = unsafe { self.fence.GetCompletedValue() };
        let Some(read) = reserve_slot(&mut spans.last_fence[i], completed, target) else {
            spans.sums.skipped += 1;
            return None;
        };
        if read == SlotRead::Completed {
            // SAFETY: both results are inside the mapping, and the recorded
            // submission fence proves their resolves and earlier list use
            // completed. No new use can be enqueued until this call returns
            // under the submission lock. An unused slot is never read, and
            // the CPU never writes the readback buffer.
            let (start, end) = unsafe {
                let at = (spans.data as *const u64).add(2 * i);
                (at.read_volatile(), at.add(1).read_volatile())
            };
            spans.sums.add(start, end);
        }
        spans.next = (i + 1) % SPANS;
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
                last_fence: [0; SPANS],
                sums: Sums::default(),
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
/// frame (they are of some [`SPANS`] submissions ago). This divides completed
/// older samples by the current meter's frame count: a steady-scene average,
/// not completed per-frame durations. Brackets include title lists, preludes,
/// comparisons and present copies. Upload/rehash and fetch lists outside the
/// brackets can run between samples, so that interval is not all GPU idle time.
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
    let mut made = SPANS_MADE.lock().unwrap_or_else(|e| e.into_inner());
    let Some(Some(spans)) = made.as_mut() else {
        return;
    };
    let last = spans.sums.last;
    let s = std::mem::replace(&mut spans.sums, Sums { last, ..Sums::default() });
    let ms = |ticks: u64| ticks as f64 / spans.ticks_per_ms / frames;
    tracing::info!(
        "the card's own stamps per frame: {:.1} submissions, working inside their brackets {:.1} ms, \
         between brackets {:.1} ms (includes unmeasured helpers and idle), the longest {:.1} ms, {:.1} samples skipped",
        s.count as f64 / frames,
        ms(s.busy),
        ms(s.between),
        s.longest as f64 / spans.ticks_per_ms,
        s.skipped as f64 / frames
    );
}

#[cfg(test)]
mod tests {
    use super::{SlotRead, Sums, reserve_slot};

    #[test]
    fn unused_timestamp_slot_is_reserved_without_reading_a_result() {
        let mut last = 0;
        assert_eq!(reserve_slot(&mut last, 0, 10), Some(SlotRead::Unused));
        assert_eq!(last, 10);
    }

    #[test]
    fn pending_timestamp_slot_keeps_its_fence_until_completion() {
        let mut last = 10;
        for completed in [0, 8, 9] {
            assert_eq!(reserve_slot(&mut last, completed, 11), None);
            assert_eq!(last, 10);
        }
        // Exactly the old fence suffices; the new execution is pending again.
        assert_eq!(reserve_slot(&mut last, 10, 11), Some(SlotRead::Completed));
        assert_eq!(last, 11);
        assert_eq!(reserve_slot(&mut last, 10, 12), None);
        assert_eq!(last, 11);
        assert_eq!(reserve_slot(&mut last, 15, 16), Some(SlotRead::Completed));
        assert_eq!(last, 16);
    }

    #[test]
    fn device_loss_never_authorizes_timestamp_slot_access() {
        for mut last in [0, 10, u64::MAX] {
            let before = last;
            assert_eq!(reserve_slot(&mut last, u64::MAX, 11), None);
            assert_eq!(last, before);
        }
    }

    #[test]
    fn timestamp_slot_requires_a_real_reserved_target() {
        for mut last in [0, 10] {
            for target in [0, u64::MAX] {
                let before = last;
                assert_eq!(reserve_slot(&mut last, 10, target), None);
                assert_eq!(last, before);
            }
        }
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
}
