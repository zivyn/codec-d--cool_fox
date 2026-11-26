//! Does the card filter a 33^3 RGBA16F Texture3D (render target and unordered
//! access, written by a compute pass) through a linear static sampler, as
//! PS_CompositeNMB reads its colour-grading tables? Prints 64 samples along
//! the diagonal of three cells, with the linear and the nearest value.
//!
//!     cargo run --release -p victory-gpu --example lut3d_probe
//!
//! Switches (environment), each a way the layer's use differs from the plain case:
//! - `SM66=1`: the sampling program as the layer's are, SM 6.6, the texture read
//!   from the heap by index, the root flags of the layer's root signatures and the
//!   composite's three static samplers (point, linear, trilinear; compiled with
//!   `dxc.exe`, `DXC=<path>` or the Windows SDK's).
//! - `LIB66_STORE=<file>` / `LIB66_LOAD=<file>` (with `SM66`): that pipeline stored
//!   in a pipeline library written to the file / loaded from it.
//! - `SHIFT=<n>`: n other static samplers made (and kept) first. `SWAP66=1`: a root
//!   signature with the composite's samplers in another order made first.
//! - `SAMPLERS=<n>`, `PAST=<k>`: a bound heap of n point samplers, k written past it.
//! - `KEEP_UAV=1`: the texture left in the unordered access state.
//! - `MANY_ROOTS=<n>` (`MANY_SAME`, `MANY_DROP`, `MANY_TWIN`, `BIND_MANY`): n more
//!   root signatures with static samplers (the card's table holds 2,032 distinct
//!   ones: the 2,032nd removes the device with DXGI_ERROR_DRIVER_INTERNAL_ERROR).
//! - `BG_COMMIT=1`: the driver's background recompiles finished first.
//!
//! Found with it (2026-11-26, 4070 SUPER): every case filters, except a pipeline
//! loaded from a library in a process that made other static samplers first
//! (`SM66=1 LIB66_STORE=a.lib`, then `SM66=1 SHIFT=10 LIB66_LOAD=a.lib`): every
//! sample is a table entry. See `pipeline::library::with_static_samplers`.
use windows::Win32::Graphics::Direct3D::Fxc::D3DCompile;
use windows::Win32::Graphics::Direct3D::{D3D_FEATURE_LEVEL_12_0, ID3DBlob};
use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::Common::*;
use windows::Win32::Graphics::Dxgi::*;
use windows::Win32::System::Threading::{CreateEventW, INFINITE, WaitForSingleObject};
use windows::core::{Interface, PCSTR, s};

const SRC: &str = r#"
#define RS "DescriptorTable(UAV(u0), SRV(t0)), UAV(u1), RootConstants(num32BitConstants=4, b0), StaticSampler(s0, filter=FILTER_MIN_MAG_LINEAR_MIP_POINT, addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP, maxLOD=12)"
RWTexture3D<float4> lut : register(u0);
Texture3D<float4> tex : register(t0);
SamplerState smp : register(s0);
RWStructuredBuffer<float4> outb : register(u1);
cbuffer C : register(b0) { uint mode; uint a; uint b; uint c; };
[RootSignature(RS)] [numthreads(1, 1, 1)]
void fill(uint3 id : SV_DispatchThreadID) { lut[id] = float4(id.x / 32.0, id.y / 32.0, id.z / 32.0, 1); }
[RootSignature(RS)] [numthreads(64, 1, 1)]
void probe(uint3 id : SV_DispatchThreadID) {
    float u = (id.x + 0.5) / 64.0 * (3.0 / 33.0) + 10.0 / 33.0;
    outb[id.x] = tex.SampleLevel(smp, float3(u, u, u), 0);
}
"#;

/// The composite's way: SM 6.6, the texture from the heap (slot 1), its three
/// static samplers alike but for the filter.
const SRC66: &str = r#"
#define SS(r, f) "StaticSampler(" r ", filter=" f ", addressU=TEXTURE_ADDRESS_CLAMP, addressV=TEXTURE_ADDRESS_CLAMP, addressW=TEXTURE_ADDRESS_CLAMP, maxAnisotropy=1, comparisonFunc=COMPARISON_NEVER, borderColor=STATIC_BORDER_COLOR_TRANSPARENT_BLACK, minLOD=0, maxLOD=12)"
#define RS "RootFlags(CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED | SAMPLER_HEAP_DIRECTLY_INDEXED), UAV(u1), " SS("s0", "FILTER_MIN_MAG_MIP_POINT") ", " SS("s1", "FILTER_MIN_MAG_LINEAR_MIP_POINT") ", " SS("s2", "FILTER_MIN_MAG_MIP_LINEAR")
SamplerState p0 : register(s0);
SamplerState p1 : register(s1);
SamplerState p2 : register(s2);
RWStructuredBuffer<float4> outb : register(u1);
[RootSignature(RS)] [numthreads(64, 1, 1)]
void probe(uint3 id : SV_DispatchThreadID) {
    Texture3D<float4> tex = ResourceDescriptorHeap[1];
    float u = (id.x + 0.5) / 64.0 * (3.0 / 33.0) + 10.0 / 33.0;
    float4 a = tex.SampleLevel(p0, float3(0.5, 0.5, 0.5), 0);
    float4 b = tex.SampleLevel(p1, float3(u, u, u), 0);
    float4 c = tex.SampleLevel(p2, float3(0.5, u, 0.5), 0);
    outb[id.x] = float4(b.x, b.y, b.z, a.x * 0.0 + c.y * 0.0 + b.w);
}
"#;

/// `source` compiled by dxc.exe for compute, entry `probe`.
fn dxc(source: &str) -> Vec<u8> {
    let exe = std::env::var_os("DXC").map_or_else(
        || {
            std::path::PathBuf::from(
                r"C:\Program Files (x86)\Windows Kits\10\bin\10.0.26100.0\x64\dxc.exe",
            )
        },
        std::path::PathBuf::from,
    );
    let dir = std::env::temp_dir();
    let (hlsl, cso) = (
        dir.join("lut3d_probe66.hlsl"),
        dir.join("lut3d_probe66.cso"),
    );
    std::fs::write(&hlsl, source).expect("hlsl written");
    let out = std::process::Command::new(exe)
        .args(["-T", "cs_6_6", "-E", "probe", "-Fo"])
        .arg(&cso)
        .arg(&hlsl)
        .output()
        .expect("dxc.exe runs");
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    std::fs::read(&cso).expect("cso")
}

fn compile(entry: PCSTR) -> Vec<u8> {
    let mut code: Option<ID3DBlob> = None;
    let mut errors: Option<ID3DBlob> = None;
    unsafe {
        let r = D3DCompile(
            SRC.as_ptr().cast(),
            SRC.len(),
            s!("probe"),
            None,
            None,
            entry,
            s!("cs_5_1"),
            0,
            0,
            &mut code,
            Some(&mut errors),
        );
        if r.is_err() {
            let e = errors.unwrap();
            panic!(
                "{}",
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    e.GetBufferPointer().cast::<u8>(),
                    e.GetBufferSize()
                ))
            );
        }
        let c = code.unwrap();
        std::slice::from_raw_parts(c.GetBufferPointer().cast::<u8>(), c.GetBufferSize()).to_vec()
    }
}

fn main() -> windows::core::Result<()> {
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0))?;
        let mut device: Option<ID3D12Device> = None;
        for i in 0.. {
            let Ok(adapter) = factory.EnumAdapters1(i) else {
                break;
            };
            let desc = adapter.GetDesc1()?;
            let name = String::from_utf16_lossy(&desc.Description);
            if name.contains("NVIDIA") {
                println!("adapter {}", name.trim_end_matches('\0'));
                D3D12CreateDevice(&adapter, D3D_FEATURE_LEVEL_12_0, &mut device)?;
                break;
            }
        }
        let device = device.expect("an NVIDIA adapter");
        let fill = compile(s!("fill"));
        let probe = compile(s!("probe"));
        // SHIFT=<n>: n root signatures with static point samplers of their own made (and
        // kept) before the probe's, so its sampler lands elsewhere in the card's table.
        let mut shifted: Vec<ID3D12RootSignature> = Vec::new();
        let shift = std::env::var("SHIFT")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
            .unwrap_or(0);
        for i in 0..shift {
            let sampler = D3D12_STATIC_SAMPLER_DESC {
                Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
                AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                MipLODBias: i as f32 / 64.0,
                MaxAnisotropy: 1,
                MaxLOD: 12.0,
                ..Default::default()
            };
            let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
                Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
                Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                    Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                        NumStaticSamplers: 1,
                        pStaticSamplers: &sampler,
                        ..Default::default()
                    },
                },
            };
            let mut blob: Option<ID3DBlob> = None;
            D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
            let blob = blob.unwrap();
            let bytes = std::slice::from_raw_parts(
                blob.GetBufferPointer().cast::<u8>(),
                blob.GetBufferSize(),
            );
            shifted.push(device.CreateRootSignature(0, bytes)?);
        }
        if shift > 0 {
            println!("{shift} root signatures with other samplers made first");
        }
        let root: ID3D12RootSignature = device.CreateRootSignature(0, &fill)?;
        let pso = |code: &[u8]| -> windows::core::Result<ID3D12PipelineState> {
            device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            })
        };
        let fill_pso = pso(&fill)?;
        // LIB_STORE=<file>: the probe's pipeline stored in a pipeline library written
        // there; LIB_LOAD=<file>: the probe's pipeline loaded from such a library.
        let device1: ID3D12Device1 = device.cast()?;
        let probe_pso = if let Some(path) = std::env::var_os("LIB_LOAD") {
            let bytes = std::fs::read(&path).expect("library file");
            let library: ID3D12PipelineLibrary = device1.CreatePipelineLibrary(&bytes)?;
            let desc = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: probe.as_ptr().cast(),
                    BytecodeLength: probe.len(),
                },
                ..Default::default()
            };
            let p: ID3D12PipelineState =
                library.LoadComputePipeline(windows::core::w!("probe"), &desc)?;
            println!("probe pipeline loaded from the library");
            std::mem::forget(bytes); // the library points into them
            std::mem::forget(library);
            p
        } else {
            let p = pso(&probe)?;
            if let Some(path) = std::env::var_os("LIB_STORE") {
                let library: ID3D12PipelineLibrary = device1.CreatePipelineLibrary(&[])?;
                library.StorePipeline(windows::core::w!("probe"), &p)?;
                let mut bytes = vec![0u8; library.GetSerializedSize()];
                library.Serialize(&mut bytes)?;
                std::fs::write(&path, &bytes).expect("library written");
                println!("probe pipeline stored ({} bytes)", bytes.len());
            }
            p
        };
        // MANY_ROOTS=<n>: n more root signatures, each with one static point sampler of
        // its own (a distinct LOD bias), and a pipeline for each; MANY_SAME=1: all alike.
        let mut many: Vec<(ID3D12RootSignature, ID3D12PipelineState)> = Vec::new();
        if let Some(n) = std::env::var("MANY_ROOTS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        {
            let same = std::env::var_os("MANY_SAME").is_some();
            let nop_src = "[numthreads(1, 1, 1)] void nop() {}";
            let mut code: Option<ID3DBlob> = None;
            D3DCompile(
                nop_src.as_ptr().cast(),
                nop_src.len(),
                s!("nop"),
                None,
                None,
                s!("nop"),
                s!("cs_5_1"),
                0,
                0,
                &mut code,
                None,
            )?;
            let code = code.unwrap();
            let nop = std::slice::from_raw_parts(
                code.GetBufferPointer().cast::<u8>(),
                code.GetBufferSize(),
            )
            .to_vec();
            // MANY_TWIN=k: first k root signatures with a sampler just like the probe's
            // (as the HLSL string gives it), made and let go.
            let twins = std::env::var("MANY_TWIN")
                .ok()
                .and_then(|v| v.parse::<u32>().ok())
                .unwrap_or(0);
            for _ in 0..twins {
                let sampler = D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER_MIN_MAG_LINEAR_MIP_POINT,
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    MipLODBias: 0.0,
                    MaxAnisotropy: 16,
                    ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
                    BorderColor: D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE,
                    MinLOD: 0.0,
                    MaxLOD: 12.0,
                    ShaderRegister: 0,
                    RegisterSpace: 0,
                    ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
                };
                let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
                    Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
                    Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                        Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                            NumStaticSamplers: 1,
                            pStaticSamplers: &sampler,
                            ..Default::default()
                        },
                    },
                };
                let mut blob: Option<ID3DBlob> = None;
                D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
                let blob = blob.unwrap();
                let bytes = std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                );
                let twin: ID3D12RootSignature = device.CreateRootSignature(0, bytes)?;
                drop(twin);
            }
            if twins > 0 {
                println!("made and let go {twins} root signatures with the probe's sampler");
            }
            for i in 0..n {
                let sampler = D3D12_STATIC_SAMPLER_DESC {
                    Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
                    MipLODBias: if same { 0.0 } else { (i % 1000) as f32 / 100.0 },
                    MaxAnisotropy: 1,
                    MinLOD: if same { 0.0 } else { (i / 1000) as f32 },
                    MaxLOD: 12.0,
                    ShaderRegister: 0,
                    ..Default::default()
                };
                let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
                    Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
                    Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                        Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                            NumStaticSamplers: 1,
                            pStaticSamplers: &sampler,
                            ..Default::default()
                        },
                    },
                };
                let mut blob: Option<ID3DBlob> = None;
                D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
                let blob = blob.unwrap();
                let bytes = std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                );
                let r: ID3D12RootSignature = match device.CreateRootSignature(0, bytes) {
                    Ok(r) => r,
                    Err(e) => {
                        println!(
                            "root signature {i} refused: {e:?} removed {:?}",
                            device.GetDeviceRemovedReason()
                        );
                        return Ok(());
                    }
                };
                let p: ID3D12PipelineState =
                    match device.CreateComputePipelineState(&D3D12_COMPUTE_PIPELINE_STATE_DESC {
                        pRootSignature: std::mem::transmute_copy(&r),
                        CS: D3D12_SHADER_BYTECODE {
                            pShaderBytecode: nop.as_ptr().cast(),
                            BytecodeLength: nop.len(),
                        },
                        ..Default::default()
                    }) {
                        Ok(p) => p,
                        Err(e) => {
                            println!(
                                "pipeline {i} refused: {e:?} removed {:?}",
                                device.GetDeviceRemovedReason()
                            );
                            return Ok(());
                        }
                    };
                // MANY_DROP=1: each released at once (only made and let go).
                if std::env::var_os("MANY_DROP").is_none() {
                    many.push((r, p));
                }
            }
            println!("made {n} more root signatures with static point samplers (alike: {same})");
        }
        let heap_props = |t| D3D12_HEAP_PROPERTIES {
            Type: t,
            ..Default::default()
        };
        let mut tex: Option<ID3D12Resource> = None;
        device.CreateCommittedResource(
            &heap_props(D3D12_HEAP_TYPE_DEFAULT),
            D3D12_HEAP_FLAG_NONE,
            &D3D12_RESOURCE_DESC {
                Dimension: D3D12_RESOURCE_DIMENSION_TEXTURE3D,
                Width: 33,
                Height: 33,
                DepthOrArraySize: 33,
                MipLevels: 1,
                Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
                SampleDesc: DXGI_SAMPLE_DESC {
                    Count: 1,
                    Quality: 0,
                },
                Flags: D3D12_RESOURCE_FLAG_ALLOW_RENDER_TARGET
                    | D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
                ..Default::default()
            },
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
            None,
            &mut tex,
        )?;
        let tex = tex.unwrap();
        let buffer = |t, bytes: u64, flags, state| -> windows::core::Result<ID3D12Resource> {
            let mut b: Option<ID3D12Resource> = None;
            device.CreateCommittedResource(
                &heap_props(t),
                D3D12_HEAP_FLAG_NONE,
                &D3D12_RESOURCE_DESC {
                    Dimension: D3D12_RESOURCE_DIMENSION_BUFFER,
                    Width: bytes,
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
                },
                state,
                None,
                &mut b,
            )?;
            Ok(b.unwrap())
        };
        let out = buffer(
            D3D12_HEAP_TYPE_DEFAULT,
            64 * 16,
            D3D12_RESOURCE_FLAG_ALLOW_UNORDERED_ACCESS,
            D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
        )?;
        let back = buffer(
            D3D12_HEAP_TYPE_READBACK,
            64 * 16,
            D3D12_RESOURCE_FLAG_NONE,
            D3D12_RESOURCE_STATE_COPY_DEST,
        )?;
        let heap: ID3D12DescriptorHeap =
            device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                Type: D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV,
                NumDescriptors: 2,
                Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                ..Default::default()
            })?;
        let step = device.GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_CBV_SRV_UAV)
            as usize;
        let cpu = heap.GetCPUDescriptorHandleForHeapStart();
        let mut uav = D3D12_UNORDERED_ACCESS_VIEW_DESC {
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            ViewDimension: D3D12_UAV_DIMENSION_TEXTURE3D,
            ..Default::default()
        };
        uav.Anonymous.Texture3D = D3D12_TEX3D_UAV {
            MipSlice: 0,
            FirstWSlice: 0,
            WSize: 33,
        };
        device.CreateUnorderedAccessView(&tex, None, Some(&uav), cpu);
        let mut srv = D3D12_SHADER_RESOURCE_VIEW_DESC {
            Format: DXGI_FORMAT_R16G16B16A16_FLOAT,
            ViewDimension: D3D12_SRV_DIMENSION_TEXTURE3D,
            Shader4ComponentMapping: D3D12_DEFAULT_SHADER_4_COMPONENT_MAPPING,
            ..Default::default()
        };
        srv.Anonymous.Texture3D = D3D12_TEX3D_SRV {
            MostDetailedMip: 0,
            MipLevels: 1,
            ResourceMinLODClamp: 0.0,
        };
        device.CreateShaderResourceView(
            &tex,
            Some(&srv),
            D3D12_CPU_DESCRIPTOR_HANDLE {
                ptr: cpu.ptr + step,
            },
        );
        let queue: ID3D12CommandQueue = device.CreateCommandQueue(&D3D12_COMMAND_QUEUE_DESC {
            Type: D3D12_COMMAND_LIST_TYPE_DIRECT,
            ..Default::default()
        })?;
        let alloc: ID3D12CommandAllocator =
            device.CreateCommandAllocator(D3D12_COMMAND_LIST_TYPE_DIRECT)?;
        let list: ID3D12GraphicsCommandList =
            device.CreateCommandList(0, D3D12_COMMAND_LIST_TYPE_DIRECT, &alloc, None)?;
        // SAMPLERS=<n>: also bind a shader-visible sampler heap of n point samplers.
        let samplers: Option<ID3D12DescriptorHeap> = match std::env::var("SAMPLERS")
            .ok()
            .and_then(|v| v.parse::<u32>().ok())
        {
            Some(n) => {
                let h: ID3D12DescriptorHeap =
                    device.CreateDescriptorHeap(&D3D12_DESCRIPTOR_HEAP_DESC {
                        Type: D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER,
                        NumDescriptors: n,
                        Flags: D3D12_DESCRIPTOR_HEAP_FLAG_SHADER_VISIBLE,
                        ..Default::default()
                    })?;
                let sstep = device
                    .GetDescriptorHandleIncrementSize(D3D12_DESCRIPTOR_HEAP_TYPE_SAMPLER)
                    as usize;
                let start = h.GetCPUDescriptorHandleForHeapStart().ptr;
                // PAST=<k>: also k point samplers written past the heap's end (as a
                // handle computed beyond it would).
                let past = std::env::var("PAST")
                    .ok()
                    .and_then(|v| v.parse::<usize>().ok())
                    .unwrap_or(0);
                for i in 0..n as usize + past {
                    device.CreateSampler(
                        &D3D12_SAMPLER_DESC {
                            Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
                            AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                            AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                            AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                            MaxLOD: 12.0,
                            ..Default::default()
                        },
                        D3D12_CPU_DESCRIPTOR_HANDLE {
                            ptr: start + i * sstep,
                        },
                    );
                }
                println!("bound a sampler heap of {n} point samplers");
                Some(h)
            }
            None => None,
        };
        match &samplers {
            Some(s) => list.SetDescriptorHeaps(&[Some(heap.clone()), Some(s.clone())]),
            None => list.SetDescriptorHeaps(&[Some(heap.clone())]),
        }
        // BIND_MANY=1: each of those bound and run once before the probe.
        if std::env::var_os("BIND_MANY").is_some() {
            for (r, p) in &many {
                list.SetComputeRootSignature(r);
                list.SetPipelineState(p);
                list.Dispatch(1, 1, 1);
            }
        }
        list.SetComputeRootSignature(&root);
        list.SetComputeRootDescriptorTable(0, heap.GetGPUDescriptorHandleForHeapStart());
        list.SetComputeRootUnorderedAccessView(1, out.GetGPUVirtualAddress());
        list.SetPipelineState(&fill_pso);
        list.Dispatch(33, 33, 33);
        let transition = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::transmute_copy(&tex),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    StateAfter: D3D12_RESOURCE_STATE_NON_PIXEL_SHADER_RESOURCE,
                }),
            },
            ..Default::default()
        };
        if std::env::var_os("KEEP_UAV").is_none() {
            list.ResourceBarrier(&[transition]);
        } else {
            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                ..Default::default()
            }]);
        }
        // SM66=<cso>: the probe as an SM 6.6 shader reading the SRV from the heap by index
        // (slot 1), with the root flags the layer's root signatures have.
        let sm66 = std::env::var_os("SM66").map(|_| dxc(SRC66));
        let _keep66;
        if let Some(code) = &sm66 {
            // SWAP66=1: first a root signature with the same three samplers in the other
            // order (s1's and s2's kinds made before s0's), kept.
            let _swapped = if std::env::var_os("SWAP66").is_some() {
                let mk = |reg: u32, filter: D3D12_FILTER| D3D12_STATIC_SAMPLER_DESC {
                    Filter: filter,
                    AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
                    MaxAnisotropy: 1,
                    ComparisonFunc: D3D12_COMPARISON_FUNC_NEVER,
                    BorderColor: D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK,
                    MaxLOD: 12.0,
                    ShaderRegister: reg,
                    ..Default::default()
                };
                let ss = [
                    mk(0, D3D12_FILTER_MIN_MAG_MIP_LINEAR),
                    mk(1, D3D12_FILTER_MIN_MAG_LINEAR_MIP_POINT),
                    mk(2, D3D12_FILTER_MIN_MAG_MIP_POINT),
                ];
                let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
                    Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
                    Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
                        Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                            NumStaticSamplers: 3,
                            pStaticSamplers: ss.as_ptr(),
                            ..Default::default()
                        },
                    },
                };
                let mut blob: Option<ID3DBlob> = None;
                D3D12SerializeVersionedRootSignature(&desc, &mut blob, None)?;
                let blob = blob.unwrap();
                let bytes = std::slice::from_raw_parts(
                    blob.GetBufferPointer().cast::<u8>(),
                    blob.GetBufferSize(),
                );
                println!("samplers made in the other order first");
                Some(device.CreateRootSignature::<ID3D12RootSignature>(0, bytes)?)
            } else {
                None
            };
            let root66: ID3D12RootSignature = device.CreateRootSignature(0, code)?;
            let desc66 = D3D12_COMPUTE_PIPELINE_STATE_DESC {
                pRootSignature: std::mem::transmute_copy(&root66),
                CS: D3D12_SHADER_BYTECODE {
                    pShaderBytecode: code.as_ptr().cast(),
                    BytecodeLength: code.len(),
                },
                ..Default::default()
            };
            let device1: ID3D12Device1 = device.cast()?;
            let pso66: ID3D12PipelineState = if let Some(path) = std::env::var_os("LIB66_LOAD") {
                let bytes = std::fs::read(&path).expect("library file");
                let library: ID3D12PipelineLibrary = device1.CreatePipelineLibrary(&bytes)?;
                let p = library.LoadComputePipeline(windows::core::w!("probe66"), &desc66)?;
                println!("probe66 loaded from the library");
                std::mem::forget(bytes);
                std::mem::forget(library);
                p
            } else {
                let p: ID3D12PipelineState = device.CreateComputePipelineState(&desc66)?;
                if let Some(path) = std::env::var_os("LIB66_STORE") {
                    let library: ID3D12PipelineLibrary = device1.CreatePipelineLibrary(&[])?;
                    library.StorePipeline(windows::core::w!("probe66"), &p)?;
                    let mut bytes = vec![0u8; library.GetSerializedSize()];
                    library.Serialize(&mut bytes)?;
                    std::fs::write(&path, &bytes).expect("library written");
                    println!("probe66 stored");
                }
                p
            };
            // BG_COMMIT=1: the driver's background recompiles of the pipelines made so
            // far are finished and put in place before the probe runs.
            if std::env::var_os("BG_COMMIT").is_some() {
                let device6: ID3D12Device6 = device.cast()?;
                let mut further = windows::core::BOOL(0);
                device6.SetBackgroundProcessingMode(
                    D3D12_BACKGROUND_PROCESSING_MODE_ALLOWED,
                    D3D12_MEASUREMENTS_ACTION_COMMIT_RESULTS_HIGH_PRIORITY,
                    None,
                    Some(&mut further),
                )?;
                println!(
                    "background work committed (further measurements wanted: {})",
                    further.as_bool()
                );
            }
            list.SetComputeRootSignature(&root66);
            list.SetComputeRootUnorderedAccessView(0, out.GetGPUVirtualAddress());
            list.SetPipelineState(&pso66);
            println!("SM 6.6 heap-indexed probe");
            _keep66 = Some((root66, pso66));
        } else {
            list.SetPipelineState(&probe_pso);
            _keep66 = None;
        }
        list.Dispatch(1, 1, 1);
        let to_copy = D3D12_RESOURCE_BARRIER {
            Type: D3D12_RESOURCE_BARRIER_TYPE_TRANSITION,
            Anonymous: D3D12_RESOURCE_BARRIER_0 {
                Transition: std::mem::ManuallyDrop::new(D3D12_RESOURCE_TRANSITION_BARRIER {
                    pResource: std::mem::transmute_copy(&out),
                    Subresource: D3D12_RESOURCE_BARRIER_ALL_SUBRESOURCES,
                    StateBefore: D3D12_RESOURCE_STATE_UNORDERED_ACCESS,
                    StateAfter: D3D12_RESOURCE_STATE_COPY_SOURCE,
                }),
            },
            ..Default::default()
        };
        list.ResourceBarrier(&[to_copy]);
        list.CopyResource(&back, &out);
        list.Close()?;
        queue.ExecuteCommandLists(&[Some(list.cast()?)]);
        let fence: ID3D12Fence = device.CreateFence(0, D3D12_FENCE_FLAG_NONE)?;
        queue.Signal(&fence, 1)?;
        let event = CreateEventW(None, false, false, None)?;
        fence.SetEventOnCompletion(1, event)?;
        WaitForSingleObject(event, INFINITE);
        let mut data = std::ptr::null_mut();
        back.Map(0, None, Some(&mut data))?;
        let v = std::slice::from_raw_parts(data.cast::<[f32; 4]>(), 64);
        for (i, t) in v.iter().enumerate() {
            let u = (i as f32 + 0.5) / 64.0 * (3.0 / 33.0) + 10.0 / 33.0;
            let linear = (u * 33.0 - 0.5) / 32.0;
            let nearest = (u * 33.0).floor() / 32.0;
            println!(
                "{i:2} u*33={:.3} got {:.4} {:.4} {:.4}  linear {:.4} nearest {:.4}",
                u * 33.0,
                t[0],
                t[1],
                t[2],
                linear,
                nearest
            );
        }
    }
    Ok(())
}
