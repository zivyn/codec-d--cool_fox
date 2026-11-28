//! Who gets the processors when there are not enough: the thread that prepares and sends the
//! submissions (the lane), the helpers it waits for, and the threads that work ahead of the frame.
//!
//! At the window spot the process keeps 8.5-9 of the 12 logical processors of a 6 core machine busy
//! (the title's own threads 3 of them, ours 3.5-4). The lane is the one thread whose time is the frame's
//! time, and it shares its core with a second thread (SMT) whenever the scheduler puts one there; a
//! thread that translates lists, which no frame waits for, runs at the priority of the title's own
//! threads. Three switches, each read at any instant (`VICTORY_AB=10,VICTORY_LANE_CORE,..`), act on the
//! thread that reads them, at a place where it may change its own state:
//!
//! - `VICTORY_LANE_CORE=1`: the lane runs on one logical processor of one core (not the first) and no
//!   other thread of the process runs on either logical processor of that core (the process's default
//!   CPU sets leave them out), so its sibling stands idle and the lane runs at the speed of a whole core.
//!   The others have 10 logical processors of 12. Off: both sets cleared again, as before.
//! - `VICTORY_LANE_HIGH=1`: the lane's priority is the highest of the process's, above the helpers it
//!   waits for (which are above the title's threads), so it never waits behind one of them for a core.
//! - `VICTORY_LIST_NICE=1` (`=2`): the threads that translate lists run one (two) levels below the title's
//!   threads: no frame waits for the lists at the cost of a processor that the lane or a helper wants, and
//!   what is left over after those and the title's threads goes to them (`VICTORY_THREAD_PRIORITY=2`
//!   did this for the whole run, from the start; this one flips).
//!
//! The title's own threads are never lowered, nor given a processor set of their own; a thread of the title
//! that has an affinity of its own keeps it (a CPU set is a preference inside the affinity: where the two
//! share nothing the set is ignored).

use std::cell::Cell;

/// A logical processor as the system lists them (group 0).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CpuSet {
    pub id: u32,
    pub logical: u8,
    pub core: u8,
}

/// What `VICTORY_LANE_CORE` does: the lane's set, the sets of everything else.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Plan {
    pub lane: u32,
    pub rest: Vec<u32>,
    pub core: u8,
}

/// The core the lane is given and the sets of the others: the core the lane is on now, unless it is the first (where
/// the system's interrupts and deferred work tend to land), then the last. `None` when the machine has fewer than
/// three cores (the others would be left with one).
pub(crate) fn plan(sets: &[CpuSet], here: Option<u8>) -> Option<Plan> {
    let mut cores: Vec<u8> = sets.iter().map(|s| s.core).collect();
    cores.sort_unstable();
    cores.dedup();
    if cores.len() < 3 {
        return None;
    }
    let core = match here {
        Some(logical) => sets.iter().find(|s| s.logical == logical).map(|s| s.core).filter(|c| *c != cores[0]),
        None => None,
    }
    .unwrap_or(*cores.last()?);
    let lane = sets.iter().filter(|s| s.core == core).min_by_key(|s| s.logical)?.id;
    let rest = sets.iter().filter(|s| s.core != core).map(|s| s.id).collect();
    Some(Plan { lane, rest, core })
}

/// The priority of a thread that translates lists under `VICTORY_LIST_NICE=nice`, which was `base` before:
/// below the title's threads by `nice` levels, never above what it was.
pub(crate) fn list_level(base: i32, nice: u32) -> i32 {
    use windows_sys::Win32::System::Threading::{THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST};
    match nice {
        0 => base,
        1 => base.min(THREAD_PRIORITY_BELOW_NORMAL),
        _ => base.min(THREAD_PRIORITY_LOWEST),
    }
}

mod os {
    use super::CpuSet;
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::{
        GetCurrentProcess, GetCurrentProcessorNumber, GetCurrentThread, GetThreadPriority, SetProcessDefaultCpuSets, SetThreadPriority,
        SetThreadSelectedCpuSets,
    };

    /// The system's processor sets of group 0 (none if there is more than one group).
    pub(super) fn cpu_sets() -> Vec<CpuSet> {
        let mut bytes = 0u32;
        // SAFETY: a size query, then the call into a buffer of that size.
        unsafe { GetSystemCpuSetInformation(std::ptr::null_mut(), 0, &mut bytes, GetCurrentProcess(), 0) };
        let mut buffer = vec![0u64; (bytes as usize).div_ceil(8).max(1)];
        // SAFETY: as above; the buffer holds `bytes` bytes, aligned for the records.
        let ok = unsafe { GetSystemCpuSetInformation(buffer.as_mut_ptr().cast(), bytes, &mut bytes, GetCurrentProcess(), 0) };
        let mut sets = Vec::new();
        let mut at = 0usize;
        while ok != 0 && at + size_of::<SYSTEM_CPU_SET_INFORMATION>() <= bytes as usize {
            // SAFETY: a record inside the buffer; each says its own size.
            let info = unsafe { &*(buffer.as_ptr().cast::<u8>().add(at).cast::<SYSTEM_CPU_SET_INFORMATION>()) };
            if info.Size == 0 {
                break;
            }
            // SAFETY: the record's type is CpuSet (the only one there is).
            let set = unsafe { &info.Anonymous.CpuSet };
            if set.Group != 0 {
                return Vec::new();
            }
            sets.push(CpuSet { id: set.Id, logical: set.LogicalProcessorIndex, core: set.CoreIndex });
            at += info.Size as usize;
        }
        sets
    }

    /// The logical processor the caller runs on.
    pub(super) fn here() -> u8 {
        // SAFETY: a plain query.
        unsafe { GetCurrentProcessorNumber() as u8 }
    }

    /// The caller's own sets (none: as the process's).
    pub(super) fn thread_sets(ids: &[u32]) -> bool {
        // SAFETY: this thread's own sets and a list of the system's own ids.
        unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), ids.as_ptr(), ids.len() as u32) != 0 }
    }

    /// What the process's threads without sets of their own run on (none: any).
    pub(super) fn process_sets(ids: &[u32]) -> bool {
        // SAFETY: this process, and a list of the system's own ids.
        unsafe { SetProcessDefaultCpuSets(GetCurrentProcess(), ids.as_ptr(), ids.len() as u32) != 0 }
    }

    pub(super) fn priority() -> i32 {
        // SAFETY: this thread's own.
        unsafe { GetThreadPriority(GetCurrentThread()) }
    }

    pub(super) fn set_priority(level: i32) -> bool {
        // SAFETY: this thread's own.
        unsafe { SetThreadPriority(GetCurrentThread(), level) != 0 }
    }
}

thread_local! {
    /// The lane's thread says so once; only it reads the switches in [`lane_sync`].
    static IS_LANE: Cell<bool> = const { Cell::new(false) };
    /// What [`lane_sync`] has set on this thread: bit 0 the core, bit 1 the priority.
    static LANE_APPLIED: Cell<u8> = const { Cell::new(0) };
    /// What this thread of the list translators ran at before ([`list_sync`]), and what it runs at now.
    static LIST_LEVELS: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
}

/// The calling thread is the lane.
pub(crate) fn this_is_the_lane() {
    IS_LANE.set(true);
}

/// The lane looks at its switches (each submission it serves): sets its priority and its core as they say, and
/// puts both back when they are off again. Nothing but a look at two words on any other thread.
pub(crate) fn lane_sync() {
    if !IS_LANE.get() {
        return;
    }
    let core = u8::from(crate::ab::live_flag!("VICTORY_LANE_CORE"));
    let high = u8::from(crate::ab::live_flag!("VICTORY_LANE_HIGH"));
    let want = core | (high << 1);
    let had = LANE_APPLIED.get();
    if want == had {
        return;
    }
    LANE_APPLIED.set(want);
    {
        use windows_sys::Win32::System::Threading::{THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_HIGHEST};
        if (want ^ had) & 2 != 0 {
            let level = if want & 2 != 0 { THREAD_PRIORITY_HIGHEST } else { THREAD_PRIORITY_ABOVE_NORMAL };
            if !os::set_priority(level) {
                tracing::warn!(error = %std::io::Error::last_os_error(), "the lane's priority could not be set (VICTORY_LANE_HIGH)");
            }
            tracing::info!(level, "the lane's priority (VICTORY_LANE_HIGH)");
        }
        if (want ^ had) & 1 != 0 {
            if want & 1 != 0 {
                match plan(&os::cpu_sets(), Some(os::here())) {
                    Some(plan) if os::thread_sets(&[plan.lane]) && os::process_sets(&plan.rest) => {
                        tracing::info!(core = plan.core, set = plan.lane, others = plan.rest.len(), "the lane is alone on a core, the others on the rest (VICTORY_LANE_CORE)");
                    }
                    Some(plan) => {
                        os::thread_sets(&[]);
                        os::process_sets(&[]);
                        tracing::warn!(core = plan.core, error = %std::io::Error::last_os_error(), "the lane could not be put alone on a core (VICTORY_LANE_CORE)");
                    }
                    None => tracing::info!("fewer than three cores: the lane stays where the system puts it (VICTORY_LANE_CORE)"),
                }
            } else {
                os::thread_sets(&[]);
                os::process_sets(&[]);
                crate::cores::keep_on_fast_cores("victory-submit");
                tracing::info!("the lane and the others are on any core again (VICTORY_LANE_CORE)");
            }
        }
    }
}

/// A thread that translates lists looks at `VICTORY_LIST_NICE` before each recording it takes: its priority is
/// what the switch says, and what it was before when it is off.
pub(crate) fn list_sync() {
    let nice = crate::ab::live_value!("VICTORY_LIST_NICE", u32, |t: Option<&str>| t.and_then(|v| v.trim().parse().ok()).unwrap_or(0));
    let (base, now) = LIST_LEVELS.get().unwrap_or_else(|| {
        let base = os::priority();
        (base, base)
    });
    let want = list_level(base, nice);
    if want != now && !os::set_priority(want) {
        tracing::warn!(error = %std::io::Error::last_os_error(), "a list thread's priority could not be set (VICTORY_LIST_NICE)");
        LIST_LEVELS.set(Some((base, now)));
        return;
    }
    LIST_LEVELS.set(Some((base, want)));
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Twelve logical processors of six cores, as the cores are numbered on the test machine: logical 2k and 2k+1
    /// are core k; ids are not their positions.
    fn six_cores() -> Vec<CpuSet> {
        (0..12u8).map(|l| CpuSet { id: 256 + u32::from(l), logical: l, core: l / 2 }).collect()
    }

    #[test]
    fn the_lane_gets_its_core_and_the_others_the_rest() {
        let sets = six_cores();
        // On core 3 now (logical 7): stays on core 3, on its first logical processor; the others have ten.
        let p = plan(&sets, Some(7)).unwrap();
        assert_eq!((p.core, p.lane), (3, 256 + 6));
        assert_eq!(p.rest.len(), 10);
        assert!(!p.rest.contains(&(256 + 6)) && !p.rest.contains(&(256 + 7)), "neither sibling is left to the others");
        // On the first core (logical 1) or nowhere known: the last core.
        assert_eq!(plan(&sets, Some(1)).unwrap().core, 5);
        assert_eq!(plan(&sets, None).unwrap().core, 5);
        assert_eq!(plan(&sets, Some(40)).unwrap().core, 5);
        assert_eq!(plan(&sets, None).unwrap().lane, 256 + 10);
    }

    #[test]
    fn a_machine_with_too_few_cores_is_left_alone() {
        let four: Vec<CpuSet> = six_cores().into_iter().filter(|s| s.core < 2).collect();
        assert_eq!(plan(&four, Some(2)), None);
        assert_eq!(plan(&[], None), None);
        // Three cores without a second thread each: the lane takes one, the others two.
        let plain: Vec<CpuSet> = (0..3u8).map(|l| CpuSet { id: u32::from(l), logical: l, core: l }).collect();
        let p = plan(&plain, Some(1)).unwrap();
        assert_eq!((p.core, p.lane, p.rest.len()), (1, 1, 2));
    }

    #[test]
    fn the_list_threads_go_below_what_they_were_and_back() {
        use windows_sys::Win32::System::Threading::{THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_LOWEST, THREAD_PRIORITY_NORMAL};
        assert_eq!(list_level(THREAD_PRIORITY_NORMAL, 0), THREAD_PRIORITY_NORMAL);
        assert_eq!(list_level(THREAD_PRIORITY_NORMAL, 1), THREAD_PRIORITY_BELOW_NORMAL);
        assert_eq!(list_level(THREAD_PRIORITY_NORMAL, 2), THREAD_PRIORITY_LOWEST);
        assert_eq!(list_level(THREAD_PRIORITY_NORMAL, 7), THREAD_PRIORITY_LOWEST);
        // Already lower (VICTORY_THREAD_PRIORITY=2): never raised.
        assert_eq!(list_level(THREAD_PRIORITY_LOWEST, 1), THREAD_PRIORITY_LOWEST);
        assert_eq!(list_level(THREAD_PRIORITY_ABOVE_NORMAL, 1), THREAD_PRIORITY_BELOW_NORMAL);
    }

    #[test]
    fn a_thread_takes_the_priority_and_the_set_asked_for_and_gives_them_back() {
        use windows_sys::Win32::System::Threading::{THREAD_PRIORITY_BELOW_NORMAL, THREAD_PRIORITY_NORMAL};
        std::thread::spawn(|| {
            assert_eq!(os::priority(), THREAD_PRIORITY_NORMAL);
            assert!(os::set_priority(list_level(THREAD_PRIORITY_NORMAL, 1)));
            assert_eq!(os::priority(), THREAD_PRIORITY_BELOW_NORMAL);
            assert!(os::set_priority(list_level(THREAD_PRIORITY_NORMAL, 0)));
            assert_eq!(os::priority(), THREAD_PRIORITY_NORMAL);
            // A set of the system's own, for this thread only, then none.
            let sets = os::cpu_sets();
            assert!(!sets.is_empty());
            assert!(os::thread_sets(&[sets[0].id]));
            assert!(os::thread_sets(&[]));
        })
        .join()
        .unwrap();
    }

    /// Only the lane's thread acts on the switches; another thread that calls the same function does nothing.
    #[test]
    fn a_thread_that_is_not_the_lane_leaves_its_priority_and_sets_alone() {
        std::thread::spawn(|| {
            let before = os::priority();
            lane_sync();
            assert!(!IS_LANE.get());
            assert_eq!(os::priority(), before);
        })
        .join()
        .unwrap();
    }
}
