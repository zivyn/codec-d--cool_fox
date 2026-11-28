//! The card's memory DLSS Super Resolution and DLSS Frame Generation take, kept out of what our
//! textures may use (NV3, `VICTORY_FRAMEGEN_BUDGET`).
//!
//! On an 8 GB card the layer's own use stands at or past the budget the system gives the process
//! (7,444 MB of 7,189 in pilot run NV2-A1). Streamline's common plugin watches that budget: with the
//! process over it, `slDLSSGSetOptions` answers 39 ("warning: out of VRAM", the log says "Exceeded
//! VRAM budget"), frame generation showed 1 generated frame in the whole run, and DLSS's own
//! evaluations failed out of memory (0.9 to 1.1 a frame) once frame generation was loaded.
//!
//! Three parts, all off unless the switch is on (it is, by default, with `VICTORY_FRAMEGEN=1`; `=0`
//! off; `=1` also with DLSS alone):
//! - What the features will take is taken from the budget every part of the layer reads
//!   (`Host::video_memory`: the texture room, the pool's pressure, the idle sweep, the tile
//!   heaps, the card copies): Streamline's own estimate for frame generation
//!   (`slDLSSGGetState` with `eRequestVRAMEstimate`; a size-based figure when it answers nothing),
//!   a size-based figure for DLSS Super Resolution (no call is mirrored here for its estimate), each
//!   until the feature has allocated it and the system's own count has it ([`reserved`]).
//! - An answer 39 backs the room off by what the card stood past its budget (and a little more),
//!   and frame generation is switched off at once (without keeping its resources).
//! - It is asked again after `VICTORY_FRAMEGEN_RETRY_S` seconds (10), when the use leaves room
//!   for it, or after four times that whatever the use ([`Recovery`]).

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::*};
use std::sync::{Mutex, OnceLock};

const MB: u64 = 1 << 20;

/// What the budget fits until frame generation says: about what a 1440p output takes.
const FG_FIRST: u64 = 384 * MB;
/// Likewise for DLSS Super Resolution.
const SR_FIRST: u64 = 192 * MB;
/// The most the back-off may add to what is kept.
const BACKOFF_MOST: u64 = 2048 * MB;
/// Room left over the estimate before frame generation is asked for again.
const RETRY_MARGIN: u64 = 128 * MB;

static ON: AtomicBool = AtomicBool::new(false);
/// What the size and amount of the last estimate were (0: none yet), and what it said.
static FG_KEY: AtomicU64 = AtomicU64::new(0);
static FG_BYTES: AtomicU64 = AtomicU64::new(FG_FIRST);
/// The estimate was Streamline's own.
static FG_OWN: AtomicBool = AtomicBool::new(false);
/// Frame generation holds its memory now (inside the card's use as the system counts it).
static FG_HELD: AtomicBool = AtomicBool::new(false);
static SR_BYTES: AtomicU64 = AtomicU64::new(SR_FIRST);
static SR_HELD: AtomicBool = AtomicBool::new(false);
static BACKOFF: AtomicU64 = AtomicU64::new(0);
/// The card's use (system's count) right before frame generation was switched on; 0 if none.
static USED_BEFORE: AtomicU64 = AtomicU64::new(0);
/// Answers 39, switch-ons asked again after one, those of them that gave 0.
static ANSWERS: [AtomicU64; 3] = [const { AtomicU64::new(0) }; 3];

pub(crate) fn set_on(on: bool) {
    ON.store(on, Relaxed);
    if on {
        tracing::info!(
            "Streamline memory budget on (VICTORY_FRAMEGEN_BUDGET): the texture room is counted against the budget less what DLSS and frame generation take, switching frame generation on is asked again {} s after an out-of-VRAM answer",
            retry_ms() / 1000
        );
    }
}

pub(crate) fn on() -> bool {
    ON.load(Relaxed)
}

fn env_mb(name: &str) -> Option<u64> {
    std::env::var(name)
        .ok()?
        .trim()
        .parse::<u64>()
        .ok()
        .map(|mb| mb * MB)
}

/// Seconds before the switch-on is asked again (`VICTORY_FRAMEGEN_RETRY_S`, 10), in milliseconds.
fn retry_ms() -> u64 {
    static MS: OnceLock<u64> = OnceLock::new();
    *MS.get_or_init(|| {
        std::env::var("VICTORY_FRAMEGEN_RETRY_S")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .map_or(10, |s| s.max(1))
            * 1000
    })
}

/// What frame generation needs for an output of `color` and `generated` frames for each one
/// rendered when Streamline gives no estimate: 64 bytes a pixel and a frame (225 MB at 1440p, 506 at
/// 2160p for one).
pub(crate) fn fallback_fg(color: [u32; 2], generated: u32) -> u64 {
    u64::from(color[0]) * u64::from(color[1]) * 64 * u64::from(generated.max(1))
}

/// What DLSS Super Resolution takes for an output of `output`: 48 bytes a pixel (168 MB at 1440p,
/// 379 at 2160p).
pub(crate) fn fallback_sr(output: [u32; 2]) -> u64 {
    u64::from(output[0]) * u64::from(output[1]) * 48
}

/// The size and amount an estimate was asked for; never 0.
pub(crate) fn fg_key(color: [u32; 2], generated: u32) -> u64 {
    (u64::from(color[0]) << 40)
        | (u64::from(color[1] & 0xff_ffff) << 16)
        | u64::from(generated & 0xffff)
        | 1 << 63
}

/// Whether the estimate must be asked for again: not yet for this size and amount.
pub(crate) fn fg_stale(key: u64) -> bool {
    FG_KEY.load(Relaxed) != key
}

/// Notes the estimate for `key`: Streamline's (`own`, if it said one), else the size-based one, or
/// `VICTORY_FRAMEGEN_RESERVE_MB` whatever either says.
pub(crate) fn fg_estimated(key: u64, own: Option<u64>, color: [u32; 2], generated: u32, code: i32) {
    let forced = env_mb("VICTORY_FRAMEGEN_RESERVE_MB");
    let bytes = forced
        .or(own)
        .unwrap_or_else(|| fallback_fg(color, generated));
    FG_KEY.store(key, Relaxed);
    FG_BYTES.store(bytes, Relaxed);
    FG_OWN.store(own.is_some() && forced.is_none(), Relaxed);
    tracing::info!(
        output = ?color,
        frames_generated = generated,
        code,
        streamline_mb = own.map_or(0, |b| b / MB),
        kept_mb = bytes / MB,
        "DLSS Frame Generation: what it takes of the card's memory ({}): kept free of our textures until it holds it",
        if forced.is_some() {
            "VICTORY_FRAMEGEN_RESERVE_MB"
        } else if own.is_some() {
            "Streamline's estimate"
        } else {
            "a size-based figure: Streamline gave none"
        }
    );
}

pub(crate) fn fg_held(held: bool) {
    FG_HELD.store(held, Relaxed);
}

/// Called once an upscaling by DLSS was recorded: `evaluations` so far, for an output of `output`.
/// The memory of DLSS is held by then (a second of them), and is in the system's count.
pub(crate) fn sr_seen(evaluations: u64, output: [u32; 2]) {
    if !on() || SR_HELD.load(Relaxed) {
        return;
    }
    if evaluations == 1 {
        let bytes = env_mb("VICTORY_DLSS_RESERVE_MB").unwrap_or_else(|| fallback_sr(output));
        SR_BYTES.store(bytes, Relaxed);
        tracing::info!(output = ?output, kept_mb = bytes / MB, "DLSS Super Resolution: kept free of our textures until it holds its memory (a size-based figure; VICTORY_DLSS_RESERVE_MB sets another)");
    }
    if evaluations >= 60 {
        SR_HELD.store(true, Relaxed);
    }
}

/// The bytes to take from the budget: what frame generation (`fg`: it is part of the run) and DLSS
/// Super Resolution (`sr`) will take and do not hold yet, and the back-off.
pub(crate) fn reserved(fg: bool, sr: bool) -> u64 {
    if !on() {
        return 0;
    }
    let fg = if fg && !FG_HELD.load(Relaxed) {
        FG_BYTES.load(Relaxed)
    } else {
        0
    };
    let sr = if sr && !SR_HELD.load(Relaxed) {
        SR_BYTES.load(Relaxed)
    } else {
        0
    };
    fg + sr + BACKOFF.load(Relaxed)
}

/// `budget` less `reserve`, but never by more than a third of it.
pub(crate) fn effective(budget: u64, reserve: u64) -> u64 {
    budget - reserve.min(budget / 3)
}

/// The back-off after an answer 39 with the card's use `over` bytes past its budget: that and a
/// little more (at least 256 MB), up to 2 GB in all.
pub(crate) fn backoff_after(current: u64, over: u64) -> u64 {
    current
        .saturating_add(over.saturating_add(RETRY_MARGIN).max(256 * MB))
        .min(BACKOFF_MOST)
}

/// Where the switch-on stands after answers 39.
#[derive(Debug, Default)]
pub(crate) struct Recovery {
    /// When the last answer 39 came (milliseconds), until a switch-on works.
    failed_at: Option<u64>,
    /// Answers 39 in a row.
    failures: u32,
}

impl Recovery {
    pub(crate) const fn new() -> Self {
        Recovery {
            failed_at: None,
            failures: 0,
        }
    }

    /// An answer 39 at `now`; how many in a row.
    pub(crate) fn failed(&mut self, now: u64) -> u32 {
        self.failed_at = Some(now);
        self.failures += 1;
        self.failures
    }

    /// A switch-on gave 0: how many answers 39 came before it (0: none).
    pub(crate) fn worked(&mut self) -> u32 {
        self.failed_at = None;
        std::mem::take(&mut self.failures)
    }

    /// Whether the switch-on is held back at `now`: before `wait` milliseconds have passed since the
    /// last answer 39, and, while the card's use leaves no room for it (`fits` is false), until four
    /// times that.
    pub(crate) fn blocked(&self, now: u64, wait: u64, fits: bool) -> bool {
        match self.failed_at {
            None => false,
            Some(at) => {
                let waited = now.saturating_sub(at);
                waited < wait || (!fits && waited < wait.saturating_mul(4))
            }
        }
    }
}

static RECOVERY: Mutex<Recovery> = Mutex::new(Recovery::new());

fn recovery() -> std::sync::MutexGuard<'static, Recovery> {
    RECOVERY.lock().unwrap_or_else(|e| e.into_inner())
}

/// Whether frame generation stays off now because the last switch-on was answered 39 and it is too
/// soon to ask again. `memory` says the system's numbers (budget, use) only when asked.
pub(crate) fn held_back(now_ms: u64, memory: impl FnOnce() -> Option<[u64; 4]>) -> bool {
    if !on() {
        return false;
    }
    let recovery = recovery();
    if recovery.failed_at.is_none() {
        return false;
    }
    let need = if FG_HELD.load(Relaxed) {
        0
    } else {
        FG_BYTES.load(Relaxed)
    };
    let fits = memory().is_none_or(|[budget, used, ..]| {
        used.saturating_add(need).saturating_add(RETRY_MARGIN) <= budget
    });
    recovery.blocked(now_ms, retry_ms(), fits)
}

/// The switch-on was answered 39 with the system saying `memory` (budget, use): backs the room off,
/// says so (once in a warning, then a line for each try). Frame generation is switched off by the caller.
pub(crate) fn out_of_vram(now_ms: u64, memory: Option<[u64; 4]>, shown: u32) {
    let [budget, used] = memory.map_or([0, 0], |m| [m[0], m[1]]);
    let over = used.saturating_sub(budget);
    let now = backoff_after(BACKOFF.load(Relaxed), over);
    BACKOFF.store(now, Relaxed);
    FG_HELD.store(false, Relaxed);
    let tries = recovery().failed(now_ms);
    let times = ANSWERS[0].fetch_add(1, Relaxed) + 1;
    if tries > 1 {
        ANSWERS[1].fetch_add(1, Relaxed);
    }
    if times == 1 {
        tracing::warn!(
            times = shown,
            used_mb = used / MB,
            budget_mb = budget / MB,
            over_mb = over / MB,
            backoff_mb = now / MB,
            kept_mb = FG_BYTES.load(Relaxed) / MB,
            retry_s = retry_ms() / 1000,
            "DLSS Frame Generation: out of the card's memory (answer 39: the process is past the budget the system gives it); frame generation is off, the texture room backs off by {} MB, and it is asked again in {} s (said once; the lines after count the tries)",
            now / MB,
            retry_ms() / 1000
        );
    } else {
        tracing::info!(
            attempt = tries,
            used_mb = used / MB,
            budget_mb = budget / MB,
            over_mb = over / MB,
            backoff_mb = now / MB,
            "DLSS Frame Generation: out of the card's memory again"
        );
    }
}

/// The switch-on gave 0 with the card's use `used` (the system's count) right before it.
pub(crate) fn switched_on(used: u64) {
    FG_HELD.store(true, Relaxed);
    USED_BEFORE.store(used, Relaxed);
    let before = recovery().worked();
    if before > 0 {
        ANSWERS[2].fetch_add(1, Relaxed);
        tracing::info!(
            answers_before = before,
            "DLSS Frame Generation: on again after being out of the card's memory"
        );
    }
}

/// Some seconds of presents with frame generation on: what the card's use rose by since the switch.
pub(crate) fn measured(used: u64) {
    let before = USED_BEFORE.swap(0, Relaxed);
    if before != 0 {
        tracing::info!(
            rose_mb = (used as i64 - before as i64) / MB as i64,
            estimate_mb = FG_BYTES.load(Relaxed) / MB,
            own_estimate = FG_OWN.load(Relaxed),
            "DLSS Frame Generation: the card's use rose by this much between the switch-on and now (textures come and go in it; a hint, not a measure)"
        );
    }
}

/// The line, from the vram line (every five seconds).
pub(crate) fn log(fg: bool, sr: bool, effective_budget: u64, os_budget: u64) {
    if !on() {
        return;
    }
    tracing::info!(
        "streamline budget: {} MB kept of the budget of {} MB (counted against {} MB): frame generation {} MB ({}, {}), DLSS {} MB ({}), back-off {} MB; out-of-VRAM answers {}, asked again {}, on again {}",
        reserved(fg, sr) / MB,
        os_budget / MB,
        effective_budget / MB,
        FG_BYTES.load(Relaxed) / MB,
        if FG_OWN.load(Relaxed) {
            "Streamline's estimate"
        } else {
            "size-based"
        },
        if !fg {
            "not in the run"
        } else if FG_HELD.load(Relaxed) {
            "held"
        } else {
            "to come"
        },
        SR_BYTES.load(Relaxed) / MB,
        if !sr {
            "not in the run"
        } else if SR_HELD.load(Relaxed) {
            "held"
        } else {
            "to come"
        },
        BACKOFF.load(Relaxed) / MB,
        ANSWERS[0].load(Relaxed),
        ANSWERS[1].load(Relaxed),
        ANSWERS[2].load(Relaxed),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_budget_loses_what_is_kept_but_never_more_than_a_third() {
        assert_eq!(effective(7189 * MB, 600 * MB), 6589 * MB);
        assert_eq!(effective(7200 * MB, 0), 7200 * MB);
        assert_eq!(effective(7200 * MB, 5000 * MB), 4800 * MB);
        assert_eq!(effective(0, 100 * MB), 0);
    }

    #[test]
    fn the_back_off_is_what_the_card_stood_over_and_a_margin_up_to_two_gigabytes() {
        // 255 MB over: 255 + 128 = 383.
        assert_eq!(backoff_after(0, 255 * MB), 383 * MB);
        // Not over (the answer came with room): 256 MB, the least.
        assert_eq!(backoff_after(0, 0), 256 * MB);
        assert_eq!(backoff_after(256 * MB, 0), 512 * MB);
        assert_eq!(backoff_after(2000 * MB, 900 * MB), 2048 * MB);
    }

    #[test]
    fn a_switch_on_is_asked_again_after_the_wait_when_the_card_has_room() {
        let mut r = Recovery::new();
        assert!(!r.blocked(0, 10_000, true), "nothing failed");
        assert_eq!(r.failed(5_000), 1);
        assert!(r.blocked(5_000, 10_000, true));
        assert!(r.blocked(14_999, 10_000, true));
        assert!(!r.blocked(15_000, 10_000, true));
    }

    #[test]
    fn without_room_it_waits_four_times_as_long_and_then_asks_anyway() {
        let mut r = Recovery::new();
        r.failed(0);
        assert!(r.blocked(10_000, 10_000, false));
        assert!(r.blocked(39_999, 10_000, false));
        assert!(!r.blocked(40_000, 10_000, false));
    }

    #[test]
    fn a_switch_on_that_works_ends_the_recovery_and_says_how_many_answers_it_took() {
        let mut r = Recovery::new();
        r.failed(0);
        r.failed(20_000);
        assert_eq!(r.worked(), 2);
        assert!(!r.blocked(20_001, 10_000, false));
        assert_eq!(r.worked(), 0);
        // The count starts over.
        assert_eq!(r.failed(30_000), 1);
    }

    #[test]
    fn the_size_based_figures_grow_with_the_output_and_the_frames() {
        assert_eq!(fallback_fg([2560, 1440], 1) / MB, 225);
        assert_eq!(fallback_fg([3840, 2160], 1) / MB, 506);
        assert_eq!(
            fallback_fg([2560, 1440], 3),
            3 * fallback_fg([2560, 1440], 1)
        );
        assert_eq!(fallback_fg([2560, 1440], 0), fallback_fg([2560, 1440], 1));
        assert_eq!(fallback_sr([2560, 1440]) / MB, 168);
    }

    #[test]
    fn the_estimate_key_names_the_size_and_the_amount_and_is_never_zero() {
        let a = fg_key([2560, 1440], 1);
        assert_ne!(a, 0);
        assert_ne!(a, fg_key([3840, 2160], 1));
        assert_ne!(a, fg_key([2560, 1440], 2));
        assert_eq!(a, fg_key([2560, 1440], 1));
        assert_ne!(fg_key([0, 0], 0), 0);
    }
}
