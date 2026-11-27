//! `victory-translate`: the shader batch of `victory translate` and
//! `victory shader-info` as a program of its own that builds on Linux too
//! (it shares their source with `apps/victory`).
//!
//! `--validate` loads DXC's validator (`dxil.dll`, or `libdxil.so` on
//! Linux: `VICTORY_DXIL`); `--pipelines` and `--warm` need the D3D12 host
//! and exist only in `victory`. The output is the one `victory translate`
//! prints, so the same comparison tools read it.

#[path = "../../victory/src/shader.rs"]
mod shader;
#[path = "../../victory/src/translate.rs"]
mod translate;

use std::path::PathBuf;

use anyhow::Result;
use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "victory-translate", version, about = "The offline shader batch")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Find shader containers in files or directories and report their layout.
    ShaderInfo {
        paths: Vec<PathBuf>,
        /// Print one line per container found.
        #[arg(long)]
        verbose: bool,
        /// Also list the decoded native instructions (implies --verbose).
        #[arg(long)]
        disasm: bool,
    },
    /// Run every shader found through the translator and report what it
    /// cannot handle yet.
    Translate {
        paths: Vec<PathBuf>,
        /// Check emitted DXIL with the DXC validator (`VICTORY_DXIL`).
        #[arg(long)]
        validate: bool,
        /// Print one line per program and outcome.
        #[arg(long)]
        list: bool,
        /// Worker threads (default: every logical core).
        #[arg(long)]
        jobs: Option<usize>,
    },
}

fn main() -> Result<()> {
    victory_core::logging::init("info");
    match Cli::parse().command {
        Command::ShaderInfo {
            paths,
            verbose,
            disasm,
        } => shader::run(paths, verbose || disasm, disasm),
        Command::Translate {
            paths,
            validate,
            list,
            jobs,
        } => {
            // As `victory translate` does.
            if let Some(n) = std::env::var("VICTORY_VIEW_LIMIT").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_view_heap_limit(n);
            }
            if let Some(n) = std::env::var("VICTORY_HOST_WAVE").ok().and_then(|v| v.parse().ok()) {
                victory_shader::emit::set_host_max_wave(n);
            }
            translate::run(paths, validate, false, list, jobs)
        }
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
