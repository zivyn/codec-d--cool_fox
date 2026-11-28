//! The edges of the console's float arithmetic (NaN, infinities, zeros of both signs, the
//! clamp bit) as RDNA2 gives them, against what the host card gives of the translation.
//! The hot spot in the hair that grows frame after frame is a value that a temporal pass keeps
//! and amplifies; a NaN or an infinity that the console's instructions turn into a number
//! and the host's pass on would make it. `VICTORY_NAN_SAFE` (`super::nan_safe`) spells the
//! NaN out in integer arithmetic where a driver may treat floats as numbers; these tests say
//! which ops need it.
//!
//! * The GPU tests run on the hardware adapter only (a hosted runner has none: they print
//!   `SKIPPED` and pass). Each runs every op of the table over twelve special values, all
//!   pairs, and lists every case where the card's result is not the model's, per op:
//!   `nan_edges_default_on_the_card` for the translation as it is, `nan_edges_nan_safe_on_the_card`
//!   with every `VICTORY_NAN_SAFE` bit. The pilot runs them on the RTX
//!   (`bench.ps1 -exe <victory_shader_tests.exe> -filter nan_tests`).
//! * The CPU tests check the models and that each switch bit changes only the ops it names.

#![allow(clippy::neg_cmp_op_on_partial_ord)]

use super::nan_safe_bits as bits;
use super::valu_tests::{build_valu_cs, run_cases_on, sgpr, vgpr, vop1, vop3, vop3_mod};
use super::{HostShader, lower};

/// What an op should give for a case.
#[derive(Clone, Copy, Debug)]
pub(super) enum Want {
    /// These bits, or any NaN where the model says NaN.
    Bits(u32),
    /// Any NaN.
    Nan,
    /// This number (a zero of either sign for a zero), or any NaN where it is one.
    Num(f32),
    /// Within `ulps` of this number; exactly for an infinity.
    Near(f32, u32),
    /// Within `tol * max(1, |value|)` of this number.
    Close(f32, f32),
    /// Two 16-bit floats, low and high; any NaN where the model says NaN.
    Halves(u16, u16),
    /// A 16-bit float in the low half (the high one is not looked at); any NaN where the
    /// model says NaN.
    LowHalf(u16),
    /// Not asserted (an op that depends on denormals, for one).
    Any,
}

pub(super) fn f(x: u32) -> f32 {
    f32::from_bits(x)
}

/// An order of the bit patterns that follows the numbers (both zeros are 0).
fn ordered(bits: u32) -> i64 {
    if bits >> 31 == 0 {
        i64::from(bits)
    } else {
        -i64::from(bits & 0x7FFF_FFFF)
    }
}

fn half_nan(h: u16) -> bool {
    h & 0x7C00 == 0x7C00 && h & 0x3FF != 0
}

pub(super) fn judge(got: u32, want: Want) -> bool {
    let g = f(got);
    match want {
        Want::Bits(b) => got == b || g.is_nan() && f(b).is_nan(),
        Want::Nan => g.is_nan(),
        Want::Num(w) => got == w.to_bits() || g.is_nan() && w.is_nan() || g == 0.0 && w == 0.0,
        Want::Near(w, ulps) => {
            if w.is_nan() {
                g.is_nan()
            } else if w.is_infinite() {
                got == w.to_bits()
            } else {
                !g.is_nan()
                    && (ordered(got) - ordered(w.to_bits())).unsigned_abs() <= u64::from(ulps)
            }
        }
        Want::Close(w, tol) => {
            if w.is_nan() {
                g.is_nan()
            } else if w.is_infinite() {
                got == w.to_bits()
            } else {
                !g.is_nan() && (g - w).abs() <= tol * w.abs().max(1.0)
            }
        }
        Want::Halves(lo, hi) => {
            let same = |got: u16, want: u16| got == want || half_nan(got) && half_nan(want);
            same(got as u16, lo) && same((got >> 16) as u16, hi)
        }
        Want::LowHalf(lo) => got as u16 == lo || half_nan(got as u16) && half_nan(lo),
        Want::Any => true,
    }
}

/// A float as a half, rounded toward zero (`v_cvt_pkrtz_f16_f32`): an overflow gives the
/// largest finite half, an infinity stays one.
fn f16_rtz(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xFF) as i32;
    let m = b & 0x7F_FFFF;
    if e == 255 {
        return sign | 0x7C00 | if m != 0 { 0x200 } else { 0 };
    }
    let e = e - 127 + 15;
    if e >= 31 {
        return sign | 0x7BFF;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = m | 0x80_0000;
        return sign | (m >> (14 - e) as u32) as u16;
    }
    sign | ((e as u32) << 10 | m >> 13) as u16
}

/// A float as a half, rounded to nearest, ties to even (`v_cvt_f16_f32`).
pub(super) fn f16_rne(x: f32) -> u16 {
    let b = x.to_bits();
    let sign = ((b >> 16) & 0x8000) as u16;
    let e = ((b >> 23) & 0xFF) as i32;
    let m = b & 0x7F_FFFF;
    if e == 255 {
        return sign | 0x7C00 | if m != 0 { 0x200 } else { 0 };
    }
    let e = e - 127 + 15;
    if e >= 31 {
        return sign | 0x7C00;
    }
    if e <= 0 {
        if e < -10 {
            return sign;
        }
        let m = m | 0x80_0000;
        let shift = (14 - e) as u32;
        let mut h = m >> shift;
        let rest = m & ((1 << shift) - 1);
        let halfway = 1 << (shift - 1);
        if rest > halfway || rest == halfway && h & 1 == 1 {
            h += 1;
        }
        return sign | h as u16;
    }
    let mut h = (e as u32) << 10 | m >> 13;
    let rest = m & 0x1FFF;
    if rest > 0x1000 || rest == 0x1000 && h & 1 == 1 {
        h += 1;
    }
    sign | h as u16
}

pub(super) fn max_f(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        b
    } else if b.is_nan() {
        a
    } else if a == 0.0 && b == 0.0 {
        // +0 above -0.
        if a.is_sign_negative() && b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        a.max(b)
    }
}

fn min_f(a: f32, b: f32) -> f32 {
    if a.is_nan() {
        b
    } else if b.is_nan() {
        a
    } else if a == 0.0 && b == 0.0 {
        if a.is_sign_negative() || b.is_sign_negative() {
            -0.0
        } else {
            0.0
        }
    } else {
        a.min(b)
    }
}

/// `0 x anything = 0`.
pub(super) fn legacy(a: f32, b: f32) -> f32 {
    if a == 0.0 || b == 0.0 { 0.0 } else { a * b }
}

fn saturate(x: f32) -> f32 {
    if x.is_nan() { 0.0 } else { x.clamp(0.0, 1.0) }
}

/// The compare ops: name, VOPC code, the model.
type Compare = (&'static str, u16, fn(f32, f32) -> bool);

const COMPARES: [Compare; 14] = [
    ("lt", 0x001, |a, b| a < b),
    ("eq", 0x002, |a, b| a == b),
    ("le", 0x003, |a, b| a <= b),
    ("gt", 0x004, |a, b| a > b),
    ("lg", 0x005, |a, b| a < b || a > b),
    ("ge", 0x006, |a, b| a >= b),
    ("o", 0x007, |a, b| !(a.is_nan() || b.is_nan())),
    ("u", 0x008, |a, b| a.is_nan() || b.is_nan()),
    ("nge", 0x009, |a, b| !(a >= b)),
    ("nlg", 0x00A, |a, b| !(a < b || a > b)),
    ("ngt", 0x00B, |a, b| !(a > b)),
    ("nle", 0x00C, |a, b| !(a <= b)),
    ("neq", 0x00D, |a, b| a != b),
    ("nlt", 0x00E, |a, b| !(a < b)),
];

/// One op of the table: its body (a result in `v0`; `v1`, `v2`, `v3` are the three inputs),
/// the `VICTORY_NAN_SAFE` bit that changes it (0: none), and what it should give.
struct Row {
    name: String,
    body: Vec<u32>,
    bit: u32,
    want: Box<dyn Fn([u32; 3]) -> Want>,
}

fn row(name: &str, body: Vec<u32>, bit: u32, want: impl Fn([u32; 3]) -> Want + 'static) -> Row {
    Row {
        name: name.to_owned(),
        body,
        bit,
        want: Box::new(want),
    }
}

/// `v_cmp_<op>_f32 s20, a, b; v_cndmask_b32 v0, 0, 1, s20`.
fn compare_body(code: u16, a: u16, b: u16) -> Vec<u32> {
    let mut body = vop3(code, 20, a, b, 0x80).to_vec();
    body.extend_from_slice(&vop3(0x101, 0, 0x80, 0x81, sgpr(20)));
    body
}

/// `<test>; v_cndmask_b32 v0, v1, 0, s20` (zero where the test holds) or
/// `v_cndmask_b32 v0, 0, v1, s20` (zero where it does not).
fn guard_body(test: [u32; 2], zero_when_true: bool) -> Vec<u32> {
    let mut body = test.to_vec();
    let select = if zero_when_true {
        vop3(0x101, 0, vgpr(1), 0x80, sgpr(20))
    } else {
        vop3(0x101, 0, 0x80, vgpr(1), sgpr(20))
    };
    body.extend_from_slice(&select);
    body
}

fn rows() -> Vec<Row> {
    let (v1, v2, v3) = (vgpr(1), vgpr(2), vgpr(3));
    let mut rows = Vec::new();
    for (name, code, model) in COMPARES {
        rows.push(row(
            &format!("v_cmp_{name}_f32"),
            compare_body(code, v1, v2),
            bits::COMPARE,
            move |v| Want::Bits(u32::from(model(f(v[0]), f(v[1])))),
        ));
    }
    // The NaN tests a shader compiler writes: x != x, unordered with itself, ordered with itself,
    // and the class test; each sends a NaN to 0 and keeps a number.
    let keep = |v: [u32; 3]| {
        if f(v[0]).is_nan() {
            Want::Num(0.0)
        } else {
            Want::Bits(v[0])
        }
    };
    rows.push(row(
        "nan guard: v_cmp_u x,x",
        guard_body(vop3(0x008, 20, v1, v1, 0), true),
        bits::COMPARE,
        keep,
    ));
    rows.push(row(
        "nan guard: v_cmp_o x,x",
        guard_body(vop3(0x007, 20, v1, v1, 0), false),
        bits::COMPARE,
        keep,
    ));
    rows.push(row(
        "nan guard: v_cmp_neq x,x",
        guard_body(vop3(0x00D, 20, v1, v1, 0), true),
        bits::COMPARE,
        keep,
    ));
    rows.push(row(
        "nan guard: v_cmp_nlt x,x",
        // x < x is false for every x: not-less-than is true for all, so a plain select keeps
        // nothing but NaN stays: not used as a guard; checks that the answer is 1 for all.
        compare_body(0x00E, v1, v1),
        bits::COMPARE,
        |_| Want::Bits(1),
    ));
    rows.push(row(
        "nan guard: v_cmp_class_f32 x,3",
        guard_body(vop3(0x088, 20, v1, 0x83, 0), true),
        0,
        keep,
    ));
    // Max and min.
    rows.push(row(
        "v_max_f32",
        vop3(0x110, 0, v1, v2, 0).to_vec(),
        bits::MAX_MIN,
        |v| Want::Bits(max_f(f(v[0]), f(v[1])).to_bits()),
    ));
    rows.push(row(
        "v_min_f32",
        vop3(0x10F, 0, v1, v2, 0).to_vec(),
        bits::MAX_MIN,
        |v| Want::Bits(min_f(f(v[0]), f(v[1])).to_bits()),
    ));
    rows.push(row(
        "v_max3_f32",
        vop3(0x154, 0, v1, v2, v3).to_vec(),
        bits::MAX_MIN,
        |v| Want::Bits(max_f(max_f(f(v[0]), f(v[1])), f(v[2])).to_bits()),
    ));
    rows.push(row(
        "v_min3_f32",
        vop3(0x151, 0, v1, v2, v3).to_vec(),
        bits::MAX_MIN,
        |v| Want::Bits(min_f(min_f(f(v[0]), f(v[1])), f(v[2])).to_bits()),
    ));
    // Products and sums: IEEE (an infinity times 0 is NaN), and the legacy ones.
    rows.push(row(
        "v_mul_f32",
        vop3(0x108, 0, v1, v2, 0).to_vec(),
        0,
        |v| Want::Num(f(v[0]) * f(v[1])),
    ));
    rows.push(row(
        "v_add_f32",
        vop3(0x103, 0, v1, v2, 0).to_vec(),
        0,
        |v| Want::Num(f(v[0]) + f(v[1])),
    ));
    rows.push(row(
        "v_sub_f32",
        vop3(0x104, 0, v1, v2, 0).to_vec(),
        0,
        |v| Want::Num(f(v[0]) - f(v[1])),
    ));
    rows.push(row(
        "v_mul_legacy_f32",
        vop3(0x107, 0, v1, v2, 0).to_vec(),
        bits::LEGACY,
        |v| Want::Num(legacy(f(v[0]), f(v[1]))),
    ));
    rows.push(row(
        "v_mad_legacy_f32",
        vop3(0x140, 0, v1, v2, v3).to_vec(),
        bits::LEGACY,
        |v| Want::Num(legacy(f(v[0]), f(v[1])) + f(v[2])),
    ));
    rows.push(row(
        "v_mul_legacy_f32 x,x",
        vop3(0x107, 0, v1, v1, 0).to_vec(),
        bits::LEGACY,
        |v| Want::Num(legacy(f(v[0]), f(v[0]))),
    ));
    // The clamp bit and the output modifiers: a NaN clamps to 0; `mul:2` keeps a NaN.
    let clamp = |op| vop3_mod(op, 0, v1, v2, 0, 0, 0, 0, true).to_vec();
    rows.push(row("v_add_f32 clamp", clamp(0x103), bits::CLAMP, |v| {
        Want::Num(saturate(f(v[0]) + f(v[1])))
    }));
    rows.push(row("v_mul_f32 clamp", clamp(0x108), bits::CLAMP, |v| {
        Want::Num(saturate(f(v[0]) * f(v[1])))
    }));
    rows.push(row(
        "v_mul_f32 mul:2",
        vop3_mod(0x108, 0, v1, v2, 0, 0, 0, 1, false).to_vec(),
        0,
        |v| Want::Num(f(v[0]) * f(v[1]) * 2.0),
    ));
    rows.push(row(
        "v_add_f32 mul:2 clamp",
        vop3_mod(0x103, 0, v1, v2, 0, 0, 0, 1, true).to_vec(),
        bits::CLAMP,
        |v| Want::Num(saturate((f(v[0]) + f(v[1])) * 2.0)),
    ));
    // The unary functions.
    let unary = |op: u16| vop1(op, 0, v1);
    rows.push(row("v_rcp_f32", vec![unary(0x02A)], 0, |v| {
        let a = f(v[0]);
        if a.is_nan() {
            Want::Nan
        } else if a == 0.0 {
            Want::Bits(
                if a.is_sign_negative() {
                    f32::NEG_INFINITY
                } else {
                    f32::INFINITY
                }
                .to_bits(),
            )
        } else if a.is_infinite() {
            Want::Num(0.0)
        } else {
            let r = 1.0 / a;
            if r.is_normal() {
                Want::Near(r, 2)
            } else {
                Want::Any
            }
        }
    }));
    rows.push(row("v_rsq_f32", vec![unary(0x02E)], 0, |v| {
        let a = f(v[0]);
        if a.is_nan() || a < 0.0 {
            Want::Nan
        } else if a == 0.0 {
            Want::Bits(
                if a.is_sign_negative() {
                    f32::NEG_INFINITY
                } else {
                    f32::INFINITY
                }
                .to_bits(),
            )
        } else if a.is_infinite() {
            Want::Num(0.0)
        } else {
            let r = (1.0 / f64::from(a).sqrt()) as f32;
            if r.is_normal() {
                Want::Near(r, 4)
            } else {
                Want::Any
            }
        }
    }));
    rows.push(row("v_sqrt_f32", vec![unary(0x033)], 0, |v| {
        let a = f(v[0]);
        if a.is_nan() || a < 0.0 {
            Want::Nan
        } else if a == 0.0 || a.is_infinite() {
            Want::Bits(a.to_bits())
        } else {
            Want::Near(a.sqrt(), 2)
        }
    }));
    rows.push(row("v_log_f32", vec![unary(0x027)], 0, |v| {
        let a = f(v[0]);
        if a.is_nan() || a < 0.0 {
            Want::Nan
        } else if a == 0.0 {
            Want::Bits(f32::NEG_INFINITY.to_bits())
        } else if a.is_infinite() {
            Want::Bits(a.to_bits())
        } else {
            Want::Close(a.log2(), 3e-6)
        }
    }));
    rows.push(row("v_exp_f32", vec![unary(0x025)], 0, |v| {
        let a = f(v[0]);
        if a.is_nan() {
            Want::Nan
        } else if a >= 128.0 {
            Want::Bits(f32::INFINITY.to_bits())
        } else if a <= -150.0 {
            Want::Num(0.0)
        } else {
            Want::Close(a.exp2(), 1e-5)
        }
    }));
    rows.push(row("v_fract_f32", vec![unary(0x020)], 0, |v| {
        let a = f(v[0]);
        if a.is_nan() || a.is_infinite() {
            Want::Nan
        } else {
            let r = a - a.floor();
            Want::Num(if r >= 1.0 {
                f32::from_bits(0x3F7F_FFFF)
            } else {
                r
            })
        }
    }));
    for (name, op, round) in [
        ("v_floor_f32", 0x024u16, f32::floor as fn(f32) -> f32),
        ("v_ceil_f32", 0x022, f32::ceil as fn(f32) -> f32),
        ("v_trunc_f32", 0x021, f32::trunc as fn(f32) -> f32),
        ("v_rndne_f32", 0x023, f32::round_ties_even as fn(f32) -> f32),
    ] {
        rows.push(row(name, vec![unary(op)], 0, move |v| {
            let a = f(v[0]);
            if a.is_nan() {
                Want::Nan
            } else if a.is_infinite() {
                Want::Bits(a.to_bits())
            } else {
                Want::Num(round(a))
            }
        }));
    }
    // Conversions to half floats: the packed one rounds toward zero (an overflow is the
    // largest finite half), the single one to nearest.
    rows.push(row(
        "v_cvt_pkrtz_f16_f32",
        vop3(0x12F, 0, v1, v2, 0).to_vec(),
        0,
        |v| Want::Halves(f16_rtz(f(v[0])), f16_rtz(f(v[1]))),
    ));
    rows.push(row("v_cvt_f16_f32", vec![vop1(0x00A, 0, v1)], 0, |v| {
        Want::LowHalf(f16_rne(f(v[0])))
    }));
    rows
}

/// Twelve special values: both zeros, plus and minus one, a fraction, a half's largest, a
/// float's near largest, both infinities and both quiet NaNs.
fn values() -> [u32; 12] {
    [
        0.0f32.to_bits(),
        (-0.0f32).to_bits(),
        1.0f32.to_bits(),
        (-2.5f32).to_bits(),
        0.3f32.to_bits(),
        65504.0f32.to_bits(),
        3e38f32.to_bits(),
        f32::INFINITY.to_bits(),
        f32::NEG_INFINITY.to_bits(),
        0x7FC0_0000,
        0xFFC0_0000,
        (-1.0f32).to_bits(),
    ]
}

fn cases() -> Vec<[u32; 4]> {
    let v = values();
    let mut cases = Vec::new();
    for (i, &x) in v.iter().enumerate() {
        for (j, &y) in v.iter().enumerate() {
            cases.push([x, y, v[(3 * i + j) % v.len()], 0]);
        }
    }
    cases
}

fn lowered(body: &[u32], nan_safe: u32) -> HostShader {
    let was = lower::TEST_NAN_SAFE.replace(Some(nan_safe));
    let shader = build_valu_cs(body);
    lower::TEST_NAN_SAFE.set(was);
    shader
}

/// Runs the table on the hardware adapter with `nan_safe` bits and returns the mismatches
/// (one line each), or `None` where there is no hardware adapter.
fn on_the_card(nan_safe: u32) -> Option<Vec<String>> {
    let cases = cases();
    let mut failures = Vec::new();
    let mut ran = false;
    let mut summary = Vec::new();
    for row in rows() {
        let shader = lowered(&row.body, nan_safe);
        let mut bad = 0;
        let mut seen = false;
        run_cases_on(&shader, &cases, &[("hardware", false)], |_, _, inp, got| {
            seen = true;
            let want = (row.want)([inp[0], inp[1], inp[2]]);
            if !judge(got, want) {
                bad += 1;
                if bad <= 6 {
                    failures.push(format!(
                        "{} (VICTORY_NAN_SAFE={nan_safe}) of {:?} gave {:#010x} ({}), the model says {want:?}",
                        row.name,
                        [f(inp[0]), f(inp[1]), f(inp[2])],
                        got,
                        f(got)
                    ));
                }
            }
        });
        ran |= seen;
        summary.push(format!(
            "{}: {bad} of {} cases differ",
            row.name,
            cases.len()
        ));
    }
    if !ran {
        eprintln!("SKIPPED (no hardware adapter): nan_tests not run on a card");
        return None;
    }
    for line in &summary {
        eprintln!("NAN_TESTS nan_safe={nan_safe} {line}");
    }
    Some(failures)
}

/// The translation as it is: where it differs from RDNA2 at the edges on this card.
#[test]
fn nan_edges_default_on_the_card() {
    if let Some(failures) = on_the_card(0) {
        assert!(
            failures.is_empty(),
            "{} mismatches (first of each op):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

/// With every `VICTORY_NAN_SAFE` bit: where it still differs.
#[test]
fn nan_edges_nan_safe_on_the_card() {
    if let Some(failures) = on_the_card(bits::ALL) {
        assert!(
            failures.is_empty(),
            "{} mismatches (first of each op):\n{}",
            failures.len(),
            failures.join("\n")
        );
    }
}

#[test]
fn the_half_float_models_round_as_the_hardware_does() {
    // 0.3 is 0x34CD to nearest and 0x34CC toward zero; 65520 is where nearest overflows.
    assert_eq!((f16_rne(0.3), f16_rtz(0.3)), (0x34CD, 0x34CC));
    assert_eq!((f16_rne(65519.0), f16_rtz(65519.0)), (0x7BFF, 0x7BFF));
    assert_eq!((f16_rne(65520.0), f16_rtz(65520.0)), (0x7C00, 0x7BFF));
    assert_eq!((f16_rne(1e6), f16_rtz(1e6)), (0x7C00, 0x7BFF));
    assert_eq!((f16_rne(-3e38), f16_rtz(-3e38)), (0xFC00, 0xFBFF));
    assert_eq!(
        (f16_rne(f32::INFINITY), f16_rtz(f32::INFINITY)),
        (0x7C00, 0x7C00)
    );
    assert_eq!((f16_rne(1.0), f16_rtz(1.0)), (0x3C00, 0x3C00));
    assert_eq!((f16_rne(-2.5), f16_rtz(-2.5)), (0xC100, 0xC100));
    assert!(half_nan(f16_rne(f32::NAN)) && half_nan(f16_rtz(f32::NAN)));
    assert_eq!((f16_rne(0.0), f16_rne(-0.0)), (0, 0x8000));
}

#[test]
fn the_max_min_and_legacy_models_follow_the_isa() {
    let nan = f32::NAN;
    assert_eq!(max_f(nan, 2.0), 2.0);
    assert_eq!(min_f(3.0, nan), 3.0);
    assert!(max_f(nan, nan).is_nan());
    assert_eq!(max_f(-0.0, 0.0).to_bits(), 0);
    assert_eq!(max_f(-0.0, -0.0).to_bits(), 0x8000_0000);
    assert_eq!(min_f(-0.0, 0.0).to_bits(), 0x8000_0000);
    assert_eq!(min_f(0.0, 0.0).to_bits(), 0);
    assert_eq!(legacy(0.0, f32::INFINITY), 0.0);
    assert_eq!(legacy(nan, -0.0), 0.0);
    assert!(legacy(nan, 1.0).is_nan());
    assert_eq!(saturate(nan), 0.0);
    assert_eq!(saturate(f32::INFINITY), 1.0);
    assert_eq!(saturate(f32::NEG_INFINITY), 0.0);
    // The judges: a NaN for a NaN, a zero for a zero, a few units in the last place.
    assert!(judge(0x7FC0_0001, Want::Bits(0xFFC0_0000)));
    assert!(!judge(0, Want::Bits(0x8000_0000)));
    assert!(judge(0x8000_0000, Want::Num(0.0)));
    assert!(judge(1.0f32.to_bits() + 2, Want::Near(1.0, 2)));
    assert!(!judge(1.0f32.to_bits() + 3, Want::Near(1.0, 2)));
    assert!(!judge(f32::INFINITY.to_bits(), Want::Near(1.0, 4)));
    assert!(judge(0x3C00_7E01, Want::Halves(0x7E00, 0x3C00)));
    assert!(!judge(0x3C00_7C00, Want::Halves(0x7E00, 0x3C00)));
    assert!(judge(0xFFFF_3C00, Want::LowHalf(0x3C00)));
}

/// Each `VICTORY_NAN_SAFE` bit changes the translation of the ops it names, and of no other
/// op of the table; a bit that names none would be a switch that does nothing.
#[test]
fn each_nan_safe_bit_changes_only_the_ops_it_names() {
    let mut named = [0usize; 4];
    for row in rows() {
        let plain = lowered(&row.body, 0).dxil;
        for (n, bit) in [bits::COMPARE, bits::LEGACY, bits::CLAMP, bits::MAX_MIN]
            .into_iter()
            .enumerate()
        {
            let changed = lowered(&row.body, bit).dxil != plain;
            assert_eq!(
                changed,
                row.bit == bit,
                "{}: bit {bit} {}",
                row.name,
                if changed { "changes it" } else { "leaves it" }
            );
            named[n] += usize::from(changed);
        }
        // All of them at once is no other than the one bit the op names.
        let all = lowered(&row.body, bits::ALL).dxil;
        let own = lowered(&row.body, row.bit).dxil;
        assert_eq!(all == plain, row.bit == 0, "{}", row.name);
        if row.bit != 0 {
            assert_eq!(all, own, "{}: the other bits change it too", row.name);
        }
    }
    assert!(named.iter().all(|&n| n > 0), "a bit with no op: {named:?}");
}
