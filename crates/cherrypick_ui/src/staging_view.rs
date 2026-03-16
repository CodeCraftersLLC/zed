use git::repository::RepoPath;
use git::status::{FileStatus, StatusCode, TrackedStatus};
use gpui::{
    AnyElement, App, Context, Entity, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    SharedString, Subscription, Window, div, px,
};
use project::git_store::{Repository, StatusEntry};
use ui::prelude::*;

struct StagingEntry {
    repo_path: RepoPath,
    status: FileStatus,
    is_staged: bool,
}

impl StagingEntry {
    fn status_char(&self) -> char {
        match self.status {
            FileStatus::Untracked => '?',
            FileStatus::Ignored => '!',
            FileStatus::Unmerged { .. } => 'U',
            FileStatus::Tracked(TrackedStatus {
                index_status,
                worktree_status,
            }) => {
                let code = if self.is_staged {
                    index_status
                } else {
                    worktree_status
                };
                match code {
                    StatusCode::Modified => 'M',
                    StatusCode::Added => 'A',
                    StatusCode::Deleted => 'D',
                    StatusCode::Renamed => 'R',
                    StatusCode::Copied => 'C',
                    StatusCode::TypeChanged => 'T',
                    StatusCode::Unmodified => ' ',
                }
            }
        }
    }
}

pub struct StagingView {
    focus_handle: FocusHandle,
    repository: Option<Entity<Repository>>,
    staged: Vec<StagingEntry>,
    unstaged: Vec<StagingEntry>,
    commit_message: String,
    _subscriptions: Vec<Subscription>,
}

impl StagingView {
    pub fn new(
        repository: Option<Entity<Repository>>,
        cx: &mut Context<Self>,
    ) -> Self {
        let mut view = Self {
            focus_handle: cx.focus_handle(),
            repository: repository.clone(),
            staged: Vec::new(),
            unstaged: Vec::new(),
            commit_message: String::new(),
            _subscriptions: Vec::new(),
        };
        view.refresh_statuses(cx);
        view
    }

    pub fn set_repository(&mut self, repo: Option<Entity<Repository>>, cx: &mut Context<Self>) {
        self.repository = repo;
        self.refresh_statuses(cx);
    }

    fn refresh_statuses(&mut self, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.as_ref() else {
            self.staged.clear();
            self.unstaged.clear();
            cx.notify();
            return;
        };

        self.staged.clear();
        self.unstaged.clear();

        let snapshot = repo.read(cx);
        for entry in snapshot.status() {
            let staging = entry.status.staging();
            match staging {
                git::status::StageStatus::Staged => {
                    self.staged.push(StagingEntry {
                        repo_path: entry.repo_path,
                        status: entry.status,
                        is_staged: true,
                    });
                }
                git::status::StageStatus::Unstaged => {
                    self.unstaged.push(StagingEntry {
                        repo_path: entry.repo_path,
                        status: entry.status,
                        is_staged: false,
                    });
                }
                git::status::StageStatus::PartiallyStaged => {
                    self.staged.push(StagingEntry {
                        repo_path: entry.repo_path.clone(),
                        status: entry.status,
                        is_staged: true,
                    });
                    self.unstaged.push(StagingEntry {
                        repo_path: entry.repo_path,
                        status: entry.status,
                        is_staged: false,
                    });
                }
            }
        }
        cx.notify();
    }

    fn stage_file(&self, path: RepoPath, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        repo.update(cx, |repo, cx| {
            repo.stage_entries(vec![path], cx).detach();
        });
    }

    fn unstage_file(&self, path: RepoPath, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        repo.update(cx, |repo, cx| {
            repo.unstage_entries(vec![path], cx).detach();
        });
    }

    fn stage_all(&self, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        repo.update(cx, |repo, cx| {
            repo.stage_all(cx).detach();
        });
    }

    fn unstage_all(&self, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        repo.update(cx, |repo, cx| {
            repo.unstage_all(cx).detach();
        });
    }

    fn render_file_entry(
        &self,
        entry: &StagingEntry,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let path_str = entry.repo_path.as_unix_str().to_string();
        let status_char = entry.status_char();
        let is_staged = entry.is_staged;
        let repo_path = entry.repo_path.clone();

        div()
            .id(SharedString::from(format!(
                "file-{}-{}",
                if is_staged { "s" } else { "u" },
                &path_str
            )))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(2.0))
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                if is_staged {
                    this.unstage_file(repo_path.clone(), cx);
                } else {
                    this.stage_file(repo_path.clone(), cx);
                }
            }))
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::BOLD)
                    .text_color(match status_char {
                        'M' => cx.theme().colors().version_control_modified,
                        'A' => cx.theme().colors().version_control_added,
                        'D' => cx.theme().colors().version_control_deleted,
                        'U' => cx.theme().colors().version_control_conflict,
                        _ => cx.theme().colors().text_muted,
                    })
                    .child(format!("{}", status_char)),
            )
            .child(
                div()
                    .flex_grow()
                    .text_sm()
                    .overflow_x_hidden()
                    .whitespace_nowrap()
                    .child(path_str),
            )
            .into_any_element()
    }
}

impl Focusable for StagingView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for StagingView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let staged_header = div()
            .flex()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().colors().text_muted)
                    .child(format!("Staged ({})", self.staged.len())),
            )
            .child(
                div()
                    .id("unstage-all-btn")
                    .text_xs()
                    .cursor_pointer()
                    .text_color(cx.theme().colors().text_accent)
                    .hover(|s| s.underline())
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.unstage_all(cx);
                    }))
                    .child("Unstage All"),
            );

        let staged_entries: Vec<_> = self
            .staged
            .iter()
            .map(|e| self.render_file_entry(e, cx))
            .collect();

        let unstaged_header = div()
            .flex()
            .items_center()
            .justify_between()
            .px_2()
            .py_1()
            .child(
                div()
                    .text_xs()
                    .font_weight(FontWeight::SEMIBOLD)
                    .text_color(cx.theme().colors().text_muted)
                    .child(format!("Unstaged ({})", self.unstaged.len())),
            )
            .child(
                div()
                    .id("stage-all-btn")
                    .text_xs()
                    .cursor_pointer()
                    .text_color(cx.theme().colors().text_accent)
                    .hover(|s| s.underline())
                    .on_click(cx.listener(|this, _event, _window, cx| {
                        this.stage_all(cx);
                    }))
                    .child("Stage All"),
            );

        let unstaged_entries: Vec<_> = self
            .unstaged
            .iter()
            .map(|e| self.render_file_entry(e, cx))
            .collect();

        div()
            .flex()
            .flex_col()
            .size_full()
            .child(unstaged_header)
            .children(unstaged_entries)
            .child(staged_header)
            .children(staged_entries)
    }
}
