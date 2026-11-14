use std::fs;
use std::path::{Path, PathBuf};

const CASES: [&str; 17] = [
    "bun",
    "biome",
    "java-gradle",
    "java-kotlin-mixed",
    "java-maven",
    "node-npm",
    "node-pnpm",
    "node-yarn",
    "python-conda",
    "python-poetry",
    "python-uv",
    "python-venv",
    "react-next",
    "react-router",
    "react-vite",
    "rust",
    "terraform",
];

fn fixtures_dir() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../script/bongos/fixtures/languages")
}

fn walk(dir: &Path, out: &mut Vec<PathBuf>) {
    for entry in fs::read_dir(dir).unwrap() {
        let path = entry.unwrap().path();
        out.push(path.clone());
        if path.is_dir() {
            walk(&path, out);
        }
    }
}

fn all_paths() -> Vec<PathBuf> {
    let mut out = Vec::new();
    walk(&fixtures_dir(), &mut out);
    out.sort();
    out
}

fn files_named(name: &str) -> Vec<PathBuf> {
    all_paths()
        .into_iter()
        .filter(|p| p.is_file() && p.file_name().is_some_and(|n| n == name))
        .collect()
}

#[test]
fn fixtures_directories_are_exactly_the_expected_cases() {
    let mut found: Vec<String> = fs::read_dir(fixtures_dir())
        .unwrap()
        .map(|e| e.unwrap())
        .filter(|e| e.path().is_dir())
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    let mut expected: Vec<String> = CASES.iter().map(|s| s.to_string()).collect();
    expected.sort();
    assert_eq!(found, expected);
}

#[test]
fn fixtures_manifests_parse() {
    let package_jsons = files_named("package.json");
    assert!(!package_jsons.is_empty());
    for path in package_jsons {
        let text = fs::read_to_string(&path).unwrap();
        serde_json::from_str::<serde_json::Value>(&text)
            .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    for name in ["Cargo.toml", "pyproject.toml"] {
        let paths = files_named(name);
        assert!(!paths.is_empty(), "no {name} found");
        for path in paths {
            let text = fs::read_to_string(&path).unwrap();
            toml::from_str::<toml::Table>(&text)
                .unwrap_or_else(|e| panic!("{}: {e}", path.display()));
        }
    }
    let poms = files_named("pom.xml");
    assert!(!poms.is_empty());
    for path in poms {
        let text = fs::read_to_string(&path).unwrap();
        check_xml(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
    let gradles = files_named("build.gradle.kts");
    assert!(!gradles.is_empty());
    for path in gradles {
        let text = fs::read_to_string(&path).unwrap();
        check_balanced(&text).unwrap_or_else(|e| panic!("{}: {e}", path.display()));
    }
}

#[test]
fn fixtures_have_no_installed_or_generated_directories() {
    let banned = ["node_modules", ".venv", "target", ".terraform"];
    for path in all_paths() {
        for component in path.strip_prefix(fixtures_dir()).unwrap().components() {
            let name = component.as_os_str().to_string_lossy();
            assert!(
                !banned.contains(&name.as_ref()),
                "{} contains {name}",
                path.display()
            );
        }
    }
}

/// Small well-formedness check: balanced tags and a single root element.
fn check_xml(text: &str) -> Result<(), String> {
    let mut stack: Vec<String> = Vec::new();
    let mut roots = 0;
    let mut rest = text;
    while let Some(start) = rest.find('<') {
        rest = &rest[start..];
        if let Some(body) = rest.strip_prefix("<?") {
            let end = body.find("?>").ok_or("unterminated declaration")?;
            rest = &body[end + 2..];
            continue;
        }
        if let Some(body) = rest.strip_prefix("<!--") {
            let end = body.find("-->").ok_or("unterminated comment")?;
            rest = &body[end + 3..];
            continue;
        }
        let end = rest.find('>').ok_or("unterminated tag")?;
        let tag = &rest[1..end];
        rest = &rest[end + 1..];
        if let Some(name) = tag.strip_prefix('/') {
            let open = stack.pop().ok_or("closing tag without opening tag")?;
            if open != name.trim() {
                return Err(format!("mismatched tag: <{open}> closed by </{name}>"));
            }
        } else {
            let self_closing = tag.ends_with('/');
            let name = tag
                .trim_end_matches('/')
                .split_whitespace()
                .next()
                .ok_or("empty tag")?
                .to_string();
            if stack.is_empty() {
                roots += 1;
            }
            if !self_closing {
                stack.push(name);
            }
        }
    }
    if !stack.is_empty() {
        return Err(format!("unclosed tags: {stack:?}"));
    }
    if roots != 1 {
        return Err(format!("expected one root element, found {roots}"));
    }
    Ok(())
}

fn check_balanced(text: &str) -> Result<(), String> {
    if text.trim().is_empty() {
        return Err("empty".into());
    }
    let mut stack = Vec::new();
    for c in text.chars() {
        match c {
            '{' | '(' => stack.push(c),
            '}' if stack.pop() != Some('{') => return Err("unbalanced }".into()),
            ')' if stack.pop() != Some('(') => return Err("unbalanced )".into()),
            _ => {}
        }
    }
    if stack.is_empty() {
        Ok(())
    } else {
        Err("unclosed bracket".into())
    }
}
