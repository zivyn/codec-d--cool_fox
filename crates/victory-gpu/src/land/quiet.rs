//! Copies of buffers that only the card's shaders touch are left out of
//! landings (`VICTORY_LAND_QUIET=1`, off by default).
//!
//! A landing fetches what shaders wrote to a copy in the card's memory,
//! merges it into the copy's CPU side and into title memory, and opens or
//! notes the guards of the title's pages on the way. In a steady frame of
//! the world 39 of the 59 MB it fetched belonged to three buffers of
//! default heaps that no write of the title's CPU had touched since they
//! were made (one of them the 21 MB working buffer of the upscaler): 0 MB
//! up, 21, 15 and 3 MB down, every frame, into memory that Direct3D says
//! the CPU of the title cannot reach. The copies of such buffers are the
//! ones that skip their whole refreshes while nobody tells of a write
//! there (`crate::shadow::Shadow::mark_gpu_only`, `crate::watch::quiet_since`):
//! the same assumption, for the other direction.
//!
//! What is left out is the landing only. The card keeps the buffer as the
//! shaders left it, and the compare on the card and everything else about
//! the ranges (declared, done with) goes on. Title memory and the CPU's
//! side hold what they held at the last landing, both the same, which is
//! what a refresh that sends only what changed needs.
//!
//! When something is told written in the buffer (the title's kernel reads,
//! DirectStorage, our own copies of the title's data) the copy lands again:
//! the next landing fetches the whole copy, merged against the CPU's side
//! as any other, so that title memory is what the card holds. Never left
//! out again for that copy.

use std::sync::Mutex;

/// Landings of a copy before it is left out: the copy is one shaders write
/// regularly, and what was written to it in its making has landed.
const WARM: u32 = 60;
/// Landings left out before the quiet is noted: the last landings' own
/// notes (the asking after a write beside the guards, a block opened) are
/// told after they are done, and would read as a write of someone else's.
const DRAIN: u32 = 32;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum State {
    /// Landing as before; this many landings were seen.
    Landing(u32),
    /// Left out; this many were, since the first.
    Draining(u32),
    /// Left out; nothing was told written since this number
    /// (`crate::watch::tick`).
    Quiet(u64),
    /// Landing as before, for good.
    Off,
}

/// What to do with a copy's landing.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(crate) enum Verdict {
    /// As before.
    Land,
    /// Left out.
    Left,
    /// Something was told written in the copy since it was left out: this
    /// landing brings the whole copy, and the later ones are as before.
    Whole,
}

/// A copy's way through the states.
pub(crate) struct Quiet(Mutex<State>);

impl Default for Quiet {
    fn default() -> Quiet {
        Quiet(Mutex::new(State::Landing(0)))
    }
}

impl Quiet {
    /// What to do with the landing of the copy now. `eligible`: the copy may
    /// be left out at all. `tick` is `crate::watch::tick`, `quiet_since`
    /// `crate::watch::quiet_since` of the copy's range (nothing told written
    /// there since the number).
    pub(crate) fn landing(
        &self,
        eligible: bool,
        tick: impl FnOnce() -> u64,
        quiet_since: impl FnOnce(u64) -> bool,
    ) -> Verdict {
        if !eligible {
            return Verdict::Land;
        }
        let mut state = self.0.lock().unwrap_or_else(|e| e.into_inner());
        let (next, verdict) = match *state {
            State::Landing(n) if n + 1 < WARM => (State::Landing(n + 1), Verdict::Land),
            State::Landing(_) => (State::Draining(0), Verdict::Left),
            State::Draining(n) if n + 1 < DRAIN => (State::Draining(n + 1), Verdict::Left),
            State::Draining(_) => (State::Quiet(tick()), Verdict::Left),
            State::Quiet(since) if quiet_since(since) => (State::Quiet(since), Verdict::Left),
            State::Quiet(_) => (State::Off, Verdict::Whole),
            State::Off => (State::Off, Verdict::Land),
        };
        *state = next;
        verdict
    }

    /// Whether the copy is left out now.
    #[cfg(test)]
    fn is_left_out(&self) -> bool {
        matches!(
            *self.0.lock().unwrap_or_else(|e| e.into_inner()),
            State::Draining(_) | State::Quiet(_)
        )
    }
}

/// Whether copies are left out (`VICTORY_LAND_QUIET=1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_LAND_QUIET").is_some_and(|v| v == "1"))
}

/// The smallest copy that is left out, in bytes
/// (`VICTORY_LAND_QUIET_MIN_KB`, 1 MiB).
pub(crate) fn min_bytes() -> u64 {
    static MIN: std::sync::OnceLock<u64> = std::sync::OnceLock::new();
    *MIN.get_or_init(|| {
        std::env::var("VICTORY_LAND_QUIET_MIN_KB")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(1024)
            << 10
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// A copy that is never told written lands for [`WARM`] landings, is
    /// left out for good after that, and is asked about the quiet once
    /// [`DRAIN`] landings are left out.
    #[test]
    fn a_quiet_copy_is_left_out_after_a_warm_up() {
        let quiet = Quiet::default();
        let asked = Cell::new(0);
        for n in 0..WARM - 1 {
            assert_eq!(
                quiet.landing(true, || 7, |_| true),
                Verdict::Land,
                "landing {n}"
            );
        }
        for n in 0..1000 {
            let verdict = quiet.landing(
                true,
                || 41,
                |since| {
                    assert_eq!(since, 41, "the tick taken at the end of the drain");
                    asked.set(asked.get() + 1);
                    true
                },
            );
            assert_eq!(verdict, Verdict::Left, "left out {n}");
            assert!(quiet.is_left_out());
        }
        assert_eq!(
            asked.get(),
            1000 - DRAIN as usize - 1,
            "not asked while draining"
        );
    }

    /// What is told written after the drain ends the quiet once: the whole
    /// copy lands, then the landings are as before, whatever is asked.
    #[test]
    fn a_write_told_brings_the_whole_copy_once_and_lands_again() {
        let quiet = Quiet::default();
        for _ in 0..WARM + DRAIN {
            quiet.landing(true, || 5, |_| true);
        }
        assert!(quiet.is_left_out());
        assert_eq!(quiet.landing(true, || 6, |_| true), Verdict::Left);
        assert_eq!(
            quiet.landing(true, || 9, |since| since != 5),
            Verdict::Whole
        );
        assert!(!quiet.is_left_out());
        for _ in 0..100 {
            assert_eq!(quiet.landing(true, || 9, |_| true), Verdict::Land);
        }
    }

    /// A copy that may not be left out never is, and does not count its
    /// landings: one that may be later starts from the beginning.
    #[test]
    fn a_copy_that_is_not_eligible_always_lands() {
        let quiet = Quiet::default();
        for _ in 0..3 * (WARM + DRAIN) {
            assert_eq!(quiet.landing(false, || 1, |_| true), Verdict::Land);
        }
        assert!(!quiet.is_left_out());
        for _ in 0..WARM - 1 {
            assert_eq!(quiet.landing(true, || 1, |_| true), Verdict::Land);
        }
        assert_eq!(quiet.landing(true, || 1, |_| true), Verdict::Left);
    }

    /// The tick is read once, at the end of the drain, and not before.
    #[test]
    fn the_tick_is_read_at_the_end_of_the_drain() {
        let quiet = Quiet::default();
        let read = Cell::new(0);
        for _ in 0..WARM + DRAIN + 5 {
            quiet.landing(
                true,
                || {
                    read.set(read.get() + 1);
                    3
                },
                |_| true,
            );
        }
        assert_eq!(read.get(), 1);
    }
}
