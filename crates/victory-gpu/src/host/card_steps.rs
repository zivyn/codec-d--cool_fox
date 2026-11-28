//! Steps of a submission that read title memory flights in the air may
//! still write, done on the card in the flights' order instead of on the
//! CPU after a wait for them (`VICTORY_CARD_STEPS`, see [`stage`]).

use std::sync::Arc;

use super::*;
use crate::shadow::Shadow;

/// The stage (`VICTORY_CARD_STEPS`, `2` by default through `defaults.rs`; `0` is off): `1` constant buffers
/// of descriptor tables (`Step::Fill` with a slot of its own), `2` the
/// title's buffer copies (`Step::Copy`) too, with the steps of the same
/// submission that depend on them. Only with the relay's second stage
/// (`relay_exact`).
///
/// Why: eleven dispatches a frame (the skinning passes, in two early
/// submissions) write through a view of the title's whole 174 MB heap, at
/// places they read out of job records in buffers: no bound of what they
/// write is known before they ran (frame capture replay2: 17af15c4015cb4b6,
/// 55679a0547649d0b, 89d08e561897d4e5, fdd56d97a806bcca and three more,
/// all through one view at offset 5920 of the title's bindless table). The
/// title's ring of constants lies in the same heap. One constant buffer of
/// 480 bytes and one copy of 20 KiB a frame, at a new place of the ring
/// each time, waited for these passes to land: 25-30 ms of a 105 ms frame,
/// the submitting thread standing (run record-1; with `VICTORY_FILL_TITLE=1`
/// in run relaycopy-1 the copy alone took over the wait).
///
/// What is done: a list of ours goes to the card in front of the
/// submission's lists, in the same call. It copies the bytes read from the
/// host copy that the flights write into a buffer of ours, and from there
/// to the destination's host copy (a copy), or the fill's descriptor slot
/// is pointed at that buffer (a fill; the slot is the fill's alone, the
/// root signatures say such descriptors are volatile, and the list that
/// names it has not gone to the card).
///
/// Why that is the value the CPU would have read after the wait. After the
/// wait title memory holds, for every byte read, what the title wrote, or
/// what the last of the flights that wrote it wrote (the landings put it
/// there, in order). The host copy on the card holds the same when our list
/// runs: every flight that may write one of these bytes writes this same
/// host copy ([`holder`]: a write through another host copy, a read-back
/// or a value written behind a flight there, and the old wait is taken),
/// they are all ahead of our list on the queue, and the title's own bytes
/// are brought up to date by a refresh step of our list, which is prepared
/// like any other list's (also under flights' declared writes:
/// `stale_under_flights`, which settles if it cannot). Nothing of this
/// submission writes these bytes earlier than our list: the steps are done
/// in front of all its lists in either case, and among themselves in the
/// order they stand in wherever one depends on another: [`plan`] takes
/// every copy and fill of the submission that writes what a taken one
/// reads or writes, or reads what a taken one writes. Every draw that
/// reads a buffer view of the heap relies on the same refresh and the same
/// order; what is new is only that a constant buffer and a copy do too.
/// Where a page is not mapped the host copy may hold other bytes than the
/// CPU's copy would have left: refused.
///
/// For a copy's destination our list declares a shader write, exactly the
/// bytes copied, after a refresh of them: from there on it is what any
/// shader's write is. The relay compares, and puts what changed into title
/// memory before the title is told anything of this flight (its
/// bottom-of-pipe writes and fences come after, in order); CPU steps of
/// later submissions that read it find it declared and wait, or are done
/// here. Refused: a step of the submission that cannot be taken (a texture,
/// vertices expanded, a fill of memory of another kind) reads the
/// destination behind the copy, or a refresh brings another host copy of it
/// up to date ([`plan`], [`elsewhere`]). While such a copy is in the air a
/// submission that refreshes another host copy of its destination, or
/// reads it in a step that is not checked otherwise, waits for it
/// ([`copied_read`]); with the copy done on the CPU it was there at once.
///
/// What stage 2 does not see: code of ours that reads title memory away
/// from a submission's steps (the pass over textures picked by index, while
/// a list is recorded). With the CPU's copy such a read of the destination
/// found the bytes at once; now it finds them when the flight has landed.
/// The landing tells the guards (`crate::watch::landing`), so a texture
/// made from older bytes is made again. This is why stage 2 is its own.
pub(super) fn stage() -> u32 {
    static STAGE: OnceLock<u32> = OnceLock::new();
    *STAGE.get_or_init(|| match crate::defaults::var("VICTORY_CARD_STEPS").ok().as_deref() {
        Some("1") => 1,
        Some("2") => 2,
        _ => 0,
    })
}

/// Whether a copy at its place in a list ([`place`]) is only in the way of
/// what is done on the card in front of the lists if that stands behind it in
/// them (`VICTORY_PLACED_ORDER=1`; off: wherever it stands), and a
/// submission where it does is cut behind the copy's list ([`cut_for`]). Off:
/// the case did not come up once in runs submit-1 to submit-3, so it has only
/// its tests behind it.
///
/// The fills of constant buffers read the title's ring (7,500 a frame in the
/// opening), and one copy of a few hundred bytes a frame writes into it at its
/// place in a list. The whole submission was refused for it, and stood 40 to
/// 60 ms until the flights had landed (run n19).
fn placed_order() -> bool {
    #[cfg(feature = "diagnostics")]
    if let Some(on) = crate::ab::pick("VICTORY_PLACED_ORDER") {
        return on;
    }
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::switch::on("VICTORY_PLACED_ORDER"))
}

/// Why [`plan`] refuses for [`place`].
const BEHIND_PLACED: &str = "a copy at its place in a list writes what it reads, or reads or writes what it writes";

/// The bytes of our buffer between reading and writing: what one submission
/// may do this way (each place read once, however many fills take it).
const BETWEEN: u64 = 4 << 20;

/// The most steps of a submission done this way.
const MOST: usize = 12288;

/// How a fill's bytes get where its list reads them, if the card brings
/// them.
#[derive(Clone)]
pub(super) enum FillTo {
    /// A descriptor slot that shows the fill's memory, and nothing else does
    /// (title heap, slot): it is made to show our buffer instead.
    Slot(usize, u32),
    /// A buffer the card can write, filled from its beginning.
    Buffer(ID3D12Resource),
}

/// Title memory something in the air writes: the host copy it goes through
/// (`None`: none, the bytes reach title memory when the flight lands), from,
/// to, and whether it is the destination of a copy done here.
pub(super) struct Writer {
    shadow: Option<Arc<Shadow>>,
    from: usize,
    to: usize,
    copy: bool,
}

/// What `items` of the line write.
pub(super) fn writers(items: &[Late]) -> Vec<Writer> {
    let mut out = Vec::new();
    for item in items {
        match item {
            Late::Run(flight) => {
                for (shadow, ranges) in &flight.back {
                    out.extend(ranges.iter().map(|&(from, to)| Writer {
                        shadow: Some(shadow.clone()),
                        from,
                        to,
                        copy: false,
                    }));
                }
                for &i in &flight.order {
                    for &(_, at, [row, rows, pitch]) in &flight.lists[i].host().deferred().backs {
                        out.push(Writer {
                            shadow: None,
                            from: at,
                            to: at + rows.saturating_sub(1) * pitch + row,
                            copy: false,
                        });
                    }
                }
                out.extend(flight.own.iter().flat_map(Taken::copied));
            }
            Late::Write(at, _, bytes) => out.push(Writer {
                shadow: None,
                from: *at,
                to: *at + *bytes,
                copy: false,
            }),
            Late::Signal(..) => {}
        }
    }
    out
}

/// The destinations of copies done here that have not landed.
pub(super) fn copies(line: &Line) -> Vec<Writer> {
    if stage() < 2 {
        return Vec::new();
    }
    copies_in(line)
}

/// [`copies`] whatever the stage says.
fn copies_in(line: &Line) -> Vec<Writer> {
    let flying = line.line.iter().filter_map(|l| match l {
        Late::Run(flight) => flight.own.as_ref(),
        _ => None,
    });
    let landing = line.landing.iter().filter(|w| w.copy).map(|w| Writer {
        shadow: w.shadow.clone(),
        ..*w
    });
    // (And those of the submissions being sent, not yet flights in the line: the thread of stage A looks at the
    // next submission while the one before is sent.)
    let sending = line.pending_copies.iter().flat_map(|p| p.1.iter()).map(|w| Writer {
        shadow: w.shadow.clone(),
        ..*w
    });
    flying.flat_map(Taken::copied).chain(landing).chain(sending).collect()
}

/// Whether `runs` refresh another host copy of a destination in `copies`,
/// or read it in a step that flights are not waited for otherwise
/// (`Step::Host`): that reads title memory, where the copy is not yet.
pub(super) fn copied_read<L: onepass::StepList>(copies: &[Writer], runs: &[L]) -> bool {
    if copies.is_empty() {
        return false;
    }
    runs.iter().any(|list| {
        list.steps(|steps| steps.iter().any(|step| match step {
            Step::Refresh(shadow, at, len) => copies
                .iter()
                .any(|w| w.from < *at + *len && *at < w.to && w.shadow.as_ref().is_none_or(|s| !Arc::ptr_eq(s, shadow))),
            Step::Host(_, (from, to), _) => copies.iter().any(|w| w.from < *to && *from < w.to),
            Step::Plane((at, len), _) => copies.iter().any(|w| w.from < at.saturating_add(*len) && *at < w.to),
            _ => false,
        }))
    })
}

/// What a step of a submission does to title memory, as the plan sees it.
/// `asked`: flights in the air stop it from being done on the CPU now.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Does {
    Copy { dst: usize, src: usize, len: usize, asked: bool },
    /// `to`: its memory can be given the bytes on the card.
    Fill { src: usize, len: usize, asked: bool, to: bool },
    /// Another read on the CPU (a texture, vertices expanded).
    Read { from: usize, to: usize, asked: bool },
    /// A refresh of a host copy (named by its address).
    Refresh { shadow: usize, from: usize, to: usize },
    /// A copy that its list does on the card ([`place`]).
    Placed { dst: usize, src: usize, len: usize },
    /// Anything, anywhere.
    Run,
}

fn meet(a: (usize, usize), b: (usize, usize)) -> bool {
    a.0 < b.1 && b.0 < a.1
}

/// The bytes `d` reads and writes as a copy or a fill (empty ranges for
/// what it does not).
fn reads_writes(d: &Does) -> Option<((usize, usize), (usize, usize))> {
    match *d {
        Does::Copy { dst, src, len, .. } => Some(((src, src + len), (dst, dst + len))),
        Does::Fill { src, len, .. } => Some(((src, src + len), (0, 0))),
        _ => None,
    }
}

fn touched(ranges: &[(usize, usize)], r: (usize, usize)) -> bool {
    touches(ranges, r.0, r.1)
}

/// The steps of `does` (by place) to be done on the card, in order, each
/// with its level; or why not. All that are asked for, and every copy and
/// fill that is in the way of one that is done there: it writes what that
/// one reads or writes, or reads what it writes, whichever comes first. So
/// what is left on the CPU writes none of their bytes, and reads of them
/// only what a taken copy has not been told to write yet: bytes a step
/// ahead of it reads, in title memory as it is.
///
/// `behind`: for [`BEHIND_PLACED`], the copy at its place and the first step
/// behind it that is in its way.
///
/// Steps of a level are done together, every read before every write; a
/// step that writes what an earlier one reads or writes, or reads what it
/// writes, is of a higher level. Our list does them level by level.
#[inline(never)]
fn plan(does: &[Does], stage: u32, by_order: bool, behind: &mut Option<(usize, usize)>) -> Result<Vec<(usize, u32)>, &'static str> {
    let takes = |d: &Does| match *d {
        Does::Copy { .. } if stage < 2 => Err("a buffer copy (VICTORY_CARD_STEPS=2 does it on the card)"),
        Does::Fill { to: false, .. } => Err("a fill of memory the card cannot be given the bytes of"),
        _ => Ok(()),
    };
    let mut taken = vec![false; does.len()];
    let (mut read, mut written) = (Vec::new(), Vec::new());
    let mut more = Vec::new();
    for (i, d) in does.iter().enumerate() {
        match *d {
            Does::Run => return Err("a step of ours runs"),
            Does::Read { asked: true, .. } => return Err("a texture reads it"),
            Does::Copy { asked: true, .. } | Does::Fill { asked: true, .. } => more.push(i),
            _ => {}
        }
    }
    while !more.is_empty() {
        // (Joined once for the round: one by one the 2,800 fills of a frame
        // sorted the ranges each time, 16 ms of the submitting thread.)
        let (mut reads, mut writes) = (Vec::new(), Vec::new());
        for i in std::mem::take(&mut more) {
            takes(&does[i])?;
            taken[i] = true;
            if let Some((r, w)) = reads_writes(&does[i]) {
                if w.0 < w.1 && meet(r, w) {
                    return Err("a copy onto itself");
                }
                reads.push(r);
                writes.push(w);
            }
        }
        join_ranges(&mut read, reads);
        if writes.iter().any(|w| w.0 < w.1) {
            join_ranges(&mut written, writes);
        }
        if taken.iter().filter(|t| **t).count() > MOST {
            return Err("too many steps depend on it");
        }
        for (i, d) in does.iter().enumerate() {
            // A fill that cannot be taken is looked at below.
            if taken[i] || matches!(d, Does::Fill { to: false, .. }) {
                continue;
            }
            if let Some((r, w)) = reads_writes(d)
                && (touched(&written, r) || touched(&written, w) || touched(&read, w))
            {
                more.push(i);
            }
        }
    }
    // A copy at its place in a list comes after what is done in front of all
    // of them: as it should after the steps ahead of it, but not after those
    // behind it that read or write its bytes.
    for (p, d) in does.iter().enumerate() {
        if let Does::Placed { dst, src, len } = *d
            && (touched(&read, (dst, dst + len)) || touched(&written, (dst, dst + len)) || touched(&written, (src, src + len)))
        {
            let (to, from) = ((dst, dst + len), (src, src + len));
            let in_the_way = |i: usize| {
                reads_writes(&does[i]).is_some_and(|(r, w)| meet(r, to) || w.0 < w.1 && (meet(w, to) || meet(w, from)))
            };
            let found = (p + 1..does.len()).find(|&i| taken[i] && in_the_way(i));
            *behind = found.map(|i| (p, i));
            if !by_order || found.is_some() {
                return Err(BEHIND_PLACED);
            }
        }
    }
    let order: Vec<usize> = (0..does.len()).filter(|&i| taken[i]).collect();
    // What stays on the CPU and reads what an earlier taken copy writes
    // would read it too early.
    let mut behind: Vec<(usize, usize)> = Vec::new();
    for (i, d) in does.iter().enumerate() {
        if taken[i] {
            if let Some((_, w)) = reads_writes(d)
                && w.0 < w.1
            {
                join_ranges(&mut behind, [w]);
            }
            continue;
        }
        match *d {
            Does::Read { from, to, .. } if touched(&behind, (from, to)) => {
                return Err("a texture or expanded vertices read what it writes");
            }
            Does::Fill { src, len, .. } if touched(&behind, (src, src + len)) => {
                return Err("a fill of memory the card cannot be given the bytes of reads what it writes");
            }
            _ => {}
        }
    }
    let mut out: Vec<(usize, u32)> = Vec::with_capacity(order.len());
    // The copies among them, which alone put others behind them or have to
    // stand behind others.
    let mut copies: Vec<usize> = Vec::new();
    for &i in &order {
        let Some((r, w)) = reads_writes(&does[i]) else {
            continue;
        };
        let writes = w.0 < w.1;
        let mut level = 0;
        // Behind what writes what this reads or writes, and, if this
        // writes, behind what reads it.
        let ahead: Box<dyn Iterator<Item = usize>> =
            if writes { Box::new(0..out.len()) } else { Box::new(copies.iter().copied()) };
        for k in ahead {
            let (j, at): (usize, u32) = out[k];
            let Some((jr, jw)) = reads_writes(&does[j]) else {
                continue;
            };
            if at >= level && (meet(jw, r) || meet(jw, w) || meet(jr, w)) {
                level = at + 1;
            }
        }
        if writes {
            copies.push(out.len());
        }
        out.push((i, level));
    }
    Ok(out)
}

/// Whether a refresh of `does` brings a host copy other than `shadow` up to
/// date in `[from, to)`.
fn elsewhere(does: &[Does], shadow: usize, from: usize, to: usize) -> bool {
    does.iter().any(|d| match *d {
        Does::Refresh { shadow: s, from: a, to: b } => s != shadow && meet((a, b), (from, to)),
        _ => false,
    })
}

/// [`elsewhere`] for refreshes kept apart from the steps ([`does_of`]).
fn elsewhere_in(refreshed: &[Refreshed], shadow: usize, from: usize, to: usize) -> bool {
    refreshed.iter().any(|&(s, a, b)| s != shadow && meet((a, b), (from, to)))
}

/// The host copy that holds `[from, to)` on the card once what `writers`
/// write is there: the one they all write it through, or without any the
/// one shaders would be given.
fn holder(writers: &[Writer], from: usize, to: usize) -> Result<Arc<Shadow>, &'static str> {
    holder_among(writers.iter().filter(|w| meet((w.from, w.to), (from, to))), from, to)
}

/// [`holder`] when `index` stands for the first `index.len` of `writers`
/// (`VICTORY_HOLDER_INDEX=1`): the writers that meet `[from, to)` are found
/// without looking at all of them, and are looked at in the same order.
fn holder_indexed(index: &WriterIndex, writers: &[Writer], from: usize, to: usize) -> Result<Arc<Shadow>, &'static str> {
    let mut at = index.meeting(|i| (writers[i].from, writers[i].to), from, to);
    at.extend((index.len..writers.len()).filter(|&i| meet((writers[i].from, writers[i].to), (from, to))));
    holder_among(at.into_iter().map(|i| &writers[i]), from, to)
}

/// The writers' places by address, to find those that meet a range: indices
/// sorted by where they begin, and the furthest end of any up to each.
struct WriterIndex {
    len: usize,
    by_from: Vec<usize>,
    reach: Vec<usize>,
}

impl WriterIndex {
    fn new(len: usize, place: impl Fn(usize) -> (usize, usize)) -> WriterIndex {
        let mut by_from: Vec<usize> = (0..len).collect();
        by_from.sort_by_key(|&i| (place(i).0, i));
        let mut reach = Vec::with_capacity(len);
        let mut far = 0;
        for &i in &by_from {
            far = far.max(place(i).1);
            reach.push(far);
        }
        WriterIndex { len, by_from, reach }
    }

    /// The indices of those that meet `[from, to)`, in order.
    fn meeting(&self, place: impl Fn(usize) -> (usize, usize), from: usize, to: usize) -> Vec<usize> {
        let end = self.by_from.partition_point(|&i| place(i).0 < to);
        let mut out = Vec::new();
        for k in (0..end).rev() {
            if self.reach[k] <= from {
                break;
            }
            let i = self.by_from[k];
            if meet(place(i), (from, to)) {
                out.push(i);
            }
        }
        out.sort_unstable();
        out
    }
}

/// Whether the writers are found through a [`WriterIndex`]
/// (`VICTORY_HOLDER_INDEX=1`; off by default).
fn holder_index_on() -> bool {
    // (May change while the title runs, `VICTORY_AB`: the index is made and used inside one planning, and holds only what
    // that planning's writers hold.)
    crate::ab::live_flag!("VICTORY_HOLDER_INDEX")
}

fn holder_among<'a>(
    writers: impl Iterator<Item = &'a Writer>,
    from: usize,
    to: usize,
) -> Result<Arc<Shadow>, &'static str> {
    let mut found: Option<&Arc<Shadow>> = None;
    for w in writers {
        match (&w.shadow, found) {
            (None, _) => return Err("a read-back or a value behind a flight is written there"),
            (Some(s), Some(f)) if !Arc::ptr_eq(s, f) => return Err("flights write it through two host copies"),
            (Some(s), _) => found = Some(s),
        }
    }
    let shadow = match found {
        Some(s) => s.clone(),
        None => crate::shadow::find(from as u64, (to - from) as u64).ok_or("no host copy of it")?,
    };
    let (start, len) = shadow.span();
    if shadow.in_place() || (from as u64) < start || to as u64 > start + len {
        return Err("no host copy holds all of it");
    }
    Ok(shadow)
}

/// A refresh of a host copy among the steps (the copy's address, from, to).
type Refreshed = (usize, usize, usize);

/// What the steps of `runs` do, and which step each is (its list by address,
/// its place in it). `written`: what flights may write.
///
/// With `lean` (`VICTORY_LEAN_STEPS`) the refreshes are not among what the
/// steps do: they are the third answer, in the order they stand in, for
/// [`elsewhere_in`] (the plan does nothing for a refresh, and a frame has
/// 21,000 of them for 4,400 copies and fills), and what is returned is made
/// in room taken once.
#[inline(never)]
fn does_of(written: &[(usize, usize)], runs: &[&HostList], lean: bool) -> (Vec<Does>, Vec<(usize, usize)>, Vec<Refreshed>) {
    does_of_in(written, runs, lean, |list| std::ptr::from_ref::<HostList>(*list) as usize)
}

/// [`does_of`] over any lists of steps (`id`: the number the list goes by in the second answer).
pub(super) fn does_of_in<L: onepass::StepList>(
    written: &[(usize, usize)],
    runs: &[L],
    lean: bool,
    id: impl Fn(&L) -> usize,
) -> (Vec<Does>, Vec<(usize, usize)>, Vec<Refreshed>) {
    let (mut does, mut steps) = if lean {
        // (Without the refreshes: some thousands, to the 26,000 of the steps.)
        let all: usize = runs.iter().map(|list| list.steps(|steps| steps.len())).sum();
        (Vec::with_capacity(all.min(8192)), Vec::with_capacity(all.min(8192)))
    } else {
        (Vec::new(), Vec::new())
    };
    let mut refreshed = Vec::new();
    // Which step each is. One list's lock at a time: a submission may
    // name a list twice.
    for list in runs {
        let list_id = id(list);
        list.steps(|all| {
            for (i, step) in all.iter().enumerate() {
                if lean && let Step::Refresh(shadow, at, len) = step {
                    refreshed.push((Arc::as_ptr(shadow) as usize, *at, *at + *len));
                    continue;
                }
                does.push(match step {
                    &Step::Copy(dst, src, len) => Does::Copy {
                        dst,
                        src,
                        len,
                        asked: copy_blocked(written, dst, src, len).is_some(),
                    },
                    Step::Fill(_, src, len, to) => Does::Fill {
                        src: *src,
                        len: *len,
                        asked: read_blocked(written, *src, *len),
                        to: to.is_some(),
                    },
                    Step::Texture(staging) => {
                        let (at, len) = staging.memory();
                        Does::Read {
                            from: at,
                            to: at + len,
                            asked: touches(written, at, at + len),
                        }
                    }
                    &Step::Plane((at, len), _) => Does::Read {
                        from: at,
                        to: at.saturating_add(len),
                        asked: touches(written, at, at.saturating_add(len)),
                    },
                    &Step::Host(_, (from, to), _) if from < to => Does::Read { from, to, asked: false },
                    Step::Refresh(shadow, at, len) => Does::Refresh {
                        shadow: Arc::as_ptr(shadow) as usize,
                        from: *at,
                        to: *at + *len,
                    },
                    Step::Run(_) => Does::Run,
                    Step::Placed(p) => Does::Placed { dst: p.dst, src: p.src, len: p.len },
                    Step::GpuWrite(..) | Step::Host(..) => continue,
                });
                steps.push((list_id, i));
            }
        });
    }
    (does, steps, refreshed)
}

/// [`does_of_in`] with `lean`, from the summaries of the lists' steps (`VICTORY_ONE_PASS`): the same answers without
/// reading the steps of the refreshes.
pub(super) fn does_of_summarized<L: onepass::StepList>(
    written: &[(usize, usize)],
    runs: &[L],
    id: impl Fn(&L) -> usize,
) -> (Vec<Does>, Vec<(usize, usize)>, Vec<Refreshed>) {
    use onepass::Op;
    let summaries: Vec<_> = runs.iter().map(|list| list.summary()).collect();
    let all: usize = summaries.iter().map(|summary| summary.ops().len()).sum();
    let (mut does, mut steps) = (Vec::with_capacity(all.min(8192)), Vec::with_capacity(all.min(8192)));
    let mut refreshed = Vec::new();
    for (list, summary) in runs.iter().zip(&summaries) {
        let list_id = id(list);
        refreshed.extend(summary.refreshes());
        for &(i, op) in summary.ops() {
            does.push(match op {
                Op::Copy { dst, src, len } => Does::Copy {
                    dst,
                    src,
                    len,
                    asked: copy_blocked(written, dst, src, len).is_some(),
                },
                Op::Fill { src, len, to } => Does::Fill {
                    src,
                    len,
                    asked: read_blocked(written, src, len),
                    to,
                },
                Op::Texture(k) => {
                    let (at, len) = summary.texture_memory(k);
                    Does::Read {
                        from: at,
                        to: at + len,
                        asked: touches(written, at, at + len),
                    }
                }
                Op::Plane { at, len } => Does::Read {
                    from: at,
                    to: at.saturating_add(len),
                    asked: touches(written, at, at.saturating_add(len)),
                },
                Op::Host { from, to } => Does::Read { from, to, asked: false },
                Op::Run => Does::Run,
                Op::Placed { dst, src, len } => Does::Placed { dst, src, len },
            });
            steps.push((list_id, i as usize));
        }
    }
    (does, steps, refreshed)
}

/// With flights in the air: before which of `runs` the submission is cut so
/// that what reads or writes the bytes of a copy at its place in an earlier
/// list ([`BEHIND_PLACED`]) is done on the card in front of a submission of
/// its own, behind that copy.
pub(super) fn cut_for(runs: &[&HostList]) -> Option<usize> {
    if stage() == 0 || !relay_exact() || !placed_order() || runs.len() < 2 {
        return None;
    }
    // (Most submissions have none.)
    let first = runs.iter().position(|l| l.deferred().copies.iter().any(|s| matches!(s, Step::Placed(_))))?;
    if first + 1 == runs.len() {
        return None;
    }
    let written = line().written.clone();
    blocked_by(&written, runs, false)?;
    let (does, steps, _) = does_of(&written, runs, false);
    let mut behind = None;
    if plan(&does, stage(), true, &mut behind) != Err(BEHIND_PLACED) {
        return None;
    }
    let (placed, reader) = behind?;
    let place = |i: usize| runs.iter().position(|l| std::ptr::from_ref::<HostList>(l) as usize == steps[i].0);
    let cut = place(placed)? + 1;
    (place(reader)? >= cut).then_some(cut)
}

/// Whether every page of `[from, to)` can be read (through the kernel:
/// asking Windows walks the page tables, milliseconds in the title's heap).
/// `known`: pages that could be, of the same submission: its fills' places
/// lie eight to a page, and each was read whole to find out.
fn mapped(from: usize, to: usize, known: &mut HashMap<usize, ()>) -> bool {
    const PAGE: usize = 4096;
    (from / PAGE..to.div_ceil(PAGE)).all(|page| {
        known.contains_key(&page) || {
            let there = read_into((page * PAGE).max(from) as *const u8, &mut [0]);
            if there {
                known.insert(page, ());
            }
            there
        }
    })
}

/// A table by addresses and numbers of ours.
type HashMap<K, V> = std::collections::HashMap<K, V, std::hash::BuildHasherDefault<AddressHash>>;

/// Where the bytes of the fill that is `step` (its list by address, its place
/// in it) go.
fn fill_to(runs: &[&HostList], step: (usize, usize)) -> Option<FillTo> {
    let list = runs.iter().find(|l| std::ptr::from_ref::<HostList>(l) as usize == step.0)?;
    match list.deferred().copies.get(step.1)? {
        Step::Fill(_, _, _, to) => to.clone(),
        _ => None,
    }
}

/// [`fill_to`] with the lists found by `run_at` (their addresses to their places among `runs`).
fn fill_to_at(runs: &[&HostList], run_at: &HashMap<usize, usize>, step: (usize, usize)) -> Option<FillTo> {
    let list = runs.get(*run_at.get(&step.0)?)?;
    match list.deferred().copies.get(step.1)? {
        Step::Fill(_, _, _, to) => to.clone(),
        _ => None,
    }
}

/// A step done on the card.
struct Op {
    from: (Arc<Shadow>, usize),
    len: usize,
    /// Its place in the buffer between.
    at: u64,
    level: u32,
    to: To,
}

enum To {
    /// A host copy, and the title address.
    Copy(Arc<Shadow>, usize),
    /// A descriptor slot (title heap, slot).
    Slot(usize, u32),
    /// A buffer's beginning.
    Buffer(ID3D12Resource),
}

/// Whether the reads of a level that lie together are one copy
/// (`VICTORY_STEPS_MERGE_READS=1`; off by default).
fn merge_reads_on() -> bool {
    // (May change while the title runs, `VICTORY_AB`: each list's reads are recorded in one go, whole or joined.)
    crate::ab::live_flag!("VICTORY_STEPS_MERGE_READS")
}

/// Since the last frame line: reads of levels, the copies made of them, and
/// the bytes of gaps they took along.
static READS: [std::sync::atomic::AtomicU64; 3] = [const { std::sync::atomic::AtomicU64::new(0) }; 3];

/// The most bytes between two reads that are still one copy: the places lie
/// at multiples of 256 in `between`, and a constant buffer of 480 bytes
/// leaves 32.
const MERGE_GAP: u64 = 256;

/// Which of `reads` (buffer, offset in it, offset in `between`, bytes) go in
/// one copy with the one before: the same buffer, the next bytes of it where
/// `between` has the next ones too, at most `gap` bytes apart. Returns the
/// first read of each copy and its bytes. Reads are in the order of their
/// places in `between`.
fn merged_reads(reads: &[(usize, u64, u64, u64)], gap: u64) -> Vec<(usize, u64)> {
    let mut out: Vec<(usize, u64)> = Vec::new();
    for (i, &(buffer, offset, at, len)) in reads.iter().enumerate() {
        if let Some(last) = out.last_mut() {
            let (b, o, a, _) = reads[last.0];
            let end = a + last.1;
            if b == buffer && at >= end && at - end <= gap && offset >= o && offset - o == at - a {
                last.1 = at + len - a;
                continue;
            }
        }
        out.push((i, len));
    }
    out
}

/// Records a level of what [`stage`] describes into `list` (open): `reads`
/// are copied into `between` (source, offset in it, offset in `between`,
/// bytes), then `writes` out of it (destination, offset in it, offset in
/// `between`, bytes). `between` is left readable as constants (and must be
/// so, unless this is the `first` level), the others in the common state,
/// as the submission's lists expect them.
fn record(
    first: bool,
    list: &ID3D12GraphicsCommandList,
    between: &ID3D12Resource,
    reads: &[(&ID3D12Resource, u64, u64, u64)],
    writes: &[(&ID3D12Resource, u64, u64, u64)],
) {
    let (reads_joined, writes_joined);
    let (reads, writes) = if merge_on() {
        let same = |a: &&ID3D12Resource, b: &&ID3D12Resource| a.as_raw() == b.as_raw();
        reads_joined = joined_copies(reads, same);
        writes_joined = joined_copies(writes, same);
        JOINED[0].fetch_add((reads.len() + writes.len()) as u64, Ordering::Relaxed);
        JOINED[1].fetch_add((reads_joined.len() + writes_joined.len()) as u64, Ordering::Relaxed);
        (&reads_joined[..], &writes_joined[..])
    } else {
        (reads, writes)
    };
    let common = D3D12_RESOURCE_STATE_COMMON;
    let (source, dest) = (D3D12_RESOURCE_STATE_COPY_SOURCE, D3D12_RESOURCE_STATE_COPY_DEST);
    let mut sources: Vec<&ID3D12Resource> = Vec::new();
    let mut dests: Vec<&ID3D12Resource> = Vec::new();
    for (all, list) in [(reads, &mut sources), (writes, &mut dests)] {
        for &(buffer, ..) in all {
            if !list.iter().any(|b| b.as_raw() == buffer.as_raw()) {
                list.push(buffer);
            }
        }
    }
    let both = |b: &ID3D12Resource| dests.iter().any(|d| d.as_raw() == b.as_raw());
    let constants = source | D3D12_RESOURCE_STATE_VERTEX_AND_CONSTANT_BUFFER;
    let mut barriers = vec![transition(between, dest, constants)];
    barriers.extend(sources.iter().map(|b| transition(b, source, if both(b) { dest } else { common })));
    let done: Vec<D3D12_RESOURCE_BARRIER> = dests.iter().map(|b| transition(b, dest, common)).collect();
    // SAFETY: an open list; live buffers, and the callers' ranges lie in
    // them. The buffers start in the common state (the list is the first of
    // its call) and are promoted by their first use.
    unsafe {
        if first {
            crate::host::ours::drain(crate::host::ours::Site::Steps);
            // What earlier lists' shaders wrote, first.
            list.ResourceBarrier(&[D3D12_RESOURCE_BARRIER {
                Type: D3D12_RESOURCE_BARRIER_TYPE_UAV,
                Flags: D3D12_RESOURCE_BARRIER_FLAG_NONE,
                Anonymous: D3D12_RESOURCE_BARRIER_0 {
                    UAV: std::mem::ManuallyDrop::new(D3D12_RESOURCE_UAV_BARRIER {
                        pResource: std::mem::ManuallyDrop::new(None),
                    }),
                },
            }]);
        } else {
            list.ResourceBarrier(&[transition(between, constants, dest)]);
        }
        if merge_reads_on() && reads.len() > 1 {
            // Places of one host copy that lie together there and in `between`
            // are one copy of the bytes from the first to the last (what lies
            // between them in `between` is nobody's).
            let keys: Vec<(usize, u64, u64, u64)> =
                reads.iter().map(|&(b, offset, at, len)| (b.as_raw() as usize, offset, at, len)).collect();
            let copies = merged_reads(&keys, MERGE_GAP);
            READS[0].fetch_add(reads.len() as u64, Ordering::Relaxed);
            READS[1].fetch_add(copies.len() as u64, Ordering::Relaxed);
            READS[2].fetch_add(copies.iter().map(|c| c.1).sum::<u64>() - reads.iter().map(|r| r.3).sum::<u64>(), Ordering::Relaxed);
            for (first, len) in copies {
                let (buffer, offset, at, _) = reads[first];
                list.CopyBufferRegion(between, at, buffer, offset, len);
            }
        } else {
            for &(buffer, offset, at, len) in reads {
                list.CopyBufferRegion(between, at, buffer, offset, len);
            }
        }
        list.ResourceBarrier(&barriers);
        for &(buffer, offset, at, len) in writes {
            list.CopyBufferRegion(buffer, offset, between, at, len);
        }
        if !done.is_empty() {
            list.ResourceBarrier(&done);
        }
    }
}

/// Whether copies that continue each other, in the buffer and in the one
/// between, are one copy (`VICTORY_STEP_MERGE=1`, off by default).
fn merge_on() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_STEP_MERGE").is_some_and(|v| v == "1"))
}

/// Since the last frame line: the copies recorded as planned, and as joined.
static JOINED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// `copies` (buffer, offset in it, offset between, bytes), in the order
/// given, each that begins where the one before ended, in the same buffer and
/// between, one with it: the same bytes, fewer calls (the planned fills of the
/// world's constants are 1,300 of 480 bytes a frame, from one copy).
fn joined_copies<T: Copy>(copies: &[(T, u64, u64, u64)], same: impl Fn(&T, &T) -> bool) -> Vec<(T, u64, u64, u64)> {
    let mut out: Vec<(T, u64, u64, u64)> = Vec::with_capacity(copies.len());
    for &(buffer, offset, at, len) in copies {
        match out.last_mut() {
            Some(last) if same(&last.0, &buffer) && last.1 + last.3 == offset && last.2 + last.3 == at => last.3 += len,
            _ => out.push((buffer, offset, at, len)),
        }
    }
    out
}

/// The list in front of a submission's, and its buffer ([`BETWEEN`] bytes).
struct Own {
    list: HostList,
    between: ID3D12Resource,
}

// SAFETY: D3D12 objects are free-threaded, and the list is touched by one
// thread at a time (the submitting one until it is sent, then the one that
// lands it, then nobody until it is taken out of `POOL`).
unsafe impl Send for Own {}

/// Those that have landed: closed, their steps dropped.
static POOL: Mutex<Vec<Own>> = Mutex::new(Vec::new());

/// Since the last frame-rate line: fills and copies done on the card, their
/// bytes, and submissions that waited instead, refused by a check.
static DONE: [std::sync::atomic::AtomicU64; 4] = [const { std::sync::atomic::AtomicU64::new(0) }; 4];

/// What [`Host::card_steps`] took off a submission: the list that does it,
/// for the submission to run first, and the steps the CPU leaves out (their
/// list by address, their place in it).
pub(super) struct Taken {
    own: Own,
    steps: std::collections::HashSet<(usize, usize), std::hash::BuildHasherDefault<AddressHash>>,
    copied: Vec<(Arc<Shadow>, usize, usize)>,
}

impl Taken {
    pub(super) fn list(&self) -> &HostList {
        &self.own.list
    }

    /// Whether step `i` of `list` is done by this.
    pub(super) fn takes(&self, list: &HostList, i: usize) -> bool {
        self.steps.contains(&(std::ptr::from_ref(list) as usize, i))
    }

    /// The steps it does, for each of `runs`: their places in the list, in order (a list the submission names twice
    /// has them under both).
    pub(super) fn by_list(&self, runs: &[&HostList]) -> Vec<Vec<usize>> {
        let ids: Vec<usize> = runs.iter().map(|list| std::ptr::from_ref::<HostList>(*list) as usize).collect();
        onepass::steps_by_list(self.steps.iter().copied(), &ids)
    }

    pub(super) fn copied(&self) -> impl Iterator<Item = Writer> {
        self.copied.iter().map(|(shadow, from, to)| Writer {
            shadow: Some(shadow.clone()),
            from: *from,
            to: *to,
            copy: true,
        })
    }

    /// The submission has run and landed: the list and its buffer are free.
    pub(super) fn landed(self) {
        // The host copies its steps held go; its commands stay until the
        // next use resets it.
        drop(std::mem::take(&mut *self.own.list.deferred()));
        POOL.lock().unwrap_or_else(|e| e.into_inner()).push(self.own);
    }
}

impl Host {
    /// With flights in the air: if what keeps `runs` from being prepared
    /// now is reads of title memory that can all be done on the card
    /// ([`stage`]), the list that does them. `None` if nothing is in the
    /// way or the old wait is due. Under the submission lock.
    pub(super) fn card_steps(&self, runs: &[&HostList], compute: bool) -> Option<Taken> {
        if stage() == 0 || !relay_exact() {
            return None;
        }
        let (written, copies) = {
            let line = line();
            if line.written.is_empty() {
                return None;
            }
            (line.written.clone(), copies(&line))
        };
        blocked_by(&written, runs, false)?;
        let taken = if copied_read(&copies, runs) {
            Err("a copy done on the card is read through another host copy")
        } else {
            self.take_steps(&written, runs, compute)
        };
        match taken {
            Ok(taken) => taken,
            Err(why) => {
                DONE[3].fetch_add(1, Ordering::Relaxed);
                #[cfg(feature = "diagnostics")]
                refused(why);
                let _ = why;
                None
            }
        }
    }

    fn take_steps(&self, written: &[(usize, usize)], runs: &[&HostList], compute: bool) -> Result<Option<Taken>, &'static str> {
        let mut spent = crate::timeline::span("steps on the card: chosen");
        // (Read once: the way the steps of this submission are looked at and the slots made.)
        let lean = lean_steps();
        // (With the summaries of the lists' steps [`onepass`] the steps are not walked, and the lists of the fills are found by a table.)
        let summarized = lean && onepass::on();
        let (does, steps, refreshed) = if summarized {
            does_of_summarized(written, runs, |list| std::ptr::from_ref::<HostList>(*list) as usize)
        } else {
            does_of(written, runs, lean)
        };
        let chosen = plan(&does, stage(), placed_order(), &mut None)?;
        if chosen.is_empty() {
            return Ok(None);
        }
        spent.next("steps on the card: what holds their bytes");
        let mut writers = {
            let line = line();
            let mut all = writers(&line.line);
            all.extend(line.landing.iter().map(|w| Writer {
                shadow: w.shadow.clone(),
                ..*w
            }));
            all
        };
        // The writers that exist now, found by address (`VICTORY_HOLDER_INDEX`);
        // the copies' destinations pushed below are looked at one by one.
        let index = holder_index_on().then(|| WriterIndex::new(writers.len(), |i| (writers[i].from, writers[i].to)));
        let holder_of = |writers: &Vec<Writer>, from: usize, to: usize| match &index {
            Some(index) => holder_indexed(index, writers, from, to),
            None => holder(writers, from, to),
        };
        // (The first list of each address: a submission may name a list twice.)
        let run_at: HashMap<usize, usize> = if summarized {
            let mut at = HashMap::default();
            for (n, list) in runs.iter().enumerate() {
                at.entry(std::ptr::from_ref::<HostList>(*list) as usize).or_insert(n);
            }
            at
        } else {
            HashMap::default()
        };
        let mut ops: Vec<Op> = Vec::with_capacity(chosen.len());
        // The places read (host copy, address, bytes, level): where in the
        // buffer between their bytes are.
        // (With `VICTORY_LEAN_STEPS` made with room for what the steps will put in: the tables grow by doubling otherwise,
        // each doubling a new allocation and every entry put in again.)
        let (places_room, held_room) = if lean { (chosen.len().min(BETWEEN as usize / 256), chosen.len().min(1024)) } else { (0, 0) };
        let mut places: HashMap<(usize, usize, usize, u32), u64> = HashMap::with_capacity_and_hasher(places_room, Default::default());
        // Some 2,800 fills a frame, of a few hundred places: what holds each
        // is looked for once, as long as no copy is added to the writers.
        let mut held: HashMap<(usize, usize), Arc<Shadow>> = HashMap::with_capacity_and_hasher(held_room, Default::default());
        let mut room = HashMap::with_capacity_and_hasher(held_room, Default::default());
        let mut end = 0u64;
        for &(i, level) in &chosen {
            let Some(((src, _), (dst, _))) = reads_writes(&does[i]) else {
                continue;
            };
            let (Does::Copy { len, .. } | Does::Fill { len, .. }) = does[i] else {
                continue;
            };
            if len == 0 {
                return Err("nothing to read");
            }
            let from = match held.get(&(src, len)) {
                Some(from) => from.clone(),
                None => {
                    let from = holder_of(&writers, src, src + len)?;
                    held.insert((src, len), from.clone());
                    from
                }
            };
            let place = (Arc::as_ptr(&from) as usize, src, len, level);
            let at = match places.get(&place) {
                Some(&at) => at,
                None => {
                    if end + len as u64 > BETWEEN {
                        return Err("too much at once");
                    }
                    if !mapped(src, src + len, &mut room) {
                        return Err("not all of what it reads is mapped");
                    }
                    let at = end;
                    end = (end + len as u64).next_multiple_of(256);
                    places.insert(place, at);
                    at
                }
            };
            let to = match does[i] {
                Does::Copy { .. } => {
                    let to = holder_of(&writers, dst, dst + len)?;
                    if elsewhere(&does, Arc::as_ptr(&to) as usize, dst, dst + len) || elsewhere_in(&refreshed, Arc::as_ptr(&to) as usize, dst, dst + len) {
                        return Err("another host copy of what it writes is read");
                    }
                    if !mapped(dst, dst + len, &mut room) {
                        return Err("not all of what it writes is mapped");
                    }
                    // What the steps behind it read of it is in that host
                    // copy, or nowhere.
                    writers.push(Writer {
                        shadow: Some(to.clone()),
                        from: dst,
                        to: dst + len,
                        copy: true,
                    });
                    held.clear();
                    To::Copy(to, dst)
                }
                _ => match if summarized { fill_to_at(runs, &run_at, steps[i]) } else { fill_to(runs, steps[i]) } {
                    Some(FillTo::Slot(heap, index)) => To::Slot(heap, index),
                    Some(FillTo::Buffer(buffer)) => To::Buffer(buffer),
                    None => continue,
                },
            };
            ops.push(Op {
                from: (from, src),
                len,
                at,
                level,
                to,
            });
        }
        spent.next("steps on the card: the list");
        let Some(own) = self.own_list(compute) else {
            return Err("no host list or buffer for it");
        };
        // First every refresh, then the writes: the bytes compared with are
        // the title's. (One step for each host copy, over all that is read of
        // it: a flush brings a copy up to date from the first range asked for
        // to the last, whatever the steps.)
        let mut spans: ByCopy<(Arc<Shadow>, usize, usize)> = ByCopy::default();
        for op in &ops {
            let to = match &op.to {
                To::Copy(to, dst) => Some((to, *dst)),
                _ => None,
            };
            for (shadow, at) in std::iter::once((&op.from.0, op.from.1)).chain(to) {
                let span = spans.entry(Arc::as_ptr(shadow) as usize).or_insert_with(|| (shadow.clone(), at, at + op.len));
                (span.1, span.2) = (span.1.min(at), span.2.max(at + op.len));
            }
        }
        for (shadow, from, to) in spans.into_values() {
            shadow.refresh(&own.list, from as u64, (to - from) as u64);
        }
        let mut copied = Vec::new();
        for op in &ops {
            if let To::Copy(to, dst) = &op.to {
                to.gpu_writes(&own.list, *dst as u64, op.len as u64);
                copied.push((to.clone(), *dst, *dst + op.len));
            }
        }
        #[cfg(feature = "diagnostics")]
        own.list.note_op(|| format!("{} copies of title memory done on the card, {end} bytes (VICTORY_CARD_STEPS)", ops.len()));
        let levels = ops.iter().map(|op| op.level).max().unwrap_or(0);
        for level in 0..=levels {
            let mut reads = Vec::new();
            let mut writes = Vec::new();
            for op in ops.iter().filter(|op| op.level == level) {
                let len = op.len as u64;
                let read = (&op.from.0.buffer, op.from.0.offset(op.from.1 as u64), op.at, len);
                // (Each place once.)
                if places.remove(&(Arc::as_ptr(&op.from.0) as usize, op.from.1, op.len, level)).is_some() {
                    reads.push(read);
                }
                match &op.to {
                    To::Copy(to, dst) => writes.push((&to.buffer, to.offset(*dst as u64), op.at, len)),
                    To::Buffer(buffer) => writes.push((buffer, 0, op.at, len)),
                    To::Slot(..) => {}
                }
            }
            record(level == 0, &own.list.list, &own.between, &reads, &writes);
        }
        own.list.close();
        // SAFETY: a live buffer.
        let gpu = unsafe { own.between.GetGPUVirtualAddress() };
        spent.next("steps on the card: the slots");
        // (11,000 slots a frame in a street, two descriptors each: on
        // several threads, a slot always on the same one, in the steps'
        // order.)
        let parts = if ops.len() < 256 { 1 } else { 8 };
        crate::land::each_under(parts, &|part| {
            if lean {
                // The hold on the buffer is taken once, the look at the heaps once for each heap: not for each of
                // thousands of slots, by threads that all do it on the same counter of the same buffer.
                let mut mine: Vec<(usize, u32, u64, u32)> =
                    Vec::with_capacity(ops.len() / parts + 1);
                for op in &ops {
                    if let To::Slot(heap, index) = &op.to
                        && *index as usize / 4 % parts == part
                    {
                        mine.push((*heap, *index, gpu + op.at, op.len as u32));
                    }
                }
                // (In the steps' order, a heap at a time: the slots of one heap are made one after the other in
                // that order, as before; those of two heaps have nothing in common.)
                mine.sort_by_key(|slot| slot.0);
                for heap in mine.chunk_by(|a, b| a.0 == b.0) {
                    crate::descriptor::constant_slots(heap[0].0, &own.between, heap.iter().map(|&(_, index, at, len)| (index, at, len)));
                }
                return;
            }
            for op in &ops {
                if let To::Slot(heap, index) = &op.to
                    && *index as usize / 4 % parts == part
                {
                    // (A heap that is gone: nothing can read the slot.)
                    crate::descriptor::constant_slot(*heap, *index, &own.between, gpu + op.at, op.len as u32);
                }
            }
        });
        let copies = ops.iter().filter(|op| matches!(op.to, To::Copy(..))).count();
        DONE[0].fetch_add((ops.len() - copies) as u64, Ordering::Relaxed);
        DONE[1].fetch_add(copies as u64, Ordering::Relaxed);
        DONE[2].fetch_add(ops.iter().map(|op| op.len as u64).sum(), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        noted(&ops);
        spent.end();
        Ok(Some(Taken {
            own,
            steps: chosen.iter().map(|&(i, _)| steps[i]).collect(),
            copied,
        }))
    }

    /// A list of ours open for recording, and its buffer.
    fn own_list(&self, compute: bool) -> Option<Own> {
        // (Of the type the queue it goes to runs: `VICTORY_ASYNC_COMPUTE=2`.)
        let pooled = {
            let mut pool = POOL.lock().unwrap_or_else(|e| e.into_inner());
            let at = pool.iter().rposition(|own| own.list.is_compute() == compute);
            at.map(|at| pool.remove(at))
        };
        if let Some(own) = pooled {
            own.list.reset();
            return Some(own);
        }
        let between = self.uav_buffer(BETWEEN)?;
        name(&between, || "copies of title memory on the card".into());
        let list = self.new_list_of(compute).ok()?;
        Some(Own { list, between })
    }
}

/// Diagnostics: the copies and fills done on the card since the last
/// frame-rate line, by bytes, host copy read and host copy written (start
/// and length; zeros for a fill): how many, and the last title addresses.
#[cfg(feature = "diagnostics")]
type Noted = std::collections::HashMap<(usize, (u64, u64), (u64, u64)), (u64, usize, usize)>;
#[cfg(feature = "diagnostics")]
static NOTED: Mutex<Option<Noted>> = Mutex::new(None);

#[cfg(feature = "diagnostics")]
fn noted(ops: &[Op]) {
    let mut noted = NOTED.lock().unwrap_or_else(|e| e.into_inner());
    let noted = noted.get_or_insert_with(Default::default);
    for op in ops {
        let (to, dst) = match &op.to {
            To::Copy(to, dst) => (to.span(), *dst),
            To::Slot(..) | To::Buffer(_) => ((0, 0), 0),
        };
        let e = noted.entry((op.len, op.from.0.span(), to)).or_default();
        *e = (e.0 + 1, op.from.1, dst);
    }
}

/// For the frame-rate line.
pub(super) fn log(frames: f64) {
    if stage() == 0 {
        return;
    }
    let d: Vec<f64> = DONE.iter().map(|d| d.swap(0, Ordering::Relaxed) as f64 / frames).collect();
    tracing::info!(
        "steps done on the card instead of waiting for flights, per frame (VICTORY_CARD_STEPS={}): {:.2} fills, {:.2} buffer \
         copies, {:.1} KB; {:.2} submissions refused and waited",
        stage(),
        d[0],
        d[1],
        d[2] / 1024.0,
        d[3],
    );
    if merge_on() {
        let j: Vec<f64> = JOINED.iter().map(|j| j.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "copies recorded for steps done on the card per frame (VICTORY_STEP_MERGE=1): {:.0} planned, {:.0} recorded after joining those that continue each other",
            j[0],
            j[1],
        );
    }
    if merge_reads_on() {
        let r: Vec<f64> = READS.iter().map(|r| r.swap(0, Ordering::Relaxed) as f64 / frames).collect();
        tracing::info!(
            "reads of steps done on the card per frame (VICTORY_STEPS_MERGE_READS=1): {:.0} places read, {:.0} copies after joining those a few bytes apart, {:.1} KB of gaps read with them",
            r[0],
            r[1],
            r[2] / 1024.0,
        );
    }
    #[cfg(feature = "diagnostics")]
    {
        let noted = NOTED.lock().unwrap_or_else(|e| e.into_inner()).take().unwrap_or_default();
        let mut noted: Vec<_> = noted.into_iter().collect();
        noted.sort_unstable_by_key(|(_, e)| std::cmp::Reverse(e.0));
        for ((len, from, to), (n, src, dst)) in noted.into_iter().take(5) {
            let n = n as f64 / frames;
            if to.1 == 0 {
                tracing::info!("done on the card, per frame: {n:.2} fills of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x})", from.0, from.1);
            } else {
                tracing::info!(
                    "done on the card, per frame: {n:.2} copies of {len:#x} bytes from the host copy of {:#x}+{:#x} (last {src:#x}) \
                     to that of {:#x}+{:#x} (last {dst:#x})",
                    from.0,
                    from.1,
                    to.0,
                    to.1
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn copy(dst: usize, src: usize, len: usize, asked: bool) -> Does {
        Does::Copy { dst, src, len, asked }
    }

    fn fill(src: usize, len: usize, asked: bool) -> Does {
        Does::Fill { src, len, asked, to: true }
    }

    fn stuck(src: usize, len: usize) -> Does {
        Does::Fill { src, len, asked: false, to: false }
    }

    /// What is done on the card, as (place, level), in stage 2.
    fn planned(does: &[Does]) -> Vec<(usize, u32)> {
        plan(does, 2, true, &mut None).unwrap()
    }

    /// Only what is asked for is taken, all of it, and a step that stands
    /// in none of their ways stays where it is.
    #[test]
    fn steps_apart_from_one_another_are_taken() {
        let does = [
            copy(1000, 5000, 100, false),
            fill(2000, 64, true),
            Does::Refresh { shadow: 1, from: 0, to: 9000 },
            copy(3000, 6000, 100, true),
            fill(1000, 64, false),
            Does::Read { from: 3100, to: 4000, asked: false },
            stuck(3100, 5),
        ];
        assert_eq!(planned(&does), [(1, 0), (3, 0)]);
        // Two fills of the same bytes.
        assert_eq!(plan(&[fill(0, 64, true), fill(32, 64, true)], 1, true, &mut None), Ok(vec![(0, 0), (1, 0)]));
        assert_eq!(plan(&[fill(0, 64, false)], 1, true, &mut None), Ok(vec![]));
        // Ends that touch are apart.
        let does = [
            copy(1000, 5000, 100, true),
            copy(900, 9000, 100, false),
            copy(1100, 9000, 5, false),
            copy(4900, 9000, 100, false),
        ];
        assert_eq!(planned(&does), [(0, 0)]);
    }

    /// What depends on a taken step, or it on it, is taken too, on the level
    /// the order of the steps asks for.
    #[test]
    fn steps_that_depend_on_one_another_are_taken_in_order() {
        let taken = copy(1000, 5000, 100, true);
        // Reads what it writes, behind and ahead of it.
        assert_eq!(planned(&[taken, fill(1099, 64, false)]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[fill(1099, 64, false), taken]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[taken, copy(7000, 1050, 10, false)]), [(0, 0), (1, 1)]);
        // Writes what it reads, and what it writes.
        assert_eq!(planned(&[copy(5099, 9000, 1, false), taken]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[taken, copy(5099, 9000, 1, false)]), [(0, 0), (1, 1)]);
        assert_eq!(planned(&[taken, copy(999, 9000, 2, false)]), [(0, 0), (1, 1)]);
        // The same list twice.
        assert_eq!(planned(&[taken, taken]), [(0, 0), (1, 1)]);
        // A chain, with steps apart from it in between, and two fills of one
        // copy on one level.
        let does = [
            copy(8000, 7000, 50, false),
            taken,
            copy(2000, 1010, 20, false),
            fill(90_000, 10, false),
            fill(2000, 20, false),
            fill(2010, 20, true),
            copy(1000, 2005, 5, false),
            fill(1000, 100, false),
        ];
        assert_eq!(planned(&does), [(1, 0), (2, 1), (4, 2), (5, 2), (6, 2), (7, 3)]);
        // A fill is in nobody's way.
        assert_eq!(planned(&[taken, fill(5000, 100, false)]), [(0, 0)]);
        // A copy ahead that a taken one only shares its source with.
        assert_eq!(planned(&[copy(7000, 5000, 10, false), taken]), [(1, 0)]);
    }

    /// A copy at its place in a list is in the way of what reads or writes its
    /// bytes behind it, not of what did ahead of it.
    #[test]
    fn copies_at_their_place_stand_in_the_way_of_what_is_behind_them() {
        let placed = Does::Placed { dst: 2000, src: 3000, len: 64 };
        let behind = |does: &[Does]| {
            let mut found = None;
            let refused = plan(does, 2, true, &mut found);
            assert!(refused.is_ok() || found.is_some() && refused == Err(BEHIND_PLACED));
            assert_eq!(plan(does, 2, false, &mut None).is_err(), does.len() > 1 && !matches!(does, [_, Does::Fill { src: 5000, .. }]));
            found
        };
        assert_eq!(behind(&[fill(2032, 64, true), placed]), None);
        assert_eq!(behind(&[placed, fill(2032, 64, true)]), Some((0, 1)));
        assert_eq!(behind(&[placed, fill(5000, 64, true)]), None);
        assert_eq!(behind(&[fill(2000, 8, true), placed, fill(2000, 8, true)]), Some((1, 2)));
        // One that writes what it copies from, and one that writes over it.
        assert_eq!(behind(&[copy(3000, 5000, 8, true), placed]), None);
        assert_eq!(behind(&[placed, copy(3000, 5000, 8, true)]), Some((0, 1)));
        assert_eq!(behind(&[copy(2060, 5000, 8, true), placed]), None);
        assert_eq!(behind(&[placed, copy(2060, 5000, 8, true)]), Some((0, 1)));
    }

    /// Whatever the steps, done level by level (in a level every read
    /// before every write) they read and leave what they read and leave one
    /// after the other, the CPU's steps ahead of all of them.
    #[test]
    fn levels_do_what_the_order_does() {
        let mut seed = 12345u32;
        let mut random = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let (mut done, mut refused) = (0, 0);
        for _ in 0..4000 {
            let does: Vec<Does> = (0..2 + random(9))
                .map(|_| {
                    let (at, other, len) = (random(60) as usize, random(60) as usize, 1 + random(8) as usize);
                    let asked = random(4) == 0;
                    match random(8) {
                        0..4 => copy(at, other, len, asked),
                        4..6 => fill(at, len, asked),
                        6 => stuck(at, len),
                        _ => Does::Read { from: at, to: at + len, asked: false },
                    }
                })
                .collect();
            let Ok(chosen) = plan(&does, 2, true, &mut None) else {
                refused += 1;
                continue;
            };
            done += 1;
            let start: Vec<u32> = (0..80).collect();
            // One after the other: what each reads, and what is left.
            let mut memory = start.clone();
            let mut wanted = Vec::new();
            for d in &does {
                let (from, to) = match *d {
                    Does::Read { from, to, .. } => (from, to),
                    _ => reads_writes(d).unwrap().0,
                };
                wanted.push(memory[from..to].to_vec());
                if let Does::Copy { dst, src, len, .. } = *d {
                    memory.copy_within(src..src + len, dst);
                }
            }
            // The CPU's, then the card's.
            let mut cpu = start.clone();
            let taken = |i: usize| chosen.iter().any(|c| c.0 == i);
            for (i, d) in does.iter().enumerate().filter(|&(i, _)| !taken(i)) {
                let (from, to) = match *d {
                    Does::Read { from, to, .. } => (from, to),
                    _ => reads_writes(d).unwrap().0,
                };
                assert_eq!(cpu[from..to], wanted[i][..], "step {i} of {does:?}");
                if let Does::Copy { dst, src, len, .. } = *d {
                    cpu.copy_within(src..src + len, dst);
                }
            }
            let mut card = cpu.clone();
            for level in 0..=chosen.iter().map(|c| c.1).max().unwrap_or(0) {
                let of: Vec<usize> = chosen.iter().filter(|c| c.1 == level).map(|c| c.0).collect();
                let before = card.clone();
                for i in of {
                    let ((from, to), (dst, _)) = reads_writes(&does[i]).unwrap();
                    assert_eq!(before[from..to], wanted[i][..], "step {i} of {does:?}: {chosen:?}");
                    if matches!(does[i], Does::Copy { .. }) {
                        card[dst..dst + to - from].copy_from_slice(&before[from..to]);
                    }
                }
            }
            assert_eq!(card, memory, "{does:?}: {chosen:?}");
        }
        assert!(done > 500 && refused > 100, "{done} {refused}");
    }

    /// What cannot be done on the card refuses the lot, where it stands in
    /// the way.
    #[test]
    fn steps_in_the_way_are_refused() {
        let taken = copy(1000, 5000, 100, true);
        assert!(plan(&[taken, Does::Run], 2, true, &mut None).is_err());
        // A texture that itself waits for flights; a fill that cannot be
        // taken but waits.
        assert!(plan(&[taken, Does::Read { from: 1, to: 2, asked: true }], 2, true, &mut None).is_err());
        assert!(plan(&[Does::Fill { src: 1, len: 1, asked: true, to: false }], 2, true, &mut None).is_err());
        // Reads on the CPU of what it writes: behind it, not ahead of it.
        for read in [Does::Read { from: 900, to: 1001, asked: false }, stuck(1050, 3)] {
            assert!(plan(&[taken, read], 2, true, &mut None).is_err(), "{read:?} behind");
            assert_eq!(planned(&[read, taken]), [(1, 0)], "{read:?} ahead");
        }
        // Also of what is taken along.
        assert!(plan(&[taken, copy(3000, 1000, 10, false), stuck(3005, 1)], 2, true, &mut None).is_err());
        // Into itself, and copies without stage 2.
        assert!(plan(&[copy(1000, 1050, 100, true)], 2, true, &mut None).is_err());
        assert!(plan(&[taken], 1, true, &mut None).is_err());
        assert!(plan(&[fill(1000, 4, true), copy(1000, 5000, 100, false)], 1, true, &mut None).is_err());
    }

    /// The plan does nothing for a refresh: with the refreshes left out of the
    /// steps (`does_of` with `lean`) the same steps are chosen, at the same
    /// levels, or refused for the same reason, and what [`elsewhere_in`] says of
    /// the refreshes is what [`elsewhere`] says of them among the steps.
    #[test]
    fn leaving_the_refreshes_out_of_the_steps_changes_no_choice() {
        let mut seed = 777u32;
        let mut random = move |n: u32| {
            seed = seed.wrapping_mul(1_103_515_245).wrapping_add(12345);
            (seed >> 16) % n
        };
        let (mut done, mut refused, mut behind) = (0, 0, 0);
        for _ in 0..4000 {
            let (mut with, mut without) = (Vec::new(), Vec::new());
            // Where each of `with` is in `without` (none: a refresh).
            let mut place: Vec<Option<usize>> = Vec::new();
            for _ in 0..2 + random(9) {
                for _ in 0..random(3) {
                    with.push(Does::Refresh { shadow: 1 + random(3) as usize, from: random(60) as usize, to: 60 + random(20) as usize });
                    place.push(None);
                }
                let (at, other, len) = (random(60) as usize, random(60) as usize, 1 + random(8) as usize);
                let asked = random(4) == 0;
                let step = match random(10) {
                    0..4 => copy(at, other, len, asked),
                    4..6 => fill(at, len, asked),
                    6 => stuck(at, len),
                    7 => Does::Read { from: at, to: at + len, asked: false },
                    8 => Does::Placed { dst: at, src: other, len },
                    _ => fill(at, len, false),
                };
                place.push(Some(without.len()));
                without.push(step);
                with.push(step);
            }
            for by_order in [true, false] {
                let (mut found_with, mut found_without) = (None, None);
                let a = plan(&with, 2, by_order, &mut found_with);
                let b = plan(&without, 2, by_order, &mut found_without);
                match (a, b) {
                    (Ok(a), Ok(b)) => {
                        done += 1;
                        let a: Vec<(usize, u32)> = a.iter().map(|&(i, level)| (place[i].expect("a refresh is never taken"), level)).collect();
                        assert_eq!(a, b, "{with:?}");
                    }
                    (Err(a), Err(b)) => {
                        refused += 1;
                        assert_eq!(a, b, "{with:?}");
                    }
                    other => panic!("{other:?} for {with:?}"),
                }
                let found = found_with.map(|(p, i)| (place[p].unwrap(), place[i].unwrap()));
                assert_eq!(found, found_without, "{with:?}");
                behind += usize::from(found.is_some());
            }
            let refreshed: Vec<Refreshed> = with
                .iter()
                .filter_map(|d| if let Does::Refresh { shadow, from, to } = *d { Some((shadow, from, to)) } else { None })
                .collect();
            for _ in 0..20 {
                let (shadow, from) = (random(4) as usize, random(80) as usize);
                let to = from + 1 + random(10) as usize;
                assert_eq!(
                    elsewhere(&with, shadow, from, to),
                    elsewhere(&without, shadow, from, to) || elsewhere_in(&refreshed, shadow, from, to)
                );
            }
        }
        assert!(done > 500 && refused > 100, "{done} {refused} {behind}");
    }

    #[test]
    fn a_refresh_of_another_host_copy_is_seen() {
        let does = [Does::Refresh { shadow: 7, from: 100, to: 200 }];
        assert!(!elsewhere(&does, 7, 100, 200));
        assert!(elsewhere(&does, 8, 199, 300));
        assert!(!elsewhere(&does, 8, 200, 300));
    }

    /// The writers found through the index are those a linear look finds, in
    /// the same order, and `holder_indexed` answers as `holder` does (the
    /// same host copy, or the same refusal) for any set of writers and ranges.
    #[test]
    fn the_index_of_writers_finds_what_a_look_at_all_of_them_finds() {
        let mut state = 0x2545_f491_4f6c_dd1du64;
        let mut next = move |m: u64| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            (state % m) as usize
        };
        let data: &'static mut [u8] = Box::leak(vec![0u8; 0x10000].into_boxed_slice());
        let shadows = [
            Shadow::unbacked(0x1000, data.as_mut_ptr(), 0x10000, false),
            Shadow::unbacked(0x1000, data.as_mut_ptr(), 0x10000, false),
        ];
        for round in 0..40 {
            let n = next(60) + 1;
            let mut writers: Vec<Writer> = (0..n)
                .map(|_| {
                    let from = 0x1000 + next(0xf000);
                    let len = if next(8) == 0 { next(0x8000) } else { next(0x200) };
                    Writer {
                        shadow: match next(10) {
                            0 => None,
                            k if k < 8 => Some(shadows[0].clone()),
                            _ => Some(shadows[1].clone()),
                        },
                        from,
                        to: from + len,
                        copy: next(3) == 0,
                    }
                })
                .collect();
            let index = WriterIndex::new(writers.len(), |i| (writers[i].from, writers[i].to));
            // Some pushed after the index was made, as the copies' destinations are.
            for _ in 0..next(5) {
                let from = 0x1000 + next(0xf000);
                writers.push(Writer { shadow: Some(shadows[0].clone()), from, to: from + 1 + next(0x400), copy: true });
            }
            for _ in 0..200 {
                let from = 0x1000 + next(0xf000);
                let to = (from + 1 + if next(6) == 0 { next(0x4000) } else { next(0x100) }).min(0x11000);
                let slow = holder(&writers, from, to);
                let fast = holder_indexed(&index, &writers, from, to);
                match (slow, fast) {
                    (Ok(a), Ok(b)) => assert!(Arc::ptr_eq(&a, &b), "round {round}: {from:#x}..{to:#x}"),
                    (Err(a), Err(b)) => assert_eq!(a, b, "round {round}: {from:#x}..{to:#x}"),
                    (a, b) => panic!("round {round}: {from:#x}..{to:#x}: {:?} against {:?}", a.is_ok(), b.is_ok()),
                }
                let all: Vec<usize> = (0..index.len).filter(|&i| meet((writers[i].from, writers[i].to), (from, to))).collect();
                assert_eq!(index.meeting(|i| (writers[i].from, writers[i].to), from, to), all);
            }
        }
    }

    /// Reads that lie together in their host copy and in the buffer between
    /// are one copy; nothing else is, and the bytes the copies read cover
    /// every byte the reads did, and only gaps of at most 256 more.
    #[test]
    fn reads_that_lie_together_are_one_copy() {
        // (buffer, offset in it, offset in between, bytes): constants of 480
        // bytes in a ring at multiples of 512, then one elsewhere, then another buffer.
        let reads = [
            (1usize, 4096u64, 0u64, 480u64),
            (1, 4608, 512, 480),
            (1, 5120, 1024, 480),
            (1, 9000, 1536, 480), // other place of the buffer: not the next bytes
            (2, 5632, 2048, 480), // another buffer
            (2, 6144, 2560, 480),
            (2, 8192, 3072, 100), // between is next, the buffer's offset is not
        ];
        let merged = merged_reads(&reads, 256);
        assert_eq!(merged, [(0, 1504), (3, 480), (4, 992), (6, 100)]);
        // What was read is read, and what is added lies in the gaps.
        let covered = |at: u64| merged.iter().any(|&(i, len)| at >= reads[i].2 && at < reads[i].2 + len);
        for &(_, _, at, len) in &reads {
            assert!((at..at + len).all(|x| covered(x)));
        }
        let bytes: u64 = merged.iter().map(|m| m.1).sum();
        let wanted: u64 = reads.iter().map(|r| r.3).sum();
        assert!(bytes - wanted <= 256 * (reads.len() as u64 - merged.len() as u64));
        // A gap past the limit, a place behind the last, and nothing.
        assert_eq!(merged_reads(&[(1, 0, 0, 100), (1, 1000, 1000, 100)], 256).len(), 2);
        assert_eq!(merged_reads(&[(1, 0, 100, 100), (1, 0, 0, 100)], 256).len(), 2);
        assert!(merged_reads(&[], 256).is_empty());
        // Without gaps allowed only touching reads join.
        assert_eq!(merged_reads(&reads[..3], 0).len(), 3);
        assert_eq!(merged_reads(&[(1, 0, 0, 512), (1, 512, 512, 512)], 0), [(0, 1024)]);
    }

    /// A submission being sent is not a flight in the line until its send is over, and the thread of stage A looks at the
    /// next submission meanwhile: the card copies of the one being sent are what it is asked about from the moment they
    /// are registered, and end with it (rev2).
    #[test]
    fn the_card_copies_of_a_submission_being_sent_are_seen_before_it_is_a_flight() {
        let data: &'static mut [u8] = Box::leak(vec![0u8; 0x4000].into_boxed_slice());
        let (mine, other) = (Shadow::unbacked(0x1000, data.as_mut_ptr(), 0x4000, false), Shadow::unbacked(0x1000, data.as_mut_ptr(), 0x4000, false));
        let mut line = Line::new();
        assert!(copies_in(&line).is_empty());
        let copy = || Writer { shadow: Some(mine.clone()), from: 0x2000, to: 0x2100, copy: true };
        let id = line.register_pending_with(vec![(0x2000, 0x2100)], vec![copy()]);
        let seen = copies_in(&line);
        assert_eq!(seen.len(), 1);
        // Another host copy of the destination refreshed by the next submission: it waits. The same host copy, or other
        // bytes: it does not.
        let refresh = |shadow: &Arc<Shadow>, at: usize, len: usize| vec![Step::Refresh(shadow.clone(), at, len)];
        assert!(copied_read(&seen, &[refresh(&other, 0x2040, 0x40)]), "another host copy of the destination");
        assert!(!copied_read(&seen, &[refresh(&mine, 0x2040, 0x40)]), "the copy's own host copy");
        assert!(!copied_read(&seen, &[refresh(&other, 0x2100, 0x40)]), "past the destination");
        // Made a flight (the registration ends under the lock the push holds), or dropped: the line holds its copies
        // only while it is registered.
        line.clear_pending(id);
        assert!(copies_in(&line).is_empty());
        let id = line.register_pending_with(vec![(0x2000, 0x2100)], vec![copy()]);
        {
            let Line { pending, pending_copies, .. } = &mut line;
            PendingFlight(Some(id)).done(pending, pending_copies);
        }
        assert!(copies_in(&line).is_empty() && line.pending_copies.is_empty());
    }

    /// A value written behind a flight, or a read-back, is in no host copy
    /// before it lands.
    #[test]
    fn what_no_host_copy_holds_is_refused() {
        let writers = [Writer { shadow: None, from: 100, to: 108, copy: false }];
        assert!(holder(&writers, 104, 120).is_err());
    }

    /// Reads are all before writes, between two buffers and within one, and
    /// the buffers are usable in the lists that follow in the same call.
    #[test]
    fn the_card_copies_what_is_recorded() {
        let Some(host) = host() else {
            return; // no host GPU
        };
        let (a, a_data) = host.shared_buffer(1 << 16).unwrap();
        let (b, b_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: mappings of 64 KiB.
        let (a_bytes, b_bytes) =
            unsafe { (std::slice::from_raw_parts_mut(a_data, 1 << 16), std::slice::from_raw_parts_mut(b_data, 1 << 16)) };
        for (i, (x, y)) in a_bytes.iter_mut().zip(b_bytes.iter_mut()).enumerate() {
            (*x, *y) = (i as u8 ^ (i >> 8) as u8, 0xee);
        }
        let was = a_bytes.to_vec();
        let own = host.own_list(false).unwrap();
        // Within `a` (there and back), from `a` to `b`, and a read alone.
        record(
            true,
            &own.list.list,
            &own.between,
            &[(&a, 33, 0, 100), (&a, 2000, 256, 100), (&a, 5001, 512, 7), (&a, 9, 1024, 4)],
            &[(&a, 2000, 0, 100), (&a, 33, 256, 100), (&b, 77, 512, 7)],
        );
        // A level behind it, of what it wrote.
        record(false, &own.list.list, &own.between, &[(&b, 77, 2048, 7)], &[(&b, 1077, 2048, 7)]);
        own.list.close();
        // A list behind it that uses the buffers in other ways.
        let next = host.new_list().unwrap();
        let (out, out_data) = host.shared_buffer(1 << 16).unwrap();
        // SAFETY: an open list; live buffers.
        unsafe {
            next.list.CopyBufferRegion(&out, 0, &own.between, 1024, 4);
            next.list.CopyBufferRegion(&b, 4096, &a, 33, 100);
        }
        next.close();
        host.execute(&[&own.list, &next]);
        // With the debug layer (`VICTORY_D3D12_DEBUG=1`, diagnostics).
        assert_eq!(host.log_messages(), None);
        assert_eq!(a_bytes[2000..2100], was[33..133]);
        assert_eq!(a_bytes[33..133], was[2000..2100]);
        assert_eq!(b_bytes[77..84], was[5001..5008]);
        assert_eq!(b_bytes[1077..1084], was[5001..5008]);
        assert_eq!(b_bytes[4096..4196], was[2000..2100]);
        // SAFETY: a mapping of 64 KiB.
        assert_eq!(unsafe { std::slice::from_raw_parts(out_data, 4) }, &was[9..13]);
        assert!(b_bytes[..77].iter().all(|&x| x == 0xee));
        a_bytes[2000..2100].copy_from_slice(&was[2000..2100]);
        a_bytes[33..133].copy_from_slice(&was[33..133]);
        assert_eq!(a_bytes, &was[..]);
        // As the landing does: its next use records anew.
        POOL.lock().unwrap().push(own);
        let own = host.own_list(false).unwrap();
        own.list.close();
    }

    /// The whole way, in a process of its own with the relay on:
    /// `VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 cargo test -p victory-gpu --lib
    /// card_steps -- --ignored --test-threads=1`. A flight's list writes
    /// part of what the next submission's copy reads, the title wrote the
    /// rest; the copy is done on the card behind the flight, and a second
    /// one reads what the first wrote. Title memory ends as on the CPU.
    #[test]
    #[ignore = "wants VICTORY_RELAY=2 VICTORY_CARD_STEPS=2 and a process of its own"]
    fn a_copy_behind_a_flight_reads_what_the_flight_wrote() {
        use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc};
        const LEN: usize = 4 << 20;
        // Without it (`TEST_UNWATCHED=1`) the title's bytes cannot be put in
        // under a flight's writes: the flights that declared them are waited
        // for.
        let write_watch = if std::env::var_os("TEST_UNWATCHED").is_some() { 0 } else { 0x20_0000 };
        let Some(host) = host() else {
            return;
        };
        assert!(relay_exact() && stage() == 2, "set VICTORY_RELAY=2 VICTORY_CARD_STEPS=2");
        // Two title buffers: in the first a copy within it, from it into the
        // second, and back.
        let (title, start) = {
            // SAFETY: memory of our own, never freed.
            let base = unsafe { VirtualAlloc(std::ptr::null(), LEN, MEM_RESERVE | MEM_COMMIT | write_watch, PAGE_READWRITE) }.cast::<u8>();
            assert!(!base.is_null());
            // SAFETY: `LEN` bytes just committed.
            (unsafe { std::slice::from_raw_parts_mut(base, LEN) }, base as usize)
        };
        const HALF: usize = LEN / 2;
        crate::shadow::register(0x1111, start as u64, HALF as u64, false);
        crate::shadow::register(0x2222, (start + HALF) as u64, HALF as u64, false);
        let first = crate::shadow::find(start as u64, 64).unwrap();
        let (slow_from, slow_to) = (host.uav_buffer(64 << 20).unwrap(), host.uav_buffer(64 << 20).unwrap());
        let before = DONE[1].load(Ordering::Relaxed);
        let mut lists = Vec::new();
        for round in 0..8usize {
            // The title writes; then a "shader" of a flight writes 300 bytes
            // of what the copy reads, behind some work.
            let seed = |i: usize| (i * 7 + round * 13 + i / 251) as u8;
            for (i, b) in title[4000..6000].iter_mut().enumerate() {
                *b = seed(i);
            }
            let (upload, data) = host.upload_buffer(4096).unwrap();
            for i in 0..300 {
                // SAFETY: inside the mapping.
                unsafe { data.add(i).write(0x80 | (round * 31 + i) as u8) };
            }
            let a = host.new_list().unwrap();
            first.refresh(&a, start as u64, HALF as u64);
            first.gpu_writes(&a, start as u64, HALF as u64);
            // SAFETY: an open list; live buffers.
            unsafe {
                for _ in 0..24 {
                    a.list.CopyBufferRegion(&slow_to, 0, &slow_from, 0, 64 << 20);
                }
                a.list.CopyBufferRegion(&first.buffer, 4500, &upload, 0, 300);
                a.list
                    .ResourceBarrier(&[transition(&first.buffer, D3D12_RESOURCE_STATE_COPY_DEST, D3D12_RESOURCE_STATE_COMMON)]);
            }
            a.close();
            host.execute_own(&a);
            // With the flight in the air, beside what it writes.
            for (i, b) in title[5300..5400].iter_mut().enumerate() {
                *b = seed(1300 + i) ^ 0x5c;
            }
            // Thousands of runs of bytes of its own.
            for b in title[1_700_000..1_708_000].iter_mut().step_by(2) {
                *b = 1 + round as u8;
            }
            // Into the second buffer, and within the first.
            let b = host.new_list().unwrap();
            b.defer_copy((start + HALF + 100) as *mut u8, (start + 4001) as *const u8, 1500);
            b.defer_copy((start + 900_001) as *mut u8, (start + 4400) as *const u8, 777);
            b.defer_copy((start + HALF + 10_000) as *mut u8, (start + 1_700_000) as *const u8, 8000);
            // What depends on them: a copy of the first copy, upload memory
            // filled from that, and the first one's bytes copied over.
            b.defer_copy((start + 1_500_000) as *mut u8, (start + HALF + 600) as *const u8, 700);
            let (staging, staged) = b.upload(host, 4096).unwrap();
            b.defer_fill_buffer(&staging, staged, (start + 1_500_100) as *const u8, 500);
            b.defer_copy((start + HALF + 300) as *mut u8, (start + 8000) as *const u8, 64);
            b.close();
            host.execute_own(&b);
            // What the first wrote, read by the next submission.
            let c = host.new_list().unwrap();
            c.defer_copy((start + 1_200_000) as *mut u8, (start + HALF + 100) as *const u8, 1500);
            c.close();
            let number = host.execute_own(&c);
            settle_now(WHY_SYNC);
            assert!(FINISHED.load(Ordering::Acquire) >= number);
            let wanted: Vec<u8> = (0..2000)
                .map(|i| match i {
                    500..800 => 0x80 | (round * 31 + i - 500) as u8,
                    1300..1400 => seed(i) ^ 0x5c,
                    _ => seed(i),
                })
                .collect();
            assert!(title[4000..6000] == wanted[..], "what the flight wrote, round {round}");
            // (The copy over the first one's bytes came last.)
            let mut over = wanted[1..1501].to_vec();
            over[200..264].fill(0);
            assert!(title[HALF + 100..HALF + 1600] == over[..], "the copy to the other buffer, round {round}");
            assert!(title[1_500_000..1_500_700] == wanted[501..1201], "the copy behind a copy, round {round}");
            // SAFETY: the mapping, which the flight wrote and has landed.
            let staged = unsafe { std::slice::from_raw_parts(staged, 500) };
            assert!(staged == &wanted[601..1101], "the fill behind it, round {round}");
            assert!(title[900_001..900_778] == wanted[400..1177], "the copy within the buffer, round {round}");
            assert!(title[1_200_000..1_201_500] == over[..], "the copy of the copy, round {round}");
            assert!(title[HALF..HALF + 100].iter().all(|&x| x == 0));
            assert!(title[HALF + 1600..HALF + 10_000].iter().all(|&x| x == 0));
            assert!(title[HALF + 18_000..].iter().all(|&x| x == 0));
            let runs = &title[HALF + 10_000..HALF + 18_000];
            assert!(runs.chunks(2).all(|b| b == [1 + round as u8, 0]), "the runs, round {round}");
            a.keep(&upload);
            lists.extend([a, b, c]);
        }
        assert_eq!(host.log_messages(), None);
        let done = DONE[1].load(Ordering::Relaxed) - before;
        eprintln!("{done} of 48 copies were done on the card, {} submissions waited", DONE[3].load(Ordering::Relaxed));
        eprintln!("settles for title memory under shader writes: {}", LATE_STATS[WHY_STALE].load(Ordering::Relaxed));
        assert!(done >= 20, "only {done} copies were done on the card");
    }
}

#[cfg(test)]
mod join_tests {
    use super::joined_copies;

    /// Copies that continue each other (same buffer, next offset, next place
    /// between) are one; others stay as they are, in their order.
    #[test]
    fn copies_that_continue_each_other_are_joined() {
        let same = |a: &u32, b: &u32| a == b;
        let planned = [
            (1u32, 0u64, 0u64, 16u64),
            (1, 16, 16, 16),
            (1, 32, 32, 8),
            // Another buffer; the offset in the buffer jumps; the place between jumps.
            (2, 40, 40, 8),
            (2, 100, 48, 8),
            (2, 108, 60, 8),
            (2, 116, 68, 4),
        ];
        assert_eq!(joined_copies(&planned, same), vec![(1, 0, 0, 40), (2, 40, 40, 8), (2, 100, 48, 8), (2, 108, 60, 12)]);
        assert!(joined_copies::<u32>(&[], same).is_empty());
        // The same bytes: the sum of the lengths.
        let total = |v: &[(u32, u64, u64, u64)]| v.iter().map(|c| c.3).sum::<u64>();
        assert_eq!(total(&joined_copies(&planned, same)), total(&planned));
    }
}
