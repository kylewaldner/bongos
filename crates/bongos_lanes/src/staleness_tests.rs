use crate::staleness::{Staleness, staleness_answer};

// The first output line decides; anything else names that line or the exit code.
#[test]
fn the_first_line_of_a_staleness_script_decides() {
    let cases = [
        ("stale\n", Some(0), Ok(Staleness::Stale)),
        ("fresh\nmore lines\n", Some(0), Ok(Staleness::Fresh)),
        ("  stale  \n", Some(1), Ok(Staleness::Stale)),
        ("maybe\nstale\n", Some(0), Err("maybe".to_string())),
        ("", Some(2), Err("exit code 2".to_string())),
        ("", None, Err("killed by a signal".to_string())),
    ];
    for (stdout, code, expected) in cases {
        assert_eq!(
            staleness_answer(stdout, code),
            expected,
            "{stdout:?} {code:?}"
        );
    }
}

// A script path that leaves `definitions/` is refused.
#[test]
fn a_staleness_script_must_be_inside_the_definitions() {
    let root = std::path::Path::new("/pin");
    assert_eq!(
        crate::staleness::staleness_program(root, "lanes/scripts/stale"),
        Ok(root.join("definitions/lanes/scripts/stale"))
    );
    for script in ["", "/bin/true", "../stale", "lanes/../../stale"] {
        assert!(
            crate::staleness::staleness_program(root, script).is_err(),
            "{script:?}"
        );
    }
}

#[cfg(unix)]
mod pinned {
    use std::cell::RefCell;
    use std::path::{Path, PathBuf};
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, Ordering};

    use bongos_lanes_model::{LaneId, LaneTable, PickGraph, Problem, TaskId};
    use gpui::{App, Task, TestAppContext};
    use tempfile::TempDir;

    use crate::dispatcher::LanesHost;
    use crate::staleness::{PinnedTree, Staleness, StalenessCheck};

    /// Sets its flag when dropped, as a pin guard removes its copy.
    struct Guard(Arc<AtomicBool>);

    impl Drop for Guard {
        fn drop(&mut self) {
            self.0.store(true, Ordering::SeqCst);
        }
    }

    /// A host whose `materialize` hands out `pin` and records the commits it was asked for.
    struct PinHost {
        pin: PathBuf,
        asked: RefCell<Vec<Option<String>>>,
        dropped: Arc<AtomicBool>,
    }

    impl LanesHost for PinHost {
        fn project_id(&self) -> String {
            "project-1".to_string()
        }

        fn is_repository_project(&self) -> bool {
            true
        }

        fn lane_table(&self, _commit: &str) -> Result<LaneTable, String> {
            Err("unused".to_string())
        }

        fn check_locations(
            &self,
            _table: &LaneTable,
            _lanes: &[LaneId],
            _cx: &App,
        ) -> Result<(), Vec<Problem>> {
            Ok(())
        }

        fn graph(&self, _cx: &App) -> Option<PickGraph> {
            None
        }

        fn host_id(&self) -> String {
            "this-host".to_string()
        }

        fn host_live(&self, _host: &str) -> bool {
            false
        }

        fn now(&self) -> String {
            crate::test_support::SINCE.to_string()
        }

        fn materialize(
            &self,
            commit: Option<&str>,
            _cx: &mut App,
        ) -> Task<Result<PinnedTree, String>> {
            self.asked.borrow_mut().push(commit.map(str::to_string));
            Task::ready(Ok(PinnedTree {
                root: self.pin.clone(),
                guard: Box::new(Guard(self.dropped.clone())),
            }))
        }
    }

    fn script(path: &Path, body: &str) {
        use std::os::unix::fs::PermissionsExt as _;
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    // The script runs from the pinned copy of the job's definitions commit, not from the lane's
    // tree, with the lane's tree as its working folder and third argument; the copy is kept
    // until the script ended.
    #[gpui::test]
    async fn the_staleness_script_runs_from_the_pinned_definitions(cx: &mut TestAppContext) {
        cx.executor().allow_parking();
        cx.update(|cx| {
            use settings::Settings as _;
            let store = settings::SettingsStore::new(cx, &settings::default_settings());
            cx.set_global(store);
            crate::LanesSettings::register(cx);
        });
        let pin = TempDir::new().unwrap();
        let tree = TempDir::new().unwrap();
        script(
            &pin.path().join("definitions/lanes/scripts/stale"),
            "echo \"$1|$2|$3|$(pwd -P)\" > \"$3/seen\"\necho stale",
        );
        // A script of the same path in the lane's tree is not the one run.
        script(&tree.path().join("lanes/scripts/stale"), "echo fresh");
        let host = PinHost {
            pin: pin.path().to_path_buf(),
            asked: RefCell::new(Vec::new()),
            dropped: Arc::new(AtomicBool::new(false)),
        };
        let check = |definition_commit: Option<&str>| StalenessCheck {
            lane: LaneId::new("build", 1),
            task: TaskId::new("F"),
            prepared_commit: Some("abc".to_string()),
            script: "lanes/scripts/stale".to_string(),
            definition_commit: definition_commit.map(str::to_string),
            tree: Some(tree.path().to_path_buf()),
            env: Vec::new(),
        };

        let answer = cx
            .update(|cx| host.staleness(check(Some("defs")), cx))
            .await;
        assert_eq!(answer, Ok(Staleness::Stale));
        let real = tree.path().canonicalize().unwrap();
        let seen = std::fs::read_to_string(tree.path().join("seen")).unwrap();
        assert_eq!(
            seen.trim(),
            format!("F|abc|{}|{}", tree.path().display(), real.display())
        );
        assert!(host.dropped.load(Ordering::SeqCst));

        // With no definitions commit, master's definitions are pinned.
        let answer = cx.update(|cx| host.staleness(check(None), cx)).await;
        assert_eq!(answer, Ok(Staleness::Stale));
        assert_eq!(*host.asked.borrow(), [Some("defs".to_string()), None]);
    }
}
