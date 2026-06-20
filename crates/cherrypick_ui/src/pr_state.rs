use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use cherrypick_pr::diff_service::{BranchDiff, DiffService};
use cherrypick_pr::{LocalPr, PrStatus, PrStore};
use gpui::{App, BackgroundExecutor, Task};

pub struct PrState {
    store: Arc<tokio::sync::Mutex<Option<PrStore>>>,
    diff_service: Arc<Mutex<DiffService>>,
    repo_path: Option<PathBuf>,
    repo_id: Arc<tokio::sync::Mutex<Option<i64>>>,
    executor: BackgroundExecutor,
}

impl PrState {
    pub fn new(cx: &App) -> Self {
        Self {
            store: Arc::new(tokio::sync::Mutex::new(None)),
            diff_service: Arc::new(Mutex::new(DiffService::new(32))),
            repo_path: None,
            repo_id: Arc::new(tokio::sync::Mutex::new(None)),
            executor: cx.background_executor().clone(),
        }
    }

    pub fn set_repo_path(&mut self, path: PathBuf) {
        self.repo_path = Some(path);
    }

    pub fn repo_path(&self) -> Option<&Path> {
        self.repo_path.as_deref()
    }

    pub fn is_initialized(&self) -> bool {
        self.repo_path.is_some()
    }

    pub fn initialize(&self) -> Task<anyhow::Result<()>> {
        let Some(repo_path) = self.repo_path.clone() else {
            return Task::ready(Err(anyhow::anyhow!("no repo path set")));
        };

        let store_lock = self.store.clone();
        let repo_id_lock = self.repo_id.clone();

        let git_info = std::thread::spawn(move || -> anyhow::Result<(String, String, String, String)> {
            let db_dir = repo_path.join(".cherrypick");
            std::fs::create_dir_all(&db_dir)?;
            let db_path = db_dir.join("prs.db").to_string_lossy().to_string();

            let repo = git2::Repository::discover(&repo_path)?;
            let mut revwalk = repo.revwalk()?;
            revwalk.push_head()?;
            revwalk.set_sorting(git2::Sort::TOPOLOGICAL | git2::Sort::REVERSE)?;
            let first_oid = revwalk
                .next()
                .ok_or_else(|| anyhow::anyhow!("no commits"))??
                .to_string();

            use sha2::Digest;
            let hash = format!("{:x}", sha2::Sha256::digest(repo_path.to_string_lossy().as_bytes()));
            let canonical = repo_path.to_string_lossy().to_string();

            Ok((db_path, first_oid, hash, canonical))
        });

        self.executor.spawn(async move {
            let (db_path, first_oid, hash, canonical) = git_info
                .join()
                .map_err(|_| anyhow::anyhow!("git info thread panicked"))??;

            let store = PrStore::open(&db_path)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            let id = store
                .ensure_repo(&first_oid, &hash, &canonical)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            *repo_id_lock.lock().await = Some(id);
            *store_lock.lock().await = Some(store);
            Ok(())
        })
    }

    pub fn list_open_prs(&self) -> Task<anyhow::Result<Vec<LocalPr>>> {
        let store_lock = self.store.clone();
        let repo_id_lock = self.repo_id.clone();

        self.executor.spawn(async move {
            let repo_id = repo_id_lock
                .lock()
                .await
                .ok_or_else(|| anyhow::anyhow!("repo not registered"))?;

            let guard = store_lock.lock().await;
            let store = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("PR store not initialized"))?;

            store
                .list_prs(repo_id, Some(PrStatus::Open))
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
    }

    pub fn create_pr(
        &self,
        title: String,
        source_branch: String,
        target_branch: String,
    ) -> Task<anyhow::Result<LocalPr>> {
        let Some(repo_path) = self.repo_path.clone() else {
            return Task::ready(Err(anyhow::anyhow!("no repo path")));
        };
        let store_lock = self.store.clone();
        let repo_id_lock = self.repo_id.clone();

        let sb = source_branch.clone();
        let tb = target_branch.clone();
        let git_info = std::thread::spawn(move || -> anyhow::Result<(String, String)> {
            let repo = git2::Repository::discover(&repo_path)?;
            let source_ref = repo
                .find_branch(&sb, git2::BranchType::Local)
                .map_err(|_| anyhow::anyhow!("branch '{}' not found", sb))?;
            let source_oid = source_ref
                .get()
                .target()
                .ok_or_else(|| anyhow::anyhow!("branch has no target"))?
                .to_string();

            let target_ref = repo
                .find_branch(&tb, git2::BranchType::Local)
                .map_err(|_| anyhow::anyhow!("branch '{}' not found", tb))?;
            let target_oid = target_ref
                .get()
                .target()
                .ok_or_else(|| anyhow::anyhow!("branch has no target"))?
                .to_string();

            Ok((source_oid, target_oid))
        });

        self.executor.spawn(async move {
            let (source_oid, target_oid) = git_info
                .join()
                .map_err(|_| anyhow::anyhow!("git thread panicked"))??;

            let repo_id = repo_id_lock
                .lock()
                .await
                .ok_or_else(|| anyhow::anyhow!("repo not registered"))?;

            let guard = store_lock.lock().await;
            let store = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("PR store not initialized"))?;

            let pr_id = store
                .create_pr(
                    repo_id,
                    &title,
                    &source_branch,
                    &target_branch,
                    &source_oid,
                    &target_oid,
                )
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))?;

            store.get_pr(pr_id).await.map_err(|e| anyhow::anyhow!("{e}"))
        })
    }

    pub fn get_branch_diff(
        &self,
        source_oid: String,
        target_oid: String,
    ) -> Task<anyhow::Result<BranchDiff>> {
        let Some(repo_path) = self.repo_path.clone() else {
            return Task::ready(Err(anyhow::anyhow!("no repo path")));
        };
        let diff_service = self.diff_service.clone();

        self.executor.spawn(async move {
            let mut ds = diff_service.lock().unwrap();
            ds.get_branch_diff(&repo_path, &source_oid, &target_oid)
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
    }

    pub fn update_pr_status(
        &self,
        pr_id: i64,
        status: PrStatus,
    ) -> Task<anyhow::Result<()>> {
        let store_lock = self.store.clone();

        self.executor.spawn(async move {
            let guard = store_lock.lock().await;
            let store = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("PR store not initialized"))?;
            store
                .update_pr_status(pr_id, status)
                .await
                .map_err(|e| anyhow::anyhow!("{e}"))
        })
    }

    pub fn get_unified_diff(
        &self,
        source_oid: String,
        target_oid: String,
    ) -> Task<anyhow::Result<String>> {
        let Some(repo_path) = self.repo_path.clone() else {
            return Task::ready(Err(anyhow::anyhow!("no repo path")));
        };

        self.executor.spawn(async move {
            let repo = git2::Repository::discover(&repo_path)?;
            let source = git2::Oid::from_str(&source_oid)?;
            let target = git2::Oid::from_str(&target_oid)?;

            let merge_base = repo.merge_base(source, target)?;
            let base_tree = repo.find_commit(merge_base)?.tree()?;
            let source_tree = repo.find_commit(source)?.tree()?;

            let mut diff_opts = git2::DiffOptions::new();
            diff_opts.patience(true).context_lines(3);

            let diff = repo.diff_tree_to_tree(
                Some(&base_tree),
                Some(&source_tree),
                Some(&mut diff_opts),
            )?;

            let mut output = String::new();
            diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
                let prefix = match line.origin() {
                    '+' => "+",
                    '-' => "-",
                    ' ' => " ",
                    'H' | 'F' => "",
                    _ => "",
                };
                let content = std::str::from_utf8(line.content()).unwrap_or("");
                if line.origin() == 'H' || line.origin() == 'F' {
                    output.push_str(content);
                } else {
                    output.push_str(prefix);
                    output.push_str(content);
                }
                true
            })?;

            Ok(output)
        })
    }

    pub fn get_pr(&self, pr_id: i64) -> Task<anyhow::Result<LocalPr>> {
        let store_lock = self.store.clone();

        self.executor.spawn(async move {
            let guard = store_lock.lock().await;
            let store = guard
                .as_ref()
                .ok_or_else(|| anyhow::anyhow!("PR store not initialized"))?;
            store.get_pr(pr_id).await.map_err(|e| anyhow::anyhow!("{e}"))
        })
    }
}
