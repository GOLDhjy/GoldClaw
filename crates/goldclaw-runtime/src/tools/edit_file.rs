use std::{fs, path::PathBuf};

use async_trait::async_trait;
use goldclaw_core::{GoldClawError, Result, Tool, ToolDefinition, ToolInvocation, ToolOutput};
use serde::Deserialize;
use serde_json::json;

use super::BuiltinTool;

pub struct EditFileTool {
    roots: Vec<PathBuf>,
    max_bytes: u64,
}

impl EditFileTool {
    pub fn new(roots: Vec<PathBuf>) -> Self {
        Self {
            roots,
            max_bytes: 512 * 1024,
        }
    }

    fn resolve_path(&self, path: &str) -> Result<PathBuf> {
        let requested = std::path::PathBuf::from(path);
        if requested.is_absolute() {
            return Ok(requested);
        }
        let base = self
            .roots
            .first()
            .ok_or_else(|| GoldClawError::InvalidInput("no edit roots configured".into()))?;
        Ok(base.join(requested))
    }
}

#[derive(Deserialize)]
struct EditArgs {
    path: String,
    old_string: String,
    new_string: String,
}

#[async_trait]
impl Tool for EditFileTool {
    fn name(&self) -> &'static str {
        "edit_file"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: EditArgs = serde_json::from_value(invocation.args.clone())?;
        let path = self.resolve_path(&args.path)?;

        let metadata = fs::metadata(&path).map_err(|error| {
            GoldClawError::Io(format!(
                "cannot stat `{}`: {error}",
                path.display()
            ))
        })?;
        if metadata.len() > self.max_bytes {
            return Err(GoldClawError::InvalidInput(format!(
                "`{}` exceeds the {} byte limit",
                path.display(),
                self.max_bytes
            )));
        }

        let original = fs::read_to_string(&path).map_err(|error| {
            GoldClawError::Io(format!("failed to read `{}`: {error}", path.display()))
        })?;

        let count = original.matches(args.old_string.as_str()).count();
        if count == 0 {
            return Err(GoldClawError::InvalidInput(format!(
                "old_string not found in `{}`",
                path.display()
            )));
        }
        if count > 1 {
            return Err(GoldClawError::InvalidInput(format!(
                "old_string matches {count} times in `{}` — must be unique",
                path.display()
            )));
        }

        let updated = original.replacen(args.old_string.as_str(), &args.new_string, 1);
        fs::write(&path, updated.as_bytes()).map_err(|error| {
            GoldClawError::Io(format!("failed to write `{}`: {error}", path.display()))
        })?;

        Ok(ToolOutput {
            summary: format!("edited {}", path.display()),
            content: json!({ "success": true, "replacements": 1 }).to_string(),
        })
    }
}

#[async_trait]
impl BuiltinTool for EditFileTool {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().into(),
            description: "通过替换文件内唯一字符串来编辑文件。old_string 必须在文件中唯一出现一次。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "path": {
                        "type": "string",
                        "description": "目标文件路径（相对于工作区根目录或绝对路径）"
                    },
                    "old_string": {
                        "type": "string",
                        "description": "要查找的文本（必须在文件中唯一出现一次）"
                    },
                    "new_string": {
                        "type": "string",
                        "description": "替换后的文本"
                    }
                },
                "required": ["path", "old_string", "new_string"]
            }),
        }
    }
}
