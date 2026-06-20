use std::path::Path;

use crate::provider::types::Message;

const DEFAULT_TOKEN_BUDGET: usize = 100_000;
const HOT_TIER_RATIO: f32 = 0.20;
const WARM_TIER_RATIO: f32 = 0.50;
const COLD_TIER_RATIO: f32 = 0.30;

pub struct ContextBudget {
    pub total: usize,
    pub hot: usize,
    pub warm: usize,
    pub cold: usize,
}

impl ContextBudget {
    pub fn new(total: usize) -> Self {
        let total_f = total as f32;
        Self {
            total,
            hot: (total_f * HOT_TIER_RATIO) as usize,
            warm: (total_f * WARM_TIER_RATIO) as usize,
            cold: (total_f * COLD_TIER_RATIO) as usize,
        }
    }
}

impl Default for ContextBudget {
    fn default() -> Self {
        Self::new(DEFAULT_TOKEN_BUDGET)
    }
}

pub struct ContextEngine {
    budget: ContextBudget,
    repo_map: Option<String>,
}

impl ContextEngine {
    pub fn new(budget: ContextBudget) -> Self {
        Self {
            budget,
            repo_map: None,
        }
    }

    pub fn build_repo_map(&mut self, repo_path: &Path) {
        let mut entries = Vec::new();
        collect_files(repo_path, repo_path, &mut entries, 200);
        self.repo_map = Some(entries.join("\n"));
    }

    pub fn repo_map(&self) -> Option<&str> {
        self.repo_map.as_deref()
    }

    pub fn estimate_tokens(text: &str) -> usize {
        text.len() / 4
    }

    pub fn truncate_history(
        messages: &[Message],
        max_tokens: usize,
    ) -> Vec<Message> {
        let mut total = 0;
        let mut result: Vec<Message> = Vec::new();

        for msg in messages.iter().rev() {
            let tokens = msg
                .content
                .iter()
                .map(|c| match c {
                    crate::provider::types::MessageContent::Text { text } => {
                        Self::estimate_tokens(text)
                    }
                    crate::provider::types::MessageContent::ToolResult { content, .. } => {
                        Self::estimate_tokens(content)
                    }
                    crate::provider::types::MessageContent::ToolUse { input, .. } => {
                        Self::estimate_tokens(&input.to_string())
                    }
                })
                .sum::<usize>();

            if total + tokens > max_tokens {
                break;
            }
            total += tokens;
            result.push(msg.clone());
        }

        result.reverse();
        result
    }

    pub fn budget(&self) -> &ContextBudget {
        &self.budget
    }
}

fn collect_files(
    dir: &Path,
    root: &Path,
    entries: &mut Vec<String>,
    max: usize,
) {
    if entries.len() >= max {
        return;
    }

    let read_dir = match std::fs::read_dir(dir) {
        Ok(d) => d,
        Err(_) => return,
    };

    for entry in read_dir {
        let entry = match entry {
            Ok(e) => e,
            Err(_) => continue,
        };

        let path = entry.path();
        let name = path.file_name().and_then(|n| n.to_str()).unwrap_or("");

        if name.starts_with('.') || name == "target" || name == "node_modules" {
            continue;
        }

        if path.is_dir() {
            collect_files(&path, root, entries, max);
        } else if path.is_file() {
            let rel = path.strip_prefix(root).unwrap_or(&path);
            entries.push(rel.to_string_lossy().to_string());
        }

        if entries.len() >= max {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::types::Message;

    #[test]
    fn budget_allocation() {
        let budget = ContextBudget::new(100_000);
        assert_eq!(budget.hot, 20_000);
        assert_eq!(budget.warm, 50_000);
        assert_eq!(budget.cold, 30_000);
        assert_eq!(budget.hot + budget.warm + budget.cold, budget.total);
    }

    #[test]
    fn estimate_tokens_rough() {
        let text = "Hello, world! This is a test.";
        let tokens = ContextEngine::estimate_tokens(text);
        assert!(tokens > 0);
        assert!(tokens < text.len());
    }

    #[test]
    fn truncate_history_respects_budget() {
        let messages: Vec<Message> = (0..100)
            .map(|i| Message::user(&format!("Message {i} with some content")))
            .collect();
        let truncated = ContextEngine::truncate_history(&messages, 100);
        assert!(truncated.len() < messages.len());
        assert!(!truncated.is_empty());
    }

    #[test]
    fn truncate_preserves_order() {
        let messages = vec![
            Message::user("first"),
            Message::user("second"),
            Message::user("third"),
        ];
        let truncated = ContextEngine::truncate_history(&messages, 10000);
        assert_eq!(truncated.len(), 3);
        assert_eq!(truncated[0].text_content(), Some("first"));
    }

    #[test]
    fn repo_map_collection() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("a.rs"), "fn main() {}").unwrap();
        std::fs::write(tmp.path().join("b.rs"), "fn test() {}").unwrap();

        let mut engine = ContextEngine::new(ContextBudget::default());
        engine.build_repo_map(tmp.path());
        let map = engine.repo_map().unwrap();
        assert!(map.contains("a.rs"));
        assert!(map.contains("b.rs"));
    }
}
