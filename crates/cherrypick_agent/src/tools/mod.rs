pub mod git_tools;
pub mod safe_path;

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use async_trait::async_trait;
use tokio::sync::oneshot;

use crate::error::{AgentError, Result};
use crate::provider::types::{RiskLevel, ToolCall, ToolDefinition};

const OUTPUT_TRUNCATION_LIMIT: usize = 50_000;

static SENSITIVE_PATTERNS: &[&str] = &[
    ".env",
    ".env.local",
    ".env.production",
    "credentials.json",
    "id_rsa",
    "id_ed25519",
    ".ssh/config",
    ".netrc",
    ".npmrc",
    "token",
    "secret",
];

#[async_trait]
pub trait ToolHandler: Send + Sync {
    fn definition(&self) -> ToolDefinition;
    fn risk_level(&self) -> RiskLevel;
    async fn execute(&self, arguments: serde_json::Value, repo_path: &Path) -> Result<String>;
    fn preview(&self, arguments: &serde_json::Value) -> String;
}

pub struct ConfirmationRequest {
    pub tool_name: String,
    pub risk_level: RiskLevel,
    pub preview: String,
    pub response: oneshot::Sender<bool>,
}

pub struct ToolExecutor {
    tools: HashMap<String, Box<dyn ToolHandler>>,
    excluded_repos: Vec<PathBuf>,
}

impl ToolExecutor {
    pub fn new() -> Self {
        let mut executor = Self {
            tools: HashMap::new(),
            excluded_repos: Vec::new(),
        };
        executor.register_builtin_tools();
        executor
    }

    fn register_builtin_tools(&mut self) {
        self.register(Box::new(git_tools::ReadFileTool));
        self.register(Box::new(git_tools::ListFilesTool));
        self.register(Box::new(git_tools::GitStatusTool));
        self.register(Box::new(git_tools::GitDiffTool));
        self.register(Box::new(git_tools::GitLogTool));
        self.register(Box::new(git_tools::SearchCodeTool));
        self.register(Box::new(git_tools::StageFilesTool));
        self.register(Box::new(git_tools::CreateCommitTool));
    }

    pub fn register(&mut self, handler: Box<dyn ToolHandler>) {
        let name = handler.definition().name.clone();
        self.tools.insert(name, handler);
    }

    pub fn definitions(&self) -> Vec<ToolDefinition> {
        self.tools.values().map(|h| h.definition()).collect()
    }

    pub fn exclude_repo(&mut self, path: PathBuf) {
        self.excluded_repos.push(path);
    }

    pub fn is_repo_excluded(&self, path: &Path) -> bool {
        self.excluded_repos.iter().any(|p| path.starts_with(p))
    }

    pub async fn execute(
        &self,
        call: &ToolCall,
        repo_path: &Path,
    ) -> Result<String> {
        let handler = self
            .tools
            .get(&call.name)
            .ok_or_else(|| AgentError::ToolNotFound(call.name.clone()))?;

        if self.is_repo_excluded(repo_path) {
            return Err(AgentError::ToolExecution(format!(
                "Repository is excluded from tool operations"
            )));
        }

        let mut output = handler.execute(call.arguments.clone(), repo_path).await?;

        if output.len() > OUTPUT_TRUNCATION_LIMIT {
            output.truncate(OUTPUT_TRUNCATION_LIMIT);
            output.push_str("\n... (output truncated)");
        }

        Ok(output)
    }

    pub fn risk_level(&self, tool_name: &str) -> Option<RiskLevel> {
        self.tools.get(tool_name).map(|h| h.risk_level())
    }

    pub fn preview(&self, call: &ToolCall) -> Option<String> {
        self.tools.get(&call.name).map(|h| h.preview(&call.arguments))
    }
}

pub fn is_sensitive_path(path: &str) -> bool {
    let lower = path.to_lowercase();
    SENSITIVE_PATTERNS.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_path_detection() {
        assert!(is_sensitive_path(".env"));
        assert!(is_sensitive_path("path/to/.env.local"));
        assert!(is_sensitive_path("credentials.json"));
        assert!(is_sensitive_path("my_secret_file"));
        assert!(!is_sensitive_path("src/main.rs"));
        assert!(!is_sensitive_path("README.md"));
    }

    #[test]
    fn executor_has_builtin_tools() {
        let executor = ToolExecutor::new();
        let defs = executor.definitions();
        assert!(!defs.is_empty());
        assert!(defs.iter().any(|d| d.name == "read_file"));
        assert!(defs.iter().any(|d| d.name == "git_status"));
    }

    #[test]
    fn repo_exclusion() {
        let mut executor = ToolExecutor::new();
        executor.exclude_repo(PathBuf::from("/excluded/repo"));
        assert!(executor.is_repo_excluded(Path::new("/excluded/repo/subdir")));
        assert!(!executor.is_repo_excluded(Path::new("/other/repo")));
    }

    #[test]
    fn risk_level_lookup() {
        let executor = ToolExecutor::new();
        assert_eq!(
            executor.risk_level("read_file"),
            Some(RiskLevel::ReadOnly)
        );
        assert_eq!(
            executor.risk_level("create_commit"),
            Some(RiskLevel::WriteLocal)
        );
        assert!(executor.risk_level("nonexistent").is_none());
    }
}
