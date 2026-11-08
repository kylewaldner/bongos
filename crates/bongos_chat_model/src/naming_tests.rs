use chrono::NaiveDate;

use crate::{Agent, MAX_NAME_CHARS, default_name, from_prompt, from_title};

#[test]
fn default_name_is_type_agent_date() {
    let date = NaiveDate::from_ymd_opt(2026, 11, 8).unwrap();
    assert_eq!(
        default_name("implement", Agent::Claude, date),
        "implement claude 2026-11-08"
    );
    assert_eq!(
        default_name("free", Agent::Codex, date),
        "free codex 2026-11-08"
    );
}

#[test]
fn prompt_uses_the_first_non_empty_line() {
    assert_eq!(
        from_prompt("\n  \n  Add proration\nto renewals\n").as_deref(),
        Some("Add proration")
    );
}

#[test]
fn prompt_collapses_whitespace() {
    assert_eq!(
        from_prompt("fix \t the   bug\u{a0}now").as_deref(),
        Some("fix the bug now")
    );
}

#[test]
fn empty_prompt_gives_no_name() {
    assert_eq!(from_prompt(""), None);
    assert_eq!(from_prompt(" \n\t\n"), None);
}

#[test]
fn short_prompt_is_kept_whole() {
    let exactly = "a".repeat(MAX_NAME_CHARS);
    assert_eq!(from_prompt(&exactly).as_deref(), Some(exactly.as_str()));
}

#[test]
fn long_prompt_is_cut_at_a_word_boundary() {
    let prompt = "Refactor the invoice renewal proration so that partial months are charged daily";
    let name = from_prompt(prompt).unwrap();
    assert_eq!(name, "Refactor the invoice renewal proration so that");
    assert!(name.chars().count() <= MAX_NAME_CHARS);
}

#[test]
fn a_word_ending_exactly_at_the_limit_is_kept() {
    let head = format!("{} {}", "a".repeat(20), "b".repeat(27));
    assert_eq!(head.chars().count(), MAX_NAME_CHARS);
    let name = from_prompt(&format!("{head} tail")).unwrap();
    assert_eq!(name, head);
}

#[test]
fn a_single_long_word_is_cut_in_the_middle() {
    let name = from_prompt(&"x".repeat(100)).unwrap();
    assert_eq!(name.chars().count(), MAX_NAME_CHARS);
}

#[test]
fn cutting_counts_characters_not_bytes() {
    let name = from_prompt(&"é".repeat(100)).unwrap();
    assert_eq!(name.chars().count(), MAX_NAME_CHARS);
    let words = "ünï cödé ".repeat(20);
    let name = from_prompt(&words).unwrap();
    assert!(name.chars().count() <= MAX_NAME_CHARS);
    assert!(!name.ends_with(' '));
}

#[test]
fn title_names_the_chat() {
    assert_eq!(
        from_title("Fix the login redirect", "claude").as_deref(),
        Some("Fix the login redirect")
    );
}

#[test]
fn title_that_is_the_program_name_is_ignored() {
    assert_eq!(from_title("claude", "claude"), None);
    assert_eq!(from_title("  Codex ", "codex"), None);
}

#[test]
fn spinner_only_titles_are_ignored() {
    assert_eq!(from_title("⠋", "claude"), None);
    assert_eq!(from_title("⠙ ⠹", "claude"), None);
    assert_eq!(from_title("✳", "claude"), None);
    assert_eq!(from_title("", "claude"), None);
    assert_eq!(from_title("   ", "claude"), None);
}

#[test]
fn a_spinner_in_front_of_a_title_is_dropped() {
    assert_eq!(
        from_title("⠴ Fix the login redirect", "claude").as_deref(),
        Some("Fix the login redirect")
    );
    assert_eq!(from_title("✳ claude", "claude"), None);
}

#[test]
fn long_titles_are_cut_like_prompts() {
    let title = "word ".repeat(30);
    let name = from_title(&title, "claude").unwrap();
    assert!(name.chars().count() <= MAX_NAME_CHARS);
    assert!(name.ends_with("word"));
}
