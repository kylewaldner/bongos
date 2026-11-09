use crate::*;

fn parse(marked: &str) -> (String, Vec<Caret>) {
    let mut text = String::new();
    let mut carets = Vec::new();
    let mut open = None;
    for c in marked.chars() {
        match c {
            'ˇ' => carets.push(Caret::point(text.len())),
            '«' => open = Some(text.len()),
            '»' => carets.push(Caret {
                head: text.len(),
                tail: open.take().unwrap_or(0),
            }),
            _ => text.push(c),
        }
    }
    (text, carets)
}

fn show(text: &str, carets: &[Caret]) -> String {
    let mut marks: Vec<(usize, char)> = Vec::new();
    for c in carets {
        if c.has_region() {
            marks.push((c.region().start, '«'));
            marks.push((c.region().end, '»'));
        } else {
            marks.push((c.head, 'ˇ'));
        }
    }
    marks.sort_by_key(|m| m.0);
    let mut out = String::new();
    let mut last = 0;
    for (offset, mark) in marks {
        out.push_str(&text[last..offset]);
        out.push(mark);
        last = offset;
    }
    out.push_str(&text[last..]);
    out
}

fn run(
    marked: &str,
    count: Option<i32>,
    f: impl Fn(&str, &[Caret], Option<i32>) -> Plan,
) -> String {
    let (text, carets) = parse(marked);
    let plan = f(text.as_str(), &carets, count);
    show(&apply(&text, &plan), &plan.carets)
}

fn up(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    upcase_word(t, c, n)
}
fn down(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    downcase_word(t, c, n)
}
fn cap(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    capitalize_word(t, c, n)
}
fn pup(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    upcase_previous_word(t, c, n)
}
fn pdown(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    downcase_previous_word(t, c, n)
}
fn pcap(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    capitalize_previous_word(t, c, n)
}

#[test]
fn upcase_word_counts() {
    assert_eq!(run("ˇfoo bar baz qux", None, up), "FOOˇ bar baz qux");
    assert_eq!(run("ˇfoo bar baz qux", Some(3), up), "FOO BAR BAZˇ qux");
    assert_eq!(run("ˇfoo bar", Some(0), up), "ˇfoo bar");
    assert_eq!(run("foo bar baz ˇqux", Some(-2), up), "foo BAR BAZ ˇqux");
    assert_eq!(run("ˇfoo bar", Some(50), up), "FOO BARˇ");
    assert_eq!(run("fˇoo", None, up), "fOOˇ");
}

#[test]
fn downcase_word_counts() {
    assert_eq!(run("ˇFOO BAR BAZ QUX", None, down), "fooˇ BAR BAZ QUX");
    assert_eq!(run("ˇFOO BAR BAZ QUX", Some(3), down), "foo bar bazˇ QUX");
    assert_eq!(run("ˇFOO", Some(0), down), "ˇFOO");
    assert_eq!(run("FOO BAR BAZ ˇQUX", Some(-2), down), "FOO bar baz ˇQUX");
}

#[test]
fn capitalize_word_counts() {
    assert_eq!(run("ˇfOO bAR bAZ qUX", None, cap), "Fooˇ bAR bAZ qUX");
    assert_eq!(run("ˇfOO bAR bAZ qUX", Some(3), cap), "Foo Bar Bazˇ qUX");
    assert_eq!(run("ˇfOO", Some(0), cap), "ˇfOO");
    assert_eq!(run("fOO bAR bAZ ˇqUX", Some(-2), cap), "fOO Bar Baz ˇqUX");
    assert_eq!(run("fooˇbar", None, cap), "fooBarˇ");
}

#[test]
fn previous_word_forms_keep_point() {
    assert_eq!(run("foo barˇ", None, pup), "foo BARˇ");
    assert_eq!(run("foo bar bazˇ", Some(3), pup), "FOO BAR BAZˇ");
    assert_eq!(run("foo barˇ", Some(0), pup), "foo barˇ");
    assert_eq!(run("FOO BAR BAZˇ", Some(2), pdown), "FOO bar bazˇ");
    assert_eq!(run("foo bAR bAZˇ", Some(2), pcap), "foo Bar Bazˇ");
    assert_eq!(run("foo barˇ", Some(50), pcap), "Foo Barˇ");
    // Negative reverses: words after point, point stays.
    assert_eq!(run("ˇfoo bar baz", Some(-2), pup), "ˇFOO BAR baz");
}

#[test]
fn region_forms_keep_the_selection() {
    assert_eq!(run("a «bc de» f", Some(5), up), "a «BC DE» f");
    assert_eq!(run("a «BC DE» f", None, down), "a «bc de» f");
    assert_eq!(run("a «bC dE» f", None, cap), "a «Bc De» f");
    assert_eq!(run("a «bc de» f", None, pup), "a «BC DE» f");
    assert_eq!(run("a «bc de» f", None, pdown), "a «bc de» f");
    assert_eq!(run("a «bc de» f", None, pcap), "a «Bc De» f");
}

#[test]
fn two_carets_in_one_word_convert_once() {
    let (text, carets) = parse("fˇoˇo bar");
    let plan = upcase_word(text.as_str(), &carets, None);
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(apply(&text, &plan), "fOO bar");
    assert_eq!(plan.carets, vec![Caret::point(3)]);
    assert_eq!(run("ˇfoo ˇbar", None, up), "FOOˇ BARˇ");
}

#[test]
fn eszett_changes_length() {
    assert_eq!(run("ˇstraße x", None, up), "STRASSEˇ x");
    // Later carets shift by the extra byte(s).
    assert_eq!(run("ˇß ˇß", None, up), "SSˇ SSˇ");
    assert_eq!(run("«ß» ˇx", None, up), "«SS» Xˇ");
    assert_eq!(run("ßˇ", Some(1), pup), "SSˇ");
    assert_eq!(run("ˇÄB", None, down), "äbˇ");
}
