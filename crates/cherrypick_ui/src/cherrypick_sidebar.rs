use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, IntoElement, Pixels,
    Render, WeakEntity, Window, actions, div, px,
};
use ui::prelude::*;
use workspace::{
    dock::DockPosition, dock::Panel, dock::PanelEvent, Workspace,
};

actions!(cherrypick, [ToggleSidebar]);

const CHERRYPICK_SIDEBAR_KEY: &str = "CherryPickSidebar";
const DEFAULT_WIDTH: f32 = 250.0;

pub struct CherryPickSidebar {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    width: Option<Pixels>,
}

impl CherryPickSidebar {
    pub fn new(
        workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.weak_handle(),
            width: None,
        })
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleSidebar, window, cx| {
        workspace.toggle_panel_focus::<CherryPickSidebar>(window, cx);
    });
}

impl Focusable for CherryPickSidebar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<PanelEvent> for CherryPickSidebar {}

impl Render for CherryPickSidebar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div().flex().flex_col()
            .key_context("CherryPickSidebar")
            .track_focus(&self.focus_handle)
            .size_full()
            .p_2()
            .child(
                div()
                    .text_color(cx.theme().colors().text_muted)
                    .child("CherryPick"),
            )
    }
}

impl Panel for CherryPickSidebar {
    fn persistent_name() -> &'static str {
        CHERRYPICK_SIDEBAR_KEY
    }

    fn panel_key() -> &'static str {
        CHERRYPICK_SIDEBAR_KEY
    }

    fn position(&self, _window: &Window, _cx: &App) -> DockPosition {
        DockPosition::Right
    }

    fn position_is_valid(&self, position: DockPosition) -> bool {
        matches!(position, DockPosition::Left | DockPosition::Right)
    }

    fn set_position(
        &mut self,
        _position: DockPosition,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        cx.notify();
    }

    fn size(&self, _window: &Window, _cx: &App) -> Pixels {
        self.width.unwrap_or(px(DEFAULT_WIDTH))
    }

    fn set_size(&mut self, size: Option<Pixels>, _window: &mut Window, cx: &mut Context<Self>) {
        self.width = size;
        cx.notify();
    }

    fn icon(&self, _window: &Window, _cx: &App) -> Option<ui::IconName> {
        Some(ui::IconName::GitBranchAlt)
    }

    fn icon_tooltip(&self, _window: &Window, _cx: &App) -> Option<&'static str> {
        Some("CherryPick Sidebar")
    }

    fn toggle_action(&self) -> Box<dyn Action> {
        Box::new(ToggleSidebar)
    }

    fn activation_priority(&self) -> u32 {
        3
    }
}
