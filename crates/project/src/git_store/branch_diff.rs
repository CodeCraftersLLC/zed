use std::{path::PathBuf, sync::Arc};

use anyhow::Result;
use buffer_diff::BufferDiff;
use collections::{HashMap, HashSet};
use futures::StreamExt;
use git::{
    repository::RepoPath,
    status::{DiffTreeType, FileStatus, StatusCode, TrackedStatus, TreeDiff, TreeDiffStatus},
};
use gpui::{
    App, AppContext as _, AsyncApp, AsyncWindowContext, Context, Entity, EventEmitter,
    SharedString, Subscription, Task, WeakEntity, Window,
};

use language::{Buffer, Capability, DiskState};
use text::BufferId;
use util::{ResultExt, paths::PathStyle, rel_path::RelPath};
use worktree::{EntryKind, WorktreeId};
use ztracing::instrument;

use crate::{
    ConflictSet, Project,
    git_store::{GitStoreEvent, Repository, RepositoryEvent},
};

#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum DiffBase {
    Head,
    Merge { base_ref: SharedString },
}

impl DiffBase {
    pub fn is_merge_base(&self) -> bool {
        matches!(self, DiffBase::Merge { .. })
    }
}

pub struct BranchDiff {
    diff_base: DiffBase,
    repo: Option<Entity<Repository>>,
    project: Entity<Project>,
    base_commit: Option<SharedString>,
    head_commit: Option<SharedString>,
    tree_diff: Option<TreeDiff>,
    tree_diff_update_needed: bool,
    tree_diff_base_task: Option<Task<()>>,
    row_notes: HashMap<RepoPath, RowNote>,
    /// Paths that are links to folders outside what the diff may read, with what
    /// is said of each. Held apart from `row_notes` because nothing beneath such
    /// a path may be loaded, notes or no notes.
    folder_links: HashMap<RepoPath, SharedString>,
    /// Rows that must not be loaded as text, with what is said of each, until
    /// the notes say better: known from attributes, before any content is read.
    held_rows: HashMap<RepoPath, SharedString>,
    /// The host has not yet said which rows are links or held: nothing is loaded
    /// until it has, so no row is ever drawn as text before it is classified.
    awaiting_known_rows: bool,
    /// A path was left out of the last file list because the worktree had not
    /// reached a folder above it yet: the list is made again when it does.
    deferred_loads: bool,
    _subscription: Subscription,
    update_needed: postage::watch::Sender<()>,
    _task: Task<()>,
}

pub enum BranchDiffEvent {
    FileListChanged,
    DiffBaseChanged,
}

/// What the host of a diff knows about a changed path that a text diff cannot
/// draw. The host may take its time over these: the diff is complete without
/// them, only less informative.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RowNote {
    /// The path is one entry whose only line is `text`, with no hunks. A renamed
    /// path's old name is not listed on its own either.
    Sentence {
        text: SharedString,
        renamed_from: Option<RepoPath>,
    },
    /// A renamed path: its text diffed against the content the old name had,
    /// `base`, instead of against nothing. The old name is not listed, and `text`
    /// is said in the entry's header beside the hunks.
    Renamed {
        from: RepoPath,
        base: git::Oid,
        text: SharedString,
    },
}

impl RowNote {
    fn renamed_from(&self) -> Option<&RepoPath> {
        match self {
            Self::Sentence { renamed_from, .. } => renamed_from.as_ref(),
            Self::Renamed { from, .. } => Some(from),
        }
    }
}

impl EventEmitter<BranchDiffEvent> for BranchDiff {}

impl BranchDiff {
    pub fn new(
        source: DiffBase,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let git_store = project.read(cx).git_store().clone();
        let repo = git_store.read(cx).active_repository();
        let git_store_subscription = cx.subscribe_in(
            &git_store,
            window,
            move |this, _git_store, event, _window, cx| {
                let should_update = match event {
                    GitStoreEvent::ActiveRepositoryChanged(new_repo_id) => {
                        this.repo.is_none() && new_repo_id.is_some()
                    }
                    GitStoreEvent::RepositoryUpdated(
                        event_repo_id,
                        RepositoryEvent::StatusesChanged | RepositoryEvent::HeadChanged,
                        _,
                    ) => this
                        .repo
                        .as_ref()
                        .is_some_and(|r| r.read(cx).snapshot().id == *event_repo_id),
                    _ => false,
                };

                if should_update {
                    cx.emit(BranchDiffEvent::FileListChanged);
                    *this.update_needed.borrow_mut() = ();
                }
            },
        );

        let worktree_subscription = cx.subscribe(&project, |this, _project, event, cx| {
            if matches!(
                event,
                crate::Event::WorktreeUpdatedEntries(..) | crate::Event::WorktreeAdded(..)
            ) && this.deferred_loads
            {
                cx.emit(BranchDiffEvent::FileListChanged);
            }
        });

        let (send, recv) = postage::watch::channel::<()>();
        let worker = window.spawn(cx, {
            let this = cx.weak_entity();
            async |cx| Self::handle_status_updates(this, recv, cx).await
        });

        Self {
            diff_base: source,
            repo,
            project,
            tree_diff: None,
            tree_diff_update_needed: false,
            tree_diff_base_task: None,
            row_notes: HashMap::default(),
            folder_links: HashMap::default(),
            held_rows: HashMap::default(),
            awaiting_known_rows: false,
            deferred_loads: false,
            base_commit: None,
            head_commit: None,
            _subscription: Subscription::join(git_store_subscription, worktree_subscription),
            _task: worker,
            update_needed: send,
        }
    }

    pub fn diff_base(&self) -> &DiffBase {
        &self.diff_base
    }

    /// Replace the notes of the changed paths. The file list is read again when
    /// they differ from the ones held.
    pub fn set_row_notes(
        &mut self,
        notes: impl IntoIterator<Item = (RepoPath, RowNote)>,
        cx: &mut Context<Self>,
    ) {
        let notes: HashMap<RepoPath, RowNote> = notes.into_iter().collect();
        if self.row_notes == notes {
            return;
        }
        self.row_notes = notes;
        cx.emit(BranchDiffEvent::FileListChanged);
    }

    /// Load nothing until the host has said which rows are links to folders and
    /// which must be held back ([`Self::set_known_rows`]): for a host that knows
    /// these from attributes, a row is never drawn as text before it is judged.
    pub fn await_known_rows(&mut self) {
        self.awaiting_known_rows = true;
    }

    /// Say which changed paths are links to folders, each with what is said of
    /// it, and which rows must not be loaded as text, each with what is said of
    /// it, until the notes say better. A link is one entry that says it, and what
    /// status lists beneath it is not loaded. The first call ends the wait of
    /// [`Self::await_known_rows`].
    pub fn set_known_rows(
        &mut self,
        links: impl IntoIterator<Item = (RepoPath, SharedString)>,
        held: impl IntoIterator<Item = (RepoPath, SharedString)>,
        cx: &mut Context<Self>,
    ) {
        let links: HashMap<RepoPath, SharedString> = links.into_iter().collect();
        let held: HashMap<RepoPath, SharedString> = held.into_iter().collect();
        let unchanged =
            self.folder_links == links && self.held_rows == held && !self.awaiting_known_rows;
        self.folder_links = links;
        self.held_rows = held;
        self.awaiting_known_rows = false;
        if !unchanged {
            cx.emit(BranchDiffEvent::FileListChanged);
        }
    }

    pub fn set_repo(&mut self, repo: Option<Entity<Repository>>, cx: &mut Context<Self>) {
        let same_repo = match (self.repo.as_ref(), repo.as_ref()) {
            (Some(current), Some(new)) => current.read(cx).id == new.read(cx).id,
            (None, None) => true,
            _ => false,
        };
        if same_repo {
            return;
        }

        self.repo = repo;
        self.tree_diff = None;
        self.tree_diff_update_needed = self.diff_base.is_merge_base();
        self.tree_diff_base_task = None;
        self.base_commit = None;
        self.head_commit = None;
        cx.emit(BranchDiffEvent::FileListChanged);
        *self.update_needed.borrow_mut() = ();
    }

    pub fn set_diff_base(&mut self, diff_base: DiffBase, cx: &mut Context<Self>) {
        if self.diff_base == diff_base {
            return;
        }

        self.tree_diff_update_needed = diff_base.is_merge_base();
        self.tree_diff = None;
        self.tree_diff_base_task = None;
        self.diff_base = diff_base;
        self.base_commit = None;
        self.head_commit = None;

        cx.emit(BranchDiffEvent::DiffBaseChanged);
        if self.tree_diff_update_needed {
            *self.update_needed.borrow_mut() = ();
        }
    }

    pub async fn handle_status_updates(
        this: WeakEntity<Self>,
        mut recv: postage::watch::Receiver<()>,
        cx: &mut AsyncWindowContext,
    ) {
        this.update(cx, |this, cx| this.spawn_reload_tree_diff(cx))
            .log_err();
        while recv.next().await.is_some() {
            let Ok(()) = this.update(cx, |this, cx| {
                let mut needs_update = this.tree_diff_update_needed;
                this.tree_diff_update_needed = false;

                if this.repo.is_none() {
                    let active_repo = this
                        .project
                        .read(cx)
                        .git_store()
                        .read(cx)
                        .active_repository();
                    if active_repo.is_some() {
                        this.repo = active_repo;
                        needs_update = true;
                    }
                } else if let Some(repo) = this.repo.as_ref() {
                    repo.update(cx, |repo, _| {
                        if let Some(branch) = &repo.branch
                            && let DiffBase::Merge { base_ref } = &this.diff_base
                            && let Some(commit) = branch.most_recent_commit.as_ref()
                            && &branch.ref_name == base_ref
                            && this.base_commit.as_ref() != Some(&commit.sha)
                        {
                            this.base_commit = Some(commit.sha.clone());
                            needs_update = true;
                        }

                        if repo.head_commit.as_ref().map(|c| &c.sha) != this.head_commit.as_ref() {
                            this.head_commit = repo.head_commit.as_ref().map(|c| c.sha.clone());
                            needs_update = true;
                        }
                    })
                }

                if needs_update {
                    this.spawn_reload_tree_diff(cx);
                }
            }) else {
                return;
            };
        }
    }

    pub fn status_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<FileStatus> {
        let (repo, path) = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(buffer_id, cx)?;
        if self.repo() == Some(&repo) {
            return self.merge_statuses(
                repo.read(cx)
                    .status_for_path(&path)
                    .map(|status| status.status),
                self.tree_diff
                    .as_ref()
                    .and_then(|diff| diff.entries.get(&path)),
            );
        }
        None
    }

    pub fn merge_statuses(
        &self,
        diff_from_head: Option<FileStatus>,
        diff_from_merge_base: Option<&TreeDiffStatus>,
    ) -> Option<FileStatus> {
        match (diff_from_head, diff_from_merge_base) {
            (None, None) => None,
            (Some(diff_from_head), None) => Some(diff_from_head),
            (Some(diff_from_head @ FileStatus::Unmerged(_)), _) => Some(diff_from_head),

            // file does not exist in HEAD
            // but *does* exist in work-tree
            // and *does* exist in merge-base
            (
                Some(FileStatus::Untracked)
                | Some(FileStatus::Tracked(TrackedStatus {
                    index_status: StatusCode::Added,
                    worktree_status: _,
                })),
                Some(_),
            ) => Some(FileStatus::Tracked(TrackedStatus {
                index_status: StatusCode::Modified,
                worktree_status: StatusCode::Modified,
            })),

            // file exists in HEAD
            // but *does not* exist in work-tree
            (Some(diff_from_head), Some(diff_from_merge_base)) if diff_from_head.is_deleted() => {
                match diff_from_merge_base {
                    TreeDiffStatus::Added => None, // unchanged, didn't exist in merge base or worktree
                    _ => Some(diff_from_head),
                }
            }

            // file exists in HEAD
            // and *does* exist in work-tree
            (Some(FileStatus::Tracked(_)), Some(tree_status)) => {
                Some(FileStatus::Tracked(TrackedStatus {
                    index_status: match tree_status {
                        TreeDiffStatus::Added { .. } => StatusCode::Added,
                        _ => StatusCode::Modified,
                    },
                    worktree_status: match tree_status {
                        TreeDiffStatus::Added => StatusCode::Added,
                        _ => StatusCode::Modified,
                    },
                }))
            }

            (_, Some(diff_from_merge_base)) => {
                Some(diff_status_to_file_status(diff_from_merge_base))
            }
        }
    }

    fn spawn_reload_tree_diff(&mut self, cx: &mut Context<Self>) {
        if !self.diff_base.is_merge_base() {
            return;
        }

        let task = cx.spawn(async move |this, cx| {
            Self::reload_tree_diff(this, cx).await.log_err();
        });

        self.tree_diff_base_task = Some(task);
        cx.notify();
    }

    /// True until the host has said which rows are links and which are held
    /// ([`Self::await_known_rows`]): nothing is listed meanwhile, which is not an
    /// empty diff.
    pub fn is_awaiting_known_rows(&self) -> bool {
        self.awaiting_known_rows
    }

    pub fn is_tree_base_loading(&self) -> bool {
        self.tree_diff_base_task
            .as_ref()
            .is_some_and(|task| !task.is_ready())
    }

    /// True while the active repository exists but its first status scan has not landed.
    pub fn is_status_loading(&self, cx: &App) -> bool {
        self.repo
            .as_ref()
            .is_some_and(|repo| !repo.read(cx).snapshot().has_scanned_statuses())
    }

    pub async fn reload_tree_diff(this: WeakEntity<Self>, cx: &mut AsyncApp) -> Result<()> {
        let task = this.update(cx, |this, cx| {
            let DiffBase::Merge { base_ref } = this.diff_base.clone() else {
                return None;
            };
            let Some(repo) = this.repo.as_ref() else {
                this.tree_diff.take();
                return None;
            };
            repo.update(cx, |repo, cx| {
                Some(repo.diff_tree(
                    DiffTreeType::MergeBase {
                        base: base_ref,
                        head: "HEAD".into(),
                    },
                    cx,
                ))
            })
        })?;
        let Some(task) = task else { return Ok(()) };

        let diff = task.await??;
        this.update(cx, |this, cx| {
            this.tree_diff = Some(diff);
            cx.emit(BranchDiffEvent::FileListChanged);
            cx.notify();
        })
    }

    pub fn repo(&self) -> Option<&Entity<Repository>> {
        self.repo.as_ref()
    }

    #[instrument(skip_all)]
    pub fn load_buffers(&mut self, cx: &mut Context<Self>) -> Vec<DiffBuffer> {
        let mut output = Vec::default();
        let Some(repo) = self.repo.clone() else {
            return output;
        };
        if self.diff_base.is_merge_base() && self.tree_diff.is_none() {
            return output;
        }
        if self.awaiting_known_rows {
            return output;
        }

        let renamed_away: HashSet<RepoPath> = self
            .row_notes
            .values()
            .filter_map(|note| note.renamed_from().cloned())
            .collect();
        let mut queue = LoadQueue {
            repo,
            snapshots: HashMap::default(),
            deferred: false,
            output: &mut output,
        };
        self.project.update(cx, |project, cx| {
            let mut seen = HashSet::default();
            let mut linked_parents = HashSet::default();

            for item in queue.repo.clone().read(cx).cached_status() {
                seen.insert(item.repo_path.clone());
                if renamed_away.contains(&item.repo_path) {
                    continue;
                }
                if let Some(link) = self.link_ancestor(&item.repo_path) {
                    linked_parents.insert(link);
                    continue;
                }
                let branch_diff = self
                    .tree_diff
                    .as_ref()
                    .and_then(|t| t.entries.get(&item.repo_path))
                    .cloned();
                let Some(status) = self.merge_statuses(Some(item.status), branch_diff.as_ref())
                else {
                    continue;
                };
                if !status.has_changes() {
                    continue;
                }

                let kind = self.row_load(&item.repo_path, branch_diff);
                queue.push(project, item.repo_path.clone(), item.status, kind, cx);
            }
            for (path, branch_diff) in self.tree_diff.iter().flat_map(|t| t.entries.iter()) {
                if !seen.insert(path.clone()) || renamed_away.contains(path) {
                    continue;
                }
                if let Some(link) = self.link_ancestor(path) {
                    linked_parents.insert(link);
                    continue;
                }
                let file_status = diff_status_to_file_status(branch_diff);
                let load = self.row_load(path, Some(branch_diff.clone()));
                queue.push(project, path.clone(), file_status, load, cx);
            }

            // Status can list only what is beneath a link to a folder (the files
            // behind a junction) and no entry for the link itself.
            for link in linked_parents {
                if let Some(sentence) = self.folder_links.get(&link) {
                    if !seen.contains(&link) {
                        let note = Load::Note(sentence.clone());
                        queue.push(project, link, FileStatus::Untracked, note, cx);
                    }
                }
            }
        });
        self.deferred_loads = queue.deferred;
        output
    }

    /// What is drawn for a changed row, strongest first, whichever list the row
    /// came from. A note that is one line of text may refine a hold; one that
    /// would draw the file's text (a rename diffed against its old name) never
    /// outranks it, so a note computed before the attributes changed cannot show
    /// what the attributes now forbid.
    fn row_load(&self, path: &RepoPath, branch_diff: Option<TreeDiffStatus>) -> Load {
        match (
            self.folder_links.get(path),
            self.row_notes.get(path),
            self.held_rows.get(path),
        ) {
            (Some(sentence), _, _) => Load::Note(sentence.clone()),
            (None, Some(RowNote::Sentence { text, .. }), _) => Load::Note(text.clone()),
            (None, _, Some(held)) => Load::Note(held.clone()),
            (None, Some(RowNote::Renamed { base, .. }), None) => {
                Load::Diff(Some(TreeDiffStatus::Modified { old: *base }))
            }
            (None, None, None) => Load::Diff(branch_diff),
        }
    }

    /// What the worktree's own snapshot says about reading `project_path`: see
    /// [`admission`]. Read in memory, so it holds before any link the host
    /// supplies, and costs no file system call.
    fn admission_of(
        project: &Project,
        project_path: &crate::ProjectPath,
        deleted: bool,
        snapshots: &mut HashMap<WorktreeId, worktree::Snapshot>,
        cx: &App,
    ) -> Admission {
        let snapshot = match snapshots.entry(project_path.worktree_id) {
            collections::hash_map::Entry::Occupied(snapshot) => snapshot.into_mut(),
            collections::hash_map::Entry::Vacant(slot) => {
                let Some(worktree) = project.worktree_for_id(project_path.worktree_id, cx) else {
                    return Admission::NotScanned;
                };
                slot.insert(worktree.read(cx).snapshot())
            }
        };
        admission(&project_path.path, deleted, |path| {
            snapshot.entry_for_path(path).map(|entry| {
                // The worktree's root may itself be reached through a link;
                // only the folders inside it are judged.
                let is_link =
                    !path.is_empty() && (entry.canonical_path.is_some() || entry.is_external);
                if is_link {
                    Ancestor::Link
                } else {
                    Ancestor::Plain
                }
            })
        })
    }

    /// The link to a folder that is a proper ancestor of `path`, if there is one.
    fn link_ancestor(&self, path: &RepoPath) -> Option<RepoPath> {
        if self.folder_links.is_empty() {
            return None;
        }
        path.as_ref()
            .ancestors()
            .skip(1)
            .map(RepoPath::from_rel_path)
            .find(|ancestor| self.folder_links.contains_key(ancestor))
    }

    /// What is said in the header of the entry for `buffer_id`, beside its hunks,
    /// when it is a renamed path.
    pub fn note_text_for_buffer_id(&self, buffer_id: BufferId, cx: &App) -> Option<SharedString> {
        let (repo, path) = self
            .project
            .read(cx)
            .git_store()
            .read(cx)
            .repository_and_path_for_buffer_id(buffer_id, cx)?;
        if self.repo() != Some(&repo) {
            return None;
        }
        match self.row_notes.get(&path)? {
            RowNote::Renamed { text, .. } => Some(text.clone()),
            RowNote::Sentence { .. } => None,
        }
    }

    /// A buffer holding only `text`, read-only and named as `project_path` is, so
    /// its header reads like the path's own. No text diff is in it.
    fn load_note(
        text: SharedString,
        project_path: &crate::ProjectPath,
        cx: &Context<'_, Project>,
    ) -> Task<Result<(Entity<Buffer>, Entity<BufferDiff>, Entity<ConflictSet>)>> {
        let file = NoteFile::new(&project_path.path, project_path.worktree_id);
        cx.spawn(async move |_project, cx| {
            let buffer = cx.new(|cx| {
                let mut buffer = Buffer::local(text.to_string(), cx);
                buffer.set_capability(Capability::ReadOnly, cx);
                buffer.file_updated(Arc::new(file), cx);
                buffer
            });
            let snapshot = buffer.read_with(cx, |buffer, _| buffer.text_snapshot());
            let diff = cx.new(|cx| BufferDiff::new_unchanged(&snapshot, None, None, cx));
            let conflict_set = cx.new(|cx| ConflictSet::new(snapshot.remote_id(), false, cx));
            Ok((buffer, diff, conflict_set))
        })
    }

    #[instrument(skip_all)]
    fn load_buffer(
        branch_diff: Option<git::status::TreeDiffStatus>,
        project_path: crate::ProjectPath,
        repo: Entity<Repository>,
        cx: &Context<'_, Project>,
    ) -> Task<Result<(Entity<Buffer>, Entity<BufferDiff>, Entity<ConflictSet>)>> {
        let task = cx.spawn(async move |project, cx| {
            let buffer = project
                .update(cx, |project, cx| project.open_buffer(project_path, cx))?
                .await?;

            let changes = if let Some(entry) = branch_diff {
                let oid = match entry {
                    git::status::TreeDiffStatus::Added { .. } => None,
                    git::status::TreeDiffStatus::Modified { old, .. }
                    | git::status::TreeDiffStatus::Deleted { old } => Some(old),
                };
                project
                    .update(cx, |project, cx| {
                        project.git_store().update(cx, |git_store, cx| {
                            git_store.open_diff_since(oid, buffer.clone(), repo, cx)
                        })
                    })?
                    .await?
            } else {
                project
                    .update(cx, |project, cx| {
                        project.open_uncommitted_diff(buffer.clone(), cx)
                    })?
                    .await?
            };
            let conflict_set = project
                .update(cx, |project, cx| {
                    project.git_store().update(cx, |git_store, cx| {
                        git_store.open_conflict_set(buffer.clone(), cx)
                    })
                })?
                .await;
            Ok((buffer, changes, conflict_set))
        });
        task
    }
}

fn diff_status_to_file_status(branch_diff: &git::status::TreeDiffStatus) -> FileStatus {
    let file_status = match branch_diff {
        git::status::TreeDiffStatus::Added { .. } => FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Added,
            worktree_status: StatusCode::Added,
        }),
        git::status::TreeDiffStatus::Modified { .. } => FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Modified,
            worktree_status: StatusCode::Modified,
        }),
        git::status::TreeDiffStatus::Deleted { .. } => FileStatus::Tracked(TrackedStatus {
            index_status: StatusCode::Deleted,
            worktree_status: StatusCode::Deleted,
        }),
    };
    file_status
}

#[derive(Debug)]
pub struct DiffBuffer {
    pub repo_path: RepoPath,
    pub file_status: FileStatus,
    pub load: Task<Result<(Entity<Buffer>, Entity<BufferDiff>, Entity<ConflictSet>)>>,
    /// `load` makes a [`RowNote`] buffer, whole in its excerpt, not a changed file.
    pub is_note: bool,
}

/// What a load is for.
enum Load {
    /// An entry that is one line of text, with no file read.
    Note(SharedString),
    /// A changed file, diffed against this base (HEAD's text when there is none).
    Diff(Option<TreeDiffStatus>),
}

/// What the worktree's snapshot knows about a folder above a path.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ancestor {
    Plain,
    /// A symbolic link or junction, or outside the worktree's root.
    Link,
}

/// Whether a path may be loaded.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Admission {
    Read,
    /// A folder above it is a link: loading it would follow the link out.
    Behind,
    /// The worktree has not reached a folder above it yet, so nothing is known
    /// of what that folder is. It is not loaded until the worktree has.
    NotScanned,
}

/// Whether `path` may be loaded, judged only on what `ancestor` says about each
/// folder above it (`None`: the worktree has no entry for it). A path whose folder
/// the worktree does not know yet fails closed ([`Admission::NotScanned`]) unless
/// the file is `deleted`: there is nothing on disk to read through a folder that
/// was removed with it.
pub fn admission(
    path: &RelPath,
    deleted: bool,
    ancestor: impl Fn(&RelPath) -> Option<Ancestor>,
) -> Admission {
    let mut unknown = false;
    for above in path.ancestors().skip(1) {
        match ancestor(above) {
            Some(Ancestor::Link) => return Admission::Behind,
            Some(Ancestor::Plain) => {}
            None => unknown = true,
        }
    }
    if unknown && !deleted {
        Admission::NotScanned
    } else {
        Admission::Read
    }
}

/// The folder to ask the worktree to list so that it comes a level closer to `path`: the nearest
/// folder above it that the worktree knows (`known` says whether it is a link, and whether it is
/// not listed yet). Never a link, and never `path` or anything beneath a folder not yet judged:
/// a requested path is scanned even through a link out of the worktree, so the scan goes one
/// level and [`admission`] judges what it found before the next is asked for.
pub fn unloaded_folder_to_scan(
    path: &RelPath,
    known: impl Fn(&RelPath) -> Option<(bool, bool)>,
) -> Option<&RelPath> {
    let (nearest, (is_link, unloaded)) = path
        .ancestors()
        .skip(1)
        .find_map(|above| known(above).map(|state| (above, state)))?;
    (unloaded && !is_link).then_some(nearest)
}

/// The one place a [`DiffBuffer`] is made: every load of a changed file passes
/// [`admission`] here first, whichever list the path came from.
struct LoadQueue<'a> {
    repo: Entity<Repository>,
    snapshots: HashMap<WorktreeId, worktree::Snapshot>,
    /// A path was left out because the worktree has not reached a folder above it.
    deferred: bool,
    output: &'a mut Vec<DiffBuffer>,
}

impl LoadQueue<'_> {
    /// A folder the worktree skips on its own (an ignored one) is never reached
    /// otherwise, so the nearest folder it knows above `project_path` is asked to
    /// list itself ([`unloaded_folder_to_scan`]): one level, whose update retries
    /// the file list. Stateless on purpose: a folder that is unloaded again is
    /// asked again.
    fn request_scan_of_unloaded_ancestor(
        &self,
        project: &Project,
        project_path: &crate::ProjectPath,
        cx: &App,
    ) {
        let Some(snapshot) = self.snapshots.get(&project_path.worktree_id) else {
            return;
        };
        let folder = unloaded_folder_to_scan(&project_path.path, |path| {
            snapshot.entry_for_path(path).map(|entry| {
                let is_link =
                    !path.is_empty() && (entry.canonical_path.is_some() || entry.is_external);
                (is_link, entry.kind == EntryKind::UnloadedDir)
            })
        });
        if let Some(folder) = folder
            && let Some(local) = project
                .worktree_for_id(project_path.worktree_id, cx)
                .and_then(|worktree| worktree.read(cx).as_local())
        {
            local.refresh_entries_for_paths(vec![folder.into()]);
        }
    }

    fn push(
        &mut self,
        project: &Project,
        repo_path: RepoPath,
        file_status: FileStatus,
        kind: Load,
        cx: &Context<'_, Project>,
    ) {
        let Some(project_path) = self.repo.read(cx).repo_path_to_project_path(&repo_path, cx)
        else {
            return;
        };
        let (load, is_note) = match kind {
            Load::Note(sentence) => (BranchDiff::load_note(sentence, &project_path, cx), true),
            Load::Diff(base) => {
                match BranchDiff::admission_of(
                    project,
                    &project_path,
                    file_status.is_deleted(),
                    &mut self.snapshots,
                    cx,
                ) {
                    Admission::Read => {}
                    Admission::Behind => return,
                    Admission::NotScanned => {
                        self.deferred = true;
                        self.request_scan_of_unloaded_ancestor(project, &project_path, cx);
                        return;
                    }
                }
                (
                    BranchDiff::load_buffer(base, project_path, self.repo.clone(), cx),
                    false,
                )
            }
        };
        self.output.push(DiffBuffer {
            repo_path,
            load,
            file_status,
            is_note,
        });
    }
}

/// The file of a note's buffer: a path the project knows by name only, which the
/// buffer store, the language servers and the worktree never see.
struct NoteFile {
    path: Arc<RelPath>,
    full_path: PathBuf,
    file_name: String,
    worktree_id: WorktreeId,
}

impl NoteFile {
    fn new(path: &Arc<RelPath>, worktree_id: WorktreeId) -> Self {
        Self {
            path: path.clone(),
            full_path: path.as_std_path().to_path_buf(),
            file_name: path.file_name().unwrap_or("untitled").to_string(),
            worktree_id,
        }
    }
}

impl language::File for NoteFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic { was_deleted: false }
    }

    fn path(&self) -> &Arc<RelPath> {
        &self.path
    }

    fn full_path(&self, _: &App) -> PathBuf {
        self.full_path.clone()
    }

    fn path_style(&self, _: &App) -> PathStyle {
        PathStyle::Posix
    }

    fn file_name<'a>(&'a self, _: &'a App) -> &'a str {
        &self.file_name
    }

    fn worktree_id(&self, _: &App) -> WorktreeId {
        self.worktree_id
    }

    fn to_proto(&self, cx: &App) -> language::proto::File {
        language::proto::File {
            worktree_id: self.worktree_id(cx).to_proto(),
            entry_id: None,
            path: self.path.as_ref().to_proto(),
            mtime: None,
            is_deleted: false,
            is_historic: true,
        }
    }

    fn is_private(&self) -> bool {
        false
    }

    fn can_open(&self) -> bool {
        false
    }
}
