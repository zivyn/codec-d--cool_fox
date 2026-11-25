//! `victory replay`: a captured frame run again (docs/replay.md).

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Args, ValueEnum};
use victory_hle::replay::play::{self, Options};

#[derive(Clone, Copy, ValueEnum)]
enum Timing {
    /// Each list.
    Lists,
    /// Each command (adds some time of its own).
    Detailed,
}

#[derive(Clone, Copy, ValueEnum)]
enum Buffers {
    AsRecorded,
    /// All in the card's memory.
    Vram,
    /// All in the PC's memory.
    Sysmem,
}

#[derive(Args)]
pub(crate) struct Replay {
    /// The folder given as VICTORY_RECORD (its last capture) or a capture.
    folder: PathBuf,
    /// How often the frame is run.
    #[arg(long, default_value_t = 5)]
    loops: u32,
    /// Do not put back, between runs, what a run changed.
    #[arg(long)]
    no_restore: bool,
    #[arg(long, value_enum, default_value_t = Timing::Lists)]
    timing: Timing,
    /// Pipelines listed with detailed timing.
    #[arg(long, default_value_t = 20)]
    top: usize,
    /// Where the picture goes (default: replay.png in the capture).
    #[arg(long)]
    image: Option<PathBuf>,
    /// Run the frame unchanged first, and print both.
    #[arg(long)]
    compare: bool,
    /// Leave out unordered-access barriers.
    #[arg(long)]
    no_barriers: bool,
    /// Join barrier calls with no work between them.
    #[arg(long)]
    merge_barriers: bool,
    /// Leave out the argument clamps: what they bound is kept from a first run.
    #[arg(long)]
    skip_clamps: bool,
    /// Leave out the hash and compare passes over card buffers.
    #[arg(long)]
    skip_hash_passes: bool,
    /// Where buffers lie.
    #[arg(long, value_enum, default_value_t = Buffers::AsRecorded)]
    buffers: Buffers,
    /// One ExecuteCommandLists for the whole frame.
    #[arg(long)]
    one_submit: bool,
    /// Run only these lists, by their place in the frame (`3..7`, or `5`);
    /// of the others only barriers.
    #[arg(long)]
    only: Option<String>,
    /// Leave out what runs a pipeline whose name or DXIL hash has this in it
    /// (may be given several times).
    #[arg(long = "skip-shader")]
    skip_shader: Vec<String>,
    /// Use the D3D12 debug layer (slow).
    #[arg(long)]
    debug_layer: bool,
}

/// `3..7` (both included) or `5`.
fn range(text: &str) -> Option<(u32, u32)> {
    match text.split_once("..") {
        Some((a, b)) => Some((a.trim().parse().ok()?, b.trim().parse().ok()?)),
        None => text.trim().parse().ok().map(|n| (n, n)),
    }
}

pub(crate) fn run(r: Replay) -> Result<()> {
    let only = match &r.only {
        Some(text) => Some(
            range(text)
                .with_context(|| format!("--only {text}: not a list or a range of lists (3..7)"))?,
        ),
        None => None,
    };
    let options = Options {
        loops: r.loops.max(1),
        restore: !r.no_restore,
        detailed: matches!(r.timing, Timing::Detailed),
        top: r.top,
        image: r.image,
        buffers: match r.buffers {
            Buffers::AsRecorded => play::Buffers::AsRecorded,
            Buffers::Vram => play::Buffers::Card,
            Buffers::Sysmem => play::Buffers::Pc,
        },
        no_barriers: r.no_barriers,
        merge_barriers: r.merge_barriers,
        skip_clamps: r.skip_clamps,
        skip_hash_passes: r.skip_hash_passes,
        one_submit: r.one_submit,
        only,
        skip_shaders: r.skip_shader,
        compare: r.compare,
        debug: r.debug_layer,
    };
    let reports = play::replay(&r.folder, &options)?;
    for (what, report) in &reports {
        println!("{}", report.text(what));
    }
    if let [(_, before), (_, after)] = &reports[..] {
        println!("{:+.2} ms", after.median_ms() - before.median_ms());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #[test]
    fn ranges() {
        assert_eq!(super::range("3..7"), Some((3, 7)));
        assert_eq!(super::range("5"), Some((5, 5)));
        assert_eq!(super::range("x"), None);
        assert_eq!(super::range("1..x"), None);
    }
}
