use cherrypick_pr::{LocalPr, PrStatus};
use gpui::{
    App, Context, FocusHandle, Focusable, FontWeight, IntoElement, Render, SharedString, Window,
    div, px,
};
use ui::prelude::*;

use crate::pr_state::PrState;

pub enum PrAction {
    Close(i64),
    Reopen(i64),
    StatusChanged,
}

pub struct PrDetail {
    focus_handle: FocusHandle,
    pr: Option<LocalPr>,
    conflict_count: Option<usize>,
    on_action: Option<Box<dyn Fn(PrAction, &mut Window, &mut App) + Send + Sync>>,
}

impl PrDetail {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            pr: None,
            conflict_count: None,
            on_action: None,
        }
    }

    pub fn set_pr(&mut self, pr: Option<LocalPr>, cx: &mut Context<Self>) {
        self.pr = pr;
        self.conflict_count = None;
        cx.notify();
    }

    pub fn set_conflict_count(&mut self, count: usize, cx: &mut Context<Self>) {
        self.conflict_count = Some(count);
        cx.notify();
    }

    pub fn on_action(
        &mut self,
        callback: impl Fn(PrAction, &mut Window, &mut App) + Send + Sync + 'static,
    ) {
        self.on_action = Some(Box::new(callback));
    }

    fn close_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pr) = &self.pr else { return };
        let pr_id = pr.id;
        if let Some(cb) = &self.on_action {
            cb(PrAction::Close(pr_id), window, cx);
        }
    }

    fn reopen_pr(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(pr) = &self.pr else { return };
        let pr_id = pr.id;
        if let Some(cb) = &self.on_action {
            cb(PrAction::Reopen(pr_id), window, cx);
        }
    }
}

impl Focusable for PrDetail {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PrDetail {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(pr) = &self.pr else {
            return div();
        };

        let status_color = match pr.status {
            PrStatus::Open => cx.theme().colors().version_control_added,
            PrStatus::Merged => cx.theme().colors().version_control_modified,
            PrStatus::Closed => cx.theme().colors().version_control_deleted,
        };

        let is_open = pr.status == PrStatus::Open;
        let is_closed = pr.status == PrStatus::Closed;
        let conflict_count = self.conflict_count;

        let mut root = div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().colors().border);

        root = root.child(
            div()
                .flex()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(pr.title.clone()),
                )
                .child(
                    div()
                        .text_xs()
                        .px_1()
                        .py(px(1.0))
                        .rounded_sm()
                        .text_color(status_color)
                        .border_1()
                        .border_color(status_color)
                        .child(pr.status.as_str()),
                ),
        );

        root = root.child(
            div()
                .text_xs()
                .text_color(cx.theme().colors().text_muted)
                .child(format!("{} → {}", pr.source_branch, pr.target_branch)),
        );

        if let Some(count) = conflict_count {
            if count > 0 {
                root = root.child(
                    div()
                        .text_xs()
                        .text_color(cx.theme().colors().version_control_conflict)
                        .child(format!("{} conflicts", count)),
                );
            }
        }

        let mut actions = div().flex().items_center().gap_1().pt_1();

        if is_open {
            actions = actions.child(
                div()
                    .id("close-pr-btn")
                    .text_xs()
                    .cursor_pointer()
                    .px_2()
                    .py(px(2.0))
                    .rounded_sm()
                    .bg(cx.theme().colors().ghost_element_background)
                    .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.close_pr(window, cx);
                    }))
                    .child("Close PR"),
            );
        }

        if is_closed {
            actions = actions.child(
                div()
                    .id("reopen-pr-btn")
                    .text_xs()
                    .cursor_pointer()
                    .px_2()
                    .py(px(2.0))
                    .rounded_sm()
                    .bg(cx.theme().colors().ghost_element_background)
                    .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.reopen_pr(window, cx);
                    }))
                    .child("Reopen PR"),
            );
        }

        root = root.child(actions);
        root
    }
}
