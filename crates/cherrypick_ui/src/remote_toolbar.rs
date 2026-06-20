use askpass::AskPassDelegate;
use git::repository::FetchOptions;
use gpui::{
    App, Context, Entity, FocusHandle, Focusable, IntoElement, Render, SharedString, WeakEntity,
    Window, div,
};
use project::git_store::Repository;
use ui::prelude::*;
use workspace::Workspace;

pub struct RemoteToolbar {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    repository: Option<Entity<Repository>>,
    ahead: u32,
    behind: u32,
}

impl RemoteToolbar {
    pub fn new(
        workspace: WeakEntity<Workspace>,
        repository: Option<Entity<Repository>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let (ahead, behind) = Self::tracking_counts(&repository, cx);
        Self {
            focus_handle: cx.focus_handle(),
            workspace,
            repository,
            ahead,
            behind,
        }
    }

    pub fn set_repository(
        &mut self,
        repo: Option<Entity<Repository>>,
        cx: &mut Context<Self>,
    ) {
        self.repository = repo;
        let (ahead, behind) = Self::tracking_counts(&self.repository, cx);
        self.ahead = ahead;
        self.behind = behind;
        cx.notify();
    }

    fn tracking_counts(repo: &Option<Entity<Repository>>, cx: &App) -> (u32, u32) {
        repo.as_ref()
            .and_then(|r| {
                r.read(cx)
                    .branch
                    .as_ref()
                    .and_then(|b| b.tracking_status())
                    .map(|s| (s.ahead, s.behind))
            })
            .unwrap_or((0, 0))
    }

    fn noop_askpass(cx: &mut Context<Self>) -> AskPassDelegate {
        AskPassDelegate::new(&mut cx.to_async(), |_prompt, _tx, _cx| {})
    }

    fn do_fetch(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let askpass = Self::noop_askpass(cx);
        window
            .spawn(cx, async move |cx| {
                let rx = repo.update(cx, |repo, cx| {
                    repo.fetch(FetchOptions::All, askpass, cx)
                });
                let _ = rx.await;
                anyhow::Ok(())
            })
            .detach();
    }

    fn do_pull(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let askpass = Self::noop_askpass(cx);
        let branch_name: Option<SharedString> = self
            .repository
            .as_ref()
            .and_then(|r| {
                r.read(cx)
                    .branch
                    .as_ref()
                    .map(|b| SharedString::from(b.name().to_string()))
            });

        window
            .spawn(cx, async move |cx| {
                let rx = repo.update(cx, |repo, cx| {
                    repo.pull(branch_name, "origin".into(), false, askpass, cx)
                });
                let _ = rx.await;
                anyhow::Ok(())
            })
            .detach();
    }

    fn do_push(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let askpass = Self::noop_askpass(cx);
        let branch_name = self
            .repository
            .as_ref()
            .and_then(|r| r.read(cx).branch.as_ref().map(|b| b.name().to_string()))
            .unwrap_or_default();

        window
            .spawn(cx, async move |cx| {
                let rx = repo.update(cx, |repo, cx| {
                    repo.push(
                        SharedString::from(branch_name.clone()),
                        SharedString::from(branch_name),
                        "origin".into(),
                        None,
                        askpass,
                        cx,
                    )
                });
                let _ = rx.await;
                anyhow::Ok(())
            })
            .detach();
    }
}

impl Focusable for RemoteToolbar {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for RemoteToolbar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .flex()
            .items_center()
            .gap_2()
            .px_2()
            .py_1()
            .border_t_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .child(format!("↑{} ↓{}", self.ahead, self.behind)),
            )
            .child(
                div()
                    .id("fetch-btn")
                    .text_xs()
                    .cursor_pointer()
                    .px_2()
                    .py(gpui::px(2.0))
                    .rounded_sm()
                    .bg(cx.theme().colors().ghost_element_background)
                    .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.do_fetch(window, cx);
                    }))
                    .child("Fetch"),
            )
            .child(
                div()
                    .id("pull-btn")
                    .text_xs()
                    .cursor_pointer()
                    .px_2()
                    .py(gpui::px(2.0))
                    .rounded_sm()
                    .bg(cx.theme().colors().ghost_element_background)
                    .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.do_pull(window, cx);
                    }))
                    .child("Pull"),
            )
            .child(
                div()
                    .id("push-btn")
                    .text_xs()
                    .cursor_pointer()
                    .px_2()
                    .py(gpui::px(2.0))
                    .rounded_sm()
                    .bg(cx.theme().colors().ghost_element_background)
                    .hover(|s| s.bg(cx.theme().colors().ghost_element_hover))
                    .on_click(cx.listener(|this, _event, window, cx| {
                        this.do_push(window, cx);
                    }))
                    .child("Push"),
            )
    }
}
