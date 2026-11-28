//! The hook of `VICTORY_PRECISE_BARRIERS`: the title's waits for shader writes
//! (barrier groups, pipeline syncs) are held by [`crate::lazy_wait::Lazy`] until
//! a command needs them.
//!
//! Only lists written down by the thin front ([`super::thin`]) are held; they
//! are translated one call at a time in order, and that loop is the one place
//! every call passes. Each call is given a [`Class`] by its op code, and what is
//! not named is [`Class::Opaque`]: it conflicts with everything before a wait,
//! so a call this hook has never heard of forces the wait out before it. Direct
//! dispatches are [`Class::Dispatch`]: `State::dispatch` gives the accesses of
//! the shaders it bound. The end of a list puts out a wait still held.
//!
//! Off, or on a list that is not thin, every wait is made where the title put
//! it, as before. Switched on or off while the title runs (`VICTORY_AB`): a
//! list that has a wait held when the switch goes off puts it out at the start
//! of its next stream.

use super::{List, This, host_list};
use crate::host::precise::Access;
use crate::lazy_wait::Lazy;

/// What a call of the title does to the waits that are held.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Sets state; records no work that reads or writes memory.
    Quiet,
    /// A wait for every earlier shader write.
    Wait,
    /// A direct dispatch: the accesses of its shaders are known.
    Dispatch,
    /// Anything else that records work.
    Opaque,
    /// The list is closed.
    Close,
}

/// Whether the waits are held (`VICTORY_PRECISE_BARRIERS=1`).
pub(super) fn on() -> bool {
    crate::ab::live_flag!("VICTORY_PRECISE_BARRIERS")
}

/// What `class` does to `lazy`; whether a wait goes in front of the call.
pub(crate) fn feed(lazy: &mut Lazy, class: Class, access: Option<&Access>) -> bool {
    match class {
        Class::Quiet => false,
        Class::Wait => {
            lazy.title_wait();
            false
        }
        Class::Dispatch => match access {
            Some(access) => lazy.command(access),
            None => lazy.opaque(),
        },
        Class::Opaque => lazy.opaque(),
        Class::Close => lazy.close(),
    }
}

/// Runs `f` on the list's host side and state; a wait goes out if it says so.
fn with(list: This, f: impl FnOnce(&mut crate::draw::State) -> bool) {
    let Some(host) = host_list(list) else {
        return;
    };
    // SAFETY: a list of ours that no thread but this one records into.
    let state = unsafe { &(*list.cast::<List>()).state };
    let emit = f(&mut state.lock().unwrap_or_else(|e| e.into_inner()));
    if emit {
        super::emit_uav_wait(host);
    }
}

/// A stream of the list begins: held, or (the switch has gone off) put out.
pub(super) fn begin(list: This, held: bool) {
    with(list, |s| s.lazy_begin(held));
}

/// A call of `class` is about to be made.
pub(super) fn op(list: This, class: Class) {
    with(list, |s| {
        if class == Class::Wait {
            // (What the title's wait always closed: the batch of clamped records.)
            s.synced();
        }
        s.lazy_op(class, None)
    });
}
