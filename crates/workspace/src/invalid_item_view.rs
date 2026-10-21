use std::{path::Path, sync::Arc};

use gpui::{EventEmitter, FocusHandle, Focusable};
use ui::{
    App, Button, ButtonCommon, ButtonStyle, Clickable, Context, FluentBuilder, InteractiveElement,
    KeyBinding, Label, LabelCommon, LabelSize, ParentElement, Render, SharedString, Styled as _,
    Window, h_flex, v_flex,
};
use zed_actions::workspace::OpenWithSystem;

use crate::Item;

/// A view to display when a certain buffer/image/other item fails to open.
#[derive(Debug)]
pub struct InvalidItemView {
    /// Which path was attempted to open.
    pub abs_path: Arc<Path>,
    /// An error message, happened when opening the item.
    pub error: SharedString,
    offers_system_open: bool,
    focus_handle: FocusHandle,
}

/// Opening one of these with the system's default handler runs it (or the program a
/// shortcut points at) instead of viewing it.
const PROGRAM_OR_SHORTCUT_EXTENSIONS: &[&str] = &[
    "exe",
    "com",
    "bat",
    "cmd",
    "msi",
    "msp",
    "scr",
    "pif",
    "lnk",
    "url",
    "appref-ms",
    "ps1",
    "psm1",
    "vbs",
    "vbe",
    "js",
    "jse",
    "wsf",
    "wsh",
    "hta",
    "cpl",
    "jar",
    "reg",
    "dll",
    "sh",
    "command",
    "app",
    "dmg",
    "pkg",
    "desktop",
];

fn has_program_extension(path: &Path) -> bool {
    // Windows ignores trailing dots and spaces, so `tool.exe.` still launches `tool.exe`.
    let name = path.file_name().map(|name| name.to_string_lossy());
    let Some(extension) = name
        .as_deref()
        .map(|name| name.trim_end_matches(['.', ' ']))
        .and_then(|name| Path::new(name).extension())
    else {
        return false;
    };
    PROGRAM_OR_SHORTCUT_EXTENSIONS
        .iter()
        .any(|program| extension.eq_ignore_ascii_case(program))
}

#[cfg(unix)]
fn has_execute_bit(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn has_execute_bit(_path: &Path) -> bool {
    false
}

/// True when the default handler would run `path` rather than show it. A link is judged
/// by its target as well as its own name, so `notes.txt -> tool.exe` is a program too.
fn is_program_or_shortcut(path: &Path) -> bool {
    let target = std::fs::canonicalize(path).ok();
    [Some(path), target.as_deref()]
        .into_iter()
        .flatten()
        .any(|path| has_program_extension(path) || has_execute_bit(path))
}

impl InvalidItemView {
    pub fn new(
        abs_path: &Path,
        is_local: bool,
        e: &anyhow::Error,
        _: &mut Window,
        cx: &mut App,
    ) -> Self {
        Self {
            offers_system_open: is_local && !is_program_or_shortcut(abs_path),
            abs_path: Arc::from(abs_path),
            error: format!("{}", e.root_cause()).into(),
            focus_handle: cx.focus_handle(),
        }
    }
}

impl Item for InvalidItemView {
    type Event = ();

    fn tab_content_text(&self, mut detail: usize, _: &App) -> SharedString {
        // Ensure we always render at least the filename.
        detail += 1;

        let path = self.abs_path.as_ref();

        let mut prefix = path;
        while detail > 0 {
            if let Some(parent) = prefix.parent() {
                prefix = parent;
                detail -= 1;
            } else {
                break;
            }
        }

        let path = if detail > 0 {
            path
        } else {
            path.strip_prefix(prefix).unwrap_or(path)
        };

        SharedString::new(path.to_string_lossy())
    }
}

impl EventEmitter<()> for InvalidItemView {}

impl Focusable for InvalidItemView {
    fn focus_handle(&self, _: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl Render for InvalidItemView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let abs_path = self.abs_path.clone();
        v_flex()
            .size_full()
            .track_focus(&self.focus_handle(cx))
            .flex_none()
            .justify_center()
            .overflow_hidden()
            .key_context("InvalidItem")
            .child(
                h_flex().size_full().justify_center().child(
                    v_flex()
                        .justify_center()
                        .gap_2()
                        .child(h_flex().justify_center().child("Could not open file"))
                        .child(
                            h_flex()
                                .justify_center()
                                .child(Label::new(self.error.clone()).size(LabelSize::Small)),
                        )
                        .when(self.offers_system_open, |contents| {
                            contents.child(
                                h_flex().justify_center().child(
                                    Button::new("open-with-system", "Open in Default App")
                                        .on_click(move |_, _, cx| {
                                            cx.open_with_system(&abs_path);
                                        })
                                        .style(ButtonStyle::Outlined)
                                        .key_binding(KeyBinding::for_action(&OpenWithSystem, cx)),
                                ),
                            )
                        }),
                ),
            )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn programs_and_shortcuts_are_recognised_by_extension_in_any_case() {
        for name in [
            "tool.exe",
            "TOOL.EXE",
            "run.Bat",
            "setup.msi",
            "link.lnk",
            "page.url",
            "x.appref-ms",
            "s.ps1",
            "m.psm1",
            "s.vbs",
            "s.js",
            "s.hta",
            "x.jar",
            "x.reg",
            "x.dll",
            "run.sh",
            "run.command",
            "Some.app",
            "x.dmg",
            "x.pkg",
            "x.desktop",
            "x.cpl",
            "x.scr",
            "x.pif",
            "tool.exe.",
            "tool.exe  ",
            "dir/sub/tool.cmd",
        ] {
            assert!(has_program_extension(Path::new(name)), "{name}");
        }
        for name in [
            "notes.txt",
            "README.md",
            "photo.png",
            "archive.tar.gz",
            "Makefile",
            "tool.exe.txt",
            "exe",
            ".exe",
            "",
            "dir.exe/file",
        ] {
            assert!(!has_program_extension(Path::new(name)), "{name}");
        }
    }

    #[test]
    fn a_plain_document_is_not_a_program() {
        let dir = tempfile::tempdir().unwrap();
        let document = dir.path().join("notes.txt");
        std::fs::write(&document, "hello").unwrap();
        assert!(!is_program_or_shortcut(&document));
        assert!(!is_program_or_shortcut(&dir.path().join("missing.txt")));
    }

    #[gpui::test]
    fn default_app_is_offered_only_for_local_documents(cx: &mut gpui::TestAppContext) {
        let dir = tempfile::tempdir().unwrap();
        let document = dir.path().join("data.bin");
        let program = dir.path().join("tool.exe");
        std::fs::write(&document, "x").unwrap();
        std::fs::write(&program, "MZ").unwrap();
        let error = anyhow::anyhow!("binary file");
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
        });

        let mut offers = |path: &Path, is_local: bool| {
            let view = cx
                .add_window(|window, cx| InvalidItemView::new(path, is_local, &error, window, cx));
            view.read_with(cx, |view, _| view.offers_system_open)
                .unwrap()
        };

        assert!(offers(&document, true));
        assert!(
            !offers(&document, false),
            "remote items are never opened locally"
        );
        assert!(
            !offers(&program, true),
            "a program must not be one click from running"
        );
    }

    #[test]
    fn a_missing_program_name_is_still_a_program() {
        let dir = tempfile::tempdir().unwrap();
        assert!(is_program_or_shortcut(&dir.path().join("gone.exe")));
    }

    #[cfg(unix)]
    #[test]
    fn an_execute_bit_marks_a_program_whatever_its_name() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tool");
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_program_or_shortcut(&binary));
        assert!(
            !is_program_or_shortcut(dir.path()),
            "a directory always has the search bit; it is not a program"
        );

        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(!is_program_or_shortcut(&binary));
    }

    #[cfg(unix)]
    #[test]
    fn a_link_is_judged_by_its_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("payload.exe");
        std::fs::write(&target, "MZ").unwrap();
        let link = dir.path().join("notes.txt");
        symlink(&target, &link).unwrap();
        assert!(is_program_or_shortcut(&link));

        let script = dir.path().join("payload");
        std::fs::write(&script, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
        let script_link = dir.path().join("readme");
        symlink(&script, &script_link).unwrap();
        assert!(is_program_or_shortcut(&script_link));
    }
}
