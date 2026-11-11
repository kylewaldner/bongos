//! Tests of the definition helpers over a temporary directory: layers, materializing, validating
//! and creating a definition. Nothing here touches the real home.

use std::fs;
use std::path::Path;

use bongos_workflow_model::layers::DEFINITION_FILE;
use bongos_workflow_model::{Layer, LayerDirs, Severity};
use tempfile::TempDir;

use crate::definitions::{
    DEFAULT_NAME, DefinitionError, ShippedFile, definition_path, materialize, new_definition,
    next_free_name, profile_dir, project_layer_dir, shipped_dir, validate_definitions,
};

/// A definition whose only node routes to a node that does not exist, so validate reports an
/// error on the line of the route.
const BROKEN: &str = "schema = 1\n\
name = \"broken\"\n\
start = \"only\"\n\
\n\
[nodes.only]\n\
kind = \"program\"\n\
use = \"wait\"\n\
routes = [{ to = \"nowhere\" }]\n\
\n\
[ends.done]\n\
kind = \"success\"\n";

fn write_definition(layer: &Path, dir: &str, text: &str) {
    let dir = layer.join(dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(dir.join(DEFINITION_FILE), text).unwrap();
}

fn profile_only(dir: &Path) -> LayerDirs {
    LayerDirs {
        profile: Some(dir.to_path_buf()),
        ..LayerDirs::default()
    }
}

#[test]
fn layer_directories_follow_the_layout() {
    assert_eq!(
        shipped_dir(Path::new("/data"), "1.2.3"),
        Path::new("/data/workflows/shipped/1.2.3")
    );
    assert_eq!(
        profile_dir(Path::new("/home/a")),
        Path::new("/home/a/.bongos/workflows")
    );
    assert_eq!(
        project_layer_dir(Path::new("/home/a/.bongos"), "web"),
        Path::new("/home/a/.bongos/projects/web/workflows")
    );
}

#[test]
fn an_empty_shipped_set_creates_nothing() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("shipped").join("1.0.0");
    assert_eq!(materialize(&dir, &[]).unwrap(), 0);
    assert!(!dir.exists(), "not even the directory");
}

#[test]
fn materializing_writes_missing_files_and_never_overwrites() {
    let temp = TempDir::new().unwrap();
    let dir = temp.path().join("shipped");
    let files = [
        ShippedFile {
            path: "a/workflow.toml",
            contents: "one",
        },
        ShippedFile {
            path: "a/prompt.md",
            contents: "two",
        },
    ];
    assert_eq!(materialize(&dir, &files).unwrap(), 2);
    fs::write(dir.join("a/prompt.md"), "edited").unwrap();
    fs::remove_file(dir.join("a/workflow.toml")).unwrap();
    assert_eq!(
        materialize(&dir, &files).unwrap(),
        1,
        "only the missing file"
    );
    assert_eq!(
        fs::read_to_string(dir.join("a/workflow.toml")).unwrap(),
        "one"
    );
    assert_eq!(
        fs::read_to_string(dir.join("a/prompt.md")).unwrap(),
        "edited"
    );

    let escape = [ShippedFile {
        path: "../outside",
        contents: "x",
    }];
    assert!(materialize(&dir, &escape).is_err());
    assert!(!temp.path().join("outside").exists());
}

#[test]
fn validate_lists_the_errors_of_a_fixture_with_node_and_line() {
    let temp = TempDir::new().unwrap();
    write_definition(temp.path(), "broken", BROKEN);
    let report = validate_definitions(&profile_only(temp.path()), None).unwrap();

    assert_eq!(report.definitions.len(), 1);
    let definition = &report.definitions[0];
    assert_eq!(definition.name, "broken");
    assert_eq!(definition.layer, Layer::Profile);
    assert!(definition.load_error.is_none());
    let line = BROKEN
        .lines()
        .position(|line| line.contains("nowhere"))
        .unwrap()
        + 1;
    let found = definition
        .diagnostics
        .iter()
        .find(|diagnostic| {
            diagnostic.severity == Severity::Error && diagnostic.message.contains("nowhere")
        })
        .unwrap_or_else(|| {
            panic!(
                "an error names the missing node: {:?}",
                definition.diagnostics
            )
        });
    assert_eq!(found.node.as_deref(), Some("only"));
    assert_eq!(found.line, line);

    let text = report.render();
    assert!(
        text.contains(&format!("error: [only] line {line}: ")),
        "{text}"
    );
    assert!(text.contains("broken (profile)"), "{text}");
    assert!(report.error_count() >= 1);
}

#[test]
fn validate_reports_a_file_that_does_not_parse_and_duplicate_names() {
    let temp = TempDir::new().unwrap();
    write_definition(
        temp.path(),
        "bad",
        "schema = 1\nname = \"bad\"\nstart = [\n",
    );
    write_definition(temp.path(), "one", BROKEN);
    write_definition(temp.path(), "two", BROKEN);
    let report = validate_definitions(&profile_only(temp.path()), None).unwrap();
    let text = report.render();
    assert!(
        report
            .resolution_errors
            .iter()
            .any(|error| error.contains("duplicate name in profile")),
        "{text}"
    );
    // A file that does not parse has no name to resolve by, so it is a problem of its layer.
    assert!(
        report
            .resolution_errors
            .iter()
            .any(|error| error.contains("bad") && error.contains("(profile)")),
        "{text}"
    );
    assert!(report.error_count() >= 2, "{text}");
}

#[test]
fn validate_one_by_name_and_unknown_names() {
    let temp = TempDir::new().unwrap();
    write_definition(temp.path(), "broken", BROKEN);
    let dirs = profile_only(temp.path());
    let report = validate_definitions(&dirs, Some("broken")).unwrap();
    assert_eq!(report.definitions.len(), 1);
    assert!(matches!(
        validate_definitions(&dirs, Some("missing")),
        Err(DefinitionError::NotFound(_))
    ));
}

#[test]
fn no_definitions_render_a_plain_line() {
    let temp = TempDir::new().unwrap();
    let report = validate_definitions(&profile_only(temp.path()), None).unwrap();
    assert_eq!(report.render(), "No workflow definitions found.\n");
    assert_eq!(report.error_count(), 0);
}

#[test]
fn new_writes_the_minimal_template_and_it_validates_clean() {
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    let created = new_definition(Some(&profile), Some("Ship the thing")).unwrap();
    assert_eq!(created.name, "Ship the thing");
    assert_eq!(created.dir, profile.join("ship-the-thing"));
    assert_eq!(created.file, created.dir.join(DEFINITION_FILE));
    assert!(created.file.is_file());

    let report = validate_definitions(&profile_only(&profile), None).unwrap();
    assert_eq!(report.definitions.len(), 1);
    assert_eq!(report.definitions[0].name, "Ship the thing");
    assert_eq!(report.error_count(), 0, "{}", report.render());
    assert_eq!(report.warning_count(), 0, "{}", report.render());
    assert!(report.render().contains("Ship the thing (profile)"));
}

#[test]
fn new_without_a_name_numbers_the_default() {
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    assert_eq!(next_free_name(&profile), DEFAULT_NAME);
    assert_eq!(
        new_definition(Some(&profile), None).unwrap().name,
        DEFAULT_NAME
    );
    assert_eq!(
        new_definition(Some(&profile), None).unwrap().name,
        format!("{DEFAULT_NAME}-2")
    );
    assert_eq!(next_free_name(&profile), format!("{DEFAULT_NAME}-3"));
}

#[test]
fn new_refuses_bad_names_existing_names_and_directories_and_never_overwrites() {
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    for bad in ["", "   ", "!!!", "a\nb", &"x".repeat(200)] {
        assert!(
            matches!(
                new_definition(Some(&profile), Some(bad)),
                Err(DefinitionError::InvalidName(_))
            ),
            "{bad:?}"
        );
    }
    assert!(!profile.exists(), "a refused name creates nothing");
    assert!(matches!(
        new_definition(None, Some("x")),
        Err(DefinitionError::NoProfileDir)
    ));

    let created = new_definition(Some(&profile), Some("Alpha")).unwrap();
    let before = fs::read_to_string(&created.file).unwrap();
    assert!(matches!(
        new_definition(Some(&profile), Some("Alpha")),
        Err(DefinitionError::NameTaken(..))
    ));
    // The same directory name with another name: the directory exists.
    assert!(matches!(
        new_definition(Some(&profile), Some("alpha")),
        Err(DefinitionError::DirectoryExists(_))
    ));
    assert_eq!(fs::read_to_string(&created.file).unwrap(), before);

    // A name that needs quoting still yields a definition that parses back to the same name.
    let quoted = new_definition(Some(&profile), Some("say \"hi\" \\ there")).unwrap();
    let report =
        validate_definitions(&profile_only(&profile), Some("say \"hi\" \\ there")).unwrap();
    assert_eq!(report.error_count(), 0, "{}", report.render());
    assert_eq!(quoted.name, "say \"hi\" \\ there");
}

#[test]
fn definition_path_resolves_by_name_and_takes_the_first_by_name() {
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("workflows");
    let dirs = profile_only(&profile);
    assert!(matches!(
        definition_path(&dirs, None),
        Err(DefinitionError::NoDefinitions)
    ));
    let beta = new_definition(Some(&profile), Some("beta")).unwrap();
    let alpha = new_definition(Some(&profile), Some("alpha")).unwrap();
    assert_eq!(definition_path(&dirs, Some("beta")).unwrap(), beta.file);
    assert_eq!(definition_path(&dirs, None).unwrap(), alpha.file);
    assert!(matches!(
        definition_path(&dirs, Some("gamma")),
        Err(DefinitionError::NotFound(_))
    ));
}

#[test]
fn a_project_definition_shadows_a_profile_one_of_the_same_name() {
    let temp = TempDir::new().unwrap();
    let profile = temp.path().join("profile");
    let project = temp.path().join("project");
    write_definition(&profile, "x", &BROKEN.replace("broken", "same"));
    write_definition(&project, "y", &BROKEN.replace("broken", "same"));
    let dirs = LayerDirs {
        profile: Some(profile),
        project: Some(project.clone()),
        ..LayerDirs::default()
    };
    assert_eq!(
        definition_path(&dirs, Some("same")).unwrap(),
        project.join("y").join(DEFINITION_FILE)
    );
}
