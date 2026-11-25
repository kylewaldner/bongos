use std::ops::RangeInclusive;

/// Merges overlapping or adjacent inclusive index ranges into the fewest ranges, sorted by start.
pub fn merge_ranges(mut ranges: Vec<RangeInclusive<usize>>) -> Vec<RangeInclusive<usize>> {
    ranges.sort_by_key(|range| *range.start());
    let mut merged: Vec<RangeInclusive<usize>> = Vec::new();
    for range in ranges {
        match merged.last_mut() {
            Some(last) if *range.start() <= last.end().saturating_add(1) => {
                if range.end() > last.end() {
                    *last = *last.start()..=*range.end();
                }
            }
            _ => merged.push(range),
        }
    }
    merged
}
