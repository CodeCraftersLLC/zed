use std::collections::HashMap;
use std::path::Path;

use crate::error::{AgentError, Result};

#[derive(Debug, Clone)]
pub struct Skill {
    pub name: String,
    pub description: String,
    pub trigger: Option<String>,
    pub allowed_tools: Vec<String>,
    pub prompt_template: String,
}

pub struct SkillLoader {
    skills: HashMap<String, Skill>,
    builtin: HashMap<String, Skill>,
}

impl SkillLoader {
    pub fn new() -> Self {
        let mut loader = Self {
            skills: HashMap::new(),
            builtin: HashMap::new(),
        };
        loader.register_builtins();
        loader
    }

    fn register_builtins(&mut self) {
        self.builtin.insert(
            "explain-commit".into(),
            Skill {
                name: "explain-commit".into(),
                description: "Explain what a commit does".into(),
                trigger: Some("/explain".into()),
                allowed_tools: vec!["git_log".into(), "git_diff".into(), "read_file".into()],
                prompt_template: "Explain the most recent commit in this repository. Use git_log to find it, then git_diff to see the changes, and explain what was done and why.".into(),
            },
        );

        self.builtin.insert(
            "review-changes".into(),
            Skill {
                name: "review-changes".into(),
                description: "Review current staged/unstaged changes".into(),
                trigger: Some("/review".into()),
                allowed_tools: vec!["git_status".into(), "git_diff".into(), "read_file".into()],
                prompt_template: "Review the current changes in this repository. Check git_status, then review the diffs. Look for bugs, style issues, and potential improvements.".into(),
            },
        );

        self.builtin.insert(
            "generate-commit-message".into(),
            Skill {
                name: "generate-commit-message".into(),
                description: "Generate a commit message for staged changes".into(),
                trigger: Some("/commit-msg".into()),
                allowed_tools: vec!["git_status".into(), "git_diff".into()],
                prompt_template: "Generate a concise, conventional commit message for the currently staged changes. Use git_diff with staged=true to see what's staged.".into(),
            },
        );

        self.builtin.insert(
            "find-related".into(),
            Skill {
                name: "find-related".into(),
                description: "Find files related to a topic".into(),
                trigger: Some("/find".into()),
                allowed_tools: vec!["search_code".into(), "list_files".into(), "read_file".into()],
                prompt_template: "Find all files related to the user's query. Use search_code and list_files to locate relevant code.".into(),
            },
        );
    }

    pub fn load_from_directory(&mut self, dir: &Path) -> Result<usize> {
        let mut count = 0;
        let entries = std::fs::read_dir(dir).map_err(|e| AgentError::Io(e))?;

        for entry in entries {
            let entry = entry.map_err(|e| AgentError::Io(e))?;
            let path = entry.path();

            if path.extension().and_then(|e| e.to_str()) == Some("md") {
                if let Ok(skill) = parse_skill_file(&path) {
                    self.skills.insert(skill.name.clone(), skill);
                    count += 1;
                }
            }
        }

        Ok(count)
    }

    pub fn get(&self, name: &str) -> Option<&Skill> {
        self.skills.get(name).or_else(|| self.builtin.get(name))
    }

    pub fn resolve_slash_command(&self, input: &str) -> Option<&Skill> {
        let trigger = input.split_whitespace().next()?;
        self.builtin
            .values()
            .chain(self.skills.values())
            .find(|s| s.trigger.as_deref() == Some(trigger))
    }

    pub fn list_all(&self) -> Vec<&Skill> {
        let mut all: Vec<&Skill> = self.builtin.values().chain(self.skills.values()).collect();
        all.sort_by_key(|s| &s.name);
        all
    }
}

fn parse_skill_file(path: &Path) -> Result<Skill> {
    let content = std::fs::read_to_string(path)?;

    let (frontmatter, body) = if content.starts_with("---") {
        let rest = &content[3..];
        if let Some(end) = rest.find("---") {
            let fm = &rest[..end].trim();
            let body = &rest[end + 3..].trim();
            (Some(fm.to_string()), body.to_string())
        } else {
            (None, content)
        }
    } else {
        (None, content)
    };

    let mut name = path
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("unknown")
        .to_string();
    let mut description = String::new();
    let mut trigger = None;
    let mut allowed_tools = Vec::new();

    if let Some(fm) = frontmatter {
        for line in fm.lines() {
            let line = line.trim();
            if let Some(val) = line.strip_prefix("name:") {
                name = val.trim().trim_matches('"').to_string();
            } else if let Some(val) = line.strip_prefix("description:") {
                description = val.trim().trim_matches('"').to_string();
            } else if let Some(val) = line.strip_prefix("trigger:") {
                trigger = Some(val.trim().trim_matches('"').to_string());
            } else if let Some(val) = line.strip_prefix("tools:") {
                allowed_tools = val
                    .trim()
                    .trim_start_matches('[')
                    .trim_end_matches(']')
                    .split(',')
                    .map(|s| s.trim().trim_matches('"').to_string())
                    .filter(|s| !s.is_empty())
                    .collect();
            }
        }
    }

    Ok(Skill {
        name,
        description,
        trigger,
        allowed_tools,
        prompt_template: body,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_skills_loaded() {
        let loader = SkillLoader::new();
        assert!(loader.get("explain-commit").is_some());
        assert!(loader.get("review-changes").is_some());
        assert!(loader.get("generate-commit-message").is_some());
        assert!(loader.get("find-related").is_some());
    }

    #[test]
    fn slash_command_resolution() {
        let loader = SkillLoader::new();
        let skill = loader.resolve_slash_command("/explain something");
        assert!(skill.is_some());
        assert_eq!(skill.unwrap().name, "explain-commit");
    }

    #[test]
    fn unknown_slash_command() {
        let loader = SkillLoader::new();
        assert!(loader.resolve_slash_command("/unknown").is_none());
    }

    #[test]
    fn list_all_skills() {
        let loader = SkillLoader::new();
        let all = loader.list_all();
        assert!(all.len() >= 4);
    }

    #[test]
    fn parse_skill_file_from_disk() {
        let tmp = tempfile::tempdir().unwrap();
        let skill_path = tmp.path().join("test-skill.md");
        std::fs::write(
            &skill_path,
            r#"---
name: test-skill
description: A test skill
trigger: /test
tools: [read_file, git_status]
---
Do something useful."#,
        )
        .unwrap();

        let skill = parse_skill_file(&skill_path).unwrap();
        assert_eq!(skill.name, "test-skill");
        assert_eq!(skill.trigger, Some("/test".into()));
        assert_eq!(skill.allowed_tools.len(), 2);
        assert!(skill.prompt_template.contains("something useful"));
    }

    #[test]
    fn load_skills_from_directory() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(
            tmp.path().join("skill-a.md"),
            "---\nname: a\n---\nSkill A",
        )
        .unwrap();
        std::fs::write(
            tmp.path().join("skill-b.md"),
            "---\nname: b\n---\nSkill B",
        )
        .unwrap();

        let mut loader = SkillLoader::new();
        let count = loader.load_from_directory(tmp.path()).unwrap();
        assert_eq!(count, 2);
        assert!(loader.get("a").is_some());
        assert!(loader.get("b").is_some());
    }
}
