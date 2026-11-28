//! The store footprint ([`crate::footprint`]) against what translated programs do: dispatched on the
//! host GPU and WARP, every byte that changed in a zeroed buffer lies in a range the footprint gave
//! for the same dispatch and root constants, and so does every byte of a buffer of known contents that
//! the program read.

use super::host::{Dispatch, Host, Root, Stride, View};
use super::valu_tests::ONE_AT_A_TIME;
use super::{HostShader, lower, validator};
use crate::footprint::{Bind, Footprint, Target, analyze};
use crate::native::{ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource};
use crate::{io, ir, resource, structure};

const DWORDS: u32 = 1 << 18;

fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

fn entry(kind: ResourceType, table_offset_dwords: u32) -> UsedResource {
    UsedResource {
        kind,
        root: RootType::Table,
        table_offset_dwords,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 4,
        slot: 1,
    }
}

/// The constants s0..s3, a table in s4, group id x in s6, then `head` (which loads the descriptors),
/// the group and thread offset into v1 and a nonzero value in v2, then `body`, which leaves each lane's
/// byte offset in v1, and the access `access` (two words, or four for a load and a store).
fn program_with(head: &[u32], used: Vec<UsedResource>, body: &[u32], access: &[u32], threads: u32) -> NativeProgram {
    let mut words = vec![0xBE85_0380]; // s_mov_b32 s5, 0
    words.extend_from_slice(head);
    words.extend_from_slice(&[
        0x8F07_8806,            // s_lshl_b32 s7, s6, 8
        vop2(0x1A, 1, 0x82, 0), // v_lshlrev_b32 v1, 2, v0
        vop2(0x25, 1, 7, 1),    // v_add_nc_u32 v1, s7, v1
        0x7E04_02FF,            // v_mov_b32 v2, 0x01234567
        0x0123_4567,
    ]);
    words.extend_from_slice(body);
    words.extend_from_slice(access);
    words.push(0xBF81_0000);
    let param = |root, sgprs, user_data, slot| RootParam { root, table_offset_dwords: 0, sgprs, base_register: 0, user_data, slot };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![param(RootType::Constants, 4, 0, 0), param(RootType::Table, 1, 4, 1)],
        used,
        io: StageIo::Cs(ComputeIo {
            // Six user SGPRs (s5 is the table's high half), group id x (s6).
            rsrc2: 6 << 1 | 1 << 7,
            threads: [threads, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// A store through the descriptor in s[8:11]: [`program_with`] over one UAV.
fn program(body: &[u32], store: [u32; 2], threads: u32) -> NativeProgram {
    let head = [
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, // s_load_dwordx4 s[8:11], s[4:5], null
        125 << 25,
    ];
    program_with(&head, vec![entry(ResourceType::Uav, 0)], body, &store, threads)
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

/// What a dispatch of `native` on each device, for each group count and constants, did, given the
/// views of the heap in order: `check(footprint, bind, label, views' contents after)` runs for each.
fn each_dispatch(
    native: &NativeProgram,
    views: &[View],
    groups: &[u32],
    constants: &[[u32; 4]],
    mut check: impl FnMut(&Footprint, &Bind, &str, &[Vec<u32>]),
) {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let shader = translate(native);
    let fp = analyze(native).expect("footprint");
    let threads = match native.io {
        StageIo::Cs(cs) => cs.threads,
        _ => unreachable!(),
    };
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
                        &Dispatch { roots: vec![(0, Root::Constants(ud))], views, samplers: &[], groups: [g, 1, 1] },
                    )
                    .expect("dispatch");
                let constant = |slot: u8, dword: u32| (slot == 0).then(|| c[dword as usize]);
                let bind = Bind { threads, groups: Some([g, 1, 1]), constants: &constant };
                check(&fp, &bind, &format!("{label}, groups {g}, constants {c:?}"), &out);
            }
        }
    }
}

/// Every nonzero dword of `got` lies in one of `ranges`; returns how many there were.
fn inside(label: &str, what: &str, got: &[u32], ranges: &[(u64, u64)]) -> usize {
    let mut seen = 0;
    for (dword, &v) in got.iter().enumerate() {
        if v == 0 {
            continue;
        }
        seen += 1;
        let at = dword as u64 * 4;
        assert!(ranges.iter().any(|r| r.0 <= at && at < r.1), "{label}: byte {at:#x} {what} is outside {ranges:?}");
    }
    assert!(seen > 0, "{label}: nothing {what}");
    seen
}

const CONSTANTS: [[u32; 4]; 3] = [[0, 0, 0, 0], [0, 4096, 12, 0], [0, 70_000 & !3, 1024, 0]];

/// Runs a program of one UAV (the table's entry 0) and checks what it wrote.
fn covered(native: &NativeProgram, groups: &[u32], constants: &[[u32; 4]]) {
    each_dispatch(native, &[View::RawUav(DWORDS)], groups, constants, |fp, bind, label, out| {
        let ranges = fp.ranges(Target::Table { user_data: 4, dword: 0 }, bind).expect("a bounded footprint");
        inside(label, "written", &out[0], &ranges);
    });
}

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

#[test]
fn what_a_program_reads_stays_in_its_load_ranges() {
    // Entry 0 is a buffer of known contents (every dword holds its own number plus one), entry 1 the
    // buffer the program copies into, a dword for each it reads, at the same offset.
    let head = [
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, // s_load_dwordx4 s[8:11], s[4:5], null
        125 << 25,
        0x3D << 26 | 0x02 << 18 | 12 << 6 | 2, // s_load_dwordx4 s[12:15], s[4:5], 32
        125 << 25 | 32,
    ];
    let used = vec![entry(ResourceType::Srv, 0), entry(ResourceType::Uav, 8)];
    let source: Vec<u32> = (1..=DWORDS).collect();
    for threads in [1, 64] {
        for (label, immediate) in [("plain", 0u32), ("with an immediate", 16)] {
            let add = [vop2(0x25, 1, 1, 1)]; // v_add_nc_u32 v1, s1, v1
            let access = [
                0x38 << 26 | 0x0C << 18 | 1 << 12 | immediate, // buffer_load_dword v2, v1, s[8:11], 0 offen offset:i
                0x80 << 24 | 2 << 16 | 2 << 8 | 1,
                0x38 << 26 | 0x1C << 18 | 1 << 12 | immediate, // buffer_store_dword v2, v1, s[12:15], 0 offen offset:i
                0x80 << 24 | 3 << 16 | 2 << 8 | 1,
            ];
            let native = program_with(&head, used.clone(), &add, &access, threads);
            let views = [View::Buffer { data: &source, stride: Stride::Raw }, View::RawUav(DWORDS)];
            each_dispatch(&native, &views, &[1, 3], &CONSTANTS, |fp, bind, at, out| {
                let at = format!("{label}, {at}");
                let read = fp.load_ranges(Target::Table { user_data: 4, dword: 0 }, bind).expect("bounded loads");
                let wrote = fp.ranges(Target::Table { user_data: 4, dword: 8 }, bind).expect("bounded stores");
                // The dwords copied are the ones read.
                inside(&at, "read", &out[0], &read);
                inside(&at, "written", &out[0], &wrote);
                // And nothing went through the other view the wrong way.
                assert_eq!(fp.ranges(Target::Table { user_data: 4, dword: 0 }, bind), Some(vec![]), "{at}");
                assert_eq!(fp.load_ranges(Target::Table { user_data: 4, dword: 8 }, bind), Some(vec![]), "{at}");
            });
        }
    }
}
