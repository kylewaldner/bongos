//! Ranks file paths against search terms. Every term must match (case-insensitively), and the
//! score favors, in order: a contiguous match in the file name, a contiguous match in a directory,
//! then scattered characters in the file name or directory. A term containing `/` is matched
//! against the whole path, and contiguous segments (`stuff/File`) outrank scattered characters.

const CONTIGUOUS_PATH: i64 = 1000;
const CONTIGUOUS_NAME: i64 = 600;
const CONTIGUOUS_DIR: i64 = 300;
const SCATTERED_NAME: i64 = 100;
const SCATTERED_DIR: i64 = 50;
const PREFIX_BONUS: i64 = 40;

fn is_subsequence(needle: &str, haystack: &str) -> bool {
    let mut hay = haystack.chars();
    needle.chars().all(|n| hay.any(|h| h == n))
}

fn score_term(path: &str, name_start: usize, term: &str) -> Option<i64> {
    if term.is_empty() {
        return Some(0);
    }
    let lower_path = path.to_lowercase();
    let lower_term = term.to_lowercase();
    // Lowercasing can change byte lengths, so split the lowered path at its last `/` itself.
    let name_start = if lower_path.len() == path.len() {
        name_start
    } else {
        lower_path.rfind('/').map_or(0, |i| i + 1)
    };
    let (dir, name) = lower_path.split_at(name_start);
    if lower_term.contains('/') {
        if let Some(at) = lower_path.find(&lower_term) {
            let on_boundary = at == 0 || lower_path.as_bytes()[at - 1] == b'/';
            return Some(CONTIGUOUS_PATH + if on_boundary { PREFIX_BONUS } else { 0 });
        }
        return is_subsequence(&lower_term, &lower_path).then_some(SCATTERED_DIR);
    }
    if let Some(at) = name.find(&lower_term) {
        return Some(CONTIGUOUS_NAME + if at == 0 { PREFIX_BONUS } else { 0 });
    }
    if dir.contains(&lower_term) {
        return Some(CONTIGUOUS_DIR);
    }
    if is_subsequence(&lower_term, name) {
        return Some(SCATTERED_NAME);
    }
    is_subsequence(&lower_term, dir).then_some(SCATTERED_DIR)
}

/// Scores `path` against `terms`, or `None` when any term does not match. Empty terms are
/// ignored; no terms at all scores 0.
pub fn score(path: &str, terms: &[&str]) -> Option<i64> {
    let name_start = path.rfind('/').map_or(0, |i| i + 1);
    let mut total = 0;
    for term in terms {
        total += score_term(path, name_start, term)?;
    }
    Some(total)
}

/// Re-ranks `paths` by score, best first, dropping non-matches and keeping at most `limit`.
/// Paths with equal scores keep their input order.
pub fn rank<'a>(paths: &[&'a str], terms: &[&str], limit: usize) -> Vec<&'a str> {
    let mut scored: Vec<(i64, usize, &'a str)> = paths
        .iter()
        .enumerate()
        .filter_map(|(i, p)| score(p, terms).map(|s| (s, i, *p)))
        .collect();
    scored.sort_by(|a, b| b.0.cmp(&a.0).then(a.1.cmp(&b.1)));
    scored.into_iter().take(limit).map(|(_, _, p)| p).collect()
}

#[cfg(test)]
#[path = "path_rank_tests.rs"]
mod tests;
