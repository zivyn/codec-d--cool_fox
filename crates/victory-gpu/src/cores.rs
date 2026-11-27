//! Which processors our own threads run on, where a processor has two kinds
//! of cores. `cpu_sets` and `keep_on_fast_cores` are fxceless/victory-fork's
//! (`framelog.rs`, commit 0b0074e0, GPL-3.0-or-later, as this).

/// The system's processor sets of group 0: each one's id and its efficiency
/// class (higher is faster).
fn cpu_sets() -> Vec<(u32, u8)> {
    use windows_sys::Win32::System::SystemInformation::{GetSystemCpuSetInformation, SYSTEM_CPU_SET_INFORMATION};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
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
        if set.Group == 0 {
            sets.push((set.Id, set.EfficiencyClass));
        }
        at += info.Size as usize;
    }
    sets
}

/// The sets of the fastest class, if there are slower ones too.
fn fast_of(sets: &[(u32, u8)]) -> Vec<u32> {
    let best = sets.iter().map(|c| c.1).max().unwrap_or(0);
    let fast: Vec<u32> = sets.iter().filter(|c| c.1 == best).map(|c| c.0).collect();
    if fast.len() == sets.len() { Vec::new() } else { fast }
}

/// `VICTORY_FAST_CORES`: 0: no thread asks; 1: the threads a frame waits for
/// one by one; 2 (the default): the threads they share their work with too;
/// 3: those that work beside the rest as well (lists translated, descriptors
/// made): more threads by then than the fast cores run at a time, while the
/// slow ones stood idle.
fn stage() -> u32 {
    static STAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *STAGE.get_or_init(|| std::env::var("VICTORY_FAST_CORES").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(2))
}

/// Asks the system to run the calling thread, which a frame waits for, on
/// the fast cores only, where the processor has two kinds. That fork's submit
/// thread was found on a slow one in 1,529 looks of 24,021. (A selection of
/// processors is a preference the scheduler keeps unless nothing of it is
/// free.) The title's threads are not asked for.
pub(crate) fn keep_on_fast_cores(what: &str) {
    ask(what, 1);
}

/// [`keep_on_fast_cores`] for one of the threads a submission's preparing or
/// a landing shares its work with.
pub(crate) fn keep_helper_on_fast_cores(what: &str) {
    ask(what, 2);
}

/// [`keep_on_fast_cores`] for one of many threads whose work is ahead of what
/// a frame waits for.
pub(crate) fn keep_many_on_fast_cores(what: &str) {
    ask(what, 3);
}

/// `VICTORY_THREAD_PRIORITY`: 0: every thread of ours at the title's own
/// priority, as before; 1 (the default): the helpers a submission's preparing
/// or a landing waits for run above it, like the thread that prepares (and
/// before the title's threads, and the ones that translate its lists, when
/// all the processors are busy); 2: the threads that translate lists run
/// below it as well.
///
/// The frame is bound by the thread that prepares and sends submissions, but
/// the process keeps some 7 of the 12 logical processors busy (a 6 core
/// processor's worth, with the second threads of its cores): whenever more
/// threads want to run than there are processors, the helpers that thread
/// waits for stand in the same line as the 8 threads that translate lists
/// (440 ms of work a frame, none of it waited for) and the title's own.
fn priority_stage() -> u32 {
    static STAGE: std::sync::OnceLock<u32> = std::sync::OnceLock::new();
    *STAGE.get_or_init(|| std::env::var("VICTORY_THREAD_PRIORITY").ok().and_then(|v| v.trim().parse().ok()).unwrap_or(1))
}

/// What a thread's priority is set to at `stage` (the switch's value) for
/// its `role`: `None`, as it is.
fn priority_for(stage: u32, role: Role) -> Option<i32> {
    use windows_sys::Win32::System::Threading::{THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL};
    match role {
        Role::Helper if stage >= 1 => Some(THREAD_PRIORITY_ABOVE_NORMAL),
        Role::Translator if stage >= 2 => Some(THREAD_PRIORITY_BELOW_NORMAL),
        _ => None,
    }
}

/// What a thread of ours does for the frame.
#[derive(Clone, Copy, PartialEq, Debug)]
pub(crate) enum Role {
    /// Work that the thread preparing submissions, or the landing of flights,
    /// waits for.
    Helper,
    /// Work ahead of the frame's critical path, never waited for at a
    /// processor's expense.
    Translator,
}

/// Sets the calling thread's priority for its `role` (`VICTORY_THREAD_PRIORITY`).
pub(crate) fn prioritize(what: &str, role: Role) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadPriority};
    let Some(level) = priority_for(priority_stage(), role) else {
        return;
    };
    // SAFETY: this thread's own priority.
    if unsafe { SetThreadPriority(GetCurrentThread(), level) } == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread's priority could not be set");
    }
}

fn ask(what: &str, from: u32) {
    use windows_sys::Win32::System::Threading::{GetCurrentThread, SetThreadSelectedCpuSets};
    if stage() < from {
        return;
    }
    static FAST: std::sync::OnceLock<(Vec<u32>, usize)> = std::sync::OnceLock::new();
    let (fast, of) = FAST.get_or_init(|| {
        let sets = cpu_sets();
        (fast_of(&sets), sets.len())
    });
    // All alike (or nothing known): nothing to ask for.
    if fast.is_empty() {
        return;
    }
    // SAFETY: our own thread and a list of the system's own set ids.
    let ok = unsafe { SetThreadSelectedCpuSets(GetCurrentThread(), fast.as_ptr(), fast.len() as u32) };
    if ok == 0 {
        tracing::warn!(what, error = %std::io::Error::last_os_error(), "the thread could not be kept on the fast cores");
    } else {
        tracing::info!(what, fast = fast.len(), of, "the thread is kept on the fast cores (VICTORY_FAST_CORES=0: not)");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities_follow_the_switch_and_the_role() {
        use windows_sys::Win32::System::Threading::{THREAD_PRIORITY_ABOVE_NORMAL, THREAD_PRIORITY_BELOW_NORMAL};
        assert_eq!(priority_for(0, Role::Helper), None);
        assert_eq!(priority_for(0, Role::Translator), None);
        assert_eq!(priority_for(1, Role::Helper), Some(THREAD_PRIORITY_ABOVE_NORMAL));
        assert_eq!(priority_for(1, Role::Translator), None);
        assert_eq!(priority_for(2, Role::Helper), Some(THREAD_PRIORITY_ABOVE_NORMAL));
        assert_eq!(priority_for(2, Role::Translator), Some(THREAD_PRIORITY_BELOW_NORMAL));
    }

    #[test]
    fn a_helper_thread_runs_above_normal_with_the_default() {
        use windows_sys::Win32::System::Threading::{GetCurrentThread, GetThreadPriority, THREAD_PRIORITY_ABOVE_NORMAL};
        if priority_stage() == 0 {
            return;
        }
        let level = std::thread::spawn(|| {
            prioritize("test", Role::Helper);
            // SAFETY: this thread's own.
            unsafe { GetThreadPriority(GetCurrentThread()) }
        })
        .join()
        .unwrap();
        assert_eq!(level, THREAD_PRIORITY_ABOVE_NORMAL);
    }

    #[test]
    fn the_fast_cores_are_those_of_the_best_class_if_there_is_a_worse_one() {
        assert_eq!(fast_of(&[(256, 1), (257, 1), (258, 0)]), [256, 257]);
        assert!(fast_of(&[(256, 0), (257, 0)]).is_empty());
        assert!(fast_of(&[]).is_empty());
        assert!(!cpu_sets().is_empty());
        std::thread::spawn(|| keep_on_fast_cores("test")).join().unwrap();
    }
}
