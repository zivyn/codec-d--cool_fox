//! The waits for every earlier shader write that WE add to the card's queue
//! (not the title's own: those have their line, `waits for every earlier shader write`), counted by where they
//! are recorded. Printed with the card's stamps (`VICTORY_CARD_SPANS=1` or `2`): each is a drain of the whole
//! queue (the last waves of what is running, then the ramp up of what follows), so their number times a drain's
//! cost bounds what our barriers cost on the card, and the count of a site says what to attack.

use std::sync::atomic::{AtomicU64, Ordering};

/// Where one of our full waits is recorded.
#[derive(Clone, Copy)]
pub(crate) enum Site {
    /// The first level of the card steps list (what earlier lists' shaders wrote, first).
    Steps,
    /// The compare list behind a submission's lists.
    Compare,
    /// An indirect clamp or the batch pass: the waits around it (one per call of the clamp's barrier).
    Clamp,
    /// A title buffer copy done at its place in a list (`place.rs`: before and after it).
    Place,
    /// The ambient stand-in's passes.
    Ambient,
}

const SITES: usize = 5;

/// What each site's name is, in the order of [`Site`].
const NAMES: [&str; SITES] = ["card steps lists", "compare lists", "indirect clamps and batch passes", "title copies at their place", "ambient stand-in"];

static COUNTS: [AtomicU64; SITES] = [const { AtomicU64::new(0) }; SITES];

/// One full wait of ours is recorded at `site`.
pub(crate) fn drain(site: Site) {
    COUNTS[site as usize].fetch_add(1, Ordering::Relaxed);
}

/// The counts since the last call, and what they are called.
pub(super) fn take() -> [(&'static str, u64); SITES] {
    std::array::from_fn(|i| (NAMES[i], COUNTS[i].swap(0, Ordering::Relaxed)))
}
