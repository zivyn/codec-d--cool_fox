//! End-to-end tests of the raw buffer reads through the host's windows (`VICTORY_BINDLESS_BUFFERS`,
//! `lower::bindless`): a synthetic GFX10 program that reads the SRV of its table with `buffer_load_dword*`
//! and `s_buffer_load_dword` and stores what it read in its UAV, lowered with the switch off (the view of
//! the table's entry) and on (the window of the entry, the view where it has none), validated by `dxil.dll`
//! when present, and run on the hardware GPU and WARP: with a window the reads come from the copy at the
//! window's bytes and stop at its length, without one they are what the view gives.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::lower::bindless::TEST_BINDLESS;
use super::{HostShader, lower, validator};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// The program: the table of views at s0 (the SRV at its first entry, the UAV at its third); it loads
/// a dword at byte 4 of the SRV, one at byte 20, one at byte 24, two dwords from byte 20, and one more
/// dword at byte 4 by `s_buffer_load_dword`, and stores the six dwords at the UAV's first six.
const PROGRAM: &[u32] = &[
    0xBE8C_0300, // s_mov_b32 s12, s0
    0xBE8D_0380, // s_mov_b32 s13, 0
    0xF408_0506, // s_load_dwordx4 s[20:23], s[12:13], 64
    0xFA00_0040,
    0xF408_0606, // s_load_dwordx4 s[24:27], s[12:13], 0
    0xFA00_0000,
    0xE030_0004, // buffer_load_dword v4, off, s[24:27], 0 offset:4
    0x8006_0400,
    0xE030_0014, // buffer_load_dword v5, off, s[24:27], 0 offset:20
    0x8006_0500,
    0xE030_0018, // buffer_load_dword v6, off, s[24:27], 0 offset:24
    0x8006_0600,
    0xE034_0014, // buffer_load_dwordx2 v[8:9], off, s[24:27], 0 offset:20
    0x8006_0800,
    0xF420_070C, // s_buffer_load_dword s28, s[24:27], 4
    0xFA00_0004,
    0x7E0E_021C, // v_mov_b32 v7, s28
    0xE070_0000, // buffer_store_dword v4, off, s[20:23], 0
    0x8005_0400,
    0xE070_0004, // buffer_store_dword v5, off, s[20:23], 0 offset:4
    0x8005_0500,
    0xE070_0008, // buffer_store_dword v6, off, s[20:23], 0 offset:8
    0x8005_0600,
    0xE070_000C, // buffer_store_dword v7, off, s[20:23], 0 offset:12
    0x8005_0700,
    0xE070_0010, // buffer_store_dword v8, off, s[20:23], 0 offset:16
    0x8005_0800,
    0xE070_0014, // buffer_store_dword v9, off, s[20:23], 0 offset:20
    0x8005_0900,
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
    let used = |kind, table_offset_dwords| UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 0,
        slot: 0,
    };
    let native = NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 32,
        layout: vec![param(RootType::Table, 0, 1, 0)],
        used: vec![used(ResourceType::Srv, 0), used(ResourceType::Uav, 16)],
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
    TEST_BINDLESS.set(Some(on));
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
    TEST_BINDLESS.set(None);
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
        bindless: lowered.bindless,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

/// What each way says of the program: off, no window table and nothing only windows read; on, the
/// table is a root SRV of the host, a dword of the user data is the heap slot of the copies' views,
/// and the table's SRV is read through windows alone (the view's branch is no use of it).
#[test]
fn a_table_buffer_is_read_through_its_window_when_the_switch_is_on() {
    let off = build(false);
    assert!(
        !off.root_srvs.contains(&super::SLOT_VIEW_WINDOWS),
        "{:?}",
        off.root_srvs
    );
    assert!(off.bindless.is_empty(), "{:?}", off.bindless);
    assert!(
        off.user_data.iter().all(|u| u.root == RootType::Table),
        "{:?}",
        off.user_data
    );
    let on = build(true);
    assert_eq!(on.root_srvs, [super::SLOT_VIEW_WINDOWS]);
    assert_eq!(
        on.bindless,
        [(0, 0)],
        "the SRV is the table's first entry, read by windows alone"
    );
    let base: Vec<_> = on
        .user_data
        .iter()
        .filter(|u| u.root == RootType::Other(super::USER_WINDOW_BASE))
        .collect();
    assert_eq!(base.len(), 1, "{:?}", on.user_data);
    assert_ne!(on.dxil, off.dxil);
}

/// The window of the heap slot of the SRV (slot 0): copy 1, from byte 16 on, 24 bytes long.
const WINDOW: [u32; 4] = [1, 16, 24, 0];

/// The dwords of the copy.
fn copy() -> Vec<u32> {
    (0..16).map(|i| 0x100 + i).collect()
}

/// What the program stores through the window [`WINDOW`]: byte 4 of the window, its last dword (byte
/// 20), nothing past its end (byte 24), the scalar load of byte 4, the two dwords from byte 20 (the second
/// is past the end).
fn through_window() -> [u32; 6] {
    let copy = copy();
    [copy[5], copy[9], 0, copy[5], copy[9], 0]
}

/// What it stores through a view of these dwords: the loads of the view, zeros past its 16 bytes.
fn through_view(view: &[u32]) -> [u32; 6] {
    let at = |byte: usize| view.get(byte / 4).copied().unwrap_or(0);
    [at(4), at(20), at(24), at(4), at(20), at(24)]
}

/// Runs the translation made with the switch `on` over the heap: the SRV's own view of `decoy` (slot 0),
/// a texture, the UAV (slot 2), copy 0's null view (slot 3) and the copy (slot 4); the window table `window`
/// (None: not bound), and returns the six dwords it stored.
fn run(on: bool, window: Option<[u32; 4]>, warp: bool) -> Option<Vec<u32>> {
    let host = Host::new(warp).ok()?;
    let decoy = [0xdead_u32, 0xbeef, 0xf00d, 0xcafe];
    let texel = [[0.0f32; 4]; 1];
    let copy = copy();
    let zeros = [0u32; 4];
    let views = [
        View::Buffer {
            data: &decoy,
            stride: Stride::Raw,
        },
        View::Texture(1, 1, &texel),
        View::RawUav(8),
        View::Buffer {
            data: &zeros,
            stride: Stride::Raw,
        },
        View::Buffer {
            data: &copy,
            stride: Stride::Raw,
        },
    ];
    let shader = build(on);
    let (root, pso) = host
        .compute_pipeline(&shader)
        .unwrap_or_else(|e| panic!("compute_pipeline failed: {e}"));
    let mut user = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        // The table starts at heap slot 0; copy 0's view is at slot 3.
        user[u.index as usize] = if u.root == RootType::Other(super::USER_WINDOW_BASE) {
            3
        } else {
            0
        };
    }
    let mut roots = vec![(0, Root::Constants(user))];
    if on {
        // The table of windows: the SRV's slot is the first entry, the rest are no window.
        let mut table = vec![0u32; 4 * 8];
        if let Some(w) = window {
            table[..4].copy_from_slice(&w);
        }
        roots.push((
            1,
            Root::View {
                srv: true,
                data: table,
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
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(words[0][..6].to_vec())
}

/// With a window the reads come from the copy at the window's bytes and a dword that is not whole inside
/// its length is 0; with none (the host did not write one) they are the view's, as with the switch off.
#[test]
fn a_window_reads_the_copy_at_its_bytes_and_stops_at_its_length() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let decoy = [0xdead_u32, 0xbeef, 0xf00d, 0xcafe];
    let mut ran = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Some(off) = run(false, None, warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        assert_eq!(
            off,
            through_view(&decoy),
            "{label}: the view, the switch off"
        );
        assert_eq!(
            run(true, None, warp).expect("device"),
            through_view(&decoy),
            "{label}: no window, the view"
        );
        assert_eq!(
            run(true, Some(WINDOW), warp).expect("device"),
            through_window(),
            "{label}: the window"
        );
        // A window from the copy's start, long enough for all the loads.
        let all = run(true, Some([1, 0, 4096, 0]), warp).expect("device");
        let copy = copy();
        assert_eq!(
            all,
            [copy[1], copy[5], copy[6], copy[1], copy[5], copy[6]],
            "{label}: a long window"
        );
        ran += 1;
    }
    assert!(ran > 0, "no SM 6.6 device available");
}
