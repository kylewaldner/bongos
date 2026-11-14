use std::path::PathBuf;

use crate::commands::Context;
use crate::fixtures::*;
use crate::store::{Task, WorkTree};
use crate::{Author, AuthorKind, PersonAtKeyboard, Status};

#[test]
fn a_new_context_is_unattributed_with_by_verbatim() {
    let context = Context::new("Some One", at("2026-11-08T12:00:00Z"));
    assert_eq!(context.by, "Some One");
    assert_eq!(context.author.kind(), AuthorKind::Unattributed);
    assert_eq!(context.author.name, "Some One");
}

#[test]
fn at_keyboard_is_a_person_whose_attestation_is_taken_once_and_never_cloned() {
    let mut context = Context::at_keyboard(PersonAtKeyboard::for_test());
    assert_eq!(context.author.kind(), AuthorKind::Person);
    assert_eq!(context.by, context.author.name);
    assert!(!context.by.is_empty());

    let mut clone = context.clone();
    assert_eq!(clone, context);
    assert_eq!(clone.author.kind(), AuthorKind::Person);
    assert!(clone.take_attestation().is_none());

    assert!(context.take_attestation().is_some());
    assert!(context.take_attestation().is_none());
}

#[test]
fn with_author_sets_the_author_and_by() {
    let mut author = Author::workflow("linear sync");
    author.run = Some("run-1".to_string());
    let context = Context::new("linear", at("2026-11-08T12:00:00Z")).with_author(author.clone());
    assert_eq!(context.author, author);
    assert_eq!(context.author.kind(), AuthorKind::Workflow);
    assert_eq!(context.by, "linear sync");
}

/// `p` holds `a` (holding `b`, holding `c`) and `d`, each with a flat files folder that is not
/// nested under its parent's, as in a new-format project.
fn flat_tree() -> WorkTree {
    let mut tree = tree(vec![
        group("a", vec![group("b", vec![leaf("c", Status::Open)])]),
        leaf("d", Status::Open),
    ]);
    fn flatten(task: &mut Task) {
        task.dir = PathBuf::from("/w/files").join(&task.slug);
        task.definition = Some(PathBuf::from(&task.slug));
        task.children.iter_mut().for_each(flatten);
    }
    flatten(&mut tree.projects[0].task);
    tree
}

#[test]
fn parent_of_follows_the_nesting_not_the_directories() {
    let tree = flat_tree();
    let id = |slug| id_of(&tree, slug);
    let parent = |slug| tree.parent_of(&id(slug)).and_then(Task::id).map(str::to_string);

    assert_eq!(parent("c"), Some(id("b")));
    assert_eq!(parent("b"), Some(id("a")));
    assert_eq!(parent("a"), Some(id("p")));
    assert_eq!(parent("d"), Some(id("p")));
    assert_eq!(parent("p"), None);
    assert!(tree.parent_of("no-such-task").is_none());
}

#[test]
fn is_ancestor_on_a_nested_tree() {
    let tree = flat_tree();
    let id = |slug| id_of(&tree, slug);

    assert!(tree.is_ancestor(&id("p"), &id("c")));
    assert!(tree.is_ancestor(&id("a"), &id("c")));
    assert!(tree.is_ancestor(&id("b"), &id("c")));
    assert!(tree.is_ancestor(&id("c"), &id("c")));

    assert!(!tree.is_ancestor(&id("c"), &id("a")));
    assert!(!tree.is_ancestor(&id("d"), &id("c")));
    assert!(!tree.is_ancestor(&id("b"), &id("d")));

    assert!(!tree.is_ancestor("no-such-task", &id("c")));
    assert!(!tree.is_ancestor(&id("a"), "no-such-task"));
    assert!(!tree.is_ancestor("no-such-task", "no-such-task"));
}
