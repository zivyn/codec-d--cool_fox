//! What the host's D3D12 objects cost in the process's memory, on the card
//! of the machine it runs on (not run by default:
//! `cargo test -p victory-gpu --release --test memory_cost -- --ignored --nocapture`).
//!
//! The title makes some two thousand command lists and the host makes a
//! command allocator and a list for each (and a second allocator and list
//! for most, the prelude): the process's memory line asks what that costs
//! and what upload buffers of several sizes cost, and what the driver keeps
//! when they go. Each phase prints the change of the private bytes, of the
//! working set, and of the committed bytes of write-combined allocations,
//! which are the ones the memory map calls "buffers shared with the card".
//!
//! `COST_LISTS` (300) lists per phase, `COST_CALLS` (2000) commands recorded
//! in each of them.
#![cfg(windows)]

use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows_sys::Win32::System::Memory::{
    MEM_COMMIT, MEM_FREE, MEMORY_BASIC_INFORMATION, PAGE_WRITECOMBINE, VirtualQuery,
};
use windows_sys::Win32::System::ProcessStatus::{
    K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

const MB: f64 = (1 << 20) as f64;

/// A number from the environment.
fn number(name: &str, otherwise: usize) -> usize {
    std::env::var(name)
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(otherwise)
}

/// Private bytes, working set, and the committed bytes of write-combined
/// allocations with how many allocations they are in.
#[derive(Clone, Copy, Default)]
struct Memory {
    private: u64,
    working: u64,
    combined: u64,
    allocations: u64,
}

fn memory() -> Memory {
    // SAFETY: plain data filled in by the calls.
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS_EX = std::mem::zeroed();
        c.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
        K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb);
        let mut combined = 0u64;
        let mut bases = std::collections::HashSet::new();
        let mut at = 0usize;
        loop {
            let mut m: MEMORY_BASIC_INFORMATION = std::mem::zeroed();
            if VirtualQuery(at as *const _, &mut m, size_of_val(&m)) == 0 {
                break;
            }
            if m.State != MEM_FREE && m.State == MEM_COMMIT && m.Protect & PAGE_WRITECOMBINE != 0 {
                combined += m.RegionSize as u64;
                bases.insert(m.AllocationBase as usize);
            }
            let next = (m.BaseAddress as usize).saturating_add(m.RegionSize);
            if next <= at {
                break;
            }
            at = next;
        }
        Memory {
            private: c.PrivateUsage as u64,
            working: c.WorkingSetSize as u64,
            combined,
            allocations: bases.len() as u64,
        }
    }
}

/// Prints what changed since `before`, per `count` objects.
fn report(what: &str, before: Memory, count: usize) -> Memory {
    let now = memory();
    let each = |a: u64, b: u64| (a as f64 - b as f64) / MB / count.max(1) as f64;
    println!(
        "{what:<58} private {:+9.1} MB ({:+7.3} each), working set {:+9.1} MB ({:+7.3} each), write-combined {:+9.1} MB in {:+4} allocations ({:+7.3} each)",
        (now.private as f64 - before.private as f64) / MB,
        each(now.private, before.private),
        (now.working as f64 - before.working as f64) / MB,
        each(now.working, before.working),
        (now.combined as f64 - before.combined as f64) / MB,
        now.allocations as i64 - before.allocations as i64,
        each(now.combined, before.combined),
    );
    now
}

fn buffer(device: &ID3D12Device, kind: D3D12_HEAP_TYPE, size: u64) -> (ID3D12Resource, *mut u8) {
    let heap = D3D12_HEAP_PROPERTIES {
        Type: kind,
        ..Default::default()
    };
    let desc = D3D12_RESOURCE_DESC {
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
    };
    let state = if kind == D3D12_HEAP_TYPE_UPLOAD {
        D3D12_RESOURCE_STATE_GENERIC_READ
    } else {
        D3D12_RESOURCE_STATE_COMMON
    };
    let mut made: Option<ID3D12Resource> = None;
    let mut data = std::ptr::null_mut();
    // SAFETY: plain creation and mapping.
    unsafe {
        device
            .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, state, None, &mut made)
            .unwrap();
        let made = made.unwrap();
        if kind == D3D12_HEAP_TYPE_UPLOAD {
            made.Map(0, None, Some(&mut data)).unwrap();
            // Touched, as a scratch chunk is when a list fills it.
            for at in (0..size as usize).step_by(4096) {
                data.cast::<u8>().add(at).write(1);
            }
        }
        (made, data.cast())
    }
}

type Pair = (ID3D12CommandAllocator, ID3D12GraphicsCommandList);

fn pair(device: &ID3D12Device, closed: bool) -> Pair {
    // SAFETY: plain creation.
    unsafe {
        let allocator: ID3D12CommandAllocator = device
            .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
            .unwrap();
        let list: ID3D12GraphicsCommandList = device
            .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)
            .unwrap();
        if closed {
            list.Close().unwrap();
        }
        (allocator, list)
    }
}

/// `calls` copies recorded into an open list.
fn record(
    list: &ID3D12GraphicsCommandList,
    from: &ID3D12Resource,
    to: &ID3D12Resource,
    calls: usize,
) {
    for i in 0..calls {
        // SAFETY: an open list and two live buffers; nothing runs.
        unsafe { list.CopyBufferRegion(to, (i as u64 % 1024) * 256, from, 0, 256) };
    }
}

#[test]
#[ignore = "needs a card: prints what lists, allocators and upload buffers cost in memory"]
fn what_lists_and_buffers_cost() {
    let lists = number("COST_LISTS", 300);
    let calls = number("COST_CALLS", 2000);
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: plain creation.
    unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap() };
    let device = device.unwrap();
    let (from, _) = buffer(&device, D3D12_HEAP_TYPE_DEFAULT, 1 << 20);
    let (to, _) = buffer(&device, D3D12_HEAP_TYPE_DEFAULT, 1 << 20);

    let mut last = memory();
    println!(
        "start: private {:.1} MB, working set {:.1} MB, {:.1} MB write-combined in {} allocations",
        last.private as f64 / MB,
        last.working as f64 / MB,
        last.combined as f64 / MB,
        last.allocations
    );

    // A host list: an allocator and an open list, as `make_list` makes them.
    let mut host: Vec<Pair> = (0..lists).map(|_| pair(&device, false)).collect();
    last = report(
        &format!("{lists} host lists made (open, nothing recorded)"),
        last,
        lists,
    );

    for (_, list) in &host {
        record(list, &from, &to, calls);
    }
    last = report(
        &format!("... {calls} commands recorded in each"),
        last,
        lists,
    );

    for (allocator, list) in &host {
        // SAFETY: nothing runs; as `HostList::reset`.
        unsafe {
            list.Close().unwrap();
            allocator.Reset().unwrap();
            list.Reset(allocator, None).unwrap();
        }
    }
    last = report(
        "... each closed and reset (the allocator kept)",
        last,
        lists,
    );

    for (_, list) in &host {
        record(list, &from, &to, calls);
    }
    last = report(
        &format!("... {calls} commands recorded in each again"),
        last,
        lists,
    );

    for (allocator, list) in &mut host {
        // SAFETY: nothing runs. The old allocator goes; a new one takes over.
        unsafe {
            list.Close().unwrap();
            let fresh: ID3D12CommandAllocator = device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap();
            list.Reset(&fresh, None).unwrap();
            *allocator = fresh;
        }
    }
    last = report(
        "... each given a new allocator (the old one released)",
        last,
        lists,
    );

    // A prelude: an allocator and a closed list.
    let preludes: Vec<Pair> = (0..lists).map(|_| pair(&device, true)).collect();
    last = report(&format!("{lists} preludes made (closed)"), last, lists);
    for (allocator, list) in &preludes {
        // SAFETY: nothing runs.
        unsafe {
            allocator.Reset().unwrap();
            list.Reset(allocator, None).unwrap();
        }
        record(list, &from, &to, 16);
        // SAFETY: an open list.
        unsafe { list.Close().unwrap() };
    }
    last = report("... each recorded with 16 commands once", last, lists);

    drop(preludes);
    last = report("preludes dropped", last, lists);
    drop(host);
    last = report("host lists dropped", last, lists);

    // Upload buffers of the sizes the host makes.
    for (size, count) in [
        (256u64 << 10, 1000usize),
        (1 << 20, 200),
        (4 << 20, 100),
        (64 << 20, 8),
    ] {
        let held: Vec<_> = (0..count)
            .map(|_| buffer(&device, D3D12_HEAP_TYPE_UPLOAD, size))
            .collect();
        last = report(
            &format!("{count} upload buffers of {} KiB, written", size >> 10),
            last,
            count,
        );
        drop(held);
        last = report("... dropped", last, count);
    }
}

/// Sizes the host makes upload buffers of in a run: scratch chunks of 256 KiB
/// and 4 MiB, memory submissions send up with (8 MiB and more), texture
/// staging of every size.
const SIZES: [u64; 9] = [
    256 << 10,
    4 << 20,
    8 << 20,
    16 << 20,
    256 << 10,
    1 << 20,
    4 << 20,
    512 << 10,
    2 << 20,
];

/// A queue run to idle: a fence signalled and reached.
struct Idle {
    queue: ID3D12CommandQueue,
    fence: ID3D12Fence,
    value: u64,
}

impl Idle {
    fn new(device: &ID3D12Device) -> Idle {
        // SAFETY: plain creation.
        unsafe {
            let queue = device
                .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                    Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                    ..Default::default()
                })
                .unwrap();
            let fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap();
            Idle {
                queue,
                fence,
                value: 0,
            }
        }
    }

    fn wait(&mut self) {
        self.value += 1;
        // SAFETY: a live queue and fence.
        unsafe {
            self.queue.Signal(&self.fence, self.value).unwrap();
            while self.fence.GetCompletedValue() < self.value {
                std::thread::yield_now();
            }
        }
    }
}

fn start(what: &str) -> Memory {
    let now = memory();
    println!(
        "[{what}] start: private {:.1} MB, working set {:.1} MB, {:.1} MB write-combined in {} allocations",
        now.private as f64 / MB,
        now.working as f64 / MB,
        now.combined as f64 / MB,
        now.allocations
    );
    now
}

/// A buffer of `size` bytes placed at `at` in `heap`, mapped and written.
fn placed(
    device: &ID3D12Device,
    heap: &ID3D12Heap,
    at: u64,
    size: u64,
    fill: u8,
) -> ID3D12Resource {
    let desc = D3D12_RESOURCE_DESC {
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
    };
    let mut made: Option<ID3D12Resource> = None;
    // SAFETY: placed inside the heap, nothing else lies there.
    unsafe {
        device
            .CreatePlacedResource(
                heap,
                at,
                &desc,
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut made,
            )
            .unwrap();
        let made = made.unwrap();
        let mut data = std::ptr::null_mut();
        made.Map(0, None, Some(&mut data)).unwrap();
        std::ptr::write_bytes(data.cast::<u8>(), fill, size as usize);
        made
    }
}

/// What of the freed upload memory the driver keeps, and what makes it let
/// go. `COST_SCENARIO` (each in a process of its own, as the driver's pools
/// would carry from one to the next): `churn` (buffers made and let go,
/// then what is tried to free the pools), `placed` (the same through
/// buffers placed in a heap of ours), `ring` (the same through one buffer
/// cut in pieces by us), `trim` (does the device have `Trim`), `steady`
/// (churn for a long time: do the pools grow or settle).
#[test]
#[ignore = "needs a card: prints what the driver keeps of freed upload buffers, and what frees it"]
fn what_frees_the_upload_memory_the_driver_keeps() {
    let scenario = std::env::var("COST_SCENARIO").unwrap_or_else(|_| "churn".into());
    let rounds = number("COST_ROUNDS", 600);
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: plain creation.
    unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap() };
    let device = device.unwrap();
    let mut idle = Idle::new(&device);
    let mut last = start(&scenario);
    let mut keep: std::collections::VecDeque<Box<dyn std::any::Any>> =
        std::collections::VecDeque::new();
    match scenario.as_str() {
        "trim" => {
            use windows::Win32::Graphics::Dxgi::IDXGIDevice3;
            use windows::core::Interface;
            println!(
                "device as an IDXGIDevice3 (for Trim): {}",
                device.cast::<IDXGIDevice3>().is_ok()
            );
        }
        "untouched" => {
            // Upload buffers of 64 MiB mapped and not written: does the
            // working set take their pages, or only the commit?
            let held: Vec<_> = (0..16)
                .map(|_| {
                    let heap = D3D12_HEAP_PROPERTIES {
                        Type: D3D12_HEAP_TYPE_UPLOAD,
                        ..Default::default()
                    };
                    let desc = D3D12_RESOURCE_DESC {
                        Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                        Width: 64 << 20,
                        Height: 1,
                        DepthOrArraySize: 1,
                        MipLevels: 1,
                        SampleDesc: DXGI_SAMPLE_DESC {
                            Count: 1,
                            Quality: 0,
                        },
                        Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                        ..Default::default()
                    };
                    let mut made: Option<ID3D12Resource> = None;
                    let mut data = std::ptr::null_mut();
                    // SAFETY: plain creation and mapping.
                    unsafe {
                        device
                            .CreateCommittedResource(
                                &heap,
                                D3D12_HEAP_FLAG_NONE,
                                &desc,
                                D3D12_RESOURCE_STATE_GENERIC_READ,
                                None,
                                &mut made,
                            )
                            .unwrap();
                        let made = made.unwrap();
                        made.Map(0, None, Some(&mut data)).unwrap();
                        (made, data.cast::<u8>())
                    }
                })
                .collect();
            last = report("16 upload buffers of 64 MiB, mapped, not written", last, 16);
            for (_, data) in &held {
                for at in (0..(32usize << 20)).step_by(4096) {
                    // SAFETY: inside the mapping.
                    unsafe { data.add(at).write(1) };
                }
            }
            last = report("... the first half of each written", last, 16);
            drop(held);
            let _ = report("... dropped", last, 16);
        }
        "heaps" => {
            // Descriptor heaps: the title's tables span 1.5 million slots.
            for (count, visible) in [
                (1_000_000u32, true),
                (1_000_000, false),
                (4_000_000, true),
                (65_536, true),
            ] {
                let desc = D3D12_DESCRIPTOR_HEAP_DESC {
                    Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                    NumDescriptors: count,
                    Flags: if visible {
                        D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE
                    } else {
                        D3D12_DESCRIPTOR_HEAP_FLAG_NONE
                    },
                    NodeMask: 0,
                };
                // SAFETY: plain creation.
                let made: Result<ID3D12DescriptorHeap, _> =
                    unsafe { device.CreateDescriptorHeap(&desc) };
                last = report(
                    &format!(
                        "a descriptor heap of {count}, shader visible {visible}: {}",
                        made.is_ok()
                    ),
                    last,
                    1,
                );
                drop(made);
                idle.wait();
                last = report("... dropped", last, 1);
            }
        }
        "ring" => {
            // One buffer of 256 MiB, pieces of the sizes cut from it in a
            // ring, as the host would cut scratch and send memory.
            let (big, data) = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, 256 << 20);
            last = report("one upload buffer of 256 MiB, written", last, 1);
            let mut at = 0u64;
            for i in 0..rounds * 20 {
                let size = SIZES[i % SIZES.len()];
                if at + size > 256 << 20 {
                    at = 0;
                }
                // SAFETY: inside the mapping.
                unsafe { std::ptr::write_bytes(data.add(at as usize), i as u8, size as usize) };
                at += size;
            }
            last = report(
                &format!("{} pieces cut and written in a ring", rounds * 20),
                last,
                1,
            );
            drop(big);
            let _ = report("the buffer dropped", last, 1);
        }
        "placed" => {
            // A heap of upload memory of ours, buffers placed in it.
            let heap_size = 256u64 << 20;
            let desc = D3D12_HEAP_DESC {
                SizeInBytes: heap_size,
                Properties: D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_UPLOAD,
                    ..Default::default()
                },
                Alignment: 0,
                Flags: D3D12_HEAP_FLAG_ALLOW_ONLY_BUFFERS,
            };
            let mut heap: Option<ID3D12Heap> = None;
            // SAFETY: plain creation.
            unsafe { device.CreateHeap(&desc, &mut heap).unwrap() };
            let heap = heap.unwrap();
            last = report("a heap of upload memory of 256 MiB", last, 1);
            let mut at = 0u64;
            for i in 0..rounds * 4 {
                let size = SIZES[i % SIZES.len()];
                if at + size > heap_size {
                    at = 0;
                    keep.clear();
                }
                keep.push_back(Box::new(placed(&device, &heap, at, size, i as u8)));
                if keep.len() > 40 {
                    keep.pop_front();
                }
                at += size;
            }
            last = report(
                &format!("{} buffers placed in it and let go", rounds * 4),
                last,
                1,
            );
            keep.clear();
            last = report("... all let go", last, 1);
            idle.wait();
            last = report("... the queue idle", last, 1);
            drop(heap);
            let _ = report("the heap dropped", last, 1);
        }
        "churn" | "steady" => {
            let rounds = if scenario == "steady" {
                rounds * 8
            } else {
                rounds
            };
            for i in 0..rounds {
                let size = SIZES[i % SIZES.len()];
                let made = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, size);
                // Held a few rounds, as a list's scratch is; a tenth for long.
                keep.push_back(Box::new(made));
                if keep.len() > 40 {
                    let old = keep.pop_front().unwrap();
                    if i % 10 == 0 {
                        keep.push_back(old);
                    }
                }
                if scenario == "steady" && (i + 1) % (rounds / 8) == 0 {
                    last = report(
                        &format!("after {} rounds, {} alive", i + 1, keep.len()),
                        last,
                        1,
                    );
                    if (i + 1) / (rounds / 8) % 2 == 0 {
                        idle.wait();
                    }
                }
            }
            last = report(
                &format!(
                    "{rounds} upload buffers made of mixed sizes, {} alive",
                    keep.len()
                ),
                last,
                1,
            );
            idle.wait();
            last = report(
                "... the queue idle (a fence signalled and reached)",
                last,
                1,
            );
            std::thread::sleep(std::time::Duration::from_secs(3));
            last = report("... 3 s later", last, 1);
            std::thread::sleep(std::time::Duration::from_secs(10));
            last = report("... 13 s later", last, 1);
            for size in [1u64 << 20, 4 << 20, 16 << 20, 32 << 20, 64 << 20, 256 << 20] {
                drop(buffer(&device, D3D12_HEAP_TYPE_UPLOAD, size));
                last = report(
                    &format!(
                        "... one upload buffer of {} MiB made and dropped",
                        size >> 20
                    ),
                    last,
                    1,
                );
                idle.wait();
                last = report("... the queue idle", last, 1);
            }
            keep.clear();
            last = report("the buffers kept dropped", last, 1);
            idle.wait();
            let _ = report("... the queue idle", last, 1);
        }
        other => panic!("COST_SCENARIO {other}?"),
    }
}

/// Where the cost of a list and its allocator lies.
#[test]
#[ignore = "needs a card: prints what an allocator and a list cost apart"]
fn what_an_allocator_and_a_list_cost_apart() {
    let n = number("COST_LISTS", 300);
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: plain creation.
    unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap() };
    let device = device.unwrap();
    let mut last = memory();
    // SAFETY: plain creation.
    let allocators: Vec<ID3D12CommandAllocator> = (0..n)
        .map(|_| unsafe {
            device
                .CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)
                .unwrap()
        })
        .collect();
    last = report(&format!("{n} allocators alone"), last, n);
    // SAFETY: lists made one at a time on one allocator, each closed at once.
    let lists: Vec<ID3D12GraphicsCommandList> = (0..n)
        .map(|_| unsafe {
            let list: ID3D12GraphicsCommandList = device
                .CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocators[0], None)
                .unwrap();
            list.Close().unwrap();
            list
        })
        .collect();
    last = report(
        &format!("{n} lists on the first allocator, closed"),
        last,
        n,
    );
    // SAFETY: nothing runs; one list open at a time on its own allocator.
    for (list, allocator) in lists.iter().zip(&allocators) {
        unsafe {
            list.Reset(allocator, None).unwrap();
            list.Close().unwrap();
        }
    }
    last = report(
        "... each reset on an allocator of its own and closed",
        last,
        n,
    );
    // Reset again with a record of some commands on a second allocator each.
    drop(lists);
    last = report("lists dropped", last, n);
    drop(allocators);
    let _ = report("allocators dropped", last, n);
}
