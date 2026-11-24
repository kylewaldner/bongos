use super::*;

#[test]
fn smart_case_is_sensitive_only_when_the_text_has_an_uppercase_letter() {
    let flags = Flags::default();
    assert!(!flags.case_sensitive("needle"));
    assert!(flags.case_sensitive("Needle"));
    let mut flags = flags;
    flags.cycle_case();
    assert!(flags.case_sensitive("needle"));
    flags.cycle_case();
    assert!(!flags.case_sensitive("Needle"));
    flags.cycle_case();
    assert_eq!(flags.case, CaseMode::Smart);
    let lit = Flags {
        regex: true,
        whole_word: true,
        open_buffers: true,
        ..Flags::default()
    };
    assert_eq!(lit.prompt_marks(), "[.*][Aa][w][buf]");
    let json = serde_json::to_string(&lit).unwrap();
    assert_eq!(serde_json::from_str::<Flags>(&json).unwrap(), lit);
}
