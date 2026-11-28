//! The audio switches that `VICTORY_AB` may change while the title runs
//! (diagnostics builds; see `docs/ab-switches.md`): the same two forms as
//! `victory_gpu`'s `live_flag!` and `live_value!`, which are private to
//! that crate. Without `VICTORY_AB` naming a switch (and in the release
//! build) it is read from its variable once, as a plain switch is.

/// A switch that is off unless its variable is `1`; named by `VICTORY_AB`,
/// off and on by phase. Read at any instant.
macro_rules! live_flag {
    ($name:literal) => {{
        #[cfg(all(windows, feature = "diagnostics"))]
        {
            static BIT: std::sync::OnceLock<Result<bool, u32>> = std::sync::OnceLock::new();
            match BIT.get_or_init(|| match victory_gpu::ab::bit($name) {
                Some(bit) => Err(bit),
                None => Ok(std::env::var_os($name).is_some_and(|v| v == "1")),
            }) {
                Ok(on) => *on,
                Err(bit) => victory_gpu::ab::on(*bit),
            }
        }
        #[cfg(not(all(windows, feature = "diagnostics")))]
        {
            static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
            *ON.get_or_init(|| std::env::var_os($name).is_some_and(|v| v == "1"))
        }
    }};
}
pub(crate) use live_flag;

/// A switch with a value of type `$ty` that `$parse` makes of the text of
/// its variable (`None` when unset); named by `VICTORY_AB`, the unset value
/// in the phases without its bit and `<switch>=<value>` (`1` if none) in
/// those with it.
macro_rules! live_value {
    ($name:literal, $ty:ty, $parse:expr) => {{
        let parse: fn(Option<&str>) -> $ty = $parse;
        #[cfg(all(windows, feature = "diagnostics"))]
        {
            static CELL: std::sync::OnceLock<Result<$ty, (u32, $ty, $ty)>> =
                std::sync::OnceLock::new();
            match CELL.get_or_init(|| match victory_gpu::ab::bit($name) {
                Some(bit) => Err((
                    bit,
                    parse(None),
                    parse(Some(victory_gpu::ab::value_b($name).unwrap_or("1"))),
                )),
                None => Ok(parse(std::env::var($name).ok().as_deref())),
            }) {
                Ok(value) => *value,
                Err((bit, off, on)) => {
                    if victory_gpu::ab::on(*bit) {
                        *on
                    } else {
                        *off
                    }
                }
            }
        }
        #[cfg(not(all(windows, feature = "diagnostics")))]
        {
            static CELL: std::sync::OnceLock<$ty> = std::sync::OnceLock::new();
            *CELL.get_or_init(|| parse(std::env::var($name).ok().as_deref()))
        }
    }};
}
pub(crate) use live_value;

/// The arms of `VICTORY_AB` that are on now, a bit per named switch (0
/// without `VICTORY_AB` and in the release build).
pub fn arm() -> u32 {
    #[cfg(all(windows, feature = "diagnostics"))]
    {
        victory_gpu::ab::bits()
    }
    #[cfg(not(all(windows, feature = "diagnostics")))]
    {
        0
    }
}
