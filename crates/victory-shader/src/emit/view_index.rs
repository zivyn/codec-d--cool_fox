//! The title's root SRVs read through the view heap (`VICTORY_VIEW_INDEX=1`,
//! also set by `VICTORY_FOLD_VIEWS=1`).
//!
//! A root SRV of the title (a buffer by address, which the host binds as a
//! raw view of its copy of the buffer from the address to the copy's end)
//! was a one-view descriptor table of the host's root signature: the
//! shader's `t<slot>` in [`super::shader::ROOT_CBV_SPACE`]. Which view the
//! table holds changes from one object to the next, and a command
//! signature cannot set a descriptor table, so two executions that differ
//! in one of these views could not be one `ExecuteIndirect`
//! (`victory-gpu`'s `draw::fold`).
//!
//! With the switch on the shader takes the view from the view heap by an
//! index it reads from the user data (`ResourceDescriptorHeap[index]`): the
//! same one-view slot the table pointed at, with the same size, so a read
//! past the copy's end still returns zeros and the title's behaviour does
//! not change. What differs between two objects is then one more dword of
//! the user data, which a command signature sets with the others. The
//! shader declares no root SRV for the slot, so the host root signature
//! has no table for it either.
//!
//! The switch is read once, when a program is translated, and is part of
//! the shader cache's key (the build script lists every `VICTORY_*` name
//! this crate reads). A program translated with it off is what it always
//! was; the host takes each program as it finds it.

use super::super::shader::USER_ROOT_VIEW_INDEX;
use super::{Lower, ResourceType, RootType, V, flags};

#[cfg(test)]
thread_local! {
    /// A test's choice of the switch, for its thread only.
    pub(in crate::emit) static TEST_VIEW_INDEX: std::cell::Cell<Option<bool>> = const { std::cell::Cell::new(None) };
}

/// Whether the title's root SRVs are read through the view heap
/// (`VICTORY_VIEW_INDEX=1` or `VICTORY_FOLD_VIEWS=1`).
pub(super) fn on() -> bool {
    #[cfg(test)]
    if let Some(on) = TEST_VIEW_INDEX.get() {
        return on;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        ["VICTORY_VIEW_INDEX", "VICTORY_FOLD_VIEWS"]
            .into_iter()
            .any(|name| std::env::var_os(name).is_some_and(|v| v == "1"))
    })
}

impl Lower<'_> {
    /// The raw buffer of the title's root SRV of root parameter `slot`: its
    /// table's view of the host, or, with the switch on, the view at the
    /// heap slot the host puts in the user data (`USER_ROOT_VIEW_INDEX`).
    /// (A program with a spilled user-data table keeps the table: it reads
    /// title memory by address and is no run of one-view draws.)
    pub(super) fn root_srv(&mut self, slot: u8) -> V {
        if !on() || crate::spill::is_spilled(self.x.native) {
            return self.s.root_srv_handle(slot);
        }
        if let Some(&(_, h)) = self.s.root_heap_srvs.iter().find(|r| r.0 == slot) {
            return h;
        }
        let index = self.host_word(RootType::Other(USER_ROOT_VIEW_INDEX), slot, 0);
        let entry = self.s.entry;
        self.s.flags |= flags::RAW_BUFFERS;
        // (Raw buffer, no flags: as the root SRV's own handle is annotated.)
        let h = self
            .s
            .heap_handle_at(entry, index, false, false, (11, 0), false);
        self.s.root_heap_srvs.push((slot, h));
        h
    }

    /// For a root view the host binds as a raw view that may begin before
    /// the title's address (`shader.rs` `USER_VIEW_BIAS`): the bytes to add
    /// to offsets through it. Every root UAV (root SRVs keep the views they
    /// have).
    pub(super) fn root_bias(&mut self, param: usize) -> Option<V> {
        use super::super::shader::USER_VIEW_BIAS;
        let rp = *self.x.native.layout.get(param)?;
        // (Title memory by address where the user-data table is spilled:
        // [`Lower::root_memory`].)
        if rp.root != RootType::Descriptor || crate::spill::is_spilled(self.x.native) {
            return None;
        }
        let kind = self
            .x
            .native
            .used
            .iter()
            .find(|u| u.user_data == rp.user_data)
            .map(|u| u.kind);
        (kind == Some(ResourceType::Uav))
            .then(|| self.host_word(RootType::Other(USER_VIEW_BIAS), rp.slot, 0))
    }
}
