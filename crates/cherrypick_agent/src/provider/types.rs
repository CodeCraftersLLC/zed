use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Role {
    #[serde(rename = "user")]
    User,
    #[serde(rename = "assistant")]
    Assistant,
    #[serde(rename = "system")]
    System,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum MessageContent {
    #[serde(rename = "text")]
    Text { text: String },
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    #[serde(rename = "tool_result")]
    ToolResult {
        tool_use_id: String,
        content: String,
        is_error: bool,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: Role,
    pub content: Vec<MessageContent>,
}

impl Message {
    pub fn user(text: &str) -> Self {
        Self {
            role: Role::User,
            content: vec![MessageContent::Text {
                text: text.to_string(),
            }],
        }
    }

    pub fn assistant(text: &str) -> Self {
        Self {
            role: Role::Assistant,
            content: vec![MessageContent::Text {
                text: text.to_string(),
            }],
        }
    }

    pub fn tool_result(tool_use_id: &str, content: &str, is_error: bool) -> Self {
        Self {
            role: Role::User,
            content: vec![MessageContent::ToolResult {
                tool_use_id: tool_use_id.to_string(),
                content: content.to_string(),
                is_error,
            }],
        }
    }

    pub fn text_content(&self) -> Option<&str> {
        self.content.iter().find_map(|c| match c {
            MessageContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub input_schema: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: serde_json::Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CompletionRequest {
    pub model: String,
    pub messages: Vec<Message>,
    pub system: Option<String>,
    pub tools: Vec<ToolDefinition>,
    pub max_tokens: u32,
    pub temperature: Option<f32>,
}

#[derive(Debug, Clone)]
pub enum StreamChunk {
    TextDelta(String),
    ToolCallStart {
        id: String,
        name: String,
    },
    ToolCallDelta(String),
    ToolCallEnd,
    Usage(Usage),
    Done,
    Error(String),
}

#[derive(Debug, Clone, Default)]
pub struct Usage {
    pub input_tokens: u32,
    pub output_tokens: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub enum RiskLevel {
    ReadOnly,
    WriteLocal,
    WriteRemote,
    Network,
    Destructive,
}

impl RiskLevel {
    pub fn requires_confirmation(&self) -> bool {
        *self >= RiskLevel::WriteLocal
    }

    pub fn label(&self) -> &'static str {
        match self {
            Self::ReadOnly => "Read-only",
            Self::WriteLocal => "Write (local)",
            Self::WriteRemote => "Write (remote)",
            Self::Network => "Network",
            Self::Destructive => "Destructive",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn user_message_creation() {
        let msg = Message::user("hello");
        assert_eq!(msg.role, Role::User);
        assert_eq!(msg.text_content(), Some("hello"));
    }

    #[test]
    fn assistant_message_creation() {
        let msg = Message::assistant("response");
        assert_eq!(msg.role, Role::Assistant);
        assert_eq!(msg.text_content(), Some("response"));
    }

    #[test]
    fn tool_result_message() {
        let msg = Message::tool_result("id-1", "output", false);
        assert_eq!(msg.role, Role::User);
        match &msg.content[0] {
            MessageContent::ToolResult {
                tool_use_id,
                is_error,
                ..
            } => {
                assert_eq!(tool_use_id, "id-1");
                assert!(!is_error);
            }
            _ => panic!("Expected ToolResult"),
        }
    }

    #[test]
    fn risk_level_ordering() {
        assert!(RiskLevel::ReadOnly < RiskLevel::WriteLocal);
        assert!(RiskLevel::WriteLocal < RiskLevel::Destructive);
    }

    #[test]
    fn risk_requires_confirmation() {
        assert!(!RiskLevel::ReadOnly.requires_confirmation());
        assert!(RiskLevel::WriteLocal.requires_confirmation());
        assert!(RiskLevel::Destructive.requires_confirmation());
    }

    #[test]
    fn message_serialization() {
        let msg = Message::user("test");
        let json = serde_json::to_string(&msg).unwrap();
        let deserialized: Message = serde_json::from_str(&json).unwrap();
        assert_eq!(deserialized.text_content(), Some("test"));
    }
}
