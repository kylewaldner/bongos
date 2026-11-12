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

fn kl(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    kill_line(t, c, n)
}
fn kr(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    kill_region_or_line(t, c, n)
}
fn cr(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    copy_region_or_line(t, c, n)
}

fn killed(plan: &Plan) -> Vec<&str> {
    plan.killed.iter().map(|k| k.text.as_str()).collect()
}

#[test]
fn kill_line_without_count() {
    let (out, plan) = run("foo ˇbar baz\nqux", None, kl);
    assert_eq!(out, "foo ˇ\nqux");
    assert_eq!(killed(&plan), ["bar baz"]);
    assert!(!plan.killed[0].backward);
    assert_eq!(run("foo ˇbar\nbaz", None, kl).0, "foo ˇ\nbaz");
    assert_eq!(run("foo bar ˇ", None, kl).0, "foo bar ˇ");
    assert_eq!(run("fooˇ", None, kl).0, "fooˇ");
    assert_eq!(run("", None, kl).0, "");
}

#[test]
fn kill_line_at_line_end_kills_the_newline() {
    let (out, plan) = run("foo barˇ\nbaz", None, kl);
    assert_eq!(out, "foo barˇbaz");
    assert_eq!(killed(&plan), ["\n"]);
}

#[test]
fn kill_line_whitespace_rule() {
    let (out, plan) = run("foo bar ˇ \t\nbaz", None, kl);
    assert_eq!(out, "foo bar ˇbaz");
    assert_eq!(killed(&plan), [" \t\n"]);
    assert_eq!(run("foo ˇ  ", None, kl).0, "foo ˇ");
    // Point at the start of the line: the whole line goes, newline included.
    let (out, plan) = run("ˇfoo bar\nbaz", None, kl);
    assert_eq!(out, "ˇbaz");
    assert_eq!(killed(&plan), ["foo bar\n"]);
    assert_eq!(run("a\nˇ\nb", None, kl).0, "a\nˇb");
    assert_eq!(run("a\nˇlast", None, kl).0, "a\nˇ");
    // Nonblank text after point keeps the newline.
    assert_eq!(run("a ˇb \nc", None, kl).0, "a ˇ\nc");
}

#[test]
fn kill_line_positive_count_kills_through_newlines() {
    let (out, plan) = run("foo ˇbar\nbaz\nqux", Some(1), kl);
    assert_eq!(out, "foo ˇbaz\nqux");
    assert_eq!(killed(&plan), ["bar\n"]);
    let (out, plan) = run("foo ˇbar\nbaz\nqux", Some(2), kl);
    assert_eq!(out, "foo ˇqux");
    assert_eq!(killed(&plan), ["bar\nbaz\n"]);
    let (out, plan) = run("foo ˇbar\nbaz\nqux", Some(3), kl);
    assert_eq!(out, "foo ˇ");
    assert_eq!(killed(&plan), ["bar\nbaz\nqux"]);
    assert_eq!(run("foo ˇbar\nbaz", Some(50), kl).0, "foo ˇ");
    assert_eq!(run("ˇfoo\nbar", Some(1), kl).0, "ˇbar");
    assert_eq!(run("fooˇ", Some(1), kl).0, "fooˇ");
}

#[test]
fn kill_line_zero_kills_to_the_line_start() {
    let (out, plan) = run("a\nfoo ˇbar", Some(0), kl);
    assert_eq!(out, "a\nˇbar");
    assert_eq!(killed(&plan), ["foo "]);
    assert!(plan.killed[0].backward);
    assert_eq!(run("ˇfoo", Some(0), kl).0, "ˇfoo");
    assert_eq!(run("a\nˇfoo", Some(0), kl).0, "a\nˇfoo");
}

#[test]
fn kill_line_negative_kills_back_through_line_starts() {
    let (out, plan) = run("a\nbc ˇd", Some(-1), kl);
    assert_eq!(out, "ˇd");
    assert_eq!(killed(&plan), ["a\nbc "]);
    assert!(plan.killed[0].backward);
    assert_eq!(run("a\nb\nc ˇd", Some(-1), kl).0, "a\nˇd");
    assert_eq!(run("a\nb\nc ˇd", Some(-2), kl).0, "ˇd");
    assert_eq!(run("a\nb\nc ˇd", Some(-50), kl).0, "ˇd");
    assert_eq!(run("foo ˇbar", Some(-1), kl).0, "ˇbar");
}

#[test]
fn kill_line_region_is_killed_whole() {
    let (out, plan) = run("a «b\nc» d", Some(3), kl);
    assert_eq!(out, "a ˇ d");
    assert_eq!(killed(&plan), ["b\nc"]);
    assert_eq!(run("a «b\nc» d", None, kl).0, "a ˇ d");
    assert_eq!(run("a «b\nc» d", Some(0), kl).0, "a ˇ d");
}

#[test]
fn kill_line_several_carets() {
    let (out, plan) = run("aˇ1\nbˇ2", None, kl);
    assert_eq!(out, "aˇ\nbˇ");
    assert_eq!(killed(&plan), ["1", "2"]);
    let (out, plan) = run("aˇbˇc", None, kl);
    assert_eq!(out, "aˇ");
    assert_eq!(killed(&plan), ["bc"]);
}

#[test]
fn kill_line_multibyte() {
    let (out, plan) = run("héˇllo ß\nx", None, kl);
    assert_eq!(out, "héˇ\nx");
    assert_eq!(killed(&plan), ["llo ß"]);
    assert_eq!(run("ünï ˇx", Some(0), kl).0, "ˇx");
    assert_eq!(run("é\nüˇ", Some(-1), kl).0, "ˇ");
}

#[test]
fn kill_region_or_line_kills_the_whole_line() {
    let (out, plan) = run("foo\nbˇar\nbaz", None, kr);
    assert_eq!(out, "foo\nˇbaz");
    assert_eq!(killed(&plan), ["bar\n"]);
    assert_eq!(run("ˇfoo\nbar", None, kr).0, "ˇbar");
    assert_eq!(run("foo\nbar\nbaˇz\n", None, kr).0, "foo\nbar\nˇ");
    assert_eq!(run("ˇ\nfoo", None, kr).0, "ˇfoo");
}

#[test]
fn kill_region_or_line_on_the_last_line() {
    let (out, plan) = run("foo\nbˇar", None, kr);
    assert_eq!(out, "fooˇ");
    assert_eq!(killed(&plan), ["bar\n"]);
    assert_eq!(plan.edits, vec![(3..7, String::new())]);
    let (out, plan) = run("ˇfoo", None, kr);
    assert_eq!(out, "ˇ");
    assert_eq!(killed(&plan), ["foo\n"]);
    let (out, plan) = run("a\nb\nˇc", None, kr);
    assert_eq!(out, "a\nbˇ");
    assert_eq!(killed(&plan), ["c\n"]);
    // Nothing on an empty last line.
    let (out, plan) = run("a\nˇ", None, kr);
    assert_eq!(out, "a\nˇ");
    assert!(plan.edits.is_empty() && plan.killed.is_empty());
    assert_eq!(run("", None, kr).0, "");
}

#[test]
fn kill_region_or_line_counts() {
    assert_eq!(run("ˇa\nb\nc", Some(2), kr).0, "ˇc");
    let (out, plan) = run("a\nˇb\nc", Some(5), kr);
    assert_eq!(out, "aˇ");
    assert_eq!(killed(&plan), ["b\nc\n"]);
    assert_eq!(run("ˇa\nb\nc\n", Some(5), kr).0, "ˇ");
    let (out, plan) = run("ˇa\nb", Some(0), kr);
    assert_eq!(out, "ˇa\nb");
    assert!(plan.edits.is_empty() && plan.killed.is_empty());
    let (out, plan) = run("a\nb\nˇc", Some(-1), kr);
    assert_eq!(out, "a\nˇc");
    assert_eq!(killed(&plan), ["b\n"]);
    assert!(plan.killed[0].backward);
    assert_eq!(run("a\nb\nˇc", Some(-9), kr).0, "ˇc");
    assert_eq!(run("ˇa\nb", Some(-1), kr).0, "ˇa\nb");
}

#[test]
fn kill_region_or_line_kills_the_region_and_ignores_the_count() {
    let (out, plan) = run("a «b\nc» d\ne", Some(3), kr);
    assert_eq!(out, "a ˇ d\ne");
    assert_eq!(killed(&plan), ["b\nc"]);
    assert_eq!(run("a «b» c", Some(0), kr).0, "a ˇ c");
    assert_eq!(run("a «b» c", None, kr).0, "a ˇ c");
}

#[test]
fn kill_region_or_line_several_carets() {
    let (out, plan) = run("aˇ\nbˇ\nc", None, kr);
    assert_eq!(out, "ˇc");
    assert_eq!(killed(&plan), ["a\n", "b\n"]);
    assert_eq!(plan.carets.len(), 1);
    let (out, plan) = run("ˇa\nbˇb\ncˇc", None, kr);
    assert_eq!(out, "ˇ");
    assert_eq!(killed(&plan), ["a\n", "bb\ncc\n"]);
    let (out, plan) = run("ˇaˇ\nb", None, kr);
    assert_eq!(out, "ˇb");
    assert_eq!(killed(&plan), ["a\n"]);
    let (out, plan) = run("a\nˇb\nˇc", None, kr);
    assert_eq!(out, "a\nˇ");
    assert_eq!(killed(&plan), ["b\nc\n"]);
}

#[test]
fn kill_region_or_line_multibyte() {
    let (out, plan) = run("é\nüˇß", None, kr);
    assert_eq!(out, "éˇ");
    assert_eq!(killed(&plan), ["üß\n"]);
    assert_eq!(run("éˇ\nü", None, kr).0, "ˇü");
}

#[test]
fn copy_region_or_line_changes_nothing() {
    let (out, plan) = run("a «b» c", None, cr);
    assert_eq!(out, "a «b» c");
    assert!(plan.edits.is_empty());
    assert_eq!(killed(&plan), ["b"]);
    let (out, plan) = run("foo\nbˇar\nbaz", None, cr);
    assert_eq!(out, "foo\nbˇar\nbaz");
    assert_eq!(killed(&plan), ["bar\n"]);
    let (out, plan) = run("foo\nbˇar", None, cr);
    assert_eq!(out, "foo\nbˇar");
    assert_eq!(killed(&plan), ["bar\n"]);
    assert!(plan.edits.is_empty());
    let (_, plan) = run("ˇa\nb\nc", Some(2), cr);
    assert_eq!(killed(&plan), ["a\nb\n"]);
    let (_, plan) = run("a «b» c", Some(4), cr);
    assert_eq!(killed(&plan), ["b"]);
    let (_, plan) = run("ˇa", Some(0), cr);
    assert!(plan.killed.is_empty());
}

#[test]
fn copy_matches_kill() {
    for (marked, count) in [
        ("foo\nbˇar\nbaz", None),
        ("foo\nbˇar", None),
        ("ˇa\nb\nc", Some(2)),
        ("a\nˇb\nc", Some(9)),
        ("a «b\nc» d", Some(2)),
        ("a\nb\nˇc", Some(-1)),
    ] {
        assert_eq!(
            run(marked, count, cr).1.killed,
            run(marked, count, kr).1.killed
        );
    }
}

#[test]
fn randomized_kills_match_removed_text() {
    let alphabet: Vec<char> = "ab ,ßé\n\n\t.".chars().collect();
    let mut rng = Lcg(2024);
    for _ in 0..4000 {
        let text = random_text(&mut rng, &alphabet);
        let carets = random_carets(&mut rng, &text);
        let count = [
            None,
            Some(1),
            Some(3),
            Some(0),
            Some(-1),
            Some(-2),
            Some(100),
        ][rng.next(7)];
        let plan = kill_line(text.as_str(), &carets, count);
        let removed = check_plan(&text, &carets, &plan);
        let joined: String = plan.killed.iter().map(|k| k.text.as_str()).collect();
        assert_eq!(joined, removed, "{text:?} {carets:?} {count:?}");

        let plan = kill_region_or_line(text.as_str(), &carets, count);
        check_plan(&text, &carets, &plan);
        assert_eq!(
            plan.edits.len(),
            plan.killed.len(),
            "{text:?} {carets:?} {count:?}"
        );
        for ((range, _), piece) in plan.edits.iter().zip(&plan.killed) {
            let removed = &text[range.clone()];
            // A last line without a newline keeps its piece ending in one.
            let rotated = removed.strip_prefix('\n').map(|rest| format!("{rest}\n"));
            assert!(
                piece.text == removed
                    || piece.text == format!("{removed}\n")
                    || rotated.as_deref() == Some(piece.text.as_str()),
                "{text:?} {carets:?} {count:?} {piece:?} {removed:?}"
            );
        }
        let copy = copy_region_or_line(text.as_str(), &carets, count);
        assert!(copy.edits.is_empty());
        assert_eq!(copy.killed, plan.killed);
    }
}

#[test]
fn pieces_carry_their_source_start_and_whole_line_flag() {
    let text = "aa\nbb\ncc\ndd";
    let c = vec![Caret::point(0), Caret::point(1), Caret::new(9, 7)];
    let plan = cr(text, &c, None);
    let pieces: Vec<_> = plan
        .killed
        .iter()
        .map(|k| (k.text.as_str(), k.source_start, k.whole_line))
        .collect();
    assert_eq!(
        pieces,
        vec![("aa\n", Some(0), true), ("c\n", Some(7), false)]
    );

    // A region merged with a whole-line item is not a whole line.
    let c = vec![Caret::new(5, 1), Caret::point(4)];
    let plan = cr(text, &c, None);
    assert_eq!(plan.killed.len(), 1);
    assert_eq!(plan.killed[0].source_start, Some(1));
    assert!(!plan.killed[0].whole_line);

    // A negative count starts at the line above.
    let plan = cr(text, &[Caret::point(7)], Some(-1));
    assert_eq!(plan.killed[0].source_start, Some(3));
    assert!(plan.killed[0].whole_line);
}

#[test]
fn an_active_empty_region_kills_and_copies_nothing() {
    let text = "foo\nbar\nbaz";
    let carets = [Caret::point(5).with_active(true)];
    assert!(carets[0].is_active() && !carets[0].has_region());
    for plan in [
        kill_region_or_line(text, &carets, None),
        kill_region_or_line(text, &carets, Some(3)),
        copy_region_or_line(text, &carets, None),
    ] {
        assert!(plan.edits.is_empty() && plan.killed.is_empty());
    }
    // An inactive caret still takes its line.
    let plan = kill_region_or_line(text, &[Caret::point(5)], None);
    assert_eq!(killed(&plan), ["bar\n"]);
    // A real region next to an active empty caret kills only the region.
    let carets = [Caret::new(3, 0), Caret::point(9).with_active(true)];
    let plan = kill_region_or_line(text, &carets, None);
    assert_eq!(killed(&plan), ["foo"]);
}

fn kw(t: &str, c: &[Caret], n: Option<i32>) -> Plan {
    kill_whole_line(t, c, n)
}

#[test]
fn kill_whole_line_takes_the_line_with_its_newline() {
    let (out, plan) = run("foo\nbˇar\nbaz", None, kw);
    assert_eq!(out, "foo\nˇbaz");
    assert_eq!(killed(&plan), ["bar\n"]);
    assert!(plan.killed[0].whole_line && !plan.killed[0].backward);
}

#[test]
fn kill_whole_line_ignores_the_region() {
    let (out, plan) = run("a «b\nc» d\ne", None, kw);
    assert_eq!(killed(&plan), ["c d\n"]);
    assert_eq!(out, "a b\nˇe");
}

#[test]
fn kill_whole_line_with_a_positive_count() {
    let (out, plan) = run("a\nˇb\nc\nd\ne", Some(3), kw);
    assert_eq!(out, "a\nˇe");
    assert_eq!(killed(&plan), ["b\nc\nd\n"]);
}

#[test]
fn kill_whole_line_with_zero_keeps_the_newline() {
    let (out, plan) = run("a\nbˇar\nc", Some(0), kw);
    assert_eq!(out, "a\nˇ\nc");
    assert_eq!(killed(&plan), ["bar"]);
    assert!(plan.killed[0].whole_line);
}

#[test]
fn kill_whole_line_with_a_negative_count_goes_up() {
    let (out, plan) = run("a\nb\nˇc\nd", Some(-2), kw);
    assert_eq!(out, "a\nˇd");
    assert_eq!(killed(&plan), ["b\nc\n"]);
    assert!(plan.killed[0].backward && plan.killed[0].whole_line);
}

#[test]
fn kill_whole_line_on_the_last_line_without_a_newline() {
    let (out, plan) = run("foo\nbˇar", None, kw);
    assert_eq!(out, "fooˇ");
    assert_eq!(killed(&plan), ["bar\n"]);
    let (out, plan) = run("ˇfoo", None, kw);
    assert_eq!(out, "ˇ");
    assert_eq!(killed(&plan), ["foo\n"]);
}

#[test]
fn kill_whole_line_with_two_carets() {
    let (out, plan) = run("aˇa\nbb\ncˇc\ndd", None, kw);
    assert_eq!(out, "ˇbb\nˇdd");
    assert_eq!(killed(&plan), ["aa\n", "cc\n"]);
}
