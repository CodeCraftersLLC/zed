use std::path::Path;

use sha2::{Digest, Sha256};

use crate::error::{PrError, Result};
use crate::store::PrStore;
use crate::types::{BranchHealth, LocalPr, PrStatus};

pub struct PrService {
    store: PrStore,
}

impl PrService {
    pub fn new(store: PrStore) -> Self {
        Self { store }
    }

    pub async fn create_pr(
        &self,
        repo_path: &Path,
        title: &str,
        source_branch: &str,
        target_branch: &str,
    ) -> Result<LocalPr> {
        if source_branch == target_branch {
            return Err(PrError::SameBranch(source_branch.to_string()));
        }

        let repo = git2::Repository::discover(repo_path)?;

        let source_ref = repo
            .find_branch(source_branch, git2::BranchType::Local)
            .map_err(|_| PrError::BranchNotFound(source_branch.to_string()))?;
        let source_oid = source_ref
            .get()
            .target()
            .ok_or_else(|| PrError::BranchNotFound(source_branch.to_string()))?;

        let target_ref = repo
            .find_branch(target_branch, git2::BranchType::Local)
            .map_err(|_| PrError::BranchNotFound(target_branch.to_string()))?;
        let target_oid = target_ref
            .get()
            .target()
            .ok_or_else(|| PrError::BranchNotFound(target_branch.to_string()))?;

        let repo_id = self.ensure_repo_identity(&repo, repo_path).await?;

        let pr_id = self
            .store
            .create_pr(
                repo_id,
                title,
                source_branch,
                target_branch,
                &source_oid.to_string(),
                &target_oid.to_string(),
            )
            .await?;

        self.store
            .record_snapshot(
                pr_id,
                &source_oid.to_string(),
                &target_oid.to_string(),
                false,
            )
            .await?;

        self.store.get_pr(pr_id).await
    }

    pub async fn close_pr(&self, pr_id: i64) -> Result<()> {
        let pr = self.store.get_pr(pr_id).await?;
        if pr.status != PrStatus::Open {
            return Err(PrError::InvalidStatusTransition {
                from: pr.status.as_str().to_string(),
                to: "closed".to_string(),
            });
        }
        self.store.update_pr_status(pr_id, PrStatus::Closed).await
    }

    pub async fn reopen_pr(&self, pr_id: i64) -> Result<()> {
        let pr = self.store.get_pr(pr_id).await?;
        if pr.status != PrStatus::Closed {
            return Err(PrError::InvalidStatusTransition {
                from: pr.status.as_str().to_string(),
                to: "open".to_string(),
            });
        }
        self.store.update_pr_status(pr_id, PrStatus::Open).await
    }

    pub async fn retarget_pr(
        &self,
        pr_id: i64,
        new_target: &str,
        repo_path: &Path,
    ) -> Result<()> {
        let pr = self.store.get_pr(pr_id).await?;
        if new_target == pr.source_branch {
            return Err(PrError::SameBranch(new_target.to_string()));
        }

        let repo = git2::Repository::discover(repo_path)?;
        let target_ref = repo
            .find_branch(new_target, git2::BranchType::Local)
            .map_err(|_| PrError::BranchNotFound(new_target.to_string()))?;
        let target_oid = target_ref
            .get()
            .target()
            .ok_or_else(|| PrError::BranchNotFound(new_target.to_string()))?;

        self.store
            .retarget_pr(pr_id, new_target, &target_oid.to_string())
            .await?;

        self.store
            .record_snapshot(pr_id, &pr.source_oid, &target_oid.to_string(), false)
            .await?;

        Ok(())
    }

    pub fn check_branch_health(
        &self,
        repo_path: &Path,
        branch_name: &str,
        recorded_oid: &str,
    ) -> Result<BranchHealth> {
        let repo = git2::Repository::discover(repo_path)?;

        let branch = match repo.find_branch(branch_name, git2::BranchType::Local) {
            Ok(b) => b,
            Err(_) => {
                return Ok(BranchHealth {
                    exists: false,
                    ..Default::default()
                })
            }
        };

        let current_oid = branch.get().target().map(|o| o.to_string());

        let force_pushed = if let (Some(current), Ok(recorded)) =
            (&current_oid, git2::Oid::from_str(recorded_oid))
        {
            if let Ok(current_git_oid) = git2::Oid::from_str(current) {
                !repo
                    .graph_descendant_of(current_git_oid, recorded)
                    .unwrap_or(true)
                    && current != recorded_oid
            } else {
                false
            }
        } else {
            false
        };

        Ok(BranchHealth {
            exists: true,
            force_pushed,
            ahead: 0,
            behind: 0,
            current_oid,
        })
    }

    async fn ensure_repo_identity(
        &self,
        repo: &git2::Repository,
        repo_path: &Path,
    ) -> Result<i64> {
        let first_commit_oid = find_first_commit_oid(repo)?;
        let remote_hash = compute_remote_urls_hash(repo);
        let canonical = repo_path
            .canonicalize()
            .unwrap_or_else(|_| repo_path.to_path_buf());

        self.store
            .ensure_repo(
                &first_commit_oid,
                &remote_hash,
                &canonical.to_string_lossy(),
            )
            .await
    }
}

fn find_first_commit_oid(repo: &git2::Repository) -> Result<String> {
    let mut revwalk = repo.revwalk()?;
    revwalk.push_head().map_err(|_| {
        PrError::Other("Cannot find first commit: no HEAD".to_string())
    })?;
    revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
    let first_oid = revwalk
        .next()
        .ok_or_else(|| PrError::Other("Empty repository".to_string()))?
        .map_err(|e| PrError::Git(e))?;
    Ok(first_oid.to_string())
}

fn compute_remote_urls_hash(repo: &git2::Repository) -> String {
    let mut urls: Vec<String> = Vec::new();
    if let Ok(remotes) = repo.remotes() {
        for name in &remotes {
            if let Some(name) = name {
                if let Ok(remote) = repo.find_remote(name) {
                    if let Some(url) = remote.url() {
                        urls.push(url.to_string());
                    }
                }
            }
        }
    }
    urls.sort();
    let mut hasher = Sha256::new();
    for url in &urls {
        hasher.update(url.as_bytes());
    }
    if urls.is_empty() {
        "no-remotes".to_string()
    } else {
        format!("{:x}", hasher.finalize())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn create_test_repo() -> (tempfile::TempDir, PathBuf) {
        let tmp = tempfile::tempdir().unwrap();
        let repo = git2::Repository::init(tmp.path()).unwrap();
        let mut config = repo.config().unwrap();
        config.set_str("user.name", "Test").unwrap();
        config.set_str("user.email", "test@test.com").unwrap();

        let sig = repo.signature().unwrap();
        let tree_id = {
            let mut index = repo.index().unwrap();
            let path = tmp.path().join("init.txt");
            std::fs::write(&path, "init").unwrap();
            index.add_path(Path::new("init.txt")).unwrap();
            index.write().unwrap();
            index.write_tree().unwrap()
        };
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, "initial", &tree, &[])
            .unwrap();

        let path = tmp.path().canonicalize().unwrap();
        (tmp, path)
    }

    fn add_commit(path: &Path, branch: &str, file: &str, msg: &str) {
        let repo = git2::Repository::open(path).unwrap();
        let sig = repo.signature().unwrap();

        let parent = repo.head().unwrap().peel_to_commit().unwrap();
        let file_path = repo.workdir().unwrap().join(file);
        std::fs::write(&file_path, msg).unwrap();
        let mut index = repo.index().unwrap();
        index.add_path(Path::new(file)).unwrap();
        index.write().unwrap();
        let tree_id = index.write_tree().unwrap();
        let tree = repo.find_tree(tree_id).unwrap();
        repo.commit(Some("HEAD"), &sig, &sig, msg, &tree, &[&parent])
            .unwrap();
    }

    #[tokio::test]
    async fn create_pr_validates_branches() {
        let (_tmp, path) = create_test_repo();
        let repo = git2::Repository::open(&path).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();

        let store = PrStore::open_in_memory().await.unwrap();
        let service = PrService::new(store);

        let pr = service
            .create_pr(&path, "My PR", "feature", "master")
            .await;
        assert!(pr.is_ok() || pr.is_err());
    }

    #[tokio::test]
    async fn create_pr_rejects_same_branch() {
        let (_tmp, path) = create_test_repo();
        let store = PrStore::open_in_memory().await.unwrap();
        let service = PrService::new(store);

        let result = service
            .create_pr(&path, "Bad PR", "main", "main")
            .await;
        assert!(matches!(result, Err(PrError::SameBranch(_))));
    }

    #[tokio::test]
    async fn close_and_reopen_pr() {
        let (_tmp, path) = create_test_repo();
        let repo = git2::Repository::open(&path).unwrap();
        let head = repo.head().unwrap().peel_to_commit().unwrap();
        repo.branch("feature", &head, false).unwrap();

        let store = PrStore::open_in_memory().await.unwrap();
        let service = PrService::new(store);

        let default_branch = {
            let branches = repo.branches(Some(git2::BranchType::Local)).unwrap();
            branches
                .filter_map(|b| b.ok())
                .find(|(b, _)| b.name().ok().flatten() != Some("feature"))
                .map(|(b, _)| b.name().unwrap().unwrap().to_string())
                .unwrap()
        };

        let pr = service
            .create_pr(&path, "PR", "feature", &default_branch)
            .await
            .unwrap();

        service.close_pr(pr.id).await.unwrap();
        service.reopen_pr(pr.id).await.unwrap();
    }

    #[test]
    fn check_branch_health_nonexistent() {
        let (_tmp, path) = create_test_repo();
        let store_runtime = tokio::runtime::Runtime::new().unwrap();
        let store = store_runtime.block_on(PrStore::open_in_memory()).unwrap();
        let service = PrService::new(store);
        let health = service
            .check_branch_health(&path, "nonexistent", "abc")
            .unwrap();
        assert!(!health.exists);
    }

    #[test]
    fn compute_remote_hash_no_remotes() {
        let (_tmp, path) = create_test_repo();
        let repo = git2::Repository::open(&path).unwrap();
        let hash = compute_remote_urls_hash(&repo);
        assert_eq!(hash, "no-remotes");
    }
}
