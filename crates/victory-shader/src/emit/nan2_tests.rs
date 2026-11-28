//! What the hair simulation and the skinning compute programs of the title do with floats, run
//! in the order they do it, against the console's rules, on the card.
//!
//! `nan_tests` runs one op at a time over the special values and finds the host exact there
//! (but for the 16-bit conversion). These are chains: the normalisation with its guard and its
//! zero test by `v_cmp_class`, and the one without a guard, since a driver can fold what one op
//! at a time shows it nothing of. And the two places where the translation does not do what the
//! console does by its float mode (every program of the library has `0xC0`: 32-bit denormals
//! flushed, 16-bit results rounded to nearest even):
//!
//! * `VICTORY_F32_FTZ=1` ([`super::denorm`]): the entry point asks the driver for the flush;
//! * `VICTORY_F16_NEAREST=2` ([`super::f16_level`]): the mixed-precision and packed 16-bit
//!   results round to nearest too.
//!
//! The GPU tests answer for themselves: a few cases each, and a deadline per test (a card that
//! does not answer ends the test binary with a `NAN2_TIMEOUT` line instead of hanging for ever).
//! The pilot runs them on the RTX (`nan-bench.ps1 -filter nan2_tests`); on a runner without a
//! card they print `SKIPPED` and pass.

use super::nan_tests::{Want, f, f16_rne, judge, legacy, max_f};
use super::valu_tests::{build_valu_cs, run_cases_on, sgpr, vgpr, vop1, vop3};
use super::{HostShader, denorm, f16_level};
use std::time::Duration;

/// `s_mov_b32 sdst, lit` (a 32-bit literal follows).
const fn s_mov_lit(sdst: u8, lit: u32) -> [u32; 2] {
    [0xBE80_0000 | (sdst as u32) << 16 | 0x03 << 8 | 0xFF, lit]
}

/// VOP3P `op vdst, src0, src1, src2` (no modifiers; `opsel_hi` is the three bits that say which
/// sources are the high halves of a packed pair, or for the mixed-precision ops 16-bit ones).
const fn vop3p(op: u16, vdst: u8, src0: u16, src1: u16, src2: u16, opsel_hi: u8) -> [u32; 2] {
    [
        0x33 << 26 | (op as u32 & 0x7F) << 16 | ((opsel_hi as u32 >> 2) & 1) << 14 | vdst as u32,
        (opsel_hi as u32 & 3) << 27 | src0 as u32 | (src1 as u32) << 9 | (src2 as u32) << 18,
    ]
}

/// The console's flush of a 32-bit denormal to a zero of its sign.
fn ftz(x: f32) -> f32 {
    if x.is_subnormal() {
        0.0f32.copysign(x)
    } else {
        x
    }
}

/// `v_rsq_f32`: an infinity for 0, 0 for an infinity, a NaN below 0.
fn rsq(d: f32) -> f32 {
    if d.is_nan() || d < 0.0 {
        f32::NAN
    } else if d == 0.0 {
        f32::INFINITY
    } else if d.is_infinite() {
        0.0
    } else {
        (1.0 / f64::from(d).sqrt()) as f32
    }
}

const EPS: f32 = 1.0 / 8_388_608.0;

/// `x * (1 / max(eps, |(x, y)|))` where both are finite (`v_cmp_class` with the mask of the
/// finite classes), 0 where not; the hair solver's guarded normalisation, as the console runs it.
fn guarded(x: f32, y: f32) -> f32 {
    if !(x.is_finite() && y.is_finite()) {
        return 0.0;
    }
    let (x, y) = (ftz(x), ftz(y));
    let t = ftz(legacy(x, x));
    let t = ftz(y.mul_add(y, t));
    let len = ftz(t.sqrt());
    let r = ftz(1.0 / max_f(EPS, len));
    ftz(legacy(x, r))
}

/// `x * rsq(x*x + y*y + z*z)`, which the skinning code writes without a guard: the legacy
/// product keeps a zero vector at 0.
fn normalised(x: f32, y: f32, z: f32) -> f32 {
    let (x, y, z) = (ftz(x), ftz(y), ftz(z));
    let d = ftz(legacy(x, x));
    let d = ftz(y.mul_add(y, d));
    let d = ftz(z.mul_add(z, d));
    ftz(legacy(x, ftz(rsq(d))))
}

fn guarded_body() -> Vec<u32> {
    let (v1, v2, v3, v5, v6, v7) = (vgpr(1), vgpr(2), vgpr(3), vgpr(5), vgpr(6), vgpr(7));
    let mut body = Vec::new();
    body.extend(vop3(0x107, 5, v1, v1, 0)); // v_mul_legacy_f32 v5, v1, v1
    body.extend(vop3(0x14B, 5, v2, v2, v5)); // v_fma_f32 v5, v2, v2, v5
    body.push(vop1(0x033, 6, v5)); // v_sqrt_f32 v6, v5
    body.extend(vop3(0x110, 6, v3, v6, 0)); // v_max_f32 v6, v3 (eps), v6
    body.push(vop1(0x02A, 6, v6)); // v_rcp_f32 v6, v6
    body.extend(vop3(0x107, 7, v1, v6, 0)); // v_mul_legacy_f32 v7, v1, v6
    body.extend(s_mov_lit(22, 0x1F8)); // the classes of the finite numbers
    body.extend(vop3(0x088, 20, v1, sgpr(22), 0)); // v_cmp_class_f32 s20, v1, s22
    body.extend(vop3(0x088, 21, v2, sgpr(22), 0)); // v_cmp_class_f32 s21, v2, s22
    body.extend(vop3(0x101, 7, 0x80, v7, sgpr(20))); // v_cndmask_b32 v7, 0, v7, s20
    body.extend(vop3(0x101, 0, 0x80, v7, sgpr(21))); // v_cndmask_b32 v0, 0, v7, s21
    body
}

fn normalised_body() -> Vec<u32> {
    let (v1, v2, v3, v5, v6) = (vgpr(1), vgpr(2), vgpr(3), vgpr(5), vgpr(6));
    let mut body = Vec::new();
    body.extend(vop3(0x107, 5, v1, v1, 0)); // v_mul_legacy_f32 v5, v1, v1
    body.extend(vop3(0x14B, 5, v2, v2, v5)); // v_fma_f32 v5, v2, v2, v5
    body.extend(vop3(0x14B, 5, v3, v3, v5)); // v_fma_f32 v5, v3, v3, v5
    body.push(vop1(0x02E, 6, v5)); // v_rsq_f32 v6, v5
    body.extend(vop3(0x107, 0, v1, v6, 0)); // v_mul_legacy_f32 v0, v1, v6
    body
}

/// `v_fma_mixlo_f16 v0, v1, v2, 0` with three 32-bit sources.
fn mixlo_body() -> Vec<u32> {
    vop3p(0x21, 0, vgpr(1), vgpr(2), 0x80, 0).to_vec()
}

fn bits(x: f32) -> u32 {
    x.to_bits()
}

fn guarded_cases() -> Vec<([u32; 4], bool)> {
    let eps = bits(EPS);
    [
        (3.0, 4.0),
        (0.0, 0.0),
        (-0.0, 0.0),
        (1.0, 0.0),
        (-2.5, 0.3),
        (1e-30, 1e-30),
        (3e38, 1.0),
        (0.0, 3e38),
        (f32::INFINITY, 1.0),
        (1.0, f32::NEG_INFINITY),
        (f32::NAN, 1.0),
        (1.0, f32::NAN),
        (65504.0, 65504.0),
        (1e-3, 2e-3),
    ]
    .into_iter()
    .map(|(x, y)| ([bits(x), bits(y), eps, 0], false))
    // (a square in the denormal range: the guard hides it, so the two modes agree)
    .chain([([bits(1e-20), 0, eps, 0], false)])
    .collect()
}

/// The cases of the unguarded normalisation; the flag marks the ones whose answer depends on
/// the denormal mode (a sum of squares between 1.4e-45 and 1.2e-38).
fn normalised_cases() -> Vec<([u32; 4], bool)> {
    [
        (3.0, 4.0, 0.0, false),
        (0.0, 0.0, 0.0, false),
        (-0.0, 0.0, 0.0, false),
        (2.0, 0.0, 0.0, false),
        (-3.0, -4.0, -12.0, false),
        (1e-25, 0.0, 0.0, false),
        (3e19, 0.0, 0.0, false),
        (f32::NAN, 1.0, 1.0, false),
        (1e-20, 0.0, 0.0, true),
        (1e-22, 1e-22, 1e-22, true),
    ]
    .into_iter()
    .map(|(x, y, z, denormal)| ([bits(x), bits(y), bits(z), 0], denormal))
    .collect()
}

fn mixlo_cases() -> Vec<([u32; 4], bool)> {
    // a product that does not fit 11 bits in the last place: nearest and toward zero differ
    [
        (0.3, 1.0),
        (-0.3, 1.0),
        (1.0 / 3.0, 1.0),
        (0.1, 1.0),
        (1000.7, 1.0),
        (65519.0, 1.0),
        (65520.0, 1.0),
        (4e-8, 1.0),
        (2e-8, 1.0),
        (1.0, 1.0),
        (f32::NAN, 1.0),
    ]
    .into_iter()
    .map(|(a, b)| ([bits(a), bits(b), 0, 0], false))
    .collect()
}

/// Runs `body` over `cases` on `devices` with a float mode and a 16-bit rounding level, on a
/// thread that gets `secs` seconds. Returns the mismatches (flagged cases are not looked at on
/// WARP, which does not take the driver's float mode), and how many cases were answered; the
/// process ends with `NAN2_TIMEOUT` if the card does not answer.
#[allow(clippy::too_many_arguments)]
fn answer(
    name: &'static str,
    secs: u64,
    body: Vec<u32>,
    cases: Vec<([u32; 4], bool)>,
    devices: &'static [(&'static str, bool)],
    ftz_on: bool,
    level: u8,
    want: fn([u32; 4]) -> Want,
) -> (Vec<String>, usize) {
    let (tx, rx) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let was = (
            denorm::TEST_F32_FTZ.replace(Some(ftz_on)),
            f16_level::TEST_F16_LEVEL.replace(Some(level)),
        );
        let shader: HostShader = build_valu_cs(&body);
        denorm::TEST_F32_FTZ.set(was.0);
        f16_level::TEST_F16_LEVEL.set(was.1);
        let inputs: Vec<[u32; 4]> = cases.iter().map(|c| c.0).collect();
        let (mut bad, mut seen) = (Vec::new(), 0usize);
        run_cases_on(&shader, &inputs, devices, |label, i, inp, got| {
            if label == "warp" && cases[i].1 {
                return;
            }
            seen += 1;
            let w = want(inp);
            if !judge(got, w) {
                bad.push(format!(
                    "{name} [{label}] of {:?} gave {:#010x} ({}), the console gives {w:?}",
                    [f(inp[0]), f(inp[1]), f(inp[2])],
                    got,
                    f(got)
                ));
            }
        });
        let _ = tx.send((bad, seen));
    });
    match rx.recv_timeout(Duration::from_secs(secs)) {
        Ok(r) => r,
        Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
            eprintln!(
                "NAN2_TIMEOUT {name}: no answer from the card in {secs} s; ending the test binary"
            );
            std::process::exit(3);
        }
        Err(e) => panic!("{name}: the test thread ended without an answer ({e})"),
    }
}

const BOTH: &[(&str, bool)] = &[("hardware", false), ("warp", true)];
const CARD: &[(&str, bool)] = &[("hardware", false)];

fn guarded_want(i: [u32; 4]) -> Want {
    Want::Near(guarded(f(i[0]), f(i[1])), 6)
}

fn normalised_want(i: [u32; 4]) -> Want {
    Want::Near(normalised(f(i[0]), f(i[1]), f(i[2])), 6)
}

fn mixlo_want(i: [u32; 4]) -> Want {
    Want::LowHalf(f16_rne(f(i[0]) * f(i[1])))
}

/// The guarded normalisation, its zero test by class mask and its legacy products, in one
/// program: what the hair solver does to every segment, with a driver that may fold.
#[test]
fn the_guarded_normalisation_chain_is_the_consoles_on_the_card() {
    let (bad, seen) = answer(
        "guarded",
        150,
        guarded_body(),
        guarded_cases(),
        BOTH,
        false,
        0,
        guarded_want,
    );
    eprintln!("NAN2 guarded chain: {} of {seen} answers differ", bad.len());
    assert!(bad.is_empty(), "{}", bad.join("\n"));
}

/// The normalisation without a guard, as the skinning code writes it. With the flush the card
/// must give what the console gives (an infinity for a vector whose squares are denormal, the
/// 0 of a zero vector); without it the answers of the denormal cases are the card's own, listed.
#[test]
fn the_flush_of_denormals_makes_the_unguarded_normalisation_the_consoles() {
    let (off, seen) = answer(
        "normalised ftz=off",
        150,
        normalised_body(),
        normalised_cases(),
        CARD,
        false,
        0,
        normalised_want,
    );
    for line in &off {
        eprintln!("NAN2 {line}");
    }
    eprintln!(
        "NAN2 unguarded normalisation, no flush: {} of {seen} answers differ",
        off.len()
    );
    let (on, seen_on) = answer(
        "normalised ftz=on",
        150,
        normalised_body(),
        normalised_cases(),
        BOTH,
        true,
        0,
        normalised_want,
    );
    eprintln!(
        "NAN2 unguarded normalisation, VICTORY_F32_FTZ=1: {} of {seen_on} answers differ",
        on.len()
    );
    assert!(
        on.is_empty(),
        "the driver does not flush 32-bit denormals for \"fp32-denorm-mode\"=\"ftz\", or the \
         chain differs elsewhere:\n{}",
        on.join("\n")
    );
}

/// `VICTORY_F16_NEAREST=2`: the result of `v_fma_mixlo_f16` is the nearest half, on the card and
/// on WARP; level 1 is not asserted (it is the host's conversion there).
#[test]
fn the_mixed_precision_result_rounds_to_nearest_with_level_2_on_the_card() {
    let (bad, seen) = answer(
        "mixlo",
        150,
        mixlo_body(),
        mixlo_cases(),
        BOTH,
        false,
        2,
        mixlo_want,
    );
    eprintln!(
        "NAN2 v_fma_mixlo_f16 level 2: {} of {seen} answers differ",
        bad.len()
    );
    assert!(bad.is_empty(), "{}", bad.join("\n"));
    let (plain, seen) = answer(
        "mixlo level 0",
        150,
        mixlo_body(),
        mixlo_cases(),
        CARD,
        false,
        0,
        mixlo_want,
    );
    eprintln!(
        "NAN2 v_fma_mixlo_f16 level 0 (the host's conversion): {} of {seen} answers differ",
        plain.len()
    );
}

fn dxil_of(body: &[u32], ftz_on: bool, level: u8) -> Vec<u8> {
    let was = (
        denorm::TEST_F32_FTZ.replace(Some(ftz_on)),
        f16_level::TEST_F16_LEVEL.replace(Some(level)),
    );
    let shader = build_valu_cs(body);
    denorm::TEST_F32_FTZ.set(was.0);
    f16_level::TEST_F16_LEVEL.set(was.1);
    shader.dxil
}

/// Level 1 is the scalar 16-bit operations as before; level 2 adds the mixed-precision and the
/// packed results, and nothing else.
#[test]
fn level_2_reaches_the_mixed_and_packed_results_that_level_1_does_not() {
    let mix = mixlo_body();
    let packed = vec![0xcc0f_4000, 0x1a02_02f8]; // v_pk_add_f16 v0, 0x3118, v1
    let cvt = vec![vop1(0x00A, 0, vgpr(1))]; // v_cvt_f16_f32 v0, v1
    let scalar = vop3(0x103, 5, vgpr(1), vgpr(2), 0).to_vec(); // v_add_f32 (no 16-bit result)
    for (name, body, level_1_reaches) in [
        ("v_fma_mixlo_f16", &mix, false),
        ("v_pk_add_f16", &packed, false),
        ("v_cvt_f16_f32", &cvt, true),
        ("v_add_f32", &scalar, false),
    ] {
        let (d0, d1, d2) = (
            dxil_of(body, false, 0),
            dxil_of(body, false, 1),
            dxil_of(body, false, 2),
        );
        assert_eq!(d0 != d1, level_1_reaches, "{name}: level 1");
        let reaches_2 = name != "v_add_f32";
        assert_eq!(
            d1 != d2,
            reaches_2 && !level_1_reaches,
            "{name}: level 2 over level 1"
        );
        assert_eq!(d0 != d2, reaches_2, "{name}: level 2 over off");
        // the same text twice is the same translation
        assert_eq!(d2, dxil_of(body, false, 2), "{name}: deterministic");
    }
}

/// The flush is a different module (an attribute on the entry), only where asked for, and the
/// validator takes it (`build_valu_cs` runs `dxil.dll` over every program it builds).
#[test]
fn the_flush_changes_the_module_only_where_asked() {
    for body in [guarded_body(), normalised_body(), mixlo_body()] {
        let off = dxil_of(&body, false, 0);
        let on = dxil_of(&body, true, 0);
        assert_ne!(on, off, "the entry carries the denormal mode");
        assert_eq!(off, dxil_of(&body, false, 0), "off is deterministic");
    }
}

/// The models of the console's rules, on values a person can check.
#[test]
fn the_console_models_flush_and_guard_as_the_float_mode_says() {
    assert_eq!(ftz(1e-40).to_bits(), 0);
    assert_eq!(ftz(-1e-40).to_bits(), 0x8000_0000);
    assert_eq!(ftz(1e-20), 1e-20);
    // a vector of squares below the smallest normal number: the root of a flushed 0
    assert_eq!(normalised(1e-20, 0.0, 0.0), f32::INFINITY);
    assert_eq!(normalised(0.0, 0.0, 0.0), 0.0);
    assert!((normalised(3.0, 4.0, 0.0) - 0.6).abs() < 1e-6);
    assert_eq!(normalised(3e19, 0.0, 0.0), 0.0);
    // the guard keeps the same vector finite
    assert!((guarded(1e-20, 0.0) - 1e-20 * 8_388_608.0).abs() < 1e-12);
    assert_eq!(guarded(f32::NAN, 1.0), 0.0);
    assert_eq!(guarded(1.0, f32::INFINITY), 0.0);
    assert!((guarded(3.0, 4.0) - 0.6).abs() < 1e-6);
    assert_eq!(f16_rne(0.3 * 1.0), 0x34CD);
}
