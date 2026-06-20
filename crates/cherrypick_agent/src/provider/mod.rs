pub mod types;
pub mod anthropic;

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::error::Result;
use types::{CompletionRequest, StreamChunk};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    fn name(&self) -> &str;

    async fn stream_completion(
        &self,
        request: CompletionRequest,
        tx: mpsc::UnboundedSender<StreamChunk>,
    ) -> Result<()>;
}
