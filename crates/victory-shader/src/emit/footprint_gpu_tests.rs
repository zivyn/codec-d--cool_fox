//! The store footprint ([`crate::footprint`]) against what translated programs write: dispatched
//! on the host GPU and WARP into a zeroed buffer, every byte that changed lies in a range the
//! footprint gave for the same dispatch and root constants.

use super::host::{Dispatch, Host, Root, View};
use super::valu_tests::ONE_AT_A_TIME;
use super::{HostShader, lower, validator};
use crate::footprint::{Bind, Target, analyze};
use crate::native::{ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource};
use crate::{io, ir, resource, structure};

const DWORDS: u32 = 1 << 18;

fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

/// The constants s0..s3, a table in s4 whose descriptor is loaded into s[8:11], group id x in s6,
/// then `body`, which leaves each lane's byte offset in v1 and a nonzero value in v2, and the
/// store `store` (two words) through the descriptor.
fn program(body: &[u32], store: [u32; 2], threads: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380,                           // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, // s_load_dwordx4 s[8:11], s[4:5], null
        125 << 25,
        0x8F07_8806,            // s_lshl_b32 s7, s6, 8
        vop2(0x1A, 1, 0x82, 0), // v_lshlrev_b32 v1, 2, v0
        vop2(0x25, 1, 7, 1),    // v_add_nc_u32 v1, s7, v1
        0x7E04_02FF,            // v_mov_b32 v2, 0x01234567
        0x0123_4567,
    ];
    words.extend_from_slice(body);
    words.extend_from_slice(&store);
    words.push(0xBF81_0000);
    let param = |root, sgprs, user_data, slot| RootParam { root, table_offset_dwords: 0, sgprs, base_register: 0, user_data, slot };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![param(RootType::Constants, 4, 0, 0), param(RootType::Table, 1, 4, 1)],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo {
            // Six user SGPRs (s5 is the table's high half), group id x (s6).
            rsrc2: 6 << 1 | 1 << 7,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

fn translate(native: &NativeProgram) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, native).expect("io::raise");
    let lowered = lower::lower(&lower::Parts {
        native,
        p: &p,
        st: &st,
        io: &io,
        res: &res,
        inputs: Vec::new(),
        outputs: Vec::new(),
        prims: Vec::new(),
        mesh_stride: 0,
        geometry: None,
    })
    .expect("lower");
    let shader = HostShader {
        stage: HwStage::Cs,
        dxil: lowered.dxil,
        user_data: lowered.user_data,
        samplers: lowered.samplers,
        static_samplers: Vec::new(),
        root_cbvs: lowered.root_cbvs,
        root_srvs: lowered.root_srvs,
        root_uavs: lowered.root_uavs,
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// Dispatches `native` for each group count and constants, on every device there is, and checks
/// the bytes it wrote against the footprint. Returns how many bytes were checked.
fn covered(native: &NativeProgram, groups: &[u32], constants: &[[u32; 4]]) -> usize {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let shader = translate(native);
    let fp = analyze(native).expect("footprint");
    let threads = match native.io {
        StageIo::Cs(cs) => cs.threads,
        _ => unreachable!(),
    };
    let mut checked = 0;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let (root, pso) = host.compute_pipeline(&shader).expect("compute_pipeline");
        for &g in groups {
            for &c in constants {
                let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
                for u in &shader.user_data {
                    ud[u.index as usize] = match (u.root, u.slot) {
                        (RootType::Constants, 0) => c[u.dword as usize],
                        (RootType::Table, 1) => 0,
                        other => panic!("unexpected user data {other:?}"),
                    };
                }
                let out = host
                    .dispatch(
                        &root,
                        &pso,
                        &Dispatch {
                            roots: vec![(0, Root::Constants(ud))],
                            views: &[View::RawUav(DWORDS)],
                            samplers: &[],
                            groups: [g, 1, 1],
                        },
                    )
                    .expect("dispatch");
                let constant = |slot: u8, dword: u32| (slot == 0).then(|| c[dword as usize]);
                let bind = Bind { threads, groups: Some([g, 1, 1]), constants: &constant };
                let ranges = fp.ranges(Target::Table { user_data: 4, dword: 0 }, &bind).expect("a bounded footprint");
                let mut written = 0;
                for (dword, &v) in out[0].iter().enumerate() {
                    if v == 0 {
                        continue;
                    }
                    written += 1;
                    let at = dword as u64 * 4;
                    assert!(
                        ranges.iter().any(|r| r.0 <= at && at < r.1),
                        "{label}: byte {at:#x} written (groups {g}, constants {c:?}) is outside {ranges:?}"
                    );
                }
                assert!(written > 0, "{label}: nothing written (groups {g}, constants {c:?})");
                checked += written;
            }
        }
    }
    checked
}

const CONSTANTS: [[u32; 4]; 3] = [[0, 0, 0, 0], [0, 4096, 12, 0], [0, 70_000 & !3, 1024, 0]];

#[test]
fn a_store_at_group_and_thread_plus_a_root_constant_stays_in_its_ranges() {
    for threads in [1, 64] {
        let add = [vop2(0x25, 1, 1, 1)]; // v_add_nc_u32 v1, s1, v1
        // buffer_store_dword v2, v1, s[8:11], 0 offen
        let store = [0x38 << 26 | 0x1C << 18 | 1 << 12, 0x80 << 24 | 2 << 16 | 2 << 8 | 1];
        covered(&program(&add, store, threads), &[1, 3], &CONSTANTS);
    }
}

#[test]
fn a_store_with_a_root_constant_in_soffset_and_an_immediate_stays_in_its_ranges() {
    for threads in [1, 64] {
        // buffer_store_dword v2, v1, s[8:11], s2 offen offset:12
        let store = [0x38 << 26 | 0x1C << 18 | 1 << 12 | 12, 2 << 24 | 2 << 16 | 2 << 8 | 1];
        covered(&program(&[], store, threads), &[1, 3], &CONSTANTS);
    }
}

#[test]
fn an_atomic_add_stays_in_its_ranges() {
    for threads in [1, 64] {
        let add = [vop2(0x25, 1, 1, 1)];
        // buffer_atomic_add v2, v1, s[8:11], 0 offen
        let store = [0x38 << 26 | 0x32 << 18 | 1 << 12, 0x80 << 24 | 2 << 16 | 2 << 8 | 1];
        covered(&program(&add, store, threads), &[1, 3], &CONSTANTS);
    }
}
