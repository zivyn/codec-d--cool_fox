//! The switches `VICTORY_AB` can flip, one entry each, and what a value after `=` may be for each.
//!
//! A name that is not here is not read through `live_flag!`, `live_value!`, `toggled!`, `asked_for!`, `pick`, `bit`,
//! `value_b` or `switched` (a test reads the sources and says so when this list and those uses differ), so naming it
//! changes nothing while the title runs: [`super::parse`] drops it, loudly.

/// How a switch reads the phase's value, which is what a value after `=` in `VICTORY_AB` may be.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Plain on and off: arm A is off, arm B is on (`live_flag!`, `toggled!`, `asked_for!`, `pick`). A value is ignored.
    Flag,
    /// On when its variable is unset (`unless_zero`): arm A is the unset variable, arm B the value, and only `0` turns it
    /// off, so it has to be named `=0`.
    OnByDefault,
    /// A whole number (`<switch>=<n>`; no value is `1`).
    Count,
    /// A number that may have a fraction.
    Float,
    /// One of these texts (no value is `1`); any other text reads as the unset variable, the same as arm A.
    Modes(&'static [&'static str]),
}

/// What [`check_value`] says of a value.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Check {
    Fine,
    /// Kept, with this said about it.
    Odd(String),
    /// The entry is dropped, for this.
    Bad(String),
}

/// Every switch that may be named, sorted by name.
pub(crate) const LIVE: &[(&str, Kind)] = &[
    ("VICTORY_ALIAS_BATCH", Kind::Flag),
    ("VICTORY_AMBIENT_GAIN", Kind::Float),
    ("VICTORY_ASK_ASIDE", Kind::OnByDefault),
    ("VICTORY_ASK_CACHE", Kind::Flag),
    ("VICTORY_ASK_EARLY", Kind::Float),
    ("VICTORY_ASK_GAP_MB", Kind::Count),
    ("VICTORY_ASK_MERGE", Kind::OnByDefault),
    ("VICTORY_ASYNC_COMPUTE", Kind::Modes(&["1", "2"])),
    ("VICTORY_ASYNC_SUBMIT", Kind::Flag),
    ("VICTORY_BINDLESS_HOST", Kind::Modes(&["1", "2"])),
    ("VICTORY_CARD_AWAKE", Kind::Flag),
    ("VICTORY_CARD_BIG_FIRST", Kind::Flag),
    ("VICTORY_CLOSED_IDLE", Kind::OnByDefault),
    ("VICTORY_COPY_BESIDE", Kind::Flag),
    ("VICTORY_DROP_EARLY", Kind::Flag),
    ("VICTORY_DSP_CONVOLVE", Kind::Flag),
    ("VICTORY_DSP_CONVOLVE_GAIN", Kind::Float),
    ("VICTORY_DSP_REVERB", Kind::Flag),
    ("VICTORY_DSP_REVERB_LEVEL", Kind::Float),
    ("VICTORY_FAULT_AROUND", Kind::Count),
    ("VICTORY_FAULT_SPIN", Kind::Count),
    ("VICTORY_FOLD_VIEWS", Kind::Flag),
    ("VICTORY_FRAMEGEN_HUDLESS", Kind::OnByDefault),
    ("VICTORY_FRAMEGEN_LIVE", Kind::Flag),
    ("VICTORY_FRONT_HIGH", Kind::Flag),
    ("VICTORY_FUSE_RUNS", Kind::Flag),
    ("VICTORY_GPU_RO_SKIP", Kind::Flag),
    ("VICTORY_GUARD_REST", Kind::Flag),
    ("VICTORY_GUARD_SHARDS", Kind::Count),
    ("VICTORY_HEAPS_EARLY", Kind::Flag),
    ("VICTORY_HEAP_SYNC_BACKOFF", Kind::Count),
    ("VICTORY_HOLDER_INDEX", Kind::Flag),
    ("VICTORY_INDIRECT_FOLD", Kind::Flag),
    ("VICTORY_JOIN_PARTS", Kind::Flag),
    ("VICTORY_JOIN_RAMP", Kind::Modes(&["1"])),
    ("VICTORY_JOIN_WAITS", Kind::Flag),
    ("VICTORY_JOIN_WIDE_MB", Kind::Count),
    ("VICTORY_KEEP_FAST", Kind::Flag),
    ("VICTORY_KEEP_SHOWN", Kind::Flag),
    ("VICTORY_LAND_ASIDE", Kind::OnByDefault),
    ("VICTORY_LAND_ECHO", Kind::Flag),
    ("VICTORY_LAND_OWN", Kind::Flag),
    ("VICTORY_LAND_OWN_UNTAKEN", Kind::Flag),
    ("VICTORY_LAND_TAIL_THREADS", Kind::Count),
    ("VICTORY_LANE_CORE", Kind::Flag),
    ("VICTORY_LANE_HIGH", Kind::Flag),
    ("VICTORY_LEAN_CARD", Kind::OnByDefault),
    ("VICTORY_LEAN_STEPS", Kind::OnByDefault),
    ("VICTORY_LEVELS_RETRY", Kind::OnByDefault),
    ("VICTORY_LIST_NICE", Kind::Count),
    ("VICTORY_LIST_VIEWS", Kind::Flag),
    ("VICTORY_MAKE_ASIDE", Kind::OnByDefault),
    ("VICTORY_MAP_THREADS", Kind::Flag),
    ("VICTORY_ONE_CALL", Kind::Flag),
    ("VICTORY_ONE_PASS", Kind::Flag),
    ("VICTORY_OPEN_ONE_VIEW", Kind::Flag),
    ("VICTORY_OPUS_PITCH", Kind::Flag),
    ("VICTORY_PAGE_REFRESH", Kind::Modes(&["1", "2"])),
    ("VICTORY_PIPELINE_BOOST", Kind::Flag),
    ("VICTORY_PIPELINE_PREPARE", Kind::Flag),
    ("VICTORY_PIPELINE_PRIORITY", Kind::OnByDefault),
    ("VICTORY_PIPELINE_WAIT_ADAPT", Kind::Flag),
    ("VICTORY_PLACED_CONSTANTS", Kind::Flag),
    ("VICTORY_PLACED_ORDER", Kind::Flag),
    ("VICTORY_PRECISE_BARRIERS", Kind::Flag),
    ("VICTORY_PRECISE_LOOK", Kind::Flag),
    ("VICTORY_PRECISE_WRITES", Kind::Flag),
    ("VICTORY_QUICK_FIND", Kind::Flag),
    ("VICTORY_QUIET_COMPARE", Kind::Flag),
    ("VICTORY_QUIET_COPIES", Kind::Flag),
    ("VICTORY_RANGE_VALID", Kind::Flag),
    ("VICTORY_READY_ONCE", Kind::Flag),
    ("VICTORY_REFRESH_NARROW", Kind::Count),
    ("VICTORY_REFRESH_NOTES", Kind::Flag),
    ("VICTORY_RESIDENT_BATCH", Kind::OnByDefault),
    ("VICTORY_RT_RESOLVE", Kind::OnByDefault),
    ("VICTORY_SAME_RECORDS", Kind::Flag),
    ("VICTORY_SEND_COPYQ", Kind::Flag),
    ("VICTORY_SNAP_ASIDE", Kind::OnByDefault),
    ("VICTORY_STAGE_A", Kind::Flag),
    ("VICTORY_STAGE_A_ASK_ASIDE", Kind::Flag),
    ("VICTORY_STAGE_A_DEEP", Kind::Flag),
    ("VICTORY_STAGE_A_FRONT2", Kind::Flag),
    ("VICTORY_STAGE_A_LOOKAHEAD", Kind::Flag),
    ("VICTORY_STAGE_A_PLAN", Kind::Flag),
    ("VICTORY_STAGE_A_RELAX", Kind::Count),
    ("VICTORY_STAGE_A_STEPS", Kind::Flag),
    ("VICTORY_STAGING_HOLD_MS", Kind::Count),
    ("VICTORY_STATE_FILTER", Kind::Modes(&["1", "2"])),
    ("VICTORY_STEPS_MERGE_READS", Kind::Flag),
    ("VICTORY_STORE_BESIDE", Kind::Flag),
    ("VICTORY_SUBMIT_FRAMES", Kind::Count),
    ("VICTORY_TAKE_PARTS", Kind::Flag),
    ("VICTORY_UNDER_TRY", Kind::Flag),
    ("VICTORY_UNDER_WIDE", Kind::Flag),
    ("VICTORY_UNWATCHED_FRAMES", Kind::Count),
    ("VICTORY_UPLOAD_AHEAD", Kind::Flag),
    ("VICTORY_UPLOAD_HELD_ONLY", Kind::Flag),
    ("VICTORY_VIEW_AT_CACHE", Kind::Flag),
    ("VICTORY_VIEW_CACHE", Kind::Modes(&["1", "2"])),
    ("VICTORY_WAIT_NICE", Kind::Flag),
    ("VICTORY_WAIT_ORDER", Kind::Flag),
    ("VICTORY_WIDE_SPIN", Kind::Count),
    ("VICTORY_WIDE_WHOLE", Kind::Flag),
];

/// How `name` reads its value, if it is a switch that may be named.
pub(crate) fn kind_of(name: &str) -> Option<Kind> {
    LIVE.binary_search_by(|(n, _)| (*n).cmp(name))
        .ok()
        .map(|at| LIVE[at].1)
}

/// What a value (`None`: none was given) of `name`, which reads it as `kind`, comes to.
pub(crate) fn check_value(name: &str, kind: Kind, value: Option<&str>) -> Check {
    if value == Some("") {
        return Check::Bad(format!(
            "{name}= has an empty value (name it without the `=`, or give it one)"
        ));
    }
    match (kind, value) {
        (Kind::Flag, None | Some("1"))
        | (Kind::OnByDefault, Some("0"))
        | (Kind::Count | Kind::Float, None) => Check::Fine,
        (Kind::Flag, Some(v)) => Check::Odd(format!(
            "{name}={v}: a plain on/off switch ignores its value (arm A is off, arm B is on, whatever the value)"
        )),
        (Kind::OnByDefault, None) => Check::Bad(format!(
            "{name} is on when its variable is unset, so named without a value both arms are on (name it {name}=0 for an arm that is off)"
        )),
        (Kind::OnByDefault, Some(v)) => Check::Bad(format!(
            "{name}={v}: only 0 turns this switch off, so both arms would be on (name it {name}=0)"
        )),
        (Kind::Count, Some(v)) => match v.parse::<u64>() {
            Ok(_) => Check::Fine,
            Err(_) => Check::Bad(format!(
                "{name}={v}: not a whole number, which the switch would read as its default (the same as arm A)"
            )),
        },
        (Kind::Float, Some(v)) => match v.parse::<f64>() {
            Ok(n) if n.is_finite() && n >= 0.0 => Check::Fine,
            _ => Check::Bad(format!(
                "{name}={v}: not a number of 0 or more, which the switch would read as its default"
            )),
        },
        (Kind::Modes(modes), None) => {
            if modes.contains(&"1") {
                Check::Fine
            } else {
                Check::Bad(format!("{name} needs a value, one of {}", modes.join(", ")))
            }
        }
        (Kind::Modes(modes), Some(v)) => {
            if modes.iter().any(|m| *m == v) {
                Check::Fine
            } else {
                Check::Bad(format!(
                    "{name}={v}: not one of {} (any other text reads as the switch off, the same as arm A)",
                    modes.join(", ")
                ))
            }
        }
    }
}

/// The switch whose name `name` is nearest to (a mistyped one, or one without its `VICTORY_`), if one is near.
pub(crate) fn nearest(name: &str) -> Option<&'static str> {
    let bare = |s: &str| s.strip_prefix("VICTORY_").unwrap_or(s).to_ascii_uppercase();
    let wanted = bare(name);
    let (apart, best) = LIVE
        .iter()
        .map(|(n, _)| (distance(&bare(n), &wanted), *n))
        .min()?;
    (apart <= 3usize.max(wanted.len() / 5)).then_some(best)
}

/// The edit distance of two texts.
fn distance(a: &str, b: &str) -> usize {
    let b: Vec<char> = b.chars().collect();
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.chars().enumerate() {
        let mut diagonal = row[0];
        row[0] = i + 1;
        for (j, cb) in b.iter().enumerate() {
            let above = row[j + 1];
            row[j + 1] = (diagonal + usize::from(ca != *cb))
                .min(above + 1)
                .min(row[j] + 1);
            diagonal = above;
        }
    }
    row[b.len()]
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;
    use std::path::Path;

    /// The names that the sources read through the A/B: the first argument of each of the ways of reading one, found
    /// by text (so that a switch made live and left out of [`LIVE`], or the other way round, fails here and not in a
    /// pilot's run).
    fn names_in_sources() -> BTreeSet<String> {
        const WAYS: [&str; 8] = [
            "live_flag!(",
            "live_value!(",
            "toggled!(",
            "asked_for!(",
            "ab::bit(",
            "ab::pick(",
            "ab::value_b(",
            "switched(",
        ];
        fn walk(dir: &Path, found: &mut BTreeSet<String>) {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                if path.is_dir() {
                    walk(&path, found);
                    continue;
                }
                let file = path.to_string_lossy().replace('\\', "/");
                // (`ab.rs` and this file name switches that do not exist, in their tests.)
                if !file.ends_with(".rs")
                    || file.ends_with("victory-gpu/src/ab.rs")
                    || file.ends_with("ab/registry.rs")
                {
                    continue;
                }
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                for line in text.lines().filter(|l| !l.trim_start().starts_with("//")) {
                    for way in WAYS {
                        for (at, _) in line.match_indices(way) {
                            if let Some(quoted) =
                                line[at + way.len()..].trim_start().strip_prefix('"')
                                && let Some((name, _)) = quoted.split_once('"')
                                && name.starts_with("VICTORY_")
                            {
                                found.insert(name.to_string());
                            }
                        }
                    }
                }
            }
        }
        let mut found = BTreeSet::new();
        let crates = Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        for entry in std::fs::read_dir(&crates)
            .expect("the workspace's crates")
            .flatten()
        {
            walk(&entry.path().join("src"), &mut found);
        }
        found
    }

    #[test]
    fn the_list_is_what_the_sources_read_through_the_ab() {
        let read = names_in_sources();
        let listed: BTreeSet<String> = LIVE.iter().map(|(n, _)| (*n).to_string()).collect();
        let unlisted: Vec<_> = read.difference(&listed).collect();
        let unread: Vec<_> = listed.difference(&read).collect();
        assert!(
            unlisted.is_empty(),
            "read through the A/B and not in LIVE (add them to ab/registry.rs): {unlisted:?}"
        );
        assert!(
            unread.is_empty(),
            "in LIVE and read by nothing through the A/B (remove them from ab/registry.rs): {unread:?}"
        );
        assert!(
            LIVE.windows(2).all(|w| w[0].0 < w[1].0),
            "LIVE is sorted by name, without repeats"
        );
    }

    #[test]
    fn a_switch_is_found_by_its_name_and_a_mistyped_one_by_the_nearest() {
        assert_eq!(
            kind_of("VICTORY_PAGE_REFRESH"),
            Some(Kind::Modes(&["1", "2"]))
        );
        assert_eq!(kind_of("VICTORY_ONE_PASS"), Some(Kind::Flag));
        assert_eq!(kind_of("VICTORY_NONE_SUCH"), None);
        assert_eq!(
            nearest("VICTORY_PAGE_REFRESHH"),
            Some("VICTORY_PAGE_REFRESH")
        );
        assert_eq!(nearest("page_refresh"), Some("VICTORY_PAGE_REFRESH"));
        assert_eq!(nearest("VICTORY_ZZZZZZZZZZZZZZ"), None);
    }

    #[test]
    fn values_are_checked_by_how_the_switch_reads_them() {
        let (flag, on, count, float, modes) = (
            Kind::Flag,
            Kind::OnByDefault,
            Kind::Count,
            Kind::Float,
            Kind::Modes(&["1", "2"]),
        );
        let fine = |k, v| check_value("VICTORY_X", k, v) == Check::Fine;
        let odd = |k, v| matches!(check_value("VICTORY_X", k, v), Check::Odd(_));
        let bad = |k, v| matches!(check_value("VICTORY_X", k, v), Check::Bad(_));
        assert!(
            fine(flag, None)
                && fine(flag, Some("1"))
                && odd(flag, Some("0"))
                && odd(flag, Some("7"))
        );
        assert!(fine(on, Some("0")) && bad(on, None) && bad(on, Some("1")));
        assert!(
            fine(count, None)
                && fine(count, Some("64"))
                && bad(count, Some("x"))
                && bad(count, Some("1.5"))
                && bad(count, Some("-1"))
        );
        assert!(
            fine(float, Some("2.5"))
                && fine(float, Some("3"))
                && bad(float, Some("nan"))
                && bad(float, Some("-1"))
        );
        assert!(
            fine(modes, None)
                && fine(modes, Some("2"))
                && bad(modes, Some("3"))
                && bad(modes, Some("0"))
                && bad(modes, Some("on"))
        );
        for kind in [flag, on, count, float, modes] {
            assert!(bad(kind, Some("")), "an empty value is bad for every kind");
        }
    }
}
