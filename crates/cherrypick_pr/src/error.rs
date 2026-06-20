use thiserror::Error;

#[derive(Debug, Error)]
pub enum PrError {
    #[error("PR not found: {0}")]
    NotFound(i64),

    #[error("Repo not found: {0}")]
    RepoNotFound(String),

    #[error("Branch not found: {0}")]
    BranchNotFound(String),

    #[error("Invalid status transition from {from} to {to}")]
    InvalidStatusTransition { from: String, to: String },

    #[error("Source and target branches cannot be the same: {0}")]
    SameBranch(String),

    #[error("PR already exists for this source/target combination")]
    DuplicatePr,

    #[error("Target branch has moved since merge started")]
    TargetMoved,

    #[error("Merge has conflicts")]
    MergeConflicts,

    #[error("Database error: {0}")]
    Database(#[from] rusqlite::Error),

    #[error("Database error: {0}")]
    AsyncDatabase(#[from] tokio_rusqlite::Error),

    #[error("Git error: {0}")]
    Git(#[from] git2::Error),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, PrError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let err = PrError::NotFound(42);
        assert!(err.to_string().contains("42"));
    }

    #[test]
    fn error_variants_are_constructible() {
        let _ = PrError::BranchNotFound("main".into());
        let _ = PrError::SameBranch("main".into());
        let _ = PrError::TargetMoved;
        let _ = PrError::MergeConflicts;
        let _ = PrError::Other("test".into());
    }
}
