use chrono::Utc;
use rusqlite::params;
use tokio_rusqlite::Connection;

use crate::error::{AgentError, Result};

const SCHEMA: &str = r#"
CREATE TABLE IF NOT EXISTS conversations (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    repo_path TEXT,
    title TEXT NOT NULL DEFAULT 'New Chat',
    created_at TEXT NOT NULL DEFAULT (datetime('now')),
    updated_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE TABLE IF NOT EXISTS messages (
    id INTEGER PRIMARY KEY AUTOINCREMENT,
    conversation_id INTEGER NOT NULL REFERENCES conversations(id) ON DELETE CASCADE,
    role TEXT NOT NULL,
    content_json TEXT NOT NULL,
    created_at TEXT NOT NULL DEFAULT (datetime('now'))
);

CREATE INDEX IF NOT EXISTS idx_messages_conversation ON messages(conversation_id);
"#;

#[derive(Debug, Clone)]
pub struct Conversation {
    pub id: i64,
    pub repo_path: Option<String>,
    pub title: String,
    pub created_at: String,
    pub updated_at: String,
}

#[derive(Debug, Clone)]
pub struct StoredMessage {
    pub id: i64,
    pub conversation_id: i64,
    pub role: String,
    pub content_json: String,
    pub created_at: String,
}

pub struct ChatStore {
    conn: Connection,
}

impl ChatStore {
    pub async fn open(path: &str) -> Result<Self> {
        let conn = Connection::open(path)
            .await
            .map_err(|e| AgentError::Database(e.to_string()))?;
        conn.call(|conn| {
            conn.execute_batch("PRAGMA journal_mode=WAL; PRAGMA foreign_keys=ON;")?;
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await
        .map_err(|e| AgentError::Database(e.to_string()))?;
        Ok(Self { conn })
    }

    pub async fn open_in_memory() -> Result<Self> {
        let conn = Connection::open_in_memory()
            .await
            .map_err(|e| AgentError::Database(e.to_string()))?;
        conn.call(|conn| {
            conn.execute_batch("PRAGMA foreign_keys=ON;")?;
            conn.execute_batch(SCHEMA)?;
            Ok(())
        })
        .await
        .map_err(|e| AgentError::Database(e.to_string()))?;
        Ok(Self { conn })
    }

    pub async fn create_conversation(&self, repo_path: Option<&str>, title: &str) -> Result<i64> {
        let rp = repo_path.map(String::from);
        let title = title.to_string();
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO conversations (repo_path, title, created_at, updated_at) VALUES (?1, ?2, ?3, ?3)",
                    params![rp, title, now],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await
            .map_err(|e| AgentError::Database(e.to_string()))
    }

    pub async fn list_conversations(&self, repo_path: Option<&str>) -> Result<Vec<Conversation>> {
        let rp = repo_path.map(String::from);
        self.conn
            .call(move |conn| {
                let mut stmt = if rp.is_some() {
                    conn.prepare(
                        "SELECT id, repo_path, title, created_at, updated_at FROM conversations
                         WHERE repo_path = ?1 ORDER BY updated_at DESC",
                    )?
                } else {
                    conn.prepare(
                        "SELECT id, repo_path, title, created_at, updated_at FROM conversations
                         ORDER BY updated_at DESC",
                    )?
                };

                let rows = if let Some(ref rp) = rp {
                    stmt.query_map(params![rp], map_conversation)?
                } else {
                    stmt.query_map([], map_conversation)?
                };

                let mut convos = Vec::new();
                for row in rows {
                    convos.push(row?);
                }
                Ok(convos)
            })
            .await
            .map_err(|e| AgentError::Database(e.to_string()))
    }

    pub async fn save_message(
        &self,
        conversation_id: i64,
        role: &str,
        content_json: &str,
    ) -> Result<i64> {
        let role = role.to_string();
        let content = content_json.to_string();
        let now = Utc::now().to_rfc3339();
        self.conn
            .call(move |conn| {
                conn.execute(
                    "INSERT INTO messages (conversation_id, role, content_json, created_at)
                     VALUES (?1, ?2, ?3, ?4)",
                    params![conversation_id, role, content, now],
                )?;
                conn.execute(
                    "UPDATE conversations SET updated_at = ?1 WHERE id = ?2",
                    params![now, conversation_id],
                )?;
                Ok(conn.last_insert_rowid())
            })
            .await
            .map_err(|e| AgentError::Database(e.to_string()))
    }

    pub async fn load_messages(&self, conversation_id: i64) -> Result<Vec<StoredMessage>> {
        self.conn
            .call(move |conn| {
                let mut stmt = conn.prepare(
                    "SELECT id, conversation_id, role, content_json, created_at
                     FROM messages WHERE conversation_id = ?1 ORDER BY id ASC",
                )?;
                let rows = stmt.query_map(params![conversation_id], |row| {
                    Ok(StoredMessage {
                        id: row.get(0)?,
                        conversation_id: row.get(1)?,
                        role: row.get(2)?,
                        content_json: row.get(3)?,
                        created_at: row.get(4)?,
                    })
                })?;
                let mut msgs = Vec::new();
                for row in rows {
                    msgs.push(row?);
                }
                Ok(msgs)
            })
            .await
            .map_err(|e| AgentError::Database(e.to_string()))
    }

    pub async fn delete_conversation(&self, id: i64) -> Result<()> {
        self.conn
            .call(move |conn| {
                conn.execute("DELETE FROM conversations WHERE id = ?1", params![id])?;
                Ok(())
            })
            .await
            .map_err(|e| AgentError::Database(e.to_string()))
    }
}

fn map_conversation(row: &rusqlite::Row) -> rusqlite::Result<Conversation> {
    Ok(Conversation {
        id: row.get(0)?,
        repo_path: row.get(1)?,
        title: row.get(2)?,
        created_at: row.get(3)?,
        updated_at: row.get(4)?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn create_and_list_conversations() {
        let store = ChatStore::open_in_memory().await.unwrap();
        let id = store
            .create_conversation(Some("/repo"), "Test Chat")
            .await
            .unwrap();
        assert!(id > 0);

        let convos = store.list_conversations(Some("/repo")).await.unwrap();
        assert_eq!(convos.len(), 1);
        assert_eq!(convos[0].title, "Test Chat");
    }

    #[tokio::test]
    async fn save_and_load_messages() {
        let store = ChatStore::open_in_memory().await.unwrap();
        let conv_id = store
            .create_conversation(None, "Chat")
            .await
            .unwrap();

        store
            .save_message(conv_id, "user", r#"[{"type":"text","text":"hello"}]"#)
            .await
            .unwrap();
        store
            .save_message(conv_id, "assistant", r#"[{"type":"text","text":"hi"}]"#)
            .await
            .unwrap();

        let msgs = store.load_messages(conv_id).await.unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[1].role, "assistant");
    }

    #[tokio::test]
    async fn delete_conversation_cascades() {
        let store = ChatStore::open_in_memory().await.unwrap();
        let conv_id = store
            .create_conversation(None, "Chat")
            .await
            .unwrap();
        store
            .save_message(conv_id, "user", "[]")
            .await
            .unwrap();

        store.delete_conversation(conv_id).await.unwrap();
        let msgs = store.load_messages(conv_id).await.unwrap();
        assert!(msgs.is_empty());
    }
}
