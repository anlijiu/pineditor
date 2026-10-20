//! The identifier under the mouse or cursor, shared by hover and go-to-definition.

use gpui_kit::component::input::{Position, Rope, RopeExt};

/// The word at `offset` plus its 0-based start and end positions.
///
/// `None` when the offset is not on an identifier (whitespace, a bracket), which
/// is what keeps hover from firing on empty space.
pub fn word_at(text: &Rope, offset: usize) -> Option<(String, Position, Position)> {
    let range = text.word_range(offset)?;
    if range.is_empty() {
        return None;
    }

    let word = text.slice(range.clone()).to_string();
    if word.is_empty() {
        return None;
    }

    Some((
        word,
        text.offset_to_position(range.start),
        text.offset_to_position(range.end),
    ))
}
