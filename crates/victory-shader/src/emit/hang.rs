//! The hangs of the host GPU (a thread group that waits for threads that never come, or a loop
//! that runs to its limit with a group barrier in each round) and what the translation does
//! about them:
//!
//! - the audit of a finished module's group barriers ([`super::bitcode::together`]): which of
//!   them some thread of the group may not reach (`VICTORY_HANG_AUDIT=1` prints one line for each
//!   module that has barriers, the kept skips' wave-wide form is taken only if its modules pass);
//! - the guard (`VICTORY_HANG_GUARD`): a translation whose group barriers are counters in shared
//!   memory with a bounded wait, whose loops that wait are bounded, and whose waits and loops
//!   that give up leave a note in the faults buffer ([`HANG_NOTE`]) and end the thread, so that
//!   a program that would have hung the card ends instead, and the host says which one.

use std::cell::{Cell, RefCell};

use super::bitcode::together::Audit;

/// The notes of the guard: `HANG_NOTE | kind << 24 | the first five hex digits of the program's
/// hash << 4 | the site` (the barrier's or the loop's number, modulo 16). Above the notes of
/// [`super::PEEK_NOTE`] and below those of loops and of the host waves that gave up.
pub const HANG_NOTE: u32 = 0xD800_0000;

/// What a note of the guard says.
pub mod kind {
    /// A group barrier whose wait for the other waves ran out.
    pub const WAIT: u32 = 1;
    /// A wave that arrived at more barriers than the budget allows.
    pub const BUDGET: u32 = 2;
    /// A loop with a barrier in it that reached its (lowered) limit.
    pub const LOOP: u32 = 3;
    /// The host waves of a console wave that met by their own counters and gave up.
    pub const PAIR: u32 = 4;
}

/// The guard's settings, from `VICTORY_HANG_GUARD` (off if unset or `0`; `1` takes the defaults;
/// a number is how many rounds a wait lasts), `VICTORY_HANG_BUDGET` (barriers a wave may arrive
/// at) and `VICTORY_HANG_LOOP` (iterations of a loop with a barrier in it). Part of the
/// translation: the shader cache keys on all three.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Guard {
    pub spins: u32,
    pub budget: u32,
    pub loops: u32,
}

impl Guard {
    pub(super) const DEFAULT: Guard = Guard { spins: 1 << 18, budget: 1 << 16, loops: 1 << 10 };

    fn parse(guard: Option<&str>, budget: Option<&str>, loops: Option<&str>) -> Option<Guard> {
        let guard = guard?.trim();
        if guard.is_empty() || guard == "0" {
            return None;
        }
        let number = |s: Option<&str>| s.and_then(|s| s.trim().parse::<u32>().ok()).filter(|&n| n > 0);
        let d = Guard::DEFAULT;
        Some(Guard {
            spins: number(Some(guard)).filter(|&n| n >= 16).unwrap_or(d.spins),
            budget: number(budget).unwrap_or(d.budget),
            loops: number(loops).unwrap_or(d.loops),
        })
    }
}

/// The guard in force (`None`: the translation is what it was).
pub(super) fn guard() -> Option<Guard> {
    #[cfg(test)]
    if let Some(g) = TEST_GUARD.get() {
        return g;
    }
    static G: std::sync::OnceLock<Option<Guard>> = std::sync::OnceLock::new();
    *G.get_or_init(|| {
        let var = |n: &str| std::env::var(n).ok();
        Guard::parse(var("VICTORY_HANG_GUARD").as_deref(), var("VICTORY_HANG_BUDGET").as_deref(), var("VICTORY_HANG_LOOP").as_deref())
    })
}

#[cfg(test)]
thread_local! {
    /// A test's guard, for its thread only (`Some(None)`: off whatever the environment says).
    pub(super) static TEST_GUARD: Cell<Option<Option<Guard>>> = const { Cell::new(None) };
    /// The last module this thread finished, as the tests of the control flow simulate it.
    pub(super) static TEST_MODULE: RefCell<Option<super::bitcode::Module>> = const { RefCell::new(None) };
}

/// The note the guard leaves: for the code and for the host's message.
pub(super) fn note(kind: u32, tag: u32, site: u32) -> u32 {
    // (the tag is `PAIR_NOTE | the first seven hex digits`: the first five of them)
    let digits = tag >> 8 & 0x000F_FFFF;
    HANG_NOTE | (kind & 7) << 24 | digits << 4 | (site & 15)
}

/// What a note says: kind, the first five hex digits of the program's hash, the site.
pub fn say(note: u32) -> (u32, u32, u32) {
    (note >> 24 & 7, note >> 4 & 0x000F_FFFF, note & 15)
}

thread_local! {
    /// Whether the modules this thread finishes are audited (the kept skips' form is being tried).
    static WANT: Cell<bool> = const { Cell::new(false) };
    /// The audit of the last module this thread finished with barriers in it.
    static LAST: RefCell<Option<Audit>> = const { RefCell::new(None) };
}

/// `VICTORY_HANG_AUDIT=1`: one line on stderr for each module with a group barrier; `=2` the
/// module's control flow and instructions too.
fn printing() -> u8 {
    static ON: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_HANG_AUDIT").ok().as_deref() {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

/// Audits the modules this thread finishes from now on (and forgets the last audit), until
/// [`stop_auditing`].
pub(super) fn audit_from_now_on() {
    WANT.set(true);
    LAST.take();
}

pub(super) fn stop_auditing() {
    WANT.set(false);
}

/// The audit of the last module finished since [`audit_from_now_on`], if it had barriers.
pub(super) fn last_audit() -> Option<Audit> {
    LAST.with_borrow(Clone::clone)
}

/// Called with every finished module: audits it where that was asked for.
pub(super) fn finished(m: &super::bitcode::Module, tag: u32) {
    #[cfg(test)]
    TEST_MODULE.set(Some(m.clone()));
    if !WANT.get() && printing() == 0 {
        return;
    }
    let audit = m.audit_barriers();
    if audit.barriers == 0 {
        LAST.take();
        return;
    }
    if printing() > 0 {
        eprintln!("HANG-AUDIT #{:07x} {}", tag & 0x0FFF_FFFF, audit.describe());
    }
    if printing() > 1 {
        eprintln!("{}", m.listing());
    }
    LAST.set(Some(audit));
}
