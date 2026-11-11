use std::path::{Path, PathBuf};

use crate::worktree_path::*;

const HOME: &str = "/home/ada";
const CENTRAL: &str = "/data/central-worktrees";
const REPOSITORY: &str = "/home/ada/code/shop";

fn input<'a>(template: Option<Configured<'a>>, slug: &'a str) -> ResolveInput<'a> {
    ResolveInput {
        template,
        repository_name: "shop",
        repository_path: Path::new(REPOSITORY),
        slug,
        task_id: "0196f1c2-aaaa-7bbb-8ccc-ddddeeeeffff",
        home: Path::new(HOME),
        central_dir: Path::new(CENTRAL),
    }
}

fn own(text: &str) -> Option<Configured<'_>> {
    Some(Configured {
        text,
        layer: TemplateLayer::Own,
    })
}

fn path_of(template: &str, slug: &str) -> PathBuf {
    resolve(&input(own(template), slug)).unwrap().path
}

#[test]
fn both_tokens_are_filled() {
    assert_eq!(
        path_of("/work/{repo}/{slug}", "fix-login"),
        PathBuf::from("/work/shop/fix-login")
    );
    assert_eq!(
        path_of("/work/{repo}-{slug}/tree", "fix-login"),
        PathBuf::from("/work/shop-fix-login/tree")
    );
}

#[test]
fn unknown_tokens_are_named() {
    assert_eq!(
        parse("/work/{branch}/{slug}"),
        Err(TemplateError::UnknownToken("branch".to_string()))
    );
    assert_eq!(
        parse("/work/{}"),
        Err(TemplateError::UnknownToken(String::new()))
    );
}

#[test]
fn unbalanced_braces_are_malformed() {
    for template in [
        "/work/{slug",
        "/work/slug}",
        "/work/{re{po}}",
        "/work/}{slug}",
    ] {
        assert_eq!(parse(template), Err(TemplateError::Malformed), "{template}");
    }
}

#[test]
fn an_empty_template_is_refused() {
    assert_eq!(parse(""), Err(TemplateError::Empty));
    assert_eq!(parse("  "), Err(TemplateError::Empty));
}

#[test]
fn parent_components_are_refused() {
    for template in ["../{slug}", "/work/../{slug}", "/work/..", "..\\x"] {
        assert_eq!(
            parse(template),
            Err(TemplateError::ParentComponent),
            "{template}"
        );
    }
    // Dots inside a name are fine.
    assert!(parse("/work/..hidden/{slug}").is_ok());
}

#[test]
fn a_template_without_slug_gets_it_appended() {
    assert_eq!(
        path_of("/work/trees", "fix"),
        PathBuf::from("/work/trees/fix")
    );
    assert_eq!(
        path_of("/work/trees/", "fix"),
        PathBuf::from("/work/trees/fix")
    );
    assert_eq!(
        path_of("/work/{repo}", "fix"),
        PathBuf::from("/work/shop/fix")
    );
    assert_eq!(path_of("~", "fix"), PathBuf::from("/home/ada/fix"));
}

#[test]
fn tilde_at_the_start_is_home() {
    assert_eq!(
        path_of("~/trees/{repo}/{slug}", "fix"),
        PathBuf::from("/home/ada/trees/shop/fix")
    );
    assert_eq!(path_of("~/{slug}", "fix"), PathBuf::from("/home/ada/fix"));
    // `~name` is not home.
    assert_eq!(
        path_of("~ada/{slug}", "fix"),
        PathBuf::from("/home/ada/code/~ada/fix")
    );
}

#[test]
fn a_tilde_from_a_token_is_not_home() {
    let mut tilde = input(own("{repo}/{slug}"), "fix");
    tilde.repository_name = "~";
    assert_eq!(
        resolve(&tilde).unwrap().path,
        PathBuf::from("/home/ada/code/~/fix")
    );
}

#[test]
fn a_relative_result_joins_the_repository_parent() {
    assert_eq!(
        path_of("{repo}-trees/{slug}", "fix"),
        PathBuf::from("/home/ada/code/shop-trees/fix")
    );
    assert_eq!(
        path_of("trees", "fix"),
        PathBuf::from("/home/ada/code/trees/fix")
    );
}

#[test]
fn a_tilde_repository_path_is_expanded_before_taking_its_parent() {
    let mut tilde = input(own("trees"), "fix");
    tilde.repository_path = Path::new("~/code/shop");
    assert_eq!(
        resolve(&tilde).unwrap().path,
        PathBuf::from("/home/ada/code/trees/fix")
    );
}

#[test]
fn token_values_are_one_path_component() {
    let mut odd = input(own("/work/{repo}/{slug}"), "a/b\\c");
    odd.repository_name = ".hidden/name";
    assert_eq!(
        resolve(&odd).unwrap().path,
        PathBuf::from("/work/-hidden-name/a-b-c")
    );

    let mut dots = input(own("/work/{repo}/{slug}"), "..");
    dots.repository_name = "..";
    assert_eq!(resolve(&dots).unwrap().path, PathBuf::from("/work/--/--"));
}

#[test]
fn empty_token_values_get_a_name() {
    let mut empty = input(own("/work/{repo}/{slug}"), "");
    empty.repository_name = "";
    assert_eq!(
        resolve(&empty).unwrap().path,
        PathBuf::from("/work/repository/task")
    );
}

#[test]
fn the_source_follows_the_layer() {
    let default = Some(Configured {
        text: "/work/{slug}",
        layer: TemplateLayer::Default,
    });
    assert_eq!(
        resolve(&input(default, "fix")).unwrap().source,
        Source::Default
    );
    assert_eq!(
        resolve(&input(own("/work/{slug}"), "fix")).unwrap().source,
        Source::Own
    );
    let central = resolve(&input(None, "fix")).unwrap();
    assert_eq!(central.source, Source::Central);
    assert_eq!(
        central.path,
        PathBuf::from("/data/central-worktrees/shop/fix")
    );
}

#[test]
fn a_bad_template_fails_the_resolve() {
    assert_eq!(
        resolve(&input(own("/work/{nope}"), "fix")),
        Err(ResolveError::Template(TemplateError::UnknownToken(
            "nope".to_string()
        )))
    );
}

#[test]
fn preview_shows_the_slug_as_task() {
    let shown = preview(
        Some("~/trees/{repo}"),
        "shop",
        Path::new(REPOSITORY),
        Path::new(HOME),
        Path::new(CENTRAL),
    );
    assert_eq!(shown, Ok("/home/ada/trees/shop/<task>".to_string()));
    let central = preview(
        None,
        "shop",
        Path::new(REPOSITORY),
        Path::new(HOME),
        Path::new(CENTRAL),
    );
    assert_eq!(
        central,
        Ok("/data/central-worktrees/shop/<task>".to_string())
    );
    assert_eq!(
        preview(
            Some("{x}"),
            "shop",
            Path::new(REPOSITORY),
            Path::new(HOME),
            Path::new(CENTRAL)
        ),
        Err(TemplateError::UnknownToken("x".to_string()))
    );
}

const SIXTY: &str = "refactor-the-checkout-flow-so-that-payment-retries-are-safe";

#[test]
fn a_short_slug_is_unchanged() {
    assert_eq!(path_slug("fix-login", "id", MAX_PATH_SLUG), "fix-login");
    let exact = "a".repeat(MAX_PATH_SLUG);
    assert_eq!(path_slug(&exact, "id", MAX_PATH_SLUG), exact);
}

#[test]
fn a_long_slug_is_cut_at_a_dash_with_a_stable_suffix() {
    assert_eq!(SIXTY.chars().count(), 59);
    let cut = path_slug(SIXTY, "task-1", MAX_PATH_SLUG);
    // The last dash at or before index 41 is the one after "that".
    assert!(
        cut.starts_with("refactor-the-checkout-flow-so-that-"),
        "{cut}"
    );
    let (stem, suffix) = cut.rsplit_once('-').unwrap();
    assert_eq!(suffix.len(), 6);
    assert!(suffix.chars().all(|c| c.is_ascii_hexdigit()));
    assert!(cut.chars().count() <= MAX_PATH_SLUG);
    assert!(SIXTY.starts_with(stem));
    assert!(SIXTY[stem.len()..].starts_with('-'));
    // FNV-1a 32 of "task-1", written out by hand elsewhere; pin it so the hash cannot drift.
    assert_eq!(suffix, expected_suffix("task-1"));
}

fn expected_suffix(id: &str) -> String {
    let mut hash = 2_166_136_261_u32;
    for byte in id.bytes() {
        hash = (hash ^ u32::from(byte)).wrapping_mul(16_777_619);
    }
    format!("{hash:08x}")[..6].to_string()
}

#[test]
fn the_hash_matches_known_fnv_1a_values() {
    // Known FNV-1a 32 test vectors: "" is 811c9dc5, "a" is e40c292c, "foobar" is bf9cf968.
    let long = "x".repeat(60);
    assert!(path_slug(&long, "", MAX_PATH_SLUG).ends_with("-811c9d"));
    assert!(path_slug(&long, "a", MAX_PATH_SLUG).ends_with("-e40c29"));
    assert!(path_slug(&long, "foobar", MAX_PATH_SLUG).ends_with("-bf9cf9"));
}

#[test]
fn a_hard_cut_when_no_dash_leaves_enough() {
    let no_dash = "x".repeat(60);
    let cut = path_slug(&no_dash, "id", MAX_PATH_SLUG);
    assert_eq!(cut.chars().count(), MAX_PATH_SLUG);
    assert!(cut.starts_with(&"x".repeat(41)));

    // The only dash is too early to keep a stem of 8 characters.
    let early = format!("abc-{}", "y".repeat(60));
    let cut = path_slug(&early, "id", MAX_PATH_SLUG);
    assert!(cut.starts_with("abc-yyyy"), "{cut}");
    assert_eq!(cut.chars().count(), MAX_PATH_SLUG);
}

#[test]
fn a_hard_cut_does_not_leave_a_double_dash() {
    let slug = format!("{}-{}", "z".repeat(41), "w".repeat(20));
    let cut = path_slug(&slug, "id", MAX_PATH_SLUG);
    assert!(!cut.contains("--"), "{cut}");
}

#[test]
fn two_long_names_with_one_prefix_get_different_folders() {
    let first = format!(
        "{}-first-variant-of-the-same-job",
        "shared-prefix".repeat(3)
    );
    let second = format!(
        "{}-second-variant-of-the-same-job",
        "shared-prefix".repeat(3)
    );
    let one = path_slug(&first, "id-one", MAX_PATH_SLUG);
    let two = path_slug(&second, "id-two", MAX_PATH_SLUG);
    assert_ne!(one, two);
    // Even with the same slug, different tasks differ.
    assert_ne!(
        path_slug(&first, "id-one", MAX_PATH_SLUG),
        path_slug(&first, "id-two", MAX_PATH_SLUG)
    );
}

#[test]
fn the_suffix_is_the_same_on_every_call() {
    let once = path_slug(SIXTY, "task-1", MAX_PATH_SLUG);
    for _ in 0..3 {
        assert_eq!(path_slug(SIXTY, "task-1", MAX_PATH_SLUG), once);
    }
    let a = path_of("/work/{slug}", SIXTY);
    assert_eq!(path_of("/work/{slug}", SIXTY), a);
}

#[test]
fn resolve_cuts_a_long_slug() {
    let path = path_of("/work/{slug}", SIXTY);
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert_eq!(
        name,
        path_slug(SIXTY, "0196f1c2-aaaa-7bbb-8ccc-ddddeeeeffff", MAX_PATH_SLUG)
    );
}

#[test]
fn a_long_root_shortens_the_slug() {
    // 160 characters of root leave less than 48 for the slug.
    let root = format!("/{}", "r".repeat(160));
    let template = format!("{root}/{{slug}}");
    let path = path_of(&template, SIXTY);
    let length = path.to_string_lossy().chars().count();
    assert!(length <= MAX_WORKTREE_PATH, "{length}");
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    assert!(name.chars().count() < MAX_PATH_SLUG, "{name}");
    assert!(name.chars().count() >= 15 - 3);
    assert_ne!(
        name,
        path_slug(SIXTY, "0196f1c2-aaaa-7bbb-8ccc-ddddeeeeffff", MAX_PATH_SLUG)
    );
}

#[test]
fn a_root_too_long_names_the_setting() {
    let root = format!("/{}", "r".repeat(190));
    let template = format!("{root}/{{slug}}");

    let Err(ResolveError::TooLong { path, setting }) = resolve(&input(own(&template), SIXTY))
    else {
        panic!("expected TooLong");
    };
    assert_eq!(setting, "work.repositories[shop].worktree_root");
    assert!(path.to_string_lossy().chars().count() > MAX_WORKTREE_PATH);

    let default = Some(Configured {
        text: &template,
        layer: TemplateLayer::Default,
    });
    let Err(ResolveError::TooLong { setting, .. }) = resolve(&input(default, SIXTY)) else {
        panic!("expected TooLong");
    };
    assert_eq!(setting, "work.default_worktree_root");

    let mut central = input(None, SIXTY);
    let long_central = format!("/{}", "c".repeat(190));
    central.central_dir = Path::new(&long_central);
    let Err(ResolveError::TooLong { setting, .. }) = resolve(&central) else {
        panic!("expected TooLong");
    };
    assert_eq!(setting, "work.default_worktree_root");
}

#[test]
fn the_too_long_message_names_path_and_setting() {
    let root = format!("/{}", "r".repeat(200));
    let template = format!("{root}/{{slug}}");
    let error = resolve(&input(own(&template), "fix")).unwrap_err();
    let message = error.to_string();
    assert!(message.starts_with("The worktree path /rrr"), "{message}");
    assert!(
        message.contains("is longer than 200 characters"),
        "{message}"
    );
    assert!(
        message.ends_with("shorten work.repositories[shop].worktree_root"),
        "{message}"
    );
}
