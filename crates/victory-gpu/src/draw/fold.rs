//! One execution for a run of per-object executions (`VICTORY_INDIRECT_FOLD=1`;
//! off by default; one of the switches `VICTORY_AB` can flip).
//!
//! The title draws its objects one execution each (one record, no count), and
//! between two it sets what is the object's: root constants, descriptor
//! tables, root views. The host makes one `ExecuteIndirect` of each (4.5
//! microseconds on the card, ~4,000 a frame). A run of them that only
//! differ in the user data of their stages (constants and the offsets of
//! tables in the view heap: what the host puts in one root parameter a
//! stage) is made one execution of a command signature that sets those
//! dwords in front of each record: `[root arguments, record]` a command,
//! `max` the number of executions. The batched clamp (`crate::indirect::batch`)
//! writes the commands: it gives each job its root arguments, which the
//! CPU puts in the table's memory when the run ends ([`State::fold_make`]).
//!
//! What is the same for all of a run (nothing between its executions but
//! the title's root settings, uploads and the pipeline it already has): the
//! pipeline, the signature, the targets and render state (no call that
//! changes them lies between); the title's root views (a view is a
//! descriptor table in the host root signature, which no signature can
//! set; [`State::fold_views`]), except the root SRVs the shaders read through
//! the view heap (`VICTORY_FOLD_VIEWS`, below); the shape of the user data.
//! What differs is the user-data dwords of the stages, which the run's
//! commands set: the dwords of each block that differ in the run, each
//! stretch of them one root argument ([`ranges`]). A call that would end a
//! run is the head of the next.
//!
//! Root views: a title root SRV that the shader reads through the view heap
//! (`VICTORY_VIEW_INDEX=1` or `VICTORY_FOLD_VIEWS=1` when the program is
//! translated: `victory_shader::emit::USER_ROOT_VIEW_INDEX`) is not a root
//! parameter of the host: the heap slot of its one-view slot is a dword of
//! the user data, like the constants, so the views of two calls may differ
//! in a run (`VICTORY_FOLD_VIEWS=1`, flipped by the A/B: the views then end
//! a run as the tables did, though the shaders are the same).
//!
//! Order: the run is made when it ends, so the host commands the calls after
//! the first needed to be ready (a texture's transition to the state the
//! shaders read it in, a copy) come before the execution of all of them. For
//! draws that store to no buffer, with the same targets, that moves a
//! transition earlier than draws that never read that texture, or read it in the
//! same state: the depth target tested and also read is the exception, where
//! the state a draw needs differs, and a call that asks for another state than
//! the run's ends it ([`State::fold_depth`]); the root arguments of the calls after the
//! first are not set on the host list at all (their dwords are in the
//! commands).

use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use super::*;
use crate::indirect::batch::{self, SLOT};

/// Since the last line: runs made one, executions in them, root-argument
/// dwords in all their commands; runs ended by what the next call runs
/// with (pipeline, signature, roots, no room in the batch, its place),
/// by more root arguments than a command holds, by a call that asks the
/// depth target for another state, by another shape of user data; and,
/// of those ended by the roots, the ones ended by a root view.
pub(crate) static COUNTS: [AtomicU64; 9] = [const { AtomicU64::new(0) }; 9];

/// The line of what the folds came to, per frame (with the one of the
/// executions: [`crate::list::runs_line`]).
pub(crate) fn line(frames: f64) {
    let c: Vec<u64> = COUNTS.iter().map(|c| c.swap(0, Relaxed)).collect();
    if c.iter().sum::<u64>() == 0 || frames <= 0.0 {
        return;
    }
    let per = |v: u64| v as f64 / frames;
    tracing::info!(
        "folded executions per frame (VICTORY_INDIRECT_FOLD): {:.0} runs of {:.1} executions on average, {:.0} executions made {:.0} fewer, {:.1} root-argument dwords a command; runs ended by {:.0} other roots or pipeline ({:.0} of them by a root view), {:.0} no place, {:.0} too many arguments, {:.0} depth state, {:.0} another shape",
        per(c[0]),
        c[1] as f64 / c[0].max(1) as f64,
        per(c[1]),
        per(c[1].saturating_sub(c[0])),
        c[2] as f64 / c[1].max(1) as f64,
        per(c[3]),
        per(c[8]),
        per(c[4]),
        per(c[5]),
        per(c[6]),
        per(c[7]),
    );
}

/// The most executions of one run (a batch holds 256 jobs).
pub(crate) const MOST: u32 = 64;

/// Whether the root views the shaders read through the view heap may differ
/// in a run (`VICTORY_FOLD_VIEWS=1`; the A/B flips it). Read once for each
/// run, at its head ([`Run::views`]).
fn views_on() -> bool {
    crate::ab::live_flag!("VICTORY_FOLD_VIEWS")
}

/// The title's root parameters of the root SRVs the shader of `t` reads
/// through the view heap.
pub(super) fn indexed_slots(t: &crate::pipeline::Translated) -> impl Iterator<Item = u8> + '_ {
    t.host
        .user_data
        .iter()
        .filter(|u| u.root == RootType::Other(victory_shader::emit::USER_ROOT_VIEW_INDEX))
        .map(|u| u.slot)
}

/// Whether a shader of the stage reads a root SRV through the view heap.
pub(super) fn indexes_views(stage: &crate::pipeline::host::Stage) -> bool {
    indexed_slots(stage.translated()).next().is_some()
}

/// Where the host is in making a run.
#[derive(Default)]
pub(super) enum Mode {
    #[default]
    Off,
    /// The stream says this many executions come (the next first) that
    /// may be one run: the next one begins it, if it can.
    Planned(u32),
    On(Box<Run>),
}

/// A run being made: the calls so far, their user data, their places.
pub(super) struct Run {
    /// What the stream still has to come of the run it planned.
    left: u32,
    // What the run is.
    kind: u32,
    /// The title's stride (for a run of one, which is made as it always was).
    stride: u32,
    compute: bool,
    pso: usize,
    root: ID3D12RootSignature,
    /// The title's root views, in the order of the stages.
    key: Vec<u64>,
    /// Whether the root views read through the view heap may differ in the
    /// run ([`views_on`] at its head); else `key` has them too.
    views: bool,
    // Where its records are.
    scratch: Option<ID3D12Resource>,
    table: (usize, u32),
    /// The place of the first call's record in the scratch buffer.
    first: u64,
    /// The places of the calls, one each.
    ats: Vec<u64>,
    // The user data.
    /// (root parameter, dwords) of each block the head set.
    layout: Vec<(u32, u32)>,
    /// The values of all calls, one call after the other.
    values: Vec<u32>,
    /// Per block, the dwords that differ between calls (bit n: dword n; a
    /// root signature holds 64 dwords).
    vary: Vec<u64>,
    calls: u32,
    // The call being prepared.
    next_vary: Vec<u64>,
    capturing: bool,
    cur_layout: Vec<(u32, u32)>,
    cur: Vec<u32>,
    cur_at: u64,
    /// The host state of the depth target the head asked for, and one a
    /// later call asked for that is another.
    depth: u32,
    late_depth: Option<u32>,
}

/// The root arguments the commands of a run set: per block, each stretch of
/// dwords that differ (bits of `vary`), (root parameter, first dword,
/// dwords). Only the dwords that differ are set: a command has room for few.
pub(crate) fn ranges(layout: &[(u32, u32)], vary: &[u64]) -> Vec<(u32, u32, u32)> {
    let mut out = Vec::new();
    for (&(param, dwords), &mask) in layout.iter().zip(vary) {
        let mut d = 0;
        while d < dwords.min(64) {
            if mask >> d & 1 == 0 {
                d += 1;
                continue;
            }
            let first = d;
            while d < dwords.min(64) && mask >> d & 1 == 1 {
                d += 1;
            }
            out.push((param, first, d - first));
        }
    }
    out
}

/// The dwords of those ranges.
fn width(ranges: &[(u32, u32, u32)]) -> u32 {
    ranges.iter().map(|r| r.2).sum()
}

/// What `vary` (per block, the dwords that differ from the head's `head`) becomes with `call` in the
/// run, in `out`.
fn varying_into(layout: &[(u32, u32)], head: &[u32], vary: &[u64], call: &[u32], out: &mut Vec<u64>) {
    out.clear();
    out.extend_from_slice(vary);
    let mut at = 0usize;
    for (b, &(_, dwords)) in layout.iter().enumerate() {
        for d in 0..dwords.min(64) as usize {
            if head[at + d] != call[at + d] {
                out[b] |= 1 << d;
            }
        }
        at += dwords as usize;
    }
}

/// [`varying_into`] in a new vector.
#[cfg(test)]
pub(crate) fn varying(layout: &[(u32, u32)], head: &[u32], vary: &[u64], call: &[u32]) -> Vec<u64> {
    let mut out = Vec::with_capacity(vary.len());
    varying_into(layout, head, vary, call, &mut out);
    out
}

/// The dwords of the ranges `vary` makes.
fn varying_width(vary: &[u64]) -> u32 {
    vary.iter().map(|m| m.count_ones()).sum()
}

/// The words a command of a run puts in front of its record: the dwords of
/// `ranges` out of `call`'s blocks (laid out as `layout`).
pub(crate) fn command_words(layout: &[(u32, u32)], call: &[u32], ranges: &[(u32, u32, u32)]) -> Vec<u32> {
    let mut words = Vec::with_capacity(width(ranges) as usize);
    let mut at = 0usize;
    for &(param, dwords) in layout {
        for &(_, first, count) in ranges.iter().filter(|r| r.0 == param) {
            words.extend_from_slice(&call[at + first as usize..at + (first + count) as usize]);
        }
        at += dwords as usize;
    }
    words
}

/// The words a command can put before a record of `kind`: a place is
/// [`SLOT`] bytes.
fn room(kind: u32) -> u32 {
    (SLOT / 4).saturating_sub(batch::words(kind).unwrap_or(SLOT / 4))
}

impl Run {
    /// The block layout of one call's values.
    fn total(&self) -> usize {
        self.layout.iter().map(|b| b.1 as usize).sum()
    }

    /// A run with no call yet: `left` more calls come of what the stream planned.
    fn new(left: u32, (kind, stride): (u32, u32), (compute, pso): (bool, usize), root: ID3D12RootSignature, key: Vec<u64>, views: bool) -> Run {
        Run {
            left,
            kind,
            stride,
            compute,
            pso,
            root,
            key,
            views,
            scratch: None,
            table: (0, 0),
            first: 0,
            ats: Vec::new(),
            layout: Vec::new(),
            values: Vec::new(),
            vary: Vec::new(),
            calls: 0,
            next_vary: Vec::new(),
            capturing: true,
            cur_layout: Vec::new(),
            cur: Vec::new(),
            cur_at: 0,
            depth: 0,
            late_depth: None,
        }
    }

    /// A run of the same kind and pipeline whose head is the call being
    /// prepared.
    fn successor(&self, left: u32) -> Run {
        Run::new(
            left,
            (self.kind, self.stride),
            (self.compute, self.pso),
            self.root.clone(),
            self.key.clone(),
            self.views,
        )
    }
}

/// The command signature of a run of `kind` whose commands set `ranges`
/// (root parameter, first dword, dwords) of the root signature `root` and
/// then execute: a command is a place of the batch's scratch buffer.
fn signature(root: &ID3D12RootSignature, kind: u32, ranges: &[(u32, u32, u32)]) -> Option<ID3D12CommandSignature> {
    type Key = (usize, u32, Vec<(u32, u32, u32)>);
    static MADE: std::sync::Mutex<Vec<(Key, ID3D12RootSignature, ID3D12CommandSignature)>> = std::sync::Mutex::new(Vec::new());
    let key: Key = (root.as_raw() as usize, kind, ranges.to_vec());
    let mut made = MADE.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, _, s)) = made.iter().find(|(k, ..)| *k == key) {
        return Some(s.clone());
    }
    let signature = make_signature(&crate::host::host()?.device, root, kind, ranges)?;
    // (Kept with the root signature it was made with.)
    made.push((key, root.clone(), signature.clone()));
    Some(signature)
}

/// [`signature`] on `device`.
pub(crate) fn make_signature(
    device: &ID3D12Device,
    root: &ID3D12RootSignature,
    kind: u32,
    ranges: &[(u32, u32, u32)],
) -> Option<ID3D12CommandSignature> {
    let mut arguments: Vec<D3D12_INDIRECT_ARGUMENT_DESC> = ranges
        .iter()
        .map(|&(param, first, count)| D3D12_INDIRECT_ARGUMENT_DESC {
            Type: D3D12_INDIRECT_ARGUMENT_TYPE_CONSTANT,
            Anonymous: D3D12_INDIRECT_ARGUMENT_DESC_0 {
                Constant: D3D12_INDIRECT_ARGUMENT_DESC_0_1 {
                    RootParameterIndex: param,
                    DestOffsetIn32BitValues: first,
                    Num32BitValuesToSet: count,
                },
            },
        })
        .collect();
    arguments.push(D3D12_INDIRECT_ARGUMENT_DESC { Type: D3D12_INDIRECT_ARGUMENT_TYPE(kind as i32), ..Default::default() });
    let desc = D3D12_COMMAND_SIGNATURE_DESC {
        ByteStride: SLOT,
        NumArgumentDescs: arguments.len() as u32,
        pArgumentDescs: arguments.as_ptr(),
        NodeMask: 0,
    };
    let mut made: Option<ID3D12CommandSignature> = None;
    // SAFETY: a description of arguments that live through the call, and a live root signature
    // (none is needed where the commands set no root arguments).
    let result = unsafe {
        device.CreateCommandSignature(&desc, if ranges.is_empty() { None } else { Some(root) }, &mut made)
    };
    if let Err(e) = result {
        once!(error = %e, kind, "host command signature of folded executions refused");
        return None;
    }
    made
}

/// Whether the executions of pipeline `p` may be folded: draws or mesh
/// dispatches whose shaders store to no buffer and read no title memory by
/// address, so that nothing the commands do depends on what an earlier
/// one wrote.
pub(super) fn eligible(p: &HostPipeline, kind: u32) -> bool {
    const DISPATCH: u32 = 2;
    kind != DISPATCH
        && !p.compute
        && p.stages.iter().all(|s| {
            let t = s.translated();
            !t.stores && !t.host.root_srvs.contains(&victory_shader::emit::SLOT_MEMORY)
        })
}

impl State {
    /// The stream says `n` executions come that may be one run.
    pub(crate) fn fold_begin(&mut self, n: u32) {
        self.fold = Mode::Planned(n.min(MOST));
    }

    /// The run of the stream is over: made, if one is open.
    pub(crate) fn fold_end(&mut self, list: &HostList) {
        if let Mode::On(run) = std::mem::take(&mut self.fold) {
            self.fold_make(list, *run);
        }
        self.fold = Mode::Off;
    }

    /// The title's root views a run needs the same (the descriptor tables
    /// of the host's root signature that hold them cannot change in a run),
    /// in the order of the stages; and, unless `views` (the run folds
    /// them), the root SRVs the shaders read through the view heap, which
    /// are dwords of the user data and could differ.
    fn fold_views<'a>(&'a self, p: &'a HostPipeline, views: bool) -> impl Iterator<Item = u64> + 'a {
        let roots = self.roots_ref(p.compute);
        let address = move |slot: u8| match roots.get(usize::from(slot)) {
            Some(&Root::View(address)) => address,
            _ => 0,
        };
        let tables = p
            .stages
            .iter()
            .flat_map(|s| s.root_cbvs.iter().chain(&s.root_srvs).chain(&s.root_uavs))
            .map(move |&(slot, _)| address(slot));
        let indexed = p
            .stages
            .iter()
            .filter(move |_| !views)
            .flat_map(|s| indexed_slots(s.translated()))
            .map(move |slot| address(slot));
        tables.chain(indexed)
    }

    /// An execution that may fold arrives (`kind`, the title's `stride`,
    /// pipeline `p`): whether this is one that is (the head of) a run, and
    /// whether the call is to be prepared as a call of one. A run is ended
    /// here by a call that cannot join it.
    pub(super) fn fold_step(&mut self, list: &HostList, p: &HostPipeline, (kind, stride): (u32, u32), can: bool) -> bool {
        let mut left = match std::mem::take(&mut self.fold) {
            Mode::Off => return false,
            Mode::Planned(n) => n,
            Mode::On(run) => {
                // The batch the records of the run are in: it takes another
                // job, and no other pass came in between.
                let batched = self.batch.as_ref().is_some_and(|b| {
                    run.scratch.as_ref().is_some_and(|s| s.as_raw() == b.scratch.as_raw()) && b.table.room() > 0
                }) && list.used(BATCH_USE);
                let views_same = self.fold_views(p, run.views).eq(run.key.iter().copied());
                let same = run.left > 0
                    && can
                    && batched
                    && run.pso == p.pso.as_raw() as usize
                    && run.kind == kind
                    && run.stride == stride
                    && views_same
                    && self.host_pipeline == run.pso
                    && self.host_roots[usize::from(run.compute)] == run.root.as_raw() as usize;
                if same {
                    let mut run = run;
                    run.left -= 1;
                    run.capturing = true;
                    run.cur_layout.clear();
                    run.cur.clear();
                    self.fold = Mode::On(run);
                    return true;
                }
                COUNTS[3].fetch_add(1, Relaxed);
                if !views_same {
                    COUNTS[8].fetch_add(1, Relaxed);
                }
                let left = run.left;
                self.fold_make(list, *run);
                left
            }
        };
        // A run begins here, if it can.
        if left < 2 || !can || batches() == 0 {
            left = 0;
        }
        if left == 0 {
            self.fold = Mode::Off;
            return false;
        }
        // (Read once, here: the run keeps what it began with.)
        let views = views_on();
        let mut run = Run::new(
            left - 1,
            (kind, stride),
            (p.compute, p.pso.as_raw() as usize),
            p.root.clone(),
            self.fold_views(p, views).collect(),
            views,
        );
        run.ats.reserve(8);
        self.fold = Mode::On(Box::new(run));
        true
    }

    /// The call's record has its place (`at`) in `scratch` (of the batch):
    /// whether it is where the run needs it, one place after the last.
    /// If not, the run ends and the call begins the next.
    pub(super) fn fold_placed(&mut self, list: &HostList, scratch: &ID3D12Resource, at: u64) -> bool {
        let table = self.batch.as_ref().map_or((0, 0), |b| b.table.memory());
        let Mode::On(run) = &mut self.fold else { return false };
        let fits = run.scratch.as_ref().is_none_or(|s| {
            s.as_raw() == scratch.as_raw() && at == run.first + u64::from(run.calls) * u64::from(SLOT) && run.table == table
        });
        if fits {
            if run.scratch.is_none() {
                run.scratch = Some(scratch.clone());
                run.table = table;
                run.first = at;
            }
            run.cur_at = at;
            return true;
        }
        COUNTS[4].fetch_add(1, Relaxed);
        let Mode::On(old) = std::mem::take(&mut self.fold) else { return false };
        let mut next = old.successor(old.left);
        self.fold_make(list, *old);
        next.scratch = Some(scratch.clone());
        next.table = table;
        next.first = at;
        next.cur_at = at;
        self.fold = Mode::On(Box::new(next));
        true
    }

    /// `bind` sets the user data of a stage (`param`, `values`): noted for
    /// the call being prepared, and whether the host list is not to be told
    /// (a call after the head: the commands of the run set what differs).
    pub(super) fn fold_note(&mut self, compute: bool, param: u32, values: &[u32]) -> bool {
        let Mode::On(run) = &mut self.fold else { return false };
        if !run.capturing || run.compute != compute {
            return false;
        }
        run.cur_layout.push((param, values.len() as u32));
        run.cur.extend_from_slice(values);
        run.calls > 0
    }

    /// `prepare_for` is about to move the depth target to `state`: whether
    /// that waits, because the run holds draws that need another state
    /// (ended here, and moved after it).
    pub(super) fn fold_depth(&mut self, state: u32) -> bool {
        let Mode::On(run) = &mut self.fold else { return false };
        if !run.capturing {
            return false;
        }
        if run.calls == 0 {
            run.depth = state;
            false
        } else if run.depth != state {
            run.late_depth = Some(state);
            true
        } else {
            false
        }
    }

    /// The call was prepared (`ran`: it can run): it joins the run or ends it.
    pub(super) fn fold_after(&mut self, list: &HostList, ran: bool) {
        let Some((run, late)) = self.fold_judge(ran) else { return };
        // This call ends the run, and is the head of the next: what it
        // needed that the host list did not get is told now.
        self.fold_make(list, *run);
        if let Some(state) = late {
            crate::list::ensure_state(list, self.depth as This, state);
        }
        self.fold_tell(&list.list);
    }

    /// [`State::fold_after`] without the host list: the call joins the run (`None`), or ends it and begins the next, which
    /// holds the call (the run that ended and the state the depth target is to be moved to after it).
    fn fold_judge(&mut self, ran: bool) -> Option<(Box<Run>, Option<u32>)> {
        let Mode::On(mut run) = std::mem::take(&mut self.fold) else { return None };
        run.capturing = false;
        if !ran {
            // A head that did not run is no run; a call after it that did not run is not in it.
            self.fold = if run.calls == 0 { Mode::Planned(run.left) } else { Mode::On(run) };
            return None;
        }
        if run.calls == 0 {
            run.layout = std::mem::take(&mut run.cur_layout);
            run.values = std::mem::take(&mut run.cur);
            run.vary = vec![0; run.layout.len()];
            run.ats.push(run.cur_at);
            run.calls = 1;
            self.fold = Mode::On(run);
            return None;
        }
        // (A root signature holds 64 dwords: the masks of what differs are 64 bits.)
        let shape = run.cur_layout == run.layout && run.layout.iter().all(|b| b.1 <= 64);
        if shape {
            let total = run.total();
            let mut next = std::mem::take(&mut run.next_vary);
            varying_into(&run.layout, &run.values[..total], &run.vary, &run.cur, &mut next);
            run.next_vary = next;
        }
        let host_same = self.host_pipeline == run.pso && self.host_roots[usize::from(run.compute)] == run.root.as_raw() as usize;
        let why = if run.late_depth.is_some() {
            6
        } else if !shape {
            7
        } else if varying_width(&run.next_vary) > room(run.kind) {
            5
        } else if !host_same {
            3
        } else {
            0
        };
        if why == 0 {
            std::mem::swap(&mut run.vary, &mut run.next_vary);
            let cur = std::mem::take(&mut run.cur);
            run.values.extend_from_slice(&cur);
            run.cur = cur;
            run.ats.push(run.cur_at);
            run.calls += 1;
            self.fold = Mode::On(run);
            return None;
        }
        COUNTS[why].fetch_add(1, Relaxed);
        let late = run.late_depth;
        let mut next = run.successor(run.left);
        next.layout = std::mem::take(&mut run.cur_layout);
        next.values = std::mem::take(&mut run.cur);
        next.vary = vec![0; next.layout.len()];
        next.ats.push(run.cur_at);
        next.scratch = run.scratch.clone();
        next.table = run.table;
        next.first = run.cur_at;
        next.calls = 1;
        next.capturing = false;
        next.depth = late.unwrap_or(run.depth);
        self.fold = Mode::On(Box::new(next));
        Some((run, late))
    }

    /// The call that ended a run (the head of the one now open) did not set its user data on the host list: sets it, after
    /// the run that ended. The list then holds these dwords, and the filter is told (`root_changed`): the next call that
    /// asks for the dwords of the call before (a material that comes back after another) must set them again.
    fn fold_tell(&mut self, l: &ID3D12GraphicsCommandList) {
        let Mode::On(run) = &mut self.fold else { return };
        let (layout, values, compute) = (std::mem::take(&mut run.layout), std::mem::take(&mut run.values), run.compute);
        self.fold_set(l, &layout, &values, compute);
        let mut at = 0usize;
        for &(param, dwords) in &layout {
            self.root_told(compute, param, &values[at..at + dwords as usize]);
            at += dwords as usize;
        }
        if let Mode::On(run) = &mut self.fold {
            run.layout = layout;
            run.values = values;
        }
    }

    /// Sets the user data blocks `layout` of `values` on the host list.
    fn fold_set(&self, l: &ID3D12GraphicsCommandList, layout: &[(u32, u32)], values: &[u32], compute: bool) {
        let mut at = 0usize;
        for &(param, dwords) in layout {
            let block = &values[at..at + dwords as usize];
            // SAFETY: an open list; the block covers the parameter's dwords.
            unsafe {
                if compute {
                    l.SetComputeRoot32BitConstants(param, dwords, block.as_ptr().cast(), 0);
                } else {
                    l.SetGraphicsRoot32BitConstants(param, dwords, block.as_ptr().cast(), 0);
                }
            }
            at += dwords as usize;
        }
    }

    /// Makes the run: its commands' root arguments are put in the table
    /// the batch's pass reads, and one `ExecuteIndirect` runs them all.
    fn fold_make(&mut self, list: &HostList, run: Run) {
        let n = run.calls;
        let Some(scratch) = run.scratch.clone().filter(|_| n > 0) else { return };
        // A run of one is the execution it always was.
        let single = n == 1;
        let ranges = if single { Vec::new() } else { ranges(&run.layout, &run.vary) };
        let sig = if single {
            host_signature(run.kind, run.stride)
        } else {
            signature(&run.root, run.kind, &ranges)
        };
        let Some(sig) = sig else { return };
        if !single {
            let total = run.total();
            for (k, &at) in run.ats.iter().enumerate() {
                let words = command_words(&run.layout, &run.values[k * total..(k + 1) * total], &ranges);
                // SAFETY: the table of the pass the records are in, live
                // until the list is reset, which the pass has not run.
                unsafe { batch::fold_words(run.table.0, run.table.1, at, &words) };
            }
            COUNTS[0].fetch_add(1, Relaxed);
            COUNTS[1].fetch_add(u64::from(n), Relaxed);
            COUNTS[2].fetch_add(u64::from(width(&ranges)) * u64::from(n), Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        list.note_op(|| format!("indirect kind {} max {n} folded at {:#x}", run.kind, run.first));
        let mesh = run.kind == 10;
        if mesh {
            self.mesh_scissor(list, true);
        }
        // SAFETY: an open list with the pipeline, root signature and what the
        // shaders read set by the calls; the scratch buffer is readable as
        // arguments, and the table's commands are in it.
        unsafe { list.list.ExecuteIndirect(&sig, n, &scratch, run.first, None::<&ID3D12Resource>, 0) };
        if mesh {
            self.mesh_scissor(list, false);
        }
        if !single {
            // What the commands set is no longer what the host list holds.
            self.forget_roots();
        }
    }
}

/// What a test needs to play a stream of executions through the state of a draw (the user data filter, the runs) on a
/// list of its own: the host list is a plain command list, and the executions are recorded by the test.
#[cfg(test)]
pub(crate) mod seam {
    use std::sync::atomic::Ordering;

    use super::*;

    /// The user data filter on (`VICTORY_STATE_FILTER=1`) for one test: the tests that set it run one at a time.
    pub(crate) struct Filter(#[allow(dead_code)] std::sync::MutexGuard<'static, ()>);

    impl Filter {
        pub(crate) fn on() -> Filter {
            let one = super::super::state_filter_tests::ONE_AT_A_TIME.lock().unwrap_or_else(|e| e.into_inner());
            super::super::TEST_STATE_FILTER.store(1, Ordering::Relaxed);
            Filter(one)
        }
    }

    impl Drop for Filter {
        fn drop(&mut self) {
            super::super::TEST_STATE_FILTER.store(u8::MAX, Ordering::Relaxed);
        }
    }

    /// The executions of one compute pipeline with one block of user data in root parameter 0 (a dispatch: the run
    /// logic is that of the draws, whose records and places the test does not need).
    pub(crate) struct Stream {
        state: State,
        root: ID3D12RootSignature,
    }

    impl Stream {
        const PSO: usize = 1;

        /// The host list has `root` and the pipeline set.
        pub(crate) fn new(root: &ID3D12RootSignature) -> Stream {
            let mut state = State::default();
            state.host_pipeline = Self::PSO;
            state.host_roots[1] = root.as_raw() as usize;
            Stream { state, root: root.clone() }
        }

        /// The stream says `n` executions come that may be one run.
        pub(crate) fn plan(&mut self, n: u32) {
            self.state.fold_begin(n);
        }

        /// An execution of the plan arrives with the user data `values`: it is prepared as `State::dispatch` does, and
        /// joins the run or ends it, in which case `execute` records the execution of the run that ended (the host
        /// list then holds what the head of that run set), before the list is given the dwords of this call.
        pub(crate) fn call(&mut self, l: &ID3D12GraphicsCommandList, values: &[u32], execute: &mut dyn FnMut()) {
            match std::mem::take(&mut self.state.fold) {
                Mode::Off => {}
                Mode::Planned(n) => {
                    let run = Run::new(n - 1, (2, 32), (true, Self::PSO), self.root.clone(), Vec::new(), false);
                    self.state.fold = Mode::On(Box::new(run));
                }
                Mode::On(mut run) => {
                    run.left -= 1;
                    run.capturing = true;
                    run.cur_layout.clear();
                    run.cur.clear();
                    self.state.fold = Mode::On(run);
                }
            }
            self.state.set_user_data(l, true, 0, values);
            if let Mode::On(run) = &mut self.state.fold {
                run.cur_at = u64::from(run.calls) * u64::from(SLOT);
            }
            if let Some((run, late)) = self.state.fold_judge(true) {
                assert_eq!((run.calls, late), (1, None), "the runs of this stream are of one");
                execute();
                self.state.fold_tell(l);
            }
        }

        /// The stream's run is over (`State::fold_end`): `execute` records its execution.
        pub(crate) fn end(&mut self, execute: &mut dyn FnMut()) {
            if let Mode::On(run) = std::mem::take(&mut self.state.fold) {
                assert_eq!(run.calls, 1, "the runs of this stream are of one");
                execute();
            }
            self.state.fold = Mode::Off;
        }

        /// An execution that is not folded: its user data is set as `bind` sets it.
        pub(crate) fn plain(&mut self, l: &ID3D12GraphicsCommandList, values: &[u32]) {
            self.state.set_user_data(l, true, 0, values);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// What a command sets is what the title's calls set: the ranges cover the dwords that differ, the head's values stand for the
    /// rest, and every call's words are where its dwords were.
    #[test]
    fn commands_set_what_the_calls_set() {
        let layout = [(3u32, 6u32), (5, 4)];
        // Random-looking calls: a few dwords differ per call.
        let mut state = 0x2545_f491_u32;
        let mut next = move || {
            state = state.wrapping_mul(1_664_525).wrapping_add(1_013_904_223);
            state >> 8
        };
        for round in 0..300 {
            let calls = 2 + next() as usize % 20;
            let head: Vec<u32> = (0..10).map(|_| next()).collect();
            let mut values = head.clone();
            let mut vary = vec![0u64; layout.len()];
            let differing = [[round % 7 % 6, (round * 3) % 6], [(round * 5) % 4, 9]];
            for _ in 1..calls {
                let mut call = head.clone();
                for (b, d) in differing.iter().enumerate() {
                    for &dword in d.iter().filter(|&&d| d < layout[b].1) {
                        if next() % 3 != 0 {
                            call[(if b == 0 { 0 } else { 6 }) + dword as usize] = next();
                        }
                    }
                }
                vary = varying(&layout, &head, &vary, &call);
                values.extend_from_slice(&call);
            }
            let ranges = ranges(&layout, &vary);
            // Applied to what the head left on the host list, the words of each command give the call.
            for k in 0..calls {
                let call = &values[k * 10..(k + 1) * 10];
                let words = command_words(&layout, call, &ranges);
                assert_eq!(words.len() as u32, width(&ranges));
                let mut held = head.clone();
                let mut w = words.iter();
                let mut at = 0usize;
                for &(param, dwords) in &layout {
                    for &(_, first, count) in ranges.iter().filter(|r| r.0 == param) {
                        for d in first..first + count {
                            held[at + d as usize] = *w.next().unwrap();
                        }
                    }
                    at += dwords as usize;
                }
                assert_eq!(held, call, "round {round} call {k}: ranges {ranges:?}");
            }
            // Nothing differs outside the ranges.
            for k in 0..calls {
                let mut at = 0usize;
                for (b, &(_, dwords)) in layout.iter().enumerate() {
                    for d in 0..dwords {
                        let inside = vary[b] >> d & 1 == 1;
                        assert!(inside || values[k * 10 + at + d as usize] == head[at + d as usize], "round {round}");
                    }
                    at += dwords as usize;
                }
            }
        }
    }

    /// A run with nothing that differs sets nothing; one dword that differs
    /// is one range; stretches of dwords that differ are one range each,
    /// and two blocks that differ are two.
    #[test]
    fn ranges_are_the_dwords_that_differ() {
        let layout = [(2u32, 8u32), (4, 3)];
        assert!(ranges(&layout, &[0, 0]).is_empty());
        assert_eq!(ranges(&layout, &[1 << 3, 0]), [(2, 3, 1)]);
        assert_eq!(ranges(&layout, &[0b11_1110, 0b111]), [(2, 1, 5), (4, 0, 3)]);
        assert_eq!(width(&ranges(&layout, &[0b11_1110, 0b111])), 8);
        // A dword that does not differ between two that do is not set (the room of a command is small): the
        // index of a view in the heap, far from the constants that differ with it.
        let far = ranges(&layout, &[0b1000_0110, 0b101]);
        assert_eq!(far, [(2, 1, 2), (2, 7, 1), (4, 0, 1), (4, 2, 1)]);
        assert_eq!(width(&far), 5);
        assert_eq!(varying_width(&[0b1000_0110, 0b101]), width(&far));
        // Bits past the dwords of a block are not dwords.
        assert!(ranges(&layout, &[1 << 9, 1 << 4]).is_empty());
    }

    /// The dwords that differ from the head are the bits, whichever call they differ in.
    #[test]
    fn varying_marks_the_dwords_that_differ_from_the_head() {
        let layout = [(0u32, 3u32), (1, 2)];
        let head = [1u32, 2, 3, 4, 5];
        let first = varying(&layout, &head, &[0, 0], &[1, 9, 3, 4, 5]);
        assert_eq!(first, [0b010, 0]);
        let second = varying(&layout, &head, &first, &[1, 2, 3, 4, 6]);
        assert_eq!(second, [0b010, 0b10]);
        // A call that is the head again marks nothing, and what was marked stays.
        assert_eq!(varying(&layout, &head, &second, &head), second);
        assert_eq!(ranges(&layout, &second), [(0, 1, 1), (1, 1, 1)]);
    }

    /// A command holds the root arguments and the record in its place.
    #[test]
    fn a_command_is_a_place() {
        for (kind, most) in [(0u32, 12u32), (1, 11), (2, 13), (10, 13)] {
            assert_eq!(room(kind), most, "kind {kind}");
        }
        assert_eq!(room(3), 0, "a kind that is not known has no room");
    }
}
