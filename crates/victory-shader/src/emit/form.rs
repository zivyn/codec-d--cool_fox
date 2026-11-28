//! The form a console wave64 compute program was lowered in for a host of 32-lane waves
//! ([`super::emit_forms`]): the choice `emit_forms` makes among the exact forms (every result is the
//! console's), the approximate ones (each host half a wave of its own: counts, ballots, first
//! lane and sums are per half) and the wave-wide branches (group barriers at the branches the
//! console takes for the whole wave). Returned with the shader ([`super::emit_as`]) so that a
//! test, and the corpus run (`VICTORY_WAVE_WHY`), can say which form a program ended in without
//! reading stderr.

/// How a program was lowered. Programs that needed no retry are [`Form::Original`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Form {
    /// The program as it is (a wave of 32 lanes, or a console wave that meets itself).
    Original,
    /// Without its early-outs ([`crate::ir::without_early_outs`]). Exact.
    WithoutEarlyOuts,
    /// Without the skips no lane is left for ([`crate::ir::without_skips`]). Exact.
    WithoutSkips,
    /// As [`Form::WithoutSkips`], with scalars followed (`deep`). Exact.
    WithoutSkipsDeep,
    /// The skips and early-outs `VICTORY_KEEP_LOOP_SKIPS` keeps over a loop, taken by the whole
    /// wave (the two host waves exchange the decision and go the same way), the rest as in the
    /// form without skips; only where no branch of it can hang the thread group
    /// ([`super::wide_class`]). Exact.
    KeptSkipsWide,
    /// Each host wave a wave of its own, where the wave operations are the allocation idiom. Exact.
    LocalIdiom,
    /// Each host wave a wave of its own, the wave operations answering per half. APPROXIMATE.
    LocalApproximate,
    /// As [`Form::LocalApproximate`], with every mask and lane read per host wave. APPROXIMATE.
    LocalApproximateAnyMask,
    /// The per-lane branches around cross-lane code taken wave-wide, before the approximate forms
    /// (the programs named by `VICTORY_WAVE_WIDE_FIRST`, `VICTORY_WAVE_WIDE=1`). Exact.
    WaveWide,
    /// The same, tried last of all. Exact.
    WaveWideLastResort,
    /// The same, taken before "without skips, scalars followed" where that form would run an
    /// exchange of the two host waves after lanes branched alone (`VICTORY_WAVE_WIDE_AUTO`). Exact.
    WaveWideAuto,
    /// SCC of a mask operation behind a per-lane skip, the skips taken wave-wide. Exact.
    LaneMaskWide,
    /// The program with the edges whose value is constant threaded.
    Threaded,
    /// No form lowered.
    Refused,
}

/// How much of the kept skips' wave-wide form `VICTORY_KEPT_SKIPS_WIDE` allows.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Mode {
    /// The default (unset, or `=0`): none.
    Off,
    /// `=1`: where no branch of the form can hang the thread group (class A or B,
    /// [`super::wide_class`]) and the audit of the emitted control flow finds no group barrier
    /// that a branch on a value that differs between threads can keep a thread from.
    Safe,
    /// `=strict`: as `Safe`, and no group barrier behind a branch on a value read from memory
    /// either (the halves may read it at different times), and no loop with a group barrier in
    /// another loop with one (a loop that runs to its limit costs the limit's number of barriers:
    /// the square of it in two loops).
    Strict,
    /// `=classes`: the classes alone (what `=1` was before the audit).
    Classes,
    /// `=audited`: only what the audit of the emitted control flow proves clean: as `Strict`
    /// (no barrier behind a branch that differs or is read from memory), and every loop that
    /// holds a group barrier ends by a condition all the threads of the group agree on, or is
    /// bounded (nested ones cut at the group's size), so that it cannot run to its limit with a
    /// barrier in each round. `VICTORY_KEPT_SKIPS_ONLY=<hash digits>,...` names the programs.
    Audited,
    /// `=all`: whatever the classes say (to see the form the title lost the card in).
    All,
}

/// `VICTORY_KEPT_SKIPS_WIDE`: with `VICTORY_KEEP_LOOP_SKIPS`, the skips and early-outs over loops of
/// a program that has to be retried are kept and taken wave-wide ([`Form::KeptSkipsWide`],
/// [`super::kept_wide`]), where that is safe. OFF unless `=1` (the safe subset) or `=all`: with it
/// on by default (integ/validation-21, 116 programs in the form) the title lost the card while
/// loading in 2 of about 4 starts, against none in about 7 starts without it; DefragHeap's cut
/// (7.8 ms) waits for a subset that is proven safe. Part of the translation: the shader cache keys
/// on the value in force ([`kept_skips_code`]).
pub(super) fn kept_skips_mode() -> Mode {
    #[cfg(test)]
    if let Some(mode) = TEST_KEPT_SKIPS_WIDE.get() {
        return mode;
    }
    match std::env::var("VICTORY_KEPT_SKIPS_WIDE").ok().as_deref().map(str::trim) {
        Some("all") => Mode::All,
        Some("strict") => Mode::Strict,
        Some("classes") => Mode::Classes,
        Some("audited") => Mode::Audited,
        Some("1") => Mode::Safe,
        _ => Mode::Off,
    }
}

/// [`kept_skips_mode`] as a number for the shader cache key (0 off, 1 safe, 2 all, 3 strict,
/// 4 classes, 5 audited).
pub(super) fn kept_skips_code() -> u8 {
    match kept_skips_mode() {
        Mode::Off => 0,
        Mode::Safe => 1,
        Mode::All => 2,
        Mode::Strict => 3,
        Mode::Classes => 4,
        Mode::Audited => 5,
    }
}

#[cfg(test)]
thread_local! {
    /// A test's `VICTORY_KEPT_SKIPS_WIDE`, for its thread only.
    pub(super) static TEST_KEPT_SKIPS_WIDE: std::cell::Cell<Option<Mode>> = const { std::cell::Cell::new(None) };
}

impl Form {
    /// Whether every result of the form is the console's (not per half-wave).
    pub fn exact(self) -> bool {
        !matches!(self, Form::LocalApproximate | Form::LocalApproximateAnyMask)
    }

    /// The name the logs and the tests use.
    pub fn label(self) -> &'static str {
        match self {
            Form::Original => "original",
            Form::WithoutEarlyOuts => "without early-outs",
            Form::WithoutSkips => "without skips",
            Form::WithoutSkipsDeep => "without skips, scalars followed",
            Form::KeptSkipsWide => "kept skips wave-wide",
            Form::LocalIdiom => "local waves (idiom)",
            Form::LocalApproximate => "local waves (APPROXIMATE)",
            Form::LocalApproximateAnyMask => "local waves (APPROXIMATE, any mask)",
            Form::WaveWide => "wave-wide branches",
            Form::WaveWideLastResort => "wave-wide branches (last resort)",
            Form::WaveWideAuto => "wave-wide branches (auto)",
            Form::LaneMaskWide => "wave-wide lane-mask skips",
            Form::Threaded => "threaded edges",
            Form::Refused => "refused",
        }
    }
}
