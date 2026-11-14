//! The landing view: its sections and rows, what each key does on a row, the "amended by" row
//! and the modal that sends keys to them. The sections come from a real project repository in a
//! `TempDir`; the key rules are checked on rows built by hand.

use bongos_project_repo::{
    AmendCommit, AmendedBy, ChangeListing, ChangeRow, ChangeState, Identity, LandMode, LandRequest,
    ListedChange, OutsideChange, SplitMaster,
};
use bongos_project_repo_model::ChangeMeta;
use bongos_work_model::Author;
use bongos_work_model::commands::Context;
use chrono::{DateTime, Duration, Utc};
use gpui::TestAppContext;

use crate::command_tests::{dispatch, wait_until};
use crate::landing_commands::LandingModal;
use crate::landing_view::{LandingCommand, LandingView, RowTarget, Section, command_for};
use crate::working_copy_tests::{fixture, start};

fn row(id: &str, name: &str, state: ChangeState, age_days: i64) -> ChangeRow {
    let at = start() - Duration::days(age_days);
    ChangeRow {
        change_id: id.to_string(),
        name: name.to_string(),
        branch: format!("work/{name}"),
        author: Author::agent("chat 1"),
        created: at,
        updated: at,
        state,
        commit: (state == ChangeState::Landed).then(|| format!("{id}{id}{id}{id}{id}{id}{id}{id}")),
        amend_commits: Vec::new(),
    }
}

fn listed(row: ChangeRow, summary: &str) -> ListedChange {
    ListedChange {
        subject: (row.state == ChangeState::Landed).then(|| format!("subject of {}", row.name)),
        row,
        summary: summary.to_string(),
        not_landed: None,
        amended: None,
    }
}

fn listing(changes: Vec<ListedChange>) -> ChangeListing {
    ChangeListing {
        split: None,
        outside: None,
        changes,
    }
}

#[test]
fn the_view_has_a_section_for_each_kind_of_change_and_leaves_empty_ones_out() {
    let mut not_landed = listed(row("c2", "refused", ChangeState::Open, 20), "adds 1 task");
    not_landed.not_landed = Some("not landed".to_string());
    let view = LandingView::build(
        &listing(vec![
            listed(row("c1", "fresh", ChangeState::Open, 1), "adds 2 tasks"),
            not_landed,
            listed(row("c3", "older", ChangeState::Landed, 9), "retags 1"),
            listed(row("c4", "newer", ChangeState::Landed, 2), "adds 5 tasks"),
            listed(row("c5", "dropped", ChangeState::Abandoned, 3), ""),
        ]),
        start(),
        14,
    );
    let sections: Vec<Section> = view.sections.iter().map(|block| block.section).collect();
    assert_eq!(
        sections,
        [Section::Waiting, Section::Landed, Section::Abandoned]
    );
    assert_eq!(
        view.lines(),
        [
            "Waiting (2)",
            "  fresh (chat 1, idle 1d): adds 2 tasks",
            "  refused (chat 1, idle 20d, stale): adds 1 task [not landed]",
            "Landed (2)",
            "  subject of newer (chat 1, c4c4c4c4): adds 5 tasks",
            "  subject of older (chat 1, c3c3c3c3): retags 1",
            "Abandoned (1)",
            "  dropped (chat 1)",
        ]
    );
    assert!(
        LandingView::build(&listing(Vec::new()), start(), 14)
            .sections
            .is_empty()
    );
}

#[test]
fn master_warnings_come_first_and_only_a_split_master_row_resolves() {
    let mut listing = listing(vec![listed(
        row("c1", "fresh", ChangeState::Open, 1),
        "adds 1 task",
    )]);
    listing.split = Some(SplitMaster {
        change_id: "c9".to_string(),
        main: "mainmainmain".to_string(),
        verified: "verifiedverified".to_string(),
        old: "old".to_string(),
    });
    listing.outside = Some(OutsideChange {
        commit: "outsideoutside".to_string(),
        passed: true,
        problems: Vec::new(),
    });
    let view = LandingView::build(&listing, start(), 14);
    assert_eq!(view.sections[0].section, Section::Master);
    let rows = view.rows();
    assert_eq!(rows[0].target, RowTarget::SplitMaster);
    assert!(
        rows[0]
            .text
            .contains("master was changed outside bongos while c9")
    );
    assert_eq!(rows[1].target, RowTarget::OutsideCommit);

    assert_eq!(view.command("r", 0), Ok(LandingCommand::ResolveSplit));
    for key in ["d", "o", "a", "k", "l", "R"] {
        assert!(view.command(key, 0).is_err(), "{key} on a split master");
    }
    // `r` on any other row does nothing: only a split master is resolved.
    assert!(view.command("r", 2).is_err());
    for key in ["r", "d", "a", "k", "l", "o"] {
        assert!(view.command(key, 1).is_err(), "{key} on an outside commit");
    }
    assert!(view.command("d", 99).is_err());
}

#[test]
fn keys_dispatch_to_the_command_of_the_row_and_refuse_what_the_change_cannot_do() {
    let change = |state| RowTarget::Change {
        change_id: "c1".to_string(),
        state,
    };
    let id = || "c1".to_string();
    for state in [ChangeState::Open, ChangeState::Requested] {
        let target = change(state);
        assert_eq!(
            command_for(&target, "d"),
            Ok(LandingCommand::OpenTaskDiff { change_id: id() })
        );
        assert_eq!(
            command_for(&target, "D"),
            Ok(LandingCommand::OpenRawDiff { change_id: id() })
        );
        assert_eq!(
            command_for(&target, "o"),
            Ok(LandingCommand::CheckOut { change_id: id() })
        );
        assert_eq!(
            command_for(&target, "a"),
            Ok(LandingCommand::Approve { change_id: id() })
        );
        assert_eq!(
            command_for(&target, "k"),
            Ok(LandingCommand::Abandon { change_id: id() })
        );
        assert_eq!(
            command_for(&target, "l"),
            Ok(LandingCommand::Land { change_id: id() })
        );
        assert!(command_for(&target, "R").is_err());
        assert!(command_for(&target, "r").is_err());
        assert!(command_for(&target, "x").is_err());
    }
    let landed = change(ChangeState::Landed);
    assert_eq!(
        command_for(&landed, "d"),
        Ok(LandingCommand::OpenTaskDiff { change_id: id() })
    );
    assert_eq!(
        command_for(&landed, "R"),
        Ok(LandingCommand::Revert { change_id: id() })
    );
    for key in ["o", "a", "k", "l", "r"] {
        assert!(
            command_for(&landed, key).is_err(),
            "{key} on a landed change"
        );
    }
    let abandoned = change(ChangeState::Abandoned);
    for key in ["o", "a", "k", "l", "R"] {
        assert!(
            command_for(&abandoned, key).is_err(),
            "{key} on an abandoned change"
        );
    }
}

#[test]
fn an_amended_change_has_one_amended_by_row_and_d_on_it_opens_the_amend_commit() {
    let mut amended = listed(row("c1", "alpha", ChangeState::Landed, 1), "adds 1 task");
    // The row's own record names the amending check; the commit subject is not consulted.
    amended.row.amend_commits = vec![AmendCommit {
        commit: "a".repeat(40),
        check: "edges".to_string(),
    }];
    amended.amended = Some(AmendedBy {
        checks: vec!["edges".to_string()],
        files: 3,
        commit: "a".repeat(40),
    });
    let view = LandingView::build(&listing(vec![amended]), start(), 14);
    let rows = view.rows();
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[1].text, "  amended by edges: 3 files");
    assert_eq!(
        view.lines()
            .iter()
            .filter(|line| line.contains("amended by"))
            .count(),
        1
    );
    assert_eq!(
        view.command("d", 1),
        Ok(LandingCommand::OpenAmendDiff {
            change_id: "c1".to_string(),
            commit: "a".repeat(40),
        })
    );
    // On the change's own row `d` is still its task-aware diff, and the amended row's other
    // keys act on its change.
    assert_eq!(
        view.command("d", 0),
        Ok(LandingCommand::OpenTaskDiff {
            change_id: "c1".to_string()
        })
    );
    assert_eq!(
        view.command("R", 1),
        Ok(LandingCommand::Revert {
            change_id: "c1".to_string()
        })
    );
    assert!(view.command("a", 1).is_err());
}

fn person_context() -> Context {
    Context::at_keyboard(bongos_work_model::PersonAtKeyboard::for_test())
}

#[gpui::test]
async fn show_changes_lists_a_real_repository_by_section_and_d_opens_the_task_diff(
    cx: &mut TestAppContext,
) {
    let (fixture, cx) = fixture(cx).await;
    let person = person_context().author;
    let at: DateTime<Utc> = start();
    let landed = fixture
        .repo
        .create_working_copy("landed-one", "work/", &person, at)
        .await
        .unwrap();
    std::fs::write(landed.path.join("definitions/plan.md"), "the plan\n").unwrap();
    let request = LandRequest {
        meta: ChangeMeta::new("Write the plan", person.clone()),
        mode: LandMode::Land,
    };
    fixture
        .repo
        .land_as_person(&landed.change_id, request, person_context(), at)
        .await
        .unwrap();
    let waiting = fixture
        .repo
        .create_working_copy("waiting-one", "work/", &person, at)
        .await
        .unwrap();
    std::fs::write(waiting.path.join("definitions/plan.md"), "another plan\n").unwrap();
    fixture
        .repo
        .git()
        .commit_all(&waiting.path, "edit", &Identity::default())
        .await
        .unwrap();
    let dropped = fixture
        .repo
        .create_working_copy("dropped-one", "work/", &person, at)
        .await
        .unwrap();
    fixture
        .repo
        .abandon_now(&dropped.change_id, at)
        .await
        .unwrap();

    let listing = fixture.repo.change_listing().await.unwrap();
    let view = LandingView::build(&listing, at, 14);
    let sections: Vec<Section> = view.sections.iter().map(|block| block.section).collect();
    assert_eq!(
        sections,
        [Section::Waiting, Section::Landed, Section::Abandoned]
    );
    let lines = view.lines();
    assert!(lines[1].starts_with("  waiting-one"), "{lines:?}");
    assert!(
        lines.iter().any(|line| line.contains("Write the plan")),
        "{lines:?}"
    );

    // The modal: `d` on the first row (the waiting change) writes and opens its diff.
    dispatch(cx, "project::ShowChanges");
    wait_until(cx, "the landing view", |cx| {
        fixture
            .workspace
            .read_with(cx, |workspace, cx| {
                workspace.active_modal::<LandingModal>(cx)
            })
            .is_some()
    })
    .await;
    cx.simulate_keystrokes("d");
    let diff = fixture
        .repo
        .location()
        .state
        .join("diffs")
        .join(format!("{}-tasks.txt", waiting.change_id));
    wait_until(cx, "the diff file", |_| diff.is_file()).await;
    let text = std::fs::read_to_string(&diff).unwrap();
    assert!(
        text.contains("changes 1 file") || text.contains("no changes"),
        "{text}"
    );
}
