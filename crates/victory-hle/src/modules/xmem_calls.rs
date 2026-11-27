//! What the title's calls into memory cost, in a line every 5 s
//! (`VICTORY_MEMORY_STATS=0`: not said): by call, how many, the pages in them
//! and the time (in all and the longest); how long they waited for the lock of
//! the views, and for the readers of title memory in place that the other
//! lock keeps them from; the host's own calls under them; and who those
//! readers are. Counted always, a few atomics a call: the title's thread that
//! commits a page spins the others for as long as its call takes.

use super::*;

#[derive(Clone, Copy)]
pub(super) enum Kind {
    Map,
    Unmap,
    Scatter,
    Alias,
    VirtualAlloc,
    VirtualFree,
}

const KINDS: [&str; 6] = [
    "map pages",
    "unmap pages",
    "scatter",
    "alias maps",
    "virtual alloc",
    "virtual free",
];

const fn zeros<const N: usize>() -> [AtomicU64; N] {
    [const { AtomicU64::new(0) }; N]
}

static CALLS: [AtomicU64; 6] = zeros();
static PAGES: [AtomicU64; 6] = zeros();
static NS: [AtomicU64; 6] = zeros();
static LONGEST: [AtomicU64; 6] = zeros();
/// Count, ns and longest ns waiting for the views' lock.
static VIEWS: [AtomicU64; 3] = zeros();
/// Changes of mapping that took the readers' lock, and that did not.
static LOCKED: [AtomicU64; 2] = zeros();
/// By host call (unmap, split, map, commit): count, ns, longest ns.
static HOST: [AtomicU64; 12] = zeros();
static LAST: AtomicU64 = AtomicU64::new(0);
static START: LazyLock<std::time::Instant> = LazyLock::new(std::time::Instant::now);

fn on() -> bool {
    static ON: LazyLock<bool> =
        LazyLock::new(|| std::env::var_os("VICTORY_MEMORY_STATS").is_none_or(|v| v != "0"));
    *ON
}

/// A call waited `d` for the views' lock.
pub(super) fn waited_for_views(d: std::time::Duration) {
    let ns = d.as_nanos() as u64;
    VIEWS[0].fetch_add(1, Ordering::Relaxed);
    VIEWS[1].fetch_add(ns, Ordering::Relaxed);
    VIEWS[2].fetch_max(ns, Ordering::Relaxed);
}

/// A change of mapping took the readers' lock (`true`) or went without.
pub(super) fn locked(took: bool) {
    LOCKED[usize::from(!took)].fetch_add(1, Ordering::Relaxed);
}

/// A call to the host, `ns` long.
pub(super) fn host(call: Call, ns: u64) {
    let i = match call {
        Call::Unmap => 0,
        Call::Split => 1,
        Call::Map => 2,
        Call::Commit => 3,
    } * 3;
    HOST[i].fetch_add(1, Ordering::Relaxed);
    HOST[i + 1].fetch_add(ns, Ordering::Relaxed);
    HOST[i + 2].fetch_max(ns, Ordering::Relaxed);
}

/// A call of the title's, counted when it ends.
pub(super) struct Timed {
    kind: Kind,
    pages: usize,
    started: std::time::Instant,
}

impl Timed {
    pub(super) fn begin(kind: Kind, pages: usize) -> Timed {
        Timed {
            kind,
            pages,
            started: std::time::Instant::now(),
        }
    }
}

impl Drop for Timed {
    fn drop(&mut self) {
        let (i, ns) = (self.kind as usize, self.started.elapsed().as_nanos() as u64);
        CALLS[i].fetch_add(1, Ordering::Relaxed);
        PAGES[i].fetch_add(self.pages as u64, Ordering::Relaxed);
        NS[i].fetch_add(ns, Ordering::Relaxed);
        LONGEST[i].fetch_max(ns, Ordering::Relaxed);
        report_if_due();
    }
}

/// The numbers of one window, taken.
struct Window {
    calls: Vec<u64>,
    pages: Vec<u64>,
    ns: Vec<u64>,
    longest: Vec<u64>,
    views: Vec<u64>,
    locked: Vec<u64>,
    host: Vec<u64>,
}

fn take(of: &[AtomicU64]) -> Vec<u64> {
    of.iter().map(|c| c.swap(0, Ordering::Relaxed)).collect()
}

fn ms(ns: u64) -> f64 {
    ns as f64 / 1e6
}

fn report_if_due() {
    let now = START.elapsed().as_millis() as u64;
    let last = LAST.load(Ordering::Relaxed);
    if now < last + 5000
        || LAST
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
    {
        return;
    }
    let seconds = if last == 0 {
        now as f64 / 1e3
    } else {
        (now - last) as f64 / 1e3
    };
    let readers = victory_gpu::mapping_stats::take();
    let window = Window {
        calls: take(&CALLS),
        pages: take(&PAGES),
        ns: take(&NS),
        longest: take(&LONGEST),
        views: take(&VIEWS),
        locked: take(&LOCKED),
        host: take(&HOST),
    };
    if on() && window.calls.iter().any(|&n| n > 0) {
        let (calls, host) = (by_call(&window), host_calls(&window));
        tracing::info!(
            "title memory calls, last {seconds:.1} s (VICTORY_MEMORY_STATS){calls}; waited for the views' lock {} times {:.0} ms (longest {:.1}); {}; host calls under them: {host}",
            window.views[0],
            ms(window.views[1]),
            ms(window.views[2]),
            readers_line(&window, &readers),
        );
    }
}

fn by_call(w: &Window) -> String {
    use std::fmt::Write;
    let mut text = String::new();
    for (i, name) in KINDS.iter().enumerate().filter(|(i, _)| w.calls[*i] > 0) {
        let _ = write!(
            text,
            "; {name} {} calls, {} pages, {:.0} ms ({:.0} us a call, longest {:.1} ms)",
            w.calls[i],
            w.pages[i],
            ms(w.ns[i]),
            w.ns[i] as f64 / 1e3 / w.calls[i] as f64,
            ms(w.longest[i])
        );
    }
    text
}

fn host_calls(w: &Window) -> String {
    let list: Vec<String> = ["unmap", "split", "map", "commit"]
        .iter()
        .enumerate()
        .filter(|(i, _)| w.host[i * 3] > 0)
        .map(|(i, name)| {
            format!(
                "{name} {}x {:.0} ms (longest {:.1})",
                w.host[i * 3],
                ms(w.host[i * 3 + 1]),
                ms(w.host[i * 3 + 2])
            )
        })
        .collect();
    list.join(", ")
}

fn readers_line(w: &Window, r: &victory_gpu::mapping_stats::Taken) -> String {
    let by_site: Vec<String> = r
        .readers
        .iter()
        .take(5)
        .map(|(at, n, ns, max)| format!("{at} {n}x {:.0} ms (longest {:.1})", ms(*ns), ms(*max)))
        .collect();
    format!(
        "changes of mapping that took the readers' lock {} (waited for readers {:.0} ms, longest {:.1}; held {:.0} ms, longest {:.1}), \
         without it {} (VICTORY_MAP_FRESH); readers of title memory in place waited for a change {} times {:.0} ms (longest {:.1}), \
         {} turned away, held {:.0} ms in all, by {}",
        w.locked[0],
        ms(r.change_waits[1]),
        ms(r.change_waits[2]),
        ms(r.change_holds[1]),
        ms(r.change_holds[2]),
        w.locked[1],
        r.reader_waits[0],
        ms(r.reader_waits[1]),
        ms(r.reader_waits[2]),
        r.refused,
        ms(r.readers.iter().map(|x| x.2).sum()),
        by_site.join(", "),
    )
}

/// The window's numbers as the line works them out, for the test.
#[cfg(test)]
pub(super) fn describe(calls: u64, pages: u64, ns: u64, longest: u64) -> String {
    let mut w = Window {
        calls: vec![0; 6],
        pages: vec![0; 6],
        ns: vec![0; 6],
        longest: vec![0; 6],
        views: vec![0; 3],
        locked: vec![0; 2],
        host: vec![0; 12],
    };
    (w.calls[0], w.pages[0], w.ns[0], w.longest[0]) = (calls, pages, ns, longest);
    by_call(&w)
}
