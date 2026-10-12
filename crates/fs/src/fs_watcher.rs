use futures::channel::oneshot;
use gpui::{BackgroundExecutor, Task};
use notify::{Event, EventKind};
use parking_lot::Mutex;
use std::{
    collections::{HashMap, HashSet},
    fs,
    ops::DerefMut,
    path::Path,
    sync::{Arc, LazyLock, OnceLock},
    time::{Duration, Instant},
};
use util::paths::SanitizedPath;

use crate::{PathEvent, PathEventKind, Watcher};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub enum WatcherMode {
    #[default]
    Native,
    Poll,
}

pub struct FsWatcher {
    executor: BackgroundExecutor,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    registrations: Arc<Mutex<HashMap<WatchKey, FsWatcherRegistration>>>,
    pending_registrations: Arc<Mutex<HashMap<Arc<std::path::Path>, Task<()>>>>,
}

#[derive(Clone, Copy)]
struct FsWatcherRegistration {
    id: WatcherRegistrationId,
    mode: WatcherMode,
}

impl FsWatcher {
    pub fn new(
        executor: BackgroundExecutor,
        tx: async_channel::Sender<()>,
        pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    ) -> Self {
        Self {
            executor,
            tx,
            pending_path_events,
            registrations: Default::default(),
            pending_registrations: Default::default(),
        }
    }

    fn add_existing_path(&self, path: Arc<Path>) -> Option<WatchApplied> {
        let case_insensitive = case_insensitive_path(&path);
        let key = WatchKey::for_registration(SanitizedPath::new(&path), case_insensitive);
        // A registration whose OS watch failed is gone from the global watcher;
        // adding the path again retries it.
        let existing = self.registrations.lock().get(&key).copied();
        if let Some(existing) = existing
            && let Some(applied) = global_watcher().applied_waiter(existing.id)
        {
            log::trace!("path to watch is already watched: {path:?}");
            // Its OS watch may still be queued by an earlier `add`.
            return applied;
        }
        let (registration, applied) = register_existing_path(
            path,
            case_insensitive,
            self.tx.clone(),
            self.pending_path_events.clone(),
        )?;
        self.registrations.lock().insert(key, registration);
        applied
    }

    fn add_path(&self, path: &Path) -> Option<WatchApplied> {
        log::trace!("watcher add: {path:?}");

        let path: Arc<Path> = path.into();
        let covering =
            recursive_registration_covering(&self.registrations.lock(), SanitizedPath::new(&path));
        if let Some(covering) = covering
            && let Some(applied) = global_watcher().applied_waiter(covering.id)
        {
            log::trace!("path to watch is covered by an existing registration: {path:?}");
            // The covering watch may still be queued by an earlier `add`.
            return applied;
        }

        if self
            .pending_registrations
            .lock()
            .contains_key(path.as_ref())
        {
            log::trace!("path to watch is already pending: {path:?}");
            return None;
        }

        if fs::symlink_metadata(path.as_ref()).is_err() {
            self.add_pending_path(path);
            return None;
        }

        self.add_existing_path(path)
    }

    fn add_pending_path(&self, path: Arc<Path>) {
        let mut pending_registrations = self.pending_registrations.lock();
        if pending_registrations.contains_key(path.as_ref()) {
            return;
        }

        let task = self.executor.spawn(poll_path_until_created(
            self.executor.clone(),
            path.clone(),
            self.tx.clone(),
            self.pending_path_events.clone(),
            self.registrations.clone(),
            self.pending_registrations.clone(),
        ));
        pending_registrations.insert(path, task);
    }
}

impl Drop for FsWatcher {
    fn drop(&mut self) {
        self.pending_registrations.lock().clear();

        let mut registrations = HashMap::new();
        {
            let old = &mut self.registrations.lock();
            std::mem::swap(old.deref_mut(), &mut registrations);
        }

        let global_watcher = global_watcher();
        for (_, registration) in registrations {
            global_watcher.remove(registration.id);
        }
    }
}

impl Watcher for FsWatcher {
    /// Queues the watch and returns without waiting for the OS; a failure is
    /// logged by the watcher thread. Use [`Watcher::add_and_wait`] to wait.
    fn add(&self, path: &std::path::Path) -> anyhow::Result<()> {
        self.add_path(path);
        Ok(())
    }

    fn add_and_wait<'a>(
        &'a self,
        path: &'a std::path::Path,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
        let applied = self.add_path(path);
        Box::pin(async move {
            let Some(applied) = applied else {
                return Ok(());
            };
            // Cancelled means the registration was removed before its watch was
            // applied, so there is nothing left to wait for.
            applied.await.unwrap_or(Ok(()))
        })
    }

    fn remove(&self, path: &std::path::Path) -> anyhow::Result<()> {
        log::trace!("remove watched path: {path:?}");
        self.pending_registrations.lock().remove(path);

        let sanitized = SanitizedPath::new(path);
        let registration = {
            let mut registrations = self.registrations.lock();
            registrations
                .remove(&WatchKey::exact(sanitized))
                .or_else(|| registrations.remove(&WatchKey::folded(sanitized)))
        };
        if let Some(registration) = registration {
            global_watcher().remove(registration.id);
        }
        Ok(())
    }
}

/// The recursive registration on a strict ancestor of `path` that already covers
/// it, if any. Both key spellings are probed so a folded registration still
/// matches; only poll watches and native macOS/Windows watches are recursive.
fn recursive_registration_covering(
    registrations: &HashMap<WatchKey, FsWatcherRegistration>,
    path: &SanitizedPath,
) -> Option<FsWatcherRegistration> {
    path.as_path().ancestors().skip(1).find_map(|ancestor| {
        let ancestor = SanitizedPath::unchecked_new(ancestor);
        [WatchKey::exact(ancestor), WatchKey::folded(ancestor)]
            .iter()
            .find_map(|key| {
                registrations
                    .get(key)
                    .filter(|registration| {
                        registration.mode == WatcherMode::Poll
                            || cfg!(any(target_os = "windows", target_os = "macos"))
                    })
                    // A watch that failed is gone from the global watcher; the
                    // local record of it must not keep covering its descendants.
                    .filter(|registration| global_watcher().is_registered(registration.id))
                    .copied()
            })
    })
}

/// Detect whether a path requires polling instead of native file watching.
///
/// Returns `true` for filesystem types where inotify/FSEvents/ReadDirectoryChanges
/// silently fail to deliver events: 9P (WSL drvfs), NFS, CIFS/SMB, FUSE (sshfs), etc.
///
/// Can be overridden with the `ZED_FILE_WATCHER_MODE` environment variable:
/// - `native` — always use native OS watcher
/// - `poll` — always use polling
/// - `auto` (default) — auto-detect based on filesystem type
pub fn requires_poll_watcher(path: &Path) -> bool {
    match std::env::var("ZED_FILE_WATCHER_MODE")
        .as_deref()
        .unwrap_or("auto")
    {
        "native" => return false,
        "poll" => return true,
        _ => {}
    }

    #[cfg(target_os = "linux")]
    {
        return detect_requires_poll_watcher_linux(path);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = path;
        false
    }
}

fn register_existing_path(
    path: Arc<Path>,
    case_insensitive: bool,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
) -> Option<(FsWatcherRegistration, Option<WatchApplied>)> {
    let mode = if requires_poll_watcher(path.as_ref()) {
        log::info!(
            "Using poll watcher ({}ms interval) for {}",
            poll_interval().as_millis(),
            path.display()
        );
        telemetry::event!("fs_watcher_poll", path = path.display().to_string());
        WatcherMode::Poll
    } else {
        WatcherMode::Native
    };
    let root_path = SanitizedPath::new_arc(path.as_ref());
    let path_for_callback = path.clone();
    let registration = global_watcher().add(
        path,
        mode,
        case_insensitive,
        move |event: &notify::Event| {
            log::trace!("watcher received event: {event:?}");
            push_notify_event(
                &tx,
                &pending_path_events,
                &root_path,
                case_insensitive,
                path_for_callback.as_ref(),
                event,
            );
        },
    )?;
    Some((
        FsWatcherRegistration {
            id: registration.id,
            mode,
        },
        registration.applied,
    ))
}

#[cfg(target_os = "linux")]
fn detect_requires_poll_watcher_linux(path: &Path) -> bool {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let c_path = match CString::new(path.as_os_str().as_bytes()) {
        Ok(p) => p,
        Err(_) => return false,
    };

    let mut stat: libc::statfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statfs(c_path.as_ptr(), &mut stat) } != 0 {
        return false;
    }

    const V9FS_MAGIC: u64 = 0x0102_1997;
    const NFS_SUPER_MAGIC: u64 = 0x0000_6969;
    const CIFS_MAGIC: u64 = 0xFF53_4D42;
    const SMB_SUPER_MAGIC: u64 = 0x0000_517B;
    const SMB2_MAGIC: u64 = 0xFE53_4D42;
    const FUSE_SUPER_MAGIC: u64 = 0x6573_5546;

    let fs_type = (stat.f_type as u64) & 0xFFFF_FFFF;
    if fs_type == FUSE_SUPER_MAGIC && is_virtiofs(path) {
        return false;
    }

    if fs_type == V9FS_MAGIC
        || fs_type == NFS_SUPER_MAGIC
        || fs_type == CIFS_MAGIC
        || fs_type == SMB_SUPER_MAGIC
        || fs_type == SMB2_MAGIC
        || fs_type == FUSE_SUPER_MAGIC
    {
        log::info!(
            "Detected network/virtual filesystem (type 0x{:x}) at {}, using poll watcher",
            fs_type,
            path.display()
        );
        return true;
    }

    if is_wsl_drvfs_path(path) {
        log::info!(
            "Detected WSL drvfs mount at {}, using poll watcher",
            path.display()
        );
        return true;
    }

    false
}

#[cfg(target_os = "linux")]
fn is_virtiofs(path: &Path) -> bool {
    let Ok(mountinfo) = std::fs::read_to_string("/proc/self/mountinfo") else {
        return false;
    };

    let mut best_mount = None;
    for line in mountinfo.lines() {
        let fields = line.split(' ').collect::<Vec<_>>();
        let Some(separator) = fields.iter().position(|field| *field == "-") else {
            continue;
        };
        let (Some(mount_point), Some(fs_type)) = (fields.get(4), fields.get(separator + 1)) else {
            continue;
        };

        let mount_point = mount_point
            .replace("\\040", " ")
            .replace("\\011", "\t")
            .replace("\\012", "\n")
            .replace("\\134", "\\");
        if path.starts_with(&mount_point)
            && best_mount.is_none_or(|(length, _)| mount_point.len() > length)
        {
            best_mount = Some((mount_point.len(), *fs_type));
        }
    }

    best_mount.is_some_and(|(_, fs_type)| fs_type == "virtiofs" || fs_type == "fuse.virtiofs")
}

#[cfg(target_os = "linux")]
fn is_wsl_drvfs_path(path: &Path) -> bool {
    if std::env::var_os("WSL_DISTRO_NAME").is_none() {
        if let Ok(version) = std::fs::read_to_string("/proc/version") {
            let version = version.to_lowercase();
            if !version.contains("microsoft") && !version.contains("wsl") {
                return false;
            }
        } else {
            return false;
        }
    }

    let Some(path) = path.to_str() else {
        return false;
    };
    if !path.starts_with("/mnt/") || path.len() < 6 {
        return false;
    }
    let after_mnt = &path[5..];
    after_mnt.starts_with(|c: char| c.is_ascii_alphabetic())
        && (after_mnt.len() == 1 || after_mnt.as_bytes()[1] == b'/')
}

/// Whether the volume backing `path` does case-insensitive name lookups, used to
/// pick exact vs. folded matching.
#[cfg(target_os = "macos")]
fn case_insensitive_path(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt as _;

    // `pathconf(_PC_CASE_SENSITIVE)` returns 1 (sensitive), 0 (insensitive), or -1
    // on error; default errors to insensitive (the APFS/HFS+ default).
    let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return true;
    };
    // SAFETY: We just initialized c_path, so it's a valid pointer
    unsafe { libc::pathconf(c_path.as_ptr(), libc::_PC_CASE_SENSITIVE) == 0 }
}

#[cfg(target_os = "linux")]
fn case_insensitive_path(_path: &Path) -> bool {
    // use std::os::unix::ffi::OsStrExt as _;

    // // Only ext4/f2fs casefold (`+F`) dirs are insensitive, reported by `statx` via
    // // STATX_ATTR_CASEFOLD; any failure (e.g. pre-4.11 ENOSYS) means case-sensitive.
    // const STATX_ATTR_CASEFOLD: u64 = 0x0000_2000;
    // let Ok(c_path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
    //     return false;
    // };
    // let mut buf = std::mem::MaybeUninit::<libc::statx>::zeroed();

    // // SAFETY: c_path is still valid, buffer has been zeroed
    // if unsafe { libc::statx(libc::AT_FDCWD, c_path.as_ptr(), 0, 0, buf.as_mut_ptr()) } != 0 {
    //     return false;
    // }

    // // SAFETY: libc statx initialized this buffer, otherwise we would've returned on a error
    // // in that function call
    // let buf = unsafe { buf.assume_init() };
    // buf.stx_attributes_mask & STATX_ATTR_CASEFOLD != 0
    //     && buf.stx_attributes & STATX_ATTR_CASEFOLD != 0
    false
}

#[cfg(target_os = "windows")]
fn case_insensitive_path(_path: &Path) -> bool {
    // todo(windows): Windows defaults to case in sensitive, but
    // they can mark specific directories as case sensitive. Mainly
    // for WSL use cases
    true
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
fn case_insensitive_path(_path: &Path) -> bool {
    // Other BSDs default to case-sensitive local filesystems.
    false
}

/// Whether `path` is `root` or sits beneath it, folding case on case-insensitive
/// volumes so a differently-cased spelling still matches.
fn path_is_under(path: &SanitizedPath, root: &SanitizedPath, case_insensitive: bool) -> bool {
    if case_insensitive {
        let path = path.as_path().to_string_lossy().to_lowercase();
        let root = root.as_path().to_string_lossy().to_lowercase();
        Path::new(&path).starts_with(Path::new(&root))
    } else {
        path.starts_with(root)
    }
}

/// Lookup key for a watch path, shared by add, remove, and dispatch so they all
/// agree on whether two spellings denote the same directory.
///
/// On case-sensitive volumes the exact (sanitized) path is the key, so genuinely
/// distinct directories stay distinct. On case-insensitive volumes the folded
/// (lowercased) spelling is the key, so any casing of a directory collides.
///
/// The two variants are distinct map keys, so a case-sensitive registration can
/// never be hit by a folded lookup (or vice versa); dispatch can therefore probe
/// both forms of an event path without risking a cross-rule false match.
///
/// NOTE: folding only normalizes case. macOS (APFS/HFS+) is also Unicode
/// normalization-insensitive (NFC vs NFD); that normalization is intentionally
/// centralized here so it can be added in one place later without touching the
/// add/remove/dispatch call sites.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
enum WatchKey {
    Exact(Arc<Path>),
    Folded(Arc<str>),
}

impl WatchKey {
    fn exact(path: &SanitizedPath) -> Self {
        Self::Exact(Arc::from(path.as_path()))
    }

    fn folded(path: &SanitizedPath) -> Self {
        let lossy = path.as_path().to_string_lossy();
        // macOS (APFS/HFS+) compares names normalization-insensitively (NFC vs
        // NFD), and FSEvents can report NFD while a config/LSP supplies NFC, so
        // normalize before folding case. Windows (NTFS) and Linux are
        // normalization-sensitive, so there we only fold case.
        #[cfg(target_os = "macos")]
        let folded = {
            use unicode_normalization::UnicodeNormalization as _;
            lossy.chars().nfc().collect::<String>().to_lowercase()
        };
        #[cfg(not(target_os = "macos"))]
        let folded = lossy.to_lowercase();
        Self::Folded(folded.into())
    }

    fn for_registration(path: &SanitizedPath, case_insensitive: bool) -> Self {
        if case_insensitive {
            Self::folded(path)
        } else {
            Self::exact(path)
        }
    }
}

async fn poll_path_until_created(
    executor: BackgroundExecutor,
    path: Arc<Path>,
    tx: async_channel::Sender<()>,
    pending_path_events: Arc<Mutex<Vec<PathEvent>>>,
    registrations: Arc<Mutex<HashMap<WatchKey, FsWatcherRegistration>>>,
    pending_registrations: Arc<Mutex<HashMap<Arc<Path>, Task<()>>>>,
) {
    loop {
        executor.timer(poll_interval()).await;

        if !pending_registrations.lock().contains_key(path.as_ref()) {
            return;
        }

        if smol::fs::symlink_metadata(path.as_ref()).await.is_err() {
            continue;
        }

        // Probe case sensitivity now that the path exists, rather than at add
        // time when it didn't.
        let case_insensitive = case_insensitive_path(path.as_ref());
        let key = WatchKey::for_registration(SanitizedPath::new(&path), case_insensitive);

        // A record whose watch has since failed does not count: register again.
        if registrations
            .lock()
            .get(&key)
            .is_some_and(|existing| global_watcher().is_registered(existing.id))
        {
            pending_registrations.lock().remove(path.as_ref());
            return;
        }

        match register_existing_path(
            path.clone(),
            case_insensitive,
            tx.clone(),
            pending_path_events.clone(),
        ) {
            Some((registration, applied)) => {
                // Removing the pending path drops this task, possibly while it
                // waits below; the guard releases the registration then.
                let mut unclaimed = UnclaimedRegistration(Some(registration.id));
                if let Some(applied) = applied
                    && let Ok(Err(error)) = applied.await
                {
                    log::warn!("failed to watch newly-created path {path:?}: {error}; retrying");
                    continue;
                }
                {
                    let mut pending_registrations = pending_registrations.lock();
                    if pending_registrations.remove(path.as_ref()).is_none() {
                        return;
                    }
                    registrations.lock().insert(key, registration);
                    unclaimed.0 = None;
                }
                enqueue_path_events(
                    &tx,
                    &pending_path_events,
                    vec![
                        PathEvent {
                            path: path.to_path_buf(),
                            kind: Some(PathEventKind::Created),
                        },
                        PathEvent {
                            path: path.to_path_buf(),
                            kind: Some(PathEventKind::Rescan),
                        },
                    ],
                );
                return;
            }
            None => {}
        }
    }
}

/// A global registration no `FsWatcher` owns yet. Dropping it removes the
/// registration, so a task cancelled mid-wait cannot leak a watch.
struct UnclaimedRegistration(Option<WatcherRegistrationId>);

impl Drop for UnclaimedRegistration {
    fn drop(&mut self) {
        if let Some(id) = self.0.take() {
            global_watcher().remove(id);
        }
    }
}

fn enqueue_path_events(
    tx: &smol::channel::Sender<()>,
    pending_path_events: &Arc<Mutex<Vec<PathEvent>>>,
    mut path_events: Vec<PathEvent>,
) {
    if path_events.is_empty() {
        return;
    }

    path_events.sort();
    let mut pending_paths = pending_path_events.lock();
    if pending_paths.is_empty() {
        tx.try_send(()).ok();
    }
    coalesce_pending_rescans(&mut pending_paths, &mut path_events);
    util::extend_sorted(&mut *pending_paths, path_events, usize::MAX, |a, b| {
        a.path.cmp(&b.path)
    });
}

fn push_notify_event(
    tx: &smol::channel::Sender<()>,
    pending_path_events: &Arc<Mutex<Vec<PathEvent>>>,
    root_path: &SanitizedPath,
    case_insensitive: bool,
    watched_root: &Path,
    event: &notify::Event,
) {
    let kind = match event.kind {
        EventKind::Create(_) => Some(PathEventKind::Created),
        EventKind::Modify(_) => Some(PathEventKind::Changed),
        EventKind::Remove(_) => Some(PathEventKind::Removed),
        _ => None,
    };
    let mut path_events = event
        .paths
        .iter()
        .filter_map(|event_path| {
            let event_path = SanitizedPath::new(event_path);
            path_is_under(event_path, root_path, case_insensitive).then(|| PathEvent {
                path: event_path.as_path().to_path_buf(),
                kind,
            })
        })
        .collect::<Vec<_>>();

    if event.need_rescan() {
        if !watcher_logging_rate_limited() {
            log::warn!("filesystem watcher lost sync for {watched_root:?}; scheduling rescan");
        }

        path_events.retain(|path_event| path_event.path != watched_root);
        path_events.push(PathEvent {
            path: watched_root.to_path_buf(),
            kind: Some(PathEventKind::Rescan),
        });
    }
    log::trace!("path_events: {:?}", path_events);
    enqueue_path_events(tx, pending_path_events, path_events);
}

fn watcher_logging_rate_limited() -> bool {
    static LAST_WARN: Mutex<Option<(Instant, usize)>> = Mutex::new(None);
    let Some((ref mut started, ref mut emitted)) = *LAST_WARN.lock() else {
        *LAST_WARN.lock() = Some((Instant::now(), 0));
        return false;
    };

    if started.elapsed().as_secs() < 1 {
        if *emitted < 20 {
            log::warn!("filesystem watcher lost sync for many files, not logging more");
            return true;
        } else {
            *emitted += 1;
        }
    } else {
        *emitted = 0;
        *started = Instant::now()
    }

    true
}

fn coalesce_pending_rescans(pending_paths: &mut Vec<PathEvent>, path_events: &mut Vec<PathEvent>) {
    if !path_events
        .iter()
        .any(|event| event.kind == Some(PathEventKind::Rescan))
    {
        return;
    }

    let mut new_rescan_paths: Vec<std::path::PathBuf> = path_events
        .iter()
        .filter(|e| e.kind == Some(PathEventKind::Rescan))
        .map(|e| e.path.clone())
        .collect();
    new_rescan_paths.sort_unstable();

    let mut deduped_rescans: Vec<std::path::PathBuf> = Vec::with_capacity(new_rescan_paths.len());
    for path in new_rescan_paths {
        if deduped_rescans
            .iter()
            .any(|ancestor| path != *ancestor && path.starts_with(ancestor))
        {
            continue;
        }
        deduped_rescans.push(path);
    }

    deduped_rescans.retain(|new_path| {
        !pending_paths
            .iter()
            .any(|pending| is_covered_rescan(pending.kind, new_path, &pending.path))
    });

    if !deduped_rescans.is_empty() {
        pending_paths.retain(|pending| {
            !deduped_rescans.iter().any(|rescan_path| {
                pending.path == *rescan_path
                    || is_covered_rescan(pending.kind, &pending.path, rescan_path)
            })
        });
    }

    path_events.retain(|event| {
        event.kind != Some(PathEventKind::Rescan) || deduped_rescans.contains(&event.path)
    });
}

fn is_covered_rescan(kind: Option<PathEventKind>, path: &Path, ancestor: &Path) -> bool {
    kind == Some(PathEventKind::Rescan) && path != ancestor && path.starts_with(ancestor)
}

#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
pub struct WatcherRegistrationId(u32);

/// Identifies one lifetime of an OS watch on a path: from the watch command
/// that asked for it to the unwatch command that releases it. Commands of the
/// same epoch may cancel out; commands of different epochs never do.
#[derive(Default, Debug, Copy, Clone, PartialEq, Eq, Hash)]
struct WatchEpoch(u64);

/// Resolves once the OS watch a registration asked for is in place (or failed).
/// A registration removed before its watch was applied resolves as cancelled.
pub(crate) type WatchApplied = oneshot::Receiver<anyhow::Result<()>>;

pub(crate) struct Registration {
    id: WatcherRegistrationId,
    applied: Option<WatchApplied>,
}

struct WatcherRegistrationState {
    callback: Arc<dyn Fn(&notify::Event) + Send + Sync>,
    key: WatchKey,
    path: Arc<SanitizedPath>,
    mode: WatcherMode,
}

struct PathRegistrationState {
    watcher_ids: Vec<WatcherRegistrationId>,
    /// The OS watch this path owns, or `None` when a recursive ancestor covers it.
    os_watch: Option<WatchEpoch>,
    /// Registrations waiting for `os_watch` to be applied. Empty once it is.
    waiters: Vec<oneshot::Sender<anyhow::Result<()>>>,
}

/// The registered watch paths for one watcher mode, keyed by [`WatchKey`] so that
/// add (dedup), remove, and dispatch share a single notion of path identity.
#[derive(Default)]
struct WatchPaths(HashMap<WatchKey, PathRegistrationState>);

impl WatchPaths {
    fn contains(&self, key: &WatchKey) -> bool {
        self.0.contains_key(key)
    }

    fn get_mut(&mut self, key: &WatchKey) -> Option<&mut PathRegistrationState> {
        self.0.get_mut(key)
    }

    fn entry(
        &mut self,
        key: WatchKey,
    ) -> std::collections::hash_map::Entry<'_, WatchKey, PathRegistrationState> {
        self.0.entry(key)
    }

    fn remove(&mut self, key: &WatchKey) -> Option<PathRegistrationState> {
        self.0.remove(key)
    }

    /// True if a recursive registration on a strict ancestor already covers
    /// `path`. Only poll watches and native macOS/Windows watches are recursive.
    fn covered_by_recursive_ancestor(&self, path: &SanitizedPath, mode: WatcherMode) -> bool {
        if mode != WatcherMode::Poll && !cfg!(any(target_os = "windows", target_os = "macos")) {
            return false;
        }
        path.as_path().ancestors().skip(1).any(|ancestor| {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            self.0.contains_key(&WatchKey::exact(ancestor))
                || self.0.contains_key(&WatchKey::folded(ancestor))
        })
    }

    /// Whether a strict ancestor of `path` has an OS watch of its own.
    fn has_own_watch_ancestor(&self, path: &SanitizedPath) -> bool {
        path.as_path().ancestors().skip(1).any(|ancestor| {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            [WatchKey::exact(ancestor), WatchKey::folded(ancestor)]
                .iter()
                .any(|key| {
                    self.0
                        .get(key)
                        .is_some_and(|state| state.os_watch.is_some())
                })
        })
    }

    /// The state of the nearest recursive ancestor registration of `path` whose
    /// OS watch is still queued, so a covered path can wait for that watch.
    fn pending_covering_state_mut(
        &mut self,
        path: &SanitizedPath,
        mode: WatcherMode,
    ) -> Option<&mut PathRegistrationState> {
        if mode != WatcherMode::Poll && !cfg!(any(target_os = "windows", target_os = "macos")) {
            return None;
        }
        let key = path.as_path().ancestors().skip(1).find_map(|ancestor| {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            [WatchKey::exact(ancestor), WatchKey::folded(ancestor)]
                .into_iter()
                .find(|key| {
                    self.0
                        .get(key)
                        .is_some_and(|state| !state.waiters.is_empty())
                })
        })?;
        self.0.get_mut(&key)
    }

    /// Collects the watcher ids of every registration whose directory is an
    /// ancestor of (or equal to) `path`. Both exact and folded keys are probed,
    /// so a real-cased event path matches a folded registration and vice versa.
    fn watcher_ids_covering(&self, path: &SanitizedPath, ids: &mut Vec<WatcherRegistrationId>) {
        for ancestor in path.as_path().ancestors() {
            let ancestor = SanitizedPath::unchecked_new(ancestor);
            if let Some(registration) = self.0.get(&WatchKey::exact(ancestor)) {
                ids.extend_from_slice(&registration.watcher_ids);
            }
            if let Some(registration) = self.0.get(&WatchKey::folded(ancestor)) {
                ids.extend_from_slice(&registration.watcher_ids);
            }
        }
    }
}

#[derive(Default)]
struct WatcherState {
    watchers: HashMap<WatcherRegistrationId, WatcherRegistrationState>,
    native_path_registrations: WatchPaths,
    poll_path_registrations: WatchPaths,
    cooldown_until: Option<Instant>,
    last_registration: WatcherRegistrationId,
    last_epoch: WatchEpoch,
}

impl WatcherState {
    fn is_native_watch_limit_cooldown_active(&self) -> bool {
        self.cooldown_until
            .is_some_and(|cooldown_until| cooldown_until > Instant::now())
    }

    fn start_native_watch_limit_cooldown(&mut self, path: &Path) {
        let should_log = !self.is_native_watch_limit_cooldown_active();
        self.cooldown_until = Some(Instant::now() + *NATIVE_WATCH_LIMIT_COOLDOWN);
        if should_log {
            log::warn!(
                "OS file watch limit reached while watching {path:?}; skipping new native file watcher registrations for {} seconds",
                NATIVE_WATCH_LIMIT_COOLDOWN.as_secs()
            );
        }
    }

    fn next_epoch(&mut self) -> WatchEpoch {
        self.last_epoch = WatchEpoch(self.last_epoch.0 + 1);
        self.last_epoch
    }

    fn path_registrations(&mut self, mode: WatcherMode) -> &mut WatchPaths {
        match mode {
            WatcherMode::Native => &mut self.native_path_registrations,
            WatcherMode::Poll => &mut self.poll_path_registrations,
        }
    }

    /// Drops a registration. When it was the last one on a path that owns an OS
    /// watch, returns a watch command for each path that watch was covering and
    /// that nothing else covers (those registrations stay, so they need a watch
    /// of their own), followed by the unwatch command that releases the watch.
    fn remove_registration(&mut self, id: WatcherRegistrationId) -> Vec<WatchCommand> {
        let Some(registration_state) = self.watchers.remove(&id) else {
            return Vec::new();
        };
        let mode = registration_state.mode;
        let paths = self.path_registrations(mode);
        let Some(path_state) = paths.get_mut(&registration_state.key) else {
            return Vec::new();
        };
        path_state.watcher_ids.retain(|&existing| existing != id);
        if !path_state.watcher_ids.is_empty() {
            return Vec::new();
        }
        let Some(path_state) = paths.remove(&registration_state.key) else {
            return Vec::new();
        };
        let Some(epoch) = path_state.os_watch else {
            return Vec::new();
        };
        // The covered paths get their own watch first and the ancestor's watch is
        // released last, so there is no moment when they are watched by neither.
        let mut commands = self.promote_covered(&registration_state.key, mode);
        commands.push(WatchCommand {
            op: WatchOp::Unwatch,
            mode,
            path: registration_state.path.clone(),
            key: registration_state.key.clone(),
            epoch,
        });
        commands
    }

    /// Gives an OS watch to every registered path that the watch on `released`
    /// was covering and that no other watch covers, shortest paths first so a
    /// path under another promoted one stays covered by it.
    fn promote_covered(&mut self, released: &WatchKey, mode: WatcherMode) -> Vec<WatchCommand> {
        let mut covered: Vec<(WatchKey, Arc<SanitizedPath>)> = Vec::new();
        for watcher in self.watchers.values() {
            let under_released = watcher.mode == mode
                && watcher.path.as_path().ancestors().skip(1).any(|ancestor| {
                    let ancestor = SanitizedPath::unchecked_new(ancestor);
                    WatchKey::exact(ancestor) == *released
                        || WatchKey::folded(ancestor) == *released
                });
            if under_released && !covered.iter().any(|(key, _)| *key == watcher.key) {
                covered.push((watcher.key.clone(), watcher.path.clone()));
            }
        }
        covered.sort_by_key(|(_, path)| path.as_path().components().count());
        let mut commands = Vec::new();
        for (key, path) in covered {
            let epoch = self.next_epoch();
            let paths = self.path_registrations(mode);
            let needs_watch = paths
                .get_mut(&key)
                .is_some_and(|state| state.os_watch.is_none())
                && !paths.has_own_watch_ancestor(&path);
            if needs_watch && let Some(state) = paths.get_mut(&key) {
                state.os_watch = Some(epoch);
                // Pending until the backend acknowledges the watch: a later
                // `applied_waiter` must wait for it. Nobody holds the receiver.
                let (waiter, _) = oneshot::channel();
                state.waiters.push(waiter);
                commands.push(WatchCommand {
                    op: WatchOp::Watch,
                    mode,
                    path,
                    key,
                    epoch,
                });
            }
        }
        commands
    }

    /// Records the outcome of a watch command. A failed watch is rolled back,
    /// so its path reads as unregistered and a later add retries it. Returns
    /// whether the command's registration was still present.
    fn finish_watch(&mut self, command: &WatchCommand, result: &anyhow::Result<()>) -> bool {
        let path_registrations = self.path_registrations(command.mode);
        let Some(path_state) = path_registrations
            .get_mut(&command.key)
            .filter(|path_state| path_state.os_watch == Some(command.epoch))
        else {
            return false;
        };
        let waiters = std::mem::take(&mut path_state.waiters);
        if result.is_err() {
            let path_state = path_registrations
                .remove(&command.key)
                .expect("path state was just found");
            for id in path_state.watcher_ids {
                self.watchers.remove(&id);
            }
            // Paths this watch covered have no OS watch of their own, unless
            // another watch still covers them. Drop the rest, every registration
            // of each, so their owners see them unregistered and retry, which
            // installs a watch for each.
            let mut affected: Vec<(WatchKey, Arc<SanitizedPath>)> = Vec::new();
            for watcher in self.watchers.values() {
                let under_failed_path = watcher.mode == command.mode
                    && watcher.path.as_path().ancestors().skip(1).any(|ancestor| {
                        let ancestor = SanitizedPath::unchecked_new(ancestor);
                        WatchKey::exact(ancestor) == command.key
                            || WatchKey::folded(ancestor) == command.key
                    });
                if under_failed_path && !affected.iter().any(|(key, _)| *key == watcher.key) {
                    affected.push((watcher.key.clone(), watcher.path.clone()));
                }
            }
            for (key, path) in affected {
                let paths = self.path_registrations(command.mode);
                let covered_only = paths
                    .get_mut(&key)
                    .is_some_and(|state| state.os_watch.is_none());
                if covered_only
                    && !paths.has_own_watch_ancestor(&path)
                    && let Some(state) = paths.remove(&key)
                {
                    for id in state.watcher_ids {
                        self.watchers.remove(&id);
                    }
                }
            }
        }
        for waiter in waiters {
            let outcome = match result {
                Ok(()) => Ok(()),
                Err(error) => Err(anyhow::anyhow!("{error:#}")),
            };
            waiter.send(outcome).ok();
        }
        true
    }
}

/// A native or poll backend. Every change goes through `update_paths`, so a
/// batch of path changes costs one backend restart where the backend has one
/// (FSEvents restarts its stream on every change).
trait WatchBackend: Send {
    fn update_paths(&mut self, ops: Vec<notify::PathOp>) -> Result<(), notify::UpdatePathsError>;
}

impl<T: notify::Watcher + Send> WatchBackend for T {
    fn update_paths(&mut self, ops: Vec<notify::PathOp>) -> Result<(), notify::UpdatePathsError> {
        notify::Watcher::update_paths(self, ops)
    }
}

type DispatchEvent = (WatcherMode, Result<notify::Event, notify::Error>);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum WatchOp {
    Watch,
    Unwatch,
}

struct WatchCommand {
    op: WatchOp,
    mode: WatcherMode,
    path: Arc<SanitizedPath>,
    key: WatchKey,
    epoch: WatchEpoch,
}

impl WatchCommand {
    fn path_op(&self) -> notify::PathOp {
        let path = self.path.as_path().to_path_buf();
        match (self.op, self.mode) {
            (WatchOp::Unwatch, _) => notify::PathOp::unwatch(path),
            (WatchOp::Watch, WatcherMode::Poll) => notify::PathOp::watch_recursive(path),
            (WatchOp::Watch, WatcherMode::Native)
                if cfg!(any(target_os = "windows", target_os = "macos")) =>
            {
                notify::PathOp::watch_recursive(path)
            }
            (WatchOp::Watch, WatcherMode::Native) => notify::PathOp::watch_non_recursive(path),
        }
    }
}

enum WorkerMessage {
    Command(WatchCommand),
    #[cfg(test)]
    Flush(std::sync::mpsc::Sender<()>),
}

/// The OS backends, one lock per mode so a slow native batch never holds up
/// a poll change.
struct WatchBackends {
    native: Mutex<Result<Box<dyn WatchBackend>, String>>,
    poll: Mutex<Option<Box<dyn WatchBackend>>>,
    make_poll: fn() -> notify::Result<Box<dyn WatchBackend>>,
}

/// Applies watch commands to the backends and records their outcome in the
/// shared state. Runs on the watcher thread for queued modes and on the
/// caller's thread for the rest.
struct WatchApplier {
    state: Arc<Mutex<WatcherState>>,
    backends: WatchBackends,
    /// Epochs whose watch failed after their registration was already removed,
    /// so the unwatch still in flight for them has nothing to release.
    failed_epochs: Mutex<HashSet<WatchEpoch>>,
}

impl WatchApplier {
    /// A watch followed by the unwatch of the same epoch never reaches the
    /// backend; an unwatch for an epoch whose watch failed has nothing to do.
    fn cancel_paired(&self, commands: Vec<WatchCommand>) -> Vec<WatchCommand> {
        let mut failed_epochs = self.failed_epochs.lock();
        let mut kept: Vec<Option<WatchCommand>> = Vec::with_capacity(commands.len());
        let mut unapplied_watches = HashMap::new();
        for command in commands {
            match command.op {
                WatchOp::Watch => {
                    unapplied_watches.insert(command.epoch, kept.len());
                    kept.push(Some(command));
                }
                WatchOp::Unwatch => {
                    if let Some(index) = unapplied_watches.remove(&command.epoch) {
                        kept[index] = None;
                    } else if !failed_epochs.remove(&command.epoch) {
                        kept.push(Some(command));
                    }
                }
            }
        }
        kept.into_iter().flatten().collect()
    }

    /// Applies `commands` for one mode as one `update_paths` batch, retrying
    /// what a failed op left unapplied. Never holds the state lock while the
    /// backend runs.
    fn apply(&self, mode: WatcherMode, commands: Vec<WatchCommand>) {
        let mut pending = self.cancel_paired(commands);
        if pending.is_empty() {
            return;
        }
        let mut finished = Vec::new();
        {
            let mut native;
            let mut poll;
            let backend: &mut Box<dyn WatchBackend> = match mode {
                WatcherMode::Native => {
                    native = self.backends.native.lock();
                    match native.as_mut() {
                        Ok(backend) => backend,
                        Err(error) => {
                            let message = format!("native file watcher unavailable: {error}");
                            drop(native);
                            for command in pending {
                                self.finish(command, Err(anyhow::anyhow!("{message}")));
                            }
                            return;
                        }
                    }
                }
                WatcherMode::Poll => {
                    poll = self.backends.poll.lock();
                    if poll.is_none() {
                        match (self.backends.make_poll)() {
                            Ok(backend) => *poll = Some(backend),
                            Err(error) => {
                                drop(poll);
                                let message = error.to_string();
                                for command in pending {
                                    self.finish(command, Err(anyhow::anyhow!("{message}")));
                                }
                                return;
                            }
                        }
                    }
                    poll.as_mut().expect("poll watcher initialized")
                }
            };
            while !pending.is_empty() {
                let ops = pending.iter().map(WatchCommand::path_op).collect();
                let Err(error) = backend.update_paths(ops) else {
                    finished.extend(pending.drain(..).map(|command| (command, Ok(()))));
                    break;
                };
                let notify::UpdatePathsError {
                    source,
                    origin,
                    remaining,
                } = error;
                let Some(_) = origin else {
                    // Every op was applied but the backend failed to restart.
                    let message = source.to_string();
                    finished.extend(
                        pending
                            .drain(..)
                            .map(|command| (command, Err(anyhow::anyhow!("{message}")))),
                    );
                    break;
                };
                let failed_index = pending.len() - remaining.len() - 1;
                let unapplied = pending.split_off(failed_index + 1);
                let failed = pending.pop().expect("the failed op is in the batch");
                finished.extend(pending.drain(..).map(|command| (command, Ok(()))));
                finished.push((failed, Err(source.into())));
                pending = unapplied;
            }
        }
        for (command, result) in finished {
            self.finish(command, result);
        }
    }

    fn finish(&self, command: WatchCommand, result: anyhow::Result<()>) {
        match (command.op, &result) {
            (WatchOp::Unwatch, Err(error)) if !is_watch_not_found_error(error) => {
                log::warn!("failed to unwatch {:?}: {error:#}", command.path.as_path());
            }
            (WatchOp::Unwatch, _) => {}
            (WatchOp::Watch, _) => {
                let mut state = self.state.lock();
                if let Err(error) = &result {
                    if command.mode == WatcherMode::Native && is_max_files_watch_error(error) {
                        state.start_native_watch_limit_cooldown(command.path.as_path());
                    } else {
                        log::warn!("failed to watch {:?}: {error:#}", command.path.as_path());
                    }
                }
                let registered = state.finish_watch(&command, &result);
                if result.is_err() && !registered {
                    self.failed_epochs.lock().insert(command.epoch);
                }
            }
        }
    }
}

/// Owns the queued modes' OS work. An FSEvents change stops the stream and
/// starts a new one over every watched path, and the start waits on
/// `fseventsd`, which can take seconds when it is busy; on this thread that
/// wait never blocks a caller. Commands queued while a batch runs are applied
/// together as the next batch, which costs one restart.
fn run_watch_worker(
    applier: Arc<WatchApplier>,
    messages: std::sync::mpsc::Receiver<WorkerMessage>,
) {
    while let Ok(first) = messages.recv() {
        let mut commands = Vec::new();
        #[cfg(test)]
        let mut flushes = Vec::new();
        for message in std::iter::once(first).chain(messages.try_iter()) {
            match message {
                WorkerMessage::Command(command) => commands.push(command),
                #[cfg(test)]
                WorkerMessage::Flush(done) => flushes.push(done),
            }
        }
        let (native, poll): (Vec<_>, Vec<_>) = commands
            .into_iter()
            .partition(|command| command.mode == WatcherMode::Native);
        applier.apply(WatcherMode::Native, native);
        applier.apply(WatcherMode::Poll, poll);
        #[cfg(test)]
        for done in flushes {
            done.send(()).ok();
        }
    }
}

/// Which modes go through the watcher thread. Only FSEvents needs it: its
/// changes restart a stream and wait on `fseventsd`. inotify, Windows, and poll
/// changes are applied on the caller's thread, so a caller that adds a watch
/// and then reads the directory never misses a change in between.
fn queued_in_production(mode: WatcherMode) -> bool {
    mode == WatcherMode::Native && cfg!(target_os = "macos")
}

pub struct GlobalWatcher {
    state: Arc<Mutex<WatcherState>>,
    applier: Arc<WatchApplier>,
    commands: std::sync::mpsc::Sender<WorkerMessage>,
    queued: fn(WatcherMode) -> bool,
    native_available: Result<(), String>,
    event_tx: async_channel::Sender<DispatchEvent>,
}

impl GlobalWatcher {
    fn new(
        event_tx: async_channel::Sender<DispatchEvent>,
        native: Result<Box<dyn WatchBackend>, String>,
        poll: Option<Box<dyn WatchBackend>>,
        make_poll: fn() -> notify::Result<Box<dyn WatchBackend>>,
        queued: fn(WatcherMode) -> bool,
    ) -> Self {
        let state = Arc::new(Mutex::new(WatcherState::default()));
        let native_available = native.as_ref().map(|_| ()).map_err(Clone::clone);
        let applier = Arc::new(WatchApplier {
            state: state.clone(),
            backends: WatchBackends {
                native: Mutex::new(native),
                poll: Mutex::new(poll),
                make_poll,
            },
            failed_epochs: Mutex::default(),
        });
        let (commands, messages) = std::sync::mpsc::channel();
        let worker_applier = applier.clone();
        std::thread::Builder::new()
            .name("fs-watcher".to_owned())
            .spawn(move || run_watch_worker(worker_applier, messages))
            .expect("failed to spawn fs watcher thread");
        Self {
            state,
            applier,
            commands,
            queued,
            native_available,
            event_tx,
        }
    }

    /// Registers `cb` for events under `path`. Never blocks on FSEvents: a new
    /// native watch on macOS is queued for the watcher thread, and `applied`
    /// resolves once it is in place. Other modes are applied before this
    /// returns. `None` while the native watch limit cooldown is active.
    fn add(
        &self,
        path: Arc<std::path::Path>,
        mode: WatcherMode,
        case_insensitive: bool,
        cb: impl Fn(&notify::Event) + Send + Sync + 'static,
    ) -> Option<Registration> {
        let path = SanitizedPath::from_arc(path);
        let key = WatchKey::for_registration(&path, case_insensitive);
        let (registration, command) = {
            let mut state = self.state.lock();
            let registrations_for_mode = state.path_registrations(mode);
            let covered = registrations_for_mode.covered_by_recursive_ancestor(&path, mode);
            let registered = registrations_for_mode.contains(&key);
            if !covered
                && !registered
                && mode == WatcherMode::Native
                && state.is_native_watch_limit_cooldown_active()
            {
                return None;
            }

            let id = state.last_registration;
            state.last_registration = WatcherRegistrationId(id.0 + 1);
            state.watchers.insert(
                id,
                WatcherRegistrationState {
                    callback: Arc::new(cb),
                    key: key.clone(),
                    path: path.clone(),
                    mode,
                },
            );

            let (waiter, applied) = oneshot::channel();
            if registered {
                let path_state = state
                    .path_registrations(mode)
                    .get_mut(&key)
                    .expect("registered path has state");
                path_state.watcher_ids.push(id);
                // A covered path has no OS watch of its own: its readiness is its
                // ancestor's.
                let covered_by_ancestor = path_state.os_watch.is_none();
                // The path's OS watch is still queued: wait for the same one.
                let applied = (!path_state.waiters.is_empty()).then(|| {
                    path_state.waiters.push(waiter);
                    applied
                });
                let applied = if applied.is_none() && covered_by_ancestor {
                    let (ancestor_waiter, ancestor_applied) = oneshot::channel();
                    state
                        .path_registrations(mode)
                        .pending_covering_state_mut(&path, mode)
                        .map(|ancestor| {
                            ancestor.waiters.push(ancestor_waiter);
                            ancestor_applied
                        })
                } else {
                    applied
                };
                return Some(Registration { id, applied });
            }

            let os_watch = (!covered).then(|| state.next_epoch());
            state
                .path_registrations(mode)
                .entry(key.clone())
                .or_insert(PathRegistrationState {
                    watcher_ids: vec![id],
                    os_watch,
                    waiters: if os_watch.is_some() {
                        vec![waiter]
                    } else {
                        Vec::new()
                    },
                });
            let Some(epoch) = os_watch else {
                // A recursive ancestor's watch covers this path. If that watch is
                // still queued, wait for it too: until it is in place nothing is
                // watching the path, whichever `FsWatcher` asked for the ancestor.
                let (ancestor_waiter, ancestor_applied) = oneshot::channel();
                let applied = state
                    .path_registrations(mode)
                    .pending_covering_state_mut(&path, mode)
                    .map(|ancestor| {
                        ancestor.waiters.push(ancestor_waiter);
                        ancestor_applied
                    });
                return Some(Registration { id, applied });
            };
            let command = WatchCommand {
                op: WatchOp::Watch,
                mode,
                path,
                key,
                epoch,
            };
            (
                Registration {
                    id,
                    applied: Some(applied),
                },
                command,
            )
        };
        self.submit(command);
        Some(registration)
    }

    /// For registration `id`: `None` when it no longer exists (removed, or its
    /// watch failed), `Some(None)` when its OS watch is already in place, and
    /// `Some(Some(receiver))` while that watch is still queued. Both facts come
    /// from one lock, so a caller can tell a vanished registration from an
    /// applied one and register again.
    fn applied_waiter(&self, id: WatcherRegistrationId) -> Option<Option<WatchApplied>> {
        let mut state = self.state.lock();
        let (key, mode, path) = state
            .watchers
            .get(&id)
            .map(|watcher| (watcher.key.clone(), watcher.mode, watcher.path.clone()))?;
        let paths = state.path_registrations(mode);
        // A covered path has no OS watch of its own: its readiness is that of
        // the ancestor watch covering it.
        let covered = paths
            .get_mut(&key)
            .is_some_and(|path_state| path_state.os_watch.is_none());
        let pending = if covered {
            paths.pending_covering_state_mut(&path, mode)
        } else {
            paths
                .get_mut(&key)
                .filter(|path_state| !path_state.waiters.is_empty())
        };
        let applied = pending.map(|path_state| {
            let (waiter, applied) = oneshot::channel();
            path_state.waiters.push(waiter);
            applied
        });
        Some(applied)
    }

    /// Whether `id` is still registered. A registration disappears when it is
    /// removed or when the OS watch it asked for failed.
    fn is_registered(&self, id: WatcherRegistrationId) -> bool {
        self.state.lock().watchers.contains_key(&id)
    }

    fn submit(&self, command: WatchCommand) {
        if !(self.queued)(command.mode) {
            self.applier.apply(command.mode, vec![command]);
            return;
        }
        if self.commands.send(WorkerMessage::Command(command)).is_err() {
            log::error!("fs watcher thread has exited; dropping a watch change");
        }
    }

    fn enqueue(&self, mode: WatcherMode, event: Result<notify::Event, notify::Error>) {
        if matches!(
            event,
            Ok(Event {
                kind: EventKind::Access(_),
                ..
            })
        ) {
            return;
        }

        // A failed send only happens once the dispatch thread has shut down, at
        // which point there's nothing left to dispatch to.
        self.event_tx.try_send((mode, event)).ok();
    }

    fn dispatch(&self, mode: WatcherMode, event: Result<notify::Event, notify::Error>) {
        let event = match event {
            Ok(event) => event,
            Err(error) => {
                log::warn!("watcher error for {mode:?}: {error}");
                return;
            }
        };

        log::trace!("global handle event for {mode:?}: {event:?}");

        let callbacks = {
            let state = self.state.lock();
            if event.need_rescan() {
                let callbacks = state
                    .watchers
                    .values()
                    .filter(|registration| registration.mode == mode)
                    .map(|registration| registration.callback.clone())
                    .collect::<Vec<_>>();
                log::warn!(
                    "filesystem watcher lost sync for {mode:?}; scheduling rescans for {} registrations",
                    callbacks.len()
                );
                callbacks
            } else {
                let path_registrations = match mode {
                    WatcherMode::Native => &state.native_path_registrations,
                    WatcherMode::Poll => &state.poll_path_registrations,
                };
                let mut ids = Vec::new();
                for path in &event.paths {
                    let sanitized = SanitizedPath::new(path);
                    path_registrations.watcher_ids_covering(sanitized, &mut ids);
                }
                ids.sort_unstable_by_key(|id| id.0);
                ids.dedup();
                ids.into_iter()
                    .filter_map(|id| state.watchers.get(&id))
                    .map(|registration| registration.callback.clone())
                    .collect::<Vec<_>>()
            }
        };

        for callback in callbacks {
            callback(&event);
        }
    }

    /// Drops a registration. Never blocks on FSEvents: releasing the OS watch
    /// it owned is queued for the watcher thread like any other change.
    pub fn remove(&self, id: WatcherRegistrationId) {
        let commands = self.state.lock().remove_registration(id);
        for command in commands {
            self.submit(command);
        }
    }

    /// Blocks until the watcher thread has applied every command sent so far.
    #[cfg(test)]
    fn flush(&self) {
        let (done, applied) = std::sync::mpsc::channel();
        self.commands
            .send(WorkerMessage::Flush(done))
            .expect("fs watcher thread is running");
        applied.recv().expect("fs watcher thread flushed");
    }
}

fn is_max_files_watch_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<notify::Error>()
        .is_some_and(|error| matches!(&error.kind, notify::ErrorKind::MaxFilesWatch))
}

/// inotify auto-removes a watch when its directory is deleted, so a later
/// unwatch races that and fails with a benign error. Either way the path is no
/// longer watched, which is all we wanted.
fn is_watch_not_found_error(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<notify::Error>()
        .is_some_and(|error| matches!(&error.kind, notify::ErrorKind::WatchNotFound))
}

static POLL_INTERVAL: LazyLock<Duration> = LazyLock::new(|| {
    let poll_ms: u64 = std::env::var("ZED_FILE_WATCHER_POLL_MS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(2000)
        .clamp(500, 30000);
    Duration::from_millis(poll_ms)
});

static NATIVE_WATCH_LIMIT_COOLDOWN: LazyLock<Duration> = LazyLock::new(|| {
    let cooldown_seconds: u64 = std::env::var("ZED_NATIVE_WATCH_LIMIT_COOLDOWN_SECONDS")
        .ok()
        .and_then(|value| value.parse().ok())
        .unwrap_or(5)
        .clamp(0, 300);
    Duration::from_secs(cooldown_seconds)
});

pub fn poll_interval() -> Duration {
    *POLL_INTERVAL
}

static FS_WATCHER_INSTANCE: OnceLock<GlobalWatcher> = OnceLock::new();

fn global_watcher() -> &'static GlobalWatcher {
    FS_WATCHER_INSTANCE.get_or_init(|| {
        let (event_tx, event_rx) = async_channel::unbounded::<DispatchEvent>();
        std::thread::Builder::new()
            .name("fs-watcher-dispatch".to_owned())
            .spawn(move || {
                while let Ok((mode, event)) = event_rx.recv_blocking() {
                    global_watcher().dispatch(mode, event);
                }
            })
            .expect("failed to spawn fs watcher dispatch thread");
        GlobalWatcher::new(
            event_tx,
            new_native_backend().map_err(|error| error.to_string()),
            None,
            new_poll_backend,
            queued_in_production,
        )
    })
}

/// Creating the backend starts no OS watch (FSEvents starts a stream only
/// once it has a path), so this is cheap on whichever thread first needs the
/// global watcher.
fn new_native_backend() -> notify::Result<Box<dyn WatchBackend>> {
    // CORE excludes Access events, which Zed discards anyway. Without this,
    // the default mask subscribes to inotify OPEN/CLOSE_* on Linux, so every
    // file read in a watched directory would queue events, increasing the
    // risk of queue overflows (and thus full rescans) under read-heavy
    // workloads like grep or language server indexing.
    let config = notify::Config::default().with_event_kinds(notify::EventKindMask::CORE);
    let watcher = <notify::RecommendedWatcher as notify::Watcher>::new(
        |event| global_watcher().enqueue(WatcherMode::Native, event),
        config,
    )?;
    Ok(Box::new(watcher))
}

fn new_poll_backend() -> notify::Result<Box<dyn WatchBackend>> {
    let config = notify::Config::default().with_poll_interval(*POLL_INTERVAL);
    let watcher = notify::PollWatcher::new(
        |event| global_watcher().enqueue(WatcherMode::Poll, event),
        config,
    )?;
    Ok(Box::new(watcher))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{collections::HashSet, path::PathBuf};

    fn rescan(path: &str) -> PathEvent {
        PathEvent {
            path: PathBuf::from(path),
            kind: Some(PathEventKind::Rescan),
        }
    }

    fn changed(path: &str) -> PathEvent {
        PathEvent {
            path: PathBuf::from(path),
            kind: Some(PathEventKind::Changed),
        }
    }

    #[derive(Default)]
    struct FakeWatchBackend {
        watched_paths: HashSet<PathBuf>,
        watch_calls: Vec<PathBuf>,
        unwatch_calls: Vec<PathBuf>,
        /// Ops per `update_paths` call, i.e. per backend restart.
        batches: Vec<usize>,
        fail_with_watch_limit: bool,
        /// Watches of these paths fail; every other op succeeds.
        failing_paths: HashSet<PathBuf>,
        /// How long each `update_paths` call takes, like a busy `fseventsd`.
        delay: Duration,
    }

    struct SharedFakeWatchBackend(Arc<Mutex<FakeWatchBackend>>);

    impl WatchBackend for SharedFakeWatchBackend {
        fn update_paths(
            &mut self,
            ops: Vec<notify::PathOp>,
        ) -> Result<(), notify::UpdatePathsError> {
            // Whether this batch fails is decided when it starts, as it would
            // be for an OS call already in flight.
            let (delay, fail) = {
                let backend = self.0.lock();
                (backend.delay, backend.fail_with_watch_limit)
            };
            std::thread::sleep(delay);
            let mut backend = self.0.lock();
            backend.batches.push(ops.len());
            let mut ops = ops.into_iter();
            while let Some(op) = ops.next() {
                let result = match &op {
                    notify::PathOp::Watch(path, _) => {
                        backend.watch_calls.push(path.clone());
                        if fail || backend.failing_paths.contains(path) {
                            Err(notify::Error::new(notify::ErrorKind::MaxFilesWatch))
                        } else {
                            backend.watched_paths.insert(path.clone());
                            Ok(())
                        }
                    }
                    notify::PathOp::Unwatch(path) => {
                        backend.unwatch_calls.push(path.clone());
                        if backend.watched_paths.remove(path) {
                            Ok(())
                        } else {
                            Err(notify::Error::generic("path was not watched"))
                        }
                    }
                };
                if let Err(source) = result {
                    return Err(notify::UpdatePathsError {
                        source,
                        origin: Some(op),
                        remaining: ops.collect(),
                    });
                }
            }
            Ok(())
        }
    }

    fn no_poll_backend() -> notify::Result<Box<dyn WatchBackend>> {
        Err(notify::Error::generic("no poll backend in this test"))
    }

    fn test_watcher(poll_watcher: Arc<Mutex<FakeWatchBackend>>) -> GlobalWatcher {
        test_watcher_with_backends(None, Some(poll_watcher))
    }

    /// Queues every mode on the watcher thread, the way macOS queues FSEvents.
    fn test_watcher_with_backends(
        native_watcher: Option<Arc<Mutex<FakeWatchBackend>>>,
        poll_watcher: Option<Arc<Mutex<FakeWatchBackend>>>,
    ) -> GlobalWatcher {
        watcher_with(native_watcher, poll_watcher, |_| true)
    }

    fn watcher_with(
        native_watcher: Option<Arc<Mutex<FakeWatchBackend>>>,
        poll_watcher: Option<Arc<Mutex<FakeWatchBackend>>>,
        queued: fn(WatcherMode) -> bool,
    ) -> GlobalWatcher {
        // Tests call `dispatch` directly to exercise dispatch synchronously,
        // rather than going through the OS watcher callbacks, so nothing is ever
        // sent on this channel; the receiver can just be dropped.
        let (event_tx, _event_rx) = async_channel::unbounded();
        GlobalWatcher::new(
            event_tx,
            native_watcher
                .map(|watcher| Box::new(SharedFakeWatchBackend(watcher)) as Box<dyn WatchBackend>)
                .ok_or_else(|| "no native backend in this test".to_owned()),
            poll_watcher
                .map(|watcher| Box::new(SharedFakeWatchBackend(watcher)) as Box<dyn WatchBackend>),
            no_poll_backend,
            queued,
        )
    }

    struct TestCase {
        name: &'static str,
        pending_paths: Vec<PathEvent>,
        path_events: Vec<PathEvent>,
        expected_pending_paths: Vec<PathEvent>,
        expected_path_events: Vec<PathEvent>,
    }

    #[test]
    fn covered_child_registration_is_not_unwatched_after_parent_is_removed() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher(backend.clone());
        let parent = Arc::<Path>::from(Path::new("/repo"));
        let child = Arc::<Path>::from(Path::new("/repo/foo.csproj"));

        let parent_registration = watcher
            .add(parent.as_ref().into(), WatcherMode::Poll, false, |_| {})
            .expect("parent watch registered");
        let child_registration = watcher
            .add(child.as_ref().into(), WatcherMode::Poll, false, |_| {})
            .expect("child watch registered");
        watcher.flush();

        watcher.remove(parent_registration.id);
        watcher.remove(child_registration.id);
        watcher.flush();

        let backend = backend.lock();
        assert_eq!(backend.watch_calls, &[parent.to_path_buf()]);
        assert_eq!(backend.unwatch_calls, &[parent.to_path_buf()]);
    }

    #[test]
    fn native_watch_limit_cools_down_subsequent_native_registrations() {
        let native_backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            ..Default::default()
        }));
        let poll_backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(native_backend.clone()), Some(poll_backend));
        let first_path = Arc::<Path>::from(Path::new("/repo/first"));
        let second_path = Arc::<Path>::from(Path::new("/repo/second"));

        let first_registration = watcher
            .add(first_path.clone(), WatcherMode::Native, false, |_| {})
            .expect("the first registration is queued");
        watcher.flush();
        let second_registration = watcher.add(second_path, WatcherMode::Native, false, |_| {});

        assert!(
            !watcher.is_registered(first_registration.id),
            "a registration whose watch hit the limit is rolled back"
        );
        assert!(
            second_registration.is_none(),
            "the cooldown skips new native registrations"
        );

        let native_backend = native_backend.lock();
        assert_eq!(native_backend.watch_calls, &[first_path.to_path_buf()]);
    }

    fn slow_backend(delay: Duration) -> Arc<Mutex<FakeWatchBackend>> {
        Arc::new(Mutex::new(FakeWatchBackend {
            delay,
            ..Default::default()
        }))
    }

    fn poll_path(path: &str) -> Arc<Path> {
        Arc::<Path>::from(Path::new(path))
    }

    #[test]
    fn a_failed_op_in_a_batch_leaves_the_ops_around_it_applied() {
        let backend = slow_backend(Duration::from_millis(200));
        backend
            .lock()
            .failing_paths
            .insert(PathBuf::from("/repo/bad"));
        let watcher = test_watcher(backend.clone());

        watcher
            .add(poll_path("/repo/busy"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        std::thread::sleep(Duration::from_millis(50));
        let first = watcher
            .add(poll_path("/repo/first"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let bad = watcher
            .add(poll_path("/repo/bad"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let last = watcher
            .add(poll_path("/repo/last"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        watcher.flush();

        let backend = backend.lock();
        // The busy watch, then one batch that stops at the failure, then the
        // untried op retried on its own.
        assert_eq!(backend.batches, vec![1, 3, 1]);
        assert!(backend.watched_paths.contains(Path::new("/repo/first")));
        assert!(backend.watched_paths.contains(Path::new("/repo/last")));
        assert!(!backend.watched_paths.contains(Path::new("/repo/bad")));
        assert!(watcher.is_registered(first.id));
        assert!(watcher.is_registered(last.id));
        assert!(
            !watcher.is_registered(bad.id),
            "only the failed watch is rolled back"
        );
    }

    #[test]
    fn the_watcher_thread_exits_when_the_watcher_is_dropped() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher(backend.clone());
        watcher
            .add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        watcher.flush();

        drop(watcher);

        // The thread owns a handle to the backend until it exits.
        let deadline = Instant::now() + Duration::from_secs(5);
        while Arc::strong_count(&backend) > 1 {
            assert!(
                Instant::now() < deadline,
                "the watcher thread outlived its watcher"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn only_fsevents_is_queued_in_production() {
        assert_eq!(
            queued_in_production(WatcherMode::Native),
            cfg!(target_os = "macos")
        );
        assert!(!queued_in_production(WatcherMode::Poll));
    }

    #[test]
    fn an_inline_mode_is_watched_before_add_returns() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = watcher_with(None, Some(backend.clone()), |_| false);

        let registration = watcher
            .add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {})
            .expect("registered");

        // A caller that adds a watch and then reads the directory must not miss
        // a change made in between, so nothing may be left to a later thread.
        assert!(
            backend
                .lock()
                .watched_paths
                .contains(&PathBuf::from("/repo/root"))
        );
        let mut applied = registration
            .applied
            .expect("a new OS watch reports its outcome");
        assert!(matches!(applied.try_recv(), Ok(Some(Ok(())))));

        watcher.remove(registration.id);
        assert_eq!(
            backend.lock().unwatch_calls,
            vec![PathBuf::from("/repo/root")]
        );
    }

    #[test]
    fn add_and_remove_never_wait_for_the_os() {
        let backend = slow_backend(Duration::from_millis(500));
        let watcher = test_watcher(backend.clone());

        let started = Instant::now();
        let first = watcher
            .add(poll_path("/repo/a"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let second = watcher
            .add(poll_path("/repo/b"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        watcher.remove(first.id);
        watcher.remove(second.id);
        let elapsed = started.elapsed();

        assert!(
            elapsed < Duration::from_millis(100),
            "add/remove blocked the caller for {elapsed:?} behind a 500 ms backend"
        );
        watcher.flush();
    }

    #[test]
    fn changes_queued_while_the_backend_is_busy_apply_as_one_batch() {
        let backend = slow_backend(Duration::from_millis(200));
        let watcher = test_watcher(backend.clone());

        watcher
            .add(poll_path("/repo/first"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        // Let the worker start applying the first watch before queueing more.
        std::thread::sleep(Duration::from_millis(50));
        for ix in 0..20 {
            watcher
                .add(
                    poll_path(&format!("/repo/dir-{ix}")),
                    WatcherMode::Poll,
                    false,
                    |_| {},
                )
                .expect("registered");
        }
        watcher.flush();

        assert_eq!(backend.lock().batches, vec![1, 20]);
        assert_eq!(backend.lock().watched_paths.len(), 21);
    }

    #[test]
    fn a_watch_removed_before_it_was_applied_never_reaches_the_backend() {
        let backend = slow_backend(Duration::from_millis(200));
        let watcher = test_watcher(backend.clone());

        watcher
            .add(poll_path("/repo/busy"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        std::thread::sleep(Duration::from_millis(50));
        let transient = watcher
            .add(
                poll_path("/repo/transient"),
                WatcherMode::Poll,
                false,
                |_| {},
            )
            .expect("registered");
        watcher.remove(transient.id);
        watcher.flush();

        let backend = backend.lock();
        assert_eq!(backend.watch_calls, vec![PathBuf::from("/repo/busy")]);
        assert!(backend.unwatch_calls.is_empty());
    }

    #[test]
    fn applied_resolves_after_the_backend_applied_the_watch() {
        let backend = slow_backend(Duration::from_millis(100));
        let watcher = test_watcher(backend.clone());

        let registration = watcher
            .add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let applied = registration.applied.expect("a new OS watch is awaited");

        smol::block_on(applied)
            .expect("not cancelled")
            .expect("watch applied");
        assert!(
            backend
                .lock()
                .watched_paths
                .contains(&PathBuf::from("/repo/root")),
            "applied resolved before the backend watched the path"
        );
    }

    #[test]
    fn a_second_waiter_for_a_queued_registration_waits_for_the_same_watch() {
        let backend = slow_backend(Duration::from_millis(200));
        let watcher = test_watcher(backend.clone());

        let registration = watcher
            .add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        // An earlier plain `add` dropped its receiver; a later `add_and_wait`
        // for the same path must still wait for the watch.
        drop(registration.applied);
        let applied = watcher
            .applied_waiter(registration.id)
            .expect("the registration exists")
            .expect("the watch is still queued");

        smol::block_on(applied)
            .expect("not cancelled")
            .expect("watch applied");
        assert!(
            backend
                .lock()
                .watched_paths
                .contains(&PathBuf::from("/repo/root")),
            "the second waiter resolved before the backend watched the path"
        );
    }

    #[test]
    fn a_path_covered_by_a_queued_ancestor_waits_for_the_ancestors_watch() {
        let backend = slow_backend(Duration::from_millis(200));
        let watcher = test_watcher(backend.clone());

        let _ancestor = watcher
            .add(poll_path("/repo"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        // Another `FsWatcher` asks for a child while the ancestor's watch is
        // still queued: there is no OS watch of its own, but it must not report
        // ready before the ancestor's is in place.
        let child = watcher
            .add(poll_path("/repo/sub"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let applied = child.applied.expect("waits for the covering watch");

        smol::block_on(applied)
            .expect("not cancelled")
            .expect("watch applied");
        assert!(
            backend
                .lock()
                .watched_paths
                .contains(&PathBuf::from("/repo")),
            "the child resolved before the ancestor was watched"
        );
    }

    #[test]
    fn a_path_covered_by_a_removed_ancestor_gets_a_watch_of_its_own() {
        let backend = slow_backend(Duration::from_millis(200));
        let watcher = test_watcher(backend.clone());

        // Keep the worker busy so the ancestor is still queued when it is removed.
        watcher
            .add(poll_path("/busy"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        std::thread::sleep(Duration::from_millis(50));
        let ancestor = watcher
            .add(poll_path("/repo"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let child = watcher
            .add(poll_path("/repo/sub"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        watcher.remove(ancestor.id);
        watcher.flush();

        assert!(watcher.is_registered(child.id));
        let backend = backend.lock();
        assert!(
            backend.watched_paths.contains(&PathBuf::from("/repo/sub")),
            "the child lost its coverage and was given no watch"
        );
        assert!(!backend.watched_paths.contains(&PathBuf::from("/repo")));
    }

    #[test]
    fn applied_reports_a_failed_watch_and_rolls_it_back() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            ..Default::default()
        }));
        let watcher = test_watcher(backend);

        let registration = watcher
            .add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let result = smol::block_on(registration.applied.expect("awaited")).expect("not cancelled");

        assert!(result.is_err());
        assert!(!watcher.is_registered(registration.id));
        let retry = watcher.add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {});
        assert!(
            retry.and_then(|retry| retry.applied).is_some(),
            "a rolled-back path asks for a new OS watch"
        );
        watcher.flush();
    }

    #[test]
    fn a_second_registration_waits_for_the_pending_watch_it_shares() {
        let backend = slow_backend(Duration::from_millis(100));
        let watcher = test_watcher(backend.clone());

        let first = watcher
            .add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        let second = watcher
            .add(poll_path("/repo/root"), WatcherMode::Poll, false, |_| {})
            .expect("registered");

        for applied in [first.applied, second.applied] {
            smol::block_on(applied.expect("both wait for the shared watch"))
                .expect("not cancelled")
                .expect("watch applied");
        }
        assert_eq!(backend.lock().watch_calls.len(), 1);
    }

    #[test]
    fn the_unwatch_of_a_failed_watch_does_not_cancel_a_newer_watch() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend {
            fail_with_watch_limit: true,
            delay: Duration::from_millis(200),
            ..Default::default()
        }));
        let watcher = test_watcher(backend.clone());
        let path = poll_path("/repo/root");

        let failing = watcher
            .add(path.clone(), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        // The worker is now applying (and failing) the first watch.
        std::thread::sleep(Duration::from_millis(50));
        watcher.remove(failing.id);
        backend.lock().fail_with_watch_limit = false;
        let replacement = watcher
            .add(path.clone(), WatcherMode::Poll, false, |_| {})
            .expect("registered");
        watcher.flush();

        let backend = backend.lock();
        assert!(watcher.is_registered(replacement.id));
        assert!(backend.watched_paths.contains(path.as_ref()));
        assert!(
            backend.unwatch_calls.is_empty(),
            "the failed watch left nothing to unwatch"
        );
    }

    fn modify_event(path: &str) -> notify::Event {
        notify::Event {
            paths: vec![PathBuf::from(path)],
            ..notify::Event::new(EventKind::Modify(notify::event::ModifyKind::Any))
        }
    }

    fn recording_watcher() -> (GlobalWatcher, Arc<Mutex<Vec<String>>>) {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(backend), None);
        let fired = Arc::new(Mutex::new(Vec::new()));
        for dir in ["/repo/a", "/repo/a/nested", "/repo/b"] {
            let fired = fired.clone();
            let label = dir.to_owned();
            watcher
                .add(
                    Arc::<Path>::from(Path::new(dir)),
                    WatcherMode::Native,
                    false,
                    move |_| {
                        fired.lock().push(label.clone());
                    },
                )
                .expect("watch registered");
        }
        watcher.flush();
        (watcher, fired)
    }

    #[test]
    fn event_dispatches_only_to_registrations_covering_its_path() {
        let (watcher, fired) = recording_watcher();

        watcher.dispatch(WatcherMode::Native, Ok(modify_event("/repo/a/file.txt")));

        // Only the directory containing the file resolves; siblings stay untouched.
        assert_eq!(*fired.lock(), vec!["/repo/a".to_owned()]);
    }

    #[test]
    fn event_dispatches_to_every_ancestor_registration() {
        let (watcher, fired) = recording_watcher();

        watcher.dispatch(
            WatcherMode::Native,
            Ok(modify_event("/repo/a/nested/file.txt")),
        );

        // Both the directory containing the file and the ancestor watching it
        // resync, each exactly once, matching the previous broadcast-and-filter
        // behavior.
        let mut got = fired.lock().clone();
        got.sort();
        assert_eq!(got, vec!["/repo/a".to_owned(), "/repo/a/nested".to_owned()]);
    }

    fn fired_count() -> (
        Arc<Mutex<usize>>,
        impl Fn(&notify::Event) + Send + Sync + 'static,
    ) {
        let fired = Arc::new(Mutex::new(0usize));
        let cb = {
            let fired = fired.clone();
            move |_: &notify::Event| *fired.lock() += 1
        };
        (fired, cb)
    }

    #[test]
    fn watch_key_folds_case_but_keeps_exact_distinct() {
        let mixed = SanitizedPath::new(Path::new("/Repo/Proj"));
        let lower = SanitizedPath::new(Path::new("/repo/proj"));

        // Folded keys collide regardless of casing; exact keys do not.
        assert_eq!(WatchKey::folded(mixed), WatchKey::folded(lower));
        assert_ne!(WatchKey::exact(mixed), WatchKey::exact(lower));
        // Exact and folded live in different key spaces even for the same path.
        assert_ne!(WatchKey::exact(mixed), WatchKey::folded(mixed));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn watch_key_folds_unicode_normalization_on_macos() {
        // "Café" precomposed (NFC) vs decomposed (NFD) are different byte
        // sequences but the same directory on a normalization-insensitive volume.
        let nfc = Path::new("/repo/Caf\u{00e9}");
        let nfd = Path::new("/repo/Cafe\u{0301}");
        assert_ne!(nfc, nfd);
        assert_eq!(
            WatchKey::folded(SanitizedPath::new(nfc)),
            WatchKey::folded(SanitizedPath::new(nfd)),
        );
    }

    #[test]
    fn case_insensitive_registration_matches_differently_cased_event() {
        let (fired, cb) = fired_count();
        let watcher = test_watcher_with_backends(Some(Default::default()), None);
        watcher
            .add(
                Path::new("/Repo/Project").into(),
                WatcherMode::Native,
                true,
                cb,
            )
            .expect("registered");

        // Event arrives lowercased (as TSGO/macOS may report it).
        watcher.dispatch(
            WatcherMode::Native,
            Ok(modify_event("/repo/project/file.txt")),
        );
        assert_eq!(*fired.lock(), 1);
    }

    #[test]
    fn case_insensitive_registration_survives_case_only_rename() {
        let (fired, cb) = fired_count();
        let watcher = test_watcher_with_backends(Some(Default::default()), None);
        watcher
            .add(
                Path::new("/Repo/Proj").into(),
                WatcherMode::Native,
                true,
                cb,
            )
            .expect("registered");

        // The watched directory was renamed to a different casing; events now
        // arrive under the new spelling.
        watcher.dispatch(WatcherMode::Native, Ok(modify_event("/Repo/PROJ/file.txt")));
        assert_eq!(*fired.lock(), 1);
    }

    #[test]
    fn case_sensitive_registration_ignores_differently_cased_event() {
        let (fired, cb) = fired_count();
        let watcher = test_watcher_with_backends(Some(Default::default()), None);
        watcher
            .add(
                Path::new("/Repo/proj").into(),
                WatcherMode::Native,
                false,
                cb,
            )
            .expect("registered");

        // On a case-sensitive volume these are genuinely different directories.
        watcher.dispatch(WatcherMode::Native, Ok(modify_event("/Repo/PROJ/file.txt")));
        assert_eq!(*fired.lock(), 0);
    }

    #[test]
    fn differently_cased_adds_dedupe_on_case_insensitive_volume() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(backend.clone()), None);
        watcher
            .add(
                Path::new("/Repo/Proj").into(),
                WatcherMode::Native,
                true,
                |_| {},
            )
            .expect("registered");
        watcher
            .add(
                Path::new("/repo/proj").into(),
                WatcherMode::Native,
                true,
                |_| {},
            )
            .expect("registered");

        watcher.flush();

        // The second, differently-cased spelling reuses the same OS watch.
        assert_eq!(backend.lock().watch_calls.len(), 1);
    }

    #[test]
    fn recursive_parent_covers_differently_cased_child() {
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher(backend.clone());
        watcher
            .add(Path::new("/Repo").into(), WatcherMode::Poll, true, |_| {})
            .expect("registered");
        watcher
            .add(
                Path::new("/repo/child").into(),
                WatcherMode::Poll,
                true,
                |_| {},
            )
            .expect("registered");
        watcher.flush();

        // The child is covered by the recursive parent despite the case mismatch.
        assert_eq!(backend.lock().watch_calls, vec![PathBuf::from("/Repo")]);
    }

    #[test]
    fn rescan_event_broadcasts_to_all_registrations_of_the_same_mode() {
        let (watcher, fired) = recording_watcher();

        let rescan = notify::Event::new(EventKind::Other).set_flag(notify::event::Flag::Rescan);
        watcher.dispatch(WatcherMode::Native, Ok(rescan));

        // A pathless rescan may have missed events anywhere, so every registration
        // of that mode resyncs, regardless of which directory it watches.
        let mut got = fired.lock().clone();
        got.sort();
        assert_eq!(
            got,
            vec![
                "/repo/a".to_owned(),
                "/repo/a/nested".to_owned(),
                "/repo/b".to_owned(),
            ]
        );
    }

    #[test]
    #[cfg(target_os = "windows")]
    fn event_dispatches_when_reported_path_has_verbatim_prefix() {
        // `notify` (and Windows APIs generally) can report a changed path using the
        // verbatim `\\?\` long-path form even when the directory was registered
        // without it.
        let backend = Arc::new(Mutex::new(FakeWatchBackend::default()));
        let watcher = test_watcher_with_backends(Some(backend), None);
        let fired = Arc::new(Mutex::new(Vec::new()));
        {
            let fired = fired.clone();
            watcher
                .add(
                    Arc::<Path>::from(Path::new("C:\\repo\\src")),
                    WatcherMode::Native,
                    false,
                    move |_| fired.lock().push(()),
                )
                .expect("watch registered");
        }

        watcher.dispatch(
            WatcherMode::Native,
            Ok(modify_event("\\\\?\\C:\\repo\\src\\main.rs")),
        );

        assert_eq!(fired.lock().len(), 1);
    }

    #[test]
    fn test_coalesce_pending_rescans() {
        let test_cases = [
            TestCase {
                name: "coalesces descendant rescans under pending ancestor",
                pending_paths: vec![rescan("/root")],
                path_events: vec![rescan("/root/child"), rescan("/root/child/grandchild")],
                expected_pending_paths: vec![rescan("/root")],
                expected_path_events: vec![],
            },
            TestCase {
                name: "new ancestor rescan replaces pending descendant rescans",
                pending_paths: vec![
                    changed("/other"),
                    rescan("/root/child"),
                    rescan("/root/child/grandchild"),
                ],
                path_events: vec![rescan("/root")],
                expected_pending_paths: vec![changed("/other")],
                expected_path_events: vec![rescan("/root")],
            },
            TestCase {
                name: "same path rescan replaces pending non-rescan event",
                pending_paths: vec![changed("/root")],
                path_events: vec![rescan("/root")],
                expected_pending_paths: vec![],
                expected_path_events: vec![rescan("/root")],
            },
            TestCase {
                name: "unrelated rescans are preserved",
                pending_paths: vec![rescan("/root-a")],
                path_events: vec![rescan("/root-b")],
                expected_pending_paths: vec![rescan("/root-a")],
                expected_path_events: vec![rescan("/root-b")],
            },
            TestCase {
                name: "batch ancestor rescan replaces descendant rescan",
                pending_paths: vec![],
                path_events: vec![rescan("/root/child"), rescan("/root")],
                expected_pending_paths: vec![],
                expected_path_events: vec![rescan("/root")],
            },
        ];

        for test_case in test_cases {
            let mut pending_paths = test_case.pending_paths;
            let mut path_events = test_case.path_events;

            coalesce_pending_rescans(&mut pending_paths, &mut path_events);

            assert_eq!(
                pending_paths, test_case.expected_pending_paths,
                "pending_paths mismatch for case: {}",
                test_case.name
            );
            assert_eq!(
                path_events, test_case.expected_path_events,
                "path_events mismatch for case: {}",
                test_case.name
            );
        }
    }
}

pub fn global<T>(f: impl FnOnce(&GlobalWatcher) -> T) -> anyhow::Result<T> {
    let global_watcher = global_watcher();
    if let Err(error) = &global_watcher.native_available {
        anyhow::bail!("native file watcher unavailable: {error}");
    }
    Ok(f(global_watcher))
}
