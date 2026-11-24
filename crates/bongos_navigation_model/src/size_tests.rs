use super::*;

#[test]
fn a_preview_is_allowed_at_exactly_the_limit_and_the_label_shows_the_size() {
    assert!(preview_allowed(1_000_000, 1_000_000));
    assert!(!preview_allowed(1_000_001, 1_000_000));
    assert_eq!(skipped_label(3_400_000), "Preview skipped: 3.4 MB");
    assert_eq!(skipped_label(512), "Preview skipped: 512 B");
}
