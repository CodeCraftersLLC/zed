use chrono::Utc;
use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::{PrError, Result};
use crate::types::{LocalPr, MergeStrategy, PrSnapshot, PrStatus};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS repos (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    first_commit_oid TEXT NOT NULL,
    remote_urls_hash TEXT NOT NULL,
    canonical_path TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    UNIQUE(first_commit_oid, remote_urls_hash)
);

CREATE TABLE IF NOT EXISTS prs (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_id INTEGER NOT NULL REFERENCES repos(id),
    title TEXT NOT NULL,
    source_branch TEXT NOT NULL,
    target_branch TEXT NOT NULL,
    source_oid TEXT NOT NULL,
    target_oid TEXT NOT NULL,
    status TEXT NOT NULL DEFAULT 'open',
    merge_strategy TEXT,
    merged_oid TEXT,
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS pr_snapshots (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    pr_id INTEGER NOT NULL REFERENCES prs(id) ON DELETE CASCADE,
    source_oid TEXT NOT NULL,
    target_oid TEXT NOT NULL,
    is_force_push INTEGER NOT NULL DEFAULT 0,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_prs_repo_id ON prs(repo_id);
CREATE INDEX IF NOT EXISTS idx_prs_status ON prs(status);
CREATE INDEX IF NOT EXISTS idx_snapshots_pr_id ON pr_snapshots(pr_id);
"#;

pub struct PrStore {
    conn: Connection,
}

impl PrStore {
    pub async fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path).await?;
        conn.call(|conn| {
            conn.execute_batch(
                "PRAGMA journal_mode=WAL;
                 PRAGMA foreign_keys=ON;
                 PRAGMA busy_timeout=5000;",
            )?;
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await?;
        Ok(Self { conn })
    }

    pub async fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory().await?;
        conn.call(|conn| {
            conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await?;
        Ok(Self { conn })
    }

    pub async fn ensure_repo(
        &self,
        first_commit_oid: &str,
        remote_urls_hash: &str,
        canonical_path: &str,
    ) -> Result<i64> {
        let fco = first_commit_oid.to_string();
        let ruh = remote_urls_hash.to_string();
        let cp = canonical_path.to_string();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT OR IGNORE INTO repos (first_commit_oid, remote_urls_hash, canonical_path)
                     VALUES (?1, ?2, ?3)",
                    params![fco, ruh, cp],
                )?;
                let id: i64 = conn.query_row(
                    "SELECT id FROM repos WHERE first_commit_oid = ?1 AND remote_urls_hash = ?2",
                    params![fco, ruh],
                    |row| row.get(0),
                )?;
                Ok(id)
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn create_pr(
        &self,
        repo_id: i64,
        title: &str,
        source_branch: &str,
        target_branch: &str,
        source_oid: &str,
        target_oid: &str,
    ) -> Result<i64> {
        let title = title.to_string();
        let source = source_branch.to_string();
        let target = target_branch.to_string();
        let s_oid = source_oid.to_string();
        let t_oid = target_oid.to_string();
        let now = Utc::now().to_rfc3339();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO prs (repo_id, title, source_branch, target_branch, source_oid, target_oid, created_at, updated_at)
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?7)",
                    params![repo_id, title, source, target, s_oid, t_oid, now],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn get_pr(&self, pr_id: i64) -> Result<LocalPr> {
        self.conn
            .call(move |conn| {
                let pr = conn.query_row(
                    "SELECT id, repo_id, title, source_branch, target_branch, source_oid, target_oid,
                            status, merge_strategy, merged_oid, created_at, updated_at
                     FROM prs WHERE id = ?1",
                    params![pr_id],
                    |row| {
                        Ok(LocalPr {
                            id: row.get(0)?,
                            repo_id: row.get(1)?,
                            title: row.get(2)?,
                            source_branch: row.get(3)?,
                            target_branch: row.get(4)?,
                            source_oid: row.get(5)?,
                            target_oid: row.get(6)?,
                            status: PrStatus::from_str(&row.get::<_, String>(7)?)
                                .unwrap_or(PrStatus::Open),
                            merge_strategy: row
                                .get::<_, Option<String>>(8)?
                                .and_then(|s| MergeStrategy::from_str(&s)),
                            merged_oid: row.get(9)?,
                            created_at: row
                                .get::<_, String>(10)?
                                .parse()
                                .unwrap_or_else(|_| Utc::now()),
                            updated_at: row
                                .get::<_, String>(11)?
                                .parse()
                                .unwrap_or_else(|_| Utc::now()),
                        })
                    },
                )?;
                Ok(pr)
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn list_prs(&self, repo_id: i64, status: Option<PrStatus>) -> Result<Vec<LocalPr>> {
        let status_str = status.map(|s| s.as_str().to_string());

        self.conn
            .call(move |conn| {
                let mut query = String::from(
                    "SELECT id, repo_id, title, source_branch, target_branch, source_oid, target_oid,
                            status, merge_strategy, merged_oid, created_at, updated_at
                     FROM prs WHERE repo_id = ?1",
                );
                if status_str.is_some() {
                    query.push_str(" AND status = ?2");
                }
                query.push_str(" ORDER BY updated_at DESC");

                let mut stmt = conn.prepare(&query)?;

                let rows = if let Some(ref s) = status_str {
                    stmt.query_map(params![repo_id, s], map_pr_row)?
                } else {
                    stmt.query_map(params![repo_id], map_pr_row)?
                };

                let mut prs = Vec::new();
                for row in rows {
                    prs.push(row?);
                }
                Ok(prs)
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn update_pr_status(&self, pr_id: i64, status: PrStatus) -> Result<()> {
        let status_str = status.as_str().to_string();
        let now = Utc::now().to_rfc3339();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE prs SET status = ?1, updated_at = ?2 WHERE id = ?3",
                    params![status_str, now, pr_id],
                )?;
                Ok(())
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn retarget_pr(
        &self,
        pr_id: i64,
        new_target_branch: &str,
        new_target_oid: &str,
    ) -> Result<()> {
        let branch = new_target_branch.to_string();
        let oid = new_target_oid.to_string();
        let now = Utc::now().to_rfc3339();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE prs SET target_branch = ?1, target_oid = ?2, updated_at = ?3 WHERE id = ?4",
                    params![branch, oid, now, pr_id],
                )?;
                Ok(())
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn update_branch_tips(
        &self,
        pr_id: i64,
        source_oid: &str,
        target_oid: &str,
    ) -> Result<()> {
        let s_oid = source_oid.to_string();
        let t_oid = target_oid.to_string();
        let now = Utc::now().to_rfc3339();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "UPDATE prs SET source_oid = ?1, target_oid = ?2, updated_at = ?3 WHERE id = ?4",
                    params![s_oid, t_oid, now, pr_id],
                )?;
                Ok(())
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn record_snapshot(
        &self,
        pr_id: i64,
        source_oid: &str,
        target_oid: &str,
        is_force_push: bool,
    ) -> Result<i64> {
        let s_oid = source_oid.to_string();
        let t_oid = target_oid.to_string();
        let now = Utc::now().to_rfc3339();

        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO pr_snapshots (pr_id, source_oid, target_oid, is_force_push, created_at)
                     VALUES (?1, ?2, ?3, ?4, ?5)",
                    params![pr_id, s_oid, t_oid, is_force_push as i32, now],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn list_snapshots(&self, pr_id: i64) -> Result<Vec<PrSnapshot>> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, pr_id, source_oid, target_oid, is_force_push, created_at
                     FROM pr_snapshots WHERE pr_id = ?1 ORDER BY created_at DESC",
                )?;
                let rows = stmt.query_map(params![pr_id], |row| {
                    Ok(PrSnapshot {
                        id: row.get(0)?,
                        pr_id: row.get(1)?,
                        source_oid: row.get(2)?,
                        target_oid: row.get(3)?,
                        is_force_push: row.get::<_, i32>(4)? != 0,
                        created_at: row
                            .get::<_, String>(5)?
                            .parse()
                            .unwrap_or_else(|_| Utc::now()),
                    })
                })?;
                let mut snapshots = Vec::new();
                for row in rows {
                    snapshots.push(row?);
                }
                Ok(snapshots)
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }

    pub async fn delete_pr(&self, pr_id: i64) -> Result<()> {
        self.conn
            .call(move |conn| {
                conn.execute("DELETE FROM prs WHERE id = ?1", params![pr_id])?;
                Ok(())
            })
            .await
            .map_err(PrError::AsyncDatabase)
    }
}

fn map_pr_row(row: &rusqlite::Row) -> rusqlite::Result<LocalPr> {
    Ok(LocalPr {
        id: row.get(0)?,
        repo_id: row.get(1)?,
        title: row.get(2)?,
        source_branch: row.get(3)?,
        target_branch: row.get(4)?,
        source_oid: row.get(5)?,
        target_oid: row.get(6)?,
        status: PrStatus::from_str(&row.get::<_, String>(7)?).unwrap_or(PrStatus::Open),
        merge_strategy: row
            .get::<_, Option<String>>(8)?
            .and_then(|s| MergeStrategy::from_str(&s)),
        merged_oid: row.get(9)?,
        created_at: row
            .get::<_, String>(10)?
            .parse()
            .unwrap_or_else(|_| Utc::now()),
        updated_at: row
            .get::<_, String>(11)?
            .parse()
            .unwrap_or_else(|_| Utc::now()),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_retrieve_pr() {
        let store = PrStore::open_in_memory().await.unwrap();
        let repo_id = store.ensure_repo("abc123", "hash1", "/tmp/repo").await.unwrap();
        let pr_id = store
            .create_pr(repo_id, "Test PR", "feature", "main", "oid1", "oid2")
            .await
            .unwrap();
        let pr = store.get_pr(pr_id).await.unwrap();
        assert_eq!(pr.title, "Test PR");
        assert_eq!(pr.source_branch, "feature");
        assert_eq!(pr.target_branch, "main");
        assert_eq!(pr.status, PrStatus::Open);
    }

    #[tokio::test]
    async fn list_prs_by_status() {
        let store = PrStore::open_in_memory().await.unwrap();
        let repo_id = store.ensure_repo("abc", "hash", "/tmp").await.unwrap();
        store.create_pr(repo_id, "PR1", "a", "main", "o1", "o2").await.unwrap();
        let pr2_id = store.create_pr(repo_id, "PR2", "b", "main", "o3", "o4").await.unwrap();
        store.update_pr_status(pr2_id, PrStatus::Closed).await.unwrap();

        let open = store.list_prs(repo_id, Some(PrStatus::Open)).await.unwrap();
        assert_eq!(open.len(), 1);
        assert_eq!(open[0].title, "PR1");

        let all = store.list_prs(repo_id, None).await.unwrap();
        assert_eq!(all.len(), 2);
    }

    #[tokio::test]
    async fn snapshot_recording() {
        let store = PrStore::open_in_memory().await.unwrap();
        let repo_id = store.ensure_repo("abc", "hash", "/tmp").await.unwrap();
        let pr_id = store.create_pr(repo_id, "PR", "f", "m", "o1", "o2").await.unwrap();
        store.record_snapshot(pr_id, "o1", "o2", false).await.unwrap();
        store.record_snapshot(pr_id, "o3", "o2", true).await.unwrap();

        let snapshots = store.list_snapshots(pr_id).await.unwrap();
        assert_eq!(snapshots.len(), 2);
        assert!(snapshots[0].is_force_push);
    }

    #[tokio::test]
    async fn delete_pr_cascades_snapshots() {
        let store = PrStore::open_in_memory().await.unwrap();
        let repo_id = store.ensure_repo("abc", "hash", "/tmp").await.unwrap();
        let pr_id = store.create_pr(repo_id, "PR", "f", "m", "o1", "o2").await.unwrap();
        store.record_snapshot(pr_id, "o1", "o2", false).await.unwrap();
        store.delete_pr(pr_id).await.unwrap();

        let snapshots = store.list_snapshots(pr_id).await.unwrap();
        assert!(snapshots.is_empty());
    }

    #[tokio::test]
    async fn ensure_repo_is_idempotent() {
        let store = PrStore::open_in_memory().await.unwrap();
        let id1 = store.ensure_repo("abc", "hash", "/tmp/a").await.unwrap();
        let id2 = store.ensure_repo("abc", "hash", "/tmp/b").await.unwrap();
        assert_eq!(id1, id2);
    }

    #[tokio::test]
    async fn update_branch_tips() {
        let store = PrStore::open_in_memory().await.unwrap();
        let repo_id = store.ensure_repo("abc", "hash", "/tmp").await.unwrap();
        let pr_id = store.create_pr(repo_id, "PR", "f", "m", "old1", "old2").await.unwrap();
        store.update_branch_tips(pr_id, "new1", "new2").await.unwrap();
        let pr = store.get_pr(pr_id).await.unwrap();
        assert_eq!(pr.source_oid, "new1");
        assert_eq!(pr.target_oid, "new2");
    }
}
