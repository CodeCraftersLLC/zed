use cherrypick_pr::LocalPr;
use gpui::{
    Action, App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    Pixels, Render, SharedString, Subscription, WeakEntity, Window, actions, div, px,
};
use project::Project;
use project::git_store::{GitStoreEvent, Repository, RepositoryEvent};
use ui::prelude::*;
use workspace::{
    Workspace,
    dock::{DockPosition, Panel, PanelEvent},
};

use crate::branch_list::BranchList;
use crate::cherrypick_view::CherryPickView;
use crate::create_pr_form::{CreatePrForm, CreatePrFormEvent};
use crate::pr_list::{PrList, PrListEvent};
use crate::pr_state::PrState;

actions!(cherrypick, [ToggleSidebar, OpenCherryPick]);

const CHERRYPICK_SIDEBAR_KEY: &str = "CherryPickSidebar";
const DEFAULT_WIDTH: f32 = 260.0;

pub struct CherryPickSidebar {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_repository: Option<Entity<Repository>>,
    branch_list: Entity<BranchList>,
    pr_list: Entity<PrList>,
    create_pr_form: Entity<CreatePrForm>,
    pr_state: PrState,
    width: Option<Pixels>,
    _subscriptions: Vec<Subscription>,
}

impl CherryPickSidebar {
    pub async fn load(
        workspace: gpui::WeakEntity<Workspace>,
        mut cx: gpui::AsyncWindowContext,
    ) -> anyhow::Result<Entity<Self>> {
        workspace.update_in(&mut cx, |workspace, window, cx| {
            CherryPickSidebar::new(workspace, window, cx)
        })
    }

    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let active_repository = project.read(cx).active_repository(cx);

        let weak_workspace = workspace.weak_handle();

        cx.new(|cx| {
            let branch_list = cx.new(|cx| BranchList::new(active_repository.clone(), cx));
            let pr_list = cx.new(|cx| PrList::new(cx));
            let create_pr_form = cx.new(|cx| CreatePrForm::new(cx));

            let mut pr_state = PrState::new(cx);
            if let Some(repo) = &active_repository {
                let path = repo.read(cx).work_directory_abs_path.clone();
                pr_state.set_repo_path(path.to_path_buf());
                pr_state.initialize().detach();
            }

            let git_sub = cx.subscribe_in(
                &git_store,
                window,
                |this: &mut Self, _git_store, event, window, cx| match event {
                    GitStoreEvent::ActiveRepositoryChanged(_) => {
                        this.update_active_repository(window, cx);
                    }
                    GitStoreEvent::RepositoryUpdated(
                        _,
                        RepositoryEvent::BranchChanged | RepositoryEvent::StatusesChanged,
                        true,
                    ) => {
                        this.refresh_branches(window, cx);
                        this.refresh_prs(cx);
                    }
                    GitStoreEvent::RepositoryAdded | GitStoreEvent::RepositoryRemoved(_) => {
                        this.update_active_repository(window, cx);
                    }
                    _ => {}
                },
            );

            let form_sub = cx.subscribe(
                &create_pr_form,
                |this: &mut Self, _form, event: &CreatePrFormEvent, cx| match event {
                    CreatePrFormEvent::Submit {
                        title,
                        source_branch,
                        target_branch,
                    } => {
                        this.handle_create_pr(
                            title.clone(),
                            source_branch.clone(),
                            target_branch.clone(),
                            cx,
                        );
                    }
                },
            );

            let pr_list_sub = cx.subscribe_in(
                &pr_list,
                window,
                |this: &mut Self, _list, event: &PrListEvent, window, cx| match event {
                    PrListEvent::Selected(pr) => {
                        this.open_pr_view(pr.clone(), window, cx);
                    }
                },
            );

            let mut sidebar = Self {
                focus_handle: cx.focus_handle(),
                workspace: weak_workspace,
                project,
                active_repository,
                branch_list,
                pr_list,
                create_pr_form,
                pr_state,
                width: None,
                _subscriptions: vec![git_sub, form_sub, pr_list_sub],
            };

            sidebar.refresh_prs(cx);
            sidebar
        })
    }

    fn update_active_repository(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.active_repository = self.project.read(cx).active_repository(cx);
        self.branch_list.update(cx, |list, cx| {
            list.set_repository(self.active_repository.clone(), cx);
        });

        if let Some(repo) = &self.active_repository {
            let path = repo.read(cx).work_directory_abs_path.clone();
            self.pr_state.set_repo_path(path.to_path_buf());
            self.pr_state.initialize().detach();
        }

        self.refresh_branches(window, cx);
        self.refresh_prs(cx);
    }

    fn refresh_branches(&mut self, _window: &mut Window, cx: &mut Context<Self>) {
        let Some(repo) = self.active_repository.clone() else {
            self.branch_list.update(cx, |list, cx| {
                list.set_branches(Vec::new(), cx);
            });
            return;
        };

        let rx = repo.update(cx, |repo, _cx| repo.branches());
        let branch_list = self.branch_list.clone();
        let create_form = self.create_pr_form.clone();
        let current_branch = self.current_branch_name(cx);
        cx.spawn(async move |_this, cx| {
            if let Ok(Ok(branches)) = rx.await {
                let mut names = Vec::new();
                let _ = branch_list.update(cx, |list, cx| {
                    list.set_branches(branches, cx);
                    names = list.local_branch_names();
                });
                if !names.is_empty() {
                    let _ = create_form.update(cx, |form, cx| {
                        form.set_branches(names, current_branch.as_deref(), cx);
                    });
                }
            }
        })
        .detach();
    }

    fn refresh_prs(&mut self, cx: &mut Context<Self>) {
        if !self.pr_state.is_initialized() {
            return;
        }

        let task = self.pr_state.list_open_prs();
        let pr_list = self.pr_list.clone();
        cx.spawn(async move |_this, cx| {
            if let Ok(prs) = task.await {
                let _ = pr_list.update(cx, |list, cx| {
                    list.set_prs(prs, cx);
                });
            }
        })
        .detach();
    }

    fn toggle_create_form(&mut self, cx: &mut Context<Self>) {
        self.create_pr_form.update(cx, |form, cx| {
            form.toggle_visible(cx);
        });
    }

    fn handle_create_pr(&mut self, title: String, source: String, target: String, cx: &mut Context<Self>) {
        log::info!("cherrypick: handle_create_pr called: '{}' {} → {}", title, source, target);

        if !self.pr_state.is_initialized() {
            log::error!("cherrypick: PR state not initialized, attempting init");
            self.pr_state.initialize().detach();
            return;
        }

        let task = self.pr_state.create_pr(title.clone(), source.clone(), target.clone());
        let pr_list = self.pr_list.clone();
        let create_form = self.create_pr_form.clone();

        cx.spawn(async move |this, cx| {
            log::info!("cherrypick: creating PR async...");
            match task.await {
                Ok(pr) => {
                    log::info!("cherrypick: PR created successfully: id={}, '{}'", pr.id, pr.title);
                    let _ = create_form.update(cx, |form, cx| {
                        form.set_visible(false, cx);
                    });
                    let _ = this.update(cx, |this, cx| {
                        this.refresh_prs(cx);
                    });
                }
                Err(e) => {
                    log::error!("cherrypick: failed to create PR: {}", e);
                }
            }
        })
        .detach();
    }

    fn open_pr_view(&mut self, pr: LocalPr, window: &mut Window, cx: &mut Context<Self>) {
        log::info!("cherrypick: opening PR view for: {}", pr.title);
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            CherryPickView::deploy_with_pr(workspace, pr, window, cx);
        });
    }

    fn open_cherrypick_view(&self, window: &mut Window, cx: &mut Context<Self>) {
        let workspace = self.workspace.clone();
        let _ = workspace.update(cx, |workspace, cx| {
            CherryPickView::deploy(workspace, window, cx);
        });
    }

    fn current_branch_name(&self, cx: &App) -> Option<String> {
        self.active_repository.as_ref().and_then(|repo| {
            repo.read(cx).branch.as_ref().map(|b| b.name().to_string())
        })
    }
}

pub fn register(workspace: &mut Workspace) {
    workspace.register_action(|workspace, _: &ToggleSidebar, window, cx| {
        workspace.toggle_panel_focus::<CherryPickSidebar>(window, cx);
    });
    workspace.register_action(|workspace, _: &OpenCherryPick, window, cx| {
        CherryPickView::deploy(workspace, window, cx);
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
        let current_branch = self.current_branch_name(cx);

        div()
            .flex()
            .flex_col()
            .key_context("CherryPickSidebar")
            .track_focus(&self.focus_handle)
            .size_full()
            .child(
                div()
                    .flex()
                    .items_center()
                    .justify_between()
                    .px_2()
                    .py_1()
                    .border_b_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        div()
                            .text_sm()
                            .font_weight(FontWeight::SEMIBOLD)
                            .child("CherryPick"),
                    )
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .gap_1()
                            .child(
                                div()
                                    .text_xs()
                                    .text_color(cx.theme().colors().text_muted)
                                    .child(
                                        current_branch
                                            .unwrap_or_else(|| "no branch".to_string()),
                                    ),
                            )
                            .child(
                                div()
                                    .id("open-view-btn")
                                    .text_xs()
                                    .cursor_pointer()
                                    .px_1()
                                    .rounded_sm()
                                    .bg(cx.theme().colors().ghost_element_background)
                                    .hover(|s| {
                                        s.bg(cx.theme().colors().ghost_element_hover)
                                    })
                                    .on_click(cx.listener(|this, _event, window, cx| {
                                        this.open_cherrypick_view(window, cx);
                                    }))
                                    .child("Open"),
                            ),
                    ),
            )
            .child(self.create_pr_form.clone())
            .child(self.branch_list.clone())
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().colors().border)
                    .child(
                        div()
                            .flex()
                            .items_center()
                            .justify_between()
                            .px_2()
                            .py_1()
                            .child(self.pr_list.clone())
                            .child(
                                div()
                                    .id("new-pr-btn")
                                    .text_xs()
                                    .cursor_pointer()
                                    .px_1()
                                    .rounded_sm()
                                    .text_color(cx.theme().colors().text_accent)
                                    .hover(|s| {
                                        s.bg(cx.theme().colors().ghost_element_hover)
                                    })
                                    .on_click(cx.listener(|this, _event, _window, cx| {
                                        this.toggle_create_form(cx);
                                    }))
                                    .child("+ New PR"),
                            ),
                    ),
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
