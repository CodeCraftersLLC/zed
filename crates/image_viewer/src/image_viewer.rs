mod image_info;
mod image_viewer_settings;

use std::{
    mem,
    path::Path,
    sync::{Arc, atomic::AtomicBool},
};

use anyhow::Context as _;
use editor::{EditorSettings, RevealInFileManager, items::entry_git_aware_label_color};
use file_icons::FileIcons;
use gpui::{
    AnyElement, App, Bounds, Context, DispatchPhase, Element, ElementId, Entity, EventEmitter,
    FocusHandle, Focusable, Font, GlobalElementId, InspectorElementId, InteractiveElement,
    IntoElement, LayoutId, MouseButton, MouseDownEvent, MouseMoveEvent, MouseUpEvent,
    ParentElement, PinchEvent, Pixels, Point, Render, ScrollDelta, ScrollWheelEvent, Style, Styled,
    Task, WeakEntity, Window, actions, checkerboard, div, img, point, px, size,
};
use language::File as _;
use persistence::ImageViewerDb;
use project::{
    ImageItem, Project, ProjectPath,
    image_store::{ImageFormat, ImageItemEvent, ImageMetadata},
};
use settings::Settings;
use theme_settings::ThemeSettings;
use ui::{Tooltip, prelude::*};
use util::paths::PathExt;
use workspace::{
    ItemId, ItemSettings, Pane, ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, Workspace,
    WorkspaceId, delete_unloaded_items,
    invalid_item_view::InvalidItemView,
    item::{HighlightedText, Item, ItemHandle, ProjectItem, SerializableItem, TabContentParams},
};

pub use crate::image_info::*;
pub use crate::image_viewer_settings::*;

actions!(
    image_viewer,
    [
        /// Zoom in the image.
        ZoomIn,
        /// Zoom out the image.
        ZoomOut,
        /// Reset zoom to 100%.
        ResetZoom,
        /// Fit the image to view.
        FitToView,
        /// Zoom to actual size (100%).
        ZoomToActualSize
    ]
);

const MIN_ZOOM: f32 = 0.1;
const MAX_ZOOM: f32 = 20.0;
const ZOOM_STEP: f32 = 1.1;
const SCROLL_LINE_MULTIPLIER: f32 = 20.0;
const BASE_SQUARE_SIZE: f32 = 32.0;

/// The longest side of the picture drawn in place of an image that is too large to decode whole.
const REDUCED_SIDE: u32 = 1024;

/// How an image is drawn, decided from its header alone.
#[derive(Debug, PartialEq, Eq)]
enum ImageDraw {
    /// Decoded whole and drawn.
    Full,
    /// Over the raster limits, so a PNG is streamed down to [`REDUCED_SIDE`] and that is drawn.
    Reduced,
    /// Over the raster limits and not a PNG: nothing can be drawn, and the viewer says so.
    TooLarge,
}

/// The limits are the ones `gpui` decodes within: a side longer than the GPU atlas holds, or a
/// raster past the memory a decode may take, is never decoded whole.
fn image_draw(metadata: &ImageMetadata) -> ImageDraw {
    let bytes_per_pixel = metadata.colors.map_or(4, |colors| {
        u64::from(colors.bits_per_pixel()).div_ceil(8).max(4)
    });
    let decoded =
        (u64::from(metadata.width) * u64::from(metadata.height)).saturating_mul(bytes_per_pixel);
    let within_limits = metadata.width <= gpui::MAX_RASTER_SIDE
        && metadata.height <= gpui::MAX_RASTER_SIDE
        && decoded <= gpui::MAX_RASTER_BYTES;
    match (within_limits, metadata.format) {
        (true, _) => ImageDraw::Full,
        (false, ImageFormat::Png) => ImageDraw::Reduced,
        (false, _) => ImageDraw::TooLarge,
    }
}

fn too_large_message(width: u32, height: u32) -> SharedString {
    format!("{width} \u{d7} {height} px is too large to display").into()
}

/// A PNG of `bytes` streamed down to [`REDUCED_SIDE`]; the failure says why the file would not
/// stream.
fn reduced_png(bytes: &[u8]) -> Result<Vec<u8>, String> {
    let thumbnail = gpui::png_thumbnail(bytes, REDUCED_SIDE, &AtomicBool::new(false))
        .map_err(|error| error.to_string())?;
    let mut png = Vec::new();
    image::DynamicImage::ImageRgba8(thumbnail)
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .map_err(|error| error.to_string())?;
    Ok(png)
}

/// What the viewer draws for the image.
enum Shown {
    Picture(Arc<gpui::Image>),
    /// A line of text in place of a picture that cannot be drawn.
    Card(SharedString),
    /// The reduced picture is still being made.
    Waiting,
}

/// The reduced picture of an image over the raster limits.
enum Reduction {
    NotNeeded,
    Pending { _task: Task<()> },
    Ready(Arc<gpui::Image>),
    Failed(SharedString),
}

pub struct ImageView {
    reduction: Reduction,
    image_item: Entity<ImageItem>,
    project: Entity<Project>,
    focus_handle: FocusHandle,
    zoom_level: f32,
    pan_offset: Point<Pixels>,
    last_mouse_position: Option<Point<Pixels>>,
    container_bounds: Option<Bounds<Pixels>>,
    image_size: Option<(u32, u32)>,
}

impl ImageView {
    fn is_dragging(&self) -> bool {
        self.last_mouse_position.is_some()
    }

    pub fn new(
        image_item: Entity<ImageItem>,
        project: Entity<Project>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let image_size = image_item
            .read(cx)
            .image_metadata
            .map(|m| (m.width, m.height));
        Self::attach(
            image_item,
            project,
            1.0,
            Point::default(),
            image_size,
            window,
            cx,
        )
    }

    /// A view of `image_item` with its subscriptions, release cleanup and reduction in place; the
    /// one setup shared by [`Self::new`] and a split.
    fn attach(
        image_item: Entity<ImageItem>,
        project: Entity<Project>,
        zoom_level: f32,
        pan_offset: Point<Pixels>,
        image_size: Option<(u32, u32)>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        cx.subscribe_in(&image_item, window, Self::on_image_event)
            .detach();
        cx.on_release(|this, cx| this.release_reduction(cx))
            .detach();

        let mut view = Self {
            image_item,
            project,
            focus_handle: cx.focus_handle(),
            zoom_level,
            pan_offset,
            last_mouse_position: None,
            container_bounds: None,
            image_size,
            reduction: Reduction::NotNeeded,
        };
        view.start_reduction(cx);
        view
    }

    /// Drops this view's own reduced picture. The full picture belongs to the `ImageItem`, which
    /// releases it when the last holder of that image goes.
    fn release_reduction(&mut self, cx: &mut App) {
        if let Reduction::Ready(image) = mem::replace(&mut self.reduction, Reduction::NotNeeded) {
            image.let_go(cx);
        }
    }

    fn start_reduction(&mut self, cx: &mut Context<Self>) {
        self.release_reduction(cx);
        let item = self.image_item.read(cx);
        let reduced = item
            .image_metadata
            .is_some_and(|metadata| image_draw(&metadata) == ImageDraw::Reduced);
        if !reduced {
            self.reduction = Reduction::NotNeeded;
            return;
        }
        let image = item.image.clone();
        self.reduction = Reduction::Pending {
            _task: cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move { reduced_png(&image.bytes) })
                .await;
            this.update(cx, |this, cx| {
                this.reduction = match result {
                    Ok(png) => {
                        let image = Arc::new(gpui::Image::from_bytes(gpui::ImageFormat::Png, png));
                        image.hold(cx);
                        Reduction::Ready(image)
                    }
                    Err(error) => Reduction::Failed(
                        format!("The image is too large to display and could not be reduced: {error}")
                            .into(),
                    ),
                };
                cx.notify();
            })
            .ok();
            }),
        };
    }

    fn shown(&self, cx: &App) -> Shown {
        let item = self.image_item.read(cx);
        let Some(metadata) = item.image_metadata else {
            return Shown::Picture(item.image.clone());
        };
        match (image_draw(&metadata), &self.reduction) {
            (ImageDraw::Full, _) => Shown::Picture(item.image.clone()),
            (ImageDraw::TooLarge, _) => {
                Shown::Card(too_large_message(metadata.width, metadata.height))
            }
            (ImageDraw::Reduced, Reduction::Ready(image)) => Shown::Picture(image.clone()),
            (ImageDraw::Reduced, Reduction::Failed(message)) => Shown::Card(message.clone()),
            (ImageDraw::Reduced, _) => Shown::Waiting,
        }
    }

    fn on_image_event(
        &mut self,
        _: &Entity<ImageItem>,
        event: &ImageItemEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        match event {
            ImageItemEvent::MetadataUpdated
            | ImageItemEvent::FileHandleChanged
            | ImageItemEvent::Reloaded => {
                self.image_size = self
                    .image_item
                    .read(cx)
                    .image_metadata
                    .map(|m| (m.width, m.height));
                self.start_reduction(cx);
                cx.emit(ImageViewEvent::TitleChanged);
                cx.notify();
            }
            ImageItemEvent::ReloadNeeded => {}
        }
    }

    fn zoom_in(&mut self, _: &ZoomIn, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level * ZOOM_STEP, None, cx);
    }

    fn zoom_out(&mut self, _: &ZoomOut, _window: &mut Window, cx: &mut Context<Self>) {
        self.set_zoom(self.zoom_level / ZOOM_STEP, None, cx);
    }

    fn reset_zoom(&mut self, _: &ResetZoom, _window: &mut Window, cx: &mut Context<Self>) {
        self.zoom_level = 1.0;
        self.pan_offset = Point::default();
        cx.notify();
    }

    fn fit_to_view(&mut self, _: &FitToView, _window: &mut Window, cx: &mut Context<Self>) {
        if let Some((bounds, image_size)) = self.container_bounds.zip(self.image_size) {
            self.zoom_level = ImageView::compute_fit_to_view_zoom(bounds, image_size);
            self.pan_offset = Point::default();
            cx.notify();
        }
    }

    fn compute_fit_to_view_zoom(container_bounds: Bounds<Pixels>, image_size: (u32, u32)) -> f32 {
        let (image_width, image_height) = image_size;
        let container_width: f32 = container_bounds.size.width.into();
        let container_height: f32 = container_bounds.size.height.into();
        let scale_x = container_width / image_width as f32;
        let scale_y = container_height / image_height as f32;
        scale_x.min(scale_y).min(1.0)
    }

    fn zoom_to_actual_size(
        &mut self,
        _: &ZoomToActualSize,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.zoom_level = 1.0;
        self.pan_offset = Point::default();
        cx.notify();
    }

    fn reveal_in_file_manager(
        &mut self,
        _: &RevealInFileManager,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if let Some(path) = self.image_item.read(cx).abs_path(cx) {
            self.project
                .update(cx, |project, cx| project.reveal_path(&path, cx));
        }
    }

    fn set_zoom(
        &mut self,
        new_zoom: f32,
        zoom_center: Option<Point<Pixels>>,
        cx: &mut Context<Self>,
    ) {
        let old_zoom = self.zoom_level;
        self.zoom_level = new_zoom.clamp(MIN_ZOOM, MAX_ZOOM);

        if let Some((center, bounds)) = zoom_center.zip(self.container_bounds) {
            let relative_center = point(
                center.x - bounds.origin.x - bounds.size.width / 2.0,
                center.y - bounds.origin.y - bounds.size.height / 2.0,
            );

            let mouse_offset_from_image = relative_center - self.pan_offset;

            let zoom_ratio = self.zoom_level / old_zoom;

            self.pan_offset += mouse_offset_from_image * (1.0 - zoom_ratio);
        }

        cx.notify();
    }

    fn handle_scroll_wheel(
        &mut self,
        event: &ScrollWheelEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.modifiers.control || event.modifiers.platform {
            let delta: f32 = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels.y.into(),
                ScrollDelta::Lines(lines) => lines.y * SCROLL_LINE_MULTIPLIER,
            };
            let zoom_factor = if delta > 0.0 {
                1.0 + delta.abs() * 0.01
            } else {
                1.0 / (1.0 + delta.abs() * 0.01)
            };
            self.set_zoom(self.zoom_level * zoom_factor, Some(event.position), cx);
        } else {
            let delta = match event.delta {
                ScrollDelta::Pixels(pixels) => pixels,
                ScrollDelta::Lines(lines) => lines.map(|d| px(d * SCROLL_LINE_MULTIPLIER)),
            };
            self.pan_offset += delta;
            cx.notify();
        }
    }

    fn handle_mouse_down(
        &mut self,
        event: &MouseDownEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if event.button == MouseButton::Left || event.button == MouseButton::Middle {
            self.last_mouse_position = Some(event.position);
            cx.notify();
        }
    }

    fn handle_mouse_up(
        &mut self,
        _event: &MouseUpEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.last_mouse_position = None;
        cx.notify();
    }

    fn handle_mouse_move(
        &mut self,
        event: &MouseMoveEvent,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if self.is_dragging() {
            if let Some(last_pos) = self.last_mouse_position {
                let delta = event.position - last_pos;
                self.pan_offset += delta;
            }
            self.last_mouse_position = Some(event.position);
            cx.notify();
        }
    }

    fn handle_pinch(&mut self, event: &PinchEvent, _window: &mut Window, cx: &mut Context<Self>) {
        let zoom_factor = 1.0 + event.delta;
        self.set_zoom(self.zoom_level * zoom_factor, Some(event.position), cx);
    }
}

struct ImageContentElement {
    image_view: Entity<ImageView>,
}

impl ImageContentElement {
    fn new(image_view: Entity<ImageView>) -> Self {
        Self { image_view }
    }
}

impl IntoElement for ImageContentElement {
    type Element = Self;

    fn into_element(self) -> Self::Element {
        self
    }
}

impl Element for ImageContentElement {
    type RequestLayoutState = ();
    type PrepaintState = Option<(AnyElement, bool)>;

    fn id(&self) -> Option<ElementId> {
        None
    }

    fn source_location(&self) -> Option<&'static core::panic::Location<'static>> {
        None
    }

    fn request_layout(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        window: &mut Window,
        cx: &mut App,
    ) -> (LayoutId, Self::RequestLayoutState) {
        (
            window.request_layout(
                Style {
                    size: size(relative(1.).into(), relative(1.).into()),
                    ..Default::default()
                },
                [],
                cx,
            ),
            (),
        )
    }

    fn prepaint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        window: &mut Window,
        cx: &mut App,
    ) -> Self::PrepaintState {
        let mut shown = self.image_view.read(cx).shown(cx);
        if let Shown::Picture(image) = &shown
            && let Some(Err(error)) = image.clone().use_render_image_result(window, cx)
        {
            shown = Shown::Card(format!("The image could not be displayed: {error}").into());
        }
        let image_view = self.image_view.read(cx);

        let first_layout = image_view.container_bounds.is_none();

        let initial_zoom_level = first_layout
            .then(|| {
                image_view
                    .image_size
                    .map(|image_size| ImageView::compute_fit_to_view_zoom(bounds, image_size))
            })
            .flatten();

        let zoom_level = initial_zoom_level.unwrap_or(image_view.zoom_level);

        let pan_offset = image_view.pan_offset;
        let border_color = cx.theme().colors().border;

        let is_dragging = image_view.is_dragging();

        let scaled_size = image_view
            .image_size
            .map(|(w, h)| (px(w as f32 * zoom_level), px(h as f32 * zoom_level)));

        let (mut left, mut top) = (px(0.0), px(0.0));
        let mut scaled_width = px(0.0);
        let mut scaled_height = px(0.0);

        if let Some((width, height)) = scaled_size {
            scaled_width = width;
            scaled_height = height;

            let center_x = bounds.size.width / 2.0;
            let center_y = bounds.size.height / 2.0;

            left = center_x - (scaled_width / 2.0) + pan_offset.x;
            top = center_y - (scaled_height / 2.0) + pan_offset.y;
        }

        self.image_view.update(cx, |this, _| {
            this.container_bounds = Some(bounds);
            if let Some(initial_zoom_level) = initial_zoom_level {
                this.zoom_level = initial_zoom_level;
            }
        });

        let mut image_content = match shown {
            Shown::Picture(image) => div()
                .relative()
                .size_full()
                .child(
                    div()
                        .absolute()
                        .left(left)
                        .top(top)
                        .w(scaled_width)
                        .h(scaled_height)
                        .child(
                            div()
                                .size_full()
                                .absolute()
                                .top_0()
                                .left_0()
                                .child(div().size_full().bg(checkerboard(
                                    cx.theme().colors().panel_background,
                                    BASE_SQUARE_SIZE * zoom_level,
                                )))
                                .border_1()
                                .border_color(border_color),
                        )
                        .child({
                            img(image)
                                .id(("image-viewer-image", self.image_view.entity_id()))
                                .size_full()
                        }),
                )
                .into_any_element(),
            Shown::Card(message) => div()
                .size_full()
                .flex()
                .items_center()
                .justify_center()
                .p_4()
                .child(Label::new(message).color(Color::Muted))
                .into_any_element(),
            Shown::Waiting => div().size_full().into_any_element(),
        };

        image_content.prepaint_as_root(bounds.origin, bounds.size.into(), window, cx);
        Some((image_content, is_dragging))
    }

    fn paint(
        &mut self,
        _id: Option<&GlobalElementId>,
        _inspector_id: Option<&InspectorElementId>,
        _bounds: Bounds<Pixels>,
        _request_layout: &mut Self::RequestLayoutState,
        prepaint: &mut Self::PrepaintState,
        window: &mut Window,
        cx: &mut App,
    ) {
        let Some((mut element, is_dragging)) = prepaint.take() else {
            return;
        };

        if is_dragging {
            let image_view = self.image_view.downgrade();
            window.on_mouse_event(move |_event: &MouseUpEvent, phase, _window, cx| {
                if phase == DispatchPhase::Bubble
                    && let Some(entity) = image_view.upgrade()
                {
                    entity.update(cx, |this, cx| {
                        this.last_mouse_position = None;
                        cx.notify();
                    });
                }
            });
        }

        element.paint(window, cx);
    }
}

pub enum ImageViewEvent {
    TitleChanged,
}

impl EventEmitter<ImageViewEvent> for ImageView {}

impl Item for ImageView {
    type Event = ImageViewEvent;

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(workspace::item::ItemEvent)) {
        match event {
            ImageViewEvent::TitleChanged => {
                f(workspace::item::ItemEvent::UpdateTab);
                f(workspace::item::ItemEvent::UpdateBreadcrumbs);
            }
        }
    }

    fn for_each_project_item(
        &self,
        cx: &App,
        f: &mut dyn FnMut(gpui::EntityId, &dyn project::ProjectItem),
    ) {
        f(self.image_item.entity_id(), self.image_item.read(cx))
    }

    fn tab_tooltip_text(&self, cx: &App) -> Option<SharedString> {
        let abs_path = self.image_item.read(cx).abs_path(cx)?;
        let file_path = abs_path.compact().to_string_lossy().into_owned();
        Some(
            util::display_name::display_name(&file_path)
                .into_owned()
                .into(),
        )
    }

    fn tab_content(&self, params: TabContentParams, _window: &Window, cx: &App) -> AnyElement {
        let project_path = self.image_item.read(cx).project_path(cx);

        let label_color = if ItemSettings::get_global(cx).git_status {
            let git_status = self
                .project
                .read(cx)
                .project_path_git_status(&project_path, cx)
                .map(|status| status.summary())
                .unwrap_or_default();

            self.project
                .read(cx)
                .entry_for_path(&project_path, cx)
                .map(|entry| {
                    entry_git_aware_label_color(git_status, entry.is_ignored, params.selected)
                })
                .unwrap_or_else(|| params.text_color())
        } else {
            params.text_color()
        };

        Label::new(self.tab_content_text(params.detail.unwrap_or_default(), cx))
            .single_line()
            .color(label_color)
            .when(params.preview, |this| this.italic())
            .into_any_element()
    }

    fn tab_content_text(&self, _: usize, cx: &App) -> SharedString {
        util::display_name::display_name(self.image_item.read(cx).file.file_name(cx))
            .into_owned()
            .into()
    }

    /// The real name: Save As proposes what is on disk, not what the display hook draws.
    fn suggested_filename(&self, cx: &App) -> SharedString {
        self.image_item
            .read(cx)
            .file
            .file_name(cx)
            .to_string()
            .into()
    }

    fn tab_icon(&self, _: &Window, cx: &App) -> Option<Icon> {
        let path = self.image_item.read(cx).abs_path(cx)?;
        ItemSettings::get_global(cx)
            .file_icons
            .then(|| FileIcons::get_icon(&path, cx))
            .flatten()
            .map(Icon::from_path)
    }

    fn breadcrumb_location(&self, cx: &App) -> ToolbarItemLocation {
        let show_breadcrumb = EditorSettings::get_global(cx).toolbar.breadcrumbs;
        if show_breadcrumb {
            ToolbarItemLocation::PrimaryLeft
        } else {
            ToolbarItemLocation::Hidden
        }
    }

    fn breadcrumbs(&self, cx: &App) -> Option<(Vec<HighlightedText>, Option<Font>)> {
        let text = breadcrumbs_text_for_image(self.project.read(cx), self.image_item.read(cx), cx);
        let font = ThemeSettings::get_global(cx).buffer_font.clone();

        Some((
            vec![HighlightedText {
                text: util::display_name::display_name(&text).into_owned().into(),
                highlights: vec![],
            }],
            Some(font),
        ))
    }

    fn can_split(&self) -> bool {
        true
    }

    fn clone_on_split(
        &self,
        _workspace_id: Option<WorkspaceId>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Task<Option<Entity<Self>>>
    where
        Self: Sized,
    {
        Task::ready(Some(cx.new(|cx| {
            Self::attach(
                self.image_item.clone(),
                self.project.clone(),
                self.zoom_level,
                self.pan_offset,
                self.image_size,
                window,
                cx,
            )
        })))
    }

    fn has_deleted_file(&self, cx: &App) -> bool {
        self.image_item.read(cx).file.disk_state().is_deleted()
    }
    fn buffer_kind(&self, _: &App) -> workspace::item::ItemBufferKind {
        workspace::item::ItemBufferKind::Singleton
    }
}

fn breadcrumbs_text_for_image(project: &Project, image: &ImageItem, cx: &App) -> String {
    let mut path = image.file.path().clone();
    if project.visible_worktrees(cx).count() > 1
        && let Some(worktree) = project.worktree_for_id(image.project_path(cx).worktree_id, cx)
    {
        path = worktree.read(cx).root_name().join(&path);
    }

    path.display(project.path_style(cx)).to_string()
}

impl SerializableItem for ImageView {
    fn serialized_item_kind() -> &'static str {
        "ImageView"
    }

    fn deserialize(
        project: Entity<Project>,
        _workspace: WeakEntity<Workspace>,
        workspace_id: WorkspaceId,
        item_id: ItemId,
        window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<Entity<Self>>> {
        let db = ImageViewerDb::global(cx);
        window.spawn(cx, async move |cx| {
            let image_path = db
                .get_image_path(item_id, workspace_id)?
                .context("No image path found")?;

            let (worktree, relative_path) = project
                .update(cx, |project, cx| {
                    project.find_or_create_worktree(image_path.clone(), false, cx)
                })
                .await
                .context("Path not found")?;
            let worktree_id = worktree.update(cx, |worktree, _cx| worktree.id());

            let project_path = ProjectPath {
                worktree_id,
                path: relative_path,
            };

            let image_item = project
                .update(cx, |project, cx| project.open_image(project_path, cx))
                .await?;

            cx.update(
                |window, cx| Ok(cx.new(|cx| ImageView::new(image_item, project, window, cx))),
            )?
        })
    }

    fn cleanup(
        workspace_id: WorkspaceId,
        alive_items: Vec<ItemId>,
        _window: &mut Window,
        cx: &mut App,
    ) -> Task<anyhow::Result<()>> {
        let db = ImageViewerDb::global(cx);
        delete_unloaded_items(alive_items, workspace_id, "image_viewers", &db, cx)
    }

    fn serialize(
        &mut self,
        workspace: &mut Workspace,
        item_id: ItemId,
        _closing: bool,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Option<Task<anyhow::Result<()>>> {
        let workspace_id = workspace.database_id()?;
        let image_path = self.image_item.read(cx).abs_path(cx)?;

        let db = ImageViewerDb::global(cx);
        Some(cx.background_spawn({
            async move {
                log::debug!("Saving image at path {image_path:?}");
                db.save_image_path(item_id, workspace_id, image_path).await
            }
        }))
    }

    fn should_serialize(&self, _event: &Self::Event) -> bool {
        false
    }
}

impl EventEmitter<()> for ImageView {}
impl Focusable for ImageView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for ImageView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .track_focus(&self.focus_handle(cx))
            .key_context("ImageViewer")
            .on_action(cx.listener(Self::zoom_in))
            .on_action(cx.listener(Self::zoom_out))
            .on_action(cx.listener(Self::reset_zoom))
            .on_action(cx.listener(Self::fit_to_view))
            .on_action(cx.listener(Self::zoom_to_actual_size))
            .on_action(cx.listener(Self::reveal_in_file_manager))
            .size_full()
            .relative()
            .bg(cx.theme().colors().editor_background)
            .child({
                let container = div()
                    .id("image-container")
                    .size_full()
                    .overflow_hidden()
                    .cursor(if self.is_dragging() {
                        gpui::CursorStyle::ClosedHand
                    } else {
                        gpui::CursorStyle::OpenHand
                    })
                    .on_scroll_wheel(cx.listener(Self::handle_scroll_wheel))
                    .on_pinch(cx.listener(Self::handle_pinch))
                    .on_mouse_down(MouseButton::Left, cx.listener(Self::handle_mouse_down))
                    .on_mouse_down(MouseButton::Middle, cx.listener(Self::handle_mouse_down))
                    .on_mouse_up(MouseButton::Left, cx.listener(Self::handle_mouse_up))
                    .on_mouse_up(MouseButton::Middle, cx.listener(Self::handle_mouse_up))
                    .on_mouse_move(cx.listener(Self::handle_mouse_move))
                    .child(ImageContentElement::new(cx.entity()));

                container
            })
    }
}

impl ProjectItem for ImageView {
    type Item = ImageItem;

    fn for_project_item(
        project: Entity<Project>,
        _: Option<&Pane>,
        item: Entity<Self::Item>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self
    where
        Self: Sized,
    {
        Self::new(item, project, window, cx)
    }

    fn for_broken_project_item(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        window: &mut Window,
        cx: &mut App,
    ) -> Option<InvalidItemView>
    where
        Self: Sized,
    {
        Some(InvalidItemView::new(abs_path, is_local, e, window, cx))
    }
}

pub struct ImageViewToolbarControls {
    image_view: Option<WeakEntity<ImageView>>,
    _subscription: Option<gpui::Subscription>,
}

impl ImageViewToolbarControls {
    pub fn new() -> Self {
        Self {
            image_view: None,
            _subscription: None,
        }
    }
}

impl Render for ImageViewToolbarControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(image_view) = self.image_view.as_ref().and_then(|v| v.upgrade()) else {
            return div().into_any_element();
        };

        let zoom_level = image_view.read(cx).zoom_level;
        let zoom_percentage = format!("{}%", (zoom_level * 100.0).round() as i32);

        h_flex()
            .gap_1()
            .child(
                IconButton::new("zoom-out", IconName::Dash)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Zoom Out", &ZoomOut, cx))
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.zoom_out(&ZoomOut, window, cx);
                                });
                            }
                        }
                    }),
            )
            .child(
                Button::new("zoom-level", zoom_percentage)
                    .label_size(LabelSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Reset Zoom", &ResetZoom, cx))
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.reset_zoom(&ResetZoom, window, cx);
                                });
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("zoom-in", IconName::Plus)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Zoom In", &ZoomIn, cx))
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.zoom_in(&ZoomIn, window, cx);
                                });
                            }
                        }
                    }),
            )
            .child(
                IconButton::new("fit-to-view", IconName::Maximize)
                    .icon_size(IconSize::Small)
                    .tooltip(|_window, cx| Tooltip::for_action("Fit to View", &FitToView, cx))
                    .on_click({
                        let image_view = image_view.downgrade();
                        move |_, window, cx| {
                            if let Some(view) = image_view.upgrade() {
                                view.update(cx, |this, cx| {
                                    this.fit_to_view(&FitToView, window, cx);
                                });
                            }
                        }
                    }),
            )
            .into_any_element()
    }
}

impl EventEmitter<ToolbarItemEvent> for ImageViewToolbarControls {}

impl ToolbarItemView for ImageViewToolbarControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.image_view = None;
        self._subscription = None;

        if let Some(item) = active_pane_item.and_then(|i| i.downcast::<ImageView>()) {
            self._subscription = Some(cx.observe(&item, |_, _, cx| {
                cx.notify();
            }));
            self.image_view = Some(item.downgrade());
            cx.notify();
            return ToolbarItemLocation::PrimaryRight;
        }

        ToolbarItemLocation::Hidden
    }
}

pub fn init(cx: &mut App) {
    workspace::register_project_item::<ImageView>(cx);
    workspace::register_serializable_item::<ImageView>(cx);
}

mod persistence {
    use std::path::PathBuf;

    use db::{
        query,
        sqlez::{domain::Domain, thread_safe_connection::ThreadSafeConnection},
        sqlez_macros::sql,
    };
    use workspace::{ItemId, WorkspaceDb, WorkspaceId};

    pub struct ImageViewerDb(ThreadSafeConnection);

    impl Domain for ImageViewerDb {
        const NAME: &str = stringify!(ImageViewerDb);

        const MIGRATIONS: &[&str] = &[sql!(
                CREATE TABLE image_viewers (
                    workspace_id INTEGER,
                    item_id INTEGER UNIQUE,

                    image_path BLOB,

                    PRIMARY KEY(workspace_id, item_id),
                    FOREIGN KEY(workspace_id) REFERENCES workspaces(workspace_id)
                    ON DELETE CASCADE
                ) STRICT;
        )];
    }

    db::static_connection!(ImageViewerDb, [WorkspaceDb]);

    impl ImageViewerDb {
        query! {
            pub async fn save_image_path(
                item_id: ItemId,
                workspace_id: WorkspaceId,
                image_path: PathBuf
            ) -> Result<()> {
                INSERT OR REPLACE INTO image_viewers(item_id, workspace_id, image_path)
                VALUES (?, ?, ?)
            }
        }

        query! {
            pub fn get_image_path(item_id: ItemId, workspace_id: WorkspaceId) -> Result<Option<PathBuf>> {
                SELECT image_path
                FROM image_viewers
                WHERE item_id = ? AND workspace_id = ?
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use fs::Fs as _;
    use gpui::{TestAppContext, VisualTestContext};
    use project::image_store::ImageColorInfo;

    fn metadata(width: u32, height: u32, format: ImageFormat) -> ImageMetadata {
        ImageMetadata {
            width,
            height,
            file_size: 1,
            colors: Some(ImageColorInfo {
                channels: 4,
                bits_per_channel: 8,
            }),
            format,
        }
    }

    /// Header dimensions whose decoded size does not fit in 64 bits must read as over the limit,
    /// not wrap past it.
    #[test]
    fn a_decoded_size_that_overflows_is_over_the_limit() {
        assert_eq!(
            image_draw(&metadata(u32::MAX, u32::MAX, ImageFormat::Jpeg)),
            ImageDraw::TooLarge
        );
        assert_eq!(
            image_draw(&metadata(u32::MAX, u32::MAX, ImageFormat::Png)),
            ImageDraw::Reduced
        );
    }

    #[test]
    fn an_image_over_the_raster_limits_is_reduced_if_it_is_a_png_and_a_card_otherwise() {
        let side = gpui::MAX_RASTER_SIDE;
        assert_eq!(
            image_draw(&metadata(800, 600, ImageFormat::Jpeg)),
            ImageDraw::Full
        );
        assert_eq!(
            image_draw(&metadata(side + 1, 10, ImageFormat::Png)),
            ImageDraw::Reduced
        );
        assert_eq!(
            image_draw(&metadata(10, side + 1, ImageFormat::Jpeg)),
            ImageDraw::TooLarge
        );
        let wide_and_tall = metadata(side, side, ImageFormat::Bmp);
        assert_eq!(
            image_draw(&wide_and_tall),
            ImageDraw::TooLarge,
            "within the side limit but past the memory a decode may take"
        );
        assert_eq!(
            too_large_message(20_000, 5),
            "20000 \u{d7} 5 px is too large to display"
        );
    }

    #[test]
    fn a_png_is_reduced_to_a_png_and_a_broken_one_says_why() {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2048, 4))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        let reduced = reduced_png(&png).unwrap();
        let (width, height) = image::load_from_memory(&reduced)
            .map(|picture| (picture.width(), picture.height()))
            .unwrap();
        assert_eq!(width, REDUCED_SIDE);
        assert!(height >= 1);
        assert!(!reduced_png(&png[..png.len() / 2]).unwrap_err().is_empty());
    }

    fn small_png() -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(2, 2))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    fn wide_png() -> Vec<u8> {
        let mut png = Vec::new();
        image::DynamicImage::ImageRgba8(image::RgbaImage::new(gpui::MAX_RASTER_SIDE + 1, 1))
            .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
            .unwrap();
        png
    }

    async fn open_image(
        cx: &mut TestAppContext,
        name: &str,
        bytes: Vec<u8>,
    ) -> (Entity<Project>, Entity<ImageItem>) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });
        let fs = fs::FakeFs::new(cx.executor());
        let path = std::path::Path::new("/root").join(name);
        fs.create_dir(path.parent().unwrap()).await.unwrap();
        fs.insert_file(path, bytes).await;
        let project = Project::test(fs, ["/root".as_ref()], cx).await;
        let image_item = project
            .update(cx, |project, cx| {
                let worktree_id = project.worktrees(cx).next().unwrap().read(cx).id();
                let path = ProjectPath {
                    worktree_id,
                    path: util::rel_path::RelPath::unix(name).unwrap().into(),
                };
                project.open_image(path, cx)
            })
            .await
            .unwrap();
        (project, image_item)
    }

    #[gpui::test]
    async fn tab_tooltip_and_breadcrumbs_use_the_display_name_but_save_as_keeps_the_real_name(
        cx: &mut TestAppContext,
    ) {
        let (project, image_item) = open_image(cx, "we\u{200B}ird.png", small_png()).await;
        let (view, cx) =
            cx.add_window_view(|window, cx| ImageView::new(image_item, project, window, cx));

        let hook = util::display_name::install_hook_for_test(Arc::new(|name: &str| {
            std::borrow::Cow::Owned(name.replace('\u{200B}', "<U+200B>"))
        }));
        let (tab, tooltip, breadcrumbs, suggested) = view.read_with(cx, |view, cx| {
            (
                view.tab_content_text(0, cx),
                view.tab_tooltip_text(cx).unwrap(),
                view.breadcrumbs(cx).unwrap().0,
                view.suggested_filename(cx),
            )
        });
        drop(hook);

        assert_eq!(tab, "we<U+200B>ird.png");
        assert!(tooltip.ends_with("we<U+200B>ird.png"), "{tooltip}");
        assert_eq!(breadcrumbs[0].text.as_ref(), "we<U+200B>ird.png");
        assert_eq!(suggested, "we\u{200B}ird.png");
    }

    /// A workspace window whose pane shows `dir/we<ZWSP>ird.png` and has a breadcrumbs toolbar
    /// item, while a display name hook that escapes the zero width space is installed.
    async fn weird_image_in_a_workspace(
        cx: &mut TestAppContext,
    ) -> (&mut VisualTestContext, util::display_name::TestHook) {
        let hook = util::display_name::install_hook_for_test(Arc::new(|name: &str| {
            std::borrow::Cow::Owned(name.replace('\u{200B}', "<U+200B>"))
        }));
        let (project, image_item) = open_image(cx, "dir/we\u{200B}ird.png", small_png()).await;
        cx.update(editor::init);
        let (workspace, cx) =
            cx.add_window_view(|window, cx| Workspace::test_new(project.clone(), window, cx));
        workspace.update_in(cx, |workspace, window, cx| {
            let breadcrumbs = cx.new(|_| breadcrumbs::Breadcrumbs::new());
            workspace.active_pane().update(cx, |pane, cx| {
                pane.toolbar()
                    .update(cx, |toolbar, cx| toolbar.add_item(breadcrumbs, window, cx))
            });
            let view = cx.new(|cx| ImageView::new(image_item, project, window, cx));
            workspace.add_item_to_active_pane(Box::new(view), None, true, window, cx);
        });
        cx.run_until_parked();
        (cx, hook)
    }

    fn assert_no_raw_zero_width_space(drawn: &[SharedString]) {
        assert!(
            !drawn.iter().any(|text| text.contains('\u{200B}')),
            "{drawn:?}"
        );
    }

    #[gpui::test]
    async fn the_drawn_tab_title_is_the_display_name(cx: &mut TestAppContext) {
        let (cx, _hook) = weird_image_in_a_workspace(cx).await;
        let drawn = cx.redraw_text();
        assert!(
            drawn.iter().any(|text| text == "we<U+200B>ird.png"),
            "{drawn:?}"
        );
        assert_no_raw_zero_width_space(&drawn);
    }

    #[gpui::test]
    async fn the_drawn_tab_tooltip_is_the_display_name(cx: &mut TestAppContext) {
        let (cx, _hook) = weird_image_in_a_workspace(cx).await;
        let tab = cx.debug_bounds("TAB-0").expect("the tab was drawn");
        cx.simulate_mouse_move(tab.center(), None, gpui::Modifiers::none());
        cx.executor()
            .advance_clock(std::time::Duration::from_secs(1));
        cx.run_until_parked();

        let drawn = cx.redraw_text();
        assert!(
            drawn
                .iter()
                .any(|text| text == util::path!("/root/dir/we<U+200B>ird.png")),
            "{drawn:?}"
        );
        assert_no_raw_zero_width_space(&drawn);
    }

    #[gpui::test]
    async fn the_drawn_breadcrumb_is_the_display_name(cx: &mut TestAppContext) {
        let (cx, _hook) = weird_image_in_a_workspace(cx).await;
        let drawn = cx.redraw_text();
        assert!(
            drawn
                .iter()
                .any(|text| text.ends_with("dir/we<U+200B>ird.png")),
            "{drawn:?}"
        );
        assert_no_raw_zero_width_space(&drawn);
    }

    #[gpui::test]
    async fn replacing_the_file_on_disk_refreshes_the_metadata_and_the_draw_decision(
        cx: &mut TestAppContext,
    ) {
        let (project, image_item) = open_image(cx, "swap.png", small_png()).await;
        let (view, cx) = cx.add_window_view(|window, cx| {
            ImageView::new(image_item.clone(), project.clone(), window, cx)
        });
        view.read_with(cx, |view, _| {
            assert_eq!(view.image_size, Some((2, 2)));
            assert!(matches!(view.reduction, Reduction::NotNeeded));
        });

        let fs = project.read_with(cx, |project, _| project.fs().clone());
        fs.as_fake()
            .insert_file(util::path!("/root/swap.png"), wide_png())
            .await;
        cx.run_until_parked();

        let width = gpui::MAX_RASTER_SIDE + 1;
        image_item.read_with(cx, |item, _| {
            let metadata = item.image_metadata.expect("metadata");
            assert_eq!((metadata.width, metadata.height), (width, 1));
        });
        view.read_with(cx, |view, _| {
            assert_eq!(view.image_size, Some((width, 1)));
            assert!(matches!(view.reduction, Reduction::Ready(_)));
        });
    }

    #[gpui::test]
    async fn a_split_view_starts_a_new_reduction_when_the_image_reloads(cx: &mut TestAppContext) {
        let (project, image_item) = open_image(cx, "wide.png", wide_png()).await;

        let (view, cx) = cx.add_window_view(|window, cx| {
            ImageView::new(image_item.clone(), project.clone(), window, cx)
        });
        cx.run_until_parked();
        view.read_with(cx, |view, _| {
            assert!(matches!(view.reduction, Reduction::Ready(_)))
        });

        let split = view
            .update_in(cx, |view, window, cx| view.clone_on_split(None, window, cx))
            .await
            .unwrap();
        cx.run_until_parked();
        split.read_with(cx, |split, _| {
            assert!(matches!(split.reduction, Reduction::Ready(_)))
        });

        image_item.update(cx, |_, cx| cx.emit(ImageItemEvent::Reloaded));
        split.read_with(cx, |split, _| {
            assert!(
                matches!(split.reduction, Reduction::Pending { .. }),
                "the split must follow the item's reloads like the original"
            )
        });
        cx.run_until_parked();
        split.read_with(cx, |split, _| {
            assert!(matches!(split.reduction, Reduction::Ready(_)))
        });
    }

    #[gpui::test]
    async fn the_picture_is_dropped_when_the_last_view_of_it_closes_and_not_before(
        cx: &mut TestAppContext,
    ) {
        let (project, image_item) = open_image(cx, "shared.png", small_png()).await;
        let window = cx.add_empty_window();
        let view = window.update(|window, cx| {
            cx.new(|cx| ImageView::new(image_item.clone(), project.clone(), window, cx))
        });
        let split = view
            .update_in(window, |view, window, cx| {
                view.clone_on_split(None, window, cx)
            })
            .await
            .unwrap();
        let image = image_item.read_with(window, |item, _| item.image.clone());
        window.update(|window, cx| image.clone().get_render_image(window, cx));
        window.run_until_parked();
        let decoded = window.update(|_, cx| image.clone().decoded(cx).expect("decoded"));

        drop(split);
        window.update(|_, _| {});
        window.update(|_, cx| {
            let still = image.clone().decoded(cx);
            assert!(
                still.is_some_and(|still| Arc::ptr_eq(&still, &decoded)),
                "closing a split dropped the picture the other view shows, which decodes again"
            )
        });

        drop(view);
        drop(image_item);
        window.update(|_, _| {});
        window.update(|_, cx| {
            assert!(
                image.clone().decoded(cx).is_none(),
                "the last view left the picture behind"
            )
        });
    }

    fn view_in(
        window: &mut gpui::VisualTestContext,
        item: &Entity<ImageItem>,
        project: &Entity<Project>,
    ) -> Entity<ImageView> {
        window.update(|window, cx| {
            cx.new(|cx| ImageView::new(item.clone(), project.clone(), window, cx))
        })
    }

    fn decode(window: &mut gpui::VisualTestContext, image: &Arc<gpui::Image>) {
        window.update(|window, cx| image.clone().get_render_image(window, cx));
        window.run_until_parked();
    }

    fn decoded(
        window: &mut gpui::VisualTestContext,
        image: &Arc<gpui::Image>,
    ) -> Option<Arc<gpui::RenderImage>> {
        window.update(|_, cx| image.clone().decoded(cx))
    }

    #[gpui::test]
    async fn the_picture_is_dropped_when_the_last_view_closes_after_another_window_closed(
        cx: &mut TestAppContext,
    ) {
        let (project, item) = open_image(cx, "two.png", small_png()).await;
        let mut first_app = cx.clone();
        let first_window = first_app.add_empty_window();
        let first = view_in(first_window, &item, &project);
        let second_window = cx.add_empty_window();
        let second = view_in(second_window, &item, &project);
        let image = item.read_with(second_window, |item, _| item.image.clone());
        decode(second_window, &image);

        first_window.update(|window, _| window.remove_window());
        drop(first);
        second_window.update(|_, _| {});
        assert!(decoded(second_window, &image).is_some());

        drop(second);
        second_window.update(|_, _| {});
        drop(item);
        second_window.update(|_, _| {});
        assert!(
            decoded(second_window, &image).is_none(),
            "the picture outlived every view"
        );
    }

    #[gpui::test]
    async fn closing_a_view_keeps_a_picture_another_item_with_the_same_bytes_shows(
        cx: &mut TestAppContext,
    ) {
        let (project_a, item_a) = open_image(cx, "a.png", small_png()).await;
        let (project_b, item_b) = open_image(cx, "b.png", small_png()).await;
        let window = cx.add_empty_window();
        let view_a = view_in(window, &item_a, &project_a);
        let view_b = view_in(window, &item_b, &project_b);
        let image = item_a.read_with(window, |item, _| item.image.clone());
        decode(window, &image);
        let before = decoded(window, &image).expect("decoded");

        drop(view_b);
        drop(item_b);
        window.update(|_, _| {});
        let after = decoded(window, &image);
        assert!(
            after.is_some_and(|after| Arc::ptr_eq(&after, &before)),
            "closing one item dropped the picture the other still shows"
        );
        drop(view_a);
    }

    #[gpui::test]
    async fn a_reload_with_other_bytes_releases_the_picture_it_displaced(cx: &mut TestAppContext) {
        let (project, item) = open_image(cx, "swap2.png", small_png()).await;
        let window = cx.add_empty_window();
        let _view = view_in(window, &item, &project);
        let displaced = item.read_with(window, |item, _| item.image.clone());
        decode(window, &displaced);
        assert!(decoded(window, &displaced).is_some());

        let fs = project.read_with(window, |project, _| project.fs().clone());
        fs.as_fake()
            .insert_file(util::path!("/root/swap2.png"), wide_png())
            .await;
        window.run_until_parked();
        window.update(|_, _| {});

        let current = item.read_with(window, |item, _| item.image.clone());
        assert!(
            !Arc::ptr_eq(&current, &displaced),
            "the file was not reloaded"
        );
        assert!(
            decoded(window, &displaced).is_none(),
            "the displaced picture stayed cached"
        );
    }

    #[gpui::test]
    async fn a_reduction_is_released_when_its_window_closed_before_its_view(
        cx: &mut TestAppContext,
    ) {
        let (project, item) = open_image(cx, "wide2.png", wide_png()).await;
        let mut app = cx.clone();
        let window = app.add_empty_window();
        let view = view_in(window, &item, &project);
        window.run_until_parked();
        let reduction = view.read_with(window, |view, _| match &view.reduction {
            Reduction::Ready(image) => image.clone(),
            _ => panic!("the picture was not reduced"),
        });
        decode(window, &reduction);
        assert!(decoded(window, &reduction).is_some());

        window.update(|window, _| window.remove_window());
        drop(view);
        cx.update(|_| {});
        cx.run_until_parked();
        cx.update(|cx| {
            assert!(
                reduction.clone().decoded(cx).is_none(),
                "the reduction was stranded by its closed window"
            )
        });
    }

    fn reduction_of(
        window: &mut gpui::VisualTestContext,
        view: &Entity<ImageView>,
    ) -> Arc<gpui::Image> {
        window.run_until_parked();
        view.read_with(window, |view, _| match &view.reduction {
            Reduction::Ready(image) => image.clone(),
            _ => panic!("the picture was not reduced"),
        })
    }

    #[gpui::test]
    async fn closing_a_view_keeps_the_reduction_another_view_with_the_same_bytes_shows(
        cx: &mut TestAppContext,
    ) {
        let (project_a, item_a) = open_image(cx, "ra.png", wide_png()).await;
        let (project_b, item_b) = open_image(cx, "rb.png", wide_png()).await;
        let window = cx.add_empty_window();
        let view_a = view_in(window, &item_a, &project_a);
        let view_b = view_in(window, &item_b, &project_b);
        let reduction = reduction_of(window, &view_a);
        assert_eq!(reduction.id, reduction_of(window, &view_b).id);
        decode(window, &reduction);
        let before = decoded(window, &reduction).expect("decoded");

        drop(view_b);
        drop(item_b);
        window.update(|_, _| {});
        window.update(|_, _| {});
        let after = decoded(window, &reduction);
        assert!(
            after.is_some_and(|after| Arc::ptr_eq(&after, &before)),
            "closing one view dropped the reduction the other still shows"
        );
        drop(view_a);
    }

    #[gpui::test]
    async fn a_reduction_and_a_full_picture_with_the_same_bytes_do_not_release_each_other(
        cx: &mut TestAppContext,
    ) {
        let wide = wide_png();
        let reduced = reduced_png(&wide).expect("reduced");
        let (project_full, item_full) = open_image(cx, "full.png", reduced).await;
        let (project_wide, item_wide) = open_image(cx, "wide3.png", wide).await;
        let window = cx.add_empty_window();
        let view_full = view_in(window, &item_full, &project_full);
        let view_wide = view_in(window, &item_wide, &project_wide);
        let reduction = reduction_of(window, &view_wide);
        let full = item_full.read_with(window, |item, _| item.image.clone());
        assert_eq!(reduction.id, full.id);
        decode(window, &full);
        let before = decoded(window, &full).expect("decoded");

        drop(view_wide);
        drop(item_wide);
        window.update(|_, _| {});
        window.update(|_, _| {});
        let after = decoded(window, &full);
        assert!(
            after.is_some_and(|after| Arc::ptr_eq(&after, &before)),
            "releasing the reduction dropped the full picture"
        );

        let view_wide = view_in(window, &item_full, &project_full);
        drop(view_full);
        drop(view_wide);
        drop(item_full);
        window.update(|_, _| {});
        window.update(|_, _| {});
        assert!(decoded(window, &full).is_none());
    }
}
