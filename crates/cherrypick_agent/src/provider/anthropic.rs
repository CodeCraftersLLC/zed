use async_trait::async_trait;
use reqwest::Client;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;

use crate::error::{AgentError, Result};
use super::LlmProvider;
use super::types::{CompletionRequest, StreamChunk, Usage};

const ANTHROPIC_API_URL: &str = "https://api.anthropic.com/v1/messages";
const ANTHROPIC_VERSION: &str = "2023-06-01";

pub struct AnthropicProvider {
    api_key: String,
    client: Client,
}

impl AnthropicProvider {
    pub fn new(api_key: String) -> Self {
        Self {
            api_key,
            client: Client::new(),
        }
    }
}

#[derive(Serialize)]
struct AnthropicRequest {
    model: String,
    messages: serde_json::Value,
    max_tokens: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    system: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<serde_json::Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    stream: bool,
}

#[derive(Deserialize, Debug)]
struct SseEvent {
    #[serde(rename = "type")]
    event_type: String,
    #[serde(default)]
    delta: Option<SseDelta>,
    #[serde(default)]
    content_block: Option<ContentBlock>,
    #[serde(default)]
    usage: Option<SseUsage>,
    #[serde(default)]
    error: Option<SseError>,
    #[serde(default)]
    index: Option<u32>,
}

#[derive(Deserialize, Debug, Default)]
struct SseDelta {
    #[serde(rename = "type", default)]
    delta_type: String,
    #[serde(default)]
    text: Option<String>,
    #[serde(default)]
    partial_json: Option<String>,
}

#[derive(Deserialize, Debug)]
struct ContentBlock {
    #[serde(rename = "type")]
    block_type: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    text: Option<String>,
}

#[derive(Deserialize, Debug)]
struct SseUsage {
    input_tokens: u32,
    output_tokens: u32,
}

#[derive(Deserialize, Debug)]
struct SseError {
    message: String,
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    fn name(&self) -> &str {
        "anthropic"
    }

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        tx: mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<()> {
        let tools_json: Vec<serde_json::Value> = request
            .tools
            .iter()
            .map(|t| {
                serde_json::json!({
                    "name": t.name,
                    "description": t.description,
                    "input_schema": t.input_schema,
                })
            })
            .collect();

        let messages_json = serde_json::to_value(&request.messages)?;

        let body = AnthropicRequest {
            model: request.model,
            messages: messages_json,
            max_tokens: request.max_tokens,
            system: request.system,
            tools: tools_json,
            temperature: request.temperature,
            stream: true,
        };

        let body_json = serde_json::to_string(&body)?;
        let response = self
            .client
            .post(ANTHROPIC_API_URL)
            .header("x-api-key", &self.api_key)
            .header("anthropic-version", ANTHROPIC_VERSION)
            .header("content-type", "application/json")
            .body(body_json)
            .send()
            .await?;

        let status = response.status();
        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            if status.as_u16() == 429 {
                return Err(AgentError::RateLimited {
                    retry_after_secs: 60,
                });
            }
            if status.as_u16() == 401 {
                return Err(AgentError::Provider("Invalid API key".into()));
            }
            return Err(AgentError::Provider(format!(
                "HTTP {}: {}",
                status, error_text
            )));
        }

        let text = response.text().await?;

        for line in text.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with(':') {
                continue;
            }
            if let Some(data) = line.strip_prefix("data: ") {
                if data == "[DONE]" {
                    let _ = tx.send(StreamChunk::Done);
                    break;
                }
                if let Ok(event) = serde_json::from_str::<SseEvent>(data) {
                    match event.event_type.as_str() {
                        "content_block_start" => {
                            if let Some(block) = &event.content_block {
                                if block.block_type == "tool_use" {
                                    let _ = tx.send(StreamChunk::ToolCallStart {
                                        id: block.id.clone().unwrap_or_default(),
                                        name: block.name.clone().unwrap_or_default(),
                                    });
                                }
                                if block.block_type == "text" {
                                    if let Some(text) = &block.text {
                                        if !text.is_empty() {
                                            let _ = tx.send(StreamChunk::TextDelta(text.clone()));
                                        }
                                    }
                                }
                            }
                        }
                        "content_block_delta" => {
                            if let Some(delta) = &event.delta {
                                if let Some(text) = &delta.text {
                                    let _ = tx.send(StreamChunk::TextDelta(text.clone()));
                                }
                                if let Some(json) = &delta.partial_json {
                                    let _ = tx.send(StreamChunk::ToolCallDelta(json.clone()));
                                }
                            }
                        }
                        "content_block_stop" => {
                            let _ = tx.send(StreamChunk::ToolCallEnd);
                        }
                        "message_delta" => {
                            if let Some(usage) = &event.usage {
                                let _ = tx.send(StreamChunk::Usage(Usage {
                                    input_tokens: usage.input_tokens,
                                    output_tokens: usage.output_tokens,
                                }));
                            }
                        }
                        "message_start" => {
                            if let Some(usage) = &event.usage {
                                let _ = tx.send(StreamChunk::Usage(Usage {
                                    input_tokens: usage.input_tokens,
                                    output_tokens: usage.output_tokens,
                                }));
                            }
                        }
                        "message_stop" => {
                            let _ = tx.send(StreamChunk::Done);
                        }
                        "error" => {
                            if let Some(err) = &event.error {
                                let _ = tx.send(StreamChunk::Error(err.message.clone()));
                            }
                        }
                        _ => {}
                    }
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_name() {
        let provider = AnthropicProvider::new("test-key".into());
        assert_eq!(provider.name(), "anthropic");
    }

    #[test]
    fn sse_event_deserialization() {
        let json = r#"{"type":"content_block_delta","delta":{"type":"text_delta","text":"Hello"}}"#;
        let event: SseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.event_type, "content_block_delta");
        assert_eq!(event.delta.as_ref().unwrap().text, Some("Hello".into()));
    }

    #[test]
    fn sse_tool_use_start() {
        let json = r#"{"type":"content_block_start","content_block":{"type":"tool_use","id":"toolu_123","name":"read_file"}}"#;
        let event: SseEvent = serde_json::from_str(json).unwrap();
        let block = event.content_block.unwrap();
        assert_eq!(block.block_type, "tool_use");
        assert_eq!(block.name, Some("read_file".into()));
    }

    #[test]
    fn sse_error_event() {
        let json = r#"{"type":"error","error":{"message":"rate limited"}}"#;
        let event: SseEvent = serde_json::from_str(json).unwrap();
        assert_eq!(event.error.unwrap().message, "rate limited");
    }
}
