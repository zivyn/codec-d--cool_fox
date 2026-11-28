//! The two queues on the software adapter: what the host asks of them (the
//! compute queue's work seen by the drawing queue after the wait, the host's
//! fence signalled by the sequencer only when both have got there, and a
//! random title through the whole book) happens, over many rounds.

use super::*;
use windows::Win32::Foundation::{HANDLE, WAIT_OBJECT_0};
use windows::Win32::Graphics::Dxgi::Common::DXGI_SAMPLE_DESC;
use windows::core::s;

/// `data[0] = data[0] * 3 + value`: what is done in what order shows in the result.
const STEP_CS: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), UAV(u0)"
cbuffer Args : register(b0) { uint value; };
RWByteAddressBuffer data : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    data.Store(0, data.Load(0) * 3u + value);
}
"#;

/// A long time on the card, then a number the CPU can work out too.
const SLOW_CS: &str = r#"
#define RS "RootConstants(num32BitConstants=1, b0), UAV(u0)"
cbuffer Args : register(b0) { uint value; };
RWByteAddressBuffer data : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    uint x = value;
    [loop]
    for (uint i = 0u; i < 400000u; ++i) {
        x = x * 1664525u + 1013904223u;
    }
    data.Store(0, x | 1u);
}
"#;

struct Harness {
    device: ID3D12Device,
    direct: ID3D12CommandQueue,
    compute: ID3D12CommandQueue,
    order: ID3D12CommandQueue,
    /// Q1's and Q2's, and the host's.
    fences: [ID3D12Fence; 3],
    step: (ID3D12RootSignature, ID3D12PipelineState),
    slow: (ID3D12RootSignature, ID3D12PipelineState),
    event: HANDLE,
    kept: Vec<(ID3D12CommandAllocator, ID3D12GraphicsCommandList)>,
    _turn: std::sync::MutexGuard<'static, ()>,
}

fn warp() -> Option<ID3D12Device> {
    use windows::Win32::Graphics::Direct3D::D3D_FEATURE_LEVEL_12_0;
    // SAFETY: plain DXGI queries and device creation on the software adapter; a card is never opened.
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

fn pipeline(
    device: &ID3D12Device,
    source: &str,
) -> Option<(ID3D12RootSignature, ID3D12PipelineState)> {
    let code = compile(source, s!("main"), s!("cs_5_1")).ok()?;
    // SAFETY: the shader's own bytes carry the root signature.
    unsafe {
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &code).ok()?;
        let pso: ID3D12PipelineState = device
            .CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
            .ok()?;
        Some((root, pso))
    }
}

impl Harness {
    fn new() -> Option<Harness> {
        let turn = crate::indirect::tests::warp_turn();
        let device = warp()?;
        // SAFETY: plain object creation.
        unsafe {
            let queue = |kind| {
                device
                    .CreateCommandQueue::<ID3D12CommandQueue>(&D3D12_COMMAND_QUEUE_DESC {
                        Type: kind,
                        ..Default::default()
                    })
                    .ok()
            };
            let fence = || {
                device
                    .CreateFence::<ID3D12Fence>(0, D3D12_FENCE_FLAG_NONE)
                    .ok()
            };
            Some(Harness {
                direct: queue(D3D12_COMMAND_LIST_TYPE_DIRECT)?,
                compute: queue(D3D12_COMMAND_LIST_TYPE_COMPUTE)?,
                order: queue(D3D12_COMMAND_LIST_TYPE_COPY)?,
                fences: [fence()?, fence()?, fence()?],
                step: pipeline(&device, STEP_CS)?,
                slow: pipeline(&device, SLOW_CS)?,
                event: CreateEventW(None, false, false, None).ok()?,
                kept: Vec::new(),
                device,
                _turn: turn,
            })
        }
    }

    fn buffer(
        &self,
        size: u64,
        heap: D3D12_HEAP_TYPE,
        flags: D3D12_RESOURCE_FLAGS,
        state: D3D12_RESOURCE_STATES,
    ) -> ID3D12Resource {
        let props = D3D12_HEAP_PROPERTIES {
            Type: heap,
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
            Flags: flags,
            ..Default::default()
        };
        let mut buffer: Option<ID3D12Resource> = None;
        // SAFETY: plain buffer creation.
        unsafe {
            self.device
                .CreateCommittedResource(
                    &props,
                    D3D12_HEAP_FLAG_NONE,
                    &desc,
                    state,
                    None,
                    &mut buffer,
                )
                .expect("a buffer");
        }
        buffer.expect("a buffer")
    }

    /// A buffer the shaders write, in the state buffers are left in.
    fn data(&self) -> ID3D12Resource {
        self.buffer(
            256,
            D3D12_HEAP_TYPE_DEFAULT,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_COMMON,
        )
    }

    fn readback(&self, size: u64) -> ID3D12Resource {
        self.buffer(
            size,
            D3D12_HEAP_TYPE_READBACK,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )
    }

    /// The values in a readback buffer, as words.
    fn words(&self, buffer: &ID3D12Resource, count: usize) -> Vec<u32> {
        let mut data = std::ptr::null_mut();
        // SAFETY: a readback buffer of ours, written by lists that have run.
        unsafe {
            buffer.Map(0, None, Some(&mut data)).expect("a mapping");
            let words = std::slice::from_raw_parts(data.cast::<u32>(), count).to_vec();
            buffer.Unmap(0, None);
            words
        }
    }

    /// A closed list of `kind` that does `record`.
    fn list(
        &mut self,
        kind: D3D12_COMMAND_LIST_TYPE,
        record: impl FnOnce(&ID3D12GraphicsCommandList),
    ) -> ID3D12CommandList {
        // SAFETY: plain creation and recording; the lists are kept until the harness is dropped.
        unsafe {
            let allocator: ID3D12CommandAllocator = self
                .device
                .CreateCommandAllocator(kind)
                .expect("an allocator");
            let list: ID3D12GraphicsCommandList = self
                .device
                .CreateCommandList(0, kind, &allocator, None)
                .expect("a list");
            record(&list);
            list.Close().expect("closed");
            let out: ID3D12CommandList = list.cast().expect("a list");
            self.kept.push((allocator, list));
            out
        }
    }

    /// `data[0] = data[0] * 3 + value` (or the slow one), as a list of `kind`.
    fn step(
        &mut self,
        kind: D3D12_COMMAND_LIST_TYPE,
        slow: bool,
        data: &ID3D12Resource,
        value: u32,
    ) -> ID3D12CommandList {
        let (root, pso) = if slow {
            self.slow.clone()
        } else {
            self.step.clone()
        };
        self.list(kind, |l| {
            // SAFETY: an open list and live objects.
            unsafe {
                l.SetComputeRootSignature(&root);
                l.SetPipelineState(&pso);
                l.SetComputeRoot32BitConstants(0, 1, (&raw const value).cast(), 0);
                l.SetComputeRootUnorderedAccessView(1, data.GetGPUVirtualAddress());
                l.Dispatch(1, 1, 1);
            }
        })
    }

    /// Copies the first word of `data` to word `at` of `back`.
    fn copy(
        &mut self,
        kind: D3D12_COMMAND_LIST_TYPE,
        back: &ID3D12Resource,
        at: u64,
        data: &ID3D12Resource,
    ) -> ID3D12CommandList {
        self.list(kind, |l| {
            // SAFETY: an open list and live buffers.
            unsafe { l.CopyBufferRegion(back, at * 4, data, 0, 4) }
        })
    }

    fn run(queue: &ID3D12CommandQueue, list: &ID3D12CommandList) {
        // SAFETY: a closed list of ours.
        unsafe { queue.ExecuteCommandLists(&[Some(list.clone())]) }
    }

    /// Waits for `fence` to reach `value`; false if it has not in `ms`.
    fn reached(&self, fence: &ID3D12Fence, value: u64, ms: u32) -> bool {
        // SAFETY: the harness's event and a fence of ours.
        unsafe {
            if fence.GetCompletedValue() >= value {
                return true;
            }
            fence
                .SetEventOnCompletion(value, self.event)
                .expect("an event on the fence");
            WaitForSingleObject(self.event, ms) == WAIT_OBJECT_0
        }
    }
}

/// What the slow step stores, worked out on the CPU.
fn slow_result(value: u32) -> u32 {
    let mut x = value;
    for _ in 0..400_000u32 {
        x = x.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
    }
    x | 1
}

/// A title's compute lane writes, the drawing queue waits for its fence and reads: what it reads is what was
/// written, every round, as the lists would run one after the other.
#[test]
fn what_the_compute_queue_writes_the_drawing_queue_reads_after_the_wait() {
    let Some(mut h) = Harness::new() else { return };
    let rounds = 40u64;
    let data = h.data();
    let back = h.readback(rounds * 4);
    let mut expected = Vec::new();
    let mut now = 0u32;
    for round in 1..=rounds {
        let value = round as u32 * 7 + 1;
        let slow = round % 5 == 0;
        now = if slow {
            slow_result(value)
        } else {
            now.wrapping_mul(3).wrapping_add(value)
        };
        expected.push(now);
        let write = h.step(D3D12_COMMAND_LIST_TYPE_COMPUTE, slow, &data, value);
        let read = h.copy(D3D12_COMMAND_LIST_TYPE_DIRECT, &back, round - 1, &data);
        // SAFETY: our queue and fence; the signal it waits for was queued in the round before.
        unsafe {
            if round > 1 {
                // (What the drawing queue reads is not written over before it has read it.)
                h.compute.Wait(&h.fences[0], round - 1).expect("a wait");
            }
        }
        Harness::run(&h.compute, &write);
        // SAFETY: our queues and fences.
        unsafe {
            h.compute.Signal(&h.fences[1], round).expect("a signal");
            // The drawing queue goes on when the compute queue has got there.
            h.direct.Wait(&h.fences[1], round).expect("a wait");
        }
        Harness::run(&h.direct, &read);
        // SAFETY: as above.
        unsafe { h.direct.Signal(&h.fences[0], round).expect("a signal") };
    }
    assert!(
        h.reached(&h.fences[0], rounds, 120_000),
        "the queues stand for each other"
    );
    assert_eq!(h.words(&back, rounds as usize), expected);
}

/// The sequencer: the host's fence reaches a value only when both queues have run everything up to it, whatever
/// order they finish in (a slow compute submission among quick drawing ones).
#[test]
fn the_hosts_fence_passes_a_value_only_when_both_queues_have_got_there() {
    let Some(mut h) = Harness::new() else { return };
    let total = 24u64;
    let compute_lane = |i: u64| i % 4 == 3;
    let (a, b) = (h.data(), h.data());
    for i in 1..=total {
        let (kind, queue, lane) = if compute_lane(i) {
            (D3D12_COMMAND_LIST_TYPE_COMPUTE, h.compute.clone(), 1)
        } else {
            (D3D12_COMMAND_LIST_TYPE_DIRECT, h.direct.clone(), 0)
        };
        let list = h.step(
            kind,
            compute_lane(i),
            if lane == 1 { &a } else { &b },
            i as u32,
        );
        Harness::run(&queue, &list);
        // SAFETY: our queues and fences, in the order the host numbers its signals.
        unsafe {
            queue.Signal(&h.fences[lane], i).expect("a signal");
            h.order.Wait(&h.fences[lane], i).expect("a wait");
            h.order.Signal(&h.fences[2], i).expect("a signal");
        }
    }
    let mut seen = 0u64;
    let started = std::time::Instant::now();
    while seen < total {
        assert!(
            started.elapsed() < std::time::Duration::from_secs(120),
            "the host's fence stands at {seen}"
        );
        // SAFETY: plain queries; a fence's value only goes up, so what is read after is at least what was.
        let (host, drawing, computing) = unsafe {
            (
                h.fences[2].GetCompletedValue(),
                h.fences[0].GetCompletedValue(),
                h.fences[1].GetCompletedValue(),
            )
        };
        assert!(host >= seen, "the host's fence goes back");
        seen = host;
        let last = |compute: bool| {
            (1..=host)
                .rev()
                .find(|&i| compute_lane(i) == compute)
                .unwrap_or(0)
        };
        assert!(
            drawing >= last(false),
            "the host's fence is at {host}, the drawing queue at {drawing}"
        );
        assert!(
            computing >= last(true),
            "the host's fence is at {host}, the compute queue at {computing}"
        );
        std::thread::yield_now();
    }
}

/// A random title through the book and the real queues: reads and writes of six buffers on two lanes, each
/// submission waiting for what the book says. Every read sees what the same submissions in the title's order
/// would give, and the queues never stand for each other.
#[test]
fn a_random_title_through_the_book_gives_what_the_order_gives() {
    let Some(mut h) = Harness::new() else { return };
    let mut dice = model_tests::Dice::new(0xfeed_f00d_1234_5678);
    let cells: Vec<ID3D12Resource> = (0..6).map(|_| h.data()).collect();
    let total = 300u64;
    let back = h.readback(total * 4);
    let mut book = Book::default();
    let mut value = [0u32; 6];
    let mut expected = vec![0u32; total as usize];
    let mut reads = Vec::new();
    let mut sent = 0u64;
    for i in 0..total {
        let title = dice.next(2) as usize;
        let cell = dice.next(6) as usize;
        let write = dice.next(3) != 0;
        let target = title;
        let mut fp = Footprint::default();
        if write {
            fp.write(cell * 64, cell * 64 + 4);
        } else {
            fp.read(cell * 64, cell * 64 + 4);
        }
        let fp = fp.sealed();
        // SAFETY: plain queries.
        let completed = [0, 1].map(|q| unsafe { h.fences[q].GetCompletedValue() });
        let want = book.waits_for(target, title, &[], &fp, completed, false);
        assert!(want <= book.signalled[1 - target]);
        let (queue, kind) = if target == 1 {
            (h.compute.clone(), D3D12_COMMAND_LIST_TYPE_COMPUTE)
        } else {
            (h.direct.clone(), D3D12_COMMAND_LIST_TYPE_DIRECT)
        };
        let list = if write {
            let v = i as u32 + 1;
            value[cell] = value[cell].wrapping_mul(3).wrapping_add(v);
            h.step(kind, false, &cells[cell], v)
        } else {
            expected[i as usize] = value[cell];
            reads.push(i as usize);
            h.copy(kind, &back, i, &cells[cell])
        };
        sent += 1;
        // SAFETY: our queues and fences; waits only for values queued as signals.
        unsafe {
            if want > 0 {
                queue.Wait(&h.fences[1 - target], want).expect("a wait");
            }
            queue.ExecuteCommandLists(&[Some(list)]);
            queue.Signal(&h.fences[target], sent).expect("a signal");
            h.order.Wait(&h.fences[target], sent).expect("a wait");
            h.order.Signal(&h.fences[2], sent).expect("a signal");
            let reason = h.device.GetDeviceRemovedReason();
            assert!(
                reason.is_ok(),
                "submission {i}: title {title}, cell {cell}, write {write}, target {target}, wait {want}: {reason:?}"
            );
        }
        book.sent(target, title, sent, fp);
        book.planned(title, i + 1);
    }
    assert!(
        h.reached(&h.fences[2], sent, 120_000),
        "the queues stand for each other"
    );
    let words = h.words(&back, total as usize);
    for &i in &reads {
        assert_eq!(words[i], expected[i], "slot {i}");
    }
    // The cells end where the order puts them.
    let finals = h.readback(24);
    for (n, cell) in cells.iter().enumerate() {
        let list = h.copy(D3D12_COMMAND_LIST_TYPE_DIRECT, &finals, n as u64, cell);
        Harness::run(&h.direct, &list);
    }
    // SAFETY: our queue and fence.
    unsafe { h.direct.Signal(&h.fences[0], sent + 1).expect("a signal") };
    assert!(h.reached(&h.fences[0], sent + 1, 60_000));
    assert_eq!(h.words(&finals, 6), value.to_vec());
}

/// A list runs on the queue of its own type, one of each, one after the other, and the device stays (a compute
/// list on the drawing queue removes it: the host's compute lists never go there).
#[test]
fn a_list_runs_on_the_queue_of_its_type() {
    let Some(mut h) = Harness::new() else { return };
    let data = h.data();
    let back = h.readback(16);
    let queues = [
        (h.direct.clone(), D3D12_COMMAND_LIST_TYPE_DIRECT),
        (h.compute.clone(), D3D12_COMMAND_LIST_TYPE_COMPUTE),
        (h.direct.clone(), D3D12_COMMAND_LIST_TYPE_DIRECT),
    ];
    for (n, (queue, kind)) in queues.iter().enumerate() {
        let write = h.step(*kind, false, &data, n as u32 + 1);
        Harness::run(queue, &write);
        // SAFETY: our queue and fence.
        unsafe { queue.Signal(&h.fences[0], n as u64 + 1).expect("a signal") };
        assert!(h.reached(&h.fences[0], n as u64 + 1, 60_000), "round {n}");
        // SAFETY: a plain query.
        let reason = unsafe { h.device.GetDeviceRemovedReason() };
        assert!(reason.is_ok(), "round {n}: {reason:?}");
    }
    let read = h.copy(D3D12_COMMAND_LIST_TYPE_DIRECT, &back, 0, &data);
    Harness::run(&h.direct, &read);
    // SAFETY: our queue and fence.
    unsafe { h.direct.Signal(&h.fences[0], 10).expect("a signal") };
    assert!(h.reached(&h.fences[0], 10, 60_000));
    // ((0 * 3 + 1) * 3 + 2) * 3 + 3
    assert_eq!(h.words(&back, 1), [18]);
}

/// A texture a drawing list left as a render target is moved out of that state on the drawing queue, in a list of
/// its own; the compute queue waits for it, writes the texture through an unordered access view in a compute list
/// (no state of drawing in it), and the drawing queue waits for that and reads the texture back: the hand-off.
#[test]
fn a_texture_goes_from_a_render_target_to_the_compute_queue_and_back() {
    let Some(mut h) = Harness::new() else { return };
    const TEXTURE_CS: &str = r#"
#define RS "DescriptorTable(UAV(u0, numDescriptors = 1))"
RWTexture2D<uint> image : register(u0);
[RootSignature(RS)]
[numthreads(1, 1, 1)]
void main() {
    image[uint2(0, 0)] = 42u;
    image[uint2(3, 3)] = 7u;
}
"#;
    let Some((root, pso)) = pipeline(&h.device, TEXTURE_CS) else {
        return;
    };
    let (render_target, unordered, copy_source) = (0x4, 0x8, 0x800);
    let desc = D3D12_RESOURCE_DESC {
        Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
        Width: 4,
        Height: 4,
        DepthOrArraySize: 1,
        MipLevels: 1,
        Format: DXGI_FORMAT_R32_UINT,
        SampleDesc: DXGI_SAMPLE_DESC {
            Count: 1,
            Quality: 0,
        },
        Flags: D3D12_RESOURCE_FLAGS(
            D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET.0
                | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS.0,
        ),
        ..Default::default()
    };
    let mut texture: Option<ID3D12Resource> = None;
    // SAFETY: plain creation; the texture starts as a drawing list leaves one.
    unsafe {
        h.device
            .CreateCommittedResource(
                &D3D12_HEAP_PROPERTIES {
                    Type: D3D12_HEAP_TYPE_DEFAULT,
                    ..Default::default()
                },
                D3D12_HEAP_FLAG_NONE,
                &desc,
                D3D12_RESOURCE_STATES(render_target),
                None,
                &mut texture,
            )
            .expect("a texture");
    }
    let texture = texture.expect("a texture");
    // SAFETY: plain creation of a heap with one view of the texture.
    let heap: ID3D12DescriptorHeap = unsafe {
        let heap = h
            .device
            .CreateDescriptorHeap::<ID3D12DescriptorHeap>(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 1,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                NodeMask: 0,
            })
            .expect("a heap");
        let view = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R32_UINT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
            Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
                Texture2D: D3D12_TEX2D_UAV {
                    MipSlice: 0,
                    PlaneSlice: 0,
                },
            },
        };
        h.device.CreateUnorderedAccessView(
            &texture,
            None::<&ID3D12Resource>,
            Some(&view),
            heap.GetCPUDescriptorHandleForHeapStart(),
        );
        heap
    };
    let back = h.readback(256 * 4);
    // The hand-off, as the host makes it: the texture out of the render target state, on the drawing queue.
    let hand = h.list(D3D12_COMMAND_LIST_TYPE_DIRECT, |l| {
        // SAFETY: an open list and a live texture.
        unsafe {
            l.ResourceBarrier(&[transition(
                &texture,
                D3D12_RESOURCE_STATES(render_target),
                D3D12_RESOURCE_STATES(unordered),
            )])
        }
    });
    // The compute list: the states it knows are the compute ones.
    let write = h.list(D3D12_COMMAND_LIST_TYPE_COMPUTE, |l| {
        // SAFETY: an open list and live objects.
        unsafe {
            l.SetDescriptorHeaps(&[Some(heap.clone())]);
            l.SetComputeRootSignature(&root);
            l.SetPipelineState(&pso);
            l.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
            l.Dispatch(1, 1, 1);
        }
    });
    // Back on the drawing queue: out of the compute state, to be read.
    let read = h.list(D3D12_COMMAND_LIST_TYPE_DIRECT, |l| {
        // SAFETY: an open list and live objects.
        unsafe {
            l.ResourceBarrier(&[transition(
                &texture,
                D3D12_RESOURCE_STATES(unordered),
                D3D12_RESOURCE_STATES(copy_source),
            )]);
            let to = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&back),
                Type: D3D12_TEXTURE_COPY_TYPE_PLACED_FOOTPRINT,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    PlacedFootprint: D3D12_PLACED_SUBRESOURCE_FOOTPRINT {
                        Offset: 0,
                        Footprint: D3D12_SUBRESOURCE_FOOTPRINT {
                            Format: DXGI_FORMAT_R32_UINT,
                            Width: 4,
                            Height: 4,
                            Depth: 1,
                            RowPitch: 256,
                        },
                    },
                },
            };
            let from = D3D12_TEXTURE_COPY_LOCATION {
                pResource: std::mem::transmute_copy(&texture),
                Type: D3D12_TEXTURE_COPY_TYPE_SUBRESOURCE_INDEX,
                Anonymous: D3D12_TEXTURE_COPY_LOCATION_0 {
                    SubresourceIndex: 0,
                },
            };
            l.CopyTextureRegion(&to, 0, 0, 0, &from, None);
        }
    });
    Harness::run(&h.direct, &hand);
    // SAFETY: our queues and fences.
    unsafe {
        h.direct.Signal(&h.fences[2], 1).expect("a signal");
        h.compute.Wait(&h.fences[2], 1).expect("a wait");
    }
    Harness::run(&h.compute, &write);
    // SAFETY: as above.
    unsafe {
        h.compute.Signal(&h.fences[1], 1).expect("a signal");
        h.direct.Wait(&h.fences[1], 1).expect("a wait");
    }
    Harness::run(&h.direct, &read);
    // SAFETY: as above.
    unsafe { h.direct.Signal(&h.fences[0], 1).expect("a signal") };
    assert!(
        h.reached(&h.fences[0], 1, 60_000),
        "the queues stand for each other"
    );
    // SAFETY: a plain query.
    let reason = unsafe { h.device.GetDeviceRemovedReason() };
    assert!(reason.is_ok(), "{reason:?}");
    let words = h.words(&back, 256);
    // (Row 0 starts at word 0, row 3 at word 3 * 64.)
    assert_eq!((words[0], words[3 * 64 + 3]), (42, 7));
}

/// What a submission of Q2 writes of host copy 7, as the book keeps it.
fn writes_copy(id: u64) -> Footprint {
    let mut fp = Footprint::default();
    fp.card_write(id, 0, u32::MAX);
    fp.sealed()
}

/// The landing thread's fetch of a card copy (a read of the copy) on the drawing queue, with the wait on the
/// compute queue the book names: compute writes (some of them slow), and the fetch reads after the right wait, so
/// it sees every write that was in the air when it was made; a fetch of another copy waits for none.
#[test]
fn a_fetch_of_a_card_copy_reads_after_the_compute_queue_writes_it() {
    let Some(mut h) = Harness::new() else { return };
    let rounds = 12u64;
    let copy = h.data();
    let back = h.readback(rounds * 4);
    // SAFETY: plain creation.
    let fetch_fence: ID3D12Fence = unsafe { h.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.expect("a fence");
    let mut book = Book::default();
    let mut expected: Vec<u32> = Vec::new();
    for round in 1..=rounds {
        let value = round as u32 * 5 + 2;
        let slow = round % 3 != 0;
        // The value of the copy once this write has run (the slow one stores a number of its own).
        expected.push(if slow {
            slow_result(value)
        } else {
            expected.last().copied().unwrap_or(0).wrapping_mul(3).wrapping_add(value)
        });
        // The submission is in the air as the route says it ([`Route::waits`], then [`Route::signal`]).
        let fp = writes_copy(7);
        book.announce(1, round, &fp);
        let write = h.step(D3D12_COMMAND_LIST_TYPE_COMPUTE, slow, &copy, value);
        Harness::run(&h.compute, &write);
        // SAFETY: our queue and fence.
        unsafe { h.compute.Signal(&h.fences[1], round).expect("a signal") };
        book.sent(1, 1, round, fp);
        // The fetch, on the drawing queue (not ordered against the compute queue but by the book).
        let (other, same) = (
            book.fetching(&[8], 0, &fetch_fence, round * 10),
            book.fetching(&[3, 7], 0, &fetch_fence, round * 10 + 1),
        );
        assert_eq!(other, 0, "a copy nothing of the compute queue writes");
        assert_eq!(same, round, "the newest write of the copy in the air");
        let read = h.copy(D3D12_COMMAND_LIST_TYPE_DIRECT, &back, round - 1, &copy);
        // SAFETY: our queues and fences; the wait is for a value queued as a signal.
        unsafe {
            h.direct.Wait(&h.fences[1], same).expect("a wait");
        }
        Harness::run(&h.direct, &read);
        // SAFETY: as above.
        unsafe { h.direct.Signal(&fetch_fence, round * 10 + 1).expect("a signal") };
        assert!(h.reached(&fetch_fence, round * 10 + 1, 120_000), "round {round}: the queues stand for each other");
        // SAFETY: a plain query.
        assert!(unsafe { h.device.GetDeviceRemovedReason() }.is_ok());
    }
    assert_eq!(h.words(&back, rounds as usize), expected);
}

/// A submission of Q2 that comes after the fetch and writes what it reads waits for the fetch's fence: the fetch
/// (behind a slow dispatch on the drawing queue) sees what was there, not what the later write makes; a write of
/// another copy waits for nothing, and a fetch that is done is forgotten.
#[test]
fn a_later_compute_write_of_a_copy_waits_for_the_fetch_that_reads_it() {
    let Some(mut h) = Harness::new() else { return };
    let rounds = 8u64;
    let copy = h.data();
    let back = h.readback(rounds * 4);
    // SAFETY: plain creation.
    let fetch_fence: ID3D12Fence = unsafe { h.device.CreateFence(0, D3D12_FENCE_FLAG_NONE) }.expect("a fence");
    let mut book = Book::default();
    let mut expected = Vec::new();
    let mut now = 0u32;
    for round in 1..=rounds {
        // What is in the copy when the fetch reads it: a slow write on the drawing queue first.
        let before = round as u32 * 11 + 3;
        let slow = h.step(D3D12_COMMAND_LIST_TYPE_DIRECT, true, &copy, before);
        Harness::run(&h.direct, &slow);
        expected.push(slow_result(before));
        // The fetch is made (noted in the book), and reads right behind it, once the CPU has looked at the book
        // (the gate: the fetch cannot be done before).
        let fetched = round * 2;
        assert_eq!(book.fetching(&[7], 0, &fetch_fence, fetched), 0, "nothing of the compute queue in the air");
        let read = h.copy(D3D12_COMMAND_LIST_TYPE_DIRECT, &back, round - 1, &copy);
        // SAFETY: our queue and fences.
        unsafe { h.direct.Wait(&h.fences[2], round).expect("a wait") };
        Harness::run(&h.direct, &read);
        // SAFETY: as above.
        unsafe { h.direct.Signal(&fetch_fence, fetched).expect("a signal") };
        // A write of the copy on the compute queue, made while the fetch is not done: it waits for it.
        let write = writes_copy(7);
        let waits = book.fetches_for(&write);
        assert_eq!(waits.len(), 1, "round {round}");
        assert_eq!(waits[0].1, fetched);
        assert!(book.fetches_for(&writes_copy(8)).is_empty(), "another copy waits for nothing");
        let after = h.step(D3D12_COMMAND_LIST_TYPE_COMPUTE, false, &copy, round as u32);
        // SAFETY: our queue; the fence is signalled behind the fetch's list.
        unsafe { h.compute.Wait(&waits[0].0, waits[0].1).expect("a wait") };
        Harness::run(&h.compute, &after);
        // SAFETY: our queue and fence; the CPU opens the gate.
        unsafe {
            h.compute.Signal(&h.fences[1], round).expect("a signal");
            h.fences[2].Signal(round).expect("the gate");
        }
        assert!(h.reached(&h.fences[1], round, 120_000), "round {round}: the queues stand for each other");
        now = slow_result(before).wrapping_mul(3).wrapping_add(round as u32);
        // Done fetches are forgotten.
        assert!(h.reached(&fetch_fence, fetched, 120_000));
        assert!(book.fetches_for(&write).is_empty(), "a fetch that is done is not waited for");
    }
    assert_eq!(h.words(&back, rounds as usize), expected);
    // The last write the compute queue made is what the copy holds now.
    let finals = h.readback(4);
    let read = h.copy(D3D12_COMMAND_LIST_TYPE_DIRECT, &finals, 0, &copy);
    Harness::run(&h.direct, &read);
    // SAFETY: our queue and fence.
    unsafe { h.direct.Signal(&h.fences[0], 1).expect("a signal") };
    assert!(h.reached(&h.fences[0], 1, 60_000));
    assert_eq!(h.words(&finals, 1), [now]);
}
