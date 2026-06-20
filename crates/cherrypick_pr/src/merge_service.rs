use std::collections::HashMap;
use std::path::Path;

use crate::error::{PrError, Result};
use crate::types::{ContentEncoding, FileContent, MergeStrategy, ThreeWayContent};

pub struct MergeSession {
    pub source_oid: String,
    pub target_oid: String,
    pub target_branch: String,
    pub conflicted_paths: Vec<String>,
    pub resolutions: HashMap<String, Vec<u8>>,
}

pub struct MergeService;

impl MergeService {
    pub fn new() -> Self {
        Self
    }

    pub fn check_conflicts(
        &self,
        repo_path: &Path,
        source_oid: &str,
        target_oid: &str,
    ) -> Result<Vec<String>> {
        let repo = git2::Repository::discover(repo_path)?;
        let source = repo.find_commit(git2::Oid::from_str(source_oid)?)?;
        let target = repo.find_commit(git2::Oid::from_str(target_oid)?)?;

        let merge_base_oid = repo.merge_base(source.id(), target.id())?;

        let mut merge_opts = git2::MergeOptions::new();
        merge_opts.file_favor(git2::FileFavor::Normal);

        let index = repo.merge_commits(&source, &target, Some(&merge_opts))?;

        if !index.has_conflicts() {
            return Ok(Vec::new());
        }

        let conflicts = index.conflicts()?;
        let mut paths = Vec::new();
        for conflict in conflicts {
            let conflict = conflict?;
            let path = conflict
                .our
                .as_ref()
                .or(conflict.their.as_ref())
                .or(conflict.ancestor.as_ref())
                .map(|e| String::from_utf8_lossy(&e.path).to_string());
            if let Some(p) = path {
                paths.push(p);
            }
        }
        Ok(paths)
    }

    pub fn get_conflict_content(
        &self,
        repo_path: &Path,
        source_oid: &str,
        target_oid: &str,
        file_path: &str,
    ) -> Result<ThreeWayContent> {
        let repo = git2::Repository::discover(repo_path)?;
        let source = repo.find_commit(git2::Oid::from_str(source_oid)?)?;
        let target = repo.find_commit(git2::Oid::from_str(target_oid)?)?;
        let merge_base_oid = repo.merge_base(source.id(), target.id())?;
        let ancestor = repo.find_commit(merge_base_oid)?;

        let base = read_file_from_tree(&repo, &ancestor.tree()?, file_path);
        let ours = read_file_from_tree(&repo, &target.tree()?, file_path)
            .unwrap_or_else(|| FileContent {
                data: Vec::new(),
                encoding: ContentEncoding::Utf8,
                is_lfs: false,
            });
        let theirs = read_file_from_tree(&repo, &source.tree()?, file_path)
            .unwrap_or_else(|| FileContent {
                data: Vec::new(),
                encoding: ContentEncoding::Utf8,
                is_lfs: false,
            });

        Ok(ThreeWayContent {
            base,
            ours,
            theirs,
        })
    }

    pub fn start_merge_session(
        &self,
        repo_path: &Path,
        source_oid: &str,
        target_oid: &str,
        target_branch: &str,
    ) -> Result<MergeSession> {
        let conflicts = self.check_conflicts(repo_path, source_oid, target_oid)?;
        Ok(MergeSession {
            source_oid: source_oid.to_string(),
            target_oid: target_oid.to_string(),
            target_branch: target_branch.to_string(),
            conflicted_paths: conflicts,
            resolutions: HashMap::new(),
        })
    }

    pub fn resolve_conflict(session: &mut MergeSession, path: &str, content: Vec<u8>) {
        session.resolutions.insert(path.to_string(), content);
    }

    pub fn reset_conflict(session: &mut MergeSession, path: &str) {
        session.resolutions.remove(path);
    }

    pub fn is_fully_resolved(session: &MergeSession) -> bool {
        session
            .conflicted_paths
            .iter()
            .all(|p| session.resolutions.contains_key(p))
    }

    pub fn merge(
        &self,
        repo_path: &Path,
        session: &MergeSession,
        strategy: MergeStrategy,
        message: &str,
        current_target_oid: &str,
    ) -> Result<String> {
        if current_target_oid != session.target_oid {
            return Err(PrError::TargetMoved);
        }

        if !Self::is_fully_resolved(session) {
            return Err(PrError::MergeConflicts);
        }

        let repo = git2::Repository::discover(repo_path)?;
        let source = repo.find_commit(git2::Oid::from_str(&session.source_oid)?)?;
        let target = repo.find_commit(git2::Oid::from_str(&session.target_oid)?)?;

        let mut merge_opts = git2::MergeOptions::new();
        let mut index = repo.merge_commits(&source, &target, Some(&mut merge_opts))?;

        for (path, content) in &session.resolutions {
            let blob_oid = repo.blob(content)?;
            let entry = git2::IndexEntry {
                ctime: git2::IndexTime::new(0, 0),
                mtime: git2::IndexTime::new(0, 0),
                dev: 0,
                ino: 0,
                mode: 0o100644,
                uid: 0,
                gid: 0,
                file_size: content.len() as u32,
                id: blob_oid,
                flags: 0,
                flags_extended: 0,
                path: path.as_bytes().to_vec(),
            };
            index.add(&entry)?;
        }

        let tree_oid = index.write_tree_to(&repo)?;
        let tree = repo.find_tree(tree_oid)?;
        let sig = repo.signature()?;

        let merge_oid = match strategy {
            MergeStrategy::MergeCommit => {
                repo.commit(None, &sig, &sig, message, &tree, &[&target, &source])?
            }
            MergeStrategy::Squash => repo.commit(None, &sig, &sig, message, &tree, &[&target])?,
        };

        let ref_name = format!("refs/heads/{}", session.target_branch);
        repo.reference(
            &ref_name,
            merge_oid,
            true,
            &format!("merge: {message}"),
        ).map_err(|e| PrError::Other(format!(
            "Failed to update branch ref '{}': {}",
            session.target_branch, e
        )))?;

        Ok(merge_oid.to_string())
    }
}

fn read_file_from_tree(
    repo: &git2::Repository,
    tree: &git2::Tree,
    path: &str,
) -> Option<FileContent> {
    let entry = tree.get_path(Path::new(path)).ok()?;
    let obj = entry.to_object(repo).ok()?;
    let blob = obj.as_blob()?;
    let data = blob.content().to_vec();
    let encoding = FileContent::detect_encoding(&data);
    let is_lfs = FileContent::is_lfs_pointer(&data);
    Some(FileContent {
        data,
        encoding,
        is_lfs,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn setup_conflict_repo() -> (tempfile::TempDir, PathBuf, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "t@t.com").unwrap();

        let sig = repo.signature().unwrap();

        std::fs::write(tmp.path().join("shared.txt"), "base content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("shared.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let base_oid = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();

        let base = repo.find_commit(base_oid).unwrap();
        repo.branch("feature", &base, false).unwrap();

        std::fs::write(tmp.path().join("shared.txt"), "main version").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("shared.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let main_oid = repo
            .commit(Some("HEAD"), &sig, &sig, "main change", &tree, &[&base])
            .unwrap();

        let obj = repo.revparse_single("refs/heads/feature").unwrap();
        repo.checkout_tree(&obj, None).unwrap();
        repo.set_head("refs/heads/feature").unwrap();

        std::fs::write(tmp.path().join("shared.txt"), "feature version").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("shared.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let feature_oid = repo
            .commit(
                Some("HEAD"),
                &sig,
                &sig,
                "feature change",
                &tree,
                &[&base],
            )
            .unwrap();

        let path = tmp.path().canonicalize().unwrap();
        (
            tmp,
            path,
            feature_oid.to_string(),
            main_oid.to_string(),
        )
    }

    #[test]
    fn detect_conflicts() {
        let (_tmp, path, source, target) = setup_conflict_repo();
        let svc = MergeService::new();
        let conflicts = svc.check_conflicts(&path, &source, &target).unwrap();
        assert!(conflicts.contains(&"shared.txt".to_string()));
    }

    #[test]
    fn get_three_way_content() {
        let (_tmp, path, source, target) = setup_conflict_repo();
        let svc = MergeService::new();
        let content = svc
            .get_conflict_content(&path, &source, &target, "shared.txt")
            .unwrap();
        assert!(content.base.is_some());
        assert!(content.ours.as_text().is_some());
        assert!(content.theirs.as_text().is_some());
    }

    #[test]
    fn merge_session_resolution_tracking() {
        let (_tmp, path, source, target) = setup_conflict_repo();
        let svc = MergeService::new();
        let mut session = svc.start_merge_session(&path, &source, &target, "master").unwrap();
        assert!(!MergeService::is_fully_resolved(&session));

        MergeService::resolve_conflict(
            &mut session,
            "shared.txt",
            b"resolved content".to_vec(),
        );
        assert!(MergeService::is_fully_resolved(&session));

        MergeService::reset_conflict(&mut session, "shared.txt");
        assert!(!MergeService::is_fully_resolved(&session));
    }

    #[test]
    fn merge_rejects_moved_target() {
        let (_tmp, path, source, target) = setup_conflict_repo();
        let svc = MergeService::new();
        let mut session = svc.start_merge_session(&path, &source, &target, "master").unwrap();
        MergeService::resolve_conflict(
            &mut session,
            "shared.txt",
            b"resolved".to_vec(),
        );

        let result = svc.merge(
            &path,
            &session,
            MergeStrategy::MergeCommit,
            "merge",
            "wrong-oid",
        );
        assert!(matches!(result, Err(PrError::TargetMoved)));
    }
}
