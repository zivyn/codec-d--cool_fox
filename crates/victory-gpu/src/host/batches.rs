//! A submission's refreshes of host copies, merged per copy and flushed (`Batch`), and the spans it works out.

use super::*;

/// Whether a title buffer copy that continues the one recorded before it
/// in its list becomes one step with it (`VICTORY_JOIN_COPIES=1`; off by
/// default).
///
/// Each copy is done at submission through the kernel, which reports a
/// page that is not there instead of faulting (`copy_mapped`), tells the
/// write watch and the guards, and ends the refreshes gathered so far: a
/// call into the kernel for some 480 bytes, 14,042 times a frame in run 86
/// (6.8 MB; "moves 19 ms" with the refreshes' own reads). How many of them
/// continue the one before is counted either way and said on its own line
/// with the frame rate; the switch is worth that many kernel calls.
///
/// Joined, the same bytes move: [`continues`] joins only copies whose
/// ranges, taken together, do not overlap, and nothing is recorded between
/// two steps that follow each other. What differs: a page that is not
/// mapped now fails the joined copy as a whole (it then goes page by page,
/// zeros for what cannot be read, as a single copy does), where before
/// only the copy that held it did.
pub(super) fn join_copies() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| crate::defaults::var_os("VICTORY_JOIN_COPIES").is_some_and(|v| v == "1"))
}

/// Since the last frame-rate line: title buffer copies recorded, and those
/// that continue the one before ([`join_copies`]).
pub(super) static COPIES_JOINED: [std::sync::atomic::AtomicU64; 2] = [const { std::sync::atomic::AtomicU64::new(0) }; 2];

/// The bytes the refreshes among `touches` refer to, each byte once per copy
/// (what a span per copy, from the lowest to the highest, is far wider than),
/// and how many refreshes there are.
pub(super) fn referenced_bytes(touches: &[Touch]) -> (u64, u64) {
    let mut ranges: Vec<(usize, usize, usize)> =
        touches.iter().filter_map(|t| if let Touch::Refresh(copy, from, to) = t { Some((*copy, *from, *to)) } else { None }).collect();
    let count = ranges.len() as u64;
    ranges.sort_unstable();
    let (mut bytes, mut open): (u64, Option<(usize, usize, usize)>) = (0, None);
    for (copy, from, to) in ranges {
        match &mut open {
            Some((c, _, end)) if *c == copy && from <= *end => *end = (*end).max(to),
            _ => {
                if let Some((_, a, b)) = open.take() {
                    bytes += (b - a) as u64;
                }
                open = Some((copy, from, to));
            }
        }
    }
    if let Some((_, a, b)) = open {
        bytes += (b - a) as u64;
    }
    (bytes, count)
}

/// Whether the spans of a flush that are left go to the threads longest first
/// (`VICTORY_REFRESH_LPT=1`, off by default): the last piece of work that a
/// thread begins is a short one, and the submitting thread, which waits for
/// all of them, waits less.
pub(super) fn refresh_lpt() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REFRESH_LPT").is_some_and(|v| v == "1"))
}

/// `spans` (anything with a range) with the longest first; equal ones in the
/// order they were.
pub(super) fn longest_first<T>(spans: &mut [(T, usize, usize)]) {
    spans.sort_by_key(|s| std::cmp::Reverse(s.2 - s.1));
}

/// Whether the copy `next` (destination, source, bytes) goes on exactly
/// where `last` ended, source and destination both, and the two as one do
/// not overlap (source over destination): then one copy of both moves the
/// bytes the two would move in turn.
pub(super) fn continues(last: (usize, usize, usize), next: (usize, usize, usize)) -> bool {
    let ((d, s, n), (dst, src, len)) = (last, next);
    let Some(all) = n.checked_add(len) else {
        return false;
    };
    d.checked_add(n) == Some(dst)
        && s.checked_add(n) == Some(src)
        && (d.checked_add(all).is_some_and(|end| end <= s) || s.checked_add(all).is_some_and(|end| end <= d))
}

/// Whether a submission's refreshes of host copies run on several threads,
/// a copy to a thread (`VICTORY_WIDE_REFRESH=0` disables it; `=1` forces it).
///
/// A submission brings some sixty copies up to date, one after the other
/// on the submitting thread: for each, the pages the title wrote are asked
/// for and read through the kernel into the copy. That is the frame line's
/// "copies", 45-50 ms a frame, and the copies have nothing to do with one
/// another: each refresh writes its own copy and its own notes, and reads
/// title memory. With the switch the copies of a flush (four or more) are
/// spread over the threads kept for the snapshots (`crate::shadow::wide`).
/// The write-watch journal and the guards are behind their own locks, as
/// they are for the title's threads.
pub(super) fn wide_refresh() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| {
        wide_refresh_enabled(
            std::env::var("VICTORY_WIDE_REFRESH").ok().as_deref(),
            std::thread::available_parallelism().ok().map(|n| n.get()),
        )
    })
}

pub(super) fn wide_refresh_enabled(setting: Option<&str>, logical_cpus: Option<usize>) -> bool {
    match setting {
        Some("1") => true,
        Some("0") => false,
        _ => logical_cpus.is_some_and(|n| n >= 4),
    }
}

/// A submission's refreshes not done yet, merged per buffer copy into the
/// range that covers them: done before a step that writes title memory or
/// gives a range to shaders, and before the lists run.
///
/// Also the textures brought up to date in the submission so far: most
/// lists of a world submission sample the same textures, and a texture in
/// physical-page memory is hashed whole each time it is checked. Checked
/// once, it is checked again only after a step that writes title memory.
#[derive(Default)]
pub(super) struct Batch {
    pub(super) refreshes: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
    /// Where each copy (by address) is in `refreshes`: the list that makes
    /// the heap's buffers ready adds some 4,100 copies, and searching them
    /// for every refresh of a submission was tens of millions of
    /// comparisons a frame.
    pub(super) index: ByCopy<usize>,
    /// Staging buffers (by address) refreshed since the last title write,
    /// or due to be.
    pub(super) textures: std::collections::HashSet<usize>,
    /// Those due ([`Batch::refresh_textures`]).
    pub(super) due: Vec<std::sync::Arc<crate::texture::Staging>>,
    /// What copies at their place in the submission's lists put into title
    /// memory ([`place`]): the host copy, the title address, the bytes.
    pub(super) placed: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, Vec<u8>)>,
    /// Fills not done yet (destination, source, bytes): [`wide_fills`].
    pub(super) fills: Vec<(usize, usize, usize)>,
    /// Copies (by address) brought up to date ahead of the steps, and the
    /// ranges (from, to) ([`Batch::refresh_ahead`]).
    pub(super) ahead: ByCopy<Vec<(usize, usize)>>,
    /// With [`narrow_on`]: what each copy's pending refresh refers to (its
    /// entry in `refreshes` is from the lowest to the highest), until it is done.
    pub(super) parts: ByCopy<Vec<(usize, usize)>>,
    /// `ahead` holds everything the submission refreshes
    /// ([`Batch::refresh_front`]).
    pub(super) front: bool,
    /// With [`lean_steps`] (read once, when the submission begins): what is
    /// refreshed ahead is kept in `first` and `rest`, not `ahead`, and the copy
    /// looked up last is remembered.
    pub(super) lean: bool,
    /// With `lean`: the first range put ahead for each copy (nearly always
    /// the only one: a table of ranges, and its allocation, for each of
    /// thousands of copies a submission is what this leaves out) ...
    pub(super) first: ByCopy<(usize, usize)>,
    /// ... and the others of the copies that have more.
    pub(super) rest: ByCopy<Vec<(usize, usize)>>,
    /// The copy `add` looked at last (0: none), and what is ahead for it:
    /// `Some((from, to))` if one range (none at all: the empty range), else
    /// `None` (several: the table is looked at). Forgotten whenever what is
    /// ahead changes.
    pub(super) memo: (usize, Option<(usize, usize)>),
    /// A range was put into `ahead` that does not begin past the end of the one
    /// before it in its copy's list: the lists are not looked through by
    /// bisection.
    pub(super) unsorted: bool,
    /// With a pass over the steps ([`onepass`], `pass_used`): what is refreshed ahead, by the copy's number in the pass,
    /// in place of `first`, `rest` and `memo`.
    pub(super) ahead_idx: onepass::Ahead,
    pub(super) pass_used: bool,
}

/// Whether the copies that a submission's shaders write are brought up to
/// date together, on several threads, ahead of its steps
/// (`VICTORY_REFRESH_AHEAD=0`: each before the step that gives it to them).
///
/// 580 copies a frame, one after the other on the submitting thread, 3.4 to
/// 6.5 ms (session play-1125-0203).
pub(super) fn ahead_of_writes() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REFRESH_AHEAD").is_none_or(|v| v != "0") && wide_refresh() && crate::watch::light())
}

/// Whether everything a submission refreshes is brought up to date in one go,
/// on several threads, ahead of its steps (`VICTORY_REFRESH_FRONT=1`, and with
/// `VICTORY_JOIN_PARTS`, which needs it; else in groups where the steps ask
/// for it). Measured on its own at nothing (run submit-1: refreshes 17.1 ms a
/// frame without, 18.3 with).
///
/// A submission's refreshes were done in up to five groups one after the other
/// (the copies its shaders write, those between its steps, what is left),
/// each ending with the threads waiting for the last copy; and the submitting
/// thread went over all the steps once for each question it has (what to ask
/// about, what flights may write too, what shaders write). A refresh may come
/// earlier than its step: the title's bytes are the same until a step writes
/// title memory, and behind such a step everything is asked for again, as it
/// was.
pub(super) fn refresh_front() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    ahead_of_writes() && (joins_asked().0 > 1 || *ON.get_or_init(|| crate::switch::on("VICTORY_REFRESH_FRONT")))
}

/// Per copy what is refreshed in front of a submission's steps, as
/// [`written_spans`]'s but of all copies, up to the copy's first write by
/// shaders in each part of the submission (`Touch::Part`: another begins).
/// What a part refreshes of a copy it has given to its shaders stays where it
/// stands: around what they write. What a later part refreshes is the
/// title's bytes of before either.
pub(super) fn front_spans(steps: impl Iterator<Item = Touch>) -> Vec<(usize, usize, usize)> {
    front_spans_gap(steps, usize::MAX)
}

/// [`front_spans`] with a span for each run of what the refreshes refer to
/// that lies more than `gap` bytes from the next (`usize::MAX`: one span per
/// copy, from the lowest to the highest, as before).
pub(super) fn front_spans_gap(steps: impl Iterator<Item = Touch>, gap: usize) -> Vec<(usize, usize, usize)> {
    let mut at: ByCopy<usize> = ByCopy::default();
    // The copies in the order they come, what each is refreshed over (before
    // the part that gave it to shaders last), and that part.
    let mut copies: Vec<usize> = Vec::new();
    let mut ranges: Vec<Vec<(usize, usize)>> = Vec::new();
    let mut given: Vec<u32> = Vec::new();
    let (mut part, mut last) = (1, usize::MAX);
    for step in steps {
        match step {
            Touch::Refresh(copy, from, to) => {
                // (Most come several times on end.)
                let i = if copies.get(last).is_some_and(|c| *c == copy) {
                    last
                } else {
                    *at.entry(copy).or_insert_with(|| {
                        copies.push(copy);
                        ranges.push(Vec::new());
                        given.push(0);
                        copies.len() - 1
                    })
                };
                last = i;
                if given[i] != part {
                    ranges[i].push((from, to));
                }
            }
            Touch::Write(copy) => {
                let i = *at.entry(copy).or_insert_with(|| {
                    copies.push(copy);
                    ranges.push(Vec::new());
                    given.push(0);
                    copies.len() - 1
                });
                given[i] = part;
            }
            Touch::Part => part += 1,
            Touch::Title | Touch::Nothing => {}
        }
    }
    let mut spans = Vec::new();
    for (copy, runs) in copies.into_iter().zip(ranges) {
        spans.extend(joined_with_gap(runs, gap).into_iter().filter(|r| r.0 < r.1).map(|(a, b)| (copy, a, b)));
    }
    spans
}

/// [`front_spans_gap`] without a list of ranges, and another of the joined
/// ones, for each copy (`VICTORY_LEAN_STEPS`): a submission has some 700 copies
/// in front of its steps (4,500 and more in the lists that make the heap's
/// buffers ready), each of which cost three small blocks of memory made and
/// let go again. The ranges are put in one list with the number of their copy,
/// brought together by copy by counting, and each copy's looked through as a
/// slice: the same spans in the same order.
pub(super) fn front_spans_flat(steps: impl Iterator<Item = Touch>, gap: usize) -> Vec<(usize, usize, usize)> {
    thread_local! {
        static AT: std::cell::RefCell<ByCopy<usize>> = std::cell::RefCell::new(ByCopy::default());
    }
    let mut at = scratch::Lean::of(&AT, true);
    // The copies in the order they come, and the part that gave each to shaders last.
    let mut copies: Vec<usize> = Vec::new();
    let mut given: Vec<u32> = Vec::new();
    // (The number of the copy, from, to) of what each refreshes before that.
    let mut flat: Vec<(u32, usize, usize)> = Vec::new();
    let (mut part, mut last) = (1, usize::MAX);
    for step in steps {
        match step {
            Touch::Refresh(copy, from, to) => {
                let i = if copies.get(last).is_some_and(|c| *c == copy) {
                    last
                } else {
                    *at.entry(copy).or_insert_with(|| {
                        copies.push(copy);
                        given.push(0);
                        copies.len() - 1
                    })
                };
                last = i;
                if given[i] != part {
                    flat.push((i as u32, from, to));
                }
            }
            Touch::Write(copy) => {
                let i = *at.entry(copy).or_insert_with(|| {
                    copies.push(copy);
                    given.push(0);
                    copies.len() - 1
                });
                given[i] = part;
            }
            Touch::Part => part += 1,
            Touch::Title | Touch::Nothing => {}
        }
    }
    drop(at);
    // Each copy's ranges together, in the order they came: where each begins, then put there.
    let mut begins = vec![0u32; copies.len() + 1];
    for &(i, ..) in &flat {
        begins[i as usize + 1] += 1;
    }
    for k in 0..copies.len() {
        begins[k + 1] += begins[k];
    }
    let mut next = begins.clone();
    let mut ranges = vec![(0usize, 0usize); flat.len()];
    for &(i, from, to) in &flat {
        ranges[next[i as usize] as usize] = (from, to);
        next[i as usize] += 1;
    }
    let mut spans = Vec::new();
    for (k, &copy) in copies.iter().enumerate() {
        let runs = &mut ranges[begins[k] as usize..begins[k + 1] as usize];
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
                spans.push((copy, a, b));
            }
            open = Some((from, to));
        }
        if let Some((a, b)) = open
            && a < b
        {
            spans.push((copy, a, b));
        }
    }
    spans
}

/// `ranges` sorted and joined where one begins no more than `gap` past the
/// end of the one before.
pub(super) fn joined_with_gap(mut ranges: Vec<(usize, usize)>, gap: usize) -> Vec<(usize, usize)> {
    ranges.sort_unstable();
    let mut out: Vec<(usize, usize)> = Vec::with_capacity(ranges.len());
    for (from, to) in ranges {
        match out.last_mut() {
            Some(last) if from <= last.1.saturating_add(gap) => last.1 = last.1.max(to),
            _ => out.push((from, to)),
        }
    }
    out
}

/// The gap under which what the refreshes refer to is one span
/// (`VICTORY_REFRESH_NARROW=1`: 64 KiB; a number: that many KiB; unset or 0:
/// every copy's refreshes are one span from the lowest to the highest, as
/// before).
///
/// A span per copy was the whole range between the first and the last thing a
/// submission refers to: 16.6 GB of range a frame in the cell, for 49.5 MB the
/// title changed. The asking about the written pages, the look for what is new
/// and the loop over the pages are all over the range.
pub(super) fn refresh_narrow_gap() -> usize {
    // (May change while the title runs, `VICTORY_AB=<s>,VICTORY_REFRESH_NARROW[=<kib>]`: each use below reads it once for
    // what it makes, and a span made either way is refreshed as it refers: see docs/ab-switches.md.)
    fn gap(text: Option<&str>) -> usize {
        match text.and_then(|v| v.trim().parse::<usize>().ok()) {
            Some(1) => 64 << 10,
            Some(kib) if kib > 1 => kib << 10,
            _ => usize::MAX,
        }
    }
    crate::ab::live_value!("VICTORY_REFRESH_NARROW", usize, gap)
}

pub(super) fn narrow_on() -> bool {
    refresh_narrow_gap() != usize::MAX
}

/// With narrowing, whether what the steps refer to in a span is checked to be
/// brought up to date (`VICTORY_REFRESH_CHECK=1`; the bytes of whole pages of
/// it that the journal tells written since the copy took them are counted).
pub(super) fn refresh_check() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_REFRESH_CHECK").is_some_and(|v| v == "1"))
}

/// Of [`Batch::refresh_front`], with [`refresh_check`]: each range the steps
/// refer to, cut to the spans refreshed, must be as the title has it. Counted in
/// [`REFRESH_SIZE`]: the bytes looked at, those found stale, those that could
/// not be looked at without asking the kernel.
pub(super) fn check_front(
    copies: &ByCopy<std::sync::Arc<crate::shadow::Shadow>>,
    referenced: &[(usize, usize, usize)],
    spans: &[(usize, usize, usize)],
) {
    for &(key, a, b) in referenced {
        let Some(shadow) = copies.get(&key) else {
            continue;
        };
        for &(_, from, to) in spans.iter().filter(|s| s.0 == key) {
            let (a, b) = (a.max(from), b.min(to));
            if a >= b {
                continue;
            }
            match shadow.stale_in(a, b) {
                Some((looked, stale)) => {
                    REFRESH_SIZE[7].fetch_add(looked as u64, Ordering::Relaxed);
                    REFRESH_SIZE[8].fetch_add(stale as u64, Ordering::Relaxed);
                }
                None => {
                    REFRESH_SIZE[9].fetch_add((b - a) as u64, Ordering::Relaxed);
                }
            }
        }
    }
}

/// What `check_front` looks at (`VICTORY_REFRESH_CHECK=1`).
pub(super) struct FrontCheck<'a> {
    pub(super) copies: &'a ByCopy<std::sync::Arc<crate::shadow::Shadow>>,
    pub(super) referenced: &'a [(usize, usize, usize)],
    pub(super) span_ranges: &'a [(usize, usize, usize)],
}

/// What a step is to the copies' refreshes, for [`written_spans`].
pub(super) enum Touch {
    /// A refresh of a copy (by address), from, to.
    Refresh(usize, usize, usize),
    /// The copy is given to shaders.
    Write(usize),
    /// Title memory is written.
    Title,
    Nothing,
    /// Another part of the submission begins.
    Part,
}

/// Of `steps`, up to the first that writes title memory: the copies given to
/// shaders, each with the range its refreshes before that cover.
pub(super) fn written_spans(steps: impl Iterator<Item = Touch>) -> Vec<(usize, usize, usize)> {
    written_spans_gap(steps, usize::MAX)
}

/// [`written_spans`] with a span for each run of what the refreshes refer to
/// ([`front_spans_gap`]).
pub(super) fn written_spans_gap(steps: impl Iterator<Item = Touch>, gap: usize) -> Vec<(usize, usize, usize)> {
    // (What it is refreshed over, given to shaders.)
    let mut spans: ByCopy<(Vec<(usize, usize)>, bool)> = ByCopy::default();
    for step in steps {
        match step {
            Touch::Refresh(copy, from, to) => {
                let span = spans.entry(copy).or_insert_with(|| (Vec::new(), false));
                if !span.1 {
                    span.0.push((from, to));
                }
            }
            // (Not refreshed before: nothing of it is due.)
            Touch::Write(copy) => {
                if let Some(span) = spans.get_mut(&copy) {
                    span.1 = true;
                }
            }
            Touch::Title => break,
            Touch::Nothing | Touch::Part => {}
        }
    }
    spans
        .into_iter()
        .filter(|(_, s)| s.1)
        .flat_map(|(copy, (runs, _))| joined_with_gap(runs, gap).into_iter().map(move |(a, b)| (copy, a, b)))
        .collect()
}

/// Whether the fills of a submission's upload memory are done together on
/// several threads (`VICTORY_WIDE_FILLS=0`: one after the other, where the
/// lists name them).
///
/// They are 7,000 to 12,000 a frame, some 500 bytes each, 5 ms of the
/// submitting thread. Each reads title memory and writes memory of its own
/// that the card alone reads, when the lists run; they are done before the
/// next step that writes title memory or runs code of ours, and before the
/// submission goes on from its steps.
pub(super) fn wide_fills() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_FILLS").is_none_or(|v| v != "0") && wide_refresh())
}

/// Whether a submission's textures are brought up to date on several
/// threads (`VICTORY_WIDE_TEXTURES=0`: one after the other, where the lists
/// name them, as before).
///
/// They are read, hashed and, if they changed, detiled into their staging
/// buffers: 5 ms a frame of the submitting thread, for some 16 MB. Each has
/// its own buffer, notes and lock, and is refreshed by the title's own
/// threads as it is. They are done before the next step that writes title
/// memory, as the copies' refreshes are, and before the lists go to the
/// card. What is new: the note of a list that its upload takes what the
/// buffer then holds (`texture.rs`, `dirty`) may come ahead of the refresh
/// an earlier list of the submission asked for, not behind it. The texture
/// is then uploaded by a later list too, once more than needed.
pub(super) fn wide_textures() -> bool {
    static ON: OnceLock<bool> = OnceLock::new();
    *ON.get_or_init(|| std::env::var_os("VICTORY_WIDE_TEXTURES").is_none_or(|v| v != "0") && wide_refresh())
}

/// Whether one of `ranges` holds `[address, end)`. With `sorted` (they are in order and do not overlap: each begins
/// past the end of the one before) the one that could is found by bisection; else every one is looked at.
pub(super) fn ranges_cover(ranges: &[(usize, usize)], address: usize, end: usize, sorted: bool) -> bool {
    if !sorted || ranges.len() <= 8 {
        return ranges.iter().any(|&(from, to)| from <= address && end <= to);
    }
    // The last that begins at or before the address is the only one that can hold it.
    let at = ranges.partition_point(|r| r.0 <= address);
    at > 0 && end <= ranges[at - 1].1
}

/// `items` without those that `known` says of, in the order they stand in. With `wide` and many of them `known` is
/// asked on the kept threads ([`crate::shadow::wide_each`]), a piece of them each.
pub(super) fn retain_where<T: Send + Sync + 'static>(items: &mut Vec<T>, known: impl Fn(&T) -> bool + Send + Sync + 'static, wide: bool) {
    const PART: usize = 256;
    if !wide || items.len() < 2 * PART {
        items.retain(|item| !known(item));
        return;
    }
    let all = std::sync::Arc::new(std::mem::take(items));
    let said: std::sync::Arc<Vec<std::sync::atomic::AtomicBool>> =
        std::sync::Arc::new((0..all.len()).map(|_| std::sync::atomic::AtomicBool::new(false)).collect());
    {
        let (all, said) = (all.clone(), said.clone());
        crate::shadow::wide_each(all.len().div_ceil(PART), move |part| {
            for i in part * PART..((part + 1) * PART).min(all.len()) {
                if known(&all[i]) {
                    said[i].store(true, Ordering::Relaxed);
                }
            }
        });
    }
    // (The jobs are done, and have let go of what they held; if one has not yet, it is a moment.)
    let mut held = all;
    let all = loop {
        match std::sync::Arc::try_unwrap(held) {
            Ok(all) => break all,
            Err(still) => {
                held = still;
                std::thread::yield_now();
            }
        }
    };
    *items = all.into_iter().zip(said.iter()).filter(|(_, said)| !said.load(Ordering::Relaxed)).map(|(item, _)| item).collect();
}

/// `spans` (a copy, from, to) without those a copy has noted as holding nothing new there
/// ([`crate::shadow::Shadow::nothing_new_noted`]): for each a lock of the copy's notes and a few reads, a quarter of a
/// microsecond, of some 9,000 spans a frame, on the submitting thread; with `VICTORY_LEAN_STEPS` on the kept threads.
pub(super) fn retain_unnoted(spans: &mut Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>, wide: bool) {
    retain_where(spans, |(shadow, from, to)| shadow.nothing_new_noted(*from, *to - *from), wide);
}

/// A pointer that the threads of [`unnoted_spans`] may hold: what it points to is held by the caller until they are done.
pub(super) struct Held<T>(*const T);

impl<T> Held<T> {
    // (A method, so that a closure takes the whole of it, which is `Send`, and not its pointer.)
    fn get(&self) -> *const T {
        self.0
    }
}

// SAFETY: only read, while the caller, which waits for the threads, holds what it points to.
unsafe impl<T> Send for Held<T> {}

unsafe impl<T> Sync for Held<T> {}

pub(super) fn span_noted(copies: &[std::sync::Arc<crate::shadow::Shadow>], &(number, from, to): &(u32, usize, usize)) -> bool {
    copies[number as usize].nothing_new_noted(from, to - from)
}

/// The places among `spans` (copy by number, from, to) of those that no copy has noted as holding nothing new there
/// ([`crate::shadow::Shadow::nothing_new_noted`]), in order, without holding the copies a second time: as
/// [`retain_unnoted`], with `wide` in pieces on the kept threads.
pub(super) fn unnoted_spans(copies: &[std::sync::Arc<crate::shadow::Shadow>], spans: &[(u32, usize, usize)], wide: bool) -> Vec<usize> {
    const PART: usize = 256;
    if !wide || spans.len() < 2 * PART {
        return (0..spans.len()).filter(|&i| !span_noted(copies, &spans[i])).collect();
    }
    let said: std::sync::Arc<Vec<std::sync::atomic::AtomicBool>> =
        std::sync::Arc::new((0..spans.len()).map(|_| std::sync::atomic::AtomicBool::new(false)).collect());
    {
        let (held_copies, held_spans, said) = (Held(copies.as_ptr()), Held(spans.as_ptr()), said.clone());
        let (copies_len, spans_len) = (copies.len(), spans.len());
        crate::shadow::wide_each(spans_len.div_ceil(PART), move |part| {
            // SAFETY: the caller holds both slices until `wide_each` has returned.
            let (copies, spans) = unsafe {
                (std::slice::from_raw_parts(held_copies.get(), copies_len), std::slice::from_raw_parts(held_spans.get(), spans_len))
            };
            for i in part * PART..((part + 1) * PART).min(spans_len) {
                if span_noted(copies, &spans[i]) {
                    said[i].store(true, Ordering::Relaxed);
                }
            }
        });
    }
    (0..spans.len()).filter(|&i| !said[i].load(Ordering::Relaxed)).collect()
}

thread_local! {
    /// What a lean batch ([`Batch::new`]) keeps ahead of the steps, kept by the thread between submissions.
    static AHEAD_FIRST: std::cell::RefCell<ByCopy<(usize, usize)>> = std::cell::RefCell::new(ByCopy::default());
    static AHEAD_REST: std::cell::RefCell<ByCopy<Vec<(usize, usize)>>> = std::cell::RefCell::new(ByCopy::default());
}

impl Drop for Batch {
    fn drop(&mut self) {
        if self.lean {
            scratch::give(&AHEAD_FIRST, std::mem::take(&mut self.first));
            scratch::give(&AHEAD_REST, std::mem::take(&mut self.rest));
        }
        if self.pass_used {
            scratch::give(&onepass::AHEAD, std::mem::take(&mut self.ahead_idx));
        }
    }
}

impl Batch {
    /// A batch for a submission's steps (`lean`: [`lean_steps`], read once).
    pub(super) fn new(lean: bool) -> Batch {
        let mut batch = Batch::default();
        batch.lean = lean;
        if lean {
            // (The tables the thread kept from the submission before, emptied.)
            batch.first = scratch::take(&AHEAD_FIRST);
            batch.rest = scratch::take(&AHEAD_REST);
        }
        batch
    }

    /// Whether what is ahead of the steps for the copy `key` covers `[address, end)`
    /// in one range: it is refreshed ahead of the steps already.
    pub(super) fn covered(&mut self, key: usize, address: usize, end: usize) -> bool {
        if !self.lean {
            return self.ahead.get(&key).is_some_and(|ranges| ranges.iter().any(|&(from, to)| from <= address && end <= to));
        }
        // The steps of a list come several on end for one copy (the views of a draw), and what is refreshed ahead
        // of most copies is one range: the copy is looked up when it is not the one before.
        if self.memo.0 != key {
            let one = match (self.first.get(&key), self.rest.contains_key(&key)) {
                (Some(&one), false) => Some(one),
                // (None at all: a range that holds nothing.)
                (None, _) => Some((usize::MAX, 0)),
                _ => None,
            };
            self.memo = (key, one);
        }
        match self.memo.1 {
            Some((from, to)) => from <= address && end <= to,
            None => {
                self.first.get(&key).is_some_and(|&(from, to)| from <= address && end <= to)
                    || self.rest.get(&key).is_some_and(|ranges| ranges_cover(ranges, address, end, !self.unsorted))
            }
        }
    }

    /// What is refreshed ahead of the steps is no more, for `copy` or for all.
    pub(super) fn forget_ahead(&mut self, copy: Option<usize>) {
        match copy {
            Some(copy) => {
                self.ahead.remove(&copy);
                self.first.remove(&copy);
                self.rest.remove(&copy);
            }
            None => {
                self.ahead.clear();
                self.first.clear();
                self.rest.clear();
            }
        }
        if self.pass_used {
            // (What is ahead by number is forgotten for all copies, also when one is named: a refresh that is then
            // done is one more, never one less.)
            self.ahead_idx.forget_all();
        }
        self.memo = (0, None);
    }

    /// Puts `[from, to)` among what is refreshed ahead of the steps for `copy`.
    pub(super) fn put_ahead(&mut self, copy: usize, from: usize, to: usize) {
        if self.lean {
            match self.first.entry(copy) {
                std::collections::hash_map::Entry::Vacant(place) => {
                    place.insert((from, to));
                }
                std::collections::hash_map::Entry::Occupied(first) => {
                    let ranges = self.rest.entry(copy).or_default();
                    let last = ranges.last().copied().unwrap_or(*first.get());
                    if from < last.1 {
                        self.unsorted = true;
                    }
                    ranges.push((from, to));
                }
            }
        } else {
            let ranges = self.ahead.entry(copy).or_default();
            if ranges.last().is_some_and(|last| from < last.1) {
                self.unsorted = true;
            }
            ranges.push((from, to));
        }
        self.memo = (0, None);
    }

    pub(super) fn add(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        if self.covered(std::sync::Arc::as_ptr(shadow) as usize, address, end) {
            return;
        }
        self.pend(shadow, address, end);
    }

    /// [`Batch::add`] of a copy that has `number` in the pass of the submission ([`onepass`]): what is ahead is looked up by it.
    pub(super) fn add_at(&mut self, number: usize, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, len: usize) {
        let end = address + len;
        if self.ahead_idx.covered(number, address, end, !self.unsorted) {
            return;
        }
        self.pend(shadow, address, end);
    }

    /// The range `[address, end)` of the copy is refreshed with the next flush: merged into what is pending for it.
    pub(super) fn pend(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>, address: usize, end: usize) {
        let key = std::sync::Arc::as_ptr(shadow) as usize;
        if narrow_on() {
            self.parts.entry(key).or_default().push((address, end));
        }
        match self.index.get(&key) {
            Some(&at) => {
                let r = &mut self.refreshes[at];
                (r.1, r.2) = (r.1.min(address), r.2.max(end));
            }
            None => {
                self.index.insert(key, self.refreshes.len());
                self.refreshes.push((shadow.clone(), address, end));
            }
        }
    }

    /// Before shaders are given a range of `shadow`'s copy: its pending
    /// refresh is done, the others stay pending.
    pub(super) fn flush_one(&mut self, shadow: &std::sync::Arc<crate::shadow::Shadow>) {
        // From here on it is refreshed around what the shaders write. (What
        // is in front of all the steps was, up to here.)
        if !self.front {
            self.forget_ahead(Some(std::sync::Arc::as_ptr(shadow) as usize));
        }
        let Some(at) = self.index.remove(&(std::sync::Arc::as_ptr(shadow) as usize)) else {
            return;
        };
        let (shadow, from, to) = self.refreshes.swap_remove(at);
        // The last entry took its place.
        if let Some(moved) = self.refreshes.get(at) {
            self.index.insert(std::sync::Arc::as_ptr(&moved.0) as usize, at);
        }
        #[cfg(feature = "diagnostics")]
        let started = std::time::Instant::now();
        // (With narrowing: what it refers to, not all between the lowest and the highest.)
        let runs = match self.parts.remove(&(std::sync::Arc::as_ptr(&shadow) as usize)) {
            Some(parts) if narrow_on() => joined_with_gap(parts, refresh_narrow_gap()),
            _ => vec![(from, to)],
        };
        for (from, to) in runs {
            if !(crate::watch::light() && shadow.nothing_new(from, to - from)) {
                Self::update(&shadow, from, to);
            }
        }
        #[cfg(feature = "diagnostics")]
        {
            let ns = started.elapsed().as_nanos() as u64;
            SPLIT[6].fetch_add(ns, Ordering::Relaxed);
            SPLIT[9].fetch_add(ns, Ordering::Relaxed);
            SPLIT[10].fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Adds the time since `lap` to counter `c`, and starts the next lap.
    #[cfg(feature = "diagnostics")]
    pub(super) fn lap(lap: &mut std::time::Instant, c: crate::copystat::C) {
        let now = std::time::Instant::now();
        crate::copystat::add(c, (now - *lap).as_nanos() as u64);
        *lap = now;
    }

    /// Brings `[from, to)` of a copy up to date, timed.
    pub(super) fn update(shadow: &crate::shadow::Shadow, from: usize, to: usize) {
        let started = std::time::Instant::now();
        let _one = crate::timeline::span("a copy's refresh");
        let copied = shadow.update(from, to - from);
        let us = started.elapsed().as_micros() as u64;
        shadow.note_refresh_cost(us);
        #[cfg(feature = "diagnostics")]
        crate::copystat::range(from, to - from, us, 0);
        PREPARING[0].fetch_add(us, Ordering::Relaxed);
        PREPARING[1].fetch_add(copied as u64, Ordering::Relaxed);
    }

    /// Whether `staging` is still to be refreshed in this submission (and
    /// notes it refreshed).
    pub(super) fn texture_due<T>(&mut self, staging: &std::sync::Arc<T>) -> bool {
        self.textures.insert(std::sync::Arc::as_ptr(staging) as *const () as usize)
    }

    /// Before a step that writes title memory: pending refreshes are done
    /// first, and textures are checked again after it.
    pub(super) fn title_written(&mut self) {
        self.flush();
        self.forget_ahead(None);
        self.textures.clear();
    }

    /// Before the steps of `runs` ([`refresh_front`]), a new part of the
    /// submission beginning at each of `parts`.
    pub(super) fn refresh_front(&mut self, runs: &[&HostList], parts: &[usize]) {
        // (With `VICTORY_LEAN_STEPS` the thread's kept table and list, emptied: the copies' holds are let go at the
        // end, with the list of what the steps touch, whichever way they go out of this function.)
        thread_local! {
            static COPIES: std::cell::RefCell<ByCopy<std::sync::Arc<crate::shadow::Shadow>>> = std::cell::RefCell::new(ByCopy::default());
            static TOUCHES: std::cell::RefCell<Vec<Touch>> = const { std::cell::RefCell::new(Vec::new()) };
        }
        let mut copies = scratch::Lean::of(&COPIES, self.lean);
        let mut due = Vec::new();
        let mut touches = scratch::Lean::of(&TOUCHES, self.lean);
        for (n, list) in runs.iter().enumerate() {
            if parts.contains(&n) {
                touches.push(Touch::Part);
            }
            let mut last = 0;
            for step in &list.deferred().copies {
                match step {
                    Step::Refresh(shadow, at, len) => {
                        #[cfg(feature = "diagnostics")]
                        crate::pagedrive::step_seen(shadow.ledger());
                        let key = std::sync::Arc::as_ptr(shadow) as usize;
                        if key != last {
                            copies.entry(key).or_insert_with(|| shadow.clone());
                            last = key;
                        }
                        touches.push(Touch::Refresh(key, *at, *at + *len));
                    }
                    Step::GpuWrite(shadow, ..) => touches.push(Touch::Write(std::sync::Arc::as_ptr(shadow) as usize)),
                    Step::Texture(staging) if wide_textures() => {
                        if self.texture_due(staging) {
                            due.push(staging.clone());
                        }
                    }
                    _ => {}
                }
            }
        }
        if refresh_sizing() {
            let (bytes, count) = referenced_bytes(&touches);
            REFRESH_SIZE[5].fetch_add(bytes, Ordering::Relaxed);
            REFRESH_SIZE[6].fetch_add(count, Ordering::Relaxed);
        }
        let checking = narrow_on() && refresh_check();
        let referenced: Vec<(usize, usize, usize)> = if checking {
            touches.iter().filter_map(|t| if let Touch::Refresh(copy, from, to) = t { Some((*copy, *from, *to)) } else { None }).collect()
        } else {
            Vec::new()
        };
        let found = if self.lean {
            front_spans_flat(touches.drain(..), refresh_narrow_gap())
        } else {
            front_spans_gap(touches.drain(..), refresh_narrow_gap())
        };
        let spans: Vec<_> = found.into_iter().filter_map(|(key, from, to)| Some((copies.get(&key)?.clone(), from, to))).collect();
        self.front = true;
        for (shadow, from, to) in &spans {
            self.put_ahead(std::sync::Arc::as_ptr(shadow) as usize, *from, *to);
        }
        let span_ranges: Vec<(usize, usize, usize)> = if checking {
            spans.iter().map(|(shadow, from, to)| (std::sync::Arc::as_ptr(shadow) as usize, *from, *to)).collect()
        } else {
            Vec::new()
        };
        let check = checking.then(|| FrontCheck { copies: &copies, referenced: &referenced, span_ranges: &span_ranges });
        self.front_go(spans, due, check, false);
    }

    /// [`Batch::refresh_front`] for a submission that has its steps looked through ([`onepass`]): the spans come from the
    /// pass's records, by the copy's number, and what is ahead is kept by that number ([`onepass::Ahead`]). The same
    /// spans are refreshed. (Not with `VICTORY_REFRESH_SIZING` or `VICTORY_REFRESH_CHECK`, which look at the steps by address.)
    pub(super) fn refresh_front_pass(&mut self, pass: &mut onepass::Pass, parts: &[usize]) {
        let mut due = Vec::new();
        let wide = wide_textures();
        pass.front_spans(parts, refresh_narrow_gap(), |staging| {
            if wide && self.texture_due(staging) {
                due.push(staging.clone());
            }
        });
        // Those a copy noted as holding nothing new are left out before the copies are held (a hold and a let go of each of
        // the 9,000 spans a frame, on the submitting thread): the same ones `front_go` would leave out, in the same order.
        const FEW: usize = 256;
        let pruned = pass.spans.len() > FEW;
        let spans: Vec<_> = if pruned {
            let all = pass.spans.len();
            let kept = unnoted_spans(&pass.copies, &pass.spans, self.lean);
            #[cfg(feature = "diagnostics")]
            FLUSHES[2].fetch_add((all - kept.len()) as u64, Ordering::Relaxed);
            kept.into_iter().map(|i| (pass.copies[pass.spans[i].0 as usize].clone(), pass.spans[i].1, pass.spans[i].2)).collect()
        } else {
            pass.spans.iter().map(|&(number, from, to)| (pass.copies[number as usize].clone(), from, to)).collect()
        };
        self.front = true;
        if !self.pass_used {
            self.pass_used = true;
            self.ahead_idx = scratch::take(&onepass::AHEAD);
        }
        self.ahead_idx.begin(pass.copies.len());
        for &(number, from, to) in &pass.spans {
            self.ahead_idx.put(number as usize, from, to, &mut self.unsorted);
        }
        self.front_go(spans, due, None, pruned);
    }

    /// The refreshes in front of the steps: `spans` (the copies' ranges) and the textures `due` brought up to date, on
    /// the threads, once the ones known to hold nothing new are left out (`pruned`: they were, by the caller).
    pub(super) fn front_go(
        &mut self,
        spans: Vec<(std::sync::Arc<crate::shadow::Shadow>, usize, usize)>,
        due: Vec<std::sync::Arc<crate::texture::Staging>>,
        check: Option<FrontCheck<'_>>,
        pruned: bool,
    ) {
        #[cfg(feature = "diagnostics")]
        {
            FLUSHES[0].fetch_add(1, Ordering::Relaxed);
            FLUSHES[1].fetch_add(spans.len() as u64, Ordering::Relaxed);
        }
        let _flush = crate::timeline::span("refreshes of copies");
        REFRESH_SIZE[0].fetch_add(spans.len() as u64, Ordering::Relaxed);
        REFRESH_SIZE[1].fetch_add(spans.iter().map(|s| (s.2 - s.1) as u64).sum(), Ordering::Relaxed);
        let started = std::time::Instant::now();
        let before = [0, 2].map(|i| PREPARING[i].load(Ordering::Relaxed));
        // Those known to copy nothing are said so here, unless they are very
        // many: then beside the others.
        const FEW: usize = 256;
        let mut spans = spans;
        #[cfg(feature = "diagnostics")]
        let mut lap = std::time::Instant::now();
        if spans.len() > FEW && !pruned {
            // Those a copy noted as holding nothing new, and nothing has
            // touched since, need no thread (nor asking ahead).
            #[cfg(feature = "diagnostics")]
            let all = spans.len();
            retain_unnoted(&mut spans, self.lean);
            #[cfg(feature = "diagnostics")]
            FLUSHES[2].fetch_add((all - spans.len()) as u64, Ordering::Relaxed);
        }
        let many = spans.len() > FEW;
        if !many {
            #[cfg(feature = "diagnostics")]
            let all = spans.len();
            spans.retain(|(shadow, from, to)| !shadow.nothing_new(*from, *to - *from));
            #[cfg(feature = "diagnostics")]
            FLUSHES[2].fetch_add((all - spans.len()) as u64, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        Self::lap(&mut lap, crate::copystat::C::FlushPruneNs);
        REFRESH_SIZE[2].fetch_add(spans.len() as u64, Ordering::Relaxed);
        if refresh_lpt() {
            longest_first(&mut spans);
        }
        let ahead = spans.len() > 1;
        if ahead {
            let ranges: Vec<(usize, usize)> = spans.iter().filter(|r| r.0.asked_ahead()).map(|r| (r.1, r.2)).collect();
            crate::watch::ask_ahead(&ranges);
        }
        #[cfg(feature = "diagnostics")]
        {
            FLUSHES[3].fetch_add(spans.len() as u64, Ordering::Relaxed);
            Self::lap(&mut lap, crate::copystat::C::FlushAskNs);
        }
        // A job for each copy: its spans one after the other.
        let mut groups = spans_by_copy(spans.iter().map(|s| std::sync::Arc::as_ptr(&s.0) as usize));
        if wait_order() {
            let cost: Vec<u64> = spans.iter().map(|(shadow, from, to)| predicted_us(shadow, *from, *to)).collect();
            costliest_first(&mut groups, &cost);
        }
        let _joining = waits::joining(waits::Wait::JoinRefresh);
        crate::shadow::wide_each(groups.len() + due.len(), move |g| {
            // The textures first: they are the longer pieces.
            let Some(g) = g.checked_sub(due.len()) else {
                due[g].refresh();
                return;
            };
            for &i in &groups[g] {
                let (shadow, from, to) = &spans[i];
                if many && shadow.nothing_new(*from, *to - *from) {
                    #[cfg(feature = "diagnostics")]
                    FLUSHES[2].fetch_add(1, Ordering::Relaxed);
                    continue;
                }
                Self::update(shadow, *from, *to);
            }
        });
        #[cfg(feature = "diagnostics")]
        Self::lap(&mut lap, crate::copystat::C::FlushEachNs);
        // Before the asking stands no more: the journal says what it found.
        if let Some(check) = &check {
            check_front(check.copies, check.referenced, check.span_ranges);
        }
        if ahead {
            crate::watch::ask_ahead_done();
        }
        // The time counted is the time it took, not the threads' sum.
        let took = started.elapsed().as_micros() as u64;
        let summed = [0, 2].map(|i| PREPARING[i].load(Ordering::Relaxed)).iter().zip(before).map(|(now, was)| now.saturating_sub(was)).collect::<Vec<_>>();
        PREPARING[2].fetch_sub(summed[1], Ordering::Relaxed);
        PREPARING[0].fetch_sub(summed[0], Ordering::Relaxed);
        PREPARING[0].fetch_add(took, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        SPLIT[6].fetch_add(started.elapsed().as_nanos() as u64, Ordering::Relaxed);
    }

    /// Before the steps of `runs` ([`ahead_of_writes`]); copies and fills
    /// that `taken` names are not done on the CPU.
    pub(super) fn refresh_ahead(&mut self, runs: &[&HostList], taken: impl Fn(&HostList, usize) -> bool) {
        let mut copies: ByCopy<std::sync::Arc<crate::shadow::Shadow>> = ByCopy::default();
        let mut touches = Vec::new();
        'steps: for list in runs {
            for (i, step) in list.deferred().copies.iter().enumerate() {
                touches.push(match step {
                    Step::Refresh(shadow, at, len) => {
                        let key = std::sync::Arc::as_ptr(shadow) as usize;
                        copies.entry(key).or_insert_with(|| shadow.clone());
                        Touch::Refresh(key, *at, *at + *len)
                    }
                    Step::GpuWrite(shadow, ..) => Touch::Write(std::sync::Arc::as_ptr(shadow) as usize),
                    Step::Copy(..) if taken(list, i) => Touch::Nothing,
                    Step::Copy(..) | Step::Run(_) | Step::Placed(_) => break 'steps,
                    _ => Touch::Nothing,
                });
            }
        }
        let due: Vec<_> = written_spans_gap(touches.into_iter(), refresh_narrow_gap())
            .into_iter()
            .filter_map(|(key, from, to)| Some((copies.get(&key)?.clone(), from, to)))
            .collect();
        for (shadow, from, to) in &due {
            self.put_ahead(std::sync::Arc::as_ptr(shadow) as usize, *from, *to);
        }
        REFRESH_SIZE[3].fetch_add(due.len() as u64, Ordering::Relaxed);
        REFRESH_SIZE[4].fetch_add(due.iter().map(|s| (s.2 - s.1) as u64).sum(), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        let began = std::time::Instant::now();
        let started = std::time::Instant::now();
        let before = PREPARING[0].load(Ordering::Relaxed);
        let copies = due.iter().map(|s| std::sync::Arc::as_ptr(&s.0) as usize).collect::<Vec<_>>();
        let cost = wait_order().then(|| due.iter().map(|(shadow, from, to)| predicted_us(shadow, *from, *to)).collect());
        wide_each_by_copy_by(copies.into_iter(), cost, move |i| {
            let (shadow, from, to) = &due[i];
            if !shadow.nothing_new(*from, *to - *from) {
                Self::update(shadow, *from, *to);
            }
        });
        // The time counted is the time it took, not the threads' sum.
        let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
        PREPARING[0].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        {
            let ns = began.elapsed().as_nanos() as u64;
            SPLIT[6].fetch_add(ns, Ordering::Relaxed);
            SPLIT[9].fetch_add(ns, Ordering::Relaxed);
        }
    }

    /// The fills noted are done.
    pub(super) fn fill(&mut self) {
        // Below this many the threads cost more than they save.
        const FEW: usize = 96;
        const PART: usize = 64;
        if self.fills.is_empty() {
            return;
        }
        let started = std::time::Instant::now();
        if self.fills.len() < FEW {
            for (dst, src, len) in self.fills.drain(..) {
                copy_mapped(dst as *mut u8, src as *const u8, len, false);
            }
        } else {
            let fills = std::mem::take(&mut self.fills);
            crate::shadow::wide_each(fills.len().div_ceil(PART), move |i| {
                for &(dst, src, len) in fills.iter().skip(i * PART).take(PART) {
                    copy_mapped(dst as *mut u8, src as *const u8, len, false);
                }
            });
        }
        let took = started.elapsed();
        PREPARING[0].fetch_add(took.as_micros() as u64, Ordering::Relaxed);
        #[cfg(feature = "diagnostics")]
        SPLIT[7].fetch_add(took.as_nanos() as u64, Ordering::Relaxed);
    }

    /// The textures due are brought up to date.
    pub(super) fn refresh_textures(&mut self) {
        if self.due.len() < 2 || !wide_textures() {
            self.due.drain(..).for_each(|staging| staging.refresh());
            return;
        }
        let _textures = crate::timeline::span("textures brought up to date");
        // The time counted is the time it took, not the threads' sum.
        let started = std::time::Instant::now();
        let before = PREPARING[2].load(Ordering::Relaxed);
        let due = std::mem::take(&mut self.due);
        crate::shadow::wide_each(due.len(), move |i| due[i].refresh());
        let summed = PREPARING[2].load(Ordering::Relaxed).saturating_sub(before);
        PREPARING[2].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
    }

    pub(super) fn flush(&mut self) {
        self.refresh_textures();
        self.index.clear();
        if self.refreshes.is_empty() {
            return;
        }
        #[cfg(feature = "diagnostics")]
        let (began, all) = (std::time::Instant::now(), self.refreshes.len());
        let _flush = crate::timeline::span("refreshes of copies");
        self.flush_all();
        #[cfg(feature = "diagnostics")]
        {
            SPLIT[6].fetch_add(began.elapsed().as_nanos() as u64, Ordering::Relaxed);
            FLUSHES[0].fetch_add(1, Ordering::Relaxed);
            FLUSHES[1].fetch_add(all as u64, Ordering::Relaxed);
        }
    }

    pub(super) fn flush_all(&mut self) {
        // With narrowing each copy's pending refresh is what its steps refer to.
        if narrow_on() && !self.parts.is_empty() {
            let gap = refresh_narrow_gap();
            let pending = std::mem::take(&mut self.refreshes);
            for (shadow, from, to) in pending {
                match self.parts.remove(&(std::sync::Arc::as_ptr(&shadow) as usize)) {
                    Some(parts) => {
                        for (a, b) in joined_with_gap(parts, gap) {
                            self.refreshes.push((shadow.clone(), a.max(from), b.min(to)));
                        }
                    }
                    None => self.refreshes.push((shadow, from, to)),
                }
            }
            self.parts.clear();
        }
        // Most of a flush's copies are as they were: they are told so here,
        // from what the submission's asking found, before anything is asked
        // and any thread woken. (Thousands of them, the heap's buffer views
        // once a frame: on the threads, below.)
        const FEW: usize = 256;
        let light = crate::watch::light();
        #[cfg(feature = "diagnostics")]
        let mut lap = std::time::Instant::now();
        if light && self.refreshes.len() > FEW {
            // (As in `refresh_front`.)
            #[cfg(feature = "diagnostics")]
            let all = self.refreshes.len();
            retain_unnoted(&mut self.refreshes, self.lean);
            #[cfg(feature = "diagnostics")]
            FLUSHES[2].fetch_add((all - self.refreshes.len()) as u64, Ordering::Relaxed);
        }
        if light && self.refreshes.len() <= FEW {
            let started = std::time::Instant::now();
            #[cfg(feature = "diagnostics")]
            let all = self.refreshes.len();
            self.refreshes.retain(|(shadow, from, to)| !shadow.nothing_new(*from, *to - *from));
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            FLUSHES[2].fetch_add((all - self.refreshes.len()) as u64, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        Self::lap(&mut lap, crate::copystat::C::FlushPruneNs);
        // One write-watch asking for the copies that lie close together,
        // not one per copy.
        let ahead = self.refreshes.len() > 1;
        if ahead {
            let started = std::time::Instant::now();
            let ranges: Vec<(usize, usize)> = self
                .refreshes
                .iter()
                .filter(|r| r.0.asked_ahead())
                .map(|r| (r.1, r.2))
                .collect();
            crate::watch::ask_ahead(&ranges);
            PREPARING[0].fetch_add(started.elapsed().as_micros() as u64, Ordering::Relaxed);
        }
        #[cfg(feature = "diagnostics")]
        Self::lap(&mut lap, crate::copystat::C::FlushAskNs);
        if wide_refresh() && self.refreshes.len() >= 4 {
            // Each copy on its own, several at a time: a copy's refresh
            // writes that copy and reads title memory, nothing of another's.
            // The time counted is the time it took, not the threads' sum.
            let started = std::time::Instant::now();
            let before = PREPARING[0].load(Ordering::Relaxed);
            #[cfg(feature = "diagnostics")]
            FLUSHES[3].fetch_add(self.refreshes.len() as u64, Ordering::Relaxed);
            if light {
                // A number to a free thread, not a job to each copy.
                let refreshes = std::mem::take(&mut self.refreshes);
                let copies = refreshes.iter().map(|s| std::sync::Arc::as_ptr(&s.0) as usize).collect::<Vec<_>>();
                let cost = wait_order().then(|| refreshes.iter().map(|(shadow, from, to)| predicted_us(shadow, *from, *to)).collect());
                wide_each_by_copy_by(copies.into_iter(), cost, move |i| {
                    let (shadow, from, to) = &refreshes[i];
                    if refreshes.len() > FEW && shadow.nothing_new(*from, *to - *from) {
                        #[cfg(feature = "diagnostics")]
                        FLUSHES[2].fetch_add(1, Ordering::Relaxed);
                        return;
                    }
                    Self::update(shadow, *from, *to);
                });
            } else {
                let jobs: Vec<Box<dyn FnOnce() + Send>> = self
                    .refreshes
                    .drain(..)
                    .map(|(shadow, from, to)| Box::new(move || Self::update(&shadow, from, to)) as Box<dyn FnOnce() + Send>)
                    .collect();
                crate::shadow::wide_run(jobs);
            }
            let summed = PREPARING[0].load(Ordering::Relaxed).saturating_sub(before);
            PREPARING[0].fetch_sub(summed.saturating_sub(started.elapsed().as_micros() as u64), Ordering::Relaxed);
        } else {
            for (shadow, from, to) in self.refreshes.drain(..) {
                Self::update(&shadow, from, to);
            }
        }
        #[cfg(feature = "diagnostics")]
        Self::lap(&mut lap, crate::copystat::C::FlushEachNs);
        if ahead {
            crate::watch::ask_ahead_done();
        }
    }
}
