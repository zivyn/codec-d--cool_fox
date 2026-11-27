//! Mesh shaders whose outputs pass the 32 KiB the DXIL validator allows: what
//! the host GPU does with them (needs the Windows SDK's `dxc.exe`, `VICTORY_PC_DXC`
//! or the newest one found next to `dxil.dll`, and a mesh shader device; skipped
//! without them).
//!
//! The compiler refuses such a shader (its validator measures vertex plus
//! primitive outputs, each component 4 bytes, the maxima rounded up to
//! multiples of 32), so the cases compile with `-Vd` and carry the hash we
//! compute, as the translator's output does.

use std::path::{Path, PathBuf};

use windows::Win32::Graphics::Direct3D12::*;
use windows::Win32::Graphics::Dxgi::*;

use super::container::{SigElement, mesh_output_bytes};
use super::host::{Draw, Host, Root};
use crate::dxbc::{Container, FourCC};

fn dxc() -> Option<PathBuf> {
    let dxc = match std::env::var_os("VICTORY_PC_DXC") {
        Some(p) => PathBuf::from(p),
        None => super::validator::Validator::default_path()?.with_file_name("dxc.exe"),
    };
    dxc.exists().then_some(dxc)
}

/// A mesh shader of `verts` vertices (a strip of `verts - 2` triangles over a
/// target of `verts` by 16 texels) with `attrs` flat `float4` attributes, and
/// the pixel shader that checks every component of its provoking vertex's.
/// Attribute `k` of vertex `v` holds `v * 256 + 4 * k + component`; the
/// constant `k.x` picks which corner of a triangle is its provoking vertex
/// (0: the strip's first, otherwise its last), so two draws check every vertex.
fn source(verts: u32, attrs: u32) -> String {
    let mut s = String::from(
        "#define RS \"RootConstants(num32BitConstants=4, b0)\"\n\
         cbuffer K : register(b0) { uint4 k; };\nstruct V { float4 pos : SV_Position;\n",
    );
    for a in 0..attrs {
        s += &format!("nointerpolation float4 a{a} : TEXCOORD{a};\n");
    }
    let (cols, prims) = (verts / 2, verts - 2);
    s += &format!(
        "}};\n[RootSignature(RS)] [outputtopology(\"triangle\")] [numthreads(128, 1, 1)]\n\
         void ms(uint t : SV_GroupIndex, out vertices V verts[{verts}], out indices uint3 tris[{prims}]) {{\n\
           SetMeshOutputCounts({verts}, {prims});\n\
           for (uint i = t; i < {verts}; i += 128) {{\n\
             float x = float(i / 2) / {} * 2 - 1;\n\
             verts[i].pos = float4(x, (i & 1) ? -1 : 1, 0.5, 1);\n",
        cols - 1
    );
    for a in 0..attrs {
        let at = |c: u32| format!("float(i * 256 + {})", 4 * a + c);
        s += &format!(
            "verts[i].a{a} = float4({}, {}, {}, {});\n",
            at(0),
            at(1),
            at(2),
            at(3)
        );
    }
    s += &format!(
        "}}\n  for (uint j = t; j < {prims}; j += 128)\n\
             tris[j] = k.x == 0 ? uint3(j, j + 1, j + 2) : uint3(j + 2, j, j + 1);\n}}\n\
         [RootSignature(RS)]\nfloat4 ps(V v) : SV_Target {{\n\
           uint id = uint(v.a0.x * (1.0 / 256) + 0.5);\n  bool ok = true;\n"
    );
    for a in 0..attrs {
        for (c, n) in ["x", "y", "z", "w"].iter().enumerate() {
            s += &format!(
                "ok = ok && v.a{a}.{n} == float(id * 256 + {});\n",
                4 * a + c as u32
            );
        }
    }
    s + "return float4(ok ? 1 : 0, id, 0, 1);\n}\n"
}

fn compile(dxc: &Path, dir: &Path, src: &str, entry: &str, profile: &str) -> Vec<u8> {
    let (hlsl, out) = (dir.join("m.hlsl"), dir.join(format!("{entry}.bin")));
    std::fs::write(&hlsl, src).unwrap();
    let status = std::process::Command::new(dxc)
        .args(["-nologo", "-Vd", "-T", profile, "-E", entry, "-Fo"])
        .arg(&out)
        .arg(&hlsl)
        .status()
        .unwrap();
    assert!(status.success(), "dxc failed for {entry}");
    let mut bytes = std::fs::read(out).unwrap();
    // (-Vd leaves the hash out)
    let digest = super::container::hash(&bytes[20..]);
    bytes[4..20].copy_from_slice(&digest);
    bytes
}

/// The output size the validator measures, from the shader's own signature.
fn measured(ms: &[u8], verts: u32) -> u64 {
    let c = Container::parse(ms).unwrap();
    let elements: Vec<SigElement> = super::signature(c.part(FourCC::OSG1).unwrap().data).unwrap();
    mesh_output_bytes(&elements, &[], verts, verts - 2)
}

fn vendor(host: &Host) -> u32 {
    // SAFETY: plain DXGI queries.
    unsafe {
        let factory: IDXGIFactory4 = CreateDXGIFactory2(DXGI_CREATE_FACTORY_FLAGS(0)).unwrap();
        let adapter: IDXGIAdapter1 = factory
            .EnumAdapterByLuid(host.device.GetAdapterLuid())
            .unwrap();
        adapter.GetDesc1().unwrap().VendorId
    }
}

/// What a draw of every (vertices, attributes) case did: the size, whether the
/// pipeline was made, and how many of the vertices' values the pixel shader
/// found right and wrong.
fn run_cases(cases: &[(u32, u32)]) -> Option<Vec<(u32, u32, u64, Result<(usize, usize), String>)>> {
    let dxc = dxc()?;
    let host = Host::new(false).ok()?;
    eprintln!("adapter vendor {:#x}", vendor(&host));
    drop(host);
    let dir = std::env::temp_dir().join(format!("victory-mesh-size-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let mut results = Vec::new();
    for &(verts, attrs) in cases {
        // (a case that loses the device is the last: a new device for each of the others)
        let host = Host::new(false).ok()?;
        let src = source(verts, attrs);
        let (ms, ps) = (
            compile(&dxc, &dir, &src, "ms", "ms_6_5"),
            compile(&dxc, &dir, &src, "ps", "ps_6_5"),
        );
        let bytes = measured(&ms, verts);
        let outcome = (|| {
            // SAFETY: the container carries its root signature.
            let root: ID3D12RootSignature = unsafe { host.device.CreateRootSignature(0, &ms) }
                .map_err(|e| e.message().to_string())?;
            let pso = host
                .mesh_pipeline_with(&root, &ms, &ps)
                .map_err(|e| format!("pipeline: {e}"))?;
            let mut seen = vec![false; verts as usize];
            let mut bad = 0;
            for provoking in [0u32, 1] {
                let texels = host
                    .draw(
                        &root,
                        &pso,
                        &Draw {
                            roots: vec![(0, Root::Constants(vec![provoking, 0, 0, 0]))],
                            textures: &[],
                            samplers: &[],
                            vertices: &[],
                            stride: 0,
                            size: (verts, 16),
                            mesh_groups: 1,
                        },
                    )
                    .map_err(|e| format!("draw: {e}"))?;
                for t in texels.iter().filter(|t| t[3] != 0.0) {
                    let id = t[1] as usize;
                    if t[0] == 1.0 && id < seen.len() {
                        seen[id] = true;
                    } else {
                        bad += 1;
                    }
                }
            }
            Ok((seen.iter().filter(|s| **s).count(), bad))
        })();
        let failed = outcome.is_err();
        results.push((verts, attrs, bytes, outcome));
        if failed {
            break;
        }
    }
    let _ = std::fs::remove_dir_all(&dir);
    Some(results)
}

/// The sizes of the translated programs we emit (33 to 36 KiB at 128 vertices) and up to the
/// 40 KiB the runtime lets NVIDIA's drivers have (`NVIDIA_MESH_OUTPUT_BYTES` in `victory-gpu`)
/// draw every vertex's attributes intact.
#[test]
fn mesh_outputs_past_32_kib_arrive_intact_on_nvidia() {
    let cases: &[(u32, u32)] = &[(128, 15), (128, 16), (128, 17), (128, 19), (256, 9)];
    let Some(results) = run_cases(cases) else {
        eprintln!("no PC dxc or no mesh shader device; skipped");
        return;
    };
    let on_nvidia = Host::new(false).is_ok_and(|h| vendor(&h) == 0x10de);
    for (verts, attrs, bytes, outcome) in &results {
        eprintln!("{verts} vertices x {attrs} float4: {bytes} bytes: {outcome:?}");
        if on_nvidia {
            assert_eq!(
                outcome,
                &Ok((*verts as usize, 0)),
                "{verts} vertices x {attrs} float4 ({bytes} bytes)"
            );
        }
    }
}

/// Where NVIDIA's drivers stop (an RTX 4060: 47104 bytes drew right at 128 and 256 vertices,
/// 49152 lost the device at both) (`cargo test -p victory-shader --release mesh_output_limit_probe
/// -- --ignored --nocapture`): each series of growing outputs ends at the first the GPU does not
/// take, which loses the device (a driver reset: do not run it with a game open).
#[test]
#[ignore = "resets the GPU driver by design"]
fn mesh_output_limit_probe() {
    // (each series ends at its first failure, which removes the device)
    let series: [&[(u32, u32)]; 2] = [
        &[
            (128, 14),
            (128, 15),
            (128, 16),
            (128, 17),
            (128, 19),
            (128, 20),
            (128, 21),
            (128, 22),
            (128, 23),
        ],
        &[
            (256, 6),
            (256, 7),
            (256, 8),
            (256, 9),
            (256, 10),
            (256, 11),
            (256, 12),
        ],
    ];
    for cases in series {
        let Some(results) = run_cases(cases) else {
            eprintln!("no PC dxc or no mesh shader device; skipped");
            return;
        };
        for (verts, attrs, bytes, outcome) in &results {
            match outcome {
                Ok((seen, bad)) => eprintln!(
                    "{verts} vertices x {attrs} float4: {bytes} bytes: made; {seen}/{verts} vertices right, {bad} wrong texels"
                ),
                Err(e) => {
                    eprintln!("{verts} vertices x {attrs} float4: {bytes} bytes: failed: {e}")
                }
            }
        }
    }
}
