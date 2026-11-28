//! Host work that a host library records into a title's command list in
//! place of a console one (DLSS in place of the title's FidelityFX
//! upscaling, `crate::streamline`): the title textures' host copies in the
//! states the library is told, prepared as a compute shader's inputs would
//! be, and what was written.
//!
//! Ported from DexNom's fork (DexNom/VIctory, `crates/victory-gpu/src/external.rs`,
//! GPL-3.0-or-later); the work runs where the title's list is translated
//! (`crate::list::external`, also with `VICTORY_THIN=1`).

use windows::Win32::Graphics::Direct3D12::ID3D12Resource;

use crate::host::HostList;
use crate::object::This;

/// A title texture's host copy, moved to the host resource `state` in
/// `list`, and the state it is in: `state` without the bits the host copy
/// cannot take (see `host::legal_state`). What the list records next finds
/// it there and must leave it there. The list holds the texture until it is
/// reset. `None` for a buffer.
pub(crate) fn host_texture(list: &HostList, resource: This, state: u32) -> Option<(ID3D12Resource, u32)> {
    let texture = crate::resource::host_resource(resource)?.clone();
    crate::list::ensure_state(list, resource, state);
    let state = crate::host::legal_state(&texture, state);
    Some((texture, state))
}

/// Prepares a title-authored input with the alias, upload and depth twin
/// rules a compute shader's resource view has in the renderer.
pub(crate) fn prepare_input(list: &HostList, resource: This) -> Option<()> {
    let texture = crate::resource::host_resource(resource)?;
    let aliased = crate::resource::alias_of(resource).is_some_and(|source| crate::texture::alias(list, resource, source.get()));
    if !aliased && crate::texture::title_written(texture) {
        crate::texture::upload_from(list, resource, 0);
    }
    list.twin_use(resource, false, false);
    Some(())
}

/// The library wrote `resource` on the host: its contents are there, not in
/// title memory.
pub(crate) fn output_written(resource: This) {
    crate::texture::host_written(resource);
}

/// Sends what the title submitted (the line of submissions first) and
/// settles it, before a library lets go of resources the GPU may still use.
pub(crate) fn finish_submitted() {
    crate::queue::before_sync();
    crate::host::settle_for_fence();
}
