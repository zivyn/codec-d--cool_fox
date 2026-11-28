//! `VICTORY_HANG_GUARD` for the lowering (a child module of it, for the state it shares): a
//! group barrier that waits for the group is a counter of each host wave in shared memory and a
//! wait on the others' counters that gives up (see [`super::super::hang`]). A wave that gives up,
//! or arrives at more barriers than its budget, or goes round a loop with a barrier in it more
//! often than the loop's bound, leaves a note in the faults buffer and ends. The group then ends
//! where it would have waited for the device to be reset.
//!
//! The counters are the waves' own, so a barrier is reached the same number of times by every
//! wave or the wait gives up: waves that left the program, or that went another way, are found
//! out where the hardware's barrier would have counted them as arrived (or hung).

use super::super::hang::{self, kind};
use super::*;

/// What the guard keeps for the lowering of one program.
#[derive(Clone, Copy)]
pub(super) struct GuardState {
    pub cfg: hang::Guard,
    /// The first of the shared-memory dwords of the guard: one counter for each host wave, the
    /// next wave's number, and a dword that takes what threads past the counters write.
    pub base: u32,
    /// The host waves of the group.
    pub waves: u32,
}

/// The dwords of shared memory the guard needs for a group of `threads`, if it can be in force.
pub(super) fn dwords(threads: u32) -> Option<u32> {
    (hang::guard().is_some() && (1..=1024).contains(&threads)).then(|| threads.div_ceil(32) + 2)
}

impl<'a> Lower<'a> {
    /// The group barrier `mode` in `block`: the hardware's, or with the guard on a barrier of
    /// counters that gives up. Fences stay fences.
    pub(super) fn barrier_op(&mut self, block: usize, mode: u32) {
        if mode & 1 != 0 {
            self.group_barriers.insert(self.cur_ir);
        }
        if mode & 1 != 0
            && self.guard.is_some()
            && block != self.s.entry
            && let Some(wave) = self.guard_wave()
        {
            self.guarded_barrier(block, mode, wave);
            return;
        }
        let mode = self.s.m.i32(mode);
        let void = self.s.t.void;
        self.s.op(block, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
    }

    /// This host wave's number among the group's (0, 1, 2...), taken in the entry block, which
    /// every thread runs: the first threads zero the counters, the group meets (a barrier of the
    /// hardware: the entry is the one place that cannot be left), and each wave's first lane
    /// takes the next number.
    fn guard_wave(&mut self) -> Option<V> {
        let g = self.guard?;
        if let Some(w) = self.guard_wave {
            return Some(w);
        }
        let (e, i32t, i1t, void) = (self.s.entry, self.s.t.i32, self.s.t.i1, self.s.t.void);
        let lds = self.s.lds();
        let flat = self.s.op(e, "flattenedThreadIdInGroup.i32", Attr::ReadNone, i32t, 96, &[]);
        let (zero, one) = (self.s.m.i32(0), self.s.m.i32(1));
        let n = self.s.m.i32(g.waves + 1);
        let mine = self.s.m.cmp(e, pred::IULT, flat, n);
        let sink = self.s.m.i32(g.waves + 1);
        let at = self.s.m.select(e, mine, flat, sink);
        let first = self.s.m.i32(g.base);
        let at = self.s.m.bin(e, Bin::Add, first, at);
        let p = self.s.m.element(e, lds, at);
        self.s.m.store(e, p, zero);
        let mode = self.s.m.i32(1 | 8);
        self.s.op(e, "barrier", Attr::NoDuplicate, void, 80, &[mode]);
        let lane = self.s.op(e, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let is_first = self.s.m.cmp(e, pred::EQ, lane, zero);
        let add = self.s.m.select(e, is_first, one, zero);
        let next = self.s.m.i32(g.base + g.waves);
        let p = self.s.m.element(e, lds, next);
        let old = self.s.m.atomic(e, Rmw::Add, p, add);
        let w = self.s.op(e, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[old]);
        let _ = i1t;
        self.s.flags |= flags::WAVE_OPS;
        self.guard_wave = Some(w);
        Some(w)
    }

    /// A note of the guard in the faults buffer from every thread that runs `block`.
    fn guard_note(&mut self, block: usize, what: u32, site: u32) {
        let tag = PROGRAM_TAG.get();
        if tag == 0 {
            return;
        }
        let value = hang::note(what, tag, site);
        let faults = self.s.root_uav_handle(super::super::shader::SLOT_FAULTS);
        let place = self.s.m.i32((1 + value % (super::super::shader::MEMORY_FAULTS - 1)) * 4);
        let (value, iu) = (self.s.m.i32(value), self.s.m.undef(self.s.t.i32));
        let (mask, align, void) = (self.s.m.i8(1), self.s.m.i32(4), self.s.t.void);
        self.s.op(block, "rawBufferStore.i32", Attr::NoUnwind, void, 140, &[faults, place, iu, value, iu, iu, iu, mask, align]);
    }

    /// The barrier of counters (`mode` is the hardware barrier's): this wave's counter goes up, a
    /// wave over its budget ends, and the wave waits until every counter has reached its own,
    /// at most the guard's number of rounds.
    fn guarded_barrier(&mut self, at: usize, mode: u32, wave: V) {
        let Some(g) = self.guard else { return };
        self.guard_waits.insert(self.cur_ir);
        let site = self.guard_sites;
        self.guard_sites += 1;
        let (i32t, i1t, void) = (self.s.t.i32, self.s.t.i1, self.s.t.void);
        let lds = self.s.lds();
        let fence = mode & !1;
        if fence != 0 {
            let m = self.s.m.i32(fence);
            self.s.op(at, "barrier", Attr::NoDuplicate, void, 80, &[m]);
        }
        let (zero, one) = (self.s.m.i32(0), self.s.m.i32(1));
        let lane = self.s.op(at, "waveGetLaneIndex", Attr::ReadOnly, i32t, 111, &[]);
        let is_first = self.s.m.cmp(at, pred::EQ, lane, zero);
        let inc = self.s.m.select(at, is_first, one, zero);
        let base = self.s.m.i32(g.base);
        let index = self.s.m.bin(at, Bin::Add, base, wave);
        let mine_ptr = self.s.m.element(at, lds, index);
        let old = self.s.m.atomic(at, Rmw::Add, mine_ptr, inc);
        let old = self.s.op(at, "waveReadLaneFirst.i32", Attr::NoUnwind, i32t, 118, &[old]);
        let mine = self.s.m.bin(at, Bin::Add, old, one);
        let budget = self.s.m.i32(g.cfg.budget);
        let over = self.s.m.cmp(at, pred::IUGT, mine, budget);
        self.s.flags |= flags::WAVE_OPS;
        // Everything made so far moves to a block of its own, ahead of the wait.
        let head = self.s.m.split_before(at);
        for b in self.block.iter_mut().filter(|b| **b == Some(at)) {
            *b = Some(head);
        }
        let (wait, check, spent, gave_up) =
            (self.s.m.new_block(), self.s.m.new_block(), self.s.m.new_block(), self.s.m.new_block());
        self.s.m.terminate(head, Term::CondBr(over, spent, wait));
        self.guard_note(spent, kind::BUDGET, site);
        self.s.m.terminate(spent, Term::Ret);
        let spins = self.s.m.phi(wait, i32t);
        let mut behind = self.s.m.bool(false);
        for j in 0..g.waves {
            let k = self.s.m.i32(g.base + j);
            let p = self.s.m.element(wait, lds, k);
            // (an atomic: the read is made every time round, and sees the other wave's add)
            let seen = self.s.m.atomic(wait, Rmw::Or, p, zero);
            let ahead = self.s.m.bin(wait, Bin::Sub, seen, mine);
            let lt = self.s.m.cmp(wait, pred::SLT, ahead, zero);
            behind = self.s.m.bin(wait, Bin::Or, behind, lt);
        }
        let any = self.s.op(wait, "waveAnyTrue", Attr::NoUnwind, i1t, 113, &[behind]);
        let next = self.s.m.bin(wait, Bin::Add, spins, one);
        let limit = self.s.m.i32(g.cfg.spins);
        let early = self.s.m.cmp(wait, pred::IULT, next, limit);
        let again = self.s.m.bin(wait, Bin::And, any, early);
        self.s.m.set_phi(spins, vec![(zero, head), (next, wait)]);
        self.s.m.terminate(wait, Term::CondBr(again, wait, check));
        self.s.m.terminate(check, Term::CondBr(any, gave_up, at));
        self.guard_note(gave_up, kind::WAIT, site);
        self.s.m.terminate(gave_up, Term::Ret);
        if fence != 0 {
            let m = self.s.m.i32(fence);
            self.s.op(at, "barrier", Attr::NoDuplicate, void, 80, &[m]);
        }
    }

    /// Whether a barrier or a wait of the guard was made in loop `h` (so far).
    fn loop_has_wait(&self, h: usize) -> bool {
        self.guard_waits.iter().any(|&b| {
            self.rpo[h] <= self.rpo[b]
                && self.x.p.blocks[h].preds.iter().any(|&q| self.block[q].is_some() && self.rpo[q] >= self.rpo[b])
        })
    }

    /// Whether loop `h` sits in another loop (the rpo of `h` is inside the other's, whose back
    /// edge comes from after `h`'s own).
    fn loop_in_loop(&self, h: usize) -> bool {
        let latch = |l: usize| {
            self.x.p.blocks[l]
                .preds
                .iter()
                .filter(|&&q| self.block[q].is_some() && self.rpo[q] >= self.rpo[l])
                .map(|&q| self.rpo[q])
                .max()
        };
        let Some(mine) = latch(h) else { return false };
        self.counters
            .keys()
            .any(|&g| g != h && self.rpo[g] < self.rpo[h] && latch(g).is_some_and(|theirs| theirs >= mine))
    }

    /// Whether a barrier that waits for the group was made in loop `h` (so far).
    fn loop_has_group_barrier(&self, h: usize) -> bool {
        self.group_barriers.iter().any(|&b| {
            self.rpo[h] <= self.rpo[b]
                && self.x.p.blocks[h].preds.iter().any(|&q| self.block[q].is_some() && self.rpo[q] >= self.rpo[b])
        })
    }

    /// The limit of loop `h` with the guard: a loop with a barrier in it goes round at most the
    /// guard's number of times. And, in the audited kept skips' form, a loop with a barrier that
    /// waits for the group in another loop goes round at most [`hang::nested_loop_limit`] times
    /// (the threads of the group: a retry loop needs a round for each thread at the very most).
    pub(super) fn guard_loop_limit(&self, h: usize, limit: u32) -> u32 {
        let limit = match self.guard {
            Some(g) if self.loop_has_wait(h) => limit.min(g.cfg.loops),
            _ => limit,
        };
        if hang::nested_loops_bounded() && self.loop_has_group_barrier(h) && self.loop_in_loop(h) {
            let group = self.s.threads.map_or(0, |t| t.iter().product::<u32>());
            return limit.min(hang::nested_loop_limit(group));
        }
        limit
    }

    /// With the guard: a note, from every lane, that loop `h` reached its bound (`next` iterations
    /// made, `limit` the bound).
    pub(super) fn guard_loop_note(&mut self, h: usize, next: V, limit: V) {
        if self.guard.is_none() {
            return;
        }
        let tag = PROGRAM_TAG.get();
        if tag == 0 || !self.loop_has_wait(h) {
            return;
        }
        let c = self.cur;
        let hit = self.s.m.cmp(c, pred::EQ, next, limit);
        let value = hang::note(kind::LOOP, tag, self.loop_number(h));
        let faults = self.s.root_uav_handle(super::super::shader::SLOT_FAULTS);
        let at = self.s.m.i32((1 + value % (super::super::shader::MEMORY_FAULTS - 1)) * 4);
        let zero = self.s.m.i32(0);
        let place = self.s.m.select(c, hit, at, zero);
        let (value, iu) = (self.s.m.i32(value), self.s.m.undef(self.s.t.i32));
        let (mask, align, void) = (self.s.m.i8(1), self.s.m.i32(4), self.s.t.void);
        self.s.op(c, "rawBufferStore.i32", Attr::NoUnwind, void, 140, &[faults, place, iu, value, iu, iu, iu, mask, align]);
    }

    /// With the guard: a note that the host waves of a console wave gave up waiting for each
    /// other, from the threads that run `block`.
    pub(super) fn guard_pair_note(&mut self, block: usize) {
        if self.guard.is_some() {
            self.guard_note(block, kind::PAIR, 0);
        }
    }
}
