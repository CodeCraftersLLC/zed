use thiserror::Error;

#[derive(Debug, Error)]
pub enum AgentError {
    #[error("Provider error: {0}")]
    Provider(String),

    #[error("Rate limited: retry after {retry_after_secs}s")]
    RateLimited { retry_after_secs: u64 },

    #[error("Context too long: {0} tokens exceeds budget")]
    ContextTooLong(usize),

    #[error("Tool execution failed: {0}")]
    ToolExecution(String),

    #[error("Tool not found: {0}")]
    ToolNotFound(String),

    #[error("Tool call denied by user")]
    ToolDenied,

    #[error("Max iterations ({0}) exceeded")]
    MaxIterations(u32),

    #[error("Max duration exceeded")]
    MaxDuration,

    #[error("Cancelled")]
    Cancelled,

    #[error("Key not found for provider: {0}")]
    KeyNotFound(String),

    #[error("Skill not found: {0}")]
    SkillNotFound(String),

    #[error("Skill parse error: {0}")]
    SkillParse(String),

    #[error("MCP error: {0}")]
    Mcp(String),

    #[error("Database error: {0}")]
    Database(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("JSON error: {0}")]
    Json(#[from] serde_json::Error),

    #[error("HTTP error: {0}")]
    Http(#[from] reqwest::Error),

    #[error("{0}")]
    Other(String),
}

pub type Result<T> = std::result::Result<T, AgentError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display() {
        let err = AgentError::ToolNotFound("test_tool".into());
        assert!(err.to_string().contains("test_tool"));
    }

    #[test]
    fn rate_limited_displays_retry() {
        let err = AgentError::RateLimited {
            retry_after_secs: 30,
        };
        assert!(err.to_string().contains("30"));
    }
}
