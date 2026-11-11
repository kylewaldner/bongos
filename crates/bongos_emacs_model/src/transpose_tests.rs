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
            '»' => carets.push(Caret::new(text.len(), open.take().unwrap_or(0))),
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
            Caret::new(head, tail)
        })
        .collect()
}

fn sorted_chars(text: &str) -> Vec<char> {
    let mut chars: Vec<char> = text.chars().collect();
    chars.sort_unstable();
    chars
}

/// Checks the invariants of a rewriting plan: sorted disjoint edits, the same
/// chars before and after, carets on char boundaries and without regions.
fn check_plan(text: &str, carets: &[Caret], plan: &Plan) -> String {
    let mut last_end = 0;
    for (range, _) in &plan.edits {
        assert!(range.start >= last_end, "{text:?} {carets:?} {plan:?}");
        last_end = range.end;
    }
    assert!(plan.killed.is_empty());
    let result = apply(text, plan);
    assert_eq!(result.len(), text.len(), "{text:?} {carets:?} {plan:?}");
    assert_eq!(sorted_chars(&result), sorted_chars(text));
    for c in &plan.carets {
        assert!(!c.has_region());
        assert!(
            result.is_char_boundary(c.head),
            "{text:?} {carets:?} {plan:?}"
        );
    }
    result
}

fn chars(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    transpose_chars(t, c, n)
}
fn words(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    transpose_words(t, c, n)
}
fn words_reverse(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    transpose_words_reverse(t, c, n)
}
fn lines(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    transpose_lines(t, c, n)
}

#[test]
fn transpose_chars_swaps_around_point() {
    assert_eq!(run("abˇcd", None, chars).0, "acbˇd");
    assert_eq!(run("abˇcd", Some(1), chars).0, "acbˇd");
    assert_eq!(run("ßˇé", None, chars).0, "éßˇ");
}

#[test]
fn transpose_chars_at_end_of_line_swaps_the_two_before_point() {
    assert_eq!(run("abcˇ", None, chars).0, "acbˇ");
    assert_eq!(run("abcˇ\ndef", None, chars).0, "acbˇ\ndef");
    assert_eq!(run("a\nbˇ\n", None, chars).0, "a\nbˇ\n");
    assert_eq!(run("a\nˇ", None, chars).0, "a\nˇ");
    assert_eq!(run("aˇ", None, chars).0, "aˇ");
}

#[test]
fn transpose_chars_count() {
    assert_eq!(run("aˇbcd", Some(2), chars).0, "bcaˇd");
    assert_eq!(run("aˇbc", Some(9), chars).0, "bcaˇ");
    assert_eq!(run("abcˇdef", Some(-1), chars).0, "acˇbdef");
    assert_eq!(run("abcˇdef", Some(-2), chars).0, "cˇabdef");
    assert_eq!(run("aˇbc", Some(-3), chars).0, "aˇbc");
    assert_eq!(run("abˇcd", Some(0), chars).0, "abˇcd");
}

#[test]
fn transpose_chars_at_the_ends() {
    assert_eq!(run("ˇab", None, chars).0, "ˇab");
    assert_eq!(run("ˇ", None, chars).0, "ˇ");
    assert_eq!(run("abˇ", Some(2), chars).0, "abˇ");
    assert_eq!(run("a\nˇb", None, chars).0, "ab\nˇ");
}

#[test]
fn transpose_chars_clears_the_region_and_acts_at_point() {
    let (out, plan) = run("a«bc»d", None, chars);
    assert_eq!(out, "abdcˇ");
    assert!(!plan.carets[0].has_region());
}

#[test]
fn transpose_chars_two_carets() {
    assert_eq!(run("aˇbcˇd", None, chars).0, "baˇdcˇ");
    let (out, plan) = run("aˇbˇc", None, chars);
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(out, "baˇc");
}

#[test]
fn transpose_words_swaps_the_word_before_point_with_the_next() {
    assert_eq!(run("foo ˇbar baz", None, words).0, "bar fooˇ baz");
    assert_eq!(run("fooˇ bar baz", None, words).0, "bar fooˇ baz");
    assert_eq!(run("foo, ˇbar", None, words).0, "bar, fooˇ");
    assert_eq!(run("fooˇbar baz", None, words).0, "baz foobarˇ");
    assert_eq!(run("ßa ˇé b", None, words).0, "é ßaˇ b");
    assert_eq!(run("foo barˇ", None, words).0, "foo barˇ");
    assert_eq!(run("ˇfoo bar", None, words).0, "ˇfoo bar");
    assert_eq!(run("ˇ", None, words).0, "ˇ");
}

#[test]
fn transpose_words_count_moves_the_word_forward() {
    assert_eq!(
        run("fooˇ bar baz qux", Some(2), words).0,
        "bar baz fooˇ qux"
    );
    assert_eq!(run("fooˇ bar", Some(5), words).0, "bar fooˇ");
    assert_eq!(run("fooˇ bar", Some(0), words).0, "fooˇ bar");
}

#[test]
fn transpose_words_negative_count_moves_it_backward() {
    assert_eq!(run("foo barˇ baz", Some(-1), words).0, "barˇ foo baz");
    assert_eq!(run("a b cˇ", Some(-2), words).0, "cˇ a b");
    assert_eq!(run("a b cˇ", Some(-9), words).0, "cˇ a b");
    assert_eq!(run("fooˇ bar", Some(-1), words).0, "fooˇ bar");
}

#[test]
fn transpose_words_reverse_moves_backward() {
    assert_eq!(run("foo barˇ baz", None, words_reverse).0, "barˇ foo baz");
    assert_eq!(run("a b cˇ", Some(2), words_reverse).0, "cˇ a b");
    assert_eq!(
        run("fooˇ bar baz", Some(-1), words_reverse).0,
        "bar fooˇ baz"
    );
}

#[test]
fn transpose_words_region_forms() {
    assert_eq!(run("«foo» bar", None, words).0, "bar fooˇ");
    assert_eq!(run("«foo bar» baz qux", None, words).0, "baz foo barˇ qux");
    assert_eq!(run("foo «bar» baz", None, words_reverse).0, "barˇ foo baz");
    assert_eq!(run("x «y» z", Some(-1), words).0, "yˇ x z");
    assert_eq!(run("x «y» z", Some(7), words).0, "x z yˇ");
    assert_eq!(run("«foo»", None, words).0, "fooˇ");
}

#[test]
fn transpose_words_carets_in_one_word_act_once() {
    let (out, plan) = run("fooˇbarˇ baz", None, words);
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(apply("foobar baz", &plan), "baz foobar");
    assert_eq!(out.matches('ˇ').count(), 1);
}

#[test]
fn transpose_lines_swaps_with_the_line_above() {
    assert_eq!(run("a\nbˇ\nc", None, lines).0, "b\naˇ\nc");
    assert_eq!(run("a\nˇb\nc", Some(1), lines).0, "b\naˇ\nc");
    assert_eq!(run("a\nbˇ", None, lines).0, "b\naˇ");
    assert_eq!(run("aˇ\nb", None, lines).0, "aˇ\nb");
    assert_eq!(run("ˇ", None, lines).0, "ˇ");
    assert_eq!(run("é\nßˇ", None, lines).0, "ß\néˇ");
}

#[test]
fn transpose_lines_count() {
    assert_eq!(run("a\nbˇ\nc\nd", Some(2), lines).0, "b\nc\naˇ\nd");
    assert_eq!(run("a\nbˇ\nc", Some(9), lines).0, "b\nc\naˇ");
    assert_eq!(run("a\nb\ncˇ", Some(-1), lines).0, "bˇ\na\nc");
    assert_eq!(run("a\nb\nc\ndˇ", Some(-2), lines).0, "cˇ\na\nb\nd");
    assert_eq!(run("a\nbˇ", Some(-1), lines).0, "a\nbˇ");
    assert_eq!(run("a\nbˇ\nc", Some(0), lines).0, "a\nbˇ\nc");
}

#[test]
fn transpose_lines_clears_the_region() {
    let (out, plan) = run("a\n«b\nc»", None, lines);
    assert_eq!(out, "a\nc\nbˇ");
    assert!(!plan.carets[0].has_region());
}

#[test]
fn randomized_transpose_plans_are_consistent() {
    let alphabet: Vec<char> = "ab c\n\n_ßé,".chars().collect();
    let mut rng = Lcg(11);
    for _ in 0..3000 {
        let text = random_text(&mut rng, &alphabet);
        let carets = random_carets(&mut rng, &text);
        let count = [None, Some(1), Some(0), Some(-1), Some(3), Some(-3)][rng.next(6)];
        for plan in [
            chars(&text, &carets, count),
            words(&text, &carets, count),
            words_reverse(&text, &carets, count),
            lines(&text, &carets, count),
        ] {
            check_plan(&text, &carets, &plan);
        }
    }
}
