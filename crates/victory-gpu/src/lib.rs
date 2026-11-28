//! D3D12.X on host D3D12.
//!
//! D3D12.X records commands from code inlined into the title, which writes
//! packets into the command list while its put pointer is below a limit and
//! calls the list's method otherwise. Our lists keep the limits at zero, so
//! every command arrives here as a call, to be run on host D3D12. The
//! title's GPU addresses are CPU addresses (console memory is unified).
//!
//! [`device`] is the device object titles create, [`dxgi`] its adapter side,
//! [`resource`] heaps and resources, [`fence`] fences, [`queue`] command
//! queues, [`list`] command allocators and lists, [`descriptor`]
//! descriptor heaps, [`root`] root signatures, [`query`] query heaps,
//! [`signature`] command signatures, [`pipeline`] pipeline states,
//! [`frame`] frame pacing, [`object`] what all console graphics objects
//! share, [`console`] which console model the title sees. Planned:
//! `present` (swap chain).

#[cfg(windows)]
mod ambient;
#[cfg(windows)]
mod bufscan;
mod asked;
mod changes;
pub mod defaults;
#[cfg(all(windows, feature = "diagnostics"))]
mod capture;
#[cfg(all(windows, feature = "diagnostics"))]
mod probe_watch;
#[cfg(any(test, all(windows, feature = "diagnostics")))]
mod capture_window;
#[cfg(windows)]
mod compose;
pub mod console;
#[cfg(all(windows, feature = "diagnostics"))]
mod copystat;
#[cfg(all(windows, feature = "diagnostics"))]
mod faultbook;
#[cfg(windows)]
mod cores;
pub mod descriptor;
pub mod device;
#[cfg(windows)]
mod draw;
pub mod dxgi;
#[cfg(windows)]
mod external;
pub mod fence;
pub mod frame;
#[cfg(all(windows, feature = "diagnostics"))]
mod book;
#[cfg(windows)]
mod host;
#[cfg(windows)]
mod indirect;
#[cfg(windows)]
mod inplace;
#[cfg(windows)]
mod land;
#[cfg(windows)]
mod lean;
pub mod list;
pub mod object;
#[cfg_attr(not(windows), allow(dead_code))]
mod occlusion;
pub mod pipeline;
mod precompiled;
#[cfg(windows)]
mod picked;
#[cfg(windows)]
mod pool;
#[cfg(all(windows, feature = "diagnostics"))]
mod popwatch;
#[cfg(windows)]
mod primitive;
#[cfg(windows)]
mod barrier;
mod precise;
#[cfg(windows)]
mod lazy_wait;
#[cfg(windows)]
mod priority;
#[cfg(windows)]
mod vram;
pub mod query;
pub mod queue;
#[cfg(all(windows, feature = "diagnostics"))]
pub mod replay;
#[cfg(windows)]
mod residency;
pub mod resource;
pub mod root;
#[cfg(windows)]
mod safecopy;
#[cfg(windows)]
mod sampler;
#[cfg(windows)]
pub mod shader_cache;
#[cfg(windows)]
mod memory;
#[cfg(windows)]
mod sizes;
#[cfg(windows)]
mod pagedrive;
#[cfg(windows)]
mod propcount;
#[cfg(windows)]
mod shadow;
#[cfg(windows)]
mod standin;
#[cfg(windows)]
mod sites;
#[cfg(windows)]
mod tiles;
pub mod signature;
#[cfg(windows)]
pub mod streamline;
#[cfg(windows)]
mod hotkeys;
mod switch;
pub use switch::on as env_on;
#[cfg(windows)]
pub mod ab;
mod opcost;
mod viewat;
#[cfg(windows)]
mod echo;
#[cfg(windows)]
mod tdr;
#[cfg(windows)]
mod texture;
pub mod timeline;
#[cfg(windows)]
pub mod titlewrite;
#[cfg(windows)]
mod upload;
#[cfg(windows)]
mod threadcpu;
#[cfg(windows)]
mod sched;
#[cfg(windows)]
mod readprobe;
#[cfg(windows)]
mod watch;
#[cfg(windows)]
mod texback;

#[cfg(windows)]
pub use host::on_failure;

/// Whether `switch` is on now, if it is one of those `VICTORY_AB` turns on and
/// off while the title runs (`ab`).
#[cfg(all(windows, feature = "diagnostics"))]
pub fn switched(switch: &str) -> Option<bool> {
    ab::pick(switch)
}

/// Whether `VICTORY_WAIT_NICE` is on now (`VICTORY_AB` may flip it while the title runs): the loader asks, at each pass of
/// a wait loop of the title's (`victory_loader::wait_nice_from`).
#[cfg(all(windows, feature = "diagnostics"))]
pub fn wait_nice_now() -> bool {
    ab::live_flag!("VICTORY_WAIT_NICE")
}

/// What the frame is at (`copystat::phase`), for the sampling profiler.
#[cfg(all(windows, feature = "diagnostics"))]
pub fn frame_phase() -> u32 {
    copystat::phase()
}
#[cfg(windows)]
pub mod addrset;
#[cfg(windows)]
pub use inplace::{title_memory_freeing, title_memory_section, title_memory_section_gone, title_memory_view};
#[cfg(windows)]
pub use shadow::{title_memory_gpu_readonly, title_memory_gpu_writable};
#[cfg(windows)]
pub use watch::mapping_stats;
#[cfg(windows)]
pub use watch::{
    Remapping, kernel_writing, title_memory_allocated, title_memory_freed, title_memory_reading, title_memory_remapped,
    title_memory_remapping, title_memory_splitting, title_memory_unmapping, title_memory_unpin_wait, title_memory_written,
    writing as kernel_will_write,
};
