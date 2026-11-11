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

fn zt(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    zap_to_char(t, c, n, 'a')
}
fn zu(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    zap_up_to_char(t, c, n, 'a')
}
fn zbt(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    zap_back_to_char(t, c, n, 'a')
}
fn zbu(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    zap_back_up_to_char(t, c, n, 'a')
}

#[test]
fn zap_to_char_forward() {
    assert_eq!(run("ˇfoo bar baz", None, zt).0, "ˇr baz");
    assert_eq!(run("ˇfoo bar baz", Some(1), zt).0, "ˇr baz");
    assert_eq!(run("ˇfoo bar baz", Some(2), zt).0, "ˇz");
    assert_eq!(run("foo ˇbar baz", None, zt).0, "foo ˇr baz");
    assert_eq!(run("ˇaab", None, zt).0, "ˇab");
}

#[test]
fn zap_up_to_char_forward() {
    assert_eq!(run("ˇfoo bar baz", None, zu).0, "ˇar baz");
    assert_eq!(run("ˇfoo bar baz", Some(2), zu).0, "ˇaz");
    // The search starts one character past point, so a target right after point is skipped
    // and repeating the command advances.
    assert_eq!(run("ˇaab", None, zu).0, "ˇab");
    assert_eq!(run("ˇaaab", None, zu).0, "ˇaab");
    let (out, plan) = run("ˇab", None, zu);
    assert_eq!(out, "ab");
    assert!(plan.edits.is_empty() && plan.killed.is_empty());
    assert_eq!(run("ˇ", None, zu).1, Plan::default());
}

#[test]
fn zap_back_forms() {
    assert_eq!(run("foo bar bazˇ", None, zbt).0, "foo bar bˇ");
    assert_eq!(run("foo bar bazˇ", None, zbu).0, "foo bar baˇ");
    assert_eq!(run("foo bar bazˇ", Some(2), zbt).0, "foo bˇ");
    assert_eq!(run("foo bar bazˇ", Some(2), zbu).0, "foo baˇ");
    assert_eq!(run("aaˇ", None, zbt).0, "aˇ");
    let (out, plan) = run("aˇ", None, zbu);
    assert_eq!(out, "a");
    assert!(plan.edits.is_empty());
}

#[test]
fn negative_count_searches_backward() {
    assert_eq!(
        run("foo bar bazˇ", Some(-1), zt).0,
        run("foo bar bazˇ", None, zbt).0
    );
    assert_eq!(run("foo bar bazˇ", Some(-2), zu).0, "foo baˇ");
    assert_eq!(run("foo ˇbar baz", Some(-1), zbt).0, "foo ˇr baz");
    assert_eq!(run("foo ˇbar baz", Some(-1), zbu).0, "foo ˇar baz");
}

#[test]
fn zap_character_absent_deletes_nothing() {
    for f in [zt, zu, zbt, zbu] {
        for n in [None, Some(1), Some(-1), Some(5)] {
            let (out, plan) = run("foˇo bor", n, f);
            assert_eq!(out, "foo bor");
            assert!(plan.edits.is_empty() && plan.killed.is_empty());
        }
    }
}

#[test]
fn zap_with_too_few_occurrences_deletes_nothing() {
    let (_, plan) = run("ˇfoo bar baz", Some(3), zt);
    assert_eq!(plan, Plan::default());
    let (_, plan) = run("foo bar bazˇ", Some(-3), zt);
    assert_eq!(plan, Plan::default());
    let (_, plan) = run("foo bar ˇbaz", Some(2), zbt);
    assert_eq!(plan, Plan::default());
}

#[test]
fn zero_count_does_nothing() {
    for f in [zt, zu, zbt, zbu] {
        assert_eq!(run("foˇo bar", Some(0), f).1, Plan::default());
    }
}

#[test]
fn zap_kills_into_the_ring_with_direction() {
    let (_, plan) = run("ˇfoo bar", None, zt);
    assert_eq!(
        plan.killed,
        vec![KilledPiece {
            text: "foo ba".into(),
            backward: false,
            source_start: None,
            whole_line: false
        }]
    );
    let (_, plan) = run("foo barˇ", None, zbt);
    assert_eq!(
        plan.killed,
        vec![KilledPiece {
            text: "ar".into(),
            backward: true,
            source_start: None,
            whole_line: false
        }]
    );
}

#[test]
fn zap_multibyte() {
    let (out, plan) = run("ˇhéllo wörld", None, |t, c, n| zap_to_char(t, c, n, 'ö'));
    assert_eq!(out, "ˇrld");
    assert_eq!(plan.killed[0].text, "héllo wö");
    let (out, _) = run("ˇhéllo wörld", None, |t, c, n| {
        zap_up_to_char(t, c, n, 'é')
    });
    assert_eq!(out, "ˇéllo wörld");
    let (out, plan) = run("straße üßx ˇ", None, |t, c, n| {
        zap_back_to_char(t, c, n, 'ß')
    });
    assert_eq!(out, "straße üˇ");
    assert_eq!(plan.killed[0].text, "ßx ");
    let (out, _) = run("a😀b😀 ˇc", Some(2), |t, c, n| {
        zap_back_up_to_char(t, c, n, '😀')
    });
    assert_eq!(out, "a😀ˇc");
}

#[test]
fn zap_ignores_the_region_and_clears_it() {
    let (out, plan) = run("«foo» bar x", None, |t, c, n| zap_to_char(t, c, n, 'r'));
    assert_eq!(out, "fooˇ x");
    assert!(!plan.carets[0].has_region());
    assert_eq!(plan.killed[0].text, " bar");
}

#[test]
fn zap_with_several_carets() {
    let (out, plan) = run("ˇaˇb ab", None, |t, c, n| zap_to_char(t, c, n, 'b'));
    assert_eq!(out, "ˇ ab");
    assert_eq!(plan.killed.len(), 1);
    let (out, plan) = run("ˇfoo x\nˇbar", None, |t, c, n| zap_to_char(t, c, n, 'x'));
    assert_eq!(out, "ˇ\nˇbar");
    assert_eq!(plan.killed.len(), 1);
    let (out, plan) = run("ˇab\nˇab", None, |t, c, n| zap_to_char(t, c, n, 'b'));
    assert_eq!(out, "ˇ\nˇ");
    assert_eq!(plan.killed.len(), 2);
}

#[test]
fn randomized_zap_kills_match_removed_text() {
    let alphabet: Vec<char> = "ab ßé\n".chars().collect();
    let mut rng = Lcg(99);
    for _ in 0..3000 {
        let text = random_text(&mut rng, &alphabet);
        let carets = random_carets(&mut rng, &text);
        let count = [None, Some(1), Some(2), Some(0), Some(-1), Some(-3)][rng.next(6)];
        let ch = ['a', 'ß', 'é', 'z'][rng.next(4)];
        let plans = [
            zap_to_char(text.as_str(), &carets, count, ch),
            zap_up_to_char(text.as_str(), &carets, count, ch),
            zap_back_to_char(text.as_str(), &carets, count, ch),
            zap_back_up_to_char(text.as_str(), &carets, count, ch),
        ];
        for plan in plans {
            let removed = check_plan(&text, &carets, &plan);
            let joined: String = plan.killed.iter().map(|k| k.text.as_str()).collect();
            assert_eq!(joined, removed, "{text:?} {carets:?} {count:?} {ch:?}");
            if ch == 'z' {
                assert!(plan.edits.is_empty());
            }
        }
    }
}
