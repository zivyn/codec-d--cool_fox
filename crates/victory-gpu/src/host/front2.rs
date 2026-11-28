//! The entry after the next, asked and refreshed beside the steps of the next, by a second thread of stage A
//! (`VICTORY_STAGE_A_FRONT2=1`, with `VICTORY_STAGE_A_STEPS`; off by default), and what it leaves for the thread that
//! prepares that entry when its turn comes: a *certificate* of the spans whose asking and refreshing need not be done again.
//!
//! The pipeline has three stages in flight, each with its own thread:
//!
//! - **K**, the submission being sent (the thread of submissions: heaps' wait, send, flight, landing);
//! - **E1**, the next submission, whose stage A the thread `victory-front` does (ask, stale bytes, refreshes, steps);
//! - **E2**, the entry after E1, whose asking and refreshes of the spans E1 leaves alone this thread (`victory-front2`) does
//!   now, beside E1's steps.
//!
//! [`crate::host::relax`]'s `VICTORY_STAGE_A_DEEP` already did a part of that on the thread of the refreshes: the
//! refreshes of the copies E1 does nothing to. What it could not do is leave anything behind: the thread that prepares E2
//! asked again, and looked at every span again, whatever was done, so that the work was done beside E1 and again after
//! it, and the second did not shorten the first (the thread of stage A is the critical path of the frame: about 40 ms of
//! a frame of 40, the lane about 13). Here the work done beside E1 is *taken as done* when E2's turn comes, for the
//! spans where that is proven, and E2's thread asks and refreshes only the rest.
//!
//! ## Who owns what (a copy, and a range of it, is in one stage's hands at a time)
//!
//! 1. **E1's thread** owns, until the lane has taken E1 (`Host::submit` joins this thread, the thread of stage A and the
//!    thread of the refreshes before it takes anything), the copies E1 declares written by shaders or places on (whole), the
//!    ranges of copies E1 refreshes (widened to the grain the pages are taken in, [`GRAIN`]) and the title memory its steps
//!    write (widened the same way): [`Held`]. A refresh of E2 never touches them: a copy refreshed for E2 before E1's take
//!    would hand the card, for E1, the bytes E2 reads (ARCH1), and a copy E1's shaders write is left to the landing.
//! 2. **This thread** owns the spans of E2 that [`Held::leaves`] says E1 leaves alone, and that no flight in the air has a
//!    declared write in (`line.written`, which holds the flights in the line and the submissions registered and not yet
//!    flights, K among them; and the copy's own declared ranges, `Shadow::writes_pending_in`). Those it asks about and
//!    refreshes, as the early offer does (a standing asking beside the thread's own: a refresh that meets another asking's
//!    pages copies more, never less).
//! 3. **The lane** owns every copy from the join to the take, and until E2's turn; **E2's thread** owns what is left of E2
//!    from its offer to its take, and takes the certificate with it ([`Cert::usable`]).
//!
//! ## Why the work can be taken as done
//!
//! E2 was in the line when this thread asked: the title's call for it, and so every byte it wrote for E2, came before
//! the asking (the argument of `prefetch` point 1 and of the lane's own ask). What it writes afterwards is for later
//! submissions, which find it with their own askings (their numbers are higher; nothing is pruned from the journal, and a
//! page the kernel's watch holds stays written until someone asks). So a span of E2 refreshed here holds, at E2's take, all
//! that E2 needs, **unless something wrote into it, or declared it written, after this thread's refresh and before E2's
//! take**. Everything that can is accounted for, and each is checked again when E2's turn comes ([`Cert::usable`]):
//!
//! - E1's steps write title memory only in the ranges [`Held`] holds (`Copy`, `Placed`; `Run` would write anywhere and
//!   vetoes the offer) and refresh only the ranges it holds: no span that touches one was offered;
//! - what flights declare written: a declared range is left alone by every refresh and waits for the landing and the
//!   thread's stale-bytes step; a span touching one at the offer was not given, and one that touches one when E2's turn
//!   comes (E1's own declarations are made by the lane after the join, before its take: `picked::declare` may name a
//!   copy no step of E1 names) is not taken as done, so E2's thread refreshes it and the stale-bytes step does what it
//!   did;
//! - any other submission that takes a number between E1 and E2 (a submission that is waited for, which the hold of
//!   `front::lock_quiet` keeps back for 200 ms at most) could write title memory, refresh a copy: the certificate says
//!   the number it was made after ([`Cert::usable`] looks at the counter, and [`crate::host::front`] looks again when it
//!   takes E2's number: if it moved, the spans are refreshed after all, and nothing was lost: the refreshes that were left
//!   out of the asking are asked by the refresh that reads them);
//! - E2 is not the entry that was named, or has more lists than it had when named (a following entry folded into it):
//!   the certificate is for the lists it names, in order, and what the later lists refresh, and every copy they or the
//!   plan's own list name, is left to E2's thread ([`Held::names`]);
//! - the spans themselves: E2's thread takes as done only a span it would itself have refreshed ahead of its steps and
//!   that lies whole inside a certified one ([`Certified::covers`]); the other refreshes it makes, with the asking of
//!   their own, and the stale-bytes step, the steps and the declarations are untouched.
//!
//! What the take of E1 does with a span refreshed here: it goes up to the card with E1's other bytes, as the early offer's
//! do (queue order keeps it behind the flights that read the old bytes; E1's lists do not read it, since no refresh of E1
//! names its range).
//!
//! ## Tests
//!
//! `held_*` / `certified_*` here (ownership by copy and by range, widening, the conditions a certificate stands on), and
//! `prefetch::copy_tests::three_stages_with_forced_conflicts_leave_no_stale_byte_for_the_card` (500 rounds: K's flight in
//! the air, E1's steps beside this thread's work on E2, E2's turn with the certificate, forced conflicts: a title copy of E1
//! into E2's span, a declaration of a certified copy by the lane after the join, a number taken in between, a folded entry
//! with a later list that refreshes a copy of the certificate, a range E1 refreshes next to E2's).

use super::*;

/// The grain a copy's pages are taken, noted and refreshed in: ownership is by whole grains.
const GRAIN: usize = crate::watch::PAGE;

/// Whether a second thread asks and refreshes the entry after the next (`VICTORY_STAGE_A_FRONT2=1`).
///
/// (May change while the title runs, `VICTORY_AB`: `front::offer` and `Host::front` read it once for a submission;
/// [`join`] and [`retire`] look at what there is whatever it says.)
pub(super) fn on() -> bool {
    crate::ab::live_flag!("VICTORY_STAGE_A_FRONT2") && usable()
}

/// What the offer needs besides the switch (as the early offer): the refreshes ahead, the one asking, the stage.
fn usable() -> bool {
    prefetch::on() && crate::queue::stage_a() && crate::watch::once()
}

fn key(shadow: &std::sync::Arc<crate::shadow::Shadow>) -> usize {
    std::sync::Arc::as_ptr(shadow) as usize
}

/// `[from, to)` widened to whole grains.
fn widen(from: usize, to: usize) -> (usize, usize) {
    (from - from % GRAIN, to.div_ceil(GRAIN) * GRAIN)
}

/// What a submission's steps do to host copies and title memory, by copy and by range: what the thread that does the entry
/// after it must leave alone.
#[derive(Default, Debug, Clone, PartialEq)]
pub(super) struct Held {
    /// Copies (by address) a step declares written by shaders or places on, sorted: whole.
    whole: Vec<usize>,
    /// (copy, from, to): the ranges the refreshes name, widened to the grain, joined, sorted.
    refreshed: Vec<(usize, usize, usize)>,
    /// The title memory the steps write (copies, places), widened to the grain, joined, sorted.
    written: Vec<(usize, usize)>,
    /// A step of ours that may write title memory anywhere (`Step::Run`).
    unknown: bool,
}

impl Held {
    /// What `lists` do (the submission's, and the planning's own list).
    pub(super) fn of(lists: &[&HostList]) -> Held {
        let mut held = Held::default();
        for list in lists {
            held.note(list.deferred().copies.iter());
        }
        held.finish()
    }

    /// What `steps` do, before [`Held::finish`].
    pub(super) fn note<'a>(&mut self, steps: impl Iterator<Item = &'a Step>) {
        for step in steps {
            match step {
                Step::Refresh(shadow, at, len) => {
                    if *len > 0 {
                        self.refreshed.push((key(shadow), *at, *at + *len));
                    }
                }
                Step::GpuWrite(shadow, ..) => self.whole.push(key(shadow)),
                Step::Placed(p) => {
                    self.whole.push(key(&p.shadow));
                    self.written.push((p.dst, p.dst + p.len));
                    self.written.push((p.src, p.src + p.len));
                }
                &Step::Copy(dst, _, len) => self.written.push((dst, dst + len)),
                Step::Run(_) => self.unknown = true,
                // (A fill reads title memory and writes the list's own upload memory; a host step, a texture and a plane
                // write memory that is no copy's.)
                Step::Fill(..) | Step::Host(..) | Step::Texture(_) | Step::Plane(..) => {}
            }
        }
    }

    /// Sorted, widened and joined (what [`Held::leaves`] needs).
    pub(super) fn finish(mut self) -> Held {
        self.whole.sort_unstable();
        self.whole.dedup();
        let mut refreshed = std::mem::take(&mut self.refreshed);
        for r in refreshed.iter_mut() {
            (r.1, r.2) = widen(r.1, r.2);
        }
        refreshed.sort_unstable();
        let mut joined: Vec<(usize, usize, usize)> = Vec::with_capacity(refreshed.len());
        for (copy, from, to) in refreshed {
            match joined.last_mut() {
                Some(last) if last.0 == copy && from <= last.2 => last.2 = last.2.max(to),
                _ => joined.push((copy, from, to)),
            }
        }
        self.refreshed = joined;
        let written: Vec<(usize, usize)> = std::mem::take(&mut self.written).into_iter().filter(|r| r.0 < r.1).map(|(a, b)| widen(a, b)).collect();
        join_ranges(&mut self.written, written);
        self
    }

    /// Both together.
    pub(super) fn merged(mut self, other: Held) -> Held {
        self.whole.extend(other.whole);
        self.refreshed.extend(other.refreshed);
        self.written.extend(other.written);
        self.unknown |= other.unknown;
        self.finish()
    }

    /// Whether the thread may begin `[from, to)` of the copy at `copy` now: no step declares the copy written or places on
    /// it, none refreshes a grain of the range, none writes title memory in a grain of it.
    pub(super) fn leaves(&self, copy: usize, from: usize, to: usize) -> bool {
        if self.unknown || self.whole.binary_search(&copy).is_ok() {
            return false;
        }
        let (a, b) = widen(from, to);
        let i = self.refreshed.partition_point(|r| (r.0, r.2) <= (copy, a));
        if self.refreshed.get(i).is_some_and(|r| r.0 == copy && r.1 < b) {
            return false;
        }
        !touches(&self.written, a, b)
    }

    /// Whether a step names the copy at all (a refresh, a declaration, a place).
    pub(super) fn names(&self, copy: usize) -> bool {
        self.whole.binary_search(&copy).is_ok() || self.refreshed.get(self.refreshed.partition_point(|r| r.0 < copy)).is_some_and(|r| r.0 == copy)
    }
}

/// Whether anything in the air may write `[from, to)` of the copy: a flight (or a submission registered and not yet one)
/// whose declared ranges are `air`, or a declaration of the copy's own.
fn in_the_air(shadow: &crate::shadow::Shadow, air: &[(usize, usize)], from: usize, to: usize) -> bool {
    air.iter().any(|r| r.0 < to && r.1 > from) || shadow.writes_pending_in(from, to)
}

/// `spans` as what the thread may do now (`held` leaves them alone and nothing in the air writes them), the spans left for
/// the entry's own thread because of what the submission before does, and those because of what is in the air.
pub(super) fn free(spans: prefetch::Spans, held: &Held, air: &[(usize, usize)]) -> (prefetch::Spans, usize, usize) {
    let mut free = Vec::with_capacity(spans.len());
    let (mut owned, mut flying) = (0, 0);
    for (shadow, from, to) in spans {
        if !held.leaves(key(&shadow), from, to) {
            owned += 1;
        } else if in_the_air(&shadow, air, from, to) {
            flying += 1;
        } else {
            free.push((shadow, from, to));
        }
    }
    (free, owned, flying)
}

/// What the thread did for the entry after the next: the entry (its number, the lists it had), the number the submission
/// before it took (nothing was numbered between), and the spans it asked about and refreshed.
pub(super) struct Cert {
    entry: u64,
    lists: Vec<usize>,
    after: u64,
    spans: prefetch::Spans,
}

impl Cert {
    pub(super) fn after(&self) -> u64 {
        self.after
    }

    /// How many lists the certificate was made for.
    pub(super) fn lists_len(&self) -> usize {
        self.lists.len()
    }

    /// For the tests.
    #[cfg(test)]
    pub(super) fn new(entry: u64, lists: Vec<usize>, after: u64, spans: prefetch::Spans) -> Cert {
        Cert { entry, lists, after, spans }
    }

    /// The spans that stand for the submission whose turn it is now: `entry` and `lists` are what the line names, `started`
    /// the number the last submission took, `air` what flights declare written, `extra` what the lists in front of the
    /// submission's (the plan's own) and after the ones the certificate was made for do. Each span that does not stand is
    /// counted by why.
    pub(super) fn usable(&self, entry: u64, lists: &[usize], started: u64, air: &[(usize, usize)], extra: &Held) -> Certified {
        let mut out = Certified::default();
        if entry != self.entry || lists.len() < self.lists.len() || lists[..self.lists.len()] != self.lists[..] {
            STATS[OTHER_ENTRY].fetch_add(1, Ordering::Relaxed);
            return out;
        }
        if started != self.after {
            STATS[NUMBERED].fetch_add(1, Ordering::Relaxed);
            return out;
        }
        if extra.unknown {
            return out;
        }
        for (shadow, from, to) in &self.spans {
            let copy = key(shadow);
            if extra.names(copy) {
                STATS[NAMED].fetch_add(1, Ordering::Relaxed);
            } else if in_the_air(shadow, air, *from, *to) {
                STATS[DECLARED].fetch_add(1, Ordering::Relaxed);
            } else {
                out.by_copy.entry(copy).or_default().push((*from, *to));
                out.spans += 1;
            }
        }
        for ranges in out.by_copy.values_mut() {
            ranges.sort_unstable();
        }
        out
    }
}

/// The spans of a certificate that stand now.
#[derive(Default)]
pub(super) struct Certified {
    by_copy: ByCopy<Vec<(usize, usize)>>,
    spans: usize,
}

impl Certified {
    pub(super) fn is_empty(&self) -> bool {
        self.spans == 0
    }

    /// For the tests.
    #[cfg(test)]
    pub(super) fn len(&self) -> usize {
        self.spans
    }

    /// Whether `[from, to)` of the copy at `copy` lies whole inside a span that was asked about and refreshed.
    pub(super) fn covers(&self, copy: usize, from: usize, to: usize) -> bool {
        self.by_copy.get(&copy).is_some_and(|ranges| ranges.iter().any(|r| r.0 <= from && to <= r.1))
    }
}

/// Spans of a certificate taken as done by the thread of the entry: how many and how many bytes of range.
pub(super) fn taken_as_done(spans: usize, bytes: usize) {
    STATS[TAKEN].fetch_add(spans as u64, Ordering::Relaxed);
    STATS[TAKEN_BYTES].fetch_add(bytes as u64, Ordering::Relaxed);
}

/// A certificate was made against a submission counter that moved by the time the thread of the entry took its number:
/// the refreshes were done after all.
pub(super) fn number_moved() {
    STATS[MOVED].fetch_add(1, Ordering::Relaxed);
}

/// What the thread is given: the lists of the submission before (addresses), what its planning's own list does, the
/// entry after it (its number and lists) and the number the submission before took.
struct Offered {
    runs: Vec<usize>,
    plan: Held,
    entry: u64,
    next: Vec<usize>,
    after: u64,
}

static GATE: prefetch::Gate<Offered> = prefetch::Gate::new();

/// What the thread did, for [`retire`].
static DONE: Mutex<Option<Cert>> = Mutex::new(None);

/// Whether anything was ever offered: from then on [`join`] looks at the gate whatever the switch says.
static OFFERED_EVER: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// Since the last frame-rate line: offers made, refused (the thread was not free), not made because a step of ours may write
/// anywhere, with nothing to refresh; spans seen, given, kept for what the submission before does, kept for what is in the
/// air; microseconds the thread worked and the lane waited for it; then what the thread of the entry made of the
/// certificates: spans taken as done and bytes of range, certificates of another entry, made after another number, spans that
/// were named by lists the certificate was not made for, spans declared written since, and numbers that moved at the take.
static STATS: [std::sync::atomic::AtomicU64; 18] = [const { std::sync::atomic::AtomicU64::new(0) }; 18];
const OFFERED: usize = 0;
const REFUSED: usize = 1;
const VETO: usize = 2;
const NOTHING: usize = 3;
const SEEN: usize = 4;
const GIVEN: usize = 5;
const OWNED: usize = 6;
const FLYING: usize = 7;
const WORKED: usize = 8;
const WAITED: usize = 9;
const TAKEN: usize = 10;
const TAKEN_BYTES: usize = 11;
const OTHER_ENTRY: usize = 12;
const NUMBERED: usize = 13;
const NAMED: usize = 14;
const DECLARED: usize = 15;
const MOVED: usize = 16;
const CERTS: usize = 17;

/// After the submission before the next has its number and its steps begin: asks the thread to do the asking and the
/// refreshes of the entry after the next (`entry`: its number and host lists, as the addresses the line gave) beside them.
/// `runs` are the lists of the submission being prepared (addresses), `plan` what its planning's own list does, `after` the
/// number it took. Whether it was offered.
pub(super) fn offer(runs: &[usize], plan: Held, entry: (u64, &[usize]), after: u64) -> bool {
    if !on() {
        return false;
    }
    // (What an earlier entry left is not for this one.)
    *DONE.lock().unwrap_or_else(|e| e.into_inner()) = None;
    start();
    OFFERED_EVER.store(true, Ordering::Release);
    match GATE.offer(Offered { runs: runs.to_vec(), plan, entry: entry.0, next: entry.1.to_vec(), after }) {
        Ok(()) => {
            STATS[OFFERED].fetch_add(1, Ordering::Relaxed);
            true
        }
        Err(_) => {
            STATS[REFUSED].fetch_add(1, Ordering::Relaxed);
            false
        }
    }
}

/// Before anything of the submission that was prepared beside this thread is taken (the lane, at the top of its submission,
/// after the thread of stage A is joined): the thread is done, and every copy is the lane's again.
pub(super) fn join() {
    if !OFFERED_EVER.load(Ordering::Acquire) {
        return;
    }
    let began = std::time::Instant::now();
    if GATE.join() {
        STATS[WAITED].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
    }
}

/// What the thread left, once, for the offer of the entry it was made for.
pub(super) fn retire() -> Option<Cert> {
    if !OFFERED_EVER.load(Ordering::Acquire) {
        return None;
    }
    DONE.lock().unwrap_or_else(|e| e.into_inner()).take()
}

fn start() {
    static STARTED: std::sync::Once = std::sync::Once::new();
    STARTED.call_once(|| {
        let made = std::thread::Builder::new().name("victory-front2".into()).spawn(|| {
            crate::cores::keep_on_fast_cores("victory-front2");
            crate::cores::prioritize("victory-front2", crate::cores::Role::Helper);
            loop {
                let offered = GATE.take();
                crate::sched::front_sync();
                let began = std::time::Instant::now();
                let _span = crate::timeline::span("stage A of the entry after the next: asked and refreshed");
                // (A refresh that panicked leaves no certificate: the thread of the entry does it.)
                let made = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| work(offered))).ok().flatten();
                if made.is_some() {
                    STATS[CERTS].fetch_add(1, Ordering::Relaxed);
                }
                *DONE.lock().unwrap_or_else(|e| e.into_inner()) = made;
                STATS[WORKED].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
                GATE.done();
            }
        });
        if let Err(e) = made {
            tracing::warn!(error = %e, "no thread for the entry after the next: its asking and refreshes are made by its own preparing");
        }
    });
}

/// The work: what the submission before does ([`Held`]), the spans of the entry after it that it leaves alone and nothing in the
/// air writes, asked and refreshed as the early offer's are.
fn work(offered: Offered) -> Option<Cert> {
    // SAFETY: the lists of the submission before are the first of the line until the lane has joined this thread
    // (`join`, before it takes anything of it); those of the entry after it stay in the line at their place (named by
    // `deeper_next_entry`), taken out only by the lane's own step, which is after the take of the submission before.
    // (The entry's own lists first: nothing else looks at them. The lists of the submission before are looked at by its own
    // thread as well, one at a time under the list's lock.)
    let spans = unsafe { prefetch::spans_of(&offered.next, &[]) };
    if spans.is_empty() {
        STATS[NOTHING].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    // SAFETY: as above.
    let runs: Vec<&HostList> = offered.runs.iter().map(|&at| unsafe { &*(at as *const HostList) }).collect();
    let held = Held::of(&runs).merged(offered.plan);
    if held.unknown {
        STATS[VETO].fetch_add(1, Ordering::Relaxed);
        return None;
    }
    let air = line().written.clone();
    let seen = spans.len();
    let (free, owned, flying) = free(spans, &held, &air);
    STATS[SEEN].fetch_add(seen as u64, Ordering::Relaxed);
    STATS[GIVEN].fetch_add(free.len() as u64, Ordering::Relaxed);
    STATS[OWNED].fetch_add(owned as u64, Ordering::Relaxed);
    STATS[FLYING].fetch_add(flying as u64, Ordering::Relaxed);
    if free.is_empty() {
        return None;
    }
    prefetch::run(free.clone());
    Some(Cert { entry: offered.entry, lists: offered.next, after: offered.after, spans: free })
}

/// For the frame-rate line.
pub(super) fn frame_line(frames: f64) {
    let s: Vec<f64> = STATS.iter().map(|s| s.swap(0, Ordering::Relaxed) as f64).collect();
    if !crate::ab::live_flag!("VICTORY_STAGE_A_FRONT2") && s.iter().all(|&n| n == 0.0) {
        return;
    }
    tracing::info!(
        "stage A, the entry after the next asked and refreshed beside the next's steps by a second thread (VICTORY_STAGE_A_FRONT2), per frame: {:.2} offered ({:.2} refused, {:.2} not made because a step may write anywhere, {:.2} with nothing to refresh); of {:.1} spans {:.1} given, {:.1} kept (the submission before refreshes, declares or writes there) and {:.2} kept (in the air); {:.2} certificates; the thread worked {:.1} ms, the lane waited {:.1} ms for it at the join; the thread of the entry took as done {:.1} spans ({:.1} MB of range), and not {:.2} (another entry or folded lists), {:.2} (a number between), {:.1} (copies the later lists name), {:.2} (declared written since), {:.2} numbers that moved at the take",
        s[OFFERED] / frames,
        s[REFUSED] / frames,
        s[VETO] / frames,
        s[NOTHING] / frames,
        s[SEEN] / frames,
        s[GIVEN] / frames,
        s[OWNED] / frames,
        s[FLYING] / frames,
        s[CERTS] / frames,
        s[WORKED] / 1000.0 / frames,
        s[WAITED] / 1000.0 / frames,
        s[TAKEN] / frames,
        s[TAKEN_BYTES] / 1e6 / frames,
        s[OTHER_ENTRY] / frames,
        s[NUMBERED] / frames,
        s[NAMED] / frames,
        s[DECLARED] / frames,
        s[MOVED] / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow::Shadow;
    use std::sync::Arc;

    fn shadow() -> Arc<Shadow> {
        let data: &'static mut [u8] = Box::leak(vec![0u8; 0x1000].into_boxed_slice());
        Shadow::unbacked(0x4000_0000, data.as_mut_ptr(), 0x1000, true)
    }

    fn held(steps: Vec<Step>) -> Held {
        let mut held = Held::default();
        held.note(steps.iter());
        held.finish()
    }

    /// What a submission does to a copy and to title memory is what the thread leaves alone, by copy and by grain: a
    /// copy declared written or placed on is whole its, a refreshed range is its grains, and a write into title memory is
    /// its grains.
    #[test]
    fn held_leaves_alone_what_the_submission_refreshes_declares_places_or_writes() {
        let (a, b, c, d) = (shadow(), shadow(), shadow(), shadow());
        let g = GRAIN;
        let steps = vec![
            // a: refreshed in the middle of its second grain (so the whole of it is held).
            Step::Refresh(a.clone(), 10 * g + 100, 50),
            // b: declared written (whole).
            Step::GpuWrite(b.clone(), 20 * g, 8),
            // c: nothing named; title memory written in the fourth grain of the 30th.
            Step::Copy(30 * g + 3 * g + 5, 0, 10),
            // d: refreshed at both ends of a gap.
            Step::Refresh(d.clone(), 40 * g, g),
            Step::Refresh(d.clone(), 44 * g, g),
        ];
        let held = held(steps);
        let (ka, kb, kc, kd) = (key(&a), key(&b), key(&c), key(&d));
        // a: the grain it refreshes is held, the next is free.
        assert!(!held.leaves(ka, 10 * g, 11 * g));
        assert!(!held.leaves(ka, 10 * g + g - 1, 11 * g + 5), "a range that reaches into the grain");
        assert!(held.leaves(ka, 11 * g, 12 * g));
        assert!(held.leaves(ka, 9 * g, 10 * g), "ends where the grain begins");
        // b: whole.
        assert!(!held.leaves(kb, 0, 1) && !held.leaves(kb, 50 * g, 51 * g));
        // c: no step names it, title memory written in a grain of it.
        assert!(held.leaves(kc, 0, g));
        assert!(!held.leaves(kc, 33 * g, 33 * g + 1));
        assert!(!held.leaves(kc, 32 * g + g - 1, 33 * g + 1));
        assert!(held.leaves(kc, 34 * g, 35 * g));
        // d: two ranges, the gap between is free.
        assert!(!held.leaves(kd, 40 * g, 40 * g + 1) && !held.leaves(kd, 44 * g, 45 * g));
        assert!(held.leaves(kd, 41 * g, 44 * g));
        // What names a copy at all.
        assert!(held.names(ka) && held.names(kb) && held.names(kd) && !held.names(kc));
        // Another copy's ranges do not leak into this one's.
        assert!(held.leaves(key(&shadow()), 10 * g, 11 * g));
        // A step that may write anywhere leaves nothing.
        let any = Held { unknown: true, ..Held::default() };
        assert!(!any.leaves(kc, 0, 1));
    }

    /// Two helds together hold what each holds; a held is the same finished twice.
    #[test]
    fn held_merges_and_is_the_same_finished_twice() {
        let (a, b) = (shadow(), shadow());
        let g = GRAIN;
        let one = held(vec![Step::Refresh(a.clone(), 0, g + 1), Step::Copy(8 * g, 0, 4)]);
        let two = held(vec![Step::Refresh(a.clone(), g, 2 * g), Step::GpuWrite(b.clone(), 0, 1)]);
        let both = one.clone().merged(two.clone());
        assert_eq!(both.clone().finish(), both, "idempotent");
        assert!(!both.leaves(key(&a), 0, 3 * g), "the two refreshed ranges are one, three grains");
        assert!(both.leaves(key(&a), 3 * g, 4 * g));
        assert!(!both.leaves(key(&b), 0, 1));
        assert!(!both.leaves(key(&shadow()), 8 * g, 8 * g + 1), "the write of the first");
        assert!(both.merged(Held { unknown: true, ..Held::default() }).unknown);
    }

    /// Spans are given to the thread only if the submission before leaves them and nothing in the air writes them.
    #[test]
    fn free_spans_are_those_nothing_holds_and_nothing_in_the_air_writes() {
        let (a, b, c, d) = (shadow(), shadow(), shadow(), shadow());
        let g = GRAIN;
        let held = held(vec![Step::Refresh(a.clone(), 0, g), Step::GpuWrite(b.clone(), 0, 1)]);
        let air = vec![(100 * g, 101 * g)];
        d.writing(60 * g, g);
        let spans: prefetch::Spans = vec![
            (a.clone(), 0, g),             // held: refreshed
            (a.clone(), 2 * g, 3 * g),     // free
            (b.clone(), 0, g),             // held: declared
            (c.clone(), 99 * g, 101 * g),  // a flight declares it
            (c.clone(), 5 * g, 6 * g),     // free
            (d.clone(), 60 * g, 60 * g + 5), // its own declaration
        ];
        let (free, owned, flying) = free(spans, &held, &air);
        assert_eq!((owned, flying), (2, 2));
        let got: Vec<(usize, usize, usize)> = free.iter().map(|(s, f, t)| (key(s), *f, *t)).collect();
        assert_eq!(got, vec![(key(&a), 2 * g, 3 * g), (key(&c), 5 * g, 6 * g)]);
    }

    /// A certificate stands only for the entry and the lists it was made for (more lists after them are allowed, and
    /// the copies they name are left out), only after the number it was made after, and each span only while nothing in the
    /// air writes it.
    #[test]
    fn certified_spans_stand_for_the_entry_the_number_and_the_lists_they_were_made_for() {
        let (a, b, c) = (shadow(), shadow(), shadow());
        let g = GRAIN;
        let cert = Cert::new(7, vec![0x10, 0x20], 41, vec![(a.clone(), 0, 2 * g), (b.clone(), 4 * g, 5 * g), (c.clone(), 6 * g, 7 * g)]);
        let none = Held::default();
        let ok = cert.usable(7, &[0x10, 0x20], 41, &[], &none);
        assert_eq!(ok.len(), 3);
        assert!(ok.covers(key(&a), 0, g) && ok.covers(key(&a), g, 2 * g) && ok.covers(key(&a), 0, 2 * g));
        assert!(!ok.covers(key(&a), g, 3 * g), "reaches out of the span");
        assert!(!ok.covers(key(&b), 0, g), "another range of the copy");
        assert!(!ok.covers(key(&shadow()), 0, g), "another copy");
        // Another entry, other lists, fewer lists, another order: nothing stands.
        assert!(cert.usable(8, &[0x10, 0x20], 41, &[], &none).is_empty());
        assert!(cert.usable(7, &[0x10, 0x30], 41, &[], &none).is_empty());
        assert!(cert.usable(7, &[0x10], 41, &[], &none).is_empty());
        assert!(cert.usable(7, &[0x20, 0x10], 41, &[], &none).is_empty());
        // A number taken in between (or none: the thread of the entry is early).
        assert!(cert.usable(7, &[0x10, 0x20], 42, &[], &none).is_empty());
        assert!(cert.usable(7, &[0x10, 0x20], 40, &[], &none).is_empty());
        // More lists after: allowed, but the copies they refresh or declare are not taken as done.
        let later = held(vec![Step::Refresh(b.clone(), 0, 1)]);
        let folded = cert.usable(7, &[0x10, 0x20, 0x30], 41, &[], &later);
        assert_eq!(folded.len(), 2);
        assert!(!folded.covers(key(&b), 4 * g, 5 * g) && folded.covers(key(&a), 0, g) && folded.covers(key(&c), 6 * g, 7 * g));
        // What flights declare written since: the span is not taken as done, whichever way it is declared.
        let air = vec![(g, g + 1)];
        let declared = cert.usable(7, &[0x10, 0x20], 41, &air, &none);
        assert_eq!(declared.len(), 2);
        assert!(!declared.covers(key(&a), 0, g) && !declared.covers(key(&a), 0, 2 * g));
        c.writing(6 * g + 10, 5);
        let own = cert.usable(7, &[0x10, 0x20], 41, &[], &none);
        assert_eq!(own.len(), 2);
        assert!(!own.covers(key(&c), 6 * g, 7 * g) && own.covers(key(&b), 4 * g, 5 * g));
        // A step that may write anywhere in the later lists: nothing.
        assert!(cert.usable(7, &[0x10, 0x20, 0x30], 41, &[], &Held { unknown: true, ..Held::default() }).is_empty());
    }
}
