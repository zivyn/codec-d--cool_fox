//! `VICTORY_LOCAL_EXIT`: a console wave64 loop over the lanes left in a mask, translated as two
//! host waves of 32 that each run as a wave of their own, leaves the loop in every lane of the
//! host wave together. Lowered (and validated by `dxil.dll` when present) with and without the
//! switch; on the host GPU where there is one.

use super::host::{Dispatch, Host, Root, View};
use super::wave64_tests::native_of;
use super::{HostShader, lower, validator};
use crate::dxbc::Container;
use crate::{ShaderError, io, ir, resource, structure};

/// What lane 5 holds when the loop is through: lanes 5 and 37 (lane 5 of each half) go round
/// `(5 & 7) + 1` times.
const EXPECTED: u32 = 6;

/// A console wave64 program of 64 threads. Lanes 0..39 are in the loop's mask (s[20:21]), lanes
/// 40..63 are not (the upper half has 24 lanes outside it). Lane `n` goes round `(n & 7) + 1`
/// times (v2 counts the rounds, v1 what is left); each round the lanes with nothing left (the
/// `v_cmpx` lanes) leave the mask, and the loop is through when the mask is empty (the
/// `s_cbranch_scc0` on the `s_andn2_b64` result, the form of the title's decal list walk). Then
/// EXEC is the whole wave again and every lane stores what lane 5 holds in v2 (the number of
/// rounds of its own half's lane 5), at its own place: all 64 lanes store 6, with the lanes of
/// the upper half outside the mask too.
const MASK_LOOP: [u32; 23] = [
    0x3408_0082, // v_lshlrev_b32 v4, 2, v0
    0x3602_0087, // v_and_b32 v1, 7, v0
    0x4A02_0281, // v_add_nc_u32 v1, 1, v1
    0x7E04_0280, // v_mov_b32 v2, 0
    0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
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
    0xD760_0006, // v_readlane_b32 s6, v2, 5
    0x0001_0B02,
    0x7E06_0206, // v_mov_b32 v3, s6
    0xE070_1000, // buffer_store_dword v3, v4, s[8:11], 0 offen
    0x8002_0304,
];

/// `MASK_LOOP` as the title's shaders are translated for a host of 32-lane waves, in the form
/// where each host wave is a wave of its own, with or without `VICTORY_LOCAL_EXIT`.
fn translated(together: bool) -> Result<HostShader, ShaderError> {
    let native = native_of(&MASK_LOOP, true, 64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    lower::TEST_LOCAL_EXIT.set(together);
    let result = if together {
        super::local_exit(&c, &native, &p, true, true).expect("a branch that leaves the loop")
    } else {
        let st = structure::structure(&p).expect("structure");
        let res =
            resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
        let io = io::raise(&p, &native).expect("io::raise");
        lower::with_local_waves_any(|| super::emit_program(&c, &native, &p, &st, &io, &res))
    };
    lower::TEST_LOCAL_EXIT.set(false);
    lower::TEST_HOST_WAVE.set(was);
    // (`VICTORY_TEST_DUMP=<folder outside the repository>`: the program as it was translated)
    if let (Some(dir), Ok((s, _))) = (std::env::var_os("VICTORY_TEST_DUMP"), &result) {
        let _ = std::fs::write(
            std::path::Path::new(&dir).join(format!("mask-loop-{together}.dxil")),
            &s.dxil,
        );
    }
    result.map(|(shader, _)| shader)
}

fn validated(shader: &HostShader) {
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
}

/// What each of the 64 lanes stored, on the host GPU (None without one).
fn lanes_on_the_card(shader: &HostShader) -> Option<Vec<u32>> {
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return None;
    };
    let Ok((root, pso)) = host.compute_pipeline(shader) else {
        eprintln!("the hardware device does not take this program; not run");
        return None;
    };
    let user = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(user))],
                views: &[View::RawUav(64)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0].to_vec())
}

/// The switch changes the translation (the branch that leaves the loop is taken by the host wave
/// together) and only when set.
#[test]
fn the_local_exit_switch_changes_the_translation_only_when_set() {
    let off = match translated(false) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("not translated for waves of 32 ({e}): the program of the test is refused");
            return;
        }
    };
    validated(&off);
    assert_eq!(off.dxil, translated(false).expect("lowered").dxil);
    let on = translated(true).expect("lowered with the loop's exit taken by the host wave");
    validated(&on);
    assert_ne!(off.dxil, on.dxil);
    assert_eq!(on.dxil, translated(true).expect("lowered").dxil);
}

/// The lanes outside the loop's mask meet the code after the loop with the others: all 64 lanes
/// store lane 5's count. Without the switch the 24 lanes of the upper half that are not in the
/// mask never meet the branch that ends the loop, go round to its limit and leave as killed
/// (they store nothing: 0).
#[test]
fn the_lanes_outside_a_loops_mask_leave_it_with_the_others() {
    let Ok(off) = translated(false) else { return };
    let on = translated(true).expect("lowered with the loop's exit taken by the host wave");
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(before) = lanes_on_the_card(&off) else {
        return;
    };
    let Some(after) = lanes_on_the_card(&on) else {
        return;
    };
    let right = |lanes: &[u32]| lanes.iter().filter(|&&v| v == EXPECTED).count();
    eprintln!(
        "lanes that stored {EXPECTED}: without the switch {} of 64, with it {} of 64",
        right(&before),
        right(&after)
    );
    eprintln!("without: {before:?}");
    eprintln!("with: {after:?}");
    assert_eq!(
        after,
        vec![EXPECTED; 64],
        "every lane stores lane 5's number of rounds"
    );
}
