use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use chrono::{DateTime, Duration, Utc};

use crate::*;

/// The fixed base of every time in these tests; other times are offsets from it.
fn base_time() -> DateTime<Utc> {
    DateTime::from_timestamp(1_700_000_000, 0).expect("a valid timestamp")
}

struct Key {
    pkcs8: Vec<u8>,
    public: Vec<u8>,
}

fn new_key() -> Key {
    let pkcs8 = generate_pkcs8().expect("generate a key");
    let public = public_key(&pkcs8).expect("read the public key");
    Key { pkcs8, public }
}

fn statement() -> ApprovalStatement {
    ApprovalStatement {
        project_id: "project-1".into(),
        change_id: "change-1".into(),
        author_patch_id: "author-patch".into(),
        amend_patch_id: "amend-patch".into(),
        commit: "c0ffee".into(),
        base: None,
        adopted: Vec::new(),
        basis: Basis::Land,
        approver: "alice".into(),
        key_id: String::new(),
        method: Method::Keyring,
        at: base_time(),
    }
}

fn expected() -> Expected {
    Expected {
        project_id: "project-1".into(),
        change_id: "change-1".into(),
        author_patch_id: "author-patch".into(),
        basis: Basis::Land,
    }
}

fn reason(state: &RowState) -> Option<&'static str> {
    match state {
        RowState::Unverified { reason, .. } => Some(reason.text()),
        _ => None,
    }
}

#[test]
fn signed_approval_round_trips_through_its_line_and_verifies() {
    let key = new_key();
    let trusted = FixedApprovers::new(vec![FixedKey::trusted("alice", key.public.clone())]);
    let mut adopting = statement();
    adopting.basis = Basis::Adopt;
    adopting.base = Some("base-tip".into());
    adopting.adopted = vec!["one".into(), "two".into()];
    let mut adopting_expected = expected();
    adopting_expected.basis = Basis::Adopt;

    for (statement, expected) in [(statement(), expected()), (adopting, adopting_expected)] {
        let signed = sign_with(&key.pkcs8, statement).expect("sign");
        assert_eq!(signed.statement.key_id, key_id(&key.public));
        assert_eq!(signed.statement.key_id.len(), 16);
        let line = signed.to_line();
        assert!(line.starts_with("ba1."));
        let parsed = SignedApproval::from_line(&line).expect("parse the line");
        assert_eq!(parsed, signed);
        assert_eq!(
            verify(&parsed, &trusted, &expected),
            RowState::Verified {
                method: Method::Keyring
            }
        );
    }
    assert_eq!(trusted.root_key_id(), Some(key_id(&key.public)));

    let other = new_key();
    let mut wrong_id = statement();
    wrong_id.key_id = key_id(&other.public);
    assert!(sign_with(&key.pkcs8, wrong_id).is_err());
}

#[test]
fn changing_any_field_breaks_the_signature() {
    let key = new_key();
    let other = new_key();
    // The second key is trusted so a changed key id still finds a key and reaches the signature.
    let trusted = FixedApprovers::new(vec![
        FixedKey::trusted("alice", key.public.clone()),
        FixedKey::trusted("alice", other.public.clone()),
    ]);
    let signed = sign_with(&key.pkcs8, statement()).expect("sign");
    let other_id = key_id(&other.public);

    let mutations: Vec<(&str, Box<dyn Fn(&mut ApprovalStatement)>)> = vec![
        ("project_id", Box::new(|s| s.project_id.push('x'))),
        ("change_id", Box::new(|s| s.change_id.push('x'))),
        ("author_patch_id", Box::new(|s| s.author_patch_id.push('x'))),
        ("amend_patch_id", Box::new(|s| s.amend_patch_id.push('x'))),
        ("commit", Box::new(|s| s.commit.push('x'))),
        ("base", Box::new(|s| s.base = Some("base-tip".into()))),
        ("adopted", Box::new(|s| s.adopted.push("one".into()))),
        ("basis", Box::new(|s| s.basis = Basis::Approve)),
        ("approver", Box::new(|s| s.approver = "mallory".into())),
        ("key_id", Box::new(move |s| s.key_id = other_id.clone())),
        ("method", Box::new(|s| s.method = Method::TestStore)),
        ("at", Box::new(|s| s.at += Duration::seconds(1))),
    ];
    assert_eq!(mutations.len(), 12, "every field is changed once");
    for (field, mutate) in mutations {
        let mut changed = signed.clone();
        mutate(&mut changed.statement);
        assert_ne!(
            changed.statement.signing_bytes(),
            signed.statement.signing_bytes(),
            "{field}"
        );
        let parsed = SignedApproval::from_line(&changed.to_line()).expect(field);
        assert_eq!(
            reason(&verify(&parsed, &trusted, &expected())),
            Some("signature does not match"),
            "{field}"
        );
    }

    let mut flipped = signed;
    flipped.signature[0] ^= 1;
    assert_eq!(
        reason(&verify(&flipped, &trusted, &expected())),
        Some("signature does not match")
    );
}

#[test]
fn encoded_newlines_and_equals_cannot_forge_a_field() {
    let key = new_key();
    let trusted = FixedApprovers::new(vec![FixedKey::trusted("alice", key.public.clone())]);
    let mut tricky = statement();
    tricky.change_id = "a\napprover=evil".into();
    tricky.commit = "a%0Aapprover=evil".into();
    tricky.amend_patch_id = "100%,=\r".into();
    tricky.base = Some("-".into());
    tricky.adopted = vec!["x,y".into(), String::new(), "%2C".into()];
    tricky.method = Method::Paired {
        requested_on: "host\nmethod=keyring".into(),
    };
    let signed = sign_with(&key.pkcs8, tricky).expect("sign");

    let bytes = signed.statement.signing_bytes();
    let text = String::from_utf8(bytes).expect("utf-8");
    assert_eq!(text.lines().count(), 13, "the header and twelve fields");
    assert_eq!(text.matches("\napprover=").count(), 1);
    assert_eq!(text.matches("\nmethod=").count(), 1);
    assert!(text.contains("\nchange_id=a%0Aapprover%3Devil\n"));
    assert!(text.contains("\ncommit=a%250Aapprover%3Devil\n"));
    for line in text.lines().skip(1) {
        assert_eq!(line.matches('=').count(), 1, "{line}");
    }

    let parsed = SignedApproval::from_line(&signed.to_line()).expect("parse");
    assert_eq!(parsed.statement, signed.statement);
    let mut tricky_expected = expected();
    tricky_expected.change_id = "a\napprover=evil".into();
    assert!(matches!(
        verify(&parsed, &trusted, &tricky_expected),
        RowState::Weaker { .. }
    ));

    // Bytes with an injected second approver line are not a statement at all.
    let forged = text.replacen("\napprover=alice\n", "\napprover=alice\napprover=evil\n", 1);
    assert!(ApprovalStatement::from_signing_bytes(forged.as_bytes()).is_err());
    // A value written with a raw = or a non-canonical escape is refused.
    let raw_equals = text.replacen("\nproject_id=project-1\n", "\nproject_id=a=b\n", 1);
    assert!(ApprovalStatement::from_signing_bytes(raw_equals.as_bytes()).is_err());
    let lowercase = text.replacen("%0A", "%0a", 1);
    assert_eq!(
        ApprovalStatement::from_signing_bytes(lowercase.as_bytes()),
        Err(LineError::NotCanonical)
    );
}

#[test]
fn untrusted_keys_and_mismatched_statements_give_the_fixed_reasons() {
    let at = base_time();
    let alice = new_key();
    let stranger = new_key();
    let chainless = new_key();
    let retired_early = new_key();
    let retired_now = new_key();
    let retired_later = new_key();
    let retired = |key: &Key, retired_at| FixedKey {
        retired_at: Some(retired_at),
        ..FixedKey::trusted("alice", key.public.clone())
    };
    let trusted = FixedApprovers::new(vec![
        FixedKey::trusted("alice", alice.public.clone()),
        FixedKey {
            chained: false,
            ..FixedKey::trusted("alice", chainless.public.clone())
        },
        retired(&retired_early, at - Duration::hours(1)),
        retired(&retired_now, at),
        retired(&retired_later, at + Duration::hours(1)),
    ]);
    let check = |key: &Key, statement: ApprovalStatement, expected: &Expected| {
        let signed = sign_with(&key.pkcs8, statement).expect("sign");
        verify(&signed, &trusted, expected)
    };

    let cases: Vec<(&str, &Key, ApprovalStatement, Expected, Option<&str>)> = vec![
        (
            "unknown key",
            &stranger,
            statement(),
            expected(),
            Some("unknown approval key"),
        ),
        (
            "no chain",
            &chainless,
            statement(),
            expected(),
            Some("approval key has no valid chain"),
        ),
        (
            "retired before",
            &retired_early,
            statement(),
            expected(),
            Some("key retired before the approval"),
        ),
        (
            "retired at the approval",
            &retired_now,
            statement(),
            expected(),
            Some("key retired before the approval"),
        ),
        (
            "retired after",
            &retired_later,
            statement(),
            expected(),
            None,
        ),
        (
            "wrong approver",
            &alice,
            ApprovalStatement {
                approver: "mallory".into(),
                ..statement()
            },
            expected(),
            Some("approver does not match the key"),
        ),
        (
            "wrong change",
            &alice,
            statement(),
            Expected {
                change_id: "change-2".into(),
                ..expected()
            },
            Some("statement is for another change"),
        ),
        (
            "wrong project",
            &alice,
            statement(),
            Expected {
                project_id: "project-2".into(),
                ..expected()
            },
            Some("statement is for another change"),
        ),
        (
            "wrong author version",
            &alice,
            statement(),
            Expected {
                author_patch_id: "other".into(),
                ..expected()
            },
            Some("statement is for another change"),
        ),
        (
            "wrong basis",
            &alice,
            statement(),
            Expected {
                basis: Basis::Edit,
                ..expected()
            },
            Some("statement is for another change"),
        ),
    ];
    for (name, key, statement, expected, want) in cases {
        let state = check(key, statement, &expected);
        assert_eq!(reason(&state), want, "{name}");
        if let RowState::Unverified { claimed_by, .. } = &state {
            assert!(!claimed_by.is_empty(), "{name}");
        }
    }

    assert_eq!(
        verify_line(None, "alice", &trusted, &expected()),
        RowState::unsigned("alice")
    );
    assert_eq!(reason(&RowState::unsigned("alice")), Some("no signature"));
}

#[test]
fn malformed_lines_are_errors() {
    let key = new_key();
    let signed = sign_with(&key.pkcs8, statement()).expect("sign");
    let line = signed.to_line();
    let parts: Vec<&str> = line.split('.').collect();
    let encode = |bytes: &[u8]| URL_SAFE_NO_PAD.encode(bytes);
    let bytes = signed.statement.signing_bytes();

    let mut not_utf8 = bytes.clone();
    not_utf8[30] = 0xff;
    let unknown_basis = String::from_utf8(bytes.clone())
        .expect("utf-8")
        .replace("basis=land", "basis=merge");
    let malformed = vec![
        String::new(),
        "ba1".to_string(),
        format!("ba1.{}", parts[1]),
        format!("{line}.extra"),
        format!("ba2.{}.{}", parts[1], parts[2]),
        format!("BA1.{}.{}", parts[1], parts[2]),
        format!("ba1.{}!.{}", parts[1], parts[2]),
        format!("ba1.{}.{}=", parts[1], parts[2]),
        format!("ba1.{}.{}", encode(&not_utf8), parts[2]),
        format!("ba1.{}.{}", encode(&bytes[..bytes.len() / 2]), parts[2]),
        format!("ba1.{}.{}", encode(&bytes[..bytes.len() - 1]), parts[2]),
        format!("ba1.{}.{}", encode(b"bongos-approval/2\n"), parts[2]),
        format!("ba1.{}.{}", encode(unknown_basis.as_bytes()), parts[2]),
        format!("ba1..{}", parts[2]),
    ];
    for bad in malformed {
        assert!(SignedApproval::from_line(&bad).is_err(), "{bad:?}");
    }
    for cut in 0..line.len() {
        // Any truncation is an error or, at worst, a statement that does not verify.
        if let Ok(parsed) = SignedApproval::from_line(&line[..cut]) {
            let trusted = FixedApprovers::new(vec![FixedKey::trusted("alice", key.public.clone())]);
            assert!(matches!(
                verify(&parsed, &trusted, &expected()),
                RowState::Unverified { .. }
            ));
        }
    }

    let trusted = FixedApprovers::new(vec![FixedKey::trusted("alice", key.public)]);
    assert_eq!(
        reason(&verify_line(
            Some("ba1.garbage"),
            "alice",
            &trusted,
            &expected()
        )),
        Some("signature does not match")
    );
}

#[test]
fn unverified_words_never_say_approved() {
    let reasons = [
        UnverifiedReason::NoSignature,
        UnverifiedReason::SignatureMismatch,
        UnverifiedReason::UnknownKey,
        UnverifiedReason::NoValidChain,
        UnverifiedReason::KeyRetired,
        UnverifiedReason::OtherChange,
        UnverifiedReason::ApproverMismatch,
    ];
    for reason in reasons {
        let state = RowState::Unverified {
            claimed_by: "mallory".into(),
            reason,
        };
        let (text, tone) = words(&ApprovalView::Row {
            approver: "alice",
            state: &state,
        });
        assert_eq!(
            text,
            format!("unverified approval claimed by mallory: {}", reason.text())
        );
        assert_eq!(tone, Tone::Error);
        assert!(!text.contains("approved"), "{text}");
        for mark in ['\u{2713}', '\u{2714}', '\u{2705}', '\u{2611}'] {
            assert!(!text.contains(mark), "{text}");
        }
    }

    let verified = RowState::Verified {
        method: Method::Keyring,
    };
    assert_eq!(
        words(&ApprovalView::Row {
            approver: "alice",
            state: &verified
        }),
        (
            "approved by alice, verified (signed with alice's approval key)".to_string(),
            Tone::Normal
        )
    );
    assert_eq!(
        words(&ApprovalView::Grant { approver: "alice" }),
        (
            "approved by alice's earlier grant, verified".to_string(),
            Tone::Normal
        )
    );
    assert_eq!(
        words(&ApprovalView::PreFeature),
        (
            "no verifiable approval (landed before approval integrity)".to_string(),
            Tone::Muted
        )
    );
}

#[test]
fn paired_and_test_store_approvals_verify_as_weaker() {
    let key = new_key();
    let trusted = FixedApprovers::new(vec![FixedKey::trusted("alice", key.public.clone())]);
    let cases = [
        (
            Method::Paired {
                requested_on: "laptop".into(),
            },
            "approved by alice, weaker verification: confirmed by pairing from another machine",
        ),
        (
            Method::TestStore,
            "approved by alice, weaker verification: test key store (debug build)",
        ),
    ];
    for (method, want) in cases {
        let signed = sign_with(
            &key.pkcs8,
            ApprovalStatement {
                method: method.clone(),
                ..statement()
            },
        )
        .expect("sign");
        let state = verify(&signed, &trusted, &expected());
        assert!(
            matches!(&state, RowState::Weaker { method: m, .. } if *m == method),
            "{state:?}"
        );
        assert_eq!(
            words(&ApprovalView::Row {
                approver: "alice",
                state: &state
            }),
            (want.to_string(), Tone::Warning)
        );
    }
}

#[test]
fn an_empty_trusted_list_verifies_nothing() {
    let key = new_key();
    let empty = FixedApprovers::default();
    let signed = sign_with(&key.pkcs8, statement()).expect("sign");
    assert_eq!(
        verify(&signed, &empty, &expected()),
        RowState::Unverified {
            claimed_by: "alice".into(),
            reason: UnverifiedReason::UnknownKey,
        }
    );
    assert_eq!(empty.root_key_id(), None);
    assert_ne!(
        empty.fingerprint(),
        FixedApprovers::new(vec![FixedKey::trusted("alice", key.public)]).fingerprint()
    );
}
