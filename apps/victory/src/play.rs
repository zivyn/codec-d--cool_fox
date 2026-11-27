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

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};

/// The title's executable, looked for beside this program.
const GAME: &str = "game_scarlett_master.exe";

/// `scripts\get-nvidia-sdks.ps1`, run by Windows PowerShell.
const FETCH: &str = include_str!("../../../scripts/get-nvidia-sdks.ps1");

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
    "VICTORY_LOG",
];

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
    /// Frames a second at most (30: the title's own rate; 1000: no cap).
    #[arg(long, default_value_t = 1000, value_parser = clap::value_parser!(u32).range(1..=1000))]
    fps: u32,
    /// Add or override a switch, as often as needed: `--set VICTORY_X=1`.
    #[arg(long = "set", value_name = "NAME=VALUE")]
    more: Vec<String>,
}

/// `%LOCALAPPDATA%\victory`.
fn home() -> Result<PathBuf> {
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
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().starts_with("play-")))
        .collect();
    old.sort();
    for gone in old.iter().rev().skip(9) {
        let _ = std::fs::remove_file(gone);
    }
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
    Ok(logs.join(format!("play-{}-{}.log", stamp(now), std::process::id())))
}

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
    std::fs::write(&log, format!("# console={console} build=release switches: {}\n", line.join(" ")))
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
            fps: 1000,
            more: Vec::new(),
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
        assert!(!s.contains_key("VICTORY_DLSS") && !s.contains_key("VICTORY_FRAMEGEN"));
        assert!(!s.contains_key("VICTORY_TEXTURE_ROOM_PERCENT"));
        assert_eq!(s.get("VICTORY_MENU_DLSS").map(String::as_str), Some("off"));
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

    #[test]
    fn stamps_are_dates() {
        assert_eq!(stamp(0), "19700101-000000");
        assert_eq!(stamp(1_795_753_200), "20261127-042000");
        assert_eq!(stamp(951_782_400), "20000229-000000");
    }
}
