use crate::{
    Editor,
    actions::{
        AddSelectionAbove, AddSelectionBelow, DuplicateLineDown, GoToDiagnostic, GoToHunk,
        GoToPreviousDiagnostic, GoToPreviousHunk, MoveLineDown, MoveLineUp, SelectAll,
        SelectLargerSyntaxNode, SelectNext, SelectSmallerSyntaxNode, ToggleGoToLine,
    },
};
use gpui::{
    App, Context, ElementId, Entity, EventEmitter, Focusable, Render, Subscription, Window,
};
use project::DisableAiSettings;
use settings::{Settings, SettingsStore};
use ui::{
    ButtonStyle, ContextMenu, IconButton, IconName, IconSize, PopoverMenu, PopoverMenuHandle,
    Tooltip, prelude::*,
};
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, item::ItemHandle};
use zed_actions::{agent::AddSelectionToThread, outline::ToggleOutline};

#[derive(Clone, Copy, Debug)]
pub struct SelectionControlsMenuOptions {
    pub include_agent_thread: bool,
    pub include_navigation: bool,
    pub include_diagnostics: bool,
    pub include_diff_hunks: bool,
    pub include_line_move_actions: bool,
    pub include_duplicate_selection: bool,
}

impl SelectionControlsMenuOptions {
    pub const fn core() -> Self {
        Self {
            include_agent_thread: false,
            include_navigation: false,
            include_diagnostics: false,
            include_diff_hunks: false,
            include_line_move_actions: false,
            include_duplicate_selection: true,
        }
    }

    pub const fn zed_quick_action_bar() -> Self {
        Self {
            include_agent_thread: true,
            include_navigation: true,
            include_diagnostics: true,
            include_diff_hunks: true,
            include_line_move_actions: true,
            include_duplicate_selection: true,
        }
    }
}

pub fn selection_controls_menu(
    editor: Entity<Editor>,
    options: SelectionControlsMenuOptions,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let focus = editor.read(cx).focus_handle(cx);
    let has_selection = editor.update(cx, |editor, cx| {
        editor.has_non_empty_selection(&editor.display_snapshot(cx))
    });
    let has_diff_hunks = options.include_diff_hunks
        && editor
            .read(cx)
            .buffer()
            .read(cx)
            .snapshot(cx)
            .has_diff_hunks();
    let disable_ai = DisableAiSettings::get_global(cx).disable_ai;

    ContextMenu::build(window, cx, move |menu, _, _| {
        menu.context(focus.clone())
            .action("Select All", Box::new(SelectAll))
            .action(
                "Select Next Occurrence",
                Box::new(SelectNext {
                    replace_newest: false,
                }),
            )
            .action("Expand Selection", Box::new(SelectLargerSyntaxNode))
            .action("Shrink Selection", Box::new(SelectSmallerSyntaxNode))
            .action(
                "Add Cursor Above",
                Box::new(AddSelectionAbove {
                    skip_soft_wrap: true,
                }),
            )
            .action(
                "Add Cursor Below",
                Box::new(AddSelectionBelow {
                    skip_soft_wrap: true,
                }),
            )
            .when(options.include_agent_thread && !disable_ai, |this| {
                this.separator().action_disabled_when(
                    !has_selection,
                    "Add to Agent Thread",
                    Box::new(AddSelectionToThread),
                )
            })
            .when(options.include_navigation, |this| {
                this.separator()
                    .action("Go to Symbol", Box::new(ToggleOutline))
                    .action("Go to Line/Column", Box::new(ToggleGoToLine))
            })
            .when(options.include_diagnostics, |this| {
                this.separator()
                    .action("Next Problem", Box::new(GoToDiagnostic::default()))
                    .action(
                        "Previous Problem",
                        Box::new(GoToPreviousDiagnostic::default()),
                    )
            })
            .when(options.include_diff_hunks, |this| {
                this.separator()
                    .action_disabled_when(!has_diff_hunks, "Next Hunk", Box::new(GoToHunk))
                    .action_disabled_when(
                        !has_diff_hunks,
                        "Previous Hunk",
                        Box::new(GoToPreviousHunk),
                    )
            })
            .when(options.include_line_move_actions, |this| {
                this.separator()
                    .action("Move Line Up", Box::new(MoveLineUp))
                    .action("Move Line Down", Box::new(MoveLineDown))
            })
            .when(options.include_duplicate_selection, |this| {
                this.action("Duplicate Selection", Box::new(DuplicateLineDown))
            })
    })
}

pub fn selection_controls_popover(
    menu_id: impl Into<ElementId>,
    trigger_id: impl Into<ElementId>,
    editor: Entity<Editor>,
    options: SelectionControlsMenuOptions,
    handle: PopoverMenuHandle<ContextMenu>,
) -> PopoverMenu<ContextMenu> {
    PopoverMenu::new(menu_id)
        .trigger_with_tooltip(
            IconButton::new(trigger_id, IconName::CursorIBeam)
                .icon_size(IconSize::Small)
                .style(ButtonStyle::Subtle)
                .toggle_state(handle.is_deployed()),
            Tooltip::text("Selection Controls"),
        )
        .with_handle(handle)
        .anchor(gpui::Anchor::TopRight)
        .menu(move |window, cx| Some(selection_controls_menu(editor.clone(), options, window, cx)))
}

pub struct EditorSelectionControls {
    active_item: Option<Box<dyn ItemHandle>>,
    handle: PopoverMenuHandle<ContextMenu>,
    options: SelectionControlsMenuOptions,
    _editor_subscription: Option<Subscription>,
    _settings_subscription: Subscription,
}

impl EditorSelectionControls {
    pub fn new(cx: &mut Context<Self>) -> Self {
        Self::with_options(SelectionControlsMenuOptions::core(), cx)
    }

    pub fn with_options(options: SelectionControlsMenuOptions, cx: &mut Context<Self>) -> Self {
        let settings_subscription = cx.observe_global::<SettingsStore>(|this, cx| {
            cx.emit(ToolbarItemEvent::ChangeLocation(
                this.toolbar_item_location(cx),
            ));
            cx.notify();
        });

        Self {
            active_item: None,
            handle: Default::default(),
            options,
            _editor_subscription: None,
            _settings_subscription: settings_subscription,
        }
    }

    fn active_editor(&self) -> Option<Entity<Editor>> {
        self.active_item
            .as_ref()
            .and_then(|item| item.downcast::<Editor>())
    }

    fn toolbar_item_location(&self, cx: &App) -> ToolbarItemLocation {
        if self
            .active_editor()
            .is_some_and(|editor| editor.read(cx).selection_menu_enabled(cx))
        {
            ToolbarItemLocation::PrimaryRight
        } else {
            ToolbarItemLocation::Hidden
        }
    }
}

impl Render for EditorSelectionControls {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let Some(editor) = self.active_editor() else {
            return div()
                .id("empty editor selection controls")
                .into_any_element();
        };

        if !editor.read(cx).selection_menu_enabled(cx) {
            return div()
                .id("disabled editor selection controls")
                .into_any_element();
        }

        selection_controls_popover(
            "editor-selection-controls",
            "toggle-editor-selection-controls",
            editor,
            self.options,
            self.handle.clone(),
        )
        .into_any_element()
    }
}

impl EventEmitter<ToolbarItemEvent> for EditorSelectionControls {}

impl ToolbarItemView for EditorSelectionControls {
    fn set_active_pane_item(
        &mut self,
        active_pane_item: Option<&dyn ItemHandle>,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) -> ToolbarItemLocation {
        self.active_item = active_pane_item.map(ItemHandle::boxed_clone);
        self._editor_subscription.take();

        if let Some(editor) = self.active_editor() {
            self._editor_subscription = Some(cx.observe(&editor, |this, _, cx| {
                cx.emit(ToolbarItemEvent::ChangeLocation(
                    this.toolbar_item_location(cx),
                ));
                cx.notify();
            }));
        }

        self.toolbar_item_location(cx)
    }
}
