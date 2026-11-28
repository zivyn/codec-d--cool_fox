//! `VICTORY_LOOPS_WAVE` and `VICTORY_LDS_FIRST_INIT` (the water's tile generation and tiled light
//! on hosts of 32-lane waves): synthetic console wave64 programs of the same shapes, lowered with
//! and without the switches (and validated by `dxil.dll` when present), and run on the host GPU
//! where there is one. Nothing of the game is in them.

use super::local_exit_tests::{EXPECTED, MASK_LOOP, lanes_on_the_card, validated};
use super::wave64_tests::native_of;
use super::{HostShader, lower};
use crate::dxbc::Container;
use crate::native::StageIo;
use crate::{ShaderError, io, ir, resource, structure};

/// `native` for a host of 32-lane waves through `emit_program` (the form cascade is not
/// involved), with `VICTORY_LOOPS_WAVE` set to `loops` for this thread, and
/// `VICTORY_LDS_FIRST_INIT` to `first`.
fn emitted(native: &crate::native::NativeProgram, loops: Option<u8>, first: bool) -> Result<HostShader, ShaderError> {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    let st = structure::structure(&p).expect("structure");
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

/// The branches that leave the loop of `MASK_LOOP` are the ones a host wave takes together, and
/// only those: the structure with them keeps their `when` and names them in `host_wide`.
#[test]
fn the_branches_that_leave_a_loop_are_found_and_taken_by_each_host_wave() {
    let native = native_of(&MASK_LOOP, true, 64);
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
    let native = native_of(&MASK_LOOP, true, 64);
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

/// On the host GPU, the lanes of `MASK_LOOP` that store the count of lane 5's rounds, without and
/// with the loops taken by each host wave together (printed: the card decides whether the lanes
/// that left the loop early run on before the others; the one with the switch must be right).
#[test]
fn the_lanes_behind_a_loop_see_the_whole_host_wave() {
    let Ok(off) = emitted(&native_of(&MASK_LOOP, true, 64), None, false) else { return };
    let Ok(on) = emitted(&native_of(&MASK_LOOP, true, 64), Some(1), false) else { return };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(before) = lanes_on_the_card(&off) else { return };
    let Some(after) = lanes_on_the_card(&on) else { return };
    let right = |lanes: &[u32]| lanes.iter().filter(|&&v| v == EXPECTED).count();
    eprintln!("lanes that stored {EXPECTED}: without {} of 64, with {} of 64", right(&before), right(&after));
    eprintln!("without: {before:?}");
    eprintln!("with: {after:?}");
    assert_eq!(after, vec![EXPECTED; 64]);
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
/// runs or not) and only when set.
#[test]
fn the_lds_first_init_switch_changes_the_translation_only_when_set() {
    let native = first_thread_init_after_the_first_thread_left();
    let off = emitted(&native, None, false);
    let on = emitted(&native, None, true);
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
    assert_eq!(on.dxil, emitted(&native, None, true).expect("lowered").dxil);
}

/// On the host GPU: without the switch the console's result (no lane runs the set-up, the
/// `0xDEAD` of the lanes' own earlier write stays), with it the first thread's constant.
#[test]
fn the_first_thread_sets_shared_memory_up_when_asked() {
    let native = first_thread_init_after_the_first_thread_left();
    let (Ok(off), Ok(on)) = (emitted(&native, None, false), emitted(&native, None, true)) else {
        return;
    };
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let Some(before) = lanes_on_the_card(&off) else { return };
    let Some(after) = lanes_on_the_card(&on) else { return };
    eprintln!("without: {before:?}");
    eprintln!("with: {after:?}");
    // (lane 0 stores nothing)
    assert!(before[1..32].iter().all(|&v| v == 0xDEAD), "as the console: the set-up is not run");
    assert!(after[1..32].iter().all(|&v| v == 0x4D), "the first thread's set-up is run");
}
