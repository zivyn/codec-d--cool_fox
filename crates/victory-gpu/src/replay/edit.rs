//! Experiments on a list's commands: the same list with something left out
//! or put otherwise.

use std::collections::{HashMap, HashSet};

use super::wire::{ALL, Barrier, Cmd, NONE, Va};

/// The buffer the clamped arguments of all clamp passes lie in, in lists
/// edited by [`clamps`].
pub const RESULTS: u32 = u32::MAX - 1;

const INDIRECT_ARGUMENT: i32 = 0x200;
const UNORDERED_ACCESS: i32 = 0x8;
const COPY_SOURCE: i32 = 0x800;

/// Drops what runs a pipeline of `psos`; how many commands that were.
pub fn skip_pipelines(cmds: Vec<Cmd>, psos: &HashSet<u32>) -> (Vec<Cmd>, u32) {
    let mut bound = NONE;
    let mut dropped = 0;
    let out = cmds
        .into_iter()
        .filter(|c| {
            if let Cmd::Pipeline { pso } = c {
                bound = *pso;
            }
            let drop = c.runs_shaders() && psos.contains(&bound);
            dropped += u32::from(drop);
            !drop
        })
        .collect();
    (out, dropped)
}

/// Drops unordered-access barriers; how many.
pub fn no_uav_barriers(cmds: Vec<Cmd>) -> (Vec<Cmd>, u32) {
    let mut dropped = 0;
    let out = cmds
        .into_iter()
        .filter_map(|c| match c {
            Cmd::Barriers { mut all } => {
                let n = all.len();
                all.retain(|b| !matches!(b, Barrier::Uav { .. }));
                dropped += (n - all.len()) as u32;
                (!all.is_empty()).then_some(Cmd::Barriers { all })
            }
            c => Some(c),
        })
        .collect();
    (out, dropped)
}

/// Barriers only: what a list left out still does to the states of
/// resources.
pub fn barriers_only(cmds: Vec<Cmd>) -> Vec<Cmd> {
    cmds.into_iter().filter(|c| matches!(c, Cmd::Barriers { .. })).collect()
}

/// Joins the barrier calls with no work between them into one; how many
/// calls that saved.
pub fn merge_barriers(cmds: Vec<Cmd>) -> (Vec<Cmd>, u32) {
    let mut out: Vec<Cmd> = Vec::with_capacity(cmds.len());
    let mut open: Option<usize> = None;
    let mut saved = 0;
    for c in cmds {
        match c {
            Cmd::Barriers { all } => {
                if let Some(i) = open
                    && let Cmd::Barriers { all: have } = &mut out[i]
                {
                    for b in all {
                        join(have, b);
                    }
                    saved += 1;
                } else {
                    open = Some(out.len());
                    let mut have = Vec::new();
                    for b in all {
                        join(&mut have, b);
                    }
                    out.push(Cmd::Barriers { all: have });
                }
            }
            c => {
                if c.works() || matches!(c, Cmd::BeginQuery { .. } | Cmd::EndQuery { .. }) {
                    open = None;
                }
                out.push(c);
            }
        }
    }
    out.retain(|c| !matches!(c, Cmd::Barriers { all } if all.is_empty()));
    (out, saved)
}

/// Adds a barrier to a call's.
fn join(have: &mut Vec<Barrier>, b: Barrier) {
    match b {
        Barrier::Uav { res, .. } => {
            let covered = have.iter().any(|h| matches!(h, Barrier::Uav { res: r, .. } if *r == res || *r == NONE));
            if covered {
                return;
            }
            if res == NONE {
                have.retain(|h| !matches!(h, Barrier::Uav { .. }));
            }
            have.push(b);
        }
        Barrier::Transition { flags: 0, res, sub, before, after } => {
            // The last one of this resource, if it is of the same part of it.
            let last = have.iter().rposition(|h| matches!(h, Barrier::Transition { res: r, .. } if *r == res));
            if let Some(i) = last
                && let Barrier::Transition { flags: 0, sub: s, before: first, after: until, .. } = have[i]
                && s == sub
                && until == before
            {
                if first != after {
                    have[i] = Barrier::Transition { flags: 0, res, sub, before: first, after };
                } else {
                    have.remove(i);
                    if after == UNORDERED_ACCESS {
                        // Still a wait for the writes before.
                        join(have, Barrier::Uav { flags: 0, res });
                    }
                }
                return;
            }
            have.push(b);
        }
        b => have.push(b),
    }
}

/// A clamp pass of ours found in a list.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Clamp {
    /// The command that dispatches it.
    pub at: usize,
    /// The scratch buffer it writes the arguments to, from its start.
    pub scratch: u32,
    pub bytes: u64,
    /// Where its arguments are in [`RESULTS`].
    pub result: u64,
}

/// The clamp passes of a list (`psos`: the clamp's pipelines), given places
/// in the results from `next` on.
pub fn find_clamps(cmds: &[Cmd], psos: &HashSet<u32>, next: &mut u64) -> Vec<Clamp> {
    let (mut bound, mut constants, mut to) = (NONE, [0u32; 6], None);
    let mut found = Vec::new();
    for (at, c) in cmds.iter().enumerate() {
        match c {
            Cmd::Pipeline { pso } => bound = *pso,
            Cmd::Constants { graphics: false, param: 0, offset, values } => {
                for (i, v) in values.iter().enumerate() {
                    if let Some(c) = constants.get_mut(*offset as usize + i) {
                        *c = *v;
                    }
                }
            }
            // The clamp's parameters: constants (kind, stride, records,
            // ...), arguments, count, then where it writes.
            Cmd::RootView { graphics: false, param: 3, va: Va::At { res, offset: 0 }, .. } => to = Some(*res),
            Cmd::Dispatch { .. } if psos.contains(&bound) => {
                if let Some(scratch) = to {
                    let bytes = u64::from(constants[1]) * u64::from(constants[2]);
                    found.push(Clamp { at, scratch, bytes, result: *next });
                    *next += bytes.next_multiple_of(256);
                }
            }
            _ => {}
        }
    }
    found
}

/// The list with a copy of what each clamp wrote into the results.
pub fn keep_clamped(cmds: Vec<Cmd>, clamps: &[Clamp]) -> Vec<Cmd> {
    let mut out = Vec::with_capacity(cmds.len() + 3 * clamps.len());
    let mut open: Option<Clamp> = None;
    for (at, c) in cmds.into_iter().enumerate() {
        if let Some(clamp) = clamps.iter().find(|c| c.at == at) {
            open = Some(*clamp);
        }
        let readable = |b: &Barrier, scratch: u32| matches!(b, Barrier::Transition { res, after: INDIRECT_ARGUMENT, .. } if *res == scratch);
        let done = match (&c, open) {
            (Cmd::Barriers { all }, Some(clamp)) => all.iter().any(|b| readable(b, clamp.scratch)),
            _ => false,
        };
        out.push(c);
        if done && let Some(clamp) = open.take() {
            let moved = |before, after| Cmd::Barriers {
                all: vec![Barrier::Transition { flags: 0, res: clamp.scratch, sub: ALL, before, after }],
            };
            out.push(moved(INDIRECT_ARGUMENT, COPY_SOURCE));
            out.push(Cmd::CopyBuffer { dst: RESULTS, dst_at: clamp.result, src: clamp.scratch, src_at: 0, len: clamp.bytes });
            out.push(moved(COPY_SOURCE, INDIRECT_ARGUMENT));
        }
    }
    out
}

/// The list without its clamp passes and the barriers of their scratch
/// buffers; what they bounded is read from the results instead.
pub fn skip_clamps(cmds: Vec<Cmd>, clamps: &[Clamp]) -> Vec<Cmd> {
    let scratches: HashSet<u32> = clamps.iter().map(|c| c.scratch).collect();
    let by_at: HashMap<usize, &Clamp> = clamps.iter().map(|c| (c.at, c)).collect();
    let mut last: HashMap<u32, u64> = HashMap::new();
    let mut out = Vec::with_capacity(cmds.len());
    for (at, c) in cmds.into_iter().enumerate() {
        if let Some(clamp) = by_at.get(&at) {
            last.insert(clamp.scratch, clamp.result);
            continue;
        }
        match c {
            Cmd::Barriers { mut all } => {
                let of_scratch = |b: &Barrier| matches!(b, Barrier::Transition { res, .. } if scratches.contains(res));
                if all.iter().any(of_scratch) {
                    // The waits made with the scratch buffer's moves are the
                    // clamp's too.
                    all.retain(|b| !of_scratch(b) && !matches!(b, Barrier::Uav { res: NONE, .. }));
                }
                if !all.is_empty() {
                    out.push(Cmd::Barriers { all });
                }
            }
            Cmd::Indirect { signature, most, args, at, count, count_at } if last.contains_key(&args) => {
                let result = last.get(&args).copied().unwrap_or(0);
                out.push(Cmd::Indirect { signature, most, args: RESULTS, at: result + at, count, count_at });
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pso(pso: u32) -> Cmd {
        Cmd::Pipeline { pso }
    }
    fn go() -> Cmd {
        Cmd::Dispatch { groups: [1, 1, 1] }
    }
    fn uav(res: u32) -> Barrier {
        Barrier::Uav { flags: 0, res }
    }
    fn moved(res: u32, before: i32, after: i32) -> Barrier {
        Barrier::Transition { flags: 0, res, sub: ALL, before, after }
    }
    fn barriers(all: &[Barrier]) -> Cmd {
        Cmd::Barriers { all: all.to_vec() }
    }

    #[test]
    fn pipelines_are_skipped() {
        let list = vec![pso(1), go(), pso(2), go(), go(), Cmd::CopyResource { dst: 1, src: 2 }, pso(1), go()];
        let (out, n) = skip_pipelines(list.clone(), &HashSet::from([2]));
        assert_eq!(n, 2);
        assert_eq!(out, vec![pso(1), go(), pso(2), Cmd::CopyResource { dst: 1, src: 2 }, pso(1), go()]);
        assert_eq!(skip_pipelines(list.clone(), &HashSet::new()), (list, 0));
    }

    #[test]
    fn uav_barriers_are_dropped() {
        let list = vec![barriers(&[uav(0)]), go(), barriers(&[uav(1), moved(1, 8, 4)])];
        assert_eq!(no_uav_barriers(list.clone()), (vec![go(), barriers(&[moved(1, 8, 4)])], 2));
        assert_eq!(barriers_only(list.clone()), vec![list[0].clone(), list[2].clone()]);
    }

    #[test]
    fn barriers_are_merged() {
        let list = vec![
            barriers(&[moved(1, 0, 8)]),
            pso(1),
            barriers(&[uav(2), moved(1, 8, 64)]),
            barriers(&[uav(0), uav(3)]),
            go(),
            // There and back again: nothing is left but the wait.
            barriers(&[moved(1, 8, 64)]),
            barriers(&[moved(1, 64, 8)]),
            go(),
            barriers(&[moved(4, 4, 128)]),
            barriers(&[moved(4, 128, 4)]),
        ];
        let (out, saved) = merge_barriers(list);
        assert_eq!(saved, 4);
        assert_eq!(out, vec![barriers(&[moved(1, 0, 64), uav(0)]), pso(1), go(), barriers(&[uav(1)]), go()]);
        // Parts of a resource are not mixed up.
        let part = |sub, before, after| Barrier::Transition { flags: 0, res: 1, sub, before, after };
        let list = vec![barriers(&[part(0, 4, 8)]), barriers(&[part(1, 4, 8)]), barriers(&[part(ALL, 8, 64)])];
        assert_eq!(merge_barriers(list).0, vec![barriers(&[part(0, 4, 8), part(1, 4, 8), part(ALL, 8, 64)])]);
    }

    #[test]
    fn clamps_are_found_kept_and_skipped() {
        let clamp = |scratch, stride, records| {
            vec![
                barriers(&[uav(0), moved(scratch, 0, 8)]),
                pso(9),
                Cmd::Constants { graphics: false, param: 0, offset: 0, values: vec![10, stride, records, 0, 0, 0] },
                Cmd::RootView { graphics: false, kind: 3, param: 3, va: Va::At { res: scratch, offset: 0 } },
                go(),
                barriers(&[uav(0), moved(scratch, 8, 0x200)]),
            ]
        };
        let run = |args, at| Cmd::Indirect { signature: 1, most: 1, args, at, count: 0, count_at: 0 };
        let mut list = clamp(5, 12, 2);
        list.extend([pso(1), run(5, 12), run(5, 0), run(6, 0)]);
        list.extend(clamp(5, 12, 100));
        list.extend([pso(1), run(5, 24), barriers(&[moved(5, 0x200, 0)])]);
        let mut next = 0;
        let found = find_clamps(&list, &HashSet::from([9]), &mut next);
        assert_eq!(found.iter().map(|c| (c.scratch, c.bytes, c.result)).collect::<Vec<_>>(), [(5, 24, 0), (5, 1200, 256)]);
        assert_eq!(next, 256 + 1280);
        let kept = keep_clamped(list.clone(), &found);
        assert_eq!(kept.len(), list.len() + 6);
        assert_eq!(kept[7], Cmd::CopyBuffer { dst: RESULTS, dst_at: 0, src: 5, src_at: 0, len: 24 });
        assert_eq!(kept.iter().filter(|c| matches!(c, Cmd::CopyBuffer { .. })).count(), 2);
        let skipped = skip_clamps(list, &found);
        assert!(!skipped.iter().any(|c| matches!(c, Cmd::Dispatch { .. } | Cmd::Barriers { .. })));
        let runs: Vec<(u32, u64)> =
            skipped.iter().filter_map(|c| if let Cmd::Indirect { args, at, .. } = c { Some((*args, *at)) } else { None }).collect();
        assert_eq!(runs, [(RESULTS, 12), (RESULTS, 0), (6, 0), (RESULTS, 256 + 24)]);
    }
}
