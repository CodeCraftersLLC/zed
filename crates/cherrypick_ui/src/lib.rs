mod cherrypick_sidebar;
mod cherrypick_view;
mod branch_list;
mod commit_graph_embed;
mod staging_view;
mod worktree_list;
mod stash_list;
mod remote_toolbar;
mod repo_state_banner;
mod settings;

pub fn init(cx: &mut gpui::App) {
    cx.observe_new(|workspace: &mut workspace::Workspace, _window, _cx| {
        cherrypick_sidebar::register(workspace);
    })
    .detach();
}
