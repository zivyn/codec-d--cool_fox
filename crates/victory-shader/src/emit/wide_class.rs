//! Which of the branches a wave-wide form takes for the whole wave can hang the group.
//!
//! A branch taken wave-wide is one decision for the two host waves of a console wave, which they
//! exchange: both go the same way. That is not a decision for the thread group. A group barrier
//! under it (the program's own `s_barrier`, or the meetings of the host waves where they are group
//! barriers) is reached only by the waves that go in, and a group of several console waves whose
//! waves decide differently waits for waves that never come (device hung, 0x887A0006). So each
//! wide branch is one of
//!
//! - [`Class::A`]: no group barrier in the blocks it guards (the meetings of the host waves of a
//!   group of several console waves are counters of their own, `pair_wait`: only the two halves
//!   take part, and they agree);
//! - [`Class::B`]: group barriers inside, and the branch still reaches them in every wave of the
//!   group: the group is one console wave (the exchange of the two halves is the whole group's
//!   decision), or the condition is made of scalars that are the same in every wave (constants,
//!   user data, the group id);
//! - [`Class::C`]: group barriers inside, and the waves of the group may decide differently.

use std::collections::{BTreeSet, HashSet};

use crate::gfx10::Fields;
use crate::ir::{self, Program, Reg, Value};
use crate::native::{NativeProgram, StageIo};
use crate::structure::{Atom, LaneExit, Structured};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Class {
    A,
    B,
    C,
}

impl Class {
    pub fn letter(self) -> char {
        match self {
            Class::A => 'a',
            Class::B => 'b',
            Class::C => 'c',
        }
    }
}

/// The class of each branch of `wide` (by IR block) of `p`, where `barriers` are the IR blocks in
/// which the lowering put a group barrier.
pub(super) fn classify(
    p: &Program,
    st: &Structured,
    native: &NativeProgram,
    wide: &HashSet<usize>,
    barriers: &BTreeSet<usize>,
) -> Vec<(usize, Class)> {
    WHY.with_borrow_mut(Vec::clear);
    let n = p.blocks.len();
    let succ: Vec<Vec<usize>> = p.blocks.iter().map(|b| b.exit.successors()).collect();
    // post-dominators (bitsets; a virtual exit after every block that ends)
    let words = n.div_ceil(64);
    let full = vec![u64::MAX; words];
    let mut pdom: Vec<Vec<u64>> = (0..n)
        .map(|b| {
            if succ[b].is_empty() {
                let mut v = vec![0; words];
                v[b / 64] |= 1 << (b % 64);
                v
            } else {
                full.clone()
            }
        })
        .collect();
    let mut changed = true;
    while changed {
        changed = false;
        for b in (0..n).rev() {
            if succ[b].is_empty() {
                continue;
            }
            let mut v = full.clone();
            for &t in &succ[b] {
                for w in 0..words {
                    v[w] &= pdom[t][w];
                }
            }
            v[b / 64] |= 1 << (b % 64);
            if v != pdom[b] {
                pdom[b] = v;
                changed = true;
            }
        }
    }
    let threads = match native.io {
        StageIo::Cs(cs) => cs.threads.iter().product::<u32>(),
        _ => 0,
    };
    let same = scalars_of_every_wave(p, native);
    let mut out = Vec::new();
    let mut sorted: Vec<usize> = wide.iter().copied().collect();
    sorted.sort_unstable();
    for d in sorted {
        // the blocks reachable from the branch that do not post-dominate it
        let mut seen = vec![false; n];
        let mut stack: Vec<usize> = succ[d].clone();
        while let Some(b) = stack.pop() {
            if seen[b] || pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                continue;
            }
            seen[b] = true;
            stack.extend(succ[b].iter().copied());
        }
        let inside = barriers.iter().any(|&b| seen.get(b).copied().unwrap_or(false));
        let class = if !inside {
            Class::A
        } else if threads <= 64 {
            WHY.with_borrow_mut(|w| w.push((d, "one console wave")));
            Class::B
        } else if condition_is_the_same_in_every_wave(st, d, &same) {
            WHY.with_borrow_mut(|w| w.push((d, "condition the same in every wave")));
            Class::B
        } else {
            Class::C
        };
        out.push((d, class));
    }
    out
}

/// Whether the branch of block `d` is on scalars every wave of the group holds alike.
fn condition_is_the_same_in_every_wave(st: &Structured, d: usize, same: &HashSet<Value>) -> bool {
    let LaneExit::Branch { when, .. } = st.exits[d] else {
        return false;
    };
    st.bools
        .support(when)
        .iter()
        .all(|a| matches!(a, Atom::Uniform(v) if same.contains(v)))
}

/// The scalar values that are the same in every wave of the thread group: the user SGPRs
/// (root constants and pointers), the thread-group id, constants, and what scalar ALU
/// instructions make of those. Not what a lane read, a load or the wave's own index makes.
fn scalars_of_every_wave(p: &Program, native: &NativeProgram) -> HashSet<Value> {
    let mut same: HashSet<Value> = HashSet::new();
    if let StageIo::Cs(cs) = native.io {
        let (ids, _) = cs.system_sgprs();
        for &(reg, v) in &p.inputs {
            if let Reg::S(n) = reg
                && (n < cs.user_sgprs() || ids.iter().flatten().any(|&i| i == n))
            {
                same.insert(v);
            }
        }
    }
    let order = ir::reverse_postorder(&p.blocks);
    let mut changed = true;
    while changed {
        changed = false;
        for &b in &order {
            let block = &p.blocks[b];
            for phi in &block.phis {
                if matches!(phi.reg, Reg::S(_) | Reg::Scc)
                    && !same.contains(&phi.value)
                    && phi.args.iter().all(|a| same.contains(a))
                {
                    same.insert(phi.value);
                    changed = true;
                }
            }
            for inst in &block.insts {
                let scalar = matches!(inst.fields, Fields::Sop { .. })
                    && !inst.name().starts_with("s_setpc")
                    && !inst.name().starts_with("s_swappc")
                    && !inst.name().starts_with("s_getpc");
                let reads_exec = inst.reads.iter().any(|r| matches!(r.0, Reg::S(c) if c == ir::EXEC || c == ir::EXEC + 1));
                if scalar && !reads_exec && inst.reads.iter().all(|r| same.contains(&r.1)) {
                    for &(_, w) in &inst.writes {
                        changed |= same.insert(w);
                    }
                }
            }
        }
    }
    same
}

/// What the last wave-wide lowering of this thread did: its wide branches and the blocks with a
/// group barrier, and the classes found (for the tests and the corpus lines).
pub(super) type Summary = Vec<(usize, Class)>;

thread_local! {
    /// Why each class-B branch of the last classification is (for the corpus lines).
    static WHY: std::cell::RefCell<Vec<(usize, &'static str)>> = const { std::cell::RefCell::new(Vec::new()) };
    static LAST: std::cell::RefCell<Summary> = const { std::cell::RefCell::new(Vec::new()) };
    static BARRIERS: std::cell::RefCell<BTreeSet<usize>> = const { std::cell::RefCell::new(BTreeSet::new()) };
}

/// The lowering put a group barrier (one that waits for the group) in IR block `b`.
pub(super) fn note_barrier(b: usize) {
    BARRIERS.with_borrow_mut(|s| {
        s.insert(b);
    });
}

/// The blocks noted since the last call.
pub(super) fn take_barriers() -> BTreeSet<usize> {
    BARRIERS.take()
}

pub(super) fn set_last(summary: Summary) {
    LAST.set(summary);
}

/// The classes of the last wave-wide form of kept skips tried on this thread.
pub(super) fn last() -> Summary {
    LAST.with_borrow(Clone::clone)
}

/// What each class is allowed: the form is taken where no branch is of class C.
pub(super) fn safe(classes: &[(usize, Class)]) -> bool {
    classes.iter().all(|&(_, c)| c != Class::C)
}

/// How a program's classes are told in a line of the corpus run.
pub(super) fn describe(classes: &[(usize, Class)]) -> String {
    let parts: Vec<String> = classes
        .iter()
        .map(|(b, c)| {
            let why = WHY.with_borrow(|w| w.iter().find(|x| x.0 == *b).map(|x| x.1));
            match why {
                Some("one console wave") => format!("B{b}=b(one wave)"),
                Some(_) => format!("B{b}=b(same condition)"),
                None => format!("B{b}={}", c.letter()),
            }
        })
        .collect();
    parts.join(",")
}
