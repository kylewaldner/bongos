use std::path::PathBuf;

use bongos_work_model::SharedArtifact;

use crate::context_digest;
use crate::digest::fnv1a64;

fn artifact(path: &str, description: &str) -> SharedArtifact {
    SharedArtifact {
        path: PathBuf::from(path),
        description: description.to_string(),
    }
}

#[test]
fn fnv1a64_matches_the_published_vectors() {
    assert_eq!(fnv1a64(b""), 0xcbf2_9ce4_8422_2325);
    assert_eq!(fnv1a64(b"a"), 0xaf63_dc4c_8601_ec8c);
    assert_eq!(fnv1a64(b"foobar"), 0x8594_4171_f739_67e8);
}

#[test]
fn digest_of_a_fixed_list_is_a_fixed_string() {
    // The value is written to records on disk, so it must be the same in every process and
    // every version. If this fails, old records would all look changed.
    let list = [
        artifact("/home/kyle/.bongos/projects/p/design.md", "The design"),
        artifact("/home/kyle/.bongos/projects/p/notes.md", ""),
    ];
    assert_eq!(context_digest(&list), "fnv1a64:7de7ab75d068e178");
    assert_eq!(context_digest(&[]), "fnv1a64:cbf29ce484222325");
}

#[test]
fn order_of_the_list_does_not_matter() {
    let a = artifact("/a", "first");
    let b = artifact("/b", "second");
    assert_eq!(
        context_digest(&[a.clone(), b.clone()]),
        context_digest(&[b, a])
    );
}

#[test]
fn any_change_to_the_list_changes_the_digest() {
    let base = context_digest(&[artifact("/a", "one")]);
    assert_ne!(base, context_digest(&[artifact("/a", "two")]));
    assert_ne!(base, context_digest(&[artifact("/b", "one")]));
    assert_ne!(base, context_digest(&[]));
    assert_ne!(
        base,
        context_digest(&[artifact("/a", "one"), artifact("/b", "x")])
    );
}

#[test]
fn moving_text_between_path_and_description_changes_the_digest() {
    assert_ne!(
        context_digest(&[artifact("/ab", "c")]),
        context_digest(&[artifact("/a", "bc")])
    );
}
