
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};


#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PrStatus {
    Open,
    Merged,
    Closed,
}

impl PrStatus {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Open => "open",
            Self::Merged => "merged",
            Self::Closed => "closed",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "open" => Some(Self::Open),
            "merged" => Some(Self::Merged),
            "closed" => Some(Self::Closed),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum MergeStrategy {
    MergeCommit,
    Squash,
}

impl MergeStrategy {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::MergeCommit => "merge",
            Self::Squash => "squash",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "merge" => Some(Self::MergeCommit),
            "squash" => Some(Self::Squash),
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LocalPr {
    pub id: i64,
    pub repo_id: i64,
    pub title: String,
    pub source_branch: String,
    pub target_branch: String,
    pub source_oid: String,
    pub target_oid: String,
    pub status: PrStatus,
    pub merge_strategy: Option<MergeStrategy>,
    pub merged_oid: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct BranchHealth {
    pub exists: bool,
    pub force_pushed: bool,
    pub ahead: u32,
    pub behind: u32,
    pub current_oid: Option<String>,
}

impl Default for BranchHealth {
    fn default() -> Self {
        Self {
            exists: true,
            force_pushed: false,
            ahead: 0,
            behind: 0,
            current_oid: None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentEncoding {
    Utf8,
    Latin1,
    Binary,
}

#[derive(Debug, Clone)]
pub struct FileContent {
    pub data: Vec<u8>,
    pub encoding: ContentEncoding,
    pub is_lfs: bool,
}

impl FileContent {
    pub fn as_text(&self) -> Option<String> {
        match self.encoding {
            ContentEncoding::Utf8 => String::from_utf8(self.data.clone()).ok(),
            ContentEncoding::Latin1 => {
                Some(self.data.iter().map(|&b| b as char).collect())
            }
            ContentEncoding::Binary => None,
        }
    }

    pub fn detect_encoding(data: &[u8]) -> ContentEncoding {
        if data.is_empty() {
            return ContentEncoding::Utf8;
        }
        if std::str::from_utf8(data).is_ok() {
            ContentEncoding::Utf8
        } else if data.contains(&0) {
            ContentEncoding::Binary
        } else {
            ContentEncoding::Latin1
        }
    }

    pub fn is_lfs_pointer(data: &[u8]) -> bool {
        data.starts_with(b"version https://git-lfs.github.com/spec/v1")
    }
}

#[derive(Debug, Clone)]
pub struct ThreeWayContent {
    pub base: Option<FileContent>,
    pub ours: FileContent,
    pub theirs: FileContent,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PrSnapshot {
    pub id: i64,
    pub pr_id: i64,
    pub source_oid: String,
    pub target_oid: String,
    pub is_force_push: bool,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RepoRecord {
    pub id: i64,
    pub first_commit_oid: String,
    pub remote_urls_hash: String,
    pub canonical_path: String,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pr_status_round_trip() {
        for status in [PrStatus::Open, PrStatus::Merged, PrStatus::Closed] {
            let s = status.as_str();
            assert_eq!(PrStatus::from_str(s), Some(status));
        }
    }

    #[test]
    fn merge_strategy_round_trip() {
        for strategy in [MergeStrategy::MergeCommit, MergeStrategy::Squash] {
            let s = strategy.as_str();
            assert_eq!(MergeStrategy::from_str(s), Some(strategy));
        }
    }

    #[test]
    fn file_content_encoding_detection() {
        assert_eq!(
            FileContent::detect_encoding(b"hello world"),
            ContentEncoding::Utf8
        );
        assert_eq!(
            FileContent::detect_encoding(b"\xff\xfe\x00\x01"),
            ContentEncoding::Binary
        );
        assert_eq!(FileContent::detect_encoding(b""), ContentEncoding::Utf8);
    }

    #[test]
    fn lfs_pointer_detection() {
        assert!(FileContent::is_lfs_pointer(
            b"version https://git-lfs.github.com/spec/v1\noid sha256:abc"
        ));
        assert!(!FileContent::is_lfs_pointer(b"regular file content"));
    }

    #[test]
    fn file_content_as_text() {
        let utf8 = FileContent {
            data: b"hello".to_vec(),
            encoding: ContentEncoding::Utf8,
            is_lfs: false,
        };
        assert_eq!(utf8.as_text(), Some("hello".to_string()));

        let binary = FileContent {
            data: vec![0, 1, 2],
            encoding: ContentEncoding::Binary,
            is_lfs: false,
        };
        assert!(binary.as_text().is_none());
    }

    #[test]
    fn branch_health_defaults() {
        let h = BranchHealth::default();
        assert!(h.exists);
        assert!(!h.force_pushed);
        assert_eq!(h.ahead, 0);
        assert_eq!(h.behind, 0);
    }
}
