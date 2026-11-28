//! [`super::GUARDS`]: the selects that keep a register's old value in lanes
//! nothing reads it in.
//!
//! The lowering writes a console register under a lane mask as
//! `new = op(...); reg = select(mask, new, old)` for every instruction, and a
//! join of two branches as a select on the lane's side. When every use of the
//! result only matters in lanes where the mask holds (it feeds another select
//! on the same mask, or on a mask that implies it, or an operation whose own
//! result matters only there), the old value is never looked at: the select
//! is its `new` operand, and the register's chain is one instruction shorter
//! per step (and its old value is no longer live across the chain).
//!
//! The analysis is a backward "must" fixpoint over the def-use graph. For each
//! value `v`, `implied(v)` is a set of `i1` instructions that hold in every
//! lane where `v`'s value can change what the shader does:
//!
//! - an effect (store, atomic, barrier, output, discard), a branch condition,
//!   a join, a lane or quad operation, a derivative, a descriptor index, or an
//!   address of shared memory: nothing is implied (the value matters in every
//!   lane that runs the instruction);
//! - an operation without such a use: what its result implies;
//! - the operand of `select(c, v, y)` that is taken when `c` holds: what the
//!   select's result implies, and `c` with the terms of it that are `and`s;
//! - the other operands of a select: what its result implies.
//!
//! A condition names an SSA value, so it only says something about a value
//! computed from the same evaluation of it: a condition counts across a
//! def-use edge only when the two ends are in one block, or when neither end
//! nor the condition's own block is in a cycle of the control flow (each runs
//! at most once). A select whose own condition is implied by its result is
//! replaced by its taken operand; it differs from the select only in lanes
//! where nothing reads it.
//!
//! The pass is exact in the sense the clean-up passes are: every lane that
//! matters computes what it did. It changes the values of lanes that are
//! masked off, which no later instruction of a lane that matters reads.

use super::{
    Bin, Effect, Module, Op, Term, V, for_each_operand, map_operands, resolve, successors,
};

/// Sweeps over the instructions before the analysis gives up (it only ever
/// shrinks sets, so it ends; this bounds a program that never settles).
const MAX_SWEEPS: usize = 48;
/// Conditions kept per value; fewer only weakens what is known.
const MAX_SET: usize = 48;
/// How far through `and`s a condition is followed for the terms it implies.
const AND_DEPTH: u32 = 6;

/// A sorted set of conditions (instruction numbers); `None` is "everything":
/// a value nothing live reads.
type Implied = Option<Vec<u32>>;

fn meet(a: Implied, b: Implied) -> Implied {
    match (a, b) {
        (None, x) | (x, None) => x,
        (Some(a), Some(b)) => {
            let mut out = Vec::with_capacity(a.len().min(b.len()));
            let (mut i, mut j) = (0, 0);
            while i < a.len() && j < b.len() {
                match a[i].cmp(&b[j]) {
                    std::cmp::Ordering::Less => i += 1,
                    std::cmp::Ordering::Greater => j += 1,
                    std::cmp::Ordering::Equal => {
                        out.push(a[i]);
                        i += 1;
                        j += 1;
                    }
                }
            }
            Some(out)
        }
    }
}

/// What the module's function numbers stand for in the analysis.
struct Calls {
    effects: Vec<Effect>,
    /// Calls whose operands every lane needs although they are no effect:
    /// descriptor handles (an index no lane may get wrong) and the samples
    /// that take their level from the neighbouring lanes.
    strict: Vec<bool>,
    /// `dx.op.unary`: its opcode is a derivative (reads the neighbours).
    unary: Vec<bool>,
}

impl Module {
    /// [`super::GUARDS`]. Returns whether any select was replaced.
    pub(super) fn drop_guards(&mut self) -> bool {
        let n = self.insts.len();
        if n == 0 || self.blocks.is_empty() {
            return false;
        }
        let reached = self.reached();
        // Where each instruction is, in program order (the order the
        // dominator tree gives the blocks, each block's joins first).
        let tree = self.dominator_tree();
        let mut block_of = vec![u32::MAX; n];
        let mut order: Vec<u32> = Vec::with_capacity(n);
        for &b in &tree.order {
            for &i in self.blocks[b].phis.iter().chain(&self.blocks[b].body) {
                block_of[i as usize] = b as u32;
                order.push(i);
            }
        }
        let cyclic = self.cyclic_blocks(&reached);
        let calls = Calls {
            effects: self.effects(),
            strict: self
                .funcs
                .iter()
                .map(|f| {
                    let name = f.name.strip_prefix("dx.op.").unwrap_or(&f.name);
                    let base = name.split('.').next().unwrap_or(name);
                    base.starts_with("createHandle")
                        || matches!(
                            base,
                            "sample" | "sampleBias" | "sampleCmp" | "sampleCmpBias" | "calcLOD"
                        )
                })
                .collect(),
            unary: self
                .funcs
                .iter()
                .map(|f| {
                    let name = f.name.strip_prefix("dx.op.").unwrap_or(&f.name);
                    name.split('.').next() == Some("unary")
                })
                .collect(),
        };
        // The users of each value: the instructions of reached blocks that
        // read it, once each; and the values a branch reads.
        let mut count = vec![0u32; n + 1];
        let mut pinned = vec![false; n];
        let each_use = |m: &Module, f: &mut dyn FnMut(u32, u32)| {
            for &i in &order {
                let mut last = u32::MAX;
                for_each_operand(&m.insts[i as usize].op, |v| {
                    if let V::Inst(d) = v
                        && d != i
                        && last != d
                    {
                        last = d;
                        f(d, i);
                    }
                });
            }
        };
        each_use(self, &mut |d, _| count[d as usize] += 1);
        for &b in &tree.order {
            if let Some(Term::CondBr(V::Inst(c), ..)) = self.blocks[b].term {
                pinned[c as usize] = true;
            }
        }
        let mut first = vec![0u32; n + 1];
        for d in 0..n {
            first[d + 1] = first[d] + count[d];
        }
        let mut fill = first.clone();
        let mut users = vec![0u32; first[n] as usize];
        each_use(self, &mut |d, u| {
            users[fill[d as usize] as usize] = u;
            fill[d as usize] += 1;
        });
        let users_of = |d: u32| &users[first[d as usize] as usize..first[d as usize + 1] as usize];
        let valid = |c: u32, bd: u32, bu: u32| {
            let bc = block_of[c as usize];
            bc != u32::MAX
                && ((bc == bd && bd == bu)
                    || (!cyclic[bc as usize] && !cyclic[bd as usize] && !cyclic[bu as usize]))
        };
        let mut implied: Vec<Implied> = vec![None; n];
        let mut settled = false;
        for _ in 0..MAX_SWEEPS {
            let mut changed = false;
            for &d in order.iter().rev() {
                let bd = block_of[d as usize];
                let mut now: Implied = if pinned[d as usize] {
                    Some(Vec::new())
                } else {
                    None
                };
                for &u in users_of(d) {
                    let bu = block_of[u as usize];
                    let through = |implied: &Implied| -> Implied {
                        implied.as_ref().map(|s| {
                            s.iter()
                                .copied()
                                .filter(|&x| valid(x, bd, bu))
                                .collect::<Vec<_>>()
                        })
                    };
                    let here = match &self.insts[u as usize].op {
                        Op::Phi(_)
                        | Op::Store(..)
                        | Op::AtomicRmw(..)
                        | Op::CmpXchg(..)
                        | Op::Load(_) => Some(Vec::new()),
                        Op::Bin(..)
                        | Op::Cast(..)
                        | Op::Cmp(..)
                        | Op::Extract(..)
                        | Op::Gep(..) => through(&implied[u as usize]),
                        Op::Select(c, t, f) => {
                            let mut s: Implied = None;
                            if *c == V::Inst(d) || *f == V::Inst(d) {
                                s = through(&implied[u as usize]);
                            }
                            if *t == V::Inst(d) {
                                let mut taken = match through(&implied[u as usize]) {
                                    Some(s) => s,
                                    // (nothing live reads the select: nor this)
                                    None => Vec::new(),
                                };
                                let live = implied[u as usize].is_some();
                                if live {
                                    let mut terms = Vec::new();
                                    self.condition_terms(*c, &mut terms, AND_DEPTH);
                                    taken.extend(terms.into_iter().filter(|&x| valid(x, bd, bu)));
                                    taken.sort_unstable();
                                    taken.dedup();
                                    taken.truncate(MAX_SET);
                                    s = meet(s, Some(taken));
                                }
                            }
                            s
                        }
                        Op::Call(f, args) => {
                            let f = *f as usize;
                            let derivative = calls.unary[f]
                                && args
                                    .first()
                                    .and_then(|&a| self.int_k(a))
                                    .is_some_and(|(_, k)| (83..=86).contains(&k));
                            if matches!(calls.effects[f], Effect::Lanes | Effect::Writes)
                                || calls.strict[f]
                                || derivative
                            {
                                Some(Vec::new())
                            } else {
                                through(&implied[u as usize])
                            }
                        }
                    };
                    now = meet(now, here);
                }
                if now != implied[d as usize] {
                    implied[d as usize] = now;
                    changed = true;
                }
            }
            if !changed {
                settled = true;
                break;
            }
        }
        if !settled {
            return false;
        }
        // A select whose result matters only where its own condition holds
        // is the operand it takes there.
        let mut repl: Vec<Option<V>> = vec![None; n];
        let mut any = false;
        for &s in &order {
            let Op::Select(V::Inst(c), t, _) = self.insts[s as usize].op else {
                continue;
            };
            let Some(set) = &implied[s as usize] else {
                continue;
            };
            if t != V::Inst(s) && set.binary_search(&c).is_ok() {
                repl[s as usize] = Some(t);
                any = true;
            }
        }
        if !any {
            return false;
        }
        for blk in &mut self.blocks {
            blk.body.retain(|&i| repl[i as usize].is_none());
            if let Some(Term::CondBr(c, t, f)) = blk.term {
                blk.term = Some(Term::CondBr(resolve(&repl, c), t, f));
            }
        }
        let (blocks, insts) = (&self.blocks, &mut self.insts);
        for blk in blocks {
            for &i in blk.phis.iter().chain(&blk.body) {
                map_operands(&mut insts[i as usize].op, |v| resolve(&repl, v));
            }
        }
        true
    }

    /// The conditions that hold wherever the `i1` `c` does: itself and, of an
    /// `and` (or a select that is one), the terms of it.
    fn condition_terms(&self, c: V, out: &mut Vec<u32>, depth: u32) {
        let V::Inst(i) = c else { return };
        if out.contains(&i) {
            return;
        }
        out.push(i);
        if depth == 0 {
            return;
        }
        match self.insts[i as usize].op {
            Op::Bin(Bin::And, a, b) => {
                self.condition_terms(a, out, depth - 1);
                self.condition_terms(b, out, depth - 1);
            }
            Op::Select(a, b, f) if self.int_k(f) == Some((1, 0)) => {
                self.condition_terms(a, out, depth - 1);
                self.condition_terms(b, out, depth - 1);
            }
            _ => {}
        }
    }

    /// Per block: whether it lies on a cycle of the control flow (a loop, or a
    /// block that branches to itself).
    fn cyclic_blocks(&self, reached: &[bool]) -> Vec<bool> {
        let n = self.blocks.len();
        let mut cyclic = vec![false; n];
        // Tarjan's components, without recursion.
        let (mut index, mut low) = (vec![u32::MAX; n], vec![0u32; n]);
        let (mut on, mut stack) = (vec![false; n], Vec::new());
        let mut next = 0u32;
        for root in (0..n).filter(|&b| reached[b]) {
            if index[root] != u32::MAX {
                continue;
            }
            let mut frames: Vec<(usize, usize)> = vec![(root, 0)];
            index[root] = next;
            low[root] = next;
            next += 1;
            stack.push(root);
            on[root] = true;
            while let Some(&(b, k)) = frames.last() {
                let succ = successors(self.blocks[b].term);
                if let Some(&s) = succ.get(k) {
                    frames.last_mut().expect("a frame").1 += 1;
                    if s == b {
                        cyclic[b] = true;
                    }
                    if index[s] == u32::MAX {
                        index[s] = next;
                        low[s] = next;
                        next += 1;
                        stack.push(s);
                        on[s] = true;
                        frames.push((s, 0));
                    } else if on[s] {
                        low[b] = low[b].min(index[s]);
                    }
                } else {
                    frames.pop();
                    if let Some(&(parent, _)) = frames.last() {
                        low[parent] = low[parent].min(low[b]);
                    }
                    if low[b] == index[b] {
                        let mut part = Vec::new();
                        while let Some(w) = stack.pop() {
                            on[w] = false;
                            part.push(w);
                            if w == b {
                                break;
                            }
                        }
                        if part.len() > 1 {
                            for w in part {
                                cyclic[w] = true;
                            }
                        }
                    }
                }
            }
        }
        cyclic
    }
}
