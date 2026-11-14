use std::path::PathBuf;

use bongos_work_model::store::{Project, Task, WorkTree};
use bongos_work_model::{Kind, TaskFile};

use crate::structure::can_move_under;

/// A task whose folder is flat, as `files/<id>/` of a project repository: nothing in its path
/// says where it sits in the tree.
fn task(id: &str, children: Vec<Task>) -> Task {
    let mut file = TaskFile::new(id, Kind::Other);
    file.id = id.to_string();
    Task {
        slug: id.to_string(),
        dir: PathBuf::from("/root/files").join(id),
        definition: None,
        pending: None,
        needs_conversion: false,
        file: Some(file),
        artifacts: None,
        errors: Vec::new(),
        broken_edges: Vec::new(),
        files: Default::default(),
        hashes: Default::default(),
        children,
    }
}

#[test]
fn the_move_target_list_leaves_out_the_moved_subtree_with_flat_folders() {
    let tree = WorkTree {
        root: PathBuf::from("/root"),
        projects: vec![Project {
            slug: "p".to_string(),
            file: None,
            task: task(
                "top",
                vec![
                    task(
                        "moved",
                        vec![task("inside", vec![task("deep", Vec::new())])],
                    ),
                    task("sibling", Vec::new()),
                ],
            ),
        }],
        ..WorkTree::default()
    };
    let targets: Vec<&str> = tree
        .tasks()
        .into_iter()
        .filter(|task| can_move_under(task, &tree, "moved"))
        .filter_map(|task| task.id())
        .collect();
    assert_eq!(targets, vec!["top", "sibling"]);
}
