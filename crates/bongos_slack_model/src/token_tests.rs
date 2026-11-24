use crate::token::{HIDDEN, SlackToken, TokenError, TokenKind, redact};

#[test]
fn token_kinds_are_detected_and_refusals_never_echo_the_token() {
    assert_eq!(
        SlackToken::parse("xoxp-1111-2222-abcd").unwrap().kind(),
        TokenKind::User
    );
    assert_eq!(
        SlackToken::parse("  xoxb-1111-2222-abcd\n").unwrap().kind(),
        TokenKind::Bot
    );

    let cases = [
        ("xoxe.xoxp-1-secretrefresh", TokenError::Refresh),
        ("xoxe-1-secretrefresh", TokenError::Refresh),
        ("xapp-1-A0SECRETAPP-1234", TokenError::AppLevel),
        ("hunter2secretgarbage", TokenError::NotAToken),
        ("", TokenError::NotAToken),
        ("xoxp-", TokenError::NotAToken),
    ];
    for (text, expected) in cases {
        let error = SlackToken::parse(text).unwrap_err();
        assert_eq!(error, expected, "{text}");
        let message = error.to_string();
        if !text.is_empty() {
            assert!(!message.contains(text), "{message}");
        }
        assert!(!message.contains("secret"), "{message}");
    }
    assert_eq!(
        TokenError::Refresh.to_string(),
        "this is a refresh token; turn off token rotation for the app and paste its user OAuth token"
    );
    assert_eq!(
        TokenError::AppLevel.to_string(),
        "an app-level token cannot read or post"
    );
}

#[test]
fn debug_hides_the_token() {
    let token = SlackToken::parse("xoxp-1111-2222-topsecret").unwrap();
    assert_eq!(format!("{token:?}"), "slack token (hidden)");
    assert_eq!(
        format!("{:?}", Some(token.clone())),
        "Some(slack token (hidden))"
    );
    assert!(!format!("{token:#?}").contains("topsecret"));
}

#[test]
fn redact_hides_every_token() {
    let token = SlackToken::parse("xoxp-1111-2222-topsecret").unwrap();
    assert_eq!(
        token.redact("Authorization: Bearer xoxp-1111-2222-topsecret failed"),
        format!("Authorization: Bearer {HIDDEN} failed")
    );
    assert_eq!(
        redact("bot xoxb-9-9-zz, refresh xoxe.xoxp-1-abc and app xapp-1-A0-9."),
        format!("bot {HIDDEN}, refresh {HIDDEN} and app {HIDDEN}.")
    );
    assert_eq!(
        redact("no token here, xox or xoxo"),
        "no token here, xox or xoxo"
    );
    assert_eq!(redact("café xoxp-1-é"), format!("café {HIDDEN}é"));
    assert_eq!(redact(""), "");
}
