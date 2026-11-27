//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod census;
mod inspect;
#[cfg(windows)]
mod play;
#[cfg(all(windows, feature = "diagnostics"))]
mod replay;
mod run;
mod shader;
mod translate;
#[cfg(windows)]
mod warm;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand, ValueEnum};

/// The Direct3D 12 runtime this program asks Windows for: Microsoft's D3D12
/// Agility SDK 1.614, from `D3D12\D3D12Core.dll` beside victory.exe where it
/// is there (exported by build.rs). Windows uses it only where its own is
/// older: Windows 10's stops at shader model 6.5, and the translated shaders
/// are 6.6 (`victory play` fetches it there; 2026-11-27, an RTX 3060 on
/// Windows 10 22H2 had every pipeline refused). Without the folder, nothing changes.
///
/// A `D3D12Core.dll` there of another version (other forks' guides put 1.618
/// or 1.620 there) made Windows refuse Direct3D 12 altogether, "the D3D12 SDK
/// version configuration of the host exe is invalid" (0x887E0003; 2026-11-27,
/// an RTX 5080 and an RTX 3080 Ti on Windows 11): [`match_d3d12_core`] asks
/// for the version of the file that is there, before Direct3D 12 is loaded.
///
/// Without the folder, it asks for [`os_runtime`]: Windows' own runtime,
/// whatever its version. Asking for 614 there made a Windows 11 whose own
/// runtime is older (22H2, build 22623, 11-27) look for 1.614 in the missing
/// folder and refuse Direct3D 12 the same way (0x887E0003; releases before
/// v2026.11.27.2, which asked for nothing, ran there).
/// Windows reads it when `d3d12.dll` is loaded, so build.rs has that load
/// wait for the first use: imported the usual way, it was loaded before
/// `main`, and v2026.11.27.4 and .5 asked for 614 whatever was set here.
/// (An atomic: the same bytes as a `u32`, in memory that can be written.)
#[cfg(windows)]
#[unsafe(no_mangle)]
#[used]
#[allow(non_upper_case_globals)]
pub static D3D12SDKVersion: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(614);

/// The Agility SDK version of a `D3D12Core.dll` (its file version is
/// `1.<sdk>.<patch>.0`, e.g. 1.614.1.0): from the file's VS_FIXEDFILEINFO.
fn agility_version(dll: &[u8]) -> Option<u32> {
    let signature = 0xFEEF_04BD_u32.to_le_bytes();
    let at = dll.windows(4).position(|w| w == signature)?;
    let most = u32::from_le_bytes(dll.get(at + 8..at + 12)?.try_into().ok()?);
    let (major, minor) = (most >> 16, most & 0xffff);
    (major == 1 && minor >= 600).then_some(minor)
}

/// The `D3D12SDKVersion` a `D3D12Core.dll` exports: Windows' own in
/// System32 says which version it is (616 on Windows 11 25H2), as Microsoft's do.
fn exported_sdk_version(dll: &[u8]) -> Option<u32> {
    let u16_at = |at: usize| Some(u16::from_le_bytes(dll.get(at..at + 2)?.try_into().ok()?));
    let u32_at = |at: usize| Some(u32::from_le_bytes(dll.get(at..at + 4)?.try_into().ok()?));
    let pe = u32_at(0x3c)? as usize;
    (dll.get(pe..pe + 4)? == b"PE\0\0").then_some(())?;
    let sections = usize::from(u16_at(pe + 6)?);
    let optional = pe + 24;
    let table = optional + usize::from(u16_at(pe + 20)?);
    // PE32+ only: the export directory is the first data directory, at 112.
    (u16_at(optional)? == 0x20b).then_some(())?;
    let offset = |rva: u32| -> Option<usize> {
        (0..sections).find_map(|i| {
            let s = table + i * 40;
            let (size, va, raw_size, raw) = (u32_at(s + 8)?, u32_at(s + 12)?, u32_at(s + 16)?, u32_at(s + 20)?);
            (va <= rva && rva < va + size.max(raw_size)).then(|| (rva - va + raw) as usize)
        })
    };
    let exports = offset(u32_at(optional + 112)?)?;
    // NumberOfNames, AddressOfFunctions, AddressOfNames, AddressOfNameOrdinals
    let count = u32_at(exports + 24)? as usize;
    let functions_at = offset(u32_at(exports + 28)?)?;
    let names_at = offset(u32_at(exports + 32)?)?;
    let ordinals_at = offset(u32_at(exports + 36)?)?;
    (0..count).find_map(|i| {
        let name = offset(u32_at(names_at + 4 * i)?)?;
        if dll.get(name..name + 16)? != b"D3D12SDKVersion\0" {
            return None;
        }
        let ordinal = usize::from(u16_at(ordinals_at + 2 * i)?);
        u32_at(offset(u32_at(functions_at + 4 * ordinal)?)?)
    })
}

/// The version asked for where no `D3D12\D3D12Core.dll` is beside this
/// program: Windows' own runtime's (from System32's `D3D12Core.dll`), so
/// Windows keeps it, at most 614, the version the exe is tested with.
///
/// It was 4 (the first Agility SDK's) in v2026.11.27.5, but Windows then
/// turned off what came after it: once Direct3D 12 read the version set
/// here, frames held at 14 fps with a white window (Windows 11 25H2,
/// whose own is 616; muted runs, 2026-11-27). Where the file is missing or
/// unreadable (Windows older than 10 2004 reads no `D3D12SDKVersion`), 4.
#[cfg(windows)]
fn os_runtime() -> u32 {
    let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap_or_else(|| r"C:\Windows".into());
    std::fs::read(system.join("System32").join("D3D12Core.dll"))
        .ok()
        .and_then(|dll| exported_sdk_version(&dll))
        .map_or(4, |v| v.min(614))
}

/// Windows' build number (19045 is Windows 10 22H2, 22000 and up Windows
/// 11), from `RtlGetVersion`, which no compatibility shim changes.
/// `VICTORY_WINDOWS_BUILD` stands in for it (to try Windows 10's path).
#[cfg(windows)]
pub(crate) fn windows_build() -> Option<u32> {
    if let Some(build) = std::env::var("VICTORY_WINDOWS_BUILD").ok().and_then(|v| v.trim().parse().ok()) {
        return Some(build);
    }
    #[repr(C)]
    struct OsVersionInfo {
        size: u32,
        major: u32,
        minor: u32,
        build: u32,
        platform: u32,
        service_pack: [u16; 128],
    }
    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }
    let mut info = OsVersionInfo { size: size_of::<OsVersionInfo>() as u32, major: 0, minor: 0, build: 0, platform: 0, service_pack: [0; 128] };
    // SAFETY: `info` is an OSVERSIONINFOW with its size set, as RtlGetVersion takes.
    (unsafe { RtlGetVersion(&mut info) } == 0).then_some(info.build)
}

/// Asks Windows for the Agility SDK version of the `D3D12\D3D12Core.dll`
/// beside this program, or for its own runtime where there is none. First
/// thing in `main`, and again after `victory play` fetched the file: Windows
/// reads `D3D12SDKVersion` when Direct3D 12 is first used.
#[cfg(windows)]
pub(crate) fn match_d3d12_core() {
    use std::sync::atomic::Ordering::Relaxed;
    let Some(core) = std::env::current_exe().ok().and_then(|p| p.parent().map(|d| d.join("D3D12").join("D3D12Core.dll")))
    else {
        return;
    };
    let Ok(bytes) = std::fs::read(&core) else {
        D3D12SDKVersion.store(os_runtime(), Relaxed);
        return;
    };
    match agility_version(&bytes) {
        Some(v) => {
            if v != 614 {
                println!("{} is the D3D12 Agility SDK 1.{v}: asking Windows for that version.", core.display());
            }
            D3D12SDKVersion.store(v, Relaxed);
        }
        None => {
            D3D12SDKVersion.store(os_runtime(), Relaxed);
            println!("{} is not a D3D12 Agility SDK runtime Windows can use: Windows' own is used.", core.display());
        }
    }
}

/// `D3D12SDKPath`'s value: a pointer to a C string.
#[cfg(windows)]
#[repr(transparent)]
pub struct SdkPath(*const std::ffi::c_char);
// SAFETY: it points at a string literal, never written.
#[cfg(windows)]
unsafe impl Sync for SdkPath {}

/// Where `D3D12Core.dll` is, from victory.exe's folder.
#[cfg(windows)]
#[unsafe(no_mangle)]
#[used]
#[allow(non_upper_case_globals)]
pub static D3D12SDKPath: SdkPath = SdkPath(c".\\D3D12\\".as_ptr());

#[derive(Parser)]
#[command(
    name = "victory",
    version,
    about = "Run an Xbox Series title natively on Windows"
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Summarise one PE image: headers, sections, imports, TLS, build id.
    Inspect {
        file: PathBuf,
        /// List every imported function.
        #[arg(long)]
        imports: bool,
        /// List every exported function.
        #[arg(long)]
        exports: bool,
    },
    /// Aggregate the imports of every PE image under the given paths and
    /// show which ones our runtime covers. Defaults to the configured
    /// game and test-title directories.
    Census {
        paths: Vec<PathBuf>,
        /// Do not count host pass-through as covered.
        #[arg(long)]
        strict: bool,
        /// List every missing `dll!function`, most used first.
        #[arg(long)]
        missing: bool,
    },
    /// List the `dll!export` pairs an import library describes.
    LibExports {
        lib: PathBuf,
        /// List every export, not just per-DLL counts.
        #[arg(long)]
        list: bool,
    },
    /// Find shader containers in files or directories (also inside engine
    /// archives, if uncompressed) and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found.
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found (containers, SGD5 files) through the
    /// translator and report what it cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with dxil.dll's validator (`VICTORY_DXIL`,
        /// else the newest Windows SDK's).
        #[arg(long)]
        validate: bool,
        /// Also create a host D3D12 pipeline for each emitted shader.
        #[arg(long)]
        pipelines: bool,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
        /// Warm the run's shader cache: a file of shader hashes (the 16
        /// hex digits a run's log names a shader by, anywhere in a line);
        /// the containers with those hashes are translated into the
        /// cache and nothing else is done. Set `VICTORY_HOST_WAVE` and
        /// `VICTORY_VIEW_LIMIT` to what the run's log says of the host.
        #[arg(long)]
        warm: Option<PathBuf>,
    },
    /// Translate every shader found (as `translate` finds them) into the
    /// shader cache, so that no run has to: a few minutes, once per change
    /// of the translator.
    Shaders {
        paths: Vec<PathBuf>,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
    },
    /// Make the host pipelines of every pipeline the title ever created
    /// (runs with `VICTORY_PIPELINE_BOOK=1` write them down), so that the
    /// driver's cache on disk has them before a run needs one: once per change
    /// of the translator or the driver. Goes on where it was stopped.
    #[cfg(windows)]
    Warm {
        /// Processes at once (default: two thirds of the logical cores).
        #[arg(long)]
        jobs: Option<usize>,
        /// At most this many pipelines.
        #[arg(long)]
        limit: Option<usize>,
        /// Those made before too.
        #[arg(long)]
        again: bool,
        /// Only the pipelines earlier runs drew with.
        #[arg(long)]
        used: bool,
        /// (What the processes it starts are given.)
        #[arg(long, hide = true)]
        these: Option<String>,
    },
    /// Preload: `shaders` and then `warm` over the title's folder, and the
    /// pipelines a run makes while it loads stored in the pipeline library
    /// the runs start with; what they say is shown as they go (the F1 menu's "PRELOAD SHADERS" has the
    /// launcher do this before the title starts). A while, maybe long on
    /// slower hardware; an update of the translator or the driver needs it
    /// again. Stopped, it goes on where it was the next time.
    #[cfg(windows)]
    Preload {
        /// The title's folder (default: the configured game folder).
        paths: Vec<PathBuf>,
    },
    /// Play: the title with this fork's settings, as a start with no
    /// command at all does (a double click on the program beside the
    /// title's `game_scarlett_master.exe`). F1 in the game is the settings
    /// menu, F3 the stats panel.
    #[cfg(windows)]
    Play(play::Play),
    /// Fetch NVIDIA's DLSS and frame generation files from NVIDIA's GitHub
    /// (about 70 MB) to where `play` looks for them.
    #[cfg(windows)]
    GetDlss,
    /// Fetch AMD's FSR 3 frame generation files from AMD's GitHub (about
    /// 30 MB) to where `play` looks for them: frame generation on cards
    /// without DLSS frame generation.
    #[cfg(windows)]
    GetFsr,
    /// Fetch Microsoft's newer Direct3D 12 runtime (D3D12Core.dll, about
    /// 22 MB from nuget.org) into `D3D12` beside this program: Windows 10,
    /// whose own Direct3D 12 does not run the translated shaders. `play`
    /// does it by itself where it is needed.
    #[cfg(windows)]
    GetD3d12,
    /// Fetch MFG Unlocker (someone else's mod, github.com/dashdogy/MFG-Unlocker,
    /// about 5 MB): DLSS frame generation on RTX 20 and 30 cards, which NVIDIA
    /// does not support. `play` loads it with F1's MFG UNLOCKER on, and fetches
    /// it by itself then.
    #[cfg(windows)]
    GetMfg,
    /// Print the highest shader model the default GPU runs here (`play`
    /// asks it in a process of its own).
    #[cfg(windows)]
    #[command(hide = true)]
    ShaderModel,
    /// Prepare, map and bind a title executable; with --execute, run it.
    Run {
        exe: PathBuf,
        /// What unimplemented imports do when called.
        #[arg(long, value_enum, default_value_t = Unimplemented::Trap)]
        unimplemented: Unimplemented,
        /// Disable host pass-through (everything must be implemented by us).
        #[arg(long)]
        strict: bool,
        /// Actually start the title. Without it, stop after binding.
        #[arg(long)]
        execute: bool,
        /// The console model the title sees (device type, memory).
        #[arg(long, value_enum, default_value_t = Console::SeriesX)]
        console: Console,
        /// Arguments for the title, after `--` (it reads the whole command
        /// line; a title that relaunches itself passes some).
        #[arg(last = true)]
        title_args: Vec<String>,
    },
    /// Run a captured frame's GPU work again (diagnostics builds; see
    /// docs/replay.md).
    #[cfg(all(windows, feature = "diagnostics"))]
    Replay(replay::Replay),
    /// Show the resolved configuration and check it.
    Config,
    /// Check NVIDIA Streamline, DLSS and DLSS Frame Generation on this PC
    /// without the title (`VICTORY_STREAMLINE_DIR`, `VICTORY_DLSS_MODE`,
    /// `VICTORY_DLSS_PRESET`): what loads and what this GPU supports; with
    /// --device, the host device handed over and DLSS run three times on
    /// stand-in textures; with --present too, three seconds of presents with
    /// frame generation in a window of ours.
    #[cfg(windows)]
    UpscalerCheck {
        /// The output size, `<width>x<height>`.
        #[arg(long, default_value = "2560x1440")]
        output: String,
        /// Make the host device and run DLSS on it.
        #[arg(long)]
        device: bool,
        /// Present for three seconds with frame generation (with --device).
        #[arg(long)]
        present: bool,
    },
}

#[derive(Clone, Copy, ValueEnum)]
enum Unimplemented {
    Trap,
    Zero,
}

#[derive(Clone, Copy, ValueEnum)]
enum Console {
    SeriesX,
    SeriesS,
}

fn main() -> Result<()> {
    #[cfg(windows)]
    match_d3d12_core();
    // Before anything else can take the address space it needs.
    #[cfg(windows)]
    victory_hle::reserve_upload_space();
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if std::env::var_os("VICTORY_AUDIO_CAPTURE").is_some() {
        victory_core::logging::init("info");
        if victory_hle::modules::audio::capture::child() {
            return Ok(());
        }
    }
    // Started with nothing after its name (a double click): play.
    let bare = std::env::args_os().len() == 1;
    let cli = if bare && cfg!(windows) { Cli::parse_from(["victory", "play"]) } else { Cli::parse() };
    // Play puts its settings into the environment and names its log file
    // before anything else is set up.
    #[cfg(windows)]
    if let Command::Play(options) = &cli.command {
        // (Read before `prepare` clears the `VICTORY_...` variables: the menu's restart sets it.)
        let preload = options.preload || std::env::var_os("VICTORY_PRELOAD").is_some_and(|v| v == "1");
        let done = play::prepare(options).and_then(|ready| {
            if preload {
                play::preload(&ready.exe);
            }
            victory_core::logging::init("info");
            // F1's MFG UNLOCKER: the mod, before the layer makes its device or loads Streamline.
            play::load_mfg();
            let console = if ready.series_s { Console::SeriesS } else { Console::SeriesX };
            run::run(&ready.exe, Unimplemented::Trap, false, true, console)
        });
        if let Err(e) = &done
            && bare
        {
            eprintln!("Error: {e:#}");
            play::hold_window();
        }
        return done;
    }
    victory_core::logging::init("info");
    match cli.command {
        Command::Inspect {
            file,
            imports,
            exports,
        } => inspect::run(&file, imports, exports),
        Command::Census {
            paths,
            strict,
            missing,
        } => census::run(paths, strict, missing),
        Command::LibExports { lib, list } => inspect::lib_exports(&lib, list),
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            pipelines,
            list,
            jobs,
            warm,
        } => {
            // `VICTORY_VIEW_LIMIT=<null slot>`: translate as on a host whose view heap is capped
            if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_view_heap_limit(n);
            }
            // `VICTORY_HOST_WAVE=32`: translate as for a GPU whose waves are at most 32 lanes
            if let Some(n) = std::env::var("VICTORY_HOST_WAVE").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_host_max_wave(n);
            }
            if let Some(file) = warm {
                return translate::warm(paths, &file, jobs);
            }
            translate::run(paths, validate, pipelines, list, jobs)
        }
        Command::Shaders { paths, jobs } => translate::precompile(paths, jobs),
        #[cfg(windows)]
        Command::Warm { jobs, limit, again, used, these } => warm::run(jobs, limit, again, used, these),
        #[cfg(windows)]
        Command::Preload { paths } => {
            play::explain_preload();
            translate::precompile(paths, None)?;
            warm::run(None, None, false, false, None)?;
            warm::into_library(None)
        }
        #[cfg(windows)]
        Command::Play(_) => unreachable!("handled above"),
        #[cfg(windows)]
        Command::GetDlss => play::get_dlss(),
        #[cfg(windows)]
        Command::GetFsr => play::get_fsr(),
        #[cfg(windows)]
        Command::GetD3d12 => play::get_d3d12(),
        #[cfg(windows)]
        Command::GetMfg => play::get_mfg(),
        #[cfg(windows)]
        Command::ShaderModel => {
            let model = victory_hle::streamline::highest_shader_model();
            if let Some(core) = victory_hle::streamline::d3d12_core_path() {
                println!("runtime {}", core.display());
            }
            match model {
                Some((major, minor)) => println!("{major}.{minor}"),
                None => println!("none"),
            }
            Ok(())
        }
        Command::Run {
            exe,
            unimplemented,
            strict,
            execute,
            console,
            title_args,
        } => {
            victory_hle::runtime::set_title_args(title_args);
            run::run(&exe, unimplemented, strict, execute, console)
        }
        #[cfg(all(windows, feature = "diagnostics"))]
        Command::Replay(options) => replay::run(options),
        Command::Config => show_config(),
        #[cfg(windows)]
        Command::UpscalerCheck { output, device, present } => {
            let size = output
                .split_once(['x', 'X'])
                .and_then(|(w, h)| Some([w.trim().parse().ok()?, h.trim().parse().ok()?]))
                .ok_or_else(|| anyhow::anyhow!("--output {output}: not <width>x<height>"))?;
            for line in victory_hle::streamline::check(size, device || present, present) {
                println!("{line}");
            }
            Ok(())
        }
    }
}

fn show_config() -> Result<()> {
    let cfg = victory_core::Config::load()?;
    let status = |p: &Option<PathBuf>| match p {
        Some(p) if p.exists() => format!("{}", p.display()),
        Some(p) => format!("{}  (does not exist)", p.display()),
        None => "(not set)".to_owned(),
    };
    println!(
        "config file : {}",
        cfg.source
            .as_ref()
            .map_or("(none)".into(), |p| p.display().to_string())
    );
    println!("game_dir    : {}", status(&cfg.game_dir));
    println!("cache_dir   : {}", cfg.cache_dir.display());
    println!("data_dir    : {}", cfg.data_dir.display());
    for t in &cfg.test_titles {
        println!("test_title  : {}", status(&Some(t.clone())));
    }
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    /// A D3D12Core.dll's VS_FIXEDFILEINFO: signature, structure version, then
    /// the file version's high and low words (1.620.2.0 here).
    fn core_bytes(major: u32, minor: u32) -> Vec<u8> {
        let mut b = vec![0u8; 40];
        for w in [0xFEEF_04BD_u32, 0x0001_0000, (major << 16) | minor, 2 << 16] {
            b.extend_from_slice(&w.to_le_bytes());
        }
        b.extend_from_slice(&[0u8; 16]);
        b
    }

    #[test]
    fn agility_versions_are_read_from_the_file() {
        assert_eq!(super::agility_version(&core_bytes(1, 620)), Some(620));
        assert_eq!(super::agility_version(&core_bytes(1, 614)), Some(614));
        // Windows' own D3D12Core.dll (10.0.x) and files without the structure are not one.
        assert_eq!(super::agility_version(&core_bytes(10, 0)), None);
        assert_eq!(super::agility_version(&[0u8; 64]), None);
        assert_eq!(super::agility_version(&core_bytes(1, 620)[..48]), None);
    }

    #[cfg(windows)]
    #[test]
    fn windows_own_runtime_tells_its_version() {
        let system = std::env::var_os("SystemRoot").map(std::path::PathBuf::from).unwrap();
        if let Ok(dll) = std::fs::read(system.join("System32").join("D3D12Core.dll")) {
            let v = super::exported_sdk_version(&dll).expect("System32's D3D12Core.dll exports D3D12SDKVersion");
            assert!(v >= 4, "{v}");
            assert!((4..=614).contains(&super::os_runtime()));
        }
        assert_eq!(super::exported_sdk_version(&[0u8; 64]), None);
        assert_eq!(super::exported_sdk_version(b"MZ"), None);
    }
}

/// Paths given on the command line, or the configured game + test titles.
pub(crate) fn default_paths(paths: Vec<PathBuf>) -> Result<Vec<PathBuf>> {
    if !paths.is_empty() {
        return Ok(paths);
    }
    let cfg = victory_core::Config::load()?;
    let all: Vec<PathBuf> = cfg.game_dir.into_iter().chain(cfg.test_titles).collect();
    anyhow::ensure!(
        !all.is_empty(),
        "no paths given and none configured (see victory.example.toml)"
    );
    Ok(all)
}
