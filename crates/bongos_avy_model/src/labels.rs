pub const HOME_ROW: [char; 9] = ['a', 's', 'd', 'f', 'g', 'h', 'j', 'k', 'l'];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Resolved {
    Chosen(usize),
    Prefix,
    NoMatch,
}

/// Prefix-free labels for `n` candidates. Starts with one leaf per key and,
/// while there are too few, expands the last leaf of the shallowest depth into
/// one child per key. Leaves are listed by depth, then alphabet order.
pub fn labels(n: usize, alphabet: &[char]) -> Vec<String> {
    if n == 0 || alphabet.is_empty() {
        return Vec::new();
    }
    // Leaves grouped by depth; levels[d] holds the leaves of length d + 1.
    let mut levels: Vec<Vec<Vec<char>>> = vec![alphabet.iter().map(|&c| vec![c]).collect()];
    let mut count = alphabet.len();
    while count < n {
        let depth = match levels.iter().position(|level| !level.is_empty()) {
            Some(depth) => depth,
            None => break,
        };
        let Some(leaf) = levels[depth].pop() else {
            break;
        };
        if alphabet.len() == 1 {
            // A single key cannot branch into more leaves than it removes.
            levels[depth].push(leaf);
            break;
        }
        if levels.len() == depth + 1 {
            levels.push(Vec::new());
        }
        for &c in alphabet {
            let mut child = leaf.clone();
            child.push(c);
            levels[depth + 1].push(child);
        }
        count += alphabet.len() - 1;
    }
    levels
        .into_iter()
        .flatten()
        .take(n)
        .map(|leaf| leaf.into_iter().collect())
        .collect()
}

pub fn resolve(labels: &[String], typed: &str) -> Resolved {
    if let Some(index) = labels.iter().position(|label| label == typed) {
        return Resolved::Chosen(index);
    }
    if labels.iter().any(|label| label.starts_with(typed)) {
        Resolved::Prefix
    } else {
        Resolved::NoMatch
    }
}
