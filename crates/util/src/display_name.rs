//! One hook for how a file or folder name is drawn. The application installs it once; the
//! project panel, tab titles, breadcrumbs and the file finder call it. It only changes what is
//! shown: paths, identities and anything that is matched or saved keep the real name.

use std::{
    borrow::Cow,
    sync::{Arc, LazyLock, RwLock},
};

pub type DisplayNameHook = Arc<dyn for<'a> Fn(&'a str) -> Cow<'a, str> + Send + Sync>;

static HOOK: LazyLock<RwLock<Option<DisplayNameHook>>> = LazyLock::new(|| RwLock::new(None));

/// Installs the hook, replacing any earlier one.
pub fn set_hook(hook: DisplayNameHook) {
    *HOOK
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(hook);
}

/// Removes the hook, so names are drawn as they are.
pub fn clear_hook() {
    *HOOK
        .write()
        .unwrap_or_else(|poisoned| poisoned.into_inner()) = None;
}

#[cfg(any(test, feature = "test-support"))]
static TEST_HOOK_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

/// A hook installed for one test: removed again when this is dropped.
#[cfg(any(test, feature = "test-support"))]
pub struct TestHook {
    _only_test_with_a_hook: std::sync::MutexGuard<'static, ()>,
}

/// Installs `hook` for the length of a test. The hook is process-wide, so tests that install one
/// take turns instead of drawing each other's names.
#[cfg(any(test, feature = "test-support"))]
pub fn install_hook_for_test(hook: DisplayNameHook) -> TestHook {
    let only_test_with_a_hook = TEST_HOOK_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    set_hook(hook);
    TestHook {
        _only_test_with_a_hook: only_test_with_a_hook,
    }
}

#[cfg(any(test, feature = "test-support"))]
impl Drop for TestHook {
    fn drop(&mut self) {
        clear_hook();
    }
}

/// `name` as it is drawn: unchanged until a hook is installed.
pub fn display_name(name: &str) -> Cow<'_, str> {
    let hook = HOOK
        .read()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .clone();
    match hook {
        Some(hook) => hook(name),
        None => Cow::Borrowed(name),
    }
}

/// [`display_name`] for text that carries highlight positions (byte offsets of matched
/// characters). Each position moves to where the same character starts in the drawn text, which
/// holds for any hook that rewrites one character at a time.
pub fn display_name_with_positions(name: &str, positions: &[usize]) -> (String, Vec<usize>) {
    let drawn = display_name(name);
    let positions = positions
        .iter()
        .map(|&position| display_name(&name[..position]).len())
        .collect();
    (drawn.into_owned(), positions)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn escape_zero_width_space(name: &str) -> Cow<'_, str> {
        if name.contains('\u{200B}') {
            Cow::Owned(name.replace('\u{200B}', "<U+200B>"))
        } else {
            Cow::Borrowed(name)
        }
    }

    #[test]
    fn names_are_drawn_through_the_installed_hook_and_unchanged_without_one() {
        let hook = install_hook_for_test(Arc::new(escape_zero_width_space));
        assert_eq!(display_name("READ\u{200B}ME.md"), "READ<U+200B>ME.md");
        assert_eq!(display_name("README.md"), "README.md");

        let (drawn, positions) = display_name_with_positions("a\u{200B}bc", &[0, 4]);
        assert_eq!(drawn, "a<U+200B>bc");
        assert_eq!(positions, [0, 9]);
        assert_eq!(&drawn[positions[1]..], "bc");

        drop(hook);
        assert_eq!(display_name("READ\u{200B}ME.md"), "READ\u{200B}ME.md");
    }
}
