use crate::mark_ring::{MARK_RING_MAX, MarkRing};

fn push(ring: &mut MarkRing<usize>, n: usize) -> bool {
    ring.push(n, |a, b| a == b)
}

fn newest_first(ring: &MarkRing<usize>) -> Vec<usize> {
    ring.iter_newest_first().copied().collect()
}

#[test]
fn push_and_pop_run_newest_first() {
    let mut ring = MarkRing::new();
    for n in [1, 2, 3] {
        assert!(push(&mut ring, n));
    }
    assert_eq!(ring.len(), 3);
    assert_eq!(ring.pop(), Some(3));
    assert_eq!(ring.pop(), Some(2));
    assert_eq!(ring.pop(), Some(1));
    assert!(ring.is_empty());
}

#[test]
fn seventeenth_push_drops_the_oldest() {
    let mut ring = MarkRing::new();
    for n in 0..MARK_RING_MAX {
        push(&mut ring, n);
    }
    assert_eq!(ring.len(), MARK_RING_MAX);
    assert!(push(&mut ring, 100));
    assert_eq!(ring.len(), MARK_RING_MAX);
    let all = newest_first(&ring);
    assert_eq!(all.first(), Some(&100));
    assert_eq!(all.last(), Some(&1));
}

#[test]
fn duplicate_of_the_newest_is_skipped() {
    let mut ring = MarkRing::new();
    assert!(push(&mut ring, 5));
    assert!(!push(&mut ring, 5));
    assert_eq!(ring.len(), 1);
}

#[test]
fn non_adjacent_duplicates_are_allowed() {
    let mut ring = MarkRing::new();
    push(&mut ring, 5);
    push(&mut ring, 6);
    assert!(push(&mut ring, 5));
    assert_eq!(newest_first(&ring), vec![5, 6, 5]);
}

#[test]
fn retain_drops_dead_entries_and_keeps_order() {
    let mut ring = MarkRing::new();
    for n in 1..=6 {
        push(&mut ring, n);
    }
    ring.retain(|n| n % 2 == 0);
    assert_eq!(newest_first(&ring), vec![6, 4, 2]);
}

#[test]
fn pop_on_empty_is_none() {
    let mut ring: MarkRing<usize> = MarkRing::default();
    assert_eq!(ring.pop(), None);
    assert!(ring.is_empty());
}
