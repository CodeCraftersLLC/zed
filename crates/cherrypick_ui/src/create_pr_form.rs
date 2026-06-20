use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    SharedString, Window, div, px,
};
use ui::prelude::*;

pub enum CreatePrFormEvent {
    Submit {
        title: String,
        source_branch: String,
        target_branch: String,
    },
}

pub struct CreatePrForm {
    focus_handle: FocusHandle,
    visible: bool,
    branch_names: Vec<String>,
    selected_source: usize,
    selected_target: usize,
    title: String,
}

impl CreatePrForm {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            visible: false,
            branch_names: Vec::new(),
            selected_source: 0,
            selected_target: 0,
            title: String::new(),
        }
    }

    pub fn toggle_visible(&mut self, cx: &mut Context<Self>) {
        self.visible = !self.visible;
        cx.notify();
    }

    pub fn set_visible(&mut self, visible: bool, cx: &mut Context<Self>) {
        self.visible = visible;
        cx.notify();
    }

    pub fn set_branches(&mut self, names: Vec<String>, current_branch: Option<&str>, cx: &mut Context<Self>) {
        self.branch_names = names;
        if self.branch_names.is_empty() {
            cx.notify();
            return;
        }

        self.selected_source = 0;
        self.selected_target = 0;

        if let Some(current) = current_branch {
            if let Some(idx) = self.branch_names.iter().position(|b| b == current) {
                self.selected_source = idx;
            }
        }

        if let Some(idx) = self.branch_names.iter().position(|b| b == "main") {
            self.selected_target = idx;
        } else if let Some(idx) = self.branch_names.iter().position(|b| b == "master") {
            self.selected_target = idx;
        }

        if self.selected_source == self.selected_target && self.branch_names.len() > 1 {
            for (i, name) in self.branch_names.iter().enumerate() {
                if i != self.selected_source {
                    self.selected_target = i;
                    break;
                }
            }
        }

        self.title = self.branch_names.get(self.selected_source)
            .cloned()
            .unwrap_or_default();

        cx.notify();
    }

    fn source_name(&self) -> &str {
        self.branch_names
            .get(self.selected_source)
            .map(|s| s.as_str())
            .unwrap_or("(none)")
    }

    fn target_name(&self) -> &str {
        self.branch_names
            .get(self.selected_target)
            .map(|s| s.as_str())
            .unwrap_or("(none)")
    }

    fn cycle_source(&mut self, cx: &mut Context<Self>) {
        if !self.branch_names.is_empty() {
            self.selected_source = (self.selected_source + 1) % self.branch_names.len();
            cx.notify();
        }
    }

    fn cycle_target(&mut self, cx: &mut Context<Self>) {
        if !self.branch_names.is_empty() {
            self.selected_target = (self.selected_target + 1) % self.branch_names.len();
            cx.notify();
        }
    }

    fn submit(&mut self, cx: &mut Context<Self>) {
        let Some(source) = self.branch_names.get(self.selected_source).cloned() else {
            log::warn!("cherrypick: no source branch selected");
            return;
        };
        let Some(target) = self.branch_names.get(self.selected_target).cloned() else {
            log::warn!("cherrypick: no target branch selected");
            return;
        };
        if source == target {
            log::warn!("cherrypick: source and target are the same: {}", source);
            return;
        }
        let title = if self.title.is_empty() {
            source.clone()
        } else {
            self.title.clone()
        };
        log::info!("cherrypick: submitting PR '{}': {} → {}", title, source, target);
        cx.emit(CreatePrFormEvent::Submit {
            title,
            source_branch: source,
            target_branch: target,
        });
    }
}

impl EventEmitter<CreatePrFormEvent> for CreatePrForm {}

impl Focusable for CreatePrForm {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for CreatePrForm {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if !self.visible {
            return div();
        }

        let source = self.source_name().to_string();
        let target = self.target_name().to_string();

        div()
            .flex()
            .flex_col()
            .gap_1()
            .px_2()
            .py_2()
            .bg(cx.theme().colors().surface_background)
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().colors().text_muted)
                    .child("Create Local PR"),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div().text_xs().text_color(cx.theme().colors().text_muted).child("From:"),
                    )
                    .child(
                        div()
                            .id("source-branch-picker")
                            .text_xs()
                            .cursor_pointer()
                            .px_1()
                            .py(px(2.0))
                            .rounded_sm()
                            .bg(cx.theme().colors().ghost_element_background)
                            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.cycle_source(cx);
                            }))
                            .child(source),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div().text_xs().text_color(cx.theme().colors().text_muted).child("Into:"),
                    )
                    .child(
                        div()
                            .id("target-branch-picker")
                            .text_xs()
                            .cursor_pointer()
                            .px_1()
                            .py(px(2.0))
                            .rounded_sm()
                            .bg(cx.theme().colors().ghost_element_background)
                            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.cycle_target(cx);
                            }))
                            .child(target),
                    ),
            )
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .pt_1()
                    .child(
                        div()
                            .id("create-pr-submit")
                            .text_xs()
                            .cursor_pointer()
                            .px_2()
                            .py(px(3.0))
                            .rounded_sm()
                            .bg(cx.theme().colors().element_background)
                            .hover(|s| s.bg(cx.theme().colors().element_hover))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.submit(cx);
                            }))
                            .child("Create PR"),
                    )
                    .child(
                        div()
                            .id("create-pr-cancel")
                            .text_xs()
                            .cursor_pointer()
                            .px_2()
                            .py(px(3.0))
                            .rounded_sm()
                            .text_color(cx.theme().colors().text_muted)
                            .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                            .on_click(cx.listener(|this, _event, _window, cx| {
                                this.set_visible(false, cx);
                            }))
                            .child("Cancel"),
                    ),
            )
    }
}
