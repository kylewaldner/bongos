use std::fs;
use std::path::Path;

use tempfile::TempDir;

use crate::layers::{DEFINITION_FILE, Entry, Layer, LayerDirs, resolve, resolve_entries};

fn definition(root: &Path, dir: &str, name: &str) {
    let dir = root.join(dir);
    fs::create_dir_all(&dir).unwrap();
    fs::write(
        dir.join(DEFINITION_FILE),
        format!("schema = 1\nname = \"{name}\"\nstart = \"a\"\n"),
    )
    .unwrap();
}

struct Dirs {
    _temp: TempDir,
    dirs: LayerDirs,
}

fn dirs() -> Dirs {
    let temp = TempDir::new().unwrap();
    let dirs = LayerDirs {
        shipped: Some(temp.path().join("shipped")),
        profile: Some(temp.path().join("profile")),
        project: Some(temp.path().join("project")),
    };
    Dirs { _temp: temp, dirs }
}

#[test]
fn project_beats_profile_beats_shipped() {
    let Dirs { _temp, dirs } = dirs();
    let shipped = dirs.shipped.clone().unwrap();
    let profile = dirs.profile.clone().unwrap();
    let project = dirs.project.clone().unwrap();
    definition(&shipped, "ship", "ship PR");
    definition(&shipped, "implement", "implement PR");
    definition(&shipped, "only-shipped", "only shipped");
    definition(&profile, "implement", "implement PR");
    definition(&profile, "mine", "my flow");
    definition(&project, "impl-local", "implement PR");

    let resolution = resolve(&dirs);
    assert!(resolution.duplicates.is_empty());
    assert!(resolution.problems.is_empty());

    let names = resolution
        .resolved
        .iter()
        .map(|resolved| resolved.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(
        names,
        ["implement PR", "my flow", "only shipped", "ship PR"]
    );

    let implement = resolution.get("implement PR").unwrap();
    assert_eq!(implement.layer, Layer::Project);
    assert_eq!(implement.dir, project.join("impl-local"));
    assert_eq!(implement.overrides, [Layer::Shipped, Layer::Profile]);

    assert_eq!(resolution.get("my flow").unwrap().layer, Layer::Profile);
    assert!(resolution.get("my flow").unwrap().overrides.is_empty());
    assert_eq!(resolution.get("ship PR").unwrap().layer, Layer::Shipped);
}

#[test]
fn precedence_uses_the_name_key_not_the_directory() {
    let Dirs { _temp, dirs } = dirs();
    definition(dirs.shipped.as_ref().unwrap(), "same-dir", "one");
    definition(dirs.project.as_ref().unwrap(), "same-dir", "two");

    let resolution = resolve(&dirs);
    assert_eq!(resolution.get("one").unwrap().layer, Layer::Shipped);
    assert_eq!(resolution.get("two").unwrap().layer, Layer::Project);
}

#[test]
fn duplicates_in_one_layer_are_reported_and_refused() {
    let Dirs { _temp, dirs } = dirs();
    let profile = dirs.profile.clone().unwrap();
    definition(dirs.shipped.as_ref().unwrap(), "ship", "dup");
    definition(&profile, "b", "dup");
    definition(&profile, "a", "dup");
    definition(&profile, "c", "unique");

    let resolution = resolve(&dirs);
    assert_eq!(resolution.duplicates.len(), 1);
    let duplicate = &resolution.duplicates[0];
    assert_eq!(duplicate.name, "dup");
    assert_eq!(duplicate.layer, Layer::Profile);
    assert_eq!(duplicate.dirs, [profile.join("a"), profile.join("b")]);
    assert_eq!(duplicate.message(), "duplicate name in profile: a/, b/");

    let dup = resolution.get("dup").unwrap();
    assert_eq!(dup.layer, Layer::Shipped);
    assert_eq!(resolution.get("unique").unwrap().layer, Layer::Profile);
}

#[test]
fn a_name_duplicated_everywhere_resolves_to_nothing() {
    let entries = ["a", "b"]
        .into_iter()
        .map(|dir| Entry {
            layer: Layer::Project,
            dir: dir.into(),
            name: "x".to_string(),
        })
        .collect();
    let resolution = resolve_entries(entries);
    assert!(resolution.resolved.is_empty());
    assert_eq!(resolution.duplicates.len(), 1);
}

#[test]
fn missing_layers_and_directories_are_empty() {
    let temp = TempDir::new().unwrap();
    let dirs = LayerDirs {
        shipped: None,
        profile: Some(temp.path().join("does-not-exist")),
        project: None,
    };
    assert_eq!(resolve(&dirs), Default::default());
}

#[test]
fn unreadable_definitions_are_problems_not_names() {
    let Dirs { _temp, dirs } = dirs();
    let profile = dirs.profile.clone().unwrap();
    definition(&profile, "good", "good");
    fs::create_dir_all(profile.join("broken")).unwrap();
    fs::write(profile.join("broken").join(DEFINITION_FILE), "schema = [").unwrap();
    fs::create_dir_all(profile.join("nameless")).unwrap();
    fs::write(
        profile.join("nameless").join(DEFINITION_FILE),
        "schema = 1\n",
    )
    .unwrap();
    fs::create_dir_all(profile.join("no-file")).unwrap();
    fs::write(profile.join("stray.txt"), "x").unwrap();

    let resolution = resolve(&dirs);
    assert_eq!(resolution.resolved.len(), 1);
    assert_eq!(resolution.problems.len(), 2);
    assert_eq!(resolution.problems[0].dir, profile.join("broken"));
    assert_eq!(resolution.problems[1].dir, profile.join("nameless"));
    assert_eq!(resolution.problems[0].layer, Layer::Profile);
}
