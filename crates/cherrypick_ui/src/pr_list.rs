use cherrypick_pr::{LocalPr, PrStatus};
use gpui::{
    AnyElement, App, Context, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    Render, SharedString, Window, div, px,
};
use ui::prelude::*;

pub enum PrListEvent {
    Selected(LocalPr),
}

pub struct PrList {
    focus_handle: FocusHandle,
    prs: Vec<LocalPr>,
}

impl PrList {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            prs: Vec::new(),
        }
    }

    pub fn set_prs(&mut self, prs: Vec<LocalPr>, cx: &mut Context<Self>) {
        self.prs = prs;
        cx.notify();
    }

    fn render_pr_item(&self, pr: &LocalPr, cx: &mut Context<Self>) -> AnyElement {
        let pr_id = pr.id;
        let title = pr.title.clone();
        let source = pr.source_branch.clone();
        let target = pr.target_branch.clone();
        let status = pr.status;

        let status_color = match status {
            PrStatus::Open => cx.theme().colors().version_control_added,
            PrStatus::Merged => cx.theme().colors().version_control_modified,
            PrStatus::Closed => cx.theme().colors().version_control_deleted,
        };

        let pr_clone = pr.clone();
        div()
            .id(SharedString::from(format!("pr-{}", pr_id)))
            .flex()
            .flex_col()
            .gap(px(2.0))
            .px_2()
            .py_1()
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .active(|style| style.bg(cx.theme().colors().ghost_element_active))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                log::info!("cherrypick: PR clicked: id={}", pr_clone.id);
                cx.emit(PrListEvent::Selected(pr_clone.clone()));
            }))
            .child(
                div()
                    .flex()
                    .items_center()
                    .gap_1()
                    .child(
                        div()
                            .w(px(6.0))
                            .h(px(6.0))
                            .rounded_full()
                            .bg(status_color),
                    )
                    .child(
                        div()
                            .text_sm()
                            .overflow_x_hidden()
                            .whitespace_nowrap()
                            .child(title),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .child(format!("{} → {}", source, target)),
            )
            .into_any_element()
    }
}

impl EventEmitter<PrListEvent> for PrList {}

impl Focusable for PrList {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for PrList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        if self.prs.is_empty() {
            return div()
                .px_2()
                .py_1()
                .text_xs()
                .text_color(cx.theme().colors().text_muted)
                .child("No open PRs");
        }

        let header = div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().colors().text_muted)
            .px_2()
            .py_1()
            .child(format!("PRs ({})", self.prs.len()));

        let items: Vec<_> = self.prs.iter().map(|pr| self.render_pr_item(pr, cx)).collect();

        div().flex().flex_col().child(header).children(items)
    }
}
