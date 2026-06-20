use std::collections::HashMap;
use std::process::Stdio;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::process::{Child, Command};
use tokio::time::{timeout, Duration};

use crate::error::{AgentError, Result};
use crate::provider::types::ToolDefinition;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_MESSAGE_SIZE: usize = 1_048_576;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct McpClientConfig {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: HashMap<String, String>,
}

#[derive(Serialize)]
struct JsonRpcRequest {
    jsonrpc: String,
    id: u64,
    method: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    params: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct JsonRpcResponse {
    id: u64,
    #[serde(default)]
    result: Option<serde_json::Value>,
    #[serde(default)]
    error: Option<JsonRpcError>,
}

#[derive(Deserialize)]
struct JsonRpcError {
    code: i64,
    message: String,
}

pub struct McpClient {
    config: McpClientConfig,
    process: Option<Child>,
    next_id: u64,
    discovered_tools: Vec<ToolDefinition>,
}

impl McpClient {
    pub fn new(config: McpClientConfig) -> Self {
        Self {
            config,
            process: None,
            next_id: 1,
            discovered_tools: Vec::new(),
        }
    }

    pub async fn start(&mut self) -> Result<()> {
        let mut cmd = Command::new(&self.config.command);
        cmd.args(&self.config.args)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());

        for (k, v) in &self.config.env {
            cmd.env(k, v);
        }

        let child = cmd.spawn().map_err(|e| {
            AgentError::Mcp(format!(
                "Failed to start MCP server '{}': {}",
                self.config.name, e
            ))
        })?;

        self.process = Some(child);
        self.discover_tools().await?;
        Ok(())
    }

    async fn discover_tools(&mut self) -> Result<()> {
        let response = self.send_request("tools/list", None).await?;

        if let Some(result) = response {
            if let Some(tools) = result.get("tools").and_then(|t| t.as_array()) {
                self.discovered_tools = tools
                    .iter()
                    .filter_map(|t| {
                        Some(ToolDefinition {
                            name: format!(
                                "mcp_{}__{}",
                                self.config.name,
                                t.get("name")?.as_str()?
                            ),
                            description: t
                                .get("description")
                                .and_then(|d| d.as_str())
                                .unwrap_or("")
                                .to_string(),
                            input_schema: t
                                .get("inputSchema")
                                .cloned()
                                .unwrap_or(serde_json::json!({"type": "object"})),
                        })
                    })
                    .collect();
            }
        }

        Ok(())
    }

    pub async fn call_tool(
        &mut self,
        tool_name: &str,
        arguments: serde_json::Value,
    ) -> Result<String> {
        let params = serde_json::json!({
            "name": tool_name,
            "arguments": arguments,
        });

        let response = self.send_request("tools/call", Some(params)).await?;

        match response {
            Some(result) => {
                if let Some(content) = result.get("content").and_then(|c| c.as_array()) {
                    let text_parts: Vec<&str> = content
                        .iter()
                        .filter_map(|c| c.get("text").and_then(|t| t.as_str()))
                        .collect();
                    Ok(text_parts.join("\n"))
                } else {
                    Ok(result.to_string())
                }
            }
            None => Ok(String::new()),
        }
    }

    async fn send_request(
        &mut self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<Option<serde_json::Value>> {
        let process = self
            .process
            .as_mut()
            .ok_or_else(|| AgentError::Mcp("MCP server not running".into()))?;

        let id = self.next_id;
        self.next_id += 1;

        let request = JsonRpcRequest {
            jsonrpc: "2.0".to_string(),
            id,
            method: method.to_string(),
            params,
        };

        let mut request_bytes = serde_json::to_vec(&request)?;
        request_bytes.push(b'\n');

        if request_bytes.len() > MAX_MESSAGE_SIZE {
            return Err(AgentError::Mcp("Request too large".into()));
        }

        let stdin = process
            .stdin
            .as_mut()
            .ok_or_else(|| AgentError::Mcp("No stdin".into()))?;
        stdin.write_all(&request_bytes).await?;
        stdin.flush().await?;

        let stdout = process
            .stdout
            .as_mut()
            .ok_or_else(|| AgentError::Mcp("No stdout".into()))?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();

        let read_result = timeout(REQUEST_TIMEOUT, reader.read_line(&mut line)).await;

        match read_result {
            Ok(Ok(0)) => Err(AgentError::Mcp("Server closed connection".into())),
            Ok(Ok(_)) => {
                if line.len() > MAX_MESSAGE_SIZE {
                    return Err(AgentError::Mcp("Response too large".into()));
                }
                let response: JsonRpcResponse = serde_json::from_str(&line)
                    .map_err(|e| AgentError::Mcp(format!("Invalid response: {e}")))?;
                if let Some(error) = response.error {
                    Err(AgentError::Mcp(format!(
                        "RPC error {}: {}",
                        error.code, error.message
                    )))
                } else {
                    Ok(response.result)
                }
            }
            Ok(Err(e)) => Err(AgentError::Mcp(format!("Read error: {e}"))),
            Err(_) => Err(AgentError::Mcp("Request timed out".into())),
        }
    }

    pub fn tools(&self) -> &[ToolDefinition] {
        &self.discovered_tools
    }

    pub async fn stop(&mut self) -> Result<()> {
        if let Some(ref mut process) = self.process {
            let _ = process.kill().await;
            self.process = None;
        }
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.process.is_some()
    }

    pub fn name(&self) -> &str {
        &self.config.name
    }
}

impl Drop for McpClient {
    fn drop(&mut self) {
        if let Some(ref mut process) = self.process {
            let _ = process.start_kill();
        }
    }
}

pub struct McpManager {
    clients: HashMap<String, McpClient>,
}

impl McpManager {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    pub fn add_server(&mut self, config: McpClientConfig) {
        let name = config.name.clone();
        self.clients.insert(name, McpClient::new(config));
    }

    pub async fn start_all(&mut self) -> Vec<(String, Result<()>)> {
        let mut results = Vec::new();
        let names: Vec<String> = self.clients.keys().cloned().collect();
        for name in names {
            if let Some(client) = self.clients.get_mut(&name) {
                let result = client.start().await;
                results.push((name, result));
            }
        }
        results
    }

    pub async fn stop_all(&mut self) {
        for client in self.clients.values_mut() {
            let _ = client.stop().await;
        }
    }

    pub fn all_tools(&self) -> Vec<ToolDefinition> {
        self.clients
            .values()
            .flat_map(|c| c.tools().iter().cloned())
            .collect()
    }

    pub fn get_client(&mut self, name: &str) -> Option<&mut McpClient> {
        self.clients.get_mut(name)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_manager_creation() {
        let mgr = McpManager::new();
        assert!(mgr.all_tools().is_empty());
    }

    #[test]
    fn add_server_config() {
        let mut mgr = McpManager::new();
        mgr.add_server(McpClientConfig {
            name: "test".into(),
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
        });
        assert!(mgr.get_client("test").is_some());
    }

    #[test]
    fn client_not_running_initially() {
        let client = McpClient::new(McpClientConfig {
            name: "test".into(),
            command: "echo".into(),
            args: vec![],
            env: HashMap::new(),
        });
        assert!(!client.is_running());
        assert_eq!(client.name(), "test");
    }

    #[test]
    fn json_rpc_request_serialization() {
        let req = JsonRpcRequest {
            jsonrpc: "2.0".into(),
            id: 1,
            method: "tools/list".into(),
            params: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        assert!(json.contains("tools/list"));
        assert!(!json.contains("params"));
    }
}
