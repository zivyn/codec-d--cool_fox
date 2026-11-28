//! The host GPU: one D3D12 device and direct queue (decision 0007), and a
//! window to present on.
//!
//! shortcut: every submission is waited for, so CPU and GPU never overlap
//! (unless the submissions are sent ahead, on by default: `VICTORY_LATE_WAIT`, see [`late`]);
//! overlap them with per-submission fences once more than clears run.

use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};

use windows::Win32::Foundation::{HANDLE, HWND, LPARAM, LRESULT, RECT, WPARAM};
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::LibraryLoader::GetModuleHandleW;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::Win32::UI::WindowsAndMessaging::*;
use windows::core::{Interface, PCWSTR, w};

mod frame_line;
mod batches;
mod landing;
mod prepare;
mod card_sync;
#[cfg(test)]
mod tests;
#[cfg(test)]
mod relay_tests;
#[cfg(test)]
mod late_tests;
#[cfg(test)]
mod wide_refresh_tests;
#[cfg(test)]
mod written_spans_tests;
#[cfg(test)]
mod trail;
#[cfg(test)]
mod spare;
#[cfg(test)]
mod by_copy_tests;
#[cfg(test)]
mod refresh_tests;
#[cfg(test)]
mod batch;
#[cfg(test)]
mod join_tests;
use self::frame_line::{card_clock_line, late_line};
#[cfg(feature = "diagnostics")]
use self::frame_line::{lines_aside, split_line};
use self::batches::{
    Batch, COPIES_JOINED, Touch, ahead_of_writes, continues, front_spans_gap, join_copies,
    narrow_on, ranges_cover, refresh_check, refresh_front, refresh_narrow_gap, wide_fills,
    wide_textures,
};
#[cfg(test)]
use self::batches::{
    front_spans, front_spans_flat, joined_with_gap, longest_first, referenced_bytes, retain_where,
    wide_refresh_enabled, written_spans, written_spans_gap,
};
pub(crate) use self::landing::{relay, walls_trace};
use self::landing::{
    HELPER_UP, QUEUE_ORDER, RELAY_MOVED, RELAY_STATS, declare_once, declared_writes, hand_back,
    land, late_threads, read_backs, relay_exact, relay_wait,
};
#[cfg(test)]
use self::landing::{declared_writes_steps, declared_writes_summarized, flagged_pieces, relay_copy};

/// Resource states without the console's own bits (compression hints from
/// bit 25 up), which the host rejects.
pub(crate) fn pc_state(state: u32) -> u32 {
    state & 0x01ff_ffff
}

/// A console state as a host texture state: PC bits only, and none of the
/// buffer-only ones the console allows on textures (vertex, constant and
/// index buffer, stream out, indirect arguments, as in "generic read").
pub(crate) fn texture_state(state: u32) -> u32 {
    const BUFFER_ONLY: u32 = 0x1 | 0x2 | 0x100 | 0x200;
    pc_state(state) & !BUFFER_ONLY
}

/// Render target views live only until their command is recorded, so a
/// ring of host descriptors is enough.
const RTV_RING: u32 = 1024;

pub(crate) struct Host {
    pub(crate) device: ID3D12Device,
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// The last value signalled on `fence`, behind the submission lock.
    submitted: Mutex<u64>,
    event: HANDLE,
    rtvs: ID3D12DescriptorHeap,
    rtv_size: u32,
    /// A ring of depth views, used as `rtvs`.
    dsvs: ID3D12DescriptorHeap,
    dsv_size: u32,
    next_dsv: AtomicU32,
    /// A ring of CPU-only view slots: the CPU side of a clear of an
    /// unordered access view, used as `rtvs`.
    cpu_views: ID3D12DescriptorHeap,
    cpu_view_size: u32,
    next_cpu_view: AtomicU32,
    /// With `VICTORY_D3D12_DEBUG` set (diagnostics builds): the debug
    /// layer's messages.
    messages: Option<ID3D12InfoQueue>,
    next_rtv: AtomicU32,
    presenter: Mutex<Option<Presenter>>,
    /// The adapter the device is on, for its memory budget
    /// ([`Host::video_memory`]); `None` if the system does not name it.
    pub(crate) adapter: Option<IDXGIAdapter3>,
    /// 1-descriptor shader-visible fallback heaps bound when a command list
    /// omits a view or sampler heap while using directly-indexed root signatures.
    fallback_view: OnceLock<Option<ID3D12DescriptorHeap>>,
    fallback_sampler: OnceLock<Option<ID3D12DescriptorHeap>>,
}

// SAFETY: D3D12 devices, queues, fences and descriptor heaps are
// free-threaded; the event handle is only waited on under `submitted`.
unsafe impl Send for Host {}
// SAFETY: as above.
unsafe impl Sync for Host {}

/// Host objects the title let go of while a submission may still use them
/// (a texture its descriptors name, a descriptor slot's old resource), by
/// the submission count they wait for. The title's descriptor writes and
/// releases reach the host at once, from its own threads, while the host
/// runs a submission: freeing there is a GPU page fault on the freed
/// texture (a device hang).
static RETIRED: Mutex<std::collections::VecDeque<(u64, Retired)>> = Mutex::new(std::collections::VecDeque::new());
/// Submissions started and finished (waited for).
static STARTED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static FINISHED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// A retired object, dropped on whatever thread finishes the submission.
#[allow(dead_code)]
enum Retired {
    Any(Box<dyn std::any::Any>),
    /// What a descriptor slot showed: 20,000 a frame, without a box each.
    Resource(ID3D12Resource),
}
// SAFETY: what is retired is D3D12 objects (free-threaded) and the
// reference-counted wrappers holding them, dropped once.
unsafe impl Send for Retired {}

/// Milliseconds since the first call.
pub(crate) fn now_ms() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_millis() as u64
}

/// [`now_ms`] to the system's tick (16 ms): a few nanoseconds, where the
/// performance counter's are 25. For what is noted tens of thousands of
/// times a frame and read in tenths of a second.
pub(crate) fn coarse_ms() -> u64 {
    use windows_sys::Win32::System::SystemInformation::GetTickCount64;
    // The same beginning as `now_ms`.
    static BEGAN: OnceLock<u64> = OnceLock::new();
    // SAFETY: a plain query.
    let now = unsafe { GetTickCount64() };
    now.saturating_sub(*BEGAN.get_or_init(|| now.saturating_sub(now_ms())))
}

/// This moment, for telling later how long ago it was ([`long_ago`]): the
/// frames begun so far (high half, `crate::frame::current`) and tenths of a
/// second since the start. Host copies of title memory, staging buffers and
/// host textures note when they were last used with it.
pub(crate) fn moment() -> u64 {
    (crate::frame::current() << 32) | ((coarse_ms() / 100) & 0xffff_ffff)
}

/// Whether `moment` lies at least `ms` milliseconds and `frames` frames
/// back. Both: at seconds per frame, what every frame uses is not touched
/// for seconds, and at many frames per second not for many frames. (A title
/// that never waits for frames has no frame count: nothing is then idle.)
pub(crate) fn long_ago(moment: u64, ms: u64, frames: u64) -> bool {
    let now = self::moment();
    let tenths = (now & 0xffff_ffff).saturating_sub(moment & 0xffff_ffff);
    (now >> 32).saturating_sub(moment >> 32) >= frames && tenths * 100 >= ms
}

/// Available physical RAM on the host in bytes, from `GlobalMemoryStatusEx`.
pub(crate) fn available_physical_ram() -> Option<u64> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    // SAFETY: plain data the call fills in.
    let mut m: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    m.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    // SAFETY: a sized structure.
    let ok = unsafe { GlobalMemoryStatusEx(&mut m) } != 0;
    ok.then_some(m.ullAvailPhys)
}

/// How long host copies of textures the title fills must have been idle to
/// be let go, in milliseconds, and how many bytes of them go at most (the
/// longest idle first): none but those of ten minutes while what is in use
/// of the card (`used`: without what our own blocks have free, with what is
/// kept free for others) is under 88 % of its budget, else enough to come
/// down to 85 %. `VICTORY_TEXTURE_KEEP=0`: [`vram_idle_ms`]' older rule,
/// all that are idle; `=1`: that rule with its two longer times.
///
/// (By those times everything idle for 2.5 s went from 75 % of the budget
/// on: 61,000 of them in the first 300 s of session play-1125-0203, at 78 to
/// 80 %, of which 13,000 were made and uploaded again.)
pub(crate) fn vram_keep(budget: u64, used: u64) -> (u64, u64) {
    const KEPT_MS: u64 = 600_000;
    const IDLE_MS: u64 = 5_000;
    const SHORT_MS: u64 = 1_500;
    if budget == 0 {
        return (IDLE_MS, u64::MAX);
    }
    let share = |percent: u64| budget / 100 * percent;
    if used < share(88) {
        (KEPT_MS, u64::MAX)
    } else if used < share(95) {
        (IDLE_MS, used - share(85))
    } else {
        (SHORT_MS, used - share(85))
    }
}

/// Effective idle timeout (in milliseconds) for VRAM-backed host textures,
/// scaled down under VRAM pressure. Never returns less than 1,000 ms so
/// handles not yet recorded into a list or slot remain protected.
///
/// With room on the card nothing has to go: in session play-1125-0127 88,000
/// of them (31 GB) went after 5 s with 7 GB of the card free, and 24,700
/// (12 GB) had to be made and uploaded again.
/// `VICTORY_TEXTURE_KEEP=0`: 5 s there too, as before.
pub(crate) fn vram_idle_ms(video_memory: Option<[u64; 4]>) -> u64 {
    static KEEP: OnceLock<bool> = OnceLock::new();
    let keep = *KEEP.get_or_init(|| std::env::var_os("VICTORY_TEXTURE_KEEP").is_none_or(|v| v != "0"));
    vram_idle_for(video_memory, keep)
}

fn vram_idle_for(video_memory: Option<[u64; 4]>, keep: bool) -> u64 {
    const KEPT_MS: u64 = 600_000;
    const LONG_MS: u64 = 30_000;
    const NORMAL_MS: u64 = 5_000;
    const MODERATE_MS: u64 = 2_500;
    const HIGH_MS: u64 = 1_000;
    let Some([budget, used, ..]) = video_memory else {
        return NORMAL_MS;
    };
    if budget == 0 {
        return NORMAL_MS;
    }
    let used_pct = (u128::from(used) * 100) / u128::from(budget);
    if used_pct >= 85 {
        HIGH_MS
    } else if used_pct >= 75 {
        MODERATE_MS
    } else if !keep {
        NORMAL_MS
    } else if used_pct >= 68 {
        LONG_MS
    } else {
        KEPT_MS
    }
}

/// Whether new memory is made resident, and waited for, before anything can
/// name it (on by default; `VICTORY_WAIT_RESIDENT=0`: as before). A heap or
/// resource of the default kind is resident once it is made, but its
/// mapping into the card's address space is work of the system's that runs
/// later: only lists sent after it wait for it. Lists sent before run on,
/// and read our descriptor heaps as they are: a slot that shows a texture
/// in a new block is a page fault for them until the block is mapped. The
/// device losses of the night of 2026-11-25 were that (run n08: the fault
/// lay in a texture made in the same tenth of a second, in a new block).
fn wait_resident() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WAIT_RESIDENT").is_none_or(|v| v != "0"))
}

/// The flag memory is made with that [`resident`] waits for.
pub(crate) fn not_resident() -> D3D12_HEAP_FLAGS {
    if wait_resident() { D3D12_HEAP_FLAG_CREATE_NOT_RESIDENT } else { D3D12_HEAP_FLAG_NONE }
}

/// A queue of its own that new memory is waited for with ([`mapped`]).
struct Aside {
    queue: ID3D12CommandQueue,
    list: ID3D12CommandList,
    fence: ID3D12Fence,
    event: windows::Win32::Foundation::HANDLE,
    value: u64,
    _keep: (ID3D12CommandAllocator, [ID3D12Resource; 2]),
}
// SAFETY: D3D12 objects are free-threaded, the event is waited on by one
// thread at a time (the lock).
unsafe impl Send for Aside {}

/// New memory of the card is mapped into its address space when this
/// returns: a copy of a few bytes is sent to a queue of ours after the
/// memory was made, which the system starts only when all mappings asked
/// for so far are done, and waited for. (`MakeResident` came back before
/// that: run n11 was lost as n08 was with it alone.) Best done off a hot
/// path: a tenth of a millisecond to some.
pub(crate) fn mapped(device: &ID3D12Device) -> windows::core::Result<()> {
    if !wait_resident() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    if batch_residency() {
        // One round trip serves every thread that asks while another runs
        // (`combine`): each of them still returns only after a round trip that
        // began after its memory was made.
        MAPPED_GROUP.join((), |items| match map_round_trip(device) {
            Ok(()) => {
                MAPPED_BATCHES.fetch_add(1, Ordering::Relaxed);
                Vec::new()
            }
            Err(e) => items.iter().map(|(id, _)| (*id, e.clone())).collect(),
        })?;
    } else {
        map_round_trip(device)?;
    }
    let us = started.elapsed().as_micros() as u64;
    MAPPED[0].fetch_add(1, Ordering::Relaxed);
    MAPPED[1].fetch_add(us, Ordering::Relaxed);
    MAPPED[2].fetch_max(us, Ordering::Relaxed);
    let n = MAPPED[0].load(Ordering::Relaxed);
    if n == 1 || n.is_multiple_of(200) {
        tracing::info!(
            "new card memory waited for until it is mapped (VICTORY_WAIT_RESIDENT=0: not), since the start: {n} times, {:.1} ms, the longest {:.1} ms",
            MAPPED[1].load(Ordering::Relaxed) as f64 / 1000.0,
            MAPPED[2].load(Ordering::Relaxed) as f64 / 1000.0
        );
    }
    Ok(())
}

/// Whether threads that ask for new memory to be made resident or mapped at
/// the same time are served by one call (on by default;
/// `VICTORY_RESIDENT_BATCH=0`: a call or round trip for each).
///
/// (May change while the title runs, `VICTORY_AB=..,VICTORY_RESIDENT_BATCH=0`: each call reads it once and returns
/// when its own memory is resident or mapped by one way or the other; a call that goes by itself while others are
/// served by a batch is a call the device takes from any thread.)
fn batch_residency() -> bool {
    crate::ab::live_value!("VICTORY_RESIDENT_BATCH", bool, crate::ab::unless_zero)
}

/// What a batch of [`resident`] calls holds: a heap or resource of the device's.
struct Pageable(ID3D12Pageable);
// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for Pageable {}

static RESIDENT_GROUP: combine::Combine<Pageable> = combine::Combine::new();
static MAPPED_GROUP: combine::Combine<()> = combine::Combine::new();

/// Batches run (one call or round trip each), counted by the frame line.
static MAPPED_BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
static RESIDENT_BATCHES: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// One round trip through the system's mapping queue: a copy of a few bytes
/// on a queue of ours, waited for. (The body of [`mapped`] as it was.)
fn map_round_trip(device: &ID3D12Device) -> windows::core::Result<()> {
    static ASIDE: Mutex<Option<Aside>> = Mutex::new(None);
    let mut aside = ASIDE.lock().unwrap_or_else(|e| e.into_inner());
    // SAFETY: plain object creation; the objects the list names are kept.
    unsafe {
        if aside.is_none() {
            let buffer = |kind: D3D12_HEAP_TYPE, state: D3D12_RESOURCE_STATES| -> windows::core::Result<ID3D12Resource> {
                let mut made: Option<ID3D12Resource> = None;
                device.CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES { Type: kind, ..Default::default() },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 256,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    state,
                    None,
                    &mut made,
                )?;
                made.ok_or_else(windows::core::Error::empty)
            };
            let from = buffer(D3D12_HEAP_TYPE_UPLOAD, D3D12_RESOURCE_STATE_GENERIC_READ)?;
            let to = buffer(D3D12_HEAP_TYPE_READBACK, D3D12_RESOURCE_STATE_COPY_DEST)?;
            let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_COPY,
                ..Default::default()
            })?;
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)?;
            list.CopyBufferRegion(&to, 0, &from, 0, 16);
            list.Close()?;
            *aside = Some(Aside {
                queue,
                list: list.cast()?,
                fence: device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?,
                event: CreateEventW(None, false, false, None)?,
                value: 0,
                _keep: (allocator, [from, to]),
            });
        }
        let Some(a) = aside.as_mut() else { return Ok(()) };
        a.queue.ExecuteCommandLists(&[Some(a.list.clone())]);
        a.value += 1;
        a.queue.Signal(&a.fence, a.value)?;
        if a.fence.GetCompletedValue() < a.value {
            a.fence.SetEventOnCompletion(a.value, a.event)?;
            WaitForSingleObject(a.event, INFINITE);
        }
    }
    Ok(())
}

/// Times, microseconds and the longest of [`mapped`].
static MAPPED: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// New memory (made with [`not_resident`]) is resident when this returns.
pub(crate) fn resident(device: &ID3D12Device, object: &impl Interface) -> windows::core::Result<()> {
    if !wait_resident() {
        return Ok(());
    }
    let started = std::time::Instant::now();
    let object: ID3D12Pageable = object.cast()?;
    let made = if batch_residency() {
        // One call for everything asked while another runs (`combine`): the
        // system makes a set of objects resident in one pass, where a call
        // for each waits for the ones before it.
        RESIDENT_GROUP.join(Pageable(object), |items| {
            RESIDENT_BATCHES.fetch_add(1, Ordering::Relaxed);
            let all: Vec<Option<ID3D12Pageable>> = items.iter().map(|(_, o)| Some(o.0.clone())).collect();
            // SAFETY: live heaps and resources of the device's.
            if items.len() == 1 || unsafe { device.MakeResident(&all) }.is_err() {
                // Alone, or the set was refused: each says for itself.
                return items
                    .iter()
                    // SAFETY: as above.
                    .filter_map(|(id, o)| unsafe { device.MakeResident(&[Some(o.0.clone())]) }.err().map(|e| (*id, e)))
                    .collect();
            }
            Vec::new()
        })
    } else {
        // SAFETY: a live heap or resource of the device's.
        unsafe { device.MakeResident(&[Some(object)]) }
    };
    let us = started.elapsed().as_micros() as u64;
    RESIDENT[0].fetch_add(1, Ordering::Relaxed);
    RESIDENT[1].fetch_add(us, Ordering::Relaxed);
    RESIDENT[2].fetch_max(us, Ordering::Relaxed);
    static SAID: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = RESIDENT[0].load(Ordering::Relaxed);
    if n >= 1000 * (SAID.load(Ordering::Relaxed) + 1) {
        SAID.store(n / 1000, Ordering::Relaxed);
        tracing::info!(
            "new memory waited for (VICTORY_WAIT_RESIDENT=0: not), since the start: {n} times, {:.1} ms, the longest {:.1} ms",
            RESIDENT[1].load(Ordering::Relaxed) as f64 / 1000.0,
            RESIDENT[2].load(Ordering::Relaxed) as f64 / 1000.0
        );
    }
    made
}

/// Times, microseconds and the longest of [`resident`].
static RESIDENT: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Whether memory the card was given stays with it while there is room (on
/// by default; `VICTORY_KEEP_FREED=0`: it is given back as before). A shader
/// that reads past a view (a raw load through a typed, structured or
/// constant buffer view has no end on AMD: `indirect::tests::raw_loads_past_views`)
/// or through a slot that shows wrong data reads whatever lies there, and
/// nothing happens; where memory was given back it is a page fault, and the
/// device is lost. Both losses of 2026-11-25 were reads in memory given back
/// seconds before (a block of textures; a list's scratch and a window of
/// title memory). Kept are: empty blocks of textures ([`crate::pool`]) below
/// 85 % of the card's budget, windows of title memory and staging buffers
/// for five minutes with 12 GiB of the PC's memory free, and 1 GiB of
/// scratch chunks.
pub(crate) fn keep_freed() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_KEEP_FREED").is_none_or(|v| v != "0"))
}

/// Effective idle timeout (in milliseconds) for system-RAM caches (staging
/// buffers, loose shadows, and the mapped pipeline library), scaled down
/// when free physical RAM is low. Never returns less than 2,000 ms.
/// With plenty of it five minutes ([`keep_freed`]).
pub(crate) fn ram_idle_ms(avail_phys: Option<u64>) -> u64 {
    let effective = match (avail_phys, crate::residency::headroom()) {
        (Some(a), Some(b)) => Some(a.min(b)),
        (a, b) => a.or(b),
    };
    ram_idle_for(effective, keep_freed() && !crate::residency::limited_host_ram())
}

fn ram_idle_for(avail_phys: Option<u64>, keep: bool) -> u64 {
    const GIB: u64 = 1024 * 1024 * 1024;
    const KEPT_MS: u64 = 300_000;
    if keep && avail_phys.is_some_and(|avail| avail >= 12 * GIB) {
        return KEPT_MS;
    }
    const NORMAL_MS: u64 = 10_000;
    const MODERATE_MS: u64 = 5_000;
    const HIGH_MS: u64 = 2_000;
    let Some(avail) = avail_phys else {
        return NORMAL_MS;
    };
    if avail < 2 * GIB {
        HIGH_MS
    } else if avail < 4 * GIB {
        MODERATE_MS
    } else {
        NORMAL_MS
    }
}

/// How many references are held on a host object (its COM count): ours
/// all, since the host's views and commands hold none. A buffer or texture
/// with one is held by its owner alone: no descriptor slot keeps it
/// (`HostHeap::kept`) and no list (`HostList::keep`).
pub(crate) fn references(object: &impl Interface) -> u32 {
    type Method = unsafe extern "system" fn(*mut std::ffi::c_void) -> u32;
    let raw = object.as_raw();
    // SAFETY: a live COM object, whose first three methods are those of
    // `IUnknown` (query, add a reference, release one); the pair leaves the
    // count as it was, and the release returns it. The first is not called.
    unsafe {
        let methods = *raw.cast::<*const [Method; 3]>();
        ((*methods)[1])(raw);
        ((*methods)[2])(raw)
    }
}

/// Once a second, before a submission starts: lets go of what nothing
/// names any more (copies of windows of title memory, staging buffers,
/// host copies of textures the title fills).
/// `VICTORY_SWEEP=0` keeps everything, as before.
fn sweep() {
    // The in-place probe and census, if asked for (`VICTORY_INPLACE_PROBE`).
    crate::inplace::tick();
    static ON: OnceLock<bool> = OnceLock::new();
    if !*ON.get_or_init(|| std::env::var_os("VICTORY_SWEEP").is_none_or(|v| v != "0")) {
        return;
    }
    static LAST: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let now = now_ms();
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 1000
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::book::check();
    crate::residency::refresh();
    if crate::residency::pressured() {
        trim_spare_chunks();
    }
    crate::pool::room(host().and_then(|h| h.video_memory()));
    crate::pipeline::trim_library();
    crate::shadow::let_go_idle();
    crate::texture::let_go_idle();
    crate::resource::let_go_idle();
    crate::resource::compact_pool();
}

/// The number of the newest submission begun, and of the newest of which all
/// before it have finished too.
pub(crate) fn submissions() -> (u64, u64) {
    (STARTED.load(Ordering::Acquire), FINISHED.load(Ordering::Acquire))
}

/// Drops `object` once every submission running now has finished: at once
/// if none is.
pub(crate) fn retire<T: 'static>(object: T) {
    // (Those the title made and the host has not begun get the next numbers.)
    let queued = crate::queue::queued();
    let started = STARTED.load(Ordering::Acquire) + queued;
    if bury_lag() == 0 && started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .push_back((started, Retired::Any(Box::new(object))));
}

/// [`retire`] for a resource.
pub(crate) fn retire_resource(object: ID3D12Resource) {
    let queued = crate::queue::queued();
    let started = STARTED.load(Ordering::Acquire) + queued;
    if bury_lag() == 0 && started <= FINISHED.load(Ordering::Acquire) {
        RETIREMENTS[0].fetch_add(1, Ordering::Relaxed);
        drop(object);
        return;
    }
    RETIREMENTS[1].fetch_add(1, Ordering::Relaxed);
    RETIRED.lock().unwrap_or_else(|e| e.into_inner()).push_back((started, Retired::Resource(object)));
}

/// How many submissions later than it could (the last one that may use it
/// has finished) an object let go is dropped, and a host list that has run
/// is reset, about two frames: the device losses of the night of 2026-11-25
/// were page faults in memory nothing of ours named any more, at no command
/// in particular. Objects let go while nothing runs wait as well: lists
/// recorded and not yet sent may name them. `VICTORY_BURY_LAG=0`: not.
fn bury_lag() -> u64 {
    static LAG: OnceLock<u64> = OnceLock::new();
    *LAG.get_or_init(|| {
        let default = if crate::residency::limited_host_ram() { 16 } else { 64 };
        std::env::var("VICTORY_BURY_LAG").ok().and_then(|v| v.parse().ok()).unwrap_or(default)
    })
}

/// Drops what waited for submissions up to `finished` ([`bury_lag`]). The
/// numbers come in order, but for threads that overtake each other: one of
/// those is dropped a little later than it could be.
fn bury(finished: u64) {
    let finished = finished.saturating_sub(bury_lag());
    let done: Vec<(u64, Retired)> = {
        let mut retired = RETIRED.lock().unwrap_or_else(|e| e.into_inner());
        let n = retired.iter().take_while(|(at, _)| *at <= finished).count();
        if n == 0 {
            return;
        }
        retired.drain(..n).collect()
    };
    drop(done);
}

/// Objects [`retire`] dropped at once (no submission was running) and
/// kept until a running submission finished, since the start.
static RETIREMENTS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// What the layer did to the host's objects lately, newest last (sweeps
/// that let host copies go, pipeline library calls, saves and trims): said
/// once when the host device is removed ([`log_removal_context`]), with
/// what the submitting thread and the pipeline workers were doing at that
/// moment. A device the driver removes "internally" names no command, and
/// the pass that happens to notice first names only the view it was at:
/// the three such losses of runs 72, 73 and 82 each came within 5 ms of a
/// pipeline library call that no line of the log tied to them.
/// `VICTORY_TRAIL=0`: nothing is kept and nothing is said.
static TRAIL: Mutex<std::collections::VecDeque<(u64, u32, String)>> = Mutex::new(std::collections::VecDeque::new());
/// The entries kept.
const TRAIL_MOST: usize = 96;

fn trail_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_TRAIL").is_none_or(|v| v != "0"))
}

/// Microseconds since the first call.
pub(crate) fn now_us() -> u64 {
    static EPOCH: OnceLock<std::time::Instant> = OnceLock::new();
    EPOCH.get_or_init(std::time::Instant::now).elapsed().as_micros() as u64
}

/// The calling thread, as the log's other lines name threads.
pub(crate) fn thread_id() -> u32 {
    // SAFETY: a plain query.
    unsafe { windows::Win32::System::Threading::GetCurrentThreadId() }
}

/// Notes something done to the host's objects ([`TRAIL`]); `what` is only
/// run when the trail is kept.
pub(crate) fn trail(what: impl FnOnce() -> String) {
    if !trail_on() {
        return;
    }
    let entry = (now_us(), thread_id(), what());
    let mut trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    if trail.len() >= TRAIL_MOST {
        trail.pop_front();
    }
    trail.push_back(entry);
}

/// What the submitting thread is doing (0 nothing: between submissions,
/// 1 preparing one, 2 waiting for the card to run it, 3 copying back what
/// it wrote), the lists of that submission, and since when ([`now_us`]).
static SUBMITTING: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

fn submitting(phase: u64, lists: usize) {
    SUBMITTING[1].store(lists as u64, Ordering::Relaxed);
    SUBMITTING[2].store(now_us(), Ordering::Relaxed);
    SUBMITTING[0].store(phase, Ordering::Relaxed);
}

/// What was going on when the host device went: whether a submission was
/// on the card, the pipeline library's state and the calls running in it,
/// and the trail.
pub(crate) fn removal_context() -> String {
    use std::fmt::Write;
    let now = now_us();
    let ms = |at: u64| now.saturating_sub(at) as f64 / 1000.0;
    let phase = match SUBMITTING[0].load(Ordering::Relaxed) {
        0 => "between submissions (nothing of ours was running on the card)",
        1 => "preparing a submission (nothing of ours was running on the card)",
        2 => "waiting for the card to run a submission",
        _ => "copying back what a submission wrote (nothing of ours was running on the card)",
    };
    let mut text = format!(
        "the submitting thread was {phase}, of {} lists, for {:.1} ms; {}; objects let go at once {} and after a running submission {}; submissions started {} finished {}",
        SUBMITTING[1].load(Ordering::Relaxed),
        ms(SUBMITTING[2].load(Ordering::Relaxed)),
        crate::pipeline::library_removal_note(),
        RETIREMENTS[0].load(Ordering::Relaxed),
        RETIREMENTS[1].load(Ordering::Relaxed),
        STARTED.load(Ordering::Relaxed),
        FINISHED.load(Ordering::Relaxed),
    );
    let trail = TRAIL.lock().unwrap_or_else(|e| e.into_inner());
    let _ = write!(text, "; the last {} things done, newest last:", trail.len());
    for (at, thread, what) in trail.iter() {
        let _ = write!(text, "\n  {:.1} ms before, thread {thread}: {what}", ms(*at));
    }
    text
}

/// Says [`removal_context`] in the log, once: called by whoever finds the
/// host device removed.
pub(crate) fn log_removal_context() {
    static SAID: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
    if !(trail_on() || dred_any()) || SAID.swap(true, Ordering::Relaxed) {
        return;
    }
    tracing::error!("when the host device went: {}", removal_context());
    if dred_any() {
        let data = host().map(|h| h.removal_data(&[])).unwrap_or_default();
        tracing::error!("device-removed data{}", if data.is_empty() { "; none" } else { &data });
    }
}

/// Time spent in submissions since the last frame-rate line, in
/// microseconds: preparing them on the CPU (title memory copies, texture
/// detiling), then waiting for the GPU.
static SUBMISSION_US: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// After submissions: shader-written ranges compared and copied back into
/// title memory (microseconds, bytes that differed), since the last
/// frame-rate line.
static COPY_BACK: [std::sync::atomic::AtomicU64; 2] =
    [std::sync::atomic::AtomicU64::new(0), std::sync::atomic::AtomicU64::new(0)];

/// The card's clock, for the frame line of every run, in microseconds since
/// the last one: how long the card had nothing of ours queued; how much of
/// that while the thread that holds the submission lock was working (the
/// card idle behind us: preparing and copying back are done with the card
/// standing); and how long that thread stood waiting for the card.
///
/// Every send of lists to the queue is followed by a signal on a fence of
/// the clock's own ([`Host::card_sent`]). A thread of the clock waits for
/// the newest signal and notes the moment the card reached it with nothing
/// newer sent; the next send ends that idle stretch.
static CARD_CLOCK_US: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Of the thread that holds the submission lock: the microseconds it has
/// worked so far (preparing, copying back; not its waits for the card), and
/// since when it works now ([`now_us`] plus one; 0: it does not).
static WORKING: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The thread that holds the submission lock starts (`on`) or stops
/// working; whether it worked until now.
fn working(on: bool) -> bool {
    let now = now_us() + 1;
    let since = WORKING[1].swap(if on { now } else { 0 }, Ordering::Relaxed);
    if since != 0 {
        WORKING[0].fetch_add(now.saturating_sub(since), Ordering::Relaxed);
    }
    since != 0
}

/// [`WORKING`] as of now: microseconds worked so far.
fn worked() -> u64 {
    let since = WORKING[1].load(Ordering::Relaxed);
    let now = if since != 0 { (now_us() + 1).saturating_sub(since) } else { 0 };
    WORKING[0].load(Ordering::Relaxed) + now
}

/// The thread that holds the submission lock stood waiting for the card
/// from `since` until now ([`CARD_CLOCK_US`]).
fn stood(since: std::time::Instant) {
    CARD_CLOCK_US[2].fetch_add(since.elapsed().as_micros() as u64, Ordering::Relaxed);
}

/// Whether the card has run everything sent to it, by the card's clock (set by its watch thread behind the last
/// signal, cleared at each send): `VICTORY_CARD_AWAKE=idle` keeps the card awake only then.
static CARD_IDLE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

pub(crate) fn card_idle() -> bool {
    CARD_IDLE.load(Ordering::Relaxed)
}

/// What [`CARD_CLOCK_US`] is measured with.
struct CardClock {
    fence: ID3D12Fence,
    /// Signals sent so far; and, while the card has reached the last of
    /// them, since when and what [`worked`] said then.
    state: Mutex<(u64, Option<(std::time::Instant, u64)>)>,
    sent: std::sync::Condvar,
}

// SAFETY: a fence (free-threaded) behind a lock.
unsafe impl Send for CardClock {}
// SAFETY: as above.
unsafe impl Sync for CardClock {}

/// The clock's thread: waits for the newest signal sent and notes when the
/// card got there with nothing newer behind it.
fn card_clock_watch(clock: &'static CardClock) {
    // SAFETY: a plain event of our own.
    let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
        return;
    };
    let mut seen = 0;
    loop {
        let target = {
            let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
            while state.0 == seen {
                state = clock.sent.wait(state).unwrap_or_else(|e| e.into_inner());
            }
            state.0
        };
        // SAFETY: the clock's fence and this thread's event.
        unsafe {
            if clock.fence.SetEventOnCompletion(target, event).is_ok() {
                WaitForSingleObject(event, INFINITE);
            }
        }
        seen = target;
        let mut state = clock.state.lock().unwrap_or_else(|e| e.into_inner());
        if state.0 == target && state.1.is_none() {
            state.1 = Some((std::time::Instant::now(), worked()));
            CARD_IDLE.store(true, Ordering::Relaxed);
        }
    }
}

/// Of the preparing: title memory copies (microseconds, bytes) and texture
/// refreshes (microseconds, bytes hashed), since the last frame-rate line.
pub(crate) static PREPARING: [std::sync::atomic::AtomicU64; 4] = [
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
    std::sync::atomic::AtomicU64::new(0),
];

/// Host buffers made since the start, as (count, bytes): in the upload heap
/// (staging, list scratch), shared with the processor (host copies of title
/// memory) and the graphics card's own (indirect arguments, twins' copies).
/// For the memory line: against what is alive, they show what was let go.
pub(crate) static MADE: [std::sync::atomic::AtomicU64; 6] = [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// What of that is alive now, for the memory line (`MADE` counts what was
/// ever made: 42,607 upload buffers, 77.9 GB, in five minutes of run 60,
/// and nothing said how many were alive): host lists; the lists' scratch
/// chunks and their bytes; of those, the chunks no list holds (kept for the
/// next list, [`Spare`]) and their bytes; chunks taken from those kept
/// instead of made, since the start; and the lists' other upload buffers
/// (texture data copied from buffers, scratch too big for a chunk) and
/// their bytes.
pub(crate) static ALIVE: [std::sync::atomic::AtomicU64; 8] = [const { std::sync::atomic::AtomicU64::new(0) }; 8];

/// The bytes of the two buffers what shaders wrote comes down to (the relay
/// thread's, and the one behind the submission lock), and how often either
/// was made smaller again ([`crate::lean::fetch_trim`]).
pub(crate) static FETCH_BYTES: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];
pub(crate) static FETCH_TRIMMED: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);

/// For the memory line: the buffers kept for the next submission to send up
/// with, their bytes, and the most bytes that are kept.
#[cfg(feature = "diagnostics")]
pub(crate) fn send_spare() -> [u64; 3] {
    send::spare_kept()
}

/// About host lists, for the memory line: lists made by the host since the
/// start; preludes made (a list's second allocator and list, made when it
/// first needs one, [`lazy_prelude`]); and, in diagnostics builds, the
/// bytes the process's private memory grew by while lists were being made
/// (what a list costs: host objects with the driver's command memory; 631
/// lists were alive in run 65, each with a prelude it may never use).
/// Then the lists alive that were never reset since they were made: the
/// host sides of title lists the title made and has not used (a host list
/// cost 0.68 MB of private memory in run 69: 431 MB for the 632 the title
/// makes at its start).
pub(crate) static LISTS: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// The process's private bytes (diagnostics: around the making of lists).
#[cfg(feature = "diagnostics")]
fn private_bytes() -> u64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c = PROCESS_MEMORY_COUNTERS {
        cb: size_of::<PROCESS_MEMORY_COUNTERS>() as u32,
        ..Default::default()
    };
    // SAFETY: our own process and a sized structure.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb) };
    c.PagefileUsage as u64
}

/// What the runtime does when the host GPU fails for good: report it like a
/// crash and end the run. The console has no such state, and carrying on
/// draws nothing at seconds per frame.
static ON_FAILURE: OnceLock<fn(&str) -> !> = OnceLock::new();

/// Sets what a host GPU failure does (the runtime's crash report).
pub fn on_failure(report: fn(&str) -> !) {
    let _ = ON_FAILURE.set(report);
}

/// Ends the run on a host GPU failure: what failed, the removal reason and
/// the debug layer's first error, if any (`VICTORY_D3D12_DEBUG=1` in a
/// diagnostics build names the command).
pub(crate) fn fail(what: std::fmt::Arguments) -> ! {
    let mut text = what.to_string();
    if let Some(host) = host() {
        // SAFETY: a plain query.
        if let Err(e) = unsafe { host.device.GetDeviceRemovedReason() } {
            text.push_str(&format!("; device removed: {e}"));
            log_removal_context();
        }
        match host.log_messages() {
            Some(first) => text.push_str(&format!("; debug layer: {first}")),
            None if host.messages.is_none() => text.push_str(
                "; a diagnostics build with VICTORY_D3D12_DEBUG=1 names the command",
            ),
            None => {}
        }
    }
    tracing::error!("{text}");
    // Tests have no log: the reason goes to their output.
    #[cfg(test)]
    eprintln!("host GPU failure: {text}");
    match ON_FAILURE.get() {
        Some(report) => report(&text),
        None => std::process::abort(),
    }
}

static HOST: OnceLock<Option<Host>> = OnceLock::new();

/// The host GPU if it is made already.
#[cfg(feature = "diagnostics")]
pub(crate) fn made_host() -> Option<&'static Host> {
    HOST.get()?.as_ref()
}

/// The host GPU, created on first use; `None` without one (a run then
/// behaves as before: nothing executes).
pub(crate) fn host() -> Option<&'static Host> {
    HOST.get_or_init(|| match create() {
        Ok(host) => Some(host),
        Err(e) => {
            tracing::warn!(error = %e, "no host D3D12 device; command lists will not run");
            None
        }
    })
    .as_ref()
}

fn create() -> windows::core::Result<Host> {
    // `VICTORY_AB` is read and checked now, so that what is wrong with it is in the log from the start.
    crate::ab::announce();
    // SAFETY: plain D3D12 object creation with valid descriptions.
    unsafe {
        // The debug layer only in builds with diagnostics.
        let debug = cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DEBUG");
        if debug {
            let mut layer: Option<ID3D12Debug> = None;
            match D3D12GetDebugInterface(&mut layer) {
                Ok(()) => layer.iter().for_each(|l| l.EnableDebugLayer()),
                Err(e) => tracing::warn!(error = %e, "no D3D12 debug layer"),
            }
            // GPU-based validation (`VICTORY_D3D12_GBV`): out-of-range
            // descriptor heap indices and the like become messages naming
            // the command instead of page faults. Without state tracking
            // (directly indexed textures are not transitioned).
            if crate::switch::on("VICTORY_D3D12_GBV")
                && let Some(l) = &layer
            {
                if let Ok(d1) = l.cast::<ID3D12Debug1>() {
                    d1.SetEnableGPUBasedValidation(true);
                }
                if let Ok(d2) = l.cast::<ID3D12Debug2>() {
                    d2.SetGPUBasedValidationFlags(
                        D3D12_GPU_BASED_VALIDATION_FLAGS_DISABLE_STATE_TRACKING,
                    );
                }
                tracing::info!("GPU-based validation on");
            }
        }
        // Diagnostics: device-removed data (the command that faulted, the
        // faulting address), read when the device is removed.
        if dred_any() {
            let mut dred: Option<ID3D12DeviceRemovedExtendedDataSettings> = None;
            if D3D12GetDebugInterface(&mut dred).is_ok()
                && let Some(dred) = dred
            {
                if dred_on() || dred_light() {
                    dred.SetAutoBreadcrumbsEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                }
                dred.SetPageFaultEnablement(D3D12_DRED_ENABLEMENT_FORCED_ON);
                tracing::info!(
                    breadcrumbs = dred_on() || dred_light(),
                    markers = dred_on(),
                    "crash tracing on (device-removed data)"
                );
            } else {
                tracing::warn!("crash tracing asked for and not there: no device-removed data settings");
            }
        }
        let mut device: Option<ID3D12Device> = None;
        // Diagnostics: the software adapter, on which a shader's wild access
        // is an access violation in this process (a crash report with the
        // address and instruction) instead of a GPU fault and driver reset.
        if cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_WARP") {
            let adapter = warp_adapter()?;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            tracing::info!("host device on the software adapter (WARP)");
        } else {
            D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
            // Whether a GPU hang ends the run or freezes the PC (#1054).
            crate::tdr::log();
        }
        let device = device.ok_or_else(windows::core::Error::empty)?;
        // Before anything is made with it (`VICTORY_RECORD`).
        #[cfg(feature = "diagnostics")]
        crate::replay::record::from_env(&device);
        // `VICTORY_D3D12_BACKGROUND=0`: the runtime's background work (the
        // driver compiling shaders again on the "D3D Background Thread"s,
        // for minutes after pipelines were made or loaded) is switched off.
        // An experiment: while those threads ran, the same shot took
        // 1,000-1,200 ms a frame, and 830-950 ms from the moment they
        // stopped (run 54, 08:32:01 to 08:32:06). Left as it is by default:
        // whether the card is slower with the shaders it then keeps is not
        // known.
        if std::env::var_os("VICTORY_D3D12_BACKGROUND").is_some_and(|v| v == "0")
            && let Ok(device6) = device.cast::<ID3D12Device6>()
        {
            let set = device6.SetBackgroundProcessingMode(
                D3D12_BACKGROUND_PROCESSING_MODE_DISABLE_BACKGROUND_WORK,
                D3D12_MEASUREMENTS_ACTION_KEEP_ALL,
                None,
                None,
            );
            tracing::info!(ok = set.is_ok(), "the host's background shader work is switched off (VICTORY_D3D12_BACKGROUND=0)");
        }
        // `VICTORY_DRIVER_CACHE=off`: the driver's and the runtime's own shader caches are off for this
        // device, so that a cold start measured here pays every compile and leaves the caches other
        // programs share as they were (`ID3D12Device9::ShaderCacheControl`, which Windows allows in
        // developer mode only; without it the call fails and the run goes on with the caches on, said
        // in the log: `bin\cold-arm.ps1` then moves the cache folders aside instead).
        if std::env::var("VICTORY_DRIVER_CACHE").is_ok_and(|v| v.eq_ignore_ascii_case("off")) {
            let set = match device.cast::<ID3D12Device9>() {
                Ok(device9) => device9
                    .ShaderCacheControl(
                        D3D12_SHADER_CACHE_KIND_FLAG_IMPLICIT_D3D_CONVERSIONS | D3D12_SHADER_CACHE_KIND_FLAG_IMPLICIT_DRIVER_MANAGED,
                        D3D12_SHADER_CACHE_CONTROL_FLAG_DISABLE,
                    )
                    .map_err(|e| e.to_string()),
                Err(e) => Err(format!("no ID3D12Device9: {e}")),
            };
            match set {
                Ok(()) => tracing::info!("the driver's shader caches are off for this process (VICTORY_DRIVER_CACHE=off)"),
                Err(why) => tracing::warn!(why, "VICTORY_DRIVER_CACHE=off did not take (ShaderCacheControl needs Windows developer mode): the caches stay on"),
            }
        }
        // the widest wave the GPU runs: 32 (NVIDIA) gets console wave64 compute programs whose
        // cross-lane code stays in 32-lane halves translated for waves of 32
        let mut o1 = D3D12_FEATURE_DATA_D3D12_OPTIONS1::default();
        if device
            .CheckFeatureSupport(D3D12_FEATURE_D3D12_OPTIONS1, (&raw mut o1).cast(), size_of_val(&o1) as u32)
            .is_ok()
        {
            tracing::info!(min = o1.WaveLaneCountMin, max = o1.WaveLaneCountMax, "host wave lanes");
            victory_shader::emit::set_host_max_wave(o1.WaveLaneCountMax);
            // (RDNA has waves of 32 and 64, the vendor's older cards of 64 alone.)
            let amd = crate::indirect::amd(&device);
            victory_shader::emit::set_host_rdna(amd && o1.WaveLaneCountMin == 32 && o1.WaveLaneCountMax == 64);
            // (NVIDIA's cards run 48 KiB of shared memory per thread group, where D3D12 promises
            // 32: what a console wave split in two host waves gives its halves to exchange
            // goes above the 32 KiB some programs use whole.)
            if crate::indirect::nvidia(&device) {
                victory_shader::emit::set_host_lds_bytes(48 * 1024);
                // (and mesh shaders whose outputs pass the 32 KiB the DXIL validator allows are
                // made and drawn right: `mesh_output_limit_probe`)
                victory_shader::emit::set_host_mesh_output_bytes(
                    crate::indirect::NVIDIA_MESH_OUTPUT_BYTES,
                );
            }
        }
        let messages = debug
            .then(|| device.cast::<ID3D12InfoQueue>().ok())
            .flatten();
        // Streamline is given the device (DLSS, `VICTORY_DLSS`); for frame
        // generation (`VICTORY_FRAMEGEN`) the direct queue is made through
        // its device proxy.
        let queue: ID3D12CommandQueue = match crate::streamline::device_made(&device) {
            Some(queue) => queue,
            None => device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })?,
        };
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let rtvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_RTV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let rtv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_RTV);
        let dsvs: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_DSV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let dsv_size = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_DSV);
        let cpu_views: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: RTV_RING,
                ..Default::default()
            })?;
        let cpu_view_size =
            device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV);
        let adapter = CreateDXGIFactory2::<IDXGIFactory4>(DXGI_CREATE_FACTORY_FLAGS(0))
            .and_then(|factory| factory.EnumAdapterByLuid::<IDXGIAdapter3>(device.GetAdapterLuid()))
            .ok();
        tracing::info!("host D3D12 device ready");
        Ok(Host {
            device,
            queue,
            fence,
            submitted: Mutex::new(0),
            event,
            rtvs,
            rtv_size,
            dsvs,
            dsv_size,
            next_dsv: AtomicU32::new(0),
            cpu_views,
            cpu_view_size,
            next_cpu_view: AtomicU32::new(0),
            messages,
            next_rtv: AtomicU32::new(0),
            presenter: Mutex::new(None),
            adapter,
            fallback_view: OnceLock::new(),
            fallback_sampler: OnceLock::new(),
        })
    }
}

impl Host {
    /// The driver's version, as the system counts it (0: not known).
    pub(crate) fn driver(&self) -> i64 {
        let adapter = self.adapter.as_ref();
        // SAFETY: a plain query.
        adapter.map_or(0, |a| unsafe { a.CheckInterfaceSupport(&IDXGIDevice::IID) }.unwrap_or(0))
    }
}

/// DXGI may return hardware for `EnumWarpAdapter` (notably under Proton).
/// Reject it before creating a device in diagnostics' software-only mode.
pub(crate) fn warp_adapter() -> windows::core::Result<IDXGIAdapter> {
    // SAFETY: plain DXGI adapter queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let adapter: IDXGIAdapter = factory.EnumWarpAdapter()?;
        let desc = adapter.cast::<IDXGIAdapter1>()?.GetDesc1()?;
        if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
            tracing::warn!(
                vendor = format_args!("{:#x}", desc.VendorId),
                flags = format_args!("{:#x}", desc.Flags),
                "DXGI did not identify the WARP adapter as software; refusing device"
            );
            return Err(windows::core::Error::from_hresult(
                windows::Win32::Foundation::E_FAIL,
            ));
        }
        Ok(adapter)
    }
}

/// The batches of clamped records in one scratch buffer
/// ([`HostList::batch_place`]).
const BATCHES: u32 = 8;

/// A host command list and its allocator, always used together.
pub(crate) struct HostList {
    allocator: ID3D12CommandAllocator,
    pub(crate) list: ID3D12GraphicsCommandList,
    /// Of the compute type (`VICTORY_ASYNC_COMPUTE=2`, [`lanes`]): it holds no draw and runs on either queue.
    compute: bool,
    /// Work for submission time, in recorded order.
    deferred: Mutex<Deferred>,
    /// Upload memory for data the list's draws make (expanded vertices),
    /// reused after a reset.
    scratch: Mutex<Scratch>,
    /// Host texture states the list assumes, by host copy: at its start and
    /// at its end. Lists are recorded on many threads and run in submission
    /// order, so the transitions from a texture's real state are made at
    /// submission, in `prelude`. Holding the copies keeps every texture the
    /// list names alive until it is reset, even if the title releases the
    /// resource first.
    states: Mutex<States>,
    /// What the list does with depth textures that have a colour twin, by
    /// twin ([`TwinUse`]).
    twins: Mutex<std::collections::HashMap<usize, TwinUse>>,
    /// The list that runs those transitions just before this one, made
    /// when the list first has such transitions ([`lazy_prelude`]).
    prelude: OnceLock<Option<Prelude>>,
    /// The pipelines the list binds, for reports of slow submissions.
    pipelines: std::sync::Arc<Mutex<std::collections::BTreeSet<String>>>,
    /// What its indirect executions ran, for the report of a removed device.
    #[cfg(feature = "diagnostics")]
    indirects: indirects::Shared,
    /// The work its indirect executions may still do (`indirect`): a
    /// shader-writable word, zeroed when the list is submitted, and after
    /// it what the clamp notes about mesh records. In memory the CPU maps.
    budget: OnceLock<Option<(ID3D12Resource, usize)>>,
    /// The same words in the card's own memory ([`card_budget`]), which the
    /// clamp uses instead when there is one: zeroed in the list before its
    /// first clamp, its notes copied to `budget` when the list closes.
    card_budget: OnceLock<Option<ID3D12Resource>>,
    /// Device-local scratch buffer for clamped indirect arguments (`indirect`),
    /// grown to the largest indirect execution recorded on the list.
    indirect_args: Mutex<Option<(ID3D12Resource, u64)>>,
    /// Scratch buffers of the batched clamps (`crate::indirect::batch`), and
    /// how many batches of this recording have a place in them.
    batch_args: Mutex<(Vec<ID3D12Resource>, u32)>,
    /// Refreshes of host copies that a thread of ours is still finding, and
    /// where among the list's steps they belong ([`aside`]).
    coming: Mutex<Option<(usize, std::sync::Arc<aside::Coming>)>>,
    /// Another number each time something changes that what a draw reads
    /// depends on ([`HostList::changed_something`]); no two lists have the
    /// same.
    epoch: std::sync::atomic::AtomicU64,
    /// The upscaling (`crate::streamline`) recorded in the list, noted for
    /// the present after its submission; 0 for none.
    upscale: std::sync::atomic::AtomicU64,
    /// The copy of the colour without the interface (`crate::streamline`)
    /// recorded in the list; 0 for none.
    hudless: std::sync::atomic::AtomicU64,
    /// Whether this recording has any `Step::Placed` copy
    /// ([`place`]'s `HostList::place_constant`).
    has_placed: std::sync::atomic::AtomicBool,
    /// Counts the list in [`ALIVE`] while it lives.
    _alive: Counted,
    /// The texts of the markers `note_op` set, in order (for device-removed
    /// data without marker texts), with the host GPU range a copy reads.
    #[cfg(feature = "diagnostics")]
    notes: std::sync::Arc<Mutex<Vec<Note>>>,
    /// With `VICTORY_GPU_TIMING`: a GPU timestamp before each noted command
    /// and at the end, and where they are read back.
    #[cfg(feature = "diagnostics")]
    stamps: OnceLock<Option<(ID3D12QueryHeap, ID3D12Resource)>>,
    /// The kind of the title's queue the list was last submitted to (0
    /// graphics, 2 compute; `u32::MAX` before any), for the timing report.
    #[cfg(feature = "diagnostics")]
    queue_kind: AtomicU32,
}

/// A list's prelude: an allocator and a closed list.
type Prelude = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

/// Whether a host list's prelude is made when the list first needs one. A
/// prelude runs the texture transitions and twin copies a list needs before
/// it; lists that name no texture never have one, and every host list was
/// made with one all the same: twice the host objects per list, for the
/// title's 630 lists and for every list made anew.
/// `VICTORY_LAZY_PRELUDE=0`: made with the list, as before.
fn lazy_prelude() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAZY_PRELUDE").is_none_or(|v| v != "0"))
}

/// Timestamps per list (diagnostics); commands past them are not timed.
#[cfg(feature = "diagnostics")]
const STAMPS: u32 = 4096;

/// A marker's text and the host range its command reads (diagnostics).
#[cfg(feature = "diagnostics")]
type Note = (String, Option<(u64, u64)>);

type States = std::collections::HashMap<
    usize,
    (u32, u32, std::sync::Arc<crate::resource::HostTexture>),
>;

/// What a list does with a depth texture that has a colour twin
/// ([`crate::resource::Twin`]). Lists are recorded on many threads and run
/// in submission order, so a list cannot know which side an earlier list
/// leaves behind: it notes which sides it uses as it finds them, and its
/// prelude copies one side into the other at submission if that side is
/// behind then. From its first use of a side on, the list knows.
struct TwinUse {
    /// The depth texture (the title resource's host copy) and its twin.
    depth: std::sync::Arc<crate::resource::HostTexture>,
    twin: std::sync::Arc<crate::resource::Twin>,
    /// The sides (depth texture, twin) the list uses as it finds them.
    needs: [bool; 2],
    /// Whether each side lacks what the other was written last, where the
    /// list is now; `None`: as the list found it.
    behind: [Option<bool>; 2],
    /// The draw or dispatch being prepared writes the twin.
    writing: bool,
    /// The depth plane's twin holds what the plane lacks although the list
    /// copied the twins since: the plane's pass waits for the list's close.
    late: bool,
}

/// The pass that draws the twin of a depth plane into the plane: one
/// triangle over each mip of each slice, its pixel shader writing the
/// twin's texel as the depth, with the depth test "always" and no stencil
/// (the stencil plane is copied through a buffer and not touched here).
///
/// A 16-bit twin holds whole steps of 1/65535, and the depth written is
/// the step times the float nearest 1/65535 (0x37800080). That is how
/// The title's tiled lighting computes the depth of the
/// rectangles it then draws with the test "equal", one class of materials
/// a draw (all six of its vertex shaders multiply the class number by that
/// constant), so the pass and the draw give the host the same float, and
/// the host turns the same float into the same 24 bits. The step divided
/// by 65535 is another float for 512 of the 65,536 steps (none under 257),
/// and lands in another 24-bit step for 127 of them (none under 385).
const PLANE_CORNERS: &str = r#"
float4 corner(uint i : SV_VertexID) : SV_Position {
    return float4(float(i & 1) * 4.0 - 1.0, float(i >> 1) * 4.0 - 1.0, 0.0, 1.0);
}
"#;
const PLANE_STEPS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<uint> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return float(twin.Load(int4(int2(p.xy), 0, 0))) * asfloat(0x37800080);
}
"#;
const PLANE_FLOATS: &str = r#"
#define RS "DescriptorTable(SRV(t0), visibility = SHADER_VISIBILITY_PIXEL)"
Texture2DArray<float> twin : register(t0);
[RootSignature(RS)]
float plane(float4 p : SV_Position) : SV_Depth {
    return twin.Load(int4(int2(p.xy), 0, 0));
}
"#;

/// Views of twins the pass reads, in a heap of ours used as a ring.
/// shortcut: a view must last until its list has run; with one or two
/// passes a frame the ring is far from coming round.
const PLANE_VIEWS: u32 = 256;

/// What the pass draws with: the heap of its views of twins, and per kind
/// of twin (16-bit steps or floats) and depth format the root signature
/// and pipeline, made when first needed (`None`: the host refused).
struct PlanePass {
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: AtomicU32,
    made: Mutex<Vec<(bool, i32, Option<PlanePipeline>)>>,
}

/// The pass's root signature and pipeline for one kind of twin and one
/// depth format.
type PlanePipeline = (ID3D12RootSignature, ID3D12PipelineState);

// SAFETY: D3D12 objects are free-threaded.
unsafe impl Send for PlanePass {}
// SAFETY: as above.
unsafe impl Sync for PlanePass {}

fn plane_pass() -> Option<&'static PlanePass> {
    static PASS: OnceLock<Option<PlanePass>> = OnceLock::new();
    PASS.get_or_init(|| {
        let host = host()?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        // SAFETY: plain heap creation and queries.
        unsafe {
            let views: ID3D12DescriptorHeap = host
                .device
                .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: kind,
                    NumDescriptors: PLANE_VIEWS,
                    Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                    NodeMask: 0,
                })
                .ok()?;
            Some(PlanePass {
                cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
                gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
                step: host.device.GetDescriptorHandleIncrementSize(kind),
                views,
                next: AtomicU32::new(0),
                made: Mutex::default(),
            })
        }
    })
    .as_ref()
}

/// Compiles function `entry` of `source` for `target` with the system's
/// compiler, as the indirect-argument clamp is (`crate::indirect`).
pub(crate) fn compile(
    source: &str,
    entry: windows::core::PCSTR,
    target: windows::core::PCSTR,
) -> windows::core::Result<Vec<u8>> {
    use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
    use windows::Win32::Graphics::Direct3D::ID3DBlob;
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; out pointers ours.
    let compiled = unsafe {
        D3DCompile(
            source.as_ptr().cast(),
            source.len(),
            windows::core::s!("victory"),
            None,
            None,
            entry,
            target,
            0,
            0,
            &mut code,
            Some(&mut errors),
        )
    };
    if let Err(e) = compiled {
        if let Some(errors) = errors {
            // SAFETY: the blob's own bytes.
            let text = unsafe {
                std::slice::from_raw_parts(
                    errors.GetBufferPointer().cast::<u8>(),
                    errors.GetBufferSize(),
                )
            };
            tracing::warn!("a shader of ours: {}", String::from_utf8_lossy(text));
        }
        return Err(e);
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes.
    let bytes = unsafe {
        std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize())
    };
    Ok(bytes.to_vec())
}

impl PlanePass {
    /// The root signature and pipeline for a twin of 16-bit steps (`steps`)
    /// or of floats and the depth format `format`.
    fn pipeline(&self, steps: bool, format: DXGI_FORMAT) -> Option<PlanePipeline> {
        let mut made = self.made.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((_, _, pipeline)) = made.iter().find(|m| (m.0, m.1) == (steps, format.0)) {
            return pipeline.clone();
        }
        let pipeline = match Self::make(steps, format) {
            Ok(pipeline) => Some(pipeline),
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    steps,
                    format = format.0,
                    "no pipeline to draw a depth plane from its twin: such planes stay as they are"
                );
                None
            }
        };
        made.push((steps, format.0, pipeline.clone()));
        pipeline
    }

    fn make(steps: bool, format: DXGI_FORMAT) -> windows::core::Result<PlanePipeline> {
        use windows::core::s;
        let device = &host().ok_or_else(windows::core::Error::empty)?.device;
        let vs = compile(PLANE_CORNERS, s!("corner"), s!("vs_5_1"))?;
        let source = if steps { PLANE_STEPS } else { PLANE_FLOATS };
        let ps = compile(source, s!("plane"), s!("ps_5_1"))?;
        let code = |bytes: &[u8]| D3D12_SHADER_BYTECODE {
            pShaderBytecode: bytes.as_ptr().cast(),
            BytecodeLength: bytes.len(),
        };
        // SAFETY: the root signature is inside the pixel shader; the
        // description points at live data and never releases its pointer.
        unsafe {
            let root: ID3D12RootSignature = device.CreateRootSignature(0, &ps[..])?;
            let desc = D3D12_GRAPHICS_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                VS: code(&vs[..]),
                PS: code(&ps[..]),
                SampleMask: u32::MAX,
                RasterizerState: D3D12_RASTERIZER_DESC {
                    FillMode: D3D12_FILL_MODE_SOLID,
                    CullMode: D3D12_CULL_MODE_NONE,
                    DepthClipEnable: true.into(),
                    ..Default::default()
                },
                // Every texel is written; stencil stays off.
                DepthStencilState: D3D12_DEPTH_STENCIL_DESC {
                    DepthEnable: true.into(),
                    DepthWriteMask: D3D12_DEPTH_WRITE_MASK_ALL,
                    DepthFunc: D3D12_COMPARISON_FUNC_ALWAYS,
                    ..Default::default()
                },
                PrimitiveTopologyType: D3D12_PRIMITIVE_TOPOLOGY_TYPE_TRIANGLE,
                DSVFormat: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                ..Default::default()
            };
            let pipeline: ID3D12PipelineState = device.CreateGraphicsPipelineState(&desc)?;
            Ok((root, pipeline))
        }
    }
}

/// Records into `list` the pass that draws `plane`, the twin of the depth
/// plane of `depth`, into that plane: `depth` in the depth-write state,
/// `plane` readable by pixel shaders. The pass sets the list's descriptor
/// heap, graphics root signature, pipeline, depth target, viewport,
/// scissor and topology and puts none of them back: it is for a list that
/// records nothing of the title's after it.
fn record_plane_pass(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    plane: &ID3D12Resource,
) {
    let (Some(host), Some(pass)) = (host(), plane_pass()) else {
        return;
    };
    // SAFETY: live resources.
    let (of, twin) = unsafe { (depth.GetDesc(), plane.GetDesc()) };
    // An R16 typeless twin (53) holds steps, read as R16_UINT (57); an R32
    // typeless one floats, read as R32_FLOAT (41). The depth view names
    // the depth-stencil format of the texture's family (44: 45, 19: 20).
    let steps = twin.Format.0 == 53;
    let format = DXGI_FORMAT(match of.Format.0 {
        44 => 45,
        19 => 20,
        typed => typed,
    });
    let Some((root, pipeline)) = pass.pipeline(steps, format) else {
        return;
    };
    for slice in 0..u32::from(of.DepthOrArraySize) {
        for mip in 0..u32::from(of.MipLevels) {
            let slot = pass.next.fetch_add(1, Ordering::Relaxed) % PLANE_VIEWS;
            let view = D3D12_SHADER_RESOURCE_VIEW_DESC {
                Format: DXGI_FORMAT(if steps { 57 } else { 41 }),
                ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2DARRAY,
                Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_SRV {
                        MostDetailedMip: mip,
                        MipLevels: 1,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                        ..Default::default()
                    },
                },
            };
            let target = D3D12_DEPTH_STENCIL_VIEW_DESC {
                Format: format,
                ViewDimension: D3D12_DSV_DIMENSION_TEXTURE2DARRAY,
                Anonymous: D3D12_DEPTH_STENCIL_VIEW_DESC_0 {
                    Texture2DArray: D3D12_TEX2D_ARRAY_DSV {
                        MipSlice: mip,
                        FirstArraySlice: slice,
                        ArraySize: 1,
                    },
                },
                ..Default::default()
            };
            let target = host.dsv(depth, Some(&target));
            let (width, height) = ((of.Width >> mip).max(1) as u32, (of.Height >> mip).max(1));
            let cpu = D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: pass.cpu + (slot * pass.step) as usize,
            };
            let gpu = D3D12_GPU_DESCRIPTOR_HANDLE {
                ptr: pass.gpu + u64::from(slot * pass.step),
            };
            // SAFETY: an open list; a slot of our heap; live resources and
            // objects (the pass's own live as long as the process).
            unsafe {
                host.device
                    .CreateShaderResourceView(plane, Some(&view), cpu);
                list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
                list.SetGraphicsRootSignature(&root);
                list.SetPipelineState(&pipeline);
                list.SetGraphicsRootDescriptorTable(0, gpu);
                list.OMSetRenderTargets(0, None, false, Some(&target));
                list.RSSetViewports(&[D3D12_VIEWPORT {
                    TopLeftX: 0.0,
                    TopLeftY: 0.0,
                    Width: width as f32,
                    Height: height as f32,
                    MinDepth: 0.0,
                    MaxDepth: 1.0,
                }]);
                list.RSSetScissorRects(&[RECT {
                    left: 0,
                    top: 0,
                    right: width as i32,
                    bottom: height as i32,
                }]);
                list.IASetPrimitiveTopology(
                    windows::Win32::Graphics::Direct3D::D3D_PRIMITIVE_TOPOLOGY(4),
                );
                list.DrawInstanced(3, 1, 0, 0);
            }
            crate::resource::PLANES[0].fetch_add(1, Ordering::Relaxed);
        }
    }
}

/// Records into `list` the copy of a depth texture into its colour twin
/// (`colour`) or of the twin into the depth texture, both already in their
/// copy states: whole textures of one size in one format family, which the
/// host copies as they are; or, for the twin of a stencil plane, that plane
/// alone, through the twin's buffer.
fn record_twin_copy(
    list: &ID3D12GraphicsCommandList,
    depth: &ID3D12Resource,
    twin: &crate::resource::Twin,
    colour: bool,
) {
    let (from, to) = if colour {
        (depth, &twin.colour.texture)
    } else {
        (&twin.colour.texture, depth)
    };
    crate::resource::TWINS[2 + usize::from(colour)].fetch_add(1, Ordering::Relaxed);
    // shortcut: the depth plane's twin is not given the plane (that is a
    // pass the other way: the plane read by a shader, the twin as its
    // target). The twin then lacks what draws and clears wrote to the
    // plane since shaders last wrote the twin. In the title
    // nothing writes that plane but the twin's pass (its 192 draws against
    // it only test, run 29), so the twin lacks nothing. Counted.
    if colour && twin.plane.is_some() {
        crate::resource::PLANES[1].fetch_add(1, Ordering::Relaxed);
    }
    let Some(through) = &twin.through else {
        // SAFETY: an open list and two live textures.
        unsafe { list.CopyResource(to, from) };
        return;
    };
    // A stencil plane and its twin: each mip and slice of the plane goes
    // into the buffer, laid out as the host lays the plane out for copies,
    // and from the buffer into the other texture, whole subresources both
    // times. In the depth texture the stencil plane's subresources follow
    // the depth plane's; the twin has the one plane.
    let Some(host) = host() else { return };
    // SAFETY: a live texture.
    let of = unsafe { depth.GetDesc() };
    let planes = u32::from(of.MipLevels) * u32::from(of.DepthOrArraySize);
    let mut rows = vec![D3D12_PLACED_SUBRESOURCE_FOOTPRINT::default(); planes as usize];
    // SAFETY: a plain query writing `planes` footprints.
    unsafe {
        host.device.GetCopyableFootprints(
            &of,
            planes,
            planes,
            0,
            Some(rows.as_mut_ptr()),
            None,
            None,
            None,
        );
    }
    let (first_from, first_to) = if colour { (planes, 0) } else { (0, planes) };
    let subresource = |texture: &ID3D12Resource, index: u32| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: a borrowed pointer the location never releases.
        pResource: unsafe { std::mem::transmute_copy(texture) },
        Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            SubresourceIndex: index,
        },
    };
    let placed = |row: &D3D12_PLACED_SUBRESOURCE_FOOTPRINT| D3D12_TEXTURE_COPY_LOCATION {
        // SAFETY: as above.
        pResource: unsafe { std::mem::transmute_copy(through) },
        Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
        Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
            PlacedFootprint: *row,
        },
    };
    let (common, source, dest) = (
        D3D12_RESOURCE_STATE_COMMON,
        D3D12_RESOURCE_STATE_COPY_SOURCE,
        D3D12_RESOURCE_STATE_COPY_DEST,
    );
    // SAFETY: an open list and live resources; the buffer was made as big
    // as these footprints, and rests in the common state between copies.
    unsafe {
        list.ResourceBarrier(&[transition(through, common, dest)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_from + i as u32;
            list.CopyTextureRegion(&placed(row), 0, 0, 0, &subresource(from, index), None);
        }
        list.ResourceBarrier(&[transition(through, dest, source)]);
        for (i, row) in rows.iter().enumerate() {
            let index = first_to + i as u32;
            list.CopyTextureRegion(&subresource(to, index), 0, 0, 0, &placed(row), None);
        }
        list.ResourceBarrier(&[transition(through, source, common)]);
    }
}

/// Whether device-removed data is on (`VICTORY_D3D12_DRED`, diagnostics
/// builds).
pub(crate) fn dred_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| cfg!(feature = "diagnostics") && crate::switch::on("VICTORY_D3D12_DRED") && !dred_faults() && !dred_light())
}

/// Breadcrumbs and the faulting address, but no markers and no texts
/// (`VICTORY_D3D12_DRED=light`): which list a removed device stopped in, at
/// which of its commands, and the pipelines the list binds; formatting a text
/// for every command is what makes the whole of it slow.
pub(crate) fn dred_light() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DRED").is_some_and(|v| v == "light")
    })
}

/// Any of the three.
pub(crate) fn dred_any() -> bool {
    dred_on() || dred_light() || dred_faults()
}

/// Whether only the faulting address of a removed device is kept
/// (`VICTORY_D3D12_DRED=faults`): no breadcrumbs, no markers and no texts, so
/// it costs next to nothing and can stay on while the title is played.
pub(crate) fn dred_faults() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        cfg!(feature = "diagnostics") && std::env::var_os("VICTORY_D3D12_DRED").is_some_and(|v| v == "faults")
    })
}

/// The lists alive while device-removed data is on, by the number their name
/// ends in, with their marker texts and the pipelines they bind: a report made
/// on another thread than the one that submits has no lists of its own.
#[cfg(feature = "diagnostics")]
#[allow(clippy::type_complexity)]
static LIST_NOTES: Mutex<
    std::collections::BTreeMap<
        u32,
        (
            std::sync::Weak<Mutex<Vec<Note>>>,
            std::sync::Weak<Mutex<std::collections::BTreeSet<String>>>,
            std::sync::Weak<Mutex<indirects::Indirects>>,
        ),
    >,
> = Mutex::new(std::collections::BTreeMap::new());

/// The number in the name of a list of ours ("victory host list 7", which the
/// debug layer ends with a note of its own in brackets).
#[cfg(feature = "diagnostics")]
fn list_number(name: &str) -> Option<u32> {
    let rest = name.strip_prefix("victory host list ")?;
    let digits = rest.split(' ').next()?;
    (rest.len() == digits.len() || rest[digits.len()..].starts_with(" (")).then(|| digits.parse().ok()).flatten()
}

/// Indirect mesh dispatches since the last frame line: the largest group
/// count a record asked for, and the groups asked in all (from the lists'
/// budget buffers, a run late).
static MESH_GROUPS: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Indirect records that ran empty for want of their list's budget, and the
/// largest cost of one (told from 2^22 on), likewise.
static BUDGET_NOTES: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// Whether a list's indirect-work budget lives in the card's own memory
/// (`VICTORY_INDIRECT_CARD_BUDGET=0` keeps it in memory the CPU maps, as
/// before). The clamp charges it with atomics, at least two in a row for
/// every clamp; in mapped memory on a card of its own each of them
/// crosses the bus.
fn card_budget() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_CARD_BUDGET").is_none_or(|v| v != "0"))
}

/// Whether each list of a submission runs and is timed on its own, slow
/// ones logged with their commands (`VICTORY_GPU_TIMING`, diagnostics
/// builds; the commands are noted with `VICTORY_D3D12_DRED` on).
#[cfg(feature = "diagnostics")]
fn timing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_GPU_TIMING"))
}

/// Names a host object for the device-removed report, which names the
/// allocations a page fault hit (only with device-removed data on), or for
/// the frame recorder (`VICTORY_RECORD`).
pub(crate) fn name(object: &impl Interface, text: impl FnOnce() -> String) {
    #[cfg(feature = "diagnostics")]
    if crate::book::on()
        && let Ok(resource) = object.cast::<ID3D12Resource>()
    {
        let text = text();
        crate::book::made(&resource, || text.clone());
        return name_it(object, || text);
    }
    name_it(object, text);
}

fn name_it(object: &impl Interface, text: impl FnOnce() -> String) {
    #[cfg(feature = "diagnostics")]
    if crate::replay::record::on() {
        let text = text();
        crate::replay::record::name(object, &text);
        if dred_any()
            && let Ok(object) = object.cast::<ID3D12Object>()
        {
            // SAFETY: a plain call on a live object.
            let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text)) };
        }
        return;
    }
    if dred_any()
        && let Ok(object) = object.cast::<ID3D12Object>()
    {
        // SAFETY: a plain call on a live object.
        let _ = unsafe { object.SetName(&windows::core::HSTRING::from(text())) };
    }
}

/// One in [`ALIVE`]`[0]` while it lives (a host list), and one in
/// [`LISTS`]`[3]` until it is first reset.
struct Counted {
    /// Not reset since it was made.
    fresh: std::sync::atomic::AtomicBool,
}

impl Counted {
    fn new() -> Counted {
        ALIVE[0].fetch_add(1, Ordering::Relaxed);
        LISTS[3].fetch_add(1, Ordering::Relaxed);
        Counted {
            fresh: std::sync::atomic::AtomicBool::new(true),
        }
    }

    /// The list is reset: it is in use.
    fn used(&self) {
        if self.fresh.swap(false, Ordering::Relaxed) {
            LISTS[3].fetch_sub(1, Ordering::Relaxed);
        }
    }
}

impl Drop for Counted {
    fn drop(&mut self) {
        ALIVE[0].fetch_sub(1, Ordering::Relaxed);
        self.used();
    }
}

/// A chunk of upload memory for lists' scratch data: the buffer, its
/// mapping.
struct Chunk {
    buffer: ID3D12Resource,
    data: usize,
}

// SAFETY: a D3D12 resource (free-threaded) and its persistent mapping,
// written only by the list that holds the chunk.
unsafe impl Send for Chunk {}

/// Upload chunks handed out front to back: the chunks (each with its
/// bytes), the one in use and the bytes used in it.
#[derive(Default)]
struct Scratch {
    chunks: Vec<(Chunk, u64)>,
    current: usize,
    used: u64,
}

// SAFETY: the chunk pointers are persistent mappings of upload buffers.
unsafe impl Send for Scratch {}

impl Drop for Scratch {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.chunks));
    }
}

const SCRATCH_CHUNK: u64 = 4 << 20;
/// A list's first chunk when the chunks are shared ([`scratch_pool`]): most
/// lists put a few constants or a rectangle's corners there, and every
/// list that did took 4 MiB of the PC's memory for it.
const SCRATCH_FIRST: u64 = 256 << 10;
/// The most bytes of chunks kept while no list holds them.
const SPARE_MOST: u64 = 128 << 20;
/// The same with [`keep_freed`].
const SPARE_KEPT: u64 = 1 << 30;

/// Whether lists share their scratch chunks: a list takes a chunk when it
/// first needs one, a small one first, and gives all of them back when it
/// is reset or dropped, for the next list. Before, a list kept every chunk
/// it ever took (4 MiB each) for as long as it lived, used or not, and the
/// host lists of held submissions, which live for one submission, each
/// made theirs anew: some 57 upload buffers of 4 MiB made, zeroed and
/// freed per frame of gameplay (run 60).
/// `VICTORY_SCRATCH_POOL=0`: chunks of 4 MiB kept by their list, as before.
fn scratch_pool() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_SCRATCH_POOL").is_none_or(|v| v != "0"))
}

/// Things of some sizes that nothing holds now, kept for whoever next asks
/// for one of the same size.
struct Spare<T> {
    kept: Vec<(T, u64)>,
    bytes: u64,
}

impl<T> Spare<T> {
    const fn new() -> Self {
        Spare { kept: Vec::new(), bytes: 0 }
    }

    /// A kept thing of `size` bytes, the one given last.
    fn take(&mut self, size: u64) -> Option<T> {
        let at = self.kept.iter().rposition(|(_, s)| *s == size)?;
        self.bytes -= size;
        Some(self.kept.remove(at).0)
    }

    /// Keeps `thing` (`size` bytes) unless that takes the kept bytes past
    /// `most`: then it comes back.
    fn give(&mut self, thing: T, size: u64, most: u64) -> Option<T> {
        if self.bytes + size > most {
            return Some(thing);
        }
        self.bytes += size;
        self.kept.push((thing, size));
        None
    }
}

/// Scratch chunks no list holds.
static SPARE: Mutex<Spare<Chunk>> = Mutex::new(Spare::new());

/// A scratch chunk of `size` bytes: one kept ([`SPARE`]) or a new one.
fn take_chunk(host: &Host, size: u64) -> Option<Chunk> {
    if scratch_pool()
        && let Some(chunk) = SPARE.lock().unwrap_or_else(|e| e.into_inner()).take(size)
    {
        ALIVE[3].fetch_sub(1, Ordering::Relaxed);
        ALIVE[4].fetch_sub(size, Ordering::Relaxed);
        ALIVE[5].fetch_add(1, Ordering::Relaxed);
        return Some(chunk);
    }
    let (buffer, data) = host.upload_buffer(size)?;
    name(&buffer, || "list scratch".into());
    ALIVE[1].fetch_add(1, Ordering::Relaxed);
    ALIVE[2].fetch_add(size, Ordering::Relaxed);
    Some(Chunk {
        buffer,
        data: data as usize,
    })
}

/// A list is done with `chunks` (it was reset after its submissions ran,
/// or dropped): kept for the next list, or freed past [`SPARE_MOST`] and
/// without the pool.
fn give_chunks(chunks: Vec<(Chunk, u64)>) {
    if chunks.is_empty() {
        return;
    }
    let mut freed = Vec::new();
    {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        for (chunk, size) in chunks {
            let back = if scratch_pool() {
                let limit = if keep_freed() && !crate::residency::limited_host_ram() {
                    SPARE_KEPT
                } else {
                    SPARE_MOST
                };
                // (`VICTORY_SCRATCH_SPARE_MB`, `VICTORY_RAM_LEAN`: less.)
                let limit = crate::lean::scratch_spare_mb(limit >> 20) << 20;
                spare.give(chunk, size, limit)
            } else {
                Some(chunk)
            };
            match back {
                None => {
                    ALIVE[3].fetch_add(1, Ordering::Relaxed);
                    ALIVE[4].fetch_add(size, Ordering::Relaxed);
                }
                Some(chunk) => {
                    ALIVE[1].fetch_sub(1, Ordering::Relaxed);
                    ALIVE[2].fetch_sub(size, Ordering::Relaxed);
                    freed.push(chunk);
                }
            }
        }
    }
    // Freed outside the lock.
    drop(freed);
}

/// Drops spare scratch chunks kept idle when host memory is pressured.
fn trim_spare_chunks() {
    let freed: Vec<(Chunk, u64)> = {
        let mut spare = SPARE.lock().unwrap_or_else(|e| e.into_inner());
        if spare.kept.is_empty() {
            return;
        }
        spare.bytes = 0;
        std::mem::take(&mut spare.kept)
    };
    let count = freed.len() as u64;
    let bytes: u64 = freed.iter().map(|(_, s)| *s).sum();
    ALIVE[1].fetch_sub(count, Ordering::Relaxed);
    ALIVE[2].fetch_sub(bytes, Ordering::Relaxed);
    ALIVE[3].fetch_sub(count, Ordering::Relaxed);
    ALIVE[4].fetch_sub(bytes, Ordering::Relaxed);
    drop(freed);
}

/// What the host keeps for copies in the card's own memory
/// ([`crate::shadow::cards`]): three lists of ours and the compare.
struct CardWork {
    /// Before a submission's lists: what changed on the copies' CPU side
    /// goes to their card side. Those not in use: a plan takes one and
    /// gives it back when its submission has run (`Host::card_done`), so
    /// that submissions sent ahead ([`late`]) each have their own.
    sync: Vec<Prelude>,
    /// After them, in the same submission: the compare of the declared
    /// ranges with their bytes from before. As `sync`.
    check: Vec<Prelude>,
    /// `sync` and `check` of the compute type, for submissions on the compute queue ([`lanes`]).
    sync_c: Vec<Prelude>,
    check_c: Vec<Prelude>,
    /// After it ran: the pieces that differ come down.
    fetch: Prelude,
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    /// One dword per piece compared, odd where the piece differs: PC
    /// memory, mapped (buffer, mapping, bytes).
    differs: (ID3D12Resource, usize, u64),
    /// Where the pieces that differ are fetched to: PC memory, mapped.
    fetched: Option<(ID3D12Resource, usize, u64)>,
    /// When `fetched` is made smaller again ([`crate::lean::fetch_trim`]).
    fetched_trim: crate::lean::Trim,
    /// The dwords of `differs` taken by plans whose results were not read
    /// yet.
    taken: u32,
    /// Plans made and not done with (`Host::card_done`).
    out: u32,
}

// SAFETY: D3D12 objects (free-threaded) and persistent mappings, used only
// under the submission lock.
unsafe impl Send for CardWork {}

static CARD_WORK: Mutex<Option<CardWork>> = Mutex::new(None);

/// The compare of a declared range of a copy in the card's memory with
/// its bytes from before, one group per 64 KiB piece (cut at multiples of
/// 64 KiB of title memory, as `Shadow::card_piece` cuts them): `differs`
/// gets a dword per piece, odd where a byte differs. The offsets are widened
/// to 16 bytes; buffers are multiples of 64 KiB, and a piece flagged for a
/// byte just outside the range costs a fetch that finds nothing.
///
/// A piece's 64 threads walk it side by side, each every 64th 16 bytes, and
/// one of them hashes their 64 hashes: 64 steps after one another. One thread
/// walked the piece before, 4,096 steps; the passes took 24.8 ms of a frame's
/// 82 on the card (capture replay1, 405 of them), most of it waiting for
/// those steps.
pub(crate) const CARD_COMPARE: &str = r#"
#define RS "RootConstants(num32BitConstants=6, b0), SRV(t0), UAV(u0), UAV(u1)"
cbuffer Span : register(b0) {
    uint first;    // the first piece
    uint skew;     // where the copy starts in its piece 0
    uint size;     // the copy's buffer, in bytes (a multiple of 64 KiB)
    uint pieces;
    uint out_at;   // the first piece's dword in `differs`, in bytes
    uint compare;  // 0: the hashes are stored; else compared with those stored (2: and then stored)
};
ByteAddressBuffer now : register(t0);
// 16 bytes per piece of the copy: the hash of its bytes when they were
// last sent up (or hashed whole, the first time).
RWByteAddressBuffer hashes : register(u0);
RWByteAddressBuffer differs : register(u1);
groupshared uint4 parts[64];
[RootSignature(RS)]
[numthreads(64, 1, 1)]
void main(uint3 id : SV_GroupID, uint thread : SV_GroupIndex) {
    uint piece = first + id.x;
    uint lo = piece * 65536u;
    lo = lo > skew ? lo - skew : 0u;
    uint hi = min((piece + 1u) * 65536u - skew, size);
    lo = lo & ~15u;
    hi = (hi + 15u) & ~15u;
    uint4 h = uint4(0x9E3779B9u, 0x85EBCA6Bu, 0xC2B2AE35u, 0x27D4EB2Fu) + thread;
    uint at = lo + thread * 16u;
    // Eight loads at once: one after the other, each waits for memory.
    [loop]
    for (; at + 7168u < hi; at += 8192u) {
        uint4 v0 = now.Load4(at + 0u);
        uint4 v1 = now.Load4(at + 1024u);
        uint4 v2 = now.Load4(at + 2048u);
        uint4 v3 = now.Load4(at + 3072u);
        uint4 v4 = now.Load4(at + 4096u);
        uint4 v5 = now.Load4(at + 5120u);
        uint4 v6 = now.Load4(at + 6144u);
        uint4 v7 = now.Load4(at + 7168u);
        h = (h ^ v0) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v1) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v2) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v3) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v4) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v5) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v6) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
        h = (h ^ v7) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    [loop]
    for (; at < hi; at += 1024u) {
        h = (h ^ now.Load4(at)) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    parts[thread] = h;
    GroupMemoryBarrierWithGroupSync();
    if (thread != 0u) return;
    h = parts[0];
    [loop]
    for (uint i = 1u; i < 64u; ++i) {
        h = (h ^ parts[i]) * uint4(0x01000193u, 0x9E3779B1u, 0x85EBCA77u, 0xC2B2AE3Du);
        h = h.yzwx ^ (h >> 15);
    }
    if (compare != 0u) {
        // The flag, and above it 31 bits of the hash (`torn`).
        uint folded = h.x ^ (h.y * 3u) ^ (h.z * 5u) ^ (h.w * 7u);
        differs.Store(out_at + id.x * 4u, (folded << 1u) | (any(h != hashes.Load4(piece * 16u)) ? 1u : 0u));
        // With the copy-back on its own thread: the next compare of the
        // piece finds what is written from here on.
        if (compare == 2u) hashes.Store4(piece * 16u, h);
    } else {
        hashes.Store4(piece * 16u, h);
    }
}
"#;

/// The hash [`CARD_COMPARE`] gives of a piece with its flag.
#[cfg(test)]
pub(crate) fn piece_hash(bytes: &[u8]) -> u32 {
    torn::hash(bytes)
}

/// One pass of [`CARD_COMPARE`] over a span's pieces, into `differs` from
/// dword `at` on (`mode` as the shader's `compare`): a group per piece, in
/// parts a dispatch can hold.
///
/// # Safety
/// The list is open with the compare's root signature, pipeline and buffers
/// set, and the pieces lie in the buffer.
unsafe fn card_pass(list: &ID3D12GraphicsCommandList, span: &crate::shadow::CardSpan, size: u32, at: u32, mode: u32) {
    const MOST: u32 = D3D12_CS_DISPATCH_MAX_THREAD_GROUPS_PER_DIMENSION;
    let mut done = 0;
    while done < span.pieces {
        let n = (span.pieces - done).min(MOST);
        let constants = [span.first + done, span.skew, size, n, (at + done) * 4, mode];
        // SAFETY: the caller's.
        unsafe {
            list.SetComputeRoot32BitConstants(0, 6, constants.as_ptr().cast(), 0);
            list.Dispatch(n, 1, 1);
        }
        done += n;
    }
}

/// Whether the pieces hashed again after ranges were sent up to a copy in
/// the card's memory are hashed in runs of pieces that touch, each piece
/// once (`VICTORY_CARD_REHASH_RUNS`; on by default, `=0` turns it off), and not in a pass
/// per range sent up.
///
/// These passes are ours, they run on the card before every submission's
/// lists: a pass takes the time of one piece however few its pieces are.
/// Run 102 sent up 514 ranges a frame: 514 such passes. The same pieces are hashed either way;
/// a piece hashed when it need not be, or not hashed when it should, costs
/// a fetch that finds nothing and loses no shader write.
fn rehash_runs() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::defaults::var_os("VICTORY_CARD_REHASH_RUNS").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line, of our own passes over copies in the
/// card's memory: hash passes after ranges were sent up, the pieces they
/// hashed, and compare passes after the lists.
static CARD_PASSES: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Since the last frame-rate line, of the list that brings copies in the card's memory up to date (one a submission
/// that has changed ranges): nanoseconds resetting its allocator and list, recording it, closing it and giving it to
/// the queue, and how many there were. The per-submission cost that the number of submissions multiplies.
#[cfg(feature = "diagnostics")]
static SYNC_LIST: [std::sync::atomic::AtomicU64; 5] = [const { std::sync::atomic::AtomicU64::new(0) }; 5];

/// Adds the time since `lap` to part `i` of [`SYNC_LIST`] and begins the next lap.
#[cfg(feature = "diagnostics")]
fn sync_lap(i: usize, lap: &mut std::time::Instant) {
    let now = std::time::Instant::now();
    SYNC_LIST[i].fetch_add((now - *lap).as_nanos() as u64, Ordering::Relaxed);
    *lap = now;
}

/// Whether what shaders wrote to copies in the card's memory is merged
/// into title memory on several threads (`VICTORY_WIDE_MERGE=1`; off by
/// default). The pieces the card's compare found differing come down in
/// one fetch and were then merged one after the other on the thread that
/// settles: about 630 pieces and 40 MB a frame (run 104), each compared
/// with the copy's CPU side byte by byte and put into title memory through
/// the kernel. A piece is its own bytes of its copy and of title memory,
/// so eight threads take an eighth of them each. (Not with [`relay`], whose
/// landings do not come here: `crate::land` has threads of its own.)
fn wide_merge() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_MERGE").is_some_and(|v| v == "1"))
}

/// One declared range of a copy in the card's memory, in a submission.
struct CardRange {
    shadow: std::sync::Arc<crate::shadow::Shadow>,
    /// As declared (title addresses), for the statistics.
    declared: (usize, usize),
    span: crate::shadow::CardSpan,
    /// Its first piece's dword in the compare's results.
    at: u32,
    /// Whether the card compares it; else every piece is fetched.
    compared: bool,
    /// Looked at by `crate::picked`, and not declared: what is found changed
    /// there was written through a view nobody declared. Not landed.
    picked: bool,
}

/// What a submission does for copies in the card's memory.
struct CardPlan {
    ranges: Vec<CardRange>,
    /// The compare, run after the submission's lists.
    check: Option<ID3D12CommandList>,
    /// The copies whose card side was brought up to date, alive until the
    /// submission has run.
    _synced: Vec<std::sync::Arc<crate::shadow::Shadow>>,
    /// The lists of ours the plan recorded (0 the list that sends, 1 the
    /// compare, 2 and 3 the same of the compute type), given back by `Host::card_done`.
    lists: Vec<(u8, Prelude)>,
    /// With [`relay`]: the number of the send to copies in the card's memory
    /// that the submission ran behind ([`QUEUE_ORDER`]).
    sent_as: u64,
    /// With [`upcheck`]: the look at what it sent up.
    looks: Option<upcheck::Looks>,
    /// With [`send`]: the memory its sends come from.
    sends: Option<send::Memory>,
    /// With `VICTORY_CARD_SPANS=1`: the slot of stamps around its passes.
    stamps: Option<usize>,
}

/// One piece of submission-time work, in recorded order.
enum Step {
    /// A copy into title memory (destination, source, bytes): the title's
    /// buffer copies, which later refreshes must see.
    Copy(usize, usize, usize),
    /// A copy of title memory into the list's own upload memory, and how
    /// the card could bring the bytes instead.
    Fill(usize, usize, usize, Option<card_steps::FillTo>),
    /// Brings a range (address, bytes) of a buffer's host copy up to date:
    /// once per submission, whichever lists ask, until a `Copy` or `Run`
    /// intervenes.
    Refresh(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Work that may write title memory.
    Run(Box<dyn Fn() + Send>),
    /// Work that writes host memory only, and the title memory it reads
    /// (from, to; empty for none), and what the work is ([`relax::Kind`]).
    Host(Box<dyn Fn() + Send>, (usize, usize), relax::Kind),
    /// Shaders write a range (address, bytes) of a buffer's host copy.
    GpuWrite(std::sync::Arc<crate::shadow::Shadow>, usize, usize),
    /// Brings a texture's staging buffer up to date: once per submission,
    /// whichever lists sample it, until a `Copy` or `Run` intervenes.
    Texture(std::sync::Arc<crate::texture::Staging>),
    /// Reads a CPU-written linear movie plane into a per-list upload buffer.
    Plane((usize, usize), Box<dyn Fn() + Send>),
    /// A title buffer copy that the list does on the card ([`place`]): what
    /// is done on the CPU for it.
    Placed(place::Placed),
}

/// What a list does on the CPU when it is submitted: copies between host
/// memory regions (title buffers, upload buffers and buffer copies), and
/// the upload buffers its GPU commands read, kept until the list is reset.
#[derive(Default)]
struct Deferred {
    /// Destination, source, bytes.
    copies: Vec<Step>,
    uploads: Vec<ID3D12Resource>,
    /// Standalone upload footprints kept through this list's fence (movie planes).
    plane_uploads: Vec<(Chunk, u64)>,
    /// The bytes of `uploads` (counted in [`ALIVE`] while they are held).
    upload_bytes: u64,
    /// Host objects the list's commands name (pipelines, root signatures,
    /// descriptor heaps): the title may release its objects before it
    /// submits the list.
    kept: Vec<windows::core::IUnknown>,
    /// The pipelines [`HostList::bind_pipeline`] kept, by address.
    bound: std::collections::HashSet<usize, std::hash::BuildHasherDefault<AddressHash>>,
    /// Buffer ranges refreshed since the last title copy (host copy,
    /// address, bytes): refreshing them again would find the same bytes.
    refreshed: std::collections::HashSet<(usize, usize, usize), std::hash::BuildHasherDefault<AddressHash>>,
    /// Ranges declared written by shaders since the list last wrote title
    /// memory (host copy, address, bytes): declaring them again would
    /// change nothing.
    gpu_written: std::collections::HashSet<(usize, usize, usize), std::hash::BuildHasherDefault<AddressHash>>,
    /// What those ranges would be by the code of the shaders ([`precise`], `VICTORY_PRECISE_LOOK`).
    precise: precise::Side,
    /// Scratch memory [`HostList::fill_scratch`] fills since the list last
    /// wrote title memory (source, bytes): its host address.
    filled: std::collections::HashMap<(usize, usize), u64, std::hash::BuildHasherDefault<AddressHash>>,
    /// Textures whose contents the list brings up to date.
    textures: std::collections::HashSet<usize>,
    /// What `copies` are to the submission's preparing ([`onepass`]), made when the list was closed (`VICTORY_ONE_PASS`):
    /// of the number of steps it has, or it is made again.
    summary: Option<std::sync::Arc<onepass::Summary>>,
    /// Bottom-of-pipe writes, done once the list has run: address, what,
    /// bytes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Textures read back (texture to buffer copies), done once the list
    /// has run: the host buffer the list copies the texels into, the title
    /// memory they go to, the bytes of a row, the rows, the title's row
    /// pitch.
    backs: Vec<(ID3D12Resource, usize, [usize; 3])>,
    /// The occlusion queries the host list has begun and not ended, and their
    /// heaps ([`crate::occlusion`]).
    queries: crate::occlusion::Open<ID3D12QueryHeap>,
    /// Results of occlusion queries among `backs`: the title memory they go
    /// to and how many 8-byte results (for the totals line).
    occluded: Vec<(usize, usize)>,
    /// Title textures of the kinds copied back ([`crate::texback`]) that
    /// the list's shaders write, copied when the list closes.
    back_wanted: Vec<crate::texback::Target>,
    /// Those copies, whose texels go to title memory once the list has run.
    texture_backs: Vec<crate::texback::Back>,
    /// Whether the indirect-work budget is zeroed for this recording (a
    /// step on the CPU, or in the list for the card's copy).
    budget_zeroed: bool,
    /// The indirect scratch buffer (`HostList::indirect_args`, by its
    /// interface pointer) that a clamp in this recording left readable as
    /// indirect arguments; 0 if none. It stays readable until the next
    /// clamp into it ([`HostList::indirect_state`]).
    indirect_readable: usize,
    /// Ranges `[dst, dst + len)` written on the GPU by `Step::Placed` in
    /// this recording, in order, with the destination host copy.
    placed_written: Vec<(usize, usize, std::sync::Arc<crate::shadow::Shadow>)>,
    /// Current per-recording GPU buffer `(buffer, capacity, used)` for
    /// inline placed constant snapshots ([`place`]).
    placed_cbv: Option<(ID3D12Resource, u64, u64)>,
}

impl Deferred {
    /// Holds `buffer` (`size` bytes of upload memory) until the list is
    /// reset.
    fn hold_upload(&mut self, buffer: ID3D12Resource, size: u64) {
        ALIVE[6].fetch_add(1, Ordering::Relaxed);
        ALIVE[7].fetch_add(size, Ordering::Relaxed);
        self.upload_bytes += size;
        self.uploads.push(buffer);
    }

    /// The steps recorded last write title memory: what was brought up to
    /// date before them is not any more.
    fn title_written(&mut self) {
        self.refreshed.clear();
        self.gpu_written.clear();
        self.filled.clear();
    }
}

impl Drop for Deferred {
    fn drop(&mut self) {
        give_chunks(std::mem::take(&mut self.plane_uploads));
        ALIVE[6].fetch_sub(self.uploads.len() as u64, Ordering::Relaxed);
        ALIVE[7].fetch_sub(self.upload_bytes, Ordering::Relaxed);
    }
}

/// Whether a queue's submissions go to the card without a wait for each
/// (stage 3 by default, with the relay; `VICTORY_LATE_WAIT=0` turns it off, and it
/// is off in the timing run, which waits for every list on its own).
///
/// Every submission was prepared, sent, waited for and copied back before
/// the next one was looked at, so the card stood idle while we prepared
/// and we stood idle while it ran: in run 86 a frame of 446 ms is 134 ms of
/// preparing, 194 ms of waiting and 78 ms of copying back, one after the
/// other, on the one thread that submits.
///
/// With the switch a queue's submission is sent and joins a line of
/// "flights" ([`Flight`]); the thread goes on to the next one while the
/// card runs. The flights are settled together ([`Host::settle`]: one wait
/// for the card, what their shaders wrote copied back once per byte, then
/// their bottom-of-pipe writes and the fence signals behind them, in
/// order) at the next point where the title, or our own preparing, can
/// observe what they do. Those points, exactly:
///
/// 1. a synchronous submission of ours ([`Host::execute`]): a present, a
///    read-back or map of a texture, a clear or upload run at once;
/// 2. the next submission reads title memory that a flight's shaders may
///    write: a title buffer copy, a fill of upload memory, a texture's
///    memory, any step that runs code of ours on title memory
///    ([`late_blocked`]);
/// 3. the next submission refreshes a range of a host copy that a flight's
///    shaders may write, and the title wrote a page of it since it was last
///    copied ([`stale_under_flights`]): the refresh leaves such a range
///    alone until the flight's writes are back, as it always did within a
///    submission;
/// 4. a queue stands at a wait for a memory value that no flight's
///    bottom-of-pipe write is known to satisfy, at an address a flight may
///    write ([`late_met`]);
/// 5. the title blocks on a fence without an event;
/// 6. the card has run every flight (a thread of ours waits for that:
///    [`late_helper`]), or the flights hold [`late_most`] host lists.
///
/// What the title is told (bottom-of-pipe writes, fence values) is told
/// when the card has really run the work, later than before and never
/// earlier; a fence signalled on a queue while flights are in the air is
/// set behind them. A wait for a memory value that a flight's
/// bottom-of-pipe write satisfies counts as met: the waiting part goes to
/// the card behind that flight, which is the order the wait asks for.
///
/// What it assumes beyond today: memory the title rewrites between two of
/// its submissions, without waiting for the first, may be seen by the
/// first's shaders (as on the console, where the card reads title memory
/// when it runs, not when the list is submitted).
pub(crate) fn late() -> bool {
    late_stage() > 0
}

/// `VICTORY_LATE_WAIT`: 0 (off), 1, or 2 ([`late_exact`]).
fn late_stage() -> u8 {
    static STAGE: OnceLock<u8> = OnceLock::new();
    *STAGE.get_or_init(|| {
        if crate::switch::on("VICTORY_GPU_TIMING") {
            return 0;
        }
        match std::env::var("VICTORY_LATE_WAIT").as_deref() {
            Ok("1") => 1,
            Ok("2") => 2,
            Ok("3") => 3,
            // The relay alone: on top of stage 3.
            Err(_) if crate::shadow::relay() => 3,
            _ => 0,
        }
    })
}

/// Stage 2 of [`late`] (`VICTORY_LATE_WAIT=2`): point 3 of its list is no
/// longer a point. Where the title wrote a page that lies in a range a
/// flight's shaders may write, the bytes the title changed are found (the
/// page as it is now against what the copy held when the flight was
/// prepared) and put into the copy, and only those: what the flight's
/// shaders write beside them stays (`Shadow::merge_under_writes`). The
/// flights are settled for it only where that cannot be done: memory that
/// is not write-watched, or a copy in the card's memory with more changed
/// runs than a submission should send up one by one.
///
/// In run 92 (stage 1) this point settled the flights five times a frame
/// of twelve, each time with the card's work still to wait for: 172 ms a
/// frame of waiting were left of 200.
///
/// What differs from settling first: a byte both the title and a flight's
/// shader wrote is the title's if the shader wrote it first, the shader's
/// if it wrote it afterwards; settled first it was always the shader's.
/// The title writing a byte a shader of its own is writing is a race on
/// the console too.
fn late_exact() -> bool {
    late_stage() > 1
}

/// Stage 3 of [`late`] (`VICTORY_LATE_WAIT=3`: stage 2, and the present is
/// no longer a point where flights are settled). The copy to the back
/// buffer is sent as a flight of its own (`Host::execute_own`, a list of
/// its own for each present in the air) and the swap chain is told to
/// present behind it; the queue's call returns, and the card finishes the
/// frame while the title records the next one. What the title is told
/// about the frame (bottom-of-pipe writes, fences) is told when the card
/// has run it, by the settling thread.
///
/// What is left out with it: the look whether the presented frame is black
/// (diagnostics; it reads the frame back at once). A frame that is saved
/// or listed is presented the old way. Once every second and a half a
/// submission settles the flights all the same, for the sweep that lets
/// idle host objects go (it needs nothing in the air).
pub(crate) fn late_free_present() -> bool {
    late_stage() > 2
}

/// Of the line's things in order (a flight: the value signalled on the
/// host's fence behind it; a write or a signal behind a flight: `None`):
/// how many, from the front, can be landed when the card has reached
/// `completed`: everything ahead of the first flight it has not run. A
/// later flight never lands before an earlier one, and what follows a
/// flight (its bottom-of-pipe writes, signals) never before the flight.
fn landable(line: &[Option<u64>], completed: u64) -> usize {
    line.iter().position(|f| f.is_some_and(|f| f > completed)).unwrap_or(line.len())
}

/// The most host lists in the air before the flights are settled
/// (`VICTORY_LATE_WAIT_LISTS`, 256; `=128` is the value before: with the
/// relay it stopped the submitting thread twice a frame for 92-103 ms in
/// all while the card stood idle 64-113 ms a frame, runs 158, 159 and 162;
/// 32 settled the flights three times a
/// frame in run 92 and 64 still 1.3-1.7 times in run 99, with 12,919 tables
/// asked and none refused; watch "tables asked, N refused"): their dynamic
/// descriptor slots, upload
/// memory and scratch chunks are held until then, and a frame's lists held
/// all at once ran the slots out before (run 45).
fn late_most() -> usize {
    static MOST: OnceLock<usize> = OnceLock::new();
    *MOST.get_or_init(|| {
        std::env::var("VICTORY_LATE_WAIT_LISTS").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(256).max(1)
    })
}

/// Whether a submission with bottom-of-pipe writes may fly
/// (`VICTORY_LATE_WAIT_WRITES=0`: it is settled as soon as it is sent, so
/// the title is told at once, as before; for telling a title that polls
/// such a value from one that does not).
fn late_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LATE_WAIT_WRITES").is_none_or(|v| v != "0"))
}

/// Per host copy, the ranges a submission's shaders may write, as declared
/// (address, end).
type Declared = (std::sync::Arc<crate::shadow::Shadow>, Vec<(usize, usize)>);

/// What [`Host::submit`] hands back for a submission it sent and did not
/// wait for.
struct Sent {
    /// Its number among the submissions started.
    number: u64,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    /// What was taken off it to be done on the card.
    own: Option<card_steps::Taken>,
    /// Title-memory ranges written when its shader textures are copied back.
    texture_back_ranges: Vec<(usize, usize)>,
    /// Its bottom-of-pipe writes.
    writes: Vec<(usize, crate::queue::Written, usize)>,
    /// Its declared writes, registered in the line until it is a flight
    /// there ([`pending_flights`]).
    pending: Option<PendingFlight>,
}

/// A submission on the card that has not been waited for ([`late`]): its
/// host lists, taken from the title's (which it may reset meanwhile), and
/// what is to be done once it has run.
struct Flight {
    number: u64,
    /// The value signalled on the host's fence behind it.
    fence: u64,
    lists: Vec<crate::list::Detached>,
    order: Vec<usize>,
    card: Option<CardPlan>,
    back: Vec<Declared>,
    /// The list of ours that ran ahead of them ([`card_steps`]).
    own: Option<card_steps::Taken>,
    texture_back_ranges: Vec<(usize, usize)>,
}

/// One thing in the line of what waits for the card, in order.
enum Late {
    Run(Flight),
    /// A bottom-of-pipe write of the flight before it: address, what, bytes.
    Write(usize, crate::queue::Written, usize),
    /// A fence (its address) signalled behind the flights before it.
    Signal(usize, u64),
}

struct Line {
    line: Vec<Late>,
    /// The title memory the flights' shaders and texture readbacks may write
    /// (address, end), in order and joined.
    written: Vec<(usize, usize)>,
    /// Host lists in the flights.
    lists: usize,
    /// The value signalled on the host's fence behind the last flight.
    target: u64,
    /// A settle has taken the line and is not done: a signal still goes
    /// behind it.
    settling: bool,
    /// Something was settled away from a queue's own calls: the helper
    /// thread looks at what queues hold.
    poke: bool,
    /// Host lists of settled flights, reset and given back outside the
    /// submission lock ([`land`]: a settle may run under a caller's own
    /// locks).
    landed: Vec<(Vec<crate::list::Detached>, Vec<usize>)>,
    /// With [`relay`]: the bottom-of-pipe writes of the flights being
    /// landed (taken out of `line`, not done yet), for [`late_met`].
    in_hand: Vec<(usize, crate::queue::Written, usize)>,
    /// With [`card_steps`]: what the flights being landed write.
    landing: Vec<card_steps::Writer>,
    /// The title memory that submissions being prepared, not yet flights in
    /// `line`, declare written, by the number [`PendingFlight`] holds
    /// (`VICTORY_PENDING_FLIGHT=1`): in `written` from the moment they are
    /// registered, and kept there by every rebuilding of `written`, until the
    /// flight is in `line`.
    pending: Vec<(u64, Vec<(usize, usize)>)>,
    /// What the registered submissions write besides the ranges ([`card_steps::Writer`]), by the same number: the
    /// destinations of the copies they do on the card, what their shaders write through which host copy, their read
    /// backs and the values written when they are done. Until the flight is in `line`, nothing else tells that title
    /// memory there is not yet what they will put (rev2: the thread of stage A refreshed another host copy of such a
    /// destination), and the thread of stage A that plans the next submission for the card (`VICTORY_STAGE_A_PLAN`)
    /// finds the host copy that holds what they write here, as it would in the flight.
    pending_copies: Vec<(u64, Vec<card_steps::Writer>)>,
    pending_next: u64,
    /// How many submissions have been registered ever (`VICTORY_STAGE_A_PLAN`): the plan the thread of stage A made for
    /// the card is against the line as this number says, and stands only while it is the same (a submission registered
    /// since writes what the plan did not know of).
    generation: u64,
}

// SAFETY: D3D12 objects (free-threaded) and host lists nothing records
// into; touched under the lock.
unsafe impl Send for Line {}

impl Line {
    const fn new() -> Line {
        Line {
            line: Vec::new(),
            written: Vec::new(),
            lists: 0,
            target: 0,
            settling: false,
            poke: false,
            landed: Vec::new(),
            in_hand: Vec::new(),
            landing: Vec::new(),
            pending: Vec::new(),
            pending_copies: Vec::new(),
            pending_next: 0,
            generation: 0,
        }
    }

    /// Declares `ranges` written by a submission being prepared: they are in
    /// `written` now. The number to end it with ([`Line::clear_pending`]).
    #[cfg(test)]
    fn register_pending(&mut self, ranges: Vec<(usize, usize)>) -> u64 {
        self.register_pending_with(ranges, Vec::new())
    }

    /// [`Line::register_pending`] with the destinations of the copies the submission does on the card.
    fn register_pending_with(&mut self, ranges: Vec<(usize, usize)>, copies: Vec<card_steps::Writer>) -> u64 {
        self.pending_next += 1;
        self.generation += 1;
        join_ranges(&mut self.written, ranges.iter().copied());
        self.pending.push((self.pending_next, ranges));
        if !copies.is_empty() {
            self.pending_copies.push((self.pending_next, copies));
        }
        self.pending_next
    }

    /// The registered submission is no more (it was dropped before it was a
    /// flight): with nothing in the air its ranges leave `written` too, so
    /// nothing waits for flights that are not there.
    fn clear_pending(&mut self, id: u64) {
        self.pending.retain(|p| p.0 != id);
        self.pending_copies.retain(|p| p.0 != id);
        if self.line.is_empty() && self.in_hand.is_empty() && !self.settling {
            self.rebuild_written(std::iter::empty());
        }
    }

    /// `written` from the ranges of the flights in the air and of the
    /// submissions registered.
    fn rebuild_written(&mut self, flights: impl IntoIterator<Item = (usize, usize)>) {
        rebuilt(&mut self.written, flights, &self.pending);
    }
}

/// `written` made again from what flights in the air declare and what the
/// registered submissions do (every place that makes `written` anew goes
/// through here, so a registration is never lost between its call and the
/// flight's).
fn rebuilt(
    written: &mut Vec<(usize, usize)>,
    flights: impl IntoIterator<Item = (usize, usize)>,
    pending: &[(u64, Vec<(usize, usize)>)],
) {
    written.clear();
    join_ranges(written, flights.into_iter().chain(pending.iter().flat_map(|p| p.1.iter().copied())));
}

/// A submission being prepared whose declared writes are registered in the
/// line ([`Line::register_pending`]); `done` when the flight is in it. If it
/// is dropped before, the registration ends with it.
struct PendingFlight(Option<u64>);

impl PendingFlight {
    #[cfg(test)]
    fn register(ranges: Vec<(usize, usize)>) -> PendingFlight {
        Self::register_with(ranges, Vec::new())
    }

    fn register_with(ranges: Vec<(usize, usize)>, copies: Vec<card_steps::Writer>) -> PendingFlight {
        PendingFlight(Some(line().register_pending_with(ranges, copies)))
    }

    /// The flight is in the line: ended under the lock the caller holds.
    fn done(mut self, pending: &mut Vec<(u64, Vec<(usize, usize)>)>, copies: &mut Vec<(u64, Vec<card_steps::Writer>)>) {
        if let Some(id) = self.0.take() {
            pending.retain(|p| p.0 != id);
            copies.retain(|p| p.0 != id);
        }
    }
}

impl Drop for PendingFlight {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            line().clear_pending(id);
        }
    }
}

/// Whether the declared writes of a submission are in the line from before
/// the card is given its lists, not only when it is in the air
/// (`VICTORY_PENDING_FLIGHT=1`; off by default).
fn pending_flights() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PENDING_FLIGHT").is_some_and(|v| v == "1"))
}

static LINE: Mutex<Line> = Mutex::new(Line::new());
/// Told when a flight joins [`LINE`] or it is poked.
static LINE_MOVED: std::sync::Condvar = std::sync::Condvar::new();

fn line() -> std::sync::MutexGuard<'static, Line> {
    LINE.lock().unwrap_or_else(|e| e.into_inner())
}

/// Why flights were settled ([`LATE_STATS`]' places from 2 on).
pub(crate) const WHY_SYNC: usize = 2;
const WHY_CARD_DONE: usize = 3;
const WHY_VALUE: usize = 4;
const WHY_STALE: usize = 5;
const WHY_READ: usize = 6;
const WHY_RUN: usize = 7;
const WHY_MOST: usize = 8;
const WHY_WRITES: usize = 9;
const WHY_FENCE: usize = 10;

/// Since the last frame-rate line: submissions sent without a wait;
/// settles; settles by reason ([`WHY_SYNC`]..); flights settled;
/// microseconds settles waited for the card; value waits met by a flight's
/// bottom-of-pipe write; fence signals set behind flights.
/// Then: the settles for title memory read, by what read it (a buffer
/// copy, a fill of upload memory, a texture).
static LATE_STATS: [std::sync::atomic::AtomicU64; 18] = [const { std::sync::atomic::AtomicU64::new(0) }; 18];

/// Since the last frame-rate line: the microseconds settles waited for the
/// card, by why they settled (the `WHY_` places).
static LATE_WAITED: [std::sync::atomic::AtomicU64; 11] = [const { std::sync::atomic::AtomicU64::new(0) }; 11];

/// Whether `[from, to)` touches one of `ranges` (in order, joined).
fn touches(ranges: &[(usize, usize)], from: usize, to: usize) -> bool {
    if from >= to {
        return false;
    }
    let at = ranges.partition_point(|r| r.1 <= from);
    ranges.get(at).is_some_and(|r| r.0 < to)
}

/// `ranges` with `more`, in order, joined where they overlap or touch.
fn join_ranges(ranges: &mut Vec<(usize, usize)>, more: impl IntoIterator<Item = (usize, usize)>) {
    ranges.extend(more.into_iter().filter(|r| r.0 < r.1));
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for &(from, to) in ranges.iter() {
        match out.last_mut() {
            Some(last) if from <= last.1 => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    *ranges = out;
}

/// Whether a submission's steps are prepared with the leaner bookkeeping
/// (on unless `VICTORY_LEAN_STEPS=0`): the tables it makes by copy
/// are the thread's kept ones ([`scratch`]), the list of what its steps do
/// (`card_steps::does_of`) leaves out the 21,000 refreshes the plan never
/// looks at, a refresh that comes behind another of the same copy does not
/// look the copy up again (`Batch::add`), what is refreshed ahead of the steps is
/// made and kept without a list of ranges and its allocation for each of
/// thousands of copies (`front_spans_flat`, `Batch::first`), the spans that copies have
/// noted as holding nothing new are left out by threads (`retain_unnoted`), and the slots the
/// card's steps make are made with one hold on their buffer and one look at the
/// heap for each thread (`descriptor::constant_slots`), not for each slot.
///
/// The steps, the refreshes and the descriptors that come out are the same:
/// only where the memory of the tables comes from, how often a table is
/// asked, and how many holds are taken differ.
fn lean_steps() -> bool {
    // (May change while the title runs, `VICTORY_AB`: each use reads it once, and a submission's tables and lists are its own.)
    crate::ab::live_value!("VICTORY_LEAN_STEPS", bool, crate::ab::unless_zero)
}

/// Whether what a submission does for the copies in the card's memory is made with the leaner bookkeeping
/// (on unless `VICTORY_LEAN_CARD=0`): the copies whose CPU side changed are found through a queue of the
/// touched ones, not by a look at all 900 for each of the 28 submissions a frame (`shadow::card_dirty`); the bytes
/// taken out of the CPU sides (44 MB a frame, in 15 submissions of under the 4 MB that went to threads, so
/// copied by the submitting thread into memory the card reads) are copied by the threads that write the pairs,
/// in one batch with them (`send::take`); and a copy's twin is asked for without a hold on it
/// (`Shadow::has_before`). The same bytes go up, in the same places.
pub(crate) fn lean_card() -> bool {
    // (May change while the title runs, `VICTORY_AB`: each use reads it once and what it makes does not outlive
    // the submission; the queue of touched copies is kept whichever way they are found.)
    crate::ab::live_value!("VICTORY_LEAN_CARD", bool, crate::ab::unless_zero)
}

/// Whether the submitting thread leaves the landed lists (reset, their upload memory and descriptor slots given back)
/// to the thread that settles flights, which does it when it is asked to ([`late_helper`]), and not at the end of each
/// submission it sends (on unless `VICTORY_LAND_ASIDE=0`): 1.4 ms a frame of the submitting thread's.
///
/// What the lists hold is let go a moment later, and what is let go goes to the next list that asks for it, which
/// takes new memory when there is none: every call of `land` takes what is due when it is made (`bury_lag` behind).
fn land_aside() -> bool {
    // (May change while the title runs, `VICTORY_AB`: each submission takes the answer once, and either way every
    // list landed is given back by the next `land`, whoever makes it.)
    crate::ab::live_value!("VICTORY_LAND_ASIDE", bool, crate::ab::unless_zero)
}

/// Whether the snapshots of the ranges shaders may write are copied by the kept threads while the submitting thread
/// goes on, and waited for before the lists go to the card (on unless `VICTORY_SNAP_ASIDE=0`): the 3.5 ms a
/// frame of the "snapshots" stage ([`crate::shadow::take_snapshots_aside`] says why the bytes stay as they are).
///
/// The wait ([`crate::shadow::snapshots_wait`]) comes after what the thread does next ("copies in the card's
/// memory", 12 ms a frame): before it settles flights for the card's results, before the next submission's
/// refreshes are offered to the prefetch thread, and before the copies the lists put into host copies and the
/// sending.
fn snap_aside() -> bool {
    // (May change while the title runs, `VICTORY_AB`: each submission takes the answer once, and a submission that
    // gave snapshots away waits for them before it sends, whatever the switch says by then.)
    crate::ab::live_value!("VICTORY_SNAP_ASIDE", bool, crate::ab::unless_zero)
}

/// Whether anything is in the air.
fn flying() -> bool {
    if !late() {
        return false;
    }
    let line = line();
    // (With the relay: also while flights are being landed.)
    line.settling || !line.pending.is_empty() || line.line.iter().any(|l| matches!(l, Late::Run(_)))
}

/// With flights in the air ([`late`]): why `runs` cannot be prepared
/// before they are settled, if so. Preparing reads title memory on the
/// CPU (the title's buffer copies, fills of upload memory, textures) and
/// runs steps of ours that may read or write it anywhere; what the
/// flights' shaders write is not there yet.
fn late_blocked(runs: &[&HostList]) -> Option<usize> {
    late_blocked_counted(runs, true)
}

/// [`late_blocked`]; `count` whether the kind of read is counted for the
/// frame line (not when only asking, as [`blocked_cut`] does).
fn late_blocked_counted(runs: &[&HostList], count: bool) -> Option<usize> {
    let (written, copies) = {
        let line = line();
        if line.written.is_empty() {
            return None;
        }
        (line.written.clone(), card_steps::copies(&line))
    };
    blocked_by(&written, runs, count).or_else(|| card_steps::copied_read(&copies, runs).then_some(WHY_READ))
}

/// Whether a submission's asking of the write watch is one that may stand
/// beside the asking of another (`VICTORY_ASK_TOKENS=1`; off by default): each
/// ends alone, and nothing it does touches the others'. With one submission
/// at a time it answers as the one asking does; it is the ground for
/// preparing two submissions at once.
fn ask_tokens() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_ASK_TOKENS").is_some_and(|v| v == "1"))
}

/// What a submission's one asking is about ([`crate::watch::once`]): per host copy the span of the ranges its lists
/// refresh, if the copy is asked ahead; with `lean` ([`lean_steps`]) in tables the thread keeps.
fn ask_spans(runs: &[&HostList], lean: bool) -> Vec<(usize, usize)> {
    ask_spans_where(runs, lean, |shadow| shadow.asked_ahead())
}

/// [`ask_spans`] for the copies `asked` says yes to (of each, at each of its refreshes), over any lists of steps.
fn ask_spans_where<L: onepass::StepList>(runs: &[L], lean: bool, asked: impl Fn(&crate::shadow::Shadow) -> bool) -> Vec<(usize, usize)> {
    ask_spans_each(runs, lean, asked, None)
}

/// [`ask_spans`] without the refreshes the thread of the entry takes as done (`VICTORY_STAGE_A_FRONT2`: a range that lies
/// whole inside a span the second thread of stage A asked about and refreshed).
fn ask_spans_skipping(runs: &[&HostList], lean: bool, skip: &front2::Certified) -> Vec<(usize, usize)> {
    ask_spans_each(runs, lean, |shadow| shadow.asked_ahead(), (!skip.is_empty()).then_some(skip))
}

/// [`ask_spans_where`], leaving out the ranges `skip` covers.
fn ask_spans_each<L: onepass::StepList>(
    runs: &[L],
    lean: bool,
    asked: impl Fn(&crate::shadow::Shadow) -> bool,
    skip: Option<&front2::Certified>,
) -> Vec<(usize, usize)> {
    thread_local! {
        static ASKED: std::cell::RefCell<ByCopy<(usize, usize)>> = std::cell::RefCell::new(ByCopy::default());
    }
    let mut spans = scratch::Lean::of(&ASKED, lean);
    for list in runs {
        list.steps(|steps| {
            for step in steps {
                if let Step::Refresh(shadow, address, len) = step
                    && asked(&**shadow)
                    && !skip.is_some_and(|skip| skip.covers(std::sync::Arc::as_ptr(shadow) as usize, *address, *address + *len))
                {
                    let span = spans
                        .entry(std::sync::Arc::as_ptr(shadow) as usize)
                        .or_insert((*address, *address + *len));
                    *span = (span.0.min(*address), span.1.max(*address + *len));
                }
            }
        });
    }
    spans.values().copied().collect()
}

/// The write watch asked about `ranges` for a submission, in place of its
/// earlier asking if it had one ([`ask_tokens`]).
fn ask_for_submission(ranges: &[(usize, usize)], asking: &mut Option<crate::watch::Standing>) {
    if ask_tokens() && crate::watch::once() {
        *asking = Some(crate::watch::ask_standing(ranges, asking.take()));
    } else {
        crate::watch::ask_once(ranges);
    }
}

/// What to do with a submission of several parts ([`joins`]) when preparing
/// it would wait for flights ([`late_blocked`]): `VICTORY_SPLIT_BLOCKED=1`
/// sends the parts in front of the first that waits, prepared and sent at
/// once, and then the rest; `=2` only counts what it would do (the frame
/// line says), for a run to compare with. Off by default.
///
/// Why: a joined submission waits for the flights its worst part reads from
/// before it does anything (profile sub1, cell, two of four frames: one
/// submission of 36 lists stood 61 and 117 ms at the start, the card running
/// the flights it waited for, and then took 55 ms of the thread's work while
/// the card ran its other work: the two never overlapped). The parts in
/// front of that one read nothing the flights write; sent first, their
/// preparing goes on while the flights run, and the card has their lists
/// sooner.
///
/// What it does not change: the order of the lists on the queue (the parts
/// are the same ones, in the same order, in calls of their own as they were
/// before they were joined), and the waits (the part that reads from flights
/// waits for the same ones; the parts before it do not write what it reads,
/// [`joinable`] saw to that).
fn split_blocked() -> u8 {
    static MODE: OnceLock<u8> = OnceLock::new();
    *MODE.get_or_init(|| match std::env::var("VICTORY_SPLIT_BLOCKED").ok().as_deref().map(str::trim) {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

/// Since the last frame-rate line: submissions of several parts that waited
/// for flights and had parts in front of the wait (split, or in `=2` only
/// counted), the lists of those parts, and all the lists of those
/// submissions.
static BLOCKED_SPLITS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// Takes [`BLOCKED_SPLITS`] for the frame-rate line.
pub(crate) fn take_blocked_splits() -> Option<[u64; 3]> {
    (split_blocked() > 0).then(|| [0, 1, 2].map(|i| BLOCKED_SPLITS[i].swap(0, Ordering::Relaxed)))
}

/// Of the places `parts` of a submission begin at (in `order`, ascending, the
/// first part beginning at 0), the last at which what comes before does not
/// wait (`waits` of the first `n` places of the order), if the whole of
/// `len` places waits. What waits only grows with the places taken, so the
/// parts in front of the first that waits are found by a bisection.
fn cut_before_wait(parts: &[usize], len: usize, waits: impl Fn(usize) -> bool) -> Option<usize> {
    if parts.is_empty() || !waits(len) {
        return None;
    }
    let free = parts.partition_point(|&at| !waits(at));
    free.checked_sub(1).map(|i| parts[i])
}

/// With `VICTORY_SPLIT_LISTS=1` (and [`split_blocked`]): the cut may also
/// fall between two lists of a part, not only between parts, so that a
/// submission of one part, or a part that reads from flights late in its
/// lists, sends the lists before that read first. It is made only where the
/// lists behind it are [`joinable`] with those before (none of their CPU
/// steps reads what those write or copies title memory), and the first half
/// has at least `VICTORY_SPLIT_MIN` lists (2): the second preparing has the
/// write watch asked again and a send of its own, about 1 to 3 ms.
fn split_lists() -> (bool, usize) {
    static ON: OnceLock<(bool, usize)> = OnceLock::new();
    *ON.get_or_init(|| {
        let min = std::env::var("VICTORY_SPLIT_MIN").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2usize).max(1);
        (std::env::var_os("VICTORY_SPLIT_LISTS").is_some_and(|v| v == "1"), min)
    })
}

/// Of `len` places of an order, the most that can go first, if the whole
/// waits (`waits` of the first `n` places; it only grows with `n`).
fn lists_before_wait(len: usize, waits: impl Fn(usize) -> bool) -> Option<usize> {
    if len < 2 || !waits(len) {
        return None;
    }
    // The least number of places that waits (`len` does): one less goes first.
    let (mut low, mut high) = (1, len);
    while low < high {
        let mid = low + (high - low) / 2;
        if waits(mid) {
            high = mid;
        } else {
            low = mid + 1;
        }
    }
    (low > 1).then_some(low - 1)
}

/// Where a submission of `lists` run in `order`, joined from the parts that
/// begin at `parts`, is to be cut ([`split_blocked`]), if it is.
fn blocked_cut(lists: &[crate::list::Detached], order: &[usize], parts: &[usize]) -> Option<usize> {
    let mode = split_blocked();
    let (by_list, min) = split_lists();
    if mode == 0 || (parts.is_empty() && !by_list) || !flying() {
        return None;
    }
    let runs: Vec<&HostList> = order.iter().map(|&i| lists[i].host()).collect();
    let waits = |n: usize| late_blocked_counted(&runs[..n], false).is_some();
    let by_part = cut_before_wait(parts, runs.len(), waits);
    let cut = by_list
        .then(|| lists_before_wait(runs.len(), waits))
        .flatten()
        // Behind it nothing may read what the lists in front write.
        .filter(|&n| n >= min && joinable(runs[..n].iter().copied(), &runs[n..]))
        .max(by_part)?;
    BLOCKED_SPLITS[0].fetch_add(1, Ordering::Relaxed);
    BLOCKED_SPLITS[1].fetch_add(cut as u64, Ordering::Relaxed);
    BLOCKED_SPLITS[2].fetch_add(runs.len() as u64, Ordering::Relaxed);
    (mode == 1).then_some(cut)
}

/// The places at which parts begin in the two halves of a submission cut at
/// `cut` (a place where a part begins, which begins the second half).
fn parts_around(parts: &[usize], cut: usize) -> (Vec<usize>, Vec<usize>) {
    let first = parts.iter().copied().filter(|&p| p < cut).collect();
    let second = parts.iter().filter(|&&p| p > cut).map(|p| p - cut).collect();
    (first, second)
}

/// Whether a small fill of upload memory, or copy of a title buffer, goes on without waiting for
/// flights when the title itself wrote the memory since they were given it
/// (`VICTORY_FILL_TITLE=1`; off by default).
///
/// The title's 174 MB heap is declared written as a whole (a root view has
/// no size), and the title keeps a ring of constants in it: one fill of 480
/// bytes a frame, at a new place each time, waited for every flight's
/// copy-back, some 30 ms of a 130 ms frame (run blame1: 38 of the 40 waits
/// named). The title writes those bytes just before the draw that reads
/// them, which takes the block's guard off ([`crate::watch::written_since_taken`]).
///
/// Or no shader's write went into that page in the last 600 frames
/// ([`crate::watch::landed_lately`]): the first alone left two thirds of
/// the wait, for constants at the end of a 64 KiB page that the title wrote
/// before the page was taken.
///
/// What can go wrong: a constant a flight's shader really writes, in a
/// block that the title (or an earlier flight's copy-back) also wrote, or
/// for the first time, is read as it was before that flight.
fn fill_is_titles(src: usize, len: usize) -> bool {
    const SMALL: usize = 64 << 10;
    static ON: OnceLock<bool> = OnceLock::new();
    let on = *ON.get_or_init(|| std::env::var_os("VICTORY_FILL_TITLE").is_some_and(|v| v == "1"));
    on && len <= SMALL && (crate::watch::written_since_taken(src, len) || !crate::watch::landed_lately(src, len, 600))
}

/// Diagnostics: the waits for flights, by what waited, why it was not done
/// on the card ([`card_steps`]), its bytes, and the declared write it lies
/// in (start, bytes).
#[cfg(feature = "diagnostics")]
type Blamed = (&'static str, &'static str, usize, usize, usize);
/// How often and how long, and the address the last one read.
#[cfg(feature = "diagnostics")]
static BLAME: Mutex<Option<std::collections::HashMap<Blamed, (u64, u64, usize)>>> = Mutex::new(None);
/// What the wait that comes next is for, and why not on the card.
#[cfg(feature = "diagnostics")]
static BLAMED: Mutex<(Option<(Blamed, usize)>, &'static str)> = Mutex::new((None, ""));

/// Diagnostics: shader writes declared over a megabyte or more: the range
/// (from, to) and what declares it ([`wide_write`]): how often, and its
/// shaders.
#[cfg(feature = "diagnostics")]
static WIDE_WRITES: Mutex<Option<std::collections::HashMap<(usize, usize, usize), (u64, String)>>> = Mutex::new(None);

/// Diagnostics: a draw or dispatch declares that it writes `len` bytes at
/// `address`, through a root view of `pipeline`, a view in a table (0) or a
/// raw address (1).
#[cfg(feature = "diagnostics")]
pub(crate) fn wide_write(address: u64, len: u64, pipeline: usize) {
    if len < 1 << 20 {
        return;
    }
    let mut all = WIDE_WRITES.lock().unwrap_or_else(|e| e.into_inner());
    let e = all
        .get_or_insert_with(Default::default)
        .entry((address as usize, (address + len) as usize, pipeline))
        .or_insert_with(|| {
            let shaders = match pipeline {
                0 => "a view in a table".into(),
                1 => "a raw address".into(),
                p => crate::pipeline::host_pipeline(p as crate::object::This).map_or_else(String::new, |p| {
                    p.stages.iter().map(|s| s.shader.hash.to_string()).collect::<Vec<_>>().join("+")
                }),
            };
            (0, shaders)
        });
    e.0 += 1;
}

#[cfg(feature = "diagnostics")]
fn blame(kind: &'static str, written: &[(usize, usize)], at: usize, len: usize) {
    let i = written.partition_point(|r| r.1 <= at);
    let (from, to) = written.get(i).copied().unwrap_or((0, 0));
    let mut blamed = BLAMED.lock().unwrap_or_else(|e| e.into_inner());
    let why = blamed.1;
    blamed.0 = Some(((kind, why, len, from, to - from), at));
}

/// Why [`card_steps`] left the submission to the wait that comes next.
#[cfg(feature = "diagnostics")]
fn refused(why: &'static str) {
    *BLAMED.lock().unwrap_or_else(|e| e.into_inner()) = (None, why);
}

/// The wait that comes next is for these ranges ([`stale_under_flights`]).
#[cfg(feature = "diagnostics")]
fn blame_stale(stale: &[(std::sync::Arc<crate::shadow::Shadow>, usize, usize)]) {
    if let Some((shadow, from, _)) = stale.first() {
        let (start, len) = shadow.span();
        let key = ("title memory changed under shader writes", crate::shadow::stale_why(), 0, start as usize, len as usize);
        BLAMED.lock().unwrap_or_else(|e| e.into_inner()).0 = Some((key, *from));
    }
}

/// The wait for what [`blame`] last named, or a wait of another reason than
/// title memory read, took `us`.
#[cfg(feature = "diagnostics")]
fn blamed(why: usize, us: u64) {
    let (named, refused) = std::mem::replace(&mut *BLAMED.lock().unwrap_or_else(|e| e.into_inner()), (None, ""));
    let (key, at) = match (why, named) {
        (WHY_READ | WHY_STALE, Some(named)) => named,
        (WHY_READ, None) => (("a host copy of what a copy done on the card wrote", refused, 0, 0, 0), 0),
        _ => {
            let kind = match why {
                WHY_SYNC => "a synchronous submission, a present or the sweep",
                WHY_VALUE => "a memory value",
                WHY_STALE => "title memory changed under shader writes",
                WHY_RUN => "a step of ours",
                WHY_MOST => "lists in the air",
                WHY_FENCE => "a blocking fence wait",
                _ => "another reason",
            };
            ((kind, "", 0, 0, 0), 0)
        }
    };
    let mut all = BLAME.lock().unwrap_or_else(|e| e.into_inner());
    let e = all.get_or_insert_with(Default::default).entry(key).or_default();
    *e = (e.0 + 1, e.1 + us, at);
}

/// Whether flights that write `written` stop a read of `len` bytes at `src`.
fn read_blocked(written: &[(usize, usize)], src: usize, len: usize) -> bool {
    touches(written, src, src + len) && !fill_is_titles(src, len)
}

/// Whether they stop a title buffer copy, and if so whether by its source.
fn copy_blocked(written: &[(usize, usize)], dst: usize, src: usize, len: usize) -> Option<bool> {
    let read = read_blocked(written, src, len);
    (read || touches(written, dst, dst + len)).then_some(read)
}

/// [`late_blocked`] against `written`, the title memory flights' shaders
/// may write (in order, joined); `count` whether the kind of read is
/// counted for the frame line.
fn blocked_by(written: &[(usize, usize)], runs: &[&HostList], count: bool) -> Option<usize> {
    blocked_by_for(written, written, runs, count)
}

/// [`blocked_by`], with what a fill of upload memory reads judged against `fills` and everything else against
/// `written` (the same ranges, or fewer for the fills: [`join_refusal`]).
fn blocked_by_for(written: &[(usize, usize)], fills: &[(usize, usize)], runs: &[&HostList], count: bool) -> Option<usize> {
    if written.is_empty() {
        return None;
    }
    // (With the summaries of the lists' steps [`onepass`] the steps are not walked; a list with a copy at its place is.)
    let scanned = if summarized() { blocked_summarized(written, fills, runs, count) } else { None };
    let placed = match scanned.unwrap_or_else(|| blocked_scan(written, fills, runs, count)) {
        Ok(placed) => placed,
        Err(why) => return Some(why),
    };
    (!placed.is_empty() && place::read_by(runs, placed)).then_some(WHY_READ)
}

/// Whether the submission reads from the steps' summaries (`VICTORY_ONE_PASS`, with `VICTORY_LEAN_STEPS`): each use reads
/// it once and the summaries are the same whichever way they are read.
fn summarized() -> bool {
    onepass::on() && lean_steps()
}

/// What a step that is not a refresh reads, for [`read_stops`].
enum Reads {
    /// A title buffer copy: destination, source, bytes.
    Copy(usize, usize, usize),
    /// A fill of upload memory: source, bytes.
    Fill(usize, usize),
    /// A texture, or a movie plane: from, bytes.
    Texture(usize, usize),
    Plane(usize, usize),
}

/// Whether `written` stops the read (`Some(why)`); `count` whether the kind of read is counted for the frame line.
fn read_stops(written: &[(usize, usize)], fills: &[(usize, usize)], read: Reads, count: bool) -> Option<usize> {
    match read {
        Reads::Copy(dst, src, len) => {
            if let Some(_read) = copy_blocked(written, dst, src, len) {
                if count {
                    LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "diagnostics")]
                    if _read {
                        blame("a buffer copy's source", written, src, len);
                    } else {
                        blame("a buffer copy's destination", written, dst, len);
                    }
                }
                return Some(WHY_READ);
            }
        }
        Reads::Fill(src, len) => {
            if read_blocked(fills, src, len) {
                if count {
                    LATE_STATS[16].fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "diagnostics")]
                    blame("a fill of upload memory", fills, src, len);
                }
                return Some(WHY_READ);
            }
        }
        Reads::Texture(at, len) => {
            if touches(written, at, at + len) {
                if count {
                    LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "diagnostics")]
                    blame("a texture", written, at, len);
                }
                return Some(WHY_READ);
            }
        }
        Reads::Plane(at, len) => {
            if touches(written, at, at.saturating_add(len)) {
                if count {
                    LATE_STATS[17].fetch_add(1, Ordering::Relaxed);
                    #[cfg(feature = "diagnostics")]
                    blame("a movie plane", written, at, len);
                }
                return Some(WHY_READ);
            }
        }
    }
    None
}

/// [`blocked_by`] over the steps: `Err(why)` for the first that stops, else the destinations of copies at their place
/// that a step reads (the readers are looked for by the caller).
fn blocked_scan<L: onepass::StepList>(written: &[(usize, usize)], fills: &[(usize, usize)], runs: &[L], count: bool) -> Result<Vec<(usize, usize)>, usize> {
    // Destinations of copies at their place, if a step reads them.
    let mut placed: Vec<(usize, usize)> = Vec::new();
    for list in runs {
        let stopped = list.steps(|steps| {
            for step in steps {
                let stop = match step {
                    &Step::Copy(dst, src, len) => read_stops(written, fills, Reads::Copy(dst, src, len), count),
                    &Step::Fill(_, src, len, _) => read_stops(written, fills, Reads::Fill(src, len), count),
                    Step::Texture(staging) => {
                        let (at, len) = staging.memory();
                        read_stops(written, fills, Reads::Texture(at, len), count)
                    }
                    &Step::Plane((at, len), _) => read_stops(written, fills, Reads::Plane(at, len), count),
                    Step::Run(_) => Some(WHY_RUN),
                    Step::Placed(p) => match p.waits(written) {
                        place::Waits::No => None,
                        place::Waits::ForReaders => {
                            placed.push((p.dst, p.dst + p.len));
                            None
                        }
                        place::Waits::Yes => {
                            if count {
                                LATE_STATS[15].fetch_add(1, Ordering::Relaxed);
                                #[cfg(feature = "diagnostics")]
                                blame("a buffer copy at its place", written, p.src, p.len);
                            }
                            Some(WHY_READ)
                        }
                    },
                    Step::Refresh(..) | Step::GpuWrite(..) | Step::Host(..) => None,
                };
                if stop.is_some() {
                    return stop;
                }
            }
            None
        });
        if let Some(why) = stopped {
            return Err(why);
        }
    }
    Ok(placed)
}

/// [`blocked_scan`] from the summaries of the lists' steps: the same answer without reading the steps; `None` where a
/// list has a copy at its place among its steps (the caller scans them all: it is one or two a frame).
fn blocked_summarized<L: onepass::StepList>(
    written: &[(usize, usize)],
    fills: &[(usize, usize)],
    runs: &[L],
    count: bool,
) -> Option<Result<Vec<(usize, usize)>, usize>> {
    use onepass::Op;
    for list in runs {
        let summary = list.summary();
        if summary.has_placed() {
            return None;
        }
        for &(_, op) in summary.ops() {
            let stop = match op {
                Op::Copy { dst, src, len } => read_stops(written, fills, Reads::Copy(dst, src, len), count),
                Op::Fill { src, len, .. } => read_stops(written, fills, Reads::Fill(src, len), count),
                Op::Texture(k) => {
                    let (at, len) = summary.texture_memory(k);
                    read_stops(written, fills, Reads::Texture(at, len), count)
                }
                Op::Plane { at, len } => read_stops(written, fills, Reads::Plane(at, len), count),
                Op::Run => Some(WHY_RUN),
                Op::Host { .. } | Op::Placed { .. } => None,
            };
            if let Some(why) = stop {
                return Some(Err(why));
            }
        }
    }
    Some(Ok(Vec::new()))
}

/// With flights in the air, after the submission's write-watch asking:
/// whether `runs` refresh a range of a host copy that a flight's shaders
/// may write and that the title wrote since it was last copied. The
/// refresh leaves ranges shaders are writing alone (`Shadow::update`), so
/// the title's bytes there would reach the card a submission late.
fn stale_under_flights(runs: &[&HostList]) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> {
    stale_merged(stale_where(runs, lean_steps(), |shadow| shadow.writes_pending()))
}

/// [`stale_under_flights`] by the pass of the submission's steps ([`onepass`]), if it has one.
fn stale_of(runs: &[&HostList], pass: Option<&mut onepass::Pass>) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> {
    match pass {
        Some(pass) => stale_merged(pass.stale()),
        None => stale_under_flights(runs),
    }
}

/// The span of what `runs` refresh of each copy that `pending` says shaders may be writing (it is asked once for each
/// copy, at its first refresh): a flush brings that much up to date at most. With `lean` ([`lean_steps`]) in a table the
/// thread keeps.
fn stale_where<L: onepass::StepList>(
    runs: &[L],
    lean: bool,
    pending: impl Fn(&crate::shadow::Shadow) -> bool,
) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> {
    use std::sync::Arc;
    // Per copy, the span of what is refreshed (a flush brings that much
    // up to date at most).
    let mut spans: Vec<(Arc<crate::shadow::Shadow>, usize, usize)> = Vec::new();
    // Where each copy is in it, if shaders may be writing it (none begins to
    // while this runs): a frame has 20,000 refreshes of some 4,500 copies.
    // (With `VICTORY_LEAN_STEPS` the thread's kept table: only looked up.)
    thread_local! {
        static PLACES: std::cell::RefCell<ByCopy<Option<usize>>> = std::cell::RefCell::new(ByCopy::default());
    }
    let mut places = scratch::Lean::of(&PLACES, lean);
    for list in runs {
        list.steps(|steps| {
            for step in steps {
                if let Step::Refresh(shadow, address, len) = step {
                    let place = places.entry(Arc::as_ptr(shadow) as usize).or_insert_with(|| {
                        pending(&**shadow).then(|| {
                            spans.push((shadow.clone(), *address, *address + *len));
                            spans.len() - 1
                        })
                    });
                    if let Some(i) = *place {
                        let s = &mut spans[i];
                        (s.1, s.2) = (s.1.min(*address), s.2.max(*address + *len));
                    }
                }
            }
        });
    }
    spans
}

/// The spans [`stale_where`] found, without those the title did not write since the copy took them (or, with
/// `late_exact`, with what can be put in beside the flights' writes put in).
fn stale_merged(
    mut spans: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> {
    if late_exact() {
        // The title's bytes are put in beside the flights' writes; a
        // settle is still due where that could not be done. At most this
        // many changed runs go up to copies in the card's memory, each a
        // copy of its own there. With [`card_steps`] the pages of the
        // title's ring of constants come here too, which were waited for
        // before: hundreds of short runs each, at about 1.5 us a run.
        let mut runs_left = if card_steps::stage() > 0 { 4096 } else { 512 };
        if under_try() {
            return merge_under_try_first(spans, &mut runs_left);
        }
        spans.retain(|(shadow, from, to)| shadow.merge_under_writes(*from, *to - *from, &mut runs_left));
        return spans;
    }
    spans.retain(|(shadow, from, to)| shadow.stale_under_writes(*from, *to - *from));
    spans
}

/// Whether the title's bytes are put in under flights' writes copy by copy without waiting for a copy that a landing
/// holds (`VICTORY_UNDER_TRY=1`; off by default): the copies whose locks are free go first and those that were held
/// are done last, when the landing may be over. The 4 ms a frame the lane spends in this stage waiting
/// (K6: 25 ms of waits in 6 frames, 5.5 of them for the pages compared on the kept threads, 3.7 for a copy's lock,
/// the rest for the guards) is mostly where a landing of the same copy is at it (`merging` is held shared by every
/// piece landed).
///
/// (May change while the title runs, `VICTORY_AB`: each submission reads it once; the copies are the same and the
/// bytes put in are the same either way, only the order and the moment of each differ.)
fn under_try() -> bool {
    crate::ab::live_flag!("VICTORY_UNDER_TRY")
}

/// [`crate::shadow::Shadow::merge_under_writes`] of each of `spans` (a copy, from, to), those whose locks another
/// thread holds after the others, which wait for them; the spans that still need the flights settled.
fn merge_under_try_first(
    spans: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    runs_left: &mut usize,
) -> Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)> {
    use crate::shadow::{Locks, Merged};
    let mut settle = Vec::with_capacity(spans.len());
    let mut later = Vec::new();
    for span in spans {
        match span.0.merge_under_writes_with(span.1, span.2 - span.1, runs_left, Locks::Try) {
            Merged::Done => {}
            Merged::Settle => settle.push(span),
            Merged::Busy => later.push(span),
        }
    }
    for span in later {
        if span.0.merge_under_writes_with(span.1, span.2 - span.1, runs_left, Locks::Wait) == Merged::Settle {
            settle.push(span);
        }
    }
    settle
}

/// How many parts of submissions, and lists, the line's thread gives to the
/// host as one (`VICTORY_JOIN_PARTS=<n>`; 8 by default (`crate::defaults`; 16 where those do not apply, the tests), 1 turns it off; `VICTORY_JOIN_LISTS`,
/// 96).
///
/// The idea is fxceless/victory-fork's, commit 39731795 (GPL-3.0-or-later, as
/// this): the title's lists wait for memory values, which cuts a frame into
/// some 29 parts, each prepared on its own: the write watch asked, the heaps'
/// records, the title's bytes under the flights' writes, a list that sends
/// what changed and one that compares. (Run submit-1, the dorm, 12 of 28 parts
/// joined, eight at most: the thread works 51.4 ms a frame instead of 54.8, the
/// frame is 0.6 to 3 ms shorter.) What differs here: the parts are taken
/// off one queue's entries of the line (`crate::queue`), still go to the queue
/// in calls of their own, and everything they refresh is brought up to date in
/// front of the first ([`refresh_front`], without which a later part's title
/// bytes under an earlier part's declared writes would go missing).
pub(crate) fn joins() -> (usize, usize) {
    if ahead_of_writes() { joins_asked() } else { (1, 1) }
}

fn joins_asked() -> (usize, usize) {
    static MOST: OnceLock<(usize, usize)> = OnceLock::new();
    let of = |name: &str| crate::defaults::var(name).ok().and_then(|v| v.trim().parse::<usize>().ok());
    let (parts, lists) = *MOST.get_or_init(|| (of("VICTORY_JOIN_PARTS").unwrap_or(16).max(1), of("VICTORY_JOIN_LISTS").unwrap_or(96).max(1)));
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_JOIN_PARTS") {
        return (if on { 8 } else { 1 }, lists);
    }
    (parts, lists)
}

/// Whether the list has a title buffer copy or work that may write title memory among its steps (from its summary with
/// `VICTORY_ONE_PASS`).
fn writes_title_memory(list: &HostList) -> bool {
    if summarized() {
        return onepass::StepList::summary(&list).has_copy_or_run();
    }
    list.deferred().copies.iter().any(|s| matches!(s, Step::Copy(..) | Step::Run(_)))
}

/// Whether the part `next` can be prepared together with `ahead`, which goes
/// before it and has not been sent: nothing done on the CPU for it reads what
/// those give their shaders, write back or write when they are done, and it
/// changes no title memory that they read ([`Step::Copy`], [`Step::Run`]).
pub(crate) fn joinable<'a>(ahead: impl Iterator<Item = &'a HostList>, next: &[&HostList]) -> bool {
    join_refusal(ahead, next).is_none()
}

/// Why a part cannot be prepared together with the ones in front of it ([`joinable`]).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum NoJoin {
    /// It copies title memory or runs a step of ours (`Step::Copy`, `Step::Run`).
    CopyOrRun,
    /// It reads title memory that the lists in front write when they are done (or declare writable in ranges under
    /// [`join_wide`]), by a fill, a texture, a plane or a placed copy.
    Written,
    /// Only what ranges of [`join_wide`] or more, declared writable by the lists in front, make it read ones' memory:
    /// refused here, joined with `VICTORY_JOIN_WIDE_MB` (counted with diagnostics).
    WideOnly,
}

/// Ranges of this many MiB or more, declared written by the lists in front of a part, do not keep it from being
/// prepared with them for what its fills of upload memory read (`VICTORY_JOIN_WIDE_MB=<n>`; 0, off, by default).
///
/// Why: a submission declares written the whole of every view its shaders may store through (the 182 MB heap that
/// holds the title's rings of constants as well), and a fill of the next part that reads a ring inside it is "read
/// what the part in front writes": no join, whatever the shaders write. The title's CPU writes those constants
/// before its call, which is before the group is prepared, so the bytes a fill reads are right when they are read
/// in the order of the group; what is not right is a fill that reads bytes the shaders in front write (a result
/// read back by the CPU and put up again within one frame). Nothing here can tell the two: an experiment, with the
/// picture to judge it by (a wrong fill is a frame of a stale constant), not a proof. Textures, planes, placed
/// copies, copies and steps of ours are judged by every range as before.
///
/// (May change while the title runs, `VICTORY_AB=..,VICTORY_JOIN_WIDE_MB=4`: each decision to join reads it once and
/// keeps no state; the parts joined go on as one submission either way.)
fn join_wide() -> usize {
    fn mib(text: Option<&str>) -> usize {
        text.and_then(|v| v.trim().parse::<usize>().ok())
            .unwrap_or(0)
    }
    let megabytes = crate::ab::live_value!("VICTORY_JOIN_WIDE_MB", usize, mib);
    megabytes << 20
}

/// [`joinable`] with the reason it is not.
pub(crate) fn join_refusal<'a>(
    ahead: impl Iterator<Item = &'a HostList>,
    next: &[&HostList],
) -> Option<NoJoin> {
    join_refusal_with(ahead, next, join_wide(), cfg!(feature = "diagnostics"))
}

/// [`join_refusal`] with the size of the ranges that do not count for fills (`wide`, 0: all count) and whether a
/// refusal that only they cause is told apart (`classify`: [`NoJoin::WideOnly`], else [`NoJoin::Written`]).
fn join_refusal_with<'a>(
    ahead: impl Iterator<Item = &'a HostList>,
    next: &[&HostList],
    wide: usize,
    classify: bool,
) -> Option<NoJoin> {
    if next.iter().any(|l| {
        l.deferred()
            .copies
            .iter()
            .any(|s| matches!(s, Step::Copy(..) | Step::Run(_)))
    }) {
        return Some(NoJoin::CopyOrRun);
    }
    let ahead: Vec<&HostList> = ahead.collect();
    // What shaders may write (whole views), and what the lists write for sure when they are done (values, texture
    // read-backs): only the first can be wide by what it declares and not by what it does.
    let declared: Vec<(usize, usize)> = declared_writes(&ahead)
        .into_iter()
        .flat_map(|(_, ranges)| ranges)
        .collect();
    let mut sure: Vec<(usize, usize)> = Vec::new();
    for list in &ahead {
        let deferred = list.deferred();
        sure.extend(deferred.writes.iter().map(|&(at, _, bytes)| (at, at + bytes)));
        sure.extend(deferred.backs.iter().map(|&(_, at, [row, rows, pitch])| {
            (at, at + rows.saturating_sub(1) * pitch + row)
        }));
    }
    // Without the wide declared ones, for what the fills read.
    let narrow: Option<Vec<(usize, usize)>> = (wide > 0 || classify).then(|| {
        let mut narrow: Vec<(usize, usize)> = Vec::new();
        let under = if wide > 0 { wide } else { 4 << 20 };
        let small = declared.iter().copied().filter(|r| r.1 - r.0 < under);
        join_ranges(&mut narrow, small.chain(sure.iter().copied()));
        narrow
    });
    let mut joined = Vec::new();
    join_ranges(&mut joined, declared.into_iter().chain(sure));
    if blocked_by(&joined, next, false).is_none() {
        return None;
    }
    match narrow {
        // (Fills judged by the narrow ones: if nothing else stands in the way, it joins, with the switch on.)
        Some(narrow) if blocked_by_for(&joined, &narrow, next, false).is_none() => {
            (wide == 0).then_some(NoJoin::WideOnly)
        }
        _ => Some(NoJoin::Written),
    }
}
/// Whether a bottom-of-pipe write of `list` meets `wait`.
pub(crate) fn will_meet(list: &HostList, wait: &crate::list::ValueWait) -> bool {
    let (address, bytes) = wait.place();
    // The last write there says.
    let deferred = list.deferred();
    let last = deferred.writes.iter().rev().find(|(at, _, n)| *at < address + bytes && address < *at + *n);
    matches!(last, Some((at, crate::queue::Written::Value(v), n)) if *at == address && *n >= bytes && wait.holds_for(*v))
}

/// Lists and the order they run in.
type Run<T> = (Vec<T>, Vec<usize>);

/// `order`'s first `cut` places and the rest, each with the lists they name;
/// as it was if a list is named on both sides.
fn split<T>(lists: Vec<T>, order: Vec<usize>, cut: usize) -> Result<[Run<T>; 2], Run<T>> {
    let (first, second) = order.split_at(cut);
    if first.is_empty() || second.is_empty() || first.iter().any(|i| second.contains(i)) {
        return Err((lists, order));
    }
    let behind: Vec<bool> = (0..lists.len()).map(|i| second.contains(&i)).collect();
    let place = |i: usize| behind[..i].iter().filter(|&&b| b == behind[i]).count();
    let orders = [first, second].map(|half| half.iter().map(|&i| place(i)).collect::<Vec<_>>());
    let mut halves: [Vec<T>; 2] = [Vec::new(), Vec::new()];
    for (list, behind) in lists.into_iter().zip(&behind) {
        halves[usize::from(*behind)].push(list);
    }
    let [a, b] = halves;
    let [x, y] = orders;
    Ok([(a, x), (b, y)])
}

/// A fence signal on a queue ([`late`]): goes behind the flights in the
/// air, if there are any, and is set when they are settled. Whether it
/// did; if not the caller sets the fence now.
pub(crate) fn late_signal(fence: usize, value: u64) -> bool {
    if !late() {
        return false;
    }
    let mut line = line();
    if line.line.is_empty() && !line.settling {
        return false;
    }
    line.line.push(Late::Signal(fence, value));
    LATE_STATS[14].fetch_add(1, Ordering::Relaxed);
    drop(line);
    // A signal alone (behind a settle under way) has no flight to wake
    // the helper thread for it.
    LINE_MOVED.notify_one();
    RELAY_MOVED.notify_one();
    true
}

/// A queue stands at `wait`, which memory does not meet now ([`late`]):
/// whether it is met once the flights in the air have run. A flight's
/// bottom-of-pipe write of a value that meets it says yes without a wait
/// for the card: the part behind the wait goes to the card behind that
/// flight, which is the order the wait asks for. If the flights may write
/// the value in a way not known here (shaders, a timestamp, another
/// width), they are settled and memory is asked again.
pub(crate) fn late_met(wait: &crate::list::ValueWait) -> bool {
    if let Some(met) = late_met_known(wait) {
        return met;
    }
    settle_now(WHY_VALUE);
    wait.met()
}

/// [`late_met`] without a wait for the card; `None`: it has to be waited for.
pub(crate) fn late_met_known(wait: &crate::list::ValueWait) -> Option<bool> {
    let (address, bytes) = wait.place();
    {
        let line = line();
        if line.line.is_empty() && line.in_hand.is_empty() {
            return Some(false);
        }
        let mut unsure = touches(&line.written, address, address + bytes);
        // (With the relay: first the writes of flights being landed.)
        let writes = line.in_hand.iter().map(|(at, what, n)| (at, what, n)).chain(line.line.iter().filter_map(|item| match item {
            Late::Write(at, what, n) => Some((at, what, n)),
            _ => None,
        }));
        for (at, what, n) in writes {
            if *at >= address + bytes || address >= *at + *n {
                continue;
            }
            match what {
                crate::queue::Written::Value(v) if *at == address && *n >= bytes => {
                    if wait.holds_for(*v) {
                        LATE_STATS[13].fetch_add(1, Ordering::Relaxed);
                        return Some(true);
                    }
                }
                _ => unsure = true,
            }
        }
        if !unsure {
            return Some(false);
        }
    }
    None
}

/// Settles the flights in the air, if any ([`Host::settle`]), for reason
/// `why` (one of the `WHY_` places). Not from inside a submission.
pub(crate) fn settle_now(why: usize) {
    if !late() {
        return;
    }
    let Some(host) = host() else {
        return;
    };
    let mut submitted = host.submitted.lock().unwrap_or_else(|e| e.into_inner());
    host.settle(&mut submitted, why);
}

/// [`settle_now`] at a present, and for a fence the title blocks on.
pub(crate) fn settle_for_present() {
    settle_now(WHY_SYNC);
}
pub(crate) fn settle_for_fence() {
    settle_now(WHY_FENCE);
}

pub(crate) mod aside;
mod askaside;
pub(crate) mod waits;
mod combine;
pub(crate) mod awake;
pub(crate) mod ours;
mod card_steps;
pub(crate) mod indirects;
#[cfg(feature = "diagnostics")]
mod panel;
mod onepass;
mod place;
pub(crate) mod precise;
mod prefetch;
mod front;
mod front2;
mod plan;
pub(crate) mod relax;
mod scratch;
mod send;
mod stage;
pub(crate) mod lanes;
pub(crate) use send::{goes_as_words, words_under};
pub(crate) mod spans;
mod torn;
#[cfg(feature = "diagnostics")]
mod traffic;
pub(crate) mod upcheck;
#[cfg(feature = "diagnostics")]
mod stamps;
#[cfg(feature = "diagnostics")]
pub(crate) use stamps::{timeline_ready, timeline_restart, timeline_stamps};

/// Since the last frame-rate line: the spans of the refreshes in front of the
/// lists' steps and their bytes, those left after the prune, then those due
/// before shader writes and their bytes.
static REFRESH_SIZE: [std::sync::atomic::AtomicU64; 10] = [const { std::sync::atomic::AtomicU64::new(0) }; 10];

/// Whether the refreshes in front count what the steps really refer to, next
/// to the span they are refreshed as (`VICTORY_REFRESH_SIZING=1`, off by
/// default: a sort of the touches of each flush).
fn refresh_sizing() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REFRESH_SIZING").is_some_and(|v| v == "1"))
}

impl Host {
    /// Whether the host runs mesh shaders at shader model 6.6 (translated
    /// mesh shaders need both); logged once.
    pub(crate) fn mesh_shaders(&self) -> bool {
        static MESH: OnceLock<bool> = OnceLock::new();
        *MESH.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS7::default();
            let mut model = D3D12_FEATURE_DATA_SHADER_MODEL {
                HighestShaderModel: D3D_SHADER_MODEL_6_6,
            };
            // SAFETY: each query writes its own structure of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS7,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
                    && self
                        .device
                        .CheckFeatureSupport(
                            D3D12_FEATURE_SHADER_MODEL,
                            (&raw mut model).cast(),
                            size_of_val(&model) as u32,
                        )
                        .is_ok()
            };
            let mesh = ok
                && options.MeshShaderTier != D3D12_MESH_SHADER_TIER_NOT_SUPPORTED
                && model.HighestShaderModel.0 >= D3D_SHADER_MODEL_6_6.0;
            tracing::info!(mesh, "host mesh shader support");
            mesh
        })
    }

    /// Whether the host GPU supports the depth bounds test; logged once.
    pub(crate) fn depth_bounds(&self) -> bool {
        static BOUNDS: OnceLock<bool> = OnceLock::new();
        *BOUNDS.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS2::default();
            // SAFETY: writes `options` of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS2,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let supported = ok && options.DepthBoundsTestSupported.as_bool();
            tracing::info!(supported, "host depth bounds support");
            supported
        })
    }

    /// A new list, open for recording.
    pub(crate) fn new_list(&self) -> windows::core::Result<HostList> {
        self.new_list_of(false)
    }

    /// A new list, of the compute type if `compute` ([`lanes::want_compute`]).
    pub(crate) fn new_list_of(&self, compute: bool) -> windows::core::Result<HostList> {
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        #[cfg(feature = "diagnostics")]
        let before = private_bytes();
        let list = self.make_list(compute);
        LISTS[0].fetch_add(1, Ordering::Relaxed);
        // (May shrink while a list is made, on another thread's account:
        // added as it is, the sum is what counts.)
        #[cfg(feature = "diagnostics")]
        LISTS[2].fetch_add(private_bytes().wrapping_sub(before), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            use crate::copystat::{C, add};
            add(C::HostLists, 1);
            add(C::HostListNs, started.elapsed().as_nanos() as u64);
        }
        list
    }

    /// An allocator and a closed list for a prelude.
    fn make_prelude(&self) -> Option<Prelude> {
        self.make_prelude_of(false)
    }

    /// [`Host::make_prelude`], of the compute type if `compute`.
    fn make_prelude_of(&self, compute: bool) -> Option<Prelude> {
        let kind = if compute { D3D12_COMMAND_LIST_TYPE_COMPUTE } else { D3D12_COMMAND_LIST_TYPE_DIRECT };
        // SAFETY: plain object creation.
        let made: windows::core::Result<Prelude> = unsafe {
            (|| {
                let allocator: ID3D12CommandAllocator = self.device.CreateCommandAllocator(kind)?;
                let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(0, kind, &allocator, None)?;
                list.Close()?;
                Ok((allocator, list))
            })()
        };
        LISTS[1].fetch_add(1, Ordering::Relaxed);
        match made {
            Ok(prelude) => Some(prelude),
            Err(e) => {
                tracing::warn!(error = %e, "no host list for a prelude");
                None
            }
        }
    }

    fn make_list(&self, compute: bool) -> windows::core::Result<HostList> {
        let kind = if compute { D3D12_COMMAND_LIST_TYPE_COMPUTE } else { D3D12_COMMAND_LIST_TYPE_DIRECT };
        // SAFETY: plain object creation.
        unsafe {
            let allocator: ID3D12CommandAllocator = self.device.CreateCommandAllocator(kind)?;
            let list: ID3D12GraphicsCommandList = self.device.CreateCommandList(0, kind, &allocator, None)?;
            let prelude = OnceLock::new();
            if !lazy_prelude() {
                let _ = prelude.set(self.make_prelude());
            }
            #[cfg(feature = "diagnostics")]
            let notes = std::sync::Arc::<Mutex<Vec<Note>>>::default();
            let pipelines = std::sync::Arc::<Mutex<std::collections::BTreeSet<String>>>::default();
            #[cfg(feature = "diagnostics")]
            let indirects = indirects::Shared::default();
            #[cfg(feature = "diagnostics")]
            if dred_on() || dred_light() {
                static NUMBER: AtomicU32 = AtomicU32::new(0);
                let number = NUMBER.fetch_add(1, Ordering::Relaxed);
                name(&list, || format!("victory host list {number}"));
                let mut all = LIST_NOTES.lock().unwrap_or_else(|e| e.into_inner());
                if number.is_multiple_of(256) {
                    all.retain(|_, l| l.0.strong_count() > 0);
                }
                all.insert(
                    number,
                    (
                        std::sync::Arc::downgrade(&notes),
                        std::sync::Arc::downgrade(&pipelines),
                        std::sync::Arc::downgrade(&indirects),
                    ),
                );
            }
            Ok(HostList {
                allocator,
                list,
                compute,
                deferred: Mutex::default(),
                scratch: Mutex::default(),
                states: Mutex::default(),
                twins: Mutex::default(),
                prelude,
                pipelines,
                #[cfg(feature = "diagnostics")]
                indirects,
                budget: OnceLock::new(),
                card_budget: OnceLock::new(),
                indirect_args: Mutex::default(),
                batch_args: Mutex::default(),
                coming: Mutex::default(),
                epoch: std::sync::atomic::AtomicU64::new(next_epoch()),
                upscale: std::sync::atomic::AtomicU64::new(0),
                hudless: std::sync::atomic::AtomicU64::new(0),
                has_placed: std::sync::atomic::AtomicBool::new(false),
                _alive: Counted::new(),
                #[cfg(feature = "diagnostics")]
                notes,
                #[cfg(feature = "diagnostics")]
                stamps: OnceLock::new(),
                #[cfg(feature = "diagnostics")]
                queue_kind: AtomicU32::new(u32::MAX),
            })
        }
    }

    /// The host queue waits for `fence` to reach `value` (the lanes' fences of
    /// `VICTORY_ASYNC_COMPUTE`): a wait for a signal already queued ahead of
    /// it on this queue is met at once.
    pub(crate) fn queue_wait(&self, fence: &ID3D12Fence, value: u64) {
        // SAFETY: our queue and a fence of ours.
        if let Err(e) = unsafe { self.queue.Wait(fence, value) } {
            tracing::warn!(error = %e, "the host queue was not told to wait for a lane");
        }
    }

    /// The host queue signals `fence` with `value` behind what it was given.
    pub(crate) fn queue_signal(&self, fence: &ID3D12Fence, value: u64) {
        // SAFETY: our queue and a fence of ours.
        if let Err(e) = unsafe { self.queue.Signal(fence, value) } {
            tracing::warn!(error = %e, "the host queue did not signal a lane");
        }
    }

    /// Runs closed lists and waits for them, and with [`late`] for
    /// everything sent to the card before them, which is settled first.
    pub(crate) fn execute(&self, runs: &[&HostList]) {
        // Behind what the title's queues were given before it.
        crate::queue::before_sync();
        let _ = self.submit(runs, &[], false);
    }

    /// Runs a closed internal upload-only host list synchronously without
    /// draining title queues (`queue::before_sync`), so it is deadlock-safe
    /// when invoked from `VICTORY_THIN=1` worker threads. Settles in-flight
    /// GPU work and waits for the upload list's fence before returning.
    pub(crate) fn execute_sync_internal(&self, list: &HostList) {
        let _ = self.submit(&[list], &[], false);
    }

    /// Moves the debug layer's messages into our log; returns the first
    /// error among them.
    pub(crate) fn log_messages(&self) -> Option<String> {
        let Some(queue) = &self.messages else {
            return None;
        };
        let mut first_error = None;
        // SAFETY: each message is read into a buffer of the size the queue
        // reports, then the queue is emptied.
        unsafe {
            for i in 0..queue.GetNumStoredMessages() {
                let mut size = 0;
                if queue.GetMessage(i, None, &mut size).is_err() || size == 0 {
                    continue;
                }
                let mut buffer = vec![0u64; size.div_ceil(8)];
                let message = buffer.as_mut_ptr().cast::<D3D12_MESSAGE>();
                if queue.GetMessage(i, Some(message), &mut size).is_ok() {
                    let m = &*message;
                    let text = std::slice::from_raw_parts(m.pDescription, m.DescriptionByteLength);
                    let text = String::from_utf8_lossy(text);
                    let text = text.trim_end_matches('\0');
                    tracing::warn!(id = m.ID.0, severity = m.Severity.0, "D3D12: {text}");
                    if m.Severity.0 <= D3D12_MESSAGE_SEVERITY_ERROR.0 && first_error.is_none() {
                        first_error = Some(text.to_owned());
                    }
                }
            }
            queue.ClearStoredMessages();
        }
        first_error
    }

    /// A host render target view of `resource`, valid until it is recorded.
    pub(crate) fn rtv(
        &self,
        resource: Option<&ID3D12Resource>,
        desc: Option<&D3D12_RENDER_TARGET_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_rtv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource, or none with a
        // description (a typed null view).
        unsafe {
            let mut handle = self.rtvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.rtv_size) as usize;
            self.device
                .CreateRenderTargetView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// A slot of CPU-only views, valid until it is recorded.
    pub(crate) fn cpu_view(&self) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_cpu_view.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a live heap.
        let mut handle = unsafe { self.cpu_views.GetCPUDescriptorHandleForHeapStart() };
        handle.ptr += (slot * self.cpu_view_size) as usize;
        handle
    }

    /// A host depth view of `resource`, valid until it is recorded.
    pub(crate) fn dsv(
        &self,
        resource: &ID3D12Resource,
        desc: Option<&D3D12_DEPTH_STENCIL_VIEW_DESC>,
    ) -> D3D12_CPU_DESCRIPTOR_HANDLE {
        let slot = self.next_dsv.fetch_add(1, Ordering::Relaxed) % RTV_RING;
        // SAFETY: a slot of our heap and a live resource.
        unsafe {
            let mut handle = self.dsvs.GetCPUDescriptorHandleForHeapStart();
            handle.ptr += (slot * self.dsv_size) as usize;
            self.device
                .CreateDepthStencilView(resource, desc.map(|d| d as *const _), handle);
            handle
        }
    }

    /// 1-descriptor shader-visible CBV/SRV/UAV fallback heap for directly-indexed root signatures.
    pub(crate) fn fallback_view_heap(&self) -> Option<&ID3D12DescriptorHeap> {
        self.fallback_view
            .get_or_init(|| {
                // SAFETY: plain 1-descriptor shader-visible heap + null SRV initialization.
                unsafe {
                    let heap: ID3D12DescriptorHeap = self
                        .device
                        .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                            Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                            NumDescriptors: 1,
                            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                            NodeMask: 0,
                        })
                        .ok()?;
                    let desc = D3D12_SHADER_RESOURCE_VIEW_DESC {
                        Format: windows::Win32::Graphics::Dxgi::Common::DXGI_FORMAT_R32_UINT,
                        ViewDimension: D3D12_SRV_DIMENSION_BUFFER,
                        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
                        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
                            Buffer: D3D12_BUFFER_SRV::default(),
                        },
                    };
                    self.device.CreateShaderResourceView(
                        None,
                        Some(&desc),
                        heap.GetCPUDescriptorHandleForHeapStart(),
                    );
                    Some(heap)
                }
            })
            .as_ref()
    }

    /// 1-descriptor shader-visible Sampler fallback heap for directly-indexed root signatures.
    pub(crate) fn fallback_sampler_heap(&self) -> Option<&ID3D12DescriptorHeap> {
        self.fallback_sampler
            .get_or_init(|| {
                // SAFETY: plain 1-descriptor shader-visible sampler heap initialization.
                unsafe {
                    let heap: ID3D12DescriptorHeap = self
                        .device
                        .CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                            Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                            NumDescriptors: 1,
                            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                            NodeMask: 0,
                        })
                        .ok()?;
                    let desc = D3D12_SAMPLER_DESC {
                        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
                        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                        MaxLOD: f32::MAX,
                        ..Default::default()
                    };
                    self.device
                        .CreateSampler(&desc, heap.GetCPUDescriptorHandleForHeapStart());
                    Some(heap)
                }
            })
            .as_ref()
    }
}

impl HostList {
    /// The timestamp heap and its read-back buffer (diagnostics, timing on).
    #[cfg(feature = "diagnostics")]
    fn stamps(&self) -> Option<&(ID3D12QueryHeap, ID3D12Resource)> {
        if !timing() {
            return None;
        }
        self.stamps
            .get_or_init(|| {
                let device = &host()?.device;
                let mut heap: Option<ID3D12QueryHeap> = None;
                let mut back: Option<ID3D12Resource> = None;
                let readback = D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_READBACK, ..Default::default() };
                let desc = D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: u64::from(STAMPS) * 8,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                };
                // SAFETY: plain object creation.
                unsafe {
                    device
                        .CreateQueryHeap(
                            &D3D12_QUERY_HEAP_DESC { Type: D3D12_QUERY_HEAP_TYPE_TIMESTAMP, Count: STAMPS, NodeMask: 0 },
                            &mut heap,
                        )
                        .ok()?;
                    device
                        .CreateCommittedResource(
                            &readback,
                            D3D12_HEAP_FLAG_NONE,
                            &desc,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            None,
                            &mut back,
                        )
                        .ok()?;
                }
                Some((heap?, back?))
            })
            .as_ref()
    }

    /// The title's queue (kind) this list is submitted to, noted for the
    /// timing report (diagnostics).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn set_queue_kind(&self, kind: u32) {
        self.queue_kind.store(kind, Ordering::Relaxed);
    }

    /// The kind of the title's queue the list was last submitted to
    /// (`u32::MAX`: none noted).
    #[cfg(feature = "diagnostics")]
    fn queue_kind(&self) -> u32 {
        self.queue_kind.load(Ordering::Relaxed)
    }

    /// The noted commands that took longest on the GPU, longest first, as
    /// (milliseconds, text) (diagnostics, timing on, after the list ran).
    #[cfg(feature = "diagnostics")]
    fn slowest(&self, frequency: u64, n: usize) -> Vec<(f64, String)> {
        let notes = self.notes.lock().unwrap_or_else(|e| e.into_inner());
        let (Some((_, back)), count) = (self.stamps(), notes.len().min(STAMPS as usize - 1)) else {
            return Vec::new();
        };
        let mut data = std::ptr::null_mut();
        let range = D3D12_RANGE { Begin: 0, End: (count + 1) * 8 };
        // SAFETY: a read-back buffer the finished list resolved into.
        let stamps: Vec<u64> = unsafe {
            if back.Map(0, Some(&range), Some(&mut data)).is_err() {
                return Vec::new();
            }
            let s = std::slice::from_raw_parts(data.cast::<u64>(), count + 1).to_vec();
            back.Unmap(0, None);
            s
        };
        let mut times: Vec<(f64, String)> = (0..count)
            .map(|i| {
                let ticks = stamps[i + 1].saturating_sub(stamps[i]);
                (ticks as f64 * 1e3 / frequency.max(1) as f64, notes[i].0.chars().take(300).collect())
            })
            .collect();
        times.sort_by(|a, b| b.0.total_cmp(&a.0));
        times.truncate(n);
        times
    }

    pub(crate) fn close(&self) {
        let _closing = crate::timeline::span("closing a host list");
        self.queries_end();
        self.twins_to_depth();
        self.textures_back();
        let scratch = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some((scratch, _)) = scratch {
            self.indirect_common(&scratch);
        }
        self.batches_common();
        self.card_budget_out();
        self.take_coming();
        #[cfg(feature = "diagnostics")]
        if let Some((heap, back)) = self.stamps() {
            let count = (self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32).min(STAMPS - 1);
            // SAFETY: an open list, our heap and read-back buffer.
            unsafe {
                self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, count);
                self.list.ResolveQueryData(heap, D3D12_QUERY_TYPE_TIMESTAMP, 0, count + 1, back, 0);
            }
        }
        // SAFETY: our list.
        if let Err(e) = unsafe { self.list.Close() } {
            fail(format_args!(
                "a host command list did not close ({e}): a command recorded into it was invalid"
            ));
        }
    }

    /// `work` finds, on a thread of ours, refreshes that belong at this
    /// place among the list's steps; they are there when the list is closed.
    pub(crate) fn refreshes_coming(&self, work: impl FnOnce() -> Vec<aside::Refresh> + Send + 'static) {
        self.take_coming();
        let at = self.deferred().copies.len();
        let coming = aside::begin(work);
        *self.coming.lock().unwrap_or_else(|e| e.into_inner()) = Some((at, coming));
    }

    /// The refreshes that were still to come are put among the steps.
    fn take_coming(&self) {
        let Some((at, coming)) = self.coming.lock().unwrap_or_else(|e| e.into_inner()).take() else {
            return;
        };
        let found = coming.wait();
        let mut deferred = self.deferred();
        let at = at.min(deferred.copies.len());
        deferred
            .copies
            .splice(at..at, found.into_iter().map(|(shadow, from, to)| Step::Refresh(shadow, from as usize, (to - from) as usize)));
    }

    /// Only after the list's last submission finished (every one is waited
    /// for).
    pub(crate) fn reset(&self) {
        self.changed_something();
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        let _resetting = crate::timeline::span("resetting a host list");
        self._alive.used();
        self.upscale.store(0, Ordering::Relaxed);
        self.hudless.store(0, Ordering::Relaxed);
        *self.coming.lock().unwrap_or_else(|e| e.into_inner()) = None;
        *self.deferred() = Deferred::default();
        self.has_placed.store(false, Ordering::Relaxed);
        self.states.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.twins.lock().unwrap_or_else(|e| e.into_inner()).clear();
        self.pipelines.lock().unwrap_or_else(|e| e.into_inner()).clear();
        #[cfg(feature = "diagnostics")]
        self.indirects.lock().unwrap_or_else(|e| e.into_inner()).clear();
        #[cfg(feature = "diagnostics")]
        self.notes.lock().unwrap_or_else(|e| e.into_inner()).clear();
        {
            let mut scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
            (scratch.current, scratch.used) = (0, 0);
            // Every submission of the list has run: nothing reads its
            // scratch memory any more, and the next list to ask takes it.
            if scratch_pool() {
                give_chunks(std::mem::take(&mut scratch.chunks));
            }
        }
        // SAFETY: the list is not executing.
        let reset = unsafe {
            self.allocator
                .Reset()
                .and_then(|()| self.list.Reset(&self.allocator, None))
        };
        if let Err(e) = reset {
            fail(format_args!("a host command list did not reset ({e})"));
        }
        #[cfg(feature = "diagnostics")]
        crate::copystat::add(crate::copystat::C::ResetNs, started.elapsed().as_nanos() as u64);
    }

    /// Something changed in this list that bringing a draw's textures and
    /// buffers up to date depends on: a texture's state, a twin, what was
    /// refreshed.
    pub(crate) fn changed_something(&self) {
        self.epoch.store(next_epoch(), Ordering::Relaxed);
    }

    /// See [`HostList::changed_something`].
    pub(crate) fn epoch(&self) -> u64 {
        self.epoch.load(Ordering::Relaxed)
    }

    /// Whether any deferred steps or texture state transitions have been
    /// recorded into this list since creation or its last [`HostList::reset`].
    pub(crate) fn has_recorded_work(&self) -> bool {
        !self.deferred().copies.is_empty()
            || !self.states.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Moves a title texture's host copy to `wanted` (a host state) in this
    /// list: the host texture and the state it leaves, if that is a change.
    /// The list's first use of a texture records nothing: its prelude brings
    /// the texture there at submission.
    pub(crate) fn transition(
        &self,
        resource: crate::object::This,
        wanted: u32,
    ) -> Option<(&'static ID3D12Resource, u32)> {
        let copy = crate::resource::host_copy(resource)?;
        // SAFETY: as in `crate::resource::host_resource`; the list's states
        // hold the copy from here on.
        let texture: &'static ID3D12Resource = unsafe { &*std::ptr::from_ref(&copy.texture) };
        let wanted = legal_for(copy.flags(), wanted);
        let wanted = if self.compute { compute_legal(wanted) } else { wanted };
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        match states.entry(std::sync::Arc::as_ptr(&copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy));
                self.changed_something();
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                if before != wanted {
                    self.changed_something();
                }
                (before != wanted).then_some((texture, before))
            }
        }
    }

    /// Moves host texture `copy` to `wanted` (a host state) in this list,
    /// as [`HostList::transition`] does a title resource's host copy, and
    /// records the barrier: for textures that are not a title resource's
    /// host copy (colour twins) and for the copies between the two.
    fn settle(&self, copy: &std::sync::Arc<crate::resource::HostTexture>, wanted: u32) {
        let wanted = legal_for(copy.flags(), wanted);
        let wanted = if self.compute { compute_legal(wanted) } else { wanted };
        self.changed_something();
        let mut states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        let before = match states.entry(std::sync::Arc::as_ptr(copy) as usize) {
            std::collections::hash_map::Entry::Vacant(v) => {
                v.insert((wanted, wanted, copy.clone()));
                None
            }
            std::collections::hash_map::Entry::Occupied(mut o) => {
                let before = std::mem::replace(&mut o.get_mut().1, wanted);
                (before != wanted).then_some(before)
            }
        };
        drop(states);
        if let Some(before) = before {
            let barrier = transition(
                &copy.texture,
                D3D12_RESOURCE_STATES(before as i32),
                D3D12_RESOURCE_STATES(wanted as i32),
            );
            // SAFETY: an open list and a live texture.
            unsafe { self.list.ResourceBarrier(&[barrier]) };
        }
    }

    /// A use of title texture `resource` about to be recorded, if it is a
    /// depth texture with a colour twin (false if it is not): of the twin
    /// (`colour`: unordered access) or of the depth texture (depth target,
    /// clear, shader read, copy, resolve), writing it or only reading. The
    /// side used is first brought up to date: by a copy recorded here if
    /// this list wrote the other side since, by the list's prelude if the
    /// list uses the side as it finds it. The twin is left in the
    /// unordered access state; the caller moves the depth texture to the
    /// state its use needs, as for any texture. A twin still ahead when the
    /// list closes is copied then (`twins_to_depth`).
    /// shortcut: every depth target bound for a draw counts as written
    /// (also when the draw only tests), and every unordered access view a
    /// shader names as written; views a shader picks from the heap by a
    /// computed index are not seen here at all.
    pub(crate) fn twin_use(
        &self,
        resource: crate::object::This,
        colour: bool,
        write: bool,
    ) -> bool {
        const UNORDERED_ACCESS: u32 = 0x8;
        // Asked for every texture a draw reads: most have no twin.
        let Some(twin) = crate::resource::twin(resource) else {
            return false;
        };
        let Some(depth) = crate::resource::host_copy(resource) else {
            return false;
        };
        let side = usize::from(colour);
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let u = twins
            .entry(std::sync::Arc::as_ptr(&twin) as usize)
            .or_insert_with(|| TwinUse {
                depth: depth.clone(),
                twin: twin.clone(),
                needs: [false; 2],
                behind: [None; 2],
                writing: false,
                late: false,
            });
        let was = (u.needs, u.behind, u.writing);
        let copy = match u.behind[side] {
            Some(behind) => behind,
            None => {
                u.needs[side] = true;
                false
            }
        };
        u.behind[side] = Some(false);
        match (write, colour) {
            // Shaders write the twin when the draw or dispatch runs: what
            // the same one reads of the depth texture is prepared before
            // (`twins_written`).
            (true, true) => u.writing = true,
            (true, false) => u.behind[1] = Some(true),
            (false, _) => {}
        }
        if was != (u.needs, u.behind, u.writing) || copy || colour {
            self.changed_something();
        }
        // shortcut: the depth plane's twin is not drawn into the plane
        // here: the pass sets a pipeline and a target of ours, which a list
        // still recording the title's commands cannot take. This use sees
        // the plane as it was; the pass runs when the list closes
        // (`twins_to_depth`). Counted.
        if copy && !colour && twin.plane.is_some() {
            u.late = true;
            crate::resource::PLANES[2].fetch_add(1, Ordering::Relaxed);
        }
        drop(twins);
        if copy {
            self.twin_copy(&depth, &twin, colour);
        } else if colour {
            self.settle(&twin.colour, UNORDERED_ACCESS);
        }
        // The depth plane's twin is written through unordered access too.
        if colour && let Some(plane) = &twin.plane {
            self.settle(plane, UNORDERED_ACCESS);
        }
        true
    }

    /// Records the pass that draws the twin of the depth plane of `depth`
    /// into the plane, if `twin` has one, with the moves to the states the
    /// pass needs; the twin is left in the unordered access state. Only
    /// for a list that records nothing of the title's afterwards
    /// (`record_plane_pass`).
    fn pass_plane(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const DEPTH_WRITE: u32 = 0x10;
        const PIXEL_SHADER_RESOURCE: u32 = 0x80;
        let Some(plane) = &twin.plane else { return };
        self.settle(depth, DEPTH_WRITE);
        self.settle(plane, PIXEL_SHADER_RESOURCE);
        record_plane_pass(&self.list, &depth.texture, &plane.texture);
        self.settle(plane, UNORDERED_ACCESS);
    }

    /// Records the copy of a depth texture into its colour twin (`colour`)
    /// or of the twin into the depth texture, with the moves to the copy
    /// states. The twin is left in the unordered access state.
    fn twin_copy(
        &self,
        depth: &std::sync::Arc<crate::resource::HostTexture>,
        twin: &crate::resource::Twin,
        colour: bool,
    ) {
        const UNORDERED_ACCESS: u32 = 0x8;
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let (from, to) = if colour {
            (depth, &twin.colour)
        } else {
            (&twin.colour, depth)
        };
        self.settle(from, COPY_SOURCE);
        self.settle(to, COPY_DEST);
        record_twin_copy(&self.list, &depth.texture, twin, colour);
        self.settle(&twin.colour, UNORDERED_ACCESS);
    }

    /// Before the list closes: the depth textures whose twins it wrote last
    /// are given what was written, and left readable by shaders. Shaders
    /// that pick a view from the heap by a computed index are not seen by
    /// `twin_use` and read the depth texture in lists that never name it
    /// (in a captured frame of the title no table names a shadow
    /// map for reading but the pass that fills the next one).
    fn twins_to_depth(&self) {
        const SHADER_RESOURCE: u32 = 0x40 | 0x80;
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        let written: Vec<_> = twins
            .values_mut()
            .filter(|u| u.behind[0] == Some(true) || u.late)
            .map(|u| {
                // Not copied again if the list copied since the last write
                // and only the depth plane's pass is left.
                let copy = u.behind[0] == Some(true);
                u.behind[0] = Some(false);
                u.late = false;
                (u.depth.clone(), u.twin.clone(), copy)
            })
            .collect();
        drop(twins);
        for (depth, twin, copy) in &written {
            if *copy {
                self.twin_copy(depth, twin, false);
            }
            // Nothing of the title's is recorded after this: the pass may
            // leave its own pipeline and target set.
            self.pass_plane(depth, twin);
            self.settle(depth, SHADER_RESOURCE);
        }
    }

    /// A draw or dispatch being prepared writes title texture `resource`
    /// through unordered access: if it is of a kind copied back
    /// ([`crate::texback`]), the list copies it when it closes.
    pub(crate) fn texture_written(&self, resource: crate::object::This) {
        if !crate::texback::wanted(resource) {
            return;
        }
        let mut deferred = self.deferred();
        if !deferred.back_wanted.iter().any(|target| target.owner() == resource as usize)
            && let Some(target) = crate::texback::Target::new(resource)
        {
            deferred.back_wanted.push(target);
        }
    }

    /// Before the list closes: the textures its shaders wrote that are copied
    /// back go into read-back buffers ([`crate::texback::record`]).
    fn textures_back(&self) {
        const COPY_SOURCE: u32 = 0x800;
        let wanted = std::mem::take(&mut self.deferred().back_wanted);
        for target in wanted {
            self.settle(&target.copy, COPY_SOURCE);
            if let Some(back) = crate::texback::record(&self.list, target) {
                self.deferred().texture_backs.push(back);
            }
        }
    }

    /// The draw or dispatch just prepared is recorded: the twins it writes
    /// now hold what their depth textures lack.
    pub(crate) fn twins_written(&self) {
        let mut twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values_mut().filter(|u| u.writing) {
            u.writing = false;
            u.behind[0] = Some(true);
            self.changed_something();
        }
    }

    /// At submission: the list that moves each texture this list uses from
    /// its real state to the one the list starts with; the textures' real
    /// states become those the list ends with. For depth textures with a
    /// colour twin it first copies the side the list uses as it finds it
    /// from the other, if a list submitted earlier left it behind.
    fn prelude(&self, hand: &mut Vec<D3D12_RESOURCE_BARRIER>) -> Option<ID3D12CommandList> {
        const COPY_DEST: u32 = 0x400;
        const COPY_SOURCE: u32 = 0x800;
        let host_state = |state: u32| D3D12_RESOURCE_STATES(state as i32);
        let states = self.states.lock().unwrap_or_else(|e| e.into_inner());
        // The copies (depth texture, twin, whether into the twin); the
        // sides are then as this list leaves them.
        let mut syncs = Vec::new();
        let twins = self.twins.lock().unwrap_or_else(|e| e.into_inner());
        for u in twins.values() {
            let real = u.twin.behind.load(Ordering::Relaxed);
            let mut behind = [real & 1 != 0, real & 2 != 0];
            for side in [0, 1] {
                if u.needs[side] && behind[side] {
                    syncs.push((u.depth.clone(), u.twin.clone(), side == 1));
                    behind = [false; 2];
                }
            }
            let left = [0, 1].map(|side| u.behind[side].unwrap_or(behind[side]));
            let left = u32::from(left[0]) | (u32::from(left[1]) << 1);
            u.twin.behind.store(left, Ordering::Relaxed);
        }
        drop(twins);
        // A copy runs with its textures in the copy states: each moves
        // there from its real state, and from there to the state the list
        // starts with (back to its real state if the list does not use it).
        let mut to_copy = Vec::new();
        let mut barriers = Vec::new();
        let mut copying = std::collections::HashMap::new();
        // (A list of the compute type: what moves a texture out of or into a state of drawing is not for the compute
        // queue, and goes to `hand`, to be made on the drawing queue ahead of everything the submission does: a
        // texture's way out of a drawing state is the first move of a submission of compute lists, `lanes`.)
        let compute = self.compute;
        let drawing = |before: u32, after: u32| compute && (before | after) & DRAWING_STATES != 0;
        for (depth, twin, colour) in &syncs {
            let (from, to) = if *colour {
                (depth, &twin.colour)
            } else {
                (&twin.colour, depth)
            };
            for (texture, state) in [(from, COPY_SOURCE), (to, COPY_DEST)] {
                let key = std::sync::Arc::as_ptr(texture) as usize;
                let real = texture.state.load(Ordering::Relaxed);
                copying.insert(key, state);
                if real != state {
                    let to = transition(&texture.texture, host_state(real), host_state(state));
                    if drawing(real, state) {
                        hand.push(to);
                    } else {
                        to_copy.push(to);
                    }
                    if !states.contains_key(&key) {
                        if drawing(state, real) {
                            // (Not back to a state of drawing from a compute queue: it stays as the copy left it.)
                            texture.state.store(state, Ordering::Relaxed);
                        } else {
                            barriers.push(transition(&texture.texture, host_state(state), host_state(real)));
                        }
                    }
                }
            }
        }
        for (key, (first, last, copy)) in states.iter() {
            let real = copy.state.swap(*last, Ordering::Relaxed);
            let real = copying.get(key).copied().unwrap_or(real);
            if real != *first {
                let barrier = transition(&copy.texture, host_state(real), host_state(*first));
                if drawing(real, *first) {
                    hand.push(barrier);
                } else {
                    barriers.push(barrier);
                }
            }
        }
        if barriers.is_empty() && syncs.is_empty() {
            return None;
        }
        let Some((allocator, list)) = self
            .prelude
            .get_or_init(|| host().and_then(|h| h.make_prelude_of(compute)))
            .as_ref()
        else {
            fail(format_args!("no host list for a state prelude"));
        };
        // SAFETY: the prelude is not executing (every submission is waited
        // for); the textures are live.
        let recorded = unsafe {
            allocator.Reset().and_then(|()| {
                list.Reset(allocator, None)?;
                if !to_copy.is_empty() {
                    list.ResourceBarrier(&to_copy);
                }
                for (depth, twin, colour) in &syncs {
                    record_twin_copy(list, &depth.texture, twin, *colour);
                }
                if !barriers.is_empty() {
                    list.ResourceBarrier(&barriers);
                }
                list.Close()
            })
        };
        if let Err(e) = recorded {
            fail(format_args!("a host state prelude did not record ({e})"));
        }
        list.cast().ok()
    }

    /// Notes what the command about to be recorded does, as a marker
    /// command whose text the list keeps (diagnostics, with device-removed
    /// data on): the report pairs the breadcrumbs' marker commands with the
    /// texts in order and names the commands a fault stopped among.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_op(&self, what: impl FnOnce() -> String) {
        self.note(what, None);
    }

    /// As [`HostList::note_op`], for a copy that reads the host GPU range
    /// `(address, bytes)`: the report places the faulting address in it.
    #[cfg(feature = "diagnostics")]
    pub(crate) fn note_read(&self, what: impl FnOnce() -> String, range: (u64, u64)) {
        self.note(what, Some(range));
    }

    #[cfg(feature = "diagnostics")]
    fn note(&self, what: impl FnOnce() -> String, range: Option<(u64, u64)>) {
        if !dred_on() {
            crate::capture::op(std::ptr::from_ref(self) as usize, what);
            return;
        }
        {
            let started = std::time::Instant::now();
            let at = self.notes.lock().unwrap_or_else(|e| e.into_inner()).len() as u32;
            if let Some((heap, _)) = self.stamps() && at < STAMPS - 1 {
                // SAFETY: an open list and our query heap.
                unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE_TIMESTAMP, at) };
            }
            let what = what();
            crate::capture::op(std::ptr::from_ref(self) as usize, || what.clone());
            let text: Vec<u16> = what.encode_utf16().chain([0]).collect();
            const UNICODE: u32 = 0;
            // SAFETY: an open list; the marker is copied when recorded.
            unsafe {
                self.list.SetMarker(UNICODE, Some(text.as_ptr().cast()), 2 * text.len() as u32)
            };
            self.notes
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .push((what, range));
            crate::copystat::add(crate::copystat::C::NoteNs, started.elapsed().as_nanos() as u64);
        }
    }

    /// Whether the list is of the compute type ([`lanes`]).
    pub(crate) fn is_compute(&self) -> bool {
        self.compute
    }

    /// Whether nothing recorded since the list was opened does any work:
    /// no draw or dispatch (the first keeps its pipeline), no copy, clear,
    /// upload or resolve (they defer a step or move a texture's state), no
    /// bottom-of-pipe write. State set on the list does not count.
    pub(crate) fn untouched(&self) -> bool {
        {
            let deferred = self.deferred();
            if !(deferred.copies.is_empty()
                && deferred.uploads.is_empty()
                && deferred.kept.is_empty()
                && deferred.textures.is_empty()
                && deferred.writes.is_empty()
                && deferred.backs.is_empty()
                && deferred.texture_backs.is_empty())
            {
                return false;
            }
        }
        self.states.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
            && self.twins.lock().unwrap_or_else(|e| e.into_inner()).is_empty()
    }

    /// Notes an indirect execution, while device-removed data is kept
    /// ([`indirects`]).
    #[cfg(feature = "diagnostics")]
    pub(crate) fn ran_indirect(&self, pipeline: usize, name: &str, kind: u32, count: u32, user: &[u32]) {
        if dred_on() || dred_light() {
            self.indirects.lock().unwrap_or_else(|e| e.into_inner()).note(pipeline, name, kind, count, user);
        }
    }

    /// Notes a pipeline the list binds.
    pub(crate) fn used_pipeline(&self, name: &str) {
        let mut pipelines = self.pipelines.lock().unwrap_or_else(|e| e.into_inner());
        if !pipelines.contains(name) {
            pipelines.insert(name.to_owned());
        }
    }

    /// The list binds a pipeline: it and its root signature are kept until
    /// the list is reset, and its name noted, the first time only (a kept
    /// pipeline's address names no other).
    pub(crate) fn bind_pipeline(&self, name: &str, pso: &ID3D12PipelineState, root: &ID3D12RootSignature) {
        {
            let mut deferred = self.deferred();
            if !deferred.bound.insert(pso.as_raw() as usize) {
                return;
            }
            deferred.kept.push(pso.clone().into());
            deferred.kept.push(root.clone().into());
        }
        self.used_pipeline(name);
    }

    /// The values the list's bottom-of-pipe writes put in memory when it has
    /// run, as (address, value).
    pub(crate) fn value_writes(&self) -> Vec<(usize, u64)> {
        let deferred = self.deferred();
        deferred
            .writes
            .iter()
            .filter_map(|&(at, what, _)| match what {
                crate::queue::Written::Value(v) => Some((at, v)),
                _ => None,
            })
            .collect()
    }

    fn deferred(&self) -> std::sync::MutexGuard<'_, Deferred> {
        self.deferred.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Copies `len` bytes from `src` to `dst` when the list is submitted.
    /// The list recorded upscaling `id` (`crate::streamline`).
    pub(crate) fn mark_upscale(&self, id: u64) {
        self.upscale.store(id, Ordering::Relaxed);
    }

    /// The upscaling the list recorded, taken (0 for none).
    pub(crate) fn take_upscale(&self) -> u64 {
        self.upscale.swap(0, Ordering::Relaxed)
    }

    /// The list recorded the copy of the colour without the interface
    /// number `id` (`crate::streamline`, frame generation).
    pub(crate) fn mark_hudless(&self, id: u64) {
        self.hudless.store(id, Ordering::Relaxed);
    }

    /// The copy of the colour without the interface the list recorded, 0 for none.
    pub(crate) fn hudless(&self) -> u64 {
        self.hudless.load(Ordering::Relaxed)
    }

    /// As [`HostList::hudless`], taken.
    pub(crate) fn take_hudless(&self) -> u64 {
        self.hudless.swap(0, Ordering::Relaxed)
    }

    pub(crate) fn defer_copy(&self, dst: *mut u8, src: *const u8, len: usize) {
        let mut deferred = self.deferred();
        let copy_dst = dst as usize;
        deferred.placed_written.retain(|(a, b, _)| *b <= copy_dst || *a >= copy_dst.saturating_add(len));
        COPIES_JOINED[0].fetch_add(1, Ordering::Relaxed);
        // A copy that goes on where the one recorded just before it ended,
        // source and destination both: with `join_copies` the two are one
        // step.
        if let Some(Step::Copy(d, s, n)) = deferred.copies.last_mut()
            && continues((*d, *s, *n), (dst as usize, src as usize, len))
        {
            COPIES_JOINED[1].fetch_add(1, Ordering::Relaxed);
            if join_copies() {
                *n += len;
                return;
            }
        }
        deferred
            .copies
            .push(Step::Copy(dst as usize, src as usize, len));
        deferred.title_written();
        self.changed_something();
    }

    /// Copies `len` bytes of title memory at `src` into the list's upload
    /// memory at `dst` when the list is submitted.
    pub(crate) fn defer_fill(&self, dst: *mut u8, src: *const u8, len: usize) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len, None));
    }

    /// The host address of `len` bytes of scratch memory that hold the title
    /// memory at `src` as of the list's submission: one fill for every ask
    /// until the list next writes title memory.
    pub(crate) fn fill_scratch(&self, src: *const u8, len: u64) -> Option<u64> {
        let key = (src as usize, len as usize);
        let known = self.deferred().filled.get(&key).copied();
        if known.is_some() {
            return known;
        }
        let (gpu, to) = self.scratch(len)?;
        self.defer_fill(to, src, len as usize);
        self.deferred().filled.insert(key, gpu);
        Some(gpu)
    }

    /// As [`HostList::defer_fill`], for upload memory that only `slot` (a
    /// title heap's address, and the slot) shows.
    pub(crate) fn defer_constants(&self, dst: *mut u8, src: *const u8, len: usize, slot: (usize, u32)) {
        self.deferred()
            .copies
            .push(Step::Fill(dst as usize, src as usize, len, Some(card_steps::FillTo::Slot(slot.0, slot.1))));
    }

    /// As [`HostList::defer_fill`], for the beginning of a buffer
    /// [`HostList::upload`] made.
    pub(crate) fn defer_fill_buffer(&self, buffer: &ID3D12Resource, dst: *mut u8, src: *const u8, len: usize) {
        let to = (card_steps::stage() > 1).then(|| card_steps::FillTo::Buffer(buffer.clone()));
        self.deferred().copies.push(Step::Fill(dst as usize, src as usize, len, to));
    }

    /// Brings `[address, address + len)` of a buffer's host copy up to date
    /// when the list is submitted (once per range until a title copy
    /// intervenes), keeping the copy alive until the list is reset.
    pub(crate) fn defer_refresh(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.refreshed.insert(key) {
            deferred
                .copies
                .push(Step::Refresh(shadow.clone(), address, len));
        }
    }

    /// Shaders of the list write `[address, address + len)` of a buffer's
    /// host copy: copied back into title memory once the list has run.
    /// Declared once until the list next writes title memory (but with
    /// `VICTORY_DECLARE_ONCE=0`): the dispatches of a frame declared the
    /// same ranges thousands of times, each a step of its own.
    pub(crate) fn defer_gpu_write(
        &self,
        shadow: &std::sync::Arc<crate::shadow::Shadow>,
        address: usize,
        len: usize,
    ) {
        let mut deferred = self.deferred();
        let key = (std::sync::Arc::as_ptr(shadow) as usize, address, len);
        if deferred.gpu_written.insert(key) || !declare_once() {
            deferred.copies.push(Step::GpuWrite(shadow.clone(), address, len));
            precise::whole(&mut deferred.precise, address, len);
        }
    }

    /// The title's `BeginQuery` on an occlusion heap of ours, recorded where
    /// it was called among the draws. A query already open is not begun twice.
    pub(crate) fn query_begin(&self, heap: &ID3D12QueryHeap, kind: u32, index: u32) {
        let began = self.deferred().queries.begin(heap.as_raw() as usize, heap, kind, index);
        if !began {
            crate::occlusion::not_run();
            return;
        }
        #[cfg(feature = "diagnostics")]
        self.note_op(|| {
            format!("BEGIN QUERY {:#x} type {kind} index {index}", heap.as_raw() as usize)
        });
        // SAFETY: an open list; the heap is kept until the list is reset, the
        // index is inside it (checked by the caller).
        unsafe { self.list.BeginQuery(heap, D3D12_QUERY_TYPE(kind as i32), index) };
    }

    /// The title's `EndQuery`: recorded only if this list began the query (a
    /// list cut between them ended it at the cut).
    pub(crate) fn query_end(&self, heap: &ID3D12QueryHeap, kind: u32, index: u32) {
        if !self.deferred().queries.end(heap.as_raw() as usize, kind, index) {
            crate::occlusion::not_run();
            return;
        }
        #[cfg(feature = "diagnostics")]
        self.note_op(|| {
            format!("END QUERY {:#x} type {kind} index {index}", heap.as_raw() as usize)
        });
        // SAFETY: an open list, a heap kept until it is reset.
        unsafe { self.list.EndQuery(heap, D3D12_QUERY_TYPE(kind as i32), index) };
    }

    /// Keeps `heap` until the list is reset (a resolve names it).
    pub(crate) fn query_keep(&self, heap: &ID3D12QueryHeap) {
        self.deferred().queries.keep(heap.as_raw() as usize, heap);
    }

    /// Ends the queries still open: a list may not close with one.
    fn queries_end(&self) {
        let open = self.deferred().queries.drain();
        for (heap, kind, index) in open {
            // SAFETY: an open list, a heap kept until it is reset.
            unsafe { self.list.EndQuery(&heap, D3D12_QUERY_TYPE(kind as i32), index) };
        }
    }

    /// Occlusion results: once the list has run, the `bytes` of `back` (the
    /// host buffer the list resolved into) go to title memory at `dst`.
    pub(crate) fn defer_occlusion_back(&self, back: ID3D12Resource, dst: *mut u8, bytes: usize) {
        let mut deferred = self.deferred();
        deferred.backs.push((back, dst as usize, [bytes, 1, bytes]));
        deferred.occluded.push((dst as usize, bytes / 8));
    }

    /// A read back: once the list has run, `rows` rows of `row` bytes go
    /// from `buffer` (rows 256-byte pitched, see `PlacedFootprint::host_rows`)
    /// to title memory at `dst`, pitched `pitch`. The buffer lives until
    /// the list is reset.
    pub(crate) fn defer_read_back(&self, buffer: ID3D12Resource, dst: *mut u8, [row, rows, pitch]: [usize; 3]) {
        self.deferred().backs.push((buffer, dst as usize, [row, rows, pitch]));
    }

    /// Stores `what` (`bytes` of it) at `dst` once the list has run.
    pub(crate) fn defer_write(&self, dst: *mut u8, what: crate::queue::Written, bytes: usize) {
        self.deferred().writes.push((dst as usize, what, bytes));
    }

    /// A mapped upload buffer of `size` bytes, alive until the list is
    /// reset.
    ///
    /// shortcut: one buffer per copy; pool them once uploads are frequent.
    pub(crate) fn upload(&self, host: &Host, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        // With [`card_steps`] the card may be the one that fills it.
        let (buffer, data) = if card_steps::stage() > 1 { host.shared_buffer(size)? } else { host.upload_buffer(size)? };
        name(&buffer, || format!("list upload {size:#x}"));
        self.deferred().hold_upload(buffer.clone(), size);
        Some((buffer, data))
    }

    /// `len` bytes of upload memory (256-byte aligned) until the list is
    /// reset: the host GPU address and the CPU pointer.
    pub(crate) fn scratch(&self, len: u64) -> Option<(u64, *mut u8)> {
        let (buffer, at, data) = self.scratch_at(len)?;
        // SAFETY: a live buffer.
        Some((unsafe { buffer.GetGPUVirtualAddress() } + at, data))
    }

    /// [`HostList::scratch`] and the buffer it lies in, for a descriptor
    /// slot to hold: the slot outlives the list's use of it.
    pub(crate) fn scratch_held(&self, len: u64) -> Option<(ID3D12Resource, u64, *mut u8)> {
        let (buffer, at, data) = self.scratch_at(len)?;
        // SAFETY: a live buffer.
        let gpu = unsafe { buffer.GetGPUVirtualAddress() } + at;
        Some((buffer, gpu, data))
    }

    /// [`HostList::scratch`]: the buffer, the offset in it and the CPU
    /// pointer.
    fn scratch_at(&self, len: u64) -> Option<(ID3D12Resource, u64, *mut u8)> {
        let host = host()?;
        let mut s = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        let len = len.max(1).next_multiple_of(256);
        if len > SCRATCH_CHUNK {
            let (buffer, data) = host.upload_buffer(len)?;
            name(&buffer, || format!("list scratch {len:#x}"));
            self.deferred().hold_upload(buffer.clone(), len);
            return Some((buffer, 0, data));
        }
        // The chunk in use while `len` fits in what is left of it, else the
        // next one (a list's first chunk may be a small one).
        while s.current < s.chunks.len() && s.used + len > s.chunks[s.current].1 {
            (s.current, s.used) = (s.current + 1, 0);
        }
        if s.current == s.chunks.len() {
            let size = if scratch_pool() && s.chunks.is_empty() && len <= SCRATCH_FIRST {
                SCRATCH_FIRST
            } else {
                SCRATCH_CHUNK
            };
            let chunk = take_chunk(host, size)?;
            s.chunks.push((chunk, size));
            s.used = 0;
        }
        let (data, buffer) = (s.chunks[s.current].0.data, s.chunks[s.current].0.buffer.clone());
        let at = s.used;
        s.used += len;
        Some((buffer, at, (data + at as usize) as *mut u8))
    }

    /// The bytes of scratch chunks the list holds now (after a reset:
    /// none while lists share their chunks).
    pub(crate) fn scratch_kept(&self) -> u64 {
        let scratch = self.scratch.lock().unwrap_or_else(|e| e.into_inner());
        scratch.chunks.iter().map(|c| c.1).sum()
    }

    /// The host address of the list's indirect-work budget, zeroed at each
    /// submission of the list from the first use after a reset on.
    pub(crate) fn budget(&self) -> Option<u64> {
        let (buffer, data) = self
            .budget
            .get_or_init(|| {
                let (b, d) = host()?.shared_buffer(256)?;
                Some((b, d as usize))
            })
            .as_ref()?;
        let card = self.card_budget.get_or_init(|| self.make_card_budget()).as_ref();
        let mut deferred = self.deferred();
        if !deferred.budget_zeroed {
            deferred.budget_zeroed = true;
            let at = *data;
            // SAFETY: the list's own mapped words, written before it runs.
            // The words after the budget hold what the clamp noted about the
            // list's last run (mesh dispatches: the largest group count
            // asked for, the groups asked in all; the records refused, the
            // largest cost; with the card's copy, as
            // the list's end copied them here): taken, then zeroed.
            let notes = Box::new(move || unsafe {
                let words = at as *mut u32;
                let (largest, all) = (words.add(1).read_volatile(), words.add(2).read_volatile());
                MESH_GROUPS[0].fetch_max(u64::from(largest), Ordering::Relaxed);
                MESH_GROUPS[1].fetch_add(u64::from(all), Ordering::Relaxed);
                let (refused, biggest) = (words.add(3).read_volatile(), words.add(4).read_volatile());
                BUDGET_NOTES[0].fetch_add(u64::from(refused), Ordering::Relaxed);
                BUDGET_NOTES[1].fetch_max(u64::from(biggest), Ordering::Relaxed);
                for k in 0..5 {
                    words.add(k).write_volatile(0);
                }
            });
            deferred.copies.push(Step::Host(notes, (0, 0), relax::Kind::BudgetNotes));
            drop(deferred);
            if let Some(card) = card {
                self.zero_card_budget(card);
            }
        }
        // SAFETY: a live buffer.
        Some(unsafe { card.unwrap_or(buffer).GetGPUVirtualAddress() })
    }

    /// The budget's words in the card's own memory ([`card_budget`]), if
    /// the list can zero them itself (`WriteBufferImmediate`).
    fn make_card_budget(&self) -> Option<ID3D12Resource> {
        if !card_budget() {
            return None;
        }
        self.list.cast::<ID3D12GraphicsCommandList2>().ok()?;
        let buffer = host()?.uav_buffer(256)?;
        name(&buffer, || "indirect budget".to_owned());
        Some(buffer)
    }

    /// Records, before the list's first clamp, the zeroing of the card's
    /// copy of the budget: common, written as a copy, then left for
    /// unordered access until the list closes ([`HostList::card_budget_out`]).
    fn zero_card_budget(&self, card: &ID3D12Resource) {
        let Ok(list2) = self.list.cast::<ID3D12GraphicsCommandList2>() else {
            return;
        };
        // SAFETY: a live buffer.
        let at = unsafe { card.GetGPUVirtualAddress() };
        let zeros = [0u64, 4, 8, 12, 16].map(|offset| D3D12_WRITEBUFFERIMMEDIATE_PARAMETER {
            Dest: at + offset,
            Value: 0,
        });
        // SAFETY: an open list; the buffer is the list's own and in the
        // common state at the start of every recording.
        unsafe {
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_COMMON,
                D3D12_RESOURCE_STATE_COPY_DEST,
            )]);
            list2.WriteBufferImmediate(zeros.len() as u32, zeros.as_ptr(), None);
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_COPY_DEST,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            )]);
        }
    }

    /// Before the list closes, if this recording charged the card's copy
    /// of the budget: its notes on mesh records go to the mapped words
    /// (read at the next submission), and it goes back to common.
    fn card_budget_out(&self) {
        if !self.deferred().budget_zeroed {
            return;
        }
        let (Some(Some(card)), Some(Some((mapped, _)))) =
            (self.card_budget.get(), self.budget.get())
        else {
            return;
        };
        // SAFETY: an open list; both buffers are the list's own. The
        // mapped one is promoted from common to the copy destination.
        unsafe {
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
            )]);
            self.list.CopyBufferRegion(mapped, 4, card, 4, 16);
            self.list.ResourceBarrier(&[transition(
                card,
                D3D12_RESOURCE_STATE_COPY_SOURCE,
                D3D12_RESOURCE_STATE_COMMON,
            )]);
        }
    }

    /// A device-local unordered-access buffer of at least `size` bytes for
    /// clamped indirect arguments, reused across executions and resets of the
    /// list. Smaller buffers replaced mid-list stay alive until
    /// [`HostList::reset`].
    pub(crate) fn indirect_scratch(&self, size: u64) -> Option<ID3D12Resource> {
        let needed = size.max(64 << 10);
        let mut slot = self.indirect_args.lock().unwrap_or_else(|e| e.into_inner());
        if let Some((buffer, cap)) = &*slot
            && *cap >= needed
        {
            return Some(buffer.clone());
        }
        let prev = slot.as_ref().map_or(0, |(_, cap)| *cap);
        let alloc = prev
            .saturating_mul(2)
            .max(needed)
            .next_multiple_of(64 << 10);
        let buffer = host()?.uav_buffer(alloc)?;
        name(&buffer, || format!("indirect arguments {alloc:#x}"));
        let old = slot.replace((buffer.clone(), alloc));
        drop(slot);
        if let Some((old_buf, _)) = old {
            // It ends the list common, as every scratch buffer does.
            self.indirect_common(&old_buf);
            self.keep(&old_buf);
        }
        Some(buffer)
    }

    /// Where the next batch of clamped records goes: a device-local buffer
    /// the list keeps from recording to recording, the batch's place in it
    /// and the state the buffer is in. Every batch of a recording has a place
    /// of its own: the executions before it may still read theirs.
    pub(crate) fn batch_place(&self) -> Option<(ID3D12Resource, u32, D3D12_RESOURCE_STATES)> {
        use crate::indirect::batch::{JOBS, SLOT};
        let mut args = self.batch_args.lock().unwrap_or_else(|e| e.into_inner());
        let (buffer, i) = ((args.1 / BATCHES) as usize, args.1 % BATCHES);
        if buffer == args.0.len() {
            let size = u64::from(BATCHES * JOBS * SLOT);
            let made = host()?.uav_buffer(size)?;
            name(&made, || format!("indirect batches {size:#x}"));
            args.0.push(made);
        }
        args.1 += 1;
        // (Its first batch finds it as the recording before left it:
        // `batches_common`.)
        let state = if i == 0 { D3D12_RESOURCE_STATE_COMMON } else { D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT };
        Some((args.0[buffer].clone(), i * JOBS * SLOT, state))
    }

    /// Before the list closes: the scratch buffers of the batched clamps go
    /// back to the common state, where the next recording expects them.
    fn batches_common(&self) {
        let mut args = self.batch_args.lock().unwrap_or_else(|e| e.into_inner());
        let used = std::mem::take(&mut args.1).div_ceil(BATCHES) as usize;
        let barriers: Vec<D3D12_RESOURCE_BARRIER> = args.0[..used]
            .iter()
            .map(|b| transition(b, D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT, D3D12_RESOURCE_STATE_COMMON))
            .collect();
        if !barriers.is_empty() {
            // SAFETY: an open list and live buffers this recording left in
            // `INDIRECT_ARGUMENT`.
            unsafe { self.list.ResourceBarrier(&barriers) };
        }
    }

    /// Moves `scratch` back to the common state if a clamp left it readable
    /// as indirect arguments ([`HostList::indirect_readable`]): where the
    /// list's first clamp expects it. Done before the list closes and before
    /// a bigger scratch buffer replaces it: one barrier per list (where
    /// there was one per indirect execution), and the list ends as it
    /// begins however it is submitted.
    fn indirect_common(&self, scratch: &ID3D12Resource) {
        {
            let mut deferred = self.deferred();
            if deferred.indirect_readable != scratch.as_raw() as usize {
                return;
            }
            deferred.indirect_readable = 0;
        }
        let barrier = transition(
            scratch,
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT,
            D3D12_RESOURCE_STATE_COMMON,
        );
        // SAFETY: an open list and a live buffer this recording left in
        // `INDIRECT_ARGUMENT`.
        unsafe { self.list.ResourceBarrier(&[barrier]) };
    }

    /// The state the list's indirect scratch buffer `scratch` is in at this
    /// point of the recording: indirect arguments if a clamp in this
    /// recording left it so ([`HostList::indirect_readable`]), otherwise
    /// common: the state a buffer is made in, and the one a recording
    /// leaves it in when it closes or replaces it ([`HostList::indirect_common`]).
    pub(crate) fn indirect_state(&self, scratch: &ID3D12Resource) -> D3D12_RESOURCE_STATES {
        if self.deferred().indirect_readable == scratch.as_raw() as usize {
            D3D12_RESOURCE_STATE_INDIRECT_ARGUMENT
        } else {
            D3D12_RESOURCE_STATE_COMMON
        }
    }

    /// A clamp recorded in this list left `scratch` readable as indirect
    /// arguments. It stays so for every execution that reads it, until the
    /// next clamp moves it back to unordered access.
    pub(crate) fn indirect_readable(&self, scratch: &ID3D12Resource) {
        self.deferred().indirect_readable = scratch.as_raw() as usize;
    }

    /// Keeps `object` alive until the list is reset.
    pub(crate) fn keep(&self, object: &impl Interface) {
        if let Ok(object) = object.cast::<windows::core::IUnknown>() {
            self.deferred().kept.push(object);
        }
    }

    /// Whether this is the first time since the reset that the list asks
    /// about `key` (a texture to bring up to date).
    pub(crate) fn first_use(&self, key: usize) -> bool {
        self.deferred().textures.insert(key)
    }

    /// Whether `key` was asked about ([`HostList::first_use`]) since the
    /// reset.
    pub(crate) fn used(&self, key: usize) -> bool {
        self.deferred().textures.contains(&key)
    }

    /// Runs `work` when the list is submitted, in order with its copies.
    /// `work` may write title memory (refreshes after it copy again).
    pub(crate) fn defer(&self, work: impl Fn() + Send + 'static) {
        let mut deferred = self.deferred();
        deferred.copies.push(Step::Run(Box::new(work)));
        deferred.title_written();
        drop(deferred);
        self.changed_something();
    }

    /// As [`HostList::defer`], for work that writes only host memory.
    ///
    /// `kind` says what the work is, for what a thread other than the one that submits may do
    /// ([`relax`], `VICTORY_STAGE_A_RELAX`).
    pub(crate) fn defer_host(&self, kind: relax::Kind, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work), (0, 0), kind));
    }

    /// As [`HostList::defer_host`], for work that reads `len` bytes of title
    /// memory at `from`.
    pub(crate) fn defer_host_read(&self, kind: relax::Kind, from: usize, len: usize, work: impl Fn() + Send + 'static) {
        self.deferred().copies.push(Step::Host(Box::new(work), (from, from + len), kind));
    }

    /// Brings a texture's staging buffer up to date when the list is
    /// submitted, keeping it alive until the list is reset.
    pub(crate) fn defer_texture(&self, staging: std::sync::Arc<crate::texture::Staging>) {
        self.deferred().copies.push(Step::Texture(staging));
    }

    /// A copy source owned exclusively by this list until reset after its
    /// last execution. A different list can prepare its movie plane concurrently.
    pub(crate) fn plane_upload(
        &self,
        size: u64,
        read: (usize, usize),
        fill: impl Fn(usize) + Send + 'static,
    ) -> Option<ID3D12Resource> {
        let chunk = take_chunk(host()?, size)?;
        let source = chunk.buffer.clone();
        let destination = chunk.data;
        let mut deferred = self.deferred();
        deferred.plane_uploads.push((chunk, size));
        deferred.copies.push(Step::Plane(read, Box::new(move || fill(destination))));
        Some(source)
    }

    /// What the list's steps are to a submission's pass ([`onepass::Summary`]): the one kept with the list if it is of
    /// the steps it has, else made now (`lane`: by the thread that submits, counted) and kept.
    fn kept_summary(&self, lane: bool) -> std::sync::Arc<onepass::Summary> {
        let mut deferred = self.deferred();
        if let Some(summary) = &deferred.summary
            && summary.steps == deferred.copies.len()
        {
            return summary.clone();
        }
        if lane {
            onepass::made_on_the_lane();
        }
        let summary = std::sync::Arc::new(onepass::Summary::of(&deferred.copies));
        deferred.summary = Some(summary.clone());
        summary
    }

    /// The list is closed and complete: with the pass over the steps on, what they are to the submission's pass is made
    /// now, by the thread that closed it, not by the submitting thread.
    pub(crate) fn summarize(&self) {
        if onepass::on() && lean_steps() {
            let _ = self.kept_summary(false);
        }
    }

    /// Runs the list's submission-time steps; refreshes go into `batch`
    /// (the title's big buffers, a 174 MB one among them, are read by most
    /// lists of a submission, through many ranges). Copies and fills that
    /// `taken` names (by their place) are left out: they are done on the
    /// card ([`card_steps`]).
    #[inline(never)]
    fn run_deferred(&self, batch: &mut Batch, air: &[(usize, usize)], mut taken: impl FnMut(usize) -> bool, mut cursor: Option<onepass::Cursor<'_>>) {
        #[cfg(feature = "diagnostics")]
        use crate::copystat::{C, add};
        // The time and the bytes of the copies and fills, some 13,000 a
        // frame between 20,000 refreshes: summed here and added once.
        let (mut us, mut bytes) = (std::time::Duration::ZERO, 0u64);
        #[cfg(feature = "diagnostics")]
        let (mut copies, mut refreshes, mut refreshed) = (0u64, 0u64, 0u64);
        let deferred = self.deferred();
        let steps = &deferred.copies;
        // With the pass over the steps ([`onepass`]) the refreshes, which are nearly all of them, come from the records
        // (the steps are not read), the others by their place; the records are of the list as it was when it was looked
        // through.
        if cursor.as_mut().is_some_and(|cursor| !cursor.fits(steps.len())) {
            cursor = None;
        }
        let mut plain = 0..steps.len();
        loop {
            let i = match cursor.as_mut() {
                Some(cursor) => match cursor.next() {
                    onepass::Item::Refresh(number, shadow, from, to) => {
                        #[cfg(feature = "diagnostics")]
                        {
                            refreshes += 1;
                            refreshed += (to - from) as u64;
                        }
                        batch.add_at(number, shadow, from, to - from);
                        continue;
                    }
                    onepass::Item::Step(i) => i,
                    onepass::Item::End => break,
                },
                None => match plain.next() {
                    Some(i) => i,
                    None => break,
                },
            };
            let step = &steps[i];
            if matches!(step, Step::Copy(..) | Step::Fill(..)) && taken(i) {
                continue;
            }
            let (started, copied) = match step {
                &Step::Copy(dst, src, len) => {
                    let started = std::time::Instant::now();
                    #[cfg(feature = "diagnostics")]
                    {
                        copies += 1;
                    }
                    batch.fill();
                    batch.title_written();
                    copy_mapped(dst as *mut u8, src as *const u8, len, true);
                    // With the submission's one asking: the journal learns it.
                    crate::watch::wrote(dst, len);
                    // What the card holds there may be another value although
                    // ours did not change.
                    if send::own() {
                        crate::shadow::send_copied(dst, len);
                    }
                    (started, len)
                }
                &Step::Fill(dst, src, len, _) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        copies += 1;
                    }
                    if wide_fills() {
                        batch.fills.push((dst, src, len));
                        bytes += len as u64;
                        continue;
                    }
                    let started = std::time::Instant::now();
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                    (started, len)
                }
                Step::Placed(p) => {
                    let started = std::time::Instant::now();
                    let free = !touches(air, p.src, p.src + p.len) && !touches(air, p.dst, p.dst + p.len);
                    batch.fill();
                    if free {
                        batch.title_written();
                    }
                    if let Some(bytes) = p.prepare(free) {
                        batch.placed.push((p.shadow.clone(), p.dst, bytes));
                    }
                    (started, p.len)
                }
                Step::Refresh(shadow, address, len) => {
                    #[cfg(feature = "diagnostics")]
                    {
                        refreshes += 1;
                        refreshed += *len as u64;
                    }
                    batch.add(shadow, *address, *len);
                    continue;
                }
                Step::Run(work) | Step::Host(work, ..) => {
                    batch.fill();
                    #[cfg(feature = "diagnostics")]
                    let started = std::time::Instant::now();
                    if matches!(step, Step::Run(_)) {
                        batch.title_written();
                    }
                    let _step = crate::timeline::span("a step of ours");
                    work();
                    // It wrote title memory, where is not known here: the
                    // submission's one asking ends, later refreshes ask anew.
                    if matches!(step, Step::Run(_)) {
                        crate::watch::ask_once_end();
                        crate::watch::end_standing_all();
                    }
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::Runs, 1);
                        add(C::RunUs, started.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::GpuWrite(shadow, address, len) => {
                    // The shaders start from the title's data: this copy's
                    // pending refresh first. The other copies' wait for
                    // the next flush of all (flushing all of them at every
                    // range a dispatch writes made thousands of write-watch
                    // askings a frame).
                    batch.flush_one(shadow);
                    #[cfg(feature = "diagnostics")]
                    let snapshot = std::time::Instant::now();
                    shadow.writing(*address, *len);
                    #[cfg(feature = "diagnostics")]
                    {
                        add(C::GpuWrites, 1);
                        add(C::SnapshotUs, snapshot.elapsed().as_micros() as u64);
                    }
                    continue;
                }
                Step::Texture(staging) => {
                    if batch.texture_due(staging) {
                        batch.due.push(staging.clone());
                        if !wide_textures() {
                            batch.refresh_textures();
                        }
                    }
                    continue;
                }
                Step::Plane((_, len), fill) => {
                    let started = std::time::Instant::now();
                    fill();
                    PREPARING[2].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
                    PREPARING[3].fetch_add(*len as u64, Ordering::Relaxed);
                    continue;
                }
            };
            us += started.elapsed();
            bytes += copied as u64;
        }
        PREPARING[0].fetch_add(us.as_micros() as u64, Ordering::Relaxed);
        PREPARING[1].fetch_add(bytes, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            add(C::Copies, copies);
            add(C::CopyBytes, bytes);
            add(C::Refreshes, refreshes);
            add(C::RefreshBytes, refreshed);
            SPLIT[7].fetch_add(us.as_nanos() as u64, Ordering::Relaxed);
        }
    }
}

/// The numbers of [`HostList::epoch`].
fn next_epoch() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

/// The numbers of `copies` (the address of the copy of each span), grouped by
/// copy, in the order of the first span of each and each group in the
/// order of its spans: with `VICTORY_REFRESH_NARROW` one copy has several spans.
pub(crate) fn spans_by_copy(copies: impl Iterator<Item = usize>) -> Vec<Vec<usize>> {
    let mut at: ByCopy<usize> = ByCopy::default();
    let mut groups: Vec<Vec<usize>> = Vec::new();
    for (i, copy) in copies.enumerate() {
        let group = *at.entry(copy).or_insert_with(|| {
            groups.push(Vec::new());
            groups.len() - 1
        });
        groups[group].push(i);
    }
    groups
}

/// [`crate::shadow::wide_each`] over the spans whose copies are `copies`, a job
/// for each copy: the spans of one copy are done one after the other by the
/// thread that has the copy (they would wait for each other's lock), those of
/// other copies at the same time. `work` gets the number of a span.
pub(crate) fn wide_each_by_copy(copies: impl Iterator<Item = usize>, work: impl Fn(usize) + Send + Sync + 'static) {
    wide_each_by_copy_by(copies, None, work);
}

/// [`wide_each_by_copy`], the copies given to the threads costliest first if `cost` (what each span is expected to take,
/// by its number) is given: the thread that waits for the last one has the shortest left to wait for
/// (`VICTORY_WAIT_ORDER`). The spans are the same ones and those of a copy still run in turn.
pub(crate) fn wide_each_by_copy_by(
    copies: impl Iterator<Item = usize>,
    cost: Option<Vec<u64>>,
    work: impl Fn(usize) + Send + Sync + 'static,
) {
    let mut groups = spans_by_copy(copies);
    if let Some(cost) = &cost {
        costliest_first(&mut groups, cost);
    }
    let _joining = waits::joining(waits::Wait::JoinRefresh);
    crate::shadow::wide_each(groups.len(), move |g| {
        for &i in &groups[g] {
            work(i);
        }
    });
}

/// Whether the copies of a flush's refreshes are given to the threads costliest first (`VICTORY_WAIT_ORDER=1`; off by
/// default): the lane works on the jobs beside the kept threads and then waits for the last one that is being run, so
/// a long job that is taken late is waited for whole (K6: 20 ms of the lane's 77 in 6 frames are the end of such
/// jobs, the longest 2 to 4 ms; with the copies of a flush in the best order the same jobs would end 0.7 ms a frame
/// sooner).
///
/// (May change while the title runs, `VICTORY_AB`: each flush reads it once; which thread takes which copy first is
/// all that differs, the copies are independent and the spans of one are still done in turn.)
fn wait_order() -> bool {
    crate::ab::live_flag!("VICTORY_WAIT_ORDER")
}

/// What refreshing `[from, to)` of `shadow` is expected to take, in microseconds: what its refreshes took lately, or,
/// of a copy not timed yet (its first refresh takes all of the range), the range at about 8 KiB a microsecond.
fn predicted_us(shadow: &crate::shadow::Shadow, from: usize, to: usize) -> u64 {
    match shadow.refresh_cost() {
        0 => (to.saturating_sub(from) >> 13) as u64,
        us => u64::from(us),
    }
}

/// `groups` (of [`spans_by_copy`]) with the costliest first: a group costs what its spans cost (`cost`, by the number
/// of a span). The groups are not changed, and those that cost the same stay in the order they were.
pub(crate) fn costliest_first(groups: &mut [Vec<usize>], cost: &[u64]) {
    groups.sort_by_key(|group| std::cmp::Reverse(group.iter().map(|&i| cost[i]).sum::<u64>()));
}

/// A table by a copy's address.
pub(crate) type ByCopy<T> = std::collections::HashMap<usize, T, std::hash::BuildHasherDefault<AddressHash>>;

/// A hash for addresses of ours: the bits that tell them apart, spread. The
/// tables it serves are asked 40,000 times a frame, and hold no one's keys
/// but our own.
#[derive(Default)]
pub(crate) struct AddressHash(u64);

impl std::hash::Hasher for AddressHash {
    fn finish(&self) -> u64 {
        self.0
    }
    fn write(&mut self, bytes: &[u8]) {
        for &b in bytes {
            self.write_usize(usize::from(b));
        }
    }
    fn write_usize(&mut self, n: usize) {
        let h = (self.0.rotate_left(5) ^ n as u64).wrapping_mul(0x9E37_79B9_7F4A_7C15);
        self.0 = h ^ (h >> 32);
    }
    fn write_u64(&mut self, n: u64) {
        self.write_usize(n as usize);
    }
    fn write_u32(&mut self, n: u32) {
        self.write_usize(n as usize);
    }
}

/// Since the last frame-rate line, nanoseconds of the submitting thread's
/// preparing: the write-watch asking for a submission; the title's bytes put
/// in under flights' writes; the lists' steps, the refreshes among them; the
/// snapshots; what goes to copies in the card's memory; the lists sent; and
/// of the steps: the refreshes, and the copies and fills (those with the
/// refreshes a copy of title memory flushes). Of the asking, which ranges it
/// is of. Of the refreshes, those before ranges are given to shaders, one
/// copy at a time: nanoseconds, and how many. Ahead of all that: the wait for
/// the submission lock, the steps done on the card planned ([`card_steps`]),
/// the flights waited for, the heaps' records and what is swept; and, at the
/// end, the wait for the heaps' records.
#[cfg(feature = "diagnostics")]
static SPLIT: [std::sync::atomic::AtomicU64; 16] = [const { std::sync::atomic::AtomicU64::new(0) }; 16];

/// Since the last frame-rate line: flushes of refreshes, their refreshes,
/// those known to copy nothing, and those done on several threads.
#[cfg(feature = "diagnostics")]
static FLUSHES: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// Counts into [`SPLIT`].
#[cfg(feature = "diagnostics")]
struct Split(std::cell::Cell<std::time::Instant>);

#[cfg(feature = "diagnostics")]
impl Split {
    fn begin() -> Split {
        Split(std::cell::Cell::new(std::time::Instant::now()))
    }

    /// Nanoseconds since the last part.
    fn so_far(&self) -> u64 {
        self.0.get().elapsed().as_nanos() as u64
    }

    /// What came since the last part was part `i`.
    fn part(&self, i: usize) {
        let now = std::time::Instant::now();
        SPLIT[i].fetch_add((now - self.0.replace(now)).as_nanos() as u64, Ordering::Relaxed);
    }
}

/// Copies title memory at submission. The ranges were checked against their
/// resources when recorded, but the title may since have unmapped pages of
/// them (tiled memory, page mappings), which read as zeros here; a title
/// destination (`title_dst`) that is not all mapped is left alone.
///
/// The source is read through the kernel (`ReadProcessMemory`), which
/// reports an unmapped page instead of faulting. Asking `VirtualQuery` first
/// cost about 2 ms per copy in the title's big pools (it walks the page
/// tables to the region's end), which made copies run at 100-200 MB/s.
pub(crate) fn copy_mapped(dst: *mut u8, src: *const u8, len: usize, title_dst: bool) {
    #[cfg(feature = "diagnostics")]
    match crate::copystat::mode() {
        1 => {
            // SAFETY: timing only: the caller vouches for the source.
            unsafe { std::ptr::copy(src, dst, len) };
            return;
        }
        2 => return,
        _ => {}
    }
    if len == 0 {
        return;
    }
    // A title destination that is not all mapped is left alone. Asking
    // Windows first (`VirtualQuery`) walks the page tables to the end of
    // the destination's region: in the title's big blocks that is
    // milliseconds a copy, and it was a seventh of the submitting thread's
    // time in gameplay (run 60: 71 of 500 looks). The kernel's copy below
    // says the same when it fails, so the question is only asked then.
    // `VICTORY_COPY_ASK=1` asks first, as before.
    static ASK: OnceLock<bool> = OnceLock::new();
    let ask_first = title_dst
        && *ASK.get_or_init(|| std::env::var_os("VICTORY_COPY_ASK").is_some_and(|v| v == "1"));
    if ask_first && !crate::texture::committed(dst, len) {
        return;
    }
    if title_dst {
        // The kernel writes the destination: it must not be guarded.
        crate::watch::writing(dst as usize, len);
    }
    #[cfg(feature = "diagnostics")]
    let moved = std::time::Instant::now();
    let since = crate::watch::changes();
    // (A title destination that is away only while the runtime changes a
    // mapping beside it is written when that is done.)
    let whole = read(src, dst, len) || title_dst && crate::watch::again_after_change(since, || read(src, dst, len));
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::MoveUs, moved.elapsed().as_micros() as u64);
    if whole {
        return;
    }
    // The copy failed: a source page is not there, or the destination is
    // not all mapped. Then the destination stays as it is (the kernel may
    // have written its first pages).
    if title_dst && !ask_first && !crate::texture::committed(dst, len) {
        crate::watch::lost("a copy of the title's", dst as usize, len);
        return;
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::Fallbacks, 1);
    #[cfg(feature = "diagnostics")]
    let sparse = std::time::Instant::now();
    if !title_dst && len >= 64 << 10 {
        read_sparse(src, dst, len);
    } else {
        read_pieces(src, dst, len);
    }
    #[cfg(feature = "diagnostics")]
    crate::copystat::add(crate::copystat::C::SparseUs, sparse.elapsed().as_micros() as u64);
}

/// After a failed bulk read into host memory, skip contiguous holes. Never
/// cache the query across copies: the title may remap the same address.
fn read_sparse(src: *const u8, dst: *mut u8, len: usize) {
    use windows_sys::Win32::System::Memory::{
        MEM_COMMIT, MEMORY_BASIC_INFORMATION, PAGE_NOACCESS, VirtualQuery,
    };
    let mut offset = 0;
    while offset < len {
        let at = src.wrapping_add(offset);
        let to = dst.wrapping_add(offset);
        let remaining = len - offset;
        // SAFETY: the query inspects an address and fills a local structure.
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        let size = unsafe { VirtualQuery(at.cast(), &mut info, size_of_val(&info)) };
        let n = (info.BaseAddress as usize).saturating_add(info.RegionSize)
            .saturating_sub(at as usize).min(remaining);
        if size != size_of_val(&info) || n == 0 {
            read_pieces(at, to, remaining);
            break;
        }
        if info.State != MEM_COMMIT || info.Protect == PAGE_NOACCESS {
            #[cfg(feature = "diagnostics")]
            crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
            // SAFETY: this path is only used with a writable host destination.
            unsafe { to.write_bytes(0, n) };
        } else if !read(at, to, n) {
            // A mapping may change between query and read.
            read_pieces(at, to, n);
        }
        offset += n;
    }
}

fn read_pieces(src: *const u8, dst: *mut u8, len: usize) {
    // Something is unmapped: page by page (64 KiB, then 4 KiB in a piece
    // that fails), zeros for what cannot be read.
    for_pieces(src as usize, len, 64 << 10, |at, n| {
        let to = dst.wrapping_add(at - src as usize);
        if !read(at as *const u8, to, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                let to = dst.wrapping_add(at - src as usize);
                if !read(at as *const u8, to, n) {
                    #[cfg(feature = "diagnostics")]
                    crate::copystat::add(crate::copystat::C::Unmapped, n as u64);
                    zero(to, n);
                }
            });
        }
    });
}

/// Zeros at `to`, in one 4 KiB page or two. It may be title memory (the
/// destination of a title's copy), which can go between the question whether
/// it is there and this.
fn zero(to: *mut u8, n: usize) {
    static ZEROS: [u8; 4096] = [0; 4096];
    if n <= ZEROS.len() && crate::safecopy::on() {
        // SAFETY: a destination that is not there fails the copy.
        if !unsafe { crate::safecopy::copy(to, ZEROS.as_ptr(), n) } {
            crate::watch::lost("zeros", to as usize, n);
        }
        return;
    }
    // SAFETY: inside the destination, which is mapped.
    unsafe { to.write_bytes(0, n) };
}

/// Reads title memory at `src` into `dst`; false if a page of it is not
/// readable.
pub(crate) fn read_into(src: *const u8, dst: &mut [u8]) -> bool {
    read(src, dst.as_mut_ptr(), dst.len())
}

/// Reads what is there of the `dst.len()` bytes of title memory at `src`,
/// zeros for the rest; returns how many bytes were there. Safe while the
/// runtime remaps title memory (the kernel, or a copy that takes the fault,
/// reads). What the runtime's record says is mapped is read in one go each,
/// and by pieces where that fails; memory it has no record of by pieces.
pub(crate) fn read_there(src: *const u8, dst: &mut [u8]) -> usize {
    dst.fill(0);
    let (start, len) = (src as usize, dst.len());
    let mut got = 0;
    let mut runs = crate::inplace::shown_runs(start, len);
    if runs.is_empty() {
        runs.push((start, len));
    }
    for (at, n) in runs {
        let piece = &mut dst[at - start..at - start + n];
        if read_into(at as *const u8, piece) {
            got += n;
            continue;
        }
        // (What failed may have written its first pages.)
        piece.fill(0);
        for_pieces(at, n, 64 << 10, |from, n| {
            let piece = &mut dst[from - start..from - start + n];
            if read_into(from as *const u8, piece) {
                got += n;
                return;
            }
            piece.fill(0);
            for_pieces(from, n, 4 << 10, |from, n| {
                let piece = &mut dst[from - start..from - start + n];
                if read_into(from as *const u8, piece) {
                    got += n;
                } else {
                    piece.fill(0);
                }
            });
        });
    }
    got
}

/// Copies `len` bytes from `src` to `dst` unless a source page is not
/// readable.
fn read(src: *const u8, dst: *mut u8, len: usize) -> bool {
    use windows_sys::Win32::System::Diagnostics::Debug::ReadProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // What of it a landing owes is landed first (never zeros, never what was).
    crate::land::owed::land_range(src as usize, len);
    if crate::safecopy::on() {
        // SAFETY: the destination is mapped writable memory of `len` bytes;
        // a source page that is not there fails the copy.
        return unsafe { crate::safecopy::copy(dst, src, len) };
    }
    let mut done = 0;
    // SAFETY: the destination is mapped writable memory of `len` bytes; the
    // kernel checks the source.
    unsafe { ReadProcessMemory(GetCurrentProcess(), src.cast(), dst.cast(), len, &mut done) != 0 }
}

#[link(name = "ntdll", kind = "raw-dylib")]
unsafe extern "system" {
    /// The kernel's write into a process's memory, `(process, address,
    /// bytes, length, written out)`: an NTSTATUS, negative when a page of
    /// the range is not there or not writable. It changes no protection.
    fn NtWriteVirtualMemory(
        process: *mut std::ffi::c_void,
        base: *mut std::ffi::c_void,
        buffer: *const std::ffi::c_void,
        size: usize,
        written: *mut usize,
    ) -> i32;
}

/// Whether title memory is written with the kernel's own call
/// (`NtWriteVirtualMemory`) and not `WriteProcessMemory`, which first asks
/// what the pages are (as `VirtualQuery` does: about 2 ms a call in the
/// title's big pools, see [`copy_mapped`]): putting back what shaders wrote
/// cost 1.2-1.9 s a frame for 430-800 pieces, 2 ms each whatever their size
/// (run 24). On by default; `VICTORY_DIRECT_WRITES=0` writes as before.
fn direct_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_DIRECT_WRITES").is_none_or(|v| v != "0"))
}

/// Copies host memory into title memory after a submission (what shaders
/// wrote), leaving pages that are not mapped alone.
pub(crate) fn write_mapped(dst: *mut u8, src: *const u8, len: usize) {
    use windows_sys::Win32::System::Diagnostics::Debug::WriteProcessMemory;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let direct = direct_writes();
    let since = crate::watch::changes();
    let write = |at: usize, n: usize| {
        let from = src.wrapping_add(at - dst as usize);
        if crate::safecopy::on() {
            // SAFETY: the source is ours; a destination page that is not
            // there, or not writable, fails the copy.
            return unsafe { crate::safecopy::copy(at as *mut u8, from, n) };
        }
        let mut done = 0;
        if direct {
            // SAFETY: as below; the kernel checks the destination and
            // writes nothing it cannot (the caller opened its guards:
            // `crate::watch::writing`).
            return unsafe {
                NtWriteVirtualMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) >= 0
            };
        }
        // SAFETY: the source is our mapped host copy; the kernel checks the
        // destination.
        unsafe { WriteProcessMemory(GetCurrentProcess(), at as _, from.cast(), n, &mut done) != 0 }
    };
    if len == 0 || write(dst as usize, len) {
        return;
    }
    for_pieces(dst as usize, len, 64 << 10, |at, n| {
        if !write(at, n) {
            for_pieces(at, n, 4 << 10, |at, n| {
                if write(at, n) {
                    return;
                }
                // The kernel's write (`VICTORY_SAFE_COPY=0`) fails where a
                // refresh guarded the block again since the caller opened
                // it; and any where the runtime was changing a mapping.
                crate::watch::writing(at, n);
                if !write(at, n) && !crate::watch::again_after_change(since, || write(at, n)) {
                    crate::watch::lost("a host copy's bytes", at, n);
                }
            });
        }
    });
}

/// Calls `f` with the pieces of `[start, start + len)` cut at multiples of
/// `step`.
fn for_pieces(start: usize, len: usize, step: usize, mut f: impl FnMut(usize, usize)) {
    let end = start + len;
    let mut at = start;
    while at < end {
        let next = ((at / step + 1) * step).min(end);
        f(at, next - at);
        at = next;
    }
}

impl Host {
    /// The queue everything of ours runs on, for a call that must go to it
    /// in order with the lists (`crate::tiles`: a texture's levels are
    /// given memory ahead of the list that uploads them).
    pub(crate) fn queue(&self) -> &ID3D12CommandQueue {
        &self.queue
    }

    /// A buffer in the upload heap, mapped for writing.
    /// A buffer of `size` bytes the card writes and we read (a read back),
    /// in the state copies into it need.
    pub(crate) fn read_back_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_READBACK,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a read-back buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATE_COPY_DEST,
                None,
                &mut buffer,
            )
        };
        match made {
            Ok(()) => buffer,
            Err(e) => {
                tracing::warn!(error = %e, size, "no read-back buffer");
                None
            }
        }
    }

    #[track_caller]
    pub(crate) fn upload_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let at = std::panic::Location::caller();
        MADE[0].fetch_add(1, Ordering::Relaxed);
        MADE[1].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of an upload buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    not_resident(),
                    &desc,
                    D3D12_RESOURCE_STATE_GENERIC_READ,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    resident(&self.device, b)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        #[cfg(feature = "diagnostics")]
        if made.is_ok()
            && let Some(b) = &buffer
        {
            crate::book::made(b, || "an upload buffer".into());
        }
        match made {
            Ok(()) => {
                let buffer = buffer?;
                crate::sites::made(&buffer, crate::sites::UPLOAD, size, data.cast(), at);
                Some((buffer, data.cast()))
            }
            Err(e) => {
                crate::residency::allocation_error(&e);
                tracing::warn!(error = %e, size, "no upload buffer");
                None
            }
        }
    }
}

/// The swap chain formats a presented texture can be copied into: its own
/// family's.
pub(crate) fn swap_format(format: DXGI_FORMAT) -> Option<DXGI_FORMAT> {
    Some(match format.0 {
        27..=32 => DXGI_FORMAT_R8G8B8A8_UNORM,
        87 | 90 | 91 => DXGI_FORMAT_B8G8R8A8_UNORM,
        23..=25 => DXGI_FORMAT_R10G10B10A2_UNORM,
        9..=14 => DXGI_FORMAT_R16G16B16A16_FLOAT,
        _ => return None,
    })
}

const BUFFERS: u32 = 2;

struct Presenter {
    swap_chain: IDXGISwapChain3,
    width: u32,
    height: u32,
    format: DXGI_FORMAT,
    copy: HostList,
    /// With presents sent ahead ([`late_free_present`]): a list for each
    /// present in the air, and the submission it last flew as (it is used
    /// again once that one has finished).
    ahead: Vec<(HostList, u64)>,
    hwnd: HWND,
    meter: Meter,
    composer: Option<crate::compose::Composer>,
    #[cfg(feature = "diagnostics")]
    panel: panel::Panel,
}

/// The frame rate, shown in the window's caption every second and logged
/// every few seconds.
struct Meter {
    since: std::time::Instant,
    frames: u32,
    #[cfg(feature = "diagnostics")]
    copy_stats_frames: u64,
    drawn: u64,
    logged: std::time::Instant,
    /// The caption the window had, once read.
    caption: Option<Vec<u16>>,
}

impl Meter {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Meter {
            since: now,
            frames: 0,
            #[cfg(feature = "diagnostics")]
            copy_stats_frames: 0,
            drawn: crate::draw::drawn(),
            logged: now,
            caption: None,
        }
    }

    /// Whether the next frame counted makes the frame-rate line.
    #[cfg(feature = "diagnostics")]
    fn line_due(&self) -> bool {
        self.since.elapsed() >= std::time::Duration::from_secs(1)
    }

    /// Counts a presented frame.
    fn frame(&mut self, hwnd: HWND) {
        #[cfg(feature = "diagnostics")]
        tracing::debug!(target: "victory_gpu::frame", token = crate::frame::current(), "present");
        #[cfg(feature = "diagnostics")]
        crate::copystat::presented();
        self.frames += 1;
        #[cfg(feature = "diagnostics")]
        {
            self.copy_stats_frames += 1;
        }
        let elapsed = self.since.elapsed();
        if elapsed < std::time::Duration::from_secs(1) {
            return;
        }
        let drawn = crate::draw::drawn();
        let frames = f64::from(self.frames);
        let fps = frames / elapsed.as_secs_f64();
        let ms = elapsed.as_secs_f64() * 1000.0 / frames;
        let draws = (drawn - self.drawn) as f64 / frames;
        (self.since, self.frames, self.drawn) = (std::time::Instant::now(), 0, drawn);
        // Of a frame, ms preparing submissions and ms waiting for the GPU.
        let [cpu, gpu] = [0, 1].map(|i| {
            SUBMISSION_US[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames
        });
        let [copy_ms, copy_mb, texture_ms, texture_mb] = [0, 1, 2, 3].map(|i| {
            let scale = if i % 2 == 0 { 1e3 } else { 1e6 };
            PREPARING[i].swap(0, Ordering::Relaxed) as f64 / scale / frames
        });
        let [back_ms, back_mb] = [0, 1].map(|i| {
            COPY_BACK[i].swap(0, Ordering::Relaxed) as f64 / [1e3, 1e6][i] / frames
        });
        let dropped = crate::draw::take_dropped() as f64 / frames;
        let indirect = crate::list::take_indirect() as f64 / frames;
        let mesh_largest = MESH_GROUPS[0].swap(0, Ordering::Relaxed);
        let mesh_groups = MESH_GROUPS[1].swap(0, Ordering::Relaxed) as f64 / frames;
        let refused = BUDGET_NOTES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        let biggest = BUDGET_NOTES[1].swap(0, Ordering::Relaxed);
        tracing::info!(
            "indirect mesh dispatches: the largest group count asked for {mesh_largest} (run: at most 65535), {mesh_groups:.0} groups asked per frame; {refused:.2} indirect records a frame ran empty for want of their list's budget (2^27), the largest cost of one {biggest} (0: under 2^22)"
        );
        crate::shadow::exact_line(frames);
        crate::descriptor::sync_line(frames);
        late_line(frames);
        crate::queue::behind_line(frames);
        crate::streamline::line(frames);
        card_clock_line(frames, ms);
        spans::line(frames);
        crate::echo::line(frames);
        crate::shadow::back_line(frames);
        crate::pagedrive::line(frames);
        #[cfg(feature = "diagnostics")]
        split_line(frames);
        #[cfg(feature = "diagnostics")]
        {
            let [reset, record, close, execute, lists] = [0, 1, 2, 3, 4].map(|i| SYNC_LIST[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if lists > 0.0 {
                tracing::info!(
                    "the list that brings copies in the card's memory up to date, per frame: {lists:.1} lists, {:.2} ms resetting allocator and list,                      {:.2} recording, {:.2} closing, {:.2} giving it to the queue ({:.0} us a list in all)",
                    reset / 1e6,
                    record / 1e6,
                    close / 1e6,
                    execute / 1e6,
                    (reset + record + close + execute) / 1e3 / lists
                );
            }
        }
        {
            let [passes, pieces, compares] = [0, 1, 2].map(|i| CARD_PASSES[i].swap(0, Ordering::Relaxed) as f64 / frames);
            let gpu_ro_left = crate::shadow::GPU_RO_LEFT.swap(0, Ordering::Relaxed) as f64 / 1e6 / frames;
            if passes + compares + gpu_ro_left > 0.0 {
                tracing::info!(
                    "our passes over copies in the card's memory per frame: {passes:.0} hash passes over {pieces:.0} pieces after \
                     ranges were sent up (VICTORY_CARD_REHASH_RUNS=1 joins them: now {}), {compares:.0} compare passes; \n                     {gpu_ro_left:.1} MB declared written in GPU-read-only memory clipped (VICTORY_GPU_RO_SKIP)",
                    if rehash_runs() { "on" } else { "off" }
                );
            }
        }
        {
            let [own, asking, still, all] =
                [0, 1, 2, 3].map(|i| crate::texture::REFRESH_WAITS[i].swap(0, Ordering::Relaxed) as f64 / 1000.0 / frames);
            if all >= 1.0 {
                tracing::info!(
                    "texture refreshes per frame, all threads: {all:.1} ms, of them {own:.1} ms waiting for a texture another thread \
                     is refreshing, {asking:.1} ms asking the write watch and the guards, {still:.1} ms waiting for title memory to \
                     hold still (the runtime remapping it); the rest is reading, hashing and detiling"
                );
            }
            // New memory waited for ([`resident`], [`mapped`]): their totals since the start are said every
            // 1,000 and 200 calls, which tells nothing of one stretch of the run; the stall of a scene's first
            // seconds is where they cost 3 to 5 ms a call against 0.3 before it (V3d, card at its budget).
            {
                static SAID: [std::sync::atomic::AtomicU64; 4] =
                    [const { std::sync::atomic::AtomicU64::new(0) }; 4];
                let now = [
                    RESIDENT[0].load(Ordering::Relaxed),
                    RESIDENT[1].load(Ordering::Relaxed),
                    MAPPED[0].load(Ordering::Relaxed),
                    MAPPED[1].load(Ordering::Relaxed),
                ];
                let [made, made_us, mapped_n, mapped_us] = std::array::from_fn(|i| {
                    now[i].saturating_sub(SAID[i].swap(now[i], Ordering::Relaxed))
                });
                if made + mapped_n > 0 {
                    let [made_batches, mapped_batches] = [&RESIDENT_BATCHES, &MAPPED_BATCHES]
                        .map(|b| b.swap(0, Ordering::Relaxed) as f64 / frames);
                    tracing::info!(
                        "new card memory waited for per frame, all threads (VICTORY_WAIT_RESIDENT=0: not; VICTORY_RESIDENT_BATCH=0: no batches,                          now {}): {:.1} MakeResident calls in {made_batches:.1} batches {:.1} ms, {:.1} waits for the mapping in {mapped_batches:.1} round trips {:.1} ms",
                        if batch_residency() { "on" } else { "off" },
                        made as f64 / frames,
                        made_us as f64 / 1000.0 / frames,
                        mapped_n as f64 / frames,
                        mapped_us as f64 / 1000.0 / frames
                    );
                }
            }
            let [through, left, filled, short] =
                [0, 1, 2, 3].map(|i| crate::texture::UNLOCKED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if through + left + filled > 0.0 {
                tracing::info!(
                    "texture refreshes that did not wait for title memory to hold still, per frame (VICTORY_TEXTURE_NO_WAIT=1): \
                     {through:.1} read through the kernel, {left:.1} left for the next refresh (a page was not there), \
                     {filled:.1} first fillings from the pages there (VICTORY_TEXTURE_FILL=0: left empty), {short:.1} of them \
                     with less than was mapped (looked at again)"
                );
            }
        }
        {
            let [copies, joined] = [0, 1].map(|i| COPIES_JOINED[i].swap(0, Ordering::Relaxed) as f64 / frames);
            if copies > 0.0 {
                tracing::info!(
                    "title buffer copies recorded per frame: {copies:.0}, of them {joined:.0} continue the one before \
                     (VICTORY_JOIN_COPIES=1 makes those one copy with it: now {})",
                    if join_copies() { "on" } else { "off" }
                );
            }
        }
        // Passes over the heap's buffer views for shaders that pick them by index.
        let picked = crate::draw::HEAP_BUFFER_PASSES.swap(0, Ordering::Relaxed) as f64 / frames;
        let picked_ms =
            crate::draw::HEAP_BUFFER_MICROS.swap(0, Ordering::Relaxed) as f64 / 1e3 / frames;
        // Copies between depth textures and their colour twins, each way.
        let [twin_to_depth, twin_to_colour] =
            [2, 3].map(|i| crate::resource::TWINS[i].swap(0, Ordering::Relaxed) as f64 / frames);
        // Depth planes drawn from their twins.
        let planes = crate::resource::PLANES[0].swap(0, Ordering::Relaxed) as f64 / frames;
        // Copies in the card's memory (`crate::shadow::cards`).
        let card = crate::shadow::card_stats();
        #[cfg(feature = "diagnostics")]
        let slow = crate::watch::slow_asks();
        // Over the same frames as the counts above.
        #[cfg(feature = "diagnostics")]
        let drops = crate::pipeline::take_drops();
        if self.logged.elapsed() >= std::time::Duration::from_secs(5) {
            self.logged = std::time::Instant::now();
            crate::threadcpu::line();
            tracing::info!(
                "placed texture views: {} writes of another view of the texture a slot showed (it kept the old view until the new was made)",
                crate::descriptor::host::KEPT_VIEWS.load(Ordering::Relaxed)
            );
            crate::tiles::status();
            crate::vram::log();
            {
                let [queued, due, away, partly, oldest] =
                    [0, 1, 2, 3, 4].map(|i| crate::draw::HEAP_TEXTURES_QUEUE[i].load(Ordering::Relaxed));
                let [same, parts, tails] =
                    [0, 1, 2].map(|i| crate::draw::HEAP_TEXTURES_PARTLY[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures picked by index, the queue: {queued} slots, {due} of them due (the oldest written {oldest} frames ago), \
                     {away} waiting for their memory, {partly} uploaded in part; since the start: {} slots found uploaded already by \
                     the draws that read them through tables, {} writes of tables' slots (due some frames later), {} slots taken out \
                     as read through a table, {parts} uploads of textures not all mapped ({tails} of them without their first page), \
                     {same} looks that found no more mapped; {} slots made stale by a newer view of other memory there, \
                     {} ranges of memory mapped, unmapped or written under texture views ({} dropped), {} slots made due by them, \
                     {} made slots found unmapped and made null",
                    crate::draw::HEAP_TEXTURES_TABLED.load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[0].load(Ordering::Relaxed),
                    crate::descriptor::host::TABLED_WRITES[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[0].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[1].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[2].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[3].load(Ordering::Relaxed),
                    crate::descriptor::host::STALE[4].load(Ordering::Relaxed)
                );
            }
            if crate::resource::held_only() {
                let [copies, bytes, grown, whole] = [0, 1, 2, 3].map(|i| crate::resource::HELD[i].load(Ordering::Relaxed));
                let [flipped, missed] = [0, 1].map(|i| crate::descriptor::host::FLIPPED[i].load(Ordering::Relaxed));
                tracing::info!(
                    "textures from the first level viewed (VICTORY_HELD_LEVELS=0: whole), since the start: {copies} host copies of the \
                     levels viewed alone ({} MB on the card, {} MB of title memory whole), {grown} replaced by one with more levels, \
                     {flipped} slots made from their new view after its upload had run ({missed} written again first); {} uploads left \
                     out as the host copy held those bytes (VICTORY_TEXTURE_SAME=0: made); {} MB of texture blocks free; \
                     {} looks for textures to let go, {:.2} ms each, the longest {:.1}",
                    bytes >> 20,
                    whole >> 20,
                    crate::draw::HEAP_TEXTURES_SAME.load(Ordering::Relaxed),
                    crate::pool::texture_room() >> 20,
                    crate::resource::SWEPT_US[0].load(Ordering::Relaxed),
                    crate::resource::SWEPT_US[1].load(Ordering::Relaxed) as f64
                        / 1e3
                        / crate::resource::SWEPT_US[0].load(Ordering::Relaxed).max(1) as f64,
                    crate::resource::SWEPT_US[2].load(Ordering::Relaxed) as f64 / 1e3
                );
            }
            {
                let mode = crate::watch::page_writes();
                let [writes, bytes, guarded, plain] = [0, 1, 2, 3].map(|i| crate::watch::WRITTEN[i].load(Ordering::Relaxed));
                let [kept, dropped, slots, uploaded] =
                    [0, 1, 2, 3].map(|i| crate::descriptor::host::PAGE_WRITES[i].load(Ordering::Relaxed));
                tracing::info!(
                    "writes behind the guards, since the start: {writes} DirectStorage writes into physical pages at addresses the                      title had mapped ({} MB); {guarded} guarded 64 KiB blocks of host copies lay under them ({}); {plain} reads to                      plain addresses finished ({}); {kept} ranges under texture views kept for the texture pass ({dropped} dropped);                      {slots} slots of placed textures held their bytes, {uploaded} of them uploaded before the write ({})                      (VICTORY_PAGE_WRITES: unset or guards opens the guarded blocks, 0 counts only, 1, slots, all)",
                    bytes >> 20,
                    if mode & 1 != 0 { "opened, taken again" } else { "left as they were" },
                    if mode & 4 != 0 { "told as well" } else { "counted only" },
                    if mode & 2 != 0 { "made due again" } else { "left as they were" }
                );
            }
            #[cfg(feature = "diagnostics")]
            tracing::info!(
                "write-watch askings of a millisecond or more: {:.1} a frame, {:.1} ms a frame; the longest asking {:.1} ms",
                slow[0] as f64 / frames,
                slow[1] as f64 / 1e3 / frames,
                slow[2] as f64 / 1e3
            );
            if crate::shadow::cards() {
                let per_frame = |i: usize| card[i] as f64 / frames;
                tracing::info!(
                    "host copies in the card's memory: {} copies {} MB, their bytes from before {} MB; per frame: \
                     {:.1} MB sent up in {:.0} ranges, {:.0} pieces compared on the card, {:.0} of them fetched \
                     ({:.1} MB) with {:.1} more waits for the card ({:.1} ms)",
                    card[7],
                    card[8] >> 20,
                    card[9] >> 20,
                    per_frame(0) / 1e6,
                    per_frame(1),
                    per_frame(2),
                    per_frame(3),
                    per_frame(4) / 1e6,
                    per_frame(5),
                    per_frame(6) / 1e3
                );
            }
            tracing::info!(
                "{fps:.1} fps, {ms:.1} ms/frame ({cpu:.1} preparing: copies {copy_ms:.1} ms \
                 {copy_mb:.1} MB, textures {texture_ms:.1} ms {texture_mb:.1} MB; {gpu:.1} on the \
                 GPU; {back_ms:.1} copying back {back_mb:.1} MB), {draws:.0} draws/frame, \
                 dropped {dropped:.0} without a host pipeline ({making} being made) and \
                 {indirect:.0} indirect; heap buffers {picked:.1} passes {picked_ms:.1} ms;                  texture slots written so far: {placed} placed, {objects} of objects; \
                 textures picked by index: {picked_textures} uploaded so far ({picked_mb} MB), \
                 {picked_waiting} waiting, {picked_away} passed over unmapped; depth textures with                  unordered access: {du0} made, bound as depth {du1}x, cleared as depth {du2}x,                  unordered views refused {du3}x; buffer views left null as the host would refuse them: {refused}",
                picked_textures = crate::draw::HEAP_TEXTURES.load(Ordering::Relaxed),
                picked_mb = crate::draw::HEAP_TEXTURE_BYTES.load(Ordering::Relaxed) >> 20,
                picked_waiting = crate::draw::HEAP_TEXTURES_WAITING.load(Ordering::Relaxed),
                picked_away = crate::draw::HEAP_TEXTURES_AWAY.load(Ordering::Relaxed),
                du0 = crate::resource::DEPTH_UNORDERED[0].load(Ordering::Relaxed),
                du1 = crate::resource::DEPTH_UNORDERED[1].load(Ordering::Relaxed),
                du2 = crate::resource::DEPTH_UNORDERED[2].load(Ordering::Relaxed),
                du3 = crate::resource::DEPTH_UNORDERED[3].load(Ordering::Relaxed),
                refused = format!(
                    "{} ({} copies made of views off their buffer's elements); depth twins: {} alive, {} depth textures left without one; twin copies per frame: {twin_to_depth:.1} into depth textures, {twin_to_colour:.1} into twins; depth planes: {planes:.1} drawn from their twins per frame, {} times not given back to a twin, {} times used before the pass",
                    crate::descriptor::host::REFUSED_VIEWS.load(Ordering::Relaxed),
                    crate::shadow::OWN_MADE.load(Ordering::Relaxed),
                    crate::resource::TWINS[0].load(Ordering::Relaxed),
                    crate::resource::TWINS[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[1].load(Ordering::Relaxed),
                    crate::resource::PLANES[2].load(Ordering::Relaxed)
                ),
                making = crate::pipeline::making(),
                placed = crate::descriptor::host::PLACED_TEXTURE_SLOTS.load(Ordering::Relaxed),
                objects = crate::descriptor::host::OBJECT_TEXTURE_SLOTS.load(Ordering::Relaxed)
            );
            #[cfg(feature = "diagnostics")]
            {
                let put = [0, 1, 2, 3].map(|i| crate::shadow::PUT[i].swap(0, Ordering::Relaxed) as f64 / frames);
                tracing::info!(
                    "what shaders wrote put into title memory per frame (VICTORY_EXACT_BACK): {:.0} pieces in {:.0} writes, {:.1} MB; {:.0} pieces joined over gaps",
                    put[0],
                    put[1],
                    put[2] / 1e6,
                    put[3]
                );
                lines_aside(std::mem::take(&mut self.copy_stats_frames) as f64);
                for (why, n) in drops.iter().take(40) {
                    tracing::info!(
                        "dropped {:.1}/frame without a host pipeline: {why}",
                        *n as f64 / frames
                    );
                }
            }
        }
        if self.caption.is_none() {
            // The window's own text, read without a message.
            let mut buffer = vec![0u16; 512];
            // SAFETY: a window handle and a buffer of that length.
            let n = unsafe { InternalGetWindowText(hwnd, &mut buffer) };
            buffer.truncate(n.max(0) as usize);
            self.caption = Some(buffer);
        }
        let Some(caption) = &self.caption else { return };
        // With frame generation showing more frames than are rendered, both rates.
        let text = match crate::streamline::shown_per_frame() {
            1 => format!(" | VIctory {fps:.0} fps {ms:.1} ms/frame {draws:.0} draws/frame"),
            per => format!(" | VIctory {fps:.0} fps (frame gen {:.0} fps) {ms:.1} ms/frame {draws:.0} draws/frame", fps * f64::from(per)),
        };
        let mut text: Vec<u16> = caption.iter().copied().chain(text.encode_utf16()).collect();
        text.push(0);
        // Setting the text waits for the window's thread (a title thread,
        // which may not pump for a while), so a thread of ours does it and
        // presenting never waits; it takes the newest text when free.
        static PENDING: Mutex<Option<(usize, Vec<u16>)>> = Mutex::new(None);
        static SETTER: std::sync::Once = std::sync::Once::new();
        *PENDING.lock().unwrap_or_else(|e| e.into_inner()) = Some((hwnd.0 as usize, text));
        SETTER.call_once(|| {
            let _ = std::thread::Builder::new()
                .name("victory-caption".into())
                .spawn(|| {
                    loop {
                        let next = PENDING.lock().unwrap_or_else(|e| e.into_inner()).take();
                        match next {
                            // SAFETY: a window handle and a NUL-terminated text.
                            Some((hwnd, text)) => unsafe {
                                let _ = SetWindowTextW(HWND(hwnd as *mut _), PCWSTR(text.as_ptr()));
                            },
                            None => std::thread::sleep(std::time::Duration::from_millis(100)),
                        }
                    }
                });
        });
    }
}

impl Host {
    /// Shows `texture`, which is in `state` (a host state) and stays in it,
    /// as frame `frame` (saved if captures are on). `None` if nothing was
    /// shown; else, in diagnostics builds, whether the frame was black.
    #[allow(dead_code)]
    pub(crate) fn present(&self, texture: &ID3D12Resource, state: u32, frame: u64) -> Option<bool> {
        self.present_planes(texture, state, None, frame)
    }

    /// Shows `texture`, optionally alpha-compositing a second `overlay` plane
    /// over it (Series S two-plane presentation), and returns them to their states.
    pub(crate) fn present_planes(
        &self,
        texture: &ID3D12Resource,
        state: u32,
        overlay: Option<(&ID3D12Resource, u32)>,
        frame: u64,
    ) -> Option<bool> {
        // SAFETY: a live resource.
        let desc = unsafe { texture.GetDesc() };
        let planes = overlay.and_then(|(over, over_state)| {
            // SAFETY: a live overlay resource.
            let over_desc = unsafe { over.GetDesc() };
            Some((
                over,
                D3D12_RESOURCE_STATES(over_state as i32),
                crate::compose::view(&desc)?,
                crate::compose::view(&over_desc)?,
            ))
        });
        let format = if planes.is_some() {
            DXGI_FORMAT_R8G8B8A8_UNORM
        } else {
            let Some(format) = swap_format(desc.Format) else {
                tracing::warn!(format = desc.Format.0, "cannot present this format");
                return None;
            };
            format
        };
        let (width, height) = (desc.Width as u32, desc.Height);
        let mut presenter = self.presenter.lock().unwrap_or_else(|e| e.into_inner());
        if presenter.is_none() {
            *presenter = match self.presenter(width, height, format) {
                Ok(p) => Some(p),
                Err(e) => {
                    tracing::warn!(error = %e, "no swap chain");
                    return None;
                }
            };
        }
        let state_u32 = state;
        let p = presenter.as_mut()?;
        if planes.is_some() && p.composer.is_none() {
            match crate::compose::Composer::new(self, format) {
                Ok(c) => p.composer = Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "two-plane composer not made; showing base plane only");
                    drop(presenter);
                    return self.present_planes(texture, state_u32, None, frame);
                }
            }
        }
        if (p.width, p.height, p.format) != (width, height, format) {
            // Presents sent ahead hold back buffers until they have run.
            settle_now(WHY_SYNC);
            crate::streamline::resizing();
            // SAFETY: no back buffer is held; every submission was waited for.
            let resized = unsafe {
                p.swap_chain
                    .ResizeBuffers(BUFFERS, width, height, format, DXGI_SWAP_CHAIN_FLAG(0))
            };
            match resized {
                Ok(()) => {
                    if p.format != format {
                        p.composer = None;
                    }
                    (p.width, p.height, p.format) = (width, height, format);
                    tracing::info!(
                        width,
                        height,
                        format = format.0,
                        frame,
                        "the swap chain's buffers were made again for another size or format"
                    );
                    self.log_video_memory("right after the swap chain's buffers were made again");
                }
                Err(e) => {
                    tracing::warn!(error = %e, width, height, "swap chain not resized");
                    return None;
                }
            }
        }
        if planes.is_some() && p.composer.is_none() {
            match crate::compose::Composer::new(self, format) {
                Ok(c) => p.composer = Some(c),
                Err(e) => {
                    tracing::warn!(error = %e, "two-plane composer not made after resize; showing base plane only");
                    drop(presenter);
                    return self.present_planes(texture, state_u32, None, frame);
                }
            }
        }
        let state = D3D12_RESOURCE_STATES(state_u32 as i32);
        // SAFETY: our swap chain, list and live source texture(s).
        unsafe {
            let index = p.swap_chain.GetCurrentBackBufferIndex();
            let Ok(back) = p.swap_chain.GetBuffer::<ID3D12Resource>(index) else {
                return None;
            };
            // Sent ahead ([`late_free_present`]) unless the frame is to be
            // read back at once (a saved or listed frame): then a list of
            // its own, one whose last flight has finished or a new one.
            #[cfg(feature = "diagnostics")]
            let ab_shot = crate::ab::take_shot();
            #[cfg(feature = "diagnostics")]
            let wanted_frame = crate::capture::wanted(frame);
            #[cfg(feature = "diagnostics")]
            let read_now = wanted_frame || crate::capture::listed(frame) || ab_shot.is_some();
            #[cfg(not(feature = "diagnostics"))]
            let read_now = false;
            let slot = if late_free_present() && !read_now {
                let finished = FINISHED.load(Ordering::Acquire);
                p.ahead.iter().position(|(_, flown)| *flown <= finished).or_else(|| {
                    let list = self.new_list().ok()?;
                    list.close();
                    p.ahead.push((list, 0));
                    Some(p.ahead.len() - 1)
                })
            } else {
                None
            };
            let copy = match slot {
                Some(i) => &p.ahead[i].0,
                None => &p.copy,
            };
            copy.reset();
            copy.keep(texture);
            let list = &copy.list;
            let composed = if let Some((over, over_state, base_fmt, over_fmt)) = planes {
                copy.keep(over);
                if let Some(composer) = p.composer.as_mut() {
                    let mut pre = Vec::with_capacity(3);
                    if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        pre.push(transition(texture, state, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE));
                    }
                    if over_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        pre.push(transition(over, over_state, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE));
                    }
                    pre.push(transition(&back, D3D12_RESOURCE_STATE_PRESENT, D3D12_RESOURCE_STATE_RENDER_TARGET));
                    list.ResourceBarrier(&pre);
                    let compose_slot = slot.map_or(0, |i| i + 1);
                    if let Err(err) = composer.record(
                        self,
                        compose_slot,
                        list,
                        [(texture, base_fmt), (over, over_fmt)],
                        &back,
                        (width, height),
                    ) {
                        copy.close();
                        drop(back);
                        drop(presenter);
                        tracing::warn!(%err, "could not record display composite; presenting the base plane");
                        return self.present_planes(texture, state_u32, None, frame);
                    }
                    static ONCE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);
                    if !ONCE.swap(true, Ordering::Relaxed) {
                        tracing::info!(
                            frame,
                            width,
                            height,
                            base_format = base_fmt.0,
                            overlay_format = over_fmt.0,
                            "display output: compositing two planes (overlay active)"
                        );
                    }
                    let mut post = Vec::with_capacity(3);
                    if state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        post.push(transition(texture, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, state));
                    }
                    if over_state != D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE {
                        post.push(transition(over, D3D12_RESOURCE_STATE_PIXEL_SHADER_RESOURCE, over_state));
                    }
                    post.push(transition(
                        &back,
                        D3D12_RESOURCE_STATE_RENDER_TARGET,
                        D3D12_RESOURCE_STATE_COPY_DEST,
                    ));
                    list.ResourceBarrier(&post);
                    true
                } else {
                    false
                }
            } else {
                false
            };
            if !composed {
                let mut pre = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    pre.push(transition(texture, state, D3D12_RESOURCE_STATE_COPY_SOURCE));
                }
                pre.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_PRESENT,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                ));
                list.ResourceBarrier(&pre);
                list.CopyResource(&back, texture);
            }
            #[cfg(feature = "diagnostics")]
            let capture = (wanted_frame || ab_shot.is_some())
                .then(|| {
                    if composed {
                        list.ResourceBarrier(&[transition(
                            &back,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            D3D12_RESOURCE_STATE_COPY_SOURCE,
                        )]);
                        let res = crate::capture::read_back(&self.device, list, &back);
                        list.ResourceBarrier(&[transition(
                            &back,
                            D3D12_RESOURCE_STATE_COPY_SOURCE,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                        )]);
                        res
                    } else {
                        crate::capture::read_back(&self.device, list, texture)
                    }
                })
                .flatten();
            #[cfg(feature = "diagnostics")]
            let probe = if slot.is_some() || composed {
                None
            } else {
                crate::capture::probe(&self.device, list, texture)
            };
            #[cfg(feature = "diagnostics")]
            let watched = if composed {
                None
            } else {
                crate::popwatch::take(&self.device, list, texture)
            };
            #[cfg(feature = "diagnostics")]
            p.panel.draw(self, copy, &back, (width, height), format);
            #[cfg(not(feature = "diagnostics"))]
            let _ = frame;
            if composed {
                list.ResourceBarrier(&[transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                )]);
            } else {
                let mut post = Vec::with_capacity(2);
                if state != D3D12_RESOURCE_STATE_COPY_SOURCE {
                    post.push(transition(texture, D3D12_RESOURCE_STATE_COPY_SOURCE, state));
                }
                post.push(transition(
                    &back,
                    D3D12_RESOURCE_STATE_COPY_DEST,
                    D3D12_RESOURCE_STATE_PRESENT,
                ));
                list.ResourceBarrier(&post);
            }
            copy.close();
            let flown = match slot {
                Some(i) => {
                    let flown = self.execute_own(copy);
                    p.ahead[i].1 = flown;
                    flown
                }
                None => {
                    self.execute(&[copy]);
                    0
                }
            };
            #[cfg(feature = "diagnostics")]
            if let Some(taken) = watched {
                crate::popwatch::sent(taken, flown, frame);
            }
            #[cfg(not(feature = "diagnostics"))]
            let _ = flown;
            #[cfg(feature = "diagnostics")]
            if let Some((buffer, footprint)) = capture {
                if wanted_frame {
                    crate::capture::save_read_back(frame, &buffer, &footprint);
                }
                // `VICTORY_AB_SHOTS`: the frame of a phase's end, named by the phase and its arm.
                if let Some(name) = &ab_shot {
                    crate::capture::save_read_back_as(name, &buffer, &footprint);
                }
            }
            #[cfg(feature = "diagnostics")]
            crate::replay::record::present(texture, state);
            let mut showing = crate::timeline::span("the host's Present");
            // Frame generation: the frame's markers, constants and tags.
            let presenting = crate::streamline::present_begins(
                frame,
                crate::streamline::Planes {
                    base: (texture, state_u32),
                    overlay: overlay.filter(|_| composed),
                    size: [width, height],
                },
            );
            let called = crate::streamline::present_called(&presenting);
            let _ = p.swap_chain.Present(0, DXGI_PRESENT(0));
            crate::streamline::present_returned(called);
            crate::streamline::present_ends(presenting);
            showing.next("the frame meter");
            #[cfg(feature = "diagnostics")]
            p.panel.frame(p.meter.line_due());
            p.meter.frame(p.hwnd);
            showing.end();
            #[cfg(feature = "diagnostics")]
            return Some(probe.is_some_and(|b| crate::capture::black(&b)));
            #[cfg(not(feature = "diagnostics"))]
            Some(false)
        }
    }

    /// Lets the swap chain and what presents with it go, once everything
    /// sent has run (`victory upscaler-check`: before Streamline is shut
    /// down; nothing presents afterwards).
    pub(crate) fn drop_presenter(&self) {
        settle_now(WHY_SYNC);
        *self.presenter.lock().unwrap_or_else(|e| e.into_inner()) = None;
    }

    fn presenter(
        &self,
        width: u32,
        height: u32,
        format: DXGI_FORMAT,
    ) -> windows::core::Result<Presenter> {
        let hwnd = match title_window() {
            Some(hwnd) => hwnd,
            None => window(width, height)?,
        };
        self.log_video_memory("right before the swap chain is made");
        // SAFETY: plain object creation for our window.
        unsafe {
            let factory: IDXGIFactory2 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
            let desc = DXGI_SWAP_CHAIN_DESC1 {
                Width: width,
                Height: height,
                Format: format,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                BufferUsage: DXGI_USAGE_RENDER_TARGET_OUTPUT,
                BufferCount: BUFFERS,
                SwapEffect: DXGI_SWAP_EFFECT_FLIP_DISCARD,
                ..Default::default()
            };
            // With frame generation, through Streamline's factory proxy and
            // on its queue proxy: the swap chain is its proxy.
            let proxied = crate::streamline::swap_chain_factory(&factory).map(|(proxy, queue)| {
                proxy
                    .CreateSwapChainForHwnd(&queue, hwnd, &desc, None, None)
                    .and_then(|s| s.cast::<IDXGISwapChain3>())
            });
            let swap_chain = match proxied {
                Some(Ok(swap_chain)) => {
                    crate::streamline::swap_chain_made(true, None);
                    // Frame generation must be off across a change between
                    // windowed and full screen (Streamline's guide, 12.0):
                    // the system's own Alt+Enter is not taken for this window.
                    if let Ok(parent) = swap_chain.GetParent::<IDXGIFactory1>() {
                        let _ = parent.MakeWindowAssociation(hwnd, DXGI_MWA_NO_ALT_ENTER);
                    }
                    swap_chain
                }
                Some(Err(e)) => {
                    crate::streamline::swap_chain_made(false, Some(&e));
                    factory.CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?.cast::<IDXGISwapChain3>()?
                }
                // (`VICTORY_STREAMLINE_PRESENT=1`, DLSS without frame
                // generation: Streamline's proxy of it, for its present hook.)
                None => crate::streamline::present_hooked(
                    factory.CreateSwapChainForHwnd(&self.queue, hwnd, &desc, None, None)?.cast::<IDXGISwapChain3>()?,
                ),
            };
            let copy = self.new_list()?;
            copy.close();
            tracing::info!(width, height, format = format.0, "presenting to a window");
            self.log_video_memory("right after the swap chain is made");
            Ok(Presenter {
                swap_chain,
                width,
                height,
                format,
                copy,
                ahead: Vec::new(),
                hwnd,
                meter: Meter::new(),
                composer: None,
                #[cfg(feature = "diagnostics")]
                panel: panel::Panel::new(),
            })
        }
    }
}

/// A visible top-level window of this process: the one the title made, if
/// it made one.
fn title_window() -> Option<HWND> {
    extern "system" fn visit(hwnd: HWND, found: LPARAM) -> windows::core::BOOL {
        let mut process = 0;
        // SAFETY: a window handle from the enumeration; `found` is ours.
        unsafe {
            GetWindowThreadProcessId(hwnd, Some(&mut process));
            if process == windows::Win32::System::Threading::GetCurrentProcessId()
                && IsWindowVisible(hwnd).as_bool()
            {
                *(found.0 as *mut Option<HWND>) = Some(hwnd);
                return false.into();
            }
        }
        true.into()
    }
    let mut found: Option<HWND> = None;
    // SAFETY: the callback only writes `found`, which outlives the call.
    let _ = unsafe { EnumWindows(Some(visit), LPARAM(&raw mut found as isize)) };
    found
}

/// `state` without the bits the host texture's flags forbid: the console
/// lets the title name states its own resources allow (unordered access on
/// a depth target, whose host copy cannot have both).
pub(crate) fn legal_state(texture: &ID3D12Resource, state: u32) -> u32 {
    // SAFETY: a plain query.
    legal_for(unsafe { texture.GetDesc() }.Flags.0 as u32, state)
}

/// [`legal_state`] for a texture with resource flags `flags`.
pub(crate) fn legal_for(flags: u32, state: u32) -> u32 {
    // (flag, set or clear forbids, state bits): render target, unordered
    // access, depth, denied shader resource.
    const FORBIDDEN: [(u32, bool, u32); 4] = [
        (0x1, false, 0x4),
        (0x4, false, 0x8),
        (0x2, false, 0x10 | 0x20),
        (0x8, true, 0x40 | 0x80),
    ];
    FORBIDDEN
        .iter()
        .filter(|&&(flag, deny, _)| (flags & flag != 0) == deny)
        .fold(state, |s, &(_, _, bits)| s & !bits)
}

/// The states only a drawing queue may use: render target, depth write and read, pixel shader resource, stream out,
/// resolve destination and source, shading rate source.
pub(crate) const DRAWING_STATES: u32 = 0x4 | 0x10 | 0x20 | 0x80 | 0x100 | 0x1000 | 0x2000 | 0x100_0000;

/// `state` for a list of the compute type (`VICTORY_ASYNC_COMPUTE=2`): the states of drawing (render target, depth,
/// pixel shader resource, stream out, resolve, shading rate) are not for a compute queue, and what a compute list
/// reads a texture in is the non-pixel shader resource state.
pub(crate) fn compute_legal(state: u32) -> u32 {
    match state & !DRAWING_STATES {
        0 if state != 0 => 0x40,
        rest => rest,
    }
}

impl Host {
    /// The card's memory as the system accounts it to this process, in
    /// bytes: the budget and the use of the card's own memory, then of the
    /// PC's memory shared with the card (where the system moves what does
    /// not fit on the card).
    pub(crate) fn video_memory(&self) -> Option<[u64; 4]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        // The card's budget moved by 256 MB or more since the last look:
        // said at once, with what was in use at the last look, so the log
        // has the moment. (It fell from 11,242 MB to some 10,300 MB with
        // the first world frame in runs 179 to 185, and the memory line,
        // seconds later, was the first to show it. We reserve nothing
        // and make the swap chain once, at the start.)
        static LAST: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];
        let now = [own.Budget, own.CurrentUsage, shared.CurrentUsage];
        let last = [0, 1, 2].map(|i| LAST[i].swap(now[i], Ordering::Relaxed));
        if last[0] != 0 && last[0].abs_diff(own.Budget) >= (256u64 << 20) {
            tracing::info!(
                "the system changed the budget of the card's memory from {} MB to {} MB (frame {}); at the last look {} MB of the card's and {} MB of the PC's shared memory were in use",
                last[0] >> 20,
                own.Budget >> 20,
                crate::frame::current(),
                last[1] >> 20,
                last[2] >> 20
            );
            self.log_video_memory("right after that change");
        }
        Some([own.Budget, own.CurrentUsage, shared.Budget, shared.CurrentUsage])
    }

    /// Both of the system's memory groups for this process, in bytes: for
    /// the card's own memory and then for the PC's memory shared with the
    /// card, the budget, the use, what could still be reserved and what
    /// is reserved (the layer reserves none).
    pub(crate) fn video_memory_groups(&self) -> Option<[[u64; 4]; 2]> {
        let adapter = self.adapter.as_ref()?;
        let mut own = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        let mut shared = DXGI_QUERY_VIDEO_MEMORY_INFO::default();
        // SAFETY: plain queries that fill our structures.
        unsafe {
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_LOCAL, &mut own)
                .ok()?;
            adapter
                .QueryVideoMemoryInfo(0, DXGI_MEMORY_SEGMENT_GROUP_NON_LOCAL, &mut shared)
                .ok()?;
        }
        let all = |m: &DXGI_QUERY_VIDEO_MEMORY_INFO| {
            [m.Budget, m.CurrentUsage, m.AvailableForReservation, m.CurrentReservation]
        };
        Some([all(&own), all(&shared)])
    }

    /// Logs [`Self::video_memory_groups`], with `when` it is said.
    pub(crate) fn log_video_memory(&self, when: &str) {
        let Some([own, shared]) = self.video_memory_groups() else {
            return;
        };
        tracing::info!(
            "video memory as the system counts it, {}: the card's own: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved; the PC's shared with the card: budget {} MB, in use {} MB, {} MB could still be reserved, {} MB reserved (the layer reserves none)",
            when,
            own[0] >> 20,
            own[1] >> 20,
            own[2] >> 20,
            own[3] >> 20,
            shared[0] >> 20,
            shared[1] >> 20,
            shared[2] >> 20,
            shared[3] >> 20
        );
    }

    /// A buffer in system memory that the CPU maps (write-back, so reading
    /// it is fast too) and shaders read and write: for host copies of title
    /// memory that shaders write, which upload memory cannot be. Starts in
    /// the common state (buffers move out of it implicitly).
    #[track_caller]
    pub(crate) fn shared_buffer(&self, size: u64) -> Option<(ID3D12Resource, *mut u8)> {
        let at = std::panic::Location::caller();
        MADE[2].fetch_add(1, Ordering::Relaxed);
        MADE[3].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_CUSTOM,
            CPUPageProperty: D3D12_CPU_PAGE_PROPERTY_WRITE_BACK,
            MemoryPoolPreference: D3D12_MEMORY_POOL_L0,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    not_resident(),
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    resident(&self.device, b)?;
                    mapped(&self.device)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        #[cfg(feature = "diagnostics")]
        if made.is_ok()
            && let Some(b) = &buffer
        {
            crate::book::made(b, || "a shared buffer".into());
        }
        match made {
            Ok(()) => {
                let buffer = buffer?;
                crate::sites::made(&buffer, crate::sites::SHARED, size, data.cast(), at);
                Some((buffer, data.cast()))
            }
            Err(e) => {
                tracing::warn!(error = %e, size, "no shared buffer");
                None
            }
        }
    }

    /// Whether buffers can lie in the card's memory and be mapped by the CPU
    /// (`D3D12_HEAP_TYPE_GPU_UPLOAD`: a resizable BAR, and a system and driver
    /// that offer it); logged once.
    pub(crate) fn mapped_card_memory(&self) -> bool {
        static THERE: OnceLock<bool> = OnceLock::new();
        *THERE.get_or_init(|| {
            let mut options = D3D12_FEATURE_DATA_D3D12_OPTIONS16::default();
            // SAFETY: writes `options` of the given size.
            let ok = unsafe {
                self.device
                    .CheckFeatureSupport(
                        D3D12_FEATURE_D3D12_OPTIONS16,
                        (&raw mut options).cast(),
                        size_of_val(&options) as u32,
                    )
                    .is_ok()
            };
            let supported = ok && options.GPUUploadHeapSupported.as_bool();
            tracing::info!(supported, "card memory the CPU maps");
            supported
        })
    }

    /// A buffer in the card's own memory that the CPU maps, shaders read and
    /// (with `written`) may write. The CPU should only write it: a read of the
    /// mapping crosses the bus a few bytes at a time.
    #[track_caller]
    pub(crate) fn mapped_card_buffer(&self, size: u64, written: bool) -> Option<(ID3D12Resource, *mut u8)> {
        let at = std::panic::Location::caller();
        if !self.mapped_card_memory() {
            return None;
        }
        MADE[4].fetch_add(1, Ordering::Relaxed);
        MADE[5].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_GPU_UPLOAD,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: if written {
                D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS
            } else {
                D3D12_RESOURCE_FLAG_NONE
            },
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        let mut data = std::ptr::null_mut();
        // SAFETY: plain creation and mapping of a CPU-visible buffer.
        let made = unsafe {
            self.device
                .CreateCommittedResource(
                    &heap,
                    not_resident(),
                    &desc,
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut buffer,
                )
                .and_then(|()| {
                    let b = buffer.as_ref().ok_or_else(windows::core::Error::empty)?;
                    resident(&self.device, b)?;
                    mapped(&self.device)?;
                    b.Map(0, None, Some(&mut data))
                })
        };
        match made {
            Ok(()) => {
                let buffer = buffer?;
                crate::vram::census::made(&buffer, crate::vram::census::MAPPED, size, at);
                Some((buffer, data.cast()))
            }
            Err(e) => {
                tracing::warn!(error = %e, size, "no buffer in card memory the CPU maps");
                None
            }
        }
    }

    /// A device-local buffer shaders may write (unordered access) and indirect
    /// execution may read, starting in the common state.
    #[track_caller]
    pub(crate) fn uav_buffer(&self, size: u64) -> Option<ID3D12Resource> {
        let at = std::panic::Location::caller();
        MADE[4].fetch_add(1, Ordering::Relaxed);
        MADE[5].fetch_add(size, Ordering::Relaxed);
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
            Width: size.max(1),
            Height: 1,
            DepthOrArraySize: 1,
            MipLevels: 1,
            SampleDesc: DXGI_SAMPLE_DESC {
                Count: 1,
                Quality: 0,
            },
            Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a device-local buffer.
        let made = unsafe {
            self.device.CreateCommittedResource(
                &heap,
                not_resident(),
                &desc,
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut buffer,
            )
        };
        #[cfg(feature = "diagnostics")]
        if let Some(b) = &buffer {
            crate::book::made(b, || "an unordered access buffer".into());
        }
        let made = made.and_then(|()| buffer.as_ref().map_or(Ok(()), |b| resident(&self.device, b))).and_then(|()| mapped(&self.device));
        match made {
            Ok(()) => {
                if let Some(b) = &buffer {
                    crate::vram::prioritize(&self.device, b, size, crate::vram::Class::High);
                    crate::vram::census::made(b, crate::vram::census::CARD, size, at);
                }
                buffer
            }
            Err(e) => {
                tracing::warn!(error = %e, size, "no unordered-access buffer");
                None
            }
        }
    }
}

/// For adapters without device-removed data (the software one): the last
/// commands each list of the failed submission noted (diagnostics, with
/// `VICTORY_D3D12_DRED`).
fn last_notes(runs: &[&HostList]) -> String {
    #[cfg(feature = "diagnostics")]
    {
        let mut text = String::new();
        for (i, r) in runs.iter().enumerate() {
            let notes = r.notes.lock().unwrap_or_else(|e| e.into_inner());
            for (n, _) in notes.iter().rev().take(6).rev() {
                text.push_str(&format!("; list {i} of {}, of its {}: {n}", runs.len(), notes.len()));
            }
        }
        text
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        let _ = runs;
        String::new()
    }
}

/// Where the faulting address `va` lies against a range `(start, bytes)` a
/// command reads (diagnostics).
#[cfg(feature = "diagnostics")]
fn fault_in(va: u64, (start, bytes): (u64, u64)) -> String {
    let end = start + bytes;
    if va < start {
        format!(" (fault {:#x} below its source)", start - va)
    } else if va < end {
        format!(" (fault at source +{:#x})", va - start)
    } else {
        format!(" (fault {:#x} past its source's end)", va - end)
    }
}

/// The commands that may have been running where a list stopped at
/// command `done`: those between the barriers around it (work between
/// barriers overlaps on the GPU), by kind, and what the markers just
/// before them say (`HostList::note_op`: diagnostics builds with
/// device-removed data on).
/// For a list that stopped at command `done`: what the indirect executions
/// there ran, from the one before it to the next barrier (four at most), and
/// what the heap slots their user data names hold now.
#[cfg(feature = "diagnostics")]
fn indirects_around(history: &[D3D12_AUTO_BREADCRUMB_OP], done: usize, all: &indirects::Shared) -> String {
    let indirect = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_EXECUTEINDIRECT;
    let all = all.lock().unwrap_or_else(|e| e.into_inner());
    let mut text = String::new();
    let mut shown = 0;
    let mesh = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_DISPATCHMESH;
    for (at, op) in history.iter().enumerate().skip(done.saturating_sub(1)) {
        if shown == 4 || (at > done && *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER) {
            break;
        }
        if !indirect(op) && !mesh(op) {
            continue;
        }
        shown += 1;
        let direct = mesh(op);
        let nth = if direct { indirects::before(history, at, mesh) } else { indirects::before(history, at, indirect) };
        let Some((what, words)) = all.nth(nth, direct) else {
            continue;
        };
        text.push_str(&format!("; command {at} ran {what}"));
        for (word, slot, what) in crate::descriptor::host::describe_slots(words) {
            text.push_str(&format!(", word {word} slot {slot}: {what}"));
        }
    }
    text
}

fn in_flight(history: &[D3D12_AUTO_BREADCRUMB_OP], markers: &[(u32, String)], done: usize) -> String {
    let barrier = |op: &D3D12_AUTO_BREADCRUMB_OP| *op == D3D12_AUTO_BREADCRUMB_OP_RESOURCEBARRIER;
    let from = history[..done].iter().rposition(barrier).map_or(0, |b| b + 1);
    let to = history[done..]
        .iter()
        .position(barrier)
        .map_or(history.len(), |b| (done + b).max(done + 1));
    let mut text = format!(", commands {from}..{to} between its barriers:");
    let mut shown = 0;
    // From just before the command the GPU had not finished: the first
    // twelve after the barrier named draws long done when a list of 79
    // mesh draws stopped at its 45th (run 41).
    for i in from.max(done.saturating_sub(4))..to {
        let op = history[i];
        if op == D3D12_AUTO_BREADCRUMB_OP_SETMARKER || shown == 12 {
            continue;
        }
        shown += 1;
        let marker = i
            .checked_sub(1)
            .filter(|&m| history[m] == D3D12_AUTO_BREADCRUMB_OP_SETMARKER)
            .and_then(|m| markers.iter().find(|(at, _)| *at as usize == m))
            .map_or("no marker", |(_, s)| s.as_str());
        text.push_str(&format!("; command {i} (op {}): {marker}", op.0));
    }
    text
}

pub(crate) fn transition(
    resource: &ID3D12Resource,
    before: D3D12_RESOURCE_STATES,
    after: D3D12_RESOURCE_STATES,
) -> D3D12_RESOURCE_BARRIER {
    D3D12_RESOURCE_BARRIER {
        Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
        Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
        Anonymous: D3D12_RESOURCE_BARRIER_0 {
            Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                // SAFETY: a borrowed pointer; the barrier never releases it.
                pResource: unsafe { std::mem::transmute_copy(resource) },
                Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                StateBefore: before,
                StateAfter: after,
            }),
        },
    }
}

/// A window of the given client size, owned by its own message thread.
fn window(width: u32, height: u32) -> windows::core::Result<HWND> {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("victory-window".into())
        .spawn(move || {
            let made = make_window(width, height);
            let ok = made.is_ok();
            let _ = sender.send(made.map(|h| h.0 as usize));
            if ok {
                pump();
            }
        })
        .map_err(|_| windows::core::Error::empty())?;
    let hwnd = receiver
        .recv()
        .map_err(|_| windows::core::Error::empty())??;
    Ok(HWND(hwnd as *mut _))
}

extern "system" fn window_proc(
    hwnd: HWND,
    message: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    // SAFETY: the default handling for our window.
    unsafe { DefWindowProcW(hwnd, message, wparam, lparam) }
}

fn make_window(width: u32, height: u32) -> windows::core::Result<HWND> {
    const CLASS: PCWSTR = w!("VIctory");
    // SAFETY: registers our class once and creates a window of it.
    unsafe {
        let instance = GetModuleHandleW(None)?;
        let class = WNDCLASSEXW {
            cbSize: size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(window_proc),
            hInstance: instance.into(),
            lpszClassName: CLASS,
            hCursor: LoadCursorW(None, IDC_ARROW)?,
            ..Default::default()
        };
        RegisterClassExW(&class);
        let mut rect = RECT {
            left: 0,
            top: 0,
            right: width as i32,
            bottom: height as i32,
        };
        AdjustWindowRect(&mut rect, WS_OVERLAPPEDWINDOW, false)?;
        CreateWindowExW(
            WINDOW_EX_STYLE(0),
            CLASS,
            w!("VIctory"),
            WS_OVERLAPPEDWINDOW | WS_VISIBLE,
            CW_USEDEFAULT,
            CW_USEDEFAULT,
            rect.right - rect.left,
            rect.bottom - rect.top,
            None,
            None,
            Some(instance.into()),
            None,
        )
    }
}

fn pump() {
    let mut message = MSG::default();
    // SAFETY: the standard message loop of this thread's window.
    unsafe {
        while GetMessageW(&mut message, None, 0, 0).as_bool() {
            let _ = TranslateMessage(&message);
            DispatchMessageW(&message);
        }
    }
}
