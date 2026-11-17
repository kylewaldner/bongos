use super::parse_stat;

#[test]
fn stat_fields_count_after_the_command_name() {
    let text = "4242 (a (weird) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 \
                987654 1000 10 18446744073709551615";
    let stat = parse_stat(text).unwrap();
    assert_eq!(stat.state, 'S');
    assert_eq!(stat.pgrp, 4242);
    assert_eq!(stat.start_time, 987654);
}
