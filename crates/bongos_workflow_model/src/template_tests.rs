use crate::template::{Scope, TemplateError, references, render};

fn scope() -> Scope {
    let mut scope = Scope::new();
    scope
        .set("inputs.goal", "ship it")
        .set("nodes.triage.mode", "planned")
        .set("item", "write docs")
        .set("branches.plan", "a, b");
    scope
}

#[test]
fn renders_every_kind_of_name() {
    let text = "Goal: {{inputs.goal}}\nMode: {{nodes.triage.mode}}\nItem: {{item}}\nAll: {{branches.plan}}\n";
    assert_eq!(
        render(text, &scope()).unwrap(),
        "Goal: ship it\nMode: planned\nItem: write docs\nAll: a, b\n"
    );
}

#[test]
fn spaces_inside_braces_are_ignored() {
    assert_eq!(render("{{ item }}", &scope()).unwrap(), "write docs");
}

#[test]
fn text_without_names_is_unchanged() {
    assert_eq!(
        render("plain { text } }}", &Scope::new()).unwrap(),
        "plain { text } }}"
    );
}

#[test]
fn unknown_names_are_rejected_with_their_line() {
    let error = render("first\n{{inputs.goal}} {{inputs.nope}}\n", &scope()).unwrap_err();
    assert_eq!(
        error,
        TemplateError::UnknownName {
            name: "inputs.nope".to_string(),
            line: 2
        }
    );
}

#[test]
fn an_unknown_name_is_never_empty_text() {
    assert!(render("{{item}}", &Scope::new()).is_err());
}

#[test]
fn values_are_not_rendered_again() {
    let mut scope = Scope::new();
    scope.set("item", "{{inputs.goal}}");
    assert_eq!(render("{{item}}", &scope).unwrap(), "{{inputs.goal}}");
}

#[test]
fn malformed_braces_are_errors() {
    assert_eq!(
        render("a\n{{ }}", &scope()).unwrap_err(),
        TemplateError::EmptyName { line: 2 }
    );
    assert_eq!(
        render("a\nb {{item", &scope()).unwrap_err(),
        TemplateError::Unterminated { line: 2 }
    );
}

#[test]
fn references_list_names_in_order() {
    let found = references("{{item}} and\n{{ nodes.a.b }}").unwrap();
    let found = found
        .iter()
        .map(|reference| (reference.name.as_str(), reference.line))
        .collect::<Vec<_>>();
    assert_eq!(found, [("item", 1), ("nodes.a.b", 2)]);
}
