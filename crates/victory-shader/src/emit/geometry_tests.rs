//! A merged geometry stage end to end: the export program as the vertex
//! shader, the geometry program after it, translated, validated by
//! `dxil.dll` when present, and drawn on the host GPU and WARP.

use super::bitcode::Attr;
use super::container::SigElement;
use super::emit;
use super::shader::Shader;
use crate::dxbc::{Container, FourCC};
use crate::native::{HwStage, NativeProgram, StageIo};
use crate::{io, resource, structure};

/// A merged geometry stage on the host: the export program as the
/// vertex shader, the geometry program after it. The export program
/// writes its position (a vertex attribute) to the vertex ring; the
/// geometry program reads its triangle's three from there, writes each
/// to its output ring and emits it. Both validate, and the pipeline
/// draws the triangle the vertex buffer holds.
#[test]
fn merged_geometry_stages_draw_their_triangle() {
    use super::bitcode::Term;
    use crate::native::{GsIo, VsInput};
    // Export: lanes with work from s3[7:0]; the ring slot from the
    // vertex index (v5) and the ring base (s6); position from v9-v12.
    let es_words: &[u32] = &[
        0x9417_FF03,
        0x0008_0000, // s_bfe_i32 s23, s3, 0x00080000
        0x92FE_8017, // s_bfm_b64 exec, s23, 0
        0xBF06_C017, // s_cmp_eq_u32 s23, 64
        0x85FE_7EC1, // s_cselect_b64 exec, -1, exec
        0xD543_0008,
        0x0019_2105, // v_mad_u32_u24 v8, v5, 16, s6
        0xD938_0100,
        0x000B_0908, // ds_write2_b64 v8, v[9:10], v[11:12] offset1:1
        0xBF8A_0000, // s_barrier
        0xBE80_2018, // s_setpc_b64 s[24:25]
    ];
    // Geometry: lanes with work from s3[15:8]; the output ring's base
    // from the stage's table (s[8:9] + 0xb8); the three vertices' ring
    // offsets packed in v0 and v1; each position read and written to
    // the output ring (12 dwords: 3 vertices of 4), then emitted.
    let gs_words: &[u32] = &[
        0x9400_FF03,
        0x0008_0008, // s_bfe_i32 s0, s3, 0x00080008
        0x92FE_8000, // s_bfm_b64 exec, s0, 0
        0xBF06_C000, // s_cmp_eq_u32 s0, 64
        0x85FE_7EC1, // s_cselect_b64 exec, -1, exec
        0x8701_FF09,
        0x0000_00FF, // s_and_b32 s1, s9, 0xff
        0xBE80_0308, // s_mov_b32 s0, s8
        0xF400_0000,
        0xFA00_00B8, // s_load_dword s0, s[0:1], 0xb8
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0x3402_0282, // v_lshlrev_b32 v1, 2, v1
        0xBE81_0382, // s_mov_b32 s1, 2
        0x3410_00F9,
        0x0586_0601, // v_lshlrev_b32 v8, s1, v0 src1_sel:WORD_1
        0x3614_06FF,
        0x0003_FFFC, // v_and_b32 v10, 0x3fffc, v3 (vertex 0)
        0x360C_02FF,
        0x0003_FFFC, // v_and_b32 v6, 0x3fffc, v1 (vertex 2)
        0xD8DC_0100,
        0x0E00_000A, // ds_read2_b32 v[14:15], v10 offset1:1
        0xD8DC_0302,
        0x1000_000A, // ds_read2_b32 v[16:17], v10 offset0:2 offset1:3
        0xD8DC_0100,
        0x1200_0008, // ds_read2_b32 v[18:19], v8 offset1:1
        0xD8DC_0302,
        0x1400_0008, // ds_read2_b32 v[20:21], v8 offset0:2 offset1:3
        0xD8DC_0100,
        0x1600_0006, // ds_read2_b32 v[22:23], v6 offset1:1
        0xD8DC_0302,
        0x1800_0006, // ds_read2_b32 v[24:25], v6 offset0:2 offset1:3
        0xD543_000C,
        0x0002_04FF,
        0x0000_0030, // v_mad_u32_u24 v12, 48, v2, s0
        0xD838_0100,
        0x000F_0E0C, // ds_write2_b32 v12, v14, v15 offset1:1
        0xD838_0302,
        0x0011_100C, // ds_write2_b32 v12, v16, v17 offset0:2 offset1:3
        0xBF90_0022, // s_sendmsg sendmsg(MSG_GS, GS_OP_EMIT, 0)
        0xD838_0504,
        0x0013_120C, // ds_write2_b32 v12, v18, v19 offset0:4 offset1:5
        0xD838_0706,
        0x0015_140C, // ds_write2_b32 v12, v20, v21 offset0:6 offset1:7
        0xBF90_0022, // emit
        0xD838_0908,
        0x0017_160C, // ds_write2_b32 v12, v22, v23 offset0:8 offset1:9
        0xD838_0B0A,
        0x0019_180C, // ds_write2_b32 v12, v24, v25 offset0:10 offset1:11
        0xBF90_0022, // emit
        0xBF90_0012, // s_sendmsg sendmsg(MSG_GS, GS_OP_CUT, 0)
        0xBF90_0003, // s_sendmsg sendmsg(MSG_GS_DONE, GS_OP_NOP)
        0xBF81_0000, // s_endpgm
    ];
    let element = |name: &str, kind| SigElement {
        name: name.into(),
        index: 0,
        kind,
        comp: 9,
        interp: 0,
        row: Some(0),
        col: 0,
        cols: 4,
        usage: 0,
        half: false,
    };
    let part = |e: SigElement, input| super::container::signature(&[e], input);
    let translate = |stage, vgprs, io, words: &[u32], parts: &[(FourCC, &[u8])]| {
        let native = NativeProgram {
            stage,
            wave64: true,
            vgprs,
            layout: vec![],
            used: vec![],
            io,
            code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
        };
        let bytes = crate::dxbc::build(parts);
        let c = Container::parse(&bytes).unwrap();
        let p = crate::spill::build(&native).unwrap();
        let st = structure::structure(&p).unwrap();
        let res = resource::track(&p, &native.layout, native.stage, Some(&st)).unwrap();
        let io = io::raise(&p, &native).unwrap();
        emit(&c, &native, &p, &st, &io, &res).unwrap()
    };
    let position = part(element("SV_Position", 3), false);
    let es = translate(
        HwStage::Es,
        13,
        StageIo::Es {
            inputs: vec![VsInput {
                usage: crate::native::Usage::Generic,
                row: 0,
                mask: 0xF,
                vgpr: 9,
            }],
            outputs: vec![],
            vertex_dwords: 4,
        },
        es_words,
        &[
            (FourCC::ISG1, &part(element("POSITION", 0), true)),
            (FourCC::OSG1, &position),
        ],
    );
    let geometry = |words: &[u32]| {
        translate(
            HwStage::Gs,
            28,
            StageIo::Gs(GsIo {
                outputs: vec![],
                ring_dwords: 12,
                invocations: 1,
                max_vertices: 3,
                vertex_dwords: 4,
            }),
            words,
            &[
                (FourCC::ISG1, &part(element("SV_Position", 3), true)),
                (FourCC::OSG1, &position),
            ],
        )
    };
    // These words occur exactly at the three Emit instructions above.
    // Keep the LDS writes and Cut unchanged, and move all Emits after the writes.
    const EMIT: u32 = 0xBF90_0022;
    let cut = gs_words.iter().position(|&w| w == 0xBF90_0012).unwrap();
    assert_eq!(gs_words[..cut].iter().filter(|&&w| w == EMIT).count(), 3);
    let batched_words: Vec<_> = gs_words[..cut]
        .iter()
        .copied()
        .filter(|&w| w != EMIT)
        .chain([EMIT; 3])
        .chain(gs_words[cut..].iter().copied())
        .collect();
    let gs = geometry(gs_words);
    let batched_gs = geometry(&batched_words);
    assert_eq!((es.stage, gs.stage), (HwStage::Es, HwStage::Gs));
    assert!(es.user_data.is_empty() && gs.user_data.is_empty());
    // A pixel shader writing ones.
    let ps = {
        let mut s = Shader::new(true);
        let b = s.entry;
        for col in 0..4 {
            let one = s.m.f32(1f32.to_bits());
            let args = [s.m.i32(0), s.m.i32(0), s.m.i8(col), one];
            s.op(b, "storeOutput.f32", Attr::NoUnwind, s.t.void, 5, &args);
        }
        let mut target = element("SV_Target", 16);
        target.usage = 0xF;
        s.outputs = vec![target];
        s.m.terminate(b, Term::Ret);
        s.finish()
    };
    if let Ok(mut v) =
        super::validator::Validator::load(super::validator::Validator::default_path().as_deref())
    {
        for c in [&es.dxil, &gs.dxil, &batched_gs.dxil, &ps] {
            v.validate(&mut c.clone()).unwrap();
        }
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    // The lower left half of the target.
    #[rustfmt::skip]
    let vertices = [
        -1.0, -1.0, 0.5, 1.0,
        -1.0, 1.0, 0.5, 1.0,
        1.0, -1.0, 0.5, 1.0,
    ];
    for warp in [false, true] {
        let Ok(host) = super::host::Host::new(warp) else {
            continue;
        };
        let root = host.root_signature_for(&[]).unwrap();
        let pso = host
            .pipeline_with_gs(&root, &es.dxil, Some(&gs.dxil), &ps)
            .unwrap();
        let draw = super::host::Draw {
            roots: Vec::new(),
            textures: &[],
            samplers: &[],
            vertices: &vertices,
            stride: 16,
            size: (32, 32),
            mesh_groups: 0,
        };
        let texels = host.draw(&root, &pso, &draw).unwrap();
        let batched_pso = host
            .pipeline_with_gs(&root, &es.dxil, Some(&batched_gs.dxil), &ps)
            .unwrap();
        let batched_texels = host.draw(&root, &batched_pso, &draw).unwrap();
        eprintln!(
            "warp {warp}: interleaved {} lit pixels, batched {} lit pixels",
            texels.iter().filter(|&&t| t == [1.0; 4]).count(),
            batched_texels.iter().filter(|&&t| t == [1.0; 4]).count()
        );
        for (i, t) in texels.iter().enumerate() {
            let (x, y) = ((i % 32) as f32, (i / 32) as f32);
            let (u, v) = ((x + 0.5) / 16.0 - 1.0, 1.0 - (y + 0.5) / 16.0);
            if (u + v).abs() < 0.1 {
                continue;
            }
            let want = if u + v < 0.0 { [1.0; 4] } else { [0.0; 4] };
            assert_eq!(*t, want, "warp {warp} pixel ({x}, {y})");
        }
        assert_eq!(batched_texels, texels, "warp {warp}, batched GS coverage");
    }
}
