//! Most-recently-used ordering for buffer and file switching.

/// Orders `items` (id, stamp) newest first, with the `current` item moved last, so the previous
/// item is first. Equal stamps keep their input order.
pub fn order<Id: PartialEq>(mut items: Vec<(Id, u64)>, current: Option<Id>) -> Vec<Id> {
    items.sort_by_key(|b| std::cmp::Reverse(b.1));
    let (mut rest, current_items): (Vec<_>, Vec<_>) = items
        .into_iter()
        .partition(|(id, _)| Some(id) != current.as_ref());
    rest.extend(current_items);
    rest.into_iter().map(|(id, _)| id).collect()
}

#[cfg(test)]
#[path = "mru_tests.rs"]
mod tests;
