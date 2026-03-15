use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, Window, div, px,
};
use ui::prelude::*;

pub struct BranchList {
    focus_handle: FocusHandle,
}

impl BranchList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Focusable for BranchList {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BranchList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .flex_col()
            .size_full()
            .p_1()
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors().text_muted)
                    .child("Branches"),
            )
    }
}
