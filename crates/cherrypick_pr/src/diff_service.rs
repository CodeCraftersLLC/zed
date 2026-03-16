use std::num::NonZeroUsize;
use std::path::Path;

use lru::LruCache;

use crate::error::Result;
use crate::types::FileContent;

#[derive(Debug, Clone)]
pub struct DiffFileEntry {
    pub path: String,
    pub status: char,
    pub insertions: usize,
    pub deletions: usize,
    pub is_binary: bool,
    pub is_lfs: bool,
    pub old_path: Option<String>,
}

#[derive(Debug, Clone)]
pub struct BranchDiff {
    pub source_oid: String,
    pub target_oid: String,
    pub merge_base_oid: String,
    pub files: Vec<DiffFileEntry>,
    pub total_insertions: usize,
    pub total_deletions: usize,
}

pub struct DiffService {
    cache: LruCache<(String, String), BranchDiff>,
}

impl DiffService {
    pub fn new(cache_size: usize) -> Self {
        Self {
            cache: LruCache::new(NonZeroUsize::new(cache_size.max(1)).unwrap()),
        }
    }

    pub fn get_branch_diff(
        &mut self,
        repo_path: &Path,
        source_oid: &str,
        target_oid: &str,
    ) -> Result<BranchDiff> {
        let key = (source_oid.to_string(), target_oid.to_string());
        if let Some(cached) = self.cache.get(&key) {
            return Ok(cached.clone());
        }

        let diff = self.compute_diff(repo_path, source_oid, target_oid)?;
        self.cache.put(key, diff.clone());
        Ok(diff)
    }

    pub fn invalidate(&mut self, source_oid: &str, target_oid: &str) {
        let key = (source_oid.to_string(), target_oid.to_string());
        self.cache.pop(&key);
    }

    pub fn invalidate_all(&mut self) {
        self.cache.clear();
    }

    fn compute_diff(
        &self,
        repo_path: &Path,
        source_oid_str: &str,
        target_oid_str: &str,
    ) -> Result<BranchDiff> {
        let repo = git2::Repository::discover(repo_path)?;
        let source_oid = git2::Oid::from_str(source_oid_str)?;
        let target_oid = git2::Oid::from_str(target_oid_str)?;

        let merge_base = repo.merge_base(source_oid, target_oid)?;

        let base_tree = repo.find_commit(merge_base)?.tree()?;
        let source_tree = repo.find_commit(source_oid)?.tree()?;

        let mut diff_opts = git2::DiffOptions::new();
        diff_opts.patience(true);

        let diff = repo.diff_tree_to_tree(
            Some(&base_tree),
            Some(&source_tree),
            Some(&mut diff_opts),
        )?;

        let stats = diff.stats()?;
        let mut files = Vec::new();

        for delta_idx in 0..diff.deltas().len() {
            let delta = diff.get_delta(delta_idx).unwrap();
            let new_file = delta.new_file();
            let old_file = delta.old_file();

            let path = new_file
                .path()
                .or_else(|| old_file.path())
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();

            let old_path = if delta.status() == git2::Delta::Renamed {
                old_file.path().map(|p| p.to_string_lossy().to_string())
            } else {
                None
            };

            let status = match delta.status() {
                git2::Delta::Added => 'A',
                git2::Delta::Deleted => 'D',
                git2::Delta::Modified => 'M',
                git2::Delta::Renamed => 'R',
                git2::Delta::Copied => 'C',
                git2::Delta::Typechange => 'T',
                _ => '?',
            };

            let is_binary = delta.flags().is_binary();

            let is_lfs = if !is_binary {
                if let Some(oid) = new_file.id().as_bytes().first() {
                    if *oid != 0 {
                        let blob = repo.find_blob(new_file.id());
                        blob.map(|b| FileContent::is_lfs_pointer(b.content()))
                            .unwrap_or(false)
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            files.push(DiffFileEntry {
                path,
                status,
                insertions: 0,
                deletions: 0,
                is_binary,
                is_lfs,
                old_path,
            });
        }

        Ok(BranchDiff {
            source_oid: source_oid_str.to_string(),
            target_oid: target_oid_str.to_string(),
            merge_base_oid: merge_base.to_string(),
            files,
            total_insertions: stats.insertions(),
            total_deletions: stats.deletions(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn create_test_repo_with_branches() -> (tempfile::TempDir, PathBuf, String, String) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "t@t.com").unwrap();

        let sig = repo.signature().unwrap();

        std::fs::write(tmp.path().join("base.txt"), "base content").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("base.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let base_commit = repo
            .commit(Some("HEAD"), &sig, &sig, "base", &tree, &[])
            .unwrap();

        let base = repo.find_commit(base_commit).unwrap();
        repo.branch("feature", &base, false).unwrap();

        let refname = "refs/heads/feature";
        let obj = repo.revparse_single(refname).unwrap();
        repo.checkout_tree(&obj, None).unwrap();
        repo.set_head(refname).unwrap();

        std::fs::write(tmp.path().join("feature.txt"), "feature work").unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new("feature.txt")).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let feature_oid = repo
            .commit(Some("HEAD"), &sig, &sig, "feature commit", &tree, &[&parent])
            .unwrap();

        let path = tmp.path().canonicalize().unwrap();
        (
            tmp,
            path,
            feature_oid.to_string(),
            base_commit.to_string(),
        )
    }

    #[test]
    fn compute_diff_finds_added_file() {
        let (_tmp, path, source_oid, target_oid) = create_test_repo_with_branches();
        let mut svc = DiffService::new(10);
        let diff = svc.get_branch_diff(&path, &source_oid, &target_oid).unwrap();
        assert!(!diff.files.is_empty());
        assert!(diff.files.iter().any(|f| f.path == "feature.txt"));
    }

    #[test]
    fn cache_hit_returns_same_result() {
        let (_tmp, path, source_oid, target_oid) = create_test_repo_with_branches();
        let mut svc = DiffService::new(10);
        let d1 = svc.get_branch_diff(&path, &source_oid, &target_oid).unwrap();
        let d2 = svc.get_branch_diff(&path, &source_oid, &target_oid).unwrap();
        assert_eq!(d1.files.len(), d2.files.len());
    }

    #[test]
    fn invalidate_clears_cache() {
        let (_tmp, path, source_oid, target_oid) = create_test_repo_with_branches();
        let mut svc = DiffService::new(10);
        svc.get_branch_diff(&path, &source_oid, &target_oid).unwrap();
        svc.invalidate(&source_oid, &target_oid);
    }
}
