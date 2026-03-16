use std::path::{Path, PathBuf};

use crate::error::{AgentError, Result};

pub fn resolve_safe_path(repo_root: &Path, relative_path: &str) -> Result<PathBuf> {
    if relative_path.is_empty() {
        return Ok(repo_root.to_path_buf());
    }

    let canonical_root = repo_root
        .canonicalize()
        .map_err(|e| AgentError::ToolExecution(format!("Cannot canonicalize repo root: {e}")))?;

    let joined = canonical_root.join(relative_path);

    let resolved = if joined.exists() {
        joined.canonicalize().map_err(|e| {
            AgentError::ToolExecution(format!("Cannot canonicalize path: {e}"))
        })?
    } else {
        let mut ancestor = joined.as_path();
        loop {
            if let Some(parent) = ancestor.parent() {
                if parent.exists() {
                    let canonical_parent = parent.canonicalize().map_err(|e| {
                        AgentError::ToolExecution(format!("Cannot canonicalize: {e}"))
                    })?;
                    let remainder = joined.strip_prefix(parent).unwrap_or(Path::new(""));
                    break canonical_parent.join(remainder);
                }
                ancestor = parent;
            } else {
                return Err(AgentError::ToolExecution(
                    "Path traversal detected: cannot resolve path".into(),
                ));
            }
        }
    };

    if !resolved.starts_with(&canonical_root) {
        return Err(AgentError::ToolExecution(
            "Path traversal detected: resolved path escapes repository".into(),
        ));
    }

    if resolved.is_symlink() {
        let link_target = std::fs::read_link(&resolved).map_err(|e| {
            AgentError::ToolExecution(format!("Cannot read symlink: {e}"))
        })?;
        let absolute_target = if link_target.is_absolute() {
            link_target
        } else {
            resolved.parent().unwrap_or(&canonical_root).join(&link_target)
        };
        let canonical_target = absolute_target.canonicalize().map_err(|e| {
            AgentError::ToolExecution(format!("Cannot canonicalize symlink target: {e}"))
        })?;
        if !canonical_target.starts_with(&canonical_root) {
            return Err(AgentError::ToolExecution(
                "Path traversal detected: symlink escapes repository".into(),
            ));
        }
    }

    Ok(resolved)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple_path_resolves() {
        let tmp = tempfile::tempdir().unwrap();
        let file = tmp.path().join("test.txt");
        std::fs::write(&file, "hello").unwrap();
        let result = resolve_safe_path(tmp.path(), "test.txt");
        assert!(result.is_ok());
    }

    #[test]
    fn dot_dot_traversal_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.txt"), "a").unwrap();
        let result = resolve_safe_path(tmp.path(), "../../etc/passwd");
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("traversal"));
    }

    #[test]
    fn symlink_escape_rejected() {
        let tmp = tempfile::tempdir().unwrap();
        let link_path = tmp.path().join("escape_link");
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink("/etc", &link_path).unwrap();
            let result = resolve_safe_path(tmp.path(), "escape_link");
            assert!(result.is_err());
        }
    }

    #[test]
    fn empty_path_returns_root() {
        let tmp = tempfile::tempdir().unwrap();
        let result = resolve_safe_path(tmp.path(), "");
        assert!(result.is_ok());
    }
}
