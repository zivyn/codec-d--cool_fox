//! Diagnostics: switches that change while the title runs, so that one run
//! measures both sides in the same scene. `VICTORY_AB=<seconds>,<switch>[=<value>][,<switch>..]`:
//! every so many seconds, at a present, the next phase begins; switch number
//! n is on in the phases whose number has bit n, so that all combinations
//! come one after another, again and again. Each phase's frame times are
//! logged at its end (the frames of its first second, `VICTORY_AB_SKIP`
//! seconds, left out). A switch named here does not go by its own variable
//! any more: off in the phases without its bit, and in those with it on (a
//! switch with a number for its value, `<switch>=<value>`: that value; with
//! none given, 1).
//!
//! `VICTORY_AB_BUNDLE=1`: the phases alternate between all the named switches off and all on (a bundle against none),
//! not every combination.
//!
//! `VICTORY_AB_START=<seconds>`: the first phase begins that many seconds
//! after the world is found (the second's draws per frame between 3000 and
//! 5500 and under 260 ms a frame, three seconds in a row: what the pilot's
//! driver takes as the world), not at the first present. Every second of a
//! running A/B makes a line "A/B second" with the phase and its frame time
//! (the second is "settled" when it is past the skipped part of its phase), so
//! that the other lines of each second (what the submit thread worked, the
//! card, the CPU) can be given to their arm by their time. With one switch
//! every phase end also logs B minus A so far with its 95 % interval, from the
//! phases' means (`effect`).
//!
//! The switches that may be named are those read through [`live_flag!`] and
//! [`live_value!`] (or [`pick`] and [`bit`] by hand): see `docs/ab-switches.md`
//! for each one and why it may change at any instant.

// Without diagnostics only the macros below are used (the switches then go by their variables alone).
#![cfg_attr(not(feature = "diagnostics"), allow(dead_code, unused_imports))]

use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

struct Asked {
    seconds: f64,
    switches: Vec<String>,
    /// For each switch, the text after `=` ("what is on is this").
    values: Vec<Option<String>>,
}

fn parse(text: &str) -> Option<Asked> {
    let mut parts = text.split(',').map(str::trim);
    let seconds = parts.next()?.parse::<f64>().ok().filter(|s| *s > 1.0)?;
    let (mut switches, mut values) = (Vec::new(), Vec::new());
    for part in parts.filter(|s| !s.is_empty()) {
        match part.split_once('=') {
            Some((name, value)) => {
                switches.push(name.trim().to_string());
                values.push(Some(value.trim().to_string()));
            }
            None => {
                switches.push(part.to_string());
                values.push(None);
            }
        }
    }
    (!switches.is_empty() && switches.len() <= 8).then_some(Asked { seconds, switches, values })
}

fn asked() -> Option<&'static Asked> {
    static ASKED: OnceLock<Option<Asked>> = OnceLock::new();
    ASKED.get_or_init(|| parse(&std::env::var("VICTORY_AB").ok()?)).as_ref()
}

/// When the phases begin and how much of each is left out.
struct Config {
    /// Seconds after the world is found (`None`: at the first present).
    start_after: Option<f64>,
    /// Seconds at the beginning of a phase whose frames are not counted.
    skip: f64,
    /// `VICTORY_AB_BUNDLE=1`: the phases alternate between every named switch off and every one on, not all
    /// combinations (a bundle against none).
    bundle: bool,
}

fn config() -> &'static Config {
    static CONFIG: OnceLock<Config> = OnceLock::new();
    CONFIG.get_or_init(|| {
        let number = |name: &str| std::env::var(name).ok().and_then(|v| v.trim().parse::<f64>().ok()).filter(|v| *v >= 0.0);
        Config {
            start_after: number("VICTORY_AB_START"),
            skip: number("VICTORY_AB_SKIP").unwrap_or(1.0),
            bundle: std::env::var_os("VICTORY_AB_BUNDLE").is_some_and(|v| v == "1"),
        }
    })
}

/// The switches that are on now, a bit each.
static BITS: AtomicU32 = AtomicU32::new(0);

/// Whether `switch` is on now, if it is one of those that change.
pub(crate) fn pick(switch: &str) -> Option<bool> {
    bit(switch).map(on)
}

/// The number of `switch` among those that change.
pub(crate) fn bit(switch: &str) -> Option<u32> {
    Some(asked()?.switches.iter().position(|s| s == switch)? as u32)
}

/// What `switch` is when it is on in the phases that change it (`<switch>=<value>`), if a value was given.
pub(crate) fn value_b(switch: &str) -> Option<&'static str> {
    let asked = asked()?;
    let at = asked.switches.iter().position(|s| s == switch)?;
    asked.values[at].as_deref()
}

/// Whether the switch of that number is on now.
pub(crate) fn on(bit: u32) -> bool {
    BITS.load(Relaxed) >> bit & 1 != 0
}

/// A switch that is off unless its variable is `1`; with `VICTORY_AB` naming it (diagnostics builds), off and on
/// while the title runs. Read at any instant it says what the phase now is: where a decision must not change half
/// way, the caller reads it once and keeps the answer.
macro_rules! live_flag {
    ($name:literal) => {{
        #[cfg(feature = "diagnostics")]
        {
            static BIT: std::sync::OnceLock<Result<bool, u32>> = std::sync::OnceLock::new();
            match BIT.get_or_init(|| match $crate::ab::bit($name) {
                Some(bit) => Err(bit),
                None => Ok(std::env::var_os($name).is_some_and(|v| v == "1")),
            }) {
                Ok(on) => *on,
                Err(bit) => $crate::ab::on(*bit),
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *ON.get_or_init(|| std::env::var_os($name).is_some_and(|v| v == "1"))
        }
    }};
}
pub(crate) use live_flag;

/// A switch with a value of type `$ty` that `$parse` (a `fn(Option<&str>) -> $ty`, the text of the variable or `None`
/// when it is not set) makes of its text; with `VICTORY_AB` naming it (diagnostics builds), the value of the unset
/// variable in the phases without its bit, and in those with it the value of `<switch>=<value>` (of `1` if none).
macro_rules! live_value {
    ($name:literal, $ty:ty, $parse:expr) => {{
        let parse: fn(Option<&str>) -> $ty = $parse;
        #[cfg(feature = "diagnostics")]
        {
            static CELL: std::sync::OnceLock<Result<$ty, (u32, $ty, $ty)>> = std::sync::OnceLock::new();
            match CELL.get_or_init(|| match $crate::ab::bit($name) {
                Some(bit) => Err((bit, parse(None), parse(Some($crate::ab::value_b($name).unwrap_or("1"))))),
                None => Ok(parse(std::env::var($name).ok().as_deref())),
            }) {
                Ok(value) => *value,
                Err((bit, off, on)) => {
                    if $crate::ab::on(*bit) {
                        *on
                    } else {
                        *off
                    }
                }
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            static CELL: std::sync::OnceLock<$ty> = std::sync::OnceLock::new();
            *CELL.get_or_init(|| parse(std::env::var($name).ok().as_deref()))
        }
    }};
}
pub(crate) use live_value;

/// The parse of a switch that is on unless its variable is `0` (`live_value!`; in `VICTORY_AB` name it with `=0`: its
/// off phases are the unset variable, on).
pub(crate) fn unless_zero(text: Option<&str>) -> bool {
    text.is_none_or(|v| v != "0")
}

/// What the run has seen of the phase under way.
struct Phase {
    started: Instant,
    number: u64,
    began: Instant,
    last: Instant,
    drawn: u64,
    frames: Vec<f32>,
    draws: u64,
    /// Of the phases that ended: their bits, the mean frame time and the frames counted.
    means: Vec<(u32, f64, usize)>,
}

/// The seconds of presents: where the world is found.
struct Seconds {
    since: Instant,
    frames: u32,
    drawn: u64,
    good: u32,
    world: Option<Instant>,
}

struct State {
    first: Instant,
    seconds: Seconds,
    phase: Option<Phase>,
}

/// Whether a second of `ms` a frame and `draws` a frame is what the world looks like.
fn world_like(ms: f64, draws: f64) -> bool {
    (3000.0..=5500.0).contains(&draws) && ms < 260.0
}

/// The frames' mean, median and nine tenths.
fn summary(frames: &mut [f32]) -> (f32, f32, f32) {
    if frames.is_empty() {
        return (0.0, 0.0, 0.0);
    }
    let mean = frames.iter().sum::<f32>() / frames.len() as f32;
    frames.sort_by(f32::total_cmp);
    (mean, frames[frames.len() / 2], frames[frames.len() * 9 / 10])
}

/// Two-sided 95 % Student t for `df` degrees of freedom.
fn t95(df: usize) -> f64 {
    const T: [f64; 30] = [
        12.706, 4.303, 3.182, 2.776, 2.571, 2.447, 2.365, 2.306, 2.262, 2.228, 2.201, 2.179, 2.160, 2.145, 2.131, 2.120, 2.110,
        2.101, 2.093, 2.086, 2.080, 2.074, 2.069, 2.064, 2.060, 2.056, 2.052, 2.048, 2.045, 2.042,
    ];
    match df {
        0 => f64::INFINITY,
        1..=30 => T[df - 1],
        _ => 1.96,
    }
}

/// B minus A of one switch from the means of the phases that ended (bit 0 of their bits: A is off, B on).
#[derive(Debug, PartialEq)]
struct Effect {
    /// Mean of the differences, each B phase against the mean of the A phases on both sides of it (so that a drift
    /// that is straight over the three cancels); with fewer than two of those, against the A phase before it.
    mean: f64,
    sd: f64,
    /// Half the 95 % interval of the mean.
    ci: f64,
    pairs: usize,
    /// Each B phase against the A phase before it.
    plain: f64,
}

fn effect(means: &[(u32, f64, usize)]) -> Option<Effect> {
    let ok = |i: usize| means.get(i).filter(|m| m.2 > 0 && m.1.is_finite()).map(|m| (m.0 & 1, m.1));
    let (mut plain, mut drift) = (Vec::new(), Vec::new());
    for i in 1..means.len() {
        let (Some((1, b)), Some((0, a_before))) = (ok(i), ok(i - 1)) else {
            continue;
        };
        plain.push(b - a_before);
        if let Some((0, a_after)) = ok(i + 1) {
            drift.push(b - (a_before + a_after) / 2.0);
        }
    }
    let used = if drift.len() >= 2 { &drift } else { &plain };
    if used.len() < 2 {
        return None;
    }
    let n = used.len() as f64;
    let mean = used.iter().sum::<f64>() / n;
    let sd = (used.iter().map(|d| (d - mean).powi(2)).sum::<f64>() / (n - 1.0)).sqrt();
    let ci = t95(used.len() - 1) * sd / n.sqrt();
    Some(Effect { mean, sd, ci, pairs: used.len(), plain: plain.iter().sum::<f64>() / plain.len() as f64 })
}

/// The title's present ends on this thread. `change`: called when another
/// phase begins, before any switch changes, with what `switch` is and what it
/// will be.
pub(crate) fn presented(change: impl Fn(&str, bool, bool)) {
    let Some(asked) = asked() else {
        return;
    };
    static STATE: Mutex<Option<State>> = Mutex::new(None);
    let now = Instant::now();
    let drawn = crate::draw::drawn();
    let mut guard = STATE.lock().unwrap_or_else(|e| e.into_inner());
    let state = guard.get_or_insert_with(|| State {
        first: now,
        seconds: Seconds { since: now, frames: 0, drawn, good: 0, world: None },
        phase: None,
    });
    let config = config();
    // The second that just ended, if one did: its milliseconds and draws per frame, and its frames.
    state.seconds.frames += 1;
    let second = if now.duration_since(state.seconds.since).as_secs_f64() >= 1.0 {
        let frames = f64::from(state.seconds.frames);
        let ms = now.duration_since(state.seconds.since).as_secs_f64() * 1000.0 / frames;
        let draws = drawn.saturating_sub(state.seconds.drawn) as f64 / frames;
        state.seconds.since = now;
        state.seconds.frames = 0;
        state.seconds.drawn = drawn;
        if state.seconds.world.is_none() {
            state.seconds.good = if world_like(ms, draws) { state.seconds.good + 1 } else { 0 };
            if state.seconds.good >= 3 {
                state.seconds.world = Some(now);
                tracing::info!("A/B: the world is found (the phases begin {:?} seconds after)", config.start_after);
            }
        }
        Some((ms, draws, frames))
    } else {
        None
    };
    let begin_at = match config.start_after {
        None => Some(state.first),
        Some(after) => state.seconds.world.map(|w| w + Duration::from_secs_f64(after)),
    };
    let Some(begin_at) = begin_at.filter(|at| now >= *at) else {
        return;
    };
    let p = state.phase.get_or_insert_with(|| {
        tracing::info!(
            "A/B begins: {} s phases of [{}] (VICTORY_AB), the first {} s of each left out",
            asked.seconds,
            asked.switches.join(" "),
            config.skip
        );
        Phase { started: now, number: 0, began: now, last: now, drawn, frames: Vec::new(), draws: 0, means: Vec::new() }
    });
    let bits = |number: u64| {
        if config.bundle {
            if number % 2 == 1 { (1u32 << asked.switches.len()) - 1 } else { 0 }
        } else {
            (number % (1 << asked.switches.len())) as u32
        }
    };
    let settled = (now - p.began).as_secs_f64() > config.skip;
    if settled {
        p.frames.push((now - p.last).as_secs_f32() * 1000.0);
        p.draws += drawn.saturating_sub(p.drawn);
    }
    (p.last, p.drawn) = (now, drawn);
    if let Some((ms, draws, frames)) = second {
        tracing::info!(
            "A/B second: phase {} bits {} settled {}: {ms:.1} ms/frame over {frames:.0} frames, {draws:.0} draws a frame",
            p.number,
            bits(p.number),
            u8::from(settled)
        );
    }
    let number = ((now - p.started).as_secs_f64() / asked.seconds) as u64;
    if number == p.number {
        return;
    }
    let (was, will) = (bits(p.number), bits(number));
    let (mean, median, most) = summary(&mut p.frames);
    let on: Vec<&str> = asked.switches.iter().enumerate().filter(|(n, _)| was >> n & 1 != 0).map(|(_, s)| s.as_str()).collect();
    tracing::info!(
        "A/B phase {} bits {was} on [{}]: {} frames, mean {mean:.1} ms, median {median:.1}, p90 {most:.1}, {:.0} draws a frame",
        p.number,
        on.join(" "),
        p.frames.len(),
        p.draws as f64 / p.frames.len().max(1) as f64
    );
    p.means.push((was, f64::from(mean), p.frames.len()));
    if (asked.switches.len() == 1 || config.bundle)
        && let Some(e) = effect(&p.means)
    {
        tracing::info!(
            "A/B so far ({}): B minus A {:+.2} ms/frame +-{:.2} (95%, {} pairs against the A phases on both sides, sd {:.2}); \
             each B against the A before it {:+.2}",
            asked.switches.join("+"),
            e.mean,
            e.ci,
            e.pairs,
            e.sd,
            e.plain
        );
    }
    for (n, switch) in asked.switches.iter().enumerate() {
        change(switch, was >> n & 1 != 0, will >> n & 1 != 0);
    }
    BITS.store(will, Relaxed);
    (p.number, p.began, p.draws) = (number, now, 0);
    p.frames.clear();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switches_and_seconds_are_read_and_frames_summed_up() {
        let a = parse("10, VICTORY_A,VICTORY_B").unwrap();
        assert_eq!((a.seconds, a.switches), (10.0, vec!["VICTORY_A".to_string(), "VICTORY_B".to_string()]));
        assert!(parse("10").is_none());
        assert!(parse("x,VICTORY_A").is_none());
        assert!(parse("0,VICTORY_A").is_none());
        assert_eq!(summary(&mut [30.0, 10.0, 20.0]), (20.0, 20.0, 30.0));
        assert_eq!(summary(&mut []), (0.0, 0.0, 0.0));
        assert_eq!(pick("VICTORY_NONE_SUCH"), None);
        assert_eq!(value_b("VICTORY_NONE_SUCH"), None);
    }

    #[test]
    fn a_switch_may_be_given_the_value_it_has_when_on() {
        let a = parse("20, VICTORY_REFRESH_NARROW=64 ,VICTORY_HOLDER_INDEX").unwrap();
        assert_eq!(a.switches, vec!["VICTORY_REFRESH_NARROW".to_string(), "VICTORY_HOLDER_INDEX".to_string()]);
        assert_eq!(a.values, vec![Some("64".to_string()), None]);
        assert!(parse("20,").is_none());
        assert!(parse("20,A,B,C,D,E,F,G,H,I").is_none());
    }

    #[test]
    fn the_world_is_a_second_of_the_draws_and_time_of_the_cell() {
        assert!(world_like(90.0, 4300.0));
        assert!(!world_like(90.0, 1200.0));
        assert!(!world_like(90.0, 6000.0));
        assert!(!world_like(300.0, 4300.0));
    }

    #[test]
    fn b_minus_a_is_found_against_the_phases_on_both_sides() {
        // A 90, B 95, A 90, B 95, A 90: five on each side, no spread.
        let m = |bits: u32, ms: f64| (bits, ms, 100);
        let e = effect(&[m(0, 90.0), m(1, 95.0), m(0, 90.0), m(1, 95.0), m(0, 90.0)]).unwrap();
        assert_eq!((e.mean, e.sd, e.pairs, e.plain), (5.0, 0.0, 2, 5.0));
        // A straight drift of +2 a phase over a constant effect of 5 is cancelled by the neighbours, not by the A before.
        let e = effect(&[m(0, 90.0), m(1, 97.0), m(0, 94.0), m(1, 101.0), m(0, 98.0)]).unwrap();
        assert!((e.mean - 5.0).abs() < 1e-9 && e.sd < 1e-9, "{e:?}");
        assert!((e.plain - 7.0).abs() < 1e-9, "{e:?}");
        // One pair is no interval.
        assert_eq!(effect(&[m(0, 90.0), m(1, 95.0), m(0, 90.0)]), None);
        // Phases without frames or with a NaN are passed over.
        assert!(effect(&[m(0, 90.0), (1, 95.0, 0), m(0, 90.0), m(1, f64::NAN), m(0, 90.0)]).is_none());
        // The interval is the t of its degrees of freedom: 2 pairs, 1 degree, 12.706.
        let e = effect(&[m(0, 90.0), m(1, 94.0), m(0, 90.0), m(1, 96.0), m(0, 90.0)]).unwrap();
        assert!((e.mean - 5.0).abs() < 1e-9);
        assert!((e.ci - 12.706).abs() < 1e-6, "{e:?}");
    }

    #[test]
    fn a_switch_that_is_on_unless_zero_is_off_only_in_the_phases_that_name_zero() {
        assert_eq!([None, Some("1"), Some(""), Some("0")].map(unless_zero), [true, true, true, false]);
    }

    #[test]
    fn the_t_of_few_degrees_is_wide_and_of_many_is_the_normal() {
        assert_eq!(t95(1), 12.706);
        assert_eq!(t95(30), 2.042);
        assert_eq!(t95(31), 1.96);
        assert!(t95(0).is_infinite());
    }
}
