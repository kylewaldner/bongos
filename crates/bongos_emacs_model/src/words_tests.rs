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

fn kw(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    kill_word(t, c, n)
}
fn bkw(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    backward_kill_word(t, c, n)
}

#[test]
fn forward_and_backward_targets() {
    let t = "foo bar_baz  qux";
    assert_eq!(forward_word_target(t, 0, None), 3);
    assert_eq!(forward_word_target(t, 0, Some(3)), 16);
    assert_eq!(forward_word_target(t, 0, Some(0)), 0);
    assert_eq!(forward_word_target(t, 16, Some(-2)), 4);
    assert_eq!(forward_word_target(t, 0, Some(100)), 16);
    assert_eq!(backward_word_target(t, 16, None), 13);
    assert_eq!(backward_word_target(t, 16, Some(3)), 0);
    assert_eq!(backward_word_target(t, 16, Some(0)), 16);
    assert_eq!(backward_word_target(t, 0, Some(-2)), 11);
    assert_eq!(backward_word_target(t, 5, Some(100)), 0);
}

#[test]
fn move_carets_select_and_merge() {
    let (t, c) = parse("fooˇ bar");
    assert_eq!(
        forward_word_carets(t.as_str(), &c, None, false),
        vec![Caret::point(7)]
    );
    assert_eq!(
        forward_word_carets(t.as_str(), &c, None, true),
        vec![Caret::new(7, 3)]
    );
    assert_eq!(
        backward_word_carets(t.as_str(), &c, Some(2), false),
        vec![Caret::point(0)]
    );
    let (t, c) = parse("aˇa bˇb");
    assert_eq!(
        forward_word_carets(t.as_str(), &c, None, false),
        vec![Caret::point(2), Caret::point(5)]
    );
    let (t, c) = parse("ˇa aˇ");
    assert_eq!(
        backward_word_carets(t.as_str(), &c, Some(5), false),
        vec![Caret::point(0)]
    );
}

#[test]
fn kill_word_counts() {
    assert_eq!(run("ˇfoo bar baz qux", None, kw).0, "ˇ bar baz qux");
    assert_eq!(run("ˇfoo bar baz qux", Some(3), kw).0, "ˇ qux");
    assert_eq!(run("ˇfoo bar", Some(0), kw).0, "ˇfoo bar");
    assert_eq!(run("foo bar baz ˇqux", Some(-2), kw).0, "foo ˇqux");
    assert_eq!(run("ˇfoo", Some(50), kw).0, "ˇ");
    assert_eq!(run("fooˇ", None, kw).0, "fooˇ");
    assert_eq!(run("", None, kw).0, "");
}

#[test]
fn backward_kill_word_counts() {
    assert_eq!(run("foo bar baz qux ˇ", None, bkw).0, "foo bar baz ˇ");
    assert_eq!(run("foo bar baz quxˇ", Some(3), bkw).0, "foo ˇ");
    assert_eq!(run("foo barˇ", Some(0), bkw).0, "foo barˇ");
    assert_eq!(run("ˇfoo bar baz", Some(-2), bkw).0, "ˇ baz");
    assert_eq!(run("foo barˇ", Some(50), bkw).0, "ˇ");
}

#[test]
fn killed_text_and_direction() {
    let (_, plan) = run("foo ˇbar baz", Some(2), kw);
    assert_eq!(
        plan.killed,
        vec![KilledPiece {
            text: "bar baz".into(),
            backward: false,
            source_start: None,
            whole_line: false
        }]
    );
    let (_, plan) = run("foo bar bazˇ", Some(2), bkw);
    assert_eq!(
        plan.killed,
        vec![KilledPiece {
            text: "bar baz".into(),
            backward: true,
            source_start: None,
            whole_line: false
        }]
    );
}

#[test]
fn region_is_killed_whole() {
    assert_eq!(run("a «b cd» e fgh", Some(3), kw).0, "a ˇ e fgh");
    assert_eq!(run("a «b cd» e", None, bkw).0, "a ˇ e");
    let (_, plan) = run("a «b cd» e", None, kw);
    assert_eq!(plan.killed[0].text, "b cd");
}

#[test]
fn two_carets_in_one_word_kill_once() {
    let (out, plan) = run("fˇoˇo bar", None, kw);
    assert_eq!(out, "fˇ bar");
    assert_eq!(plan.edits.len(), 1);
    assert_eq!(plan.killed.len(), 1);
    assert_eq!(plan.killed[0].text, "oo");
    let (out, plan) = run("fˇoo bˇar", None, kw);
    assert_eq!(out, "fˇ bˇ");
    assert_eq!(plan.killed.len(), 2);
}

#[test]
fn non_ascii_words() {
    assert_eq!(run("ˇßtraße x", None, kw).0, "ˇ x");
    assert_eq!(run("héllo wörldˇ", None, bkw).0, "héllo ˇ");
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

#[test]
fn randomized_kills_match_removed_text() {
    let alphabet: Vec<char> = "ab_ ,ßé\n.9".chars().collect();
    let mut rng = Lcg(42);
    for _ in 0..2000 {
        let len = rng.next(20);
        let text: String = (0..len)
            .map(|_| alphabet[rng.next(alphabet.len())])
            .collect();
        let bounds: Vec<usize> = (0..=text.len())
            .filter(|&i| text.is_char_boundary(i))
            .collect();
        let carets: Vec<Caret> = (0..1 + rng.next(3))
            .map(|_| {
                let head = bounds[rng.next(bounds.len())];
                let tail = if rng.next(3) == 0 {
                    bounds[rng.next(bounds.len())]
                } else {
                    head
                };
                Caret::new(head, tail)
            })
            .collect();
        let count = [None, Some(1), Some(3), Some(0), Some(-2), Some(100)][rng.next(6)];
        for backward in [false, true] {
            let plan = if backward {
                backward_kill_word(text.as_str(), &carets, count)
            } else {
                kill_word(text.as_str(), &carets, count)
            };
            let mut removed = String::new();
            let mut last_end = 0;
            for (range, new) in &plan.edits {
                assert!(
                    range.start >= last_end,
                    "edits overlap or unsorted: {text:?} {plan:?}"
                );
                assert!(new.is_empty());
                removed.push_str(&text[range.clone()]);
                last_end = range.end;
            }
            let joined: String = plan.killed.iter().map(|k| k.text.as_str()).collect();
            assert_eq!(joined, removed, "{text:?} {carets:?} {count:?}");
            let result = apply(&text, &plan);
            assert_eq!(result.len(), text.len() - removed.len());
            for c in &plan.carets {
                assert!(result.is_char_boundary(c.head) && result.is_char_boundary(c.tail));
            }
        }
    }
}
