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

/// What of the freed upload memory the driver keeps, and what makes it let go.
#[test]
#[ignore = "needs a card: prints what the driver keeps of freed upload buffers, and what frees it"]
fn what_frees_the_upload_memory_the_driver_keeps() {
    let mut device: Option<ID3D12Device> = None;
    // SAFETY: plain creation.
    unsafe { D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device).unwrap() };
    let device = device.unwrap();
    // SAFETY: plain creation.
    let queue: ID3D12CommandQueue = unsafe {
        device
            .CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
                Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
                ..Default::default()
            })
            .unwrap()
    };
    // SAFETY: plain creation.
    let fence: ID3D12Fence = unsafe { device.CreateFence(0, D3D12_FENCE_FLAG_NONE).unwrap() };
    let mut value = 0u64;
    let mut idle = || {
        value += 1;
        // SAFETY: a live queue and fence; the wait is on this thread's own event.
        unsafe {
            queue.Signal(&fence, value).unwrap();
            while fence.GetCompletedValue() < value {
                std::thread::yield_now();
            }
        }
    };

    // The sizes of what the host makes in a run: scratch chunks of 256 KiB
    // and 4 MiB, memory the submissions send up (8 MiB and more), blocks of
    // staging (64 MiB), made and let go in turns, a tenth of them kept.
    let mut last = memory();
    println!(
        "start: private {:.1} MB, working set {:.1} MB, {:.1} MB write-combined in {} allocations",
        last.private as f64 / MB,
        last.working as f64 / MB,
        last.combined as f64 / MB,
        last.allocations
    );
    let sizes = [
        256u64 << 10,
        4 << 20,
        8 << 20,
        16 << 20,
        256 << 10,
        1 << 20,
        4 << 20,
    ];
    let mut kept = Vec::new();
    let rounds = number("COST_ROUNDS", 600);
    for i in 0..rounds {
        let size = sizes[i % sizes.len()];
        let made = buffer(&device, D3D12_HEAP_TYPE_UPLOAD, size);
        if i % 10 == 0 {
            kept.push(made);
        } else if i % 3 == 0 {
            // Held for a few more rounds, as a list's scratch is.
            kept.push(made);
            if kept.len() > 40 {
                kept.remove(1);
            }
        }
    }
    last = report(
        &format!(
            "{rounds} upload buffers made of mixed sizes, most let go, {} alive",
            kept.len()
        ),
        last,
        1,
    );
    idle();
    last = report(
        "... the queue idle (a fence signalled and reached)",
        last,
        1,
    );
    std::thread::sleep(std::time::Duration::from_secs(3));
    last = report("... 3 s later", last, 1);
    for size in [1u64 << 20, 4 << 20, 16 << 20, 32 << 20, 64 << 20] {
        drop(buffer(&device, D3D12_HEAP_TYPE_UPLOAD, size));
        last = report(
            &format!(
                "... one upload buffer of {} MiB made and dropped",
                size >> 20
            ),
            last,
            1,
        );
        idle();
        last = report("... the queue idle", last, 1);
    }
    let (from, _) = buffer(&device, D3D12_HEAP_TYPE_DEFAULT, 1 << 20);
    drop(from);
    last = report("... one default buffer made and dropped", last, 1);
    drop(kept);
    last = report("the buffers kept dropped", last, 1);
    idle();
    last = report("... the queue idle", last, 1);
    for size in [64u64 << 20, 64 << 20] {
        drop(buffer(&device, D3D12_HEAP_TYPE_UPLOAD, size));
        last = report("... one upload buffer of 64 MiB made and dropped", last, 1);
    }
    let _ = last;
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
