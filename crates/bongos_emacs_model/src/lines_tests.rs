use crate::*;

/// Parses `ˇ` (point) or `«…»` (region, head at the end) markers.
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
) -> (String, Plan) {
    let (text, carets) = parse(marked);
    let plan = f(text.as_str(), &carets, count);
    let result = apply(&text, &plan);
    (show(&result, &plan.carets), plan)
}

struct Lcg(u64);
impl Lcg {
    fn next(&mut self, bound: usize) -> usize {
        self.0 = self
            .0
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((self.0 >> 33) as usize) % bound
    }
}
fn random_text(rng: &mut Lcg, alphabet: &[char]) -> String {
    let len = rng.next(24);
    (0..len)
        .map(|_| alphabet[rng.next(alphabet.len())])
        .collect()
}

fn random_carets(rng: &mut Lcg, text: &str) -> Vec<Caret> {
    let bounds: Vec<usize> = (0..=text.len())
        .filter(|&i| text.is_char_boundary(i))
        .collect();
    (0..1 + rng.next(3))
        .map(|_| {
            let head = bounds[rng.next(bounds.len())];
            let tail = if rng.next(3) == 0 {
                bounds[rng.next(bounds.len())]
            } else {
                head
            };
            Caret { head, tail }
        })
        .collect()
}

fn dup(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    duplicate_line(t, c, n)
}
fn above(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    copy_from_above(t, c, n)
}

fn inserted(plan: &Plan) -> String {
    plan.edits.iter().map(|(_, new)| new.as_str()).collect()
}

fn check_insertions(text: &str, carets: &[Caret], plan: &Plan) -> String {
    let mut last_end = 0;
    for (range, new) in &plan.edits {
        assert!(range.is_empty() && !new.is_empty());
        assert!(range.start >= last_end, "{text:?} {carets:?} {plan:?}");
        last_end = range.end;
    }
    let result = apply(text, plan);
    assert_eq!(result.len(), text.len() + inserted(plan).len());
    for c in &plan.carets {
        assert!(
            result.is_char_boundary(c.head) && result.is_char_boundary(c.tail),
            "{text:?} {carets:?} {plan:?}"
        );
    }
    assert!(plan.killed.is_empty());
    result
}

#[test]
fn duplicate_line_copies_the_line_below() {
    assert_eq!(run("fooˇ bar", None, dup).0, "foo bar\nfooˇ bar");
    assert_eq!(run("a\nbˇ\nc", Some(1), dup).0, "a\nb\nbˇ\nc");
    assert_eq!(run("ˇ", None, dup).0, "\nˇ");
    assert_eq!(run("é ˇß", None, dup).0, "é ß\né ˇß");
}

#[test]
fn duplicate_line_count() {
    assert_eq!(run("aˇb", Some(3), dup).0, "ab\naˇb\nab\nab");
    assert_eq!(run("aˇb\nc", Some(2), dup).0, "ab\naˇb\nab\nc");
    assert_eq!(run("aˇb", Some(0), dup).0, "aˇb");
    assert_eq!(run("aˇb", Some(-2), dup).0, "aˇb");
}

#[test]
fn duplicate_line_copies_a_region_once_after_itself() {
    let (out, plan) = run("x «foo» y", None, dup);
    assert_eq!(out, "x foofooˇ y");
    assert!(!plan.carets[0].has_region());
    assert_eq!(run("x «foo» y", Some(3), dup).0, "x foofooˇ y");
    assert_eq!(run("«a\nb»\nc", None, dup).0, "a\nba\nbˇ\nc");
    assert_eq!(run("«x»", None, dup).0, "xxˇ");
}

#[test]
fn duplicate_line_carets_on_one_line_copy_it_once() {
    let (out, plan) = run("ˇa ˇb\nc", None, dup);
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(apply("a b\nc", &plan), "a b\na b\nc");
    assert_eq!(out.matches('ˇ').count(), 1);
    assert_eq!(run("aˇ\nbˇ", None, dup).0, "a\naˇ\nb\nbˇ");
}

#[test]
fn copy_from_above_copies_to_the_end_of_the_line() {
    assert_eq!(run("abc def\nˇ", None, above).0, "abc def\nabc defˇ");
    assert_eq!(run("abc def\nxxˇ", None, above).0, "abc def\nxxc defˇ");
    assert_eq!(run("ßé\nˇ", None, above).0, "ßé\nßéˇ");
}

#[test]
fn copy_from_above_copies_n_characters_from_the_column() {
    assert_eq!(run("abc def\n  ˇ", Some(3), above).0, "abc def\n  c dˇ");
    assert_eq!(run("abc\nˇ", Some(2), above).0, "abc\nabˇ");
    assert_eq!(run("abc\nˇ", Some(9), above).0, "abc\nabcˇ");
    assert_eq!(run("ßé\nˇ", Some(1), above).0, "ßé\nßˇ");
    assert_eq!(run("aßé\nxˇ", Some(1), above).0, "aßé\nxßˇ");
    assert_eq!(run("abc\nˇ", Some(0), above).0, "abc\nˇ");
    assert_eq!(run("abc\nˇ", Some(-1), above).0, "abc\nˇ");
}

#[test]
fn copy_from_above_over_a_blank_line_uses_the_nearest_non_blank_line() {
    assert_eq!(run("abc\n\n  \nˇ", None, above).0, "abc\n\n  \nabcˇ");
    assert_eq!(
        run("xyz\nabc\n \t\nˇ", Some(2), above).0,
        "xyz\nabc\n \t\nabˇ"
    );
    assert_eq!(run("abc\nˇ\n", None, above).0, "abc\nabcˇ\n");
}

#[test]
fn copy_from_above_does_nothing_without_text_to_copy() {
    assert_eq!(run("ˇabc", None, above).0, "ˇabc");
    assert_eq!(run("\n\nˇ", None, above).0, "\n\nˇ");
    assert_eq!(run("ab\nxxxˇ", None, above).0, "ab\nxxxˇ");
    assert_eq!(run("ˇ", None, above).0, "ˇ");
}

#[test]
fn copy_from_above_leaves_a_region_alone() {
    let (out, plan) = run("abc\n«x»", None, above);
    assert_eq!(out, "abc\n«x»");
    assert!(plan.edits.is_empty());
}

#[test]
fn copy_from_above_with_several_carets() {
    assert_eq!(run("abc\nˇ\nˇ", None, above).0, "abc\nabcˇ\nabcˇ");
    assert_eq!(run("abc\nˇ\n«x»\nˇ", Some(1), above).0, "abc\naˇ\n«x»\nxˇ");
}

#[test]
fn whole_line_span_covers_the_touched_lines() {
    let span = |marked: &str| {
        let (text, carets) = parse(marked);
        let range = whole_line_span(text.as_str(), &carets[0]);
        text[range].to_string()
    };
    assert_eq!(span("ab ˇcd\nef"), "ab cd");
    assert_eq!(span("a\nb«c\nd»e\nf"), "bc\nde");
    assert_eq!(span("«a\nb\n»c"), "a\nb");
    assert_eq!(span("a\n«b»"), "b");
    assert_eq!(span("a\nˇ"), "");
    assert_eq!(span("ˇ"), "");
    assert_eq!(span("é«ß\nß»é"), "éß\nßé");
}

#[test]
fn randomized_line_plans_are_consistent() {
    let alphabet: Vec<char> = "ab \t\n\nßé".chars().collect();
    let mut rng = Lcg(13);
    for _ in 0..3000 {
        let text = random_text(&mut rng, &alphabet);
        let carets = random_carets(&mut rng, &text);
        let count = [None, Some(1), Some(0), Some(-1), Some(3)][rng.next(5)];
        for plan in [dup(&text, &carets, count), above(&text, &carets, count)] {
            check_insertions(&text, &carets, &plan);
        }
        let plan = above(&text, &carets, count);
        for (_, new) in &plan.edits {
            assert!(text.contains(new.as_str()), "{text:?} {carets:?} {plan:?}");
        }
    }
}
