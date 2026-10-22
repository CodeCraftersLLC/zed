//! `<details>` disclosure regions that span several Markdown blocks.
//!
//! CommonMark ends an HTML block of type 6, which `<details>` starts, at the first blank line. A
//! `<details>` whose body is written as Markdown therefore opens in one HTML block and closes in
//! another, with ordinary blocks in between. This module pairs each opening block with its closing
//! one so the renderer can hide everything between them while the region is collapsed.

use std::{collections::BTreeMap, ops::Range};

use crate::parser::{MarkdownEvent, MarkdownTag, MarkdownTagEnd};

/// The events a collapsed `<details>` hides.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct DetailsSpan {
    /// The events strictly between the opening block's end and the closing block's start.
    pub hidden_events: Range<usize>,
}

/// Counts of `<details` and `</details` tags in an HTML block.
pub(crate) fn details_tag_counts(html: &str) -> (usize, usize) {
    let lowercase = html.to_ascii_lowercase();
    let count_tags = |prefix: &str| {
        lowercase
            .match_indices(prefix)
            .filter(|(ix, _)| {
                lowercase[ix + prefix.len()..]
                    .chars()
                    .next()
                    .is_none_or(|next| next.is_whitespace() || next == '>' || next == '/')
            })
            .count()
    };
    (count_tags("<details"), count_tags("</details"))
}

/// Pairs every HTML block that opens more `<details>` than it closes with the block that closes
/// it. The key is the source offset of the opening block.
pub(crate) fn extract_details_spans(
    source: &str,
    events: &[(Range<usize>, MarkdownEvent)],
) -> BTreeMap<usize, DetailsSpan> {
    let mut spans = BTreeMap::new();
    // The opening blocks still waiting for their close: (source offset, index of its End event).
    let mut open: Vec<(usize, usize)> = Vec::new();

    let mut index = 0;
    while index < events.len() {
        let (range, event) = &events[index];
        if !matches!(event, MarkdownEvent::Start(MarkdownTag::HtmlBlock)) {
            index += 1;
            continue;
        }
        let block_end = events[index..]
            .iter()
            .position(|(_, event)| matches!(event, MarkdownEvent::End(MarkdownTagEnd::HtmlBlock)))
            .map_or(events.len() - 1, |offset| index + offset);

        let (opens, closes) = details_tag_counts(&source[range.clone()]);
        for _ in closes..opens {
            open.push((range.start, block_end));
        }
        for _ in opens..closes {
            if let Some((start, opener_end)) = open.pop() {
                spans.insert(
                    start,
                    DetailsSpan {
                        hidden_events: opener_end + 1..index,
                    },
                );
            }
        }
        index = block_end + 1;
    }

    spans
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_markdown_with_options;

    fn spans(source: &str) -> BTreeMap<usize, DetailsSpan> {
        let parsed = parse_markdown_with_options(source, true, false, false);
        extract_details_spans(source, &parsed.events)
    }

    #[test]
    fn counts_tags_not_prefixes() {
        assert_eq!(details_tag_counts("<details><summary>S</summary>"), (1, 0));
        assert_eq!(details_tag_counts("</details>"), (0, 1));
        assert_eq!(details_tag_counts("<DETAILS open>x</Details>"), (1, 1));
        assert_eq!(details_tag_counts("<detailsx>"), (0, 0));
    }

    #[test]
    fn a_region_that_spans_blocks_is_paired() {
        let source = "<details>\n<summary>S</summary>\n\nbody\n\n</details>\n\nafter";
        let spans = spans(source);
        assert_eq!(spans.keys().copied().collect::<Vec<_>>(), [0]);
        let parsed = parse_markdown_with_options(source, true, false, false);
        let hidden = &spans[&0].hidden_events;
        assert!(!hidden.is_empty());
        // The closing block starts right after the hidden events.
        assert_eq!(
            parsed.events[hidden.end].1,
            MarkdownEvent::Start(MarkdownTag::HtmlBlock)
        );
        assert_eq!(
            source[parsed.events[hidden.end].0.clone()].trim_end(),
            "</details>"
        );
    }

    #[test]
    fn a_region_closed_in_its_own_block_needs_no_span() {
        assert!(spans("<details><summary>S</summary>body</details>").is_empty());
    }

    #[test]
    fn nested_regions_pair_innermost_first() {
        let source = "<details>\n<summary>A</summary>\n\n<details>\n<summary>B</summary>\n\nx\n\n</details>\n\n</details>\n";
        let spans = spans(source);
        assert_eq!(spans.len(), 2);
        let outer = &spans[&0];
        let inner_start = source.find("<details>\n<summary>B").unwrap();
        let inner = &spans[&inner_start];
        assert!(outer.hidden_events.start < inner.hidden_events.start);
        assert!(inner.hidden_events.end < outer.hidden_events.end);
    }

    #[test]
    fn an_unterminated_region_has_no_span() {
        assert!(spans("<details>\n<summary>S</summary>\n\nbody").is_empty());
    }
}
