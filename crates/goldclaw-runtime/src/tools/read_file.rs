use std::{
    fs,
    path::{Path, PathBuf},
};

use async_trait::async_trait;
use goldclaw_core::{GoldClawError, Result, Tool, ToolDefinition, ToolInvocation, ToolOutput};
use serde::Deserialize;
use serde_json::json;

use super::BuiltinTool;

pub struct ReadWorkspaceTool {
    roots: Vec<PathBuf>,
    max_bytes: u64,
}

impl ReadWorkspaceTool {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            max_bytes: 64 * 1024,
        }
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf> {
        let requested = PathBuf::from(path);
        let candidate = if requested.is_absolute() {
            requested
        } else {
            let base = self
                .roots
                .first()
                .ok_or_else(|| GoldClawError::InvalidInput("no read roots configured".into()))?;
            base.join(requested)
        };

        fs::canonicalize(&candidate).map_err(|error| {
            GoldClawError::Io(format!(
                "failed to resolve `{}`: {error}",
                candidate.display()
            ))
        })
    }

    fn read_text_file(&self, path: &Path) -> Result<String> {
        let metadata = fs::metadata(path)?;
        if !metadata.is_file() {
            return Err(GoldClawError::InvalidInput(format!(
                "`{}` is not a regular file",
                path.display()
            )));
        }
        if metadata.len() > self.max_bytes {
            return Err(GoldClawError::InvalidInput(format!(
                "`{}` exceeds the {} byte limit",
                path.display(),
                self.max_bytes
            )));
        }
        fs::read_to_string(path).map_err(|error| {
            GoldClawError::Io(format!("failed to read `{}`: {error}", path.display()))
        })
    }
}

#[derive(Deserialize)]
struct ReadArgs {
    path: String,
}

#[async_trait]
impl Tool for ReadWorkspaceTool {
    fn name(&self) -> &'static str {
        "read_file"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: ReadArgs = serde_json::from_value(invocation.args.clone())?;
        let path = self.resolve_path(&args.path)?;
        let content = self.read_text_file(&path)?;
        Ok(ToolOutput {
            summary: format!("read {}", path.display()),
            content,
        })
    }
}

#[async_trait]
impl BuiltinTool for ReadWorkspaceTool {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().into(),
            description: "读取工作区内的 UTF-8 文本文件。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "相对于工作区根目录的文件路径（或绝对路径）"
                    }
                },
                "required": ["path"]
            }),
        }
    }
}
