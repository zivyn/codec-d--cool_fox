use super::Spare;

/// Kept things come back by size, the last given first, and nothing is
/// kept past the limit.
#[test]
fn kept_things_come_back_by_size_within_the_limit() {
    let mut spare: Spare<u32> = Spare::new();
    assert_eq!(spare.take(4), None);
    assert_eq!(spare.give(1, 4, 10), None);
    assert_eq!(spare.give(2, 1, 10), None);
    assert_eq!(spare.give(3, 4, 10), None);
    assert_eq!(spare.bytes, 9);
    // Past the limit: handed back, nothing changes.
    assert_eq!(spare.give(4, 4, 10), Some(4));
    assert_eq!(spare.bytes, 9);
    assert_eq!(spare.take(2), None, "no thing of that size");
    assert_eq!(spare.take(4), Some(3));
    assert_eq!(spare.take(4), Some(1));
    assert_eq!(spare.take(4), None);
    assert_eq!(spare.take(1), Some(2));
    assert_eq!(spare.bytes, 0);
    assert!(spare.kept.is_empty());
}
