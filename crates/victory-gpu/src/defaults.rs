//! The settings the title is played with, built into the executable.
//!
//! A `VICTORY_*` variable is a way to ask for something else, not a thing
//! that has to be set to get the best known behaviour: the executable alone,
//! with no variable in its environment, runs with [`DEFAULTS`] (what the
//! launcher and the pilot's runs set until then, each checked in the game).
//! A variable that is set wins, `=0` turns a default that is on off (a few
//! take a number: `VICTORY_JOIN_PARTS=1`, `VICTORY_MAP_THREADS=1`
//! stands for off or one).
//!
//! The rule: a switch is only a way to compare. When the comparison (in the
//! process by `VICTORY_AB`, or in pairs of runs) shows the switch a win, the
//! next integration moves it here; one that shows none is removed, or kept
//! off with a line in its doc comment that says it showed none.
//!
//! This crate's own tests read the environment only (the switches off or as
//! the code says), so that the tests do not depend on the title's settings.

use std::env::VarError;
use std::ffi::OsString;

/// What an unset variable is read as: the settings of the launcher before
/// they were built in (`Play-VIctory.cmd`, and the pilot's `auto-run.ps1`
/// besides its diagnostics: the log level, the panel, the captures).
///
/// `VICTORY_RELAY` and `VICTORY_WIDE_BACK` were on by default in the code
/// already; they are here so that this is the whole set.
pub const DEFAULTS: &[(&str, &str)] = &[
    ("VICTORY_AMBIENT_AIR", "1.5"),
    ("VICTORY_TEXTURE_NO_WAIT", "1"),
    ("VICTORY_RELAY", "2"),
    ("VICTORY_WIDE_BACK", "1"),
    ("VICTORY_CARD_REHASH_RUNS", "1"),
    ("VICTORY_JOIN_COPIES", "1"),
    ("VICTORY_INDIRECT_WINDOW", "1"),
    ("VICTORY_RELAY_COPY", "1"),
    ("VICTORY_CARD_STEPS", "2"),
    ("VICTORY_INDIRECT_BATCH", "2"),
    ("VICTORY_MAP_THREADS", "1"),
    ("VICTORY_CARD_AWAKE", "1"),
    ("VICTORY_SHOW_AFTER", "1"),
    ("VICTORY_AMBIENT", "0.8"),
    ("VICTORY_THIN", "1"),
    ("VICTORY_JOIN_PARTS", "8"),
    ("VICTORY_UPLOAD_AHEAD", "1"),
    ("VICTORY_LEVELS_ON_DEMAND", "1"),
    ("VICTORY_CARD_BUFFERS_MIN_KB", "64"),
];

/// What `switch` is when nothing sets it.
pub fn default_of(switch: &str) -> Option<&'static str> {
    DEFAULTS
        .iter()
        .find(|(name, _)| *name == switch)
        .map(|(_, value)| *value)
}

/// The value of `switch`: the environment's if it is set, else its default.
pub fn var(switch: &str) -> Result<String, VarError> {
    resolve(std::env::var(switch), switch, !cfg!(test))
}

/// [`var`] for a value that need not be text.
pub fn var_os(switch: &str) -> Option<OsString> {
    std::env::var_os(switch).or_else(|| {
        if cfg!(test) {
            None
        } else {
            default_of(switch).map(OsString::from)
        }
    })
}

/// [`var`] with the environment's answer given: an unset variable (not one
/// that is set to something that is not text) reads as its default, if the
/// defaults apply.
fn resolve(
    env: Result<String, VarError>,
    switch: &str,
    defaults: bool,
) -> Result<String, VarError> {
    match env {
        Err(VarError::NotPresent) if defaults => default_of(switch)
            .map(str::to_string)
            .ok_or(VarError::NotPresent),
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The settings `Play-VIctory.cmd` and the pilot's `auto-run.ps1` set before
    /// they were built in: the executable alone must behave as with these.
    const OLD_LAUNCHER: [(&str, &str); 19] = [
        ("VICTORY_AMBIENT_AIR", "1.5"),
        ("VICTORY_TEXTURE_NO_WAIT", "1"),
        ("VICTORY_RELAY", "2"),
        ("VICTORY_WIDE_BACK", "1"),
        ("VICTORY_CARD_REHASH_RUNS", "1"),
        ("VICTORY_JOIN_COPIES", "1"),
        ("VICTORY_INDIRECT_WINDOW", "1"),
        ("VICTORY_RELAY_COPY", "1"),
        ("VICTORY_CARD_STEPS", "2"),
        ("VICTORY_INDIRECT_BATCH", "2"),
        ("VICTORY_MAP_THREADS", "1"),
        ("VICTORY_CARD_AWAKE", "1"),
        ("VICTORY_SHOW_AFTER", "1"),
        ("VICTORY_AMBIENT", "0.8"),
        ("VICTORY_THIN", "1"),
        ("VICTORY_JOIN_PARTS", "8"),
        ("VICTORY_UPLOAD_AHEAD", "1"),
        ("VICTORY_LEVELS_ON_DEMAND", "1"),
        ("VICTORY_CARD_BUFFERS_MIN_KB", "64"),
    ];

    #[test]
    fn the_defaults_are_the_old_launchers_settings() {
        assert_eq!(DEFAULTS, OLD_LAUNCHER);
        for (name, value) in OLD_LAUNCHER {
            assert_eq!(default_of(name), Some(value), "{name}");
        }
        let mut names: Vec<_> = DEFAULTS.iter().map(|(name, _)| *name).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), DEFAULTS.len(), "a switch is listed twice");
        assert_eq!(default_of("VICTORY_NONE_SUCH"), None);
    }

    #[test]
    fn a_set_variable_wins_and_an_unset_one_reads_as_its_default() {
        let unset = || Err(VarError::NotPresent);
        assert_eq!(resolve(unset(), "VICTORY_THIN", true), Ok("1".to_string()));
        assert_eq!(
            resolve(Ok("0".to_string()), "VICTORY_THIN", true),
            Ok("0".to_string())
        );
        assert_eq!(
            resolve(Ok(String::new()), "VICTORY_THIN", true),
            Ok(String::new())
        );
        assert_eq!(
            resolve(unset(), "VICTORY_THIN", false),
            Err(VarError::NotPresent)
        );
        assert_eq!(
            resolve(unset(), "VICTORY_NONE_SUCH", true),
            Err(VarError::NotPresent)
        );
        let odd = VarError::NotUnicode(OsString::from("x"));
        assert_eq!(resolve(Err(odd.clone()), "VICTORY_THIN", true), Err(odd));
    }

    /// The numbers among the defaults parse as numbers (the text ones are
    /// checked where they are read: `ambient`).
    #[test]
    fn the_numbers_among_the_defaults_parse() {
        assert_eq!(
            default_of("VICTORY_CARD_BUFFERS_MIN_KB")
                .unwrap()
                .parse::<u64>(),
            Ok(64)
        );
        assert_eq!(
            default_of("VICTORY_JOIN_PARTS").unwrap().parse::<usize>(),
            Ok(8)
        );
        assert_eq!(
            default_of("VICTORY_MAP_THREADS").unwrap().parse::<usize>(),
            Ok(1)
        );
        assert_eq!(
            default_of("VICTORY_CARD_STEPS").unwrap().parse::<u32>(),
            Ok(2)
        );
    }
}
