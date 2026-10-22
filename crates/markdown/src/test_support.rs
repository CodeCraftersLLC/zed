//! Helpers for tests that need to know what a Markdown view shows.

use std::{cell::RefCell, ops::Range, rc::Rc};

use gpui::{
    App, AppContext as _, Element, ElementId, Entity, GlobalElementId, Hitbox, InspectorElementId,
    IntoElement, LayoutId, Render, VisualTestContext, Window, px, size,
};

use crate::{
    CodeBlockRenderer, CopyButtonVisibility, Markdown, MarkdownElement, MarkdownStyle,
    RenderedMarkdown, WrapButtonVisibility,
};

/// What a drawn Markdown view showed.
#[derive(Default)]
pub(crate) struct Shown {
    pub lines: Vec<String>,
    /// The destination and source range of each link.
    pub links: Vec<(String, Range<usize>)>,
}

/// A [`MarkdownElement`] that records what it shows when it is painted.
struct RecordingMarkdown {
    element: MarkdownElement,
    shown: Rc<RefCell<Shown>>,
}

impl IntoElement for RecordingMarkdown {
    type Element = Self;

    fn into_element(self) -> Self {
        self
    }
}

impl Element for RecordingMarkdown {
    type RequestLayoutState = RenderedMarkdown;
    type PrepaintState = Hitbox;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        self.element.request_layout(id, inspector_id, window, cx)
    }

    fn prepaint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        self.element
            .prepaint(id, inspector_id, bounds, request_layout, window, cx)
    }

    fn paint(
        &mut self,
        id: Option<&GlobalElementId>,
        inspector_id: Option<&InspectorElementId>,
        bounds: gpui::Bounds<gpui::Pixels>,
        request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        self.element.paint(
            id,
            inspector_id,
            bounds,
            request_layout,
            prepaint,
            window,
            cx,
        );
        let text = &request_layout.text;
        *self.shown.borrow_mut() = Shown {
            lines: text
                .lines
                .iter()
                .map(|line| line.layout.wrapped_text())
                .collect(),
            links: text
                .links
                .iter()
                .map(|link| (link.destination_url.to_string(), link.source_range.clone()))
                .collect(),
        };
    }
}

/// The view a Markdown element is drawn in: interactive elements need a view to be painted in.
struct Harness {
    markdown: Entity<Markdown>,
    shown: Rc<RefCell<Shown>>,
}

impl Render for Harness {
    fn render(&mut self, _: &mut Window, _: &mut gpui::Context<Self>) -> impl IntoElement {
        RecordingMarkdown {
            element: MarkdownElement::new(self.markdown.clone(), MarkdownStyle::default())
                .code_block_renderer(CodeBlockRenderer::Default {
                    copy_button_visibility: CopyButtonVisibility::Hidden,
                    wrap_button_visibility: WrapButtonVisibility::Hidden,
                    border: false,
                }),
            shown: self.shown.clone(),
        }
    }
}

pub(crate) fn draw_markdown(markdown: &Entity<Markdown>, cx: &mut VisualTestContext) -> Shown {
    cx.run_until_parked();
    let shown = Rc::new(RefCell::new(Shown::default()));
    let harness = cx.new(|_| Harness {
        markdown: markdown.clone(),
        shown: shown.clone(),
    });
    cx.draw(
        Default::default(),
        size(px(800.), px(4000.)),
        |_window, _cx| gpui::AnyView::from(harness),
    );
    shown.take()
}

/// The text `markdown` shows once drawn: the laid-out lines, then the reason line of each Mermaid
/// diagram that could not be drawn, which is drawn as an element rather than as text.
pub fn rendered_plain_text(markdown: &Entity<Markdown>, cx: &mut VisualTestContext) -> String {
    let mut text = draw_markdown(markdown, cx).lines.join("\n");
    for reason in markdown.read_with(cx, |markdown, _| markdown.mermaid_state.failure_reasons()) {
        text.push('\n');
        text.push_str(&reason);
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::MarkdownOptions;
    use gpui::{Context, TestAppContext};
    use ui::prelude::*;

    fn markdown_view<'a>(
        source: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Markdown>, &'a mut VisualTestContext) {
        struct TestWindow;

        impl Render for TestWindow {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
            }
        }

        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
        let (_, cx) = cx.add_window_view(|_, _| TestWindow);
        let markdown = cx.new(|cx| {
            Markdown::new_with_options(
                source.to_string().into(),
                None,
                None,
                MarkdownOptions {
                    parse_html: true,
                    render_mermaid_diagrams: true,
                    ..Default::default()
                },
                cx,
            )
        });
        (markdown, cx)
    }

    fn shown(source: &str, cx: &mut TestAppContext) -> String {
        let (markdown, cx) = markdown_view(source, cx);
        rendered_plain_text(&markdown, cx)
    }

    #[gpui::test]
    fn inline_html_is_rendered_not_printed(cx: &mut TestAppContext) {
        assert_eq!(shown("<b>bold html</b>", cx), "bold html");
        assert_eq!(
            shown("see <b>bold</b> and <i>it</i> here", cx),
            "see bold and it here"
        );
        assert_eq!(
            shown("**md** <u>under</u> <s>gone</s>", cx),
            "md under gone"
        );
        assert_eq!(
            shown("press <kbd>Ctrl</kbd>+<kbd>C</kbd>", cx),
            "press Ctrl+C"
        );
        assert_eq!(shown("H<sub>2</sub>O and x<sup>2</sup>", cx), "H2O and x2");
        assert_eq!(shown("a<br>b", cx), "a\nb");
        assert_eq!(shown("a<br/>b<br />c", cx), "a\nb\nc");
        assert_eq!(
            shown(
                "<mark>hit</mark> and <a href=\"https://example.com\">link</a>",
                cx
            ),
            "hit and link"
        );
    }

    #[gpui::test]
    fn unknown_inline_html_renders_nothing_not_its_source(cx: &mut TestAppContext) {
        assert_eq!(shown("a <span class=\"x\">b</span> c", cx), "a b c");
        assert_eq!(shown("a <custom-tag>b</custom-tag> c", cx), "a b c");
        assert_eq!(shown("a <!-- note --> b", cx), "a  b");
    }

    #[gpui::test]
    fn unbalanced_inline_html_does_not_leak_into_later_text(cx: &mut TestAppContext) {
        assert_eq!(
            shown("closing </b> first <b>then open", cx),
            "closing  first then open"
        );
        assert_eq!(
            shown("<b>open\n\nnext paragraph", cx),
            "open\nnext paragraph"
        );
    }

    #[gpui::test]
    fn open_inline_tags_style_the_text_until_they_close(cx: &mut TestAppContext) {
        use crate::{MarkdownElementBuilder, inline_html::InlineTagKind};
        use gpui::{FontStyle, FontWeight, TextStyle};
        use std::sync::Arc;

        let (markdown, cx) = markdown_view("x", cx);
        let element = MarkdownElement::new(markdown, MarkdownStyle::default());
        let default_style = TextStyle::default();
        cx.update(|_, cx| {
            let mut builder = MarkdownElementBuilder::new(
                &Default::default(),
                default_style.clone(),
                Arc::new(theme::SyntaxTheme::default()),
            );
            builder.open_inline_tag(
                InlineTagKind::Bold,
                element.inline_tag_style(InlineTagKind::Bold, cx),
                None,
            );
            builder.open_inline_tag(
                InlineTagKind::Italic,
                element.inline_tag_style(InlineTagKind::Italic, cx),
                None,
            );
            let style = builder.text_style();
            assert_eq!(style.font_weight, FontWeight::BOLD);
            assert_eq!(style.font_style, FontStyle::Italic);

            // A closing tag that was never opened changes nothing.
            builder.close_inline_tag(InlineTagKind::Underline, 0);
            assert_eq!(builder.text_style().font_weight, FontWeight::BOLD);

            builder.close_inline_tag(InlineTagKind::Bold, 0);
            let style = builder.text_style();
            assert_eq!(style.font_weight, default_style.font_weight);
            assert_eq!(style.font_style, FontStyle::Italic);

            builder.close_all_inline_tags();
            assert_eq!(builder.text_style().font_style, default_style.font_style);

            for (kind, has_style) in [
                (
                    InlineTagKind::Underline,
                    element
                        .inline_tag_style(InlineTagKind::Underline, cx)
                        .underline
                        .is_some(),
                ),
                (
                    InlineTagKind::Strikethrough,
                    element
                        .inline_tag_style(InlineTagKind::Strikethrough, cx)
                        .strikethrough
                        .is_some(),
                ),
                (
                    InlineTagKind::Mark,
                    element
                        .inline_tag_style(InlineTagKind::Mark, cx)
                        .background_color
                        .is_some(),
                ),
                (
                    InlineTagKind::Superscript,
                    element
                        .inline_tag_style(InlineTagKind::Superscript, cx)
                        .font_size
                        .is_some(),
                ),
            ] {
                assert!(has_style, "{kind:?}");
            }
        });
    }

    #[gpui::test]
    fn an_html_link_is_a_link(cx: &mut TestAppContext) {
        let source = "go <a href=\"https://example.com/x\">here</a> now";
        let (markdown, cx) = markdown_view(source, cx);
        let shown = draw_markdown(&markdown, cx);
        assert_eq!(shown.links.len(), 1);
        assert_eq!(shown.links[0].0, "https://example.com/x");
        // The link covers the opening tag, its text and the closing tag.
        assert_eq!(
            &source[shown.links[0].1.clone()],
            "<a href=\"https://example.com/x\">here</a>"
        );
    }

    #[gpui::test]
    fn a_details_region_shows_its_summary_and_expands_on_toggle(cx: &mut TestAppContext) {
        let source = "<details>\n<summary>S</summary>\n\nbody\n\n</details>\n\nafter";
        let (markdown, cx) = markdown_view(source, cx);

        let collapsed = rendered_plain_text(&markdown, cx);
        assert!(collapsed.contains('S'), "{collapsed:?}");
        assert!(!collapsed.contains("body"), "{collapsed:?}");
        assert!(collapsed.contains("after"), "{collapsed:?}");
        assert!(!collapsed.contains("</details>"), "{collapsed:?}");

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        let expanded = rendered_plain_text(&markdown, cx);
        assert!(expanded.contains("body"), "{expanded:?}");
        assert!(expanded.contains("after"), "{expanded:?}");

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        assert!(!rendered_plain_text(&markdown, cx).contains("body"));
    }

    #[gpui::test]
    fn a_details_region_with_the_open_attribute_starts_expanded(cx: &mut TestAppContext) {
        let source = "<details open>\n<summary>S</summary>\n\nbody\n\n</details>";
        let text = shown(source, cx);
        assert!(text.contains("body"), "{text:?}");
    }

    #[gpui::test]
    fn a_details_region_in_one_block_collapses_too(cx: &mut TestAppContext) {
        let source = "<details><summary>S</summary><p>hidden text</p></details>\n\nafter";
        let (markdown, cx) = markdown_view(source, cx);
        let collapsed = rendered_plain_text(&markdown, cx);
        assert!(
            collapsed.contains('S') && !collapsed.contains("hidden text"),
            "{collapsed:?}"
        );
        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        assert!(rendered_plain_text(&markdown, cx).contains("hidden text"));
    }

    #[gpui::test]
    fn a_collapsed_nested_region_hides_the_inner_one(cx: &mut TestAppContext) {
        let source = "<details>\n<summary>A</summary>\n\n<details>\n<summary>B</summary>\n\ninner\n\n</details>\n\nouter body\n\n</details>\n\nafter";
        let (markdown, cx) = markdown_view(source, cx);
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            text.contains('A') && !text.contains('B') && !text.contains("inner"),
            "{text:?}"
        );
        assert!(text.contains("after"), "{text:?}");

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            text.contains('B') && text.contains("outer body"),
            "{text:?}"
        );
        assert!(!text.contains("inner"), "{text:?}");
    }
}
