use std::collections::HashSet;

use async_trait::async_trait;
use goldclaw_core::{GoldClawError, Result, Tool, ToolDefinition, ToolInvocation, ToolOutput};
use regex::Regex;
use serde::Serialize;
use serde_json::json;
use tokio::process::Command;

use super::BuiltinTool;

// ── Security validator ──────────────────────────────────────────────────────

#[derive(Debug, Serialize)]
pub struct CheckResult {
    pub safe: bool,
    pub reason: Option<String>,
    pub warnings: Vec<String>,
}

pub struct CommandValidator {
    blacklist: Vec<Regex>,
    whitelist: HashSet<String>,
    sensitive_paths: Vec<Regex>,
}

impl CommandValidator {
    pub fn new() -> Self {
        Self {
            blacklist: build_blacklist(),
            whitelist: build_whitelist(),
            sensitive_paths: build_sensitive_paths(),
        }
    }

    pub fn validate(&self, cmd: &str) -> CheckResult {
        let cmd = cmd.trim();
        if cmd.is_empty() {
            return CheckResult {
                safe: false,
                reason: Some("empty command".into()),
                warnings: vec![],
            };
        }

        let lower = cmd.to_lowercase();
        for pattern in &self.blacklist {
            if pattern.is_match(&lower) {
                return CheckResult {
                    safe: false,
                    reason: Some(format!("blocked by security rule: {pattern}")),
                    warnings: vec![],
                };
            }
        }

        let base = match extract_base_command(cmd) {
            Some(b) => b,
            None => {
                return CheckResult {
                    safe: false,
                    reason: Some("could not parse command".into()),
                    warnings: vec![],
                };
            }
        };

        if !self.whitelist.contains(&base) {
            return CheckResult {
                safe: false,
                reason: Some(format!("command '{base}' is not in the allowed list")),
                warnings: vec![],
            };
        }

        let mut warnings = Vec::new();
        for pattern in &self.sensitive_paths {
            if pattern.is_match(cmd) {
                warnings.push(format!("references sensitive path: {pattern}"));
            }
        }
        warnings.extend(dangerous_pattern_warnings(cmd));

        CheckResult {
            safe: true,
            reason: None,
            warnings,
        }
    }
}

fn extract_base_command(cmd: &str) -> Option<String> {
    let cmd = cmd.trim().strip_prefix("sudo ").unwrap_or(cmd.trim());
    if cmd.starts_with(['|', '&', ';']) {
        return None;
    }
    let base = cmd.split_whitespace().next()?;
    if base.contains('/') {
        base.rsplit('/').next().map(str::to_string)
    } else {
        Some(base.to_string())
    }
}

fn dangerous_pattern_warnings(cmd: &str) -> Vec<String> {
    let mut w = Vec::new();
    if cmd.contains("&&") || cmd.contains("||") || cmd.contains(';') {
        w.push("shell operators present — verify each part".into());
    }
    if cmd.contains("$(") || cmd.contains('`') {
        w.push("command substitution present — verify no injection".into());
    }
    if cmd.contains('|') && !cmd.contains("||") {
        w.push("pipe present — verify piped command is safe".into());
    }
    if cmd.contains('>') {
        w.push("redirection present — verify target path is safe".into());
    }
    if cmd.contains('*') || cmd.contains('?') {
        w.push("wildcards present — verify scope is limited".into());
    }
    if cmd.contains('~') || cmd.starts_with('/') {
        w.push("absolute/home path — verify access scope".into());
    }
    w
}

fn build_blacklist() -> Vec<Regex> {
    let patterns = [
        r"^\s*rm\s+(-[rf]+\s+|.*\s+-[rf]+\s*)(/|\.\.|~|/home|/etc|/usr|/var|/root)",
        r"^\s*rm\s+(-[rf]+\s+)*\.\s*$",
        r">\s*/dev/sd[a-z]",
        r"mkfs\.",
        r"fdisk",
        r"dd\s+if=.*of=/dev/",
        r":\(\)\s*\{\s*:\|:&\s*\}\s*;:",
        r"sudo\s+rm",
        r"chmod\s+(777|a\+rwx|-R\s+777)",
        r">\s*/etc/passwd",
        r">\s*/etc/shadow",
        r">\s*/boot/",
        r"shutdown",
        r"reboot",
        r"init\s+[06]",
        r"halt",
        r"poweroff",
        r"kill\s+-9\s+-1",
        r"killall\s+-9",
        r"pkill\s+-9",
        r">\s*/proc/",
        r">\s*/sys/",
        r"nc\s+.*-l",
        r"nmap",
        r"masscan",
        r"curl\s+.*\|\s*bash",
        r"wget\s+.*\|\s*bash",
        r"curl\s+.*\|\s*sh",
        r"wget\s+.*\|\s*sh",
        r"eval\s+.*\$\(curl",
        r"eval\s+.*\$\(wget",
        r"base64\s+.*\|\s*bash",
        r"crontab\s+-r",
        r">\s*/etc/cron",
        r"userdel",
        r"useradd",
        r"passwd\s+",
        r">\s*~/.ssh/",
        r">\s*/root/.ssh/",
        r"mount\s+.*\s+/",
    ];
    patterns
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
}

fn build_whitelist() -> HashSet<String> {
    [
        "ls", "dir", "pwd", "whoami", "date", "uname", "hostname", "cat", "head", "tail",
        "less", "more", "wc", "sort", "uniq", "grep", "egrep", "fgrep", "sed", "awk", "cut",
        "tr", "column", "echo", "printf", "tee", "xargs", "find", "locate", "which", "whereis",
        "type", "mkdir", "touch", "cp", "mv", "rm", "chmod", "chown", "ln", "readlink",
        "realpath", "basename", "dirname", "tar", "gzip", "gunzip", "zip", "unzip", "xz",
        "unxz", "bzip2", "bunzip2", "diff", "cmp", "patch", "git", "svn", "hg", "curl", "wget",
        "ping", "nslookup", "dig", "host", "netstat", "ss", "ps", "top", "htop", "jobs", "bg",
        "fg", "kill", "pkill", "killall", "env", "export", "set", "unset", "printenv", "du",
        "df", "free", "uptime", "vmstat", "iostat", "npm", "yarn", "pnpm", "node", "npx",
        "cargo", "rustc", "rustup", "python", "python3", "pip", "pip3", "go", "gofmt", "java",
        "javac", "mvn", "gradle", "docker", "docker-compose", "make", "cmake", "jq", "yq",
        "ag", "rg", "fd", "fzf", "bat", "eza", "lsd", "tree", "stat", "file", "md5sum",
        "sha256sum", "sha512sum", "timeout", "parallel", "rsync", "ssh", "scp",
    ]
    .iter()
    .map(|s| s.to_string())
    .collect()
}

fn build_sensitive_paths() -> Vec<Regex> {
    let patterns = [
        r"^/etc/shadow",
        r"^/etc/gshadow",
        r"^/etc/sudoers",
        r"^/root/",
        r"^/boot/",
        r"^/dev/(sd|hd|nvme)[a-z]",
        r"~/\.ssh/",
        r"\.pem$",
        r"\.key$",
        r"id_rsa",
        r"id_ed25519",
        r"_history$",
    ];
    patterns
        .iter()
        .filter_map(|p| Regex::new(p).ok())
        .collect()
}

// ── BashCheckTool ───────────────────────────────────────────────────────────

pub struct BashCheckTool {
    validator: CommandValidator,
}

impl BashCheckTool {
    pub fn new() -> Self {
        Self {
            validator: CommandValidator::new(),
        }
    }
}

#[derive(serde::Deserialize)]
struct CheckArgs {
    command: String,
}

#[async_trait]
impl Tool for BashCheckTool {
    fn name(&self) -> &'static str {
        "bash_check"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: CheckArgs = serde_json::from_value(invocation.args.clone())?;
        let result = self.validator.validate(&args.command);
        let payload = json!({
            "safe": result.safe,
            "reason": result.reason,
            "warnings": result.warnings,
        });
        Ok(ToolOutput {
            summary: format!(
                "bash_check: {} — {}",
                if result.safe { "safe" } else { "blocked" },
                args.command.chars().take(60).collect::<String>()
            ),
            content: payload.to_string(),
        })
    }
}

#[async_trait]
impl BuiltinTool for BashCheckTool {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().into(),
            description: "检查一条 bash 命令是否安全（dry run，不实际执行）。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要检查的 bash 命令"
                    }
                },
                "required": ["command"]
            }),
        }
    }
}

// ── BashExecTool ────────────────────────────────────────────────────────────

pub struct BashExecTool {
    validator: CommandValidator,
}

impl BashExecTool {
    pub fn new() -> Self {
        Self {
            validator: CommandValidator::new(),
        }
    }
}

#[derive(serde::Deserialize)]
struct ExecArgs {
    command: String,
    #[serde(default = "default_timeout")]
    timeout_secs: u64,
    cwd: Option<String>,
}

fn default_timeout() -> u64 {
    30
}

#[async_trait]
impl Tool for BashExecTool {
    fn name(&self) -> &'static str {
        "bash_exec"
    }

    async fn execute(&self, invocation: &ToolInvocation) -> Result<ToolOutput> {
        let args: ExecArgs = serde_json::from_value(invocation.args.clone())?;
        let timeout_secs = args.timeout_secs.min(300);

        let check = self.validator.validate(&args.command);
        if !check.safe {
            let reason = check
                .reason
                .unwrap_or_else(|| "command blocked by security policy".into());
            return Err(GoldClawError::Unauthorized(reason));
        }

        let mut cmd = Command::new("bash");
        cmd.arg("-c").arg(&args.command);
        if let Some(cwd) = &args.cwd {
            cmd.current_dir(cwd);
        }

        let output = tokio::time::timeout(
            std::time::Duration::from_secs(timeout_secs),
            cmd.output(),
        )
        .await
        .map_err(|_| {
            GoldClawError::Internal(format!(
                "command timed out after {timeout_secs}s"
            ))
        })?
        .map_err(|error| GoldClawError::Io(format!("failed to spawn bash: {error}")))?;

        let stdout = String::from_utf8_lossy(&output.stdout).into_owned();
        let stderr = String::from_utf8_lossy(&output.stderr).into_owned();
        let exit_code = output.status.code().unwrap_or(-1);
        let success = output.status.success();

        let payload = json!({
            "success": success,
            "stdout": stdout,
            "stderr": stderr,
            "exit_code": exit_code,
            "warnings": check.warnings,
        });

        Ok(ToolOutput {
            summary: format!(
                "bash_exec exit={exit_code}: {}",
                args.command.chars().take(60).collect::<String>()
            ),
            content: payload.to_string(),
        })
    }
}

#[async_trait]
impl BuiltinTool for BashExecTool {
    fn tool_definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: self.name().into(),
            description: "执行安全的 bash 命令（带黑名单保护）。命令必须在白名单内且不触发安全规则。".into(),
            parameters: json!({
                "type": "object",
                "properties": {
                    "command": {
                        "type": "string",
                        "description": "要执行的 bash 命令"
                    },
                    "timeout_secs": {
                        "type": "integer",
                        "description": "超时秒数（默认 30，最大 300）"
                    },
                    "cwd": {
                        "type": "string",
                        "description": "工作目录（默认：当前目录）"
                    }
                },
                "required": ["command"]
            }),
        }
    }
}
