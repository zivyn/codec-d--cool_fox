//! Ported from fxceless/victory-fork PR #1189 (Series S interface plane), `VICTORY_COMPOSE=1`.
//! A present's second plane laid over its first.
//!
//! A Series S title presents two planes (run 2026-11-25: plane 0 the
//! game, 1920x1080 or what the display mode makes it, plane 1 the
//! interface, 3840x2160); the console's display lays the second over the
//! first and scales both to the screen. The host shows one texture, so
//! the interface is laid over the game here, at the game plane's size,
//! into a texture of ours that is shown instead (filtered: the interface
//! is scaled down). shortcut: the planes' placement and source rectangles
//! are not read; both cover the screen. The interface's alpha is taken as
//! straight (`VICTORY_PLANE_PREMULTIPLIED=1`: premultiplied).

use std::sync::Mutex;

use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;

/// Frames of descriptors in the ring (three each): presents in flight
/// must not find theirs rewritten.
const SLOTS: u32 = 16;

const SOURCE: &str = r#"
#define RS "RootConstants(num32BitConstants=3, b0), DescriptorTable(SRV(t0, numDescriptors=2), UAV(u0)), StaticSampler(s0, filter=FILTER_MIN_MAG_MIP_LINEAR, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP)"
cbuffer Size : register(b0) { uint w; uint h; uint premultiplied; };
Texture2D<float4> game : register(t0);
Texture2D<float4> ui : register(t1);
RWTexture2D<float4> outp : register(u0);
SamplerState linear_clamp : register(s0);
[RootSignature(RS)]
[numthreads(8, 8, 1)]
void main(uint3 id : SV_DispatchThreadID) {
    if (id.x >= w || id.y >= h) return;
    float2 uv = (float2(id.xy) + 0.5) / float2(w, h);
    float4 g = game.Load(int3(id.xy, 0));
    float4 u = ui.SampleLevel(linear_clamp, uv, 0);
    float3 c = premultiplied != 0 ? g.rgb * (1.0 - u.a) + u.rgb : lerp(g.rgb, u.rgb, u.a);
    outp[id.xy] = float4(c, 1.0);
}
"#;

struct Pass {
    root: ID3D12RootSignature,
    pso: ID3D12PipelineState,
    views: ID3D12DescriptorHeap,
    cpu: usize,
    gpu: u64,
    step: u32,
    next: u32,
    /// The texture shown instead, by size.
    out: Option<(u64, u32, ID3D12Resource)>,
}
// SAFETY: D3D12 objects are free-threaded; the pass is used under a lock.
unsafe impl Send for Pass {}

/// The pass: not tried yet, could not be made, or made.
static PASS: Mutex<Option<Option<Pass>>> = Mutex::new(None);

fn make(device: &ID3D12Device) -> windows::core::Result<Pass> {
    let mut code: Option<ID3DBlob> = None;
    // SAFETY: the source and names live through the call; the out pointer
    // is ours.
    unsafe {
        D3DCompile(
            SOURCE.as_ptr().cast(),
            SOURCE.len(),
            windows::core::s!("present planes"),
            None,
            None,
            windows::core::s!("main"),
            windows::core::s!("cs_5_1"),
            0,
            0,
            &mut code,
            None,
        )?;
    }
    let code = code.ok_or_else(windows::core::Error::empty)?;
    // SAFETY: the blob's own bytes (the root signature is inside the
    // shader); plain object creation and queries.
    unsafe {
        let bytes = std::slice::from_raw_parts(code.GetBufferPointer().cast::<u8>(), code.GetBufferSize());
        let root: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
        let pso: ID3D12PipelineState = device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
            pRootSignature: std::mem::transmute_copy(&root),
            CS: D3D12_SHADER_BYTECODE {
                pShaderBytecode: bytes.as_ptr().cast(),
                BytecodeLength: bytes.len(),
            },
            ..Default::default()
        })?;
        let kind = D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV;
        let views: ID3D12DescriptorHeap = device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
            Type: kind,
            NumDescriptors: SLOTS * 3,
            Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
            NodeMask: 0,
        })?;
        Ok(Pass {
            cpu: views.GetCPUDescriptorHandleForHeapStart().ptr,
            gpu: views.GetGPUDescriptorHandleForHeapStart().ptr,
            step: device.GetDescriptorHandleIncrementSize(kind),
            root,
            pso,
            views,
            next: 0,
            out: None,
        })
    }
}

/// Whether a host texture's format is one this pass reads and writes
/// (8-bit RGBA, typeless or not).
fn rgba8(format: DXGI_FORMAT) -> bool {
    matches!(format.0, 27 | 28)
}

fn premultiplied() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_PLANE_PREMULTIPLIED").is_some_and(|v| v == "1"))
}

/// Records, into the present's `list`, `ui` laid over `game` into a
/// texture of ours, and gives that texture, left in the copy-source
/// state. `game` is in the copy-source state (the present put it there);
/// `ui` is in `ui_state` and is left in it. `None` (nothing recorded) if
/// a format is not 8-bit RGBA or the pass could not be made.
pub(crate) fn record(
    device: &ID3D12Device,
    list: &ID3D12GraphicsCommandList,
    game: &ID3D12Resource,
    ui: &ID3D12Resource,
    ui_state: u32,
) -> Option<ID3D12Resource> {
    // SAFETY: live resources; plain queries.
    let (gd, ud) = unsafe { (game.GetDesc(), ui.GetDesc()) };
    if !rgba8(gd.Format) || !rgba8(ud.Format) {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| {
            tracing::warn!(game = gd.Format.0, ui = ud.Format.0, "present planes not laid over each other: a format this pass does not read");
        });
        return None;
    }
    let mut kept = PASS.lock().unwrap_or_else(|e| e.into_inner());
    if kept.is_none() {
        *kept = Some(match make(device) {
            Ok(p) => {
                tracing::info!(premultiplied = premultiplied(), "present planes: the interface is laid over the game");
                Some(p)
            }
            Err(e) => {
                tracing::warn!(error = %e, "present planes: the pass that lays them over each other could not be made");
                None
            }
        });
    }
    let pass = kept.as_mut()?.as_mut()?;
    let (w, h) = (gd.Width, gd.Height);
    if pass.out.as_ref().is_none_or(|o| (o.0, o.1) != (w, h)) {
        let heap = D3D12_HEAP_PROPERTIES {
            Type: D3D12_HEAP_TYPE_DEFAULT,
            ..Default::default()
        };
        let desc = D3D12_RESOURCE_DESC {
            Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE2D,
            Width: w,
            Height: h,
            DepthOrArraySize: 1,
            MipLevels: 1,
            Format: DXGI_FORMAT_R8G8B8A8_UNORM,
            SampleDesc: DXGI_SAMPLE_DESC { Count: 1, Quality: 0 },
            Layout: D3D12_TEXTURE_LAYOUT_UNKNOWN,
            Flags: D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            ..Default::default()
        };
        let mut out: Option<ID3D12Resource> = None;
        // SAFETY: plain creation of a device-local texture.
        unsafe {
            device
                .CreateCommittedResource(&heap, D3D12_HEAP_FLAG_NONE, &desc, D3D12_RESOURCE_STATE_COPY_SOURCE, None, &mut out)
                .ok()?;
        }
        pass.out = Some((w, h, out?));
    }
    let out = pass.out.as_ref()?.2.clone();
    let first = (pass.next % SLOTS) * 3;
    pass.next = pass.next.wrapping_add(1);
    let cpu = |i: u32| D3D12_CPU_DESCRIPTOR_HANDLE { ptr: pass.cpu + ((first + i) * pass.step) as usize };
    let gpu = D3D12_GPU_DESCRIPTOR_HANDLE { ptr: pass.gpu + u64::from(first * pass.step) };
    let srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_SRV_DIMENSION_TEXTURE2D,
        Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
        Anonymous: D3D12_SHADER_RESOURCE_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_SRV {
                MostDetailedMip: 0,
                MipLevels: 1,
                ..Default::default()
            },
        },
    };
    let uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
        Format: DXGI_FORMAT_R8G8B8A8_UNORM,
        ViewDimension: D3D12_UAV_DIMENSION_TEXTURE2D,
        Anonymous: D3D12_UNORDERED_ACCESS_VIEW_DESC_0 {
            Texture2D: D3D12_TEX2D_UAV { MipSlice: 0, PlaneSlice: 0 },
        },
    };
    let read = D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE;
    let ui_state = D3D12_RESOURCE_STATES(ui_state as i32);
    let t = crate::host::transition;
    let mut before = vec![
        t(game, D3D12_RESOURCE_STATE_COPY_SOURCE, read),
        t(&out, D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_UNORDERED_ACCESS),
    ];
    let mut after = vec![
        t(game, read, D3D12_RESOURCE_STATE_COPY_SOURCE),
        t(&out, D3D12_RESOURCE_STATE_UNORDERED_ACCESS, D3D12_RESOURCE_STATE_COPY_SOURCE),
    ];
    if ui_state != read {
        before.push(t(ui, ui_state, read));
        after.push(t(ui, read, ui_state));
    }
    let constants: [u32; 3] = [w as u32, h, u32::from(premultiplied())];
    // SAFETY: an open list (the present's); slots of our heap; live
    // resources and objects (the pass's own live as long as the process,
    // the texture is kept by the present's list).
    unsafe {
        device.CreateShaderResourceView(game, Some(&srv), cpu(0));
        device.CreateShaderResourceView(ui, Some(&srv), cpu(1));
        device.CreateUnorderedAccessView(&out, None::<&ID3D12Resource>, Some(&uav), cpu(2));
        list.ResourceBarrier(&before);
        list.SetDescriptorHeaps(&[Some(pass.views.clone())]);
        list.SetComputeRootSignature(&pass.root);
        list.SetPipelineState(&pass.pso);
        list.SetComputeRoot32BitConstants(0, 3, constants.as_ptr().cast(), 0);
        list.SetComputeRootDescriptorTable(1, gpu);
        list.Dispatch((w as u32).div_ceil(8), h.div_ceil(8), 1);
        list.ResourceBarrier(&after);
    }
    Some(out)
}
