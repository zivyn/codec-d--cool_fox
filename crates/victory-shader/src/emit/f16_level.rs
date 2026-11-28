//! How far `VICTORY_F16_NEAREST` reaches.
//!
//! The console rounds every 16-bit float result to nearest even (the float mode word of every
//! program of the title's library says so, and the hair simulation sets the mode again before
//! its 16-bit stores). The host's `f32tof16` rounds toward zero on the RTX, one step of the last
//! place lower in about a sixth of the cases (`nan_tests`: 24 of 144).
//!
//! * `VICTORY_F16_NEAREST=1`: the scalar 16-bit operations (`v_add_f16`, `v_mul_f16`,
//!   `v_cvt_f16_f32` ...), as before.
//! * `VICTORY_F16_NEAREST=2`: also the results the hair simulation writes its 16-bit state with,
//!   `v_fma_mixlo_f16` and `v_fma_mixhi_f16`, and the packed `v_pk_*_f16` ones. Level 1 never
//!   reached those.
//!
//! Level 2 is the default since integ/validation-24 (2026-11-28), with `VICTORY_F32_FTZ=1`: in the
//! pilot's runs the pair was clean (smooth walls, no red spot) where the controls without it
//! whited out; `=0` is off, `=1` the scalar operations only. Part of the translation (the shader
//! cache keys on the level in force, [`super::f16_nearest_level`]).

/// 0 (off), 1 (scalar 16-bit operations) or 2 (also mixed-precision and packed ones).
pub(super) fn level() -> u8 {
    #[cfg(test)]
    if let Some(level) = TEST_F16_LEVEL.get() {
        return level;
    }
    static LEVEL: std::sync::OnceLock<u8> = std::sync::OnceLock::new();
    *LEVEL.get_or_init(|| match std::env::var("VICTORY_F16_NEAREST").as_deref() {
        Ok("0") => 0,
        Ok("1") => 1,
        _ => 2,
    })
}

/// Whether the mixed-precision and packed 16-bit results round to nearest.
pub(super) fn mix_and_packed() -> bool {
    level() >= 2
}

#[cfg(test)]
thread_local! {
    /// A test's level, for its thread only.
    pub(super) static TEST_F16_LEVEL: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}
