//! The `VICTORY_*` on/off switches (`VICTORY_D3D12_DEBUG=1` and its siblings).

/// Whether the environment variable `name` switches something on: it is set,
/// and is not empty, `0` or `false` (`victory_shader::env_on`, the one rule).
///
/// A switch used to be on whenever its variable existed, so `VICTORY_D3D12_WARP=0` (typed to switch it off, or left
/// over from comparing two runs) still selected the software adapter.
pub fn on(name: &str) -> bool {
    victory_shader::env_on(name)
}

/// A switch that is on unless its variable is `0`, read once; with
/// `VICTORY_AB` naming it (diagnostics builds, `crate::ab`), on and off while
/// the title runs.
#[cfg(windows)]
#[macro_export]
#[doc(hidden)]
macro_rules! toggled {
    ($name:literal) => {{
        #[cfg(feature = "diagnostics")]
        {
            static BIT: std::sync::OnceLock<Result<bool, u32>> = std::sync::OnceLock::new();
            match BIT.get_or_init(|| match $crate::ab::bit($name) {
                Some(bit) => Err(bit),
                None => Ok(std::env::var_os($name).is_none_or(|v| v != "0")),
            }) {
                Ok(on) => *on,
                Err(bit) => $crate::ab::on(*bit),
            }
        }
        #[cfg(not(feature = "diagnostics"))]
        {
            static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *ON.get_or_init(|| std::env::var_os($name).is_none_or(|v| v != "0"))
        }
    }};
}
