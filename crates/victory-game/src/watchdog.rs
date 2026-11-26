//! The title's own hang watchdog, and how our stalls are kept from tripping it.
//!
//! The title runs a thread that wakes once a second. It ends the process on
//! purpose (a store through a null pointer, which the console catches and a PC
//! does not) when its heartbeat, a clock value its main loop writes, has not
//! moved for 30 wakes, and it checks nothing while a pause byte is set. The
//! title itself sets that byte when the system suspends it and clears it when it
//! resumes (there is a second byte, the "ping", that makes it forget the count
//! so far). A sleep that ran over two seconds (a debugger stop, the whole
//! process frozen) also restarts the count, so only a stall of the title's
//! threads alone, with the watchdog thread running, trips it: a frame
//! recording that keeps the title's submitting thread for tens of seconds, host
//! pipelines made on its threads, a first-use wait for the ray-tracing
//! builder's programs.
//!
//! Our layer takes a [`hold`] around such a stall. While any hold lives the
//! pause byte is kept set; when the last one ends the byte goes back to what it
//! was and the ping is set, so the count starts again. A hold is no licence to
//! hide a real hang: when the heartbeat has not moved for
//! [`DEFAULT_MAX`] (`VICTORY_WATCHDOG_HOLD_MAX_S`) of holding the byte is
//! given back and the watchdog runs as the title made it. `VICTORY_WATCHDOG_HOLD=0`
//! turns all of this off.
//!
//! The three bytes are found by short byte patterns of the title's code, not
//! by offsets, so a build that has the same code is covered, and a build that
//! does not is left untouched. What is not found or not unique is not used.

use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, OnceLock};
use std::time::{Duration, Instant};

/// How long the pause is kept while the heartbeat does not move.
pub const DEFAULT_MAX: Duration = Duration::from_secs(900);

/// How often a hold looks at the bytes and the clock.
const TICK: Duration = Duration::from_millis(250);

/// A hold that lasted this long is logged when it ends.
const LOGGED: Duration = Duration::from_secs(5);

/// A pattern: what must be there (`None`: anything).
type Pattern = &'static [Option<u8>];

macro_rules! pattern {
    ($($b:tt),* $(,)?) => { &[$(pattern!(@ $b)),*] };
    (@ _) => { None };
    (@ $b:literal) => { Some($b) };
}

/// The handler of the title's two lifecycle events (suspend, resume): the
/// first `mov` (a 4-byte offset after `0xc6 0x05`, then 1) sets the ping, the
/// second (`0x88 0x05`) stores the pause byte.
const HANDLER: Pattern = pattern![
    0x48, 0x85, 0xc9, 0x74, _, 0x8b, 0x41, 0x0c, 0x83, 0xf8, 0x24, 0x74, _, 0x83, 0xf8, 0x25, 0x75,
    _, 0x31, 0xc0, 0xc6, 0x05, _, _, _, _, 0x01, 0xeb, 0x02, 0xb0, 0x01, 0x88, 0x05, _, _, _, _,
    0xc3
];
/// Where the ping's offset begins in [`HANDLER`], and the pause's.
const PING_AT: usize = 22;
const PAUSE_AT: usize = 33;

/// The heartbeat: reads the clock and stores it in a global.
const BEAT: Pattern = pattern![
    0x56, 0x48, 0x83, 0xec, 0x30, 0x48, 0x8d, 0x74, 0x24, 0x28, 0x48, 0x89, 0xf1, 0xff, 0x15, _, _,
    _, _, 0x48, 0x8b, 0x06, 0x48, 0x89, 0x05, _, _, _, _, 0x48, 0x83, 0xc4, 0x30, 0x5e, 0xc3
];
const BEAT_AT: usize = 25;

/// Where the title's bytes are, as offsets from its image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sites {
    /// Set: the watchdog checks nothing.
    pub pause: u32,
    /// Set: the watchdog forgets the time that has passed.
    pub ping: u32,
    /// The clock value the main loop stores (a `u64`), if its code was found.
    pub heartbeat: Option<u32>,
}

/// The offsets of every place in `code` (at `rva`) that matches `pattern`,
/// by its longest fixed run.
fn matches(code: &[u8], pattern: Pattern) -> Vec<usize> {
    let (mut best, mut run) = ((0, 0), (0, 0));
    for (i, b) in pattern.iter().enumerate() {
        if b.is_some() {
            run = if run.1 == 0 {
                (i, 1)
            } else {
                (run.0, run.1 + 1)
            };
            if run.1 > best.1 {
                best = run;
            }
        } else {
            run = (0, 0);
        }
    }
    let needle: Vec<u8> = pattern[best.0..best.0 + best.1]
        .iter()
        .flatten()
        .copied()
        .collect();
    memchr::memmem::find_iter(code, &needle)
        .filter_map(|hit| hit.checked_sub(best.0))
        .filter(|&at| {
            code.get(at..at + pattern.len()).is_some_and(|w| {
                w.iter()
                    .zip(pattern)
                    .all(|(b, p)| p.is_none_or(|p| p == *b))
            })
        })
        .collect()
}

/// Where the instruction whose 4-byte offset begins at `at` points, an
/// offset from the image (`end`: where the instruction ends).
fn target(code: &[u8], at: usize, end: usize, rva: u32) -> Option<u32> {
    let disp = i32::from_le_bytes(code.get(at..at + 4)?.try_into().ok()?);
    u32::try_from(i64::from(rva) + end as i64 + i64::from(disp)).ok()
}

/// The bytes' places, from the executable sections `(rva, bytes)` of the
/// image; `None` unless the handler is found exactly once.
pub fn locate(sections: &[(u32, &[u8])]) -> Option<Sites> {
    let (mut handler, mut beat) = (None, Vec::new());
    let mut handlers = 0;
    for &(rva, code) in sections {
        for at in matches(code, HANDLER) {
            handlers += 1;
            // `mov byte ptr [rip+d], 1` is seven bytes, `mov [rip+d], al` six.
            let ping = target(code, at + PING_AT, at + PING_AT + 5, rva)?;
            let pause = target(code, at + PAUSE_AT, at + PAUSE_AT + 4, rva)?;
            handler = Some((pause, ping));
        }
        for at in matches(code, BEAT) {
            beat.extend(target(code, at + BEAT_AT, at + BEAT_AT + 4, rva));
        }
    }
    let (pause, ping) = handler.filter(|_| handlers == 1)?;
    let heartbeat = match beat[..] {
        [at] if at % 8 == 0 => Some(at),
        _ => None,
    };
    Some(Sites {
        pause,
        ping,
        heartbeat,
    })
}

#[derive(Debug)]
struct State {
    holds: u32,
    /// Our pause is in the byte (what the title had there is `prior`).
    applied: bool,
    prior: u8,
    /// The heartbeat did not move for `max` of holding: the byte is given back
    /// until every hold has ended.
    expired: bool,
    /// The last heartbeat seen, and since when it is the same.
    beat: u64,
    since: Instant,
    stop: bool,
}

#[derive(Debug)]
struct Inner {
    pause: usize,
    ping: usize,
    heartbeat: Option<usize>,
    max: Duration,
    tick: Duration,
    state: Mutex<State>,
    wake: Condvar,
}

impl Inner {
    fn state(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|e| e.into_inner())
    }

    fn pause(&self) -> &AtomicU8 {
        // SAFETY: `Keeper::new`'s contract: a byte that stays mapped.
        unsafe { AtomicU8::from_ptr(self.pause as *mut u8) }
    }

    fn ping(&self) -> &AtomicU8 {
        // SAFETY: as `pause`.
        unsafe { AtomicU8::from_ptr(self.ping as *mut u8) }
    }

    fn beat(&self) -> u64 {
        // SAFETY: `Keeper::new`'s contract: eight aligned bytes that stay mapped.
        self.heartbeat.map_or(0, |at| {
            unsafe { AtomicU64::from_ptr(at as *mut u64) }.load(Ordering::Relaxed)
        })
    }

    /// The pause given back: what the title had, and a ping unless `quiet`.
    fn give_back(&self, s: &mut State, ping: bool) {
        if s.applied {
            if s.prior == 0 {
                self.pause().store(0, Ordering::SeqCst);
            }
            if ping {
                self.ping().store(1, Ordering::SeqCst);
            }
            s.applied = false;
        }
    }

    fn release(&self) {
        let mut s = self.state();
        s.holds -= 1;
        if s.holds == 0 {
            let ping = !s.expired;
            self.give_back(&mut s, ping);
            s.expired = false;
        }
    }

    /// One look while held: the byte is set again if the title cleared it, and
    /// the pause is given back after `max` without a heartbeat.
    fn look(&self, s: &mut State) {
        if !s.applied {
            return;
        }
        let beat = self.beat();
        if beat != s.beat {
            (s.beat, s.since) = (beat, Instant::now());
        }
        if s.since.elapsed() >= self.max {
            tracing::warn!(
                held_s = s.since.elapsed().as_secs(),
                "the title's watchdog is released: its heartbeat has not moved while we held it"
            );
            s.expired = true;
            self.give_back(s, false);
        } else if self.pause().load(Ordering::Relaxed) == 0 {
            self.pause().store(1, Ordering::SeqCst);
        }
    }

    fn keep(self: &Arc<Self>) {
        let mut s = self.state();
        while !s.stop {
            if s.holds == 0 {
                s = self.wake.wait(s).unwrap_or_else(|e| e.into_inner());
                continue;
            }
            self.look(&mut s);
            s = self
                .wake
                .wait_timeout(s, self.tick)
                .unwrap_or_else(|e| e.into_inner())
                .0;
        }
    }
}

/// Keeps the title's pause byte while a hold lives. One per process
/// ([`arm`]); tests make their own over memory of theirs.
#[derive(Debug)]
pub struct Keeper(Arc<Inner>);

impl Keeper {
    /// # Safety
    /// `pause` and `ping` are bytes, and `heartbeat` (if any) eight bytes
    /// aligned to eight, which stay mapped and writable for as long as the
    /// keeper or a hold of it lives.
    pub unsafe fn new(
        pause: usize,
        ping: usize,
        heartbeat: Option<usize>,
        max: Duration,
    ) -> Keeper {
        // SAFETY: this function's contract.
        unsafe { Keeper::with_tick(pause, ping, heartbeat, max, TICK) }
    }

    /// # Safety
    /// As [`Keeper::new`].
    unsafe fn with_tick(
        pause: usize,
        ping: usize,
        heartbeat: Option<usize>,
        max: Duration,
        tick: Duration,
    ) -> Keeper {
        let inner = Arc::new(Inner {
            pause,
            ping,
            heartbeat,
            max,
            tick,
            state: Mutex::new(State {
                holds: 0,
                applied: false,
                prior: 0,
                expired: false,
                beat: 0,
                since: Instant::now(),
                stop: false,
            }),
            wake: Condvar::new(),
        });
        let keeper = Arc::clone(&inner);
        let spawned = std::thread::Builder::new()
            .name("watchdog-hold".into())
            .spawn(move || keeper.keep());
        if let Err(e) = spawned {
            tracing::warn!("the title's watchdog hold has no thread to keep it: {e}");
        }
        Keeper(inner)
    }

    /// The title's watchdog pauses until the hold is dropped (and every other
    /// one).
    pub fn hold(&self, reason: &'static str) -> Hold {
        let inner = &self.0;
        let mut s = inner.state();
        s.holds += 1;
        if s.holds == 1 {
            s.prior = inner.pause().load(Ordering::SeqCst);
            (s.beat, s.since, s.expired) = (inner.beat(), Instant::now(), false);
            s.applied = true;
            inner.pause().store(1, Ordering::SeqCst);
            inner.wake.notify_all();
        } else if s.applied && inner.pause().load(Ordering::Relaxed) == 0 {
            inner.pause().store(1, Ordering::SeqCst);
        }
        Hold {
            inner: Some(Arc::clone(inner)),
            reason,
            began: Instant::now(),
        }
    }
}

impl Drop for Keeper {
    fn drop(&mut self) {
        let mut s = self.0.state();
        s.stop = true;
        self.0.wake.notify_all();
    }
}

/// While it lives, the title's watchdog does not count (see [`Keeper::hold`]).
#[must_use = "the watchdog counts again when the hold is dropped"]
pub struct Hold {
    inner: Option<Arc<Inner>>,
    reason: &'static str,
    began: Instant,
}

impl Drop for Hold {
    fn drop(&mut self) {
        if let Some(inner) = self.inner.take() {
            inner.release();
            if self.began.elapsed() >= LOGGED {
                tracing::info!(
                    reason = self.reason,
                    seconds = self.began.elapsed().as_secs_f32(),
                    "the title's watchdog was held"
                );
            }
        }
    }
}

static KEEPER: OnceLock<Keeper> = OnceLock::new();

/// Whether `VICTORY_WATCHDOG_HOLD` leaves the holds on (they are, unless it is `0`).
fn enabled() -> bool {
    std::env::var_os("VICTORY_WATCHDOG_HOLD").is_none_or(|v| v != "0")
}

/// How long the pause is kept without a heartbeat (`VICTORY_WATCHDOG_HOLD_MAX_S`).
fn max() -> Duration {
    std::env::var("VICTORY_WATCHDOG_HOLD_MAX_S")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .map_or(DEFAULT_MAX, Duration::from_secs)
}

/// Finds the watchdog's bytes in the title's mapped image and starts keeping
/// them. `code` and `data` are the image's executable and writable sections as
/// `(rva, length)`. Returns whether holds will do anything.
///
/// # Safety
/// `base` is the title's image, mapped and relocated with those sections, and
/// it stays mapped for the rest of the process.
pub unsafe fn arm(base: usize, code: &[(u32, u32)], data: &[(u32, u32)]) -> bool {
    if !enabled() || KEEPER.get().is_some() {
        return false;
    }
    let sections: Vec<(u32, &[u8])> = code
        .iter()
        // SAFETY: a mapped section of the image, by this function's contract.
        .map(|&(rva, len)| {
            (rva, unsafe {
                std::slice::from_raw_parts((base + rva as usize) as *const u8, len as usize)
            })
        })
        .collect();
    let Some(sites) = locate(&sections) else {
        tracing::info!(
            "the title's watchdog was not found: our stalls are not held for it (another build)"
        );
        return false;
    };
    let writable = |rva: u32, len: u32| data.iter().any(|&(d, n)| rva >= d && rva + len <= d + n);
    if !writable(sites.pause, 1) || !writable(sites.ping, 1) {
        tracing::warn!(
            ?sites,
            "the title's watchdog bytes are not in writable data: left alone"
        );
        return false;
    }
    let heartbeat = sites
        .heartbeat
        .filter(|&h| writable(h, 8))
        .map(|h| base + h as usize);
    // SAFETY: inside writable sections of the image, which stays mapped.
    let keeper = unsafe {
        Keeper::new(
            base + sites.pause as usize,
            base + sites.ping as usize,
            heartbeat,
            max(),
        )
    };
    tracing::info!(
        ?sites,
        "the title's watchdog is found: our stalls are held for it"
    );
    KEEPER.set(keeper).is_ok()
}

/// A hold on the title's watchdog (see the module): around a stall of ours
/// that keeps a title thread, so that it is not taken for the title's hang.
/// Nothing if [`arm`] found nothing or it is off.
pub fn hold(reason: &'static str) -> Hold {
    match KEEPER.get() {
        Some(k) => k.hold(reason),
        None => Hold {
            inner: None,
            reason,
            began: Instant::now(),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A pattern's bytes, the wildcards given as `fill`.
    fn bytes(p: Pattern, fill: u8) -> Vec<u8> {
        p.iter().map(|b| b.unwrap_or(fill)).collect()
    }

    /// Code (at `rva`) with the patterns in it, and the places they point at.
    fn code(rva: u32, pause: u32, ping: u32, beat: u32) -> Vec<u8> {
        let disp = |at: usize, end: usize, to: u32| {
            (i64::from(to) - i64::from(rva) - (at + end) as i64) as i32
        };
        let mut c = vec![0x90; 40];
        let at = c.len();
        c.extend(bytes(HANDLER, 0x11));
        c[at + PING_AT..at + PING_AT + 4]
            .copy_from_slice(&disp(at, PING_AT + 5, ping).to_le_bytes());
        c[at + PAUSE_AT..at + PAUSE_AT + 4]
            .copy_from_slice(&disp(at, PAUSE_AT + 4, pause).to_le_bytes());
        c.extend([0xcc; 7]);
        let at = c.len();
        c.extend(bytes(BEAT, 0x22));
        c[at + BEAT_AT..at + BEAT_AT + 4]
            .copy_from_slice(&disp(at, BEAT_AT + 4, beat).to_le_bytes());
        c.extend([0xcc; 9]);
        c
    }

    #[test]
    fn the_bytes_are_found_by_their_code() {
        let c = code(0x500, 0x1001, 0x1002, 0x1008);
        let found = locate(&[(0x500, &c)]).unwrap();
        assert_eq!(
            found,
            Sites {
                pause: 0x1001,
                ping: 0x1002,
                heartbeat: Some(0x1008)
            }
        );
        // Elsewhere in the image, the same code gives the same bytes.
        assert_eq!(locate(&[(0, &[0u8; 64]), (0x500, &c)]), Some(found));
        // A clock that is not aligned is not a clock.
        let c = code(0x500, 0x1001, 0x1002, 0x1009);
        assert_eq!(locate(&[(0x500, &c)]).unwrap().heartbeat, None);
    }

    #[test]
    fn nothing_is_found_where_the_code_is_not_or_is_twice() {
        assert_eq!(locate(&[(0, &[0u8; 4096])]), None);
        let c = code(0, 0x1001, 0x1002, 0x1008);
        assert_eq!(locate(&[(0, &c), (0x10000, &c)]), None);
        // One byte off.
        let mut off = c.clone();
        off[40 + 10] ^= 1;
        assert_eq!(locate(&[(0, &off)]), None);
        // Without the clock's code the pause is still found, with no heartbeat.
        let mut no_beat = c;
        let at = no_beat.len() - 9 - BEAT.len();
        no_beat[at] ^= 1;
        let found = locate(&[(0, &no_beat)]).unwrap();
        assert_eq!(found.heartbeat, None);
    }

    /// Memory standing in for the title's: pause, ping, heartbeat.
    struct Title {
        mem: Box<[AtomicU64; 3]>,
    }

    impl Title {
        fn new() -> Title {
            Title {
                mem: Box::new([AtomicU64::new(0), AtomicU64::new(0), AtomicU64::new(1)]),
            }
        }
        fn byte(&self, i: usize) -> &AtomicU8 {
            // SAFETY: the first byte of a word of `mem`.
            unsafe { AtomicU8::from_ptr(self.mem[i].as_ptr().cast()) }
        }
        fn pause(&self) -> u8 {
            self.byte(0).load(Ordering::SeqCst)
        }
        fn ping(&self) -> u8 {
            self.byte(1).load(Ordering::SeqCst)
        }
        fn keeper(&self, max: Duration) -> Keeper {
            let at = |i: usize| self.mem[i].as_ptr() as usize;
            // SAFETY: words of `mem`, which outlives the keeper in these tests.
            unsafe { Keeper::with_tick(at(0), at(1), Some(at(2)), max, Duration::from_millis(5)) }
        }
    }

    const LONG: Duration = Duration::from_secs(60);

    #[test]
    fn holds_nest_and_the_last_gives_the_pause_back_with_a_ping() {
        let title = Title::new();
        let keeper = title.keeper(LONG);
        let a = keeper.hold("a");
        assert_eq!(title.pause(), 1);
        let b = keeper.hold("b");
        drop(a);
        assert_eq!(title.pause(), 1);
        assert_eq!(title.ping(), 0);
        drop(b);
        assert_eq!((title.pause(), title.ping()), (0, 1));
    }

    #[test]
    fn the_titles_own_pause_is_not_taken_away() {
        let title = Title::new();
        title.byte(0).store(1, Ordering::SeqCst);
        let keeper = title.keeper(LONG);
        drop(keeper.hold("a"));
        assert_eq!(title.pause(), 1);
    }

    #[test]
    fn a_pause_the_title_clears_is_set_again() {
        let title = Title::new();
        let keeper = title.keeper(LONG);
        let _hold = keeper.hold("a");
        title.byte(0).store(0, Ordering::SeqCst);
        let began = Instant::now();
        while title.pause() == 0 && began.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        assert_eq!(title.pause(), 1);
    }

    #[test]
    fn a_hang_is_not_held_for_ever() {
        let title = Title::new();
        let keeper = title.keeper(Duration::from_millis(60));
        let hold = keeper.hold("a");
        let began = Instant::now();
        while title.pause() == 1 && began.elapsed() < Duration::from_secs(5) {
            std::thread::sleep(Duration::from_millis(2));
        }
        // Given back, with no ping: the watchdog goes on counting.
        assert_eq!((title.pause(), title.ping()), (0, 0));
        // And not taken again by a second hold of the same stall.
        let second = keeper.hold("b");
        assert_eq!(title.pause(), 0);
        drop((hold, second));
        assert_eq!(title.ping(), 0);
        // A stall of its own later is held anew.
        let again = keeper.hold("c");
        assert_eq!(title.pause(), 1);
        drop(again);
        assert_eq!((title.pause(), title.ping()), (0, 1));
    }

    #[test]
    fn a_title_that_goes_on_is_held_past_the_limit() {
        let title = Title::new();
        let keeper = title.keeper(Duration::from_millis(60));
        let _hold = keeper.hold("a");
        let beat = &title.mem[2];
        let began = Instant::now();
        while began.elapsed() < Duration::from_millis(300) {
            beat.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(5));
        }
        assert_eq!(title.pause(), 1);
    }

    #[test]
    fn an_unarmed_hold_does_nothing() {
        drop(hold("nothing"));
    }
}
