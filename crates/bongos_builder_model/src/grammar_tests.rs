use crate::grammar::{is_label, kind_name, unblock_name};
use bongos_work_model::{Kind, UnblockPoint};

#[test]
fn labels() {
    for good in ["a", "0", "a-b", "9-lives", "schema2"] {
        assert!(is_label(good), "{good}");
    }
    for bad in ["", "-a", "A", "a_b", "a b", "a/b", "é"] {
        assert!(!is_label(bad), "{bad}");
    }
}

#[test]
fn names_match_the_serialized_names() {
    for kind in [
        Kind::PrShip,
        Kind::PrMerge,
        Kind::Document,
        Kind::Spike,
        Kind::Babysit,
        Kind::Other,
    ] {
        let json = format!("{kind:?}");
        assert_eq!(
            kind_name(kind).replace('-', ""),
            json.to_lowercase(),
            "{kind:?}"
        );
    }
    assert_eq!(unblock_name(UnblockPoint::Review), "review");
    assert_eq!(unblock_name(UnblockPoint::Implemented), "implemented");
    assert_eq!(unblock_name(UnblockPoint::Merged), "merged");
}
