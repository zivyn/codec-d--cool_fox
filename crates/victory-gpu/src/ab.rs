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
//! `VICTORY_AB_SHOTS=N` (with `VICTORY_CAPTURE=<folder>`; the pilot's `auto-run.ps1` always has one): a picture of every
//! phase of every Nth cycle (a cycle is a pair for one switch or a bundle, `2^n` combinations of `n` switches; 1: every
//! phase), saved by the host's next present a second and a half before the phase ends as `ab-<phase>-<A|B>.png`
//! (`ab-<phase>-b<bits>` with several switches), at half size like the capture's other frames. Pictures of both arms
//! from the same process: `bench1-ab.py --shots <tag>` makes the montage and the difference of each pair.
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
//!
//! What is wrong with the list is said, never silently dropped (`parse`, `Setup::lines`): at the start of the process
//! (`announce`) and again where the world is found, at WARN, naming each entry. An entry that cannot flip anything (a
//! name that is not in `registry::LIVE`, a value its switch would not read, a name twice) is dropped and the rest runs;
//! a list with no phase length, nothing left or more than 8 switches (31 with `VICTORY_AB_BUNDLE=1`) does not run at
//! all, and the line says so. The line `A/B ... will flip N switch(es): [bit0 ..]` is the final list, and `A/B begins:`
//! repeats it with the phase plan.

// Without diagnostics only the macros below are used (the switches then go by their variables alone).
#![cfg_attr(not(feature = "diagnostics"), allow(dead_code, unused_imports))]

mod registry;

use registry::{Check, kind_of, nearest};
use std::sync::atomic::{AtomicU32, Ordering::Relaxed};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The most switches named when every combination of them is a phase (`2^8` phases come before the arms repeat).
const MOST_COMBINED: usize = 8;
/// The most switches named in a bundle (`VICTORY_AB_BUNDLE=1`): a bit each of [`BITS`].
const MOST_BUNDLED: usize = 31;

/// A list that `VICTORY_AB` gave and that can run. Every entry that could not was dropped, and `notes` says which and why.
struct Asked {
    seconds: f64,
    switches: Vec<String>,
    /// For each switch, the text after `=` ("what is on is this").
    values: Vec<Option<String>>,
    /// The entries of the list that were dropped, as they were written.
    dropped: Vec<String>,
    /// What is to be said about the list at the start and again where the world is found: each dropped entry and why,
    /// and each value that is kept and is not read.
    notes: Vec<String>,
}

/// A list that cannot run at all, and why.
struct Refusal {
    reason: String,
    notes: Vec<String>,
}

/// Reads `VICTORY_AB`'s text. An entry that cannot flip anything (a name that is not a live switch, a value that its
/// switch would not read, a name given twice) is dropped and the rest runs, because a run that flips the others is
/// still a measurement of them and the line says what it left out. A list that has no phase length, nothing left to flip
/// or more switches than a run can hold is refused: there the A/B would be something else than what was asked.
fn parse(text: &str, bundle: bool) -> Result<Asked, Refusal> {
    let mut notes = Vec::new();
    let mut parts = text.split(',').map(str::trim);
    let first = parts.next().unwrap_or("");
    let seconds = match first.parse::<f64>() {
        Ok(s) if s.is_finite() && s > 1.0 => s,
        _ => {
            let reason = format!("the first entry must be the length of a phase in seconds, a number above 1: got {first:?}");
            return Err(Refusal { reason, notes });
        }
    };
    let (mut switches, mut values, mut dropped) = (Vec::new(), Vec::new(), Vec::new());
    for part in parts {
        if part.is_empty() {
            notes.push("an empty entry (a stray comma) was skipped".to_string());
            continue;
        }
        let (name, value) = match part.split_once('=') {
            Some((name, value)) => (name.trim(), Some(value.trim())),
            None => (part, None),
        };
        if name.is_empty() {
            notes.push(format!("{part:?} has no switch name: dropped"));
            dropped.push(part.to_string());
            continue;
        }
        let Some(kind) = kind_of(name) else {
            let hint = nearest(name).map(|near| format!(" (did you mean {near}?)")).unwrap_or_default();
            notes.push(format!(
                "{part}: not a switch the A/B can flip{hint}, so the entry is dropped: either the name is mistyped or the switch is not read through the live switches \
                 (docs/ab-switches.md), and naming it would change nothing while the title runs"
            ));
            dropped.push(part.to_string());
            continue;
        };
        if switches.iter().any(|s| s == name) {
            notes.push(format!("{part}: {name} is named twice, the second entry is dropped"));
            dropped.push(part.to_string());
            continue;
        }
        match registry::check_value(name, kind, value) {
            Check::Fine => {}
            Check::Odd(why) => notes.push(format!("{why}; the entry is kept")),
            Check::Bad(why) => {
                notes.push(format!("{why}; the entry is dropped"));
                dropped.push(part.to_string());
                continue;
            }
        }
        switches.push(name.to_string());
        values.push(value.map(str::to_string));
    }
    let most = if bundle { MOST_BUNDLED } else { MOST_COMBINED };
    let reason = if switches.is_empty() {
        Some("no switch is left to flip (none was named, or every entry was dropped)".to_string())
    } else if switches.len() > most && bundle {
        Some(format!("{} switches are named and a bundle holds at most {most}", switches.len()))
    } else if switches.len() > most {
        Some(format!(
            "{} switches are named and every combination of them is a phase, so at most {most} fit: name fewer, or set VICTORY_AB_BUNDLE=1 \
             for up to {MOST_BUNDLED} flipped together as one bundle (all off against all on)",
            switches.len()
        ))
    } else {
        None
    };
    match reason {
        Some(reason) => Err(Refusal { reason, notes }),
        None => Ok(Asked { seconds, switches, values, dropped, notes }),
    }
}

impl Asked {
    /// The switches as they will be flipped: `bit0 VICTORY_A, bit1 VICTORY_B=2`.
    fn list(&self) -> String {
        let one = |n: usize, s: &String| match &self.values[n] {
            Some(v) => format!("bit{n} {s}={v}"),
            None => format!("bit{n} {s}"),
        };
        self.switches.iter().enumerate().map(|(n, s)| one(n, s)).collect::<Vec<_>>().join(", ")
    }
}

/// What `VICTORY_AB` came to.
enum Plan {
    /// Not set.
    Off,
    /// Set, in a build that has no A/B (only `victory-diag.exe` has).
    NoDiagnostics,
    Refused(Refusal),
    Run(Asked),
}

/// Everything the environment says about the A/B, read once.
struct Setup {
    /// `VICTORY_AB` as it was written.
    raw: String,
    plan: Plan,
    config: Config,
    /// What is wrong with the `VICTORY_AB_*` variables.
    config_notes: Vec<String>,
}

impl Setup {
    /// From the variables `get` finds (the process's, or a test's).
    fn read(get: &dyn Fn(&str) -> Option<String>, diagnostics: bool) -> Setup {
        let (config, config_notes) = Config::read(get);
        let raw = get("VICTORY_AB").unwrap_or_default();
        let plan = if raw.trim().is_empty() {
            Plan::Off
        } else if !diagnostics {
            Plan::NoDiagnostics
        } else {
            match parse(&raw, config.bundle) {
                Ok(asked) => Plan::Run(asked),
                Err(refusal) => Plan::Refused(refusal),
            }
        };
        Setup { raw, plan, config, config_notes }
    }

    /// The lines to log about what was asked, `when` being the moment: whether each is a warning (everything that is
    /// not as asked), and its text. The final list of switches the A/B flips is the last line of a list that runs.
    fn lines(&self, when: &str) -> Vec<(bool, String)> {
        let raw = &self.raw;
        let mut lines: Vec<(bool, String)> = self.config_notes.iter().map(|n| (true, format!("A/B ({when}): {n}"))).collect();
        match &self.plan {
            Plan::Off => {}
            Plan::NoDiagnostics => lines.push((
                true,
                format!(
                    "A/B ({when}): VICTORY_AB={raw:?} is set and this build has no A/B (it is in the diagnostics build, victory-diag.exe): \
                     NOT RUNNING, every switch goes by its own variable"
                ),
            )),
            Plan::Refused(refusal) => {
                lines.extend(refusal.notes.iter().map(|n| (true, format!("A/B ({when}): VICTORY_AB entry {n}"))));
                lines.push((true, format!("A/B ({when}): VICTORY_AB={raw:?} is NOT RUNNING: {}", refusal.reason)));
            }
            Plan::Run(asked) => {
                lines.extend(asked.notes.iter().map(|n| (true, format!("A/B ({when}): VICTORY_AB entry {n}"))));
                let dropped = if asked.dropped.is_empty() {
                    String::new()
                } else {
                    format!("; DROPPED {} of the entries: [{}]", asked.dropped.len(), asked.dropped.join(", "))
                };
                lines.push((
                    !asked.dropped.is_empty() || !asked.notes.is_empty(),
                    format!("A/B ({when}): VICTORY_AB={raw:?} will flip {} switch(es): [{}]{dropped}", asked.switches.len(), asked.list()),
                ));
            }
        }
        lines
    }

    fn say(&self, when: &str) {
        for (warn, line) in self.lines(when) {
            if warn {
                tracing::warn!("{line}");
            } else {
                tracing::info!("{line}");
            }
        }
    }
}

/// The environment's A/B, read (and said) the first time anything asks, which is [`announce`] at the start.
fn setup() -> &'static Setup {
    static SETUP: OnceLock<Setup> = OnceLock::new();
    SETUP.get_or_init(|| {
        let setup = Setup::read(&|name| std::env::var(name).ok(), cfg!(feature = "diagnostics"));
        setup.say("at startup");
        setup
    })
}

/// At the start of the process: reads `VICTORY_AB` and says what is wrong with it, so that a list that cannot run (or
/// that runs without some entries) is in the log long before the world is found.
pub(crate) fn announce() {
    setup();
}

fn asked() -> Option<&'static Asked> {
    match &setup().plan {
        Plan::Run(asked) => Some(asked),
        _ => None,
    }
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
    /// `VICTORY_AB_SHOTS=N`: a picture of every phase of every Nth cycle (0: none), see [`shot_due`].
    shots: u64,
}

impl Config {
    /// From the variables `get` finds, with what is wrong with them: a variable that is not a number of 0 or more is
    /// ignored (it used to be, silently).
    fn read(get: &dyn Fn(&str) -> Option<String>) -> (Config, Vec<String>) {
        let mut notes = Vec::new();
        let mut number = |name: &str, default: &str| {
            let text = get(name).filter(|v| !v.trim().is_empty())?;
            match text.trim().parse::<f64>() {
                Ok(n) if n.is_finite() && n >= 0.0 => Some(n),
                _ => {
                    notes.push(format!("{name}={text:?} is not a number of 0 or more: ignored ({default})"));
                    None
                }
            }
        };
        let start_after = number("VICTORY_AB_START", "the phases begin at the first present");
        let skip = number("VICTORY_AB_SKIP", "1 s is left out").unwrap_or(1.0);
        let shots = number("VICTORY_AB_SHOTS", "no pictures").map_or(0, |v| v as u64);
        let bundle_text = get("VICTORY_AB_BUNDLE").filter(|v| !v.is_empty());
        let bundle = bundle_text.as_deref() == Some("1");
        if let Some(text) = bundle_text.filter(|v| v != "1") {
            notes.push(format!("VICTORY_AB_BUNDLE={text:?} is not 1: ignored (every combination of the switches is a phase)"));
        }
        (Config { start_after, skip, bundle, shots }, notes)
    }
}

fn config() -> &'static Config {
    &setup().config
}

/// The line that says the A/B begins: the whole plan, so that one search of the log confirms what is running.
fn begins(asked: &Asked, config: &Config, raw: &str) -> String {
    let n = asked.switches.len();
    let phases = if config.bundle {
        "a bundle: even phases have every switch off (A), odd phases every one on (B)".to_string()
    } else {
        format!("every combination: phase p has the switches whose bit is set in p mod {} on, {} phases a cycle", 1u64 << n, 1u64 << n)
    };
    let start = match config.start_after {
        None => "at the first present".to_string(),
        Some(after) => format!("{after} s after the world is found"),
    };
    let shots = match config.shots {
        0 => "no pictures".to_string(),
        every => format!("pictures of every phase of one cycle in {every}"),
    };
    let dropped = if asked.dropped.is_empty() { String::new() } else { format!("; DROPPED [{}]", asked.dropped.join(", ")) };
    format!(
        "A/B begins: {} s phases (the first {} s of each left out), first phase {start}; {n} switch(es) [{}]; {phases}; {shots} (VICTORY_AB={raw:?}){dropped}",
        asked.seconds,
        config.skip,
        asked.list()
    )
}

/// The switches that are on now, a bit each.
static BITS: AtomicU32 = AtomicU32::new(0);

/// Whether `switch` is on now, if it is one of those that change.
pub(crate) fn pick(switch: &str) -> Option<bool> {
    bit(switch).map(on)
}

/// The number of `switch` among those that change.
pub fn bit(switch: &str) -> Option<u32> {
    Some(asked()?.switches.iter().position(|s| s == switch)? as u32)
}

/// What `switch` is when it is on in the phases that change it (`<switch>=<value>`), if a value was given.
pub fn value_b(switch: &str) -> Option<&'static str> {
    let asked = asked()?;
    let at = asked.switches.iter().position(|s| s == switch)?;
    asked.values[at].as_deref()
}

/// Whether the switch of that number is on now.
pub fn on(bit: u32) -> bool {
    BITS.load(Relaxed) >> bit & 1 != 0
}

/// The switches that are on now, a bit each (for logs that tag what they count with the arm).
pub fn bits() -> u32 {
    BITS.load(Relaxed)
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

/// How many phases come before the arms are the same again: a pair for one switch or a bundle, `2^n` combinations of
/// `n` switches.
fn cycle_len(switches: usize, bundle: bool) -> u64 {
    if bundle { 2 } else { 1 << switches }
}

/// Whether phase `phase` is given a picture with `VICTORY_AB_SHOTS=shots`: every phase of every `shots`th cycle, so that
/// the pictures that are taken come in the arms that make a cycle, the pair or the combinations, from one stretch of the
/// run. (A cycle's first phase is the one with every switch off.)
fn shot_due(phase: u64, shots: u64, cycle: u64) -> bool {
    shots > 0 && cycle > 0 && (phase / cycle) % shots == 0
}

/// The name of a phase's picture: `ab-<phase>-A` or `-B` with one switch or a bundle (A: off, B: on), and
/// `ab-<phase>-b<bits>` with several switches (the bits of the ones that are on).
fn shot_name(phase: u64, bits: u32, two_arms: bool) -> String {
    if two_arms {
        format!("ab-{phase}-{}", if bits == 0 { 'A' } else { 'B' })
    } else {
        format!("ab-{phase}-b{bits}")
    }
}

/// A picture asked for by the phase under way: its number and bits. It is taken by the next present of the host, which
/// may come a frame or two after the title's (so it is asked for before the phase ends, not at its end).
static SHOT: Mutex<Option<(u64, u32)>> = Mutex::new(None);
static SHOT_ARMED: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

fn arm_shot(phase: u64, bits: u32) {
    *SHOT.lock().unwrap_or_else(|e| e.into_inner()) = Some((phase, bits));
    SHOT_ARMED.store(true, std::sync::atomic::Ordering::Release);
}

/// What is asked for and not taken yet is dropped: the phase it was for is over.
fn clear_shot() {
    SHOT_ARMED.store(false, std::sync::atomic::Ordering::Release);
    *SHOT.lock().unwrap_or_else(|e| e.into_inner()) = None;
}

/// The phase and bits of the picture asked for, which then is not asked for any more.
fn take_pending() -> Option<(u64, u32)> {
    if !SHOT_ARMED.load(std::sync::atomic::Ordering::Acquire) {
        return None;
    }
    let taken = SHOT.lock().unwrap_or_else(|e| e.into_inner()).take();
    SHOT_ARMED.store(false, std::sync::atomic::Ordering::Release);
    taken
}

/// At a present of the host: the name of the picture this frame is to be saved as (`VICTORY_AB_SHOTS`), if one is asked for.
pub(crate) fn take_shot() -> Option<String> {
    let (phase, bits) = take_pending()?;
    Some(shot_name(phase, bits, asked()?.switches.len() == 1 || config().bundle))
}

/// Whether pictures can be saved: there is a capture folder (never the working directory).
fn can_shoot() -> bool {
    #[cfg(feature = "diagnostics")]
    {
        crate::capture::folder().is_some()
    }
    #[cfg(not(feature = "diagnostics"))]
    {
        false
    }
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
    /// This phase's picture was asked for.
    shot_armed: bool,
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
    let setup = setup();
    let asked = match &setup.plan {
        Plan::Run(asked) => Some(asked),
        // (The world is still looked for: it is where the refusal is said again.)
        Plan::Refused(_) => None,
        Plan::Off | Plan::NoDiagnostics => return,
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
    let config = &setup.config;
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
                if asked.is_some() {
                    tracing::info!("A/B: the world is found (the phases begin {:?} seconds after)", config.start_after);
                } else {
                    tracing::warn!("A/B: the world is found, and the A/B is NOT running (VICTORY_AB was refused)");
                }
                setup.say("the world is found");
            }
        }
        Some((ms, draws, frames))
    } else {
        None
    };
    let Some(asked) = asked else {
        return;
    };
    let begin_at = match config.start_after {
        None => Some(state.first),
        Some(after) => state.seconds.world.map(|w| w + Duration::from_secs_f64(after)),
    };
    let Some(begin_at) = begin_at.filter(|at| now >= *at) else {
        return;
    };
    let p = state.phase.get_or_insert_with(|| {
        tracing::info!("{}", begins(asked, config, &setup.raw));
        Phase { started: now, number: 0, began: now, last: now, drawn, frames: Vec::new(), draws: 0, means: Vec::new(), shot_armed: false }
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
    // A picture of this phase, asked for a second and a half before it ends (after what is left out at its start).
    if config.shots > 0 && !p.shot_armed {
        let arm_at = (asked.seconds - 1.5).max(config.skip + 0.5);
        if arm_at < asked.seconds
            && (now - p.began).as_secs_f64() >= arm_at
            && shot_due(p.number, config.shots, cycle_len(asked.switches.len(), config.bundle))
            && can_shoot()
        {
            arm_shot(p.number, bits(p.number));
            p.shot_armed = true;
        }
    }
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
    p.shot_armed = false;
    clear_shot();
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The list a pilot's run named: nine switches, one with a value, the A/B a bundle.
    const PILOT: &str = "10,VICTORY_UNDER_TRY,VICTORY_WAIT_ORDER,VICTORY_ONE_PASS,VICTORY_STAGE_A,VICTORY_STAGE_A_STEPS,\
                         VICTORY_PAGE_REFRESH=2,VICTORY_STAGE_A_RELAX=7,VICTORY_STAGE_A_DEEP,VICTORY_STAGE_A_PLAN";

    fn names(a: &Asked) -> Vec<&str> {
        a.switches.iter().map(String::as_str).collect()
    }

    /// The environment of a test: the variables it names, and nothing else.
    fn env<'a>(vars: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<String> + 'a {
        move |name| vars.iter().find(|(n, _)| *n == name).map(|(_, v)| (*v).to_string())
    }

    #[test]
    fn switches_and_seconds_are_read_and_frames_summed_up() {
        let a = parse("10, VICTORY_ONE_PASS,VICTORY_HOLDER_INDEX", false).ok().unwrap();
        assert_eq!((a.seconds, names(&a)), (10.0, vec!["VICTORY_ONE_PASS", "VICTORY_HOLDER_INDEX"]));
        assert!(a.notes.is_empty() && a.dropped.is_empty());
        assert_eq!(summary(&mut [30.0, 10.0, 20.0]), (20.0, 20.0, 30.0));
        assert_eq!(summary(&mut []), (0.0, 0.0, 0.0));
        assert_eq!(pick("VICTORY_NONE_SUCH"), None);
        assert_eq!(value_b("VICTORY_NONE_SUCH"), None);
    }

    #[test]
    fn a_switch_may_be_given_the_value_it_has_when_on() {
        let a = parse("20, VICTORY_REFRESH_NARROW=64 ,VICTORY_HOLDER_INDEX", false).ok().unwrap();
        assert_eq!(names(&a), vec!["VICTORY_REFRESH_NARROW", "VICTORY_HOLDER_INDEX"]);
        assert_eq!(a.values, vec![Some("64".to_string()), None]);
        assert_eq!(a.list(), "bit0 VICTORY_REFRESH_NARROW=64, bit1 VICTORY_HOLDER_INDEX");
    }

    /// The failure that wasted a pilot run: nine switches (one over what every combination can hold) and no word of it.
    #[test]
    fn nine_switches_run_as_a_bundle_and_are_refused_loudly_as_combinations() {
        let a = parse(PILOT, true).ok().unwrap();
        assert_eq!(a.switches.len(), 9);
        assert_eq!(a.values[5], Some("2".to_string()), "VICTORY_PAGE_REFRESH=2 is flipped with its value");
        assert_eq!(a.values[6], Some("7".to_string()));
        assert!(a.notes.is_empty() && a.dropped.is_empty(), "nothing is wrong with the list as a bundle: {:?}", a.notes);
        let Err(refusal) = parse(PILOT, false) else { panic!("nine switches cannot be every combination") };
        assert!(refusal.reason.contains("9 switches") && refusal.reason.contains("at most 8"), "{}", refusal.reason);
        assert!(refusal.reason.contains("VICTORY_AB_BUNDLE=1"), "it says what to do: {}", refusal.reason);
        // Eight combine, thirty-one bundle, thirty-two do not (plain on/off switches, as many as there are).
        let real: Vec<&str> =
            registry::LIVE.iter().filter(|(_, kind)| *kind == registry::Kind::Flag).map(|(name, _)| *name).collect();
        assert!(real.len() >= 32);
        let many = |n: usize| format!("10,{}", real[..n].join(","));
        assert_eq!(parse(&many(8), false).ok().map(|a| a.switches.len()), Some(8));
        assert!(parse(&many(9), false).is_err());
        assert_eq!(parse(&many(31), true).ok().map(|a| a.switches.len()), Some(31));
        let Err(refusal) = parse(&many(32), true) else { panic!("thirty-two do not fit the bits") };
        assert!(refusal.reason.contains("32 switches") && refusal.reason.contains("at most 31"), "{}", refusal.reason);
    }

    #[test]
    fn an_unknown_switch_is_dropped_by_name_and_the_rest_runs() {
        let a = parse("10,VICTORY_ONE_PASS,VICTORY_ONE_PAS,VICTORY_HOLDER_INDEX", false).ok().unwrap();
        assert_eq!(names(&a), vec!["VICTORY_ONE_PASS", "VICTORY_HOLDER_INDEX"]);
        assert_eq!(a.dropped, vec!["VICTORY_ONE_PAS".to_string()]);
        assert_eq!(a.notes.len(), 1);
        assert!(a.notes[0].starts_with("VICTORY_ONE_PAS:") && a.notes[0].contains("dropped"), "{}", a.notes[0]);
        assert!(a.notes[0].contains("did you mean VICTORY_ONE_PASS?"), "{}", a.notes[0]);
        // The name without its prefix is found too.
        let a = parse("10,VICTORY_ONE_PASS,PAGE_REFRESH=2", false).ok().unwrap();
        assert!(a.notes[0].contains("did you mean VICTORY_PAGE_REFRESH?"), "{}", a.notes[0]);
    }

    #[test]
    fn a_switch_that_is_not_live_is_dropped_and_says_so() {
        // Read once, in the loader: never through the A/B (docs/ab-switches.md).
        let a = parse("10,VICTORY_WAIT_NICE,VICTORY_ONE_PASS", false).ok().unwrap();
        assert_eq!(names(&a), vec!["VICTORY_ONE_PASS"]);
        assert!(a.notes[0].starts_with("VICTORY_WAIT_NICE:") && a.notes[0].contains("not a switch the A/B can flip"), "{}", a.notes[0]);
        assert!(a.notes[0].contains("live switches"), "{}", a.notes[0]);
    }

    #[test]
    fn a_value_the_switch_would_not_read_is_dropped_and_one_it_ignores_is_kept_with_a_note() {
        let a = parse(
            "10,VICTORY_PAGE_REFRESH=3,VICTORY_REFRESH_NARROW=wide,VICTORY_MAKE_ASIDE,VICTORY_LEAN_CARD=0,VICTORY_HOLDER_INDEX=,VICTORY_ONE_PASS=0",
            false,
        )
        .ok()
        .unwrap();
        assert_eq!(names(&a), vec!["VICTORY_LEAN_CARD", "VICTORY_ONE_PASS"]);
        assert_eq!(a.dropped.len(), 4);
        let note = |name: &str| {
            a.notes.iter().find(|n| n.starts_with(name)).unwrap_or_else(|| panic!("no note for {name}: {:?}", a.notes))
        };
        assert!(note("VICTORY_PAGE_REFRESH=3").contains("not one of 1, 2") && note("VICTORY_PAGE_REFRESH=3").ends_with("dropped"));
        assert!(note("VICTORY_REFRESH_NARROW=wide").contains("not a whole number"));
        assert!(note("VICTORY_MAKE_ASIDE").contains("both arms are on") && note("VICTORY_MAKE_ASIDE").contains("VICTORY_MAKE_ASIDE=0"));
        assert!(note("VICTORY_HOLDER_INDEX=").contains("empty value"));
        // A plain on/off switch given a value: kept, and said.
        assert!(note("VICTORY_ONE_PASS=0").contains("ignores its value") && note("VICTORY_ONE_PASS=0").ends_with("kept"));
    }

    #[test]
    fn a_name_given_twice_or_an_empty_entry_is_said() {
        let a = parse("10,VICTORY_ONE_PASS,VICTORY_ONE_PASS=1,,=3", false).ok().unwrap();
        assert_eq!(names(&a), vec!["VICTORY_ONE_PASS"]);
        assert_eq!(a.dropped, vec!["VICTORY_ONE_PASS=1".to_string(), "=3".to_string()]);
        assert!(a.notes.iter().any(|n| n.contains("named twice")));
        assert!(a.notes.iter().any(|n| n.contains("stray comma")));
        assert!(a.notes.iter().any(|n| n.contains("no switch name")));
    }

    #[test]
    fn a_list_that_cannot_run_is_refused_with_its_reason() {
        let reason = |text: &str| match parse(text, false) {
            Err(refusal) => refusal.reason,
            Ok(_) => panic!("{text:?} should be refused"),
        };
        assert!(reason("").contains("length of a phase"));
        assert!(reason("x,VICTORY_ONE_PASS").contains("got \"x\""));
        assert!(reason("0,VICTORY_ONE_PASS").contains("above 1"));
        assert!(reason("1,VICTORY_ONE_PASS").contains("above 1"));
        assert!(reason("inf,VICTORY_ONE_PASS").contains("above 1"));
        assert!(reason("10").contains("no switch is left"));
        assert!(reason("10,").contains("no switch is left"));
        // Every entry dropped leaves nothing, and the notes of the refusal name each.
        let Err(refusal) = parse("10,VICTORY_NOPE,VICTORY_PAGE_REFRESH=9", false) else { panic!("nothing is left") };
        assert!(refusal.reason.contains("every entry was dropped"));
        assert_eq!(refusal.notes.len(), 2);
    }

    #[test]
    fn the_setup_says_what_the_list_comes_to_at_each_moment() {
        let all = |setup: &Setup, when: &str| setup.lines(when).into_iter().map(|(_, l)| l).collect::<Vec<_>>().join("\n");
        // Not set, or empty: nothing to say.
        assert!(Setup::read(&env(&[]), true).lines("at startup").is_empty());
        assert!(Setup::read(&env(&[("VICTORY_AB", " ")]), true).lines("at startup").is_empty());
        // A list that runs: the final list is the one line, and no warning when nothing was dropped.
        let ok = Setup::read(&env(&[("VICTORY_AB", PILOT), ("VICTORY_AB_BUNDLE", "1")]), true);
        let lines = ok.lines("at startup");
        assert_eq!(lines.len(), 1);
        assert!(!lines[0].0, "no warning for a list that is as asked");
        assert!(lines[0].1.contains("will flip 9 switch(es)") && lines[0].1.contains("bit5 VICTORY_PAGE_REFRESH=2"), "{}", lines[0].1);
        // The same list as combinations: refused, at WARN, at startup and again where the world is found.
        let refused = Setup::read(&env(&[("VICTORY_AB", PILOT)]), true);
        for when in ["at startup", "the world is found"] {
            let lines = refused.lines(when);
            assert!(lines.iter().all(|(warn, _)| *warn), "{lines:?}");
            let text = all(&refused, when);
            assert!(text.contains(&format!("A/B ({when})")) && text.contains("NOT RUNNING") && text.contains("at most 8"), "{text}");
        }
        // A bad entry: WARN names it, and the final list leaves it out.
        let some = Setup::read(&env(&[("VICTORY_AB", "10,VICTORY_ONE_PASS,VICTORY_PAGE_REFRESH=3")]), true);
        let lines = some.lines("the world is found");
        assert!(lines.iter().all(|(warn, _)| *warn));
        let text = all(&some, "the world is found");
        assert!(text.contains("VICTORY_AB entry VICTORY_PAGE_REFRESH=3: not one of 1, 2"), "{text}");
        assert!(text.contains("will flip 1 switch(es): [bit0 VICTORY_ONE_PASS]; DROPPED 1 of the entries: [VICTORY_PAGE_REFRESH=3]"), "{text}");
        // A build without the A/B says it does not run.
        let release = Setup::read(&env(&[("VICTORY_AB", PILOT)]), false);
        assert!(matches!(release.plan, Plan::NoDiagnostics));
        assert!(all(&release, "at startup").contains("this build has no A/B"));
    }

    #[test]
    fn the_variables_of_the_phases_are_checked_too() {
        let vars = [("VICTORY_AB_START", "70"), ("VICTORY_AB_SKIP", "2"), ("VICTORY_AB_SHOTS", "4"), ("VICTORY_AB_BUNDLE", "1")];
        let (config, notes) = Config::read(&env(&vars));
        assert_eq!((config.start_after, config.skip, config.shots, config.bundle), (Some(70.0), 2.0, 4, true));
        assert!(notes.is_empty());
        let vars = [("VICTORY_AB_START", "soon"), ("VICTORY_AB_SKIP", "-1"), ("VICTORY_AB_SHOTS", "x"), ("VICTORY_AB_BUNDLE", "yes")];
        let (config, notes) = Config::read(&env(&vars));
        assert_eq!((config.start_after, config.skip, config.shots, config.bundle), (None, 1.0, 0, false));
        assert_eq!(notes.len(), 4, "{notes:?}");
        assert!(notes[0].starts_with("VICTORY_AB_START=\"soon\"") && notes[3].starts_with("VICTORY_AB_BUNDLE=\"yes\""), "{notes:?}");
        // Unset or empty is the default, without a word.
        assert!(Config::read(&env(&[("VICTORY_AB_START", "")])).1.is_empty());
    }

    #[test]
    fn the_line_that_begins_the_run_has_the_list_and_the_plan() {
        let vars = [
            ("VICTORY_AB", "10,VICTORY_PAGE_REFRESH=2,VICTORY_ONE_PASS,VICTORY_NOPE"),
            ("VICTORY_AB_START", "70"),
            ("VICTORY_AB_SKIP", "2"),
        ];
        let setup = Setup::read(&env(&vars), true);
        let Plan::Run(asked) = &setup.plan else { panic!("the list runs") };
        let line = begins(asked, &setup.config, &setup.raw);
        assert!(line.starts_with("A/B begins: 10 s phases (the first 2 s of each left out)"), "{line}");
        assert!(line.contains("70 s after the world is found"), "{line}");
        assert!(line.contains("[bit0 VICTORY_PAGE_REFRESH=2, bit1 VICTORY_ONE_PASS]"), "{line}");
        assert!(line.contains("every combination") && line.contains("p mod 4") && line.contains("4 phases a cycle"), "{line}");
        assert!(line.contains("DROPPED [VICTORY_NOPE]") && line.contains("no pictures"), "{line}");
        let vars = [("VICTORY_AB", PILOT), ("VICTORY_AB_BUNDLE", "1"), ("VICTORY_AB_SHOTS", "2")];
        let bundle = Setup::read(&env(&vars), true);
        let Plan::Run(asked) = &bundle.plan else { panic!("the bundle runs") };
        let line = begins(asked, &bundle.config, &bundle.raw);
        assert!(line.contains("at the first present") && line.contains("a bundle") && line.contains("one cycle in 2"), "{line}");
        assert!(line.contains("9 switch(es)"), "{line}");
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
    fn pictures_come_from_every_phase_of_every_nth_cycle_named_by_phase_and_arm() {
        // One switch (a cycle of two): every cycle is every pair; every 2nd cycle is phases 0, 1, 4, 5, 8, 9.
        let due = |shots, cycle| (0..10).filter(|&p| shot_due(p, shots, cycle)).collect::<Vec<u64>>();
        assert_eq!(due(1, 2), (0..10).collect::<Vec<u64>>());
        assert_eq!(due(2, 2), vec![0, 1, 4, 5, 8, 9]);
        assert_eq!(due(3, 2), vec![0, 1, 6, 7]);
        // Two switches (a cycle of four) and none asked for.
        assert_eq!(due(2, 4), vec![0, 1, 2, 3, 8, 9]);
        assert!(due(0, 2).is_empty());
        assert_eq!(cycle_len(1, false), 2);
        assert_eq!(cycle_len(3, false), 8);
        assert_eq!(cycle_len(5, true), 2);
        assert_eq!(shot_name(4, 0, true), "ab-4-A");
        assert_eq!(shot_name(5, 1, true), "ab-5-B");
        assert_eq!(shot_name(7, 0b111, true), "ab-7-B");
        assert_eq!(shot_name(6, 0b10, false), "ab-6-b2");
    }

    #[test]
    fn a_picture_asked_for_is_taken_once_and_dropped_when_its_phase_is_over() {
        // (Nothing else in the tests asks for pictures: this is the only one that touches them.)
        clear_shot();
        assert_eq!(take_pending(), None);
        arm_shot(6, 1);
        assert_eq!(take_pending(), Some((6, 1)));
        assert_eq!(take_pending(), None);
        arm_shot(8, 0);
        clear_shot();
        assert_eq!(take_pending(), None);
    }

    #[test]
    fn the_t_of_few_degrees_is_wide_and_of_many_is_the_normal() {
        assert_eq!(t95(1), 12.706);
        assert_eq!(t95(30), 2.042);
        assert_eq!(t95(31), 1.96);
        assert!(t95(0).is_infinite());
    }
}
