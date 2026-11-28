use super::{flagged_pieces, landable};

/// Two flights and what follows each: nothing of the second lands
/// before the card has run it, and nothing behind a flight lands
/// before the flight.
#[test]
fn flights_land_in_the_order_they_were_sent() {
    // Flight (fence 5), its write, a signal, flight (fence 6), its write.
    let line = [Some(5), None, None, Some(6), None];
    assert_eq!(landable(&line, 4), 0);
    assert_eq!(landable(&line, 5), 3);
    assert_eq!(landable(&line, 6), 5);
    assert_eq!(landable(&line, u64::MAX), 5);
    // Signals with no flight ahead land at once; behind one they wait.
    assert_eq!(landable(&[None, None], 0), 2);
    assert_eq!(landable(&[None, Some(9), None], 8), 1);
    assert_eq!(landable(&[], 3), 0);
    // After the first three landed, the rest when its turn comes.
    assert_eq!(landable(&line[3..], 5), 0);
    assert_eq!(landable(&line[3..], 6), 2);
}

/// A piece flagged under one range of a plan is fetched under every
/// range that holds a part of it; pieces of other copies are not.
#[test]
fn a_piece_flagged_under_one_range_is_fetched_under_all() {
    let (a, b) = (0x1000usize, 0x2000usize);
    // Copy a: piece 3 under two ranges, flagged under the first only;
    // piece 4 not flagged; copy b: piece 3 not flagged, piece 9 flagged.
    let flags = [(a, 3, true), (a, 4, false), (a, 3, false), (b, 3, false), (b, 9, true)];
    let fetch = flagged_pieces(&flags);
    assert!(fetch.contains(&(a, 3)));
    assert!(!fetch.contains(&(a, 4)));
    assert!(!fetch.contains(&(b, 3)));
    assert!(fetch.contains(&(b, 9)));
    assert_eq!(fetch.len(), 2);
}
