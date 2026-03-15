use gpui::{
    App, Context, EventEmitter, FocusHandle, Focusable, IntoElement, Render,
    SharedString, WeakEntity, Window, div,
};
use ui::prelude::*;
use workspace::{Item, Workspace, item::ItemEvent};

pub enum CherryPickViewEvent {
    UpdateTab,
}

pub struct CherryPickView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
}

impl CherryPickView {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        cx: &mut Context<Self>,
    ) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
        }
    }
}

impl Focusable for CherryPickView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<CherryPickViewEvent> for CherryPickView {}

impl Render for CherryPickView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .key_context("CherryPickView")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(
                div()
                    .flex_grow()
                    .p_4()
                    .child(
                        div()
                            .text_color(cx.theme().colors().text_muted)
                            .child("Commit Graph (placeholder)"),
                    ),
            )
            .child(
                div()
                    .flex_grow()
                    .p_4()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        div()
                            .text_color(cx.theme().colors().text_muted)
                            .child("Staging Area (placeholder)"),
                    ),
            )
    }
}

impl Item for CherryPickView {
    type Event = CherryPickViewEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        "CherryPick".into()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<SharedString> {
        Some("CherryPick Git Client".into())
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        match event {
            CherryPickViewEvent::UpdateTab => f(ItemEvent::UpdateTab),
        }
    }
}
