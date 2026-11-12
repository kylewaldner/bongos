use crate::layout::*;

#[test]
fn templates_resolve_the_folder_token() {
    assert_eq!(
        resolve_template(DEFAULT_REPOSITORY_TEMPLATE, "/p/app"),
        "/p/app/repository"
    );
    assert_eq!(
        resolve_template(DEFAULT_STATE_TEMPLATE, "/p/app"),
        "/p/app/state"
    );
    assert_eq!(resolve_template("/elsewhere/x", "/p/app"), "/elsewhere/x");
}

#[test]
fn known_folders_are_the_ones_of_the_layout() {
    for name in ["documents", "tasks", "workflows", "profiles"] {
        assert!(is_known_folder(name), "{name}");
    }
    assert!(!is_known_folder("scratch"));
    assert!(!is_known_folder("definitions"));
}

#[test]
fn working_copies_never_sit_inside_the_repository() {
    assert!(!DEFAULT_WORKING_COPIES_TEMPLATE.starts_with(DEFAULT_REPOSITORY_TEMPLATE));
    assert_eq!(GITATTRIBUTES_CONTENTS, "* text=auto\n");
}
