//! Indirect executions joined (`VICTORY_INDIRECT_MERGE=1`, off by default):
//! an execution of one record that comes right after another on the same
//! host list is recorded with it, as one `ExecuteIndirect` of more records,
//! when nothing could tell the two apart:
//!
//! - no title call that records into the list came between them (a draw,
//!   a barrier, a copy, a target or viewport set), and the calls between
//!   them that set what the next draw or dispatch binds (pipeline, heaps,
//!   root constants, tables and views, vertex and index buffers: recorded
//!   only at that binding) set what was set
//!   ([`crate::draw::State::rebinds`]);
//! - neither has a count buffer, and each asks for one record (the title's
//!   records with a count are clamped one execution at a time, by a pass
//!   that writes the count the host reads: two would need a pass of their
//!   own that adds the counts);
//! - the same kind and host pipeline;
//! - its record needed no clamp pass of its own (it was placed in an open
//!   batch, or found in a window clamped ahead): a pass recorded for it
//!   would run before the executions held back;
//! - the binding of the run's last execution left the list's epoch
//!   ([`crate::host::HostList::epoch`]) where it found it, and nothing moved
//!   it since: no texture changed state, no twin was written, nothing was
//!   refreshed or copied, so binding again would record the same state;
//! - its record lies right after the run's last in the same scratch buffer,
//!   the run's step apart (a window's step, or a batch's slot). The run is
//!   recorded with a signature of that stride.
//!
//! The run's execution is held back ([`Held`]) until the list records
//! anything else: every title call that may record into the list, the
//! list's close, and the clamp passes and bindings of later executions
//! record it first ([`crate::host::HostList::flush_indirect`]). An
//! execution that joins records nothing: not its binding (as above, the
//! same), not the execution.
//!
//! shortcut: what a binding reads when it is recorded (descriptors copied
//! into host slots, title memory refreshed by address) is for a joined
//! execution as the run's first found it; the title's processor could have
//! written it between the two calls (on the console the card reads it when
//! the commands run, for both).
//!
//! The counters run with the switch off too: the frame line says how many
//! executions could have joined the one before them, and what kept the
//! others apart.

use std::sync::atomic::{AtomicU64, Ordering};

use windows::Win32::Graphics::Direct3D12::*;

/// Whether executions are joined (`VICTORY_INDIRECT_MERGE=1`).
pub(crate) fn on() -> bool {
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_INDIRECT_MERGE").is_some_and(|v| v == "1"))
}

/// An indirect execution as the joining weighs it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Call {
    /// The list's title calls before it ([`crate::host::HostList::title_calls`]).
    pub(crate) calls: u64,
    /// The title's changes of the binding before it, of root constants and
    /// of the rest ([`crate::draw::State::rebinds`]).
    pub(crate) rebinds: [u64; 2],
    /// PC numbering: 0 draw, 1 indexed draw, 2 dispatch, 10 mesh dispatch.
    pub(crate) kind: u32,
    /// The host pipeline (its object's address).
    pub(crate) pipeline: usize,
    /// One record and no count buffer.
    pub(crate) single: bool,
    /// Kept alone whatever comes before or after it: structure builders,
    /// and mesh dispatches under `VICTORY_MESH_TEST=scissor` (scissors set
    /// around each).
    pub(crate) alone: bool,
    /// A clamp pass was recorded for its record (its own, a window's or a
    /// batch's that it opened).
    pub(crate) clamped: bool,
    /// The list's epoch before its binding.
    pub(crate) epoch: u64,
    /// The scratch buffer its record is in (the object's address), the
    /// record's offset there, and the bytes from one record to the next
    /// there.
    pub(crate) buffer: usize,
    pub(crate) at: u64,
    pub(crate) step: u32,
}

/// Executions recorded as one, or that could have been: the joining's
/// view of a list's last indirect executions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Run {
    pub(crate) kind: u32,
    pub(crate) pipeline: usize,
    pub(crate) single: bool,
    pub(crate) alone: bool,
    pub(crate) buffer: usize,
    /// The first record's offset, the step between records, and the
    /// records.
    pub(crate) at: u64,
    pub(crate) step: u32,
    pub(crate) records: u32,
    /// The list's title calls, the title's changes of the binding, and the
    /// list's epoch, after the last execution.
    pub(crate) calls: u64,
    pub(crate) rebinds: [u64; 2],
    pub(crate) epoch: u64,
    /// The last execution's binding left the epoch where it found it.
    pub(crate) settled: bool,
}

impl Run {
    /// A run of `call` alone, which left the list's epoch at `epoch`.
    pub(crate) fn of(call: &Call, epoch: u64) -> Run {
        Run {
            kind: call.kind,
            pipeline: call.pipeline,
            single: call.single,
            alone: call.alone,
            buffer: call.buffer,
            at: call.at,
            step: call.step,
            records: 1,
            calls: call.calls,
            rebinds: call.rebinds,
            epoch,
            settled: epoch == call.epoch,
        }
    }

    /// `call` (which [`Verdict::Joins`] it) is the run's last execution
    /// now; its binding left the epoch at `epoch` (its call's own when it
    /// was not bound again).
    pub(crate) fn join(&mut self, call: &Call, epoch: u64) {
        self.records += 1;
        self.calls = call.calls;
        self.rebinds = call.rebinds;
        self.settled = epoch == call.epoch;
        self.epoch = epoch;
    }

    /// Where the next record of the run would be.
    pub(crate) fn next(&self) -> u64 {
        self.at + u64::from(self.records) * u64::from(self.step)
    }
}

/// Whether an execution joins a list's run, and if not, why.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Verdict {
    Joins,
    /// The first indirect execution of the list (since its reset).
    First,
    /// A title call that records into the list came between.
    Between,
    /// It or the run has a count buffer or more than one record.
    Counted,
    /// It or the run is kept alone ([`Call::alone`]).
    Alone,
    /// Another kind of execution, or another pipeline.
    Differs,
    /// A clamp pass was recorded for it.
    Clamped,
    /// The title changed the binding between (pipeline, heaps, tables,
    /// views, buffers).
    Rebound,
    /// The title changed root constants between, and nothing else of the
    /// binding (a signature that sets root constants per record could
    /// join these).
    Constants,
    /// The run's last binding moved the list's epoch, or something moved
    /// it since.
    Unsettled,
    /// Its record is not the one after the run's last in the same buffer,
    /// the run's step apart.
    Apart,
}

/// The rules (the module's): whether `call` joins `run`, the list's run
/// when the call came.
pub(crate) fn verdict(run: Option<&Run>, call: &Call) -> Verdict {
    let Some(run) = run else {
        return Verdict::First;
    };
    if call.calls != run.calls {
        Verdict::Between
    } else if !call.single || !run.single {
        Verdict::Counted
    } else if call.alone || run.alone {
        Verdict::Alone
    } else if call.kind != run.kind || call.pipeline != run.pipeline {
        Verdict::Differs
    } else if call.clamped {
        Verdict::Clamped
    } else if call.rebinds[1] != run.rebinds[1] {
        Verdict::Rebound
    } else if call.rebinds[0] != run.rebinds[0] {
        Verdict::Constants
    } else if !run.settled || call.epoch != run.epoch {
        Verdict::Unsettled
    } else if call.buffer != run.buffer || call.step != run.step || call.at != run.next() {
        Verdict::Apart
    } else {
        Verdict::Joins
    }
}

/// A run's execution not recorded yet: the signature it is recorded with
/// (of the title's stride for one record, of the run's step for more) and
/// the scratch buffer it reads.
pub(crate) struct Held {
    pub(crate) signature: ID3D12CommandSignature,
    pub(crate) buffer: ID3D12Resource,
}

impl Held {
    /// Records the execution of `run`.
    pub(crate) fn record(&self, list: &ID3D12GraphicsCommandList, run: &Run) {
        // SAFETY: an open list with the run's binding set (nothing was
        // recorded since: everything that records records this first); the
        // run's records are in `buffer`, which `self` holds; the signature
        // is cached for the process.
        unsafe { list.ExecuteIndirect(&self.signature, run.records, &self.buffer, run.at, None, 0) };
    }
}

/// Of indirect executions run, by [`Verdict`], then those joined.
static COUNTS: [AtomicU64; 12] = [const { AtomicU64::new(0) }; 12];
const JOINED: usize = 11;

/// An execution ran with `verdict`; `joined` if it was recorded with the
/// run before it.
pub(crate) fn count(verdict: Verdict, joined: bool) {
    COUNTS[verdict as usize].fetch_add(1, Ordering::Relaxed);
    if joined {
        COUNTS[JOINED].fetch_add(1, Ordering::Relaxed);
    }
}

/// The frame line of the joining: per frame of the last `frames`.
pub(crate) fn line(frames: f64) {
    let [joins, first, between, counted, alone, differs, clamped, rebound, constants, unsettled, apart, joined] =
        std::array::from_fn::<f64, 12, _>(|i| COUNTS[i].swap(0, Ordering::Relaxed) as f64 / frames);
    let all = joins + first + between + counted + alone + differs + clamped + rebound + constants + unsettled + apart;
    if all == 0.0 {
        return;
    }
    tracing::info!(
        "indirect executions per frame: {all:.0}, {joins:.0} could join the one before them in one execution \
         (VICTORY_INDIRECT_MERGE=1 joins them: now {}), {joined:.0} joined; of the others {first:.0} first on their \
         list, {between:.0} after a title call that records, {counted:.0} with a count buffer or more records (theirs \
         or the one's before), {alone:.0} kept alone, {differs:.0} of another kind or pipeline, {clamped:.0} clamped \
         by a pass of their own, {rebound:.0} after the title changed their binding, {constants:.0} after it changed \
         root constants only, {unsettled:.0} after a binding that changed the list, {apart:.0} with their record apart",
        if on() { "on" } else { "off" }
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    const DRAW: u32 = 0;
    const DISPATCH: u32 = 2;

    /// A batched dispatch record in slot `slot` of buffer 0x100, after
    /// `calls` title calls, at epoch 7.
    fn call(calls: u64, slot: u64) -> Call {
        Call {
            calls,
            rebinds: [2, 5],
            kind: DISPATCH,
            pipeline: 0x5000,
            single: true,
            alone: false,
            clamped: false,
            epoch: 7,
            buffer: 0x100,
            at: 64 + slot * 32,
            step: 32,
        }
    }

    #[test]
    fn the_next_record_with_nothing_between_joins() {
        let first = call(3, 0);
        let mut run = Run::of(&first, 7);
        assert!(run.settled);
        let second = call(3, 1);
        assert_eq!(verdict(Some(&run), &second), Verdict::Joins);
        run.join(&second, second.epoch);
        assert_eq!((run.records, run.next()), (2, 64 + 2 * 32));
        // The third follows the second.
        assert_eq!(verdict(Some(&run), &call(3, 2)), Verdict::Joins);
        // Not the second again, nor one past the next.
        assert_eq!(verdict(Some(&run), &call(3, 1)), Verdict::Apart);
        assert_eq!(verdict(Some(&run), &call(3, 3)), Verdict::Apart);
    }

    #[test]
    fn the_first_of_a_list_joins_nothing() {
        assert_eq!(verdict(None, &call(0, 0)), Verdict::First);
    }

    #[test]
    fn a_title_call_between_keeps_them_apart() {
        let run = Run::of(&call(3, 0), 7);
        assert_eq!(verdict(Some(&run), &call(4, 1)), Verdict::Between);
    }

    #[test]
    fn counts_and_more_records_keep_them_apart() {
        let run = Run::of(&call(3, 0), 7);
        let counted = Call { single: false, ..call(3, 1) };
        assert_eq!(verdict(Some(&run), &counted), Verdict::Counted);
        // Nor does a single record join a run of a counted one.
        let run = Run::of(&Call { single: false, ..call(3, 0) }, 7);
        assert_eq!(verdict(Some(&run), &call(3, 1)), Verdict::Counted);
    }

    #[test]
    fn executions_kept_alone_join_nothing() {
        let run = Run::of(&call(3, 0), 7);
        assert_eq!(verdict(Some(&run), &Call { alone: true, ..call(3, 1) }), Verdict::Alone);
        let run = Run::of(&Call { alone: true, ..call(3, 0) }, 7);
        assert_eq!(verdict(Some(&run), &call(3, 1)), Verdict::Alone);
    }

    #[test]
    fn another_kind_or_pipeline_keeps_them_apart() {
        let run = Run::of(&call(3, 0), 7);
        assert_eq!(verdict(Some(&run), &Call { kind: DRAW, ..call(3, 1) }), Verdict::Differs);
        assert_eq!(verdict(Some(&run), &Call { pipeline: 0x6000, ..call(3, 1) }), Verdict::Differs);
    }

    #[test]
    fn a_binding_the_title_changed_keeps_them_apart() {
        let run = Run::of(&call(3, 0), 7);
        // Another table or view between: not joined.
        assert_eq!(verdict(Some(&run), &Call { rebinds: [2, 6], ..call(3, 1) }), Verdict::Rebound);
        // Root constants only: what a signature with root constants could join.
        assert_eq!(verdict(Some(&run), &Call { rebinds: [3, 5], ..call(3, 1) }), Verdict::Constants);
        assert_eq!(verdict(Some(&run), &Call { rebinds: [3, 6], ..call(3, 1) }), Verdict::Rebound);
        // A joined execution carries the counts on.
        let mut run = Run::of(&call(3, 0), 7);
        run.join(&call(3, 1), 7);
        assert_eq!(verdict(Some(&run), &call(3, 2)), Verdict::Joins);
    }

    #[test]
    fn a_record_clamped_by_a_pass_of_its_own_does_not_join() {
        let run = Run::of(&call(3, 0), 7);
        assert_eq!(verdict(Some(&run), &Call { clamped: true, ..call(3, 1) }), Verdict::Clamped);
    }

    #[test]
    fn a_binding_that_changed_the_list_keeps_the_next_apart() {
        // Its binding moved the epoch from 7 to 8 (a texture changed state).
        let mut run = Run::of(&call(3, 0), 8);
        assert!(!run.settled);
        assert_eq!(verdict(Some(&run), &Call { epoch: 8, ..call(3, 1) }), Verdict::Unsettled);
        // Settled, but something moved the epoch since.
        run = Run::of(&call(3, 0), 7);
        assert_eq!(verdict(Some(&run), &Call { epoch: 9, ..call(3, 1) }), Verdict::Unsettled);
        // A second execution whose binding settles starts a run the third
        // can join.
        let second = Call { epoch: 8, ..call(3, 1) };
        let mut run = Run::of(&second, 8);
        assert_eq!(verdict(Some(&run), &Call { epoch: 8, ..call(3, 2) }), Verdict::Joins);
        // A joined execution whose binding moved the epoch unsettles the run.
        run.join(&Call { epoch: 8, ..call(3, 2) }, 9);
        assert_eq!(verdict(Some(&run), &Call { epoch: 9, ..call(3, 3) }), Verdict::Unsettled);
    }

    #[test]
    fn records_must_follow_in_the_same_buffer_at_the_same_step() {
        let run = Run::of(&call(3, 0), 7);
        assert_eq!(verdict(Some(&run), &Call { buffer: 0x200, ..call(3, 1) }), Verdict::Apart);
        // A window walked at 48 bytes is not a run of 32-byte slots.
        assert_eq!(verdict(Some(&run), &Call { step: 48, at: 64 + 48, ..call(3, 1) }), Verdict::Apart);
        // The same record again (a batch gives a repeated record its slot).
        assert_eq!(verdict(Some(&run), &call(3, 0)), Verdict::Apart);
    }

    #[test]
    fn the_title_calls_come_first_among_the_reasons() {
        let run = Run::of(&call(3, 0), 8);
        let other = Call { kind: DRAW, clamped: true, single: false, epoch: 9, buffer: 0x200, ..call(4, 5) };
        assert_eq!(verdict(Some(&run), &other), Verdict::Between);
    }
}
