use super::{
    Flight, Late, Line, PendingFlight, continues, cut_before_wait, join_ranges, line, lists_before_wait, parts_around, rebuilt, split, touches,
};

/// A submission registered before it is a flight is in `written` at once
/// (what a later submission's reads of title memory are checked against),
/// stays there whenever `written` is made again (a landing does), and goes
/// when it is a flight in the line.
#[test]
fn a_registered_submission_stays_in_written_until_it_is_a_flight() {
    let flight = |ranges: Vec<(usize, usize)>| {
        Late::Run(Flight {
            number: 1,
            fence: 1,
            lists: Vec::new(),
            order: Vec::new(),
            card: None,
            back: Vec::new(),
            own: None,
            texture_back_ranges: ranges,
        })
    };
    let mut line = Line::new();
    // A flight in the air.
    line.line.push(flight(vec![(100, 200)]));
    join_ranges(&mut line.written, [(100, 200)]);
    let id = line.register_pending(vec![(500, 600), (150, 250)]);
    assert_eq!(line.written, [(100, 250), (500, 600)]);
    assert!(touches(&line.written, 520, 530), "a read there waits for it");
    // A landing makes `written` again from the flights that are still in
    // the air: what landed goes, the registration stays.
    line.line.clear();
    line.rebuild_written([(900, 950)]);
    assert_eq!(line.written, [(150, 250), (500, 600), (900, 950)]);
    // Nothing in the air: only the registration.
    line.rebuild_written(std::iter::empty());
    assert_eq!(line.written, [(150, 250), (500, 600)]);
    // The flight is in the line (the push joins its ranges, the same
    // ones), and the registration ends: nothing is lost or doubled.
    line.line.push(flight(vec![(150, 250), (500, 600)]));
    join_ranges(&mut line.written, [(150, 250), (500, 600)]);
    line.clear_pending(id);
    assert_eq!(line.written, [(150, 250), (500, 600)]);
    line.rebuild_written([(150, 250), (500, 600)]);
    assert_eq!(line.written, [(150, 250), (500, 600)]);
    // It lands: nothing is left.
    line.line.clear();
    line.rebuild_written(std::iter::empty());
    assert!(line.written.is_empty());
}

/// Two submissions registered together end alone, whichever goes first.
#[test]
fn registered_submissions_end_alone() {
    let mut line = Line::new();
    let first = line.register_pending(vec![(10, 20)]);
    let second = line.register_pending(vec![(30, 40)]);
    assert_ne!(first, second);
    // (Nothing in the air: ending one rebuilds `written` from the other.)
    line.clear_pending(first);
    assert_eq!(line.written, [(30, 40)]);
    line.clear_pending(second);
    assert!(line.written.is_empty());
    // Ending one twice, or one that was never there, does nothing.
    line.clear_pending(second);
    line.clear_pending(99);
    // Any rebuilding goes through `rebuilt`: with nothing registered it is
    // the flights' ranges joined.
    let mut written = vec![(1, 2)];
    rebuilt(&mut written, [(5, 9), (7, 12), (20, 30)], &[]);
    assert_eq!(written, [(5, 12), (20, 30)]);
}

/// A registration that is dropped, not made a flight (the preparing did
/// not get that far), ends in the line too.
#[test]
fn a_registration_dropped_before_it_is_a_flight_ends() {
    // Unusual addresses: the line is the process's.
    let ours = |line: &Line| line.pending.iter().any(|p| p.1 == [(0x7000_0000_0000, 0x7000_0000_1000)]);
    let held = PendingFlight::register(vec![(0x7000_0000_0000, 0x7000_0000_1000)]);
    assert!(ours(&line()));
    assert!(touches(&line().written, 0x7000_0000_0800, 0x7000_0000_0801));
    drop(held);
    assert!(!ours(&line()));
    // With nothing in the air its ranges left `written` too.
    {
        let line = line();
        if line.line.is_empty() && line.in_hand.is_empty() && !line.settling {
            assert!(!touches(&line.written, 0x7000_0000_0800, 0x7000_0000_0801));
        }
    }
    // Done under the lock the push holds: the same.
    let held = PendingFlight::register(vec![(0x7000_0000_0000, 0x7000_0000_1000)]);
    {
        let mut line = line();
        let Line { pending, pending_copies, .. } = &mut *line;
        held.done(pending, pending_copies);
    }
    assert!(!ours(&line()));
}

/// Parts of a joined submission (lists 0..12, parts beginning at 3, 5, 9)
/// are cut in front of the first that waits, and only if the whole does.
#[test]
fn a_joined_submission_is_cut_in_front_of_the_part_that_waits() {
    let parts = [3, 5, 9];
    // What waits grows with the lists taken: from the list `from` on.
    let waits = |from: usize| move |n: usize| n > from;
    // The part at 9 reads from flights (a list at 10): cut at 9.
    assert_eq!(cut_before_wait(&parts, 12, waits(9)), Some(9));
    // The part at 5 (list 6): the parts before it go, cut at 5.
    assert_eq!(cut_before_wait(&parts, 12, waits(5)), Some(5));
    // The first part itself (a list at 1 or 2): nothing in front of it.
    assert_eq!(cut_before_wait(&parts, 12, waits(1)), None);
    assert_eq!(cut_before_wait(&parts, 12, waits(2)), None);
    // The list at 3 is the first of the second part: the first part
    // (lists 0..3) goes alone.
    assert_eq!(cut_before_wait(&parts, 12, waits(3)), Some(3));
    assert_eq!(cut_before_wait(&parts, 12, waits(4)), Some(3));
    // Nothing waits, or one part only: no cut.
    assert_eq!(cut_before_wait(&parts, 12, |_| false), None);
    assert_eq!(cut_before_wait(&[], 12, |_| true), None);
    // Waiting from the first list on: nothing is in front of it.
    assert_eq!(cut_before_wait(&parts, 12, |n| n > 0), None);
}

/// Between lists the cut is the most lists that do not wait, if the whole does.
#[test]
fn a_submission_is_cut_after_the_last_list_that_does_not_wait() {
    let waits = |from: usize| move |n: usize| n > from;
    assert_eq!(lists_before_wait(12, waits(5)), Some(5));
    assert_eq!(lists_before_wait(12, waits(11)), Some(11));
    assert_eq!(lists_before_wait(12, waits(0)), None);
    assert_eq!(lists_before_wait(12, waits(12)), None);
    assert_eq!(lists_before_wait(1, |_| true), None);
    assert_eq!(lists_before_wait(2, waits(1)), Some(1));
}

/// Cut at a place where a part begins, the two halves have the parts
/// that begin in them, and the lists they run are the whole's, in order.
#[test]
fn the_halves_of_a_cut_submission_run_the_same_lists_in_the_same_order() {
    let lists = vec!['a', 'b', 'c', 'd', 'e', 'f'];
    // The order runs list 4, then 0, 1, 2, 3, 5; parts begin at 2, 4.
    let order = vec![4, 0, 1, 2, 3, 5];
    let parts = [2, 4];
    let cut = 4;
    let (first, second) = parts_around(&parts, cut);
    assert_eq!(first, [2]);
    assert!(second.is_empty());
    let Ok([(la, oa), (lb, ob)]) = split(lists.clone(), order.clone(), cut) else {
        panic!("no cut");
    };
    let run = |lists: &[char], order: &[usize]| order.iter().map(|&i| lists[i]).collect::<Vec<_>>();
    let mut both = run(&la, &oa);
    both.extend(run(&lb, &ob));
    assert_eq!(both, run(&lists, &order));
    assert_eq!(run(&la, &oa), ['e', 'a', 'b', 'c']);
    // A cut in the middle of the parts keeps the later ones, from 0.
    let (first, second) = parts_around(&[2, 4, 7, 9], 4);
    assert_eq!((first, second), (vec![2], vec![3, 5]));
    // A cut inside a part: the part is the second half's first, begun at 0.
    let (first, second) = parts_around(&[2, 4, 7, 9], 5);
    assert_eq!((first, second), (vec![2, 4], vec![2, 4]));
}

/// Copies join only when both ends continue and the whole does not
/// overlap itself.
#[test]
fn copies_join_only_end_to_end_and_apart() {
    // (destination, source, bytes)
    assert!(continues((1000, 5000, 16), (1016, 5016, 32)));
    assert!(continues((5000, 1000, 16), (5016, 1016, 32)));
    // A gap, or only one side continues.
    assert!(!continues((1000, 5000, 16), (1020, 5016, 32)));
    assert!(!continues((1000, 5000, 16), (1016, 5020, 32)));
    assert!(!continues((1000, 5000, 16), (1000, 5000, 16)));
    // Together they would overlap: the second reads what the first wrote.
    assert!(!continues((1000, 1016, 16), (1016, 1032, 16)));
    assert!(!continues((1016, 1000, 16), (1032, 1016, 16)));
    // They touch without overlapping.
    assert!(continues((1000, 1032, 16), (1016, 1048, 16)));
    assert!(!continues((usize::MAX - 8, 0, 8), (0, 8, 8)));
}

/// Ranges are found by what they hold, not by where they start, and a
/// range that only touches another's end does not touch it.
#[test]
fn ranges_touch_where_they_overlap() {
    let mut ranges = Vec::new();
    join_ranges(&mut ranges, [(40, 50), (10, 20), (20, 25), (45, 60), (7, 7)]);
    assert_eq!(ranges, [(10, 25), (40, 60)]);
    join_ranges(&mut ranges, [(25, 40)]);
    assert_eq!(ranges, [(10, 60)]);
    let ranges = [(10usize, 25usize), (40, 60), (100, 101)];
    for (from, to, hit) in [
        (0, 10, false),
        (0, 11, true),
        (24, 25, true),
        (25, 40, false),
        (25, 41, true),
        (59, 200, true),
        (60, 100, false),
        (100, 101, true),
        (101, 500, false),
        (30, 30, false),
        (50, 50, false),
    ] {
        assert_eq!(touches(&ranges, from, to), hit, "{from}..{to}");
    }
    assert!(!touches(&[], 0, usize::MAX));
}
