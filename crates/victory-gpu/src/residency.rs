//! Host-memory admission for speculative texture work and reusable upload storage.
use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

const MIB: u64 = 1 << 20;
const GIB: u64 = 1 << 30;
static PRESSURED: AtomicBool = AtomicBool::new(false);
static HEADROOM: AtomicU64 = AtomicU64::new(u64::MAX);

fn memory_status() -> Option<MEMORYSTATUSEX> {
    // SAFETY: the API initializes the fields of a correctly sized structure.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    (unsafe { GlobalMemoryStatusEx(&mut status) } != 0).then_some(status)
}

// The texture sweep may run for seconds inside a single submitted frame.
static NEXT_COMMIT_SAMPLE: AtomicU64 = AtomicU64::new(0);
static AVAILABLE_COMMIT: AtomicU64 = AtomicU64::new(u64::MAX);
static RETRY_UPLOAD_AT: AtomicU64 = AtomicU64::new(0);

pub(crate) fn allocation_error(error: &windows::core::Error) {
    if error.code() == windows::Win32::Foundation::E_OUTOFMEMORY {
        RETRY_UPLOAD_AT.fetch_max(crate::host::coarse_ms().saturating_add(500), Ordering::Relaxed);
    }
}

thread_local! {
    static STAGING_DEFERRED: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

pub(crate) fn note_staging_deferred() {
    STAGING_DEFERRED.set(true);
}

pub(crate) fn take_staging_deferred() -> bool {
    STAGING_DEFERRED.replace(false)
}

pub(crate) fn upload_refused() -> bool {
    crate::host::coarse_ms() < RETRY_UPLOAD_AT.load(Ordering::Relaxed)
}

fn sample_commit(now: u64) -> u64 {
    let next = NEXT_COMMIT_SAMPLE.load(Ordering::Relaxed);
    if now >= next
        && NEXT_COMMIT_SAMPLE
            .compare_exchange(next, now.saturating_add(32), Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
        && let Some(status) = memory_status()
    {
        AVAILABLE_COMMIT.store(status.ullAvailPageFile, Ordering::Relaxed);
    }
    AVAILABLE_COMMIT.load(Ordering::Relaxed)
}

fn has_upload_reserve(commit: u64, bake: bool) -> bool {
    // Keep a protective floor against Rust/game OOM aborts without starving
    // bindless world textures or one-shot ped decoration bakes when steady-state
    // commit headroom sits in the 1-4 GiB range on 32 GiB hosts.
    commit >= if bake { 256 * MIB } else { 512 * MIB }
}

fn has_growth_reserve(commit: u64, bake: bool) -> bool {
    // Growing a fresh 64 MiB upload block for speculative bindless prefetch
    // requires more headroom than reusing slices already committed inside the
    // staging pool.
    commit >= if bake { 256 * MIB } else { 1024 * MIB }
}

/// Leave commit for game allocations and submissions already in flight while
/// still letting bindless textures and one-shot material bakes upload.
pub(crate) fn admit_texture_pass(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_upload_reserve(sample_commit(now), bake)
}

/// Whether the staging allocator may commit a brand-new OS upload buffer
/// (reusing existing free ranges in already-committed pool blocks stays open
/// whenever `admit_texture_pass` is true).
pub(crate) fn allow_staging_growth(bake: bool) -> bool {
    let now = crate::host::coarse_ms();
    if now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) {
        return false;
    }
    has_growth_reserve(sample_commit(now), bake)
}

pub(crate) fn limited_host_ram() -> bool {
    static LIMITED: OnceLock<bool> = OnceLock::new();
    *LIMITED.get_or_init(|| memory_status().is_some_and(|s| crate::lean::total_physical(s.ullTotalPhys) <= 32 * GIB))
}

/// `VICTORY_PRESSURE_LEGACY=1`: the bounds of a PC of 32 GiB ([`physical_bounds`])
/// on every PC.
fn legacy_bounds() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PRESSURE_LEGACY").is_some_and(|v| v == "1"))
}

/// The free physical memory at or below which the pressure state is entered
/// and at or above which it is left, in bytes, for a PC with `total` bytes.
///
/// 3 GiB and 5 GiB suit a PC of 32 GiB or less. On one of 40 GiB with the
/// title at 22 GiB and the user's own programs at 12-14 GiB, the free memory
/// of the cutscene stall (V3a-V3d, four runs) fell to 2.7-3.0 GiB, then
/// stood at 3.3-4.9 GiB for the next six minutes: the state was entered in
/// every run and never left (the exit asks for 5 GiB), and for all that time
/// the texture queue was held to 64 MiB a frame while its backlog of 4,000
/// slots lasted. (`available` memory includes the standby list, which is
/// given back on demand: the system pages only when it is near an end.) The
/// bounds grow with the PC from 2 and 3 GiB at 40 GiB (5 % and 7.5 % of it)
/// to the old ones at 64 GiB.
fn physical_bounds(total: u64, legacy: bool) -> (u64, u64) {
    if legacy || total <= 32 * GIB {
        (3 * GIB, 5 * GIB)
    } else {
        ((total / 20).clamp(2 * GIB, 3 * GIB), (total / 40 * 3).clamp(3 * GIB, 5 * GIB))
    }
}

fn next_pressure(was: bool, physical: u64, commit: u64, (enter, leave): (u64, u64)) -> bool {
    if was {
        physical < leave || commit < 6 * GIB
    } else {
        physical <= enter || commit <= 4 * GIB
    }
}

/// Called by the once-per-second submission sweep. A failed sample keeps the last good state.
pub(crate) fn refresh() {
    let Some(status) = memory_status() else { return };
    let (physical, commit) = (status.ullAvailPhys, status.ullAvailPageFile);
    HEADROOM.store(physical.min(commit), Ordering::Relaxed);
    let was = pressured();
    let bounds = physical_bounds(crate::lean::total_physical(status.ullTotalPhys), legacy_bounds());
    let pressure = next_pressure(was, physical, commit, bounds);
    if pressure != was {
        PRESSURED.store(pressure, Ordering::Relaxed);
        if pressure {
            tracing::warn!(
                pid = std::process::id(),
                frame = crate::frame::current(),
                physical_free_mib = physical >> 20,
                available_commit_mib = commit >> 20,
                buffer_mirror_span_mib = crate::shadow::live_span_bytes() >> 20,
                texture_staging_mib = crate::texture::live_staging_bytes() >> 20,
                spare_scratch_mib = crate::host::ALIVE[4].load(Ordering::Relaxed) >> 20,
                game_filled_gpu_texture_mib = crate::resource::KINDS[1].load(Ordering::Relaxed) >> 20,
                "host memory pressure: limiting texture prefetch and idle upload storage"
            );
        } else {
            tracing::info!(physical_free_mib = physical >> 20, available_commit_mib = commit >> 20, "host memory pressure cleared");
        }
    }
}

pub(crate) fn headroom() -> Option<u64> {
    let bytes = HEADROOM.load(Ordering::Relaxed);
    (bytes != u64::MAX).then_some(bytes)
}

pub(crate) fn pressured() -> bool {
    PRESSURED.load(Ordering::Relaxed)
}

/// True when OS commit headroom is critically low (< 1.5 GiB) or an allocation
/// just failed with `E_OUTOFMEMORY`, requiring emergency throttling.
pub(crate) fn critical_commit() -> bool {
    let now = crate::host::coarse_ms();
    now < RETRY_UPLOAD_AT.load(Ordering::Relaxed) || sample_commit(now) < 1536 * MIB
}

/// A soft per-frame admission limit. A single texture can cross it. The
/// queue retains unserved slots so they can be retried on later frames.
pub(crate) fn prefetch_limit(bytes: u64, pressure: bool, critical: bool, limited: bool, staging: u64) -> u64 {
    if critical {
        bytes.min(16 << 20)
    } else if pressure || (limited && staging >= GIB) {
        bytes.min(64 << 20)
    } else {
        bytes
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bake_and_lookahead_leave_room_for_live_game_allocations() {
        for (commit, bake, lookahead, growth) in [
            (128 * MIB, false, false, false),
            (256 * MIB, true, false, false),
            (512 * MIB, true, true, false),
            (768 * MIB, true, true, false),
            (1024 * MIB, true, true, true),
        ] {
            assert_eq!(has_upload_reserve(commit, true), bake);
            assert_eq!(has_upload_reserve(commit, false), lookahead);
            assert_eq!(has_growth_reserve(commit, false), growth);
        }
    }

    #[test]
    fn either_constraint_enters_pressure_and_both_must_recover() {
        let old = (3 * GIB, 5 * GIB);
        assert!(next_pressure(false, 12 * GIB, 4 * GIB, old));
        assert!(next_pressure(false, 3 * GIB, 16 * GIB, old));
        assert!(next_pressure(true, 4 * GIB, 7 * GIB, old));
        assert!(next_pressure(true, 6 * GIB, 5 * GIB, old));
        assert!(!next_pressure(true, 5 * GIB, 6 * GIB, old));
        assert!(!next_pressure(false, 4 * GIB, 5 * GIB, old));
    }

    #[test]
    fn bounds_follow_the_size_of_the_pc() {
        let old = (3 * GIB, 5 * GIB);
        for total in [8 * GIB, 16 * GIB, 31 * GIB, 32 * GIB] {
            assert_eq!(physical_bounds(total, false), old);
        }
        assert_eq!(physical_bounds(40 * GIB, false), (2 * GIB, 3 * GIB));
        assert_eq!(physical_bounds(40 * GIB - 100 * MIB, false), (2 * GIB, 3 * GIB));
        // 5 % and 7.5 % between those, to a few bytes of rounding.
        let near = |a: u64, b: u64| a.abs_diff(b) < 16;
        let (enter, leave) = physical_bounds(48 * GIB, false);
        assert!(near(enter, 12 * GIB / 5) && near(leave, 18 * GIB / 5));
        let (enter, leave) = physical_bounds(64 * GIB, false);
        assert!(enter == 3 * GIB && near(leave, 24 * GIB / 5));
        assert_eq!(physical_bounds(128 * GIB, false), (3 * GIB, 5 * GIB));
        // The switch gives every PC the old bounds.
        assert_eq!(physical_bounds(40 * GIB, true), old);
        // They never cross: leaving asks for more than entering.
        for total in (33..=256).map(|g| g * GIB) {
            let (enter, leave) = physical_bounds(total, false);
            assert!(enter < leave && enter >= 2 * GIB && leave <= 5 * GIB, "{total}");
        }
    }

    #[test]
    fn a_40_gib_pc_leaves_pressure_at_the_free_memory_the_cutscene_runs_left_it_at() {
        let bounds = physical_bounds(40 * GIB, false);
        // Free memory of run V3d: 2.7 GiB at the stall, then 3.3 to 4.9 GiB.
        let mut state = false;
        let mut seen = Vec::new();
        let frees = [10 * GIB, 5 * GIB, 2 * GIB - 1, 3 * GIB / 2, 3 * GIB + 300 * MIB, 4 * GIB, 4 * GIB + 900 * MIB];
        for free in frees {
            state = next_pressure(state, free, 13 * GIB, bounds);
            seen.push(state);
        }
        assert_eq!(seen, [false, false, true, true, false, false, false]);
        // With the old bounds the state is entered at 3 GiB and stays until 5.
        let old = physical_bounds(40 * GIB, true);
        let mut state = false;
        let frees = [10 * GIB, 2 * GIB + 700 * MIB, 3 * GIB + 300 * MIB, 4 * GIB, 4 * GIB + 900 * MIB];
        for free in frees {
            state = next_pressure(state, free, 13 * GIB, old);
        }
        assert!(state);
    }

    #[test]
    fn prefetch_respects_existing_budgets_and_resumes_after_recovery() {
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, true, GIB - 1), 512 << 20);
        assert_eq!(prefetch_limit(512 << 20, false, false, false, 2 * GIB), 512 << 20);
        assert_eq!(prefetch_limit(0, true, true, false, 0), 0);
        assert_eq!(prefetch_limit(8 << 20, true, true, false, 0), 8 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, false, false, 0), 64 << 20);
        assert_eq!(prefetch_limit(512 << 20, true, true, false, 0), 16 << 20);
        assert_eq!(prefetch_limit(192 << 20, false, false, false, 0), 192 << 20);
    }
}
