//! The console's float mode for 32-bit floats: denormals are flushed, as inputs and as results.
//!
//! Every program of the title's shader library (a census of every compute, vertex, pixel, mesh,
//! geometry and export record) carries the float mode word `0xC0`: 32-bit denormals flushed to
//! zero (`FP_DENORM` of f32 = 0), 16-bit and 64-bit ones kept, rounding to nearest even
//! everywhere. The host's DXIL default (`any`) lets the card keep them, and a value below
//! 1.2e-38 then lives on where the console has an exact zero: a damped velocity that has
//! decayed, the square of a small difference. The normalisation `v * rsq(dot(v, v))` of a vector
//! whose squares are denormal is the console's `legacy(v, +inf)` (the dot product is 0; a
//! legacy product is 0 only where a factor is exactly 0, so the result is an infinity) and the
//! card's `v * 1e20` (the dot product is a denormal, the root a large number): the two sides
//! then disagree in whether a value is finite.
//!
//! `VICTORY_F32_FTZ=1` writes the shader model 6.2 function attribute
//! `"fp32-denorm-mode"="ftz"` on the entry point, which asks the driver for the console's mode.
//! Off by default; part of the translation (the shader cache keys on it).

/// Whether the entry point asks for flushed 32-bit denormals (`VICTORY_F32_FTZ=1`).
pub(super) fn f32_ftz() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_F32_FTZ.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_F32_FTZ").is_some_and(|v| v == "1"))
}

#[cfg(test)]
thread_local! {
    /// A test's choice for [`f32_ftz`], for its thread only.
    pub(super) static TEST_F32_FTZ: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}
