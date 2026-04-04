mod read_file;
mod update_soul;

pub use read_file::ReadWorkspaceTool;
pub use update_soul::UpdateSoulTool;

use async_trait::async_trait;
use goldclaw_core::{Tool, ToolDefinition};

/// A tool that lives inside the runtime and exposes itself to the LLM via the
/// native function-calling API (OpenAI-compatible `tools` parameter).
///
/// `BuiltinTool` extends `Tool` so callers can still invoke `.execute(&invocation)`
/// directly (used in tests and by `execute_tool` in the runtime).
#[async_trait]
pub trait BuiltinTool: Tool {
    /// Returns the JSON schema definition the LLM receives via the `tools` parameter.
    fn tool_definition(&self) -> ToolDefinition;
}
