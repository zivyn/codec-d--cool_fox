//! The guard of the shader cache's key: every `VICTORY_*` switch the translator's code reads is
//! in `translation_fingerprint` (the text of the variable, or its value in force), or is named in
//! `NOT_THE_TRANSLATION` and read only where it changes nothing written. A switch that changes
//! the translation and is in neither is served from a cache made with the other value of it: an
//! A/B with it shows the first arm twice.
//!
//! The scan reads this crate's source files whose names do not hold `test` (the rule `build.rs`
//! lists the switches by), so a test's own switches are not the translator's.

use super::{KEYED_BY_VALUE, NOT_THE_TRANSLATION, switch_texts, translation_fingerprint};
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

fn collect(dir: &Path, out: &mut Vec<PathBuf>) {
    for e in std::fs::read_dir(dir).unwrap().flatten() {
        let path = e.path();
        if path.is_dir() {
            collect(&path, out);
            continue;
        }
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        if name.ends_with(".rs") && !name.contains("test") {
            out.push(path);
        }
    }
}

/// The translator's source files: the path from `src` with `/`, and the text (line endings as
/// the checkout has them: only `\n` is looked for).
fn sources() -> Vec<(String, String)> {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    let mut files = Vec::new();
    collect(&src, &mut files);
    assert!(!files.is_empty());
    files.sort();
    files
        .into_iter()
        .map(|p| {
            let rel = p
                .strip_prefix(&src)
                .unwrap()
                .to_string_lossy()
                .replace('\\', "/");
            (
                rel,
                std::fs::read_to_string(&p).unwrap().replace("\r\n", "\n"),
            )
        })
        .collect()
}

/// Every `"VICTORY_NAME"` of `text`: the name and where it is.
fn names(text: &str) -> Vec<(&str, usize)> {
    let mut out = Vec::new();
    let mut from = 0;
    while let Some(at) = text[from..].find("\"VICTORY_") {
        let start = from + at + 1;
        let tail = &text[start..];
        let end = tail
            .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
            .unwrap_or(tail.len());
        if tail[end..].starts_with('"') {
            out.push((&tail[..end], start));
        }
        from = start + end;
    }
    out
}

fn all_names() -> BTreeSet<String> {
    sources()
        .iter()
        .flat_map(|(_, text)| names(text).into_iter().map(|(n, _)| n.to_owned()))
        .collect()
}

#[test]
fn the_list_the_build_makes_is_the_one_the_guard_reads() {
    let built: BTreeSet<String> = env!("VICTORY_SHADER_SWITCHES")
        .split(',')
        .filter(|n| !n.is_empty())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        built,
        all_names(),
        "build.rs and this scan must read the same files by the same rule"
    );
}

#[test]
fn the_two_lists_name_switches_the_code_reads_and_do_not_overlap() {
    let read = all_names();
    for name in NOT_THE_TRANSLATION.iter().chain(KEYED_BY_VALUE) {
        assert!(
            read.contains(*name),
            "{name} is in a list of the key but nothing reads it any more"
        );
    }
    for name in NOT_THE_TRANSLATION {
        assert!(!KEYED_BY_VALUE.contains(name), "{name} is in both lists");
    }
}

/// A switch named by the code and in neither list changes the key by the text of its variable.
/// This is what a switch added to the translator is held to: nothing to add anywhere, and a
/// failure here means the key does not see it.
#[test]
fn every_other_switch_the_translator_names_changes_the_key() {
    let none = switch_texts(|_| None);
    let mut still = Vec::new();
    for name in all_names() {
        if NOT_THE_TRANSLATION.contains(&name.as_str()) || KEYED_BY_VALUE.contains(&name.as_str()) {
            continue;
        }
        let set = switch_texts(|n| (n == name).then(|| "x".into()));
        if set == none {
            still.push(name);
        }
    }
    assert!(
        still.is_empty(),
        "switches the key does not hold: {still:?}"
    );
    // And the listed ones are left out of it, or the lists would be no use.
    for name in NOT_THE_TRANSLATION {
        assert_eq!(
            switch_texts(|n| (n == *name).then(|| "x".into())),
            none,
            "{name}"
        );
    }
}

/// The ones the key holds by their value in force are in it with that value.
#[test]
fn the_value_in_force_of_the_others_is_in_the_key() {
    let key = translation_fingerprint();
    let has = |name: &str| {
        let label = format!("{name}=");
        key.windows(label.len()).any(|w| w == label.as_bytes())
    };
    for name in KEYED_BY_VALUE {
        if *name == "VICTORY_RDNA_FLOAT" {
            continue;
        }
        assert!(has(*name), "{name} is not in the fingerprint");
    }
    // `VICTORY_RDNA_FLOAT` is what `host_rdna` says when the host is RDNA, and `host_settings`
    // (hashed by the cache beside the fingerprint) holds that.
    let all = sources();
    let lower = &all
        .iter()
        .find(|(rel, _)| rel == "emit/lower.rs")
        .unwrap()
        .1;
    let body = |start: &str| {
        let at = lower
            .find(start)
            .unwrap_or_else(|| panic!("{start} is gone"));
        let rest = &lower[at..];
        rest[..rest.find("\n}\n").unwrap()].to_owned()
    };
    assert!(body("pub fn host_settings()").contains("host_rdna()"));
    assert!(body("pub(super) fn host_rdna()").contains("rdna_float()"));
}

/// What the host tells the translator (the device's waves, the view heap, the group's memory)
/// is in `host_settings`, the other half of the key: each setter of one has its getter there.
#[test]
fn every_setting_the_host_makes_is_in_host_settings() {
    // (setter, what `host_settings` reads of it)
    const SETTINGS: &[(&str, &str)] = &[
        ("set_host_lds_bytes", "host_lds_bytes()"),
        ("set_host_max_wave", "host_max_wave()"),
        ("set_host_mesh_output_bytes", "host_mesh_output_bytes()"),
        ("set_host_rdna", "host_rdna()"),
        ("set_raw_views", "raw_views()"),
        ("set_view_heap_limit", "view_heap_limit()"),
    ];
    // (public setters that are no setting: a phi of the code being built, and the tag a program's
    // notes carry, which its container decides)
    const NOT_SETTINGS: &[&str] = &["set_phi", "set_program_tag"];
    let sources = sources();
    let mut found = BTreeSet::new();
    for (_, text) in &sources {
        let mut rest = text.as_str();
        while let Some(at) = rest.find("pub fn set_") {
            let tail = &rest[at + "pub fn ".len()..];
            let end = tail.find('(').unwrap();
            found.insert(tail[..end].to_owned());
            rest = &tail[end..];
        }
    }
    let known: BTreeSet<String> = SETTINGS
        .iter()
        .map(|s| s.0)
        .chain(NOT_SETTINGS.iter().copied())
        .map(str::to_owned)
        .collect();
    assert_eq!(
        found, known,
        "a public setter the host can call: if it changes what is written, it goes in `host_settings`"
    );
    let lower = &sources
        .iter()
        .find(|(rel, _)| rel == "emit/lower.rs")
        .unwrap()
        .1;
    let at = lower.find("pub fn host_settings()").unwrap();
    let rest = &lower[at..];
    let body = &rest[..rest.find("\n}\n").unwrap()];
    for (setter, getter) in SETTINGS {
        assert!(
            body.contains(getter),
            "{setter} sets what `host_settings` does not read ({getter})"
        );
    }
}

/// An environment read names its switch with a literal, which is how the list is made.
#[test]
fn every_environment_read_names_its_switch_by_a_literal() {
    // (file, the argument): where the name is not at the call. Each is a loop over names that
    // are written as literals in the same file, or the helper that the callers pass a literal to.
    const BY_NAME: &[(&str, &str)] = &[
        ("lib.rs", "name"),
        ("emit/mod.rs", "name"),
        ("emit/mod.rs", "var"),
        ("emit/hang.rs", "n"),
        ("emit/view_index.rs", "name"),
    ];
    let mut bad = Vec::new();
    for (rel, text) in sources() {
        assert!(
            !text.contains("env::vars"),
            "{rel} reads the whole environment"
        );
        for call in ["env::var(", "env::var_os(", "env_on("] {
            let mut from = 0;
            while let Some(at) = text[from..].find(call) {
                let start = from + at + call.len();
                from = start;
                if text[..from - call.len()].ends_with("fn ") {
                    continue;
                }
                let arg: String = text[start..]
                    .trim_start()
                    .chars()
                    .take_while(|c| !matches!(c, ')' | ',' | '.'))
                    .collect();
                let arg = arg.trim();
                let literal = arg.starts_with("\"VICTORY_") && arg.ends_with('"');
                if !literal && !BY_NAME.contains(&(rel.as_str(), arg)) {
                    bad.push(format!("{rel}: {call}{arg}"));
                }
            }
        }
    }
    assert!(
        bad.is_empty(),
        "reads whose switch the list cannot see: {bad:?}"
    );
}

/// Where each switch that is no part of the translation may be read. The ones that only print
/// are read as a flag (`env_on`) next to the line they print; the others are tools' paths and the
/// host's test device, in the files named, or only in the tests of `emit/mod.rs`.
#[test]
fn the_switches_left_out_of_the_key_are_read_only_where_they_change_nothing() {
    enum Shape {
        Prints,
        Files(&'static [&'static str]),
        TestsOfMod,
    }
    use Shape::*;
    const SHAPES: &[(&str, Shape)] = &[
        ("VICTORY_D3D12_DEBUG", Files(&["emit/host.rs"])),
        ("VICTORY_DESC_WHY", Prints),
        ("VICTORY_DXIL", Files(&["emit/validator.rs"])),
        ("VICTORY_HANG_AUDIT", Files(&["emit/hang.rs"])),
        ("VICTORY_MASK_WHY", Prints),
        ("VICTORY_PC_DXC", TestsOfMod),
        ("VICTORY_REF_PS", TestsOfMod),
        ("VICTORY_REF_VS", TestsOfMod),
        ("VICTORY_SHADER_CENSUS", Files(&["emit/bitcode/census.rs"])),
        ("VICTORY_SHADER_SWITCHES", Files(&["emit/mod.rs"])),
        ("VICTORY_WARP", Files(&["emit/host.rs"])),
        ("VICTORY_WAVE_WHY", Prints),
    ];
    let listed: BTreeSet<&str> = SHAPES.iter().map(|s| s.0).collect();
    assert_eq!(
        listed,
        NOT_THE_TRANSLATION.iter().copied().collect::<BTreeSet<_>>()
    );
    for (rel, text) in sources() {
        // (the lists of names themselves)
        let mut text = text;
        if rel == "emit/mod.rs" {
            for start in ["pub const NOT_THE_TRANSLATION", "pub const KEYED_BY_VALUE"] {
                let at = text.find(start).unwrap();
                let end = at + text[at..].find("\n];").unwrap();
                text.replace_range(at..end, "");
            }
        }
        let tests_from = text
            .find("\n#[cfg(test)]\nmod tests {")
            .unwrap_or(text.len());
        for (name, at) in names(&text) {
            let Some((_, shape)) = SHAPES.iter().find(|s| s.0 == name) else {
                continue;
            };
            match shape {
                Prints => {
                    let line_start = text[..at].rfind('\n').map_or(0, |i| i + 1);
                    // (this line and the forty after it: the longest block that only prints)
                    let window: Vec<&str> = text[line_start..].lines().take(41).collect();
                    let call = format!("env_on(\"{name}\")");
                    assert!(
                        window[0].contains(&call),
                        "{rel}: {name} is not read as a flag: {}",
                        window[0]
                    );
                    let prints = ["eprintln!", "let why =", "widen(", "own_bits::find("];
                    assert!(
                        window.iter().any(|l| prints.iter().any(|p| l.contains(p))),
                        "{rel}: {name} is read where nothing prints: {}",
                        window[0]
                    );
                }
                Files(files) => assert!(files.contains(&rel.as_str()), "{rel} reads {name}"),
                TestsOfMod => assert!(
                    rel == "emit/mod.rs" && at > tests_from,
                    "{rel} reads {name} outside the tests"
                ),
            }
        }
    }
}
