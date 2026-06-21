use crate::{
    Editor,
    actions::{
        AddSelectionAbove, AddSelectionBelow, DuplicateLineDown, SelectAll, SelectLargerSyntaxNode,
        SelectNext, SelectSmallerSyntaxNode,
    },
};
use gpui::{
    App, Context, ElementId, Entity, EventEmitter, Focusable, Render, Subscription, Window,
};
use settings::SettingsStore;
use ui::{
    ButtonStyle, ContextMenu, IconButton, IconName, IconSize, PopoverMenu, PopoverMenuHandle,
    Tooltip, prelude::*,
};
use workspace::{ToolbarItemEvent, ToolbarItemLocation, ToolbarItemView, item::ItemHandle};

#[derive(Clone, Copy, Debug)]
pub struct SelectionControlsMenuOptions {
    pub include_duplicate_selection: bool,
}

impl SelectionControlsMenuOptions {
    pub const fn core() -> Self {
        Self {
            include_duplicate_selection: true,
        }
    }

    pub const fn without_duplicate_selection() -> Self {
        Self {
            include_duplicate_selection: false,
        }
    }
}

pub fn selection_controls_menu_items(
    menu: ContextMenu,
    options: SelectionControlsMenuOptions,
) -> ContextMenu {
    menu.action("Select All", Box::new(SelectAll))
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
        .when(options.include_duplicate_selection, |this| {
            this.separator()
                .action("Duplicate Selection", Box::new(DuplicateLineDown))
        })
}

pub fn selection_controls_menu(
    editor: Entity<Editor>,
    options: SelectionControlsMenuOptions,
    window: &mut Window,
    cx: &mut App,
) -> Entity<ContextMenu> {
    let focus = editor.read(cx).focus_handle(cx);

    ContextMenu::build(window, cx, move |menu, _, _| {
        selection_controls_menu_items(menu.context(focus.clone()), options)
    })
}

pub fn selection_controls_popover_with_menu(
    menu_id: impl Into<ElementId>,
    trigger_id: impl Into<ElementId>,
    handle: PopoverMenuHandle<ContextMenu>,
    build_menu: impl Fn(&mut Window, &mut App) -> Option<Entity<ContextMenu>> + 'static,
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
        .menu(build_menu)
}

pub fn selection_controls_popover(
    menu_id: impl Into<ElementId>,
    trigger_id: impl Into<ElementId>,
    editor: Entity<Editor>,
    options: SelectionControlsMenuOptions,
    handle: PopoverMenuHandle<ContextMenu>,
) -> PopoverMenu<ContextMenu> {
    selection_controls_popover_with_menu(menu_id, trigger_id, handle, move |window, cx| {
        Some(selection_controls_menu(editor.clone(), options, window, cx))
    })
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
