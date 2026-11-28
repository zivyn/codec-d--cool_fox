//! The iteration limit of a translated loop (`VICTORY_LOOP_LIMITS`): a loop of more
//! iterations than its stage allows is cut where the stage says, and runs to its end when the
//! table names it (by stage, by program or by loop), on the host GPU where there is one. Lowered
//! and validated by `dxil.dll` when present. The notes the cuts leave are `loop_note_tests`.

use std::collections::HashSet;

use super::wave64_tests::{lowered, native_of, on_the_card};
use super::{HostShader, LoopLimits, lower, validator};
use crate::dxbc::Container;
use crate::{io, ir, resource, structure};

/// The iterations of the loop below.
const ROUNDS: u32 = 6000;

/// A loop of `ROUNDS` iterations (more than a compute loop gets by default, 4096) that counts in
/// s6 and stores the count to dword 0 of the raw UAV when it is through.
const LONG: [u32; 8] = [
    0xBE86_0380,          // s_mov_b32 s6, 0
    0x8006_8106,          // L: s_add_u32 s6, s6, 1
    0xB606_0000 | ROUNDS, // s_cmpk_ge_u32 s6, ROUNDS
    0xBF85_0001,          // s_cbranch_scc1 past the jump
    0xBF82_FFFC,          // s_branch L
    0x7E00_0206,          // v_mov_b32 v0, s6
    0x38 << 26 | 0x1C << 18,
    0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
];

/// The program lowered with `VICTORY_LOOP_LIMITS=spec`.
fn shader(spec: &str) -> HostShader {
    let native = native_of(&LONG, false, 32);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    lower::set_program_tag(b"a program");
    lower::TEST_LOOP_LIMITS.with_borrow_mut(|t| *t = Some(LoopLimits::parse(spec)));
    let result = lowered(&native, &p, &HashSet::new());
    lower::TEST_LOOP_LIMITS.with_borrow_mut(|t| *t = None);
    let shader = result.expect("lower");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// The first digits of the program's hash, as the logs name it.
fn hash_digits(n: usize) -> String {
    blake3::hash(b"a program").to_hex()[..n].to_string()
}

/// The same digits with the first one changed: another program.
fn other_digits(n: usize) -> String {
    let mut d = hash_digits(n).into_bytes();
    d[0] = if d[0] == b'0' { b'1' } else { b'0' };
    String::from_utf8(d).expect("hex")
}

#[test]
fn the_limit_a_table_names_is_the_one_in_the_translation() {
    let default = shader("").dxil;
    assert_eq!(
        default,
        shader("cs=4096").dxil,
        "the stage's own number named is the default"
    );
    assert_eq!(
        default,
        shader("ps=9000,vs=9000,ms=9000").dxil,
        "other stages"
    );
    let (mine, other) = (hash_digits(5), other_digits(5));
    assert_eq!(
        default,
        shader(&format!("{other}=9000")).dxil,
        "another program"
    );
    assert_eq!(
        default,
        shader(&format!("{mine}:1=9000")).dxil,
        "another loop of the program"
    );
    let raised = shader("cs=9000").dxil;
    assert_ne!(default, raised);
    assert_eq!(raised, shader(&format!("{mine}=9000")).dxil, "by program");
    assert_eq!(raised, shader(&format!("{mine}:0=9000")).dxil, "by loop");
    assert_eq!(
        raised,
        shader(&format!("{other}=100,{mine}=9000")).dxil,
        "among others"
    );
    assert_eq!(
        raised,
        shader(&format!("cs=100,{}=9000", hash_digits(7))).dxil,
        "the program beats its stage"
    );
}

/// The loop is cut at 4096 by default (the lane leaves as killed and stores nothing) and runs
/// its 6000 rounds when the table allows it.
#[test]
fn a_loop_of_more_iterations_than_its_stage_allows_completes_when_the_table_allows_it() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let mine = hash_digits(5);
    let cut = shader("");
    let Some(got) = on_the_card(&cut, [0, 0]) else {
        return;
    };
    assert_ne!(got, ROUNDS, "the default limit of a compute loop is 4096");
    for spec in [
        "cs=8192".to_string(),
        "cs=0x2000".to_string(),
        "cs=8k".to_string(),
        format!("{mine}=8192"),
        format!("{mine}:0=ROUNDS"),
        format!("{mine}:0=6001"),
    ] {
        let spec = spec.replace("ROUNDS", &ROUNDS.to_string());
        let Some(got) = on_the_card(&shader(&spec), [0, 0]) else {
            return;
        };
        assert_eq!(got, ROUNDS, "VICTORY_LOOP_LIMITS={spec}");
    }
    // (a limit that is lower than the loop cuts it again)
    let Some(got) = on_the_card(&shader("cs=5000"), [0, 0]) else {
        return;
    };
    assert_ne!(got, ROUNDS, "a limit of 5000 stops a loop of 6000");
    eprintln!(
        "ran on the hardware device: the loop of {ROUNDS} rounds, cut by default and complete when allowed"
    );
}

/// A console wave64 waterfall loop whose two halves are done at different rounds. Each lane
/// holds its half's number in v1 (lanes 0..31: 0, lanes 32..63: 1), the loop serves the lanes
/// that hold the first live lane's value in each round (`v_readfirstlane`, `v_cmpx_eq_u32`,
/// `s_andn2_wrexec_b64`, `s_cbranch_execnz`: the console compiler's own form), so the lower half
/// is served in round 1 and the upper half in round 2. Each served lane keeps s6, which
/// `v_readlane_b32` took from lane 63 (a wave64 program that crosses its halves, run as two host
/// waves of 32 that exchange). Lane 63 stores what it holds: 1 when it was served, -1 when the
/// loop left it behind (it is cut at its limit and the lane leaves as killed: nothing stored).
const WATERFALL_HALVES: [u32; 17] = [
    0x2C02_0085, // v_lshrrev_b32 v1, 5, v0
    0x7E04_02C1, // v_mov_b32 v2, -1
    0xD760_0006, // v_readlane_b32 s6, v1, 63
    0x0001_7F01,
    0xBE96_047E, // s_mov_b64 s[22:23], exec
    0xBE94_047E, // s_mov_b64 s[20:21], exec
    0x7E08_0501, // L: v_readfirstlane_b32 s4, v1
    0x7DA4_0204, // v_cmpx_eq_u32 exec, s4, v1
    0x7E04_0206, // v_mov_b32 v2, s6
    0xBE94_3A14, // s_andn2_wrexec_b64 s[20:21], s[20:21]
    0xBF89_FFFB, // s_cbranch_execnz L
    0xBEFE_0416, // s_mov_b64 exec, s[22:23]
    0x7D84_00BF, // v_cmp_eq_u32 vcc, 63, v0
    0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
    0x7E00_0302, // v_mov_b32 v0, v2
    0x38 << 26 | 0x1C << 18,
    0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
];

/// `WATERFALL_HALVES` as the title's shaders are translated for a host of 32-lane waves.
fn waterfall_on_32_lanes(half_exit: bool) -> Result<HostShader, crate::ShaderError> {
    let mut words = WATERFALL_HALVES.to_vec();
    words.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    let native = native_of(&words, true, 64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    lower::TEST_HALF_EXIT.set(half_exit);
    let shader = super::emit(&c, &native, &p, &st, &io, &res);
    lower::TEST_HALF_EXIT.set(false);
    lower::TEST_HOST_WAVE.set(was);
    // (`VICTORY_TEST_DUMP=<folder outside the repository>`: the program as it was translated)
    if let (Some(dir), Ok(s)) = (std::env::var_os("VICTORY_TEST_DUMP"), &shader) {
        let _ = std::fs::write(
            std::path::Path::new(&dir).join(format!("waterfall-halves-{half_exit}.dxil")),
            &s.dxil,
        );
    }
    shader
}

/// `VICTORY_HALF_EXIT=1` adds the clearing of the half's exchange dword where it leaves the loop,
/// and nothing without it.
#[test]
fn the_half_exit_switch_changes_the_translation_only_when_set() {
    let off = waterfall_on_32_lanes(false).expect("lowered").dxil;
    let on = waterfall_on_32_lanes(true).expect("lowered").dxil;
    assert_eq!(off, waterfall_on_32_lanes(false).expect("lowered").dxil);
    assert_ne!(off, on);
    assert!(on.len() > off.len());
}

/// A waterfall whose halves finish in different rounds finishes in every lane: the upper half
/// is not left running to the loop's limit when the lower half has left (without the switch it
/// was: lane 63 held -1).
#[test]
fn a_waterfall_loop_whose_halves_finish_in_different_rounds_serves_every_lane() {
    let shader = match waterfall_on_32_lanes(true) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("not translated for waves of 32 ({e}): the program of the test is refused");
            return;
        }
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(got) = on_the_card(&shader, [0, 0]) else {
        return;
    };
    assert_eq!(
        got, 1,
        "lane 63 (upper half) was served in round 2 of the waterfall"
    );
    eprintln!("ran on the hardware device: the waterfall of two rounds served the upper half");
}

/// A loop that the first console wave of a group of 128 threads leaves when a value that only lane
/// 63 holds says so: the other lanes hold 5, lane 63 holds -1, and every round reads lane 63 with
/// `v_readlane_b32`, the way a walk over a table decides that it is through. The second console
/// wave leaves at once. The loop is through in its first round; s4 counts the rounds.
const LANE_63_EXIT: [u32; 16] = [
    0xBE8C_047E, // s_mov_b64 s[12:13], exec
    0x7DA8_00C0, // v_cmpx_gt_u32 exec, 64, v0
    0xBF88_000D, // s_cbranch_execz past the store
    0xBE84_0380, // s_mov_b32 s4, 0
    0x7E04_0285, // L: v_mov_b32 v2, 5
    0x7E06_02C1, // v_mov_b32 v3, -1
    0x7D84_00BF, // v_cmp_eq_u32 vcc, 63, v0
    0x0004_0685, // v_cndmask_b32 v2, 5, v3, vcc
    0xD760_0003, // v_readlane_b32 s3, v2, 63
    0x0001_7F02,
    0x8104_8104, // s_add_i32 s4, s4, 1
    0xBF06_C103, // s_cmp_eq_u32 s3, -1
    0xBF84_FFF7, // s_cbranch_scc0 L
    0x7E00_0204, // v_mov_b32 v0, s4
    0x38 << 26 | 0x1C << 18,
    0x80 << 24 | 2 << 16, // buffer_store_dword v0, off, s[8:11], 0
];

/// What a lane 63 that is read to end a loop does in two host waves of 32: both have the value of
/// the lane in the upper half, so the loop is through in its first round (a stale value of the
/// exchange would have it run to its limit, as DefragHeap's walk does).
#[test]
fn a_loop_that_ends_on_the_value_of_lane_63_is_through_in_its_first_round() {
    let native = native_of(&LANE_63_EXIT, true, 128);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    lower::set_program_tag(b"a lane 63 loop");
    let shader = match lowered(&native, &p, &HashSet::new()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("not translated for waves of 32 ({e}): the program of the test is refused");
            return;
        }
    };
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let Some(got) = on_the_card(&shader, [0, 0]) else {
        return;
    };
    assert_eq!(got, 1, "the rounds of a loop that lane 63 ends in the first");
    eprintln!("ran on the hardware device: the loop on lane 63 was through in one round");
}
