use std::{
    fs,
    path::{Path, PathBuf},
};

use sandbox::SandboxNetPolicy;

use crate::{
    ConfineError, Confinement, HostGit, JobDeclaration, JobKind, JobTree, RULE, TreeAccess,
    TreeGit, annotate,
    protected::{ProtectedKind, ProtectedLocation},
};

fn base() -> (tempfile::TempDir, PathBuf) {
    let temp = tempfile::tempdir().unwrap();
    let base = fs::canonicalize(temp.path()).unwrap();
    (temp, base)
}

fn dir(base: &Path, relative: &str) -> PathBuf {
    let path = base.join(relative);
    fs::create_dir_all(&path).unwrap();
    path
}

fn declaration(scratch: PathBuf, trees: Vec<JobTree>, declared_dirs: Vec<PathBuf>) -> JobDeclaration {
    JobDeclaration {
        kind: JobKind::Program,
        trees,
        scratch,
        declared_dirs,
        extra_writable: Vec::new(),
        network: SandboxNetPolicy::Blocked,
    }
}

fn location(path: PathBuf, kind: ProtectedKind) -> ProtectedLocation {
    ProtectedLocation { path, kind }
}

fn plan_declaring(
    scratch: PathBuf,
    declared: PathBuf,
    protected: &[ProtectedLocation],
) -> Result<Confinement, ConfineError> {
    Confinement::plan_with_protected(
        &declaration(scratch, Vec::new(), vec![declared]),
        &HostGit::default(),
        protected,
    )
}

#[test]
fn refuses_a_declared_dir_equal_to_containing_or_inside_run_state() {
    let (_temp, base) = base();
    let scratch = dir(&base, "scratch");
    let state = dir(&base, "data/state");
    let protected = [location(state.clone(), ProtectedKind::RunState)];

    for declared in [state.clone(), base.join("data"), dir(&base, "data/state/inner")] {
        let error = plan_declaring(scratch.clone(), declared.clone(), &protected).unwrap_err();
        assert_eq!(
            error,
            ConfineError::ProtectedLocation {
                path: declared.clone(),
                kind: ProtectedKind::RunState,
                protected: state.clone(),
            }
        );
        assert_eq!(
            error.to_string(),
            format!(
                "the writable path {} overlaps the run-state database {}",
                declared.display(),
                state.display()
            )
        );
    }

    // A sibling of the protected location is fine.
    let sibling = dir(&base, "data/other");
    assert!(plan_declaring(scratch, sibling, &protected).is_ok());
}

#[test]
fn refuses_a_writable_master_repository_and_hosts_directory() {
    let (_temp, base) = base();
    let scratch = dir(&base, "scratch");
    let master = dir(&base, "master");
    let hosts = dir(&base, "run/hosts");
    let socket = hosts.join("engine.sock");
    let protected = [
        location(master.clone(), ProtectedKind::Master),
        location(socket.clone(), ProtectedKind::HostSocket),
    ];

    let error = Confinement::plan_with_protected(
        &declaration(
            scratch.clone(),
            vec![JobTree {
                path: master.clone(),
                access: TreeAccess::Writable,
                git: TreeGit::None,
            }],
            Vec::new(),
        ),
        &HostGit::default(),
        &protected,
    )
    .unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "the writable path {} overlaps the master repository {}",
            master.display(),
            master.display()
        )
    );

    let error = plan_declaring(scratch, hosts.clone(), &protected).unwrap_err();
    assert_eq!(
        error,
        ConfineError::ProtectedLocation {
            path: hosts.clone(),
            kind: ProtectedKind::HostSocket,
            protected: socket.clone(),
        }
    );
    assert_eq!(
        error.to_string(),
        format!(
            "the writable path {} overlaps the engine host socket {}",
            hosts.display(),
            socket.display()
        )
    );
}

#[cfg(unix)]
#[test]
fn refuses_a_symlink_into_a_protected_location_after_canonicalizing() {
    let (_temp, base) = base();
    let scratch = dir(&base, "scratch");
    let keys = dir(&base, "keys");
    let inner = dir(&base, "keys/inner");
    let protected = [location(keys.clone(), ProtectedKind::Approvals)];

    // A declared directory that is a symlink into the protected location.
    let link = base.join("link");
    std::os::unix::fs::symlink(&inner, &link).unwrap();
    let error = plan_declaring(scratch.clone(), link, &protected).unwrap_err();
    assert_eq!(
        error,
        ConfineError::ProtectedLocation {
            path: inner,
            kind: ProtectedKind::Approvals,
            protected: keys.clone(),
        }
    );

    // A protected location given through a symlink.
    let keys_link = base.join("keys-link");
    std::os::unix::fs::symlink(&keys, &keys_link).unwrap();
    let protected = [location(keys_link.clone(), ProtectedKind::Approvals)];
    let error = plan_declaring(scratch, keys.clone(), &protected).unwrap_err();
    assert_eq!(
        error.to_string(),
        format!(
            "the writable path {} overlaps the approval keys {}",
            keys.display(),
            keys_link.display()
        )
    );
}

#[test]
fn annotates_a_refusal_under_each_protected_location_with_its_rule() {
    let protected = [
        location(PathBuf::from("/data/state"), ProtectedKind::RunState),
        location(PathBuf::from("/data/master"), ProtectedKind::Master),
        location(PathBuf::from("/run/hosts"), ProtectedKind::HostSocket),
        location(PathBuf::from("/data/keys"), ProtectedKind::Approvals),
    ];
    let roots = vec![PathBuf::from("/work/tree")];
    let annotate = |line: &str| annotate::refusal_with_protected(line, &roots, &protected);

    assert_eq!(
        annotate("sqlite: /data/state/run.db: Read-only file system").as_deref(),
        Some("bongos: writing run state from a job is refused: /data/state/run.db")
    );
    assert_eq!(
        annotate("touch: cannot touch '/data/master/x': Permission denied").as_deref(),
        Some("bongos: writing the master repository from a job is refused: /data/master/x")
    );
    assert_eq!(
        annotate("cp: '/data/keys/k': Operation not permitted").as_deref(),
        Some("bongos: writing approval keys from a job is refused: /data/keys/k")
    );
    for line in [
        "nc: /run/hosts/engine.sock: No such file or directory",
        "connect '/run/hosts/engine.sock': Connection refused",
        "sh: /run/hosts/engine.sock: Permission denied",
    ] {
        assert_eq!(
            annotate(line).as_deref(),
            Some("bongos: talking to the engine host from a job is refused: /run/hosts/engine.sock"),
            "{line}"
        );
    }

    // A missing path is only a refusal for the engine host.
    assert_eq!(
        annotate("cat: '/data/master/x': No such file or directory"),
        None
    );
    // Outside every protected location the general rule applies.
    assert_eq!(
        annotate("sh: /etc/x: Read-only file system"),
        Some(format!("bongos: {RULE}: /etc/x"))
    );
    assert_eq!(annotate("sh: /work/tree/x: Permission denied"), None);
}

#[test]
fn an_empty_protected_list_changes_nothing() {
    let (_temp, base) = base();
    let scratch = dir(&base, "scratch");
    let tree = dir(&base, "tree");
    let declared = dir(&base, "lane");
    let accepted = declaration(
        scratch.clone(),
        vec![JobTree {
            path: tree,
            access: TreeAccess::Writable,
            git: TreeGit::None,
        }],
        vec![declared],
    );
    let refused = declaration(scratch, Vec::new(), vec![PathBuf::from("relative")]);
    let host = HostGit::default();

    let before = Confinement::plan(&accepted, &host).unwrap();
    let after = Confinement::plan_with_protected(&accepted, &host, &[]).unwrap();
    assert_eq!(after.writable_roots(), before.writable_roots());
    assert_eq!(after.masks(), before.masks());
    assert_eq!(after.exclusions(), before.exclusions());
    assert_eq!(after.sandbox_policy(), before.sandbox_policy());

    assert_eq!(
        Confinement::plan_with_protected(&refused, &host, &[]).unwrap_err(),
        Confinement::plan(&refused, &host).unwrap_err()
    );

    let line = "sh: /etc/x: Read-only file system";
    let roots = vec![PathBuf::from("/work")];
    assert_eq!(
        annotate::refusal_with_protected(line, &roots, &[]),
        annotate::refusal(line, &roots)
    );
}

#[test]
fn refuses_an_unresolvable_protected_path_as_given() {
    let (_temp, base) = base();
    let scratch = dir(&base, "scratch");
    let lane = dir(&base, "lane");

    // Missing: compared as given, inside the declared directory.
    let missing = lane.join("gone/run.db");
    let protected = [location(missing.clone(), ProtectedKind::RunState)];
    let error = plan_declaring(scratch.clone(), lane.clone(), &protected).unwrap_err();
    assert_eq!(
        error,
        ConfineError::ProtectedLocation {
            path: lane.clone(),
            kind: ProtectedKind::RunState,
            protected: missing,
        }
    );

    // Resolved, `lane/../gone/run.db` lies outside the lane; as given it lies inside it.
    let as_given = lane.join("../gone/run.db");
    let protected = [location(as_given.clone(), ProtectedKind::RunState)];
    let error = plan_declaring(scratch, lane.clone(), &protected).unwrap_err();
    assert_eq!(
        error,
        ConfineError::ProtectedLocation {
            path: lane,
            kind: ProtectedKind::RunState,
            protected: as_given,
        }
    );
}
