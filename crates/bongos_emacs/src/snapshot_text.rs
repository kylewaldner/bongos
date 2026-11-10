//! The editor's text as the model's `TextSource`.

use bongos_emacs_model::TextSource;
use multi_buffer::{MultiBufferOffset, MultiBufferSnapshot};

/// A multi-buffer snapshot read through byte offsets. A word follows the language's word
/// characters at each offset (`foo_bar` is one word).
pub struct SnapshotText<'a> {
    snapshot: &'a MultiBufferSnapshot,
}

impl<'a> SnapshotText<'a> {
    pub fn new(snapshot: &'a MultiBufferSnapshot) -> Self {
        Self { snapshot }
    }

    pub fn snapshot(&self) -> &MultiBufferSnapshot {
        self.snapshot
    }
}

impl TextSource for SnapshotText<'_> {
    fn len(&self) -> usize {
        self.snapshot.len().0
    }

    fn chars_at(&self, offset: usize) -> Box<dyn Iterator<Item = char> + '_> {
        Box::new(self.snapshot.chars_at(MultiBufferOffset(offset)))
    }

    fn reversed_chars_at(&self, offset: usize) -> Box<dyn Iterator<Item = char> + '_> {
        Box::new(self.snapshot.reversed_chars_at(MultiBufferOffset(offset)))
    }

    fn is_word_char(&self, c: char, offset: usize) -> bool {
        self.snapshot
            .char_classifier_at(MultiBufferOffset(offset))
            .is_word(c)
    }
}
