//! What the system's video memory manager says about this process, and the
//! moments it changes the budget (VRAM1).
//!
//! `IDXGIAdapter3::QueryVideoMemoryInfo` gives the budget and the use of a
//! memory group and nothing else: not what the system moved out of the card,
//! not how much of ours is in the PC's memory against its will. At the
//! window spot the use the API gives for the card's own memory is 0.3 to
//! 0.6 GB more than the dedicated memory the system's own per-process
//! counter shows, in the processes whose card works 80 to 95 ms a frame and
//! not in those that work 57 to 65 (32 runs, GPUGAP1's follow-up: 0.59). The
//! kernel's statistics (`D3DKMTQueryStatistics`, what Task Manager reads) say
//! it directly, per process and per memory group: the bytes the system
//! demoted, by the priority class of the allocation, the allocations
//! resident in a segment they do not prefer, and how much the system evicted
//! of the process (by class) since the line before.
//!
//! - [`log`]: the line (every five seconds with the vram line;
//!   `VICTORY_VRAM_OS=0`: none).
//! - [`start`]: the system's budget change events (the layer asked for the
//!   event once, a thread counts and says each change of 32 MB or more, the
//!   moment it happens: the vram line only looks every five seconds).

use std::ffi::c_void;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use windows_sys::Wdk::Graphics::Direct3D::*;
use windows_sys::Win32::Foundation::LUID;

const MB: u64 = 1 << 20;

/// Whether the kernel statistics are read (`VICTORY_VRAM_OS=0`: not).
pub(crate) fn on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_VRAM_OS").is_none_or(|v| v != "0"))
}

/// The adapter and the process the statistics are asked for.
struct Ctx {
    luid: LUID,
    /// A handle of this process that the kernel may query (`HANDLE` as a number, for `Sync`).
    process: usize,
}

static CTX: OnceLock<Option<Ctx>> = OnceLock::new();
static FAILED: AtomicU64 = AtomicU64::new(0);

fn ctx() -> Option<&'static Ctx> {
    CTX.get_or_init(|| {
        let host = crate::host::host()?;
        let adapter = host.adapter.as_ref()?;
        let desc = {
            use windows::core::Interface;
            // SAFETY: a plain query of a live adapter.
            unsafe {
                adapter
                    .cast::<windows::Win32::Graphics::Dxgi::IDXGIAdapter>()
                    .ok()?
                    .GetDesc()
            }
            .ok()?
        };
        use windows_sys::Win32::System::Threading::{
            GetCurrentProcessId, OpenProcess, PROCESS_QUERY_INFORMATION,
        };
        // SAFETY: opens a handle of our own process, kept for the run.
        let process = unsafe { OpenProcess(PROCESS_QUERY_INFORMATION, 0, GetCurrentProcessId()) };
        if process.is_null() {
            tracing::warn!(
                "vram os: no handle of this process for the kernel statistics (said once)"
            );
            return None;
        }
        Some(Ctx {
            luid: LUID {
                LowPart: desc.AdapterLuid.LowPart,
                HighPart: desc.AdapterLuid.HighPart,
            },
            process: process as usize,
        })
    })
    .as_ref()
}

/// One kernel statistics query of `kind`; `set` fills the query's own part. `None`: the kernel said no.
#[allow(clippy::field_reassign_with_default)]
fn call(
    ctx: &Ctx,
    kind: i32,
    set: impl FnOnce(&mut D3DKMT_QUERYSTATISTICS),
) -> Option<D3DKMT_QUERYSTATISTICS> {
    let mut q = D3DKMT_QUERYSTATISTICS::default();
    q.Type = kind;
    q.AdapterLuid = ctx.luid;
    q.hProcess = ctx.process as *mut c_void;
    set(&mut q);
    // SAFETY: a plain query of the kernel; it fills `QueryResult` of our structure (passed as a mutable pointer).
    let status = unsafe { D3DKMTQueryStatistics(&raw mut q) };
    (status >= 0).then_some(q)
}

/// The process's use of a memory group, as the kernel counts it, in bytes.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Group {
    pub(crate) budget: u64,
    pub(crate) requested: u64,
    pub(crate) usage: u64,
    /// Bytes the system demoted out of the group, by priority class: minimum, low, normal, high, maximum.
    pub(crate) demoted: [u64; 5],
}

fn group(ctx: &Ctx, which: i32) -> Option<Group> {
    let r = call(ctx, D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP, |q| {
        q.Anonymous.QueryProcessSegmentGroup = which
    })
    .or_else(|| {
        call(ctx, D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT_GROUP2, |q| {
            q.Anonymous.QueryProcessSegmentGroup2 =
                D3DKMT_QUERYSTATISTICS_QUERY_PROCESS_SEGMENT_GROUP2 {
                    PhysicalAdapterIndex: 0,
                    SegmentGroup: which as u16,
                }
        })
    })?;
    // SAFETY: the kernel filled the process segment group member for this type of query.
    let g = unsafe { r.QueryResult.ProcessSegmentGroupInformation };
    Some(Group {
        budget: g.Budget,
        requested: g.Requested,
        usage: g.Usage,
        demoted: g.Demoted,
    })
}

/// A memory segment of the adapter, and what the process has in it.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Segment {
    pub(crate) id: u32,
    /// The aperture (PC memory the card reaches), not the card's own.
    pub(crate) aperture: bool,
    pub(crate) limit: u64,
    pub(crate) committed: u64,
    pub(crate) resident: u64,
    /// Evicted since the start (all processes), and by priority class.
    pub(crate) evicted: u64,
    pub(crate) evicted_by_class: [u64; 5],
    /// The process's own: committed bytes, allocations (count, bytes) resident in a segment they do not
    /// prefer, evicted in the period (count), and bytes evicted to make room for a submission.
    pub(crate) mine_committed: u64,
    pub(crate) off_count: u32,
    pub(crate) off_bytes: u64,
    pub(crate) evicted_in_period: u32,
    pub(crate) evicted_for_prep: u64,
}

fn segments(ctx: &Ctx) -> Vec<Segment> {
    let Some(r) = call(ctx, D3DKMT_QUERYSTATISTICS_ADAPTER, |_| {}) else {
        return Vec::new();
    };
    // SAFETY: the kernel filled the adapter member for this type of query.
    let count = unsafe { r.QueryResult.AdapterInformation.NbSegments };
    let mut out = Vec::new();
    for id in 0..count.min(8) {
        let mut s = Segment {
            id,
            ..Segment::default()
        };
        if let Some(r) = call(ctx, D3DKMT_QUERYSTATISTICS_SEGMENT, |q| {
            q.Anonymous.QuerySegment = D3DKMT_QUERYSTATISTICS_QUERY_SEGMENT { SegmentId: id }
        }) {
            // SAFETY: as above, the segment member.
            let i = unsafe { r.QueryResult.SegmentInformation };
            s.aperture = i.Aperture != 0;
            s.limit = i.CommitLimit;
            s.committed = i.BytesCommitted;
            s.resident = i.BytesResident;
            s.evicted = i.Memory.TotalBytesEvicted;
            s.evicted_by_class = i.TotalBytesEvictedByPriority;
        }
        if let Some(r) = call(ctx, D3DKMT_QUERYSTATISTICS_PROCESS_SEGMENT, |q| {
            q.Anonymous.QueryProcessSegment = D3DKMT_QUERYSTATISTICS_QUERY_SEGMENT { SegmentId: id }
        }) {
            // SAFETY: as above, the process segment member.
            let i = unsafe { r.QueryResult.ProcessSegmentInformation };
            s.mine_committed = i.BytesCommitted;
            s.off_count = i.VideoMemory.AllocsResidentInNonPreferred.Count;
            s.off_bytes = i.VideoMemory.AllocsResidentInNonPreferred.Bytes;
            s.evicted_in_period = i.NbReferencedAllocationEvictedInPeriod;
            s.evicted_for_prep = i.VideoMemory.TotalBytesEvictedDueToPreparation;
        }
        out.push(s);
    }
    out
}

/// What the kernel said at one look.
#[derive(Clone, Debug, Default)]
pub(crate) struct Reading {
    pub(crate) local: Option<Group>,
    pub(crate) shared: Option<Group>,
    pub(crate) segments: Vec<Segment>,
}

/// Looks now. `None` when there is no kernel answer (said once).
pub(crate) fn read() -> Option<Reading> {
    let ctx = ctx()?;
    let local = group(ctx, D3DKMT_MEMORY_SEGMENT_GROUP_LOCAL);
    let shared = group(ctx, D3DKMT_MEMORY_SEGMENT_GROUP_NON_LOCAL);
    if local.is_none() && shared.is_none() {
        if FAILED.fetch_add(1, Ordering::Relaxed) == 0 {
            tracing::warn!(
                "vram os: the kernel gave no statistics of this process's video memory (said once)"
            );
        }
        return None;
    }
    Some(Reading {
        local,
        shared,
        segments: segments(ctx),
    })
}

/// Evictions seen at the line before: all segments' bytes, by class, and of ours for a submission.
static PREVIOUS: Mutex<Option<([u64; 5], u64, u64)>> = Mutex::new(None);

/// What the line says about the classes we set: the bytes asked at each class and the bytes of that class the
/// system demoted (`(set, demoted)`, high then maximum then low).
pub(crate) fn share(set_high: u64, set_max: u64, set_low: u64, r: &Reading) -> [(u64, u64); 3] {
    let d = r.local.map_or([0; 5], |g| g.demoted);
    [(set_high, d[3]), (set_max, d[4]), (set_low, d[1])]
}

/// The line for the vram line's moment: `api_used` and `api_shared` are what `QueryVideoMemoryInfo` said a moment
/// before; `set` are the bytes given the high, maximum and low priority class.
pub(crate) fn log(api_used: u64, api_shared: u64, set: [u64; 3]) {
    if !on() {
        return;
    }
    let Some(r) = read() else {
        return;
    };
    let mb = |v: u64| v / MB;
    let classes = |d: &[u64; 5]| {
        d.iter()
            .map(|v| (v / MB).to_string())
            .collect::<Vec<_>>()
            .join("/")
    };
    let part = |name: &str, g: &Option<Group>| match g {
        Some(g) => format!(
            "{name}: budget {} MB, requested {} MB, usage {} MB, demoted by class (minimum/low/normal/high/maximum) {} MB",
            mb(g.budget),
            mb(g.requested),
            mb(g.usage),
            classes(&g.demoted)
        ),
        None => format!("{name}: no answer"),
    };
    let gap = r
        .local
        .map_or(0i64, |g| (api_used / MB) as i64 - (g.usage / MB) as i64);
    let gap_shared = r
        .shared
        .map_or(0i64, |g| (api_shared / MB) as i64 - (g.usage / MB) as i64);
    // Evictions since the line before.
    let mut by_class = [0u64; 5];
    let mut prep = 0u64;
    let mut total = 0u64;
    for s in &r.segments {
        for (to, from) in by_class.iter_mut().zip(s.evicted_by_class) {
            *to += from;
        }
        prep += s.evicted_for_prep;
        total += s.evicted;
    }
    let (delta, delta_prep, delta_total) = {
        let mut prev = PREVIOUS.lock().unwrap_or_else(|e| e.into_inner());
        let had = prev.replace((by_class, prep, total));
        had.map_or(([0; 5], 0, 0), |(c, p, t)| {
            let mut d = [0u64; 5];
            for i in 0..5 {
                d[i] = by_class[i].saturating_sub(c[i]);
            }
            (d, prep.saturating_sub(p), total.saturating_sub(t))
        })
    };
    let segs = r
        .segments
        .iter()
        .map(|s| {
            format!(
                "{} {}: limit {} MB, committed {} MB, resident {} MB, ours committed {} MB, ours in a segment they do not prefer {} allocations ({} MB), \
                 ours evicted in the period {}",
                s.id,
                if s.aperture { "aperture" } else { "segment" },
                mb(s.limit),
                mb(s.committed),
                mb(s.resident),
                mb(s.mine_committed),
                s.off_count,
                mb(s.off_bytes),
                s.evicted_in_period
            )
        })
        .collect::<Vec<_>>()
        .join("; ");
    let [high, maximum, low] = share(set[0], set[1], set[2], &r);
    tracing::info!(
        "vram os (the kernel's statistics of this process, VICTORY_VRAM_OS=0: none): the card's own: the API says {} MB in use, the kernel {} MB ({} MB more by the API); \
         {}; the PC's memory shared with the card: the API says {} MB, the kernel {} MB ({} MB more by the API); {}; \
         of what we gave priority high ({} MB) the system demoted {} MB, maximum ({} MB) {} MB, low ({} MB) {} MB; \
         evicted since the line before (all processes, by class) {} MB ({} MB), {} MB of ours for a submission's preparation; \
         segments: [{}]; budget change events: {}, the last {} MB",
        mb(api_used),
        mb(r.local.map_or(0, |g| g.usage)),
        gap,
        part("local group", &r.local),
        mb(api_shared),
        mb(r.shared.map_or(0, |g| g.usage)),
        gap_shared,
        part("non-local group", &r.shared),
        mb(high.0),
        mb(high.1),
        mb(maximum.0),
        mb(maximum.1),
        mb(low.0),
        mb(low.1),
        mb(delta_total),
        classes(&delta),
        mb(delta_prep),
        segs,
        EVENTS.load(Ordering::Relaxed),
        LAST_BUDGET.load(Ordering::Relaxed) / MB,
    );
}

/// Budget change events seen, and the budget the last one left (bytes).
static EVENTS: AtomicU64 = AtomicU64::new(0);
static LAST_BUDGET: AtomicU64 = AtomicU64::new(0);

/// Asks the system for an event at every change of the card's budget and
/// starts the thread that counts them (once).
pub(crate) fn start(host: &'static crate::host::Host) {
    use windows::Win32::Foundation::HANDLE;
    use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let Some(adapter) = host.adapter.as_ref() else {
            return;
        };
        // SAFETY: an auto-reset event of ours, never closed (it lives as long as the registration).
        let Ok(event) = (unsafe { CreateEventW(None, false, false, None) }) else {
            return;
        };
        // SAFETY: a live adapter and an event that outlives the registration.
        if let Err(e) = unsafe { adapter.RegisterVideoMemoryBudgetChangeNotificationEvent(event) } {
            tracing::warn!(error = %e, "no budget change events from the system (said once)");
            return;
        }
        let raw = event.0 as usize;
        let spawned = std::thread::Builder::new().name("victory-budget".into()).spawn(move || {
            let event = HANDLE(raw as *mut c_void);
            loop {
                // SAFETY: waits on the event made above.
                unsafe { WaitForSingleObject(event, INFINITE) };
                let Some(host) = crate::host::host() else {
                    continue;
                };
                let Some([own, shared]) = host.video_memory_groups() else {
                    continue;
                };
                let before = LAST_BUDGET.swap(own[0], Ordering::Relaxed);
                let n = EVENTS.fetch_add(1, Ordering::Relaxed) + 1;
                if before == 0 || before.abs_diff(own[0]) >= 32 * MB {
                    tracing::info!(
                        "the system's budget event {n} (frame {}): the card's budget {} MB -> {} MB, in use {} MB, the PC's shared memory in use {} MB",
                        crate::frame::current(),
                        before / MB,
                        own[0] / MB,
                        own[1] / MB,
                        shared[1] / MB
                    );
                }
            }
        });
        if spawned.is_err() {
            tracing::warn!("no thread for the system's budget events (said once)");
        }
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn what_the_system_demoted_is_read_by_the_class_we_gave() {
        let r = Reading {
            local: Some(Group {
                demoted: [MB, 2 * MB, 3 * MB, 4 * MB, 5 * MB],
                ..Group::default()
            }),
            ..Reading::default()
        };
        // High is the fourth class, maximum the fifth, low the second.
        assert_eq!(
            share(100, 200, 300, &r),
            [(100, 4 * MB), (200, 5 * MB), (300, 2 * MB)]
        );
        assert_eq!(
            share(1, 2, 3, &Reading::default()),
            [(1, 0), (2, 0), (3, 0)]
        );
    }
}
