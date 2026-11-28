//! The group barriers of the kept skips' wave-wide form, checked without a card: the audit of the
//! emitted control flow ([`super::bitcode::together`]) and a model of the two host waves of a
//! console wave running that control flow ([`super::bitcode::sim`]), which must pass the same
//! barriers the same number of times, whatever the lanes hold and however the waves are
//! interleaved.
//!
//! The programs are a lane branch around a region in which the two host waves meet (an LDS read of
//! what the lane 16 above wrote), the region being straight code, a loop, a loop in a loop, or a
//! loop whose lanes need different numbers of rounds, and the branch a skip or an early-out. The
//! lanes below s0 take the branch (or, `reverse`, the lanes from s0 on): s0 of 0 or 64 puts no
//! lane in, 32 leaves one half with none, 16 and 48 leave both with some.

use super::bitcode::sim;
use super::bitcode::{Attr, Bin, Module, Term, pred};
use super::form::Mode;
use super::hang;
use super::shader::Shader;
use super::{Form, HostShader, lower};
use crate::dxbc::Container;
use crate::native::{ComputeIo, HwStage, NativeProgram, ResourceType, RootParam, RootType, StageIo, UsedResource};
use crate::{ShaderError, io, ir, resource, structure};

const S_ENDPGM: u32 = 0xBF81_0000;

/// The program around `body` (a console wave64 compute program of `threads` threads).
fn native_of(body: &[u32], threads: u32) -> NativeProgram {
    let mut words = vec![
        0xBE85_0380, // s_mov_b32 s5, 0
        0x3D << 26 | 0x02 << 18 | 8 << 6 | 2,
        125 << 25, // s_load_dwordx4 s[8:11], s[4:5], 0
    ];
    words.extend_from_slice(body);
    words.push(S_ENDPGM);
    NativeProgram {
        stage: HwStage::Cs,
        wave64: true,
        vgprs: 16,
        layout: vec![
            RootParam { root: RootType::Constants, table_offset_dwords: 0, sgprs: 4, base_register: 0, user_data: 0, slot: 0 },
            RootParam { root: RootType::Table, table_offset_dwords: 0, sgprs: 1, base_register: 0, user_data: 4, slot: 1 },
        ],
        used: vec![UsedResource {
            kind: ResourceType::Uav,
            root: RootType::Table,
            table_offset_dwords: 0,
            lower_bound: 0,
            stride: 0,
            dwords: 4,
            user_data: 4,
            slot: 1,
        }],
        io: StageIo::Cs(ComputeIo { rsrc2: 5 << 1, threads: [threads, 1, 1] }),
        code: words.iter().flat_map(|w| w.to_le_bytes()).collect(),
    }
}

/// What the branch goes around.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Over {
    /// A skip: the region, then the join (the restore of EXEC and a store) and the end.
    Skip,
    /// An early-out: the region, then the end of the program.
    EarlyOut,
}

/// The region.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Body {
    Straight,
    Looped,
    /// A loop of two rounds in each round of a loop of two.
    Nested,
    /// A loop whose lanes need `lane / 16` more rounds (lanes drop out of EXEC, one branch for
    /// the wave takes the next round).
    Counted,
    /// A loop of two rounds whose exit is tested at the top, before the round's exchange.
    TopExit,
    /// Two skips in a row: the lanes below 40 go on from the first region into a loop of two rounds,
    /// the others leave (so a half may have no lane for the second region).
    TwoSkips,
    /// A serial walk: a loop of as many rounds as the second root constant says (`s1`, 0 rounds
    /// skip it), the same for every lane and every half, one exchange in each round (the shape of
    /// a list walked by one lane for a count read from the data).
    Serial,
    /// The walk with a retry loop in each round: `s1` rounds, each a loop of `s2` rounds (at least
    /// one) with an exchange in each (a compare-and-swap that the lanes repeat until it takes).
    SerialNested,
}

/// A loop of the counter `s<counter>` against the root constant `s<limit>`, round `body`.
fn counted(counter: u32, limit: u32, body: &[u32]) -> Vec<u32> {
    let mut w = vec![0xBE80_0380 | counter << 16]; // s_mov_b32 s<counter>, 0
    w.extend_from_slice(body);
    w.extend_from_slice(&[
        0x8000_8100 | counter << 16 | counter, // s_add_u32 s<counter>, s<counter>, 1
        0xBF0A_0000 | limit << 8 | counter,    // s_cmp_lt_u32 s<counter>, s<limit>
        0xBF85_0000 | (-(body.len() as i32 + 3) as u16 as u32), // s_cbranch_scc1 back to the round
    ]);
    w
}

/// `counted`, skipped when `s<limit>` is 0.
fn counted_or_none(counter: u32, limit: u32, body: &[u32]) -> Vec<u32> {
    let l = counted(counter, limit, body);
    let mut w = vec![
        0xBF06_8000 | limit,                 // s_cmp_eq_u32 s<limit>, 0
        0xBF85_0000 | l.len() as u32,        // s_cbranch_scc1 past the loop
    ];
    w.extend(l);
    w
}

/// `v4 = v0 + 1; lds[t] = v4; out[t] = lds[(t + 16) & 63]`.
fn exchange_words() -> Vec<u32> {
    vec![
        0x4A08_0081, // v_add_nc_u32 v4, 1, v0
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
    ]
}

fn region(body: Body) -> Vec<u32> {
    let mut w = Vec::new();
    match body {
        Body::Straight => w.extend(exchange_words()),
        Body::Looped => {
            w.push(0xBE95_0380); // s_mov_b32 s21, 0
            w.extend(exchange_words());
            w.extend_from_slice(&[
                0x8015_8115, // s_add_u32 s21, s21, 1
                0xBF0A_8215, // s_cmp_lt_u32 s21, 2
                0xBF85_FFF1, // s_cbranch_scc1 L
            ]);
        }
        Body::Nested => {
            w.push(0xBE96_0380); // s_mov_b32 s22, 0
            let inner = region(Body::Looped);
            let len = inner.len() + 3;
            w.extend(inner);
            w.extend_from_slice(&[
                0x8016_8116, // s_add_u32 s22, s22, 1
                0xBF0A_8216, // s_cmp_lt_u32 s22, 2
                0xBF85_0000 | (-(len as i32) as u16 as u32), // s_cbranch_scc1 OL
            ]);
        }
        Body::TopExit => {
            let round = exchange_words();
            w.push(0xBE95_0380); // s_mov_b32 s21, 0
            w.push(0xBF09_8215); // L: s_cmp_ge_u32 s21, 2
            w.push(0xBF85_0000 | (round.len() as u32 + 2)); // s_cbranch_scc1 DONE
            let len = round.len() as i32 + 4;
            w.extend(round);
            w.push(0x8015_8115); // s_add_u32 s21, s21, 1
            w.push(0xBF82_0000 | (-len as u16 as u32)); // s_branch L
        }
        Body::TwoSkips => {
            w.extend(exchange_words());
            let second = region(Body::Looped);
            w.extend_from_slice(&[
                0x7D88_00A8, // v_cmp_gt_u32 vcc, 40, v0
                0x87FE_6A7E, // s_and_b64 exec, exec, vcc
                0xBF88_0000 | second.len() as u32, // s_cbranch_execz past the second region
            ]);
            w.extend(second);
        }
        Body::Serial => w.extend(counted_or_none(21, 1, &exchange_words())),
        Body::SerialNested => {
            let inner = counted(21, 2, &exchange_words());
            w.extend(counted_or_none(22, 1, &inner));
        }
        Body::Counted => {
            w.push(0xBE95_0380); // s_mov_b32 s21, 0
            w.extend(exchange_words());
            let len = w.len() as i32 - 1 + 4;
            w.extend_from_slice(&[
                0x8015_8115, // s_add_u32 s21, s21, 1
                0x7D82_1015, // v_cmp_lt_u32 vcc, s21, v8
                0x87FE_6A7E, // s_and_b64 exec, exec, vcc
                0xBF89_0000 | (-len as u16 as u32), // s_cbranch_execnz L
            ]);
        }
    }
    w
}

/// The program, in a group of `threads`.
fn lane_branch(over: Over, body: Body, threads: u32, reverse: bool) -> NativeProgram {
    let region = region(body);
    let mut words = vec![
        0xBE94_0380, // s_mov_b32 s20, 0
        0x7E08_02C1, // v_mov_b32 v4, -1
        0x3406_0082, // v_lshlrev_b32 v3, 2, v0
        0x2C10_0084, // v_lshrrev_b32 v8, 4, v0
        0xD834_0000, // ds_write_b32 v3, v4
        0x0000_0403,
        0xBF8C_C07F, // s_waitcnt lgkmcnt(0)
        if reverse { 0x7D86_0000 } else { 0x7D88_0000 }, // v_cmp_le_u32 / v_cmp_gt_u32 vcc, s0, v0
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
    let mut native = native_of(&words, threads);
    if let StageIo::Cs(cs) = &mut native.io {
        cs.rsrc2 |= 1 << 15; // 512 bytes of LDS
    }
    native
}

/// The program lowered as the title's are for a host of 32-lane waves, and the module that came
/// out.
fn lowered(native: &NativeProgram, mode: Mode, guard: Option<hang::Guard>) -> (Result<HostShader, ShaderError>, Form, Option<Module>) {
    let p = ir::build(&native.code, native.wave64, native.vgprs).expect("ir::build");
    let st = structure::structure(&p).expect("structure");
    let res = resource::track(&p, &native.layout, native.stage, Some(&st)).expect("resource::track");
    let io = io::raise(&p, native).expect("io::raise");
    let bytes = crate::dxbc::build(&[]);
    let c = Container::parse(&bytes).expect("container");
    lower::set_program_tag(b"a hang test");
    let was = lower::TEST_HOST_WAVE.replace(Some(32));
    ir::TEST_KEEP_LOOP_SKIPS.set(Some(true));
    super::form::TEST_KEPT_SKIPS_WIDE.set(Some(mode));
    hang::TEST_GUARD.set(Some(guard));
    hang::TEST_MODULE.take();
    let (result, form) = super::emit_as(&c, native, &p, &st, &io, &res);
    hang::TEST_GUARD.set(None);
    super::form::TEST_KEPT_SKIPS_WIDE.set(None);
    ir::TEST_KEEP_LOOP_SKIPS.set(None);
    lower::TEST_HOST_WAVE.set(was);
    (result, form, hang::TEST_MODULE.take())
}

fn shapes() -> Vec<(Over, Body, bool)> {
    let mut v = Vec::new();
    for over in [Over::Skip, Over::EarlyOut] {
        for body in [Body::Straight, Body::Looped, Body::Nested, Body::Counted, Body::TopExit, Body::TwoSkips] {
            for reverse in [false, true] {
                v.push((over, body, reverse));
            }
        }
    }
    v
}

const THRESHOLDS: [u32; 9] = [0, 1, 16, 31, 32, 33, 48, 63, 64];

/// The shapes that were lowered into the kept skips' wave-wide form with `mode` (the others are not
/// the form the audit and the model are for).
fn kept_wide_shapes(mode: Mode) -> Vec<((Over, Body, bool), Module)> {
    let mut found = Vec::new();
    for shape in shapes() {
        let native = lane_branch(shape.0, shape.1, 64, shape.2);
        let (result, form, module) = lowered(&native, mode, None);
        if result.is_ok() && form == Form::KeptSkipsWide {
            found.push((shape, module.expect("a module")));
        }
    }
    found
}

#[test]
fn the_shapes_with_a_loop_take_the_kept_skips_wave_wide_form_unless_a_half_can_miss_a_barrier() {
    let found = kept_wide_shapes(Mode::Safe);
    let names: Vec<_> = found.iter().map(|f| f.0).collect();
    // (no loop: the skip is left out; lanes that need different rounds: the audit refuses)
    let has = |body: Body| names.iter().any(|s| s.1 == body);
    assert!(
        has(Body::Looped) && has(Body::Nested) && !has(Body::Straight) && !has(Body::Counted),
        "the shapes in the kept skips' form: {names:?} (forms: {:?})",
        shapes()
            .into_iter()
            .map(|s| (s, lowered(&lane_branch(s.0, s.1, 64, s.2), Mode::Safe, None).1))
            .collect::<Vec<_>>()
    );
}

/// Collects what is wrong and says all of it at once (the first listing is the module's text).
#[derive(Default)]
struct Findings {
    lines: Vec<String>,
    listing: Option<String>,
}

impl Findings {
    fn add(&mut self, line: String, module: &Module) {
        if self.listing.is_none() {
            self.listing = Some(format!("{line}
{}", module.listing()));
        }
        self.lines.push(line);
    }

    fn done(self) {
        assert!(self.lines.is_empty(), "{}

{}", self.lines.join("
"), self.listing.unwrap_or_default());
    }
}

/// What the model says of `module` with `s0` lanes in the branch: the problems it found, and
/// whether every thread passed the same barriers.
fn model(module: &Module, s0: u32, seed: u64) -> Result<(Vec<String>, bool), String> {
    let report = sim::run(module, 64, &[s0, 0, 0, 0], seed)?;
    let mut problems = report.problems.clone();
    problems.dedup();
    Ok((problems, report.alike()))
}

/// The classes alone (the argument about the wide branches) let a shape through whose barriers a
/// half can miss: a loop whose lanes need different rounds, one half leaving it before the other,
/// with a group barrier in each round. The audit of the emitted control flow sees it, and with
/// it the form is not taken.
#[test]
fn the_classes_alone_let_through_a_loop_whose_halves_leave_in_different_rounds() {
    let by_classes = kept_wide_shapes(Mode::Classes);
    let audited = kept_wide_shapes(Mode::Safe);
    let missed: Vec<_> = by_classes.iter().filter(|(_, m)| !m.audit_barriers().safe(true)).map(|f| f.0).collect();
    assert!(missed.iter().any(|s| s.1 == Body::Counted), "the classes found every shape with a barrier a half can miss: {missed:?}");
    for shape in &missed {
        assert!(audited.iter().all(|a| a.0 != *shape), "{shape:?} is in the audited form");
    }
    // (the others are not touched: what the classes took and the audit accepts is the same)
    let rest: Vec<_> = by_classes.iter().map(|f| f.0).filter(|s| !missed.contains(s)).collect();
    assert_eq!(rest, audited.iter().map(|f| f.0).collect::<Vec<_>>());
    eprintln!("the classes alone let through: {missed:?}");
}

/// The audit of the emitted control flow finds no group barrier that a thread can fail to reach.
#[test]
fn the_group_barriers_of_the_audited_kept_skips_form_are_reached_by_every_thread() {
    let mut found = Findings::default();
    for (shape, module) in kept_wide_shapes(Mode::Safe) {
        let audit = module.audit_barriers();
        if audit.barriers == 0 || !audit.safe(false) {
            found.add(format!("{shape:?}: {}", audit.describe()), &module);
        }
    }
    found.done();
}

/// Both host waves pass every barrier the same number of times, whatever the lanes hold and
/// however the waves are interleaved: exact counts for every thread, and nothing waits for a
/// thread that is not there.
#[test]
fn the_two_halves_pass_the_same_barriers_in_the_audited_kept_skips_form() {
    let mut found = Findings::default();
    for (shape, module) in &kept_wide_shapes(Mode::Safe) {
        for s0 in THRESHOLDS {
            for seed in 1..=3 {
                match model(module, s0, seed) {
                    Err(e) => found.add(format!("{shape:?} s0={s0}: {e}"), module),
                    Ok((problems, alike)) if !problems.is_empty() || !alike => {
                        found.add(format!("{shape:?} s0={s0} seed={seed}: {problems:?} alike={alike}"), module);
                    }
                    Ok(_) => {}
                }
            }
        }
    }
    found.done();
}

/// The audit sees every problem the model finds, in every form the shapes end in: a barrier that a
/// thread can fail to reach is a hang in whichever form it stands.
#[test]
fn the_audit_sees_every_problem_the_model_finds_in_every_form() {
    let mut found = Findings::default();
    let mut model_found = 0;
    for shape in shapes() {
        let native = lane_branch(shape.0, shape.1, 64, shape.2);
        for mode in [Mode::Classes, Mode::Safe, Mode::Off] {
            let (result, form, module) = lowered(&native, mode, None);
            let (Ok(_), Some(module)) = (result, module) else { continue };
            let audit = module.audit_barriers();
            for s0 in [0, 16, 32, 48, 64] {
                match model(&module, s0, 7) {
                    Err(e) => found.add(format!("{shape:?} {form:?} {mode:?} s0={s0}: {e}"), &module),
                    Ok((problems, alike)) if !problems.is_empty() || !alike => {
                        model_found += 1;
                        if audit.safe(true) {
                            found.add(format!("{shape:?} {form:?} {mode:?} s0={s0}: {problems:?}, the audit: {}", audit.describe()), &module);
                        }
                    }
                    Ok(_) => {}
                }
            }
        }
    }
    assert!(model_found > 0, "the model found nothing in any form");
    found.done();
}

/// The serial walks (the loop of a list walked by the lanes below `s0`, for a count from the data,
/// alone and with a retry loop in each round), as a skip and as an early-out.
fn walks() -> Vec<(Over, Body)> {
    let mut v = Vec::new();
    for over in [Over::Skip, Over::EarlyOut] {
        for body in [Body::Serial, Body::SerialNested] {
            v.push((over, body));
        }
    }
    v
}

/// The walk in the audited kept skips' form.
fn walk_module(over: Over, body: Body, mode: Mode) -> (Form, Module) {
    let native = lane_branch(over, body, 64, false);
    let (result, form, module) = lowered(&native, mode, None);
    assert!(result.is_ok(), "{over:?} {body:?} {mode:?}: {:?}", result.err());
    (form, module.expect("a module"))
}

/// The barriers every thread passed, which must be the same list for all of them.
fn walk_barriers(module: &Module, s0: u32, count: u32, retries: u32, seed: u64) -> usize {
    let report = sim::run(module, 64, &[s0, count, retries, 0], seed)
        .unwrap_or_else(|e| panic!("s0={s0} count={count} retries={retries}: {e}"));
    assert!(
        report.problems.is_empty() && report.alike(),
        "s0={s0} count={count} retries={retries} seed={seed}: {:?} alike={}",
        report.problems,
        report.alike()
    );
    report.barriers[0].len()
}

/// A walk whose count and retries come from the data, in the audited form: the audit takes it
/// (and the loops nest two deep, which `strict` does not take), and in the model every thread of
/// the group passes the same barriers whatever the count, the retries and the lanes that have work
/// (none, one, one half, all), with every interleaving tried.
#[test]
fn the_audited_form_takes_the_serial_walks_and_every_thread_passes_the_same_barriers() {
    for (over, body) in walks() {
        let (form, module) = walk_module(over, body, Mode::Audited);
        assert_eq!(form, Form::KeptSkipsWide, "{over:?} {body:?}: {}", module.audit_barriers().describe());
        let audit = module.audit_barriers();
        assert!(audit.safe(false) && audit.nest <= 2, "{over:?} {body:?}: {}", audit.describe());
        if body == Body::SerialNested {
            assert_eq!(audit.nest, 2, "{}", audit.describe());
            let (_, strict, _) = lowered(&lane_branch(over, body, 64, false), Mode::Strict, None);
            assert_ne!(strict, Form::KeptSkipsWide, "strict takes loops two deep");
        }
        for s0 in [0, 1, 16, 32, 33, 48, 64] {
            for count in [0, 1, 3, 9] {
                for retries in [1, 2, 5] {
                    for seed in 1..=2 {
                        walk_barriers(&module, s0, count, retries, seed);
                    }
                }
            }
        }
    }
}

/// The loop ends as soon as no lane has work and by its own count: with no lane in the branch (the
/// data not there, all lanes empty) the group passes no barrier of the walk, whatever the count; with lanes it
/// passes the same number in each round and ends after the count's rounds (the exit is the scalar
/// count's: the same in every thread, so the halves leave in the same round).
#[test]
fn the_serial_walk_ends_with_the_lanes_and_by_its_count() {
    for (over, body) in walks() {
        let (_, module) = walk_module(over, body, Mode::Audited);
        // (what the program passes outside the walk, with no lane in it and no round)
        let outside = walk_barriers(&module, 0, 0, 1, 3);
        for count in [0, 1, 5, 4000] {
            assert_eq!(walk_barriers(&module, 0, count, 1, 3), outside, "{over:?} {body:?}: lanes empty, count {count}");
        }
        for s0 in [1, 32, 33, 64] {
            let none = walk_barriers(&module, s0, 0, 1, 3);
            let per_round = walk_barriers(&module, s0, 1, 1, 3) - none;
            assert!(per_round > 0, "{over:?} {body:?} s0={s0}: no barrier in a round");
            for count in [2, 3, 9] {
                assert_eq!(
                    walk_barriers(&module, s0, count, 1, 3),
                    none + count as usize * per_round,
                    "{over:?} {body:?} s0={s0}: {count} rounds"
                );
            }
        }
    }
}

/// A module by hand: loops three deep around the wait of the two host waves of a console wave, and
/// a barrier that waits for the group in the innermost one only if `group`.
fn deep_wait_module(group: bool) -> Module {
    let mut s = Shader::new(false);
    s.threads = Some([128, 1, 1]);
    let (e, void) = (s.entry, s.t.void);
    let c = s.m.bool(true);
    let (h1, h2, h3, l3, l2, l1, end) = (
        s.m.new_block(),
        s.m.new_block(),
        s.m.new_block(),
        s.m.new_block(),
        s.m.new_block(),
        s.m.new_block(),
        s.m.new_block(),
    );
    s.m.terminate(e, Term::Br(h1));
    s.m.terminate(h1, Term::Br(h2));
    s.m.terminate(h2, Term::Br(h3));
    s.m.mark_wait(h3);
    if group {
        let mode = s.m.i32(9);
        s.op(h3, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }
    s.m.terminate(h3, Term::Br(l3));
    s.m.terminate(l3, Term::CondBr(c, h3, l2));
    s.m.terminate(l2, Term::CondBr(c, h2, l1));
    s.m.terminate(l1, Term::CondBr(c, h1, end));
    s.m.terminate(end, Term::Ret);
    s.m
}

/// The loops that hold only the waits of the two host waves of a console wave (cheap a round) are
/// told from the loops that hold a barrier that waits for the group (a round costs the barrier): the
/// audited form allows the first three deep and the second two deep.
#[test]
fn the_audit_counts_the_loops_of_group_barriers_apart_from_those_of_the_hosts_waits() {
    let waits = deep_wait_module(false).audit_barriers();
    assert!(waits.safe(false) && waits.nest == 3 && waits.barrier_nest == 0 && waits.waits == 1, "{}", waits.describe());
    let both = deep_wait_module(true).audit_barriers();
    assert!(both.safe(false) && both.nest == 3 && both.barrier_nest == 3, "{}", both.describe());
}

/// With the guard on the audited form is the same form: audited as the program has it, made with the
/// guard's counters in place of the barriers, and every thread of the group still passes the same
/// waits and stores whatever the count.
#[test]
fn the_audited_form_is_taken_with_the_guard_on_too() {
    let guard = hang::Guard { spins: 1 << 12, budget: 1 << 16, loops: 1 << 10 };
    for (over, body) in walks() {
        let native = lane_branch(over, body, 64, false);
        let (plain, plain_form, plain_module) = lowered(&native, Mode::Audited, None);
        let (guarded, form, module) = lowered(&native, Mode::Audited, Some(guard));
        assert!(plain.is_ok() && guarded.is_ok(), "{over:?} {body:?}");
        assert_eq!((plain_form, form), (Form::KeptSkipsWide, Form::KeptSkipsWide), "{over:?} {body:?}");
        let (plain_module, module) = (plain_module.expect("a module"), module.expect("a module"));
        assert!(module.listing().len() > plain_module.listing().len(), "the guard's counters are in the form");
        for s0 in [0, 1, 32, 64] {
            for count in [0, 2, 5] {
                let a = sim::run(&plain_module, 64, &[s0, count, 2, 0], 5).expect("run");
                let b = sim::run(&module, 64, &[s0, count, 2, 0], 5).unwrap_or_else(|e| panic!("{over:?} {body:?} guarded: {e}"));
                assert!(b.problems.is_empty(), "{over:?} {body:?} s0={s0} count={count}: {:?}", b.problems);
                assert_eq!(a.stores, b.stores, "{over:?} {body:?} s0={s0} count={count}: the guard changed what is stored");
            }
        }
    }
}

/// `VICTORY_KEPT_SKIPS_ONLY`: unset, every program; set, those whose hash starts with one of the
/// digits named (in any case, spaces around the commas allowed), and none if it names nothing.
#[test]
fn the_audited_form_can_be_asked_for_by_program() {
    let tag = 0xF000_0000 | 0x0238_4A6F;
    assert!(super::kept_wide::named_in(tag, None));
    assert!(super::kept_wide::named_in(tag, Some("02384a6")));
    assert!(super::kept_wide::named_in(tag, Some("2233808, 0238")));
    assert!(super::kept_wide::named_in(tag, Some("02384A")));
    assert!(!super::kept_wide::named_in(tag, Some("2233808,d89ff74")));
    assert!(!super::kept_wide::named_in(tag, Some("")));
}

/// A retry loop that never ends (the data not there, a compare that never takes) is cut at
/// [`hang::NESTED_LOOP_LIMIT`] rounds in the audited form: the barriers of the walk are the count's
/// times that many, not the square of the loop limit; without the bound (`strict` is not taken, the
/// classes alone take it) the same inputs pass more.
#[test]
fn a_retry_loop_in_the_audited_form_is_cut_at_its_bound() {
    let (_, bounded) = walk_module(Over::Skip, Body::SerialNested, Mode::Audited);
    let (form, unbounded) = walk_module(Over::Skip, Body::SerialNested, Mode::Classes);
    assert_eq!(form, Form::KeptSkipsWide);
    let limit = hang::NESTED_LOOP_LIMIT as usize;
    for (module, cut) in [(&bounded, true), (&unbounded, false)] {
        let one = walk_barriers(module, 1, 3, 1, 4);
        let two = walk_barriers(module, 1, 3, 2, 4);
        let per_retry = (two - one) / 3;
        assert!(per_retry > 0);
        let many = walk_barriers(module, 1, 3, 200, 4);
        let rounds = if cut { limit } else { 200 };
        assert_eq!(many, one + 3 * (rounds - 1) * per_retry, "cut={cut}: {many} barriers for 3 x 200 retries");
    }
}

/// A module by hand: a branch on the thread's own parity, a barrier on one side (and on the
/// other, if `both`), and one after the join.
fn parity_module(both: bool, together: bool) -> Module {
    let mut s = Shader::new(false);
    s.threads = Some([64, 1, 1]);
    let (e, i32t, void) = (s.entry, s.t.i32, s.t.void);
    let flat = s.op(e, "flattenedThreadIdInGroup.i32", Attr::ReadNone, i32t, 96, &[]);
    let (one, zero) = (s.m.i32(1), s.m.i32(0));
    let bit = s.m.bin(e, Bin::And, flat, one);
    let c = s.m.cmp(e, pred::NE, bit, zero);
    if together {
        s.m.mark_together(c);
    }
    let (a, b, join) = (s.m.new_block(), s.m.new_block(), s.m.new_block());
    s.m.terminate(e, Term::CondBr(c, a, b));
    let mode = s.m.i32(9);
    s.op(a, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    s.m.terminate(a, Term::Br(join));
    if both {
        s.op(b, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }
    s.m.terminate(b, Term::Br(join));
    s.op(join, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    s.m.terminate(join, Term::Ret);
    s.m
}

/// The tools see what they are for: a barrier behind a branch on the thread's own parity is
/// found by the audit and is a hang in the model; the same branch said to be the group's
/// (an exchanged decision) is not.
#[test]
fn the_audit_and_the_model_see_a_barrier_behind_a_branch_on_the_threads_own_value() {
    for both in [false, true] {
        let module = parity_module(both, false);
        let audit = module.audit_barriers();
        assert!(!audit.differs.is_empty(), "both={both}: {}", audit.describe());
        assert!(!audit.safe(true));
        let report = sim::run(&module, 64, &[], 3).expect("run");
        assert!(!report.problems.is_empty(), "both={both}: the model saw nothing wrong");
    }
    let module = parity_module(false, true);
    assert!(module.audit_barriers().differs.is_empty(), "a decision the group exchanged was flagged");
}

/// A module by hand: a branch on the thread's own parity to a block that ends the program (what a
/// loop's limit does to a thread that reaches it), and a barrier after the join.
fn cut_module(marked: bool) -> Module {
    let mut s = Shader::new(false);
    s.threads = Some([64, 1, 1]);
    let (e, i32t, void) = (s.entry, s.t.i32, s.t.void);
    let flat = s.op(e, "flattenedThreadIdInGroup.i32", Attr::ReadNone, i32t, 96, &[]);
    let (one, zero) = (s.m.i32(1), s.m.i32(0));
    let bit = s.m.bin(e, Bin::And, flat, one);
    let c = s.m.cmp(e, pred::NE, bit, zero);
    let (cut, join) = (s.m.new_block(), s.m.new_block());
    if marked {
        s.m.mark_abort(cut);
    }
    s.m.terminate(e, Term::CondBr(c, cut, join));
    s.m.terminate(cut, Term::Ret);
    let mode = s.m.i32(9);
    s.op(join, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    s.m.terminate(join, Term::Ret);
    s.m
}

/// A thread that a loop's limit cuts leaves the program: that is an error path, not a branch the
/// audit follows (every loop of the program has one, and with them every barrier after a loop would
/// be a finding). The same exit that is not the limit's is a finding.
#[test]
fn the_exit_of_a_thread_cut_by_a_loops_limit_is_no_flow_of_the_program() {
    let cut = cut_module(true).audit_barriers();
    assert!(cut.safe(false) && cut.cuts == 1, "{}", cut.describe());
    let other = cut_module(false).audit_barriers();
    assert!(!other.safe(true) && other.cuts == 0, "{}", other.describe());
}

/// With the guard on, a barrier is counters and a wait: the shapes still pass the same barriers
/// (those of the entry) and make the same stores, and a half that goes away leaves the other
/// ending with a note instead of waiting for ever.
#[test]
fn the_guard_keeps_the_stores_and_ends_a_wait_for_threads_that_are_gone() {
    let guard = hang::Guard { spins: 64, budget: 1 << 16, loops: 1 << 10 };
    let mut tried = 0;
    for shape in shapes() {
        let native = lane_branch(shape.0, shape.1, 64, shape.2);
        let (plain, form, module) = lowered(&native, Mode::Safe, None);
        let (guarded, gform, gmodule) = lowered(&native, Mode::Safe, Some(guard));
        let (Ok(_), Some(module), Ok(_), Some(gmodule)) = (plain, module, guarded, gmodule) else { continue };
        if form != Form::KeptSkipsWide || gform != Form::KeptSkipsWide {
            continue;
        }
        tried += 1;
        for s0 in [0, 16, 32, 64] {
            let a = sim::run(&module, 64, &[s0, 0, 0, 0], 5).expect("run");
            let b = sim::run(&gmodule, 64, &[s0, 0, 0, 0], 5).unwrap_or_else(|e| panic!("{shape:?} guarded: {e}"));
            assert!(b.problems.is_empty(), "{shape:?} guarded s0={s0}: {:?}", b.problems);
            assert_eq!(a.stores, b.stores, "{shape:?} s0={s0}: the guard changed what is stored");
        }
        // the upper half is gone after a while: the lower half's wait runs out
        let all_in = if shape.2 { 0 } else { 64 };
        let b = sim::run_with(&gmodule, 64, &[all_in, 0, 0, 0], 5, Some((1, 32..64))).expect("run");
        let notes: Vec<u64> = b.notes.iter().copied().filter(|&v| v >= u64::from(hang::HANG_NOTE) && v < 0xE000_0000).collect();
        assert!(
            notes.iter().any(|&n| hang::say(n as u32).0 == hang::kind::WAIT || hang::say(n as u32).0 == hang::kind::BUDGET),
            "{shape:?}: no note of the guard (notes {:x?}, problems {:?})
{}",
            b.notes,
            b.problems,
            gmodule.listing().chars().take(7000).collect::<String>()
        );
        assert!(!b.problems.iter().any(|p| p.contains("hang")), "{shape:?}: the group hung: {:?}", b.problems);
    }
    assert!(tried >= 2, "only {tried} shapes were lowered in the kept skips' form with and without the guard");
}

/// On the card (they lose it if the hardware hangs on what they test: run them one at a time, and
/// not on a machine whose picture matters).
#[cfg(windows)]
mod on_the_card {
    use super::*;
    use crate::emit::host::{Dispatch, Host, Root, View};

    fn run(shader: &HostShader, host: &Host, below: u32, groups: u32) -> Result<Option<Vec<u32>>, String> {
        let Ok((root, pso)) = host.compute_pipeline(shader) else {
            return Ok(None);
        };
        let mut ud = vec![0u32; shader.user_data_dwords().max(1) as usize];
        for u in &shader.user_data {
            if let (RootType::Constants, 0, 0) = (u.root, u.slot, u.dword) {
                ud[u.index as usize] = below;
            }
        }
        let out = host.dispatch(
            &root,
            &pso,
            &Dispatch { roots: vec![(0, Root::Constants(ud))], views: &[View::RawUav(128)], samplers: &[], groups: [groups, 1, 1] },
        )?;
        Ok(Some(out[0][..128].to_vec()))
    }

    /// Whether the hardware finishes a group whose halves do not pass the same barriers: the shapes
    /// whose lanes need different rounds (a loop with group barriers in it that one half leaves
    /// before the other) in each form they take, with every number of lanes in the branch, in one
    /// group and in many. A dispatch that does not finish (device removed) fails the test and says
    /// the shape, the form and the number of lanes.
    #[test]
    #[ignore = "can lose the card: the hardware's answer to a barrier the other half never reaches"]
    fn the_shapes_whose_halves_pass_different_barriers_finish_on_the_card() {
        let _one = super::super::valu_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
        let Ok(host) = Host::new(false) else {
            eprintln!("no SM 6.6 hardware device; not run");
            return;
        };
        for shape in shapes() {
            let native = lane_branch(shape.0, shape.1, 64, shape.2);
            for mode in [Mode::Classes, Mode::Off] {
                let (result, form, module) = lowered(&native, mode, None);
                let (Ok(shader), Some(module)) = (result, module) else { continue };
                let audit = module.audit_barriers();
                for below in [64u32, 48, 33, 32, 31, 16, 1, 0] {
                    for groups in [1u32, 4096] {
                        match run(&shader, &host, below, groups) {
                            Ok(Some(_)) => {}
                            Ok(None) => return,
                            Err(e) => panic!(
                                "{shape:?} {form:?} {mode:?}, {below} lanes, {groups} groups: {e} (audit: {})",
                                audit.describe()
                            ),
                        }
                    }
                }
                eprintln!("{shape:?} {form:?} {mode:?}: finished on the card (audit: {})", audit.describe());
            }
        }
    }
}
