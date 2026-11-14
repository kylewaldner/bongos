use crate::checks::{
    CheckOutcome, FileFacts, SizeLimits, compile_secret_patterns, no_cycle, no_state, references,
    secrets, size_and_kind,
};
use crate::summary::summarize;
use crate::test_trees::{after, tree};

#[test]
fn no_state_refuses_state_keys_state_paths_and_state_stores() {
    let paths = [
        "definitions/task.toml",
        "definitions/tasks/state/task.toml",
        "state/host.lock",
        "definitions/files/t1/notes.md",
        "definitions/documents/state.db-wal",
    ];
    let task = "schema = 1\nid = \"a\"\ngoal = \"a\"\nstatus = \"open\"\n[[todos]]\nname = \"x\"\nstate = \"done\"\n";
    let outcome = no_state(paths, [("definitions/tasks/a/task.toml", task)]);
    assert_eq!(
        outcome.message(),
        Some(
            "state/host.lock: the state part never lands\n\
             definitions/files/t1/notes.md: the files part never lands\n\
             definitions/documents/state.db-wal: a state store never lands\n\
             definitions/tasks/a/task.toml: holds state, which belongs to the state store: status, todo.state"
        )
    );
    assert_eq!(
        no_state(["definitions/tasks/state/task.toml"], []),
        CheckOutcome::Pass
    );
}

#[test]
fn no_cycle_names_the_edge_that_closes_the_cycle() {
    let master = tree(&[
        ("tasks/build", &after(&["tests"])),
        ("tasks/deploy", &after(&["build"])),
        ("tasks/tests", ""),
    ]);
    let new = tree(&[
        ("tasks/build", &after(&["tests"])),
        ("tasks/deploy", &after(&["build"])),
        ("tasks/tests", &after(&["deploy"])),
    ]);
    let summary = summarize(&master, &new);
    assert_eq!(
        no_cycle(&new, &summary).message(),
        Some("cycle: tests after deploy closes build -> deploy -> tests")
    );
    assert_eq!(no_cycle(&master, &summary), CheckOutcome::Pass);

    let nested = tree(&[
        ("tasks/g", ""),
        ("tasks/g/tasks/a", &after(&["b"])),
        ("tasks/g/tasks/b", &after(&["a"])),
    ]);
    let outcome = no_cycle(&nested, &summarize(&master, &nested));
    assert_eq!(outcome.message().map(|m| m.lines().count()), Some(1));
}

#[test]
fn references_refuse_edges_to_no_sibling_and_unknown_workflows() {
    let tree = tree(&[
        (
            "tasks/a",
            &format!(
                "{}[[todos]]\nname = \"review\"\nworkflow = \"review-change\"\n",
                after(&["b"])
            ),
        ),
        (
            "tasks/b",
            "[[todos]]\nname = \"ghost\"\nworkflow = \"missing\"\n",
        ),
        ("tasks/b/tasks/c", &after(&["a"])),
    ]);
    let outcome = references(&tree, &|name| name == "review-change");
    assert_eq!(
        outcome.message(),
        Some(
            "c: after names a, which is not a sibling\n\
             b: todo ghost names workflow missing, which does not exist"
        )
    );
}

#[test]
fn size_and_kind_refuse_large_text_stray_images_and_binaries() {
    let limits = SizeLimits {
        text_file_limit_bytes: 10,
        image_limit_bytes: 20,
    };
    let files = [
        FileFacts {
            path: "definitions/plan.md",
            len: 5,
            head: b"hello",
        },
        FileFacts {
            path: "definitions/notes.md",
            len: 11,
            head: b"hello world",
        },
        FileFacts {
            path: "definitions/documents/shot.PNG",
            len: 20,
            head: b"\x89PNG\0",
        },
        FileFacts {
            path: "definitions/tasks/a/documents/big.webp",
            len: 21,
            head: b"RIFF",
        },
        FileFacts {
            path: "definitions/tasks/a/logo.jpg",
            len: 1,
            head: b"\xff",
        },
        FileFacts {
            path: "definitions/tool.bin",
            len: 4,
            head: b"ab\0c",
        },
    ];
    assert_eq!(
        size_and_kind(files, limits).message(),
        Some(
            "definitions/notes.md: a text file of 11 bytes is over the limit of 10\n\
             definitions/tasks/a/documents/big.webp: an image of 21 bytes is over the limit of 20\n\
             definitions/tasks/a/logo.jpg: an image belongs in a documents/ folder\n\
             definitions/tool.bin: a binary file never lands"
        )
    );
    assert_eq!(SizeLimits::default().text_file_limit_bytes, 1_048_576);
}

#[test]
fn a_secret_pattern_hit_or_a_secret_field_refuses() {
    let patterns = compile_secret_patterns(&[
        "AKIA[0-9A-Z]{16}".to_string(),
        "sk-ant-[A-Za-z0-9_-]{20,}".to_string(),
    ])
    .unwrap();
    let key = "AKIA".to_string() + &"Z".repeat(16);
    let plan = format!("intro\nthe key is {key}\n");
    let project = "title = \"P\"\n[linear]\ntoken = \"abc\"\n";
    let outcome = secrets(
        [
            ("definitions/plan.md", plan.as_str()),
            ("definitions/project.toml", project),
            ("definitions/documents/ok.md", "nothing here"),
        ],
        &patterns,
        &["linear.token".to_string(), "linear.missing".to_string()],
    );
    let message = outcome.message().unwrap();
    assert_eq!(
        message,
        "definitions/plan.md:2: matches the secret pattern AKIA[0-9A-Z]{16}\n\
         definitions/project.toml: linear.token is secret and may not hold a value in the repository"
    );
    assert!(!message.contains(&key));
    assert!(compile_secret_patterns(&["(".to_string()]).is_err());
}
