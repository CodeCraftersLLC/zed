use git_graph::GitGraph;
use gpui::AppContext as _;
use workspace::Workspace;

pub fn open_git_graph(workspace: &mut Workspace, window: &mut gpui::Window, cx: &mut gpui::Context<Workspace>) {
    let existing = workspace.items_of_type::<GitGraph>(cx).next();
    if let Some(existing) = existing {
        workspace.activate_item(&existing, true, true, window, cx);
        return;
    }

    let project = workspace.project().clone();
    if project.read(cx).active_repository(cx).is_none() {
        log::warn!("cherrypick: no active repository for git graph");
        return;
    }

    let workspace_handle = workspace.weak_handle();
    let git_graph = cx.new(|cx| GitGraph::new(project, workspace_handle, window, cx));
    workspace.add_item_to_active_pane(Box::new(git_graph), None, true, window, cx);
}
