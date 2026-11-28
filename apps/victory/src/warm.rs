//! `victory warm`: host pipelines made ahead of a run (`known_pipelines`), in
//! processes of their own: a compiler that crashes or hangs costs one pipeline.

use std::collections::HashSet;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use victory_hle::known_pipelines as known;

/// Pipelines a process makes before another takes its place: the compiler's
/// and the translations' memory goes with it.
const AT_ONCE: usize = 40;
/// What a process is given at most, by what its pipelines are thought to cost (ms): the biggest
/// programs take seconds, so a process of forty of them would still be at work when the others
/// had finished, and would hold the compiler's memory of all of them.
const AT_MOST_MS: u64 = 60_000;
/// A pipeline that takes longer is taken for one the compiler hangs on.
const LONGEST: Duration = Duration::from_secs(300);

/// Windows' idle priority class: a child that lets the machine's other work
/// go first (the normal class, 0x20, is for a launch that waits).
const IDLE_PRIORITY_CLASS: u32 = 0x40;

/// As the runs count one that takes long.
const SLOW_MS: f64 = 1000.0;

fn folder() -> Result<PathBuf> {
    let cfg = victory_core::Config::load()?;
    Ok(cfg.cache_subdir("shaders")?.with_file_name("pipelines"))
}

/// What `victory warm` is asked.
pub struct Ask {
    pub jobs: Option<usize>,
    pub limit: Option<usize>,
    pub again: bool,
    pub used: bool,
    pub compute: bool,
    pub now: bool,
    pub skip_resolves: bool,
    pub status: bool,
    pub verify: bool,
    pub paths: Vec<PathBuf>,
    pub these: Option<String>,
    pub shard: Option<PathBuf>,
}

/// The archives that hold the title's compute shaders, when none are named.
fn archives(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let game = cfg.game_dir.context("no game folder configured and no archive named")?;
    let mut found: Vec<PathBuf> = Some(game.join("shaders.rpf")).into_iter().filter(|p| p.is_file()).collect();
    // The title is the largest executable of the folder.
    let title = std::fs::read_dir(&game)
        .into_iter()
        .flatten()
        .flatten()
        .filter(|e| e.path().extension().is_some_and(|x| x.eq_ignore_ascii_case("exe")))
        .filter_map(|e| Some((e.metadata().ok()?.len(), e.path())))
        .max();
    found.extend(title.map(|(_, path)| path));
    anyhow::ensure!(!found.is_empty(), "no shaders.rpf or title executable in {}", game.display());
    Ok(found)
}

/// Every compute program of `paths` as the title creates its pipeline from
/// (its container), once each.
fn corpus_compute(paths: &[PathBuf], threads: usize) -> Vec<Vec<u8>> {
    use victory_shader::{Container, FourCC, HwStage, native_programs};
    let seen = Mutex::new(HashSet::new());
    let found = Mutex::new(Vec::new());
    for path in paths {
        let Ok(data) = std::fs::read(path) else { continue };
        let jobs = crate::shader::sources(&data);
        let next = AtomicUsize::new(0);
        std::thread::scope(|s| {
            for _ in 0..threads.min(jobs.len()) {
                s.spawn(|| {
                    while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let Some(bytes) = job.bytes() else { continue };
                        let compute = Container::parse(&bytes).is_ok_and(|c| {
                            c.part(FourCC::DXIL).or(c.part(FourCC::ACCC)).is_some_and(|p| {
                                native_programs(p.data).is_ok_and(|ps| ps.iter().any(|p| p.stage == HwStage::Cs))
                            })
                        });
                        if !compute {
                            continue;
                        }
                        // A container found in the middle of a file runs to the file's end here.
                        let bytes: Vec<u8> = match job {
                            crate::shader::Source::Raw { .. } => {
                                let total = bytes
                                    .get(0x18..0x1c)
                                    .map_or(0, |b| u32::from_le_bytes([b[0], b[1], b[2], b[3]]) as usize);
                                bytes[..total.min(bytes.len())].to_vec()
                            }
                            crate::shader::Source::Packed(_) => bytes.into_owned(),
                        };
                        if lock(&seen).insert(blake3::hash(&bytes)) {
                            lock(&found).push(bytes);
                        }
                    }
                });
            }
        });
    }
    found.into_inner().unwrap_or_else(|e| e.into_inner())
}

/// What tells the archives of the last corpus pass from these ones.
fn corpus_stamp(paths: &[PathBuf]) -> String {
    let mut stamp = String::new();
    for p in paths {
        let m = std::fs::metadata(p).ok();
        let when = m.as_ref().and_then(|m| m.modified().ok()).and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok());
        stamp += &format!("{} {} {}\n", p.display(), m.map_or(0, |m| m.len()), when.map_or(0, |d| d.as_secs()));
    }
    // (The programs a run leaves out are not written down: with other switches, or after the
    // default changed, a book that was seeded lacks some.)
    stamp += &known::left_out_signature();
    stamp.push('\n');
    stamp
}

pub fn run(ask: Ask) -> Result<()> {
    let Ask { jobs, limit, again, used, compute, now, skip_resolves, status, verify, paths, these, shard } = ask;
    let dir = folder()?;
    if let Some(these) = these {
        return make(&dir, &these, shard);
    }
    let began = Instant::now();
    victory_hle::known_pipelines::use_translations(dir.with_file_name("shaders"));
    let fingerprint = known::fingerprint();
    let cores = std::thread::available_parallelism().map_or(4, |n| n.get());
    if !status {
        // What an earlier run made and was stopped before it took into the library.
        absorb_all(&dir, cores);
    }
    if compute {
        // Once per set of archives: what a pass found stays in the book.
        let paths = archives(paths)?;
        let stamp = corpus_stamp(&paths);
        let marker = dir.join("warm-corpus.txt");
        if std::fs::read_to_string(&marker).ok().as_deref() == Some(stamp.as_str()) {
            println!("compute programs of the archives are in the book already");
        } else {
            std::fs::create_dir_all(&dir)?;
            println!("reading compute programs from {} archive(s)", paths.len());
            let found = corpus_compute(&paths, cores);
            let (n, took) = (found.len(), began.elapsed().as_secs_f64());
            let added = known::seed_compute(&dir, found);
            println!("{n} compute programs found in {took:.0} s, {added} added to the book");
            std::fs::write(&marker, stamp)?;
        }
    }
    let done_file = dir.join("warm-done.txt");
    let bad_file = dir.join("warm-bad.txt");
    let book = known::keys(&dir);
    let keys: Vec<&String> = book.iter().map(|(k, _)| k).collect();
    let listed = std::fs::read_to_string(&done_file).unwrap_or_default();
    let mut done: HashSet<&str> = HashSet::new();
    for line in listed.lines() {
        if let Some((of, key)) = line.split_once(' ')
            && of == fingerprint
        {
            done.insert(key);
        }
    }
    let bad = std::fs::read_to_string(&bad_file).unwrap_or_default();
    let bad: HashSet<&str> = bad.lines().filter_map(|l| l.split(' ').next()).collect();
    if verify {
        let made: Vec<usize> = (0..keys.len()).filter(|&n| done.contains(keys[n].as_str())).collect();
        let began = Instant::now();
        known::absorb(&dir, &[], &made, cores);
        let (compiled, loaded) = known::library_counts();
        println!(
            "{} pipelines made before asked of the pipeline library: {loaded} loaded, {compiled} compiled by this process ({:.1} s)",
            made.len(),
            began.elapsed().as_secs_f64()
        );
        return Ok(());
    }
    let early = std::fs::read_to_string(dir.join("early-pipelines.txt")).unwrap_or_default();
    let early: HashSet<&str> = early.lines().collect();
    // Those earlier runs drew with first, the others in no order (by key).
    let mut all: Vec<usize> = (0..keys.len())
        .filter(|&n| {
            let key = keys[n].as_str();
            // `--used`: those drawn with, and with `--compute` every compute one.
            let wanted = !used || early.contains(key) || (compute && book[n].1);
            (again || !done.contains(key)) && !bad.contains(key) && wanted
        })
        .collect();
    let weights = known::weights(&dir);
    let weight = |n: usize| weights.get(n);
    let resolves = if skip_resolves {
        let before = all.len();
        all.retain(|&n| !weight(n).is_some_and(|w| w.resolve));
        before - all.len()
    } else {
        0
    };
    // What each took in earlier runs, else a guess from its size: the longest go first, so that
    // what is left for the end is short and the processes finish together.
    let took = slow_times(&dir);
    let cost = |n: usize| cost_ms(took.get(keys[n].as_str()).copied(), weight(n).map_or(0, |w| w.bytes));
    // Drawn with first, then compute (a run waits for those whole), then the rest; the longest first.
    all.sort_by_key(|&n| (!early.contains(keys[n].as_str()), !book[n].1, std::cmp::Reverse(cost(n)), keys[n]));
    all.truncate(limit.unwrap_or(usize::MAX));
    // A launch that waits for this takes the whole machine.
    let jobs = jobs.unwrap_or(if now { cores } else { cores * 2 / 3 }).max(1);
    println!(
        "{} pipelines known ({} compute), {} made before ({fingerprint}), {} bad; {} to make, {jobs} processes",
        keys.len(),
        book.iter().filter(|(_, c)| *c).count(),
        done.len(),
        bad.len(),
        all.len()
    );
    if skip_resolves {
        println!("{resolves} ray-tracing resolves left to the title (VICTORY_RESOLVE_ASYNC makes them in the background)");
    }
    if status || all.is_empty() {
        return Ok(());
    }
    let priority = if now { 0x20 } else { IDLE_PRIORITY_CLASS };
    let costs: Vec<u64> = all.iter().map(|&n| cost(n)).collect();
    let chunks: Vec<&[usize]> = chunks_by_cost(&all, &costs, AT_ONCE, AT_MOST_MS);
    let next = AtomicUsize::new(0);
    let state = Mutex::new((Vec::<f64>::new(), 0usize, 0usize));
    let done_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&done_file)?);
    // (Read back by the runs: these are made beside the others, not before.)
    let slow_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(dir.join("slow-pipelines.txt"))?);
    let bad_out = Mutex::new(std::fs::OpenOptions::new().create(true).append(true).open(&bad_file)?);
    std::thread::scope(|s| {
        for _ in 0..jobs.min(chunks.len()) {
            s.spawn(|| {
                while let Some(&chunk) = chunks.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let mut rest = chunk;
                    while !rest.is_empty() {
                        let (made, ended) = child(rest, priority, &dir, |n, ms, there| {
                            if ms >= SLOW_MS {
                                let _ = writeln!(lock(&slow_out), "\n{} {ms:.0}", keys[n]);
                            }
                            let mut state = lock(&state);
                            state.0.push(ms);
                            state.1 += usize::from(!there);
                            let count = state.0.len();
                            if count % 100 == 0 || count == all.len() {
                                let took = began.elapsed().as_secs_f64();
                                let left = took / count as f64 * (all.len() - count) as f64;
                                println!("warm {count} of {} ({:.0}%), {took:.0} s, about {left:.0} s left", all.len(), 100.0 * count as f64 / all.len() as f64);
                            }
                            drop(state);
                            let _ = writeln!(lock(&done_out), "{fingerprint} {}", keys[n]);
                        });
                        rest = &rest[made.min(rest.len())..];
                        match (ended, rest.split_first()) {
                            (Some(why), Some((&n, after))) => {
                                // The one it was at.
                                println!("pipeline {n} {}: {why}", keys[n]);
                                let _ = writeln!(lock(&bad_out), "{} {why}", keys[n]);
                                lock(&state).2 += 1;
                                rest = after;
                            }
                            _ => break,
                        }
                    }
                }
            });
        }
    });
    absorb_all(&dir, cores);
    let (mut times, refused, crashed) = state.into_inner().unwrap_or_else(|e| e.into_inner());
    times.sort_by(f64::total_cmp);
    let at = |q: f64| times.get((times.len() as f64 * q) as usize).copied().unwrap_or(0.0);
    println!(
        "{} made in {:.0} s ({refused} have no host pipeline, {crashed} ended their process); ms each: median {:.1}, p90 {:.1}, p99 {:.1}, most {:.0}, sum {:.0} s",
        times.len(),
        began.elapsed().as_secs_f64(),
        at(0.5),
        at(0.9),
        at(0.99),
        times.last().copied().unwrap_or(0.0),
        times.iter().sum::<f64>() / 1e3
    );
    Ok(())
}

/// What a pipeline is thought to cost (ms): what it took in an earlier run, else its shaders' size
/// over 20 (the programs of 60 KB the driver takes 3 to 4 s for, alone).
fn cost_ms(took: Option<u64>, bytes: u64) -> u64 {
    took.unwrap_or(bytes / 20).max(1)
}

/// How long the pipelines that took a second or more took, from the runs' `slow-pipelines.txt`
/// (the most of each).
fn slow_times(dir: &Path) -> std::collections::HashMap<String, u64> {
    let mut took = std::collections::HashMap::new();
    let Ok(file) = std::fs::File::open(dir.join("slow-pipelines.txt")) else { return took };
    for line in BufReader::new(file).lines().map_while(Result::ok) {
        if let Some((key, ms)) = line.split_once(' ')
            && let Ok(ms) = ms.trim().parse::<u64>()
        {
            let most = took.entry(key.to_owned()).or_insert(0);
            *most = (*most).max(ms);
        }
    }
    took
}

/// `all` cut into runs of at most `count` pipelines and about `most_ms` of `costs` (one for each
/// of `all`); one is never empty, however much its first costs.
fn chunks_by_cost<'a>(all: &'a [usize], costs: &[u64], count: usize, most_ms: u64) -> Vec<&'a [usize]> {
    let mut chunks = Vec::new();
    let (mut from, mut sum) = (0, 0u64);
    for (at, cost) in costs.iter().enumerate().take(all.len()) {
        if at > from && (at - from >= count || sum + cost > most_ms) {
            chunks.push(&all[from..at]);
            (from, sum) = (at, 0);
        }
        sum += cost;
    }
    if from < all.len() {
        chunks.push(&all[from..]);
    }
    chunks
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

/// Has a process make `these`; how many it made, and how it ended if not well.
fn child(these: &[usize], priority: u32, dir: &Path, mut each: impl FnMut(usize, f64, bool)) -> (usize, Option<String>) {
    let list: Vec<String> = these.iter().map(usize::to_string).collect();
    static SHARDS: AtomicUsize = AtomicUsize::new(0);
    let shard = dir.join(SHARD_DIR).join(format!("{}-{}.lib", std::process::id(), SHARDS.fetch_add(1, Ordering::Relaxed)));
    let spawned = std::env::current_exe().and_then(|exe| {
        use std::os::windows::process::CommandExt;
        Command::new(exe)
            .arg("warm")
            .args(["--these", &list.join(","), "--shard"])
            .arg(&shard)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .creation_flags(priority)
            .spawn()
    });
    let Ok(mut process) = spawned else { return (these.len(), None) };
    let Some(out) = process.stdout.take() else { return (these.len(), None) };
    let process = Mutex::new(process);
    let (heard, over) = (Mutex::new(Instant::now()), AtomicBool::new(false));
    let hung = AtomicBool::new(false);
    let mut made = 0;
    std::thread::scope(|s| {
        s.spawn(|| {
            while !over.load(Ordering::Relaxed) {
                std::thread::sleep(Duration::from_millis(500));
                if lock(&heard).elapsed() > LONGEST {
                    hung.store(true, Ordering::Relaxed);
                    let _ = lock(&process).kill();
                    return;
                }
            }
        });
        for line in BufReader::new(out).lines().map_while(Result::ok) {
            *lock(&heard) = Instant::now();
            let mut words = line.split(' ');
            if words.next() == Some("made")
                && let (Some(n), Some(ms), Some(there)) = (words.next(), words.next(), words.next())
                && let (Ok(n), Ok(ms)) = (n.parse(), ms.parse())
            {
                made += 1;
                each(n, ms, there == "1");
            }
        }
        over.store(true, Ordering::Relaxed);
    });
    let status = lock(&process).wait();
    let well = status.as_ref().is_ok_and(|s| s.success());
    if well {
        return (these.len(), None);
    }
    let why = if hung.load(Ordering::Relaxed) {
        format!("hung for {} s", LONGEST.as_secs())
    } else {
        status.map_or_else(|e| e.to_string(), |s| format!("{s}"))
    };
    (made, Some(why))
}

/// Where the processes of a warm keep what they made, until it is taken into
/// the pipeline library (`absorb_all`).
const SHARD_DIR: &str = "warm-shards";

/// Takes the shards of the processes (each with the list of its pipelines
/// beside it) into the pipeline library of `dir`, which is what the title
/// loads its host pipelines from: made once here, not once per run, and kept
/// whatever the driver's own cache keeps (it is capped, and a pipeline that
/// took a minute to compile was not found in it by the next process).
fn absorb_all(dir: &Path, threads: usize) {
    let folder = dir.join(SHARD_DIR);
    let mut shards = Vec::new();
    let mut which: Vec<usize> = Vec::new();
    for entry in std::fs::read_dir(&folder).into_iter().flatten().flatten() {
        let path = entry.path();
        if path.extension().is_some_and(|e| e == "lib")
            && let Ok(listed) = std::fs::read_to_string(path.with_extension("keys"))
        {
            which.extend(listed.split(',').filter_map(|n| n.trim().parse::<usize>().ok()));
            shards.push(path);
        }
    }
    if shards.is_empty() {
        let _ = std::fs::remove_dir_all(&folder);
        return;
    }
    which.sort_unstable();
    which.dedup();
    let began = Instant::now();
    println!("taking {} pipelines of {} processes into the pipeline library", which.len(), shards.len());
    let there = known::absorb(dir, &shards, &which, threads);
    println!("{there} of {} are in the library ({:.0} s)", which.len(), began.elapsed().as_secs_f64());
    if there == which.len() {
        let _ = std::fs::remove_dir_all(&folder);
    }
}

/// What such a process does.
fn make(dir: &Path, these: &str, shard: Option<PathBuf>) -> Result<()> {
    let these: Vec<usize> = these.split(',').map(|n| n.parse().context("--these")).collect::<Result<_>>()?;
    let cfg = victory_core::Config::load()?;
    victory_hle::known_pipelines::use_translations(cfg.cache_subdir("shaders")?);
    let mut out = std::io::stdout();
    let (mut made, mut saved) = (Vec::<String>::new(), Instant::now());
    // The library of what was made, beside a list of those: the shard.
    let keep = |made: &[String]| {
        if let Some(shard) = &shard
            && known::save_shard(shard)
        {
            let _ = std::fs::write(shard.with_extension("keys"), made.join(","));
        }
    };
    // (After the host is up, which the first library call needs: the cache's library is the first place to look.)
    let _ = known::use_cache_library(dir);
    known::make(dir, &these, |n, came| {
        let told = match came {
            None => writeln!(out, "at {n}"),
            Some((ms, there)) => {
                if there {
                    made.push(n.to_string());
                }
                writeln!(out, "made {n} {ms:.2} {}", u8::from(there))
            }
        };
        if told.and_then(|()| out.flush()).is_err() {
            // Whoever asked is gone.
            std::process::exit(1);
        }
        if came.is_some() && saved.elapsed() > Duration::from_secs(3) {
            keep(&made);
            saved = Instant::now();
        }
    });
    keep(&made);
    // Without what a run's end does (nothing to save, the host's threads).
    let _ = out.flush();
    std::process::exit(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_pipeline_is_thought_to_cost_what_it_took_else_by_its_size() {
        assert_eq!(cost_ms(Some(8_000), 1_000), 8_000);
        // A program of 60 KB: three seconds.
        assert_eq!(cost_ms(None, 60_000), 3_000);
        assert_eq!(cost_ms(None, 0), 1);
    }

    #[test]
    fn processes_are_given_runs_cut_by_count_and_by_cost() {
        let all: Vec<usize> = (0..10).collect();
        // Many cheap ones: by count.
        let cheap = vec![1; 10];
        let chunks = chunks_by_cost(&all, &cheap, 4, 60_000);
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), [4, 4, 2]);
        // The biggest first, cut by what they cost: four of 20 s do not fit in one of 60 s.
        let big = [20_000, 20_000, 20_000, 20_000, 1, 1, 1, 1, 1, 1];
        let chunks = chunks_by_cost(&all, &big, 40, 60_000);
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), [3, 7]);
        // One that costs more than the whole is still a run of its own.
        let huge = [100_000, 1, 1];
        let chunks = chunks_by_cost(&all[..3], &huge, 40, 60_000);
        assert_eq!(chunks.iter().map(|c| c.len()).collect::<Vec<_>>(), [1, 2]);
        // Every pipeline is in exactly one run, in order.
        let joined: Vec<usize> = chunks_by_cost(&all, &big, 40, 60_000).concat();
        assert_eq!(joined, all);
        assert!(chunks_by_cost(&[], &[], 40, 60_000).is_empty());
    }
}
