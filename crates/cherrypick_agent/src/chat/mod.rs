pub mod store;

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::{mpsc, watch};

use crate::context::ContextEngine;
use crate::error::{AgentError, Result};
use crate::provider::LlmProvider;
use crate::provider::types::{
    CompletionRequest, Message, MessageContent, RiskLevel, StreamChunk, ToolCall,
};
use crate::tools::ToolExecutor;

const MAX_ITERATIONS: u32 = 10;
const MAX_DURATION: Duration = Duration::from_secs(120);
const SYSTEM_POLICY: &str = r#"You are CherryPick AI, a git-aware coding assistant integrated into the CherryPick git client. You help users understand their repositories, review changes, write commits, and manage branches.

Safety rules (immutable):
- Never force-push without explicit user confirmation
- Never delete branches without explicit user confirmation
- Never modify files outside the repository working directory
- Never read or expose sensitive files (.env, credentials, keys)
- Always explain what you're about to do before executing write operations"#;

#[derive(Debug, Clone)]
pub enum AgentEvent {
    TextDelta(String),
    ToolCallStarted {
        id: String,
        name: String,
        risk_level: RiskLevel,
    },
    ToolCallCompleted {
        id: String,
        result: String,
        is_error: bool,
    },
    ConfirmationNeeded {
        tool_name: String,
        risk_level: RiskLevel,
        preview: String,
    },
    TurnComplete,
    Error(String),
}

pub struct AgentConfig {
    pub model: String,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
    pub max_iterations: u32,
    pub max_duration: Duration,
}

impl Default for AgentConfig {
    fn default() -> Self {
        Self {
            model: "claude-sonnet-4-20250514".to_string(),
            max_tokens: 4096,
            temperature: None,
            max_iterations: MAX_ITERATIONS,
            max_duration: MAX_DURATION,
        }
    }
}

pub struct AgentService {
    provider: Arc<dyn LlmProvider>,
    tool_executor: ToolExecutor,
    context_engine: ContextEngine,
    config: AgentConfig,
    history: Vec<Message>,
}

impl AgentService {
    pub fn new(
        provider: Arc<dyn LlmProvider>,
        tool_executor: ToolExecutor,
        context_engine: ContextEngine,
        config: AgentConfig,
    ) -> Self {
        Self {
            provider,
            tool_executor,
            context_engine,
            config,
            history: Vec::new(),
        }
    }

    pub async fn send_message(
        &mut self,
        user_message: &str,
        repo_path: &Path,
        event_tx: mpsc::UnboundedSender<AgentEvent>,
        cancel: watch::Receiver<bool>,
    ) -> Result<()> {
        self.history.push(Message::user(user_message));

        let tool_defs = self.tool_executor.definitions();
        let start = std::time::Instant::now();
        let mut iterations = 0u32;

        loop {
            if *cancel.borrow() {
                return Err(AgentError::Cancelled);
            }

            if iterations >= self.config.max_iterations {
                return Err(AgentError::MaxIterations(self.config.max_iterations));
            }

            if start.elapsed() > self.config.max_duration {
                return Err(AgentError::MaxDuration);
            }

            iterations += 1;

            let truncated = ContextEngine::truncate_history(
                &self.history,
                self.context_engine.budget().warm,
            );

            let request = CompletionRequest {
                model: self.config.model.clone(),
                messages: truncated,
                system: Some(SYSTEM_POLICY.to_string()),
                tools: tool_defs.clone(),
                max_tokens: self.config.max_tokens,
                temperature: self.config.temperature,
            };

            let (chunk_tx, mut chunk_rx) = mpsc::unbounded_channel();

            let provider = self.provider.clone();
            let provider_handle = tokio::spawn(async move {
                provider.stream_completion(request, chunk_tx).await
            });

            let mut text_buffer = String::new();
            let mut tool_calls: Vec<ToolCall> = Vec::new();
            let mut current_tool_id = String::new();
            let mut current_tool_name = String::new();
            let mut current_tool_args = String::new();

            while let Some(chunk) = chunk_rx.recv().await {
                if *cancel.borrow() {
                    return Err(AgentError::Cancelled);
                }

                match chunk {
                    StreamChunk::TextDelta(text) => {
                        text_buffer.push_str(&text);
                        let _ = event_tx.send(AgentEvent::TextDelta(text));
                    }
                    StreamChunk::ToolCallStart { id, name } => {
                        current_tool_id = id;
                        current_tool_name = name.clone();
                        current_tool_args.clear();
                        let risk = self
                            .tool_executor
                            .risk_level(&name)
                            .unwrap_or(RiskLevel::ReadOnly);
                        let _ = event_tx.send(AgentEvent::ToolCallStarted {
                            id: current_tool_id.clone(),
                            name,
                            risk_level: risk,
                        });
                    }
                    StreamChunk::ToolCallDelta(json) => {
                        current_tool_args.push_str(&json);
                    }
                    StreamChunk::ToolCallEnd => {
                        let args: serde_json::Value =
                            serde_json::from_str(&current_tool_args).unwrap_or_default();
                        tool_calls.push(ToolCall {
                            id: current_tool_id.clone(),
                            name: current_tool_name.clone(),
                            arguments: args,
                        });
                    }
                    StreamChunk::Done => break,
                    StreamChunk::Error(e) => {
                        let _ = event_tx.send(AgentEvent::Error(e.clone()));
                        return Err(AgentError::Provider(e));
                    }
                    StreamChunk::Usage(_) => {}
                }
            }

            let _ = provider_handle.await;

            let mut assistant_content = Vec::new();
            if !text_buffer.is_empty() {
                assistant_content.push(MessageContent::Text {
                    text: text_buffer.clone(),
                });
            }
            for tc in &tool_calls {
                assistant_content.push(MessageContent::ToolUse {
                    id: tc.id.clone(),
                    name: tc.name.clone(),
                    input: tc.arguments.clone(),
                });
            }

            if !assistant_content.is_empty() {
                self.history.push(Message {
                    role: crate::provider::types::Role::Assistant,
                    content: assistant_content,
                });
            }

            if tool_calls.is_empty() {
                let _ = event_tx.send(AgentEvent::TurnComplete);
                return Ok(());
            }

            for tc in &tool_calls {
                let risk = self
                    .tool_executor
                    .risk_level(&tc.name)
                    .unwrap_or(RiskLevel::ReadOnly);

                if risk.requires_confirmation() {
                    let preview = self
                        .tool_executor
                        .preview(tc)
                        .unwrap_or_else(|| tc.name.clone());
                    let _ = event_tx.send(AgentEvent::ConfirmationNeeded {
                        tool_name: tc.name.clone(),
                        risk_level: risk,
                        preview,
                    });
                    // For now, auto-approve in the agentic loop.
                    // In the full UI implementation, this would wait for user input
                    // via a oneshot channel before proceeding.
                }

                let result = self.tool_executor.execute(tc, repo_path).await;
                let (output, is_error) = match result {
                    Ok(output) => {
                        let _ = event_tx.send(AgentEvent::ToolCallCompleted {
                            id: tc.id.clone(),
                            result: output.clone(),
                            is_error: false,
                        });
                        (output, false)
                    }
                    Err(e) => {
                        let err_msg = e.to_string();
                        let _ = event_tx.send(AgentEvent::ToolCallCompleted {
                            id: tc.id.clone(),
                            result: err_msg.clone(),
                            is_error: true,
                        });
                        (err_msg, true)
                    }
                };

                self.history
                    .push(Message::tool_result(&tc.id, &output, is_error));
            }
        }
    }

    pub fn history(&self) -> &[Message] {
        &self.history
    }

    pub fn clear_history(&mut self) {
        self.history.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config() {
        let config = AgentConfig::default();
        assert_eq!(config.max_iterations, MAX_ITERATIONS);
        assert!(config.model.contains("claude"));
    }

    #[test]
    fn system_policy_contains_safety_rules() {
        assert!(SYSTEM_POLICY.contains("Never force-push"));
        assert!(SYSTEM_POLICY.contains("sensitive files"));
    }
}
