use std::{path::Path, sync::Arc};

use gpui::{EventEmitter, FocusHandle, Focusable, Task};
use ui::{
    App, Button, ButtonCommon, ButtonStyle, Clickable, Context, FluentBuilder, InteractiveElement,
    KeyBinding, Label, LabelCommon, LabelSize, ParentElement, Render, SharedString, Styled as _,
    Window, h_flex, v_flex,
};
use zed_actions::workspace::OpenWithSystem;

use crate::{Item, Workspace};

/// A view to display when a certain buffer/image/other item fails to open.
#[derive(Debug)]
pub struct InvalidItemView {
    /// Which path was attempted to open.
    pub abs_path: Arc<Path>,
    /// An error message, happened when opening the item.
    pub error: SharedString,
    is_local: bool,
    /// Whether the default handler would show the file rather than run it. Not offered until the
    /// path has been inspected, off the UI thread, by [`Self::classify_in_background`].
    offers_system_open: bool,
    classification: Option<Task<()>>,
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
    "py",
    "pyw",
    "pyc",
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
    let name = path.file_name().map(|name| name.to_string_lossy());
    let Some(extension) = name
        .as_deref()
        // Windows ignores trailing dots and spaces, so `tool.exe.` still launches `tool.exe`.
        // Elsewhere they are part of the name.
        .map(|name| {
            if cfg!(windows) {
                name.trim_end_matches(['.', ' '])
            } else {
                name
            }
        })
        .and_then(|name| Path::new(name).extension())
    else {
        return false;
    };
    PROGRAM_OR_SHORTCUT_EXTENSIONS
        .iter()
        .any(|program| extension.eq_ignore_ascii_case(program))
}

/// Only extension-less files are judged by the execute bit: exFAT, NTFS, SMB and WSL mounts
/// report every file as executable, and a name with an extension is already covered by
/// `PROGRAM_OR_SHORTCUT_EXTENSIONS`. A trailing dot (`tool.`) names no extension.
#[cfg(unix)]
fn has_execute_bit(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    path.extension()
        .is_none_or(|extension| extension.is_empty())
        && std::fs::metadata(path)
            .is_ok_and(|metadata| metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn has_execute_bit(_path: &Path) -> bool {
    false
}

/// True when the default handler would run `path` rather than show it. A link is judged
/// by its target as well as its own name, so `notes.txt -> tool.exe` is a program too, and a
/// link is judged by the extension of its target when it reads the execute bit.
///
/// Looks at the file system, which can block on a slow mount: not for the UI thread. Use
/// [`is_named_like_program_or_shortcut`] there.
pub fn is_program_or_shortcut(path: &Path) -> bool {
    let target = std::fs::canonicalize(path).ok();
    let resolved = target.as_deref().unwrap_or(path);
    has_program_extension(path) || has_program_extension(resolved) || has_execute_bit(resolved)
}

/// True when `path`, or `link_target` when it is known, is named like a program or shortcut.
/// Reads no file: it only sees names.
pub fn is_named_like_program_or_shortcut(path: &Path, link_target: Option<&Path>) -> bool {
    has_program_extension(path) || link_target.is_some_and(has_program_extension)
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
            is_local,
            offers_system_open: false,
            classification: None,
            abs_path: Arc::from(abs_path),
            error: format!("{}", e.root_cause()).into(),
            focus_handle: cx.focus_handle(),
        }
    }

    /// Looks at the path, which can block on a slow or unreachable file system, and offers the
    /// default app once it is known not to run a program.
    fn classify_in_background(&mut self, cx: &mut Context<Self>) {
        if !self.is_local || self.classification.is_some() {
            return;
        }
        let abs_path = self.abs_path.clone();
        self.classification = Some(cx.spawn(async move |this, cx| {
            let is_program = cx
                .background_executor()
                .spawn(async move { is_program_or_shortcut(&abs_path) })
                .await;
            this.update(cx, |this, cx| {
                this.offers_system_open = !is_program;
                cx.notify();
            })
            .ok();
        }));
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

    fn added_to_workspace(
        &mut self,
        _workspace: &mut Workspace,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.classify_in_background(cx);
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
            "x.py",
            "x.pyw",
            "x.pyc",
            "dir/sub/tool.cmd",
        ] {
            assert!(has_program_extension(Path::new(name)), "{name}");
        }
        // Windows ignores trailing dots and spaces in a name; other systems keep them.
        for name in ["tool.exe.", "tool.exe  ", "tool.exe. ."] {
            assert_eq!(
                has_program_extension(Path::new(name)),
                cfg!(windows),
                "{name:?}"
            );
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
            view.read_with(cx, |view, _| {
                assert!(!view.offers_system_open, "before it is looked at")
            })
            .unwrap();
            view.update(cx, |view, _, cx| view.classify_in_background(cx))
                .unwrap();
            cx.run_until_parked();
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
    fn a_trailing_dot_names_no_extension_so_the_execute_bit_still_counts() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let binary = dir.path().join("tool.");
        std::fs::write(&binary, "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(&binary, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_program_or_shortcut(&binary));
    }

    #[cfg(unix)]
    #[test]
    fn an_execute_bit_on_a_named_document_does_not_hide_the_default_app() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        for name in ["report.pdf", "notes.txt", "data.bin"] {
            let document = dir.path().join(name);
            std::fs::write(&document, "x").unwrap();
            std::fs::set_permissions(&document, std::fs::Permissions::from_mode(0o777)).unwrap();
            assert!(
                !is_program_or_shortcut(&document),
                "{name} on a mount that reports every file as executable"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn a_link_to_a_named_document_is_not_a_program_whatever_its_own_name() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let dir = tempfile::tempdir().unwrap();
        let document = dir.path().join("report.pdf");
        std::fs::write(&document, "x").unwrap();
        // A mount that reports every file as executable.
        std::fs::set_permissions(&document, std::fs::Permissions::from_mode(0o777)).unwrap();
        let link = dir.path().join("report");
        symlink(&document, &link).unwrap();
        assert!(!is_program_or_shortcut(&link));
    }

    #[test]
    fn names_alone_mark_a_program_without_reading_the_file_system() {
        assert!(is_named_like_program_or_shortcut(
            Path::new("/gone/tool.exe"),
            None
        ));
        assert!(is_named_like_program_or_shortcut(
            Path::new("/gone/notes.txt"),
            Some(Path::new("/gone/payload.bat"))
        ));
        assert!(!is_named_like_program_or_shortcut(
            Path::new("/gone/notes.txt"),
            Some(Path::new("/gone/other.txt"))
        ));
        assert!(!is_named_like_program_or_shortcut(
            Path::new("/gone/tool"),
            None
        ));
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
