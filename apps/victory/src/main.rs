//! `victory`: the command-line front end.
//!
//! Every command reads the user's own files and writes only to stdout or to
//! the user's cache directory (outside the repository).

mod bvh_check;
mod census;
mod inspect;
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
        /// Also every compute pipeline the archives hold (a compute
        /// pipeline is its shader alone, so no run is needed to know it):
        /// with `--used`, those and the drawn-with ones; without, all.
        /// The archives are read once; `paths` names them (default: the
        /// game folder's shaders.rpf and executable).
        #[arg(long)]
        compute: bool,
        /// The user waits for it: the processes run at normal priority,
        /// one per logical core.
        #[arg(long)]
        now: bool,
        /// Leave the ray-tracing resolves to the title: they are the ~1,000 biggest programs
        /// (nine tenths of a warm's compile time), and with `VICTORY_RESOLVE_ASYNC=1` the title
        /// makes the ones a scene binds in the background, without waiting for them.
        #[arg(long)]
        skip_resolves: bool,
        /// Say what there is to make and stop.
        #[arg(long)]
        status: bool,
        /// Ask the pipeline library, as a run does, for every pipeline made
        /// before (this build, this driver): how many it has and how many
        /// would be compiled by the title.
        #[arg(long)]
        verify: bool,
        /// Archives for `--compute`.
        paths: Vec<PathBuf>,
        /// (What the processes it starts are given.)
        #[arg(long, hide = true)]
        these: Option<String>,
        /// (Where they save what they made.)
        #[arg(long, hide = true)]
        shard: Option<PathBuf>,
    },
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
    /// Check the ray-tracing structures in a dump of the title memory by
    /// address (`VICTORY_MEMORY_DUMP`, diagnostics builds): which are whole,
    /// which have a fault, which overlap another.
    BvhCheck {
        /// The folder the run wrote `pages.txt` and `arena-<n>.bin` to.
        folder: PathBuf,
        /// One line per structure.
        #[arg(long)]
        list: bool,
    },
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
    victory_core::logging::init("info");
    // The audio recorder `run` starts for `VICTORY_AUDIO_CAPTURE`.
    #[cfg(all(windows, feature = "diagnostics"))]
    if victory_hle::modules::audio::capture::child() {
        return Ok(());
    }
    let cli = Cli::parse();
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
        Command::Warm {
            jobs,
            limit,
            again,
            used,
            compute,
            now,
            skip_resolves,
            status,
            verify,
            paths,
            these,
            shard,
        } => {
            warm::run(warm::Ask {
                jobs,
                limit,
                again,
                used,
                compute,
                now,
                skip_resolves,
                status,
                verify,
                paths,
                these,
                shard,
            })
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
        Command::BvhCheck { folder, list } => bvh_check::run(&folder, list),
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
    check_policy(&cfg)
}

/// Reports each configured path the asset policy refuses, and fails if
/// there is one, so that `victory config` exits nonzero.
fn check_policy(cfg: &victory_core::Config) -> Result<()> {
    let mut violations = 0;
    for p in cfg
        .game_dir
        .iter()
        .chain(&cfg.test_titles)
        .chain([&cfg.cache_dir, &cfg.data_dir])
    {
        if let Err(e) = victory_core::ensure_outside_repo(p) {
            println!("POLICY VIOLATION: {e}");
            violations += 1;
        }
    }
    anyhow::ensure!(
        violations == 0,
        "{violations} configured path(s) violate the asset policy"
    );
    Ok(())
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

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use super::check_policy;

    fn outside() -> PathBuf {
        std::env::temp_dir().join("victory-config-test")
    }

    #[test]
    fn config_check_passes_paths_outside_the_repository() {
        let cfg = victory_core::Config {
            cache_dir: outside().join("cache"),
            data_dir: outside().join("data"),
            ..Default::default()
        };
        assert!(check_policy(&cfg).is_ok());
    }

    #[test]
    fn config_check_fails_a_path_inside_the_repository() {
        let cfg = victory_core::Config {
            game_dir: Some(PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("game")),
            cache_dir: outside().join("cache"),
            data_dir: outside().join("data"),
            ..Default::default()
        };
        let err = check_policy(&cfg).unwrap_err();
        assert!(err.to_string().contains("1 configured path"), "{err}");
    }
}
