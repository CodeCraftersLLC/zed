use std::sync::Arc;
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

/// What drawing a cell with a line break needs, worked out once while parsing so drawing copies
/// nothing.
#[derive(Clone, Debug)]
pub struct MultilineCell {
    lines: Arc<[SharedString]>,
    flattened: SharedString,
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
        /// Prepared only for a cell with a line break, so a single-line cell holds nothing but
        /// its content.
        multiline: Option<MultilineCell>,
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
            multiline: content.contains(['\n', '\r']).then(|| MultilineCell {
                lines: split_lines(&content),
                flattened: single_line_text(&content),
            }),
            cached_value: content,
        }
    }

    /// The cell's text on one line, as drawn, as a slice of one element. `display_value` is the
    /// full text.
    pub fn single_line(&self) -> Option<&[SharedString]> {
        match self {
            TableCell::Real {
                cached_value,
                multiline,
                ..
            } => Some(match multiline {
                Some(multiline) => std::slice::from_ref(&multiline.flattened),
                None => std::slice::from_ref(cached_value),
            }),
            TableCell::Virtual => None,
        }
    }

    /// Every line of the cell's text.
    pub fn lines(&self) -> Option<&[SharedString]> {
        match self {
            TableCell::Real {
                cached_value,
                multiline,
                ..
            } => Some(match multiline {
                Some(multiline) => &multiline.lines,
                None => std::slice::from_ref(cached_value),
            }),
            TableCell::Virtual => None,
        }
    }

    /// Whether drawing this cell needed prepared data beyond its content.
    #[cfg(test)]
    pub(crate) fn has_prepared_lines(&self) -> bool {
        matches!(
            self,
            TableCell::Real {
                multiline: Some(_),
                ..
            }
        )
    }

    /// Get the display value for this cell
    pub fn display_value(&self) -> Option<&SharedString> {
        match self {
            TableCell::Real { cached_value, .. } => Some(cached_value),
            TableCell::Virtual => None,
        }
    }
}

fn split_lines(content: &SharedString) -> Arc<[SharedString]> {
    // `\r\n` is one break, as in `single_line_text`.
    content
        .split("\r\n")
        .flat_map(|line| line.split(['\r', '\n']))
        .map(|line| SharedString::from(line.to_owned()))
        .collect()
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
    fn every_kind_of_line_break_splits_a_cell_with_crlf_as_one() {
        let lines = |value: &str| {
            split_lines(&value.into())
                .iter()
                .map(|line| line.to_string())
                .collect::<Vec<_>>()
        };
        assert_eq!(lines("a\rb"), ["a", "b"]);
        assert_eq!(lines("a\r\nb"), ["a", "b"]);
        assert_eq!(lines("a\nb\r\r\nc"), ["a", "b", "", "c"]);
    }

    #[test]
    fn a_cell_splits_into_its_lines_once() {
        let text: SharedString = "a\nbb\n".into();
        let lines = split_lines(&text);
        assert_eq!(
            lines.iter().map(|l| l.as_ref()).collect::<Vec<_>>(),
            ["a", "bb", ""]
        );
    }

    #[test]
    fn line_breaks_in_a_cell_are_drawn_as_return_markers() {
        let text = |value: &str| single_line_text(&value.into()).to_string();
        assert_eq!(text("plain"), "plain");
        assert_eq!(text("a\nb"), "a↵b");
        assert_eq!(text("a\r\nb"), "a↵b");
        assert_eq!(text("a\rb\n\nc"), "a↵b↵↵c");
    }
}
