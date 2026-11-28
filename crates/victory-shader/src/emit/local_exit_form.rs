//! `VICTORY_LOCAL_EXIT`: a console wave64 program that only translates as two host waves of 32,
//! each a wave of its own (`lower::with_local_waves_any`), with the branches that leave a loop
//! taken by the host wave together ([`lower::local_exit`] says why).

use std::collections::BTreeSet;

use super::{Container, HostShader, NativeProgram, code_digits, emit_program, lower};
use crate::structure::{Atom, LaneExit, Structured};
use crate::{ShaderError, io, ir, resource, structure};

/// A loop: the blocks of its body, and the blocks closed by a branch that leaves it and that the
/// console takes for the whole wave ([`Structured::wave_any`]: `execz`, `execnz`, `vccz`, `vccnz`,
/// SCC of a mask operation).
struct Loop {
    body: Vec<bool>,
    exits: Vec<usize>,
}

/// Each block's place in reverse postorder (`usize::MAX`: unreachable) and the program's loops,
/// one for each header of a back edge.
fn loops(p: &ir::Program, st: &Structured) -> (Vec<usize>, Vec<Loop>) {
    let order = ir::reverse_postorder(&p.blocks);
    let n = p.blocks.len();
    let mut index = vec![usize::MAX; n];
    for (i, &b) in order.iter().enumerate() {
        index[b] = i;
    }
    let mut bodies: Vec<(usize, Vec<bool>)> = Vec::new();
    for &latch in &order {
        for header in p.blocks[latch].exit.successors() {
            // (a back edge goes to a block that is not later in the order)
            if index[header] > index[latch] {
                continue;
            }
            // the loop: the header and what reaches the latch without passing it
            let mut body = vec![false; n];
            body[header] = true;
            let mut stack = vec![latch];
            while let Some(b) = stack.pop() {
                if std::mem::replace(&mut body[b], true) {
                    continue;
                }
                stack.extend(
                    p.blocks[b]
                        .preds
                        .iter()
                        .copied()
                        .filter(|&q| index[q] != usize::MAX),
                );
            }
            match bodies.iter_mut().find(|(h, _)| *h == header) {
                Some((_, b)) => (0..n).for_each(|i| b[i] |= body[i]),
                None => bodies.push((header, body)),
            }
        }
    }
    let loops = bodies
        .into_iter()
        .map(|(_, body)| {
            let exits = (0..n)
                .filter(|&b| body[b])
                .filter(|&b| {
                    matches!(p.blocks[b].exit, ir::Exit::Branch { taken, not_taken }
                        if !(body[taken] && body[not_taken]))
                        && st.wave_any.get(b).copied().flatten().is_some()
                })
                .collect();
            Loop { body, exits }
        })
        .collect();
    (index, loops)
}

/// The per-lane branches in the way of the loop's exits: the lanes of the host wave must all be at
/// an exit when it asks the wave whether to leave, in the same round. A block that every lane
/// reaches eventually (it post-dominates the branch, as a loop's only exit does) is not reached
/// by them together: in one round of the loop (back edges and exits end it) the branches whose
/// round-region holds an exit, and then those whose region holds one of these branches, go to
/// the host wave too. None where one of them has no wave-wide meaning.
fn branches_around_exits(
    p: &ir::Program,
    st: &Structured,
    index: &[usize],
    lp: &Loop,
) -> Option<Vec<usize>> {
    let n = p.blocks.len();
    let words = (n + 1).div_ceil(64);
    let goes_on = |b: usize, s: usize| lp.body[s] && index[s] > index[b];
    let mut blocks: Vec<usize> = (0..n).filter(|&b| lp.body[b]).collect();
    blocks.sort_by_key(|&b| std::cmp::Reverse(index[b]));
    // the end of a round, in every post-dominator set (bit `n`)
    let mut end = vec![0u64; words];
    end[n / 64] |= 1 << (n % 64);
    // post-dominators within one round
    let mut pdom: Vec<Vec<u64>> = vec![Vec::new(); n];
    for &b in &blocks {
        let succ = p.blocks[b].exit.successors();
        let mut v: Option<Vec<u64>> = None;
        let mut meet = |set: &[u64]| {
            if let Some(a) = v.as_mut() {
                a.iter_mut().zip(set).for_each(|(x, y)| *x &= y);
            } else {
                v = Some(set.to_vec());
            }
        };
        for &s in &succ {
            if goes_on(b, s) {
                meet(&pdom[s]);
            } else {
                meet(&end);
            }
        }
        let mut v = v.unwrap_or_else(|| end.clone());
        v[b / 64] |= 1 << (b % 64);
        pdom[b] = v;
    }
    let mut wide: BTreeSet<usize> = BTreeSet::new();
    let mut targets: Vec<usize> = lp.exits.clone();
    loop {
        let mut grew = false;
        for &d in &blocks {
            if wide.contains(&d) || lp.exits.contains(&d) {
                continue;
            }
            let LaneExit::Branch { when, .. } = st.exits[d] else {
                continue;
            };
            if st
                .bools
                .support(when)
                .iter()
                .all(|a| matches!(a, Atom::Uniform(_)))
            {
                continue;
            }
            // the blocks of the round reachable from the branch that do not post-dominate it
            let mut seen = vec![false; n];
            let mut stack: Vec<usize> = p.blocks[d]
                .exit
                .successors()
                .into_iter()
                .filter(|&s| goes_on(d, s))
                .collect();
            while let Some(b) = stack.pop() {
                if seen[b] || pdom[d][b / 64] >> (b % 64) & 1 == 1 {
                    continue;
                }
                seen[b] = true;
                stack.extend(
                    p.blocks[b]
                        .exit
                        .successors()
                        .into_iter()
                        .filter(|&s| goes_on(b, s)),
                );
            }
            if targets.iter().any(|&t| seen[t]) {
                st.wave_any.get(d).copied().flatten()?;
                wide.insert(d);
                targets.push(d);
                grew = true;
            }
        }
        if !grew {
            return Some(wide.into_iter().collect());
        }
    }
}

/// The branches (by block) that the host wave takes together to begin with: the ones that leave a
/// loop and the ones in their way. None when the program has none, or when one of those has no
/// wave-wide meaning.
pub(super) fn wide_branches(p: &ir::Program) -> Option<Vec<usize>> {
    let first = structure::structure(p).ok()?;
    let (index, loops) = loops(p, &first);
    let mut wide = BTreeSet::new();
    for lp in loops.iter().filter(|lp| !lp.exits.is_empty()) {
        wide.extend(lp.exits.iter().copied());
        wide.extend(branches_around_exits(p, &first, &index, lp)?);
    }
    if wide.is_empty() {
        return None;
    }
    // A host wave with no lane at all must not go round its loops: the branches around them that
    // the console takes for its wave (the early-out after `s_and_saveexec`) go to the host wave too
    // (they are in the program unless a form left them out).
    let headers: Vec<usize> = loops
        .iter()
        .filter(|lp| !lp.exits.is_empty())
        .flat_map(|lp| {
            (0..lp.body.len())
                .filter(|&b| lp.body[b] && p.blocks[b].preds.iter().any(|&q| !lp.body[q]))
        })
        .collect();
    let st = structure::structure_wide(p, &wide.iter().copied().collect()).ok()?;
    wide.extend(lower::branches_around(&st, &headers).unwrap_or_default());
    Some(wide.into_iter().collect())
}

/// `VICTORY_LOCAL_EXIT=1`: `p` as two host waves of 32 that each run as a wave of their own
/// ([`lower::with_local_waves_any`], or [`lower::with_local_waves`] without `any`) with the
/// branches that leave a loop taken by the host wave together, the way the console takes them for
/// its wave: the lanes of a host wave leave a loop in the same round. The branches in the way
/// follow ([`branches_around_exits`]). None when the program has no such branch or the form does
/// not lower: the caller goes on with the plain form of the same program.
pub(super) fn local_exit(
    c: &Container,
    native: &NativeProgram,
    p: &ir::Program,
    why: bool,
    any: bool,
) -> Option<Result<(HostShader, Option<u32>), ShaderError>> {
    let mut wide: std::collections::HashSet<usize> = wide_branches(p)?.into_iter().collect();
    loop {
        let st = structure::structure_wide(p, &wide).ok()?;
        let io = io::raise(p, native).ok()?;
        let res = resource::track(p, &native.layout, native.stage, Some(&st)).ok()?;
        lower::take_needs();
        let r = if any {
            lower::with_local_waves_any(|| emit_program(c, native, p, &st, &io, &res))
        } else {
            lower::with_local_waves(|| emit_program(c, native, p, &st, &io, &res))
        };
        let needs = lower::take_needs();
        let stuck = matches!(&r, Err(ShaderError::Unsupported { what, .. }) if lower::needs_every_lane(what));
        let more: Vec<usize> = if stuck {
            lower::branches_around(&st, &needs)
                .unwrap_or_default()
                .into_iter()
                .filter(|b| !wide.contains(b))
                .collect()
        } else {
            Vec::new()
        };
        if more.is_empty() {
            if why {
                let mut sorted: Vec<usize> = wide.iter().copied().collect();
                sorted.sort_unstable();
                eprintln!(
                    "WAVE64-LOCAL-EXIT ~{} {} branches taken by the host wave together, {}: {sorted:?}",
                    code_digits(c),
                    sorted.len(),
                    if r.is_ok() { "lowered" } else { "NOT lowered" }
                );
            }
            return Some(r);
        }
        wide.extend(more);
    }
}
