//! Lists every `VICTORY_*` switch the translator's code names (the files
//! whose names do not hold `test`: the ones its guard test reads too), whose
//! values join the shader cache key at run time (`emit::translation_fingerprint`;
//! the translator's own source is hashed by `victory-gpu`'s build script).
//! Reads only this crate's `src` folder; writes nothing but cargo directives.

use std::path::{Path, PathBuf};

fn files(dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for e in entries.flatten() {
        let p = e.path();
        if p.is_dir() {
            files(&p, out);
        } else if p.extension().is_some_and(|x| x == "rs") {
            out.push(p);
        }
    }
}

fn main() {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
    println!("cargo:rerun-if-changed=src");
    println!("cargo:rerun-if-changed=Cargo.toml");
    let mut list = Vec::new();
    files(&src, &mut list);
    list.sort();
    let mut switches = std::collections::BTreeSet::new();
    for p in &list {
        // (what the guard test does not read: the tests' names are no part of the translator)
        if p.file_name().is_some_and(|n| n.to_string_lossy().contains("test")) {
            continue;
        }
        let text = std::fs::read(p).unwrap_or_default();
        let s = String::from_utf8_lossy(&text);
        let mut rest = &s[..];
        while let Some(at) = rest.find("\"VICTORY_") {
            let tail = &rest[at + 1..];
            let end = tail
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(tail.len());
            if tail[end..].starts_with('"') {
                switches.insert(tail[..end].to_string());
            }
            rest = &tail[end..];
        }
    }
    let joined: Vec<_> = switches.into_iter().collect();
    println!("cargo:rustc-env=VICTORY_SHADER_SWITCHES={}", joined.join(","));
}
