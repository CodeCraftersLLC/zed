use git::repository::Branch;
use gpui::{
    AnyElement, App, Context, Div, Entity, FocusHandle, Focusable, FontWeight, IntoElement, Render,
    SharedString, Window, div, px,
};
use project::git_store::Repository;
use ui::prelude::*;

pub struct BranchList {
    focus_handle: FocusHandle,
    repository: Option<Entity<Repository>>,
    branches: Vec<Branch>,
    show_local: bool,
    show_remote: bool,
}

impl BranchList {
    pub fn new(repository: Option<Entity<Repository>>, cx: &mut Context<Self>) -> Self {
        Self {
            focus_handle: cx.focus_handle(),
            repository,
            branches: Vec::new(),
            show_local: true,
            show_remote: true,
        }
    }

    pub fn set_repository(&mut self, repo: Option<Entity<Repository>>, cx: &mut Context<Self>) {
        self.repository = repo;
        cx.notify();
    }

    pub fn set_branches(&mut self, branches: Vec<Branch>, cx: &mut Context<Self>) {
        self.branches = branches;
        self.branches.sort_by(|a, b| {
            b.priority_key()
                .partial_cmp(&a.priority_key())
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        cx.notify();
    }

    pub fn local_branch_names(&self) -> Vec<String> {
        self.branches
            .iter()
            .filter(|b| !b.is_remote())
            .map(|b| b.name().to_string())
            .collect()
    }

    fn local_branches(&self) -> Vec<&Branch> {
        self.branches.iter().filter(|b| !b.is_remote()).collect()
    }

    fn remote_branches(&self) -> Vec<&Branch> {
        self.branches.iter().filter(|b| b.is_remote()).collect()
    }

    fn checkout_branch(&self, branch_name: String, cx: &mut Context<Self>) {
        let Some(repo) = self.repository.clone() else {
            return;
        };
        let rx = repo.update(cx, |repo, _cx| repo.change_branch(branch_name));
        cx.spawn(async move |_this, _cx| {
            let _ = rx.await;
        })
        .detach();
    }

    fn render_branch_item(
        &self,
        branch: &Branch,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let name = branch.name().to_string();
        let is_head = branch.is_head;
        let tracking = branch.tracking_status();
        let checkout_name = name.clone();

        let mut row = div()
            .id(SharedString::from(format!("branch-{}", &name)))
            .flex()
            .items_center()
            .gap_1()
            .px_2()
            .py(px(3.0))
            .rounded_sm()
            .cursor_pointer()
            .hover(|style| style.bg(cx.theme().colors().ghost_element_hover))
            .active(|style| style.bg(cx.theme().colors().ghost_element_active))
            .on_click(cx.listener(move |this, _event, _window, cx| {
                this.checkout_branch(checkout_name.clone(), cx);
            }));

        if is_head {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_accent)
                    .child("●"),
            );
        }

        let mut name_el = div()
            .flex_grow()
            .text_sm()
            .overflow_x_hidden()
            .whitespace_nowrap();
        if is_head {
            name_el = name_el.font_weight(FontWeight::SEMIBOLD);
        }
        name_el = name_el.child(name);
        row = row.child(name_el);

        if let Some(status) = tracking {
            row = row.child(
                div()
                    .text_xs()
                    .text_color(cx.theme().colors().text_muted)
                    .child(format!("↑{} ↓{}", status.ahead, status.behind)),
            );
        }

        row.into_any_element()
    }

    fn render_section_items(
        &self,
        label: &str,
        branches: &[&Branch],
        expanded: bool,
        cx: &mut Context<Self>,
    ) -> Div {
        if !expanded || branches.is_empty() {
            return div();
        }

        let header = div()
            .text_xs()
            .font_weight(FontWeight::SEMIBOLD)
            .text_color(cx.theme().colors().text_muted)
            .px_2()
            .py_1()
            .child(format!("{} ({})", label, branches.len()));

        let mut container = div().flex().flex_col().child(header);
        for branch in branches {
            container = container.child(self.render_branch_item(branch, cx));
        }
        container
    }
}

impl Focusable for BranchList {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for BranchList {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let local = self.local_branches();
        let remote = self.remote_branches();

        let mut root = div()
            .id("branch-list")
            .flex()
            .flex_col()
            .size_full()
            .overflow_y_scroll()
            .py_1();

        root = root.child(self.render_section_items("Local", &local, self.show_local, cx));
        root = root.child(self.render_section_items("Remote", &remote, self.show_remote, cx));
        root
    }
}
