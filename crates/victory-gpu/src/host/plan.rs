//! The steps given to the card, planned by the thread of stage A while the submission before is sent
//! (`VICTORY_STAGE_A_PLAN=1`, with `VICTORY_STAGE_A_STEPS`; off by default).
//!
//! The thread of stage A ([`super::front`]) declined every submission that reads title memory flights in the air write:
//! the thread of submissions plans those reads for the card ([`card_steps`], 7.7 ms a frame, the title buffer copies and
//! the fills of constants that the card does behind the flights instead of the CPU after a wait) or waits, and the whole
//! of stage A stayed with it (about 36 ms of 56, STAGE3). Here the thread plans them too, and goes on with the refreshes,
//! the steps (the fills that are left, which write the list's own upload memory) and the rest.
//!
//! What the plan reads, and what makes it the plan the thread of submissions would have made after the send:
//!
//! 1. **The line of flights.** It reads what flights write (`written`) and, to find the host copy that holds a range
//!    once they have written it, who writes what through which copy ([`card_steps::snapshot`]: the flights in the line,
//!    those being landed, and the submissions registered and not yet flights). The submission being sent is one of
//!    those: its flight enters the line only after its send, so at the offer it is *registered* with everything the
//!    flight will say (`PendingFlight`: its ranges, and now [`pending_writers`]: its shaders' writes through their host
//!    copies, its read backs, the values it writes when done, the copies it does on the card), taken from the same lists
//!    before the card is given anything of it. A plan made beside the send sees the same writers the plan made after it
//!    would, and where flights land in between it sees more of them, never fewer: what is written only shrinks, so a step
//!    the card does that the later plan would have left to the CPU is only a step done on the card in order behind the
//!    flights (every one it reads from is ahead of the list on the queue), and no step that is asked at the later look
//!    was left to the CPU. Everything is read in one hold of the line's lock together with the generation
//!    ([`Line::generation`], how many submissions were ever registered).
//! 2. **Nothing else of title memory.** The plan does not read the bytes (the card reads them from the host copies, which
//!    the list's refresh steps bring up to date with the other refreshes of the submission, in the order stage A does
//!    them); the only look at title memory is whether the pages are mapped.
//! 3. **What the plan writes.** The list of ours (recorded, closed: nothing of it is seen by anyone until the lane gives
//!    it to the queue), a buffer of ours, and descriptor slots that only the fills' own recording names. The slots are
//!    made when the plan is *committed* ([`card_steps::Chosen::commit`]), which is the last thing the thread does before
//!    its point of no return (the number): a plan dropped before is only a list back in the pool.
//! 4. **Revalidation.** Before it commits, and again when the lane takes the submission, the generation of the line must be
//!    the one the plan was made against ([`agrees`]): a submission registered in between would write what the plan did not
//!    know. Nothing can register one (the lane registers its own submission after it has taken this one, and the
//!    submissions that wait for their number wait until stage A is taken, `front::lock_quiet`), so a mismatch is not
//!    expected. Before the commit it gives the submission back (the lane plans it, counted as "redone"); after it the steps
//!    are done and cannot be undone, so the lane stops the process, as for lists that differ.
//! 5. **Ownership of upload memory.** The fills the thread does write the destination of each `Step::Fill`, which is memory
//!    of its list's own scratch chunks or upload buffers (`HostList::scratch`, `upload`), held by the list until it is
//!    reset after its last execution and taken by no other list; the title's submissions after it are other lists with
//!    other chunks, and the submission before is another list. The one buffer the plan adds is its own (`between`), owned
//!    by the plan until it has landed. So no page the thread writes is read by the card for another submission.
//!
//! What stays on the thread of submissions: `ExecuteCommandLists`, the signals, `QUEUE_ORDER`, the submission lock, and
//! the plan's list being first on the queue (it goes in front of the submission's lists, as when the lane plans).

use super::*;

/// Whether the thread of stage A plans the submission's steps for the card.
///
/// (May change while the title runs, `VICTORY_AB`: `Host::front` reads it once per submission, before any step, and what
/// the lane finds with the submission it takes is used whatever the switch says by then.)
pub(super) fn on() -> bool {
    crate::ab::live_flag!("VICTORY_STAGE_A_PLAN")
}

/// A plan the thread chose, with the generation of the line it was made against.
pub(super) struct Planned {
    pub(super) chosen: card_steps::Chosen,
    generation: u64,
}

impl Planned {
    /// The generation of the line it was made against.
    pub(super) fn generation(&self) -> u64 {
        self.generation
    }

    /// Whether the line is still as it was (nothing was registered since).
    pub(super) fn holds(&self) -> bool {
        agrees(self.generation, line().generation)
    }
}

/// What came of planning a submission on the thread.
pub(super) enum Tried {
    Planned(Planned),
    /// Nothing in the air writes anything the submission reads, any more.
    NotBlocked,
    /// Something is in the way and the plan takes nothing of it (the lane waits then).
    NothingToTake,
    /// The plan was refused, and why.
    Refused(&'static str),
}

/// Whether a plan made against generation `planned_at` of the line stands at `now`: only if it is the same.
pub(super) fn agrees(planned_at: u64, now: u64) -> bool {
    planned_at == now
}

/// Since the last frame-rate line: plans made, offers where nothing was in the way any more, offers where the plan took
/// nothing, plans refused, plans given up before the commit because another submission was registered (redone by the
/// thread of submissions), plans made and given up for another reason, the microseconds the thread planned, plans the
/// thread of submissions took over.
static STATS: [std::sync::atomic::AtomicU64; 8] =
    [const { std::sync::atomic::AtomicU64::new(0) }; 8];
const PLANNED: usize = 0;
const NOT_BLOCKED: usize = 1;
const NOTHING: usize = 2;
const REFUSED: usize = 3;
const REDONE: usize = 4;
const DROPPED: usize = 5;
const US: usize = 6;
const TAKEN: usize = 7;

/// The reasons plans were refused, with how many times.
static WHY: Mutex<Vec<(&'static str, u64)>> = Mutex::new(Vec::new());

/// A plan was given up before the commit because the line had changed.
pub(super) fn redone() {
    STATS[REDONE].fetch_add(1, Ordering::Relaxed);
}

/// A plan was made and the submission was declined for another reason.
pub(super) fn dropped() {
    STATS[DROPPED].fetch_add(1, Ordering::Relaxed);
}

/// The thread of submissions takes a submission with the plan the thread of stage A made, against generation
/// `planned_at` of the line: it must be the line's now. The steps of the submission are done and cannot be undone, and a
/// plan that did not know of a flight's writes may have left to the CPU what the card was to do after it: stops the
/// process, as it does for lists that differ ([`front::take`]).
pub(super) fn check(planned_at: u64) {
    let now = line().generation;
    if !agrees(planned_at, now) {
        fail(format_args!(
            "stage A planned the steps for the card against generation {planned_at} of the line of flights and the submission is taken at {now} (a submission was registered since; the steps are done and cannot be undone)"
        ));
    }
    STATS[TAKEN].fetch_add(1, Ordering::Relaxed);
}

impl Host {
    /// Plans the steps of `runs` (the next submission's lists) for the card, as [`Host::card_steps`] would when it is
    /// sent, from the line as it is now with the submission being sent in it ([`card_steps::snapshot`]).
    pub(super) fn plan_ahead(&self, runs: &[&HostList]) -> Tried {
        let began = std::time::Instant::now();
        let tried = self.plan_ahead_in(runs);
        STATS[US].fetch_add(began.elapsed().as_micros() as u64, Ordering::Relaxed);
        match &tried {
            Tried::Planned(_) => STATS[PLANNED].fetch_add(1, Ordering::Relaxed),
            Tried::NotBlocked => STATS[NOT_BLOCKED].fetch_add(1, Ordering::Relaxed),
            Tried::NothingToTake => STATS[NOTHING].fetch_add(1, Ordering::Relaxed),
            Tried::Refused(why) => {
                let mut all = WHY.lock().unwrap_or_else(|e| e.into_inner());
                match all.iter_mut().find(|w| w.0 == *why) {
                    Some(w) => w.1 += 1,
                    None => all.push((*why, 1)),
                }
                STATS[REFUSED].fetch_add(1, Ordering::Relaxed)
            }
        };
        tried
    }

    fn plan_ahead_in(&self, runs: &[&HostList]) -> Tried {
        if card_steps::stage() == 0 || !relay_exact() {
            return Tried::Refused("the steps for the card are off");
        }
        let snapshot = card_steps::snapshot();
        if snapshot.written.is_empty() || blocked_by(&snapshot.written, runs, false).is_none() {
            return Tried::NotBlocked;
        }
        if card_steps::copied_read(&snapshot.copies, runs) {
            return Tried::Refused("a copy done on the card is read through another host copy");
        }
        let (written, writers, generation) =
            (snapshot.written, snapshot.writers, snapshot.generation);
        match self.choose_steps(&written, runs, false, Some(writers)) {
            Ok(Some(chosen)) => Tried::Planned(Planned { chosen, generation }),
            Ok(None) => Tried::NothingToTake,
            Err(why) => Tried::Refused(why),
        }
    }
}

/// What a submission writes besides the ranges its flight will declare, as the line sees it once it is a flight
/// ([`card_steps::writers`]): its shaders' writes by host copy (`back`), its read backs (`reads_back`: the title
/// address and the row, the rows and the pitch), the values its lists write when they are done (`writes`: address, bytes)
/// and the copies it does on the card (`copies`). Registered with the submission ([`PendingFlight`]) before the card is
/// given anything of it.
pub(super) fn pending_writers(
    back: &[Declared],
    reads_back: impl IntoIterator<Item = (usize, [usize; 3])>,
    writes: impl IntoIterator<Item = (usize, usize)>,
    copies: impl IntoIterator<Item = card_steps::Writer>,
) -> Vec<card_steps::Writer> {
    let mut out = Vec::new();
    for (shadow, ranges) in back {
        out.extend(
            ranges
                .iter()
                .map(|&(from, to)| card_steps::Writer::new(Some(shadow.clone()), from, to, false)),
        );
    }
    for (at, [row, rows, pitch]) in reads_back {
        out.push(card_steps::Writer::new(
            None,
            at,
            at + rows.saturating_sub(1) * pitch + row,
            false,
        ));
    }
    for (at, bytes) in writes {
        out.push(card_steps::Writer::new(None, at, at + bytes, false));
    }
    out.extend(copies);
    out
}

/// The read backs and the written values of `runs`, for [`pending_writers`].
pub(super) fn backs_and_writes(
    runs: &[&HostList],
) -> (Vec<(usize, [usize; 3])>, Vec<(usize, usize)>) {
    let (mut backs, mut writes) = (Vec::new(), Vec::new());
    for list in runs {
        let deferred = list.deferred();
        backs.extend(deferred.backs.iter().map(|&(_, at, rows)| (at, rows)));
        writes.extend(deferred.writes.iter().map(|&(at, _, bytes)| (at, bytes)));
    }
    (backs, writes)
}

/// For the frame-rate line.
pub(super) fn frame_line(frames: f64) {
    let s: Vec<f64> = STATS
        .iter()
        .map(|s| s.swap(0, Ordering::Relaxed) as f64)
        .collect();
    let mut why: Vec<(&'static str, u64)> =
        std::mem::take(&mut *WHY.lock().unwrap_or_else(|e| e.into_inner()));
    if !on() && s.iter().all(|&n| n == 0.0) {
        return;
    }
    why.sort_by_key(|w| std::cmp::Reverse(w.1));
    let why: Vec<String> = why
        .iter()
        .map(|(text, n)| format!("{:.2} {text}", *n as f64 / frames))
        .collect();
    tracing::info!(
        "stage A plans the steps for the card on its thread (VICTORY_STAGE_A_PLAN), per frame: {:.2} planned, {:.2} taken by the thread of submissions, {:.2} found nothing in the way any more, {:.2} found nothing to take, {:.2} refused [{}], {:.2} planned and given up for another reason, {:.2} planned against a line that had changed (redone by the thread of submissions); the thread planned {:.1} ms",
        s[PLANNED] / frames,
        s[TAKEN] / frames,
        s[NOT_BLOCKED] / frames,
        s[NOTHING] / frames,
        s[REFUSED] / frames,
        why.join("; "),
        s[DROPPED] / frames,
        s[REDONE] / frames,
        s[US] / 1000.0 / frames
    );
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::shadow::Shadow;
    use card_steps::Writer;
    use std::sync::Arc;

    fn shadow() -> Arc<Shadow> {
        let data: &'static mut [u8] = Box::leak(vec![0u8; 0x4000].into_boxed_slice());
        Shadow::unbacked(0x1000, data.as_mut_ptr(), 0x4000, false)
    }

    /// A plan stands against the generation it was made against and no other; the line counts what is registered, and a
    /// registration that ends (a flight in the line, a submission dropped) does not take it back.
    #[test]
    fn a_plan_stands_only_while_nothing_else_is_registered() {
        let mut line = Line::new();
        let made_at = line.generation;
        assert!(agrees(made_at, line.generation));
        let one = line.register_pending_with(vec![(0x2000, 0x2100)], Vec::new());
        assert!(
            !agrees(made_at, line.generation),
            "a submission registered since"
        );
        // The plan is made against the line with the submission being sent in it: the next registration is the mismatch.
        let after_one = line.generation;
        assert!(agrees(after_one, line.generation));
        line.clear_pending(one);
        assert!(
            agrees(after_one, line.generation),
            "a registration that ends changes nothing a plan was told"
        );
        let two = line.register_pending_with(vec![(0x3000, 0x3100)], Vec::new());
        assert!(
            !agrees(after_one, line.generation),
            "forced: another submission registered after the plan was made"
        );
        let after_two = line.generation;
        {
            let Line {
                pending,
                pending_copies,
                ..
            } = &mut line;
            PendingFlight(Some(two)).done(pending, pending_copies);
        }
        assert!(
            agrees(after_two, line.generation),
            "it became a flight: the same writers"
        );
        assert!(!agrees(0, u64::MAX) && agrees(u64::MAX, u64::MAX));
    }

    /// What a submission registers before its send says what its flight will say once it is in the line: the same writers,
    /// through the same host copies, the same values and read backs, the same copies done on the card.
    #[test]
    fn what_is_registered_before_the_send_is_what_the_flight_says_after_it() {
        let (a, b) = (shadow(), shadow());
        let back: Vec<Declared> = vec![
            (a.clone(), vec![(0x1000, 0x1100), (0x1800, 0x1900)]),
            (b.clone(), vec![(0x2000, 0x2400)]),
        ];
        let reads_back = vec![
            (0x3000usize, [16usize, 4usize, 256usize]),
            (0x3800, [8, 1, 8]),
        ];
        let writes = vec![(0x4000usize, 8usize)];
        let copy = || Writer::new(Some(a.clone()), 0x1400, 0x1440, true);
        let registered = pending_writers(&back, reads_back.clone(), writes.clone(), [copy()]);

        // The flight, as `card_steps::writers` reads it: its shaders' writes by host copy, its read backs, its card copies, and
        // the value writes that follow it in the line. (The read backs are those of its lists: a flight with no lists here
        // is given them as writers of the same title memory, which is what the reading of a list's `backs` produces.)
        let mut items = vec![Late::Run(Flight {
            number: 1,
            fence: 1,
            lists: Vec::new(),
            order: Vec::new(),
            card: None,
            back: back.clone(),
            own: None,
            texture_back_ranges: Vec::new(),
        })];
        items.push(Late::Write(0x4000, crate::queue::Written::Value(7), 8));
        let mut flight: Vec<(usize, usize, bool, usize)> = card_steps::writers(&items)
            .iter()
            .map(Writer::parts)
            .collect();
        // (What a list's read backs add to a flight, as `card_steps::writers` does for each list of its order.)
        for &(at, [row, rows, pitch]) in &reads_back {
            flight.push((at, at + rows.saturating_sub(1) * pitch + row, false, 0));
        }
        flight.push((0x1400, 0x1440, true, Arc::as_ptr(&a) as usize));
        let mut before: Vec<(usize, usize, bool, usize)> =
            registered.iter().map(Writer::parts).collect();
        before.sort_unstable();
        flight.sort_unstable();
        assert_eq!(before, flight);
    }

    /// The registered writers are in the line's writers from the registration until the flight is in the line, and a
    /// snapshot of the line has them while it is registered, once and no more.
    #[test]
    fn the_writers_of_a_submission_being_sent_are_in_the_snapshot_until_it_is_a_flight() {
        let a = shadow();
        let mut line = Line::new();
        let writer = Writer::new(Some(a.clone()), 0x1000, 0x1100, false);
        let id = line.register_pending_with(
            vec![(0x1000, 0x1100)],
            vec![writer, Writer::new(None, 0x2000, 0x2008, false)],
        );
        let snapshot_of = |line: &Line| {
            let mut all = card_steps::writers(&line.line);
            all.extend(line.landing.iter().map(Writer::dup));
            all.extend(
                line.pending_copies
                    .iter()
                    .flat_map(|p| p.1.iter())
                    .map(Writer::dup),
            );
            all.len()
        };
        assert_eq!(snapshot_of(&line), 2);
        // Only copies are destinations of copies done on the card: the thread of stage A's look at them is unchanged.
        assert!(card_steps::copies(&line).is_empty());
        line.clear_pending(id);
        assert_eq!(snapshot_of(&line), 0);
    }
}
