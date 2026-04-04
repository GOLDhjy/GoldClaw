use std::{fs, path::PathBuf};

use async_trait::async_trait;
use goldclaw_core::{GoldClawError, Result, Tool, ToolDefinition, ToolInvocation, ToolOutput};
use serde::Deserialize;
use serde_json::json;

use super::BuiltinTool;

pub struct UpdateSoulTool {
    soul_path: PathBuf,
}

impl UpdateSoulTool {
    pub fn new(soul_path: PathBuf) -> Self {
        Self { soul_path }
    }
}

#[derive(Deserialize)]
struct UpdateSoulArgs {
    content: String,
}

fn normalize_soul_content(content: &str) -> Result<String> {
    let trimmed = content.trim();
    if trimmed.is_empty() {
        return Err(GoldClawError::InvalidInput(
            "soul content cannot be empty".into(),
        ));
    }
    let mut normalized = trimmed.to_string();
    normalized.push('\n');
    Ok(normalized)
}

#[async_trait]
impl Tool for UpdateSoulTool {
    fn name(&self) -> &'static str {
        "update_soul"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: UpdateSoulArgs = serde_json::from_value(invocation.args.clone())?;
        let content = normalize_soul_content(&args.content)?;

        if let Some(parent) = self.soul_path.parent() {
            fs::create_dir_all(parent)?;
        }
        fs::write(&self.soul_path, content.as_bytes()).map_err(|error| {
            GoldClawError::Io(format!(
                "failed to write `{}`: {error}",
                self.soul_path.display()
            ))
        })?;

        Ok(ToolOutput {
            summary: format!("updated {}", self.soul_path.display()),
            content,
        })
    }
}

#[async_trait]
impl BuiltinTool for UpdateSoulTool {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().into(),
            description: "永久修改助手的人设文件（soul.md）。当用户要求修改名字、称呼、回复语言、对话风格或任何长期规则时必须调用。content 必须是完整的 soul.md 全文，不能只传片段。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "content": {
                        "type": "string",
                        "description": "新的 soul.md 完整内容（必须包含所有原有字段）"
                    }
                },
                "required": ["content"]
            }),
        }
    }
}
