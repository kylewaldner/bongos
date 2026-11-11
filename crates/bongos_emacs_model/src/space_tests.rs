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

/// Checks the invariants every plan keeps and returns the removed text.
fn check_plan(text: &str, carets: &[Caret], plan: &Plan) -> String {
    let mut removed = String::new();
    let mut last_end = 0;
    for (range, new) in &plan.edits {
        assert!(
            range.start >= last_end,
            "edits overlap or unsorted: {text:?} {carets:?} {plan:?}"
        );
        assert!(new.is_empty());
        removed.push_str(&text[range.clone()]);
        last_end = range.end;
    }
    let result = apply(text, plan);
    assert_eq!(result.len(), text.len() - removed.len());
    for c in &plan.carets {
        assert!(
            result.is_char_boundary(c.head) && result.is_char_boundary(c.tail),
            "{text:?} {carets:?} {plan:?}"
        );
    }
    removed
}

fn dhs(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    delete_horizontal_space(t, c, n)
}
fn jos(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    just_one_space(t, c, n)
}
fn dbl(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    delete_blank_lines(t, c, n)
}

#[test]
fn delete_horizontal_space_both_sides() {
    assert_eq!(run("foo  ˇ  bar", None, dhs).0, "fooˇbar");
    assert_eq!(run("foo\t ˇ\t bar", None, dhs).0, "fooˇbar");
    assert_eq!(run("fooˇ  bar", None, dhs).0, "fooˇbar");
    assert_eq!(run("foo  ˇbar", None, dhs).0, "fooˇbar");
    assert_eq!(run("fooˇbar", None, dhs).0, "fooˇbar");
    assert_eq!(run("é  ˇ  ü", None, dhs).0, "éˇü");
    assert_eq!(run("", None, dhs).0, "");
}

#[test]
fn delete_horizontal_space_keeps_newlines() {
    assert_eq!(run("a \n ˇ b", None, dhs).0, "a \nˇb");
    assert_eq!(run("a \nˇ\n b", None, dhs).0, "a \nˇ\n b");
}

#[test]
fn delete_horizontal_space_any_count_is_before_point_only() {
    assert_eq!(run("foo  ˇ  bar", Some(1), dhs).0, "fooˇ  bar");
    assert_eq!(run("foo  ˇ  bar", Some(0), dhs).0, "fooˇ  bar");
    assert_eq!(run("foo  ˇ  bar", Some(-4), dhs).0, "fooˇ  bar");
    assert_eq!(run("foo  ˇ  bar", Some(9), dhs).0, "fooˇ  bar");
}

#[test]
fn delete_horizontal_space_clears_region_and_acts_at_point() {
    let (out, plan) = run("x «ab»  y", None, dhs);
    assert_eq!(out, "x abˇy");
    assert!(!plan.carets[0].has_region());
    let (out, _) = run("x  «ab» y", None, dhs);
    assert_eq!(out, "x  abˇy");
}

#[test]
fn delete_horizontal_space_is_not_a_kill() {
    let (_, plan) = run("a  ˇ  b", None, dhs);
    assert!(plan.killed.is_empty());
    assert_eq!(plan.edits.len(), 1);
}

#[test]
fn delete_horizontal_space_two_carets_in_one_run_merge() {
    let (out, plan) = run("a ˇ ˇ b", None, dhs);
    assert_eq!(out, "aˇb");
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(plan.carets.len(), 1);
    assert_eq!(run("a ˇb c ˇ d", None, dhs).0, "aˇb cˇd");
}

#[test]
fn just_one_space_default() {
    assert_eq!(run("foo   ˇ  bar", None, jos).0, "foo ˇbar");
    assert_eq!(run("fooˇbar", None, jos).0, "foo ˇbar");
    assert_eq!(run("foo\t ˇ\tbar", None, jos).0, "foo ˇbar");
    assert_eq!(run("fooˇ", None, jos).0, "foo ˇ");
    assert_eq!(run("ˇ", None, jos).0, " ˇ");
    assert_eq!(run("é  ˇ  ü", None, jos).0, "é ˇü");
    assert_eq!(run("foo  ˇ", None, jos).0, "foo ˇ");
}

#[test]
fn just_one_space_already_one_space_is_a_no_op() {
    let (out, plan) = run("foo ˇbar", None, jos);
    assert_eq!(out, "foo ˇbar");
    assert!(plan.edits.is_empty());
    let (out, plan) = run("foo ˇbar", Some(1), jos);
    assert_eq!(out, "foo ˇbar");
    assert!(plan.edits.is_empty());
}

#[test]
fn just_one_space_n_spaces() {
    assert_eq!(run("a ˇb", Some(3), jos).0, "a   ˇb");
    assert_eq!(run("aˇb", Some(2), jos).0, "a  ˇb");
    assert_eq!(run("a    ˇb", Some(2), jos).0, "a  ˇb");
    assert_eq!(run("a  ˇ b", Some(0), jos).0, "aˇb");
    assert_eq!(run("aˇb", Some(0), jos).0, "aˇb");
}

#[test]
fn just_one_space_positive_keeps_newlines() {
    let (out, plan) = run("a\nˇ b", Some(1), jos);
    assert_eq!(out, "a\n ˇb");
    assert!(plan.edits.is_empty());
    assert_eq!(run("a\n  ˇ  b", None, jos).0, "a\n ˇb");
}

#[test]
fn just_one_space_negative_removes_newlines() {
    assert_eq!(run("a \n\n ˇ\t b", Some(-1), jos).0, "a ˇb");
    assert_eq!(run("a \n\n ˇ\t b", Some(-2), jos).0, "a  ˇb");
    assert_eq!(run("a\n\nˇ\nb", Some(-1), jos).0, "a ˇb");
    assert_eq!(run("aˇb", Some(-1), jos).0, "a ˇb");
}

#[test]
fn just_one_space_clears_region_and_acts_at_point() {
    let (out, plan) = run("x «ab»  y", None, jos);
    assert_eq!(out, "x ab ˇy");
    assert!(!plan.carets[0].has_region());
}

#[test]
fn just_one_space_two_carets() {
    let (out, plan) = run("a ˇ ˇ b", None, jos);
    assert_eq!(out, "a ˇb");
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(run("aˇb cˇd", None, jos).0, "a ˇb c ˇd");
}

#[test]
fn delete_blank_lines_one_of_several_keeps_one() {
    assert_eq!(run("a\n\nˇ\n\n\nb", None, dbl).0, "a\nˇ\nb");
    assert_eq!(run("a\nˇ\n\nb", None, dbl).0, "a\nˇ\nb");
    assert_eq!(run("a\n\n\nˇ\nb", None, dbl).0, "a\nˇ\nb");
    assert_eq!(run("a\n  \nˇ\t\n \nb", None, dbl).0, "a\nˇ\t\nb");
    assert_eq!(run("\n\nˇ\nb", None, dbl).0, "ˇ\nb");
}

#[test]
fn delete_blank_lines_lone_blank_line_is_deleted() {
    assert_eq!(run("a\nˇ\nb", None, dbl).0, "a\nˇb");
    assert_eq!(run("a\n  ˇ \nb", None, dbl).0, "a\nˇb");
    assert_eq!(run("a\nˇ  ", None, dbl).0, "a\nˇ");
    assert_eq!(run("ˇ\nb", None, dbl).0, "ˇb");
    assert_eq!(run("ˇ", None, dbl).0, "ˇ");
}

#[test]
fn delete_blank_lines_on_nonblank_line_deletes_following() {
    assert_eq!(run("aˇ\n\n\n\nb", None, dbl).0, "aˇ\nb");
    assert_eq!(run("ˇa\n \n\t\nb", None, dbl).0, "ˇa\nb");
    let (out, plan) = run("aˇ\nb", None, dbl);
    assert_eq!(out, "aˇ\nb");
    assert!(plan.edits.is_empty());
    let (out, plan) = run("aˇ", None, dbl);
    assert_eq!(out, "aˇ");
    assert!(plan.edits.is_empty());
    // Blank lines before the line stay.
    assert_eq!(run("\n\naˇ\n\nb", None, dbl).0, "\n\naˇ\nb");
}

#[test]
fn delete_blank_lines_run_at_the_end_of_the_text() {
    assert_eq!(run("a\nˇ\n\n\n", None, dbl).0, "a\nˇ");
    assert_eq!(run("aˇ\n\n\n", None, dbl).0, "aˇ\n");
}

#[test]
fn delete_blank_lines_ignores_count_and_clears_region() {
    assert_eq!(
        run("a\n\nˇ\n\nb", Some(7), dbl).0,
        run("a\n\nˇ\n\nb", None, dbl).0
    );
    assert_eq!(run("a\n\nˇ\n\nb", Some(0), dbl).0, "a\nˇ\nb");
    let (out, plan) = run("a\n«\n\n»\nb", None, dbl);
    assert_eq!(out, "a\nˇ\nb");
    assert!(!plan.carets[0].has_region());
}

#[test]
fn delete_blank_lines_two_carets_in_one_run_act_once() {
    let (out, plan) = run("a\nˇ\nˇ\nb", None, dbl);
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(apply("a\n\n\nb", &plan), "a\n\nb");
    assert_eq!(out.matches('ˇ').count(), 2);
    assert_eq!(run("aˇ\n\nbˇ\n\nc", None, dbl).0, "aˇ\nbˇ\nc");
}

#[test]
fn randomized_space_plans_are_consistent() {
    let alphabet: Vec<char> = "ab \t\n\nßé".chars().collect();
    let mut rng = Lcg(7);
    for _ in 0..3000 {
        let text = random_text(&mut rng, &alphabet);
        let carets = random_carets(&mut rng, &text);
        let count = [None, Some(1), Some(0), Some(-1), Some(3)][rng.next(5)];
        for plan in [dhs(&text, &carets, count), dbl(&text, &carets, count)] {
            check_plan(&text, &carets, &plan);
            assert!(plan.killed.is_empty());
        }
        let plan = dhs(&text, &carets, None);
        let result = apply(&text, &plan);
        let again = dhs(result.as_str(), &plan.carets, None);
        assert!(again.edits.is_empty(), "{text:?} {carets:?}");

        let n = count.unwrap_or(1);
        let plan = jos(&text, &carets, count);
        let mut last_end = 0;
        for (range, new) in &plan.edits {
            assert!(range.start >= last_end);
            assert!(new.chars().all(|c| c == ' ') && new.len() == n.unsigned_abs() as usize);
            last_end = range.end;
        }
        let result = apply(&text, &plan);
        for c in &plan.carets {
            assert!(result.is_char_boundary(c.head));
        }
        let again = jos(result.as_str(), &plan.carets, count);
        assert!(again.edits.is_empty(), "{text:?} {carets:?} {count:?}");
    }
}
