//! End-to-end tests of dispatches made one (`VICTORY_FUSE_DISPATCH`, `lower/fuse.rs`): a synthetic GFX10 program
//! whose stores depend on its group id (all three components), on a dispatch size the title passes in constants, on
//! its thread id and on a root constant is lowered as it always was and as a member of a run, run once for each member
//! of a run as the title does, and once as the run's one dispatch; the bytes the two ways write are the same. Run on
//! the hardware GPU and WARP, validated by `dxil.dll` when present.

use super::host::{Dispatch, Host, Root, View};
use super::lower::fuse::TEST_FUSE;
use super::valu_tests::ONE_AT_A_TIME;
use super::{
    HostShader, SLOT_FUSE, fuse_fill, fuse_fill_native, fuse_native_rows, lower, validator,
};
use crate::native::{
    ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource,
};
use crate::{io, ir, resource, structure};

/// The bytes of a group's slot: 32 threads, a dword each.
const SLOT_BYTES: u32 = 128;

fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

/// SOP2: `op sdst, ssrc0, ssrc1`.
fn sop2(op: u32, sdst: u32, ssrc0: u32, ssrc1: u32) -> u32 {
    0b10 << 30 | op << 23 | sdst << 16 | ssrc1 << 8 | ssrc0
}

const S_ADD_U32: u32 = 0x00;
const S_LSHL_B32: u32 = 0x1E;
const S_MUL_I32: u32 = 0x26;

/// An inline constant of the scalar sources.
const fn inline(n: u32) -> u32 {
    128 + n
}

/// The program, wave32 with 32 threads: constants s0..s3 (the member's first slot, the dispatch size in x and y, a
/// salt), the table in s4, the group ids in s6..s8. Each thread stores
/// `salt + gx + (gy << 8) + (gz << 16) + thread` at `(first + gx + x * (gy + y * gz)) * 128 + thread * 4`.
fn program() -> NativeProgram {
    let words = [
        0xBE85_0380u32,                        // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 12 << 6 | 2, // s_load_dwordx4 s[12:15], s[4:5], null
        125 << 25,
        sop2(S_MUL_I32, 9, 2, 8),            // s9 = y * gz
        sop2(S_ADD_U32, 9, 9, 7),            // s9 += gy
        sop2(S_MUL_I32, 9, 9, 1),            // s9 *= x
        sop2(S_ADD_U32, 9, 9, 6),            // s9 += gx
        sop2(S_ADD_U32, 9, 9, 0),            // s9 += first
        sop2(S_LSHL_B32, 10, 9, inline(7)),  // s10 = s9 * 128
        sop2(S_LSHL_B32, 16, 7, inline(8)),  // s16 = gy << 8
        sop2(S_ADD_U32, 16, 16, 6),          // s16 += gx
        sop2(S_LSHL_B32, 17, 8, inline(16)), // s17 = gz << 16
        sop2(S_ADD_U32, 16, 16, 17),
        sop2(S_ADD_U32, 16, 16, 3),        // s16 += salt
        vop2(0x1A, 1, 0x82, 0),            // v_lshlrev_b32 v1, 2, v0
        vop2(0x25, 1, 10, 1),              // v_add_nc_u32 v1, s10, v1
        0x7E04_0200 | 16,                  // v_mov_b32 v2, s16
        vop2(0x25, 2, 0x100, 2),           // v_add_nc_u32 v2, v0, v2
        0x38 << 26 | 0x1C << 18 | 1 << 12, // buffer_store_dword v2, v1, s[12:15], 0 offen
        0x80 << 24 | 3 << 16 | 2 << 8 | 1,
        0xBF81_0000, // s_endpgm
    ];
    let param = |root, sgprs, user_data, slot| RootParam {
        root,
        table_offset_dwords: 0,
        sgprs,
        base_register: 0,
        user_data,
        slot,
    };
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout: vec![
            param(RootType::Constants, 4, 0, 0),
            param(RootType::Table, 1, 4, 1),
        ],
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
            // Six user SGPRs, the group ids x, y and z (s6, s7, s8).
            rsrc2: 6 << 1 | 1 << 7 | 1 << 8 | 1 << 9,
            threads: [32, 1, 1],
        }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `native` lowered as a member of a run (`fused`) or as it always was.
fn translate(native: &NativeProgram, fused: Option<bool>) -> HostShader {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, native).expect("io::raise");
    TEST_FUSE.set(fused);
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
    });
    TEST_FUSE.set(None);
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
        root_indexed_views: res.root_indexed_views,
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        v.validate(&mut shader.dxil.clone())
            .expect("DXIL validation failed");
    } else {
        eprintln!("no dxil.dll; validation skipped");
    }
    shader
}

/// One dispatch of the run: its groups and constants (first slot, dispatch size x and y, salt).
struct Member {
    groups: [u32; 3],
    first: u32,
    salt: u32,
}

impl Member {
    fn new(groups: [u32; 3], first: u32, salt: u32) -> Member {
        Member {
            groups,
            first,
            salt,
        }
    }

    /// The constants the title passes: the dispatch size is among them.
    fn constants(&self) -> [u32; 4] {
        [self.first, self.groups[0], self.groups[1], self.salt]
    }

    fn slots(&self) -> u32 {
        self.groups.iter().product()
    }
}

/// The user data the shader reads, in its own order, for a member: the constants, and the table at heap offset 0.
fn user_data(shader: &HostShader, m: &Member) -> Vec<u32> {
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let c = m.constants();
    for u in &shader.user_data {
        ud[u.index as usize] = match (u.root, u.slot) {
            (RootType::Constants, 0) => c[u.dword as usize],
            (RootType::Table, 1) => 0,
            other => panic!("unexpected user data {other:?}"),
        };
    }
    ud
}

/// What the members write when each is a dispatch of its own, and when they are one: the same bytes.
fn same_bytes(members: &[Member]) {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let native = program();
    let plain = translate(&native, Some(false));
    let fused = translate(&native, Some(true));
    assert!(plain.root_cbvs.is_empty(), "{:?}", plain.root_cbvs);
    assert_eq!(fused.root_cbvs, [SLOT_FUSE]);
    assert!(fused.root_srvs.is_empty() && fused.root_uavs.is_empty());
    assert_eq!(
        fused.user_data, plain.user_data,
        "the program reads the same user data either way"
    );
    let slots: u32 = members.iter().map(Member::slots).sum();
    // (The slots the members write, from the first any of them has to the last.)
    let extent = members
        .iter()
        .map(|m| m.first + m.slots())
        .max()
        .unwrap_or(0);
    let dwords = extent * SLOT_BYTES / 4;
    for (label, warp) in [("hardware", false), ("warp", true)] {
        let Ok(host) = Host::new(warp) else {
            eprintln!("no SM 6.6 {label} device; skipped");
            continue;
        };
        let views = [View::RawUav(dwords)];
        let (root, pso) = host.compute_pipeline(&plain).expect("compute_pipeline");
        // The title's way: one dispatch for each member, each into a zeroed buffer; the members write where
        // the others do not, so the buffer they would all have written is the union.
        let mut want = vec![0u32; dwords as usize];
        let mut singly = Vec::new();
        for m in members {
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![(0, Root::Constants(user_data(&plain, m)))],
                        views: &views,
                        samplers: &[],
                        groups: m.groups,
                    },
                )
                .expect("dispatch of a member");
            singly.push(out[0].clone());
            for (w, &o) in want.iter_mut().zip(&out[0]) {
                if o != 0 {
                    assert_eq!(
                        *w, 0,
                        "{label}: two members wrote the same dword (the test's own mistake)"
                    );
                    *w = o;
                }
            }
        }
        assert_eq!(
            want.iter().filter(|&&w| w != 0).count(),
            (slots * 32) as usize,
            "{label}: every thread of every member wrote"
        );
        // The run's: one table, one dispatch.
        let dwords_of_user = fused.user_data_dwords() as usize;
        let groups: Vec<[u32; 3]> = members.iter().map(|m| m.groups).collect();
        let values: Vec<u32> = members.iter().flat_map(|m| user_data(&fused, m)).collect();
        let mut table = vec![0u32; super::fuse_rows(members.len(), dwords_of_user) * 4];
        let dims = fuse_fill(&mut table, dwords_of_user, &groups, &values).expect("a table");
        let (root, pso) = host
            .compute_pipeline(&fused)
            .expect("compute_pipeline of the member's form");
        let out = host
            .dispatch(
                &root,
                &pso,
                &Dispatch {
                    // (The root constants of the stage hold nothing the program reads.)
                    roots: vec![
                        (0, Root::Constants(vec![0xdead_beef; dwords_of_user.max(1)])),
                        (
                            1,
                            Root::View {
                                srv: false,
                                data: table,
                            },
                        ),
                    ],
                    views: &views,
                    samplers: &[],
                    groups: dims,
                },
            )
            .expect("dispatch of the run");
        let wrong = out[0].iter().zip(&want).filter(|(a, b)| a != b).count();
        assert_eq!(
            wrong,
            0,
            "{label}: {wrong} of {dwords} dwords differ between the members one by one and the run, for {} members",
            members.len()
        );
        // A dispatch as the title made it (a table of no members: its own group ids, its own group counts) is that
        // dispatch too.
        for (m, one) in members.iter().zip(&singly) {
            let mut table = vec![0u32; fuse_native_rows(dwords_of_user) * 4];
            fuse_fill_native(&mut table, dwords_of_user, &user_data(&fused, m)).expect("a table");
            let out = host
                .dispatch(
                    &root,
                    &pso,
                    &Dispatch {
                        roots: vec![
                            (0, Root::Constants(vec![0xdead_beef; dwords_of_user.max(1)])),
                            (
                                1,
                                Root::View {
                                    srv: false,
                                    data: table,
                                },
                            ),
                        ],
                        views: &views,
                        samplers: &[],
                        groups: m.groups,
                    },
                )
                .expect("dispatch of a member in its own form");
            assert!(
                out[0] == *one,
                "{label}: the dispatch of {:?} in its own form writes other bytes",
                m.groups
            );
        }
    }
}

/// Three-dimensional members of different shapes, with different constants and salts: the group id's three
/// components and the dispatch size in x and y matter to every store.
#[test]
fn a_run_of_dispatches_writes_what_the_dispatches_one_by_one_write() {
    same_bytes(&[
        Member::new([3, 2, 2], 0, 0x1000_0000),
        Member::new([1, 1, 1], 12, 0x2000_0000),
        Member::new([5, 1, 4], 13, 0x3000_0000),
        Member::new([2, 3, 1], 33, 0x4000_0000),
        Member::new([7, 1, 1], 39, 0x5000_0000),
    ]);
}

#[test]
fn a_run_of_one_dispatch_is_that_dispatch() {
    same_bytes(&[Member::new([4, 3, 2], 5, 0x7000_0000)]);
}

/// Many one-group members, as the title's tail has them: the program finds its member among dozens.
#[test]
fn a_run_of_many_small_dispatches_writes_what_they_write() {
    let members: Vec<Member> = (0..97)
        .map(|k| Member::new([1 + k % 3, 1, 1 + k % 2], k * 8, (k + 1) << 20))
        .collect();
    // (Their slots do not overlap: member k starts at 8k and has at most 3 * 2 = 6.)
    same_bytes(&members);
}

/// A run of more groups than a row of the dispatch holds: two rows, the groups of the second past the run's end
/// leave, and the group numbers go on across the rows.
#[test]
fn a_run_of_more_groups_than_a_row_is_two_rows() {
    same_bytes(&[
        Member::new([65535, 1, 1], 0, 0x1000_0000),
        Member::new([5, 1, 1], 65535, 0x2000_0000),
    ]);
}

/// With the switch unset, a program is what it was: the same code as before the switch existed (the plain form is
/// what `Some(false)` makes too), and no root view.
#[test]
fn without_the_switch_the_program_is_as_it_was() {
    let _one = ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let native = program();
    let unset = translate(&native, None);
    let off = translate(&native, Some(false));
    assert_eq!(unset.dxil, off.dxil);
    assert!(unset.root_cbvs.is_empty());
    let on = translate(&native, Some(true));
    assert_ne!(on.dxil, off.dxil);
}
