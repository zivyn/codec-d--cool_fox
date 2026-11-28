//! Tests of the store footprint ([`crate::footprint`]): the interval
//! evaluation, and synthetic programs assembled from words.

use crate::footprint::{Bind, Expr, Footprint, Target, analyze, analyze_causes};
use crate::native::{ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource};

const S_ENDPGM: u32 = 0xBF81_0000;

fn bx(e: Expr) -> Box<Expr> {
    Box::new(e)
}

fn none(_: u8, _: u32) -> Option<u32> {
    None
}

fn bind<'a>(threads: [u32; 3], groups: Option<[u32; 3]>, constants: &'a dyn Fn(u8, u32) -> Option<u32>) -> Bind<'a> {
    Bind { threads, groups, constants }
}

#[test]
fn intervals_cover_what_the_expression_can_be() {
    let b = bind([64, 1, 1], Some([3, 1, 1]), &none);
    assert_eq!(Expr::Thread(0).eval(&b), Some((0, 63)));
    assert_eq!(Expr::Thread(1).eval(&b), Some((0, 0)));
    assert_eq!(Expr::Group(0).eval(&b), Some((0, 2)));
    let scaled = Expr::Add(bx(Expr::Shl(bx(Expr::Group(0)), bx(Expr::Const(8)))), bx(Expr::Mul(bx(Expr::Thread(0)), bx(Expr::Const(4)))));
    assert_eq!(scaled.eval(&b), Some((0, 512 + 252)));
    // Without the groups, an offset that uses them is not known.
    assert_eq!(scaled.eval(&bind([64, 1, 1], None, &none)), None);
    // Either of two.
    assert_eq!(Expr::Select(bx(Expr::Const(8)), bx(Expr::Const(100))).eval(&b), Some((8, 100)));
    // A mask bounds what it is applied to, known or not.
    assert_eq!(Expr::And(bx(Expr::Top), bx(Expr::Const(255))).eval(&b), Some((0, 255)));
    assert_eq!(Expr::Shr(bx(Expr::Top), bx(Expr::Const(28))).eval(&b), Some((0, 15)));
    assert_eq!(Expr::Min(bx(Expr::Top), bx(Expr::Const(40))).eval(&b), Some((0, 40)));
    assert_eq!(Expr::Top.eval(&b), None);
    // Exact when every operand is.
    let bfe = Expr::Bfe(bx(Expr::Const(0xABCD)), bx(Expr::Const(4)), bx(Expr::Const(8)));
    assert_eq!(bfe.eval(&b), Some((0xBC, 0xBC)));
    assert_eq!(Expr::Or(bx(Expr::Const(0xF0)), bx(Expr::Const(0x0F))).eval(&b), Some((0xFF, 0xFF)));
    // Root constants as the host has them, and not.
    let some = |slot: u8, dword: u32| (slot == 0 && dword == 1).then_some(4096);
    let c = Expr::Root { slot: 0, dword: 1 };
    assert_eq!(c.eval(&bind([1, 1, 1], None, &some)), Some((4096, 4096)));
    assert_eq!(Expr::Root { slot: 0, dword: 2 }.eval(&bind([1, 1, 1], None, &some)), None);
}

#[test]
fn what_could_wrap_past_32_bits_is_not_known() {
    let b = bind([64, 1, 1], Some([1, 1, 1]), &none);
    let max = Expr::Const(u32::MAX);
    assert_eq!(Expr::Add(bx(max.clone()), bx(Expr::Const(1))).eval(&b), None);
    assert_eq!(Expr::Add(bx(max.clone()), bx(Expr::Const(0))).eval(&b), Some((u64::from(u32::MAX), u64::from(u32::MAX))));
    assert_eq!(Expr::Sub(bx(Expr::Const(1)), bx(Expr::Const(2))).eval(&b), None);
    assert_eq!(Expr::Sub(bx(Expr::Const(5)), bx(Expr::Thread(0))).eval(&b), None);
    assert_eq!(Expr::Sub(bx(Expr::Const(100)), bx(Expr::Thread(0))).eval(&b), Some((37, 100)));
    assert_eq!(Expr::Mul(bx(Expr::Const(1 << 20)), bx(Expr::Const(1 << 12))).eval(&b), None);
    assert_eq!(Expr::Shl(bx(Expr::Const(1 << 20)), bx(Expr::Const(12))).eval(&b), None);
    assert_eq!(Expr::Shl(bx(Expr::Thread(0)), bx(Expr::Const(30))).eval(&b), None);
    // The amount is taken modulo 32 as the hardware does.
    assert_eq!(Expr::Shl(bx(Expr::Const(1)), bx(Expr::Const(33))).eval(&b), Some((2, 2)));
}

/// Root parameters: constants in s0..s3, a table in s4 (its high half s5 is
/// set to 0 by the code).
fn table_layout() -> (Vec<RootParam>, Vec<UsedResource>) {
    let param = |root, sgprs, user_data, slot| RootParam { root, table_offset_dwords: 0, sgprs, base_register: 0, user_data, slot };
    let used = |user_data, slot, stride| UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Table,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride,
        dwords: 4,
        user_data,
        slot,
    };
    (vec![param(RootType::Constants, 4, 0, 0), param(RootType::Table, 1, 4, 1)], vec![used(4, 1, 0)])
}

fn compute(words: &[u32], layout: Vec<RootParam>, used: Vec<UsedResource>, rsrc2: u32, threads: u32) -> NativeProgram {
    NativeProgram {
        stage: HwStage::Cs,
        wave64: false,
        vgprs: 8,
        layout,
        used,
        io: StageIo::Cs(ComputeIo { rsrc2, threads: [threads, 1, 1] }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// `s_mov_b32 s5, 0; s_load_dwordx4 s[8:11], s[4:5], <soffset>` (null, or the SGPR `soffset`).
fn load_descriptor(soffset: u32) -> [u32; 3] {
    [0xBE85_0380, 0x3D << 26 | 0x02 << 18 | 8 << 6 | 2, soffset << 25]
}

/// `buffer_store_dword v2, <vaddr>, s[8:11], 0` with the given address modes and immediate offset.
fn store(vaddr: u8, offen: bool, idxen: bool, offset: u16) -> [u32; 2] {
    [0x38 << 26 | 0x1C << 18 | u32::from(idxen) << 13 | u32::from(offen) << 12 | u32::from(offset), 0x80 << 24 | 2 << 16 | 2 << 8 | u32::from(vaddr)]
}

/// VOP2 `op vdst, src0, vsrc1`.
fn vop2(op: u32, vdst: u32, src0: u32, vsrc1: u32) -> u32 {
    op << 25 | vdst << 17 | vsrc1 << 9 | src0
}

const V_LSHLREV_B32: u32 = 0x1A;
const V_ADD_NC_U32: u32 = 0x25;
/// Five user SGPRs (s0..s4), one thread-id VGPR.
const RSRC2_TABLE: u32 = 5 << 1;

fn footprint(words: &[u32], layout: Vec<RootParam>, used: Vec<UsedResource>, rsrc2: u32) -> Footprint {
    let native = compute(words, layout, used, rsrc2, 64);
    analyze(&native).unwrap_or_else(|| {
        let p = crate::spill::build(&native).expect("spill::build");
        let st = crate::structure::structure(&p);
        let why = crate::resource::track(&p, &native.layout, native.stage, st.as_ref().ok());
        panic!("the front half of the translation fails: {:?}", why.err())
    })
}

fn table_target() -> Target {
    Target::Table { user_data: 4, dword: 0 }
}

#[test]
fn a_store_at_a_constant_offset() {
    let (layout, used) = table_layout();
    let mut words = load_descriptor(125).to_vec();
    words.extend(store(0, false, false, 16));
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    assert!(!f.unbounded && !f.by_address);
    assert_eq!(f.stores.len(), 1);
    assert_eq!(f.stores[0].target, table_target());
    let b = bind([64, 1, 1], Some([1, 1, 1]), &none);
    assert_eq!(f.ranges(table_target(), &b), Some(vec![(16, 20)]));
    // Another view of the same program has no stores.
    assert_eq!(f.ranges(Target::Table { user_data: 9, dword: 0 }, &b), Some(vec![]));
}

#[test]
fn thread_ids_times_four_from_a_root_constant() {
    let (layout, used) = table_layout();
    let mut words = load_descriptor(125).to_vec();
    words.push(vop2(V_LSHLREV_B32, 1, 0x82, 0)); // v_lshlrev_b32 v1, 2, v0
    words.push(vop2(V_ADD_NC_U32, 1, 1, 1)); // v_add_nc_u32 v1, s1, v1
    words.extend(store(1, true, false, 0));
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    assert_eq!(f.stores.len(), 1, "{f:?}");
    for base in [0u32, 4096, 1 << 20] {
        let constants = move |slot: u8, dword: u32| (slot == 0 && dword == 1).then_some(base);
        let b = bind([64, 1, 1], Some([2, 1, 1]), &constants);
        let want = (u64::from(base), u64::from(base) + 63 * 4 + 4);
        assert_eq!(f.ranges(table_target(), &b), Some(vec![want]));
    }
    // A constant the host does not have: anywhere in the view.
    assert_eq!(f.ranges(table_target(), &bind([64, 1, 1], None, &none)), None);
    // (The thread count is the program's: fewer threads, fewer bytes.)
    let constants = |_: u8, _: u32| Some(0);
    assert_eq!(f.ranges(table_target(), &bind([8, 1, 1], None, &constants)), Some(vec![(0, 32)]));
}

#[test]
fn a_structured_buffer_is_reached_by_index_times_stride() {
    let (layout, mut used) = table_layout();
    used[0].stride = 16;
    let mut words = load_descriptor(125).to_vec();
    words.extend(store(0, false, true, 0)); // idxen: v0, the thread
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    let b = bind([64, 1, 1], None, &none);
    assert_eq!(f.ranges(table_target(), &b), Some(vec![(0, 63 * 16 + 4)]));
}

#[test]
fn an_index_into_a_buffer_of_no_known_stride_is_not_known() {
    let (layout, used) = table_layout();
    let mut words = load_descriptor(125).to_vec();
    words.extend(store(0, false, true, 0));
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    assert_eq!(f.ranges(table_target(), &bind([64, 1, 1], None, &none)), None);
}

#[test]
fn a_descriptor_picked_by_a_computed_index_is_any_of_the_table() {
    let (layout, used) = table_layout();
    // s_lshl_b32 s6, s0, 5; s_load_dwordx4 s[8:11], s[4:5], s6 (a root constant picks the entry).
    let mut words = vec![0x8F06_8500];
    words.extend(load_descriptor(6));
    words.extend(store(0, true, false, 0));
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    assert_eq!(f.stores[0].target, Target::TableAny { user_data: 4 });
    let b = bind([4, 1, 1], None, &none);
    assert_eq!(f.ranges(table_target(), &b), Some(vec![(0, 16)]));
    assert_eq!(f.ranges(Target::Table { user_data: 4, dword: 8 }, &b), Some(vec![(0, 16)]));
    assert_eq!(f.ranges(Target::Table { user_data: 5, dword: 0 }, &b), Some(vec![]));
}

/// A root descriptor in s4/s5, built into a buffer descriptor by the code
/// (s8..s11); group id x in s6.
fn root_view_words() -> Vec<u32> {
    vec![
        0xBE88_0304, // s_mov_b32 s8, s4
        0x8809_FF05,
        0x0010_0000, // s_or_b32 s9, s5, 0x100000
        0xBE8A_03C1, // s_mov_b32 s10, -1
        0xBE8B_03FF,
        0x1101_4FAC, // s_mov_b32 s11, 0x11014fac
    ]
}

fn root_layout() -> (Vec<RootParam>, Vec<UsedResource>) {
    let param = RootParam { root: RootType::Descriptor, table_offset_dwords: 0, sgprs: 2, base_register: 0, user_data: 4, slot: 3 };
    let used = UsedResource {
        kind: ResourceType::Uav,
        root: RootType::Descriptor,
        table_offset_dwords: 0,
        lower_bound: 0,
        stride: 0,
        dwords: 4,
        user_data: 4,
        slot: 3,
    };
    (vec![param], vec![used])
}

/// Six user SGPRs, group id x enabled (s6), one thread-id VGPR.
const RSRC2_ROOT: u32 = 6 << 1 | 1 << 7;

#[test]
fn a_root_view_by_group_and_thread() {
    let (layout, used) = root_layout();
    let mut words = root_view_words();
    words.push(0x8F07_8806); // s_lshl_b32 s7, s6, 8
    words.push(vop2(V_LSHLREV_B32, 1, 0x82, 0));
    words.push(vop2(V_ADD_NC_U32, 1, 7, 1)); // v_add_nc_u32 v1, s7, v1
    words.extend(store(1, true, false, 0));
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_ROOT);
    assert_eq!(f.stores.len(), 1, "{f:?}");
    assert_eq!(f.stores[0].target, Target::Root { slot: 3 });
    let b = bind([64, 1, 1], Some([3, 1, 1]), &none);
    assert_eq!(f.ranges(Target::Root { slot: 3 }, &b), Some(vec![(0, 2 * 256 + 63 * 4 + 4)]));
    // An indirect dispatch has no group counts.
    assert_eq!(f.ranges(Target::Root { slot: 3 }, &bind([64, 1, 1], None, &none)), None);
}

#[test]
fn a_loop_carried_offset_is_not_known() {
    let (layout, used) = table_layout();
    let mut words = load_descriptor(125).to_vec();
    words.push(0x7E02_0280); // v_mov_b32 v1, 0
    words.extend(store(1, true, false, 0)); // L:
    words.push(vop2(V_ADD_NC_U32, 1, 0x84, 1)); // v_add_nc_u32 v1, 4, v1
    words.push(0x7D88_02C0); // v_cmp_gt_u32 vcc, 64, v1
    words.push(0xBF87_FFFB); // s_cbranch_vccnz L
    words.push(S_ENDPGM);
    let (f, causes) = analyze_causes(&compute(&words, layout, used, RSRC2_TABLE, 64)).expect("the front half runs");
    assert_eq!(f.ranges(table_target(), &bind([64, 1, 1], Some([1, 1, 1]), &none)), None);
    assert_eq!(causes, vec![Some(crate::footprint::Cause::Cyclic)]);
}

#[test]
fn a_formatted_store_is_not_known_but_other_views_are() {
    let (layout, used) = table_layout();
    let mut words = load_descriptor(125).to_vec();
    // buffer_store_format_x v2, off, s[8:11], 0 (MUBUF opcode 4)
    words.extend([0x38 << 26 | 4 << 18, 0x80 << 24 | 2 << 16 | 2 << 8]);
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    let b = bind([64, 1, 1], Some([1, 1, 1]), &none);
    assert!(!f.unbounded);
    assert_eq!(f.ranges(table_target(), &b), None);
    assert_eq!(f.ranges(Target::Table { user_data: 9, dword: 0 }, &b), Some(vec![]));
}

#[test]
fn an_indirect_jump_leaves_nothing_known() {
    let (layout, used) = table_layout();
    let mut words = load_descriptor(125).to_vec();
    words.extend(store(0, false, false, 0));
    words.push(0xBE80_2000 | 6); // s_setpc_b64 s[6:7]
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    assert!(f.unbounded);
    assert_eq!(f.ranges(table_target(), &bind([64, 1, 1], None, &none)), None);
}

#[test]
fn a_program_that_stores_nothing_has_an_empty_footprint() {
    let (layout, used) = table_layout();
    let mut words = load_descriptor(125).to_vec();
    // buffer_load_dword v2, off, s[8:11], 0
    words.extend([0x38 << 26 | 0x0C << 18, 0x80 << 24 | 2 << 16 | 2 << 8]);
    words.push(S_ENDPGM);
    let f = footprint(&words, layout, used, RSRC2_TABLE);
    assert_eq!(f, Footprint::default());
    assert_eq!(f.ranges(table_target(), &bind([1, 1, 1], None, &none)), Some(vec![]));
}

#[test]
fn ranges_are_joined_and_at_most_four() {
    let at = |offset: u32| crate::footprint::Store { target: table_target(), offset: Expr::Const(offset), bytes: 4 };
    let f = Footprint { stores: [0u32, 4, 100, 200, 300, 400, 500].map(at).to_vec(), ..Footprint::default() };
    let b = bind([1, 1, 1], None, &none);
    // 0..8 are one, then 100, 200, 300, 400, 500: six ranges, the nearest joined.
    let got = f.ranges(table_target(), &b).unwrap();
    assert_eq!(got.len(), 4);
    assert!(got.windows(2).all(|w| w[0].1 <= w[1].0));
    for offset in [0u64, 4, 100, 200, 300, 400, 500] {
        assert!(got.iter().any(|r| r.0 <= offset && offset + 4 <= r.1));
    }
}
