use crate::case;
use crate::session::{Counts, End, Found, Key, Reason, Session};
use crate::simulate::{simulate, trace};
use crate::template::{Template, parse};
use regex::Regex;
use std::ops::Range;

fn matcher(re: &Regex) -> impl FnMut(&str, usize) -> Option<Found> + '_ {
    move |text, from| {
        let caps = re.captures_at(text, from)?;
        let whole = caps.get(0)?;
        Some(Found {
            range: whole.range(),
            groups: caps
                .iter()
                .map(|group| group.map(|g| g.as_str().to_string()))
                .collect(),
            next_char_len: 0,
        })
    }
}

fn run(text: &str, pattern: &str, to: &str, keys: &[Key]) -> String {
    let re = Regex::new(pattern).unwrap();
    let session = Session::new(false, parse(to, true).unwrap());
    simulate(text, 0, matcher(&re), session, keys)
}

fn all(text: &str, pattern: &str, to: &str) -> String {
    run(text, pattern, to, &[Key::All])
}

fn ys(n: usize) -> Vec<Key> {
    vec![Key::Yes; n]
}

#[test]
fn simulate_replacement_containing_from_does_not_loop() {
    assert_eq!(all("foo foo", "foo", "foofoo"), "foofoo foofoo");
    assert_eq!(run("foo foo", "foo", "foofoo", &ys(5)), "foofoo foofoo");
    assert_eq!(run("foofoo", "foo", "foofoo", &ys(9)), "foofoofoofoo");
}

#[test]
fn simulate_longer_shorter_empty_and_newline_replacements() {
    let text = "a foo b foo\nfoo";
    for to in ["longer one", "x", "", "two\nlines", "\n"] {
        let expected = text.replace("foo", to);
        assert_eq!(all(text, "foo", to), expected, "{to:?}");
        assert_eq!(run(text, "foo", to, &ys(3)), expected, "{to:?}");
    }
}

#[test]
fn simulate_line_start_zero_width() {
    assert_eq!(all("a\nb\nc", "(?m)^", "> "), "> a\n> b\n> c");
    assert_eq!(run("a\nb\nc", "(?m)^", "> ", &ys(3)), "> a\n> b\n> c");
    // `^` without multiline only matches once.
    assert_eq!(all("a\nb", "^", "> "), "> a\nb");
}

#[test]
fn simulate_line_end_zero_width() {
    assert_eq!(all("a\nb", "(?m)$", ";"), "a;\nb;");
    assert_eq!(all("a\nb\n", "(?m)$", ";"), "a;\nb;\n;");
    assert_eq!(all("ab", "$", ";"), "ab;");
}

#[test]
fn simulate_word_boundary_zero_width() {
    assert_eq!(all("ab cd", "\\b", "|"), "|ab| |cd|");
    assert_eq!(
        run("ab cd", "\\b", "|", &[Key::Yes, Key::No, Key::Yes]),
        "|ab |cd"
    );
}

#[test]
fn simulate_empty_matches_never_split_multibyte_text() {
    assert_eq!(all("é日b", "", "-"), "-é-日-b-");
    assert_eq!(all("é日b", "x*", "-"), "-é-日-b-");
    assert_eq!(
        run("日日", "", "-", &[Key::No, Key::Yes, Key::Yes]),
        "日-日-"
    );
}

#[test]
fn simulate_empty_match_after_a_nonempty_one_is_passed_over() {
    assert_eq!(all("axxb", "x*", "-"), "-a-b-");
    assert_eq!(run("axxb", "x*", "-", &ys(9)), "-a-b-");
    assert_eq!(
        all("axxb", "x*", "-"),
        Regex::new("x*").unwrap().replace_all("axxb", "-")
    );
}

#[test]
fn simulate_does_not_wrap() {
    let re = Regex::new("foo").unwrap();
    let session = Session::new(false, parse("bar", false).unwrap());
    let t = trace("foo foo foo", 4, matcher(&re), session, &ys(9));
    assert_eq!(t.text, "foo bar bar");
    assert_eq!(t.asked, vec![4..7, 8..11]);
    assert_eq!(
        t.end,
        Some(End {
            reason: Reason::NoMore,
            counts: Counts {
                replaced: 2,
                skipped: 0
            }
        })
    );
}

#[test]
fn simulate_nothing_to_replace_leaves_the_text() {
    let re = Regex::new("zzz").unwrap();
    let session = Session::new(false, parse("bar", false).unwrap());
    let t = trace("foo", 0, matcher(&re), session, &[Key::All]);
    assert_eq!(t.text, "foo");
    assert_eq!(t.end.map(|end| end.reason), Some(Reason::NoMatch));
}

#[test]
fn simulate_exit_keys_stop_where_they_are() {
    assert_eq!(run("a a a", "a", "b", &[Key::Yes, Key::Exit]), "b a a");
    assert_eq!(run("a a a", "a", "b", &[Key::Yes, Key::Abort]), "b a a");
    assert_eq!(
        run("a a a", "a", "b", &[Key::No, Key::ThisAndExit]),
        "a b a"
    );
    // Running out of keys exits as `q`.
    assert_eq!(run("a a a", "a", "b", &[Key::Yes]), "b a a");
}

#[test]
fn simulate_groups_and_counter() {
    assert_eq!(
        all(
            "userService orderService",
            "(\\w+)Service",
            "\\1Repository\\#"
        ),
        "userRepository0 orderRepository1"
    );
    assert_eq!(
        run(
            "k1=v1\nk2=v2",
            "(\\w+)=(\\w+)",
            "\\2=\\1 (\\&)",
            &[Key::Yes, Key::All]
        ),
        "v1=k1 (k1=v1)\nv2=k2 (k2=v2)"
    );
}

#[test]
fn simulate_undo_restores_the_text_and_asks_again() {
    let re = Regex::new("foo").unwrap();
    let session = Session::new(false, parse("x\\#", true).unwrap());
    let keys = [Key::Yes, Key::Yes, Key::Undo, Key::Undo, Key::No, Key::Yes];
    let t = trace("foo foo foo", 0, matcher(&re), session, &keys);
    assert_eq!(t.text, "foo x0 foo");
    assert_eq!(t.asked, vec![0..3, 3..6, 6..9, 3..6, 0..3, 4..7, 7..10]);
}

#[test]
fn simulate_this_and_wait_then_previous() {
    let re = Regex::new("o").unwrap();
    let session = Session::new(false, parse("00", false).unwrap());
    let keys = [
        Key::Yes,
        Key::ThisAndWait,
        Key::Previous,
        Key::Previous,
        Key::Yes,
        Key::No,
    ];
    let t = trace("o o o", 0, matcher(&re), session, &keys);
    assert_eq!(t.text, "00 00 o");
    assert_eq!(t.shown, vec![3..5, 0..2]);
}

#[test]
fn simulate_preserves_case() {
    let re = Regex::new("(?i)listener").unwrap();
    let session = Session::new(true, parse("handler", false).unwrap());
    assert_eq!(
        simulate(
            "listener Listener LISTENER",
            0,
            matcher(&re),
            session,
            &[Key::All]
        ),
        "handler Handler HANDLER"
    );
}

/// splitmix64: small, seeded and deterministic.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }

    fn pick<'a>(&mut self, items: &[&'a str]) -> &'a str {
        items[self.below(items.len())]
    }

    fn string(&mut self, pieces: &[&str], max: usize) -> String {
        let len = self.below(max + 1);
        (0..len).map(|_| self.pick(pieces)).collect()
    }
}

const TEXT_PIECES: &[&str] = &["a", "b", "A", "o", " ", "\n", "é", "日", "ab", "foo"];

// Patterns whose matches do not look at the text before them, so replacing
// earlier matches cannot change later ones, plus zero-width ones whose next
// search always starts past an original character.
const ZERO_WIDTH: &[&str] = &[
    "", "^", "$", "(?m)^", "(?m)$", "\\b", "\\B", "a*", "b?", "(?:ab)*",
];

struct Case {
    text: String,
    start: usize,
    pattern: String,
    template: Template,
    preserve_case: bool,
    keys: Vec<Key>,
}

fn random_case(rng: &mut Rng) -> Case {
    let text = rng.string(TEXT_PIECES, 12);
    let boundaries: Vec<usize> = (0..=text.len())
        .filter(|&i| text.is_char_boundary(i))
        .collect();
    let start = if rng.below(3) == 0 {
        0
    } else {
        boundaries[rng.below(boundaries.len())]
    };
    let pattern = if rng.below(4) == 0 {
        rng.pick(ZERO_WIDTH).to_string()
    } else {
        let from = rng.string(&["a", "b", "o", "é", " ", "\n", "日", "A"], 3);
        let from = if from.is_empty() {
            "a".to_string()
        } else {
            from
        };
        let literal = regex::escape(&from);
        let fold = if rng.below(3) == 0 { "(?i)" } else { "" };
        let group = if rng.below(3) == 0 { "(" } else { "" };
        let close = if group.is_empty() { "" } else { ")" };
        format!("{fold}{group}{literal}{close}")
    };
    // TO: longer, shorter, empty, with a newline, containing FROM, and the
    // template escapes.
    let to = rng.string(
        &[
            "x", "Y", "\n", "foo", "a", "ab", "日", "\\&", "\\#", "\\1", "\\\\",
        ],
        4,
    );
    let template = parse(&to, true).unwrap();
    let keys = (0..rng.below(14))
        .map(|_| match rng.below(10) {
            0 => Key::All,
            1..=5 => Key::Yes,
            _ => Key::No,
        })
        .collect();
    Case {
        text,
        start,
        pattern,
        template,
        preserve_case: rng.below(2) == 0,
        keys,
    }
}

fn replacement(case: &Case, found: &Found, counter: usize) -> String {
    let groups: Vec<Option<&str>> = found.groups.iter().map(|g| g.as_deref()).collect();
    let expanded = case.template.expand(&groups, counter);
    case::transfer(case.preserve_case, found.matched(), &expanded)
}

// The matches a left-to-right scan of the original text visits from
// `start`, without the session: the next search starts at the end of the
// previous match, one character later after an empty match, and an empty
// match right at the end of a non-empty one is passed over.
fn reference_matches(text: &str, start: usize, re: &Regex) -> Vec<Found> {
    let mut find = matcher(re);
    let mut out: Vec<Found> = Vec::new();
    let mut from = start;
    let mut after_nonempty = false;
    while from <= text.len() {
        let Some(found) = find(text, from) else { break };
        let range = found.range.clone();
        let next_char = text[range.end..].chars().next().map_or(0, char::len_utf8);
        if range.is_empty() {
            if !(after_nonempty && range.start == from) {
                out.push(found);
            }
            if next_char == 0 {
                break;
            }
            from = range.end + next_char;
            after_nonempty = false;
        } else {
            out.push(found);
            from = range.end;
            after_nonempty = true;
        }
    }
    out
}

struct Expected {
    text: String,
    asked: Vec<Range<usize>>,
    batch: Vec<Range<usize>>,
    counts: Counts,
}

// Applies the decisions to the reference matches: each key decides the next
// match, `!` replaces it and all later ones, and running out of keys exits.
fn expected(case: &Case, matches: &[Found]) -> Expected {
    let mut text = case.text[..matches.first().map_or(0, |m| m.range.start)].to_string();
    let mut last = text.len();
    let mut asked = Vec::new();
    let mut batch = Vec::new();
    let mut counts = Counts::default();
    let mut keys = case.keys.iter();
    let mut batching = false;
    for found in matches {
        text.push_str(&case.text[last..found.range.start]);
        last = found.range.end;
        let at = text.len()..text.len() + found.range.len();
        if !batching {
            asked.push(at.clone());
            match keys.next() {
                Some(Key::Yes) => {}
                Some(Key::No) => {
                    counts.skipped += 1;
                    text.push_str(found.matched());
                    continue;
                }
                Some(Key::All) => batching = true,
                _ => {
                    last = found.range.start;
                    break;
                }
            }
        }
        if batching {
            batch.push(at);
        }
        text.push_str(&replacement(case, found, counts.replaced));
        counts.replaced += 1;
    }
    text.push_str(&case.text[last..]);
    Expected {
        text,
        asked,
        batch,
        counts,
    }
}

#[test]
fn simulate_property_matches_the_reference_scan() {
    let mut rng = Rng(0x5eed_0011_2233_4455);
    for index in 0..1000 {
        let case = random_case(&mut rng);
        let re = Regex::new(&case.pattern).unwrap();
        let matches = reference_matches(&case.text, case.start, &re);
        let want = expected(&case, &matches);
        let session = Session::new(case.preserve_case, case.template.clone());
        let got = trace(&case.text, case.start, matcher(&re), session, &case.keys);
        let label = format!(
            "case {index}: text {:?} start {} pattern {:?} keys {:?}",
            case.text, case.start, case.pattern, case.keys
        );
        // Every match visited once, in order, and nothing else changed.
        assert_eq!(got.asked, want.asked, "{label}");
        assert_eq!(got.batch, want.batch, "{label}");
        assert_eq!(got.text, want.text, "{label}");
        assert_eq!(got.end.map(|end| end.counts), Some(want.counts), "{label}");
        // Text before the start offset never changes.
        assert!(got.text.starts_with(&case.text[..case.start]), "{label}");
    }
}

#[test]
fn simulate_property_all_equals_replace_all() {
    let mut rng = Rng(0xa11_0f_7e_5eed);
    for index in 0..1000 {
        let mut case = random_case(&mut rng);
        case.keys = vec![Key::All];
        let re = Regex::new(&case.pattern).unwrap();
        let session = Session::new(case.preserve_case, case.template.clone());
        let got = simulate(&case.text, case.start, matcher(&re), session, &case.keys);
        let want = expected(&case, &reference_matches(&case.text, case.start, &re)).text;
        let label = format!(
            "case {index}: text {:?} start {} pattern {:?}",
            case.text, case.start, case.pattern
        );
        assert_eq!(got, want, "{label}");
        if case.start == 0 {
            // From the start, the `regex` crate's own iteration is the oracle.
            let mut counter = 0;
            let independent = re.replace_all(&case.text, |caps: &regex::Captures| {
                let found = Found {
                    range: caps.get(0).map_or(0..0, |m| m.range()),
                    groups: caps
                        .iter()
                        .map(|group| group.map(|g| g.as_str().to_string()))
                        .collect(),
                    next_char_len: 0,
                };
                counter += 1;
                replacement(&case, &found, counter - 1)
            });
            assert_eq!(got, independent, "{label}");
        }
    }
}
