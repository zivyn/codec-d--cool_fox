//! Which group barriers of a finished module can be reached by some threads of the group and not
//! by others: the barriers of a wave-wide form (two host waves of a console wave, and the other
//! console waves of the group) must be reached by every thread of the group, the same number of
//! times, or the group waits for threads that never come.
//!
//! A barrier is safe when every branch it depends on has the same answer in every thread of
//! the group. That is the uniformity analysis compilers run for GPUs: a value is
//!
//! - [`Level::Same`]: the same in every thread (constants, the group id, what the translation
//!   made the halves agree on by an exchange, [`Module::mark_together`]), or made of such values;
//! - [`Level::Loaded`]: made of values read from memory at an address every thread holds alike:
//!   the same in every thread unless something writes that memory while the halves read it;
//! - [`Level::Differs`]: anything else (a thread's own index, a lane's value, a wave's answer,
//!   what an atomic returned, shared memory a thread wrote).
//!
//! A phi joining two arms of a branch that [`Level::Differs`] differs; the blocks a differing
//! branch alone leads to (its region: reachable from it, not post-dominating it) are run by some
//! threads only, and a loop it leaves is run a different number of times by each thread.
//! A barrier in such a region is a finding.

use super::{Module, Op, Term, V};

/// How much a value can differ between the threads of a group.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(crate) enum Level {
    Same,
    Loaded,
    Differs,
}

/// A group barrier that some branch can keep a thread of the group from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Finding {
    /// The block with the barrier.
    pub barrier: usize,
    /// The block of the branch that decides who gets there.
    pub branch: usize,
    /// What makes the branch's condition differ (or be loaded).
    pub why: String,
}

/// What [`Module::audit_barriers`] found.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Audit {
    /// The barriers that wait for the group.
    pub barriers: usize,
    /// The waits of the two host waves of a console wave for each other, by counters of their own
    /// ([`Module::mark_wait`]); they are audited for the two halves, not for the group.
    pub waits: usize,
    /// The loops with such a barrier in them, and how deep they nest (a loop that runs to its
    /// limit costs the limit's number of barriers; in a loop in a loop, its square).
    pub loops: usize,
    pub nest: usize,
    /// The branches to a loop's limit exit (a thread that reaches the limit leaves the program):
    /// left out of the control flow the audit follows, since a loop that reaches its limit has
    /// stopped doing what the program does. Without the limit's exits every barrier below is
    /// reached by every thread; a thread cut at the limit leaves before the barriers after it, as
    /// it does in every form of the program that has loops (the hardware counts a thread that
    /// has left as arrived).
    pub cuts: usize,
    /// Barriers in the region of a branch whose condition [`Level::Differs`].
    pub differs: Vec<Finding>,
    /// Barriers in the region of a branch on a value read from memory ([`Level::Loaded`]).
    pub loaded: Vec<Finding>,
}

impl Audit {
    /// Whether every barrier is reached alike by every thread, taking memory read at the same
    /// address by all threads as alike (`loads_alike`) or not.
    pub fn safe(&self, loads_alike: bool) -> bool {
        self.differs.is_empty() && (loads_alike || self.loaded.is_empty())
    }

    /// One line for the corpus runs and the pilot.
    pub fn describe(&self) -> String {
        let list = |v: &[Finding]| {
            let mut parts: Vec<String> = v.iter().map(|f| format!("B{}<-B{}:{}", f.barrier, f.branch, f.why)).collect();
            parts.dedup();
            parts.truncate(6);
            parts.join(",")
        };
        let pair = |v: &[Finding]| v.iter().filter(|f| f.why.starts_with("(pair wait)")).count();
        format!(
            "barriers={} waits={} cuts={} (pair differs {}, loaded {}) loops={} nest={} differs={} [{}] loaded={} [{}]",
            self.barriers,
            self.waits,
            self.cuts,
            pair(&self.differs),
            pair(&self.loaded),
            self.loops,
            self.nest,
            self.differs.len(),
            list(&self.differs),
            self.loaded.len(),
            list(&self.loaded)
        )
    }
}

/// How a `dx.op` call's result depends on the thread.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Rule {
    /// A function of its arguments.
    Pure,
    /// Reads memory: of its arguments, at most `Loaded`.
    Memory,
    /// The group's, the same in every thread.
    Same,
    /// A thread's or a wave's own.
    Differs,
}

fn rule(name: &str) -> Rule {
    let n = name.strip_prefix("dx.op.").unwrap_or(name);
    let n = n.split('.').next().unwrap_or(n);
    match n {
        "cbufferLoad" | "cbufferLoadLegacy" => Rule::Pure,
        "groupId" => Rule::Same,
        _ if n.starts_with("wave")
            || n.starts_with("quad")
            || n.starts_with("thread")
            || n.starts_with("flattened")
            || n.starts_with("atomic")
            || n.starts_with("load")
            || n.starts_with("dispatch")
            || n.starts_with("coverage")
            || n.starts_with("sampleIndex")
            || n.starts_with("deriv")
            || n.starts_with("barycentrics") =>
        {
            Rule::Differs
        }
        _ if n.contains("Load") || n.starts_with("sample") || n.starts_with("texture") || n.starts_with("gather") => {
            Rule::Memory
        }
        _ => Rule::Pure,
    }
}

impl Module {
    /// Marks `v` as the same in every thread of the group: the translation made the halves agree
    /// on it (a decision both exchanged and read after a meeting).
    pub fn mark_together(&mut self, v: V) {
        if let V::Inst(i) = v {
            self.together.insert(i);
        }
    }

    /// Marks `v` as the same in the two host waves of a console wave (what they read after a
    /// meeting), though not in the other console waves of the group.
    pub fn mark_paired(&mut self, v: V) {
        if let V::Inst(i) = v {
            self.paired.insert(i);
        }
    }

    /// Marks block `b` as a wait of the two host waves of a console wave for each other.
    pub fn mark_wait(&mut self, b: usize) {
        self.waits.push(b);
    }

    /// Marks block `b` as the one a thread stopped by a loop's iteration limit leaves through.
    pub fn mark_abort(&mut self, b: usize) {
        self.aborts.push(b);
    }

    /// The names of the functions a call of `f` goes to.
    fn func_name(&self, f: u32) -> &str {
        &self.funcs[f as usize].name
    }

    /// The group barriers of the module and where they stand against the branches that can
    /// keep threads of the group from them.
    pub(crate) fn audit_barriers(&self) -> Audit {
        let mut audit = self.audit_with(false);
        if !self.waits.is_empty() {
            let pair = self.audit_with(true);
            audit.waits = pair.barriers;
            let tell = |f: Finding| Finding { why: format!("(pair wait) {}", f.why), ..f };
            audit.differs.extend(pair.differs.into_iter().map(tell));
            audit.loaded.extend(pair.loaded.into_iter().map(tell));
            audit.cuts = audit.cuts.max(pair.cuts);
            audit.loops = audit.loops.max(pair.loops);
            audit.nest = audit.nest.max(pair.nest);
        }
        audit
    }

    /// The audit of the barriers that wait for the group (`pair` false), or of the waits of the
    /// two host waves of a console wave for each other (what the halves read after a meeting is
    /// then alike, as it is in one console wave).
    fn audit_with(&self, pair: bool) -> Audit {
        let same = |i: u32| self.together.contains(&i) || pair && self.paired.contains(&i);
        let n = self.blocks.len();
        let mut audit = Audit::default();
        // the blocks reached from the entry, and their predecessors
        let mut seen = vec![false; n];
        let mut stack = vec![0usize];
        if n == 0 {
            return audit;
        }
        seen[0] = true;
        let mut succ: Vec<Vec<usize>> = vec![Vec::new(); n];
        let is_abort = |b: usize| self.aborts.contains(&b);
        while let Some(b) = stack.pop() {
            for &s in super::successors(self.blocks[b].term).iter() {
                // (the exit of a thread cut by a loop's limit is no flow of the program)
                if is_abort(s) {
                    audit.cuts += 1;
                    continue;
                }
                succ[b].push(s);
                if !seen[s] {
                    seen[s] = true;
                    stack.push(s);
                }
            }
        }
        // the barriers that wait for the group (mode bit 0), or the waits of the pairs
        let mut sync: Vec<usize> = Vec::new();
        if pair {
            sync.extend(self.waits.iter().copied().filter(|&b| seen[b]));
        } else {
            for (b, blk) in self.blocks.iter().enumerate().filter(|(b, _)| seen[*b]) {
                for &i in &blk.body {
                    if let Op::Call(f, args) = &self.insts[i as usize].op
                        && self.func_name(*f) == "dx.op.barrier"
                        && args.get(1).and_then(|&m| self.int_value(m)).is_some_and(|m| m & 1 != 0)
                    {
                        sync.push(b);
                    }
                }
            }
        }
        audit.barriers = sync.len();
        if sync.is_empty() {
            return audit;
        }
        let pdom = post_dominators(&succ, &seen);
        let order = self.reverse_postorder(&succ, &seen);
        // loops: header and body of each back edge's loop
        let mut rpo = vec![usize::MAX; n];
        for (i, &b) in order.iter().enumerate() {
            rpo[b] = i;
        }
        let mut preds: Vec<Vec<usize>> = vec![Vec::new(); n];
        for b in 0..n {
            for &s in &succ[b] {
                preds[s].push(b);
            }
        }
        let mut loops: Vec<Vec<bool>> = Vec::new();
        let mut headers: Vec<usize> = Vec::new();
        for &b in &order {
            for &s in &succ[b] {
                if rpo[s] <= rpo[b] {
                    // back edge b -> s: the body is what reaches b without passing s (the back
                    // edges of one header make one loop)
                    let mut body = vec![false; n];
                    body[s] = true;
                    let mut work = vec![b];
                    while let Some(x) = work.pop() {
                        if !body[x] {
                            body[x] = true;
                            work.extend(preds[x].iter().copied().filter(|&q| seen[q]));
                        }
                    }
                    match headers.iter().position(|&h| h == s) {
                        Some(k) => {
                            for x in 0..n {
                                loops[k][x] |= body[x];
                            }
                        }
                        None => {
                            headers.push(s);
                            loops.push(body);
                        }
                    }
                }
            }
        }
        let with_barrier: Vec<usize> =
            (0..loops.len()).filter(|&k| sync.iter().any(|&b| loops[k][b])).collect();
        audit.loops = with_barrier.len();
        audit.nest = with_barrier
            .iter()
            .map(|&k| {
                with_barrier.iter().filter(|&&j| (0..n).all(|b| !loops[k][b] || loops[j][b])).count()
            })
            .max()
            .unwrap_or(0);
        let words = n.div_ceil(64);
        let in_set = |set: &[u64], b: usize| set[b / 64] >> (b % 64) & 1 == 1;
        // levels, to a fixpoint (they only go up)
        let mut level = vec![Level::Same; self.insts.len()];
        let mut branch_level = vec![Level::Same; n];
        // blocks only some threads run, and the blocks where the arms of a branch meet
        let mut region = vec![[false; 2]; n];
        let mut join = vec![[false; 2]; n];
        let mut loop_differs = vec![[false; 2]; loops.len()];
        let level_of = |level: &[Level], v: V| match v {
            V::Inst(i) => level[i as usize],
            _ => Level::Same,
        };
        let mut regions: Vec<Option<Vec<bool>>> = vec![None; n];
        let mut changed = true;
        while changed {
            changed = false;
            for b in 0..n {
                if !seen[b] {
                    continue;
                }
                let blk = &self.blocks[b];
                for &i in blk.phis.iter().chain(&blk.body) {
                    let inst = &self.insts[i as usize];
                    let mut l = if same(i) {
                        Level::Same
                    } else {
                        match &inst.op {
                            Op::Bin(_, a, c) | Op::Cmp(_, a, c) => level_of(&level, *a).max(level_of(&level, *c)),
                            Op::Cast(_, a) | Op::Extract(a, _) => level_of(&level, *a),
                            Op::Select(a, c, d) => level_of(&level, *a).max(level_of(&level, *c)).max(level_of(&level, *d)),
                            Op::Gep(_, p, idx) => idx.iter().fold(level_of(&level, *p), |m, x| m.max(level_of(&level, *x))),
                            Op::Phi(inc) => {
                                let mut m = inc.iter().fold(Level::Same, |m, x| m.max(level_of(&level, x.0)));
                                // (joins after a branch that differs, and values of a loop that
                                // some threads left earlier)
                                if join[b][0] || region[b][0] {
                                    m = Level::Differs;
                                } else if join[b][1] || region[b][1] {
                                    m = m.max(Level::Loaded);
                                }
                                m
                            }
                            // shared memory: what threads wrote, unless it is an exchange
                            Op::Load(_) | Op::AtomicRmw(..) | Op::CmpXchg(..) => Level::Differs,
                            Op::Store(..) => Level::Same,
                            Op::Call(f, args) => {
                                let args = || args.iter().skip(1).fold(Level::Same, |m, x| m.max(level_of(&level, *x)));
                                match rule(self.func_name(*f)) {
                                    Rule::Pure => args(),
                                    Rule::Same => Level::Same,
                                    Rule::Memory => match args() {
                                        Level::Same | Level::Loaded => Level::Loaded,
                                        Level::Differs => Level::Differs,
                                    },
                                    Rule::Differs => Level::Differs,
                                }
                            }
                        }
                    };
                    // a value made inside a loop some threads left earlier than others
                    if matches!(inst.op, Op::Phi(_)) || !same(i) {
                        for (k, body) in loops.iter().enumerate() {
                            if body[b] {
                                if loop_differs[k][0] {
                                    l = Level::Differs;
                                } else if loop_differs[k][1] {
                                    l = l.max(Level::Loaded);
                                }
                            }
                        }
                    }
                    if l > level[i as usize] {
                        level[i as usize] = l;
                        changed = true;
                    }
                }
                if let Some(Term::CondBr(c, t, f)) = blk.term {
                    // (a loop's limit check: the back edge, or out of the program)
                    if is_abort(t) || is_abort(f) {
                        continue;
                    }
                    let l = level_of(&level, c);
                    if l > branch_level[b] {
                        branch_level[b] = l;
                        changed = true;
                    }
                    if l > Level::Same && t != f {
                        // (slot 0 marks what differs, slot 1 what is loaded)
                        let k = usize::from(l == Level::Loaded);
                        if regions[b].is_none() {
                            // the blocks reachable from the branch that do not post-dominate it
                            let mut reg = vec![false; n];
                            let mut work: Vec<usize> = succ[b].clone();
                            while let Some(x) = work.pop() {
                                if reg[x] || in_set(&pdom[b * words..(b + 1) * words], x) {
                                    continue;
                                }
                                reg[x] = true;
                                work.extend(succ[x].iter().copied());
                            }
                            regions[b] = Some(reg);
                        }
                        let reg = regions[b].as_ref().expect("made above");
                        for x in 0..n {
                            if reg[x] && !region[x][k] {
                                region[x][k] = true;
                                changed = true;
                            }
                        }
                        // the joins: blocks outside the region that the region leads to
                        for x in 0..n {
                            if reg[x] {
                                for &s in &succ[x] {
                                    if !reg[s] && seen[s] && !join[s][k] {
                                        join[s][k] = true;
                                        changed = true;
                                    }
                                }
                            }
                        }
                        // the loops it leaves
                        for (kl, body) in loops.iter().enumerate() {
                            if body[b] && (!body[t] || !body[f]) && !loop_differs[kl][k] {
                                loop_differs[kl][k] = true;
                                changed = true;
                            }
                        }
                    }
                }
            }
        }
        // each barrier against the branches whose regions hold it
        for &b in &sync {
            for d in 0..n {
                let Some(reg) = &regions[d] else { continue };
                if !reg[b] {
                    continue;
                }
                let why = self.root_cause(&level, d, &same);
                let finding = Finding { barrier: b, branch: d, why };
                if branch_level[d] == Level::Differs {
                    audit.differs.push(finding);
                } else {
                    audit.loaded.push(finding);
                }
            }
            // a loop whose exit some threads take earlier runs the barrier a different
            // number of times
            for (k, body) in loops.iter().enumerate() {
                if body[b] && loop_differs[k][0] && !audit.differs.iter().any(|f| f.barrier == b) {
                    audit.differs.push(Finding { barrier: b, branch: 0, why: "a loop some threads leave earlier".into() });
                }
            }
        }
        audit
    }

    /// What makes the condition of block `d`'s branch differ (a short name).
    fn root_cause(&self, level: &[Level], d: usize, same: &dyn Fn(u32) -> bool) -> String {
        let Some(Term::CondBr(c, ..)) = self.blocks[d].term else { return "?".into() };
        let mut v = c;
        let mut guard = 0;
        loop {
            guard += 1;
            let V::Inst(i) = v else { return "a constant".into() };
            let at = level[i as usize];
            let inst = &self.insts[i as usize];
            let operands: Vec<V> = match &inst.op {
                Op::Bin(_, a, b) | Op::Cmp(_, a, b) => vec![*a, *b],
                Op::Cast(_, a) | Op::Extract(a, _) => vec![*a],
                Op::Select(a, b, c) => vec![*a, *b, *c],
                Op::Gep(_, p, idx) => std::iter::once(*p).chain(idx.iter().copied()).collect(),
                Op::Phi(inc) => inc.iter().map(|x| x.0).collect(),
                Op::Load(_) => return "shared memory read".into(),
                Op::AtomicRmw(..) | Op::CmpXchg(..) => return "an atomic".into(),
                Op::Store(..) => return "?".into(),
                Op::Call(f, args) => match rule(self.func_name(*f)) {
                    Rule::Pure | Rule::Memory => args.iter().skip(1).copied().collect(),
                    _ => {
                        let name = self.func_name(*f);
                        return name.strip_prefix("dx.op.").unwrap_or(name).to_string();
                    }
                },
            };
            let next = operands
                .iter()
                .copied()
                .find(|&o| matches!(o, V::Inst(j) if level[j as usize] == at && !same(j)));
            match next {
                Some(o) if guard < 200 => v = o,
                _ => {
                    return match &inst.op {
                        Op::Phi(_) => "a join after a branch that differs".into(),
                        Op::Call(f, _) => self.func_name(*f).trim_start_matches("dx.op.").to_string(),
                        _ => "an operand".into(),
                    };
                }
            }
        }
    }

    fn reverse_postorder(&self, succ: &[Vec<usize>], seen: &[bool]) -> Vec<usize> {
        let n = succ.len();
        let mut order = Vec::new();
        let mut state = vec![0u8; n];
        let mut stack: Vec<(usize, usize)> = vec![(0, 0)];
        state[0] = 1;
        while let Some(&mut (b, ref mut k)) = stack.last_mut() {
            if *k < succ[b].len() {
                let s = succ[b][*k];
                *k += 1;
                if seen[s] && state[s] == 0 {
                    state[s] = 1;
                    stack.push((s, 0));
                }
            } else {
                order.push(b);
                stack.pop();
            }
        }
        order.reverse();
        order
    }
}

/// Post-dominator sets: row `b` of the result (`words` dwords each) has bit `x` set when `x`
/// post-dominates `b` (a virtual exit after every block that ends).
fn post_dominators(succ: &[Vec<usize>], seen: &[bool]) -> Vec<u64> {
    let n = succ.len();
    let words = n.div_ceil(64);
    let mut pdom = vec![u64::MAX; n * words];
    for b in 0..n {
        if seen[b] && succ[b].is_empty() {
            let row = &mut pdom[b * words..(b + 1) * words];
            row.fill(0);
            row[b / 64] |= 1 << (b % 64);
        }
    }
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if !seen[b] || succ[b].is_empty() {
                continue;
            }
            let mut v = vec![u64::MAX; words];
            for &t in &succ[b] {
                for w in 0..words {
                    v[w] &= pdom[t * words + w];
                }
            }
            v[b / 64] |= 1 << (b % 64);
            if v[..] != pdom[b * words..(b + 1) * words] {
                pdom[b * words..(b + 1) * words].copy_from_slice(&v);
                changed = true;
            }
        }
    }
    pdom
}

impl Module {
    /// The module's control flow and instructions as text (a diagnostic, `VICTORY_HANG_AUDIT=2`):
    /// one line per instruction, `%n` for the instruction numbered `n`, `T` after the ones the
    /// translation marked as the same in every thread.
    pub(crate) fn listing(&self) -> String {
        use std::fmt::Write;
        let name = |v: V| -> String {
            match v {
                V::Inst(i) => format!("%{i}"),
                V::Const(c) => match &self.consts[c as usize].1 {
                    super::Const::Int(x) => format!("{x}"),
                    super::Const::Float(b) => format!("f{b:#x}"),
                    super::Const::Undef => "undef".into(),
                    super::Const::Aggregate(_) => "{..}".into(),
                },
                V::Global(g) => format!("@{}", self.globals[g as usize].name),
                V::Func(f) => format!("f{f}"),
            }
        };
        let list = |vs: &[V]| vs.iter().map(|&v| name(v)).collect::<Vec<_>>().join(", ");
        let n = self.blocks.len();
        let mut seen = vec![false; n];
        let mut stack = vec![0usize];
        if n > 0 {
            seen[0] = true;
        }
        while let Some(b) = stack.pop() {
            for &s in super::successors(self.blocks[b].term).iter() {
                if !seen[s] {
                    seen[s] = true;
                    stack.push(s);
                }
            }
        }
        let mut out = String::new();
        for b in (0..n).filter(|&b| seen[b]) {
            let blk = &self.blocks[b];
            let _ = writeln!(out, "B{b}:");
            for &i in blk.phis.iter().chain(&blk.body) {
                let inst = &self.insts[i as usize];
                let text = match &inst.op {
                    Op::Bin(op, a, c) => format!("{op:?} {}, {}", name(*a), name(*c)),
                    Op::Cast(op, a) => format!("{op:?} {}", name(*a)),
                    Op::Cmp(p, a, c) => format!("cmp{p} {}, {}", name(*a), name(*c)),
                    Op::Select(c, a, d) => format!("select {}, {}, {}", name(*c), name(*a), name(*d)),
                    Op::Extract(a, k) => format!("extract {}, {k}", name(*a)),
                    Op::Call(f, args) => format!("call {}({})", self.func_name(*f).trim_start_matches("dx.op."), list(args)),
                    Op::Phi(inc) => format!(
                        "phi {}",
                        inc.iter().map(|(v, b)| format!("[{}, B{b}]", name(*v))).collect::<Vec<_>>().join(", ")
                    ),
                    Op::Gep(_, p, idx) => format!("gep {}[{}]", name(*p), list(idx)),
                    Op::Load(p) => format!("load {}", name(*p)),
                    Op::Store(p, v) => format!("store {}, {}", name(*p), name(*v)),
                    Op::AtomicRmw(op, p, v) => format!("atomic {op:?} {}, {}", name(*p), name(*v)),
                    Op::CmpXchg(p, a, c) => format!("cmpxchg {}, {}, {}", name(*p), name(*a), name(*c)),
                };
                let t = if self.together.contains(&i) { " T" } else { "" };
                let _ = writeln!(out, "  %{i} = {text}{t}");
            }
            let _ = match blk.term {
                Some(Term::Br(t)) => writeln!(out, "  br B{t}"),
                Some(Term::CondBr(c, t, f)) => writeln!(out, "  br {}, B{t}, B{f}", name(c)),
                _ => writeln!(out, "  ret"),
            };
        }
        out
    }
}
