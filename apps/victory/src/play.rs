//! `victory play`, and `victory` started with no command at all (a double
//! click): the title with the settings this fork plays with, so that the
//! program alone, put beside the title's `game_scarlett_master.exe`, is all
//! there is to start. What `scripts\play-foxhound.ps1` does, built in.
//!
//! Nothing is written beside the program or into the title's folder: the
//! settings the F1 menu saves (`victory-settings.txt`), the logs and NVIDIA's
//! files are kept under `%LOCALAPPDATA%\victory`, where the caches and the
//! saves already are.
//!
//! NVIDIA's DLSS and frame generation files are NVIDIA's and are not part of
//! this program. Where they are missing, the first start asks once whether to
//! fetch them from NVIDIA's own GitHub (`victory get-dlss` does it later);
//! without them the title runs with its own upscaling.
//!
//! AMD's FSR 3 frame generation files are AMD's, likewise: on a card without
//! DLSS Frame Generation (or with FSR 3 chosen in F1) the first start asks
//! once whether to fetch them from AMD's own GitHub (`victory get-fsr`).
//!
//! The one thing that goes beside the program: on a PC whose Direct3D 12 runs
//! no shader model 6.6 (Windows 10's own stops at 6.5), Microsoft's newer
//! runtime, `D3D12\D3D12Core.dll` from Microsoft's NuGet package, after asking
//! (`victory get-d3d12`). Windows loads it only from there (main.rs).
//!
//! MFG Unlocker, someone else's mod that unlocks DLSS Frame Generation on RTX
//! 20 and 30 cards, is an option (F1, MFG UNLOCKER; off by default, asked once
//! on such a card): on, it is fetched from its author's GitHub release into
//! `%LOCALAPPDATA%\victory\mfg-unlocker` and loaded by this program at the
//! start (`victory get-mfg` fetches it by hand). It writes its own logs and
//! settings beside the program.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The title's executable, looked for beside this program.
const GAME: &str = "game_scarlett_master.exe";

/// `scripts\get-nvidia-sdks.ps1`, run by Windows PowerShell.
const FETCH: &str = include_str!("../../../scripts/get-nvidia-sdks.ps1");

/// `scripts\get-amd-fsr.ps1`, run by Windows PowerShell.
const FETCH_FSR: &str = include_str!("../../../scripts/get-amd-fsr.ps1");

/// `scripts\get-d3d12.ps1`, run by Windows PowerShell.
const FETCH_D3D12: &str = include_str!("../../../scripts/get-d3d12.ps1");

/// The two AMD files FSR 3 frame generation loads.
const FSR_FILES: [&str; 2] = ["amd_fidelityfx_loader_dx12.dll", "amd_fidelityfx_framegeneration_dx12.dll"];

/// The settings the title is started with (`scripts\play-foxhound.ps1` has
/// the same, with what each is for).
const SETTINGS: &[(&str, &str)] = &[
    // Submission and copy machinery
    ("VICTORY_THIN", "1"),
    ("VICTORY_JOIN_PARTS", "8"),
    ("VICTORY_RELAY", "2"),
    ("VICTORY_RELAY_COPY", "1"),
    ("VICTORY_WIDE_BACK", "1"),
    ("VICTORY_CARD_REHASH_RUNS", "1"),
    ("VICTORY_JOIN_COPIES", "1"),
    ("VICTORY_CARD_STEPS", "2"),
    ("VICTORY_CARD_AWAKE", "1"),
    ("VICTORY_CARD_BUFFERS_MIN_KB", "64"),
    ("VICTORY_INDIRECT_WINDOW", "1"),
    ("VICTORY_INDIRECT_BATCH", "2"),
    ("VICTORY_UPLOAD_AHEAD", "1"),
    ("VICTORY_SHOW_AFTER", "1"),
    ("VICTORY_MAP_THREADS", "1"),
    // Shader writes into big write-combined copies stay on the card
    ("VICTORY_WC_NO_BACK", "1"),
    ("VICTORY_WC_NO_BACK_MIN_KB", "1024"),
    // The title's page-mapping copies kept without Windows views
    ("VICTORY_SOFT_VIEWS", "copies"),
    // Textures
    ("VICTORY_TEXTURE_POOL", "1"),
    ("VICTORY_RIPE_BLOCKS", "0"),
    ("VICTORY_TEXTURE_NO_WAIT", "1"),
    ("VICTORY_TEXTURE_CATCHUP", "0"),
    ("VICTORY_TEXTURE_STAGE_MS", "16"),
    ("VICTORY_LEVELS_ON_DEMAND", "0"),
    ("VICTORY_PRELUDE_UPLOADS", "1"),
    ("VICTORY_SHOW_NEW_LATER", "1"),
    ("VICTORY_POOL_SMALL", "0"),
    ("VICTORY_TABLED_REREAD", "1"),
    ("VICTORY_QUEUE_HOST_TEXTURES", "0"),
    // Descriptor heaps' records
    ("VICTORY_HEAP_SYNC_FIRST", "1"),
    ("VICTORY_SYNC_IN_PLACE", "1"),
    ("VICTORY_SPANS_QUICK", "1"),
    ("VICTORY_SYNC_FINE", "1"),
    ("VICTORY_HEAP_SYNC_AT_SUBMIT", "1"),
    // Light without the title's ray tracing
    ("VICTORY_AMBIENT", "0.8"),
    ("VICTORY_AMBIENT_AIR", "1.5"),
    ("VICTORY_AMBIENT_SPECULAR", "1"),
    // Pixel programs keep the console's early depth test
    ("VICTORY_EARLY_DEPTH", "1"),
    // Left out: the decal heap's defragmentation and the heat-haze pass
    ("VICTORY_LEAVE_OUT", "02384a60fda6f2d0,731a39171b568298"),
    // Pipelines the title makes are written down, for `victory warm`
    ("VICTORY_PIPELINE_BOOK", "1"),
];

/// Path settings a start keeps from its surroundings; every other
/// `VICTORY_...` variable is cleared before the settings are put in (a
/// restart from the menu inherits the last run's).
const KEPT: &[&str] = &[
    "VICTORY_CONFIG",
    "VICTORY_GAME_DIR",
    "VICTORY_CACHE_DIR",
    "VICTORY_DATA_DIR",
    "VICTORY_STREAMLINE_DIR",
    "VICTORY_FIDELITYFX_DIR",
    "VICTORY_LOG",
];

const FG_TYPES: [&str; 3] = ["auto", "dlss", "fsr"];

const RESOLUTIONS: [&str; 3] = ["1440", "1080to1440", "4k"];
const DLSS_MODES: [&str; 5] = ["off", "dlaa", "quality", "balanced", "performance"];

#[derive(clap::Args, Clone, Debug)]
pub(crate) struct Play {
    /// The title's executable. Without it: `game_scarlett_master.exe` beside
    /// this program, else in the current folder.
    exe: Option<PathBuf>,
    /// The Series S profile (1280x720 inside: faster, softer).
    #[arg(long)]
    series_s: bool,
    /// 1440: output and scene 2560x1440; 1080to1440: scene 1920x1080
    /// upscaled by the title; 4k: the stock 3840x2160 output.
    #[arg(long, default_value = "1440", value_parser = RESOLUTIONS)]
    res: String,
    /// DLSS in place of the title's upscaling (needs NVIDIA's files).
    #[arg(long, default_value = "quality", value_parser = DLSS_MODES)]
    dlss: String,
    /// DLSS frame generation on from the start: frames shown for each one
    /// rendered, 2 to 6. Without it, it is ready and the F1 menu switches it.
    #[arg(long, value_parser = clap::value_parser!(u32).range(2..=6))]
    framegen: Option<u32>,
    /// Frame generation not made ready at the start.
    #[arg(long)]
    no_framegen_ready: bool,
    /// Which frame generation: auto (DLSS frame generation where the card has
    /// it, RTX 40 and later, else AMD FSR 3), dlss, or fsr (2X at most). FSR 3
    /// needs AMD's FidelityFX DLLs (`VICTORY_FIDELITYFX_DIR`, else a
    /// `fidelityfx` folder beside this program) and, on Series X, DLSS.
    #[arg(long, default_value = "auto", value_parser = FG_TYPES)]
    fg_type: String,
    /// Frames a second at most (30: the title's own rate; 1000: no cap).
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=1000))]
    fps: u32,
    /// Add or override a switch, as often as needed: `--set VICTORY_X=1`.
    #[arg(long = "set", value_name = "NAME=VALUE")]
    more: Vec<String>,
    /// Make every shader before the title starts (`victory preload`, as the
    /// F1 menu's "PRELOAD SHADERS" does): a while, maybe long.
    #[arg(long)]
    pub(crate) preload: bool,
}

/// What a preload is, said before it begins.
pub(crate) fn explain_preload() {
    println!();
    println!("Preloading shaders: every shader of the title is translated, then the pipelines the game");
    println!("made in earlier runs are built, and those it needs while loading are stored where it loads");
    println!("them from at the start, so it does not stop to make them while you play.");
    println!("This takes a while, and possibly a long time, depending on your hardware (CPU above all).");
    println!("What is done is kept: stopped (close this window), the next preload goes on from there.");
    println!("A later update of this build, or a new graphics driver, may need a new preload.");
    println!();
}

/// Runs `victory preload` over the title's folder with this start's settings
/// (the translations depend on them), then lets the start go on, whatever came of it.
pub(crate) fn preload(exe: &Path) {
    let folder = exe.parent().map(Path::to_path_buf).unwrap_or_default();
    let began = std::time::Instant::now();
    let done = std::env::current_exe().and_then(|program| {
        std::process::Command::new(program)
            .arg("preload")
            .arg(&folder)
            // Its own lines in this window; the start's log is the title's.
            .env_remove("VICTORY_LOG_FILE")
            .status()
    });
    let minutes = began.elapsed().as_secs_f64() / 60.0;
    match done {
        Ok(s) if s.success() => println!("Shaders preloaded in {minutes:.1} min. Starting the game."),
        Ok(s) => println!("The preload stopped ({s}) after {minutes:.1} min; what it did is kept. Starting the game."),
        Err(e) => println!("The preload did not start ({e}). Starting the game."),
    }
}

/// `%LOCALAPPDATA%\victory`, or the folder `VICTORY_HOME` names.
fn home() -> Result<PathBuf> {
    if let Some(home) = victory_core::config::home_override() {
        return Ok(home);
    }
    let base = std::env::var_os("LOCALAPPDATA").context("no %LOCALAPPDATA% to keep settings and logs in")?;
    Ok(PathBuf::from(base).join("victory"))
}

/// Where `victory get-dlss` puts NVIDIA's files.
fn nvidia_home() -> Result<PathBuf> {
    Ok(home()?.join("nvidia-sdk"))
}

fn has_streamline(dir: &Path) -> bool {
    dir.join("sl.interposer.dll").is_file()
}

/// The folder with NVIDIA's files: the one `VICTORY_STREAMLINE_DIR` names,
/// where `get-dlss` puts them, or beside this program (a repository's
/// `nvidia-sdk`, or the files themselves).
fn streamline_dir(here: &Path) -> Option<PathBuf> {
    let named = std::env::var_os("VICTORY_STREAMLINE_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let ours = nvidia_home().ok().map(|d| d.join("Streamline").join("bin").join("x64"));
    let beside = [here.join("nvidia-sdk").join("Streamline").join("bin").join("x64"), here.to_path_buf()];
    named.into_iter().chain(ours).chain(beside).find(|d| has_streamline(d))
}

/// Where `victory get-fsr` puts AMD's files.
fn amd_home() -> Result<PathBuf> {
    Ok(home()?.join("amd-fidelityfx"))
}

fn has_fsr(dir: &Path) -> bool {
    FSR_FILES.iter().all(|f| dir.join(f).is_file())
}

/// The folder with AMD's FSR 3 files: the one `VICTORY_FIDELITYFX_DIR` names,
/// where `get-fsr` puts them, or beside this program (a `fidelityfx` folder,
/// a repository's `amd-sdk`, or the files themselves).
fn fidelityfx_dir(here: &Path) -> Option<PathBuf> {
    let named = std::env::var_os("VICTORY_FIDELITYFX_DIR").filter(|v| !v.is_empty()).map(PathBuf::from);
    let ours = amd_home().ok();
    let beside = [here.join("fidelityfx"), here.join("amd-sdk").join("FidelityFX").join("bin"), here.to_path_buf()];
    named.into_iter().chain(ours).chain(beside).find(|d| has_fsr(d))
}

/// The lines `NAME=value` of the menu's file.
fn parse_menu(text: &str) -> Vec<(String, String)> {
    text.lines()
        .filter_map(|l| l.split_once('='))
        .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
        .filter(|(k, _)| {
            k.starts_with("VICTORY_") && k.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
        })
        .collect()
}

/// The settings of a start and whether it is the Series S profile: the
/// options, what the menu saved over them (`menu`), `--set` over both.
/// `streamline`: the folder with NVIDIA's files, if there is one.
fn settings(o: &Play, menu: &[(String, String)], streamline: Option<&Path>) -> (BTreeMap<String, String>, bool) {
    let saved = |k: &str| menu.iter().rev().find(|(n, _)| n == k).map(|(_, v)| v.as_str());
    let series_s = saved("VICTORY_MENU_CONSOLE").map_or(o.series_s, |c| c == "series-s");
    let res = saved("VICTORY_MENU_RES").filter(|r| RESOLUTIONS.contains(r)).unwrap_or(&o.res);
    let dlss = saved("VICTORY_MENU_DLSS").filter(|d| DLSS_MODES.contains(d)).unwrap_or(&o.dlss);
    // 0: off; 2 to 6: that many frames shown for each one rendered (1, from an older file: 2).
    let (mut framegen, mut times) = (o.framegen.is_some(), o.framegen.unwrap_or(2));
    if let Some(saved) = saved("VICTORY_MENU_FRAMEGEN") {
        let n = saved.parse::<u32>().ok().filter(|n| (1..=6).contains(n));
        framegen = n.is_some();
        if let Some(n) = n.filter(|n| *n >= 2) {
            times = n;
        }
    }
    let fg_type = saved("VICTORY_MENU_FG_TYPE").filter(|t| FG_TYPES.contains(t)).unwrap_or(&o.fg_type);
    let fps = saved("VICTORY_MENU_FPS")
        .and_then(|f| f.parse::<u32>().ok())
        .filter(|f| (1..=1000).contains(f))
        .unwrap_or(o.fps);

    let mut s: BTreeMap<String, String> = SETTINGS.iter().map(|(k, v)| ((*k).to_owned(), (*v).to_owned())).collect();
    let mut set = |k: &str, v: &str| drop(s.insert(k.to_owned(), v.to_owned()));
    if !series_s {
        match res {
            "1440" => set("VICTORY_NATIVE_1440P", "1"),
            "1080to1440" => set("VICTORY_RENDER_1080P", "1"),
            _ => {}
        }
    }
    let (mut dlss_on, mut framegen_x) = (false, None);
    if dlss != "off"
        && let Some(dir) = streamline
    {
        dlss_on = true;
        set("VICTORY_DLSS", "1");
        set("VICTORY_DLSS_MODE", dlss);
        set("VICTORY_STREAMLINE_DIR", &dir.to_string_lossy());
        if framegen {
            framegen_x = Some(times);
        } else if !o.no_framegen_ready {
            framegen_x = Some(0); // ready, off
        }
        if let Some(x) = framegen_x {
            set("VICTORY_FRAMEGEN", "1");
            set("VICTORY_FRAMEGEN_X", &x.to_string());
            // Frame generation takes 1.5 to 2 GB of the card's memory, which the layer
            // otherwise fills with textures: on from the start it is left a fifth of the
            // budget; ready but off, a little less is held back.
            set("VICTORY_TEXTURE_ROOM_PERCENT", if framegen { "80" } else { "86" });
        }
    }
    // AMD FSR 3 frame generation, made ready as DLSS frame generation is: auto, the layer uses
    // it only where DLSS frame generation does not run; fsr, DLSS frame generation is not asked.
    // The layer checks the files and the card and says at the start what runs and why not.
    if fg_type != "dlss" && (framegen || !o.no_framegen_ready) {
        set("VICTORY_FSR_FRAMEGEN", "1");
        if fg_type == "fsr" {
            s.remove("VICTORY_FRAMEGEN");
        }
        let mut set = |k: &str, v: &str| drop(s.insert(k.to_owned(), v.to_owned()));
        let x = *framegen_x.get_or_insert(if framegen { times } else { 0 });
        set("VICTORY_FRAMEGEN_X", &x.to_string());
        if !s.contains_key("VICTORY_TEXTURE_ROOM_PERCENT") {
            s.insert("VICTORY_TEXTURE_ROOM_PERCENT".to_owned(), if framegen { "80" } else { "86" }.to_owned());
        }
    }
    let mut set = |k: &str, v: &str| drop(s.insert(k.to_owned(), v.to_owned()));
    set("VICTORY_FRAME_INTERVAL_US", &(1_000_000.0 / f64::from(fps)).round().to_string());
    for (k, v) in menu.iter().filter(|(k, _)| !k.starts_with("VICTORY_MENU_")) {
        if v.is_empty() {
            s.remove(k);
        } else {
            s.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in o.more.iter().filter_map(|m| m.split_once('=')) {
        s.insert(k.trim().to_owned(), v.trim().to_owned());
    }
    // For the menu: what it shows as the current choices.
    let mut set = |k: &str, v: &str| drop(s.insert(k.to_owned(), v.to_owned()));
    set("VICTORY_MENU_CONSOLE", if series_s { "series-s" } else { "series-x" });
    set("VICTORY_MENU_RES", res);
    set("VICTORY_MENU_DLSS", if dlss_on { dlss } else { "off" });
    set("VICTORY_MENU_FRAMEGEN", &framegen_x.unwrap_or(0).to_string());
    set("VICTORY_MENU_FG_TYPE", fg_type);
    set("VICTORY_MENU_FPS", &fps.to_string());
    (s, series_s)
}

/// `year-month-day-hour-minute-second` (UTC) of `seconds` since 1970.
fn stamp(seconds: u64) -> String {
    let (days, rest) = (seconds / 86_400, seconds % 86_400);
    // (Howard Hinnant's civil-from-days.)
    let z = days as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let (day, month) = (doy - (153 * mp + 2) / 5 + 1, if mp < 10 { mp + 3 } else { mp - 9 });
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!("{year:04}{month:02}{day:02}-{:02}{:02}{:02}", rest / 3600, rest % 3600 / 60, rest % 60)
}

/// A new log file's name under `logs`, the ten newest of ours kept. The
/// process id is part of it: two starts in one second (a test beside a
/// player's own start, 2026-11-26) had the same name, and one of them was
/// left without a log.
fn log_file(logs: &Path) -> Result<PathBuf> {
    std::fs::create_dir_all(logs).with_context(|| format!("creating {}", logs.display()))?;
    let mut old: Vec<PathBuf> = std::fs::read_dir(logs)
        .into_iter()
        .flatten()
        .flatten()
        .map(|e| e.path())
        // ("play-": the name before 2026-11-27; it sorts first, so goes first.)
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with(LOG_PREFIX) || n.to_string_lossy().starts_with("play-")))
        .collect();
    old.sort();
    for gone in old.iter().rev().skip(9) {
        let _ = std::fs::remove_file(gone);
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    Ok(logs.join(format!("{LOG_PREFIX}{}-{}.log", stamp(now), std::process::id())))
}

/// The start of the built-in launcher's log names (scripts\play-foxhound.ps1's
/// are `victory-nvidia-<date>-<time>.log`, kept apart from these).
const LOG_PREFIX: &str = "victory-nvidia-play-";

/// Runs the fetcher: NVIDIA's files from NVIDIA's GitHub into `to`.
fn fetch(to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let script = to.join("get-nvidia-sdks.ps1");
    std::fs::write(&script, FETCH).with_context(|| format!("writing {}", script.display()))?;
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-To")
        .arg(to)
        .status()
        .context("starting Windows PowerShell")?;
    let _ = std::fs::remove_file(&script);
    anyhow::ensure!(
        status.success() && has_streamline(&to.join("Streamline").join("bin").join("x64")),
        "NVIDIA's files did not arrive (see above); the title runs without DLSS and frame generation"
    );
    Ok(())
}

/// `victory get-dlss`.
pub(crate) fn get_dlss() -> Result<()> {
    let to = nvidia_home()?;
    let _ = std::fs::remove_file(to.join("declined"));
    fetch(&to)
}

/// Asks once, at a start without NVIDIA's files, whether to fetch them.
fn offer_dlss() {
    let Ok(to) = nvidia_home() else { return };
    let declined = to.join("declined");
    if declined.exists() || !std::io::stdin().is_terminal() {
        println!("No NVIDIA files: running without DLSS and frame generation (`victory get-dlss` fetches them).");
        return;
    }
    println!("NVIDIA's DLSS and frame generation files are not on this PC yet. They are NVIDIA's own,");
    println!("under NVIDIA's licences, and come from github.com/NVIDIA-RTX/Streamline (about 70 MB, once).");
    print!("Download them now? [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    if answer.trim().to_ascii_lowercase().starts_with('n') {
        let _ = std::fs::create_dir_all(&to);
        let _ = std::fs::write(&declined, "asked once; `victory get-dlss` fetches them\r\n");
        println!("Not downloaded. `victory get-dlss` does it later; the title runs with its own upscaling.");
    } else if let Err(e) = fetch(&to) {
        println!("{e:#}");
    }
}

/// Runs the AMD fetcher: AMD's FSR 3 files from AMD's GitHub into `to`.
fn fetch_fsr(to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let script = to.join("get-amd-fsr.ps1");
    std::fs::write(&script, FETCH_FSR).with_context(|| format!("writing {}", script.display()))?;
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-To")
        .arg(to)
        .status()
        .context("starting Windows PowerShell")?;
    let _ = std::fs::remove_file(&script);
    anyhow::ensure!(
        status.success() && has_fsr(to),
        "AMD's files did not arrive (see above); the title runs without FSR 3 frame generation"
    );
    Ok(())
}

/// `victory get-fsr`.
pub(crate) fn get_fsr() -> Result<()> {
    let to = amd_home()?;
    let _ = std::fs::remove_file(to.join("declined"));
    fetch_fsr(&to)
}

/// Asks once, at a start that would use FSR 3 without AMD's files, whether to fetch them.
fn offer_fsr(card: &str) {
    let Ok(to) = amd_home() else { return };
    let declined = to.join("declined");
    if declined.exists() || !std::io::stdin().is_terminal() {
        println!("No AMD FSR 3 files: frame generation stays off on this card (`victory get-fsr` fetches them).");
        return;
    }
    if card.is_empty() {
        println!("FSR 3 frame generation is chosen, and AMD's files for it are not on this PC yet.");
    } else {
        println!("This card ({card}) has no DLSS frame generation (it needs an RTX 40 or later), but AMD's");
        println!("FSR 3 frame generation runs on it (2x). AMD's files for it are not on this PC yet.");
    }
    println!("They are AMD's own, under AMD's licence, and come from github.com/GPUOpen-LibrariesAndSDKs/FidelityFX-SDK");
    println!("(about 30 MB, once).");
    print!("Download them now? [Y/n] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    if answer.trim().to_ascii_lowercase().starts_with('n') {
        let _ = std::fs::create_dir_all(&to);
        let _ = std::fs::write(&declined, "asked once; `victory get-fsr` fetches them\r\n");
        println!("Not downloaded. `victory get-fsr` does it later; the title runs without frame generation.");
    } else if let Err(e) = fetch_fsr(&to) {
        println!("{e:#}");
    }
}

/// `scripts\get-mfg-unlocker.ps1`, run by Windows PowerShell.
const FETCH_MFG: &str = include_str!("../../../scripts/get-mfg-unlocker.ps1");

/// The mod's DLL, under its own name: loaded by this program (main.rs, [`load_mfg`]), not by
/// Windows in place of one of its own, so nothing is renamed or put beside the game.
const MFG_DLL: &str = "MFGUnlocker.dll";

/// Where `victory get-mfg` puts MFG Unlocker.
fn mfg_home() -> Result<PathBuf> {
    Ok(home()?.join("mfg-unlocker"))
}

/// A GeForce RTX of the 20 or 30 series ("NVIDIA GeForce RTX 3080", "... RTX 2060 SUPER"): DLSS
/// runs, DLSS Frame Generation only with MFG Unlocker.
fn older_rtx(name: &str) -> bool {
    let name = name.to_ascii_uppercase();
    let Some(at) = name.find("RTX") else { return false };
    let model: String = name[at + 3..].trim_start().chars().take_while(char::is_ascii_digit).collect();
    model.len() == 4 && (model.starts_with("20") || model.starts_with("30"))
}

/// Runs the fetcher: MFG Unlocker from its author's GitHub release into `to`.
fn fetch_mfg(to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {}", to.display()))?;
    let script = to.join("get-mfg-unlocker.ps1");
    std::fs::write(&script, FETCH_MFG).with_context(|| format!("writing {}", script.display()))?;
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-To")
        .arg(to)
        .status()
        .context("starting Windows PowerShell")?;
    let _ = std::fs::remove_file(&script);
    anyhow::ensure!(
        status.success() && to.join(MFG_DLL).is_file(),
        "MFG Unlocker did not arrive (see above); the game runs without it"
    );
    Ok(())
}

/// `victory get-mfg`: fetches it (F1's MFG UNLOCKER turns it on).
pub(crate) fn get_mfg() -> Result<()> {
    let to = mfg_home()?;
    let _ = std::fs::remove_file(to.join("declined"));
    fetch_mfg(&to)?;
    println!("Turn it on in the game: F1, MFG UNLOCKER, ON (or `victory play --set VICTORY_MFG_UNLOCKER=1`).");
    Ok(())
}

/// Asks once, on an RTX 20 or 30 card, whether to turn MFG Unlocker on. No is the default: it is
/// someone else's mod, NVIDIA does not support what it does, and it was not tested here.
/// `true`: turned on (saved in `menu_file`, as the F1 menu would).
fn offer_mfg(card: &str, menu_file: &Path) -> bool {
    let Ok(to) = mfg_home() else { return false };
    let declined = to.join("declined");
    if declined.exists() || !std::io::stdin().is_terminal() {
        return false;
    }
    println!("This card ({card}) runs DLSS, but NVIDIA's DLSS frame generation only on RTX 40 and later.");
    println!("MFG Unlocker, someone else's mod (github.com/dashdogy/MFG-Unlocker, MIT licence, about 5 MB),");
    println!("unlocks it on RTX 20 and 30 cards. NVIDIA does not support that, and it was not tested with");
    println!("this game. On, it is downloaded now and at every start loaded by victory.exe; F1, MFG UNLOCKER");
    println!("turns it off again. Off, the game uses AMD's FSR 3 frame generation (2x) on this card.");
    print!("Turn MFG Unlocker on? [y/N] ");
    let _ = std::io::stdout().flush();
    let mut answer = String::new();
    let _ = std::io::stdin().read_line(&mut answer);
    let _ = std::fs::create_dir_all(&to);
    if !answer.trim().to_ascii_lowercase().starts_with('y') {
        let _ = std::fs::write(&declined, "asked once; F1, MFG UNLOCKER turns it on\r\n");
        println!("Left off. F1, MFG UNLOCKER turns it on later.");
        return false;
    }
    let mut text = std::fs::read_to_string(menu_file).unwrap_or_default();
    if !text.is_empty() && !text.ends_with('\n') {
        text.push_str("\r\n");
    }
    text.push_str("VICTORY_MFG_UNLOCKER=1\r\n");
    if let Err(e) = std::fs::write(menu_file, text) {
        println!("Could not save the choice ({e}); on for this start only.");
    }
    true
}

/// With MFG Unlocker on: the DLL, fetched first where it is missing (no question: turning it on
/// was the choice). `None`: not there (the reason said).
fn mfg_dll() -> Option<PathBuf> {
    let to = mfg_home().ok()?;
    let dll = to.join(MFG_DLL);
    if !dll.is_file() {
        println!("MFG UNLOCKER is on and not on this PC yet: downloading it.");
        if let Err(e) = fetch_mfg(&to) {
            println!("{e:#}");
            return None;
        }
    }
    Some(dll)
}

/// Loads MFG Unlocker (`VICTORY_MFG_UNLOCKER_DLL`, set by [`prepare`] with the option on) into
/// this process, before the layer makes its device or loads Streamline: it wraps NVIDIA's frame
/// generation from there, as it would loaded by Windows in place of `winmm.dll`. It writes its
/// own logs and settings (`RTXMFG-...`) beside the program; Backspace opens its menu.
pub(crate) fn load_mfg() {
    let Some(dll) = std::env::var_os("VICTORY_MFG_UNLOCKER_DLL").filter(|v| !v.is_empty()).map(PathBuf::from) else {
        return;
    };
    match victory_hle::streamline::load_mod("MFG Unlocker (F1, MFG UNLOCKER)", &dll) {
        Ok(()) => println!("MFG Unlocker loaded ({}). Backspace opens its menu.", dll.display()),
        Err(code) => println!("MFG Unlocker did not load (Windows error {code}): running without it."),
    }
}

/// `D3D12` beside this program: where Windows looks for Microsoft's newer
/// Direct3D 12 runtime (main.rs's `D3D12SDKPath`).
fn d3d12_dir() -> Result<PathBuf> {
    let program = std::env::current_exe().context("this program's own path")?;
    Ok(program.parent().map(Path::to_path_buf).unwrap_or_default().join("D3D12"))
}

/// The highest shader model the default GPU runs, asked by a process of our
/// own (`victory shader-model`: a process keeps the first Direct3D 12 runtime
/// it loads, and this one is yet to choose). `None`: no answer.
fn shader_model() -> Option<(u32, u32)> {
    let program = std::env::current_exe().ok()?;
    let out = std::process::Command::new(program)
        .arg("shader-model")
        .env_remove("VICTORY_LOG_FILE")
        .stdin(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .output()
        .ok()?;
    let text = String::from_utf8_lossy(&out.stdout);
    let (major, minor) = text.lines().rev().find_map(|l| l.trim().split_once('.'))?;
    Some((major.parse().ok()?, minor.parse().ok()?))
}

/// Runs the fetcher: Microsoft's D3D12Core.dll from nuget.org into `to`.
fn fetch_d3d12(to: &Path) -> Result<()> {
    std::fs::create_dir_all(to).with_context(|| format!("creating {} (beside victory.exe)", to.display()))?;
    let script = std::env::temp_dir().join(format!("victory-get-d3d12-{}.ps1", std::process::id()));
    std::fs::write(&script, FETCH_D3D12).with_context(|| format!("writing {}", script.display()))?;
    let status = std::process::Command::new("powershell.exe")
        .args(["-NoProfile", "-ExecutionPolicy", "Bypass", "-File"])
        .arg(&script)
        .arg("-To")
        .arg(to)
        .status()
        .context("starting Windows PowerShell")?;
    let _ = std::fs::remove_file(&script);
    anyhow::ensure!(
        status.success() && to.join("D3D12Core.dll").is_file(),
        "Microsoft's Direct3D 12 runtime did not arrive (see above)"
    );
    Ok(())
}

/// `victory get-d3d12`.
pub(crate) fn get_d3d12() -> Result<()> {
    fetch_d3d12(&d3d12_dir()?)
}

/// Makes sure the GPU runs the translated shaders' model (6.6) before the
/// title starts: where Windows' own Direct3D 12 does not (Windows 10),
/// Microsoft's newer runtime is fetched beside this program, after asking.
/// Below 6.6 every pipeline is refused and the title closes within seconds,
/// so a start that still cannot run it stops here, saying why.
fn check_shader_model() -> Result<()> {
    let needed = victory_hle::streamline::SHADER_MODEL_NEEDED;
    let Some(found) = shader_model() else { return Ok(()) };
    if found >= needed {
        return Ok(());
    }
    let to = d3d12_dir()?;
    let had = to.join("D3D12Core.dll").is_file();
    if !had {
        println!("This PC's Direct3D 12 runs shader model {}.{}, and the game's translated shaders need 6.6.", found.0, found.1);
        println!("Windows 10's own Direct3D 12 stops at 6.5; Microsoft's newer runtime (the D3D12 Agility SDK)");
        println!("runs 6.6. It is Microsoft's own, under Microsoft's licence, from nuget.org (about 22 MB, once),");
        println!("and goes into {}.", to.display());
        let yes = if std::io::stdin().is_terminal() {
            print!("Download it now? [Y/n] ");
            let _ = std::io::stdout().flush();
            let mut answer = String::new();
            let _ = std::io::stdin().read_line(&mut answer);
            !answer.trim().to_ascii_lowercase().starts_with('n')
        } else {
            true
        };
        if yes {
            match fetch_d3d12(&to) {
                Ok(()) => match shader_model() {
                    Some(now) if now >= needed => {
                        println!("Shader model {}.{} with Microsoft's runtime. Starting the game.", now.0, now.1);
                        return Ok(());
                    }
                    now => {
                        let now = now.unwrap_or(found);
                        println!("With Microsoft's runtime the GPU still runs shader model {}.{}.", now.0, now.1);
                    }
                },
                Err(e) => println!("{e:#}"),
            }
        }
    }
    let now = shader_model().unwrap_or(found);
    anyhow::bail!(
        "this PC's Direct3D 12 runs shader model {}.{}, and the game needs 6.6: every shader would be \
         refused and the game would close at once. {}",
        now.0,
        now.1,
        if to.join("D3D12Core.dll").is_file() {
            "Microsoft's newer runtime is in place (D3D12 beside victory.exe), so it is the graphics \
             driver: update it, or use Windows 11"
        } else {
            "`victory get-d3d12` fetches Microsoft's newer runtime into D3D12 beside victory.exe; \
             or use Windows 11"
        }
    )
}

/// DLLs beside this program under the names of Windows' own that it loads (a
/// mod's proxy: MFG Unlocker as `winmm.dll` unlocks DLSS Frame Generation on
/// RTX 20 and 30 cards), for the log: a report then says what else ran.
fn mod_dlls(here: &Path) -> Vec<String> {
    const NAMES: [&str; 7] = ["winmm.dll", "version.dll", "dxgi.dll", "d3d12.dll", "xinput1_4.dll", "dinput8.dll", "winhttp.dll"];
    NAMES.iter().filter(|n| here.join(n).is_file()).map(|n| (*n).to_owned()).collect()
}

/// How long the first start of a new victory.exe plays before it restarts.
const WARMUP_SECONDS: u32 = 20;

/// The file that says this victory.exe (its bytes) had its warm-up start:
/// `%LOCALAPPDATA%\victory\warmed\<hash>`. `None`: the exe could not be read.
fn warmup_mark(home: &Path, program: &Path) -> Option<PathBuf> {
    let bytes = std::fs::read(program).ok()?;
    let hash = blake3::hash(&bytes).to_hex();
    Some(home.join("warmed").join(&hash.as_str()[..16]))
}

/// What a start needs once its settings are in the environment.
pub(crate) struct Ready {
    pub(crate) exe: PathBuf,
    pub(crate) series_s: bool,
}

/// Finds the title, puts the settings into the environment and names the
/// log file (`VICTORY_LOG_FILE`). Called first thing, before any thread of
/// ours is started and before logging is set up.
pub(crate) fn prepare(o: &Play) -> Result<Ready> {
    let program = std::env::current_exe().context("this program's own path")?;
    let here = program.parent().map(Path::to_path_buf).unwrap_or_default();
    let exe = match &o.exe {
        Some(exe) => exe.clone(),
        None => [here.join(GAME), PathBuf::from(GAME)].into_iter().find(|p| p.is_file()).with_context(|| {
            format!(
                "no {GAME} beside this program ({}): put the program into your own dump's folder, \
                 or name the title's executable (`victory play <path to {GAME}>`)",
                here.display()
            )
        })?,
    };
    anyhow::ensure!(exe.is_file(), "no {}", exe.display());
    let exe = std::fs::canonicalize(&exe).unwrap_or(exe);
    // (Windows' own long form `\\?\C:\...` back to the plain one.)
    let exe = PathBuf::from(exe.to_string_lossy().trim_start_matches(r"\\?\").to_owned());

    check_shader_model()?;

    let home = home()?;
    std::fs::create_dir_all(&home).with_context(|| format!("creating {}", home.display()))?;
    let menu_file = home.join("victory-settings.txt");
    let menu = parse_menu(&std::fs::read_to_string(&menu_file).unwrap_or_default());
    let wants_dlss = menu
        .iter()
        .rev()
        .find(|(k, _)| k == "VICTORY_MENU_DLSS")
        .map_or(o.dlss.as_str(), |(_, v)| v.as_str())
        != "off";
    if wants_dlss && streamline_dir(&here).is_none() {
        offer_dlss();
    }
    let streamline = streamline_dir(&here);
    let (mut s, series_s) = settings(o, &menu, streamline.as_deref());
    // FSR 3 made ready: AMD's files, asked for only where they would be used (FSR 3 chosen,
    // or a card without DLSS Frame Generation).
    if s.contains_key("VICTORY_FSR_FRAMEGEN") {
        if fidelityfx_dir(&here).is_none() {
            let chosen = s.get("VICTORY_MENU_FG_TYPE").is_some_and(|t| t == "fsr");
            let card = victory_hle::streamline::adapter_name().unwrap_or_default();
            if chosen || !victory_hle::streamline::dlss_framegen_card(&card) {
                offer_fsr(if chosen { "" } else { &card });
            }
        }
        if let Some(dir) = fidelityfx_dir(&here) {
            s.insert("VICTORY_FIDELITYFX_DIR".to_owned(), dir.to_string_lossy().into_owned());
        }
    }
    // MFG Unlocker (F1, MFG UNLOCKER): DLSS frame generation on RTX 20 and 30 cards. Asked once
    // on such a card with DLSS on, where the choice was never made and no mod DLL is beside us.
    let mods = mod_dlls(&here);
    if s.contains_key("VICTORY_DLSS")
        && !menu.iter().any(|(k, _)| k == "VICTORY_MFG_UNLOCKER")
        && !s.contains_key("VICTORY_MFG_UNLOCKER")
        && mods.is_empty()
        && let Some(card) = victory_hle::streamline::adapter_name().filter(|c| older_rtx(c))
        && offer_mfg(&card, &menu_file)
    {
        s.insert("VICTORY_MFG_UNLOCKER".to_owned(), "1".to_owned());
    }
    if s.get("VICTORY_MFG_UNLOCKER").is_some_and(|v| v == "1")
        && let Some(dll) = mfg_dll()
    {
        s.insert("VICTORY_MFG_UNLOCKER_DLL".to_owned(), dll.to_string_lossy().into_owned());
    }
    // The first start of this victory.exe (no mark for its bytes yet): the game plays a little
    // and starts again by itself (crates\victory-gpu\src\host\menu.rs, `warmup`), since a new
    // exe's first run stays at about 14 fps. `--set VICTORY_WARMUP_RESTART=0`: not done.
    if !s.contains_key("VICTORY_WARMUP_RESTART")
        && let Some(mark) = warmup_mark(&home, &program)
        && !mark.exists()
    {
        println!("First start of this version of victory.exe: the game restarts by itself after {WARMUP_SECONDS} seconds,");
        println!("once (a new version's first run otherwise stays at about 14 fps).");
        s.insert("VICTORY_WARMUP_RESTART".to_owned(), WARMUP_SECONDS.to_string());
        s.insert("VICTORY_WARMUP_MARK".to_owned(), mark.to_string_lossy().into_owned());
    }
    // Where the menu saves, and how it starts the title again (`--set` is not carried over).
    let quoted = |p: &Path| format!("'{}'", p.to_string_lossy().replace('\'', "''"));
    s.insert("VICTORY_MENU_FILE".to_owned(), menu_file.to_string_lossy().into_owned());
    s.insert("VICTORY_MENU_PLAY".to_owned(), program.to_string_lossy().into_owned());
    s.insert(
        "VICTORY_MENU_ARGS".to_owned(),
        format!("play {}{}", quoted(&exe), if o.no_framegen_ready { " --no-framegen-ready" } else { "" }),
    );
    let log = log_file(&home.join("logs"))?;
    let console = if series_s { "series-s" } else { "series-x" };
    let line: Vec<String> = s.iter().map(|(k, v)| format!("{k}={v}")).collect();
    let mods_line = if mods.is_empty() {
        String::new()
    } else {
        println!("Beside this program, loaded in its place by Windows: {} (a mod, e.g. MFG Unlocker).", mods.join(", "));
        format!("# beside victory.exe (mods, loaded instead of Windows' own): {}\n", mods.join(" "))
    };
    std::fs::write(&log, format!("# console={console} build=release switches: {}\n{mods_line}", line.join(" ")))
        .with_context(|| format!("writing {}", log.display()))?;
    println!("Log: {}", log.display());
    s.insert("VICTORY_LOG_FILE".to_owned(), log.to_string_lossy().into_owned());

    let leftover: Vec<String> = std::env::vars_os()
        .filter_map(|(k, _)| k.into_string().ok())
        .filter(|k| k.starts_with("VICTORY_") && !KEPT.contains(&k.as_str()))
        .collect();
    // SAFETY: the first thing this process does: no other thread of ours runs yet.
    unsafe {
        for k in leftover {
            std::env::remove_var(k);
        }
        for (k, v) in &s {
            std::env::set_var(k, v);
        }
    }
    Ok(Ready { exe, series_s })
}

/// After a start that went wrong in a window of its own (a double click):
/// the text stays until a key is pressed.
pub(crate) fn hold_window() {
    if std::io::stdin().is_terminal() {
        print!("Press Enter to close this window.");
        let _ = std::io::stdout().flush();
        let _ = std::io::stdin().read_line(&mut String::new());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn options() -> Play {
        Play {
            exe: None,
            series_s: false,
            res: "1440".to_owned(),
            dlss: "quality".to_owned(),
            framegen: None,
            no_framegen_ready: false,
            fg_type: "auto".to_owned(),
            fps: 1000,
            more: Vec::new(),
            preload: false,
        }
    }

    #[test]
    fn a_plain_start_is_the_launchers() {
        let dir = Path::new(r"C:\nvidia");
        let (s, series_s) = settings(&options(), &[], Some(dir));
        assert!(!series_s);
        for (k, v) in SETTINGS {
            assert_eq!(s.get(*k).map(String::as_str), Some(*v), "{k}");
        }
        let get = |k: &str| s.get(k).map(String::as_str);
        assert_eq!(get("VICTORY_NATIVE_1440P"), Some("1"));
        assert_eq!((get("VICTORY_DLSS"), get("VICTORY_DLSS_MODE")), (Some("1"), Some("quality")));
        assert_eq!(get("VICTORY_STREAMLINE_DIR"), Some(r"C:\nvidia"));
        // frame generation ready, off, with a little memory held back for it
        assert_eq!((get("VICTORY_FRAMEGEN"), get("VICTORY_FRAMEGEN_X")), (Some("1"), Some("0")));
        assert_eq!(get("VICTORY_TEXTURE_ROOM_PERCENT"), Some("86"));
        assert_eq!(get("VICTORY_FRAME_INTERVAL_US"), Some("1000"));
        assert_eq!((get("VICTORY_MENU_DLSS"), get("VICTORY_MENU_FRAMEGEN")), (Some("quality"), Some("0")));
        assert_eq!((get("VICTORY_MENU_CONSOLE"), get("VICTORY_MENU_FPS")), (Some("series-x"), Some("1000")));
    }

    #[test]
    fn without_nvidias_files_there_is_no_dlss() {
        let (s, _) = settings(&options(), &[], None);
        let get = |k: &str| s.get(k).map(String::as_str);
        assert!(!s.contains_key("VICTORY_DLSS") && !s.contains_key("VICTORY_FRAMEGEN"));
        assert_eq!(get("VICTORY_MENU_DLSS"), Some("off"));
        // FSR 3 frame generation is made ready instead (the layer says whether it can run)
        assert_eq!((get("VICTORY_FSR_FRAMEGEN"), get("VICTORY_FRAMEGEN_X")), (Some("1"), Some("0")));
        assert_eq!(get("VICTORY_TEXTURE_ROOM_PERCENT"), Some("86"));
        assert_eq!(get("VICTORY_MENU_FG_TYPE"), Some("auto"));
    }

    #[test]
    fn fsr_chosen_leaves_dlss_frame_generation_unasked() {
        let mut o = options();
        o.framegen = Some(3);
        let menu = parse_menu("VICTORY_MENU_FG_TYPE=fsr\r\n");
        let (s, _) = settings(&o, &menu, Some(Path::new("x")));
        let get = |k: &str| s.get(k).map(String::as_str);
        assert_eq!((get("VICTORY_DLSS"), get("VICTORY_FRAMEGEN")), (Some("1"), None));
        assert_eq!((get("VICTORY_FSR_FRAMEGEN"), get("VICTORY_FRAMEGEN_X")), (Some("1"), Some("3")));
        assert_eq!((get("VICTORY_MENU_FG_TYPE"), get("VICTORY_MENU_FRAMEGEN")), (Some("fsr"), Some("3")));
        // dlss: no FSR 3
        o.fg_type = "dlss".to_owned();
        let (s, _) = settings(&o, &[], Some(Path::new("x")));
        assert_eq!((s.get("VICTORY_FRAMEGEN").map(String::as_str), s.get("VICTORY_FSR_FRAMEGEN")), (Some("1"), None));
    }

    #[test]
    fn what_the_menu_saved_goes_over_the_options() {
        let menu = parse_menu(
            "# Saved by the F1 menu\r\nVICTORY_MENU_FRAMEGEN=6\r\nVICTORY_MENU_CONSOLE=series-s\r\n\
             VICTORY_MENU_DLSS=performance\r\nVICTORY_MENU_FPS=60\r\nVICTORY_BVH_BUILDER=\r\n\
             VICTORY_AMBIENT=0.5\r\nVICTORY_EARLY_DEPTH=\r\nnot a setting=1\r\n",
        );
        let (s, series_s) = settings(&options(), &menu, Some(Path::new("x")));
        let get = |k: &str| s.get(k).map(String::as_str);
        assert!(series_s);
        // the Series S profile has no picture size of its own
        assert_eq!(get("VICTORY_NATIVE_1440P"), None);
        assert_eq!((get("VICTORY_DLSS_MODE"), get("VICTORY_FRAMEGEN_X")), (Some("performance"), Some("6")));
        assert_eq!(get("VICTORY_TEXTURE_ROOM_PERCENT"), Some("80"));
        assert_eq!(get("VICTORY_FRAME_INTERVAL_US"), Some("16667"));
        // a switch with a value is set, an empty one unset; the menu's own choices are not switches
        assert_eq!((get("VICTORY_AMBIENT"), get("VICTORY_EARLY_DEPTH")), (Some("0.5"), None));
        assert_eq!(get("VICTORY_BVH_BUILDER"), None);
        assert_eq!(get("VICTORY_MENU_FRAMEGEN"), Some("6"));
    }

    #[test]
    fn options_and_set_are_applied() {
        let mut o = options();
        (o.res, o.dlss, o.framegen, o.fps) = ("1080to1440".to_owned(), "off".to_owned(), Some(4), 30);
        o.more = vec!["VICTORY_AMBIENT=0".to_owned(), "VICTORY_NEW = 2".to_owned()];
        let (s, _) = settings(&o, &[], Some(Path::new("x")));
        let get = |k: &str| s.get(k).map(String::as_str);
        assert_eq!((get("VICTORY_RENDER_1080P"), get("VICTORY_NATIVE_1440P")), (Some("1"), None));
        // no DLSS, so no frame generation either
        assert_eq!((get("VICTORY_DLSS"), get("VICTORY_FRAMEGEN")), (None, None));
        assert_eq!(get("VICTORY_FRAME_INTERVAL_US"), Some("33333"));
        assert_eq!((get("VICTORY_AMBIENT"), get("VICTORY_NEW")), (Some("0"), Some("2")));
    }

    /// AMD's files are offered only to cards without DLSS Frame Generation (or FSR 3 chosen).
    #[test]
    fn cards_with_dlss_frame_generation() {
        use victory_hle::streamline::dlss_framegen_card as fg;
        assert!(fg("NVIDIA GeForce RTX 5070") && fg("NVIDIA GeForce RTX 4070 SUPER") && fg("NVIDIA GeForce RTX 5090 Laptop GPU"));
        assert!(!fg("NVIDIA GeForce RTX 3080 Ti") && !fg("NVIDIA GeForce RTX 2060") && !fg("NVIDIA GeForce GTX 1080"));
        assert!(!fg("AMD Radeon RX 7900 XTX") && !fg("Intel(R) Arc(TM) A770 Graphics") && !fg(""));
    }

    /// MFG Unlocker is offered only to RTX 20 and 30 cards.
    #[test]
    fn cards_offered_mfg_unlocker() {
        assert!(older_rtx("NVIDIA GeForce RTX 3080 Ti") && older_rtx("NVIDIA GeForce RTX 2060 SUPER") && older_rtx("NVIDIA GeForce RTX 3050 Laptop GPU"));
        assert!(!older_rtx("NVIDIA GeForce RTX 5070") && !older_rtx("NVIDIA GeForce RTX 4090") && !older_rtx("NVIDIA GeForce GTX 1080"));
        assert!(!older_rtx("NVIDIA RTX A2000") && !older_rtx("AMD Radeon RX 7900 XTX") && !older_rtx(""));
    }

    #[test]
    fn stamps_are_dates() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_795_753_200), "20261127-042000");
        assert_eq!(stamp(951_782_400), "20000229-000000");
    }
}
