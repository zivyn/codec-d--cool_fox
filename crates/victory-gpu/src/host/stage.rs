//! The bus leg of a submission's sends on a copy queue (`VICTORY_SEND_COPYQ=1`).
//!
//! What a submission sends up (`send::take`) is bytes in upload memory, which
//! the card's drawing queue copies into the copies in its own memory: 17-31
//! MB a frame over a bus of ~6.5 GB/s, 3-5 ms in which that queue does
//! nothing else (a frame captured with the sends in video memory
//! (`VICTORY_SEND_BAR`) showed 5.4 ms of 30 for 342 copies, 0.4 ms after).
//! Writing them into video memory from the CPU instead costs the submitting
//! thread 5 ms more (`send::on_card`), so the bytes stay in upload memory.
//!
//! Here the bus leg is one copy on a copy queue of its own, from the
//! submission's upload memory into a buffer in video memory of the same
//! layout (a stage), started as soon as the bytes are taken. The submitting
//! thread then records the sync list and the rest of its preparation while
//! the bytes cross, and the drawing queue waits on the copy's fence value
//! before the sync list, whose copies and scatters read the stage (video
//! memory to video memory) instead of upload memory.
//!
//! Why this is ordered like what it replaces, with nothing to prove about
//! what flights in the air read:
//! - The copy queue touches only the stage. The copies in the card's memory
//!   (`Shadow::buffer`), their states and their order against the flights
//!   are those of the drawing queue as before: the same list, in the same
//!   place of the queue, with the same barriers.
//! - A stage is written by the copy queue only while it is out of the pool,
//!   and goes back to the pool when the submission that uses it has run and
//!   its plan is done (`send::give_back`), which is after the drawing queue
//!   waited for the copy and read the stage. So the copy queue never writes
//!   a stage the drawing queue may still read, and the upload memory is not
//!   given back (`send::give_back`) before the copy that reads it is done.
//! - Fence values rise in the order the copies are made (one lock across
//!   record, execute and signal), and a wait for a value is a wait for every
//!   copy before it.
//! - A stage is in the common state whenever a queue takes it over: the
//!   copy queue promotes it to a copy destination and it decays; the drawing
//!   queue moves it to the read states explicitly and back to the common
//!   state at the end of the sends (`send::Own::record`).
//! - Nothing here changes what is sent. A submission that takes no stage
//!   (switch off, too little to pay for a fence, no room under the cap, any
//!   failure) sends from upload memory as before, so the switch may change
//!   at any instant: each submission decides once, when its bytes are taken.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use super::*;

/// Whether sends go over the bus on a copy queue (`VICTORY_SEND_COPYQ=1`,
/// off by default; may be named by `VICTORY_AB`).
pub(super) fn on() -> bool {
    crate::ab::live_flag!("VICTORY_SEND_COPYQ")
}

/// The least a submission sends for a stage (`VICTORY_SEND_COPYQ_MIN_KB`,
/// 512 KiB): a copy of less does not pay for a fence and two more calls.
fn least() -> u64 {
    static LEAST: OnceLock<u64> = OnceLock::new();
    *LEAST.get_or_init(|| {
        std::env::var("VICTORY_SEND_COPYQ_MIN_KB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(512)
            << 10
    })
}

/// The most video memory the stages take together (`VICTORY_SEND_COPYQ_MB`,
/// 64): the card sits over its budget, so the stages are few.
fn cap() -> u64 {
    static CAP: OnceLock<u64> = OnceLock::new();
    *CAP.get_or_init(|| {
        std::env::var("VICTORY_SEND_COPYQ_MB")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .unwrap_or(64)
            << 20
    })
}

/// Since the last frame-rate line: submissions that took a stage, the bytes
/// they sent over the copy queue, those left out for sending too little,
/// for want of room under the cap, and for a failure.
pub(super) static STATS: [AtomicU64; 5] = [const { AtomicU64::new(0) }; 5];

/// The copy queue, its fence, and the last value signalled on it.
pub(super) struct Lane {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    /// Held across recording, executing and signalling a copy, so that the
    /// values rise in the order the copies are on the queue.
    last: Mutex<u64>,
}

// SAFETY: D3D12 queues and fences are free-threaded.
unsafe impl Send for Lane {}
// SAFETY: as above.
unsafe impl Sync for Lane {}

/// A buffer in video memory a submission's bytes cross into, with the list
/// of its copy (the list is not reset before the stage comes back).
pub(super) struct Stage {
    vram: ID3D12Resource,
    size: u64,
    allocator: ID3D12CommandAllocator,
    list: ID3D12GraphicsCommandList,
}

// SAFETY: D3D12 objects are free-threaded; a stage has one owner at a time.
unsafe impl Send for Stage {}

/// A stage with the copy into it on the queue, and the value signalled
/// behind that copy.
pub(super) struct Staged {
    stage: Stage,
    value: u64,
}

impl Staged {
    /// The buffer the sends read.
    pub(super) fn buffer(&self) -> &ID3D12Resource {
        &self.stage.vram
    }

    /// The value of the copy queue's fence the drawing queue waits for.
    #[cfg(test)]
    pub(super) fn value(&self) -> u64 {
        self.value
    }

    /// The drawing queue stands behind the copy.
    pub(super) fn wait(&self, direct: &ID3D12CommandQueue) -> windows::core::Result<()> {
        match lane() {
            // SAFETY: the queue's own wait on a fence of ours.
            Some(lane) => unsafe { direct.Wait(&lane.fence, self.value) },
            None => Ok(()),
        }
    }

    /// The submission has run: the stage may be written again.
    pub(super) fn give_back(self) {
        POOL.lock()
            .unwrap_or_else(|e| e.into_inner())
            .give(self.stage);
    }
}

impl Lane {
    fn new(device: &ID3D12Device) -> Option<Lane> {
        // SAFETY: plain object creation.
        unsafe {
            let queue: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_COPY,
                    Priority: D3D12_COMMAND_QUEUE_PRIORITY_HIGH.0,
                    ..Default::default()
                })
                .ok()?;
            let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).ok()?;
            Some(Lane {
                queue,
                fence,
                last: Mutex::new(0),
            })
        }
    }

    /// Copies `bytes` of `from` (upload memory) to the start of `stage` and
    /// returns the value signalled behind the copy.
    fn send(&self, stage: &Stage, from: &ID3D12Resource, bytes: u64) -> Option<u64> {
        let mut last = self.last.lock().unwrap_or_else(|e| e.into_inner());
        // SAFETY: the stage's list and allocator last ran before the stage
        // came back to the pool (the drawing queue waited for that copy);
        // live buffers of at least `bytes`.
        unsafe {
            stage.allocator.Reset().ok()?;
            stage.list.Reset(&stage.allocator, None).ok()?;
            stage.list.CopyBufferRegion(&stage.vram, 0, from, 0, bytes);
            stage.list.Close().ok()?;
            let lists = [Some(stage.list.cast::<ID3D12CommandList>().ok()?)];
            self.queue.ExecuteCommandLists(&lists);
            let value = *last + 1;
            self.queue.Signal(&self.fence, value).ok()?;
            *last = value;
            Some(value)
        }
    }
}

/// The lane, made at first use on the host's device; `None` if the system
/// gives no copy queue.
fn lane() -> Option<&'static Lane> {
    static LANE: OnceLock<Option<Lane>> = OnceLock::new();
    LANE.get_or_init(|| {
        let host = crate::host::host()?;
        let lane = Lane::new(&host.device);
        if lane.is_none() {
            tracing::warn!(
                "no copy queue: sends cross the bus on the drawing queue (VICTORY_SEND_COPYQ=1)"
            );
        }
        lane
    })
    .as_ref()
}

impl Stage {
    fn new(device: &ID3D12Device, size: u64) -> Option<Stage> {
        // SAFETY: plain object creation.
        unsafe {
            let mut made: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_DEFAULT,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: size,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    D3D12_RESOURCE_STATE_COMMON,
                    None,
                    &mut made,
                )
                .ok()?;
            let vram = made?;
            name(&vram, || {
                "bytes a submission sends up, crossing on the copy queue".into()
            });
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_COPY)
                .ok()?;
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_COPY, &allocator, None)
                .ok()?;
            list.Close().ok()?;
            Some(Stage {
                vram,
                size,
                allocator,
                list,
            })
        }
    }
}

/// What the pool does for a request.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum Pick {
    /// The idle stage at this index.
    Reuse(usize),
    /// Make one of `size`, after letting go of the `free` smallest idle ones.
    Make { size: u64, free: usize },
    /// No room: what is out and what is needed do not fit the cap.
    No,
}

/// The size a stage for `need` bytes is made with: a power of two.
pub(super) fn size_for(need: u64) -> u64 {
    need.next_power_of_two().max(64 << 10)
}

/// The choice, from the sizes of the idle stages (ascending), the sum of all
/// stages made and not let go of, the bytes needed and the cap.
pub(super) fn pick(idle: &[u64], total: u64, need: u64, cap: u64) -> Pick {
    if let Some(i) = idle.iter().position(|&s| s >= need) {
        return Pick::Reuse(i);
    }
    let size = size_for(need);
    // Idle ones are all smaller than `need`: they are let go of, smallest
    // first, until the new one fits.
    let (mut total, mut free) = (total, 0);
    while total + size > cap && free < idle.len() {
        total -= idle[free];
        free += 1;
    }
    if total + size > cap {
        Pick::No
    } else {
        Pick::Make { size, free }
    }
}

struct Pool {
    /// Idle stages, ascending by size.
    idle: Vec<Stage>,
    /// The sizes of all stages made and not let go of (those out, too).
    total: u64,
}

static POOL: Mutex<Pool> = Mutex::new(Pool {
    idle: Vec::new(),
    total: 0,
});

impl Pool {
    fn take(&mut self, device: &ID3D12Device, need: u64, cap: u64) -> Option<Stage> {
        let sizes: Vec<u64> = self.idle.iter().map(|s| s.size).collect();
        match pick(&sizes, self.total, need, cap) {
            Pick::Reuse(i) => Some(self.idle.remove(i)),
            Pick::Make { size, free } => {
                for gone in self.idle.drain(..free) {
                    self.total -= gone.size;
                }
                let made = Stage::new(device, size)?;
                self.total += size;
                Some(made)
            }
            Pick::No => None,
        }
    }

    fn give(&mut self, stage: Stage) {
        let at = self.idle.partition_point(|s| s.size <= stage.size);
        self.idle.insert(at, stage);
    }
}

/// Starts the copy of `bytes` of `from` (a submission's upload memory) to a
/// stage and returns it, or `None` where the sends go as before.
pub(super) fn stage(device: &ID3D12Device, from: &ID3D12Resource, bytes: u64) -> Option<Staged> {
    if bytes < least() {
        STATS[2].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let lane = lane()?;
    let taken = POOL
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take(device, bytes, cap());
    let Some(stage) = taken else {
        STATS[3].fetch_add(1, Ordering::Relaxed);
        return None;
    };
    match lane.send(&stage, from, bytes) {
        Some(value) => {
            STATS[0].fetch_add(1, Ordering::Relaxed);
            STATS[1].fetch_add(bytes, Ordering::Relaxed);
            Some(Staged { stage, value })
        }
        None => {
            // What the queue may still hold of it is not known: the stage
            // is kept alive and never used again.
            STATS[4].fetch_add(1, Ordering::Relaxed);
            std::mem::forget(stage);
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use windows::Win32::Foundation::WAIT_OBJECT_0;

    const MB: u64 = 1 << 20;

    #[test]
    fn a_stage_is_a_power_of_two_with_a_floor() {
        assert_eq!(size_for(1), 64 << 10);
        assert_eq!(size_for(600 << 10), MB);
        assert_eq!(size_for(MB), MB);
        assert_eq!(size_for(MB + 1), 2 * MB);
    }

    #[test]
    fn the_smallest_idle_stage_that_fits_is_reused() {
        assert_eq!(
            pick(&[MB, 2 * MB, 8 * MB], 11 * MB, MB + 1, 64 * MB),
            Pick::Reuse(1)
        );
        assert_eq!(pick(&[MB, 2 * MB], 3 * MB, MB, 64 * MB), Pick::Reuse(0));
    }

    #[test]
    fn a_new_one_is_made_while_there_is_room_under_the_cap() {
        assert_eq!(
            pick(&[], 40 * MB, 3 * MB, 64 * MB),
            Pick::Make {
                size: 4 * MB,
                free: 0
            }
        );
        assert_eq!(
            pick(&[MB], 40 * MB, 3 * MB, 64 * MB),
            Pick::Make {
                size: 4 * MB,
                free: 0
            }
        );
    }

    #[test]
    fn small_idle_ones_are_let_go_of_to_make_room_for_a_larger() {
        // 60 MB made, 3 idle (1, 2, 4 MB), 16 MB wanted: 60 + 16 > 64; letting
        // go of 1 and 2 gives 57 + 16 > 64, of 4 as well 53 + 16 > 64: no room.
        assert_eq!(
            pick(&[MB, 2 * MB, 4 * MB], 60 * MB, 9 * MB, 64 * MB),
            Pick::No
        );
        // The same with 40 MB made: 40 + 16 fits at once.
        assert_eq!(
            pick(&[MB, 2 * MB, 4 * MB], 40 * MB, 9 * MB, 64 * MB),
            Pick::Make {
                size: 16 * MB,
                free: 0
            }
        );
        // 52 made: 52 + 16 > 64; without the 1 and 2 MB ones 49 + 16 > 64;
        // with the 4 MB one gone too, 45 + 16 fits.
        assert_eq!(
            pick(&[MB, 2 * MB, 4 * MB], 52 * MB, 9 * MB, 64 * MB),
            Pick::Make {
                size: 16 * MB,
                free: 3
            }
        );
    }

    #[test]
    fn what_is_out_is_never_let_go_of() {
        // All 64 MB are out (no idle ones): nothing can be made.
        assert_eq!(pick(&[], 64 * MB, MB, 64 * MB), Pick::No);
        // A stage larger than the cap is never made.
        assert_eq!(pick(&[], 0, 65 * MB, 64 * MB), Pick::No);
    }

    #[test]
    fn the_pool_keeps_idle_stages_in_ascending_order_of_size() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        let mut pool = Pool {
            idle: Vec::new(),
            total: 0,
        };
        // All three out at once, so that none is reused for another.
        let out: Vec<Stage> = [2 * MB, MB, 4 * MB]
            .into_iter()
            .map(|size| {
                let stage = pool.take(&device, size, 64 * MB).expect("a stage");
                assert_eq!(stage.size, size);
                stage
            })
            .collect();
        for stage in out {
            pool.give(stage);
        }
        let sizes: Vec<u64> = pool.idle.iter().map(|s| s.size).collect();
        assert_eq!(sizes, [MB, 2 * MB, 4 * MB]);
        assert_eq!(pool.total, 7 * MB);
        let again = pool.take(&device, MB + 1, 64 * MB).expect("the 2 MB one");
        assert_eq!(again.size, 2 * MB);
        assert_eq!(pool.total, 7 * MB, "reused, none made");
    }

    fn warp() -> Option<ID3D12Device> {
        use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
        // SAFETY: plain DXGI queries and device creation on the software
        // adapter; a card is never opened.
        unsafe {
            let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).ok()?;
            let adapter: IDXGIAdapter = factory.EnumWarpAdapter().ok()?;
            let desc = adapter.cast::<IDXGIAdapter1>().ok()?.GetDesc1().ok()?;
            if desc.Flags & DXGI_ADAPTER_FLAG_SOFTWARE.0 as u32 == 0 {
                return None;
            }
            let mut device: Option<ID3D12Device> = None;
            D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device).ok()?;
            device
        }
    }

    fn buffer(
        device: &ID3D12Device,
        kind: D3D12_HEAP_TYPE,
        state: D3D12_RESOURCE_STATES,
        size: u64,
    ) -> ID3D12Resource {
        // SAFETY: plain object creation.
        unsafe {
            let mut made: Option<ID3D12Resource> = None;
            device
                .CreateCommittedResource(
                    &D3D12_HEAP_PROPERTIES {
                        Type: kind,
                        ..Default::default()
                    },
                    D3D12_HEAP_FLAG_NONE,
                    &D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: size,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    },
                    state,
                    None,
                    &mut made,
                )
                .expect("a buffer");
            made.expect("a buffer")
        }
    }

    /// Waits for `value` on `fence`, up to ten seconds.
    fn reached(fence: &ID3D12Fence, value: u64) -> bool {
        // SAFETY: an event of the test's own.
        unsafe {
            if fence.GetCompletedValue() >= value {
                return true;
            }
            let event = CreateEventW(None, false, false, None).expect("an event");
            fence
                .SetEventOnCompletion(value, event)
                .expect("an event on the fence");
            WaitForSingleObject(event, 10_000) == WAIT_OBJECT_0
        }
    }

    /// The whole path on the software adapter: bytes in upload memory cross
    /// on the copy queue into stages, the drawing queue waits for the last
    /// copy, moves the stages to the read states, copies them on to a
    /// readback buffer and puts them back in the common state; two rounds,
    /// the second on stages that came back.
    #[test]
    fn bytes_cross_on_the_copy_queue_and_are_read_by_the_drawing_queue_after_its_wait() {
        let _turn = crate::indirect::tests::warp_turn();
        let Some(device) = warp() else { return };
        // SAFETY: plain object creation and use of objects of the test's own;
        // the mapped memory is the test's alone and large enough.
        unsafe {
            let lane = Lane::new(&device).expect("a copy queue");
            let direct: ID3D12CommandQueue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .expect("a queue");
            let direct_fence: ID3D12Fence = device
                .CreateFence(0, D3D12_FENCE_FLAG_NONE)
                .expect("a fence");
            let allocator: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .expect("an allocator");
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
                .expect("a list");
            list.Close().expect("closed");
            let mut pool = Pool {
                idle: Vec::new(),
                total: 0,
            };
            let mut direct_value = 0;
            let bytes = 3 * MB / 2;
            let mut last_copy = 0;
            for round in 0..2u8 {
                // Two submissions' bytes, as upload memory of their own.
                let mut staged = Vec::new();
                let mut uploads = Vec::new();
                for part in 0..2u8 {
                    let upload = buffer(
                        &device,
                        D3D12_HEAP_TYPE_UPLOAD,
                        D3D12_RESOURCE_STATE_GENERIC_READ,
                        bytes,
                    );
                    let mut data = std::ptr::null_mut();
                    upload.Map(0, None, Some(&mut data)).expect("mapped");
                    let fill = round * 16 + part + 1;
                    for i in 0..bytes as usize {
                        data.cast::<u8>()
                            .add(i)
                            .write(fill.wrapping_add((i % 251) as u8));
                    }
                    upload.Unmap(0, None);
                    let stage = pool.take(&device, bytes, 64 * MB).expect("a stage");
                    let value = lane.send(&stage, &upload, bytes).expect("a copy");
                    assert!(
                        value > last_copy,
                        "the values rise in the order of the copies"
                    );
                    last_copy = value;
                    staged.push(Staged { stage, value });
                    uploads.push(upload);
                }
                assert_eq!(staged[1].value, staged[0].value + 1);
                // The drawing queue waits for the later copy, which is a
                // wait for the earlier one as well.
                direct.Wait(&lane.fence, staged[1].value).expect("a wait");
                let readbacks: Vec<ID3D12Resource> = (0..2)
                    .map(|_| {
                        buffer(
                            &device,
                            D3D12_HEAP_TYPE_READBACK,
                            D3D12_RESOURCE_STATE_COPY_DEST,
                            bytes,
                        )
                    })
                    .collect();
                allocator.Reset().expect("reset");
                list.Reset(&allocator, None).expect("reset");
                let read = D3D12_RESOURCE_STATE_COPY_SOURCE
                    | D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
                let first: Vec<D3D12_RESOURCE_BARRIER> = staged
                    .iter()
                    .map(|s| crate::host::transition(s.buffer(), D3D12_RESOURCE_STATE_COMMON, read))
                    .collect();
                list.ResourceBarrier(&first);
                for (s, readback) in staged.iter().zip(&readbacks) {
                    list.CopyBufferRegion(readback, 0, s.buffer(), 0, bytes);
                }
                let last: Vec<D3D12_RESOURCE_BARRIER> = staged
                    .iter()
                    .map(|s| crate::host::transition(s.buffer(), read, D3D12_RESOURCE_STATE_COMMON))
                    .collect();
                list.ResourceBarrier(&last);
                list.Close().expect("closed");
                direct.ExecuteCommandLists(&[Some(
                    list.cast::<ID3D12CommandList>().expect("a list"),
                )]);
                direct_value += 1;
                direct
                    .Signal(&direct_fence, direct_value)
                    .expect("a signal");
                assert!(
                    reached(&direct_fence, direct_value),
                    "the drawing queue finished (round {round})"
                );
                assert!(
                    lane.fence.GetCompletedValue() >= staged[1].value,
                    "the copies were done before the drawing queue went on"
                );
                for (part, (s, readback)) in staged.iter().zip(&readbacks).enumerate() {
                    let mut data = std::ptr::null_mut();
                    readback.Map(0, None, Some(&mut data)).expect("mapped");
                    let fill = round * 16 + part as u8 + 1;
                    for i in (0..bytes as usize).step_by(4099) {
                        assert_eq!(
                            data.cast::<u8>().add(i).read(),
                            fill.wrapping_add((i % 251) as u8),
                            "byte {i} of part {part}, round {round}"
                        );
                    }
                    readback.Unmap(0, None);
                    assert_eq!(s.value(), staged[0].value + part as u64);
                }
                // The submissions have run: the stages are given back.
                for s in staged {
                    pool.give(s.stage);
                }
                drop(uploads);
            }
            assert_eq!(
                pool.total,
                4 * MB,
                "two stages of 2 MB made in the first round, reused in the second"
            );
        }
    }
}
