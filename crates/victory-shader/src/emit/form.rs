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
    /// form without skips: the barriers are at those branches, outside the loops. Exact.
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
    /// SCC of a mask operation behind a per-lane skip, the skips taken wave-wide. Exact.
    LaneMaskWide,
    /// The program with the edges whose value is constant threaded.
    Threaded,
    /// No form lowered.
    Refused,
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
            Form::LaneMaskWide => "wave-wide lane-mask skips",
            Form::Threaded => "threaded edges",
            Form::Refused => "refused",
        }
    }
}
