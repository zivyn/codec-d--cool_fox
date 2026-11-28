//! Refreshes driven by the pages the title wrote (`VICTORY_PAGE_REFRESH`).
//!
//! A host copy takes title memory again when the write-watch journal
//! ([`crate::watch`]) says a page of it was written after the copy took it.
//! The journal answers "which pages of this range were ever written" and the
//! copy compares each with the asking after which it took the page: the work
//! of a refresh, and of the look that finds nothing to refresh, is a walk of
//! the range's pages and of the journal's entries in it, which hold every page
//! written since tracking began, not the ones that are stale.
//!
//! Here the index is turned round. Every page the journal notes as written is
//! also told to the copies that cover it (a [`Ledger`] each, found by page in
//! the [`Plane`]'s index, aliased views of one physical page included), and a
//! copy's ledger holds exactly the pages noted since the copy took them: what
//! a refresh has to look at and what a look has to ask is that set and its
//! size, not the range's.
//!
//! The invariant (I-LEDGER): for every page of a copy that the copy took once
//! (`copied[page] != 0`) and the journal says was written after that
//! (`written[page] > copied[page]`), the ledger holds the page, once the
//! notes queued for it are drained. It holds because the journal's one way of
//! recording a write (`Journal::note`) queues the page here in the same
//! critical section; a ledger is registered before its copy is shared (a copy
//! never taken needs no entry: all its pages are stale by `copied == 0`); the
//! entry is removed only by a refresh that took the page under an asking at
//! least as new as the note (`settle`, under the copy's `copied` lock). A
//! ledger may hold more than the journal calls stale (pages a copy took in
//! parts, a note that is older than the copy's own taking): such entries are
//! looked at once more and removed by the refresh that finds them current.
//!
//! Modes (`VICTORY_PAGE_REFRESH`, flip-safe: the ledgers are kept whenever the
//! variable is named, at the start or in `VICTORY_AB`; only what reads them
//! changes):
//! - 0: off. The old walk decides; the ledgers are kept up to date if named.
//! - 1: shadow. The old walk decides; each look and each refresh also asks
//!   the ledger and counts the disagreements. `missed` (the journal says
//!   stale, the ledger does not) must stay 0: it is the stale byte that would
//!   reach the card. `extra` is the ledger holding more than needed.
//! - 2: driven. The ledger decides which pages are stale and whether a look
//!   finds nothing new; the journal is asked only whether its asking covers
//!   the range (what a look needs to be valid), never walked.

use std::collections::{BTreeMap, HashMap};
use std::sync::atomic::{
    AtomicU64, AtomicUsize,
    Ordering::{Acquire, Relaxed, Release},
};
use std::sync::{Arc, Mutex, RwLock};

use crate::host::AddressHash;
use crate::watch::PAGE;

/// Pages from which a copy is found by its range, not page by page in the index.
const BIG: usize = 4096;
/// Notes queued before the thread that adds one applies them.
const HELD: usize = 1 << 14;

/// Whether the ledgers are kept (`VICTORY_PAGE_REFRESH` set to something but 0, or named in `VICTORY_AB`).
pub(crate) fn tracking() -> bool {
    if cfg!(test) {
        return true;
    }
    static ON: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ON.get_or_init(|| {
        std::env::var("VICTORY_PAGE_REFRESH").is_ok_and(|v| !matches!(v.trim(), "" | "0"))
            || crate::ab::bit("VICTORY_PAGE_REFRESH").is_some()
    })
}

/// The mode (see the module): 0 off, 1 shadow, 2 driven. May change while the title runs (`VICTORY_AB`); each
/// decision reads it once.
pub(crate) fn mode() -> u8 {
    #[cfg(test)]
    {
        if let Some(forced) = TEST_MODE.with(|m| m.get()) {
            return forced;
        }
    }
    if !tracking() {
        return 0;
    }
    fn of_text(text: Option<&str>) -> u8 {
        match text.map(str::trim) {
            Some("1") => 1,
            Some("2") => 2,
            _ => 0,
        }
    }
    crate::ab::live_value!("VICTORY_PAGE_REFRESH", u8, of_text)
}

#[cfg(test)]
thread_local! {
    static TEST_MODE: std::cell::Cell<Option<u8>> = const { std::cell::Cell::new(None) };
}

/// A test's mode, for its thread only.
#[cfg(test)]
pub(crate) fn force_mode(mode: Option<u8>) {
    TEST_MODE.with(|m| m.set(mode));
}

/// What a copy has to take again: the pages noted written since it took them.
pub(crate) struct Ledger {
    /// The pages (title address / 64 KiB) the copy covers: `[first, end)`.
    first: usize,
    end: usize,
    /// The copy's own number.
    id: u64,
    /// Page -> the highest asking number that noted it, until a refresh that took it under an asking at least as new.
    dirty: Mutex<BTreeMap<usize, u64>>,
    /// How many pages `dirty` holds (read without the lock).
    count: AtomicUsize,
}

impl Ledger {
    pub(crate) fn new(first: usize, end: usize, id: u64) -> Arc<Ledger> {
        Arc::new(Ledger {
            first,
            end,
            id,
            dirty: Mutex::new(BTreeMap::new()),
            count: AtomicUsize::new(0),
        })
    }

    fn dirty(&self) -> std::sync::MutexGuard<'_, BTreeMap<usize, u64>> {
        self.dirty.lock().unwrap_or_else(|e| e.into_inner())
    }

    /// Page `page` was noted written by asking `now`. Whether the copy covers it.
    fn mark(&self, page: usize, now: u64) -> bool {
        if page < self.first || page >= self.end {
            return false;
        }
        let mut dirty = self.dirty();
        let was = dirty.insert(page, now).filter(|&w| w > now);
        if let Some(w) = was {
            // (Numbers only grow; an older note after a newer one keeps the newer.)
            dirty.insert(page, w);
        }
        self.count.store(dirty.len(), Release);
        true
    }

    /// How many pages are held.
    pub(crate) fn len(&self) -> usize {
        self.count.load(Acquire)
    }

    /// The pages `[first, last)` that are held, with their numbers, in page order: the shape of the journal's list,
    /// of the pages that matter.
    pub(crate) fn dirty_in(&self, first: usize, last: usize) -> Vec<(usize, u64)> {
        if self.len() == 0 || first >= last {
            return Vec::new();
        }
        self.dirty()
            .range(first..last)
            .map(|(&p, &w)| (p, w))
            .collect()
    }

    /// Whether a page of `[first, last)` was noted after the copy took it: `taken` holds, for each page from `first`,
    /// the asking after which the copy took it (0: never), and `ranged` the asking after which a range that holds them
    /// all was taken whole (0: none). The journal's rule, over the pages held.
    pub(crate) fn stale_in(&self, first: usize, last: usize, taken: &[u64], ranged: u64) -> bool {
        if self.len() == 0 || first >= last {
            return false;
        }
        self.dirty()
            .range(first..last)
            .any(|(&page, &w)| w > taken.get(page - first).copied().unwrap_or(0).max(ranged))
    }

    /// Whether a page of `[first, last)` was noted by an asking after `bound`.
    pub(crate) fn any_after(&self, first: usize, last: usize, bound: u64) -> bool {
        if self.len() == 0 || first >= last {
            return false;
        }
        self.dirty().range(first..last).any(|(_, &w)| w > bound)
    }

    /// The pages the copy took just now: each page with the asking it took it under. A page noted by that asking or an
    /// earlier one is current and goes; one noted after it stays.
    pub(crate) fn settle(&self, taken: &[(usize, u64)]) {
        if taken.is_empty() || self.len() == 0 {
            return;
        }
        let mut dirty = self.dirty();
        let mut gone = 0u64;
        for &(page, bound) in taken {
            if dirty.get(&page).is_some_and(|&w| w <= bound) {
                dirty.remove(&page);
                gone += 1;
            }
        }
        self.count.store(dirty.len(), Release);
        drop(dirty);
        if gone > 0 {
            count(CLEARED, gone);
        }
    }

    /// The number noted for `page`, if it is held.
    #[cfg(test)]
    pub(crate) fn noted(&self, page: usize) -> Option<u64> {
        self.dirty().get(&page).copied()
    }
}

#[derive(Default)]
struct Index {
    /// Page -> the copies that cover it (copies under [`BIG`] pages).
    pages: HashMap<usize, Vec<Arc<Ledger>>, std::hash::BuildHasherDefault<AddressHash>>,
    big: Vec<Arc<Ledger>>,
    /// All the registered ledgers, by their copy's number.
    all: HashMap<u64, Arc<Ledger>, std::hash::BuildHasherDefault<AddressHash>>,
}

impl Index {
    fn mark(&self, page: usize, now: u64) -> u64 {
        let mut marked = 0;
        if let Some(ledgers) = self.pages.get(&page) {
            marked += ledgers.iter().filter(|l| l.mark(page, now)).count() as u64;
        }
        marked + self.big.iter().filter(|l| l.mark(page, now)).count() as u64
    }
}

/// The pages the journal noted, the copies that cover them and the way from the one to the other.
pub(crate) struct Plane {
    /// (page, asking) as the journal noted them, until applied.
    noted: Mutex<Vec<(usize, u64)>>,
    /// Notes queued or being applied: a look that finds none may rely on the ledgers.
    held: AtomicUsize,
    /// One thread applies notes at a time, and the others wait for it.
    draining: Mutex<()>,
    index: RwLock<Index>,
}

/// The one of the process.
static PLANE: Plane = Plane::new();

impl Plane {
    pub(crate) const fn new() -> Plane {
        Plane {
            noted: Mutex::new(Vec::new()),
            held: AtomicUsize::new(0),
            draining: Mutex::new(()),
            index: RwLock::new(Index {
                pages: HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
                big: Vec::new(),
                all: HashMap::with_hasher(std::hash::BuildHasherDefault::new()),
            }),
        }
    }

    /// The copy of `ledger` exists: notes from now on reach it.
    pub(crate) fn register(&self, ledger: &Arc<Ledger>) {
        let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
        if index.all.insert(ledger.id, ledger.clone()).is_some() {
            return;
        }
        if ledger.end - ledger.first > BIG {
            index.big.push(ledger.clone());
            return;
        }
        for page in ledger.first..ledger.end {
            index.pages.entry(page).or_default().push(ledger.clone());
        }
    }

    /// The copy is gone (or is not a copy any more).
    pub(crate) fn retire(&self, ledger: &Ledger) {
        let mut index = self.index.write().unwrap_or_else(|e| e.into_inner());
        if index.all.remove(&ledger.id).is_none() {
            return;
        }
        if ledger.end - ledger.first > BIG {
            index.big.retain(|l| l.id != ledger.id);
            return;
        }
        for page in ledger.first..ledger.end {
            if let Some(ledgers) = index.pages.get_mut(&page) {
                ledgers.retain(|l| l.id != ledger.id);
                if ledgers.is_empty() {
                    index.pages.remove(&page);
                }
            }
        }
    }

    /// The journal noted `page` as written by asking `now` (called with the journal held: the note and the queue's
    /// entry are one step).
    pub(crate) fn note(&self, page: usize, now: u64, alias: &dyn Fn(usize) -> Vec<usize>) {
        let queued = {
            let mut noted = self.noted.lock().unwrap_or_else(|e| e.into_inner());
            noted.push((page, now));
            self.held.fetch_add(1, Release);
            noted.len()
        };
        if queued >= HELD {
            self.drain(alias);
        }
    }

    /// Applies the notes queued: each page is told to the copies that cover it, and to those that cover the other
    /// views of its physical page (`alias`: the pages the title shows the same physical page at). After it returns,
    /// every note the journal made before the call is in the ledgers.
    pub(crate) fn drain(&self, alias: &dyn Fn(usize) -> Vec<usize>) {
        if self.held.load(Acquire) == 0 {
            return;
        }
        let _one = self.draining.lock().unwrap_or_else(|e| e.into_inner());
        let notes = std::mem::take(&mut *self.noted.lock().unwrap_or_else(|e| e.into_inner()));
        if notes.is_empty() {
            return;
        }
        let (mut marks, mut aliased) = (0u64, 0u64);
        {
            let index = self.index.read().unwrap_or_else(|e| e.into_inner());
            for &(page, now) in &notes {
                marks += index.mark(page, now);
                for other in alias(page) {
                    if other != page {
                        let n = index.mark(other, now);
                        marks += n;
                        aliased += n;
                    }
                }
            }
        }
        // Applied: a look that sees none held may rely on the ledgers.
        self.held.fetch_sub(notes.len(), Release);
        count(NOTES, notes.len() as u64);
        count(MARKS, marks);
        count(ALIASED, aliased);
    }

    /// The copies with a page held and the pages held, now.
    pub(crate) fn gauge(&self) -> (u64, u64) {
        let index = self.index.read().unwrap_or_else(|e| e.into_inner());
        index
            .all
            .values()
            .map(|l| l.len() as u64)
            .filter(|&n| n > 0)
            .fold((0, 0), |(c, p), n| (c + 1, p + n))
    }

    /// How many ledgers are registered.
    #[cfg(test)]
    pub(crate) fn registered(&self) -> usize {
        self.index
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .all
            .len()
    }
}

/// The pages the title shows `page`'s physical page at, other than `page` itself.
fn views_of(page: usize) -> Vec<usize> {
    crate::inplace::pages_of(page * PAGE)
        .into_iter()
        .map(|at| at / PAGE)
        .filter(|&other| other != page)
        .collect()
}

/// The journal's note, told to the copies ([`Plane::note`]).
pub(crate) fn noted(page: usize, now: u64) {
    if tracking() {
        // (Under the journal's lock: no look at the views here. A note applied by `drain` finds the other views.)
        PLANE.note(page, now, &|_| Vec::new());
    }
}

/// Every note made so far is in the ledgers.
pub(crate) fn drain() {
    if !tracking() {
        return;
    }
    let aliases = crate::inplace::any_aliases();
    PLANE.drain(&|p| if aliases { views_of(p) } else { Vec::new() });
}

/// A copy of title memory `[start, start + len)` is made: its ledger.
pub(crate) fn ledger_of(start: u64, len: u64, id: u64) -> Arc<Ledger> {
    let first = start as usize / PAGE;
    let end = (start + len).div_ceil(PAGE as u64) as usize;
    let ledger = Ledger::new(first, end.max(first), id);
    if tracking() {
        PLANE.register(&ledger);
    }
    ledger
}

/// The copy is gone.
pub(crate) fn retire(ledger: &Ledger) {
    if tracking() {
        PLANE.retire(ledger);
    }
}

/// The pages of a refresh of `[from, to)` as the driven path gives them: the ledger's, drained first.
pub(crate) fn written(ledger: &Ledger, from: usize, to: usize) -> Vec<(usize, u64)> {
    drain();
    count(LISTED, 1);
    let list = ledger.dirty_in(from / PAGE, to.div_ceil(PAGE));
    count(LISTED_PAGES, list.len() as u64);
    list
}

/// [`crate::watch::nothing_new`] by the ledger (mode 2): nothing of `[start, end)` was written that the copy does not
/// hold, as its asking covers the range (`covered`), every page of it was taken once or a range that holds it whole was
/// (`ranged`), and no page the ledger holds there is newer than the copy's.
pub(crate) fn nothing_new(
    ledger: &Ledger,
    start: usize,
    end: usize,
    base: usize,
    copied: &[u64],
    ranged: u64,
) -> bool {
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    if !crate::watch::covered(start, end) {
        return false;
    }
    let Some(taken) = first
        .checked_sub(base)
        .and_then(|from| copied.get(from..last - base))
    else {
        return false;
    };
    if !(ranged != 0 || taken.iter().all(|&at| at != 0)) {
        return false;
    }
    drain();
    count(ANSWERED, 1);
    !ledger.stale_in(first, last, taken, ranged)
}

/// Mode 1: a look at `[start, end)` of a copy (the caller holds the copy's `copied`): the journal's answer, read first,
/// against the ledger's, drained after it.
pub(crate) fn check_look(
    ledger: &Ledger,
    start: usize,
    end: usize,
    base: usize,
    copied: &[u64],
    ranged: u64,
) {
    let journal = crate::watch::journal_stale(start, end, base, copied, ranged);
    drain();
    let (first, last) = (start / PAGE, end.div_ceil(PAGE));
    let held = first
        .checked_sub(base)
        .and_then(|from| copied.get(from..last - base))
        .is_some_and(|taken| ledger.stale_in(first, last, taken, ranged));
    compare_look(journal, held, (start, end));
}

/// Mode 1: the journal's answer to a look against the ledger's (`journal_stale`: a page of the range was written after
/// the copy took it, by the journal; `ledger_stale`: the same, by the ledger, drained after the journal was read).
pub(crate) fn compare_look(journal_stale: bool, ledger_stale: bool, at: (usize, usize)) {
    count(LOOKS, 1);
    if journal_stale && !ledger_stale {
        count(LOOK_MISSED, 1);
        missed("a look", at);
    } else if !journal_stale && ledger_stale {
        count(LOOK_EXTRA, 1);
    }
}

/// Mode 1: of a refresh of `[from, to)`, the pages the journal says to take again (`journal`) against those the ledger
/// holds (`held`, with their numbers) among them, in page order.
pub(crate) fn compare_refresh(journal: &[usize], held: &[(usize, u64)], at: (usize, usize)) {
    count(LISTS, journal.len() as u64);
    for page in journal {
        if held.binary_search_by_key(page, |&(p, _)| p).is_err() {
            count(LIST_MISSED, 1);
            missed("a refresh", (*page * PAGE, *page * PAGE + PAGE));
        }
    }
    let _ = at;
}

/// Mode 1: pages the ledger holds that the refresh found current: counted, and removed by the caller's `settle`.
pub(crate) fn extra(pages: u64) {
    if pages > 0 {
        count(LIST_EXTRA, pages);
    }
}

/// Mode 1: of the declared ranges of flights, the pages the journal says are stale against the ledger's.
pub(crate) fn compare_under(journal: &[usize], ledger: &[usize], at: (usize, usize)) {
    count(UNDERS, journal.len() as u64);
    for page in journal {
        if ledger.binary_search(page).is_err() {
            count(UNDER_MISSED, 1);
            missed("a flight's range", (*page * PAGE, *page * PAGE + PAGE));
        }
    }
    let _ = at;
}

fn missed(what: &str, at: (usize, usize)) {
    // (The first few say where; the line's count says how many.)
    if STATS[LOOK_MISSED].load(Relaxed)
        + STATS[LIST_MISSED].load(Relaxed)
        + STATS[UNDER_MISSED].load(Relaxed)
        <= 8
    {
        tracing::warn!(
            "page refresh: {what} would have left {:#x}..{:#x} stale: the journal says it was written and the copy's ledger does not hold it",
            at.0,
            at.1
        );
    }
}

/// Of a step that refreshes a copy: whether its ledger holds nothing (the walk could leave it out with one load).
pub(crate) fn step_seen(ledger: &Ledger) {
    if !tracking() {
        return;
    }
    count(STEPS, 1);
    if ledger.len() == 0 {
        count(STEPS_CLEAN, 1);
    }
}

const NOTES: usize = 0;
const MARKS: usize = 1;
const ALIASED: usize = 2;
const LOOKS: usize = 3;
const LOOK_MISSED: usize = 4;
const LOOK_EXTRA: usize = 5;
const LISTS: usize = 6;
const LIST_MISSED: usize = 7;
const LIST_EXTRA: usize = 8;
const UNDERS: usize = 9;
const UNDER_MISSED: usize = 10;
const ANSWERED: usize = 11;
const LISTED: usize = 12;
const LISTED_PAGES: usize = 13;
const STEPS: usize = 14;
const STEPS_CLEAN: usize = 15;
const CLEARED: usize = 16;
const GATES: usize = 17;
const GATE_HITS: usize = 18;
const GATE_MISSED: usize = 19;

/// Counters; those of the shadow check are cumulative in the line, the others per frame.
static STATS: [AtomicU64; 20] = [const { AtomicU64::new(0) }; 20];

fn count(what: usize, n: u64) {
    STATS[what].fetch_add(n, Relaxed);
}

/// What the shadow check has found since the start: (looks missed, pages of refreshes missed, pages under flights
/// missed). All 0 when the ledger holds what the journal calls stale.
pub(crate) fn missed_total() -> [u64; 4] {
    [LOOK_MISSED, LIST_MISSED, UNDER_MISSED, GATE_MISSED].map(|i| STATS[i].load(Relaxed))
}

/// Mode 1: the ledger's gate said a span needs no look (`stale`: the journal says a page of it was written after the
/// copy took it, which would be a stale byte left behind).
pub(crate) fn gate_checked(stale: bool) {
    count(GATES, 1);
    if stale {
        count(GATE_MISSED, 1);
        missed("the gate", (0, 0));
    }
}

/// Mode 2: a span left out by the gate.
pub(crate) fn gate_hit() {
    count(GATE_HITS, 1);
}

/// What the shadow check has compared since the start (looks, pages of refreshes, pages under flights).
#[cfg(test)]
pub(crate) fn compared_total() -> u64 {
    [LOOKS, LISTS, UNDERS]
        .iter()
        .map(|&i| STATS[i].load(Relaxed))
        .sum()
}

/// What the ledger has decided since the start (looks answered, refreshes listed).
#[cfg(test)]
pub(crate) fn driven_total() -> u64 {
    [ANSWERED, LISTED]
        .iter()
        .map(|&i| STATS[i].load(Relaxed))
        .sum()
}

/// The frame line.
pub(crate) fn line(frames: f64) {
    if !tracking() {
        return;
    }
    let per = |i: usize| STATS[i].swap(0, Relaxed) as f64 / frames;
    let (copies, pages) = PLANE.gauge();
    let [looks_missed, lists_missed, unders_missed, gates_missed] = missed_total();
    let (looks, lists, unders, extras) = (
        per(LOOKS),
        per(LISTS),
        per(UNDERS),
        per(LOOK_EXTRA) + per(LIST_EXTRA),
    );
    tracing::info!(
        "page-driven refresh (VICTORY_PAGE_REFRESH={}), per frame: {:.0} pages noted written, {:.0} marks in copies ({:.0} through other views of the page); \
         ledger answered {:.0} looks and listed {:.0} pages for {:.0} refreshes; shadow check: {looks:.0} looks, {lists:.0} pages of refreshes, {unders:.0} pages under flights compared, \
         {extras:.1} extra; MISSED so far: {looks_missed} looks, {lists_missed} pages of refreshes, {unders_missed} pages under flights, {gates_missed} spans the gate would have left out (all must be 0); \
         the gate: {:.0} spans checked against the journal (shadow), {:.0} left out (driven); \
         {:.0} entries cleared; {:.0} of {:.0} refresh steps are of copies that hold nothing; now {copies} copies hold {pages} pages",
        mode(),
        per(NOTES),
        per(MARKS),
        per(ALIASED),
        per(ANSWERED),
        per(LISTED_PAGES),
        per(LISTED),
        per(GATES),
        per(GATE_HITS),
        per(CLEARED),
        per(STEPS_CLEAN),
        per(STEPS),
    );
}

#[cfg(test)]
mod tests {
    use super::{Ledger, Plane};
    use std::collections::BTreeMap;
    use std::sync::Arc;

    fn random(seed: &mut u64) -> usize {
        *seed ^= *seed << 13;
        *seed ^= *seed >> 7;
        *seed ^= *seed << 17;
        (*seed >> 8) as usize
    }

    /// A copy of the model (`Model`): the pages it covers and the asking after which it took each (0: never).
    struct Model {
        ledger: Arc<Ledger>,
        first: usize,
        end: usize,
        copied: Vec<u64>,
        live: bool,
    }

    /// The journal's rule for the pages of `[from, to)` that a refresh takes again, and the ledger's; the ledger's
    /// must hold every page of the first that the copy took before.
    fn stale_by_journal(
        c: &Model,
        journal: &BTreeMap<usize, u64>,
        from: usize,
        to: usize,
    ) -> Vec<usize> {
        (from..to)
            .filter(|&p| {
                c.copied[p - c.first] != 0
                    && journal.get(&p).is_some_and(|&w| w > c.copied[p - c.first])
            })
            .collect()
    }

    /// Random notes, copies made and retired, refreshes of random ranges and pages shown at other addresses: the
    /// pages a ledger holds are always a superset of those the journal says stale, and a refresh leaves a copy
    /// exactly as the journal's rule does.
    #[test]
    fn a_ledger_holds_every_page_the_journal_calls_stale_through_creation_retirement_and_aliases() {
        for round in 0..40u64 {
            let mut seed = 0x9e37_79b9_7f4a_7c15u64 ^ (round * 0x1234_5679);
            let plane = Plane::new();
            let mut journal: BTreeMap<usize, u64> = BTreeMap::new();
            let mut copies: Vec<Model> = Vec::new();
            let mut now = 0u64;
            let mut next_id = 1u64;
            // Pages 8..16 are another view of pages 0..8, in page order.
            let alias = |p: usize| {
                if p < 8 {
                    vec![p + 8]
                } else if p < 16 {
                    vec![p - 8]
                } else {
                    Vec::new()
                }
            };
            for _ in 0..400 {
                match random(&mut seed) % 8 {
                    0 | 1 => {
                        // A copy of a random range (some of them big).
                        let first = random(&mut seed) % 20;
                        let big = random(&mut seed) % 5 == 0;
                        let end = first + 1 + random(&mut seed) % if big { 6000 } else { 12 };
                        let ledger = Ledger::new(first, end, next_id);
                        next_id += 1;
                        plane.register(&ledger);
                        copies.push(Model {
                            ledger,
                            first,
                            end,
                            copied: vec![0; end - first],
                            live: true,
                        });
                    }
                    2 => {
                        if let Some(c) = copies
                            .iter_mut()
                            .filter(|c| c.live)
                            .nth(random(&mut seed) % 3)
                        {
                            plane.retire(&c.ledger);
                            c.live = false;
                        }
                    }
                    3..=5 => {
                        // The title writes some pages; an asking finds them (all that were written since the last).
                        now += 1;
                        for _ in 0..1 + random(&mut seed) % 4 {
                            let page = random(&mut seed) % 20;
                            // The journal notes the page and, as `wrote` does, its other views.
                            journal.insert(page, now);
                            plane.note(page, now, &alias);
                            for other in alias(page) {
                                journal.insert(other, now);
                            }
                        }
                        if random(&mut seed) % 2 == 0 {
                            plane.drain(&alias);
                        }
                    }
                    _ => {
                        // A refresh of a random range of a random live copy.
                        let live: Vec<usize> =
                            (0..copies.len()).filter(|&i| copies[i].live).collect();
                        if live.is_empty() {
                            continue;
                        }
                        let c = &mut copies[live[random(&mut seed) % live.len()]];
                        let from = c.first + random(&mut seed) % (c.end - c.first);
                        let to = (from + 1 + random(&mut seed) % 8).min(c.end);
                        now += 1;
                        plane.drain(&alias);
                        let by_journal = stale_by_journal(c, &journal, from, to);
                        let held: Vec<usize> = c
                            .ledger
                            .dirty_in(from, to)
                            .into_iter()
                            .map(|(p, _)| p)
                            .collect();
                        for p in &by_journal {
                            assert!(
                                held.contains(p),
                                "round {round}: page {p} is stale by the journal and not in the ledger"
                            );
                        }
                        // The refresh takes every page of the range (whole pages) under `now`, as `copy_stale` does.
                        let taken: Vec<(usize, u64)> = (from..to).map(|p| (p, now)).collect();
                        for p in from..to {
                            c.copied[p - c.first] = now;
                        }
                        c.ledger.settle(&taken);
                        // Nothing the journal knew is stale in the range now, and the ledger holds none of the range.
                        assert!(stale_by_journal(c, &journal, from, to).is_empty());
                        assert!(c.ledger.dirty_in(from, to).is_empty(), "round {round}");
                    }
                }
            }
            // At the end every live copy's stale pages are held.
            plane.drain(&alias);
            for c in copies.iter().filter(|c| c.live) {
                let by_journal = stale_by_journal(c, &journal, c.first, c.end);
                for p in by_journal {
                    assert!(
                        c.ledger.noted(p).is_some(),
                        "round {round}: page {p} stale at the end and not held"
                    );
                }
            }
            assert_eq!(plane.registered(), copies.iter().filter(|c| c.live).count());
        }
    }

    /// A note made while the copy is being refreshed (a newer asking than the refresh's) stays; an older one goes.
    #[test]
    fn a_note_newer_than_the_refresh_stays_and_an_older_one_goes() {
        let plane = Plane::new();
        let ledger = Ledger::new(10, 20, 1);
        plane.register(&ledger);
        plane.note(12, 5, &|_| Vec::new());
        plane.note(13, 9, &|_| Vec::new());
        plane.drain(&|_| Vec::new());
        assert_eq!((ledger.noted(12), ledger.noted(13)), (Some(5), Some(9)));
        // A refresh under asking 7 took pages 12 and 13.
        ledger.settle(&[(12, 7), (13, 7)]);
        assert_eq!((ledger.noted(12), ledger.noted(13)), (None, Some(9)));
        // A later note of 12 comes back; an older one after a newer one does not lower it.
        plane.note(12, 11, &|_| Vec::new());
        plane.note(12, 8, &|_| Vec::new());
        plane.drain(&|_| Vec::new());
        assert_eq!(ledger.noted(12), Some(11));
        assert_eq!(ledger.len(), 2);
    }

    /// A page outside the copy is not held, whatever route the note takes (a view, a big copy by range).
    #[test]
    fn notes_reach_only_the_copies_that_cover_the_page() {
        let plane = Plane::new();
        let small = Ledger::new(100, 104, 1);
        let big = Ledger::new(0, 10_000, 2);
        let other = Ledger::new(200, 204, 3);
        for l in [&small, &big, &other] {
            plane.register(l);
        }
        plane.note(101, 3, &|p| if p == 101 { vec![201] } else { Vec::new() });
        plane.note(5000, 4, &|_| Vec::new());
        plane.note(9999, 5, &|_| Vec::new());
        plane.note(10_000, 6, &|_| Vec::new());
        plane.drain(&|p| if p == 101 { vec![201] } else { Vec::new() });
        assert_eq!(small.noted(101), Some(3));
        assert_eq!(
            other.noted(201),
            Some(3),
            "through the other view of the physical page"
        );
        assert_eq!(big.noted(101), Some(3));
        assert_eq!(big.noted(5000), Some(4));
        assert_eq!(big.noted(9999), Some(5));
        assert_eq!(big.noted(10_000), None);
        assert_eq!(small.len(), 1);
        assert_eq!(other.len(), 1);
        plane.retire(&big);
        plane.retire(&small);
        plane.note(101, 7, &|_| Vec::new());
        plane.drain(&|_| Vec::new());
        assert_eq!(
            small.noted(101),
            Some(3),
            "a retired copy is told nothing more"
        );
    }

    /// Several threads note and drain together: no note is lost, and a thread that drains after its own notes finds
    /// them in the ledger, whichever thread applied them.
    #[test]
    fn notes_are_in_the_ledger_when_the_drain_of_the_thread_that_made_them_returns() {
        let plane = Arc::new(Plane::new());
        let ledger = Ledger::new(0, 4096, 1);
        plane.register(&ledger);
        let handles: Vec<_> = (0..6u64)
            .map(|t| {
                let (plane, ledger) = (plane.clone(), ledger.clone());
                std::thread::spawn(move || {
                    for k in 0..2000u64 {
                        let page = (t * 2000 + k) as usize % 4096;
                        let now = 1 + t * 10_000 + k;
                        plane.note(page, now, &|_| Vec::new());
                        if k % 7 == 0 {
                            plane.drain(&|_| Vec::new());
                            assert!(
                                ledger.noted(page).is_some_and(|w| w >= 1),
                                "the note of page {page} is not in the ledger after the drain"
                            );
                        }
                    }
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }
        plane.drain(&|_| Vec::new());
        assert_eq!(plane.gauge().0, 1);
    }
}
