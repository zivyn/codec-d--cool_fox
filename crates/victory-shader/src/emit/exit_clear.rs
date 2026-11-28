//! `VICTORY_HALF_EXIT` for every way out of a loop that exchanges (a child module of the lowering,
//! for the state it shares): when no lane of a host wave goes on in a loop whose rounds exchange
//! with the other host wave, the block on the way out clears this half's exchange dword
//! ([`Lower::half_leaves`]), so that the other half does not read what this one last put there
//! in every round it still has to make.
//!
//! The way out may be the branch that closes the loop (the back edge, taken or not) or a branch
//! inside it that leaves (`while (c) { ... }` compiled with its test at the top). Either may be
//! lowered before the first exchange of the loop (the blocks go in reverse postorder, and the
//! exchange may lie in a block after the exit's): a loop passed that way is noted, and once an
//! exchange turns up in it the program is lowered again with that loop forced (`again`), so the
//! clearing stands at the exit that came first.

use std::cell::RefCell;
use std::collections::BTreeSet;

use super::*;

thread_local! {
    /// The loops (by header block) whose exits clear the dword wherever they come in the order of
    /// the blocks, for the lowering in progress on this thread.
    static FORCED: RefCell<BTreeSet<usize>> = const { RefCell::new(BTreeSet::new()) };
    /// The loops this lowering passed an exit of before an exchange of theirs.
    static LATE: RefCell<BTreeSet<usize>> = const { RefCell::new(BTreeSet::new()) };
}

#[cfg(test)]
thread_local! {
    /// The clearings emitted by the lowerings on this thread, and the exits passed before an exchange.
    pub(in crate::emit) static CLEARS: std::cell::Cell<(u32, u32)> = const { std::cell::Cell::new((0, 0)) };
}

/// What the lowering of one program knows of its loops for the clearing.
#[derive(Default)]
pub(super) struct State {
    /// Per loop header, the IR blocks of the loop (made at the first use).
    bodies: Option<HashMap<usize, Vec<bool>>>,
    /// The loops an exchange was lowered in.
    exchanged_in: HashSet<usize>,
    /// The loops an exit was lowered in before any exchange of the loop.
    passed: BTreeSet<usize>,
}

/// Starts the lowering of a program: no loop forced.
pub(super) fn begin() {
    FORCED.with_borrow_mut(BTreeSet::clear);
    LATE.with_borrow_mut(BTreeSet::clear);
}

/// Ends it.
pub(super) fn end() {
    begin();
}

/// Whether the lowering that just ended passed exits of loops before an exchange of theirs that it
/// has not forced yet: the program is lowered again with them forced.
pub(super) fn again() -> bool {
    let late = LATE.take();
    FORCED.with_borrow_mut(|forced| {
        let new = late.iter().any(|h| !forced.contains(h));
        forced.extend(late);
        new
    })
}

impl Lower<'_> {
    /// The blocks of each loop: the header, and what reaches a block that goes back to it.
    fn exit_clear_bodies(&mut self) {
        if self.exit_clear.bodies.is_some() {
            return;
        }
        let p = self.x.p;
        let mut bodies = HashMap::new();
        for &h in self.counters.keys() {
            let mut body = vec![false; p.blocks.len()];
            body[h] = true;
            let mut stack: Vec<usize> = p.blocks[h]
                .preds
                .iter()
                .copied()
                .filter(|&q| self.block[q].is_some() && self.rpo[h] <= self.rpo[q])
                .collect();
            while let Some(n) = stack.pop() {
                if !std::mem::replace(&mut body[n], true) {
                    stack.extend(p.blocks[n].preds.iter().copied().filter(|&q| self.block[q].is_some()));
                }
            }
            bodies.insert(h, body);
        }
        self.exit_clear.bodies = Some(bodies);
    }

    /// An exchange of the two host waves was lowered in the current block: the loops it lies in
    /// exchange in their rounds (`half_exit_branch` asks), and one whose exit was passed before
    /// this is lowered again.
    pub(super) fn note_exchange(&mut self) {
        if !half_exit() {
            return;
        }
        self.exit_clear_bodies();
        let at = self.cur_ir;
        let State { bodies, exchanged_in, passed } = &mut self.exit_clear;
        for (&h, body) in bodies.iter().flatten() {
            if body[at] {
                exchanged_in.insert(h);
                if passed.contains(&h) {
                    LATE.with_borrow_mut(|late| {
                        late.insert(h);
                    });
                }
            }
        }
    }

    /// The conditional branch closing IR block `b` (`taken`, `not_taken`; the DXIL blocks of its
    /// edges `t` and `f`; `cond`: the lane's condition to take it, with the loop's limit in it),
    /// if it is the way out of a loop whose rounds exchange: the half that has no lane to go on
    /// clears its dword on the way out.
    pub(super) fn half_exit_branch(&mut self, b: usize, taken: usize, not_taken: usize, cond: V, t: usize, f: usize) {
        if !half_exit() || !self.exchange.as_ref().is_some_and(|e| !e.1) || self.local || self.pair_meet {
            return;
        }
        self.exit_clear_bodies();
        let back = |l: &Self, to: usize| l.counters.contains_key(&to) && l.rpo[to] <= l.rpo[b];
        // the loops this branch leaves, and whether `taken` is the way that stays in them
        let (headers, stays) = if back(self, taken) {
            (vec![taken], true)
        } else if back(self, not_taken) {
            (vec![not_taken], false)
        } else {
            let bodies = self.exit_clear.bodies.iter().flatten();
            let mut headers: Vec<usize> = bodies
                .filter(|(_, body)| body[b] && body[taken] != body[not_taken])
                .map(|(&h, _)| h)
                .collect();
            headers.sort_unstable();
            let Some(&first) = headers.first() else {
                return;
            };
            let stays = self.exit_clear.bodies.as_ref().and_then(|m| m.get(&first)).is_some_and(|body| body[taken]);
            (headers, stays)
        };
        let forced = FORCED.with_borrow(Clone::clone);
        let known = headers.iter().any(|h| {
            self.exit_clear.exchanged_in.contains(h)
                || forced.contains(h)
                || self.exchanged > self.loop_exchanged.get(h).copied().unwrap_or(u32::MAX)
        });
        if !known {
            #[cfg(test)]
            CLEARS.set((CLEARS.get().0, CLEARS.get().1 + 1));
            self.exit_clear.passed.extend(headers);
            return;
        }
        #[cfg(test)]
        CLEARS.set((CLEARS.get().0 + 1, CLEARS.get().1));
        if stays {
            self.half_leaves(f, cond);
        } else {
            let goes_on = self.negated(cond);
            self.half_leaves(t, goes_on);
        }
    }
}
