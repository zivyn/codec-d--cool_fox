//! The CPU time of the threads of ours that a frame waits behind, per frame
//! (every 5 seconds, a line of the frame-rate meter; always on, a few system
//! calls).
//!
//! A frame's wall time is noisy when the title's threads and ours compete for
//! the cores (86-95 ms for one build, run to run). The time a thread ran, user
//! and kernel (`GetThreadTimes`), does not count the time it was waiting for a
//! core or for a lock, so a cut of work shows there however the run was
//! scheduled: a thread that does less work is on the CPU less.

use std::collections::HashMap;
use std::sync::Mutex;

pub(crate) mod census;

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME};
use windows_sys::Win32::System::Threading::{
    GetCurrentThreadId, GetThreadTimes, OpenThread, THREAD_QUERY_LIMITED_INFORMATION,
};

struct State {
    /// Role and thread of each thread that said what it is.
    threads: Vec<(&'static str, u32)>,
    /// What each had run (100 ns) when the line was last made.
    last: HashMap<u32, u64>,
    frame: u64,
    at: std::time::Instant,
}

static STATE: Mutex<Option<State>> = Mutex::new(None);

/// The calling thread is one of `role` ("submit", "wide", ...).
pub(crate) fn register(role: &'static str) {
    let tid = unsafe { GetCurrentThreadId() };
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = state.get_or_insert_with(|| State {
        threads: Vec::new(),
        last: HashMap::new(),
        frame: 0,
        at: std::time::Instant::now(),
    });
    if !state.threads.iter().any(|t| t.1 == tid) {
        state.threads.push((role, tid));
    }
}

/// How long thread `tid` has run, user and kernel, in units of 100 ns.
fn ran(tid: u32) -> Option<u64> {
    let ticks = |t: FILETIME| u64::from(t.dwHighDateTime) << 32 | u64::from(t.dwLowDateTime);
    let zero = FILETIME {
        dwLowDateTime: 0,
        dwHighDateTime: 0,
    };
    let (mut created, mut exited, mut kernel, mut user) = (zero, zero, zero, zero);
    // SAFETY: a handle opened here and closed; plain out structures.
    unsafe {
        let thread = OpenThread(THREAD_QUERY_LIMITED_INFORMATION, 0, tid);
        if thread.is_null() {
            return None;
        }
        let ok = GetThreadTimes(thread, &mut created, &mut exited, &mut kernel, &mut user) != 0;
        CloseHandle(thread);
        ok.then(|| ticks(kernel) + ticks(user))
    }
}

/// The CPU time per frame of the threads of each role since the last line.
/// `None` the first time (it only takes the starting values).
fn per_frame() -> Option<(Vec<(&'static str, usize, f64)>, u64, f64)> {
    let mut state = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = state.as_mut()?;
    let frame = crate::frame::current();
    let first = state.last.is_empty();
    let mut sums: Vec<(&'static str, usize, u64)> = Vec::new();
    for &(role, tid) in &state.threads {
        let Some(now) = ran(tid) else {
            continue;
        };
        let was = state.last.insert(tid, now).unwrap_or(now);
        let spent = now.saturating_sub(was);
        match sums.iter_mut().find(|s| s.0 == role) {
            Some(s) => {
                s.1 += 1;
                s.2 += spent;
            }
            None => sums.push((role, 1, spent)),
        }
    }
    let wall = state.at.elapsed().as_secs_f64() * 1e3;
    let frames = frame.saturating_sub(state.frame);
    (state.frame, state.at) = (frame, std::time::Instant::now());
    if first || frames == 0 {
        return None;
    }
    let ms = |ticks: u64| ticks as f64 / 1e4 / frames as f64;
    Some((
        sums.into_iter()
            .map(|(role, n, ticks)| (role, n, ms(ticks)))
            .collect(),
        frames,
        wall,
    ))
}

/// The line: CPU ms per frame of each role, and the lane's share of the wall
/// time it ran in.
pub(crate) fn line() {
    census::line();
    let Some((roles, frames, wall)) = per_frame() else {
        return;
    };
    let text: Vec<String> = roles
        .iter()
        .map(|(role, n, ms)| format!("{role} {ms:.1} ms ({n} threads)"))
        .collect();
    let lane = roles
        .iter()
        .find(|r| r.0 == "submit")
        .map_or(0.0, |r| r.2 * frames as f64 / wall * 100.0);
    tracing::info!(
        "CPU time per frame of our threads, user + kernel, over {frames} frames in {wall:.0} ms (time spent waiting for a core or a lock is not counted): {}; the submitting thread ran {lane:.0}% of the wall time",
        text.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A thread that spins is counted by role, one that sleeps is not, and the
    /// first line only takes the starting values.
    #[test]
    fn a_spinning_thread_is_counted_and_a_sleeping_one_is_not() {
        let (stop, wait) = std::sync::mpsc::channel::<()>();
        let (ready, registered) = std::sync::mpsc::channel::<()>();
        let (ready2, registered2) = std::sync::mpsc::channel::<()>();
        let sleeper = std::thread::spawn(move || {
            register("test-sleeper");
            ready.send(()).unwrap();
            let _ = wait.recv();
        });
        let stopped = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let flag = stopped.clone();
        let spinner = std::thread::spawn(move || {
            register("test-spinner");
            ready2.send(()).unwrap();
            while !flag.load(std::sync::atomic::Ordering::Relaxed) {
                std::hint::spin_loop();
            }
        });
        registered.recv().unwrap();
        registered2.recv().unwrap();
        let tids: Vec<u32> = STATE
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .threads
            .iter()
            .filter(|t| t.0.starts_with("test-"))
            .map(|t| t.1)
            .collect();
        assert_eq!(tids.len(), 2);
        let before: Vec<u64> = tids.iter().map(|&t| ran(t).unwrap()).collect();
        std::thread::sleep(std::time::Duration::from_millis(200));
        let after: Vec<u64> = tids.iter().map(|&t| ran(t).unwrap()).collect();
        let spent: Vec<u64> = after.iter().zip(&before).map(|(a, b)| a - b).collect();
        let (spinning, sleeping) = if STATE
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .threads
            .iter()
            .find(|t| t.1 == tids[0])
            .unwrap()
            .0
            == "test-spinner"
        {
            (spent[0], spent[1])
        } else {
            (spent[1], spent[0])
        };
        // 100 ns units: the spinner ran a good part of 200 ms, the sleeper next to nothing.
        assert!(spinning > 500_000, "{spinning}");
        assert!(sleeping < 200_000, "{sleeping}");
        stopped.store(true, std::sync::atomic::Ordering::Relaxed);
        stop.send(()).unwrap();
        sleeper.join().unwrap();
        spinner.join().unwrap();
    }
}
