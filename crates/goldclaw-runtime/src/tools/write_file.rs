use std::{fs, path::PathBuf};

use async_trait::async_trait;
use goldclaw_core::{GoldClawError, Result, Tool, ToolDefinition, ToolInvocation, ToolOutput};
use serde::Deserialize;
use serde_json::json;

use super::BuiltinTool;

pub struct WriteFileTool {
    roots: Vec<PathBuf>,
}

impl WriteFileTool {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self { roots }
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf> {
        let requested = PathBuf::from(path);
        if requested.is_absolute() {
            return Ok(requested);
        }
        let base = self
            .roots
            .first()
            .ok_or_else(|| GoldClawError::InvalidInput("no write roots configured".into()))?;
        Ok(base.join(requested))
    }
}

#[derive(Deserialize)]
struct WriteArgs {
    path: String,
    content: String,
}

#[async_trait]
impl Tool for WriteFileTool {
    fn name(&self) -> &'static str {
        "write_file"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: WriteArgs = serde_json::from_value(invocation.args.clone())?;
        let path = self.resolve_path(&args.path)?;

        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).map_err(|error| {
                GoldClawError::Io(format!(
                    "failed to create directories for `{}`: {error}",
                    path.display()
                ))
            })?;
        }

        let bytes = args.content.as_bytes();
        fs::write(&path, bytes).map_err(|error| {
            GoldClawError::Io(format!("failed to write `{}`: {error}", path.display()))
        })?;

        Ok(ToolOutput {
            summary: format!("wrote {} bytes to {}", bytes.len(), path.display()),
            content: json!({ "bytes_written": bytes.len(), "success": true }).to_string(),
        })
    }
}

#[async_trait]
impl BuiltinTool for WriteFileTool {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().into(),
            description: "将内容写入文件（若父目录不存在则自动创建）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "目标文件路径（相对于工作区根目录或绝对路径）"
                    },
                    "content": {
                        "type": "string",
                        "description": "要写入的文本内容（覆盖原文件）"
                    }
                },
                "required": ["path", "content"]
            }),
        }
    }
}
