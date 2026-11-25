use crate::leap_year::is_leap_year;

#[test]
fn test_a_year_divisible_by_four_is_a_leap_year() {
    assert!(is_leap_year(2024));
}

#[test]
fn test_a_year_not_divisible_by_four_is_not_a_leap_year() {
    assert!(!is_leap_year(2023));
}

#[test]
fn test_a_century_not_divisible_by_400_is_not_a_leap_year() {
    assert!(!is_leap_year(1900));
}

#[test]
fn test_a_century_divisible_by_400_is_a_leap_year() {
    assert!(is_leap_year(2000));
}
