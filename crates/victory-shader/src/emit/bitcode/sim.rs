//! A model of the threads of a group running a finished module's control flow, for the tests of
//! the group barriers: every thread is run on its own, a wave's operations (`waveAnyTrue`...) wait
//! until every live lane of the wave is at the same one, and a group barrier waits until every
//! live thread of the group is at the same one. The interleaving of the threads is a seeded
//! pseudo-random one, so that a shared-memory race shows (or the exchange that means to keep one
//! away is seen to work).
//!
//! What it checks, besides running: threads that wait at different barriers, a barrier that a
//! thread that left the program never reaches, waves whose lanes are at different wave
//! operations, a group that stops with nothing able to run (a hang). What it returns: the barriers
//! each thread passed, in order, and the stores it made.
//!
//! Only the operations the translation's own synthetic programs use are modelled; the first one
//! that is not ends the run with an error that names it.

use std::collections::HashMap;

use super::{Bin, Cast, Const, Module, Op, Rmw, Term, TypeDef, V, pred};

/// What a thread holds in a value.
#[derive(Clone, Debug)]
enum Val {
    I(u64),
    Agg(Vec<u64>),
    /// An element of a global array.
    Ptr(u32, u64),
}

/// A wave operation a thread waits at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WaveOp {
    Any,
    All,
    First,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum State {
    Run,
    Wave(u32, WaveOp, u64),
    Barrier(u32),
    Done,
}

struct Thread {
    block: usize,
    ip: usize,
    prev: usize,
    vals: Vec<Option<Val>>,
    state: State,
    /// The block's phis are made.
    entered: bool,
    barriers: Vec<u32>,
    /// A wave operation's answer, to be taken by the instruction that asked.
    answer: Option<u64>,
}

/// What a run found.
#[derive(Debug, Default)]
pub(crate) struct Report {
    /// The barriers (the instruction numbers) each thread passed, in order.
    pub barriers: Vec<Vec<u32>>,
    /// Everything that is wrong: mismatched barriers, lanes of a wave at different wave
    /// operations, threads that left before a barrier the others reached, a hang.
    pub problems: Vec<String>,
    /// The words the threads stored to buffers, by byte offset.
    pub stores: HashMap<u64, u64>,
    /// The notes the threads left in the faults buffer (the root UAV 253), in order (a thread's
    /// note that is not a note is the sink's: the faults buffer's dword 0).
    pub notes: Vec<u64>,
}

impl Report {
    /// Whether every thread of the group passed the same barriers in the same order.
    pub fn alike(&self) -> bool {
        self.barriers.windows(2).all(|w| w[0] == w[1])
    }
}

/// The group: `threads` threads in waves of 32, the root constants `user`.
pub(crate) fn run(m: &Module, threads: usize, user: &[u32], seed: u64) -> Result<Report, String> {
    run_with(m, threads, user, seed, None)
}

/// [`run`], where the threads `gone.1` leave the program (as if killed) once the group has passed
/// `gone.0` barriers.
pub(crate) fn run_with(
    m: &Module,
    threads: usize,
    user: &[u32],
    seed: u64,
    gone: Option<(usize, std::ops::Range<usize>)>,
) -> Result<Report, String> {
    let mut sim = Sim { m, user, mem: HashMap::new(), report: Report::default(), seed: seed | 1, kill: gone, passed: 0 };
    let mut ts: Vec<Thread> = (0..threads)
        .map(|_| Thread {
            block: 0,
            ip: 0,
            prev: usize::MAX,
            vals: vec![None; m.insts.len()],
            state: State::Run,
            entered: false,
            barriers: Vec::new(),
            answer: None,
        })
        .collect();
    let mut steps = 0u64;
    loop {
        let runnable: Vec<usize> = (0..threads).filter(|&t| ts[t].state == State::Run).collect();
        if runnable.is_empty() {
            if ts.iter().all(|t| t.state == State::Done) {
                break;
            }
            if !sim.resolve(&mut ts) {
                let at: Vec<State> = ts.iter().map(|t| t.state).take(8).collect();
                sim.report.problems.push(format!("a hang: nothing can run, the first threads are at {at:?}"));
                break;
            }
            continue;
        }
        let t = runnable[(sim.next() % runnable.len() as u64) as usize];
        for _ in 0..1 + sim.next() % 6 {
            if ts[t].state != State::Run {
                break;
            }
            sim.step(&mut ts, t)?;
            steps += 1;
        }
        sim.resolve(&mut ts);
        if steps > 20_000_000 {
            sim.report.problems.push("the threads ran 20 million steps (a loop that does not end)".into());
            break;
        }
    }
    sim.report.barriers = ts.into_iter().map(|t| t.barriers).collect();
    Ok(sim.report)
}

struct Sim<'a> {
    m: &'a Module,
    user: &'a [u32],
    mem: HashMap<u32, Vec<u64>>,
    report: Report,
    seed: u64,
    /// Threads that leave after a number of barriers, and the barriers the group has passed.
    kill: Option<(usize, std::ops::Range<usize>)>,
    passed: usize,
}

impl Sim<'_> {
    fn next(&mut self) -> u64 {
        self.seed ^= self.seed << 13;
        self.seed ^= self.seed >> 7;
        self.seed ^= self.seed << 17;
        self.seed >> 8
    }

    fn bits(&self, ty: super::Ty) -> u32 {
        match self.m.types[ty.0 as usize] {
            TypeDef::Int(b) => b,
            _ => 32,
        }
    }

    fn mask(&self, ty: super::Ty, v: u64) -> u64 {
        match self.bits(ty) {
            64 => v,
            b => v & ((1u64 << b) - 1),
        }
    }

    fn constant(&self, c: u32) -> Val {
        match &self.m.consts[c as usize].1 {
            Const::Int(x) => Val::I(self.mask(self.m.consts[c as usize].0, *x as u64)),
            Const::Float(b) => Val::I(u64::from(*b)),
            Const::Undef => Val::I(0),
            Const::Aggregate(l) => Val::Agg(
                l.iter()
                    .map(|&k| match self.constant(k) {
                        Val::I(x) => x,
                        _ => 0,
                    })
                    .collect(),
            ),
        }
    }

    fn val(&self, t: &Thread, v: V) -> Result<Val, String> {
        match v {
            V::Const(c) => Ok(self.constant(c)),
            V::Inst(i) => t.vals[i as usize].clone().ok_or_else(|| format!("value {i} read before it was made")),
            V::Global(g) => Ok(Val::Ptr(g, 0)),
            V::Func(_) => Ok(Val::I(0)),
        }
    }

    fn int(&self, t: &Thread, v: V) -> Result<u64, String> {
        match self.val(t, v)? {
            Val::I(x) => Ok(x),
            Val::Agg(a) => Ok(a.first().copied().unwrap_or(0)),
            Val::Ptr(..) => Err("a pointer used as a number".into()),
        }
    }

    fn global(&mut self, g: u32) -> &mut Vec<u64> {
        let n = match self.m.types[self.m.globals[g as usize].ty.0 as usize] {
            TypeDef::Array(n, _) => n as usize,
            _ => 1,
        };
        self.mem.entry(g).or_insert_with(|| vec![0; n])
    }

    fn name(&self, f: u32) -> String {
        self.m.funcs[f as usize].name.trim_start_matches("dx.op.").to_string()
    }

    /// One instruction (or phis, terminator) of thread `t`.
    fn step(&mut self, ts: &mut [Thread], t: usize) -> Result<(), String> {
        let m = self.m;
        let block = ts[t].block;
        // (phis at the block's entry, all from the values before)
        if !ts[t].entered {
            ts[t].entered = true;
            if ts[t].prev != usize::MAX {
                let mut made = Vec::new();
                for &i in &m.blocks[block].phis {
                    if let Op::Phi(inc) = &m.insts[i as usize].op {
                        let from = ts[t].prev;
                        let Some(&(v, _)) = inc.iter().find(|x| x.1 == from) else {
                            return Err(format!("phi {i} has no value from block {from}"));
                        };
                        made.push((i, self.val(&ts[t], v)?));
                    }
                }
                for (i, v) in made {
                    ts[t].vals[i as usize] = Some(v);
                }
            }
        }
        let blk = &m.blocks[block];
        if ts[t].ip < blk.body.len() {
            let i = blk.body[ts[t].ip];
            let inst = &m.insts[i as usize];
            let ty = inst.ty;
            let pending = ts[t].answer;
            let thread = &ts[t];
            let mut consumed = false;
            let mut park: Option<State> = None;
            let result: Option<Val> = match &inst.op {
                Op::Bin(op, a, b) => {
                    let (a, b) = (self.int(thread, *a)?, self.int(thread, *b)?);
                    if matches!(m.types[ty.0 as usize], TypeDef::Float) {
                        let (x, y) = (f32::from_bits(a as u32), f32::from_bits(b as u32));
                        let r = match op {
                            Bin::Add => x + y,
                            Bin::Sub => x - y,
                            Bin::Mul => x * y,
                            _ => return Err(format!("a float {op:?}")),
                        };
                        Some(Val::I(u64::from(r.to_bits())))
                    } else {
                        let bits = self.bits(ty);
                        let sign = |x: u64| if bits >= 64 { x as i64 } else { ((x << (64 - bits)) as i64) >> (64 - bits) };
                        let r = match op {
                            Bin::Add => a.wrapping_add(b),
                            Bin::Sub => a.wrapping_sub(b),
                            Bin::Mul => a.wrapping_mul(b),
                            Bin::UDiv => a.checked_div(b).unwrap_or(0),
                            Bin::URem => a.checked_rem(b).unwrap_or(0),
                            Bin::Shl => a.checked_shl(b as u32).unwrap_or(0),
                            Bin::LShr => a.checked_shr(b as u32).unwrap_or(0),
                            Bin::And => a & b,
                            Bin::Or => a | b,
                            Bin::Xor => a ^ b,
                            Bin::AShr => (sign(a) >> (b as u32).min(63)) as u64,
                            Bin::SDiv => sign(a).checked_div(sign(b)).unwrap_or(0) as u64,
                        };
                        Some(Val::I(self.mask(ty, r)))
                    }
                }
                Op::Cast(op, v) => {
                    let x = self.int(thread, *v)?;
                    let from = m.type_of(*v);
                    let r = match op {
                        Cast::Trunc | Cast::ZExt | Cast::Bitcast => x,
                        Cast::SExt => {
                            let b = self.bits(from);
                            (((x << (64 - b)) as i64) >> (64 - b)) as u64
                        }
                        _ => return Err(format!("a cast {op:?}")),
                    };
                    Some(Val::I(self.mask(ty, r)))
                }
                Op::Cmp(p, av, bv) => {
                    let (a, b) = (self.int(thread, *av)?, self.int(thread, *bv)?);
                    let bits = self.bits(m.type_of(*av));
                    let s = |x: u64| if bits >= 64 { x as i64 } else { ((x << (64 - bits)) as i64) >> (64 - bits) };
                    let r = match *p {
                        pred::EQ => a == b,
                        pred::NE => a != b,
                        pred::IUGT => a > b,
                        pred::IUGE => a >= b,
                        pred::IULT => a < b,
                        pred::IULE => a <= b,
                        pred::SGT => s(a) > s(b),
                        pred::SGE => s(a) >= s(b),
                        pred::SLT => s(a) < s(b),
                        pred::SLE => s(a) <= s(b),
                        other => return Err(format!("a compare {other}")),
                    };
                    Some(Val::I(u64::from(r)))
                }
                Op::Select(c, a, b) => {
                    let c = self.int(thread, *c)?;
                    Some(self.val(thread, if c & 1 != 0 { *a } else { *b })?)
                }
                Op::Extract(a, k) => match self.val(thread, *a)? {
                    Val::Agg(l) => Some(Val::I(l.get(*k as usize).copied().unwrap_or(0))),
                    v => Some(v),
                },
                Op::Gep(_, p, idx) => {
                    let Val::Ptr(g, _) = self.val(thread, *p)? else { return Err("a gep of a non-pointer".into()) };
                    let k = self.int(thread, *idx.last().ok_or("a gep without an index")?)?;
                    Some(Val::Ptr(g, k))
                }
                Op::Load(p) => {
                    let Val::Ptr(g, k) = self.val(thread, *p)? else { return Err("a load of a non-pointer".into()) };
                    Some(Val::I(self.global(g).get(k as usize).copied().unwrap_or(0)))
                }
                Op::Store(p, v) => {
                    let Val::Ptr(g, k) = self.val(thread, *p)? else { return Err("a store to a non-pointer".into()) };
                    let x = self.int(thread, *v)?;
                    if let Some(w) = self.global(g).get_mut(k as usize) {
                        *w = x;
                    }
                    None
                }
                Op::AtomicRmw(op, p, v) => {
                    let Val::Ptr(g, k) = self.val(thread, *p)? else { return Err("an atomic on a non-pointer".into()) };
                    let x = self.int(thread, *v)?;
                    let old = self.global(g).get_mut(k as usize).map(|w| {
                        let old = *w;
                        *w = match op {
                            Rmw::Xchg => x,
                            Rmw::Add => old.wrapping_add(x),
                            Rmw::Sub => old.wrapping_sub(x),
                            Rmw::And => old & x,
                            Rmw::Or => old | x,
                            Rmw::Xor => old ^ x,
                            Rmw::UMax => old.max(x),
                            Rmw::UMin => old.min(x),
                            Rmw::Max | Rmw::Min => old,
                        } & 0xFFFF_FFFF;
                        old
                    });
                    Some(Val::I(old.unwrap_or(0)))
                }
                Op::CmpXchg(..) => return Err("a compare-exchange".into()),
                Op::Phi(_) => None,
                Op::Call(f, args) => {
                    let name = self.name(*f);
                    let a: Vec<u64> = args.iter().map(|&v| self.int(thread, v).unwrap_or(0)).collect();
                    let flat = t as u64;
                    match name.as_str() {
                        "flattenedThreadIdInGroup.i32" => Some(Val::I(flat)),
                        "threadIdInGroup.i32" => Some(Val::I(if a[1] == 0 { flat } else { 0 })),
                        "groupId.i32" => Some(Val::I(0)),
                        "waveGetLaneIndex" => Some(Val::I(flat % 32)),
                        "waveAnyTrue" | "waveAllTrue" | "waveReadLaneFirst.i32" => {
                            let op = match name.as_str() {
                                "waveAnyTrue" => WaveOp::Any,
                                "waveAllTrue" => WaveOp::All,
                                _ => WaveOp::First,
                            };
                            match pending {
                                Some(r) => {
                                    consumed = true;
                                    Some(Val::I(r))
                                }
                                None => {
                                    park = Some(State::Wave(i, op, a[1]));
                                    None
                                }
                            }
                        }
                        "barrier" if a[1] & 1 != 0 => {
                            if pending.is_some() {
                                consumed = true;
                            } else {
                                park = Some(State::Barrier(i));
                            }
                            None
                        }
                        "barrier" => None,
                        // (a handle is the slot of its binding: the root UAV 253 is the faults buffer)
                        n if n.starts_with("createHandle") || n.starts_with("annotateHandle") => Some(Val::I(a[1])),
                        n if n.starts_with("cbufferLoadLegacy") => {
                            let row = a[2] as usize;
                            Some(Val::Agg((0..4).map(|k| u64::from(self.user.get(row * 4 + k).copied().unwrap_or(0))).collect()))
                        }
                        n if n.starts_with("rawBufferStore") => {
                            if a[1] == 253 {
                                self.report.notes.push(a[4]);
                            } else {
                                self.report.stores.insert(a[2], a[4]);
                            }
                            None
                        }
                        n if n.starts_with("rawBufferLoad") || n.starts_with("bufferLoad") => Some(Val::Agg(vec![0; 5])),
                        other => return Err(format!("unmodelled call {other}")),
                    }
                }
            };
            if consumed {
                ts[t].answer = None;
            }
            if let Some(state) = park {
                ts[t].state = state;
                return Ok(());
            }
            if let Some(v) = result {
                ts[t].vals[i as usize] = Some(v);
            }
            ts[t].ip += 1;
            return Ok(());
        }
        // the terminator
        match blk.term {
            Some(Term::Br(b)) => {
                ts[t].prev = block;
                ts[t].block = b;
                ts[t].ip = 0;
                ts[t].entered = false;
            }
            Some(Term::CondBr(c, a, b)) => {
                let c = self.int(&ts[t], c)? & 1;
                ts[t].prev = block;
                ts[t].block = if c != 0 { a } else { b };
                ts[t].ip = 0;
                ts[t].entered = false;
            }
            Some(Term::Ret) | None => ts[t].state = State::Done,
        }
        Ok(())
    }

    /// Lets waves and the group go on where every live thread has come to the same operation.
    fn resolve(&mut self, ts: &mut [Thread]) -> bool {
        let mut any = false;
        // waves
        for w in 0..ts.len().div_ceil(32) {
            let lanes: Vec<usize> = (w * 32..((w + 1) * 32).min(ts.len())).filter(|&t| ts[t].state != State::Done).collect();
            if lanes.is_empty() || !lanes.iter().all(|&t| matches!(ts[t].state, State::Wave(..))) {
                continue;
            }
            let State::Wave(site, op, _) = ts[lanes[0]].state else { continue };
            if !lanes.iter().all(|&t| matches!(ts[t].state, State::Wave(s, o, _) if s == site && o == op)) {
                self.report.problems.push(format!("lanes of wave {w} are at different wave operations"));
                // (let them through, with their own answer)
                for &t in &lanes {
                    if let State::Wave(_, _, a) = ts[t].state {
                        ts[t].answer = Some(a);
                        ts[t].state = State::Run;
                    }
                }
                any = true;
                continue;
            }
            let args: Vec<u64> = lanes
                .iter()
                .map(|&t| match ts[t].state {
                    State::Wave(_, _, a) => a,
                    _ => 0,
                })
                .collect();
            let answer = match op {
                WaveOp::Any => u64::from(args.iter().any(|&a| a & 1 != 0)),
                WaveOp::All => u64::from(args.iter().all(|&a| a & 1 != 0)),
                WaveOp::First => args[0],
            };
            for &t in &lanes {
                ts[t].answer = Some(answer);
                ts[t].state = State::Run;
            }
            any = true;
        }
        // the group
        let live: Vec<usize> = (0..ts.len()).filter(|&t| ts[t].state != State::Done).collect();
        if !live.is_empty() && live.iter().all(|&t| matches!(ts[t].state, State::Barrier(_))) {
            let State::Barrier(site) = ts[live[0]].state else { return any };
            if live.iter().any(|&t| ts[t].state != State::Barrier(site)) {
                self.report.problems.push("threads wait at different barriers".into());
            }
            if live.len() < ts.len() {
                self.report.problems.push(format!(
                    "{} threads left the program before a barrier the others reached",
                    ts.len() - live.len()
                ));
            }
            for &t in &live {
                if let State::Barrier(s) = ts[t].state {
                    ts[t].barriers.push(s);
                }
                ts[t].answer = Some(1);
                ts[t].state = State::Run;
            }
            any = true;
            self.passed += 1;
            if let Some((after, who)) = &self.kill
                && self.passed == *after
            {
                let n = ts.len();
                for t in who.clone().filter(|&t| t < n) {
                    ts[t].state = State::Done;
                }
            }
        }
        any
    }
}
