//! End-to-end tests of the title's root SRVs read through the view heap
//! (`VICTORY_VIEW_INDEX`, `lower::view_index`): a synthetic GFX10 program
//! that builds a buffer descriptor around a root SRV's address and reads two
//! dwords through it, lowered with the switch off (a root SRV, a table of
//! the host's root signature) and on (an index in the user data, the view
//! at that heap slot), validated by `dxil.dll` when present, and run on the
//! hardware GPU and WARP.

use windows::Win32::Graphics::Direct3D12::*;

use super::host::{Dispatch, Host, Root, Stride, View};
use super::lower::view_index::TEST_VIEW_INDEX;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// The root parameter the title's SRV is in.
const SLOT: u8 = 5;

/// The program: the table of views at s0 (the UAV at its third entry), the
/// root SRV's address at s1:s2; it builds a descriptor around the address,
/// loads the dwords at bytes 4 and 64 of the buffer, and stores them at the
/// first two dwords of the UAV.
const PROGRAM: &[u32] = &[
    0xBE8C_0300, // s_mov_b32 s12, s0
    0xBE8D_0380, // s_mov_b32 s13, 0
    0xF408_0506, // s_load_dwordx4 s[20:23], s[12:13], 64
    0xFA00_0040,
    0xBE88_0301, // s_mov_b32 s8, s1
    0x8809_FF02, // s_or_b32 s9, s2, 0x100000
    0x0010_0000,
    0xBE8A_03C1, // s_mov_b32 s10, -1
    0xBE8B_03FF, // s_mov_b32 s11, 0x11014fac
    0x1101_4FAC,
    0xE030_0004, // buffer_load_dword v4, off, s[8:11], 0 offset:4
    0x8002_0400,
    0xE030_0040, // buffer_load_dword v5, off, s[8:11], 0 offset:64
    0x8002_0500,
    0xE070_0000, // buffer_store_dword v4, off, s[20:23], 0
    0x8005_0400,
    0xE070_0004, // buffer_store_dword v5, off, s[20:23], 0 offset:4
    0x8005_0500,
    0xBF81_0000, // s_endpgm
];

/// Lowers [`PROGRAM`] (wave32, one thread) with the switch `on` or off.
fn build(on: bool) -> HostShader {
    let param = |root, user_data, sgprs, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![
            param(RootType::Table, 0, 1, 0),
            param(RootType::Descriptor, 1, 2, SLOT),
        ],
        used: vec![
            UsedResource {
                kind: ResourceType::Uav,
                root: RootType::Table,
                table_offset_dwords: 16,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 0,
                slot: 0,
            },
            UsedResource {
                kind: ResourceType::Srv,
                root: RootType::Descriptor,
                table_offset_dwords: 0,
                lower_bound: 0,
                stride: 0,
                dwords: 4,
                user_data: 1,
                slot: SLOT,
            },
        ],
        io: StageIo::Cs(ComputeIo {
            rsrc2: 2 << 1,
            threads: [1, 1, 1],
        }),
        code: PROGRAM.iter().flat_map(|w| w.to_le_bytes()).collect(),
    };
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io");
    TEST_VIEW_INDEX.set(Some(on));
    let lowered = lower::lower(&lower::Parts {
        native: &native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        geometry: None,
    });
    TEST_VIEW_INDEX.set(None);
    let lowered = lowered.expect("lowers");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: Vec::new(),
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

/// What each way says of the program: off, the root SRV is a parameter of
/// the root signature and the user data holds nothing of it; on, there is
/// none, and a dword of the user data (past the table's) is the heap slot.
#[test]
fn a_root_srv_is_an_index_in_the_user_data_when_the_switch_is_on() {
    let off = build(false);
    assert_eq!(off.root_srvs, [SLOT]);
    assert!(
        off.user_data.iter().all(|u| u.root == RootType::Table),
        "{:?}",
        off.user_data
    );
    let on = build(true);
    assert!(on.root_srvs.is_empty(), "{:?}", on.root_srvs);
    let index: Vec<_> = on
        .user_data
        .iter()
        .filter(|u| u.root == RootType::Other(super::USER_ROOT_VIEW_INDEX))
        .collect();
    assert_eq!(index.len(), 1, "{:?}", on.user_data);
    assert_eq!(index[0].slot, SLOT);
    assert_ne!(on.dxil, off.dxil);
}

/// The two ways read the same: the dword at byte 4 of the view, and zero
/// past its size (a read of the 16 bytes the view has, at byte 64), from the
/// view at the heap slot the user data names, wherever in the heap it is.
#[test]
fn a_root_srv_read_through_the_heap_reads_what_the_table_did() {
    let data = [0x11u32, 0x22, 0x33, 0x44];
    let texel = [[0.0f32; 4]; 1];
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let host = match Host::new(warp) {
            Ok(host) => host,
            Err(e) => {
                eprintln!("no SM 6.6 {label} device ({e}); skipped");
                continue;
            }
        };
        // The table: two textures and the UAV (the third entry); then, in
        // the heap past it, two views of buffers: the one the shader picks
        // is the second.
        let decoy = [0xdead_u32, 0xbeef, 0xf00d, 0xcafe];
        let views = [
            View::Texture(1, 1, &texel),
            View::Texture(1, 1, &texel),
            View::RawUav(2),
            View::Buffer {
                data: &decoy,
                stride: Stride::Raw,
            },
            View::Buffer {
                data: &data,
                stride: Stride::Raw,
            },
        ];
        for on in [true, false] {
            let shader = build(on);
            let (root, pso) = match host.compute_pipeline(&shader) {
                Ok(p) => p,
                Err(e) => panic!("{label}: compute_pipeline failed: {e}"),
            };
            let mut user = vec![0u32; shader.user_data_dwords().max(1) as usize];
            for u in &shader.user_data {
                user[u.index as usize] = if u.root == RootType::Other(super::USER_ROOT_VIEW_INDEX) {
                    4
                } else {
                    0
                };
            }
            let mut roots = vec![(0, Root::Constants(user))];
            if !on {
                // The root SRV, a buffer of the same four dwords.
                roots.push((
                    1,
                    Root::View {
                        srv: true,
                        data: data.to_vec(),
                    },
                ));
            }
            let words = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots,
                        views: &views,
                        samplers: &[],
                        groups: [1, 1, 1],
                    },
                )
                .unwrap_or_else(|e| panic!("{label}, on {on}: dispatch failed: {e}"));
            assert_eq!(words[0][0], 0x22, "{label}, on {on}: the dword at byte 4");
            if on {
                // (A root descriptor has no size: past it is whatever lies there.)
                assert_eq!(words[0][1], 0, "{label}: a read past the view");
            }
        }
    }
}
