use super::{Batch, costliest_first, spans_by_copy, wide_each_by_copy, wide_each_by_copy_by};
use crate::shadow::Shadow;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use windows_sys::Win32::System::Memory::{MEM_COMMIT, MEM_RELEASE, MEM_RESERVE, PAGE_READWRITE, VirtualAlloc, VirtualFree};

const PAGE: usize = 64 << 10;
const MEM_WRITE_WATCH: u32 = 0x20_0000;

#[test]
fn spans_are_grouped_by_copy_in_order() {
    assert_eq!(spans_by_copy([5, 7, 5, 9, 7].into_iter()), vec![vec![0, 2], vec![1, 4], vec![3]]);
    assert!(spans_by_copy(std::iter::empty()).is_empty());
}

/// The copies go costliest first, each with all its spans in the order they had, and copies that cost the same in
/// the order of their first span; nothing is added or lost.
#[test]
fn the_copies_go_costliest_first_and_whole() {
    let mut groups = spans_by_copy([5, 7, 5, 9, 7, 11].into_iter());
    assert_eq!(groups, vec![vec![0, 2], vec![1, 4], vec![3], vec![5]]);
    // Span costs: the first copy 3 + 4, the second 10 + 1, the third 7, the fourth 7.
    costliest_first(&mut groups, &[3, 10, 4, 7, 1, 7]);
    assert_eq!(groups, vec![vec![1, 4], vec![0, 2], vec![3], vec![5]]);
    let mut seen: Vec<usize> = groups.iter().flatten().copied().collect();
    seen.sort_unstable();
    assert_eq!(seen, vec![0, 1, 2, 3, 4, 5]);
    // Nothing known: as they were.
    let mut same = spans_by_copy([1, 2, 1, 3].into_iter());
    costliest_first(&mut same, &[0, 0, 0, 0]);
    assert_eq!(same, vec![vec![0, 2], vec![1], vec![3]]);
    costliest_first(&mut [], &[]);
}

/// Whatever the order, every span is run once, and those of a copy one after the other, in their order.
#[test]
fn every_span_runs_once_in_whatever_order_the_copies_go() {
    let copies = [1usize, 2, 1, 3, 2, 3, 1];
    let cost: Vec<u64> = vec![5, 1, 5, 9, 1, 9, 5];
    let ran = Arc::new(std::sync::Mutex::new(Vec::new()));
    let (done, wait) = std::sync::mpsc::channel();
    {
        let ran = ran.clone();
        std::thread::spawn(move || {
            wide_each_by_copy_by(copies.into_iter(), Some(cost), move |i| {
                ran.lock().unwrap().push(i);
            });
            let _ = done.send(());
        });
    }
    assert!(wait.recv_timeout(std::time::Duration::from_secs(30)).is_ok());
    let ran = ran.lock().unwrap().clone();
    let mut all = ran.clone();
    all.sort_unstable();
    assert_eq!(all, vec![0, 1, 2, 3, 4, 5, 6], "each span once");
    for copy in 1..=3usize {
        let mine: Vec<usize> = ran.iter().copied().filter(|&i| copies[i] == copy).collect();
        let mut in_order = mine.clone();
        in_order.sort_unstable();
        assert_eq!(mine, in_order, "the spans of copy {copy} in turn, in their order");
    }
}

/// A copy's refreshes are timed: the mean follows what they take, and a copy not timed says 0.
#[test]
fn what_a_copys_refreshes_took_is_a_moving_mean() {
    let data = vec![0u8; 4096];
    let copy = Shadow::unbacked(0x10_0000, data.as_ptr() as *mut u8, 4096, false);
    assert_eq!(copy.refresh_cost(), 0);
    copy.note_refresh_cost(800);
    assert_eq!(copy.refresh_cost(), 800);
    copy.note_refresh_cost(0);
    assert_eq!(copy.refresh_cost(), 600);
    for _ in 0..40 {
        copy.note_refresh_cost(0);
    }
    assert_eq!(copy.refresh_cost(), 1, "never 0 once timed");
    copy.note_refresh_cost(u64::MAX);
    assert!(copy.refresh_cost() > 1_000_000_000);
    // A copy not timed is expected by its range.
    let fresh = Shadow::unbacked(0x20_0000, data.as_ptr() as *mut u8, 4096, false);
    assert_eq!(super::predicted_us(&fresh, 0, 8 << 20), 1024);
    assert_eq!(super::predicted_us(&copy, 0, 8 << 20), u64::from(copy.refresh_cost()));
}

/// Two spans of one copy (each of more than 1 MiB, which a refresh cuts in
/// parts on several threads) and the one span of another copy: the copies
/// end as the title's memory is, and the spans of the first are not run
/// in turn on two threads (they used to take the copy's lock one from
/// the other's thread).
#[test]
fn spans_of_one_copy_are_done_together_and_give_the_same_bytes() {
    assert!(crate::safecopy::on());
    const SPAN: usize = 24 * PAGE;
    let alloc = |len: usize| {
        // SAFETY: memory of ours, released at the end.
        let at = unsafe { VirtualAlloc(std::ptr::null(), len, MEM_RESERVE | MEM_COMMIT | MEM_WRITE_WATCH, PAGE_READWRITE) } as usize;
        assert_ne!(at, 0);
        for page in 0..len / PAGE {
            // SAFETY: inside the allocation.
            unsafe { ((at + page * PAGE + 11) as *mut u8).write_volatile(3 + page as u8) };
        }
        at
    };
    let (a, b) = (alloc(2 * SPAN), alloc(SPAN));
    let mut sides = [vec![0u8; 2 * SPAN], vec![0u8; SPAN]];
    let copies = [
        Shadow::unbacked(a as u64, sides[0].as_mut_ptr(), (2 * SPAN) as u64, false),
        Shadow::unbacked(b as u64, sides[1].as_mut_ptr(), SPAN as u64, false),
    ];
    // Spans: the first copy's two, the other's one in between them.
    let spans: Vec<(Arc<Shadow>, usize, usize)> =
        vec![(copies[0].clone(), a, a + SPAN), (copies[1].clone(), b, b + SPAN), (copies[0].clone(), a + SPAN, a + 2 * SPAN)];
    let keys: Vec<usize> = spans.iter().map(|s| Arc::as_ptr(&s.0) as usize).collect();
    let (done, wait) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        wide_each_by_copy(keys.into_iter(), move |i| {
            let (shadow, from, to) = &spans[i];
            Batch::update(shadow, *from, *to);
        });
        let _ = done.send(());
    });
    assert!(wait.recv_timeout(std::time::Duration::from_secs(30)).is_ok(), "the refreshes did not end");
    for page in 0..2 * SPAN / PAGE {
        assert_eq!(sides[0][page * PAGE + 11], 3 + page as u8, "first copy, page {page}");
    }
    for page in 0..SPAN / PAGE {
        assert_eq!(sides[1][page * PAGE + 11], 3 + page as u8, "second copy, page {page}");
    }
    // SAFETY: ours.
    unsafe {
        VirtualFree(a as *mut _, 0, MEM_RELEASE);
        VirtualFree(b as *mut _, 0, MEM_RELEASE);
    }
}

/// The jobs of different copies run at the same time (the first span of
/// each waits for the other to have begun: one thread doing them in turn
/// would wait for ever), and the spans of one copy never at the same time:
/// they are one job, one after the other.
#[test]
fn different_copies_run_at_the_same_time_and_the_spans_of_one_copy_in_turn() {
    let copies = [1usize, 2, 1, 1, 2];
    let began = Arc::new(AtomicUsize::new(0));
    let met = Arc::new(AtomicUsize::new(0));
    let running = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let most = Arc::new([AtomicUsize::new(0), AtomicUsize::new(0)]);
    let (done, wait) = std::sync::mpsc::channel();
    {
        let (began, met, running, most) = (began.clone(), met.clone(), running.clone(), most.clone());
        std::thread::spawn(move || {
            wide_each_by_copy(copies.into_iter(), move |i| {
                let copy = copies[i] - 1;
                let now = running[copy].fetch_add(1, Ordering::SeqCst) + 1;
                most[copy].fetch_max(now, Ordering::SeqCst);
                if i < 2 {
                    began.fetch_add(1, Ordering::SeqCst);
                    let waiting = std::time::Instant::now();
                    while began.load(Ordering::SeqCst) < 2 && waiting.elapsed().as_secs() < 20 {
                        std::thread::sleep(std::time::Duration::from_millis(1));
                    }
                    if began.load(Ordering::SeqCst) == 2 {
                        met.fetch_add(1, Ordering::SeqCst);
                    }
                }
                std::thread::sleep(std::time::Duration::from_millis(20));
                running[copy].fetch_sub(1, Ordering::SeqCst);
            });
            let _ = done.send(());
        });
    }
    assert!(wait.recv_timeout(std::time::Duration::from_secs(60)).is_ok());
    assert_eq!(met.load(Ordering::SeqCst), 2, "each of the two copies saw the other begin while it ran");
    assert_eq!(most[0].load(Ordering::SeqCst), 1, "the spans of the first copy were run at the same time");
    assert_eq!(most[1].load(Ordering::SeqCst), 1, "the spans of the second copy were run at the same time");
}
