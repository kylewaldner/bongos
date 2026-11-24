use crate::types::{ConversationId, Destination, DestinationError, Ts};

#[test]
fn destinations_parse_and_malformed_ones_are_refused() {
    let channel: Destination = "C0123".parse().unwrap();
    assert_eq!(channel.conversation, ConversationId("C0123".to_string()));
    assert_eq!(channel.thread, None);
    assert_eq!(channel.to_string(), "C0123");

    let thread = Destination::parse(" D0ABC:1730000000.000100 ").unwrap();
    assert_eq!(thread.conversation.as_str(), "D0ABC");
    assert_eq!(thread.thread, Some(Ts::new("1730000000.000100")));
    assert_eq!(thread.to_string(), "D0ABC:1730000000.000100");
    assert_eq!(Destination::parse(&thread.to_string()), Ok(thread));
    assert!(Destination::parse("G01").is_ok());

    assert_eq!(Destination::parse(""), Err(DestinationError::Empty));
    assert_eq!(Destination::parse("   "), Err(DestinationError::Empty));
    for bad in [
        "#general",
        "c0123",
        "C",
        "X0123",
        "C01-23",
        ":1730000000.000100",
    ] {
        assert!(
            matches!(
                Destination::parse(bad),
                Err(DestinationError::BadConversation | DestinationError::Empty)
            ),
            "{bad}"
        );
    }
    for bad in [
        "C0123:",
        "C0123:abc",
        "C0123:1.",
        "C0123:.5",
        "C0123:1.2.3",
        "C0123:1.1",
        "C0123:1730000000",
        "C0123:1730000000.0001000",
    ] {
        assert!(
            matches!(Destination::parse(bad), Err(DestinationError::BadTs)),
            "{bad}"
        );
    }

    // The errors never echo the input, which may be a pasted token.
    let secret = "xoxp-1234-secret";
    assert!(
        !Destination::parse(secret)
            .unwrap_err()
            .to_string()
            .contains(secret)
    );
    let with_ts = format!("C0123:{secret}");
    assert!(
        !Destination::parse(&with_ts)
            .unwrap_err()
            .to_string()
            .contains(secret)
    );

    // Timestamps order by seconds, then by the fraction, as decimals.
    let mut stamps = [
        Ts::new("1730000000.000100"),
        Ts::new("999999999.900000"),
        Ts::new("1730000000.000099"),
        Ts::new("1730000001.000000"),
        Ts::new("1730000000.1"),
    ];
    stamps.sort();
    let sorted: Vec<&str> = stamps.iter().map(Ts::as_str).collect();
    assert_eq!(
        sorted,
        [
            "999999999.900000",
            "1730000000.000099",
            "1730000000.000100",
            "1730000000.1",
            "1730000001.000000"
        ]
    );
}
