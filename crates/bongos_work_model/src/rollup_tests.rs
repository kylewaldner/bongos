use std::collections::HashSet;

use crate::fixtures::*;
use crate::rollup::rollup;
use crate::store::Task;
use crate::*;

fn none() -> HashSet<TaskId> {
    HashSet::new()
}

fn twelve_leaves() -> Vec<Task> {
    let leaves = |prefix: &str, done: usize| -> Vec<Task> {
        (0..6)
            .map(|index| {
                let status = if index < done {
                    Status::Done
                } else {
                    Status::Open
                };
                leaf(&format!("{prefix}{index}"), status)
            })
            .collect()
    };
    vec![group("g1", leaves("a", 3)), group("g2", leaves("b", 1))]
}

#[test]
fn progress_counts_leaves_and_skips_cancelled() {
    let mut t = tree(twelve_leaves());
    let top = &t.projects[0].task;
    let r = rollup(top, &t, &none());
    assert_eq!((r.done, r.total), (4, 12));
    assert_eq!(r.progress_text(), "4/12");

    // Cancelling an open leaf drops it from the denominator.
    let g2 = t.projects[0].task.children.get_mut(1).unwrap();
    file_mut(g2.children.last_mut().unwrap()).status = Status::Cancelled;
    let r = rollup(&t.projects[0].task, &t, &none());
    assert_eq!(r.progress_text(), "4/11");
    // The group alone shows its own share.
    assert_eq!(
        rollup(&t.projects[0].task.children[0], &t, &none()).progress_text(),
        "3/6"
    );
}

#[test]
fn blocked_excludes_leaves_behind_a_moving_task() {
    let mut running = pr_ship("running");
    file_mut(&mut running).worktree = Some("/wt".to_string());
    let behind_running = after(pr_ship("behind-running"), "running", UnblockPoint::Merged);
    let idle = pr_ship("idle");
    let behind_idle = after(pr_ship("behind-idle"), "idle", UnblockPoint::Merged);
    // Held back by a moving task and an idle one: counted.
    let behind_both = after(
        after(pr_ship("behind-both"), "running", UnblockPoint::Merged),
        "idle",
        UnblockPoint::Merged,
    );
    let t = tree(vec![
        running,
        behind_running,
        idle,
        behind_idle,
        behind_both,
    ]);
    assert_eq!(
        derive::state(find(&t, "behind-running"), &t),
        derive::State::Blocked
    );
    let r = rollup(&t.projects[0].task, &t, &none());
    assert_eq!(r.blocked, 2);

    let only_running = tree(vec![
        {
            let mut running = pr_ship("running");
            file_mut(&mut running).worktree = Some("/wt".to_string());
            running
        },
        after(pr_ship("behind-running"), "running", UnblockPoint::Merged),
    ]);
    assert_eq!(
        rollup(&only_running.projects[0].task, &only_running, &none()).blocked,
        0
    );
}

#[test]
fn needs_you_reaches_every_ancestor_with_the_drill_path() {
    let t = tree(vec![
        group(
            "mid",
            vec![group("deep", vec![pr_ship("target"), pr_ship("other")])],
        ),
        group("side", vec![pr_ship("quiet")]),
    ]);
    let needs: HashSet<TaskId> = [id_of(&t, "target")].into();
    let ids = |slugs: &[&str]| -> Vec<TaskId> { slugs.iter().map(|s| id_of(&t, s)).collect() };

    let r = rollup(&t.projects[0].task, &t, &needs);
    assert_eq!(r.needs_you, 1);
    assert_eq!(
        r.first_needs_you,
        Some(ids(&["p", "mid", "deep", "target"]))
    );
    let r = rollup(find(&t, "mid"), &t, &needs);
    assert_eq!(r.first_needs_you, Some(ids(&["mid", "deep", "target"])));
    let r = rollup(find(&t, "deep"), &t, &needs);
    assert_eq!(r.first_needs_you, Some(ids(&["deep", "target"])));
    let r = rollup(find(&t, "side"), &t, &needs);
    assert_eq!((r.needs_you, r.first_needs_you), (0, None));
}
