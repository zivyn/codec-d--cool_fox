//! What stage A's thread does beyond the first version, each part by its own bit
//! (`VICTORY_STAGE_A_RELAX=<bits>`; off by default), and the account of why it does not do the rest.
//!
//! The thread of stage A ([`super::front`]) declines a submission before any step of it is done if
//! it would have to do something that only the thread of submissions may do. Every reason is a
//! question with one of three answers:
//!
//! - **Never** (hard): the thread cannot do it, or it was not proven that it may. The reason is
//!   named and counted. These are `Step::Run` (a title buffer copy that may write anywhere: it ends
//!   every asking that stands, which the next-but-one submission's early refresh stands on),
//!   `Host` steps that put work on the host queue or write what the send of the submission before
//!   still reads ([`Class::Unproven`]), the relay not being on, nothing in the air, the present's
//!   sweep, a submission cut for places, the check of copies.
//! - **By a bit**: the thread may do it, and it is a switch because it is a choice whether it pays:
//!   [`BOOKKEEPING`], [`HOST_READS`], [`PLANES`] (the steps), [`WAIT_READS`] and [`WAIT_STALE`] (the
//!   waits for flights).
//! - **Free**: refreshes, title buffer copies, fills, places, declarations, texture refreshes.
//!
//! The question a `Host` step asks, answered once per kind of step ([`Kind`], which every
//! `HostList::defer_host` names): does what it does depend on *when* it is done, other than by the
//! submission's number and by the steps before it in its list? A step that only notes things for
//! later (a note carrying the submission's number, a flag of a staging buffer the title's recording
//! reads, an atomic) does not: the thread of stage A takes the number before its steps, as the
//! thread of submissions does, and the order of the steps is the order of the lists. A step that
//! calls the host queue (`UpdateTileMappings`) or changes a copy the card's send reads does: the
//! thread of submissions is sending the submission before at the same time, so the call would be
//! ordered against that send by chance.
//!
//! The waits ([`wait_until`]) are bounded (the threads that make a number for a submission they wait
//! for hold back for stage A at most 200 ms, `front::lock_quiet`, so the thread gives up before that, 150 ms in all
//! and declines, which loses nothing: nothing was done yet).

use super::*;

/// Host steps that only note something for later: a slot flip, a texture's flags, the budget's words
/// of the list, a diagnostics note ([`Class::Bookkeeping`]).
pub(crate) const BOOKKEEPING: u32 = 1;
/// Host steps that read title memory and write the list's own scratch memory ([`Class::ReadsTitle`]).
pub(crate) const HOST_READS: u32 = 2;
/// Movie planes filled from title memory into the list's own upload memory (`Step::Plane`).
pub(crate) const PLANES: u32 = 4;
/// Wait on the thread for the flights that write what the submission reads to land, instead of
/// declining (the thread of submissions would have planned the reads for the card, or waited).
pub(crate) const WAIT_READS: u32 = 8;
/// Wait on the thread for the flights whose declared writes hold title bytes the refreshes must put
/// in, instead of declining.
pub(crate) const WAIT_STALE: u32 = 16;
/// Every bit there is.
pub(crate) const ALL: u32 = BOOKKEEPING | HOST_READS | PLANES | WAIT_READS | WAIT_STALE;

/// At most this long in all the thread waits for flights to land (for the reads and for the bytes under flights together)
/// before it gives the submission back: less than the 200 ms `front::lock_quiet` holds the threads that make a number
/// for a submission they wait for.
pub(super) const TOTAL_WAIT: std::time::Duration = std::time::Duration::from_millis(150);

fn parse_bits(text: Option<&str>) -> u32 {
    text.and_then(|t| t.trim().parse::<u32>().ok()).map_or(0, |n| n & ALL)
}

/// The bits of `VICTORY_STAGE_A_RELAX` that are on now.
///
/// (May change while the title runs, `VICTORY_AB`: `Host::front` reads it once for a submission, before any of its
/// steps, and what it decides is for that submission alone: the steps of a submission are all done or none.)
pub(super) fn bits() -> u32 {
    crate::ab::live_value!("VICTORY_STAGE_A_RELAX", u32, parse_bits)
}

/// Whether the entry after the next has the refreshes of its untouched copies begun while the next is prepared
/// (`VICTORY_STAGE_A_DEEP=1`).
///
/// (May change while the title runs, `VICTORY_AB`: `front::offer` reads it once per submission; what was offered is joined by
/// `prefetch::join`, which looks at the gate whatever the switch says.)
pub(super) fn deep() -> bool {
    crate::ab::live_flag!("VICTORY_STAGE_A_DEEP")
}

/// What a `Host` step is, for what a thread other than the one that submits may do with it (every `HostList::defer_host`
/// says).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[allow(dead_code)]
pub(crate) enum Kind {
    /// Not said: never done by the thread.
    Other,
    /// A texture slot's flip is noted with the submission's number (`HostHeap::flip`).
    SlotFlip,
    /// A placed texture's levels changed: noted with the submission's number.
    LevelsChanged,
    /// The list's own budget words are read into counters and zeroed.
    BudgetNotes,
    /// A texture's staging buffer is marked as uploaded by this list.
    UploadNoted,
    /// A texture's host copy is noted as holding what the staging buffer was filled from.
    HeldNoted,
    /// A line of the diagnostics book.
    Diagnostics,
    /// Rectangle vertices read from title memory and expanded into the list's scratch memory.
    RectVertices,
    /// Tiles given to a texture's levels on the host queue (levels on demand).
    TilesGiven,
    /// Tiles claimed for a write by the card, on the host queue.
    TilesClaimed,
    /// Pages of title memory brought into the arenas the card's compute reads.
    MemoryLanded,
}

/// What the thread may do with a kind of `Host` step.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Class {
    /// Notes for later (the bit [`BOOKKEEPING`]).
    Bookkeeping,
    /// Reads title memory, writes memory of the list's own (the bit [`HOST_READS`]).
    ReadsTitle,
    /// Not proven to be safe, with why.
    Unproven(&'static str),
}

impl Kind {
    /// Every kind, for the tests.
    #[cfg(test)]
    pub(crate) const ALL: [Kind; 11] = [
        Kind::Other,
        Kind::SlotFlip,
        Kind::LevelsChanged,
        Kind::BudgetNotes,
        Kind::UploadNoted,
        Kind::HeldNoted,
        Kind::Diagnostics,
        Kind::RectVertices,
        Kind::TilesGiven,
        Kind::TilesClaimed,
        Kind::MemoryLanded,
    ];

    pub(crate) fn name(self) -> &'static str {
        match self {
            Kind::Other => "Host: a step nobody named",
            Kind::SlotFlip => "Host: a descriptor slot's flip noted",
            Kind::LevelsChanged => "Host: a texture's levels changed, noted",
            Kind::BudgetNotes => "Host: the list's budget words taken",
            Kind::UploadNoted => "Host: a texture's upload noted",
            Kind::HeldNoted => "Host: a texture's held hash noted",
            Kind::Diagnostics => "Host: a diagnostics note",
            Kind::RectVertices => "Host: rectangle vertices read from title memory",
            Kind::TilesGiven => "Host: tiles given to a texture (host queue)",
            Kind::TilesClaimed => "Host: tiles claimed for a card write (host queue)",
            Kind::MemoryLanded => "Host: pages landed in the arenas",
        }
    }

    /// What the thread of stage A may do with it, and why not if it may not.
    ///
    /// The arguments, per kind (each closure was read in full):
    /// - `SlotFlip`: takes the flip it registered out of a global list and pushes it, with the number of the
    ///   submission as of now, on another; its readers (`texture_slots_flip`) look at flips whose number the
    ///   finished submissions have reached, which this submission has not, whichever thread pushed.
    /// - `LevelsChanged`: pushes the number and an address on a list read the same way.
    /// - `BudgetNotes`: reads and zeroes five words of the list's own mapped memory (the list is not in flight:
    ///   it is being submitted) and adds to counters.
    /// - `UploadNoted`, `HeldNoted`: flags and a hash of one staging buffer, set in the order of this list's steps
    ///   (the texture's own refresh is a step of the same list); the title's recording reads the flags to decide
    ///   whether to record an upload, and a list recorded later is behind this submission in the line, so the
    ///   upload it skips is on the queue first whichever thread set them.
    /// - `Diagnostics`: a line in a book under its own lock.
    /// - `RectVertices`: reads title memory (the same read the thread of submissions would do, under the same
    ///   checks of flights: `copied_read` looks at host reads) and writes the list's scratch memory.
    /// - `TilesGiven`, `TilesClaimed`: `UpdateTileMappings` on the host queue: a call on the queue is ordered
    ///   with the `ExecuteCommandLists` calls of the thread of submissions by the time they are made, and at
    ///   the time this would run that thread is sending the submission before: tiles would be remapped under
    ///   its lists.
    /// - `MemoryLanded`: reads pages of title memory into copies of the arenas, under a lock the title's own
    ///   threads take; whether the send of the submission before still reads those copies was not shown.
    pub(crate) fn class(self) -> Class {
        match self {
            Kind::SlotFlip | Kind::LevelsChanged | Kind::BudgetNotes | Kind::UploadNoted | Kind::HeldNoted | Kind::Diagnostics => {
                Class::Bookkeeping
            }
            Kind::RectVertices => Class::ReadsTitle,
            Kind::TilesGiven | Kind::TilesClaimed => Class::Unproven("it maps tiles on the host queue, ordered against the send of the submission before"),
            Kind::MemoryLanded => Class::Unproven("it writes arena copies that the send of the submission before may read: not proven"),
            Kind::Other => Class::Unproven("a step nobody named"),
        }
    }
}

/// The bits of `mask` as the text of the frame line: `1+8`.
pub(super) fn describe(mask: u32) -> String {
    let parts: Vec<&str> = [
        (BOOKKEEPING, "1 bookkeeping steps"),
        (HOST_READS, "2 host reads"),
        (PLANES, "4 planes"),
        (WAIT_READS, "8 wait for the reads"),
        (WAIT_STALE, "16 wait for bytes under flights"),
    ]
    .iter()
    .filter(|(bit, _)| mask & bit != 0)
    .map(|(_, name)| *name)
    .collect();
    if parts.is_empty() { "nothing".to_string() } else { parts.join(" + ") }
}

/// What the steps of a submission's lists ask of the thread: the bits that would let it through and the step, if any,
/// that no bit does.
#[derive(Default, Debug)]
pub(super) struct Seen {
    /// The bits of the steps and flights this submission needs.
    pub(super) need: u32,
    /// A step the bits cannot let through, with why.
    pub(super) hard: Option<&'static str>,
    /// The kinds of step that need a bit, each once.
    pub(super) kinds: Vec<&'static str>,
}

impl Seen {
    fn needs(&mut self, bit: u32, name: &'static str) {
        self.need |= bit;
        if !self.kinds.contains(&name) {
            self.kinds.push(name);
        }
    }

    fn never(&mut self, why: &'static str) {
        self.hard.get_or_insert(why);
    }

    /// Looks at the steps of one list.
    pub(super) fn steps<'a>(&mut self, steps: impl Iterator<Item = &'a Step>) {
        for step in steps {
            match step {
                Step::Run(_) => self.never("Run: a title buffer copy that may write anywhere"),
                Step::Plane(..) => self.needs(PLANES, "Plane: a movie plane filled from title memory"),
                Step::Host(_, _, kind) => match kind.class() {
                    Class::Bookkeeping => self.needs(BOOKKEEPING, kind.name()),
                    Class::ReadsTitle => self.needs(HOST_READS, kind.name()),
                    Class::Unproven(_) => self.never(kind.name()),
                },
                Step::Copy(..) | Step::Fill(..) | Step::Refresh(..) | Step::GpuWrite(..) | Step::Texture(_) | Step::Placed(_) => {}
            }
        }
    }
}

/// Which step of `runs` flights stop, for the decline line (the first, as the scan finds it): called only on a decline.
pub(super) fn what_waits(written: &[(usize, usize)], copies: &[card_steps::Writer], runs: &[&HostList]) -> &'static str {
    for list in runs {
        for step in list.deferred().copies.iter() {
            match step {
                &Step::Copy(dst, src, len) => {
                    if let Some(read) = copy_blocked(written, dst, src, len) {
                        return if read { "a title buffer copy's source" } else { "a title buffer copy's destination" };
                    }
                }
                &Step::Fill(_, src, len, _) => {
                    if read_blocked(written, src, len) {
                        return "a fill of upload memory";
                    }
                }
                Step::Texture(staging) => {
                    let (at, len) = staging.memory();
                    if touches(written, at, at + len) {
                        return "a texture's refresh";
                    }
                }
                &Step::Plane((at, len), _) => {
                    if touches(written, at, at.saturating_add(len)) {
                        return "a movie plane";
                    }
                }
                Step::Placed(p) => {
                    if !matches!(p.waits(written), place::Waits::No) {
                        return "a buffer copy at its place";
                    }
                }
                _ => {}
            }
        }
    }
    if card_steps::copied_read(copies, runs) {
        return "a refresh or host read of a copy done on the card";
    }
    "(gone by the time it was looked at)"
}

/// Waits until `done` is true of what `lock` holds, told by `changed`, at most `most`: whether it is. (The wait is made
/// in slices of 4 ms: a change that is not told, as a registration that ends, is found by the next look.)
pub(super) fn wait_until<T>(lock: &Mutex<T>, changed: &std::sync::Condvar, done: impl Fn(&T) -> bool, most: std::time::Duration) -> bool {
    let began = std::time::Instant::now();
    let mut held = lock.lock().unwrap_or_else(|e| e.into_inner());
    loop {
        if done(&held) {
            return true;
        }
        let left = most.saturating_sub(began.elapsed());
        if left.is_zero() {
            return false;
        }
        held = changed.wait_timeout(held, left.min(std::time::Duration::from_millis(4))).unwrap_or_else(|e| e.into_inner()).0;
    }
}

/// [`wait_until`] on the line of flights ([`LINE_LANDED`] tells when some have landed).
pub(super) fn wait_flights(done: impl Fn(&Line) -> bool, most: std::time::Duration) -> bool {
    wait_until(&LINE, &LINE_LANDED, done, most)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn list(kinds: &[Kind]) -> Vec<Step> {
        kinds.iter().map(|&kind| Step::Host(Box::new(|| {}), (0, 0), kind)).collect()
    }

    fn seen(steps: &[Step]) -> Seen {
        let mut seen = Seen::default();
        seen.steps(steps.iter());
        seen
    }

    /// Every kind is one of the three answers, and the answer is the one the bit table says.
    #[test]
    fn each_kind_of_host_step_has_its_answer() {
        for kind in Kind::ALL {
            let s = seen(&list(&[kind]));
            match kind.class() {
                Class::Bookkeeping => {
                    assert_eq!((s.need, s.hard), (BOOKKEEPING, None), "{kind:?}");
                    assert_eq!(s.kinds, vec![kind.name()]);
                }
                Class::ReadsTitle => assert_eq!((s.need, s.hard), (HOST_READS, None), "{kind:?}"),
                Class::Unproven(_) => assert_eq!((s.need, s.hard), (0, Some(kind.name())), "{kind:?}"),
            }
        }
        // Names are told apart (they are what the frame line counts by).
        let mut names: Vec<&str> = Kind::ALL.iter().map(|k| k.name()).collect();
        names.sort_unstable();
        names.dedup();
        assert_eq!(names.len(), Kind::ALL.len());
    }

    /// The kinds that note things for later are let through by bit 1 alone.
    #[test]
    fn bookkeeping_steps_need_bit_one_only() {
        for kind in [Kind::SlotFlip, Kind::LevelsChanged, Kind::BudgetNotes, Kind::UploadNoted, Kind::HeldNoted, Kind::Diagnostics] {
            let s = seen(&list(&[kind]));
            assert_eq!(s.need & !BOOKKEEPING, 0, "{kind:?}");
            assert_eq!(s.need & BOOKKEEPING, BOOKKEEPING, "{kind:?}");
            assert!(s.hard.is_none());
        }
    }

    /// The reads of title memory need bit 2 and nothing more, and a movie plane bit 4; a step of the kinds that call the
    /// host queue is never let through, nor `Run`.
    #[test]
    fn reads_planes_and_the_steps_that_are_never_done() {
        assert_eq!(seen(&list(&[Kind::RectVertices])).need, HOST_READS);
        let plane = [Step::Plane((0x1000, 64), Box::new(|| {}))];
        assert_eq!(seen(&plane).need, PLANES);
        assert!(seen(&plane).hard.is_none());
        for kind in [Kind::TilesGiven, Kind::TilesClaimed, Kind::MemoryLanded, Kind::Other] {
            let s = seen(&list(&[kind]));
            assert_eq!(s.hard, Some(kind.name()), "{kind:?} is never done on the thread");
        }
        let run = [Step::Run(Box::new(|| {}))];
        assert!(seen(&run).hard.is_some());
        // The first hard reason stands and the bits of the others are still said.
        let mixed = [
            Step::Host(Box::new(|| {}), (0, 0), Kind::SlotFlip),
            Step::Run(Box::new(|| {})),
            Step::Host(Box::new(|| {}), (0, 0), Kind::TilesGiven),
        ];
        let s = seen(&mixed);
        assert_eq!(s.need, BOOKKEEPING);
        assert_eq!(s.hard, Some("Run: a title buffer copy that may write anywhere"));
    }

    /// For each kind of step and each setting of the bits: let through exactly when the kind's bit is on, the kinds that are
    /// never done not by any setting; a list of several kinds needs all their bits.
    #[test]
    fn each_relaxed_kind_is_let_through_by_its_bit_and_only_by_it() {
        let wants = |kind: Kind| match kind {
            Kind::SlotFlip | Kind::LevelsChanged | Kind::BudgetNotes | Kind::UploadNoted | Kind::HeldNoted | Kind::Diagnostics => Some(BOOKKEEPING),
            Kind::RectVertices => Some(HOST_READS),
            Kind::TilesGiven | Kind::TilesClaimed | Kind::MemoryLanded | Kind::Other => None,
        };
        let through = |steps: &[Step], bits: u32| {
            let s = seen(steps);
            s.hard.is_none() && s.need & !bits == 0
        };
        for kind in Kind::ALL {
            for bits in 0..=ALL {
                let expected = wants(kind).is_some_and(|bit| bits & bit != 0);
                assert_eq!(through(&list(&[kind]), bits), expected, "{kind:?} with bits {bits}");
            }
        }
        let plane = [Step::Plane((0x1000, 64), Box::new(|| {}))];
        for bits in 0..=ALL {
            assert_eq!(through(&plane, bits), bits & PLANES != 0, "a plane with bits {bits}");
            assert!(!through(&[Step::Run(Box::new(|| {}))], bits), "a run with bits {bits}");
        }
        let both = list(&[Kind::SlotFlip, Kind::RectVertices]);
        assert!(!through(&both, BOOKKEEPING) && !through(&both, HOST_READS) && through(&both, BOOKKEEPING | HOST_READS));
    }

    /// Free steps ask for nothing.
    #[test]
    fn plain_steps_ask_for_nothing() {
        let steps = [Step::Copy(8, 16, 4), Step::Fill(1, 2, 3, None)];
        let s = seen(&steps);
        assert_eq!((s.need, s.hard, s.kinds.len()), (0, None, 0));
    }

    /// The same kind of step counts once in the list of kinds however many there are.
    #[test]
    fn a_kind_is_named_once() {
        let s = seen(&list(&[Kind::SlotFlip, Kind::SlotFlip, Kind::SlotFlip, Kind::HeldNoted]));
        assert_eq!(s.kinds, vec![Kind::SlotFlip.name(), Kind::HeldNoted.name()]);
    }

    #[test]
    fn the_bits_parse_and_are_cut_to_those_there_are() {
        assert_eq!(parse_bits(None), 0);
        assert_eq!(parse_bits(Some("")), 0);
        assert_eq!(parse_bits(Some("1")), BOOKKEEPING);
        assert_eq!(parse_bits(Some(" 7 ")), BOOKKEEPING | HOST_READS | PLANES);
        assert_eq!(parse_bits(Some("255")), ALL);
        assert_eq!(parse_bits(Some("x")), 0);
        assert_eq!(describe(0), "nothing");
        assert_eq!(describe(BOOKKEEPING | WAIT_READS), "1 bookkeeping steps + 8 wait for the reads");
    }

    /// A wait ends when what it waits for happens, and gives up at the time it was given, never later than a slice past it.
    #[test]
    fn a_wait_for_flights_ends_when_they_land_and_gives_up_in_time() {
        let state = std::sync::Arc::new((Mutex::new(0u32), std::sync::Condvar::new()));
        let ms = std::time::Duration::from_millis;
        // Already so: no wait.
        assert!(wait_until(&state.0, &state.1, |n| *n == 0, ms(500)));
        // Never so: it gives up.
        let began = std::time::Instant::now();
        assert!(!wait_until(&state.0, &state.1, |n| *n == 9, ms(30)));
        assert!(began.elapsed() >= ms(25) && began.elapsed() < ms(2000));
        // Becomes so while it waits, told.
        let waiter = {
            let state = state.clone();
            std::thread::spawn(move || wait_until(&state.0, &state.1, |n| *n == 1, ms(10_000)))
        };
        std::thread::sleep(ms(30));
        *state.0.lock().unwrap() = 1;
        state.1.notify_all();
        assert!(waiter.join().unwrap());
        // Becomes so and is not told: the next slice finds it.
        let waiter = {
            let state = state.clone();
            std::thread::spawn(move || wait_until(&state.0, &state.1, |n| *n == 2, ms(10_000)))
        };
        std::thread::sleep(ms(30));
        *state.0.lock().unwrap() = 2;
        assert!(waiter.join().unwrap(), "found without being told");
    }
}
