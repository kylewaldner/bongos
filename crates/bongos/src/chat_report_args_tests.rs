use clap::Parser as _;

use super::Args;

fn parse(words: &[&str]) -> Args {
    Args::try_parse_from(std::iter::once("bongos").chain(words.iter().copied())).unwrap()
}

#[test]
fn chat_report_takes_every_word_after_it() {
    let args = parse(&["--chat-report", "state", "idle"]);
    assert_eq!(
        args.chat_report,
        Some(vec!["state".to_string(), "idle".to_string()])
    );
    assert!(args.paths_or_urls.is_empty());

    let args = parse(&["--chat-report", "claude-hook"]);
    assert_eq!(args.chat_report, Some(vec!["claude-hook".to_string()]));
    assert!(args.paths_or_urls.is_empty());

    let args = parse(&["--chat-report", "state", "--odd", "-x"]);
    assert_eq!(
        args.chat_report,
        Some(vec![
            "state".to_string(),
            "--odd".to_string(),
            "-x".to_string()
        ])
    );
}

#[test]
fn paths_without_chat_report_stay_paths() {
    let args = parse(&["src/main.rs", "README.md"]);
    assert_eq!(args.chat_report, None);
    assert_eq!(args.paths_or_urls, vec!["src/main.rs", "README.md"]);
}

#[test]
fn chat_report_needs_a_mode() {
    assert!(Args::try_parse_from(["bongos", "--chat-report"]).is_err());
}
