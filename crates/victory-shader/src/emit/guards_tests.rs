//! `VICTORY_SHADER_OPT` bit 128 (`bitcode::opt::GUARDS`): the selects that keep a register's old
//! value in the lanes EXEC leaves out are dropped where nothing reads the old value. Synthetic
//! console programs (a chain of VALU writes under a partial EXEC, and the join after it) are
//! translated with and without the pass, validated by `dxil.dll` when present, and run on the
//! hardware adapter and WARP: both must store what the console would.
//!
//! A skipped run is loud: the line starts with `GPU-TEST-SKIPPED`, and with
//! `VICTORY_REQUIRE_GPU=1` (the pilot's run of the CI test binary on the RTX) a skip is a failure.

use super::bitcode::opt;
use super::host::{Dispatch, Host, Root, View};
use super::wave64_tests::native_of;
use super::{HostShader, lower, validator};
use crate::dxbc::Container;
use crate::{io, ir, resource, structure};

/// What a test that cannot run says, and (with `VICTORY_REQUIRE_GPU=1`) is.
fn skipped(what: &str) {
    eprintln!("GPU-TEST-SKIPPED: {what}");
    assert!(
        std::env::var_os("VICTORY_REQUIRE_GPU").is_none_or(|v| v != "1"),
        "VICTORY_REQUIRE_GPU=1 and the test could not run: {what}"
    );
}

/// The lanes with `tid < LIMIT` run the chain.
const fn limit(wave64: bool) -> u32 {
    if wave64 { 40 } else { 20 }
}

/// A console program of `threads` threads (v0 = the thread's number): v1..v3 get values for every
/// lane; then the lanes below `limit` (EXEC cut by `s_and_saveexec`) write v1 = tid + 5,
/// v2 = 21 ^ v1, v3 = v1 + v2, each read only by the next write under the same EXEC; EXEC is
/// restored and every lane stores v3 (the lanes that were cut hold 7).
fn chain(wave64: bool) -> Vec<u32> {
    let k = 0x80 + limit(wave64);
    let mut w = vec![
        0x4A02_0081,     // v_add_nc_u32 v1, 1, v0
        0x7E04_0280,     // v_mov_b32 v2, 0
        0x7E06_0287,     // v_mov_b32 v3, 7
        0x3408_0082,     // v_lshlrev_b32 v4, 2, v0
        0x7D88_0000 | k, // v_cmp_gt_u32 vcc, limit, v0
    ];
    // s_and_saveexec s12, vcc
    w.push(if wave64 { 0xBE8C_246A } else { 0xBE8C_3C6A });
    w.extend([
        0x4A02_0085, // v_add_nc_u32 v1, 5, v0
        0x3A04_0295, // v_xor_b32 v2, 21, v1
        0x4A06_0501, // v_add_nc_u32 v3, v1, v2
    ]);
    // s_mov exec, s12
    w.push(if wave64 { 0xBEFE_040C } else { 0xBEFE_030C });
    w.extend([
        0xE070_1000, // buffer_store_dword v3, v4, s[8:11], 0 offen
        0x8002_0304,
    ]);
    w
}

/// What lane `tid` stores.
fn expected(tid: u32, wave64: bool) -> u32 {
    if tid < limit(wave64) {
        let v1 = tid + 5;
        (21 ^ v1) + v1
    } else {
        7
    }
}

/// `chain` translated for a host of 32-lane waves with the module passes `passes`.
fn translated(wave64: bool, passes: u32) -> HostShader {
    let threads = if wave64 { 64 } else { 32 };
    let native = native_of(&chain(wave64), wave64, threads);
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("track");
    let io = io::raise(&p, &native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    let before = super::TEST_SHADER_OPT.replace(Some(passes));
    let made = super::emit_program(&c, &native, &p, &st, &io, &res);
    super::TEST_SHADER_OPT.set(before);
    lower::TEST_HOST_WAVE.set(was);
    let (shader, wave) = made.expect("the chain translates for waves of 32");
    assert_ne!(wave, Some(64), "it does not need a wave of 64");
    if let Ok(mut v) = validator::Validator::load(validator::Validator::default_path().as_deref()) {
        let mut copy = shader.dxil.clone();
        v.validate(&mut copy).expect("DXIL validation failed");
    }
    shader
}

/// What each lane stored, on the hardware adapter (`warp` false) or WARP; None if there is none.
fn lanes(shader: &HostShader, wave64: bool, warp: bool) -> Option<Vec<u32>> {
    let label = if warp { "WARP" } else { "hardware" };
    let _one = super::valu_tests::ONE_AT_A_TIME
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let host = match Host::new(warp) {
        Ok(h) => h,
        Err(e) => {
            skipped(&format!("no SM 6.6 {label} device ({e})"));
            return None;
        }
    };
    let (root, pso) = match host.compute_pipeline(shader) {
        Ok(x) => x,
        Err(e) => {
            skipped(&format!(
                "the {label} device does not take the program ({e})"
            ));
            return None;
        }
    };
    let threads = if wave64 { 64 } else { 32 };
    let user = vec![0u32; shader.user_data_dwords().max(1) as usize];
    let out = host
        .dispatch(
            &root,
            &pso,
            &Dispatch {
                roots: vec![(0, Root::Constants(user))],
                views: &[View::RawUav(threads)],
                samplers: &[],
                groups: [1, 1, 1],
            },
        )
        .unwrap_or_else(|e| panic!("{label}: dispatch failed: {e}"));
    Some(out[0].to_vec())
}

fn guards_change_nothing_the_lanes_store(wave64: bool) {
    let without = translated(wave64, opt::DEFAULT);
    let with = translated(wave64, opt::DEFAULT | opt::GUARDS);
    assert!(
        with.dxil.len() < without.dxil.len(),
        "the guards of v1 and v2 are gone: {} bytes against {}",
        with.dxil.len(),
        without.dxil.len()
    );
    let threads = if wave64 { 64 } else { 32 };
    let want: Vec<u32> = (0..threads).map(|t| expected(t, wave64)).collect();
    for warp in [false, true] {
        for (name, shader) in [("without", &without), ("with", &with)] {
            if let Some(got) = lanes(shader, wave64, warp) {
                assert_eq!(got, want, "{name} the guards pass, warp {warp}");
            }
        }
    }
}

#[test]
fn guards_of_a_wave32_chain_go_and_the_lanes_store_the_same() {
    guards_change_nothing_the_lanes_store(false);
}

#[test]
fn guards_of_a_wave64_chain_in_two_host_waves_go_and_the_lanes_store_the_same() {
    guards_change_nothing_the_lanes_store(true);
}

/// The pass changes nothing unless asked (the usual passes are what they were).
#[test]
fn the_guards_pass_is_off_unless_asked() {
    assert_eq!(opt::DEFAULT & opt::GUARDS, 0);
    let a = translated(false, opt::DEFAULT);
    let b = translated(false, opt::DEFAULT);
    assert_eq!(a.dxil, b.dxil, "translation is deterministic");
}
