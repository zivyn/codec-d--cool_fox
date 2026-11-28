//! Which console the title runs on. Titles pick settings (resolution,
//! memory budgets) by it, so the user chooses; Series X unless told.

use std::sync::atomic::{AtomicBool, Ordering};

const GIB: usize = 1 << 30;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum Console {
    #[default]
    SeriesX,
    SeriesS,
}

impl Console {
    /// Title memory in bytes: (GPU-optimal, standard). Series X in the
    /// advanced configuration; Series S has one range, counted as standard
    /// (7,776 MiB, the published console-mode figure).
    // shortcut: later system updates gave Series S titles more (amount not
    // published here); the title sizes nothing from this figure
    // (it goes by the device type), so update it if a title does.
    pub const fn title_memory(self) -> (usize, usize) {
        match self {
            Console::SeriesX => (10 * GIB, 7 * GIB / 2),
            Console::SeriesS => (0, 7_776 << 20),
        }
    }

    pub const fn name(self) -> &'static str {
        match self {
            Console::SeriesX => "Xbox Series X",
            Console::SeriesS => "Xbox Series S",
        }
    }
}

static SERIES_S: AtomicBool = AtomicBool::new(false);

/// Set before the title starts.
pub fn set(console: Console) {
    SERIES_S.store(console == Console::SeriesS, Ordering::Relaxed);
    victory_formats::texture::set_gpu(tile_gpu(console));
}

/// The tile configuration the XOR swizzle modes are laid out by:
/// the console's, or `VICTORY_TILE_GPU=<pipes>,<packers>` (counts, like
/// `8,4`) to try another.
fn tile_gpu(console: Console) -> victory_formats::texture::Gpu {
    use victory_formats::texture::Gpu;
    let own = match console {
        Console::SeriesX => Gpu::SERIES_X,
        Console::SeriesS => Gpu::SERIES_S,
    };
    let spec = std::env::var("VICTORY_TILE_GPU").ok();
    let chosen = spec.as_deref().and_then(|spec| {
        let (pipes, packers) = spec.split_once(',')?;
        Gpu::from_counts(pipes.trim().parse().ok()?, packers.trim().parse().ok()?)
    });
    if let (Some(spec), None) = (&spec, chosen) {
        tracing::warn!(%spec, "VICTORY_TILE_GPU is not a pair of pipe and packer counts addrlib has patterns for; the console's is used");
    }
    let gpu = chosen.unwrap_or(own);
    // What the XOR modes are laid out by, for every run: an estimate unless the variable says another, and the two
    // switches that take them out again.
    tracing::info!(
        pipes = 1u32 << gpu.pipes_log2(),
        packers = 1u32 << gpu.packers_log2(),
        source = if chosen.is_some() { "VICTORY_TILE_GPU" } else { "estimate for the console" },
        xor_upload = crate::resource::xor_upload(),
        xor_readback = crate::resource::xor_readback(),
        "XOR swizzle modes are laid out for this many pipes and packers (VICTORY_XOR_UPLOAD=0 and VICTORY_XOR_READBACK=0 leave them out)"
    );
    gpu
}

pub fn current() -> Console {
    if SERIES_S.load(Ordering::Relaxed) {
        Console::SeriesS
    } else {
        Console::SeriesX
    }
}

static POOLS_OF_SERIES_X: AtomicBool = AtomicBool::new(false);

/// The title's memory pools were sized as a Series X's
/// (`VICTORY_TITLE_POOLS=series-x`, `victory_game::pools`) although the
/// device is another: what the title is told of its memory follows.
pub fn set_pools_of_series_x(on: bool) {
    POOLS_OF_SERIES_X.store(on, Ordering::Relaxed);
}

/// The console whose memory the title is told it has: the one it runs as,
/// or a Series X once its pools are sized as one's.
pub fn pools() -> Console {
    if POOLS_OF_SERIES_X.load(Ordering::Relaxed) {
        Console::SeriesX
    } else {
        current()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_pools_follow_the_device_unless_sized_as_a_series_x() {
        set(Console::SeriesS);
        assert_eq!(pools(), Console::SeriesS);
        set_pools_of_series_x(true);
        assert_eq!(pools(), Console::SeriesX);
        assert_eq!(current(), Console::SeriesS, "the device stays what it was");
        set_pools_of_series_x(false);
        assert_eq!(pools(), Console::SeriesS);
    }
}
