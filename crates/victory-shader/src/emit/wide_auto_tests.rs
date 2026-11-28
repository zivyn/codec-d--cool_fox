//! `VICTORY_WAVE_WIDE_AUTO`: the form a console wave64 compute program takes where a count of the
//! wave's lanes sits behind an inner skip that ends together with an outer one (the shape of the
//! cascade-shadow rasters' tile programs), for a host of 32-lane waves.
//!
//! The program is 64 threads (one console wave): `if (t < s0) { if (t < s1) { n = count of the
//! lanes with t == s2; out[t] = n ? 2 : 1 } }`, the inner `s_cbranch_execz` ending at the outer
//! one's join (`own_join` false), or at a restore of EXEC of its own. The count crosses the two
//! host waves, so each lane's answer needs the other half's lanes. `looped`: all of it in a
//! uniform loop.

use super::host::{Dispatch, Host, Root, View};
use super::wave64_tests::native_of;
use super::{Form, HostShader, lower, validator};
use crate::dxbc::Container;
use crate::native::{NativeProgram, RootType};
use crate::{ShaderError, io, ir, resource, structure};

fn count_behind_two_skips(own_join: bool, looped: bool) -> NativeProgram {
    let gate = [
        0x7D84_0002, // v_cmp_eq_u32 vcc, s2, v0
        0xBE90_106A, // s_bcnt1_i32_b64 s16, vcc
        0xBF06_8010, // s_cmp_eq_u32 s16, 0
        0x8511_8281, // s_cselect_b32 s17, 1, 2
        0x7E02_0211, // v_mov_b32 v1, s17
        0x3404_0082, // v_lshlrev_b32 v2, 2, v0
        0x38 << 26 | 0x1C << 18 | 1 << 12,
        0x80 << 24 | 2 << 16 | 1 << 8 | 2, // buffer_store_dword v1, v2, s[8:11], 0 offen
    ];
    let inner = gate.len() as u32;
    let outer = 3 + inner + u32::from(own_join);
    let mut words = vec![
        0x7D88_0000,         // v_cmp_gt_u32 vcc, s0, v0
        0xBE8C_246A,         // s_and_saveexec_b64 s[12:13], vcc
        0xBF88_0000 | outer, // s_cbranch_execz to the join
        0x7D88_0001,         // v_cmp_gt_u32 vcc, s1, v0
        0xBE8E_246A,         // s_and_saveexec_b64 s[14:15], vcc
        0xBF88_0000 | inner, // s_cbranch_execz to the join, or to its own
    ];
    words.extend_from_slice(&gate);
    if own_join {
        words.push(0xBEFE_040E); // s_mov_b64 exec, s[14:15]
    }
    words.push(0xBEFE_040C); // s_mov_b64 exec, s[12:13]
    if looped {
        words.push(0xBF06_8003); // s_cmp_eq_u32 s3, 0
        let back = -(words.len() as i32 + 1);
        words.push(0xBF85_0000 | (back as u32 & 0xFFFF)); // s_cbranch_scc1 to the start
    }
    native_of(&words, true, 64)
}

fn branches_named(p: &ir::Program, name: &str) -> usize {
    p.blocks
        .iter()
        .filter(|b| b.insts.last().is_some_and(|i| i.name() == name))
        .count()
}

/// The program lowered for a host of 32-lane waves with `VICTORY_WAVE_WIDE_AUTO` on or off (and
/// `VICTORY_KEEP_LOOP_SKIPS` off: a skip over a loop is another form's business), validated by
/// `dxil.dll` where there is one; the form it ended in.
fn lowered(native: &NativeProgram, auto: bool) -> (Result<HostShader, ShaderError>, Form) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res =
        resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    lower::set_program_tag(b"a wide auto test");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    ir::TEST_KEEP_LOOP_SKIPS.set(Some(false));
    super::TEST_WAVE_WIDE_AUTO.set(Some(auto));
    let result = super::emit_as(&c, native, &p, &st, &io, &res);
    super::TEST_WAVE_WIDE_AUTO.set(None);
    ir::TEST_KEEP_LOOP_SKIPS.set(None);
    lower::TEST_HOST_WAVE.set(was);
    if let (Ok(shader), Ok(mut v)) = (
        &result.0,
        validator::Validator::load(validator::Validator::default_path().as_deref()),
    ) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    result
}

fn form_of(native: &NativeProgram, auto: bool) -> Form {
    let (r, form) = lowered(native, auto);
    r.unwrap_or_else(|e| panic!("not translated (auto {auto}): {e}"));
    form
}

/// Only the deep form of `ir::drop_skips_as` leaves the inner skip out, and the count lies behind
/// it: with the switch the program takes the wave-wide form (the wave takes the skip together);
/// without it, "without skips, scalars followed". An inner skip with a join of its own is left
/// out by the shallow form already ("without skips", whatever the switch says), and where the
/// wave-wide form would put a group barrier of the translator's in a loop, the deep form stays.
#[test]
fn a_count_behind_a_skip_that_ends_with_an_outer_one_takes_the_wave_wide_form() {
    let native = count_behind_two_skips(false, false);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    assert_eq!(branches_named(&p, "s_cbranch_execz"), 2);
    let shallow = ir::drop_skips_as(&p, false, false)
        .0
        .expect("the outer skip is left out");
    assert_eq!(
        branches_named(&shallow, "s_cbranch_execz"),
        1,
        "the inner skip stays without `deep`"
    );
    let deep = ir::drop_skips_as(&p, true, false).0.expect("left out");
    assert_eq!(
        branches_named(&deep, "s_cbranch_execz"),
        0,
        "`deep` leaves the inner skip out too"
    );
    let count = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .find(|i| i.name() == "s_bcnt1_i32_b64")
        .expect("the count");
    let first = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .next()
        .expect("an instruction");
    assert!(
        ir::deep_skip_holds(&p, count.machine.offset),
        "the count lies behind the deep-only skip"
    );
    assert!(
        !ir::deep_skip_holds(&p, first.machine.offset),
        "the program's start does not"
    );
    assert_eq!(form_of(&native, false), Form::WithoutSkipsDeep);
    assert_eq!(form_of(&native, true), Form::WaveWideAuto);

    let own = count_behind_two_skips(true, false);
    let p = ir::build(&own.code, own.wave64, own.vgprs).expect("ir::build");
    let shallow = ir::drop_skips_as(&p, false, false).0.expect("left out");
    assert_eq!(
        branches_named(&shallow, "s_cbranch_execz"),
        0,
        "an inner skip with a join of its own goes"
    );
    assert_eq!(form_of(&own, false), Form::WithoutSkips);
    assert_eq!(form_of(&own, true), Form::WithoutSkips);

    // In a loop the pattern holds, but the wave-wide form would put the count's meetings into the
    // loop: "without skips, scalars followed" stays.
    let looped = count_behind_two_skips(false, true);
    let p = ir::build(&looped.code, looped.wave64, looped.vgprs).expect("ir::build");
    let count = p
        .blocks
        .iter()
        .flat_map(|b| &b.insts)
        .find(|i| i.name() == "s_bcnt1_i32_b64")
        .expect("the count");
    assert!(ir::deep_skip_holds(&p, count.machine.offset));
    assert_eq!(form_of(&looped, true), Form::WithoutSkipsDeep);
    assert_eq!(form_of(&looped, false), Form::WithoutSkipsDeep);
}

/// What the console leaves in `out` for lanes `t < min(s0, s1)`: 2 where lane `s2` is one of them
/// (the count is 1), else 1.
fn expected(args: [u32; 3]) -> Vec<u32> {
    let live = args[0].min(args[1]);
    let mut out = vec![0u32; 64];
    for t in 0..live {
        out[t as usize] = if args[2] < live { 2 } else { 1 };
    }
    out
}

fn run(shader: &HostShader, host: &Host, args: [u32; 3]) -> Option<Vec<u32>> {
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            eprintln!(
                "the hardware device does not take this program (waves of 32?): {e}; not run"
            );
            return None;
        }
    };
    let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
    for u in &shader.user_data {
        if let (RootType::Constants, 0, d @ 0..=2) = (u.root, u.slot, u.dword) {
            ud[u.index as usize] = args[d as usize];
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
    Some(out[0][..64].to_vec())
}

/// On the card: the wave-wide form gives the console's count to every lane, whichever half holds
/// the counted lane and however many lanes take the skips (all of both halves, only some of the
/// upper, only the lower, the counted lane outside). Each case a few times over (the order the
/// halves arrive in varies). The form "without skips, scalars followed" is run on the same cases
/// and what it gets wrong is printed, not asserted: how it fails depends on the card.
#[test]
fn the_wave_wide_form_gives_the_consoles_count_on_the_card() {
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let native = count_behind_two_skips(false, false);
    let (wide, wide_form) = lowered(&native, true);
    let (old, old_form) = lowered(&native, false);
    let (Ok(wide), Ok(old)) = (wide, old) else {
        eprintln!("not translated for waves of 32: the program of the test is refused");
        return;
    };
    assert_eq!(
        (wide_form, old_form),
        (Form::WaveWideAuto, Form::WithoutSkipsDeep)
    );
    let Ok(host) = Host::new(false) else {
        eprintln!("no SM 6.6 hardware device; not run");
        return;
    };
    let cases = [
        [64u32, 64, 40],
        [64, 64, 5],
        [64, 64, 63],
        [64, 50, 40],
        [64, 50, 55],
        [40, 64, 35],
        [40, 64, 45],
        [20, 64, 40],
        [20, 64, 10],
        [33, 33, 32],
        [32, 64, 31],
    ];
    let reps = 20;
    let (mut wrong_lanes, mut wrong_runs, mut runs) = (0usize, 0usize, 0usize);
    for args in cases.into_iter().cycle().take(cases.len() * reps) {
        let Some(got) = run(&wide, &host, args) else {
            return;
        };
        assert_eq!(
            got,
            expected(args),
            "wave-wide form, s0={} s1={} s2={}",
            args[0],
            args[1],
            args[2]
        );
        let Some(got) = run(&old, &host, args) else {
            return;
        };
        runs += 1;
        let bad = got
            .iter()
            .zip(expected(args))
            .filter(|(g, e)| **g != *e)
            .count();
        if bad > 0 {
            wrong_runs += 1;
            wrong_lanes += bad;
            if wrong_runs <= 3 {
                eprintln!(
                    "scalars followed, s0={} s1={} s2={}: {bad} lanes differ from the console",
                    args[0], args[1], args[2]
                );
            }
        }
    }
    eprintln!(
        "ran on the hardware device: the wave-wide form gave the console's count in all {runs} runs; \
         the form without skips, scalars followed, differed in {wrong_runs} runs ({wrong_lanes} lanes)"
    );
}
