use cherrypick_pr::diff_service::BranchDiff;
use cherrypick_pr::LocalPr;
use gpui::{
    App, Context, Entity, EventEmitter, FocusHandle, Focusable, FontWeight, IntoElement,
    SharedString, Subscription, WeakEntity, Window, div,
};
use project::Project;
use project::git_store::{GitStoreEvent, Repository, RepositoryEvent};
use ui::prelude::*;
use workspace::{Item, Workspace, item::ItemEvent};

use crate::diff_file_list::DiffFileList;
use crate::pr_state::PrState;
use crate::staging_view::StagingView;

pub enum ViewMode {
    Staging,
    PrDetail(LocalPr),
    BranchCompare {
        source_branch: String,
        target_branch: String,
        source_oid: String,
        target_oid: String,
    },
}

pub enum CherryPickViewEvent {
    UpdateTab,
}

pub struct CherryPickView {
    focus_handle: FocusHandle,
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    active_repository: Option<Entity<Repository>>,
    mode: ViewMode,
    staging_view: Entity<StagingView>,
    diff_file_list: Entity<DiffFileList>,
    unified_diff_text: Option<String>,
    pr_state: PrState,
    _subscriptions: Vec<Subscription>,
}

impl CherryPickView {
    pub fn new(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let project = workspace.project().clone();
        let git_store = project.read(cx).git_store().clone();
        let active_repository = project.read(cx).active_repository(cx);

        let staging_view = cx.new(|cx| StagingView::new(active_repository.clone(), cx));
        let diff_file_list = cx.new(|cx| DiffFileList::new(cx));

        let mut pr_state = PrState::new(cx);
        if let Some(repo) = &active_repository {
            let path = repo.read(cx).work_directory_abs_path.clone();
            pr_state.set_repo_path(path.to_path_buf());
            pr_state.initialize().detach();
        }

        let git_sub = cx.subscribe_in(
            &git_store,
            window,
            |this: &mut Self, _git_store, event, _window, cx| match event {
                GitStoreEvent::ActiveRepositoryChanged(_) => {
                    this.update_active_repository(cx);
                }
                GitStoreEvent::RepositoryUpdated(
                    _,
                    RepositoryEvent::StatusesChanged | RepositoryEvent::BranchChanged,
                    true,
                ) => {
                    this.refresh(cx);
                }
                _ => {}
            },
        );

        Self {
            focus_handle: cx.focus_handle(),
            workspace: workspace.weak_handle(),
            project,
            active_repository,
            mode: ViewMode::Staging,
            staging_view,
            diff_file_list,
            unified_diff_text: None,
            pr_state,
            _subscriptions: vec![git_sub],
        }
    }

    pub fn deploy(
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let existing = workspace
            .active_pane()
            .read(cx)
            .items()
            .find_map(|item| item.downcast::<CherryPickView>());

        if let Some(existing) = existing {
            workspace.activate_item(&existing, true, true, window, cx);
        } else {
            let view = cx.new(|cx| CherryPickView::new(workspace, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        }
    }

    pub fn deploy_with_pr(
        workspace: &mut Workspace,
        pr: LocalPr,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let view = cx.new(|cx| {
            let mut v = CherryPickView::new(workspace, window, cx);
            v.show_pr(pr, cx);
            v
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    pub fn deploy_compare(
        workspace: &mut Workspace,
        source_branch: String,
        target_branch: String,
        source_oid: String,
        target_oid: String,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let view = cx.new(|cx| {
            let mut v = CherryPickView::new(workspace, window, cx);
            v.show_compare(source_branch, target_branch, source_oid, target_oid, cx);
            v
        });
        workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
    }

    fn show_pr(&mut self, pr: LocalPr, cx: &mut Context<Self>) {
        let source_oid = pr.source_oid.clone();
        let target_oid = pr.target_oid.clone();
        self.mode = ViewMode::PrDetail(pr);
        self.load_diff(source_oid, target_oid, cx);
        cx.emit(CherryPickViewEvent::UpdateTab);
        cx.notify();
    }

    fn show_compare(
        &mut self,
        source_branch: String,
        target_branch: String,
        source_oid: String,
        target_oid: String,
        cx: &mut Context<Self>,
    ) {
        self.mode = ViewMode::BranchCompare {
            source_branch,
            target_branch,
            source_oid: source_oid.clone(),
            target_oid: target_oid.clone(),
        };
        self.load_diff(source_oid, target_oid, cx);
        cx.emit(CherryPickViewEvent::UpdateTab);
        cx.notify();
    }

    fn load_diff(&mut self, source_oid: String, target_oid: String, cx: &mut Context<Self>) {
        let file_task = self.pr_state.get_branch_diff(source_oid.clone(), target_oid.clone());
        let text_task = self.pr_state.get_unified_diff(source_oid, target_oid);
        let diff_list = self.diff_file_list.clone();

        cx.spawn(async move |this, cx| {
            if let Ok(diff) = file_task.await {
                let _ = diff_list.update(cx, |list, cx| {
                    list.set_diff(Some(diff), cx);
                });
            }
            if let Ok(text) = text_task.await {
                let _ = this.update(cx, |this, cx| {
                    this.unified_diff_text = Some(text);
                    cx.notify();
                });
            }
        })
        .detach();
    }

    fn update_active_repository(&mut self, cx: &mut Context<Self>) {
        self.active_repository = self.project.read(cx).active_repository(cx);
        self.staging_view.update(cx, |view, cx| {
            view.set_repository(self.active_repository.clone(), cx);
        });

        if let Some(repo) = &self.active_repository {
            let path = repo.read(cx).work_directory_abs_path.clone();
            self.pr_state.set_repo_path(path.to_path_buf());
            self.pr_state.initialize().detach();
        }
        cx.notify();
    }

    fn refresh(&mut self, cx: &mut Context<Self>) {
        self.staging_view.update(cx, |view, cx| {
            view.set_repository(self.active_repository.clone(), cx);
        });
        cx.notify();
    }

    fn tab_title(&self) -> String {
        match &self.mode {
            ViewMode::Staging => "CherryPick".to_string(),
            ViewMode::PrDetail(pr) => format!("PR: {}", pr.title),
            ViewMode::BranchCompare {
                source_branch,
                target_branch,
                ..
            } => format!("{} → {}", source_branch, target_branch),
        }
    }

    fn render_header(&self, cx: &App) -> gpui::Div {
        match &self.mode {
            ViewMode::Staging => self.render_staging_header(cx),
            ViewMode::PrDetail(pr) => self.render_pr_header(pr, cx),
            ViewMode::BranchCompare {
                source_branch,
                target_branch,
                ..
            } => self.render_compare_header(source_branch, target_branch, cx),
        }
    }

    fn render_staging_header(&self, cx: &App) -> gpui::Div {
        let Some(repo) = self.active_repository.as_ref() else {
            return div()
                .p_2()
                .text_color(cx.theme().colors().text_muted)
                .child("No repository");
        };

        let snapshot = repo.read(cx);
        let branch_name = snapshot
            .branch
            .as_ref()
            .map(|b| b.name().to_string())
            .unwrap_or_else(|| "detached HEAD".to_string());

        let head_info = snapshot
            .head_commit
            .as_ref()
            .map(|c| {
                format!(
                    "{} {}",
                    &c.sha[..7.min(c.sha.len())],
                    c.message.lines().next().unwrap_or("")
                )
            })
            .unwrap_or_else(|| "no commits".to_string());

        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child(branch_name),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .child(head_info),
            )
    }

    fn render_pr_header(&self, pr: &LocalPr, cx: &App) -> gpui::Div {
        let status_label = pr.status.as_str();
        let status_color = match pr.status {
            cherrypick_pr::PrStatus::Open => cx.theme().colors().version_control_added,
            cherrypick_pr::PrStatus::Merged => cx.theme().colors().version_control_modified,
            cherrypick_pr::PrStatus::Closed => cx.theme().colors().version_control_deleted,
        };

        div()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
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
                            .rounded_sm()
                            .text_color(status_color)
                            .child(status_label),
                    ),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .child(format!(
                        "{} → {}",
                        pr.source_branch, pr.target_branch
                    )),
            )
    }

    fn render_compare_header(
        &self,
        source: &str,
        target: &str,
        cx: &App,
    ) -> gpui::Div {
        div()
            .flex()
            .items_center()
            .gap_2()
            .p_2()
            .border_b_1()
            .border_color(cx.theme().colors().border)
            .child(
                div()
                    .text_sm()
                    .font_weight(FontWeight::SEMIBOLD)
                    .child("Comparing"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors().text_accent)
                    .child(source.to_string()),
            )
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .child("→"),
            )
            .child(
                div()
                    .text_sm()
                    .text_color(cx.theme().colors().text_accent)
                    .child(target.to_string()),
            )
    }

    fn render_unified_diff(&self, cx: &mut Context<Self>) -> gpui::Stateful<gpui::Div> {
        let Some(diff_text) = &self.unified_diff_text else {
            return div()
                .id("unified-diff-empty")
                .p_2()
                .text_xs()
                .text_color(cx.theme().colors().text_muted)
                .child("Loading diff...");
        };

        let added_color = cx.theme().colors().version_control_added;
        let deleted_color = cx.theme().colors().version_control_deleted;
        let muted = cx.theme().colors().text_muted;
        let text_color = cx.theme().colors().text;

        let mut container = div()
            .id("unified-diff")
            .flex()
            .flex_col()
            .flex_grow()
            .overflow_y_scroll()
            .p_1()
            .text_xs()
            .font_family("monospace");

        for line in diff_text.lines() {
            let color = if line.starts_with('+') && !line.starts_with("+++") {
                added_color
            } else if line.starts_with('-') && !line.starts_with("---") {
                deleted_color
            } else if line.starts_with("@@") {
                muted
            } else if line.starts_with("diff ") || line.starts_with("index ") {
                muted
            } else {
                text_color
            };

            let bg = if line.starts_with('+') && !line.starts_with("+++") {
                Some(cx.theme().colors().version_control_added.opacity(0.1))
            } else if line.starts_with('-') && !line.starts_with("---") {
                Some(cx.theme().colors().version_control_deleted.opacity(0.1))
            } else {
                None
            };

            let mut line_div = div()
                .text_color(color)
                .px_1()
                .child(line.to_string());

            if let Some(bg_color) = bg {
                line_div = line_div.bg(bg_color);
            }

            container = container.child(line_div);
        }

        container
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
        let header = self.render_header(cx);

        let mut root = div()
            .key_context("CherryPickView")
            .track_focus(&self.focus_handle)
            .size_full()
            .flex()
            .flex_col()
            .child(header);

        match &self.mode {
            ViewMode::Staging => {
                root = root.child(
                    div().flex_grow().child(self.staging_view.clone()),
                );
            }
            ViewMode::PrDetail(_) | ViewMode::BranchCompare { .. } => {
                root = root.child(self.diff_file_list.clone());
                root = root.child(self.render_unified_diff(cx));
            }
        }

        root
    }
}

impl Item for CherryPickView {
    type Event = CherryPickViewEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        SharedString::from(self.tab_title())
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
