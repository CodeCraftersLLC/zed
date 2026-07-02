use anyhow::Result;
use buffer_diff::BufferDiff;
use editor::{Editor, EditorEvent, MultiBuffer};
use gpui::{
    AnyElement, App, AppContext as _, AsyncApp, Context, Entity, EventEmitter, FocusHandle,
    Focusable, Font, IntoElement, Render, SharedString, Task, Window,
};
use language::{
    Buffer, Capability, DiskState, File as LanguageFile, HighlightedText, OffsetRangeExt, Point,
    ReplicaId, TextBuffer,
};
use multi_buffer::PathKey;
use project::{Project, ProjectPath};
use settings::WorktreeId;
use std::{
    any::{Any, TypeId},
    path::{Path, PathBuf},
    sync::Arc,
};
use ui::{Color, Icon, IconName, Label, LabelCommon as _};
use util::ResultExt as _;
use util::paths::PathStyle;
use util::rel_path::RelPath;
use workspace::{
    Item, ItemHandle as _, ItemNavHistory, ToolbarItemLocation, Workspace,
    item::{ItemEvent, SaveOptions, TabContentParams},
    searchable::SearchableItemHandle,
};

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum CommitDiffPreviewStatus {
    Added,
    Deleted,
    Modified,
    Renamed,
    Copied,
    Typechange,
}

#[derive(Clone, Debug)]
pub struct CommitDiffPreviewFile {
    pub display_path: Arc<str>,
    pub old_path: Option<Arc<str>>,
    pub new_path: Option<Arc<str>>,
    pub status: CommitDiffPreviewStatus,
    pub old_text: Option<Arc<str>>,
    pub new_text: Option<Arc<str>>,
    pub is_binary: bool,
    pub is_truncated: bool,
    pub load_error: Option<Arc<str>>,
}

#[derive(Clone, Debug)]
pub struct CommitDiffPreviewOptions {
    pub title: SharedString,
    pub context_lines: u32,
    pub reveal_all_hunks: bool,
}

impl Default for CommitDiffPreviewOptions {
    fn default() -> Self {
        Self {
            title: "Commit Diff".into(),
            context_lines: 3,
            reveal_all_hunks: true,
        }
    }
}

pub struct CommitDiffPreview {
    editor: Entity<Editor>,
    title: SharedString,
    file_count: usize,
    _populate_task: Task<Result<()>>,
}

struct PreviewEntry {
    index: usize,
    display_path: Arc<str>,
    metadata_path: Arc<str>,
    was_deleted: bool,
    old_text: Option<Arc<str>>,
    new_text: Arc<str>,
}

struct PreviewBufferFile {
    path: Arc<RelPath>,
    full_path: PathBuf,
    file_name: String,
    was_deleted: bool,
}

impl PreviewBufferFile {
    fn new(metadata_path: &str, was_deleted: bool) -> Self {
        let path = display_rel_path(metadata_path);
        let file_name = path.file_name().unwrap_or("untitled").to_string();
        let full_path = path.as_std_path().to_path_buf();

        Self {
            path,
            full_path,
            file_name,
            was_deleted,
        }
    }
}

impl LanguageFile for PreviewBufferFile {
    fn as_local(&self) -> Option<&dyn language::LocalFile> {
        None
    }

    fn disk_state(&self) -> DiskState {
        DiskState::Historic {
            was_deleted: self.was_deleted,
        }
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
        WorktreeId::from_usize(0)
    }

    fn to_proto(&self, cx: &App) -> language::proto::File {
        language::proto::File {
            worktree_id: self.worktree_id(cx).to_proto(),
            entry_id: None,
            path: self.path.as_ref().to_proto(),
            mtime: None,
            is_deleted: self.was_deleted,
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

impl CommitDiffPreviewFile {
    fn into_entry(self, index: usize) -> PreviewEntry {
        let CommitDiffPreviewFile {
            display_path,
            old_path,
            new_path,
            status,
            old_text,
            new_text,
            is_binary,
            is_truncated,
            load_error,
        } = self;
        let metadata_path = new_path
            .clone()
            .or_else(|| old_path.clone())
            .unwrap_or_else(|| display_path.clone());
        let was_deleted = matches!(status, CommitDiffPreviewStatus::Deleted);

        if let Some(error) = load_error {
            return PreviewEntry {
                index,
                display_path: display_path.clone(),
                metadata_path,
                was_deleted,
                old_text: None,
                new_text: format!("Unable to load diff for {display_path}: {error}\n").into(),
            };
        }

        if is_binary {
            return PreviewEntry {
                index,
                display_path: display_path.clone(),
                metadata_path,
                was_deleted,
                old_text: None,
                new_text: format!("Binary file: {display_path}\n").into(),
            };
        }

        if is_truncated {
            return PreviewEntry {
                index,
                display_path: display_path.clone(),
                metadata_path,
                was_deleted,
                old_text: None,
                new_text: format!("File too large to preview: {display_path}\n").into(),
            };
        }

        match status {
            CommitDiffPreviewStatus::Added => PreviewEntry {
                index,
                display_path,
                metadata_path,
                was_deleted,
                old_text: None,
                new_text: new_text.unwrap_or_else(|| "".into()),
            },
            CommitDiffPreviewStatus::Deleted => PreviewEntry {
                index,
                display_path,
                metadata_path,
                was_deleted,
                old_text,
                new_text: "".into(),
            },
            CommitDiffPreviewStatus::Modified
            | CommitDiffPreviewStatus::Renamed
            | CommitDiffPreviewStatus::Copied
            | CommitDiffPreviewStatus::Typechange => PreviewEntry {
                index,
                display_path,
                metadata_path,
                was_deleted,
                old_text,
                new_text: new_text.unwrap_or_else(|| "".into()),
            },
        }
    }
}

impl CommitDiffPreview {
    pub fn new(
        files: Vec<CommitDiffPreviewFile>,
        options: CommitDiffPreviewOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let multibuffer = cx.new(|cx| {
            let mut multibuffer = MultiBuffer::new(Capability::ReadOnly);
            if options.reveal_all_hunks {
                multibuffer.set_all_diff_hunks_expanded(cx);
            }
            multibuffer
        });

        let file_count = files.len();
        let editor = cx.new(|cx| {
            let mut editor =
                Editor::for_multibuffer(multibuffer.clone(), Some(project.clone()), window, cx);
            editor.start_temporary_diff_override();
            editor.disable_diagnostics(cx);
            if options.reveal_all_hunks {
                editor.set_expand_all_diff_hunks(cx);
            }
            editor.set_render_diff_hunks_as_unstaged(true, cx);
            editor.set_render_diff_hunk_controls(
                Arc::new(|_, _, _, _, _, _, _, _| gpui::Empty.into_any_element()),
                cx,
            );
            editor
        });

        let language_registry = project.read(cx).languages().clone();
        let entries = files
            .into_iter()
            .enumerate()
            .map(|(index, file)| file.into_entry(index))
            .collect::<Vec<_>>();
        let context_lines = options.context_lines;
        let populate_task = cx.spawn(async move |_, cx| {
            populate_entries(multibuffer, entries, context_lines, language_registry, cx)
                .await
                .log_err();
            Ok(())
        });

        Self {
            editor,
            title: options.title,
            file_count,
            _populate_task: populate_task,
        }
    }

    fn title(&self) -> SharedString {
        if self.file_count == 0 {
            return self.title.clone();
        }
        let suffix = if self.file_count == 1 {
            "1 file".to_string()
        } else {
            format!("{} files", self.file_count)
        };
        format!("{} ({suffix})", self.title).into()
    }
}

async fn populate_entries(
    multibuffer: Entity<MultiBuffer>,
    entries: Vec<PreviewEntry>,
    context_lines: u32,
    language_registry: Arc<language::LanguageRegistry>,
    cx: &mut AsyncApp,
) -> Result<()> {
    for entry in entries {
        let language = language_registry
            .load_language_for_file_path(Path::new(entry.metadata_path.as_ref()))
            .await
            .ok();

        let buffer = cx.new(|cx| {
            let buffer_file: Arc<dyn LanguageFile> = Arc::new(PreviewBufferFile::new(
                entry.metadata_path.as_ref(),
                entry.was_deleted,
            ));
            let mut buffer = Buffer::build(
                TextBuffer::new(
                    ReplicaId::LOCAL,
                    cx.entity_id().as_non_zero_u64().into(),
                    entry.new_text.to_string(),
                ),
                Some(buffer_file),
                Capability::ReadWrite,
            );
            buffer.set_language_registry(language_registry.clone());
            buffer.set_language(language.clone(), cx);
            buffer
        });
        buffer.update(cx, |buffer, _| buffer.parsing_idle()).await;

        let diff = build_snapshot_diff(entry.old_text.clone(), &buffer, cx).await?;
        register_entry(
            &multibuffer,
            entry.index,
            entry.display_path,
            buffer,
            diff,
            context_lines,
            cx,
        );
    }

    Ok(())
}

async fn build_snapshot_diff(
    old_text: Option<Arc<str>>,
    buffer: &Entity<Buffer>,
    cx: &mut AsyncApp,
) -> Result<Entity<BufferDiff>> {
    let language = cx.update(|cx| buffer.read(cx).language().cloned());
    let language_registry = cx.update(|cx| buffer.read(cx).language_registry());
    let snapshot = cx.update(|cx| buffer.read(cx).snapshot());
    let diff = cx.new(|cx| BufferDiff::new(&snapshot.text, language, language_registry, cx));
    let new_text = snapshot.text.clone();
    diff.update(cx, |diff, cx| diff.set_base_text(old_text, new_text, cx))
        .await;
    Ok(diff)
}

fn register_entry(
    multibuffer: &Entity<MultiBuffer>,
    index: usize,
    display_path: Arc<str>,
    buffer: Entity<Buffer>,
    diff: Entity<BufferDiff>,
    context_lines: u32,
    cx: &mut AsyncApp,
) {
    cx.update(|cx| {
        let snapshot = buffer.read(cx).snapshot();
        let diff_snapshot = diff.read(cx).snapshot(cx);
        let mut ranges = diff_snapshot
            .hunks(&snapshot)
            .map(|hunk| hunk.buffer_range.to_point(&snapshot))
            .collect::<Vec<_>>();
        if ranges.is_empty() {
            ranges.push(Point::new(0, 0)..snapshot.max_point());
        }

        multibuffer.update(cx, |multibuffer, cx| {
            multibuffer.set_excerpts_for_path(
                display_path_key(index, &display_path),
                buffer.clone(),
                ranges,
                context_lines,
                cx,
            );
            multibuffer.add_diff(diff.clone(), cx);
        });
    });
}

fn display_path_key(index: usize, display_path: &str) -> PathKey {
    PathKey::with_sort_prefix(index as u64, display_rel_path(display_path))
}

fn display_rel_path(display_path: &str) -> Arc<RelPath> {
    RelPath::new(Path::new(display_path), PathStyle::Posix)
        .unwrap_or_else(|_| {
            RelPath::new(Path::new("untitled"), PathStyle::Posix)
                .expect("static fallback path is repo-relative")
        })
        .into_owned()
        .into()
}

impl EventEmitter<EditorEvent> for CommitDiffPreview {}

impl Focusable for CommitDiffPreview {
    fn focus_handle(&self, cx: &App) -> FocusHandle {
        self.editor.focus_handle(cx)
    }
}

impl Item for CommitDiffPreview {
    type Event = EditorEvent;

    fn tab_icon(&self, _window: &Window, _cx: &App) -> Option<Icon> {
        Some(Icon::new(IconName::Diff).color(Color::Muted))
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, _cx: &App) -> AnyElement {
        Label::new(self.title())
            .color(if params.selected {
                Color::Default
            } else {
                Color::Muted
            })
            .into_any_element()
    }

    fn tab_tooltip_text(&self, _cx: &App) -> Option<ui::SharedString> {
        Some(self.title())
    }

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> SharedString {
        self.title()
    }

    fn to_item_events(event: &EditorEvent, f: &mut dyn FnMut(ItemEvent)) {
        Editor::to_item_events(event, f)
    }

    fn telemetry_event_text(&self) -> Option<&'static str> {
        Some("Commit Diff Preview Opened")
    }

    fn deactivated(&mut self, window: &mut Window, cx: &mut Context<Self>) {
        self.editor
            .update(cx, |editor, cx| editor.deactivated(window, cx));
    }

    fn act_as_type<'a>(
        &'a self,
        type_id: TypeId,
        self_handle: &'a Entity<Self>,
        _: &'a App,
    ) -> Option<gpui::AnyEntity> {
        if type_id == TypeId::of::<Self>() {
            Some(self_handle.clone().into())
        } else if type_id == TypeId::of::<Editor>() {
            Some(self.editor.clone().into())
        } else {
            None
        }
    }

    fn as_searchable(&self, _: &Entity<Self>, _: &App) -> Option<Box<dyn SearchableItemHandle>> {
        Some(Box::new(self.editor.clone()))
    }

    fn active_project_path(&self, cx: &App) -> Option<ProjectPath> {
        self.editor.read(cx).active_project_path(cx)
    }

    fn set_nav_history(
        &mut self,
        nav_history: ItemNavHistory,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, _| {
            editor.set_nav_history(Some(nav_history));
        });
    }

    fn navigate(
        &mut self,
        data: Arc<dyn Any + Send>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> bool {
        self.editor
            .update(cx, |editor, cx| editor.navigate(data, window, cx))
    }

    fn breadcrumb_location(&self, _: &App) -> ToolbarItemLocation {
        ToolbarItemLocation::PrimaryLeft
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        self.editor.breadcrumbs(cx)
    }

    fn added_to_workspace(
        &mut self,
        workspace: &mut Workspace,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.editor.update(cx, |editor, cx| {
            editor.added_to_workspace(workspace, window, cx)
        });
    }

    fn can_save(&self, cx: &App) -> bool {
        self.editor.read(cx).can_save(cx)
    }

    fn save(
        &mut self,
        options: SaveOptions,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> gpui::Task<Result<()>> {
        self.editor
            .update(cx, |editor, cx| editor.save(options, project, window, cx))
    }
}

impl Render for CommitDiffPreview {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        self.editor.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_buffer_file_uses_display_path_for_header_metadata() {
        let file = PreviewBufferFile::new("src/lib.rs", false);

        assert_eq!(
            file.path().display(PathStyle::Posix).to_string(),
            "src/lib.rs"
        );
        assert_eq!(file.file_name, "lib.rs");
        assert_eq!(
            file.disk_state(),
            DiskState::Historic { was_deleted: false }
        );
        assert!(!file.can_open());
    }

    #[test]
    fn preview_buffer_file_marks_deleted_historic_files() {
        let file = PreviewBufferFile::new("src/deleted.rs", true);

        assert_eq!(file.disk_state(), DiskState::Historic { was_deleted: true });
    }

    #[test]
    fn preview_entry_uses_metadata_path_separately_from_display_path() {
        let entry = CommitDiffPreviewFile {
            display_path: "old.rs -> new.rs".into(),
            old_path: Some("old.rs".into()),
            new_path: Some("new.rs".into()),
            status: CommitDiffPreviewStatus::Renamed,
            old_text: Some("old".into()),
            new_text: Some("new".into()),
            is_binary: false,
            is_truncated: false,
            load_error: None,
        }
        .into_entry(7);

        assert_eq!(entry.index, 7);
        assert_eq!(entry.display_path.as_ref(), "old.rs -> new.rs");
        assert_eq!(entry.metadata_path.as_ref(), "new.rs");
        assert!(!entry.was_deleted);
    }

    #[test]
    fn display_path_key_preserves_path_with_sort_prefix() {
        let key = display_path_key(3, "crates/cherrypick-ui/src/app_shell.rs");

        assert_eq!(key.sort_prefix, Some(3));
        assert_eq!(
            key.path.display(PathStyle::Posix).to_string(),
            "crates/cherrypick-ui/src/app_shell.rs"
        );
    }
}
