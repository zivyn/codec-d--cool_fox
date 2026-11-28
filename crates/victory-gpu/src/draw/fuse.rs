//! Dispatches of one program made one (`VICTORY_FUSE_DISPATCH=1` when the program is translated,
//! `VICTORY_FUSE_RUNS=1` while the title runs; one of the switches `VICTORY_AB` can flip).
//!
//! The title's tail is some 2,700 tiny dispatches a frame, each a command of the card with its own launch and its own
//! chain of dependent loads (user data, descriptor, parameter, data) in front of one or a few groups of threads.
//! A compute program translated with `VICTORY_FUSE_DISPATCH=1` (`victory_shader::emit` `lower/fuse.rs`) reads its user
//! data and its group id from a record of a table, the root CBV `SLOT_FUSE`, instead of from the root constants and
//! the dispatch: a run of dispatches of one program is one table of their records and one dispatch of all their groups.
//!
//! What a run is: the dispatches of the same host pipeline (and the same view heaps) one after the other, with
//! nothing between them but what the title sets for each (constants, tables, views, the pipeline it already has, and,
//! with `VICTORY_PRECISE_BARRIERS`, waits that no dispatch after them needs): the stream says how many come
//! ([`State::fuse_begin`]) and the run is made when it ends ([`State::fuse_end`]) or when a dispatch cannot join it
//! (another program, no room in the table, a wait that has to be made). Each dispatch is bound as always (its
//! descriptor tables, its declarations of what it writes, the host copies it reads), and what is bound for the root
//! constants of its stage goes into its record instead; the dispatch itself is the run's. Where the dispatches ran in
//! the order the title made them, with the card free to overlap the ones with no wait between them, they now are
//! the groups of one dispatch, in the same order.
//!
//! Which dispatches join: those of programs that read no root view of the title and touch only buffers
//! ([`joinable`]). The host list is told of the table's address and the dispatch when the run ends, so a
//! texture a later dispatch of the run needs in another state, or a copy, would come before the dispatches of the
//! ones in front of it: a program that reads or writes a texture is its own run. A program translated for runs
//! is always dispatched through a table, a run of one when nothing joins it (`VICTORY_FUSE_RUNS` off, a list that
//! is not written down call by call, another program next).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use victory_shader::emit::{SLOT_FUSE, fuse_fill, fuse_fits, fuse_rows};

use super::*;
use crate::pipeline::host::Stage;

/// Since the last line: runs made, the dispatches in them, their groups, and the runs of one dispatch; runs made
/// where the next dispatch had another program or other heaps, no room, a wait to be made, the end of the stream's
/// plan, or nothing to wait for (a dispatch that joins nothing); runs the card could not be given.
pub(crate) static COUNTS: [AtomicU64; 10] = [const { AtomicU64::new(0) }; 10];

/// The line of what the runs came to, per frame (with the one of the executions: [`crate::list::runs_line`]).
pub(crate) fn line(frames: f64) {
    let c: Vec<u64> = COUNTS.iter().map(|c| c.swap(0, Relaxed)).collect();
    if c[0] == 0 || frames <= 0.0 {
        return;
    }
    let per = |v: u64| v as f64 / frames;
    tracing::info!(
        "dispatches made one per frame (VICTORY_FUSE_RUNS): {:.0} runs of {:.1} dispatches on average ({:.0} of one dispatch), {:.0} dispatches made {:.0} fewer, {:.0} groups a run; runs ended by {:.0} another program or heaps, {:.0} no room, {:.0} a wait, {:.0} the stream's plan, {:.0} no plan; {:.0} not made",
        per(c[0]),
        c[1] as f64 / c[0] as f64,
        per(c[3]),
        per(c[1]),
        per(c[1].saturating_sub(c[0])),
        c[2] as f64 / c[0] as f64,
        per(c[4]),
        per(c[5]),
        per(c[6]),
        per(c[7]),
        per(c[8]),
        per(c[9]),
    );
}

/// Why a run was made: counts in [`COUNTS`] from 4 on.
const OTHER: usize = 4;
const ROOM: usize = 5;
pub(super) const WAIT: usize = 6;
const PLAN: usize = 7;
const ALONE: usize = 8;

/// The groups of a run in all, at most (the program counts them in 31 bits).
const MOST_GROUPS: u64 = (u32::MAX >> 1) as u64;

/// What the stage's program was translated for: a table of a run instead of root constants.
pub(super) fn shaped(stage: &Stage) -> bool {
    stage.translated().host.root_cbvs.contains(&SLOT_FUSE)
}

/// Where the host's root signature has the table of a program translated for runs.
fn table_param(stage: &Stage) -> Option<u32> {
    stage
        .root_cbvs
        .iter()
        .find(|r| r.0 == SLOT_FUSE)
        .map(|r| r.1)
}

/// Whether the program of `stage` may share a run: it reads and writes only buffers, through the descriptor tables of
/// the title, and has no stand-in for the indirect light to follow it.
fn joinable(stage: &Stage) -> bool {
    stage.shader.joins_runs() && !crate::bufscan::on()
}

/// The dispatches being made one.
#[derive(Default)]
pub(super) struct Fuse {
    /// The stream says more dispatches come that may be one run.
    planned: bool,
    run: Option<Run>,
    /// What the last bind of a program translated for runs gave the user data of its stage.
    pub(super) cur: Vec<u32>,
    /// A run that was made, emptied, for the next one's room.
    spare: Option<Run>,
}

/// The dispatches so far: what the host list must still have when they are made, and their records.
#[derive(Default)]
struct Run {
    pso: usize,
    heaps: [usize; 2],
    /// The root parameter of the table, and the dwords of user data of a dispatch.
    param: u32,
    dwords: usize,
    groups: Vec<[u32; 3]>,
    /// The user data of all, one dispatch after the other.
    values: Vec<u32>,
    total: u64,
}

impl State {
    /// Whether a run is open.
    pub(super) fn fuse_open(&self) -> bool {
        self.fuse.run.is_some()
    }

    /// The stream says more dispatches come that may be one run.
    pub(crate) fn fuse_begin(&mut self) {
        self.fuse.planned = true;
    }

    /// The stream's run of dispatches is over: made, if one is open.
    pub(crate) fn fuse_end(&mut self, list: &HostList) {
        self.fuse_flush(list, PLAN);
        self.fuse.planned = false;
    }

    /// A dispatch of the program `p` with `groups` is about to be bound (`shaped`: its program is translated for
    /// runs): the open run, which this one cannot join, is made first, while the host list is as the run left it.
    pub(super) fn fuse_room(
        &mut self,
        list: &HostList,
        p: &HostPipeline,
        groups: [u32; 3],
        shaped: bool,
    ) {
        let Some(run) = &self.fuse.run else { return };
        let stage = p.stages.first();
        let size = groups.iter().map(|&g| u64::from(g)).product::<u64>();
        let why = if !shaped
            || run.pso != p.pso.as_raw() as usize
            || run.heaps != self.heaps
            || !stage.is_some_and(joinable)
        {
            OTHER
        } else if !fuse_fits(run.groups.len() + 1, run.dwords) || run.total + size > MOST_GROUPS {
            ROOM
        } else {
            return;
        };
        self.fuse_flush(list, why);
    }

    /// The dispatch just bound (`bind` captured the user data of its stage) is a member of the run: begun, or
    /// made at once if nothing is to join it.
    pub(super) fn fuse_add(&mut self, list: &HostList, p: &HostPipeline, groups: [u32; 3]) {
        let Some(stage) = p.stages.first() else {
            return;
        };
        let size = groups.iter().map(|&g| u64::from(g)).product::<u64>();
        let Some(param) = table_param(stage) else {
            once!(
                "a program translated for runs of dispatches has no table in its root signature: the dispatch is dropped"
            );
            return;
        };
        if size == 0 {
            // (A dispatch of no groups does nothing.)
            return;
        }
        let dwords = stage.user_data.map_or(0, |u| u.1 as usize);
        self.fuse.cur.resize(dwords, 0);
        if self.fuse.run.is_none() {
            let spare = self.fuse.spare.take().unwrap_or_default();
            self.fuse.run = Some(Run {
                pso: p.pso.as_raw() as usize,
                heaps: self.heaps,
                param,
                dwords,
                ..spare
            });
        }
        let Some(run) = self.fuse.run.as_mut() else {
            return;
        };
        run.groups.push(groups);
        run.values.extend_from_slice(&self.fuse.cur);
        run.total += size;
        if !(self.fuse.planned && joinable(stage)) {
            self.fuse_flush(list, ALONE);
        }
    }

    /// The run is made: its table is written into the list's upload memory, and the host list is given its address
    /// and the dispatch of all the groups.
    pub(super) fn fuse_flush(&mut self, list: &HostList, why: usize) {
        let Some(mut run) = self.fuse.run.take() else {
            return;
        };
        let n = run.groups.len();
        let rows = fuse_rows(n, run.dwords);
        let made = (|| {
            let (gpu, data) = list.scratch(rows as u64 * 16)?;
            // SAFETY: `rows * 16` bytes of mapped upload memory, 256-byte aligned, the list's until it is reset.
            let out = unsafe { std::slice::from_raw_parts_mut(data.cast::<u32>(), rows * 4) };
            let dims = fuse_fill(out, run.dwords, &run.groups, &run.values)?;
            // SAFETY: an open list that has the run's pipeline, root signature and heaps (the dispatches that joined
            // were bound with them, and nothing else was recorded since: `fuse_room`).
            unsafe {
                list.list.SetComputeRootConstantBufferView(run.param, gpu);
                list.list.Dispatch(dims[0], dims[1], dims[2]);
            }
            Some(())
        })();
        if made.is_some() {
            COUNTS[0].fetch_add(1, Relaxed);
            COUNTS[1].fetch_add(n as u64, Relaxed);
            COUNTS[2].fetch_add(run.total, Relaxed);
            COUNTS[3].fetch_add(u64::from(n == 1), Relaxed);
            COUNTS[why].fetch_add(1, Relaxed);
        } else {
            COUNTS[9].fetch_add(1, Relaxed);
            once!("the table of a run of dispatches could not be made: its dispatches are dropped");
        }
        run.groups.clear();
        run.values.clear();
        run.total = 0;
        self.fuse.spare = Some(run);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A run of members is the table the program reads, whatever the dispatches' groups.
    #[test]
    fn the_counts_have_room_for_every_reason() {
        for why in [OTHER, ROOM, WAIT, PLAN, ALONE] {
            assert!(why < COUNTS.len());
        }
        assert!(MOST_GROUPS < u64::from(u32::MAX));
    }
}
