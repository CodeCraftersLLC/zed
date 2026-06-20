use std::path::Path;

use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use tokio::sync::mpsc;

#[derive(Debug, Clone)]
pub enum BranchEvent {
    BranchTipChanged {
        branch: String,
        old_oid: Option<String>,
        new_oid: String,
    },
    ForceDetected {
        branch: String,
        old_oid: String,
        new_oid: String,
    },
    BranchDeleted {
        branch: String,
    },
    WatcherError(String),
}

pub struct BranchWatcher {
    _watcher: Option<RecommendedWatcher>,
    receiver: mpsc::UnboundedReceiver<BranchEvent>,
}

impl BranchWatcher {
    pub fn start(repo_path: &Path) -> Self {
        let (tx, rx) = mpsc::unbounded_channel();

        let git_dir = if repo_path.join(".git").is_dir() {
            repo_path.join(".git")
        } else {
            repo_path.to_path_buf()
        };

        let refs_dir = git_dir.join("refs").join("heads");
        let head_file = git_dir.join("HEAD");

        let tx_clone = tx.clone();
        let watcher_result = notify::recommended_watcher(move |res: std::result::Result<Event, notify::Error>| {
            match res {
                Ok(event) => {
                    if matches!(
                        event.kind,
                        EventKind::Modify(_) | EventKind::Create(_) | EventKind::Remove(_)
                    ) {
                        for path in &event.paths {
                            if let Some(branch) = extract_branch_name(path, &refs_dir) {
                                match event.kind {
                                    EventKind::Remove(_) => {
                                        let _ = tx_clone.send(BranchEvent::BranchDeleted {
                                            branch,
                                        });
                                    }
                                    _ => {
                                        let new_oid = std::fs::read_to_string(path)
                                            .unwrap_or_default()
                                            .trim()
                                            .to_string();
                                        let _ = tx_clone.send(BranchEvent::BranchTipChanged {
                                            branch,
                                            old_oid: None,
                                            new_oid,
                                        });
                                    }
                                }
                            }
                        }
                    }
                }
                Err(e) => {
                    let _ = tx_clone.send(BranchEvent::WatcherError(e.to_string()));
                }
            }
        });

        let watcher = match watcher_result {
            Ok(mut w) => {
                let refs_path = git_dir.join("refs").join("heads");
                if refs_path.exists() {
                    let _ = w.watch(&refs_path, RecursiveMode::Recursive);
                }
                let _ = w.watch(&head_file, RecursiveMode::NonRecursive);
                Some(w)
            }
            Err(e) => {
                let _ = tx.send(BranchEvent::WatcherError(format!(
                    "Failed to create watcher: {e}"
                )));
                None
            }
        };

        Self {
            _watcher: watcher,
            receiver: rx,
        }
    }

    pub async fn next_event(&mut self) -> Option<BranchEvent> {
        self.receiver.recv().await
    }

    pub fn try_next_event(&mut self) -> Option<BranchEvent> {
        self.receiver.try_recv().ok()
    }
}

fn extract_branch_name(path: &Path, refs_dir: &Path) -> Option<String> {
    path.strip_prefix(refs_dir)
        .ok()
        .map(|rel| rel.to_string_lossy().to_string())
}

pub fn detect_force_push(
    repo_path: &Path,
    old_oid: &str,
    new_oid: &str,
) -> bool {
    let repo = match git2::Repository::discover(repo_path) {
        Ok(r) => r,
        Err(_) => return false,
    };

    let old = match git2::Oid::from_str(old_oid) {
        Ok(o) => o,
        Err(_) => return false,
    };

    let new = match git2::Oid::from_str(new_oid) {
        Ok(o) => o,
        Err(_) => return false,
    };

    if old == new {
        return false;
    }

    !repo.graph_descendant_of(new, old).unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn extract_branch_name_from_path() {
        let refs_dir = PathBuf::from("/repo/.git/refs/heads");
        let path = PathBuf::from("/repo/.git/refs/heads/main");
        assert_eq!(
            extract_branch_name(&path, &refs_dir),
            Some("main".to_string())
        );
    }

    #[test]
    fn extract_nested_branch_name() {
        let refs_dir = PathBuf::from("/repo/.git/refs/heads");
        let path = PathBuf::from("/repo/.git/refs/heads/feature/login");
        assert_eq!(
            extract_branch_name(&path, &refs_dir),
            Some("feature/login".to_string())
        );
    }

    #[test]
    fn extract_returns_none_for_unrelated_path() {
        let refs_dir = PathBuf::from("/repo/.git/refs/heads");
        let path = PathBuf::from("/other/path");
        assert_eq!(extract_branch_name(&path, &refs_dir), None);
    }

    #[test]
    fn force_push_detection_on_nonexistent_repo() {
        assert!(!detect_force_push(Path::new("/nonexistent"), "abc", "def"));
    }

    #[test]
    fn force_push_same_oid_is_not_force() {
        let tmp = tempfile::tempdir().unwrap();
        git2::Repository::init(tmp.path()).unwrap();
        assert!(!detect_force_push(tmp.path(), "abc", "abc"));
    }
}
