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

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum DetailsTag {
    Open,
    Close,
}

/// The `<details` and `</details` tags of an HTML block, in source order.
fn details_tags(html: &str) -> Vec<DetailsTag> {
    let lowercase = html.to_ascii_lowercase();
    lowercase
        .match_indices('<')
        .filter_map(|(ix, _)| {
            let rest = &lowercase[ix..];
            let (tag, name_len) = if rest.starts_with("<details") {
                (DetailsTag::Open, "<details".len())
            } else if rest.starts_with("</details") {
                (DetailsTag::Close, "</details".len())
            } else {
                return None;
            };
            rest[name_len..]
                .chars()
                .next()
                .is_none_or(|next| next.is_whitespace() || next == '>' || next == '/')
                .then_some(tag)
        })
        .collect()
}

/// What an HTML block leaves open and what it closes of the blocks before it.
#[derive(Debug, Default, Eq, PartialEq)]
pub(crate) struct DetailsBalance {
    /// The `<details>` of the block, counted in the order they open, that it does not close.
    pub unclosed: Vec<usize>,
    /// How many `</details>` of the block close a `<details>` an earlier block opened.
    pub closes_earlier: usize,
}

pub(crate) fn details_balance(html: &str) -> DetailsBalance {
    let mut balance = DetailsBalance::default();
    let mut opened = 0;
    for tag in details_tags(html) {
        match tag {
            DetailsTag::Open => {
                balance.unclosed.push(opened);
                opened += 1;
            }
            DetailsTag::Close => {
                if balance.unclosed.pop().is_none() {
                    balance.closes_earlier += 1;
                }
            }
        }
    }
    balance
}

/// Pairs every HTML block that leaves a `<details>` open with the block that closes it. The key is
/// the source offset of the opening block.
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

        let balance = details_balance(&source[range.clone()]);
        for _ in 0..balance.closes_earlier {
            if let Some((start, opener_end)) = open.pop() {
                spans.insert(
                    start,
                    DetailsSpan {
                        hidden_events: opener_end + 1..index,
                    },
                );
            }
        }
        for _ in &balance.unclosed {
            open.push((range.start, block_end));
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

    fn balance(unclosed: &[usize], closes_earlier: usize) -> DetailsBalance {
        DetailsBalance {
            unclosed: unclosed.to_vec(),
            closes_earlier,
        }
    }

    #[test]
    fn tags_are_matched_by_name_not_prefix() {
        assert_eq!(details_balance("<detailsx></detailsx>"), balance(&[], 0));
        assert_eq!(
            details_balance("<DETAILS open>x</Details>"),
            balance(&[], 0)
        );
    }

    #[test]
    fn the_unclosed_details_of_a_block_are_the_ones_without_a_closing_tag() {
        assert_eq!(
            details_balance("<details><summary>S</summary>"),
            balance(&[0], 0)
        );
        assert_eq!(details_balance("<details>x</details>"), balance(&[], 0));
        // The second opens after the first is closed.
        assert_eq!(
            details_balance("<details>a</details>\n<details>\n<summary>S</summary>"),
            balance(&[1], 0)
        );
        assert_eq!(
            details_balance("<details><details>x</details>"),
            balance(&[0], 0)
        );
        assert_eq!(details_balance("<details><details>x"), balance(&[0, 1], 0));
        // A closing tag for a region an earlier block opened closes nothing in this one.
        assert_eq!(details_balance("</details>\n<details>"), balance(&[0], 1));
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
    fn a_block_that_closes_one_region_and_opens_another_pairs_both() {
        let source = "<details>\n<summary>A</summary>\n\nbody A\n\n</details>\n<details>\n<summary>B</summary>\n\nbody B\n\n</details>";
        let spans = spans(source);
        let second_opening = source.find("</details>\n<details>").unwrap();
        assert_eq!(
            spans.keys().copied().collect::<Vec<_>>(),
            [0, second_opening]
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
