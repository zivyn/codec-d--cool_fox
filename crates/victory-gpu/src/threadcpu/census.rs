//! The CPU census: where the process's processor time goes, by thread group, every 5 seconds (a line
//! of the frame-rate meter, always on). The process's own threads are walked with `NtGetNextThread` (a handle
//! each, a few calls for each thread); the snapshot of every thread of the system that the toolhelp calls take
//! (thousands of them) is only the way out if that call is not there. The line says what the walk took, and a
//! walk that takes long is done less often ([`SLOW_MS`]).
//!
//! The role line before it ([`super::line`]) counts only the threads that said what they are (the
//! submitting thread, the pools, the relay): 250 of the ~850 ms of processor time a frame took at the
//! window spot. This one counts every thread of the process, user and kernel apart, and puts each in
//! a group: the title's own threads (those that start in one of its images), our threads by their
//! names (`victory-wide-1` is `victory-wide`), the others by the module they start in (the card's
//! driver, the system's pools). Kernel time on a title thread is system calls, faults and the
//! handlers of ours that run in them; user time on a thread of ours is our work or our spinning.
//!
//! Read it next to `the process ran X ms user + Y ms kernel` of the frame account: they are the same
//! number, and the groups add up to it (what is left is threads that ended between two lines).

use std::collections::HashMap;
use std::sync::Mutex;

use windows_sys::Wdk::System::Threading::{NtQueryInformationThread, ThreadQuerySetWin32StartAddress};
use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE, INVALID_HANDLE_VALUE, LocalFree};
use windows_sys::Win32::System::Diagnostics::ToolHelp::{CreateToolhelp32Snapshot, TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next};
use windows_sys::Win32::System::LibraryLoader::{
    GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS, GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT, GetModuleFileNameW, GetModuleHandleA,
    GetModuleHandleExW, GetProcAddress,
};
use windows_sys::Win32::System::Threading::{
    GetCurrentProcess, GetCurrentProcessId, GetProcessTimes, GetThreadDescription, GetThreadId, GetThreadTimes, OpenThread,
    THREAD_QUERY_INFORMATION,
};

/// `NtGetNextThread(process, thread, access, attributes, flags, &next)`: the next thread of `process` after `thread`
/// (null: the first), as a handle opened with `access`; an NTSTATUS, `STATUS_NO_MORE_ENTRIES` after the last.
type GetNextThread = unsafe extern "system" fn(HANDLE, HANDLE, u32, u32, u32, *mut HANDLE) -> i32;

/// The call, if this system has it (looked up, not linked: a system without it still starts).
fn get_next_thread() -> Option<GetNextThread> {
    static CALL: std::sync::OnceLock<Option<GetNextThread>> = std::sync::OnceLock::new();
    *CALL.get_or_init(|| {
        // SAFETY: plain lookups of a module every process has; the function has the signature above.
        unsafe {
            let ntdll = GetModuleHandleA(c"ntdll.dll".as_ptr().cast());
            if ntdll.is_null() {
                return None;
            }
            GetProcAddress(ntdll, c"NtGetNextThread".as_ptr().cast())
                .map(|f| std::mem::transmute::<unsafe extern "system" fn() -> isize, GetNextThread>(f))
        }
    })
}

/// A walk that takes longer than this (ms) makes the next lines wait, one more for each time it is over: the census
/// is a diagnostic and does not cost the frame more than a few ms in all.
const SLOW_MS: f64 = 2.0;

/// Whose a thread is.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Kind {
    /// Starts in one of the title's images.
    Title,
    /// Starts in this executable (ours, named or not).
    Ours,
    /// The card's driver, the system.
    Other,
}

/// One thread as the census sees it.
#[derive(Clone, Debug)]
pub(crate) struct Row {
    pub tid: u32,
    pub group: String,
    pub name: String,
    pub kind: Kind,
    /// Processor time since the thread began, 100 ns.
    pub user: u64,
    pub kernel: u64,
}

/// The group of a thread: the title's threads together; ours and any named thread by the name without
/// its number (`victory-wide-1`, `dstorage 7`); the rest by the module they start in.
pub(crate) fn group_of(name: &str, module: &str, exe: &str) -> (String, Kind) {
    let module = module.to_ascii_lowercase();
    let stem = module.strip_suffix(".dll").or_else(|| module.strip_suffix(".exe")).unwrap_or(&module);
    if stem.starts_with("victory-image") {
        return ("title".into(), Kind::Title);
    }
    let kind = if !exe.is_empty() && module == exe.to_ascii_lowercase() { Kind::Ours } else { Kind::Other };
    let base = name.trim_end_matches(|c: char| c.is_ascii_digit()).trim_end_matches(['-', ' ', '_']);
    if !base.is_empty() {
        return (base.to_string(), kind);
    }
    if !name.is_empty() {
        return (name.to_string(), kind);
    }
    match (kind, stem.is_empty()) {
        (Kind::Ours, _) => ("ours (no name)".into(), kind),
        (_, true) => ("?".into(), kind),
        _ => (stem.to_string(), kind),
    }
}

/// What the census says of one group between two lines.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Group {
    pub name: String,
    pub kind: Kind,
    /// Threads that ran at all.
    pub threads: usize,
    /// ms of processor time over the interval.
    pub user_ms: f64,
    pub kernel_ms: f64,
    /// The busiest thread, in cores (processor time over wall time).
    pub busiest: f64,
}

impl Group {
    pub(crate) fn total(&self) -> f64 {
        self.user_ms + self.kernel_ms
    }
}

/// What the threads ran between two lines, by group, most first. `last` is what each thread had run at the
/// earlier line (a thread not in it is new: all it has run counts), `wall_ms` the time between the lines.
/// The processor time of threads that ended between the lines is not here.
pub(crate) fn groups(now: &[Row], last: &HashMap<u32, (u64, u64)>, wall_ms: f64) -> Vec<Group> {
    let mut by: Vec<Group> = Vec::new();
    for r in now {
        let (was_user, was_kernel) = last.get(&r.tid).copied().unwrap_or((0, 0));
        let (user, kernel) = (r.user.saturating_sub(was_user) as f64 / 1e4, r.kernel.saturating_sub(was_kernel) as f64 / 1e4);
        let cores = if wall_ms > 0.0 { (user + kernel) / wall_ms } else { 0.0 };
        let at = match by.iter().position(|g| g.name == r.group && g.kind == r.kind) {
            Some(at) => at,
            None => {
                by.push(Group { name: r.group.clone(), kind: r.kind, threads: 0, user_ms: 0.0, kernel_ms: 0.0, busiest: 0.0 });
                by.len() - 1
            }
        };
        let g = &mut by[at];
        g.threads += usize::from(user + kernel > 0.0);
        g.user_ms += user;
        g.kernel_ms += kernel;
        g.busiest = g.busiest.max(cores);
    }
    by.sort_by(|a, b| b.total().total_cmp(&a.total()));
    by
}

struct State {
    last: HashMap<u32, (u64, u64)>,
    /// Group and kind of each thread seen, once it has a name or a module to go by.
    known: HashMap<u32, (String, Kind)>,
    process: (u64, u64),
    frame: u64,
    at: std::time::Instant,
    /// Lines still to leave out, after a walk that took long ([`SLOW_MS`]).
    skip: u32,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

fn ticks(t: FILETIME) -> u64 {
    u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime)
}

/// The processor time of the whole process, user and kernel, 100 ns.
fn process_times() -> (u64, u64) {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: plain out structures, this process.
    unsafe { GetProcessTimes(GetCurrentProcess(), &mut created, &mut exited, &mut kernel, &mut user) };
    (ticks(user), ticks(kernel))
}

/// The file name of the module that holds `address`.
fn module_of(address: usize) -> String {
    if address == 0 {
        return String::new();
    }
    let mut module = std::ptr::null_mut();
    // SAFETY: looks the module up by an address; no reference is taken.
    let found = unsafe {
        GetModuleHandleExW(
            GET_MODULE_HANDLE_EX_FLAG_FROM_ADDRESS | GET_MODULE_HANDLE_EX_FLAG_UNCHANGED_REFCOUNT,
            address as *const u16,
            &mut module,
        )
    };
    if found == 0 {
        return String::new();
    }
    let mut path = [0u16; 260];
    // SAFETY: a loaded module and a buffer of the length given.
    let len = unsafe { GetModuleFileNameW(module, path.as_mut_ptr(), path.len() as u32) } as usize;
    let path = String::from_utf16_lossy(&path[..len.min(path.len())]);
    path.rsplit(['\\', '/']).next().unwrap_or("").to_string()
}

/// This executable's file name.
fn exe_name() -> &'static str {
    static EXE: std::sync::OnceLock<String> = std::sync::OnceLock::new();
    EXE.get_or_init(|| std::env::current_exe().ok().and_then(|p| p.file_name().map(|n| n.to_string_lossy().into_owned())).unwrap_or_default())
}

/// Every thread of the process, with what it has run.
fn snapshot(known: &mut HashMap<u32, (String, Kind)>) -> Vec<Row> {
    let mut rows = Vec::new();
    if !walk(known, &mut rows) {
        rows.clear();
        toolhelp(known, &mut rows);
    }
    rows
}

/// The threads of this process, one handle after the other (`NtGetNextThread`); false if the call is not there or
/// gives none (then the toolhelp snapshot does it).
fn walk(known: &mut HashMap<u32, (String, Kind)>, rows: &mut Vec<Row>) -> bool {
    let Some(next_thread) = get_next_thread() else {
        return false;
    };
    // SAFETY: the pseudo handle of this process.
    let process = unsafe { GetCurrentProcess() };
    let mut last: HANDLE = std::ptr::null_mut();
    loop {
        let mut next: HANDLE = std::ptr::null_mut();
        // SAFETY: `last` is null or a thread handle of ours from the call before; `next` is written by the call.
        let status = unsafe { next_thread(process, last, THREAD_QUERY_INFORMATION, 0, 0, &mut next) };
        if !last.is_null() {
            // SAFETY: opened by the call before.
            unsafe { CloseHandle(last) };
        }
        if status < 0 || next.is_null() {
            // The end of the threads, or no walk at all (not one thread seen).
            return !rows.is_empty();
        }
        // SAFETY: a thread handle of ours with query access.
        let tid = unsafe { GetThreadId(next) };
        read(next, tid, known, rows);
        last = next;
    }
}

/// Every thread of the system listed, ours picked out: what there is when `NtGetNextThread` is not.
fn toolhelp(known: &mut HashMap<u32, (String, Kind)>, rows: &mut Vec<Row>) {
    // SAFETY: a snapshot of this process's threads, closed below.
    let (process, snap) = unsafe { (GetCurrentProcessId(), CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0)) };
    if snap == INVALID_HANDLE_VALUE {
        return;
    }
    // SAFETY: zeroed is a valid THREADENTRY32; its size is set before use.
    let mut entry: THREADENTRY32 = unsafe { std::mem::zeroed() };
    entry.dwSize = size_of::<THREADENTRY32>() as u32;
    // SAFETY: `snap` is open and `entry` is sized.
    let mut more = unsafe { Thread32First(snap, &mut entry) } != 0;
    while more {
        if entry.th32OwnerProcessID == process {
            let tid = entry.th32ThreadID;
            // SAFETY: a thread of this process, opened for reading and closed.
            let thread = unsafe { OpenThread(THREAD_QUERY_INFORMATION, 0, tid) };
            if !thread.is_null() {
                read(thread, tid, known, rows);
                // SAFETY: opened above.
                unsafe { CloseHandle(thread) };
            }
        }
        // SAFETY: as above.
        more = unsafe { Thread32Next(snap, &mut entry) } != 0;
    }
    // SAFETY: opened above.
    unsafe { CloseHandle(snap) };
}

/// What thread `tid` (open as `thread`) has run, and what it is called, as a row.
fn read(thread: HANDLE, tid: u32, known: &mut HashMap<u32, (String, Kind)>, rows: &mut Vec<Row>) {
    let zero = FILETIME { dwLowDateTime: 0, dwHighDateTime: 0 };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: plain out structures.
    let ok = unsafe { GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user) } != 0;
    let mut name = String::new();
    let mut text = std::ptr::null_mut();
    // SAFETY: the system allocates the string; it is freed below.
    unsafe {
        if GetThreadDescription(thread, &mut text) >= 0 && !text.is_null() {
            let len = (0..).take_while(|&i| *text.add(i) != 0).count();
            name = String::from_utf16_lossy(std::slice::from_raw_parts(text, len));
            LocalFree(text.cast());
        }
    }
    let (group, kind) = match known.get(&tid) {
        Some(g) => g.clone(),
        None => {
            let mut start = 0usize;
            // SAFETY: writes one pointer-sized start address.
            unsafe {
                NtQueryInformationThread(
                    thread,
                    ThreadQuerySetWin32StartAddress,
                    (&raw mut start).cast(),
                    size_of::<usize>() as u32,
                    std::ptr::null_mut(),
                )
            };
            let found = group_of(&name, &module_of(start), exe_name());
            // A thread of ours that has no name yet is looked at again at the next line.
            if !(found.1 == Kind::Ours && name.is_empty()) {
                known.insert(tid, found.clone());
            }
            found
        }
    };
    if ok {
        rows.push(Row { tid, group, name, kind, user: ticks(user), kernel: ticks(kernel) });
    }
}

/// The text of the first line for `groups` over `frames` frames in `wall_ms`, `process` being the ms of user and
/// kernel time of the whole process (what it ran that no thread present accounts for is of threads that ended).
pub(crate) fn text(groups: &[Group], process: (f64, f64), frames: u64, wall_ms: f64, logical: usize) -> String {
    let per = |ms: f64| ms / frames as f64;
    let total = process.0 + process.1;
    let sum = |kind: Kind| groups.iter().filter(|g| g.kind == kind).fold(0.0, |a, g| a + g.total());
    let accounted = groups.iter().fold(0.0, |a, g| a + g.total());
    let parts: Vec<String> = groups
        .iter()
        .filter(|g| per(g.total()) >= 0.5)
        .take(18)
        .map(|g| {
            format!(
                "{} {:.1} ({:.1}+{:.1}, {} thr, max {:.2} core)",
                g.name,
                per(g.total()),
                per(g.user_ms),
                per(g.kernel_ms),
                g.threads,
                g.busiest
            )
        })
        .collect();
    format!(
        "CPU census per frame, every thread of the process, over {frames} frames in {wall_ms:.0} ms: process {:.0} ms = {:.1} cores of {logical} logical ({:.0} user + {:.0} kernel); title {:.0}, ours {:.0}, driver and system {:.0}, threads that ended {:.0}; by group (ms: user+kernel): {}",
        per(total),
        total / wall_ms,
        per(process.0),
        per(process.1),
        per(sum(Kind::Title)),
        per(sum(Kind::Ours)),
        per(sum(Kind::Other)),
        per((total - accounted).max(0.0)),
        parts.join(", ")
    )
}

/// The busiest threads, as a line: tid, group, name, cores.
fn busiest(now: &[Row], last: &HashMap<u32, (u64, u64)>, wall_ms: f64) -> String {
    let mut all: Vec<(f64, &Row)> = now
        .iter()
        .map(|r| {
            let was = last.get(&r.tid).copied().unwrap_or((0, 0));
            ((r.user.saturating_sub(was.0) + r.kernel.saturating_sub(was.1)) as f64 / 1e4 / wall_ms, r)
        })
        .collect();
    all.sort_by(|a, b| b.0.total_cmp(&a.0));
    all.iter()
        .take(10)
        .filter(|(cores, _)| *cores >= 0.02)
        .map(|(cores, r)| {
            let user = r.user.saturating_sub(last.get(&r.tid).map_or(0, |l| l.0)) as f64 / 1e4 / wall_ms;
            format!("{} {} {:.2} ({:.0}% user)", r.tid, if r.name.is_empty() { &r.group } else { &r.name }, cores, 100.0 * user / cores.max(1e-9))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// How many lines to leave out after a walk of the threads that took `ms`: none up to [`SLOW_MS`], then one more for
/// each time it is over, at most 8.
fn skipped_after(ms: f64) -> u32 {
    if ms <= SLOW_MS { 0 } else { ((ms / SLOW_MS) as u32).min(8) }
}

/// The census lines, with the numbers since the line before.
pub(crate) fn line() {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = state.get_or_insert_with(|| State {
        last: HashMap::new(),
        known: HashMap::new(),
        process: (0, 0),
        frame: 0,
        at: std::time::Instant::now(),
        skip: 0,
    });
    if state.skip > 0 {
        state.skip -= 1;
        return;
    }
    let walking = std::time::Instant::now();
    let rows = snapshot(&mut state.known);
    let took = walking.elapsed().as_secs_f64() * 1e3;
    state.skip = skipped_after(took);
    let process = process_times();
    let frame = crate::frame::current();
    let wall = state.at.elapsed().as_secs_f64() * 1e3;
    let first = state.last.is_empty();
    let frames = frame.saturating_sub(state.frame);
    let was = std::mem::replace(&mut state.last, rows.iter().map(|r| (r.tid, (r.user, r.kernel))).collect());
    let was_process = std::mem::replace(&mut state.process, process);
    state.frame = frame;
    state.at = std::time::Instant::now();
    state.known.retain(|tid, _| state.last.contains_key(tid));
    if first || frames == 0 || wall <= 0.0 {
        return;
    }
    let by = groups(&rows, &was, wall);
    let spent = (process.0.saturating_sub(was_process.0) as f64 / 1e4, process.1.saturating_sub(was_process.1) as f64 / 1e4);
    let logical = std::thread::available_parallelism().map_or(0, |n| n.get());
    tracing::info!("{}", text(&by, spent, frames, wall, logical));
    tracing::info!(
        "CPU census, the busiest threads (thread, name, cores over the interval; the census itself took {took:.2} ms for {} threads): {}",
        rows.len(),
        busiest(&rows, &was, wall)
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(tid: u32, group: &str, kind: Kind, user: u64, kernel: u64) -> Row {
        Row { tid, group: group.into(), name: String::new(), kind, user, kernel }
    }

    #[test]
    fn a_thread_is_grouped_by_where_it_starts_then_by_its_name_then_by_its_module() {
        let exe = "victory-diag.exe";
        assert_eq!(group_of("", "victory-image-b99f.dll", exe), ("title".into(), Kind::Title));
        // A name the title gave to its own thread does not make it ours.
        assert_eq!(group_of("Render", "victory-image-b99f.dll", exe), ("title".into(), Kind::Title));
        assert_eq!(group_of("victory-wide-3", "victory-diag.exe", exe), ("victory-wide".into(), Kind::Ours));
        assert_eq!(group_of("dstorage 11", "victory-diag.exe", exe), ("dstorage".into(), Kind::Ours));
        assert_eq!(group_of("victory-submit", "VICTORY-DIAG.EXE", exe), ("victory-submit".into(), Kind::Ours));
        assert_eq!(group_of("", "victory-diag.exe", exe), ("ours (no name)".into(), Kind::Ours));
        assert_eq!(group_of("", "nvwgf2umx.dll", exe), ("nvwgf2umx".into(), Kind::Other));
        assert_eq!(group_of("D3D Background Thread 2", "d3d12.dll", exe), ("D3D Background Thread".into(), Kind::Other));
        assert_eq!(group_of("", "", exe), ("?".into(), Kind::Other));
        // A name that is only a number stays whole.
        assert_eq!(group_of("42", "x.dll", exe), ("42".into(), Kind::Other));
    }

    #[test]
    fn what_threads_ran_since_the_last_line_is_summed_by_group_and_a_new_thread_counts_whole() {
        let last: HashMap<u32, (u64, u64)> = [(1, (100_000, 0)), (2, (0, 0)), (3, (50_000, 10_000))].into();
        let now = vec![
            // Ran 10 ms user + 5 ms kernel since the line.
            row(1, "title", Kind::Title, 200_000, 50_000),
            // Idle.
            row(2, "victory-list", Kind::Ours, 0, 0),
            // Ran 20 ms user.
            row(3, "title", Kind::Title, 250_000, 10_000),
            // New: all it has run counts, 30 ms.
            row(4, "victory-list", Kind::Ours, 300_000, 0),
        ];
        let by = groups(&now, &last, 100.0);
        assert_eq!(by.len(), 2);
        // The title's 35 ms before the list threads' 30.
        let (title, list) = (&by[0], &by[1]);
        assert_eq!((list.name.as_str(), list.threads, list.user_ms, list.kernel_ms), ("victory-list", 1, 30.0, 0.0));
        assert_eq!((title.name.as_str(), title.threads, title.user_ms, title.kernel_ms), ("title", 2, 30.0, 5.0));
        // 20 ms of the busiest title thread over 100 ms of wall time.
        assert!((title.busiest - 0.20).abs() < 1e-9, "{}", title.busiest);
        assert!((list.busiest - 0.30).abs() < 1e-9, "{}", list.busiest);
    }

    #[test]
    fn the_text_says_per_frame_and_what_the_threads_do_not_account_for() {
        let last = HashMap::new();
        let now = vec![row(1, "title", Kind::Title, 4_000_000, 1_000_000), row(2, "victory-list", Kind::Ours, 2_000_000, 0)];
        let by = groups(&now, &last, 1000.0);
        // 800 ms of the process in 10 frames, 700 of them in the threads.
        let t = text(&by, (600.0, 200.0), 10, 1000.0, 12);
        assert!(t.contains("process 80 ms = 0.8 cores of 12 logical (60 user + 20 kernel)"), "{t}");
        assert!(t.contains("title 50, ours 20, driver and system 0, threads that ended 10"), "{t}");
        assert!(t.contains("title 50.0 (40.0+10.0, 1 thr, max 0.50 core)"), "{t}");
    }

    /// The census sees the threads of this test process: a thread that spins is counted for its name's
    /// group, in user time, and the process's own total is at least what the groups hold.
    #[test]
    fn a_spinning_thread_of_ours_shows_in_its_group() {
        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stop.clone();
        let spinner = std::thread::Builder::new()
            .name("victory-census-test-7".into())
            .spawn(move || {
                while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                    std::hint::spin_loop();
                }
            })
            .unwrap();
        let mut known = HashMap::new();
        let before = snapshot(&mut known);
        let began = std::time::Instant::now();
        std::thread::sleep(std::time::Duration::from_millis(300));
        let after = snapshot(&mut known);
        let wall = began.elapsed().as_secs_f64() * 1e3;
        let last: HashMap<u32, (u64, u64)> = before.iter().map(|r| (r.tid, (r.user, r.kernel))).collect();
        let by = groups(&after, &last, wall);
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        spinner.join().unwrap();
        let g = by.iter().find(|g| g.name == "victory-census-test").expect("the group of the spinner");
        assert_eq!(g.kind, Kind::Ours);
        assert!(g.busiest > 0.3, "the spinner ran {:.2} of a core", g.busiest);
        assert!(g.user_ms > g.kernel_ms);
    }

    /// The walk of the process's own threads sees what the system's snapshot sees of them: this thread and one that
    /// is started, with their names; and a slow walk makes the lines wait, a quick one does not.
    #[test]
    fn the_walk_of_our_threads_agrees_with_the_snapshot_and_a_slow_one_is_done_less_often() {
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        let (seen_tx, seen_rx) = std::sync::mpsc::channel::<u32>();
        let sleeper = std::thread::Builder::new()
            .name("victory-census-walk".into())
            .spawn(move || {
                // SAFETY: plain query of the calling thread's id.
                seen_tx.send(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }).unwrap();
                let _ = rx.recv();
            })
            .unwrap();
        let tid = seen_rx.recv().unwrap();
        let (mut known, mut walked, mut listed) = (HashMap::new(), Vec::new(), Vec::new());
        // (This system has the call: the toolhelp snapshot is only the way out.)
        assert!(walk(&mut known, &mut walked), "no walk of the threads");
        toolhelp(&mut known, &mut listed);
        tx.send(()).unwrap();
        sleeper.join().unwrap();
        for rows in [&walked, &listed] {
            let row = rows.iter().find(|r| r.tid == tid).expect("the thread that was started");
            assert_eq!(row.name, "victory-census-walk");
            assert_eq!(row.group, "victory-census-walk");
        }
        // Every thread the walk saw is one the snapshot saw, but for those that began or ended between the two.
        let listed_tids: Vec<u32> = listed.iter().map(|r| r.tid).collect();
        let missing = walked.iter().filter(|r| !listed_tids.contains(&r.tid)).count();
        assert!(missing <= 2, "{missing} threads of the walk are not in the snapshot");
        assert_eq!((skipped_after(0.3), skipped_after(2.0)), (0, 0));
        assert_eq!((skipped_after(5.0), skipped_after(500.0)), (2, 8));
    }
}
