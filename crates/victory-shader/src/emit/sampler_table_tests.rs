//! A sampler read through a one-descriptor table of the sampler heap is the
//! sampler a static sampler of the same register would be
//! (`victory_gpu::pipeline::sampler_table` binds the immediate samplers of
//! the title's pipelines that way, so that nothing the driver puts into a
//! compiled pipeline depends on the process that compiled it): the same
//! compute shader, with the root signature that has the static samplers and
//! with the one that has the tables (the same registers and space, the same
//! flags as the host's: both heaps directly indexed), sampling a texture
//! the filters and address modes read differently, on WARP and on the
//! default adapter where there is one. Needs the Windows SDK's `dxc.exe`
//! (`VICTORY_PC_DXC` or the newest one found next to `dxil.dll`); skipped
//! without it.

use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Direct3D::ID3DBlob;
use windows::Win32::Graphics::Direct3D12::*;

use super::host::{Dispatch, Host, Root, View};

/// A skip is no pass on the CI runner, which has the SDK's compiler and WARP.
fn not_on_ci() -> bool {
    std::env::var_os("GITHUB_ACTIONS").is_none()
}

fn dxc() -> Option<PathBuf> {
    let dxc = match std::env::var_os("VICTORY_PC_DXC") {
        Some(p) => PathBuf::from(p),
        None => super::validator::Validator::default_path()?.with_file_name("dxc.exe"),
    };
    dxc.exists().then_some(dxc)
}

/// Two immediate samplers (`s0`: point, wrapping; `s1`: bilinear, white border) read a 4x4 texture at
/// four places, one of them off its edges on each side; the answers go to a raw buffer, `s0`'s then `s1`'s
/// for each place.
const SOURCE: &str = "
Texture2D<float4> tex : register(t0);
SamplerState point_wrap : register(s0);
SamplerState linear_border : register(s1);
RWByteAddressBuffer result : register(u0);
[numthreads(1, 1, 1)]
void main() {
    float2 uvs[4] = { float2(0.30, 0.55), float2(0.5, 0.5), float2(1.3, 0.2), float2(-0.2, 0.9) };
    for (uint i = 0; i < 4; i++) {
        float4 a = tex.SampleLevel(point_wrap, uvs[i], 0);
        float4 b = tex.SampleLevel(linear_border, uvs[i], 0);
        result.Store4((i * 2) * 16, asuint(a));
        result.Store4((i * 2 + 1) * 16, asuint(b));
    }
}
";

fn compile(dxc: &Path, dir: &Path) -> Vec<u8> {
    let (hlsl, out) = (dir.join("s.hlsl"), dir.join("cs.bin"));
    std::fs::write(&hlsl, SOURCE).unwrap();
    let status = std::process::Command::new(dxc)
        .args(["-nologo", "-Vd", "-T", "cs_6_6", "-E", "main", "-Fo"])
        .arg(&out)
        .arg(&hlsl)
        .status()
        .unwrap();
    assert!(status.success(), "dxc failed");
    let mut bytes = std::fs::read(out).unwrap();
    // (-Vd leaves the hash out)
    let digest = super::container::hash(&bytes[20..]);
    bytes[4..20].copy_from_slice(&digest);
    bytes
}

fn point_wrap() -> D3D12_SAMPLER_DESC {
    D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_MIRROR,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
        MaxLOD: 12.0,
        ..Default::default()
    }
}

fn linear_border() -> D3D12_SAMPLER_DESC {
    D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_LINEAR_MIP_POINT,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_BORDER,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
        BorderColor: [1.0; 4],
        MaxLOD: 12.0,
        ..Default::default()
    }
}

/// The static sampler that says what `d` says, as register `n` (the host's own conversion keeps the
/// white border as the preset of that name).
fn as_static(d: &D3D12_SAMPLER_DESC, n: u32) -> D3D12_STATIC_SAMPLER_DESC {
    D3D12_STATIC_SAMPLER_DESC {
        Filter: d.Filter,
        AddressU: d.AddressU,
        AddressV: d.AddressV,
        AddressW: d.AddressW,
        MipLODBias: d.MipLODBias,
        MaxAnisotropy: d.MaxAnisotropy,
        ComparisonFunc: d.ComparisonFunc,
        BorderColor: if d.BorderColor == [1.0; 4] {
            D3D12_STATIC_BORDER_COLOR_OPAQUE_WHITE
        } else {
            D3D12_STATIC_BORDER_COLOR_TRANSPARENT_BLACK
        },
        MinLOD: d.MinLOD,
        MaxLOD: d.MaxLOD,
        ShaderRegister: n,
        RegisterSpace: super::SAMPLER_SPACE,
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }
}

fn root(
    host: &Host,
    params: &[D3D12_ROOT_PARAMETER1],
    statics: &[D3D12_STATIC_SAMPLER_DESC],
) -> ID3D12RootSignature {
    let desc = D3D12_VERSIONED_ROOT_SIGNATURE_DESC {
        Version: D3D_ROOT_SIGNATURE_VERSION_1_1,
        Anonymous: D3D12_VERSIONED_ROOT_SIGNATURE_DESC_0 {
            Desc_1_1: D3D12_ROOT_SIGNATURE_DESC1 {
                NumParameters: params.len() as u32,
                pParameters: params.as_ptr(),
                NumStaticSamplers: statics.len() as u32,
                pStaticSamplers: statics.as_ptr(),
                Flags: D3D12_ROOT_SIGNATURE_FLAG_CBV_SRV_UAV_HEAP_DIRECTLY_INDEXED
                    | D3D12_ROOT_SIGNATURE_FLAG_SAMPLER_HEAP_DIRECTLY_INDEXED,
            },
        },
    };
    let mut blob: Option<ID3DBlob> = None;
    let mut error: Option<ID3DBlob> = None;
    // SAFETY: the description points at live arrays; the blob is read before it goes.
    unsafe {
        if let Err(e) = D3D12SerializeVersionedRootSignature(&desc, &mut blob, Some(&mut error)) {
            let text = error.map(|b| {
                String::from_utf8_lossy(std::slice::from_raw_parts(
                    b.GetBufferPointer().cast::<u8>(),
                    b.GetBufferSize(),
                ))
                .into_owned()
            });
            panic!("root signature: {e}: {text:?}");
        }
        let blob = blob.unwrap();
        let bytes =
            std::slice::from_raw_parts(blob.GetBufferPointer().cast::<u8>(), blob.GetBufferSize());
        host.device
            .CreateRootSignature(0, bytes)
            .expect("the root signature is made")
    }
}

/// A table of one `kind` range of register 0 of space 0.
fn table(range: &D3D12_DESCRIPTOR_RANGE1) -> D3D12_ROOT_PARAMETER1 {
    D3D12_ROOT_PARAMETER1 {
        ParameterType: D3D12_ROOT_PARAMETER_TYPE_DESCRIPTOR_TABLE,
        Anonymous: D3D12_ROOT_PARAMETER1_0 {
            DescriptorTable: D3D12_ROOT_DESCRIPTOR_TABLE1 {
                NumDescriptorRanges: 1,
                pDescriptorRanges: range,
            },
        },
        ShaderVisibility: D3D12_SHADER_VISIBILITY_ALL,
    }
}

fn range(
    kind: D3D12_DESCRIPTOR_RANGE_TYPE,
    register: u32,
    flags: D3D12_DESCRIPTOR_RANGE_FLAGS,
) -> D3D12_DESCRIPTOR_RANGE1 {
    D3D12_DESCRIPTOR_RANGE1 {
        RangeType: kind,
        NumDescriptors: 1,
        BaseShaderRegister: register,
        RegisterSpace: super::SAMPLER_SPACE,
        Flags: flags,
        OffsetInDescriptorsFromTableStart: 0,
    }
}

#[test]
fn a_sampler_read_through_a_table_is_the_static_sampler_of_its_register() {
    let Some(dxc) = dxc() else {
        eprintln!("no PC dxc.exe; skipped");
        assert!(
            not_on_ci(),
            "no PC dxc.exe on the CI runner: the test did not run"
        );
        return;
    };
    let dir = std::env::temp_dir().join(format!("victory-sampler-table-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let cs = compile(&dxc, &dir);

    // 4x4 texels, each different, so that filtering, wrapping, mirroring and the border all show.
    let texels: Vec<[f32; 4]> = (0..16)
        .map(|i| {
            [
                i as f32 / 16.0,
                (i % 4) as f32 * 0.3,
                (i / 4) as f32 * 0.2,
                0.25 + (i % 3) as f32 * 0.25,
            ]
        })
        .collect();
    // The heap holds other samplers around the two (a table that read the wrong slot would show).
    let other = D3D12_SAMPLER_DESC {
        Filter: D3D12_FILTER_MIN_MAG_MIP_LINEAR,
        AddressU: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        AddressV: D3D12_TEXTURE_ADDRESS_MODE_WRAP,
        AddressW: D3D12_TEXTURE_ADDRESS_MODE_CLAMP,
        MaxAnisotropy: 1,
        ComparisonFunc: D3D12_COMPARISON_FUNC_LESS_EQUAL,
        MaxLOD: 12.0,
        ..Default::default()
    };
    let (first, second) = (3usize, 5usize);
    let mut samplers = [other; 7];
    samplers[first] = point_wrap();
    samplers[second] = linear_border();

    let volatile = D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE
        | D3D12_DESCRIPTOR_RANGE_FLAG_DATA_VOLATILE;
    let srv = range(D3D12_DESCRIPTOR_RANGE_TYPE_SRV, 0, volatile);
    let uav = range(D3D12_DESCRIPTOR_RANGE_TYPE_UAV, 0, volatile);
    let s0 = range(
        D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER,
        0,
        D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE,
    );
    let s1 = range(
        D3D12_DESCRIPTOR_RANGE_TYPE_SAMPLER,
        1,
        D3D12_DESCRIPTOR_RANGE_FLAG_DESCRIPTORS_VOLATILE,
    );
    let views = [table(&srv), table(&uav)];
    let tabled = [views[0], views[1], table(&s0), table(&s1)];
    let statics = [as_static(&point_wrap(), 0), as_static(&linear_border(), 1)];

    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            continue;
        };
        let run = |root: &ID3D12RootSignature, tables: Option<(usize, usize)>| {
            let pso = host
                .compute_pipeline_with(root, &cs)
                .unwrap_or_else(|e| panic!("{label}: pipeline: {e}"));
            let mut roots = vec![
                (
                    0,
                    Root::Table {
                        sampler: false,
                        index: 0,
                    },
                ),
                (
                    1,
                    Root::Table {
                        sampler: false,
                        index: 1,
                    },
                ),
            ];
            if let Some((a, b)) = tables {
                roots.push((
                    2,
                    Root::Table {
                        sampler: true,
                        index: a,
                    },
                ));
                roots.push((
                    3,
                    Root::Table {
                        sampler: true,
                        index: b,
                    },
                ));
            }
            let out = host
                .dispatch(
                    root,
                    &pso,
                    &Dispatch {
                        roots,
                        views: &[View::Texture(4, 4, &texels), View::RawUav(32)],
                        samplers: &samplers,
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}: dispatch: {e}"));
            out.into_iter().next().unwrap()
        };
        let with_statics = run(&root(&host, &views, &statics), None);
        let with_tables = run(&root(&host, &tabled, &[]), Some((first, second)));
        assert_eq!(
            with_tables, with_statics,
            "{label}: the tables read other samplers than the static ones"
        );
        // The two differ from one another (point against bilinear, wrap against border), so the check
        // above is not one of two zeros or of two equal answers...
        let words = |out: &[u32], sampler: usize| -> Vec<u32> {
            (0..4)
                .flat_map(|i| out[(i * 2 + sampler) * 4..][..4].to_vec())
                .collect()
        };
        assert_ne!(
            words(&with_statics, 0),
            words(&with_statics, 1),
            "{label}: the two samplers read alike"
        );
        assert!(
            with_statics.iter().any(|&w| w != 0),
            "{label}: nothing was read"
        );
        // ...and a table pointed at the other sampler's slot reads what the other register would.
        let swapped = run(&root(&host, &tabled, &[]), Some((second, first)));
        assert_ne!(
            swapped, with_statics,
            "{label}: the slot a table points at does not matter"
        );
        ran += 1;
    }
    if ran == 0 {
        eprintln!("no SM 6.6 device; skipped");
        assert!(
            not_on_ci(),
            "no SM 6.6 device (WARP) on the CI runner: the test did not run"
        );
    }
}
