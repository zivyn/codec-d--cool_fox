//! The form a console wave64 compute program is lowered in for a host of 32-lane waves
//! ([`super::Form`]): which choice `emit_forms` makes where `VICTORY_KEEP_LOOP_SKIPS` keeps a skip
//! over a loop. Lowered, validated by `dxil.dll` when present, and run on the host GPU where it
//! takes waves of 32 (WARP does not: the tests that run need the card).
//!
//! The programs are a lane branch around a region in which the two host waves meet (an LDS read
//! of what the lane 16 above wrote, in the console wave), with the region a loop of two rounds or
//! straight code, and the branch a skip (over the region to the join) or an early-out (to the end).
//! The lanes below s0 take the branch.

use super::host::{Dispatch, Host, Root, View};
use super::wave64_tests::native_of;
use super::{Form, HostShader, lower, validator};
use crate::dxbc::Container;
use crate::native::{NativeProgram, RootType, StageIo};
use crate::{ShaderError, io, ir, resource, structure};

/// What the branch goes around.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Over {
    /// A skip: the region, then the join (the restore of EXEC and a store) and the end.
    Skip,
    /// An early-out: the region, then the end of the program.
    EarlyOut,
}

/// The region: `v4 = v0 + 1; lds[t] = v4; out[t] = lds[(t + 16) & 63]`, twice if `looped`.
fn region(looped: bool) -> Vec<u32> {
    let mut w = Vec::new();
    if looped {
        w.push(0xBE95_0380); // s_mov_b32 s21, 0
    }
    w.extend_from_slice(&[
        0x4A08_0081, // L: v_add_nc_u32 v4, 1, v0
        0xD834_0000, // ds_write_b32 v3, v4
        0x0000_0403,
        0x4A0C_0090, // v_add_nc_u32 v6, 16, v0
        0x360C_0CBF, // v_and_b32 v6, 63, v6
        0x340C_0C82, // v_lshlrev_b32 v6, 2, v6
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xD8D8_0000, // ds_read_b32 v7, v6
        0x0700_0006,
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0xE070_1000, // buffer_store_dword v7, v3, s[8:11], 0 offen
        0x8002_0703,
    ]);
    if looped {
        w.extend_from_slice(&[
            0x8015_8115, // s_add_u32 s21, s21, 1
            0xBF0A_8215, // s_cmp_lt_u32 s21, 2
            0xBF85_FFF1, // s_cbranch_scc1 L
        ]);
    }
    w
}

/// The program (64 threads, one console wave; 512 bytes of LDS).
fn lane_branch(over: Over, looped: bool) -> NativeProgram {
    let region = region(looped);
    let mut words = vec![
        0xBE94_0380, // s_mov_b32 s20, 0
        0x7E08_02C1, // v_mov_b32 v4, -1
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0xD834_0000, // ds_write_b32 v3, v4
        0x0000_0403,
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        0x7D88_0000, // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A, // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | region.len() as u32, // s_cbranch_execz past the region
    ];
    words.extend(region);
    if over == Over::Skip {
        words.extend_from_slice(&[
            0xBEFE_040C, // s_mov_b64 exec, s[12:13]
            0x7E10_0214, // v_mov_b32 v8, s20
            0xE070_1100, // buffer_store_dword v8, v3, s[8:11], 0 offen offset:256
            0x8002_0803,
        ]);
    }
    let mut native = native_of(&words, true, 64);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    native
}

/// The program lowered as the title's are for a host of 32-lane waves, with the skips over loops
/// kept (`VICTORY_KEEP_LOOP_SKIPS`, the default) or left out, and the form it ended in.
fn lowered_as(native: &NativeProgram, keep: bool) -> (Result<HostShader, ShaderError>, Form) {
    lowered_with(native, keep, false)
}

/// [`lowered_as`], with the kept skips taken wave-wide (`VICTORY_KEPT_SKIPS_WIDE`) or not.
fn lowered_with(native: &NativeProgram, keep: bool, wide: bool) -> (Result<HostShader, ShaderError>, Form) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    lower::set_program_tag(b"a form test");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    ir::TEST_KEEP_LOOP_SKIPS.set(Some(keep));
    super::form::TEST_KEPT_SKIPS_WIDE.set(Some(wide));
    let result = super::emit_as(&c, native, &p, &st, &io, &res);
    super::form::TEST_KEPT_SKIPS_WIDE.set(None);
    ir::TEST_KEEP_LOOP_SKIPS.set(None);
    lower::TEST_HOST_WAVE.set(was);
    result
}

fn validated(shader: &HostShader) {
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
}

/// The form of the program with the switch on (the kept skips taken wave-wide or not) and off,
/// translated each way.
fn forms(native: &NativeProgram, wide: bool) -> ((HostShader, Form), (HostShader, Form)) {
    let (on, on_form) = lowered_with(native, true, wide);
    let (off, off_form) = lowered_with(native, false, false);
    let on = on.unwrap_or_else(|e| panic!("kept skips: {e} (form {on_form:?})"));
    let off = off.unwrap_or_else(|e| panic!("skips left out: {e} (form {off_form:?})"));
    validated(&on);
    validated(&off);
    ((on, on_form), (off, off_form))
}

/// A skip over a loop is kept by the switch, and the form of the program is still the exact one
/// it had without it: the skip left out, every lane walks the loop, no group barrier at a branch.
/// (It was an approximate form, each host wave a wave of its own, for 17 programs of the title,
/// and the wave-wide branches for 15, which put group barriers where the card was lost.)
#[test]
fn a_skip_over_a_loop_does_not_take_the_exact_form_from_the_program() {
    for over in [Over::Skip, Over::EarlyOut] {
        let native = lane_branch(over, true);
        let ((on, on_form), (off, off_form)) = forms(&native, false);
        let exact = if over == Over::Skip { Form::WithoutSkips } else { Form::WithoutEarlyOuts };
        assert_eq!(off_form, exact, "{over:?}: the skip left out");
        assert_eq!(on_form, Form::LoopSkipsLeftOut, "{over:?}: the skip kept");
        assert!(on_form.exact());
        assert_eq!(on.dxil, off.dxil, "{over:?}: the same program as without the switch");
    }
}

/// With `VICTORY_KEPT_SKIPS_WIDE=1` the kept skip (and the early-out over a loop, which the switch
/// kept as well) is taken by the whole wave, and the loop is code every lane runs: exact, with
/// the skip in the translation.
#[test]
fn a_kept_skip_over_a_loop_taken_wave_wide_is_an_exact_form() {
    for over in [Over::Skip, Over::EarlyOut] {
        let native = lane_branch(over, true);
        let ((on, on_form), (off, _)) = forms(&native, true);
        assert_eq!(on_form, Form::KeptSkipsWide, "{over:?}");
        assert!(on_form.exact());
        assert_ne!(on.dxil, off.dxil, "{over:?}: the skip over the loop is in the translation");
    }
}

/// What has no loop in it is as it was: the switch changes nothing.
#[test]
fn a_skip_or_an_early_out_without_a_loop_is_left_out_whatever_the_switch_says() {
    for (over, form) in [(Over::Skip, Form::WithoutSkips), (Over::EarlyOut, Form::WithoutEarlyOuts)] {
        let native = lane_branch(over, false);
        let ((on, on_form), (off, off_form)) = forms(&native, false);
        assert_eq!((on_form, off_form), (form, form), "{over:?}");
        assert_eq!(on.dxil, off.dxil, "{over:?}");
    }
}

/// The skip that the scalars after the join keep stays the wave-wide form last of all, as before
/// (the switch is not what keeps it).
#[test]
fn a_skip_the_scalars_keep_is_the_last_resort_whatever_the_switch_says() {
    let native = super::wave64_tests::lds_read_after_write_in_a_lane_branch();
    let ((on, on_form), (off, off_form)) = forms(&native, false);
    assert_eq!((on_form, off_form), (Form::WaveWideLastResort, Form::WaveWideLastResort));
    assert_eq!(on.dxil, off.dxil);
}

/// The lanes below `below` run the region; the output as the console leaves it.
fn expected(below: u32, over: Over) -> Vec<u32> {
    let mut out = vec![0u32; 128];
    for t in 0..64u32 {
        if t >= below {
            continue;
        }
        let from = (t + 16) & 63;
        out[t as usize] = if from < below { from + 1 } else { u32::MAX };
        if over == Over::Skip {
            // (the join's store: s20, which is 0)
            out[64 + t as usize] = 0;
        }
    }
    out
}

fn run(shader: &HostShader, host: &Host, below: u32) -> Option<Vec<u32>> {
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("the hardware device does not take this program (waves of 32?): {e}; not run");
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = below;
        }
    }
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(ud))],
                views: &[View::RawUav(128)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("dispatch failed: {e}"));
    Some(out[0][..128].to_vec())
}

/// On the card: the region's results are the console's however many lanes take the branch (all, some
/// of both halves, only some of the lower, none: the wave that has no lane skips the loop). Each case
/// a few times over: the order the halves arrive in varies.
#[test]
fn the_kept_skip_over_a_loop_gives_the_consoles_results_on_the_card() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    for over in [Over::Skip, Over::EarlyOut] {
      for wide in [false, true] {
        let native = lane_branch(over, true);
        let (on, form) = lowered_with(&native, true, wide);
        let Ok(on) = on else {
            eprintln!("not translated for waves of 32 ({over:?}): the program of the test is refused");
            return;
        };
        assert!(form.exact(), "{over:?}: {form:?}");
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        for below in [64u32, 48, 33, 32, 31, 20, 2, 0].into_iter().cycle().take(8 * 10) {
            let Some(got) = run(&on, &host, below) else {
                return;
            };
            assert_eq!(got, expected(below, over), "{over:?}, {below} lanes take the branch");
        }
        eprintln!("ran on the hardware device ({over:?}, {form:?}): the loop's results in every case");
      }
    }
}

/// A loop of `s1` rounds that tests its exit at the top (a scalar test: both host waves leave in the
/// same round) and reads the first lane of v0 + 1 in its body, which the two host waves of the console wave
/// exchange (each asks the other whether it has a lane). The exit comes before the exchange in the order the blocks are lowered. After the
/// loop, `out[0] = s22` (what the last round read: 1).
const TOP_EXIT: [u32; 12] = [
    0x4A04_0081, // v_add_nc_u32 v2, 1, v0
    0xD760_0017, // v_readlane_b32 s23, v2, 63 (a read across the halves: they exchange)
    0x0001_7F02,
    0xBE94_0380, // s_mov_b32 s20, 0
    0xBE96_0380, // s_mov_b32 s22, 0
    0xBF09_0114, // L: s_cmp_ge_u32 s20, s1
    0xBF85_0003, // s_cbranch_scc1 DONE
    0x7E2C_0502, // v_readfirstlane_b32 s22, v2
    0x8114_8114, // s_add_i32 s20, s20, 1
    0xBF82_FFFB, // s_branch L
    0x7E02_0216, // DONE: v_mov_b32 v1, s22
    0x38 << 26 | 0x1C << 18,
];

fn top_exit_lowered(half_exit: bool) -> Result<HostShader, ShaderError> {
    let mut words = TOP_EXIT.to_vec();
    words.push(0x80 << 24 | 2 << 16 | 1 << 8); // buffer_store_dword v1, off, s[8:11], 0
    let native = native_of(&words, true, 64);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, &native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    lower::set_program_tag(b"a top exit");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    lower::TEST_HALF_EXIT.set(Some(half_exit));
    lower::exit_clear::CLEARS.set((0, 0));
    let (shader, form) = super::emit_as(&c, &native, &p, &st, &io, &res);
    eprintln!("top exit, half exit {half_exit}: form {form:?}, (clearings, exits passed first) {:?}", lower::exit_clear::CLEARS.get());
    lower::TEST_HALF_EXIT.set(None);
    lower::TEST_HOST_WAVE.set(was);
    shader
}

/// The half that leaves a loop clears its exchange dword on the way out wherever the exit stands:
/// at the top of the loop, before the exchange of its body (it was only where the exit came after
/// an exchange of the loop in the order the blocks are lowered). The clearing is in the translation
/// with `VICTORY_HALF_EXIT` and not without it.
#[test]
fn the_half_that_leaves_a_loop_at_its_top_clears_its_exchange_dword() {
    let on = top_exit_lowered(true).unwrap_or_else(|e| panic!("lowered with the clearing: {e}"));
    let off = top_exit_lowered(false).unwrap_or_else(|e| panic!("lowered without it: {e}"));
    validated(&on);
    validated(&off);
    assert_eq!(off.dxil, top_exit_lowered(false).expect("lowered").dxil);
    assert_ne!(on.dxil, off.dxil, "the exit at the top of the loop clears the dword");
    assert!(on.dxil.len() > off.dxil.len());
}

/// ... and the loop still runs on the card as it did: three rounds, each reading the first lane.
#[test]
fn a_loop_with_its_exit_at_the_top_still_exchanges_on_the_card() {
    let _one = super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
    let on = top_exit_lowered(true).expect("lowered");
    for (rounds, expected) in [(3u32, 1u32), (1, 1)] {
        let Some(got) = super::wave64_tests::on_the_card(&on, [0, rounds]) else {
            return;
        };
        assert_eq!(got, expected, "{rounds} rounds");
    }
    eprintln!("ran on the hardware device: the loop with its exit at the top");
}
