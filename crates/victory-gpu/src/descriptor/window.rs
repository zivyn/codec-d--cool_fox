//! The host's side of the windows of buffer views (`VICTORY_BINDLESS_HOST`; the shaders' side is
//! `victory_shader::emit` `bindless.rs`, `VICTORY_BINDLESS_BUFFERS`).
//!
//! A buffer view of a table was a descriptor the host made for the table entry and copied into the heap.
//! The window of the entry says the same in sixteen bytes of a table the shaders read: the number of the
//! host copy the view lies in, the byte of the copy the view begins at, its length. Every host copy has
//! one raw view in the heap for good (a run of slots, copy `n` at `n` past the run's first), so what is
//! made per binding is the window.
//!
//! The levels of `VICTORY_BINDLESS_HOST`:
//! - 0 (default): the table is there (its slots all say "no window") for shaders translated with the
//!   switch, and nothing is written.
//! - 1: the window is written beside each buffer view the host makes, wherever the raw twin of the view
//!   is (the view's own range: a view that does not begin on 16 bytes has none). The views are made as
//!   before, so a program reads the same either way and the picture is the check.
//! - 2: also, a table made for a draw does not make the views of its entries that the draw's program
//!   reads through windows alone (`HostShader::bindless`): it writes the window and leaves the slot's
//!   descriptor. No view is created and no descriptor copied for them.
//!
//! What the windows rest on: a copy that has a number is held for good (the view of it in the heap is
//! made once, and a window names it by number), at most `WINDOW_COPIES` (`victory_shader::emit`) of them; one more finds no number
//! and its views are made as before.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::shadow::Shadow;

/// `VICTORY_BINDLESS_HOST`: the level (see the module).
pub(crate) fn level() -> u8 {
    fn mode(text: Option<&str>) -> u8 {
        match text {
            Some("1") => 1,
            Some("2") => 2,
            _ => 0,
        }
    }
    crate::ab::live_value!("VICTORY_BINDLESS_HOST", u8, mode)
}

/// Whether a table of windows is made for the view heap: shaders translated with
/// `VICTORY_BINDLESS_BUFFERS` bind it, whatever the level.
pub(crate) fn table_wanted() -> bool {
    level() > 0 || victory_shader::emit::bindless_buffers()
}

/// A buffer view's range in a host copy: the copy, and the bytes it begins and ends at in it.
pub(crate) type Noted = (Arc<Shadow>, u64, u64);

thread_local! {
    /// What the buffer view this thread made last shows ([`note`]).
    static NOTED: RefCell<Option<Noted>> = const { RefCell::new(None) };
    /// The view this thread is asked for is wanted as a window only ([`set_only`]).
    static ONLY: Cell<bool> = const { Cell::new(false) };
}

/// Forgets what was noted.
pub(crate) fn clear() {
    NOTED.with(|n| *n.borrow_mut() = None);
}

/// A buffer view was made (or, with [`only`], found) that shows bytes `start..end` of `shadow`, as its raw
/// twin does.
pub(crate) fn note(shadow: &Arc<Shadow>, start: u64, end: u64) {
    if level() > 0 {
        NOTED.with(|n| *n.borrow_mut() = Some((shadow.clone(), start, end)));
    }
}

/// What was noted since [`clear`].
pub(crate) fn take() -> Option<Noted> {
    NOTED.with(|n| n.borrow_mut().take())
}

/// Whether the view being made is wanted as a window only: nothing is created, the range is noted.
pub(crate) fn only() -> bool {
    ONLY.with(Cell::get)
}

pub(crate) fn set_only(only: bool) {
    ONLY.with(|o| o.set(only));
}

/// Windows written, views left unmade, views refused a window for want of a number, and views of
/// buffers and of other kinds made by the lists, since the last frame line.
pub(crate) static COUNTS: [AtomicU64; 6] = [const { AtomicU64::new(0) }; 6];

pub(crate) const WRITTEN: usize = 0;
pub(crate) const SKIPPED: usize = 1;
pub(crate) const NO_NUMBER: usize = 2;
pub(crate) const BUFFERS_MADE: usize = 3;
pub(crate) const OTHERS_MADE: usize = 4;
pub(crate) const FELL_BACK: usize = 5;

pub(crate) fn count(which: usize) {
    COUNTS[which].fetch_add(1, Ordering::Relaxed);
}

/// With the frame line: what the windows did per frame over `frames`.
pub(crate) fn line(frames: f64) {
    let [written, skipped, none, buffers, others, fell] =
        [0, 1, 2, 3, 4, 5].map(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64);
    if written + skipped + none + buffers + others + fell == 0.0 {
        return;
    }
    tracing::info!(
        "views of lists' tables made per frame: {:.0} of buffers, {:.0} of other kinds; windows of buffer views (VICTORY_BINDLESS_HOST={}): {:.0} written, {:.0} views left unmade for them, {:.0} asked for with no view heap number left, {:.0} asked to be left unmade that had to be made",
        buffers / frames,
        others / frames,
        level(),
        written / frames,
        skipped / frames,
        none / frames,
        fell / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_view_is_asked_for_as_a_window_only_until_it_is_not() {
        clear();
        assert!(take().is_none());
        assert!(!only());
        set_only(true);
        assert!(only());
        set_only(false);
        assert!(!only());
    }
}
