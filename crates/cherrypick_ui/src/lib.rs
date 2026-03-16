pub mod cherrypick_sidebar;
pub use cherrypick_sidebar::CherryPickSidebar;
pub(crate) mod cherrypick_view;
pub(crate) mod branch_list;
mod commit_graph_embed;
pub(crate) mod create_pr_form;
pub(crate) mod diff_file_list;
pub(crate) mod pr_detail;
pub(crate) mod pr_list;
pub(crate) mod pr_state;
pub(crate) mod staging_view;
mod worktree_list;
mod stash_list;
pub(crate) mod remote_toolbar;
mod repo_state_banner;
mod settings;

pub fn init(cx: &mut gpui::App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        cherrypick_sidebar::register(workspace);
    })
    .detach();
}
