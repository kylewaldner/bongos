use std::sync::Arc;

/// The texts a prompt completes over. Shared, so a background pass can hold them while the next
/// keystroke arrives.
pub type Texts = Arc<[Arc<str>]>;

/// Builds shared candidate texts from any list of strings.
pub fn texts<I, S>(items: I) -> Texts
where
    I: IntoIterator<Item = S>,
    S: AsRef<str>,
{
    items
        .into_iter()
        .map(|item| Arc::from(item.as_ref()))
        .collect()
}

/// A candidate that matched: its position in the source texts and its score (higher is better;
/// zero for an empty query).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub index: u32,
    pub score: u32,
}

/// The longest prefix, by characters, that every item starts with, compared exactly (case
/// matters). Empty when there are no items or they share nothing. The result borrows from the
/// first item.
pub fn common_prefix<'a>(items: impl IntoIterator<Item = &'a str>) -> &'a str {
    let mut items = items.into_iter();
    let Some(first) = items.next() else {
        return "";
    };
    let mut end = first.len();
    for item in items {
        let mut shared = 0;
        for ((ix, a), b) in first[..end].char_indices().zip(item.chars()) {
            if a != b {
                break;
            }
            shared = ix + a.len_utf8();
        }
        end = shared;
        if end == 0 {
            break;
        }
    }
    &first[..end]
}

/// The match count of the input row: `3/417`, matches over candidates.
pub fn count_text(matched: usize, total: usize) -> String {
    format!("{matched}/{total}")
}
