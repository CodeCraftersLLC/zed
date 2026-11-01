use text::Anchor;
use ui::SharedString;

/// Position of a cell within the source CSV buffer
#[derive(Clone, Debug)]
pub struct CellContentSpan {
    /// Start anchor of the cell content in the source buffer
    pub start: Anchor,
    /// End anchor of the cell content in the source buffer
    pub end: Anchor,
}

/// A table cell with its content and position in the source buffer
#[derive(Clone, Debug)]
pub enum TableCell {
    /// Cell existing in the CSV
    Real {
        /// Position of this cell in the source buffer
        position: CellContentSpan,
        /// Cached display value (for performance)
        cached_value: SharedString,
        /// `cached_value` on one line, worked out once while parsing so drawing a cell does no
        /// per-frame work on its text.
        single_line_value: SharedString,
    },
    /// Virtual cell, created to pad malformed row
    Virtual,
}

impl TableCell {
    /// Create a TableCell with buffer position tracking
    pub fn from_buffer_position(
        content: SharedString,
        start_offset: usize,
        end_offset: usize,
        buffer_snapshot: &text::BufferSnapshot,
    ) -> Self {
        let start_anchor = buffer_snapshot.anchor_before(start_offset);
        let end_anchor = buffer_snapshot.anchor_after(end_offset);

        Self::Real {
            position: CellContentSpan {
                start: start_anchor,
                end: end_anchor,
            },
            single_line_value: single_line_text(&content),
            cached_value: content,
        }
    }

    /// The cell's text on one line, as drawn. `display_value` is the full text.
    pub fn single_line_value(&self) -> Option<&SharedString> {
        match self {
            TableCell::Real {
                single_line_value, ..
            } => Some(single_line_value),
            TableCell::Virtual => None,
        }
    }

    /// Get the display value for this cell
    pub fn display_value(&self) -> Option<&SharedString> {
        match self {
            TableCell::Real { cached_value, .. } => Some(cached_value),
            TableCell::Virtual => None,
        }
    }
}

/// Line breaks become `↵` so a multi-line cell keeps the row height of its neighbours. The
/// tooltip carries the full text.
fn single_line_text(content: &SharedString) -> SharedString {
    if !content.contains(['\n', '\r']) {
        return content.clone();
    }
    let mut text = String::with_capacity(content.len());
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                chars.next_if_eq(&'\n');
                text.push('↵');
            }
            '\n' => text.push('↵'),
            _ => text.push(ch),
        }
    }
    text.into()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn line_breaks_in_a_cell_are_drawn_as_return_markers() {
        let text = |value: &str| single_line_text(&value.into()).to_string();
        assert_eq!(text("plain"), "plain");
        assert_eq!(text("a\nb"), "a↵b");
        assert_eq!(text("a\r\nb"), "a↵b");
        assert_eq!(text("a\rb\n\nc"), "a↵b↵↵c");
    }
}
