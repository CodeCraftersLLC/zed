use gpui::{
    AnyView, DismissEvent, Entity, EntityId, EventEmitter, FocusHandle, Focusable, ManagedView,
    MouseButton, Subscription,
};
use ui::prelude::*;

#[derive(Debug)]
pub enum DismissDecision {
    Dismiss(bool),
    Pending,
}

pub trait ModalView: ManagedView {
    fn on_before_dismiss(
        &mut self,
        _window: &mut Window,
        _: &mut Context<Self>,
    ) -> DismissDecision {
        DismissDecision::Dismiss(true)
    }

    fn fade_out_background(&self) -> bool {
        false
    }

    fn render_bare(&self) -> bool {
        false
    }
}

trait ModalViewHandle {
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> DismissDecision;
    fn view(&self) -> AnyView;
    fn focus_handle(&self, cx: &App) -> FocusHandle;
    fn fade_out_background(&self, cx: &mut App) -> bool;
    fn render_bare(&self, cx: &mut App) -> bool;
}

impl<V: ModalView> ModalViewHandle for Entity<V> {
    fn on_before_dismiss(&mut self, window: &mut Window, cx: &mut App) -> DismissDecision {
        self.update(cx, |this, cx| this.on_before_dismiss(window, cx))
    }

    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn focus_handle(&self, cx: &App) -> FocusHandle {
        Focusable::focus_handle(self, cx)
    }

    fn fade_out_background(&self, cx: &mut App) -> bool {
        self.read(cx).fade_out_background()
    }

    fn render_bare(&self, cx: &mut App) -> bool {
        self.read(cx).render_bare()
    }
}

pub struct ActiveModal {
    modal: Box<dyn ModalViewHandle>,
    _subscriptions: [Subscription; 2],
    previous_focus_handle: Option<FocusHandle>,
    focus_handle: FocusHandle,
}

pub struct ModalLayer {
    active_modal: Option<ActiveModal>,
    dismiss_on_focus_lost: bool,
}

pub(crate) struct ModalOpenedEvent;

impl EventEmitter<ModalOpenedEvent> for ModalLayer {}

impl Default for ModalLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl ModalLayer {
    pub fn new() -> Self {
        Self {
            active_modal: None,
            dismiss_on_focus_lost: false,
        }
    }

    /// Toggles a modal of type `V`. If a modal of the same type is currently active,
    /// it will be hidden. If a different modal is active, it will be replaced with the new one.
    /// If no modal is active, the new modal will be shown.
    ///
    /// If closing the current modal fails (e.g., due to `on_before_dismiss` returning
    /// `DismissDecision::Dismiss(false)` or `DismissDecision::Pending`), the new modal
    /// will not be shown.
    pub fn toggle_modal<V, B>(&mut self, window: &mut Window, cx: &mut Context<Self>, build_view: B)
    where
        V: ModalView,
        B: FnOnce(&mut Window, &mut Context<V>) -> V,
    {
        if let Some(active_modal) = &self.active_modal {
            let should_close = active_modal.modal.view().downcast::<V>().is_ok();
            let did_close = self.hide_modal(window, cx);
            if should_close || !did_close {
                return;
            }
        }
        let new_modal = cx.new(|cx| build_view(window, cx));
        self.show_modal(new_modal, window, cx);
        cx.emit(ModalOpenedEvent);
    }

    /// Shows a modal and sets up subscriptions for dismiss events and focus tracking.
    /// The modal is automatically focused after being shown.
    fn show_modal<V>(&mut self, new_modal: Entity<V>, window: &mut Window, cx: &mut Context<Self>)
    where
        V: ModalView,
    {
        let focus_handle = cx.focus_handle();
        self.active_modal = Some(ActiveModal {
            modal: Box::new(new_modal.clone()),
            _subscriptions: [
                cx.subscribe_in(
                    &new_modal,
                    window,
                    |this, _, _: &DismissEvent, window, cx| {
                        this.hide_modal(window, cx);
                    },
                ),
                cx.on_focus_out(&focus_handle, window, |this, _event, window, cx| {
                    if this.dismiss_on_focus_lost {
                        this.hide_modal(window, cx);
                    }
                }),
            ],
            previous_focus_handle: window.focused(cx),
            focus_handle,
        });
        let layer = cx.entity().downgrade();
        let modal_id = new_modal.entity_id();
        cx.defer_in(window, move |this, window, cx| {
            // The modal may have been dismissed before this ran.
            if !this.is_active(modal_id) {
                return;
            }
            window.focus(&Focusable::focus_handle(&new_modal, cx), cx);
            // The modal is not in a drawn frame yet, and something else, such as the click that
            // opened it, can still take focus before it is. Claim again on the next frame.
            window.on_next_frame(move |window, cx| {
                layer
                    .update(cx, |layer, cx| layer.reclaim_focus(modal_id, window, cx))
                    .ok();
            });
        });
        cx.notify();
    }

    fn is_active(&self, modal_id: EntityId) -> bool {
        self.active_modal
            .as_ref()
            .is_some_and(|active_modal| active_modal.modal.view().entity_id() == modal_id)
    }

    /// Gives focus back to the modal `modal_id` if it is still the active one and focus has
    /// left it.
    fn reclaim_focus(&mut self, modal_id: EntityId, window: &mut Window, cx: &mut Context<Self>) {
        if !self.is_active(modal_id) {
            return;
        }
        let Some(active_modal) = &self.active_modal else {
            return;
        };
        // A bare modal is drawn without this layer's wrapper, so the layer's own handle holds no
        // focus: the modal's handle contains the focus inside it.
        let modal_focus_handle = active_modal.modal.focus_handle(cx);
        let holder = if active_modal.modal.render_bare(cx) {
            &modal_focus_handle
        } else {
            &active_modal.focus_handle
        };
        if !holder.contains_focused(window, cx) {
            window.focus(&modal_focus_handle, cx);
        }
    }

    /// Attempts to hide the currently active modal.
    ///
    /// The modal's `on_before_dismiss` method is called to determine if dismissal should proceed.
    /// If dismissal is allowed, the modal is removed and focus is restored to the previously
    /// focused element.
    ///
    /// Returns `true` if the modal was successfully hidden, `false` otherwise.
    pub fn hide_modal(&mut self, window: &mut Window, cx: &mut Context<Self>) -> bool {
        let Some(active_modal) = self.active_modal.as_mut() else {
            self.dismiss_on_focus_lost = false;
            return false;
        };

        match active_modal.modal.on_before_dismiss(window, cx) {
            DismissDecision::Dismiss(should_dismiss) => {
                if !should_dismiss {
                    self.dismiss_on_focus_lost = !should_dismiss;
                    return false;
                }
            }
            DismissDecision::Pending => {
                self.dismiss_on_focus_lost = false;
                return false;
            }
        }

        if let Some(active_modal) = self.active_modal.take() {
            if let Some(previous_focus) = active_modal.previous_focus_handle
                && active_modal.focus_handle.contains_focused(window, cx)
            {
                previous_focus.focus(window, cx);
            }
            cx.notify();
        }
        self.dismiss_on_focus_lost = false;
        true
    }

    /// Returns the currently active modal if it is of type `V`.
    pub fn active_modal<V>(&self) -> Option<Entity<V>>
    where
        V: 'static,
    {
        let active_modal = self.active_modal.as_ref()?;
        active_modal.modal.view().downcast::<V>().ok()
    }

    pub fn has_active_modal(&self) -> bool {
        self.active_modal.is_some()
    }
}

impl Render for ModalLayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(active_modal) = &self.active_modal else {
            return div().into_any_element();
        };

        if active_modal.modal.render_bare(cx) {
            return active_modal.modal.view().into_any_element();
        }

        div()
            .absolute()
            .size_full()
            .inset_0()
            .occlude()
            .when(active_modal.modal.fade_out_background(cx), |this| {
                let mut background = cx.theme().colors().elevated_surface_background;
                background.fade_out(0.2);
                this.bg(background)
            })
            .on_mouse_down(
                MouseButton::Left,
                cx.listener(|this, _, window, cx| {
                    this.hide_modal(window, cx);
                }),
            )
            .child(
                v_flex()
                    .h(px(0.0))
                    .top_20()
                    .items_center()
                    .track_focus(&active_modal.focus_handle)
                    .child(
                        h_flex()
                            .occlude()
                            .child(active_modal.modal.view())
                            .on_mouse_down(MouseButton::Left, |_, _, cx| {
                                cx.stop_propagation();
                            }),
                    ),
            )
            .into_any_element()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MultiWorkspace, Workspace, tests::init_test};
    use fs::FakeFs;
    use gpui::{Context, TestAppContext, VisualTestContext, Window};
    use project::Project;

    struct TestModal {
        focus_handle: FocusHandle,
        inner_focus_handle: FocusHandle,
    }

    impl TestModal {
        fn new(cx: &mut Context<Self>) -> Self {
            Self {
                focus_handle: cx.focus_handle(),
                inner_focus_handle: cx.focus_handle(),
            }
        }
    }

    impl Focusable for TestModal {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<DismissEvent> for TestModal {}
    impl ModalView for TestModal {}

    impl Render for TestModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .track_focus(&self.focus_handle)
                .child(div().track_focus(&self.inner_focus_handle).child("modal"))
        }
    }

    async fn workspace_window(
        cx: &mut TestAppContext,
    ) -> (Entity<Workspace>, &mut VisualTestContext) {
        init_test(cx);
        let fs = FakeFs::new(cx.executor());
        let project = Project::test(fs, [], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project, window, cx));
        let workspace = multi_workspace.read_with(cx, |multi, _| multi.workspace().clone());
        (workspace, cx)
    }

    fn open_modal(
        workspace: &Entity<Workspace>,
        cx: &mut VisualTestContext,
    ) -> (Entity<TestModal>, EntityId) {
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| TestModal::new(cx));
        });
        cx.run_until_parked();
        let modal = workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<TestModal>(cx).expect("a modal")
        });
        let id = modal.entity_id();
        (modal, id)
    }

    fn reclaim(workspace: &Entity<Workspace>, modal_id: EntityId, cx: &mut VisualTestContext) {
        let layer = workspace.read_with(cx, |workspace, _| workspace.modal_layer());
        layer.update_in(cx, |layer, window, cx| {
            layer.reclaim_focus(modal_id, window, cx)
        });
    }

    #[gpui::test]
    async fn a_modal_takes_focus_when_it_opens(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_window(cx).await;
        let (modal, _) = open_modal(&workspace, cx);
        let focus_handle = modal.read_with(cx, |modal, _| modal.focus_handle.clone());
        cx.update(|window, cx| assert!(focus_handle.contains_focused(window, cx)));
    }

    #[gpui::test]
    async fn reclaiming_returns_focus_to_a_modal_that_lost_it(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_window(cx).await;
        let outside = cx
            .update(|window, cx| window.focused(cx))
            .expect("something is focused");
        let (modal, modal_id) = open_modal(&workspace, cx);
        let focus_handle = modal.read_with(cx, |modal, _| modal.focus_handle.clone());

        // What the click that opened the modal does after the modal has claimed focus.
        cx.update(|window, cx| outside.focus(window, cx));
        cx.update(|window, cx| assert!(!focus_handle.contains_focused(window, cx)));

        reclaim(&workspace, modal_id, cx);
        cx.update(|window, cx| assert!(focus_handle.contains_focused(window, cx)));
    }

    #[gpui::test]
    async fn reclaiming_leaves_focus_that_is_inside_the_modal_alone(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_window(cx).await;
        let (modal, modal_id) = open_modal(&workspace, cx);
        let inner = modal.read_with(cx, |modal, _| modal.inner_focus_handle.clone());
        cx.update(|window, cx| inner.focus(window, cx));
        cx.update(|window, cx| window.draw(cx).clear());

        reclaim(&workspace, modal_id, cx);
        cx.update(|window, _| assert!(inner.is_focused(window)));
    }

    #[gpui::test]
    async fn a_modal_dismissed_before_it_is_focused_is_never_focused(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_window(cx).await;
        let outside = cx
            .update(|window, cx| window.focused(cx))
            .expect("something is focused");
        let modal_focus = workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| TestModal::new(cx));
            let modal = workspace.active_modal::<TestModal>(cx).expect("a modal");
            let focus_handle = modal.read(cx).focus_handle.clone();
            workspace.hide_modal(window, cx);
            focus_handle
        });
        cx.run_until_parked();

        cx.update(|window, _| {
            assert!(!modal_focus.is_focused(window));
            assert!(outside.is_focused(window), "focus stayed where it was");
        });
    }

    struct BareModal {
        focus_handle: FocusHandle,
        inner_focus_handle: FocusHandle,
    }

    impl Focusable for BareModal {
        fn focus_handle(&self, _: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl EventEmitter<DismissEvent> for BareModal {}

    impl ModalView for BareModal {
        fn render_bare(&self) -> bool {
            true
        }
    }

    impl Render for BareModal {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .track_focus(&self.focus_handle)
                .child(div().track_focus(&self.inner_focus_handle).child("modal"))
        }
    }

    #[gpui::test]
    async fn reclaiming_leaves_focus_inside_a_bare_modal_alone(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_window(cx).await;
        workspace.update_in(cx, |workspace, window, cx| {
            workspace.toggle_modal(window, cx, |_, cx| BareModal {
                focus_handle: cx.focus_handle(),
                inner_focus_handle: cx.focus_handle(),
            });
        });
        cx.run_until_parked();
        let modal = workspace.read_with(cx, |workspace, cx| {
            workspace.active_modal::<BareModal>(cx).expect("a modal")
        });
        let modal_id = modal.entity_id();
        let inner = modal.read_with(cx, |modal, _| modal.inner_focus_handle.clone());
        cx.update(|window, cx| inner.focus(window, cx));
        cx.update(|window, cx| window.draw(cx).clear());

        reclaim(&workspace, modal_id, cx);
        cx.update(|window, _| assert!(inner.is_focused(window)));
    }

    #[gpui::test]
    async fn reclaiming_ignores_a_modal_that_is_gone(cx: &mut TestAppContext) {
        let (workspace, cx) = workspace_window(cx).await;
        let outside = cx
            .update(|window, cx| window.focused(cx))
            .expect("something is focused");
        let (_, first_modal_id) = open_modal(&workspace, cx);
        workspace.update_in(cx, |workspace, window, cx| workspace.hide_modal(window, cx));
        cx.run_until_parked();

        cx.update(|window, cx| outside.focus(window, cx));
        reclaim(&workspace, first_modal_id, cx);
        cx.update(|window, _| assert!(outside.is_focused(window), "focus stayed where it was"));

        // A different modal opened since is not the one that asked.
        let (second, _) = open_modal(&workspace, cx);
        let second_focus = second.read_with(cx, |modal, _| modal.focus_handle.clone());
        cx.update(|window, cx| outside.focus(window, cx));
        reclaim(&workspace, first_modal_id, cx);
        cx.update(|window, cx| assert!(!second_focus.contains_focused(window, cx)));
    }
}
