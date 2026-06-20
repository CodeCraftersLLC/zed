use cherrypick_pr::diff_service::{BranchDiff, DiffFileEntry};
use gpui::{
    AnyElement, App, Context, FocusHandle, Focusable, FontWeight, IntoElement, Render, SharedString,
    Window, div, px,
};
use ui::prelude::*;

pub struct DiffFileList {
    focus_handle: FocusHandle,
    diff: Option<BranchDiff>,
}

impl DiffFileList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            diff: None,
        }
    }

    pub fn set_diff(&mut self, diff: Option<BranchDiff>, cx: &mut Context<Self>) {
        self.diff = diff;
        cx.notify();
    }

    fn render_summary(&self, diff: &BranchDiff, cx: &mut Context<Self>) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(format!("{} files changed", diff.files.len())),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().version_control_added)
                    .child(format!("+{}", diff.total_insertions)),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().version_control_deleted)
                    .child(format!("-{}", diff.total_deletions)),
            )
    }

    fn render_file_entry(&self, entry: &DiffFileEntry, cx: &mut Context<Self>) -> AnyElement {
        let status_color = match entry.status {
            'A' => cx.theme().colors().version_control_added,
            'D' => cx.theme().colors().version_control_deleted,
            'M' => cx.theme().colors().version_control_modified,
            'R' => cx.theme().colors().version_control_renamed,
            'C' => cx.theme().colors().version_control_modified,
            'T' => cx.theme().colors().version_control_modified,
            _ => cx.theme().colors().text_muted,
        };

        let path = entry.path.clone();
        let display_path = if let Some(old) = &entry.old_path {
            format!("{} → {}", old, path)
        } else {
            path.clone()
        };

        let ins = entry.insertions;
        let del = entry.deletions;
        let is_binary = entry.is_binary;

        div()
            .id(SharedString::from(format!("diff-file-{}", &path)))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(3.0))
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .w(px(14.0))
                    .text_color(status_color)
                    .child(format!("{}", entry.status)),
            )
            .child(
                div()
                    .flex_grow()
                    .text_sm()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .child(display_path),
            )
            .when(!is_binary && (ins > 0 || del > 0), |el| {
                el.child(
                    div()
                        .flex()
                        .gap(px(4.0))
                        .text_xs()
                        .child(
                            div()
                                .text_color(cx.theme().colors().version_control_added)
                                .child(format!("+{}", ins)),
                        )
                        .child(
                            div()
                                .text_color(cx.theme().colors().version_control_deleted)
                                .child(format!("-{}", del)),
                        ),
                )
            })
            .when(is_binary, |el| {
                el.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors().text_muted)
                        .child("binary"),
                )
            })
            .into_any_element()
    }
}

impl Focusable for DiffFileList {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for DiffFileList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.diff.is_none() {
            return div()
                .id("diff-file-list-empty")
                .p_2()
                .text_sm()
                .text_color(cx.theme().colors().text_muted)
                .child("No diff to display");
        }

        let diff = self.diff.as_ref().unwrap();
        let summary = self.render_summary(diff, cx);
        let file_entries: Vec<_> = diff
            .files
            .iter()
            .map(|f| self.render_file_entry(f, cx))
            .collect();

        div()
            .id("diff-file-list")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .child(summary)
            .children(file_entries)
    }
}
