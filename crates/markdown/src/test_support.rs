//! Helpers for tests that need to know what a Markdown view shows.

#[cfg(test)]
use std::ops::Range;
use std::{cell::RefCell, rc::Rc};

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
    #[cfg(test)]
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
            #[cfg(test)]
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

            builder.close_all_inline_tags(0);
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
    fn a_code_tag_styles_the_text_until_its_closing_tag(cx: &mut TestAppContext) {
        use crate::MarkdownElementBuilder;
        use gpui::{FontWeight, TextStyle, TextStyleRefinement, hsla};
        use std::sync::Arc;

        let (markdown, cx) = markdown_view("x", cx);
        let code_background = hsla(0.5, 0.5, 0.5, 1.);
        let element = MarkdownElement::new(
            markdown,
            MarkdownStyle {
                inline_code: TextStyleRefinement {
                    background_color: Some(code_background),
                    ..Default::default()
                },
                ..Default::default()
            },
        );
        cx.update(|_, cx| {
            for (open, render_inline_html) in [
                ("<code>", true),
                ("<code class=\"language-rs\">", true),
                ("<code>", false),
            ] {
                let mut builder = MarkdownElementBuilder::new(
                    &Default::default(),
                    TextStyle::default(),
                    Arc::new(theme::SyntaxTheme::default()),
                );
                // Bold text around the tag, as `**a <code>x</code> b**` has.
                builder.push_text_style(TextStyleRefinement {
                    font_weight: Some(FontWeight::BOLD),
                    ..Default::default()
                });

                element.push_inline_html(
                    &mut builder,
                    open,
                    &(0..open.len()),
                    render_inline_html,
                    cx,
                );
                assert_eq!(
                    builder.text_style().background_color,
                    Some(code_background),
                    "{open} opened"
                );

                element.push_inline_html(&mut builder, "</code>", &(0..7), render_inline_html, cx);
                let style = builder.text_style();
                assert_eq!(style.background_color, None, "{open} closed");
                assert_eq!(style.font_weight, FontWeight::BOLD, "{open} closed");
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
    fn an_html_link_left_open_runs_to_the_end_of_its_block(cx: &mut TestAppContext) {
        let source = "go <a href=\"https://example.com/x\">here and more\n\nnext";
        let (markdown, cx) = markdown_view(source, cx);
        let shown = draw_markdown(&markdown, cx);
        assert_eq!(shown.links.len(), 1);
        assert_eq!(
            source[shown.links[0].1.clone()].trim_end(),
            "<a href=\"https://example.com/x\">here and more"
        );
    }

    /// A window whose root view is the Markdown element, which is what a screen reader reads.
    struct MarkdownWindow(Entity<Markdown>);

    impl Render for MarkdownWindow {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            MarkdownElement::new(self.0.clone(), MarkdownStyle::default()).code_block_renderer(
                CodeBlockRenderer::Default {
                    copy_button_visibility: CopyButtonVisibility::Hidden,
                    wrap_button_visibility: WrapButtonVisibility::Hidden,
                    border: false,
                },
            )
        }
    }

    fn accessible_markdown_view<'a>(
        source: &str,
        cx: &'a mut TestAppContext,
    ) -> (Entity<Markdown>, &'a mut VisualTestContext) {
        cx.update(|cx| {
            if !cx.has_global::<settings::SettingsStore>() {
                settings::init(cx);
            }
            if !cx.has_global::<theme::GlobalTheme>() {
                theme_settings::init(theme::LoadThemes::JustBase, cx);
            }
        });
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
        let (_, cx) = cx.add_window_view({
            let markdown = markdown.clone();
            move |_, _| MarkdownWindow(markdown)
        });
        cx.enable_accessibility();
        (markdown, cx)
    }

    /// The role, node and expanded state of each node a screen reader is given.
    fn a11y_nodes(
        cx: &mut VisualTestContext,
    ) -> Vec<(gpui::accesskit::Role, gpui::accesskit::NodeId, Option<bool>)> {
        cx.a11y_tree()
            .expect("the window built an accessibility tree")
            .nodes
            .into_iter()
            .map(|(id, node)| (node.role(), id, node.is_expanded()))
            .collect()
    }

    #[gpui::test]
    fn a_details_summary_is_an_expandable_control_for_a_screen_reader(cx: &mut TestAppContext) {
        let source = "<details>\n<summary>S</summary>\n\nbody\n\n</details>";
        let (markdown, cx) = accessible_markdown_view(source, cx);
        cx.run_until_parked();

        let toggle = |cx: &mut VisualTestContext| {
            a11y_nodes(cx)
                .into_iter()
                .find(|(role, _, _)| *role == gpui::accesskit::Role::DisclosureTriangle)
                .map(|(_, id, expanded)| (id, expanded))
        };
        let (id, expanded) = toggle(cx).expect("the summary is a disclosure control");
        assert_eq!(expanded, Some(false));

        cx.simulate_a11y_action(id, gpui::accesskit::Action::Click);
        assert_eq!(toggle(cx).map(|(_, expanded)| expanded), Some(Some(true)));
        assert!(
            markdown.read_with(cx, |markdown, _| markdown.is_details_open((0, 0), false)),
            "the click toggled the region"
        );
    }

    #[gpui::test]
    fn a_diagram_that_cannot_be_drawn_has_no_code_button_when_controls_are_hidden(
        cx: &mut TestAppContext,
    ) {
        let (markdown, cx) = accessible_markdown_view("```mermaid\nsankey-beta\na,b,1\n```", cx);
        cx.run_until_parked();
        let reasons =
            markdown.read_with(cx, |markdown, _| markdown.mermaid_state.failure_reasons());
        assert_eq!(reasons.len(), 1, "{reasons:?}");

        let buttons: Vec<_> = a11y_nodes(cx)
            .into_iter()
            .filter(|(role, _, _)| *role == gpui::accesskit::Role::Button)
            .collect();
        assert!(buttons.is_empty(), "{buttons:?}");
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
    fn content_before_the_closing_tag_in_the_closing_block_is_hidden_with_the_region(
        cx: &mut TestAppContext,
    ) {
        let source = "<details>\n<summary>S</summary>\n\nbody\n\n<p>block hidden</p>\n</details>\n<p>block after</p>\n\nafter";
        let (markdown, cx) = markdown_view(source, cx);
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            !text.contains("body") && !text.contains("block hidden"),
            "{text:?}"
        );
        assert!(
            text.contains("block after") && text.contains("after"),
            "{text:?}"
        );
        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(text.contains("block hidden"), "{text:?}");
    }

    #[gpui::test]
    fn each_details_of_one_block_toggles_its_own_body(cx: &mut TestAppContext) {
        // One HTML block holds a complete `<details>` and one whose body follows in later blocks.
        let source = "<details><summary>First</summary>inline</details>\n<details>\n<summary>Second</summary>\n\nsecond body\n\n</details>\n\nafter";
        let (markdown, cx) = markdown_view(source, cx);

        let text = rendered_plain_text(&markdown, cx);
        assert!(
            text.contains("First") && text.contains("Second"),
            "{text:?}"
        );
        assert!(
            !text.contains("inline") && !text.contains("second body"),
            "{text:?}"
        );
        assert!(text.contains("after"), "{text:?}");

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(text.contains("inline"), "{text:?}");
        assert!(!text.contains("second body"), "{text:?}");

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 1)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(text.contains("second body"), "{text:?}");
        assert!(!text.contains("inline"), "{text:?}");
    }

    #[gpui::test]
    fn nested_regions_one_block_opens_together_toggle_separately(cx: &mut TestAppContext) {
        let source = "<details>\n<summary>A</summary>\n<details>\n<summary>B</summary>\n\ninner\n\n</details>\n\nouter body\n\n</details>\n\nafter";
        let (markdown, cx) = markdown_view(source, cx);
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            text.contains('A') && !text.contains('B') && !text.contains("outer body"),
            "{text:?}"
        );

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            text.contains('B') && text.contains("outer body"),
            "{text:?}"
        );
        assert!(!text.contains("inner"), "{text:?}");
        assert!(text.contains("after"), "{text:?}");

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 1)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            text.contains("inner") && text.contains("outer body"),
            "{text:?}"
        );

        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            !text.contains("inner") && !text.contains("outer body") && text.contains("after"),
            "{text:?}"
        );
    }

    #[gpui::test]
    fn a_region_in_a_list_item_is_numbered_and_closed_with_the_block(cx: &mut TestAppContext) {
        let source = "<details>x</details>\n<ul><li><details><summary>S</summary>\n\nbody\n\n</details></li></ul>\n\nafter";
        let (markdown, cx) = markdown_view(source, cx);
        let text = rendered_plain_text(&markdown, cx);
        assert!(
            text.contains('S') && !text.contains("body") && text.contains("after"),
            "{text:?}"
        );

        // The region in the list is the block's second.
        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 1)));
        let text = rendered_plain_text(&markdown, cx);
        assert!(text.contains("body"), "{text:?}");
    }

    #[gpui::test]
    fn a_toggle_does_not_outlive_the_document_it_was_made_in(cx: &mut TestAppContext) {
        let source = "<details>\n<summary>S</summary>\n\nbody\n\n</details>";
        let (markdown, cx) = markdown_view(source, cx);
        markdown.update(cx, |markdown, _| markdown.toggle_details((0, 0)));
        assert!(rendered_plain_text(&markdown, cx).contains("body"));

        markdown.update(cx, |markdown, cx| markdown.replace("", cx));
        markdown.update(cx, |markdown, cx| markdown.replace(source, cx));
        let text = rendered_plain_text(&markdown, cx);
        assert!(text.contains('S') && !text.contains("body"), "{text:?}");
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
