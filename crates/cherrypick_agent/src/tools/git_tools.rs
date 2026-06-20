use std::path::Path;

use async_trait::async_trait;
use serde_json::json;

use crate::error::{AgentError, Result};
use crate::provider::types::{RiskLevel, ToolDefinition};
use super::ToolHandler;
use super::safe_path::resolve_safe_path;

pub struct ReadFileTool;

#[async_trait]
impl ToolHandler for ReadFileTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "read_file".to_string(),
            description: "Read the contents of a file".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative path to the file" }
                },
                "required": ["path"]
            }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::ReadOnly }

    async fn execute(&self, args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let path = args["path"].as_str().ok_or_else(|| AgentError::ToolExecution("Missing 'path' argument".into()))?;

        if super::is_sensitive_path(path) {
            return Err(AgentError::ToolExecution("Cannot read sensitive files".into()));
        }

        let full_path = resolve_safe_path(repo_path, path)?;

        std::fs::read_to_string(&full_path)
            .map_err(|e| AgentError::ToolExecution(format!("Failed to read file: {e}")))
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        format!("Read file: {}", args["path"].as_str().unwrap_or("?"))
    }
}

pub struct ListFilesTool;

#[async_trait]
impl ToolHandler for ListFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "list_files".to_string(),
            description: "List files in a directory".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": { "type": "string", "description": "Relative directory path" }
                }
            }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::ReadOnly }

    async fn execute(&self, args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let dir_path = args["path"].as_str().unwrap_or(".");
        let full_path = resolve_safe_path(repo_path, dir_path)?;

        let mut entries = Vec::new();
        let read_dir = std::fs::read_dir(&full_path)
            .map_err(|e| AgentError::ToolExecution(format!("Failed to list directory: {e}")))?;

        for entry in read_dir {
            let entry = entry.map_err(|e| AgentError::ToolExecution(e.to_string()))?;
            let name = entry.file_name().to_string_lossy().to_string();
            let ft = entry.file_type().map_err(|e| AgentError::ToolExecution(e.to_string()))?;
            let prefix = if ft.is_dir() { "d " } else { "f " };
            entries.push(format!("{prefix}{name}"));
        }

        entries.sort();
        Ok(entries.join("\n"))
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        format!("List files in: {}", args["path"].as_str().unwrap_or("."))
    }
}

pub struct GitStatusTool;

#[async_trait]
impl ToolHandler for GitStatusTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_status".to_string(),
            description: "Show the working tree status of the git repository".to_string(),
            input_schema: json!({ "type": "object", "properties": {} }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::ReadOnly }

    async fn execute(&self, _args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let repo = git2::Repository::discover(repo_path)
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        let statuses = repo.statuses(Some(
            git2::StatusOptions::new()
                .include_untracked(true)
                .recurse_untracked_dirs(true),
        )).map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        let mut output = Vec::new();
        for entry in statuses.iter() {
            let path = entry.path().unwrap_or("?");
            let status = entry.status();
            let indicator = format_status(status);
            output.push(format!("{indicator} {path}"));
        }

        if output.is_empty() {
            Ok("Working tree clean".to_string())
        } else {
            Ok(output.join("\n"))
        }
    }

    fn preview(&self, _args: &serde_json::Value) -> String {
        "Show git status".to_string()
    }
}

fn format_status(status: git2::Status) -> String {
    let index = if status.is_index_new() { "A" }
        else if status.is_index_modified() { "M" }
        else if status.is_index_deleted() { "D" }
        else if status.is_index_renamed() { "R" }
        else { " " };

    let wt = if status.is_wt_new() { "?" }
        else if status.is_wt_modified() { "M" }
        else if status.is_wt_deleted() { "D" }
        else { " " };

    format!("{index}{wt}")
}

pub struct GitDiffTool;

#[async_trait]
impl ToolHandler for GitDiffTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_diff".to_string(),
            description: "Show changes in the working directory or between commits".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "staged": { "type": "boolean", "description": "Show staged changes only" }
                }
            }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::ReadOnly }

    async fn execute(&self, args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let staged = args["staged"].as_bool().unwrap_or(false);
        let repo = git2::Repository::discover(repo_path)
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        let diff = if staged {
            let head_tree = repo.head().ok().and_then(|h| h.peel_to_tree().ok());
            repo.diff_tree_to_index(head_tree.as_ref(), None, None)
        } else {
            repo.diff_index_to_workdir(None, None)
        }.map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        let mut output = String::new();
        diff.print(git2::DiffFormat::Patch, |_delta, _hunk, line| {
            let prefix = match line.origin() {
                '+' => "+",
                '-' => "-",
                ' ' => " ",
                _ => "",
            };
            if let Ok(content) = std::str::from_utf8(line.content()) {
                output.push_str(prefix);
                output.push_str(content);
            }
            true
        }).map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        if output.is_empty() {
            Ok("No changes".to_string())
        } else {
            Ok(output)
        }
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        if args["staged"].as_bool().unwrap_or(false) {
            "Show staged diff".to_string()
        } else {
            "Show working directory diff".to_string()
        }
    }
}

pub struct GitLogTool;

#[async_trait]
impl ToolHandler for GitLogTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "git_log".to_string(),
            description: "Show recent commit history".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "count": { "type": "integer", "description": "Number of commits to show (default 10)" }
                }
            }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::ReadOnly }

    async fn execute(&self, args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let count = args["count"].as_u64().unwrap_or(10) as usize;
        let repo = git2::Repository::discover(repo_path)
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        let mut revwalk = repo.revwalk().map_err(|e| AgentError::ToolExecution(e.to_string()))?;
        revwalk.push_head().map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        let mut output = Vec::new();
        for (i, oid) in revwalk.enumerate() {
            if i >= count { break; }
            let oid = oid.map_err(|e| AgentError::ToolExecution(e.to_string()))?;
            let commit = repo.find_commit(oid).map_err(|e| AgentError::ToolExecution(e.to_string()))?;
            let short = &oid.to_string()[..7];
            let msg = commit.message().unwrap_or("").lines().next().unwrap_or("");
            let author = commit.author().name().unwrap_or("").to_string();
            output.push(format!("{short} {author}: {msg}"));
        }

        Ok(output.join("\n"))
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        let count = args["count"].as_u64().unwrap_or(10);
        format!("Show last {count} commits")
    }
}

pub struct SearchCodeTool;

#[async_trait]
impl ToolHandler for SearchCodeTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "search_code".to_string(),
            description: "Search for a pattern in repository files".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": { "type": "string", "description": "Text pattern to search for" },
                    "path": { "type": "string", "description": "Optional path filter" }
                },
                "required": ["pattern"]
            }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::ReadOnly }

    async fn execute(&self, args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let pattern = args["pattern"].as_str()
            .ok_or_else(|| AgentError::ToolExecution("Missing 'pattern'".into()))?;
        let search_path = args["path"].as_str().unwrap_or(".");
        let full_path = resolve_safe_path(repo_path, search_path)?;

        let mut results = Vec::new();
        search_recursive(&full_path, pattern, repo_path, &mut results, 100)?;

        if results.is_empty() {
            Ok(format!("No matches found for '{pattern}'"))
        } else {
            Ok(results.join("\n"))
        }
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        format!("Search for: {}", args["pattern"].as_str().unwrap_or("?"))
    }
}

fn search_recursive(
    dir: &Path,
    pattern: &str,
    repo_root: &Path,
    results: &mut Vec<String>,
    max_results: usize,
) -> Result<()> {
    if results.len() >= max_results {
        return Ok(());
    }

    let entries = match std::fs::read_dir(dir) {
        Ok(e) => e,
        Err(_) => return Ok(()),
    };

    for entry in entries {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }

        let metadata = match std::fs::symlink_metadata(&path) {
            Ok(m) => m,
            Err(_) => continue,
        };
        if metadata.file_type().is_symlink() {
            continue;
        }

        if path.is_dir() {
            search_recursive(&path, pattern, repo_root, results, max_results)?;
        } else if path.is_file() {
            if let Ok(content) = std::fs::read_to_string(&path) {
                for (lineno, line) in content.lines().enumerate() {
                    if line.contains(pattern) {
                        let rel = path.strip_prefix(repo_root).unwrap_or(&path);
                        results.push(format!("{}:{}: {}", rel.display(), lineno + 1, line.trim()));
                        if results.len() >= max_results {
                            return Ok(());
                        }
                    }
                }
            }
        }
    }

    Ok(())
}

pub struct StageFilesTool;

#[async_trait]
impl ToolHandler for StageFilesTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "stage_files".to_string(),
            description: "Stage files for commit".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "paths": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Paths to stage"
                    }
                },
                "required": ["paths"]
            }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::WriteLocal }

    async fn execute(&self, args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let paths: Vec<String> = serde_json::from_value(args["paths"].clone())
            .map_err(|e| AgentError::ToolExecution(format!("Invalid paths: {e}")))?;

        let repo = git2::Repository::discover(repo_path)
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;
        let mut index = repo.index()
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        for path in &paths {
            if super::is_sensitive_path(path) {
                return Err(AgentError::ToolExecution(format!("Cannot stage sensitive file: {path}")));
            }
            index.add_path(Path::new(path))
                .map_err(|e| AgentError::ToolExecution(format!("Failed to stage {path}: {e}")))?;
        }

        index.write().map_err(|e| AgentError::ToolExecution(e.to_string()))?;
        Ok(format!("Staged {} file(s)", paths.len()))
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        let paths: Vec<&str> = args["paths"]
            .as_array()
            .map(|a| a.iter().filter_map(|v| v.as_str()).collect())
            .unwrap_or_default();
        format!("Stage: {}", paths.join(", "))
    }
}

pub struct CreateCommitTool;

#[async_trait]
impl ToolHandler for CreateCommitTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "create_commit".to_string(),
            description: "Create a git commit with staged changes".to_string(),
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": { "type": "string", "description": "Commit message" }
                },
                "required": ["message"]
            }),
        }
    }

    fn risk_level(&self) -> RiskLevel { RiskLevel::WriteLocal }

    async fn execute(&self, args: serde_json::Value, repo_path: &Path) -> Result<String> {
        let message = args["message"].as_str()
            .ok_or_else(|| AgentError::ToolExecution("Missing 'message'".into()))?;

        let repo = git2::Repository::discover(repo_path)
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;
        let sig = repo.signature()
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;
        let mut index = repo.index()
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;
        let tree_id = index.write_tree()
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;
        let tree = repo.find_tree(tree_id)
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        let parents: Vec<git2::Commit> = match repo.head() {
            Ok(head) => vec![head.peel_to_commit()
                .map_err(|e| AgentError::ToolExecution(e.to_string()))?],
            Err(_) => vec![],
        };
        let parent_refs: Vec<&git2::Commit> = parents.iter().collect();

        let oid = repo.commit(Some("HEAD"), &sig, &sig, message, &tree, &parent_refs)
            .map_err(|e| AgentError::ToolExecution(e.to_string()))?;

        Ok(format!("Created commit {}", &oid.to_string()[..7]))
    }

    fn preview(&self, args: &serde_json::Value) -> String {
        format!("Create commit: {}", args["message"].as_str().unwrap_or("?"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn read_file_blocks_sensitive() {
        let tool = ReadFileTool;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.execute(
            json!({"path": ".env"}),
            Path::new("/tmp"),
        ));
        assert!(result.is_err());
    }

    #[test]
    fn read_file_blocks_traversal() {
        let tool = ReadFileTool;
        let rt = tokio::runtime::Runtime::new().unwrap();
        let result = rt.block_on(tool.execute(
            json!({"path": "../../etc/passwd"}),
            Path::new("/tmp/repo"),
        ));
        assert!(result.is_err());
    }

    #[test]
    fn format_status_flags() {
        let status = git2::Status::INDEX_NEW;
        let s = format_status(status);
        assert!(s.starts_with('A'));
    }

    #[test]
    fn tool_definitions_valid() {
        let tools: Vec<Box<dyn ToolHandler>> = vec![
            Box::new(ReadFileTool),
            Box::new(ListFilesTool),
            Box::new(GitStatusTool),
            Box::new(GitDiffTool),
            Box::new(GitLogTool),
            Box::new(SearchCodeTool),
            Box::new(StageFilesTool),
            Box::new(CreateCommitTool),
        ];
        for tool in &tools {
            let def = tool.definition();
            assert!(!def.name.is_empty());
            assert!(!def.description.is_empty());
        }
    }
}
