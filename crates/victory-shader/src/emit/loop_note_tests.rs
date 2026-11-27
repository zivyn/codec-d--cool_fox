//! A loop that reaches its iteration limit leaves a note in the faults buffer when
//! `VICTORY_LOOP_NOTES=1`: lowered (and validated by `dxil.dll` when present) with the notes
//! on and off, the notes add code and the validator takes both. The host's reading of the
//! notes is `memory.rs`.

use super::wave64_tests::native_of;
use super::{lower, validator};
use crate::{io, ir, resource, structure};

fn dxil(body: &[u32], notes: bool) -> Vec<u8> {
    let native = native_of(body, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    lower::set_program_tag(b"a program");
    let (was, was_notes) = (
        lower::TEST_HOST_WAVE.replace(Some(32)),
        lower::TEST_LOOP_NOTES.replace(notes),
    );
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
    lower::TEST_HOST_WAVE.set(was);
    lower::TEST_LOOP_NOTES.set(was_notes);
    let lowered = lowered.expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = lowered.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    lowered.dxil
}

/// A loop closed by a conditional branch back to its header: the flag at byte 0 is read until
/// it is 1.
const WAIT: [u32; 9] = [
    0x7E02_0280, // v_mov_b32 v1, 0
    0xE030_1000, // L: buffer_load_dword v2, v1, s[8:11], 0 offen
    0x8002_0201,
    0xBF8C_3F70, // s_waitcnt vmcnt(0)
    0x7E0C_0502, // v_readfirstlane_b32 s6, v2
    0xBF06_8106, // s_cmp_eq_u32 s6, 1
    0xBF84_FFFA, // s_cbranch_scc0 L
    0x7E06_0285, // v_mov_b32 v3, 5
    0xBF80_0000, // s_nop
];

/// A loop closed by a jump back, left from its middle after ten rounds.
const COUNT: [u32; 7] = [
    0xBE86_0380, // s_mov_b32 s6, 0
    0x8006_8106, // L: s_add_u32 s6, s6, 1
    0xBF09_8A06, // s_cmp_ge_u32 s6, 10
    0xBF85_0001, // s_cbranch_scc1 past the jump
    0xBF82_FFFC, // s_branch L
    0x7E06_0285, // v_mov_b32 v3, 5
    0xBF80_0000, // s_nop
];

#[test]
fn a_loop_closed_by_a_branch_notes_its_cut_when_asked() {
    assert!(dxil(&WAIT, true).len() > dxil(&WAIT, false).len());
}

#[test]
fn a_loop_closed_by_a_jump_notes_its_cut_when_asked() {
    assert!(dxil(&COUNT, true).len() > dxil(&COUNT, false).len());
}

#[test]
fn without_the_switch_the_translation_is_the_same_each_time() {
    assert_eq!(dxil(&WAIT, false), dxil(&WAIT, false));
}
