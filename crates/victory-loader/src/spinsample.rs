//! How much of the title's processor time goes into its wait loops: `VICTORY_SPIN_SAMPLE=<ms>` looks, every `<ms>`
//! milliseconds, at each thread of the title that ran since the last look, and when the place it is at is a loop around
//! a `monitorx`/`mwaitx` that faulted (`console_insn::wait_site_near`), the processor time it used since the last look is
//! counted as spent waiting. Those loops are `pause` by default (`VICTORY_PATCH_WAITS`): a core that reads a value as
//! fast as it goes, beside the thread the scheduler put on its SMT sibling. Every five seconds one line says the
//! share, the threads (by id, as the census names them) and the places that hold most of it.
//!
//! A thread is stopped for a few microseconds to see where it is, and only when it used more than 0.2 ms since the last
//! look: at 5 ms that is a handful of threads, some hundreds of stops a second.

use std::collections::HashMap;
use std::time::{Duration, Instant};

use windows_sys::Win32::Foundation::{CloseHandle, FILETIME, HANDLE};
use windows_sys::Win32::System::Threading::{
    GetThreadTimes, OpenThread, THREAD_GET_CONTEXT, THREAD_QUERY_INFORMATION, THREAD_SUSPEND_RESUME,
};

use crate::crash::locate;
use crate::stacks::{other_threads, rip_of, start_of};

/// How often a line is logged.
const REPORT: Duration = Duration::from_secs(5);
/// A thread that used less than this since the last look (100 ns units) is not stopped to look at.
const WORTH: u64 = 2_000;

/// The interval asked for, in milliseconds: `VICTORY_SPIN_SAMPLE=5`. 1 to 1000.
fn asked(text: &str) -> Option<Duration> {
    let ms: u64 = text.trim().parse().ok()?;
    (1..=1000).contains(&ms).then(|| Duration::from_millis(ms))
}

/// What one thread has done so far in a report's window.
#[derive(Default, Clone, Copy)]
struct Used {
    /// 100 ns units of processor time, all of it and in wait loops.
    all: u64,
    waiting: u64,
}

/// What a window of looks says, for the line.
#[derive(Default)]
struct Window {
    looks: u64,
    in_loops: u64,
    by_thread: HashMap<u32, Used>,
    by_place: HashMap<usize, u64>,
}

impl Window {
    /// One thread's `cpu` (100 ns units) since the last look, and where it was found: in the wait loop around `site`, if
    /// it was in one.
    fn add(&mut self, id: u32, cpu: u64, looked: bool, site: Option<usize>) {
        let used = self.by_thread.entry(id).or_default();
        used.all += cpu;
        if !looked {
            return;
        }
        self.looks += 1;
        if let Some(site) = site {
            self.in_loops += 1;
            used.waiting += cpu;
            *self.by_place.entry(site).or_default() += cpu;
        }
    }

    fn totals(&self) -> Used {
        self.by_thread
            .values()
            .fold(Used::default(), |sum, u| Used {
                all: sum.all + u.all,
                waiting: sum.waiting + u.waiting,
            })
    }

    /// The `n` largest by their share, the biggest first.
    fn top<K: Copy + Ord>(of: impl Iterator<Item = (K, u64)>, n: usize) -> Vec<(K, u64)> {
        let mut all: Vec<(K, u64)> = of.filter(|&(_, v)| v > 0).collect();
        all.sort_unstable_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        all.truncate(n);
        all
    }
}

fn ms(units: u64) -> f64 {
    units as f64 / 10_000.0
}

struct Seen {
    handle: HANDLE,
    title: bool,
    last: u64,
}

fn cpu(thread: HANDLE) -> u64 {
    // SAFETY: zeroed is a valid FILETIME, and the call writes them.
    let mut t: [FILETIME; 4] = unsafe { std::mem::zeroed() };
    let [a, b, c, d] = &mut t;
    // SAFETY: an open thread; the pointers are to the structures above.
    unsafe { GetThreadTimes(thread, a, b, c, d) };
    let of = |f: &FILETIME| u64::from(f.dwHighDateTime) << 32 | u64::from(f.dwLowDateTime);
    of(&t[2]) + of(&t[3])
}

/// Reads `VICTORY_SPIN_SAMPLE` and, if set, starts the thread that looks.
pub fn spin_sample_on_schedule() {
    let Some(every) = std::env::var("VICTORY_SPIN_SAMPLE")
        .ok()
        .as_deref()
        .and_then(asked)
    else {
        return;
    };
    let _ = std::thread::Builder::new()
        .name("victory-spin-sample".into())
        .spawn(move || run(every));
}

fn run(every: Duration) {
    let mut threads: HashMap<u32, Seen> = HashMap::new();
    let mut window = Window::default();
    let mut refreshed = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .unwrap_or_else(Instant::now);
    let mut reported = Instant::now();
    loop {
        std::thread::sleep(every);
        if refreshed.elapsed() >= Duration::from_secs(1) {
            refreshed = Instant::now();
            let now = other_threads();
            threads.retain(|id, t| {
                let keep = now.contains(id);
                if !keep {
                    // SAFETY: opened below.
                    unsafe { CloseHandle(t.handle) };
                }
                keep
            });
            for id in now {
                threads.entry(id).or_insert_with(|| {
                    // SAFETY: opens a thread of our own process; kept open until it is gone.
                    let handle = unsafe {
                        OpenThread(
                            THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                            0,
                            id,
                        )
                    };
                    let title =
                        !handle.is_null() && locate(start_of(handle)).starts_with("victory-image");
                    Seen {
                        handle,
                        title,
                        last: if handle.is_null() { 0 } else { cpu(handle) },
                    }
                });
            }
        }
        for (&id, t) in threads
            .iter_mut()
            .filter(|(_, t)| t.title && !t.handle.is_null())
        {
            let now = cpu(t.handle);
            let used = now.saturating_sub(t.last);
            t.last = now;
            if used < WORTH {
                window.add(id, used, false, None);
                continue;
            }
            let site = rip_of(t.handle).and_then(crate::console_insn::wait_site_near);
            window.add(id, used, true, site);
        }
        if reported.elapsed() >= REPORT {
            let seconds = reported.elapsed().as_secs_f64();
            reported = Instant::now();
            line(&std::mem::take(&mut window), seconds, every);
        }
    }
}

fn line(window: &Window, seconds: f64, every: Duration) {
    let Used { all, waiting } = window.totals();
    let threads: Vec<String> =
        Window::top(window.by_thread.iter().map(|(&id, u)| (id, u.waiting)), 6)
            .into_iter()
            .map(|(id, w)| {
                format!(
                    "{id} {:.0} of {:.0}",
                    ms(w) / seconds,
                    ms(window.by_thread[&id].all) / seconds
                )
            })
            .collect();
    let places: Vec<String> = Window::top(window.by_place.iter().map(|(&at, &v)| (at, v)), 6)
        .into_iter()
        .map(|(at, v)| format!("{} {:.0}", locate(at), ms(v) / seconds))
        .collect();
    tracing::info!(
        "title wait loops (VICTORY_SPIN_SAMPLE={} ms), per second over {seconds:.1} s: the title's threads ran {:.0} ms of processor time, {:.0} ms of it ({:.0}%) in a loop around a monitorx/mwaitx ({} looks, {} in a loop); threads (id, ms in loops of ms): {}; places (ms): {}",
        every.as_millis(),
        ms(all) / seconds,
        ms(waiting) / seconds,
        if all > 0 {
            100.0 * waiting as f64 / all as f64
        } else {
            0.0
        },
        window.looks,
        window.in_loops,
        threads.join(", "),
        places.join(", ")
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_interval_is_a_number_of_milliseconds_in_range() {
        assert_eq!(asked("5"), Some(Duration::from_millis(5)));
        assert_eq!(asked(" 20 "), Some(Duration::from_millis(20)));
        assert_eq!(asked("0"), None);
        assert_eq!(asked("1001"), None);
        assert_eq!(asked("fast"), None);
    }

    #[test]
    fn a_window_counts_the_time_of_a_look_that_found_a_loop_for_that_thread_and_place() {
        let mut w = Window::default();
        // Thread 1 ran 10 ms and was in a loop; thread 2 ran 10 ms and was elsewhere; thread 3 ran little and was not looked at.
        w.add(1, 100_000, true, Some(0x4000));
        w.add(2, 100_000, true, None);
        w.add(3, 500, false, None);
        w.add(1, 50_000, true, Some(0x4000));
        let t = w.totals();
        assert_eq!((t.all, t.waiting), (250_500, 150_000));
        assert_eq!((w.looks, w.in_loops), (3, 2));
        assert_eq!(w.by_place[&0x4000], 150_000);
        let top = Window::top(w.by_thread.iter().map(|(&id, u)| (id, u.waiting)), 6);
        assert_eq!(top, vec![(1, 150_000)]);
    }

    #[test]
    fn the_largest_come_first_and_ties_go_by_key() {
        let top = Window::top([(3u32, 5u64), (1, 9), (2, 9), (4, 0)].into_iter(), 2);
        assert_eq!(top, vec![(1, 9), (2, 9)]);
    }

    /// A thread running a loop of `pause` over a `monitorx; mwaitx` site that is registered is seen at it.
    #[test]
    fn a_spinning_thread_is_found_in_the_loop_it_spins_in() {
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering::Relaxed};
        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = std::sync::mpsc::channel();
        let spinner = {
            let stop = stop.clone();
            std::thread::spawn(move || {
                // SAFETY: a plain query.
                tx.send(unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() })
                    .unwrap();
                while !stop.load(Relaxed) {
                    std::hint::spin_loop();
                }
            })
        };
        let id = rx.recv().unwrap();
        // SAFETY: a thread of this process.
        let handle = unsafe {
            OpenThread(
                THREAD_SUSPEND_RESUME | THREAD_GET_CONTEXT | THREAD_QUERY_INFORMATION,
                0,
                id,
            )
        };
        assert!(!handle.is_null());
        let rip = (0..50).find_map(|_| {
            std::thread::sleep(Duration::from_millis(2));
            rip_of(handle)
        });
        stop.store(true, Relaxed);
        // SAFETY: opened above.
        unsafe { CloseHandle(handle) };
        spinner.join().unwrap();
        assert!(
            rip.is_some_and(|r| r != 0),
            "the thread's place is read: {rip:?}"
        );
    }
}
