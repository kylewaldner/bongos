use crate::range_merge::merge_ranges;

#[test]
fn test_disjoint_ranges_stay_apart() {
    assert_eq!(merge_ranges(vec![5..=6, 1..=2]), vec![1..=2, 5..=6]);
}

#[test]
fn test_overlapping_ranges_merge() {
    assert_eq!(merge_ranges(vec![1..=5, 3..=8]), vec![1..=8]);
}

#[test]
fn test_ranges_that_share_an_end_merge() {
    assert_eq!(merge_ranges(vec![1..=3, 3..=5]), vec![1..=5]);
}

#[test]
fn test_adjacent_ranges_merge() {
    assert_eq!(merge_ranges(vec![4..=6, 1..=3]), vec![1..=6]);
}

#[test]
fn test_contained_range_is_absorbed() {
    assert_eq!(merge_ranges(vec![1..=9, 2..=3]), vec![1..=9]);
}
