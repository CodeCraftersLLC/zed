//! Inline HTML tags inside Markdown paragraphs, such as `<b>`, `<kbd>` or `<a href>`.
//!
//! Only the tags listed in [`InlineTagKind`] change how text is drawn. Every other tag renders as
//! nothing, never as its source.

use std::borrow::Cow;

/// An inline tag the renderer gives a meaning to.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum InlineTagKind {
    Bold,
    Italic,
    Underline,
    Strikethrough,
    Code,
    Subscript,
    Superscript,
    Mark,
    Link,
    LineBreak,
}

impl InlineTagKind {
    fn from_name(name: &str) -> Option<Self> {
        Some(match name {
            "b" | "strong" => Self::Bold,
            "i" | "em" => Self::Italic,
            "u" | "ins" => Self::Underline,
            "s" | "del" | "strike" => Self::Strikethrough,
            "kbd" | "code" | "samp" | "tt" => Self::Code,
            "sub" => Self::Subscript,
            "sup" => Self::Superscript,
            "mark" => Self::Mark,
            "a" => Self::Link,
            "br" => Self::LineBreak,
            _ => return None,
        })
    }
}

/// One inline HTML tag, as found in the Markdown source.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum InlineTag {
    /// An opening tag of a kind that changes how text is drawn. `href` is set for `<a href>`.
    Open {
        kind: InlineTagKind,
        href: Option<String>,
    },
    /// The closing tag of a kind that changes how text is drawn.
    Close(InlineTagKind),
    /// A comment, a doctype, a self-closing tag, or a tag the renderer does not draw.
    Ignored,
}

impl InlineTag {
    pub(crate) fn parse(source: &str) -> Self {
        let Some(inner) = source.trim().strip_prefix('<') else {
            return Self::Ignored;
        };
        let (closing, inner) = match inner.strip_prefix('/') {
            Some(inner) => (true, inner),
            None => (false, inner),
        };
        let name_len = inner
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '-'))
            .unwrap_or(inner.len());
        let name = inner[..name_len].to_ascii_lowercase();
        let Some(kind) = InlineTagKind::from_name(&name) else {
            return Self::Ignored;
        };
        if closing {
            return Self::Close(kind);
        }
        let attributes = inner[name_len..].trim_end_matches('>');
        if is_self_closing(attributes) && kind != InlineTagKind::LineBreak {
            return Self::Ignored;
        }
        let href = (kind == InlineTagKind::Link)
            .then(|| attribute_value(attributes, "href"))
            .flatten();
        Self::Open { kind, href }
    }
}

/// One attribute of a tag.
struct Attribute<'a> {
    name: &'a str,
    value: Option<&'a str>,
    quoted: bool,
}

/// The attributes in the text of a tag after its name, up to its `>`.
fn attributes(mut rest: &str) -> impl Iterator<Item = Attribute<'_>> {
    std::iter::from_fn(move || {
        rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '/');
        if rest.is_empty() {
            return None;
        }
        let name_len = rest
            .find(|c: char| c.is_whitespace() || c == '=' || c == '/')
            .unwrap_or(rest.len());
        let (name, after) = rest.split_at(name_len);
        let Some(after_equals) = after.trim_start().strip_prefix('=') else {
            rest = after;
            return Some(Attribute {
                name,
                value: None,
                quoted: false,
            });
        };
        let after_equals = after_equals.trim_start();
        let (value, remaining, quoted) = match after_equals.chars().next() {
            Some(quote @ ('"' | '\'')) => {
                let body = &after_equals[1..];
                let end = body.find(quote).unwrap_or(body.len());
                (&body[..end], body.get(end + 1..).unwrap_or(""), true)
            }
            _ => {
                let end = after_equals
                    .find(char::is_whitespace)
                    .unwrap_or(after_equals.len());
                (&after_equals[..end], &after_equals[end..], false)
            }
        };
        rest = remaining;
        Some(Attribute {
            name,
            value: Some(value),
            quoted,
        })
    })
}

/// The value of attribute `name` in the text of a tag after its name, if it has one.
fn attribute_value(attributes_text: &str, name: &str) -> Option<String> {
    attributes(attributes_text)
        .find_map(|attribute| {
            attribute
                .name
                .eq_ignore_ascii_case(name)
                .then_some(attribute.value)
                .flatten()
        })
        .map(|value| decode_ampersands(value).into_owned())
}

/// Whether the text of a tag after its name ends in `/>`. A `/` that ends an unquoted attribute
/// value, as in `<a href=https://example.com/>`, belongs to the value and closes nothing.
fn is_self_closing(attributes_text: &str) -> bool {
    let attributes_text = attributes_text.trim_end();
    if !attributes_text.ends_with('/') {
        return false;
    }
    let ends_a_value = attributes(attributes_text).last().is_some_and(|last| {
        !last.quoted
            && last
                .value
                .is_some_and(|value| value.ends_with('/') && attributes_text.ends_with(value))
    });
    !ends_a_value
}

/// The only entity that matters in a URL: `&amp;` between query parameters.
fn decode_ampersands(value: &str) -> Cow<'_, str> {
    if value.contains("&amp;") {
        Cow::Owned(value.replace("&amp;", "&"))
    } else {
        Cow::Borrowed(value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open(kind: InlineTagKind) -> InlineTag {
        InlineTag::Open { kind, href: None }
    }

    #[test]
    fn style_tags_open_and_close_whatever_their_case() {
        assert_eq!(InlineTag::parse("<b>"), open(InlineTagKind::Bold));
        assert_eq!(InlineTag::parse("<STRONG>"), open(InlineTagKind::Bold));
        assert_eq!(
            InlineTag::parse("</b>"),
            InlineTag::Close(InlineTagKind::Bold)
        );
        assert_eq!(InlineTag::parse("<kbd>"), open(InlineTagKind::Code));
        assert_eq!(InlineTag::parse("<ins>"), open(InlineTagKind::Underline));
        assert_eq!(
            InlineTag::parse("<del>"),
            open(InlineTagKind::Strikethrough)
        );
        assert_eq!(InlineTag::parse("<br>"), open(InlineTagKind::LineBreak));
        assert_eq!(InlineTag::parse("<br/>"), open(InlineTagKind::LineBreak));
        assert_eq!(InlineTag::parse("<br />"), open(InlineTagKind::LineBreak));
    }

    #[test]
    fn other_tags_and_comments_are_ignored() {
        for source in [
            "<span class=\"x\">",
            "</span>",
            "<!-- note -->",
            "<img src=\"a.png\">",
            "<b/>",
            "x",
        ] {
            assert_eq!(InlineTag::parse(source), InlineTag::Ignored, "{source}");
        }
    }

    #[test]
    fn a_slash_that_ends_an_unquoted_value_does_not_close_the_tag() {
        for source in [
            "<a href=https://example.com/>",
            "<a href=https://example.com/docs/>",
            "<a class=x href=/>",
        ] {
            assert!(
                matches!(
                    InlineTag::parse(source),
                    InlineTag::Open {
                        kind: InlineTagKind::Link,
                        href: Some(_)
                    }
                ),
                "{source}"
            );
        }
        assert_eq!(
            InlineTag::parse("<a href=https://example.com/>"),
            InlineTag::Open {
                kind: InlineTagKind::Link,
                href: Some("https://example.com/".to_string())
            }
        );
        for source in [
            "<a href=\"x/\"/>",
            "<a href=x />",
            "<a href=x/ />",
            "<a href='x' />",
            "<b hidden/>",
        ] {
            assert_eq!(InlineTag::parse(source), InlineTag::Ignored, "{source}");
        }
    }

    #[test]
    fn a_link_carries_its_href() {
        let href = |source: &str| match InlineTag::parse(source) {
            InlineTag::Open {
                kind: InlineTagKind::Link,
                href,
            } => href,
            other => panic!("{source}: {other:?}"),
        };
        assert_eq!(
            href("<a href=\"https://example.com\">"),
            Some("https://example.com".into())
        );
        assert_eq!(
            href("<a title=\"t\" href='https://example.com/?a=1&amp;b=2'>"),
            Some("https://example.com/?a=1&b=2".into())
        );
        assert_eq!(
            href("<a href=https://example.com>"),
            Some("https://example.com".into())
        );
        assert_eq!(href("<a name=\"top\">"), None);
        assert_eq!(href("<a>"), None);
    }
}
