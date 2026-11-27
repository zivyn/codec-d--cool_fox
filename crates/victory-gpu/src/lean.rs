//! Caps on the memory the host keeps for reuse, for a PC with little of it.
//!
//! The host keeps memory it could give back so that the next ask finds it:
//! snapshots of copies, scratch chunks of lists, the memory submissions send
//! up, empty blocks of staging, the buffers pieces come down to. On a PC of
//! more than 32 GiB some of that is kept for minutes ([`crate::host::keep_freed`]),
//! and one of 16 GiB ran out of memory with the snapshots alone (run 80).
//! All of it is off by default, so that a run's frame times stay what they
//! were; `VICTORY_RAM_LEAN=1` turns the caps below on together, `2` also
//! cuts the windows of title memory finer, and each has a variable of its
//! own that wins over the level:
//!
//! - `VICTORY_KEEP_SNAPSHOTS_MB` (1024; lean 256): snapshots kept between
//!   submissions ([`crate::shadow`]).
//! - `VICTORY_SCRATCH_SPARE_MB` (1024 on a PC of more than 32 GiB, else
//!   128; lean 128): scratch chunks no list holds.
//! - `VICTORY_SEND_SPARE_MB` (768; lean 128): memory the submissions send
//!   up with, that nothing holds.
//! - `VICTORY_STAGING_SPARE` (8; lean 2): empty blocks of texture staging.
//! - `VICTORY_FETCH_TRIM` (off; lean on): the buffers what shaders wrote
//!   comes down to are made smaller again when a minute of fetches never
//!   needed a quarter of them.
//! - `VICTORY_WINDOW_KB` and `VICTORY_WINDOW_TAILS` (level 2: 1024 and on):
//!   see `shadow::window`.
//!
//! `VICTORY_RAM_TOTAL_GB=N` makes the host take the PC for one with N GiB
//! (for the caps that depend on the size of the PC: [`crate::residency`]),
//! to see on a PC of 40 GiB what one of 32 GiB gets.

use std::sync::OnceLock;

/// The level: 0 off, 1 the caps, 2 the caps and finer windows.
pub(crate) fn level() -> u8 {
    static LEVEL: OnceLock<u8> = OnceLock::new();
    *LEVEL.get_or_init(|| level_of(std::env::var("VICTORY_RAM_LEAN").ok().as_deref()))
}

fn level_of(value: Option<&str>) -> u8 {
    match value.map(str::trim) {
        None | Some("" | "0") => 0,
        Some("2") => 2,
        Some(_) => 1,
    }
}

/// A number the variable `name` sets, if it does.
fn set(name: &str) -> Option<u64> {
    std::env::var(name).ok().and_then(|v| v.trim().parse().ok())
}

/// What a cap is: the variable's number; else, at level 1 or more, `lean`;
/// else `normal`.
fn cap(setting: Option<u64>, level: u8, lean: u64, normal: u64) -> u64 {
    match setting {
        Some(n) => n,
        None if level >= 1 => lean,
        None => normal,
    }
}

/// Megabytes of snapshots kept between submissions.
pub(crate) fn snapshots_mb(normal: u64) -> u64 {
    cap(set("VICTORY_KEEP_SNAPSHOTS_MB"), level(), 256, normal)
}

/// Megabytes of scratch chunks kept while no list holds them.
pub(crate) fn scratch_spare_mb(normal: u64) -> u64 {
    cap(set("VICTORY_SCRATCH_SPARE_MB"), level(), 128, normal)
}

/// Megabytes of the memory submissions send up with, kept while none does.
pub(crate) fn send_spare_mb(normal: u64) -> u64 {
    cap(set("VICTORY_SEND_SPARE_MB"), level(), 128, normal)
}

/// Empty blocks of texture staging that are kept.
pub(crate) fn staging_spare(normal: usize) -> usize {
    cap(set("VICTORY_STAGING_SPARE"), level(), 2, normal as u64) as usize
}

/// Whether the buffers the fetches come down to shrink ([`Trim`]).
pub(crate) fn fetch_trim() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| match std::env::var("VICTORY_FETCH_TRIM") {
        Ok(v) => v.trim() != "0",
        Err(_) => level() >= 1,
    })
}

/// The window grid in KiB that level 2 asks for.
pub(crate) fn window_kb() -> Option<u64> {
    (level() >= 2).then_some(1024)
}

/// Whether level 2 asks for tails.
pub(crate) fn window_tails() -> bool {
    level() >= 2
}

/// The PC's physical memory, in bytes, as the host takes it: what
/// `VICTORY_RAM_TOTAL_GB` says, else `real`.
pub(crate) fn total_physical(real: u64) -> u64 {
    total_of(set("VICTORY_RAM_TOTAL_GB"), real)
}

fn total_of(setting: Option<u64>, real: u64) -> u64 {
    match setting {
        Some(gb) if gb > 0 => gb << 30,
        _ => real,
    }
}

/// How many uses of a buffer make its window.
const WINDOW: u32 = 1800;

/// What a buffer that only grows is worth keeping at: it was made for the
/// biggest ask of the run, and stays that big for ever. This counts its
/// asks; at the end of each [`WINDOW`] of them, a buffer that is four times
/// the biggest need of the window is to be made again, at that need.
#[derive(Default)]
pub(crate) struct Trim {
    asks: u32,
    most: u64,
}

impl Trim {
    pub(crate) const fn new() -> Trim {
        Trim { asks: 0, most: 0 }
    }

    /// One ask for `need` bytes of a buffer of `have`: the size to make it
    /// anew at, if this ask ended a window in which `have` was more than
    /// four times what was needed (`unit`: sizes are made up to a multiple
    /// of it).
    pub(crate) fn ask(&mut self, have: u64, need: u64, unit: u64) -> Option<u64> {
        self.most = self.most.max(need);
        self.asks += 1;
        if self.asks < WINDOW {
            return None;
        }
        let most = self.most.next_multiple_of(unit).max(unit);
        (self.asks, self.most) = (0, 0);
        (have / 4 >= most).then_some(most)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn levels_come_from_the_variable() {
        assert_eq!(level_of(None), 0);
        assert_eq!(level_of(Some("0")), 0);
        assert_eq!(level_of(Some("")), 0);
        assert_eq!(level_of(Some("1")), 1);
        assert_eq!(level_of(Some(" 2 ")), 2);
        assert_eq!(level_of(Some("yes")), 1);
    }

    #[test]
    fn a_variable_wins_over_the_level() {
        assert_eq!(cap(None, 0, 256, 1024), 1024);
        assert_eq!(cap(None, 1, 256, 1024), 256);
        assert_eq!(cap(None, 2, 256, 1024), 256);
        assert_eq!(cap(Some(512), 0, 256, 1024), 512);
        assert_eq!(cap(Some(0), 1, 256, 1024), 0);
    }

    #[test]
    fn the_pcs_memory_is_what_is_said() {
        assert_eq!(total_of(None, 40 << 30), 40 << 30);
        assert_eq!(total_of(Some(32), 40 << 30), 32 << 30);
        assert_eq!(total_of(Some(0), 40 << 30), 40 << 30);
    }

    #[test]
    fn a_buffer_shrinks_after_a_window_of_small_asks() {
        let unit = 16 << 20;
        let mut trim = Trim::new();
        // A buffer of 512 MiB asked for 20 MiB at a time: nothing happens
        // until the window is full, then it is made at 32 MiB.
        for _ in 1..WINDOW {
            assert_eq!(trim.ask(512 << 20, 20 << 20, unit), None);
        }
        assert_eq!(trim.ask(512 << 20, 20 << 20, unit), Some(32 << 20));
        // The next window starts empty.
        assert_eq!(trim.ask(32 << 20, 20 << 20, unit), None);
    }

    #[test]
    fn a_buffer_the_window_used_stays() {
        let unit = 16 << 20;
        let mut trim = Trim::new();
        // One big ask in the window keeps the buffer.
        assert_eq!(trim.ask(512 << 20, 400 << 20, unit), None);
        for _ in 2..WINDOW {
            assert_eq!(trim.ask(512 << 20, 1 << 20, unit), None);
        }
        assert_eq!(trim.ask(512 << 20, 1 << 20, unit), None);
        // A buffer that is not four times the need stays too.
        let mut trim = Trim::new();
        for _ in 1..=WINDOW {
            assert_eq!(trim.ask(64 << 20, 20 << 20, unit), None);
        }
    }
}
