use serde::{Deserialize, Serialize};

/// How many entries a ring keeps.
pub const HISTORY_CAPACITY: usize = 100;

/// The input history of one prompt name: newest first, no duplicates, at most
/// [`HISTORY_CAPACITY`] entries. It serializes as a plain JSON array of strings, newest first.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(from = "Vec<String>", into = "Vec<String>")]
pub struct HistoryRing {
    entries: Vec<String>,
}

impl HistoryRing {
    pub fn new() -> Self {
        Self::default()
    }

    /// Records an entry as the newest. An equal earlier entry moves to the front instead of
    /// repeating, and the oldest entry is dropped past the capacity. An empty entry is ignored.
    pub fn push(&mut self, entry: impl Into<String>) {
        let entry = entry.into();
        if entry.is_empty() {
            return;
        }
        self.entries.retain(|existing| *existing != entry);
        self.entries.insert(0, entry);
        self.entries.truncate(HISTORY_CAPACITY);
    }

    /// The entries, newest first.
    pub fn entries(&self) -> &[String] {
        &self.entries
    }

    /// The entry `ix` steps back, 0 being the newest.
    pub fn get(&self, ix: usize) -> Option<&str> {
        self.entries.get(ix).map(String::as_str)
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn to_json(&self) -> String {
        serde_json::to_string(self).unwrap_or_else(|_| "[]".to_string())
    }

    /// Reads a ring from its JSON. Stored data that is missing, malformed or of another shape
    /// gives an empty ring, never an error: history is a convenience.
    pub fn from_json(json: &str) -> Self {
        serde_json::from_str(json).unwrap_or_default()
    }
}

impl From<Vec<String>> for HistoryRing {
    /// Normalizes stored entries (newest first): drops empty entries and later duplicates, then
    /// applies the capacity.
    fn from(stored: Vec<String>) -> Self {
        let mut entries: Vec<String> = Vec::with_capacity(stored.len().min(HISTORY_CAPACITY));
        for entry in stored {
            if entry.is_empty() || entries.contains(&entry) {
                continue;
            }
            entries.push(entry);
            if entries.len() == HISTORY_CAPACITY {
                break;
            }
        }
        Self { entries }
    }
}

impl From<HistoryRing> for Vec<String> {
    fn from(ring: HistoryRing) -> Self {
        ring.entries
    }
}
