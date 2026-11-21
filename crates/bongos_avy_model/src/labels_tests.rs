use crate::labels::{HOME_ROW, Resolved, labels, resolve};

fn strings(items: &[&str]) -> Vec<String> {
    items.iter().map(|s| s.to_string()).collect()
}

#[test]
fn nine_candidates_get_single_keys() {
    assert_eq!(
        labels(9, &HOME_ROW),
        strings(&["a", "s", "d", "f", "g", "h", "j", "k", "l"])
    );
    assert!(labels(0, &HOME_ROW).is_empty());
    assert!(labels(3, &[]).is_empty());
}

#[test]
fn ten_candidates_give_eight_singles_then_two_pairs() {
    assert_eq!(
        labels(10, &HOME_ROW),
        strings(&["a", "s", "d", "f", "g", "h", "j", "k", "la", "ls"])
    );
}

#[test]
fn hundred_candidates_are_prefix_free_and_shorter_first() {
    let all = labels(100, &HOME_ROW);
    assert_eq!(all.len(), 100);
    assert_eq!(all.iter().map(|l| l.len()).max(), Some(3));
    for (i, a) in all.iter().enumerate() {
        for (j, b) in all.iter().enumerate() {
            assert!(i == j || !b.starts_with(a.as_str()), "{a} prefixes {b}");
        }
    }
    assert!(all.windows(2).all(|w| w[0].len() <= w[1].len()));
}

#[test]
fn resolve_chosen_prefix_and_no_match() {
    let all = labels(10, &HOME_ROW);
    assert_eq!(resolve(&all, "d"), Resolved::Chosen(2));
    assert_eq!(resolve(&all, "ls"), Resolved::Chosen(9));
    assert_eq!(resolve(&all, "l"), Resolved::Prefix);
    assert_eq!(resolve(&all, ""), Resolved::Prefix);
    assert_eq!(resolve(&all, "x"), Resolved::NoMatch);
    assert_eq!(resolve(&all, "lx"), Resolved::NoMatch);
}
