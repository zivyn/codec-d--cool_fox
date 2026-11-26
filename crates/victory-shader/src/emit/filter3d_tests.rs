//! A level-0 sample of a 3D texture with a linear, clamping immediate
//! sampler, by the card and by hand (`VICTORY_FILTER_3D`): the same
//! trilinear values.

use super::host::{Dispatch, Host, Root, View};
use super::intersect_tests::build_compute_shader;
use super::lower::TEST_FILTER_3D;
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};

/// Texture size per axis.
const N: u32 = 4;

/// Points (normalized) and the value a trilinear filter gives there of a
/// texture whose texel (x, y, z) holds (x, 10 y, 100 z, 1).
fn cases() -> Vec<([f32; 3], [f32; 4])> {
    let at = |p: f32| ((p * N as f32 - 0.5).clamp(0.0, (N - 1) as f32), p);
    [
        [0.3f32, 0.55, 0.8],
        [0.125, 0.125, 0.125],
        [0.01, 0.99, 0.5],
        [0.7, 0.2, 0.33],
    ]
    .into_iter()
    .map(|p| {
        let [x, y, z] = p.map(|v| at(v).0);
        (p, [x, 10.0 * y, 100.0 * z, 1.0])
    })
    .collect()
}

/// The program: the immediate sampler of the colour-grading tables
/// (PS_CompositeNMB: clamp, linear, point mip), then for each case
/// `image_sample_lz` 3D of four channels at the case's point, stored.
fn program() -> NativeProgram {
    let mut words = vec![
        0xBE81_0380, // s_mov_b32 s1, 0
        0xF40C_0400,
        0xFA00_0000, // s_load_dwordx8 s[16:23], s[0:1], 0: the 3D SRV
        0xF408_0300,
        0xFA00_0020, // s_load_dwordx4 s[12:15], s[0:1], 32: the output UAV
        0x7E1E_0280, // v_mov_b32 v15, 0: output byte offsets
    ];
    // s_mov_b32 s24..s27: the sampler words.
    for (k, w) in [0x92u32, 0x00C0_0000, 0x1450_0000, 0]
        .into_iter()
        .enumerate()
    {
        words.extend([0xBE98_03FF + ((k as u32) << 16), w]);
    }
    for (i, (p, _)) in cases().iter().enumerate() {
        for (reg, v) in p.iter().enumerate() {
            // v_mov_b32 v<reg>, literal
            words.extend([0x7E00_02FF | ((reg as u32) << 17), v.to_bits()]);
        }
        // image_sample_lz (0x27) 3D (dim 2), dmask xyzw, v[0:2] -> v[10:13],
        // resource s[16:23], sampler s[24:27].
        words.extend([0xF000_0F10 | (0x27 << 18), 0x00C4_0A00]);
        for ch in 0..4u32 {
            // buffer_store_dword v<10+ch>, v15, s[12:15], 0 offen offset:...
            words.extend([
                0xE070_1000 | ((i as u32 * 4 + ch) * 4),
                0x8003_000F | ((10 + ch) << 8),
            ]);
        }
    }
    words.push(0xBF81_0000); // s_endpgm
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 16,
        layout: vec![RootParam {
            root: RootType::Table,
            table_offset_dwords: 0,
            sgprs: 1,
            base_register: 0,
            user_data: 0,
            slot: 0,
        }],
        used: [(ResourceType::Srv, 0, 8), (ResourceType::Uav, 8, 4)]
            .map(|(kind, at, dwords)| UsedResource {
                kind,
                root: RootType::Table,
                table_offset_dwords: at,
                lower_bound: 0,
                stride: 0,
                dwords,
                user_data: 0,
                slot: 0,
            })
            .to_vec(),
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [1, 1, 1],
        }),
        code: words.into_iter().flat_map(u32::to_le_bytes).collect(),
    }
}

fn translate(by_hand: bool) -> super::HostShader {
    let was = TEST_FILTER_3D.replace(Some(by_hand));
    let built = build_compute_shader(&program());
    TEST_FILTER_3D.set(was);
    let mut shader = built.expect("translates").0;
    // (The tests' builder leaves the root signature's samplers out.)
    shader.static_samplers = shader
        .samplers
        .iter()
        .map(|&w| super::StaticSampler::from_words(w))
        .collect();
    shader
}

/// The switch changes the program (eight reads and their weights for each
/// sample), and only with it on; the sampler stays bound either way.
#[test]
fn the_switch_takes_the_other_form() {
    let card = translate(false);
    let hand = translate(true);
    assert_ne!(card.dxil, hand.dxil);
    assert!(hand.dxil.len() > card.dxil.len());
    assert_eq!(card.static_samplers, hand.static_samplers);
    assert_eq!(card.static_samplers[0].filter, 0x14, "linear, point mip");
}

/// Both forms give the trilinear values, on WARP and on the card.
#[test]
fn by_hand_and_by_the_card_agree() {
    let texels: Vec<[f32; 4]> = (0..N * N * N)
        .map(|i| {
            [
                (i % N) as f32,
                10.0 * (i / N % N) as f32,
                100.0 * (i / (N * N)) as f32,
                1.0,
            ]
        })
        .collect();
    let cases = cases();
    for (name, warp) in [("WARP", true), ("hardware", false)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {name} device; skipped");
            continue;
        };
        for by_hand in [false, true] {
            let shader = translate(by_hand);
            let (root, pso) = host.compute_pipeline(&shader).unwrap();
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(
                            0,
                            Root::Constants(vec![0; shader.user_data_dwords() as usize]),
                        )],
                        views: &[
                            View::Texture3D(N, N, N, &texels),
                            View::RawUav(4 * cases.len() as u32),
                        ],
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap();
            let got: Vec<f32> = out[0].iter().copied().map(f32::from_bits).collect();
            for (i, (p, want)) in cases.iter().enumerate() {
                for ch in 0..4 {
                    let g = got[4 * i + ch];
                    // The card's weights have 8 fractional bits.
                    let tolerance = 0.01 * (1.0 + want[ch].abs());
                    assert!(
                        (g - want[ch]).abs() <= tolerance,
                        "{name} by_hand={by_hand} at {p:?} channel {ch}: {g} not {}",
                        want[ch]
                    );
                }
            }
        }
    }
}
