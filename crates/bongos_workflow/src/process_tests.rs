use std::process::{Child, Command, Stdio};

use super::{ProcessRecord, matches, parse_stat, record_alive, start_time, sysinfo_stat};

#[test]
fn stat_fields_count_after_the_command_name() {
    let text = "4242 (a (weird) name) S 1 4242 4242 0 -1 4194560 100 0 0 0 1 2 0 0 20 0 1 0 \
                987654 1000 10 18446744073709551615";
    let stat = parse_stat(text).unwrap();
    assert_eq!(stat.state, 'S');
    assert_eq!(stat.pgrp, 4242);
    assert_eq!(stat.start_time, 987654);
}

/// A child that runs until its stdin closes. A plain std child: the test reaps it itself.
#[allow(clippy::disallowed_methods)]
fn waiting_child() -> Child {
    Command::new("cat")
        .stdin(Stdio::piped())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .expect("cat runs")
}

/// Closes the child's stdin and waits until it has exited without reaping it, so it stays a
/// zombie until `child.wait()`.
fn make_zombie(child: &mut Child) {
    drop(child.stdin.take());
    // SAFETY: waitid only fills `info`; WNOWAIT leaves the child waitable, so it is not reaped.
    let mut info: libc::siginfo_t = unsafe { std::mem::zeroed() };
    let result = unsafe {
        libc::waitid(
            libc::P_PID,
            child.id() as libc::id_t,
            &mut info,
            libc::WEXITED | libc::WNOWAIT,
        )
    };
    assert_eq!(result, 0, "waitid failed");
}

#[test]
fn a_zombie_does_not_count_as_alive() {
    let mut child = waiting_child();
    let pid = child.id();
    let record = ProcessRecord {
        pid,
        start_time: start_time(pid).expect("the child has a start time"),
    };
    assert!(record_alive(record));
    assert!(!record_alive(ProcessRecord {
        start_time: record.start_time + 1,
        ..record
    }));
    assert!(!record_alive(ProcessRecord {
        start_time: 0,
        ..record
    }));

    make_zombie(&mut child);
    assert_eq!(start_time(pid), Some(record.start_time), "still a zombie");
    assert!(!record_alive(record));

    child.wait().unwrap();
    assert!(!record_alive(record));
}

#[test]
fn the_sysinfo_probe_matches_its_own_start_time_and_sees_a_zombie() {
    let mut child = waiting_child();
    let pid = child.id();
    let seconds = sysinfo_stat(pid)
        .expect("sysinfo sees the child")
        .start_time;
    assert!(seconds > 0);
    let record = ProcessRecord {
        pid,
        start_time: seconds,
    };
    assert!(matches(sysinfo_stat(pid), record));
    assert!(!matches(
        sysinfo_stat(pid),
        ProcessRecord {
            start_time: seconds + 1,
            ..record
        }
    ));

    make_zombie(&mut child);
    assert!(!matches(sysinfo_stat(pid), record));

    child.wait().unwrap();
    assert!(!matches(sysinfo_stat(pid), record));
}
