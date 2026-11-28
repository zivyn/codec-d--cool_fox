//! What the faults that reach our vectored handler cost (`super::install`), by kind, by thread and by block, for the
//! log of a diagnostics build (`faults taken in the vectored handler per frame`): the number a frame, the wall time
//! and the processor time of the thread inside the handler (cycles of the thread, so a wait is not in them), what it
//! waited for the guards' stripes and held them, and which threads held the stripes it waited for.
//!
//! Without diagnostics this is a handful of empty functions.

/// What a trip into the handler turned out to be.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum Kind {
    /// A guard page the lazy landing armed (`VICTORY_LAND_LAZY`) or the read probe, answered.
    Owed,
    /// A fault of our own copy (`crate::safecopy`), which fails and goes on.
    Own,
    /// A read of, or a write to, a view the runtime was changing.
    Back,
    /// A write of the title's done by us beside the guard (`VICTORY_STORE_BESIDE`).
    Store,
    /// A write that opened blocks.
    Open,
    /// A write to a block that was open already when the handler looked.
    Seen,
    /// Not ours: passed on to the handlers after this one (illegal instructions, the title's own exceptions, ...).
    Passed,
}

#[cfg(not(feature = "diagnostics"))]
pub(super) struct Probe;

#[cfg(not(feature = "diagnostics"))]
#[allow(dead_code)]
impl Probe {
    #[inline(always)]
    pub(super) fn begin() -> Probe {
        Probe
    }
    #[inline(always)]
    pub(super) fn is(&mut self, _kind: Kind) {}
    #[inline(always)]
    pub(super) fn code(&mut self, _code: u32) {}
    #[inline(always)]
    pub(super) fn waited(&mut self, _ns: u64) {}
    #[inline(always)]
    pub(super) fn held(&mut self, _ns: u64) {}
    #[inline(always)]
    pub(super) fn opened(&mut self, _blocks: usize, _beside: usize) {}
    #[inline(always)]
    pub(super) fn heat(&mut self, _block: usize, _frame: u64, _before: Option<u32>) {}
}

/// A look at a stripe that was busy, by a thread in the handler: it looked for `d`, and got it or not.
#[cfg(not(feature = "diagnostics"))]
#[inline(always)]
pub(super) fn spun(_got_it: bool, _d: std::time::Duration) {}

#[cfg(feature = "diagnostics")]
pub(super) use on::{Probe, held, holder, lines, spun, waited_on};

#[cfg(feature = "diagnostics")]
mod on {
    use super::Kind;
    use std::sync::atomic::{AtomicU32, AtomicU64, Ordering::Relaxed};
    use std::time::{Duration, Instant};

    const KINDS: usize = 7;
    const NAMES: [&str; KINDS] = [
        "an owed page landed",
        "a fault of our own copy",
        "a view the runtime changed",
        "a store done beside the guard",
        "a write that opened blocks",
        "a write to a block open already",
        "passed on to the handlers after ours",
    ];

    /// Per kind: faults, wall ns, thread cycles, ns waiting for the guards, ns holding them, blocks opened.
    static KIND: [[AtomicU64; 6]; KINDS] = [const { [const { AtomicU64::new(0) }; 6] }; KINDS];

    /// Per thread (by a hash of its id): id, faults, wall ns, ns waiting for the guards, thread cycles.
    const THREADS: usize = 128;
    #[allow(clippy::type_complexity)]
    static THREAD: [(AtomicU32, AtomicU64, AtomicU64, AtomicU64, AtomicU64); THREADS] = [const {
        (
            AtomicU32::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
            AtomicU64::new(0),
        )
    };
        THREADS];

    /// The exceptions passed on: code, number, wall ns.
    const CODES: usize = 16;
    static CODE: [(AtomicU32, AtomicU64, AtomicU64); CODES] =
        [const { (AtomicU32::new(0), AtomicU64::new(0), AtomicU64::new(0)) }; CODES];

    /// How long before a block opened by a fault had been opened by one before: never, this frame, the one before, up
    /// to 4 frames, up to 16, longer ago.
    static AGES: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];
    /// Faults whose block before it, whose block after it, and either of them, was opened in this frame already.
    static NEAR: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];
    /// Blocks opened beside the one that faulted (`VICTORY_FAULT_AROUND`).
    static BESIDE: AtomicU64 = AtomicU64::new(0);

    /// The thread that took each stripe last (which is the one that holds it when it is busy).
    static HOLDER: [AtomicU32; 64] = [const { AtomicU32::new(0) }; 64];
    /// Faults that waited for a stripe, by the thread that held it: id, waits, ns.
    const HOLDERS: usize = 16;
    static HELD_BY: [(AtomicU32, AtomicU64, AtomicU64); HOLDERS] =
        [const { (AtomicU32::new(0), AtomicU64::new(0), AtomicU64::new(0)) }; HOLDERS];
    /// Looks for a busy stripe (`VICTORY_FAULT_SPIN`): found free, not found in time, ns spent.
    static SPIN: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

    fn tid() -> u32 {
        // SAFETY: a question about this thread.
        unsafe { windows_sys::Win32::System::Threading::GetCurrentThreadId() }
    }

    /// The cycles this thread has run.
    fn cycles() -> u64 {
        use windows_sys::Win32::System::Threading::GetCurrentThread;
        use windows_sys::Win32::System::WindowsProgramming::QueryThreadCycleTime;
        let mut cycles = 0u64;
        // SAFETY: the pseudo handle of this thread, and an out pointer of the right size.
        unsafe { QueryThreadCycleTime(GetCurrentThread(), &mut cycles) };
        cycles
    }

    /// Thread cycles in a nanosecond (the cycles are counted at the rate of the timestamp counter).
    fn ghz() -> f64 {
        static GHZ: std::sync::OnceLock<f64> = std::sync::OnceLock::new();
        *GHZ.get_or_init(|| {
            let (before, began) = (cycles(), Instant::now());
            while began.elapsed() < Duration::from_millis(3) {
                std::hint::spin_loop();
            }
            let ns = began.elapsed().as_nanos().max(1) as f64;
            (cycles().saturating_sub(before) as f64 / ns).max(0.1)
        })
    }

    /// A trip into the handler, from the moment it begins until it is dropped.
    pub(in crate::watch) struct Probe {
        began: Instant,
        cycles: u64,
        kind: Kind,
        code: u32,
        wait: u64,
        hold: u64,
        blocks: u64,
    }

    impl Probe {
        pub(in crate::watch) fn begin() -> Probe {
            Probe {
                began: Instant::now(),
                cycles: cycles(),
                kind: Kind::Passed,
                code: 0,
                wait: 0,
                hold: 0,
                blocks: 0,
            }
        }

        pub(in crate::watch) fn is(&mut self, kind: Kind) {
            self.kind = kind;
        }

        /// What the exception was, if it is passed on.
        pub(in crate::watch) fn code(&mut self, code: u32) {
            self.code = code;
        }

        pub(in crate::watch) fn waited(&mut self, ns: u64) {
            self.wait += ns;
        }

        pub(in crate::watch) fn held(&mut self, ns: u64) {
            self.hold += ns;
        }

        /// Blocks opened, of which `beside` for being beside the one that faulted.
        pub(in crate::watch) fn opened(&mut self, blocks: usize, beside: usize) {
            self.blocks += blocks as u64;
            BESIDE.fetch_add(beside as u64, Relaxed);
        }

        /// The fault opened `block` in frame `frame`; `before`: the age of the last note of it (none: never).
        pub(in crate::watch) fn heat(&mut self, block: usize, frame: u64, before: Option<u32>) {
            let slot = match before {
                None => 0,
                Some(0) => 1,
                Some(1) => 2,
                Some(2..=4) => 3,
                Some(5..=16) => 4,
                Some(_) => 5,
            };
            AGES[slot].fetch_add(1, Relaxed);
            let this_frame = |block: usize| super::super::lean::age(block, frame) == Some(0);
            let (low, high) = (block > 0 && this_frame(block - 1), this_frame(block + 1));
            NEAR[0].fetch_add(u64::from(low), Relaxed);
            NEAR[1].fetch_add(u64::from(high), Relaxed);
            NEAR[2].fetch_add(u64::from(low || high), Relaxed);
        }
    }

    impl Drop for Probe {
        fn drop(&mut self) {
            let wall = self.began.elapsed().as_nanos() as u64;
            let cycles = cycles().saturating_sub(self.cycles);
            let kind = &KIND[self.kind as usize];
            for (counter, add) in
                kind.iter()
                    .zip([1, wall, cycles, self.wait, self.hold, self.blocks])
            {
                counter.fetch_add(add, Relaxed);
            }
            if self.kind == Kind::Passed {
                let code = self.code.max(1);
                if let Some(at) = (0..CODES).find(|&at| {
                    let there = CODE[at].0.load(Relaxed);
                    there == code
                        || (there == 0
                            && (CODE[at]
                                .0
                                .compare_exchange(0, code, Relaxed, Relaxed)
                                .is_ok()
                                || CODE[at].0.load(Relaxed) == code))
                }) {
                    CODE[at].1.fetch_add(1, Relaxed);
                    CODE[at].2.fetch_add(wall, Relaxed);
                }
            }
            let me = tid().max(1);
            let start = me as usize % THREADS;
            for step in 0..8 {
                let slot = &THREAD[(start + step) % THREADS];
                let there = slot.0.load(Relaxed);
                if there == me
                    || (there == 0
                        && (slot.0.compare_exchange(0, me, Relaxed, Relaxed).is_ok()
                            || slot.0.load(Relaxed) == me))
                {
                    slot.1.fetch_add(1, Relaxed);
                    slot.2.fetch_add(wall, Relaxed);
                    slot.3.fetch_add(self.wait, Relaxed);
                    slot.4.fetch_add(cycles, Relaxed);
                    break;
                }
            }
        }
    }

    /// The calling thread took stripe `stripe`.
    pub(in crate::watch) fn held(stripe: usize) {
        HOLDER[stripe % HOLDER.len()].store(tid(), Relaxed);
    }

    /// The thread that took stripe `stripe` last.
    pub(in crate::watch) fn holder(stripe: usize) -> u32 {
        HOLDER[stripe % HOLDER.len()].load(Relaxed)
    }

    /// A thread in the handler waited `d` for a stripe that `holder` had taken.
    pub(in crate::watch) fn waited_on(holder: u32, d: Duration) {
        if !super::super::lean::faulting() {
            return;
        }
        let key = holder.max(1);
        let start = key as usize % HOLDERS;
        for step in 0..HOLDERS {
            let at = (start + step) % HOLDERS;
            let there = HELD_BY[at].0.load(Relaxed);
            if there == key
                || (there == 0
                    && (HELD_BY[at]
                        .0
                        .compare_exchange(0, key, Relaxed, Relaxed)
                        .is_ok()
                        || HELD_BY[at].0.load(Relaxed) == key))
            {
                HELD_BY[at].1.fetch_add(1, Relaxed);
                HELD_BY[at].2.fetch_add(d.as_nanos() as u64, Relaxed);
                return;
            }
        }
    }

    /// A thread in the handler looked for a busy stripe for `d`, and got it (or not, and slept on it).
    pub(in crate::watch) fn spun(got_it: bool, d: Duration) {
        SPIN[usize::from(!got_it)].fetch_add(1, Relaxed);
        SPIN[2].fetch_add(d.as_nanos() as u64, Relaxed);
    }

    /// What thread `tid` is, by the census: its group, and whether it is the title's.
    fn group(tid: u32) -> String {
        match crate::threadcpu::census::group_of_tid(tid) {
            Some((group, true)) => format!("{group} (title)"),
            Some((group, false)) => group,
            None => "?".into(),
        }
    }

    /// The lines of the log, per frame: what each kind of trip into the handler cost since the last, and the rest.
    pub(in crate::watch) fn lines(frames: f64) -> Vec<String> {
        let frames = frames.max(1.0);
        let ghz = ghz();
        let mut out = Vec::new();
        let mut kinds = Vec::new();
        for (name, counters) in NAMES.iter().zip(&KIND) {
            let v: [u64; 6] = std::array::from_fn(|i| counters[i].swap(0, Relaxed));
            if v[0] == 0 {
                continue;
            }
            let n = v[0] as f64;
            let us = |ns: u64| ns as f64 / 1e3 / n;
            kinds.push(format!(
                "{name}: {:.1} a frame, {:.2} ms a frame (each {:.1} us wall, {:.1} us of processor time, {:.1} us waiting for the guards, {:.1} us holding them, {:.2} blocks opened)",
                n / frames,
                v[1] as f64 / 1e6 / frames,
                us(v[1]),
                v[2] as f64 / ghz / 1e3 / n,
                us(v[3]),
                us(v[4]),
                v[5] as f64 / n
            ));
        }
        out.push(format!(
            "faults taken in the vectored handler per frame (diagnostics; thread cycles at {ghz:.2} GHz): {}",
            if kinds.is_empty() { "none".to_string() } else { kinds.join("; ") }
        ));
        let mut threads: Vec<(u32, u64, u64, u64, u64)> = THREAD
            .iter()
            .map(|t| {
                (
                    t.0.load(Relaxed),
                    t.1.swap(0, Relaxed),
                    t.2.swap(0, Relaxed),
                    t.3.swap(0, Relaxed),
                    t.4.swap(0, Relaxed),
                )
            })
            .filter(|t| t.1 > 0)
            .collect();
        threads.sort_unstable_by_key(|t| std::cmp::Reverse(t.2));
        let (mut title, mut all) = ((0u64, 0u64, 0u64), (0u64, 0u64, 0u64));
        let rows: Vec<String> = threads
            .iter()
            .enumerate()
            .map(|(i, t)| {
                let name = group(t.0);
                all = (all.0 + t.1, all.1 + t.2, all.2 + t.3);
                if name.ends_with("(title)") {
                    title = (title.0 + t.1, title.1 + t.2, title.2 + t.3);
                }
                (i < 8).then(|| {
                    format!(
                        "{} {name} {:.1} faults, {:.2} ms ({:.2} waiting, {:.2} of processor time)",
                        t.0,
                        t.1 as f64 / frames,
                        t.2 as f64 / 1e6 / frames,
                        t.3 as f64 / 1e6 / frames,
                        t.4 as f64 / ghz / 1e6 / frames
                    )
                })
            })
            .flatten()
            .collect();
        let ages: [u64; 6] = std::array::from_fn(|i| AGES[i].swap(0, Relaxed));
        let near: [u64; 3] = std::array::from_fn(|i| NEAR[i].swap(0, Relaxed));
        let beside = BESIDE.swap(0, Relaxed);
        let opened: u64 = ages.iter().sum();
        let pct = |n: u64| 100.0 * n as f64 / opened.max(1) as f64;
        out.push(format!(
            "faults by thread per frame, in all {:.1} faults {:.2} ms ({:.2} waiting for the guards), of the title's threads {:.1} faults {:.2} ms ({:.2} waiting); the busiest: {}; \
             the block opened had been opened before: never {:.0}% of the {:.1} a frame, this frame {:.0}%, one frame ago {:.0}%, 2-4 ago {:.0}%, 5-16 {:.0}%, longer {:.0}%; \
             the block before it opened in this frame already {:.0}%, the one after {:.0}%, either {:.0}% (what VICTORY_FAULT_AROUND can open together); {:.1} blocks a frame opened beside the one that faulted",
            all.0 as f64 / frames,
            all.1 as f64 / 1e6 / frames,
            all.2 as f64 / 1e6 / frames,
            title.0 as f64 / frames,
            title.1 as f64 / 1e6 / frames,
            title.2 as f64 / 1e6 / frames,
            rows.join(", "),
            pct(ages[0]),
            opened as f64 / frames,
            pct(ages[1]),
            pct(ages[2]),
            pct(ages[3]),
            pct(ages[4]),
            pct(ages[5]),
            pct(near[0]),
            pct(near[1]),
            pct(near[2]),
            beside as f64 / frames
        ));
        let mut codes: Vec<(u32, u64, u64)> = CODE
            .iter()
            .map(|c| {
                (
                    c.0.load(Relaxed),
                    c.1.swap(0, Relaxed),
                    c.2.swap(0, Relaxed),
                )
            })
            .filter(|c| c.1 > 0)
            .collect();
        codes.sort_unstable_by_key(|c| std::cmp::Reverse(c.2));
        let mut held: Vec<(u32, u64, u64)> = HELD_BY
            .iter()
            .map(|h| {
                (
                    h.0.load(Relaxed),
                    h.1.swap(0, Relaxed),
                    h.2.swap(0, Relaxed),
                )
            })
            .filter(|h| h.1 > 0)
            .collect();
        held.sort_unstable_by_key(|h| std::cmp::Reverse(h.2));
        let [got, slept, ns] = [0, 1, 2].map(|i| SPIN[i].swap(0, Relaxed));
        out.push(format!(
            "exceptions passed on, per frame (code, a frame, ms a frame): {}; stripes the faults waited for, by the thread that held them (thread, waits a frame, ms a frame): {}; \
             looks for a busy stripe (VICTORY_FAULT_SPIN={}): {:.1} found it free, {:.1} gave up, {:.2} ms a frame spent",
            codes.iter().take(6).map(|c| format!("{:#x} {:.1} {:.2}", c.0, c.1 as f64 / frames, c.2 as f64 / 1e6 / frames)).collect::<Vec<_>>().join(", "),
            held.iter().take(6).map(|h| format!("{} {} {:.1} {:.2}", h.0, group(h.0), h.1 as f64 / frames, h.2 as f64 / 1e6 / frames)).collect::<Vec<_>>().join(", "),
            super::super::lean::spin_us(),
            got as f64 / frames,
            slept as f64 / frames,
            ns as f64 / 1e6 / frames
        ));
        out
    }
}
