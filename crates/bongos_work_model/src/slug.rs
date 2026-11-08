use std::collections::HashSet;

use chrono::NaiveDate;

/// The longest slug [`slugify`] returns.
pub const MAX_SLUG_LEN: usize = 40;

/// The slug used when nothing usable is left of the input.
pub const EMPTY_SLUG: &str = "task";

/// Lowercase ASCII letters and digits; every run of anything else becomes one `-`; no leading or
/// trailing `-`; at most [`MAX_SLUG_LEN`] characters; [`EMPTY_SLUG`] if nothing is left.
pub fn slugify(text: &str) -> String {
    let mut slug = String::new();
    for character in text.chars() {
        if character.is_ascii_alphanumeric() {
            slug.push(character.to_ascii_lowercase());
        } else if !slug.is_empty() && !slug.ends_with('-') {
            slug.push('-');
        }
    }
    truncate_slug(&mut slug, MAX_SLUG_LEN);
    if slug.is_empty() {
        EMPTY_SLUG.to_string()
    } else {
        slug
    }
}

fn truncate_slug(slug: &mut String, max_len: usize) {
    // A slug is ASCII, so any byte index is a character boundary.
    slug.truncate(max_len);
    while slug.ends_with('-') {
        slug.pop();
    }
}

/// `base` if no sibling uses it, else `base-2`, `base-3` and so on, the first one that is free.
/// The base is shortened when needed so the result stays within [`MAX_SLUG_LEN`].
pub fn unique_slug<S: AsRef<str>>(base: &str, existing_siblings: &[S]) -> String {
    let taken: HashSet<&str> = existing_siblings.iter().map(AsRef::as_ref).collect();
    if !taken.contains(base) {
        return base.to_string();
    }
    (2u64..)
        .map(|number| {
            let suffix = format!("-{number}");
            let mut stem = base.to_string();
            truncate_slug(&mut stem, MAX_SLUG_LEN.saturating_sub(suffix.len()));
            format!("{stem}{suffix}")
        })
        .find(|candidate| !taken.contains(candidate.as_str()))
        .unwrap_or_else(|| unreachable!("an unbounded range always yields a free slug"))
}

/// The directory name of a one-off: `<YYYY-MM-DD>-<slug>`.
pub fn oneoff_dir_name(date: NaiveDate, slug: &str) -> String {
    format!("{}-{slug}", date.format("%Y-%m-%d"))
}

/// The slug in a one-off directory name from [`oneoff_dir_name`]: the name without its date. A
/// name with no date prefix is returned as it is.
pub fn oneoff_slug(dir_name: &str) -> &str {
    let dated = dir_name.get(..11).is_some_and(|prefix| {
        prefix.ends_with('-') && NaiveDate::parse_from_str(&prefix[..10], "%Y-%m-%d").is_ok()
    });
    match dir_name.get(11..) {
        Some(slug) if dated && !slug.is_empty() => slug,
        _ => dir_name,
    }
}
