//! What command lists cost in the PC's memory: N allocators with a direct list
//! each are made, then each records C small copies and runs, then each is reset,
//! then all go. After each step: the process's private bytes and its
//! write-combined private memory (committed, and in physical memory), by a walk
//! of the address space.
//!
//!     cargo run --release -p victory-gpu --example list_memory [N] [C]
//!
//! Found with it (2026-11-28, RTX 4070 SUPER, 500 lists): an empty allocator and list
//! about 0.8 MB of private memory; 20,000 recorded copies 1.8 MB more each, kept over
//! resets; the driver keeps its write-combined pages after the lists go. Upload
//! buffers: most of 1-8 MB ones given back on release, 64 KB ones kept (their chunks).
use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::Interface;

fn private_mb() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{K32GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS_EX};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let mut c: PROCESS_MEMORY_COUNTERS_EX = unsafe { std::mem::zeroed() };
    c.cb = size_of::<PROCESS_MEMORY_COUNTERS_EX>() as u32;
    // SAFETY: our own process, a counters structure of the size given.
    unsafe { K32GetProcessMemoryInfo(GetCurrentProcess(), (&raw mut c).cast(), c.cb) };
    c.PrivateUsage as f64 / (1 << 20) as f64
}

/// Write-combined private memory: committed MB, MB in memory (one page in
/// sixteen asked), regions.
fn write_combined() -> (f64, f64, usize) {
    use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_PRIVATE, MEMORY_BASIC_INFORMATION, PAGE_WRITECOMBINE, VirtualQuery};
    use windows_sys::Win32::System::ProcessStatus::K32QueryWorkingSetEx;
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    let (mut committed, mut present, mut regions) = (0u64, 0u64, 0usize);
    let mut at = 0usize;
    loop {
        let mut info: MEMORY_BASIC_INFORMATION = unsafe { std::mem::zeroed() };
        // SAFETY: a query of our own address space.
        if unsafe { VirtualQuery(at as *const _, &mut info, size_of::<MEMORY_BASIC_INFORMATION>()) } == 0 {
            break;
        }
        if info.State == MEM_COMMIT && info.Type == MEM_PRIVATE && info.Protect & PAGE_WRITECOMBINE != 0 {
            committed += info.RegionSize as u64;
            regions += 1;
            let base = info.BaseAddress as usize;
            let mut asked: Vec<[usize; 2]> = (0..info.RegionSize).step_by(16 << 12).map(|o| [base + o, 0]).collect();
            // SAFETY: entries of two words, as the call fills them.
            if unsafe { K32QueryWorkingSetEx(GetCurrentProcess(), asked.as_mut_ptr().cast(), (asked.len() * 16) as u32) } != 0 {
                present += asked.iter().filter(|a| a[1] & 1 != 0).count() as u64 * (16 << 12);
            }
        }
        at = info.BaseAddress as usize + info.RegionSize;
    }
    let mb = |b: u64| b as f64 / (1 << 20) as f64;
    (mb(committed), mb(present), regions)
}

fn say(step: &str) {
    let (wc, wc_in, regions) = write_combined();
    println!("{step:52} private {:8.0} MB; write-combined {wc:7.0} MB committed ({wc_in:6.0} MB in memory, {regions} regions)", private_mb());
}

fn main() -> windows::core::Result<()> {
    let args: Vec<usize> = std::env::args().skip(1).filter_map(|a| a.parse().ok()).collect();
    let (n, copies) = (args.first().copied().unwrap_or(500), args.get(1).copied().unwrap_or(20_000));
    // SAFETY: plain D3D12 calls with valid out pointers; every list is waited for.
    unsafe {
        let mut device: Option<ID3D12Device> = None;
        D3D12CreateDevice(None, D3D_FEATURE_LEVEL_12_0, &mut device)?;
        let device = device.unwrap();
        let queue: ID3D12CommandQueue =
            device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC { Type: D3D12_COMMAND_LIST_TYPE_DIRECT, ..Default::default() })?;
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        let event = CreateEventW(None, false, false, None)?;
        let buffer = |size: u64| -> windows::core::Result<ID3D12Resource> {
            let mut r: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_DEFAULT, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: size,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_COMMON,
                None,
                &mut r,
            )?;
            Ok(r.unwrap())
        };
        let (a, b) = (buffer(1 << 20)?, buffer(1 << 20)?);
        say("device and two buffers");
        let mut lists = Vec::with_capacity(n);
        for _ in 0..n {
            let allocator: ID3D12CommandAllocator = device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
            let list: ID3D12GraphicsCommandList = device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &allocator, None)?;
            list.Close()?;
            lists.push((allocator, list));
        }
        say(&format!("{n} allocators and lists made, empty"));
        let mut value = 0;
        for (allocator, list) in &lists {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            for i in 0..copies as u64 {
                list.CopyBufferRegion(&b, (i * 4) % (1 << 20), &a, (i * 16) % (1 << 20), 4);
            }
            list.Close()?;
            queue.ExecuteCommandLists(&[Some(list.cast()?)]);
            value += 1;
            queue.Signal(&fence, value)?;
            fence.SetEventOnCompletion(value, event)?;
            WaitForSingleObject(event, INFINITE);
        }
        say(&format!("each recorded {copies} copies and ran"));
        for (allocator, list) in &lists {
            allocator.Reset()?;
            list.Reset(allocator, None)?;
            list.Close()?;
        }
        say("each reset (as a pooled list is)");
        drop(lists);
        say("all gone");
        // Upload buffers made and released, as the layer's churn does.
        let upload = |size: u64| -> windows::core::Result<ID3D12Resource> {
            let mut r: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES { Type: D3D12_HEAP_TYPE_UPLOAD, ..Default::default() },
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: size,
                    Height: 1,
                    DepthOrArraySize: 1,
                    MipLevels: 1,
                    SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
                    Layout: D3D12_TEXTURE_LAYOUT_ROW_MAJOR,
                    ..Default::default()
                },
                D3D12_RESOURCE_STATE_GENERIC_READ,
                None,
                &mut r,
            )?;
            Ok(r.unwrap())
        };
        for (count, size) in [(2000usize, 1u64 << 20), (200, 8 << 20), (8000, 64 << 10)] {
            let mut alive = Vec::with_capacity(count);
            for _ in 0..count {
                let r = upload(size)?;
                let mut data = std::ptr::null_mut();
                r.Map(0, None, Some(&mut data))?;
                std::ptr::write_bytes(data.cast::<u8>(), 1, size as usize);
                alive.push(r);
            }
            say(&format!("{count} upload buffers of {} KB alive, written", size >> 10));
            drop(alive);
            say("  ... released");
        }
    }
    Ok(())
}
