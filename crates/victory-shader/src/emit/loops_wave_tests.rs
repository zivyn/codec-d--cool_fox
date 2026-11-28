//! `VICTORY_LOOPS_WAVE` and `VICTORY_LDS_FIRST_INIT` (the water's tile generation and tiled light
//! on hosts of 32-lane waves): synthetic console wave64 programs of the same shapes, lowered with
//! and without the switches (and validated by `dxil.dll` when present), and run on the host GPU
//! where there is one. Nothing of the game is in them.

use std::collections::HashSet;

use super::host::{Dispatch, Host, Root, View};
use super::local_exit_tests::validated;
use super::wave64_tests::native_of;
use super::{HostShader, lower};
use crate::dxbc::Container;
use crate::native::StageIo;
use crate::{ShaderError, io, ir, resource, structure};

/// A console wave64 program of 64 threads of the shape of a tile list's generation: a loop in
/// which lane `n` goes round `(n & 7) + 1` times and leaves it when its own count is through (the
/// mask of the lanes still in it, `s[12:13]`, loses the lanes that are through; the loop ends
/// when it is empty, `s_cbranch_scc1` on the `s_andn2_b64` result), then, with every lane back,
/// the count of the wave's lanes (`s_bcnt1_i32_b64 exec`) and each lane's rank among them
/// (`v_mbcnt`), which the lanes store: lane `n` stores `n`, and the count, 64, at `n + 64`.
const TILE_COUNT_LOOP: [u32; 25] = [
    0x3602_0087, // v_and_b32 v1, 7, v0
    0x4A02_0281, // v_add_nc_u32 v1, 1, v1
    0x7E04_0280, // v_mov_b32 v2, 0
    0xBE82_047E, // s_mov_b64 s[2:3], exec
    0xBE8C_047E, // s_mov_b64 s[12:13], exec
    0xBE80_047E, // s_mov_b64 s[0:1], exec
    0x87FE_0C00, // L: s_and_b64 exec, s[0:1], s[12:13]
    0x4A04_0481, // v_add_nc_u32 v2, 1, v2
    0xBEFE_0400, // s_mov_b64 exec, s[0:1]
    0x7D8C_0302, // v_cmp_ge_u32 vcc, v2, v1
    0xBE8E_246A, // s_and_saveexec_b64 s[14:15], vcc
    0x8A8C_7E0C, // s_andn2_b64 s[12:13], s[12:13], exec
    0xBF85_FFF9, // s_cbranch_scc1 L
    0xBEFE_0402, // s_mov_b64 exec, s[2:3]
    0xBE96_107E, // s_bcnt1_i32_b64 s22, exec
    0xD765_0005, // v_mbcnt_lo_u32_b32 v5, -1, 0
    0x0001_00C1,
    0xD766_0006, // v_mbcnt_hi_u32_b32 v6, -1, v5
    0x0002_0AC1,
    0x7E0E_0216, // v_mov_b32 v7, s22
    0x3408_0082, // v_lshlrev_b32 v4, 2, v0
    0xE070_1000, // buffer_store_dword v6, v4, s[8:11], 0 offen
    0x8002_0604,
    0xE070_1100, // buffer_store_dword v7, v4, s[8:11], 0 offen offset:256
    0x8002_0704,
];

/// What each of the 128 words of the program's output holds after a dispatch on the host GPU
/// (None without one).
fn on_the_card(shader: &HostShader) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("SKIPPED (no wave32 hardware): no SM 6.6 hardware device; the GPU part of this test did not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("SKIPPED (no wave32 hardware): the hardware device does not take this program; the GPU part of this test did not run");
        return None;
    };
    let user = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch { roots: vec![(0, Root::Constants(user))], views: &[View::RawUav(128)], samplers: &[], groups: [1, 1, 1] },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].to_vec())
}

/// What the lanes of `TILE_COUNT_LOOP` store when every lane is back after the loop.
fn tile_count_expected() -> Vec<u32> {
    (0..64).chain(std::iter::repeat_n(64, 64)).collect()
}

/// `native` for a host of 32-lane waves through `emit_program` (the form cascade is not
/// involved), with `VICTORY_LOOPS_WAVE` set to `loops` for this thread, and
/// `VICTORY_LDS_FIRST_INIT` to `first`.
fn emitted(native: &crate::native::NativeProgram, loops: Option<u8>, first: bool) -> Result<HostShader, ShaderError> {
    emitted_wide(native, &HashSet::new(), loops, first)
}

/// [`emitted`] with the branches closing the blocks of `wide` taken wave-wide (the form the
/// water's tiled light ends in: every lane runs the code behind such a branch, guarded by its
/// EXEC bit).
fn emitted_wide(
    native: &crate::native::NativeProgram,
    wide: &HashSet<usize>,
    loops: Option<u8>,
    first: bool,
) -> Result<HostShader, ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    let st = structure::structure_wide(&p, wide).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    super::TEST_LOOPS_WAVE.set(loops);
    lower::TEST_LDS_FIRST_INIT.set(Some(first));
    let result = super::emit_program(&c, native, &p, &st, &io, &res);
    lower::TEST_LDS_FIRST_INIT.set(None);
    super::TEST_LOOPS_WAVE.set(None);
    lower::TEST_HOST_WAVE.set(was);
    result.map(|(shader, _)| shader)
}

/// The branches that leave the loop of `TILE_COUNT_LOOP` are the ones a host wave takes together, and
/// only those: the structure with them keeps their `when` and names them in `host_wide`.
#[test]
fn the_branches_that_leave_a_loop_are_found_and_taken_by_each_host_wave() {
    let native = native_of(&TILE_COUNT_LOOP, true, 64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    assert!(st.host_wide.is_empty(), "no branch is the host wave's before the retry");
    let loops = lower::loop_branches(&p, &st);
    eprintln!("loop branches: {loops:?}");
    assert!(!loops.is_empty(), "the loop of the program has a branch that leaves it");
    for &b in &loops {
        assert!(matches!(st.exits[b], crate::structure::LaneExit::Branch { .. }), "block {b}");
    }
    let host = structure::structure_wide_host(&p, &st.wide, &loops).expect("structure");
    assert_eq!(host.host_wide, loops);
    assert!(loops.iter().all(|b| host.wide.contains(b)), "taken wave-wide, by each host wave");
    // (a branch the wave takes together already is not asked again)
    assert!(lower::loop_branches(&p, &host).is_empty());
}

/// The switch changes the translation (the loop's exits are each host wave's own answer) and only
/// when set; both translations validate.
#[test]
fn the_loops_wave_switch_changes_the_translation_only_when_set() {
    let native = native_of(&TILE_COUNT_LOOP, true, 64);
    let off = emitted(&native, None, false);
    let on = emitted(&native, Some(1), false);
    eprintln!(
        "off: {:?}; on: {:?}",
        off.as_ref().map(|s| s.dxil.len()).map_err(ToString::to_string),
        on.as_ref().map(|s| s.dxil.len()).map_err(ToString::to_string)
    );
    let Ok(off) = off else { return };
    validated(&off);
    assert_eq!(off.dxil, emitted(&native, None, false).expect("lowered").dxil);
    let Ok(on) = on else { return };
    validated(&on);
    assert_eq!(on.dxil, emitted(&native, Some(1), false).expect("lowered").dxil);
    assert_ne!(off.dxil, on.dxil, "the loop's exits are taken by each host wave");
    // (`=2` is every compute program: this one too)
    assert_eq!(on.dxil, emitted(&native, Some(2), false).expect("lowered").dxil);
}

/// On the host GPU: what the lanes of `TILE_COUNT_LOOP` store without and with the loops taken by
/// each host wave together (printed: the card decides whether the lanes that left the loop early
/// run on before the others and count themselves as a part of the wave; the one with the switch
/// must be right).
#[test]
fn the_lanes_behind_a_loop_see_the_whole_wave() {
    let native = native_of(&TILE_COUNT_LOOP, true, 64);
    let Ok(off) = emitted(&native, None, false) else { return };
    let Ok(on) = emitted(&native, Some(1), false) else { return };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(before) = on_the_card(&off) else { return };
    let Some(after) = on_the_card(&on) else { return };
    let expected = tile_count_expected();
    let right = |lanes: &[u32]| lanes.iter().zip(&expected).filter(|(a, b)| a == b).count();
    eprintln!("words as the console has them (of 128): without the switch {}, with it {}", right(&before), right(&after));
    eprintln!("without: {before:?}");
    eprintln!("with: {after:?}");
    assert_eq!(after, expected);
}

/// A console wave64 loop over the lanes left in a mask whose end is found behind an inner skip (the
/// `s_cbranch_scc0` after `s_andn2_b64` of the mask), the whole of it behind a skip taken when no lane
/// is left (`s_cbranch_execz DONE`). With no lane left the loop never ends (EXEC stays empty, the
/// inner skip is always taken, the end is never reached), which the console never runs: the
/// skip must stay where the forms without skips would take it out, with
/// `VICTORY_KEEP_STALLING_SKIPS`.
const SKIP_OVER_A_STALLING_LOOP: [u32; 24] = [
    0x3408_0082, // v_lshlrev_b32 v4, 2, v0
    0x3602_0087, // v_and_b32 v1, 7, v0
    0x4A02_0281, // v_add_nc_u32 v1, 1, v1
    0x7E04_0280, // v_mov_b32 v2, 0
    0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0xBF88_000B, // s_cbranch_execz DONE
    0xBE94_047E, // s_mov_b64 s[20:21], exec
    0xBEFE_0414, // L: s_mov_b64 exec, s[20:21]
    0xBE98_047E, // s_mov_b64 s[24:25], exec
    0x7DA4_0280, // v_cmpx_eq_u32 exec, 0, v1
    0xBF88_0002, // s_cbranch_execz SKIP
    0x8A94_7E14, // s_andn2_b64 s[20:21], s[20:21], exec
    0xBF84_0004, // s_cbranch_scc0 DONE
    0x8AFE_7E18, // SKIP: s_andn2_b64 exec, s[24:25], exec
    0x4A04_0481, // v_add_nc_u32 v2, 1, v2
    0x4A02_02C1, // v_add_nc_u32 v1, -1, v1
    0xBF82_FFF6, // s_branch L
    0xBEFE_040C, // DONE: s_mov_b64 exec, s[12:13]
    0x7E06_0282, // v_mov_b32 v3, 2
    0xE070_1000, // buffer_store_dword v3, v4, s[8:11], 0 offen
    0x8002_0304,
    0xBF81_0000, // s_endpgm
    0xBF81_0000,
];

/// With `VICTORY_KEEP_STALLING_SKIPS` the skip over a loop that never ends with no lane left stays;
/// without it the form without skips takes it out.
#[test]
fn a_skip_over_a_loop_that_never_ends_with_no_lane_left_stays_when_asked() {
    let native = native_of(&SKIP_OVER_A_STALLING_LOOP, true, 64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let was = ir::TEST_KEEP_STALLING_SKIPS.replace(Some(false));
    let off = ir::drop_skips_as(&p, true, false);
    ir::TEST_KEEP_STALLING_SKIPS.set(Some(true));
    let on = ir::drop_skips_as(&p, true, false);
    ir::TEST_KEEP_STALLING_SKIPS.set(was);
    assert!(off.0.is_some(), "without the switch the skip is taken out");
    assert!(on.0.is_none(), "with it the skip stays");
}

/// 32 threads of a console wave64: every lane writes `0xDEAD` to `lds[0]`; then the lanes but the
/// first (the first thread's pixel is not water, say: `v_cmpx_ne_u32 exec, 0, v0` leaves it out)
/// reach the thread group's set-up, `v_cmpx_eq_u32 exec, 0, v0` and `lds[0] = 0x4D`, which on the
/// console no lane runs. Then every lane that is left stores `lds[0]` at its own place.
fn first_thread_init_after_the_first_thread_left() -> crate::native::NativeProgram {
    let words = [
        0x7E0C_0280, // v_mov_b32 v6, 0
        0x7E12_02FF, // v_mov_b32 v9, lit
        0x0000_DEAD,
        0xD834_0000, // ds_write_b32 v6, v9
        0x0000_0906,
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xBE8C_047E, // s_mov_b64 s[12:13], exec
        0x7DAA_0080, // v_cmpx_ne_u32 exec, 0, v0
        0xBF88_0000 | 15, // s_cbranch_execz END
        0xBE8E_047E, // s_mov_b64 s[14:15], exec
        0x7DA4_0080, // v_cmpx_eq_u32 exec, 0, v0
        0x7E0C_0280, // v_mov_b32 v6, 0
        0x7E0E_02FF, // v_mov_b32 v7, lit
        0x0000_004D,
        0xD834_0000, // ds_write_b32 v6, v7
        0x0000_0706,
        0xBEFE_040E, // s_mov_b64 exec, s[14:15]
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD8D8_0000, // ds_read_b32 v8, v6
        0x0800_0006,
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0xE070_1000, // buffer_store_dword v8, v3, s[8:11], 0 offen
        0x8002_0803,
        0xBEFE_040C, // END: s_mov_b64 exec, s[12:13]
    ];
    let mut native = native_of(&words, true, 32);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    native
}

/// The switch changes the translation (the first thread sets shared memory up whether its lane
/// runs or not) and only when set; the branch around the set-up is taken wave-wide (block 0), so that
/// the first thread's lane is where the set-up is.
#[test]
fn the_lds_first_init_switch_changes_the_translation_only_when_set() {
    let native = first_thread_init_after_the_first_thread_left();
    let wide = HashSet::from([0]);
    let off = emitted_wide(&native, &wide, None, false);
    let on = emitted_wide(&native, &wide, None, true);
    eprintln!(
        "off: {:?}; on: {:?}",
        off.as_ref().map(|s| s.dxil.len()).map_err(ToString::to_string),
        on.as_ref().map(|s| s.dxil.len()).map_err(ToString::to_string)
    );
    let off = off.expect("lowered without the switch");
    let on = on.expect("lowered with the switch");
    validated(&off);
    validated(&on);
    assert_ne!(off.dxil, on.dxil);
    assert_eq!(on.dxil, emitted_wide(&native, &wide, None, true).expect("lowered").dxil);
}

/// On the host GPU: without the switch the console's result (no lane runs the set-up, the
/// `0xDEAD` of the lanes' own earlier write stays), with it the first thread's constant.
#[test]
fn the_first_thread_sets_shared_memory_up_when_asked() {
    let native = first_thread_init_after_the_first_thread_left();
    let wide = HashSet::from([0]);
    let (Ok(off), Ok(on)) = (emitted_wide(&native, &wide, None, false), emitted_wide(&native, &wide, None, true)) else {
        return;
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(before) = on_the_card_32(&off) else { return };
    let Some(after) = on_the_card_32(&on) else { return };
    eprintln!("without: {before:?}");
    eprintln!("with: {after:?}");
    // (lane 0 stores nothing)
    assert!(before[1..32].iter().all(|&v| v == 0xDEAD), "as the console: the set-up is not run");
    assert!(after[1..32].iter().all(|&v| v == 0x4D), "the first thread's set-up is run");
}

/// The 64 words of a 32-thread program's output.
fn on_the_card_32(shader: &HostShader) -> Option<Vec<u32>> {
    on_the_card(shader).map(|mut words| {
        words.truncate(64);
        words
    })
}
