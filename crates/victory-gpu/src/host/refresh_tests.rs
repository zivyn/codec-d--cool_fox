use super::Batch;
use crate::shadow::Shadow;
use std::sync::Arc;
use windows_sys::Win32::System::Memory::{
    CreateFileMappingW, FILE_MAP_WRITE, MEM_COMMIT, MEM_RESERVE, MapViewOfFile, PAGE_READWRITE,
    VirtualAlloc,
};

const PAGE: usize = 64 << 10;
const MEM_WRITE_WATCH: u32 = 0x20_0000;

/// Title memory for the tests (never given back: copies of it stay in
/// the guards): write-watched, or a view of a section.
fn title(len: usize, view: bool) -> usize {
    // SAFETY: plain allocations.
    let at = unsafe {
        if view {
            let section =
                CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, 0, len as u32, std::ptr::null());
            let at = MapViewOfFile(section, FILE_MAP_WRITE, 0, 0, len).Value as usize;
            // As the runtime tells of it.
            crate::title_memory_section(section as usize, len as u64, true);
            crate::title_memory_remapped(at, len);
            crate::title_memory_view(at, len, section as usize, 0);
            at
        } else {
            VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) as usize
        }
    };
    assert_ne!(at, 0);
    at
}

/// Title memory of `blocks` pages in a placeholder, one view to a page,
/// and every fourth page not there: as a title that streams has it.
fn with_holes(blocks: usize) -> usize {
    use windows_sys::Win32::System::Memory::{
        MEM_PRESERVE_PLACEHOLDER, MEM_RELEASE, MEM_REPLACE_PLACEHOLDER, MEM_RESERVE_PLACEHOLDER, MapViewOfFile3, PAGE_NOACCESS,
        VirtualAlloc2, VirtualFree,
    };
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    // SAFETY: a placeholder and a section of our own.
    unsafe {
        let at = VirtualAlloc2(
            GetCurrentProcess(),
            std::ptr::null(),
            blocks * PAGE,
            MEM_RESERVE | MEM_RESERVE_PLACEHOLDER,
            PAGE_NOACCESS,
            std::ptr::null_mut(),
            0,
        ) as usize;
        assert_ne!(at, 0);
        let len = blocks * PAGE;
        let section =
            CreateFileMappingW(-1isize as _, std::ptr::null(), PAGE_READWRITE, (len >> 32) as u32, len as u32, std::ptr::null());
        for block in (0..blocks).filter(|b| b % 4 != 3) {
            let to = at + block * PAGE;
            VirtualFree(to as *mut _, PAGE, MEM_RELEASE | MEM_PRESERVE_PLACEHOLDER);
            let view = MapViewOfFile3(
                section,
                GetCurrentProcess(),
                to as *const _,
                (block * PAGE) as u64,
                PAGE,
                MEM_REPLACE_PLACEHOLDER,
                PAGE_READWRITE,
                std::ptr::null_mut(),
                0,
            );
            assert_eq!(view.Value as usize, to);
            crate::title_memory_remapped(to, PAGE);
        }
        at
    }
}

fn memory(len: usize) -> *mut u8 {
    // SAFETY: a plain allocation.
    unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT, PAGE_READWRITE).cast() }
}

struct Random(u64);

impl Random {
    fn below(&mut self, n: usize) -> usize {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        (self.0 >> 11) as usize % n
    }
}

/// Prepares as a submission does: one asking for all the ranges, the
/// refreshes in several flushes, the asking's end.
fn submission(ranges: &[(Arc<Shadow>, usize, usize)], flushes: usize) {
    // One at a time, as submissions are.
    static ONE: std::sync::Mutex<()> = std::sync::Mutex::new(());
    let _one = ONE.lock().unwrap_or_else(|e| e.into_inner());
    let asked: Vec<(usize, usize)> = ranges.iter().filter(|r| !r.0.unwatched()).map(|r| (r.1, r.1 + r.2)).collect();
    crate::watch::ask_once(&asked);
    let mut batch = Batch::default();
    for part in ranges.chunks(ranges.len().div_ceil(flushes).max(1)) {
        for (shadow, at, len) in part {
            batch.add(shadow, *at, *len);
        }
        batch.flush();
    }
    crate::watch::ask_once_end();
}

/// Whatever is written where, by the title or by us, and whichever
/// copies (of both kinds of memory, over one another, none on a page
/// line) are refreshed in which ranges: a refreshed range holds what
/// title memory holds.
#[test]
fn refreshed_ranges_hold_title_memory() {
    // SAFETY (all blocks): title memory and copies of the test's own,
    // written by this thread and, in `Batch::flush`, by the threads that
    // refresh, which are done when it returns.
    let mut random = Random(0x1234_5678_9ABC_DEF1);
    const LEN: usize = 24 * PAGE;
    let regions = [title(LEN, false), title(LEN, true)];
    let mut copies = Vec::new();
    for &region in &regions {
        for (from, to) in [(0, LEN), (100, LEN - 5000), (3 * PAGE + 777, 9 * PAGE + 10), (8 * PAGE, 8 * PAGE + 300), (PAGE, 20 * PAGE)] {
            let data = memory(to - from);
            copies.push((Shadow::unbacked((region + from) as u64, data, (to - from) as u64, false), data));
        }
    }
    let byte = |at: usize| unsafe { (at as *const u8).read_volatile() };
    for round in 0..600 {
        for _ in 0..random.below(4) {
            let at = regions[random.below(2)] + random.below(LEN - 64);
            let n = 1 + random.below(60);
            if random.below(3) == 0 {
                // What shaders wrote.
                let was: Vec<u8> = (0..n).map(|i| byte(at + i)).collect();
                let now: Vec<u8> = was.iter().map(|b| if random.below(2) == 0 { *b } else { random.below(256) as u8 }).collect();
                Shadow::put(at, &was, &now);
                assert!(now.iter().enumerate().all(|(i, &b)| byte(at + i) == b), "not put");
                continue;
            }
            for i in 0..n {
                unsafe { ((at + i) as *mut u8).write_volatile(random.below(256) as u8) };
            }
        }
        let mut ranges = Vec::new();
        for (shadow, _) in &copies {
            if random.below(3) == 0 {
                continue;
            }
            let (start, len) = shadow.span();
            let (start, len) = (start as usize, len as usize);
            // Often the range of the time before, as a list's is.
            let from = if random.below(2) == 0 { 0 } else { random.below(len) };
            let n = if random.below(2) == 0 { len - from } else { 1 + random.below(len - from) };
            ranges.push((shadow.clone(), start + from, n));
        }
        if ranges.is_empty() {
            continue;
        }
        let flushes = 1 + random.below(3);
        submission(&ranges, flushes);
        for (shadow, at, n) in &ranges {
            let data = copies.iter().find(|c| Arc::ptr_eq(&c.0, shadow)).unwrap().1;
            let start = shadow.span().0 as usize;
            let same = unsafe {
                std::slice::from_raw_parts(*at as *const u8, *n) == std::slice::from_raw_parts(data.add(at - start), *n)
            };
            assert!(same, "round {round}: {at:#x}+{n:#x} of a copy at {start:#x}; first byte {}", byte(*at));
        }
    }
}

/// A copy in the card's memory, with the card's side and the sends to it
/// played by the test.
struct Carded {
    shadow: Arc<Shadow>,
    data: usize,
    start: usize,
    len: usize,
    /// The last send, the last send every fetch to come is behind, and
    /// the card's bytes: the queue's order is the order it is locked in.
    card: std::sync::Mutex<(u64, u64, Vec<u8>)>,
    /// Whether a flight that writes it has not landed.
    flying: std::sync::atomic::AtomicBool,
    /// The dword of each eight its shaders write.
    class: usize,
}

impl Carded {
    fn new(start: usize, len: usize, class: usize) -> Carded {
        let data = memory(len);
        Carded {
            shadow: Shadow::unbacked(start as u64, data, len as u64, true),
            data: data as usize,
            start,
            len,
            card: std::sync::Mutex::new((0, 0, vec![0; len])),
            flying: std::sync::atomic::AtomicBool::new(false),
            class,
        }
    }

    /// What changed on the CPU's side goes up.
    fn send(&self) {
        let mut card = self.card.lock().unwrap();
        card.0 += 1;
        let (number, merged) = (card.0, card.1);
        for (a, b) in self.shadow.send_up(number, merged) {
            // SAFETY: the copy's bytes; nothing writes those that are to
            // go up.
            unsafe { std::ptr::copy_nonoverlapping((self.data + a) as *const u8, card.2[a..b].as_mut_ptr(), b - a) };
        }
    }
}

/// Everything at once: the title writes its memory on two threads, the
/// submitting thread puts its bytes in under shader writes
/// (`Shadow::merge_under_writes`), refreshes copies that overlap (two in
/// the card's memory, one not), sends up what changed and declares what
/// the next flight's shaders write, and what flights wrote is landed
/// meanwhile (`crate::land`, beside the guards). When all of it stands
/// still and the copies are refreshed once more, each of them, and the
/// card's side of those that have one, hold what title memory holds, and
/// title memory what was written last.
///
/// Into the first copy shaders write besides through a view nobody
/// declared, words the title does not write: nothing that goes up ever
/// puts an older value over those.
///
/// (It fails in the first rounds if `landed_beside` notes nothing, or
/// only takes the block out of the guards' clean ones and leaves its bit,
/// or if a fault leaves the bit; in some thousand rounds,
/// `VICTORY_TEST_ROUNDS`, if what is landed is noted before it is in.)
#[test]
fn landings_title_writes_and_light_refreshes_at_once() {
    use super::merge_under_try_first;
    use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
    assert!(crate::safecopy::on());
    const LEN: usize = 24 * PAGE;
    let rounds: usize = std::env::var("VICTORY_TEST_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(120);
    let region = title(LEN, true);
    let dword = |at: usize| unsafe { &*((region + at) as *const AtomicU32) };
    let carded = [Carded::new(region, LEN, 3), Carded::new(region + 4 * PAGE + 1000, LEN - 6 * PAGE - 4, 5)];
    let plain = {
        let (start, len) = (region + 700, LEN - 9000);
        let data = memory(len);
        (Shadow::unbacked(start as u64, data, len as u64, false), start, len, data as usize)
    };
    // What was last written so, by offset.
    let mut undeclared = std::collections::HashMap::new();
    let exact = std::env::var_os("VICTORY_SEND_CHANGED").is_some_and(|v| v != "0");
    let stop = AtomicBool::new(false);
    // Also when an assertion fails on one of the threads.
    struct Stop<'a>(&'a AtomicBool);
    impl Drop for Stop<'_> {
        fn drop(&mut self) {
            self.0.store(true, Ordering::Relaxed);
        }
    }
    // Held shared by who writes title memory, alone while it is looked at.
    let still = std::sync::RwLock::new(());
    let (fly, flights) = std::sync::mpsc::channel::<(usize, Vec<(usize, usize)>, u32)>();
    std::thread::scope(|s| {
        for t in 0..2usize {
            let (stop, still) = (&stop, &still);
            s.spawn(move || {
                let mut random = Random(0x7777_1234_0000_0001 + t as u64 * 0x1111);
                let mut value = 0u32;
                while !stop.load(Ordering::Relaxed) {
                    {
                        let _moving = still.read().unwrap();
                        for _ in 0..3 {
                            value += 1;
                            // Of each eight dwords, one a thread.
                            let at = random.below(LEN / 32) * 32 + 4 * (1 + t);
                            dword(at).store(value, Ordering::Relaxed);
                        }
                    }
                    std::thread::sleep(std::time::Duration::from_micros(20));
                }
            });
        }
        // Lands flights.
        {
            let (carded, stop) = (&carded, &stop);
            s.spawn(move || {
                let _stop = Stop(stop);
                while let Ok((which, ranges, value)) = flights.recv() {
                    let c = &carded[which];
                    // Fetched: the card's bytes, behind every send so far.
                    let (after, now) = {
                        let card = c.card.lock().unwrap();
                        (card.0, card.2.clone())
                    };
                    // By piece: 64 KiB of title memory.
                    let mut pieces = Vec::new();
                    for &(from, to) in &ranges {
                        let mut at = from;
                        while at < to {
                            let next = ((c.start + at) / PAGE * PAGE + PAGE - c.start).min(to);
                            pieces.push((at, next, now.as_ptr() as usize + at));
                            at = next;
                        }
                    }
                    let parts: Vec<_> = pieces.chunks(3).collect();
                    crate::land::each(parts.len(), &|i| {
                        let mut notes = crate::land::Notes::default();
                        // SAFETY: the bytes of `now`; each piece once.
                        unsafe { c.shadow.land_card(parts[i], after, &mut notes) };
                        notes.flush();
                    });
                    for &(from, to) in &ranges {
                        for at in (from..to).filter(|at| (c.start - region + at) % 32 == 4 * c.class) {
                            // (The next fetch may have brought more.)
                            assert!(dword(c.start - region + at).load(Ordering::Relaxed) >= value, "not landed");
                        }
                    }
                    let done: Vec<_> = ranges.iter().map(|&(a, b)| (c.start + a, c.start + b)).collect();
                    c.shadow.copy_back_done(&done);
                    c.card.lock().unwrap().1 = after;
                    c.flying.store(false, Ordering::Release);
                }
            });
        }
        let _stop = Stop(&stop);
        let mut random = Random(0xABCD_EF01_2345_6789);
        let check = |round: usize, random: &mut Random, quiet: bool| {
            // (One way and the other in turn: the one that leaves a copy a landing holds for the end, `VICTORY_UNDER_TRY`.)
            if round % 2 == 1 {
                let spans: Vec<_> =
                    carded.iter().filter(|c| c.shadow.writes_pending()).map(|c| (c.shadow.clone(), c.start, c.start + c.len)).collect();
                let mut runs = usize::MAX;
                assert!(merge_under_try_first(spans, &mut runs).is_empty(), "round {round}");
            } else {
                for c in &carded {
                    if c.shadow.writes_pending() {
                        let mut runs = usize::MAX;
                        assert!(!c.shadow.merge_under_writes(c.start, c.len, &mut runs), "round {round}");
                    }
                }
            }
            let mut ranges = Vec::new();
            for (shadow, start, len) in
                carded.iter().map(|c| (&c.shadow, c.start, c.len)).chain([(&plain.0, plain.1, plain.2)])
            {
                // All of it, or a part; often the same part.
                let (from, n) = match random.below(3) {
                    _ if quiet => (0, len),
                    0 => (0, len),
                    1 => (len / 3 / 4 * 4, len / 2 / 4 * 4),
                    _ => {
                        let from = random.below(len / 4) * 4;
                        (from, (4 + random.below((len - from) / 4) * 4).min(len - from))
                    }
                };
                ranges.push((shadow.clone(), start + from, n));
            }
            submission(&ranges, 1 + random.below(3));
            carded.iter().for_each(Carded::send);
        };
        let mut value = 1u32;
        for round in 0..rounds {
            check(round, &mut random, false);
            if exact {
                let c = &carded[0];
                let mut card = c.card.lock().unwrap();
                for _ in 0..40 {
                    value += 1;
                    let at = random.below(LEN / 32) * 32 + 4 * 6;
                    card.2[at..at + 4].copy_from_slice(&value.to_le_bytes());
                    undeclared.insert(at, value);
                }
            }
            // The flight of this submission: its shaders' writes.
            for (which, c) in carded.iter().enumerate() {
                if c.flying.swap(true, Ordering::Acquire) {
                    continue;
                }
                value += 1;
                let mut ranges: Vec<(usize, usize)> = Vec::new();
                let first = random.below(6) * PAGE / 2;
                for k in 0..2 {
                    let from = first + k * 9 * PAGE + 4 * random.below(9000);
                    let to = (from + 4 * (1 + random.below(7 * PAGE / 4))).min(c.len);
                    if from < to {
                        ranges.push((from, to));
                    }
                }
                for &(from, to) in &ranges {
                    c.shadow.writing(c.start + from, to - from);
                }
                {
                    let mut card = c.card.lock().unwrap();
                    for &(from, to) in &ranges {
                        for at in (from..to).filter(|at| (c.start - region + at) % 32 == 4 * c.class) {
                            card.2[at..at + 4].copy_from_slice(&value.to_le_bytes());
                        }
                    }
                }
                fly.send((which, ranges, value)).unwrap();
            }
            if round % 20 != 19 {
                continue;
            }
            // Still: no flight in the air, no title write.
            let _still = still.write().unwrap();
            while carded.iter().any(|c| c.flying.load(Ordering::Acquire)) {
                assert!(!stop.load(Ordering::Relaxed), "the landing thread failed");
                std::thread::yield_now();
            }
            check(round, &mut random, true);
            // SAFETY: the test's own memory, written by nobody now.
            let bytes = |at: usize, len: usize| unsafe { std::slice::from_raw_parts(at as *const u8, len) };
            let first = |a: &[u8], b: &[u8]| a.iter().zip(b).position(|(x, y)| x != y);
            for c in &carded {
                let (held, there) = (bytes(c.data, c.len), bytes(c.start, c.len));
                assert_eq!(first(held, there), None, "round {round}: a card copy's CPU side (class {})", c.class);
                let card = c.card.lock().unwrap();
                let mut side = card.2.clone();
                if c.class == carded[0].class {
                    // (They may have been landed meanwhile, by a flight
                    // that declared their range, and may not.)
                    for (&at, &value) in &undeclared {
                        let on_card = u32::from_le_bytes(side[at..at + 4].try_into().unwrap());
                        assert_eq!(on_card, value, "round {round}: an older value at {at:#x}");
                        side[at..at + 4].copy_from_slice(&there[at..at + 4]);
                    }
                }
                assert_eq!(first(&side, there), None, "round {round}: a card copy's card side (class {})", c.class);
            }
            assert_eq!(first(bytes(plain.3, plain.2), bytes(plain.1, plain.2)), None, "round {round}: the other copy");
        }
        drop(fly);
    });
}

/// A copy in the card's memory with shaders writing all of it, and a title that writes one dword of it between two
/// puttings in of its bytes. A copy whose lock a landing holds is left (`Locks::Try`: `Busy`, not a byte of it
/// touched), and the call that follows, or one that waits, does all that was owed.
#[test]
fn a_copy_that_is_held_is_left_alone_by_a_try_and_done_by_the_next_call() {
    use crate::shadow::{Locks, Merged};
    const LEN: usize = 8 * PAGE;
    let region = title(LEN, true);
    let c = Carded::new(region, LEN, 0);
    c.shadow.update(c.start, c.len);
    c.shadow.writing(c.start, c.len);
    let mut runs = usize::MAX;
    // The first look takes the pages whole.
    assert_eq!(c.shadow.merge_under_writes_with(c.start, c.len, &mut runs, Locks::Wait), Merged::Done);
    let at = 3 * PAGE + 64;
    // SAFETY (both): the test's own memory.
    let word = |from: usize| unsafe { ((from + at) as *const u32).read_volatile() };
    let put = |v: u32| unsafe { ((c.start + at) as *mut u32).write_volatile(v) };
    for round in 1..=6u32 {
        put(0x5000 + round);
        let was = word(c.data);
        assert_ne!(was, 0x5000 + round);
        // A landing holds `merging` (shared, or here alone), or the snapshot's lock is held.
        {
            let _held = c.shadow.hold_merging();
            assert_eq!(c.shadow.merge_under_writes_with(c.start, c.len, &mut runs, Locks::Try), Merged::Busy, "round {round}");
        }
        assert_eq!(word(c.data), was, "round {round}: a busy copy was written");
        {
            let _held = c.shadow.hold_before();
            assert_eq!(c.shadow.merge_under_writes_with(c.start, c.len, &mut runs, Locks::Try), Merged::Busy, "round {round}");
        }
        assert_eq!(word(c.data), was, "round {round}: a busy copy was written");
        // Free: the title's word goes in (by the call that tries, or the one that waits).
        let how = if round % 2 == 0 { Locks::Try } else { Locks::Wait };
        assert_eq!(c.shadow.merge_under_writes_with(c.start, c.len, &mut runs, how), Merged::Done, "round {round}");
        assert_eq!(word(c.data), 0x5000 + round, "round {round}: the title's byte is not in");
        // And nothing is left owed: the same again changes nothing.
        assert_eq!(c.shadow.merge_under_writes_with(c.start, c.len, &mut runs, Locks::Try), Merged::Done);
        assert_eq!(word(c.data), 0x5000 + round);
    }
}

/// With a copy held by a thread of a landing, [`merge_under_try_first`] puts the title's bytes into the others
/// first (the held one is waited for last, when the holder lets go, which it does only after the others are
/// done), and it names no copy that needs the flights settled.
#[test]
fn the_copies_that_are_not_held_go_first_and_the_held_one_last() {
    use super::merge_under_try_first;
    use std::sync::atomic::{AtomicBool, Ordering};
    const LEN: usize = 8 * PAGE;
    let region = title(3 * LEN, true);
    let copies: Vec<Carded> = (0..3).map(|i| Carded::new(region + i * LEN, LEN, i)).collect();
    for c in &copies {
        c.shadow.update(c.start, c.len);
        c.shadow.writing(c.start, c.len);
    }
    let spans = || -> Vec<_> { copies.iter().map(|c| (c.shadow.clone(), c.start, c.start + c.len)).collect() };
    let mut runs = usize::MAX;
    assert!(merge_under_try_first(spans(), &mut runs).is_empty());
    let at = 3 * PAGE + 64;
    // SAFETY (both): the test's own memory.
    let word = |from: usize| unsafe { ((from + at) as *const u32).read_volatile() };
    let put = |c: &Carded, v: u32| unsafe { ((c.start + at) as *mut u32).write_volatile(v) };
    for round in 0..8usize {
        let value = 0x7000 + round as u32;
        // Each round another copy is the held one.
        let held = round % 3;
        for c in &copies {
            put(c, value);
        }
        let (holding, others_first) = (AtomicBool::new(false), AtomicBool::new(false));
        std::thread::scope(|s| {
            s.spawn(|| {
                let _held = copies[held].shadow.hold_merging();
                holding.store(true, Ordering::Release);
                // Let go when the others hold the title's word, or after 10 s (then the test fails).
                let began = std::time::Instant::now();
                while began.elapsed() < std::time::Duration::from_secs(10) {
                    if copies.iter().enumerate().filter(|(i, _)| *i != held).all(|(_, c)| word(c.data) == value) {
                        others_first.store(true, Ordering::Release);
                        break;
                    }
                    std::thread::sleep(std::time::Duration::from_micros(200));
                }
            });
            while !holding.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            let left = merge_under_try_first(spans(), &mut runs);
            assert!(left.is_empty(), "round {round}");
        });
        assert!(others_first.load(Ordering::Acquire), "round {round}: the held copy was waited for first");
        for c in &copies {
            assert_eq!(word(c.data), value, "round {round}: copy {}", c.class);
        }
    }
}

/// The title's bytes put in under shader writes, for a copy of 174 MB
/// whose shader writes were declared as 40 ranges that overlap, with 12
/// pages written since. Run with `--ignored --nocapture`, and with
/// `VICTORY_LIGHT_REFRESH=0` for the way it was.
#[test]
#[ignore]
fn bytes_under_overlapping_ranges() {
    const BIG: usize = 174 << 20;
    let big = title(BIG, true);
    // SAFETY (both): the test's own memory.
    unsafe { std::ptr::write_bytes(big as *mut u8, 1, BIG) };
    let copy = Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true);
    copy.update(big, BIG);
    copy.take_dirty();
    for i in 0..40 {
        copy.writing(big + i * 5920, BIG - i * 5920);
    }
    let mut best = f64::MAX;
    for round in 0..30 {
        for i in 0..12 {
            unsafe { ((big + (i * 211 + round) % 2700 * PAGE + 64) as *mut u8).write_volatile(round as u8) };
        }
        let started = std::time::Instant::now();
        let mut runs = usize::MAX;
        assert!(!copy.merge_under_writes(big, BIG, &mut runs));
        best = best.min(started.elapsed().as_secs_f64() * 1e3);
        copy.take_dirty();
    }
    println!("VICTORY_LIGHT_REFRESH {}: best {best:.3} ms", crate::watch::light());
}

/// A range nothing wrote is known to be as it was, at its ends too, and
/// is so no more once something wrote it.
#[test]
fn unchanged_ranges_are_known() {
    for view in [false, true] {
        let region = title(8 * PAGE, view);
        // (A page read for the first time counts as written.)
        // SAFETY: the test's own memory.
        unsafe { std::ptr::write_bytes(region as *mut u8, 1, 8 * PAGE) };
        let data = memory(8 * PAGE);
        let copy = Shadow::unbacked(region as u64, data, (8 * PAGE) as u64, false);
        // Ends in the middle of pages.
        let (at, len) = (region + 500, 5 * PAGE);
        let look = |expected: bool, what: &str| {
            // The submission's one asking, which takes two ranges. (Again
            // if a submission of another test ended it.)
            let mut known = false;
            for _ in 0..100 {
                crate::watch::ask_once(&[(at, at + len), (region + 7 * PAGE, region + 8 * PAGE)]);
                known = copy.nothing_new(at, len);
                if known || !expected {
                    break;
                }
            }
            assert_eq!(known, expected, "view {view}: {what}");
            let copied = copy.update(at, len);
            crate::watch::ask_once_end();
            copied
        };
        assert_eq!(look(false, "never taken"), len);
        assert_eq!(look(true, "taken"), 0);
        assert_eq!(look(true, "taken"), 0);
        // SAFETY: the test's own memory.
        unsafe { ((region + 600) as *mut u8).write_volatile(1) };
        // (The kernel names the 4 KiB, the guards only the block.)
        assert_eq!(look(false, "written at its end"), if view { PAGE } else { 4096 } - 500);
        assert_eq!(look(true, "taken again"), 0);
        // Outside the range, on its last page.
        // SAFETY: as above.
        unsafe { ((region + 5 * PAGE + 600) as *mut u8).write_volatile(1) };
        assert_eq!(look(false, "written on its last page"), 500);
        // A longer range was never taken whole.
        assert!(!copy.nothing_new(region, 6 * PAGE));
    }
}

/// Of a copy in the card's memory only the dwords title memory changed go
/// up, and none beside them, not even between two in 64 bytes; a page
/// never taken goes up whole.
#[test]
fn only_what_differs_goes_up() {
    let region = title(4 * PAGE, true);
    let data = memory(4 * PAGE);
    let copy = Shadow::unbacked(region as u64, data, (4 * PAGE) as u64, true);
    copy.update(region, 4 * PAGE);
    assert_eq!(copy.take_dirty(), [(0, 4 * PAGE)], "all of it, though zeros are no change");
    // What the relay does: shader writes into title memory, and into the
    // CPU's side.
    crate::watch::landing(region + PAGE + 640, 64);
    // SAFETY: the test's own memory.
    unsafe {
        std::ptr::write_bytes((region + PAGE + 640) as *mut u8, 7, 64);
        std::ptr::write_bytes(data.add(PAGE + 640), 7, 64);
    }
    copy.update(region, 4 * PAGE);
    let again = copy.take_dirty();
    // SAFETY: as above.
    unsafe {
        ((region + 2 * PAGE + 129) as *mut u8).write_volatile(9);
        ((region + 2 * PAGE + 136) as *mut u32).write_volatile(9);
        ((region + 2 * PAGE + 140) as *mut u32).write_volatile(9);
        // The value that was there.
        ((region + 2 * PAGE + 132) as *mut u32).write_volatile(0);
    }
    copy.update(region, 4 * PAGE);
    let written = copy.take_dirty();
    if std::env::var_os("VICTORY_SEND_CHANGED").is_some_and(|v| v != "0") {
        assert_eq!(again, []);
        assert_eq!(written, [(2 * PAGE + 128, 2 * PAGE + 132), (2 * PAGE + 136, 2 * PAGE + 144)]);
    } else {
        assert_eq!(again, [(PAGE, 2 * PAGE)]);
        assert_eq!(written, [(2 * PAGE, 3 * PAGE)]);
    }
    // SAFETY: as above.
    assert_eq!(unsafe { data.add(2 * PAGE + 129).read() }, 9);
}

/// The refreshes of a frame as run record-1 had them: 30 submissions, the
/// first with a copy for each of 4,400 ranges, the others with 60 and a
/// big copy that 55 landed pieces and 9 title writes opened, while a
/// thread lands what shaders wrote. Run with `--ignored --nocapture`,
/// and with `VICTORY_LIGHT_REFRESH=0` for the way it was.
#[test]
#[ignore]
fn frame_of_refreshes() {
    let mut random = Random(0x9876_5432_1234_5671);
    const EACH: usize = 224 << 10;
    const BIG: usize = 174 << 20;
    let watched = title(3000 * EACH, false);
    // `VICTORY_BENCH_HOLES=0`: views without holes.
    let holes = std::env::var_os("VICTORY_BENCH_HOLES").is_none_or(|v| v != "0");
    let views = if holes { with_holes(1400 * EACH / PAGE) } else { title(1400 * EACH, true) };
    let there = |at: usize| !holes || (at - views) / PAGE % 4 != 3;
    let big = title(BIG, true);
    // SAFETY: the test's own memory.
    unsafe {
        std::ptr::write_bytes(watched as *mut u8, 1, 3000 * EACH);
        for at in (views..views + 1400 * EACH).step_by(PAGE).filter(|&at| there(at)) {
            std::ptr::write_bytes(at as *mut u8, 1, PAGE);
        }
        std::ptr::write_bytes(big as *mut u8, 1, BIG);
    }
    let mut small = Vec::new();
    for i in 0..4400 {
        let (start, at) = if i < 3000 { (watched, i * EACH) } else { (views, (i - 3000) * EACH) };
        let data = memory(EACH);
        // The range views read, not all of the copy.
        small.push((Shadow::unbacked((start + at) as u64, data, EACH as u64, false), start + at + 4000, EACH - 9000));
    }
    let heap = (Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true), big, BIG);
    // The same memory, as another copy: what is landed is landed through
    // it, and the refreshes are of the first.
    let lands = Shadow::unbacked(big as u64, memory(BIG), BIG as u64, true);
    let (mut best, mut asking) = ([f64::MAX; 2], [0f64; 2]);
    let (mut sum, mut landing) = (0.0, 0.0);
    const FRAMES: usize = 12;
    for frame in 0..FRAMES {
        let mut took = [0f64; 2];
        for i in 0..30 {
            // The title's writes, and the relay's beside the submission.
            for _ in 0..9 {
                let at = views + random.below(1400 * EACH / 8) * 8;
                if there(at) {
                    // SAFETY: the test's own memory.
                    unsafe { (at as *mut u64).write_volatile(frame as u64) };
                }
            }
            for _ in 0..60 {
                let at = watched + random.below(3000 * EACH / 8) * 8;
                // SAFETY: the test's own memory.
                unsafe { (at as *mut u64).write_volatile(frame as u64) };
            }
            let first = random.below(BIG / PAGE - 60);
            let lands = lands.clone();
            let land = std::thread::spawn(move || {
                let now = vec![frame as u8 ^ i as u8 ^ 0x55; PAGE];
                let started = std::time::Instant::now();
                for pieces in (first..first + 55).collect::<Vec<_>>().chunks(4) {
                    let pieces: Vec<_> =
                        pieces.iter().map(|p| (p * PAGE + 8, (p + 1) * PAGE - 8, now.as_ptr() as usize)).collect();
                    let mut notes = crate::land::Notes::default();
                    // SAFETY: the bytes of `now`; nothing else lands here.
                    unsafe { lands.land_card(&pieces, 0, &mut notes) };
                    notes.flush();
                }
                started.elapsed().as_secs_f64() * 1e3
            });
            let ranges: Vec<(Arc<Shadow>, usize, usize)> = if i == 0 {
                small.iter().cloned().chain([heap.clone()]).collect()
            } else {
                (0..60).map(|_| small[random.below(4400)].clone()).chain([heap.clone()]).collect()
            };
            let started = std::time::Instant::now();
            submission(&ranges, 3);
            took[usize::from(i != 0)] += started.elapsed().as_secs_f64() * 1e3;
            let put = land.join().unwrap();
            if frame >= 2 {
                landing += put;
            }
            heap.0.take_dirty();
        }
        if frame >= 2 {
            sum += took[0] + took[1];
            for k in 0..2 {
                best[k] = best[k].min(took[k]);
                asking[k] += took[k];
            }
        }
    }
    println!(
        "VICTORY_LIGHT_REFRESH {}: a frame's refreshes {:.2} ms in the mean; the submission of all copies {:.2} (best {:.2}), the 29 \
         others {:.2} (best {:.2}); the landing thread's puts {:.2}",
        crate::watch::light(),
        sum / (FRAMES - 2) as f64,
        asking[0] / (FRAMES - 2) as f64,
        best[0],
        asking[1] / (FRAMES - 2) as f64,
        best[1],
        landing / (FRAMES - 2) as f64,
    );
    #[cfg(feature = "diagnostics")]
    println!("per frame, as `crate::watch::asks`: {:?}", crate::watch::asks().map(|n| n / FRAMES as u64));
}
