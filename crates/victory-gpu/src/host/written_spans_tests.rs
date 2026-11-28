use super::{Touch, front_spans, front_spans_flat, front_spans_gap, longest_first, referenced_bytes, split, written_spans};

/// The spans made without a list for each copy are those that are made with
/// one, in the same order, whatever the steps: refreshes of many copies and
/// of one on end, copies given to shaders, parts, empty ranges, any gap.
#[test]
fn spans_made_flat_are_the_spans_made_by_copy() {
    let mut seed = 0x1234_5678_9abc_def1u64;
    let mut random = move |n: usize| {
        seed ^= seed << 13;
        seed ^= seed >> 7;
        seed ^= seed << 17;
        (seed >> 11) as usize % n
    };
    let (mut spans_seen, mut empty) = (0, 0);
    for trial in 0..3000 {
        let copies = 1 + random(12);
        let steps: Vec<(usize, usize, usize, usize)> = (0..random(60))
            .map(|_| (random(10), 1 + random(copies), random(400), random(100)))
            .collect();
        let make = || {
            steps.clone().into_iter().map(|(kind, copy, from, len)| match kind {
                0..6 => Touch::Refresh(copy * 0x1000, from, from + len),
                6 => Touch::Write(copy * 0x1000),
                7 => Touch::Part,
                8 => Touch::Title,
                _ => Touch::Nothing,
            })
        };
        for gap in [0, 1, 50, 64 << 10, usize::MAX] {
            let by_copy = front_spans_gap(make(), gap);
            let flat = front_spans_flat(make(), gap);
            assert_eq!(flat, by_copy, "trial {trial} gap {gap}: {steps:?}");
            spans_seen += by_copy.len();
            empty += usize::from(by_copy.is_empty());
        }
    }
    assert!(spans_seen > 5000 && empty > 20, "{spans_seen} {empty}");
}

/// What the steps refer to is the bytes of the joined ranges of each copy; the
/// span per copy is from the lowest to the highest.
#[test]
fn referenced_bytes_are_the_joined_ranges_of_each_copy() {
    use Touch::{Nothing, Refresh, Write};
    let touches = [Refresh(1, 0, 100), Refresh(1, 50, 120), Nothing, Refresh(1, 1000, 1010), Refresh(2, 0, 10), Write(1), Refresh(2, 10, 20), Refresh(2, 5, 6)];
    // Copy 1: 0..120 and 1000..1010 (130); copy 2: 0..20.
    assert_eq!(referenced_bytes(&touches), (150, 6));
    let found = front_spans(touches.into_iter());
    let span: usize = found.iter().map(|s| s.2 - s.1).sum();
    assert!(span > 1000, "the spans are wider: {found:?}");
    assert_eq!(referenced_bytes(&[]), (0, 0));
}

/// Narrowed spans: one per run of what the steps refer to; the union (gap
/// unbounded) is what it was; with no gap every referenced byte and no other
/// is in a span, and with a gap what is between runs that lie near is too.
#[test]
fn narrow_spans_hold_what_the_steps_refer_to_and_the_union_is_as_before() {
    use super::{front_spans_gap, joined_with_gap, written_spans_gap};
    use Touch::{Nothing, Part, Refresh, Write};
    let steps = || {
        vec![
            Refresh(1, 0, 100),
            Refresh(1, 150, 200),
            Refresh(1, 10_000, 10_100),
            Refresh(2, 5, 6),
            Part,
            Write(2),
            Refresh(2, 900, 910),
            Nothing,
            Write(1),
            Refresh(1, 20_000, 20_010),
            Refresh(3, 0, 8),
            Write(3),
        ]
    };
    // The union, as before: one span per copy.
    let mut union = front_spans(steps().into_iter());
    union.sort_unstable();
    assert_eq!(union, [(1, 0, 10_100), (2, 5, 6), (3, 0, 8)]);
    // No gap: the runs, each byte referred to (before the copy was given to shaders) once.
    let mut exact = front_spans_gap(steps().into_iter(), 0);
    exact.sort_unstable();
    assert_eq!(exact, [(1, 0, 100), (1, 150, 200), (1, 10_000, 10_100), (2, 5, 6), (3, 0, 8)]);
    // A gap of 50 joins the first two of copy 1.
    let mut near = front_spans_gap(steps().into_iter(), 50);
    near.sort_unstable();
    assert_eq!(near, [(1, 0, 200), (1, 10_000, 10_100), (2, 5, 6), (3, 0, 8)]);
    // Written ahead: the same for the copies given to shaders before the first title write.
    let mut written = written_spans_gap(steps().into_iter(), 0);
    written.sort_unstable();
    assert_eq!(written, [(1, 0, 100), (1, 150, 200), (1, 10_000, 10_100), (2, 5, 6), (3, 0, 8)]);
    let mut union_written = written_spans(steps().into_iter());
    union_written.sort_unstable();
    assert_eq!(union_written, [(1, 0, 10_100), (2, 5, 6), (3, 0, 8)]);
    // Every narrow span lies in the union of its copy.
    for (copy, a, b) in exact {
        assert!(union.iter().any(|&(c, from, to)| c == copy && from <= a && b <= to));
    }
    assert_eq!(joined_with_gap(vec![(10, 20), (0, 5), (21, 30)], 0), [(0, 5), (10, 20), (21, 30)]);
    assert_eq!(joined_with_gap(vec![(10, 20), (0, 5), (21, 30)], 1), [(0, 5), (10, 30)]);
    assert_eq!(joined_with_gap(vec![(10, 20), (0, 5), (21, 30)], usize::MAX), [(0, 30)]);
}

/// The longest span goes first, equal ones in the order they were.
#[test]
fn spans_go_longest_first_and_equal_ones_keep_their_order() {
    let mut spans = vec![("a", 0, 10), ("b", 0, 100), ("c", 5, 15), ("d", 0, 50)];
    longest_first(&mut spans);
    assert_eq!(spans.iter().map(|s| s.0).collect::<Vec<_>>(), ["b", "d", "a", "c"]);
}

#[test]
fn everything_is_refreshed_in_front_but_what_a_part_gave_its_shaders() {
    use Touch::{Nothing, Part, Refresh, Title, Write};
    let found = front_spans(
        [
            Refresh(1, 10, 20),
            Refresh(1, 12, 25),
            Refresh(2, 0, 5),
            Nothing,
            Write(1),
            // (Around what the part's own shaders write.)
            Refresh(1, 0, 100),
            Write(3),
            Refresh(3, 1, 2),
            Title,
            Refresh(4, 7, 9),
            Part,
            // The next part's bytes under the first one's writes.
            Refresh(1, 30, 40),
            Write(4),
            Refresh(4, 0, 9),
        ]
        .into_iter(),
    );
    assert_eq!(found, [(1, 10, 40), (2, 0, 5), (4, 7, 9)]);
}

#[test]
fn a_submission_is_cut_between_its_lists() {
    // Lists c and e are in neither half's order: they go with the first.
    let Ok([first, second]) = split(vec!['a', 'b', 'c', 'd', 'e'], vec![1, 3, 0], 1) else {
        panic!("no list is named twice");
    };
    let run = |(lists, order): &(Vec<char>, Vec<usize>)| order.iter().map(|&i| lists[i]).collect::<String>();
    assert_eq!((run(&first), run(&second)), ("b".into(), "da".into()));
    assert_eq!(first.0.len() + second.0.len(), 5);
    assert!(split(vec!['a', 'b'], vec![0, 1, 0], 2).is_err());
    assert!(split(vec!['a', 'b'], vec![0, 1], 0).is_err());
}

#[test]
fn what_is_written_is_refreshed_ahead_up_to_its_write_and_to_title_writes() {
    use Touch::{Nothing, Refresh, Title, Write};
    let mut found = written_spans(
        [
            Refresh(1, 10, 20),
            Refresh(2, 0, 5),
            Nothing,
            Refresh(1, 30, 40),
            Write(1),
            Refresh(1, 0, 100),
            Write(3),
            Refresh(3, 1, 2),
            Refresh(4, 7, 9),
            Write(4),
            Title,
            Write(2),
        ]
        .into_iter(),
    );
    found.sort_unstable();
    assert_eq!(found, [(1, 10, 40), (4, 7, 9)]);
}
