//! The fences a compute program gets where it waits on flags other groups write: a loop that
//! reads a raw UAV buffer with `glc` until a flag is set, then stores and publishes with an
//! atomic. Lowered (and validated by `dxil.dll` when present); counted, not run: whether the
//! host orders the stores is a matter of its memory model, which only the game shows.

use super::wave64_tests::native_of;
use super::{lower, validator};
use crate::{io, ir, resource, structure};

/// `(waiting loops, fences placed)` of the program `body` (32 threads, a console wave of 32).
fn counts(body: &[u32]) -> (usize, u32) {
    let native = native_of(body, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
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
    });
    lower::TEST_HOST_WAVE.set(was);
    let lowered = lowered.expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = lowered.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    (lowered.spin_loops, lowered.store_fences)
}

/// A loop on the flag at byte 0 (`glc` when `coherent`), then a store, the wait for it, and an
/// atomic add on the same buffer.
fn wait_then_publish(coherent: bool) -> Vec<u32> {
    vec![
        0x7E02_0280,                                      // v_mov_b32 v1, 0
        if coherent { 0xE030_D000 } else { 0xE030_1000 }, // buffer_load_dword v2, v1, s[8:11], 0 offen (glc dlc)
        0x8002_0201,
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0x7E0C_0502, // v_readfirstlane_b32 s6, v2
        0xBF06_8106, // s_cmp_eq_u32 s6, 1
        0xBF84_FFFA, // s_cbranch_scc0 back to the load
        0x7E06_0285, // v_mov_b32 v3, 5
        0xE070_1000, // buffer_store_dword v3, v1, s[8:11], 0 offen
        0x8002_0301,
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xE0C8_1000, // buffer_atomic_add v3, v1, s[8:11], 0 offen
        0x8002_0301,
    ]
}

#[test]
fn a_loop_on_a_coherent_flag_fences_at_its_exit_and_before_the_atomic_that_publishes() {
    let (loops, fences) = counts(&wait_then_publish(true));
    assert_eq!(loops, 1, "one waiting loop");
    // (one where the loop is left, one between the store and the atomic)
    assert_eq!(fences, 2);
}

#[test]
fn a_loop_that_does_not_read_with_glc_gets_no_fences() {
    assert_eq!(counts(&wait_then_publish(false)), (0, 0));
}

#[test]
fn stores_without_a_waiting_loop_get_no_fences() {
    let body = [
        0x7E02_0280, // v_mov_b32 v1, 0
        0x7E06_0285, // v_mov_b32 v3, 5
        0xE070_1000, // buffer_store_dword v3, v1, s[8:11], 0 offen
        0x8002_0301,
        0xBF8C_3F70, // s_waitcnt vmcnt(0)
        0xE0C8_1000, // buffer_atomic_add v3, v1, s[8:11], 0 offen
        0x8002_0301,
    ];
    assert_eq!(counts(&body), (0, 0));
}
