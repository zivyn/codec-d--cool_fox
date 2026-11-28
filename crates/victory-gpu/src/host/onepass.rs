//! One walk over the steps of a submission's lists (`VICTORY_ONE_PASS=1`; off by default, with `VICTORY_LEAN_STEPS`).
//!
//! A submission's lists hold some 26,000 steps a frame, 21,800 of them refreshes of some 4,500 copies, and the
//! submitting thread went over them four times, each time looking each copy up in a table by its address: what to ask
//! the write watch about (`ask_spans`, and a read of the copy's own flags at every step), the copies flights may be
//! writing (`stale_under_flights`, again in `settle_stale`'s rounds), what is refreshed in front of the steps
//! (`Batch::refresh_front`), and the steps themselves (`HostList::run_deferred`, which asks `Batch::covered` of every
//! refresh).
//!
//! Here each list is summarized once, when it is closed (by the thread that translated it; by the submitting thread if it
//! was not: `Summary`), and the lists of a submission are looked through once, when the submission knows them: each distinct copy gets a number the first
//! time a step names it (one lookup by address for a copy that is not the one before, one hold on it), and the steps
//! that matter to the four users are written down as records of that number and the range: a refresh, a range given
//! to shaders (`GpuWrite`), a texture. Each user then goes over the records, which are in one flat table the thread
//! keeps for the next submission, and finds its copy by number in a table, not by address in a hash.
//!
//! What stays as it was: the order of the steps (the records are in the order of the lists, and a list put in front
//! of the others, as the planning's own list is when the asking was begun beside the planning, goes first), the moment
//! each question is asked of a copy (`asked_ahead` when the asking is gathered, `writes_pending` when the copies flights
//! may write are looked for: once for each copy, not at each step, but at the same moment), and what each user decides
//! (the tests compare each with the walk it replaces). Nothing a pass holds outlives the submission: it is emptied
//! when it is given back, and the holds on the copies go with it.

use super::scratch::Reset;
use super::*;
use crate::shadow::Shadow;
use std::sync::Arc;

/// Whether the lists are looked through once (`VICTORY_ONE_PASS=1`, with `VICTORY_LEAN_STEPS`): 0.1 ms a frame of
/// a pass's own, in place of four walks of the steps, each with a table of copies by address.
pub(super) fn on() -> bool {
    // (May change while the title runs, `VICTORY_AB`: a submission reads it once, makes its pass and uses it to the end.)
    crate::ab::live_flag!("VICTORY_ONE_PASS")
}

/// Since the last frame-rate line: lists looked through, records made, copies met, microseconds spent making them, and
/// lists whose steps did not agree with their records when they were followed (it should be none), and lists whose summary
/// the submitting thread had to make (the others were made by the thread that closed them).
static STATS: [std::sync::atomic::AtomicU64; 6] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 6];

/// For the frame-rate line.
pub(super) fn line(frames: f64) {
    let s: Vec<f64> = STATS
        .iter()
        .map(|s| s.swap(0, std::sync::atomic::Ordering::Relaxed) as f64)
        .collect();
    if s[0] == 0.0 {
        return;
    }
    tracing::info!(
        "the steps of the lists looked through once, per frame (VICTORY_ONE_PASS): {:.0} lists, {:.0} records of {:.0} copies, {:.2} ms taking them in ({:.0} of the lists summarized by the submitting thread, the others when they were closed); {:.0} lists whose steps did not agree with their records",
        s[0] / frames,
        s[1] / frames,
        s[2] / frames,
        s[3] / 1000.0 / frames,
        s[5] / frames,
        s[4] / frames
    );
}

/// A list's summary was made by the submitting thread.
pub(super) fn made_on_the_lane() {
    STATS[5].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
}

/// A list of steps that can be looked through: a host list (under its lock, as the walks always did) or, in tests,
/// a vector.
pub(super) trait StepList {
    fn steps<R>(&self, with: impl FnOnce(&[Step]) -> R) -> R;

    /// What the steps are to the four users ([`Summary`]): the one the list keeps, made when the list is
    /// closed by the thread that translated it, if it has one that is of the steps it has now.
    fn summary(&self) -> Arc<Summary>;
}

impl StepList for &HostList {
    fn steps<R>(&self, with: impl FnOnce(&[Step]) -> R) -> R {
        with(&self.deferred().copies)
    }

    fn summary(&self) -> Arc<Summary> {
        self.kept_summary(true)
    }
}

#[cfg(test)]
impl StepList for Vec<Step> {
    fn steps<R>(&self, with: impl FnOnce(&[Step]) -> R) -> R {
        with(self)
    }

    fn summary(&self) -> Arc<Summary> {
        Arc::new(Summary::of(self))
    }
}

/// What a list's steps are to the four users, by the number the copy has in the list: made once for a list, by the
/// thread that closes it, and kept with it. A submission's pass takes the records of each of its lists, with the
/// numbers of the copies changed to the submission's own, and never reads the steps of a refresh.
pub(super) struct Summary {
    /// How many steps the list had: the summary is of a list that has no other number.
    pub(super) steps: usize,
    /// The copies the refreshes and ranges given to shaders name, by their number in the list.
    copies: Vec<Arc<Shadow>>,
    textures: Vec<Arc<crate::texture::Staging>>,
    /// One record for each step, in the order of the list.
    recs: Vec<Rec>,
}

impl Summary {
    /// The summary of `steps`.
    pub(super) fn of(steps: &[Step]) -> Summary {
        thread_local! {
            static AT: std::cell::RefCell<ByCopy<u32>> = std::cell::RefCell::new(ByCopy::default());
        }
        let mut at = scratch::Lean::of(&AT, true);
        let mut summary = Summary {
            steps: steps.len(),
            copies: Vec::new(),
            textures: Vec::new(),
            recs: Vec::with_capacity(steps.len()),
        };
        // The steps of a list name one copy several on end (the views of a draw).
        let mut last = (0usize, 0u32);
        for (index, step) in steps.iter().enumerate() {
            let (shadow, rec) = match step {
                Step::Refresh(shadow, from, len) => (
                    Some(shadow),
                    Rec {
                        what: 0,
                        from: *from,
                        to: *from + *len,
                    },
                ),
                Step::GpuWrite(shadow, ..) => (
                    Some(shadow),
                    Rec {
                        what: WRITE,
                        from: 0,
                        to: index,
                    },
                ),
                Step::Texture(staging) => {
                    summary.textures.push(staging.clone());
                    (
                        None,
                        Rec {
                            what: TEXTURE,
                            from: summary.textures.len() - 1,
                            to: index,
                        },
                    )
                }
                _ => (
                    None,
                    Rec {
                        what: EVENT,
                        from: index,
                        to: 0,
                    },
                ),
            };
            let mut rec = rec;
            if let Some(shadow) = shadow {
                let key = Arc::as_ptr(shadow) as usize;
                if last.0 != key {
                    let copies = &mut summary.copies;
                    let number = *at.entry(key).or_insert_with(|| {
                        copies.push(shadow.clone());
                        (copies.len() - 1) as u32
                    });
                    last = (key, number);
                }
                rec.what |= last.1;
            }
            summary.recs.push(rec);
        }
        summary
    }
}

/// The record of a range given to shaders; of a texture; of any other step (an event: a copy, a fill, a run ...).
const WRITE: u32 = 1 << 31;
const TEXTURE: u32 = 1 << 30;
const EVENT: u32 = 1 << 29;
/// The number of the copy.
const NUMBER: u32 = EVENT - 1;
/// What a copy's place in a table says before it has one, and that it is left out.
const UNSEEN: u32 = u32::MAX;
const LEFT_OUT: u32 = u32::MAX - 1;

/// What a step is to the four users. Of a refresh: the copy's number, `from` and `to`. Of every other step the place
/// of the step in its list is in `to` (of an event, [`EVENT`], in `from`, as it has nothing else to say): a `GpuWrite`
/// has the number with [`WRITE`], a texture [`TEXTURE`] and in `from` the place of the texture in the pass's list of them.
#[derive(Clone, Copy)]
pub(super) struct Rec {
    what: u32,
    from: usize,
    to: usize,
}

/// A submission's steps, looked through once. Kept by the thread from one submission to the next
/// ([`PASS`], through `scratch::Lean`): emptied when it is given back.
#[derive(Default)]
pub(super) struct Pass {
    /// The copies, by number: in the order the steps first name them.
    pub(super) copies: Vec<Arc<Shadow>>,
    /// Number of a copy by its address: needed while the lists are looked through.
    table: ByCopy<u32>,
    recs: Vec<Rec>,
    /// Where the records of each list are (from, to), in the order the lists go.
    lists: Vec<(u32, u32)>,
    /// How many steps each of them had when it was looked through (by the same number as `lists`).
    steps: Vec<u32>,
    /// The submission's number of each copy of the list being taken in.
    map: Vec<u32>,
    textures: Vec<Arc<crate::texture::Staging>>,
    // What the users work in (each makes it of the size it needs: the thread keeps the room).
    slot: Vec<u32>,
    found: Vec<(u32, usize, usize)>,
    rank: Vec<u32>,
    order: Vec<u32>,
    given: Vec<u32>,
    flat: Vec<(u32, usize, usize)>,
    begins: Vec<u32>,
    next: Vec<u32>,
    ranges: Vec<(usize, usize)>,
    /// What [`Pass::front_spans`] found: (number of the copy, from, to).
    pub(super) spans: Vec<(u32, usize, usize)>,
}

impl Reset for Pass {
    fn reset(&mut self) {
        // (The copies' and textures' holds are let go here.)
        self.copies.reset();
        self.table.reset();
        self.recs.reset();
        self.lists.reset();
        self.steps.reset();
        self.map.reset();
        self.textures.reset();
        self.slot.reset();
        self.found.reset();
        self.rank.reset();
        self.order.reset();
        self.given.reset();
        self.flat.reset();
        self.begins.reset();
        self.next.reset();
        self.ranges.reset();
        self.spans.reset();
    }
}

thread_local! {
    /// The pass this thread kept from the submission before, emptied.
    pub(super) static PASS: std::cell::RefCell<Pass> = std::cell::RefCell::new(Pass::default());
    /// What a batch kept ahead of the steps by number of copy.
    pub(super) static AHEAD: std::cell::RefCell<Ahead> = std::cell::RefCell::new(Ahead::default());
}

impl Pass {
    /// How many lists are in it.
    pub(super) fn lists(&self) -> usize {
        self.lists.len()
    }

    /// Looks through `lists`, one after the other, and puts them after the ones in it.
    pub(super) fn add<L: StepList>(&mut self, lists: &[L]) {
        for list in lists {
            self.look(list, false);
        }
    }

    /// Looks through `list` and puts it in front of the ones in it (the records are after the others', and
    /// the list is the first to be looked at).
    pub(super) fn add_first<L: StepList>(&mut self, list: &L) {
        self.look(list, true);
    }

    fn look<L: StepList>(&mut self, list: &L, first: bool) {
        let (began, copies, timed) = (
            self.recs.len() as u32,
            self.copies.len(),
            std::time::Instant::now(),
        );
        let summary = list.summary();
        // The copies of the list, each with the number it has in the submission (the next if it has none).
        self.map.clear();
        for copy in &summary.copies {
            let key = Arc::as_ptr(copy) as usize;
            let copies = &mut self.copies;
            let number = *self.table.entry(key).or_insert_with(|| {
                copies.push(copy.clone());
                (copies.len() - 1) as u32
            });
            self.map.push(number);
        }
        let textures = self.textures.len();
        self.textures.extend(summary.textures.iter().cloned());
        let map = &self.map;
        self.recs.extend(summary.recs.iter().map(|rec| {
            let mut rec = *rec;
            if rec.what & EVENT == 0 {
                if rec.what & TEXTURE != 0 {
                    rec.from += textures;
                } else {
                    rec.what = rec.what & WRITE | map[(rec.what & NUMBER) as usize];
                }
            }
            rec
        }));
        let seen = (began, self.recs.len() as u32);
        let counts = [
            1,
            (seen.1 - seen.0) as u64,
            (self.copies.len() - copies) as u64,
            timed.elapsed().as_micros() as u64,
        ];
        for (count, add) in STATS.iter().zip(counts) {
            count.fetch_add(add, std::sync::atomic::Ordering::Relaxed);
        }
        if first {
            self.lists.insert(0, seen);
            self.steps.insert(0, summary.steps as u32);
        } else {
            self.lists.push(seen);
            self.steps.push(summary.steps as u32);
        }
    }

    /// Per copy that `keep` says yes to (asked once, at the copy's first refresh among the lists named), the span of
    /// what those lists refresh of it, from the lowest to the highest, in the order the copies first come: handed to
    /// `out`.
    pub(super) fn spans_of(
        &mut self,
        lists: std::ops::Range<usize>,
        keep: impl Fn(&Shadow) -> bool,
        mut out: impl FnMut(&Arc<Shadow>, usize, usize),
    ) {
        self.slot.clear();
        self.slot.resize(self.copies.len(), UNSEEN);
        self.found.clear();
        for list in lists {
            let (from, to) = self.lists[list];
            for rec in &self.recs[from as usize..to as usize] {
                if rec.what & (WRITE | TEXTURE | EVENT) != 0 {
                    continue;
                }
                let number = rec.what as usize;
                let state = self.slot[number];
                match state {
                    LEFT_OUT => {}
                    UNSEEN => {
                        if keep(&*self.copies[number]) {
                            self.slot[number] = self.found.len() as u32;
                            self.found.push((rec.what, rec.from, rec.to));
                        } else {
                            self.slot[number] = LEFT_OUT;
                        }
                    }
                    at => {
                        let span = &mut self.found[at as usize];
                        span.1 = span.1.min(rec.from);
                        span.2 = span.2.max(rec.to);
                    }
                }
            }
        }
        for &(number, from, to) in &self.found {
            out(&self.copies[number as usize], from, to);
        }
    }

    /// What the write watch is asked about for `lists` (as `ask_spans`: the span of each copy that is asked ahead).
    pub(super) fn asks(&mut self, lists: std::ops::Range<usize>) -> Vec<(usize, usize)> {
        let mut out = Vec::new();
        self.spans_of(
            lists,
            |shadow| shadow.asked_ahead(),
            |_, from, to| out.push((from, to)),
        );
        out
    }

    /// The spans of the copies flights may be writing (as `stale_where`: before the look at what the title wrote).
    pub(super) fn stale(&mut self) -> Vec<(Arc<Shadow>, usize, usize)> {
        let all = self.lists.len();
        let mut out = Vec::new();
        self.spans_of(
            0..all,
            |shadow| shadow.writes_pending(),
            |shadow, from, to| out.push((shadow.clone(), from, to)),
        );
        out
    }

    /// What `front_spans_flat` makes of the steps, by number of copy, in `self.spans`: each copy's refreshes up to
    /// the part that gave it to shaders last, joined where they lie within `gap`, the copies in the order a refresh
    /// or a range given to shaders first names them. A part begins at each of the lists of `parts`. `texture` is
    /// told each texture the lists name, in order.
    pub(super) fn front_spans(
        &mut self,
        parts: &[usize],
        gap: usize,
        mut texture: impl FnMut(&Arc<crate::texture::Staging>),
    ) {
        self.rank.clear();
        self.rank.resize(self.copies.len(), UNSEEN);
        self.order.clear();
        self.given.clear();
        self.flat.clear();
        let mut part = 1u32;
        for (n, &(from, to)) in self.lists.iter().enumerate() {
            if parts.contains(&n) {
                part += 1;
            }
            for rec in &self.recs[from as usize..to as usize] {
                if rec.what & TEXTURE != 0 {
                    texture(&self.textures[rec.from]);
                    continue;
                }
                if rec.what & EVENT != 0 {
                    continue;
                }
                let number = (rec.what & NUMBER) as usize;
                let mut at = self.rank[number];
                if at == UNSEEN {
                    at = self.order.len() as u32;
                    self.rank[number] = at;
                    self.order.push(number as u32);
                    self.given.push(0);
                }
                if rec.what & WRITE != 0 {
                    self.given[at as usize] = part;
                } else if self.given[at as usize] != part {
                    self.flat.push((at, rec.from, rec.to));
                }
            }
        }
        // Each copy's ranges together, in the order they came: where each begins, then put there.
        let copies = self.order.len();
        self.begins.clear();
        self.begins.resize(copies + 1, 0);
        for &(at, ..) in &self.flat {
            self.begins[at as usize + 1] += 1;
        }
        for k in 0..copies {
            self.begins[k + 1] += self.begins[k];
        }
        self.next.clear();
        self.next.extend_from_slice(&self.begins);
        self.ranges.clear();
        self.ranges.resize(self.flat.len(), (0, 0));
        for &(at, from, to) in &self.flat {
            self.ranges[self.next[at as usize] as usize] = (from, to);
            self.next[at as usize] += 1;
        }
        self.spans.clear();
        for k in 0..copies {
            let copy = self.order[k];
            let runs = &mut self.ranges[self.begins[k] as usize..self.begins[k + 1] as usize];
            runs.sort_unstable();
            let mut open: Option<(usize, usize)> = None;
            for &(from, to) in runs.iter() {
                if let Some(last) = open.as_mut()
                    && from <= last.1.saturating_add(gap)
                {
                    last.1 = last.1.max(to);
                    continue;
                }
                if let Some((a, b)) = open.take()
                    && a < b
                {
                    self.spans.push((copy, a, b));
                }
                open = Some((from, to));
            }
            if let Some((a, b)) = open
                && a < b
            {
                self.spans.push((copy, a, b));
            }
        }
    }

    /// The records of list number `list` (in the order the lists go), to be followed one after the other.
    pub(super) fn cursor(&self, list: usize) -> Option<Cursor<'_>> {
        let &(from, to) = self.lists.get(list)?;
        Some(Cursor {
            pass: self,
            at: from as usize,
            end: to as usize,
            steps: self.steps[list] as usize,
            lost: false,
        })
    }
}

/// What comes next in a list, by its records.
pub(super) enum Item<'a> {
    /// A refresh: the copy's number, the copy, the range.
    Refresh(usize, &'a Arc<Shadow>, usize, usize),
    /// Any other step: its place in the list.
    Step(usize),
    End,
}

/// A list's steps in their order, by its records: the refreshes (which are nearly all of them, and have everything the
/// batch needs in the record: the step itself is never read) and the places of the others. The list is taken as it was
/// when it was looked through: if it has another number of steps now (it should not) the records are not used
/// ([`Cursor::fits`]) and the steps are done as without a pass.
pub(super) struct Cursor<'a> {
    pass: &'a Pass,
    at: usize,
    end: usize,
    steps: usize,
    lost: bool,
}

impl Drop for Cursor<'_> {
    fn drop(&mut self) {
        if self.lost {
            STATS[4].fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }
}

impl<'a> Cursor<'a> {
    /// Whether the list still has the steps the records are of (counted if not).
    pub(super) fn fits(&mut self, steps: usize) -> bool {
        self.lost = self.lost || steps != self.steps;
        !self.lost
    }

    /// The next step, in the order of the list.
    pub(super) fn next(&mut self) -> Item<'a> {
        if self.at >= self.end {
            return Item::End;
        }
        let rec = self.pass.recs[self.at];
        self.at += 1;
        if rec.what & EVENT != 0 {
            Item::Step(rec.from)
        } else if rec.what & (WRITE | TEXTURE) != 0 {
            Item::Step(rec.to)
        } else {
            let number = rec.what as usize;
            Item::Refresh(number, &self.pass.copies[number], rec.from, rec.to)
        }
    }
}

/// What a batch has refreshed in front of its steps, by the copy's number in the pass: the first range of each (nearly
/// always its only one) in a plain table, the others of those that have more apart. Forgetting all of it is a number
/// that goes up by one. The same answers as `Batch`'s tables by address (`first`, `rest`; the test of the batch
/// compares them).
#[derive(Default)]
pub(super) struct Ahead {
    epoch: u32,
    /// The epoch a copy's first range was put in; one that is not the epoch is none.
    stamp: Vec<u32>,
    first: Vec<(usize, usize)>,
    more: ByCopy<Vec<(usize, usize)>>,
}

impl Reset for Ahead {
    fn reset(&mut self) {
        self.epoch = 0;
        self.stamp.reset();
        self.first.reset();
        self.more.reset();
    }
}

impl Ahead {
    /// Nothing is ahead, for `copies` numbers.
    pub(super) fn begin(&mut self, copies: usize) {
        self.stamp.clear();
        self.stamp.resize(copies, 0);
        self.first.clear();
        self.first.resize(copies, (0, 0));
        self.more.clear();
        self.epoch = 1;
    }

    /// Whether `[address, end)` of copy `number` is in one range that is ahead (`sorted`: the ranges of a copy that has
    /// more than one are in order and apart).
    pub(super) fn covered(&self, number: usize, address: usize, end: usize, sorted: bool) -> bool {
        if self.stamp.get(number) != Some(&self.epoch) {
            return false;
        }
        let (from, to) = self.first[number];
        if from <= address && end <= to {
            return true;
        }
        !self.more.is_empty()
            && self
                .more
                .get(&number)
                .is_some_and(|ranges| ranges_cover(ranges, address, end, sorted))
    }

    /// Puts `[from, to)` among the ranges ahead for copy `number`; `unsorted` is set when it does not begin past the
    /// end of the one before it.
    pub(super) fn put(&mut self, number: usize, from: usize, to: usize, unsorted: &mut bool) {
        if self.stamp[number] != self.epoch {
            self.stamp[number] = self.epoch;
            self.first[number] = (from, to);
            return;
        }
        let ranges = self.more.entry(number).or_default();
        let last = ranges.last().copied().unwrap_or(self.first[number]);
        if from < last.1 {
            *unsorted = true;
        }
        ranges.push((from, to));
    }

    /// What is ahead for copy `number` is no more.
    #[cfg(test)]
    pub(super) fn forget_one(&mut self, number: usize) {
        if let Some(stamp) = self.stamp.get_mut(number) {
            *stamp = 0;
        }
        if !self.more.is_empty() {
            self.more.remove(&number);
        }
    }

    /// What is ahead is no more, for any copy.
    pub(super) fn forget_all(&mut self) {
        self.epoch += 1;
        if !self.more.is_empty() {
            self.more.clear();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A small generator of numbers (xorshift).
    struct Dice(u64);

    impl Dice {
        fn below(&mut self, n: usize) -> usize {
            self.0 ^= self.0 << 13;
            self.0 ^= self.0 >> 7;
            self.0 ^= self.0 << 17;
            (self.0 >> 11) as usize % n
        }
    }

    /// Lists of random steps over some copies: refreshes (several on end for a copy, ranges that lie apart and that
    /// overlap), ranges given to shaders, and steps that are neither.
    fn random_lists(dice: &mut Dice, copies: &[Arc<Shadow>]) -> Vec<Vec<Step>> {
        (0..1 + dice.below(7))
            .map(|_| {
                let mut steps = Vec::new();
                let mut at = dice.below(copies.len());
                for _ in 0..dice.below(60) {
                    if dice.below(3) == 0 {
                        at = dice.below(copies.len());
                    }
                    let base = 0x10_0000 * (at + 1);
                    let (from, len) = (base + dice.below(3000), 1 + dice.below(500));
                    steps.push(match dice.below(14) {
                        0 | 1 => Step::GpuWrite(copies[at].clone(), from, len),
                        2 => Step::Copy(base, base + 8, 4),
                        3 => Step::Fill(base, base + 8, 4, None),
                        4 => Step::Run(Box::new(|| {})),
                        _ => Step::Refresh(copies[at].clone(), from, len),
                    });
                }
                steps
            })
            .collect()
    }

    fn some_copies(memory: &[u8], n: usize) -> Vec<Arc<Shadow>> {
        (0..n)
            .map(|i| {
                Shadow::unbacked(
                    0x10_0000 * (i as u64 + 1),
                    memory.as_ptr().cast_mut(),
                    4096,
                    false,
                )
            })
            .collect()
    }

    fn key(shadow: &Arc<Shadow>) -> usize {
        Arc::as_ptr(shadow) as usize
    }

    /// What the walks that the pass replaces make of the same lists, whether the pass was made in the order of the lists
    /// or with the first put in front of the others (as the planning's own list is): the asks (per copy, of all of the
    /// lists and of all but the first), the copies flights may write, with the ranges, in order.
    #[test]
    fn the_pass_asks_and_finds_the_stale_what_the_walks_do() {
        let mut dice = Dice(0x9E37_79B9_7F4A_7C15);
        let memory = vec![0u8; 4096];
        let copies = some_copies(&memory, 11);
        let (mut asked, mut stale_found) = (0, 0);
        for trial in 0..600 {
            let lists = random_lists(&mut dice, &copies);
            // (The answers a copy gives: by its number, as `asked_ahead` and `writes_pending` give by what they hold.)
            let (asks, pending) = (|s: &Shadow| s.id() % 3 != 0, |s: &Shadow| s.id() % 2 == 0);
            for in_front in [false, true] {
                let mut pass = Pass::default();
                if in_front && lists.len() > 1 {
                    pass.add(&lists[1..]);
                    pass.add_first(&lists[0]);
                } else {
                    pass.add(&lists);
                }
                for lean in [false, true] {
                    for from in [0, lists.len() / 2] {
                        let mut want = ask_spans_where(&lists[from..], lean, asks);
                        want.sort_unstable();
                        let mut got = Vec::new();
                        pass.spans_of(from..lists.len(), asks, |_, a, b| got.push((a, b)));
                        got.sort_unstable();
                        assert_eq!(
                            got, want,
                            "trial {trial} lean {lean} from {from} in front {in_front}"
                        );
                        asked += got.len();
                    }
                    let want: Vec<_> = stale_where(&lists, lean, pending)
                        .iter()
                        .map(|(s, a, b)| (key(s), *a, *b))
                        .collect();
                    let mut got = Vec::new();
                    pass.spans_of(0..lists.len(), pending, |s, a, b| got.push((key(s), a, b)));
                    assert_eq!(
                        got, want,
                        "trial {trial} lean {lean} in front {in_front}: the copies, ranges and order"
                    );
                    stale_found += got.len();
                }
            }
        }
        assert!(
            asked > 500 && stale_found > 500,
            "{asked} asked, {stale_found} stale"
        );
    }

    /// What is refreshed in front of the steps, by number of copy, is what `front_spans_flat` and `front_spans_gap` make
    /// of the same steps by address (the touches as `Batch::refresh_front` makes them), for gaps of every kind and parts
    /// that begin at any list.
    #[test]
    fn the_spans_in_front_are_those_of_the_steps_by_address() {
        let mut dice = Dice(0x2545_f491_4f6c_dd1d);
        let memory = vec![0u8; 4096];
        let copies = some_copies(&memory, 9);
        let mut found = 0;
        for trial in 0..800 {
            let lists = random_lists(&mut dice, &copies);
            let parts: Vec<usize> = (0..lists.len()).filter(|_| dice.below(3) == 0).collect();
            let gap = [usize::MAX, 0, 64, 700, 64 << 10][dice.below(5)];
            let mut touches = Vec::new();
            for (n, list) in lists.iter().enumerate() {
                if parts.contains(&n) {
                    touches.push(Touch::Part);
                }
                for step in list {
                    match step {
                        Step::Refresh(shadow, at, len) => {
                            touches.push(Touch::Refresh(key(shadow), *at, *at + *len))
                        }
                        Step::GpuWrite(shadow, ..) => touches.push(Touch::Write(key(shadow))),
                        _ => {}
                    }
                }
            }
            let by_address = front_spans_flat(touches.iter().map(clone_touch), gap);
            assert_eq!(
                by_address,
                front_spans_gap(touches.iter().map(clone_touch), gap),
                "trial {trial}"
            );
            let mut pass = Pass::default();
            pass.add(&lists);
            pass.front_spans(&parts, gap, |_| {});
            let by_number: Vec<_> = pass
                .spans
                .iter()
                .map(|&(n, a, b)| (key(&pass.copies[n as usize]), a, b))
                .collect();
            assert_eq!(
                by_number, by_address,
                "trial {trial} gap {gap} parts {parts:?}"
            );
            found += by_number.len();
        }
        assert!(found > 1000, "{found} spans");
    }

    fn clone_touch(t: &Touch) -> Touch {
        match t {
            Touch::Refresh(c, a, b) => Touch::Refresh(*c, *a, *b),
            Touch::Write(c) => Touch::Write(*c),
            Touch::Title => Touch::Title,
            Touch::Nothing => Touch::Nothing,
            Touch::Part => Touch::Part,
        }
    }

    /// The cursor of each list gives the steps in their order: a refresh with its copy and range as the step has them,
    /// every other step as its place; and it does not fit a list that has another number of steps.
    #[test]
    fn the_cursor_gives_the_steps_of_each_list_in_order() {
        let mut dice = Dice(0x0123_4567_89ab_cdef);
        let memory = vec![0u8; 4096];
        let copies = some_copies(&memory, 9);
        let (mut refreshes, mut others) = (0, 0);
        for trial in 0..300 {
            let lists = random_lists(&mut dice, &copies);
            let mut pass = Pass::default();
            pass.add(&lists);
            for (n, list) in lists.iter().enumerate() {
                let mut cursor = pass.cursor(n).unwrap();
                assert!(cursor.fits(list.len()));
                for (i, step) in list.iter().enumerate() {
                    match (step, cursor.next()) {
                        (Step::Refresh(shadow, at, len), Item::Refresh(number, copy, from, to)) => {
                            assert!(
                                Arc::ptr_eq(copy, shadow)
                                    && Arc::ptr_eq(&pass.copies[number], shadow)
                            );
                            assert_eq!((from, to), (*at, *at + *len), "trial {trial}");
                            refreshes += 1;
                        }
                        (Step::Refresh(..), _) | (_, Item::Refresh(..)) => {
                            panic!("trial {trial} list {n} step {i}: a refresh is not")
                        }
                        (_, Item::Step(at)) => {
                            assert_eq!(at, i, "trial {trial} list {n}");
                            others += 1;
                        }
                        (_, Item::End) => {
                            panic!("trial {trial} list {n}: the records end at step {i}")
                        }
                    }
                }
                assert!(matches!(cursor.next(), Item::End));
                assert!(!pass.cursor(n).unwrap().fits(list.len() + 1));
            }
        }
        assert!(
            refreshes > 1000 && others > 500,
            "{refreshes} refreshes, {others} others"
        );
    }

    /// A pass made twice from the same thread's kept one finds nothing of the first, and lets go of its holds.
    #[test]
    fn a_pass_given_back_holds_nothing() {
        let mut dice = Dice(0x7f4a_7c15_9e37_79b9);
        let memory = vec![0u8; 4096];
        let copies = some_copies(&memory, 5);
        // (A copy may be held by the table of copies too: what is counted is what the pass adds.)
        let held: Vec<usize> = copies.iter().map(Arc::strong_count).collect();
        {
            let mut pass = scratch::Lean::of(&PASS, true);
            let mut lists = random_lists(&mut dice, &copies);
            lists.push(vec![Step::Refresh(copies[0].clone(), 1, 2)]);
            pass.add(&lists);
            drop(lists);
            assert!(
                Arc::strong_count(&copies[0]) > held[0],
                "the pass holds the copy"
            );
        }
        assert!(
            copies
                .iter()
                .map(Arc::strong_count)
                .eq(held.iter().copied()),
            "the copies are let go of"
        );
        let pass = scratch::Lean::of(&PASS, true);
        assert_eq!(pass.lists(), 0);
        assert!(pass.copies.is_empty());
    }

    /// What is ahead by number of copy answers as what is ahead by address does, for ranges put in order and apart
    /// or not, one or several for a copy, a copy forgotten and everything forgotten.
    #[test]
    fn what_is_ahead_by_number_answers_as_by_address() {
        let mut dice = Dice(0xdead_beef_1234_5678);
        let (mut yes, mut no) = (0, 0);
        for trial in 0..500 {
            let mut plain = Batch::new(true);
            let mut ahead = Ahead::default();
            ahead.begin(9);
            let mut unsorted = false;
            for step in 0..200 {
                let number = if dice.below(3) == 0 {
                    dice.below(9)
                } else {
                    (step / 7) % 9
                };
                let key = 0x1000 * (number + 1);
                let base = 0x10_0000 * (number + 1);
                match dice.below(12) {
                    0 | 1 => {
                        let from = base + dice.below(3000);
                        let to = if trial % 3 == 0 {
                            from + dice.below(400)
                        } else {
                            base + 3000 + dice.below(1000)
                        };
                        plain.put_ahead(key, from, to);
                        ahead.put(number, from, to, &mut unsorted);
                    }
                    2 => {
                        if dice.below(2) == 0 {
                            plain.forget_ahead(Some(key));
                            ahead.forget_one(number);
                        } else {
                            plain.forget_ahead(None);
                            ahead.forget_all();
                        }
                    }
                    _ => {
                        let at = base + dice.below(4000);
                        let end = at + dice.below(300);
                        let a = plain.covered(key, at, end);
                        assert_eq!(
                            a,
                            ahead.covered(number, at, end, !unsorted),
                            "trial {trial} step {step}: {at:#x}..{end:#x}"
                        );
                        assert_eq!(plain.unsorted, unsorted);
                        if a {
                            yes += 1;
                        } else {
                            no += 1;
                        }
                    }
                }
            }
        }
        assert!(yes > 500 && no > 500, "{yes} covered, {no} not");
    }
}
