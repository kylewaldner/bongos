//! Preview size limits.

/// A file may be previewed up to and including `limit` bytes.
pub fn preview_allowed(size: u64, limit: u64) -> bool {
    size <= limit
}

/// `Preview skipped: 3.4 MB`, using decimal units with one decimal place from KB up.
pub fn skipped_label(size: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if size < 1000 {
        return format!("Preview skipped: {size} B");
    }
    let mut value = size as f64 / 1000.0;
    let mut unit = 0;
    while value >= 1000.0 && unit + 1 < UNITS.len() {
        value /= 1000.0;
        unit += 1;
    }
    format!("Preview skipped: {value:.1} {}", UNITS[unit])
}

#[cfg(test)]
#[path = "size_tests.rs"]
mod tests;
