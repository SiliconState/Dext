use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{CacheControl, OaiFunctionDef, OaiTool, WireTool};

#[derive(Serialize, Clone)]
pub(crate) struct Tool {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
    pub(crate) input_schema: Value,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RuntimeTool {
    pub(crate) name: &'static str,
    pub(crate) description: &'static str,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

pub(crate) fn runtime_tool_definitions() -> Vec<RuntimeTool> {
    vec![RuntimeTool {
        name: "subagent-runtime",
        description: "Run a detached subagent runtime from an input file into one output bundle.",
    }]
}

pub(crate) fn slash_command_definitions() -> Vec<SlashCommand> {
    vec![
        SlashCommand {
            name: "subagent",
            usage: "/subagent <task> [--tools t1,t2] [--max-iter N] [--system PROMPT] [--readonly] [--inline|--detached]",
            description: "Run a user-requested subagent while keeping delegation out of provider-visible tools.",
        },
        SlashCommand {
            name: "browser",
            usage: "/browser [off|agent-browser|status]",
            description: "Enable/status optional agent-browser automation for heavy web/HTML work.",
        },
        SlashCommand {
            name: "tools",
            usage: "/tools [default|full|status]",
            description: "Show or switch the provider-visible tool count profile.",
        },
        SlashCommand {
            name: "pack",
            usage: "/pack [list|inspect <name>|run <name> <task>]",
            description: "Discover and invoke source-first Dext packs without provider-visible tools.",
        },
        SlashCommand {
            name: "shelves",
            usage: "/shelves",
            description: "List typed shelf manifests and provider-neutral ability metadata.",
        },
    ]
}
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub(crate) enum ToolProfile {
    Full,
    #[default]
    Lean,
}

impl ToolProfile {
    pub(crate) fn from_env() -> Self {
        std::env::var("DEXT_TOOL_PROFILE")
            .ok()
            .and_then(|v| Self::parse(&v))
            .unwrap_or_default()
    }

    pub(crate) fn parse(raw: &str) -> Option<Self> {
        match raw.trim().to_ascii_lowercase().as_str() {
            "full" | "all" => Some(Self::Full),
            "lean" | "default" | "slim" | "minimal" | "min" | "frugal" => Some(Self::Lean),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Full => "full",
            Self::Lean => "lean",
        }
    }
}

pub(crate) fn provider_tool_definitions() -> Vec<Tool> {
    vec![
        Tool {
            name: "read_file",
            description: "Read a file from disk. Output is line-numbered (1-indexed) and capped for safety. Explicit offset+limit reads get a larger cap and are cached for overlapping follow-up reads; still prefer rg first and focused windows.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or relative file path"},
                    "offset": {"type": "integer", "description": "1-indexed start line (default 1)"},
                    "limit": {"type": "integer", "description": "max lines to return"}
                },
                "required": ["path"]
            }),
        },
        Tool {
            name: "read_symbol",
            description: "Read a source symbol by name, or the enclosing block around a 1-indexed line number. Returns a line-numbered block plus context. Lightweight fast text/range heuristic; use rg first for exact symbols or line hits.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or relative source file path"},
                    "symbol": {"type": "string", "description": "Function/type/impl/constant name to locate. Mutually exclusive with line."},
                    "line": {"type": "integer", "description": "1-indexed line number; returns the enclosing block or paragraph. Mutually exclusive with symbol."},
                    "context": {"type": "integer", "description": "lines of context before/after the block (default 5, max 50)"}
                },
                "required": ["path"]
            }),
        },
        Tool {
            name: "write_file",
            description: "Write content to a file, creating or overwriting it. Creates parent directories if needed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                },
                "required": ["path", "content"]
            }),
        },
        Tool {
            name: "edit_file",
            description: "Replace an exact string in a file. old_string must appear exactly once.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "old_string": {"type": "string"},
                    "new_string": {"type": "string"}
                },
                "required": ["path", "old_string", "new_string"]
            }),
        },
        Tool {
            name: "multi_edit",
            description: "Apply a batch of edits to one file atomically. Each edit replaces old_string with new_string. Set replace_all=true to replace every occurrence; otherwise old_string must be unique at time of application. If any edit fails, nothing is written.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "edits": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "old_string": {"type": "string"},
                                "new_string": {"type": "string"},
                                "replace_all": {"type": "boolean"}
                            },
                            "required": ["old_string", "new_string"]
                        }
                    }
                },
                "required": ["path", "edits"]
            }),
        },
        Tool {
            name: "bash",
            description: "Execute a shell command via bash -c and return exit code, stdout, and stderr. Commands run with pipefail enabled. Stdout/stderr are capped, and the process is timed out if it runs too long; set timeout for legitimate long tasks. Bash calls are atomic: Dext cleans the tool process group after the shell exits, so shell backgrounding, nohup, or disown are not persistent; setsid-style detaches are unsupported because they escape Dext cleanup. For user-requested persistent local services, prefer an OS supervisor (on Linux with systemd: systemd-run --user with a dext- unit, then inspect/stop it with systemctl --user). Prefer heredocs/arrays for complex quoting, and treat unexpected stderr on exit 0 as suspicious. The unsafe pip flag '--break-system-packages' is blocked unless explicitly overridden.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "command": {"type": "string"},
                    "timeout": {"type": "integer", "description": "Optional timeout in seconds for legitimate long-running commands. Defaults to DEXT_BASH_TIMEOUT_SECS or 60."}
                },
                "required": ["command"]
            }),
        },
        Tool {
            name: "fd",
            description: "Fast file finder (fd). Pattern is a regex by default. Stdout/stderr are capped; add a path or flags to narrow results.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Root directory, defaults to '.'"},
                    "extra_args": {"type": "array", "items": {"type": "string"}, "description": "Extra fd flags, e.g. ['-t','f','-H']"}
                },
                "required": ["pattern"]
            }),
        },
        Tool {
            name: "rg",
            description: "ripgrep: fast regex content search for files and code. Stdout/stderr are capped; narrow the pattern or scope if needed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Path to search, defaults to '.'"},
                    "extra_args": {"type": "array", "items": {"type": "string"}, "description": "Extra rg flags, e.g. ['-i','--glob','*.rs']"}
                },
                "required": ["pattern"]
            }),
        },
        Tool {
            name: "jq",
            description: "Query or transform JSON with jq. Pass the filter and either JSON text or a file path. Stdout/stderr are capped for large results.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "filter": {"type": "string", "description": "jq filter, e.g. '.items[].name'"},
                    "json": {"type": "string", "description": "JSON text (mutually exclusive with path)"},
                    "path": {"type": "string", "description": "Path to a JSON file (mutually exclusive with json)"}
                },
                "required": ["filter"]
            }),
        },
        Tool {
            name: "fzf",
            description: "Non-interactive fuzzy filter. Returns lines from 'items' ranked by match to 'query'. Output is capped; prefilter large item sets when possible.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string"},
                    "items": {"type": "array", "items": {"type": "string"}}
                },
                "required": ["query", "items"]
            }),
        },
        Tool {
            name: "http",
            description: "HTTP request via Dext's built-in client. Args are HTTPie-ish, e.g. ['GET','https://api.x','Auth:Bearer abc'] or ['POST','url','name=john']. Response output is capped. Add --extract-text (or --text) to strip HTML/script/style noise and pretty-print JSON for research pages.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "args": {"type": "array", "items": {"type": "string"}},
                    "stdin": {"type": "string", "description": "Optional stdin body"}
                },
                "required": ["args"]
            }),
        },
        Tool {
            name: "browser",
            description: "Optional browser automation through the browser tool. Pass browser args such as ['open','https://example.com'], ['snapshot'], ['click','@ref'], or start with ['skills','get','core','--full'].",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "args": {"type": "array", "items": {"type": "string"}},
                    "stdin": {"type": "string", "description": "Optional stdin body"}
                },
                "required": ["args"]
            }),
        },
        Tool {
            name: "awk",
            description: "awk text processor. Pass full arg list. Non-destructive: do not write files from awk — use write_file instead. Stdout/stderr are capped.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "args": {"type": "array", "items": {"type": "string"}, "description": "e.g. ['-F,','{print $1}','data.csv']"},
                    "stdin": {"type": "string"}
                },
                "required": ["args"]
            }),
        },
        Tool {
            name: "git_diff",
            description: "Show git diff for the repo. Returns staged, unstaged, or a specific commit range. Output is capped; prefer stat=true first for broad reviews, then target paths/hunks to avoid repeated capped diffs.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Optional file or directory to diff. Defaults to entire repo."},
                    "staged": {"type": "boolean", "description": "Show staged changes (git diff --cached). Default false."},
                    "commit": {"type": "string", "description": "Commit range or ref, e.g. 'HEAD~3..HEAD' or 'main'."},
                    "stat": {"type": "boolean", "description": "Show --stat summary instead of full patch. Useful before targeted diffs."}
                }
            }),
        },
        Tool {
            name: "git_log",
            description: "Show recent git log entries. Returns oneline format by default. Useful for understanding recent changes.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "count": {"type": "integer", "description": "Number of commits to show (default 10, max 50)."},
                    "path": {"type": "string", "description": "Optional file or directory to filter commits."},
                    "oneline": {"type": "boolean", "description": "Use oneline format (default true). Set false for full format."}
                }
            }),
        },
        Tool {
            name: "git_commit",
            description: "Stage files and create a git commit. Prefer this over raw bash git commands for commits. The commit message should be a good conventional commit.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "message": {"type": "string"},
                    "paths": {"type": "array", "items": {"type": "string"}, "description": "Files to stage (git add). Empty or omitted stages all tracked changes."},
                    "all": {"type": "boolean", "description": "Stage all changes including untracked files (git add -A). Default false."}
                },
                "required": ["message"]
            }),
        },
        Tool {
            name: "todo_read",
            description: "Read the current todo/task list for this project. Returns structured tasks with status (pending/in_progress/completed). Use this to check what work remains before starting new tasks.",
            input_schema: json!({
                "type": "object",
                "properties": {}
            }),
        },
        Tool {
            name: "todo_write",
            description: "Update the todo/task list for this project. Replaces the entire list. Use this to track multi-step work — add tasks before starting, mark in_progress while working, and completed when done. This persists across sessions.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "todos": {
                        "type": "array",
                        "items": {
                            "type": "object",
                            "properties": {
                                "text": {"type": "string", "description": "What to do"},
                                "status": {"type": "string", "enum": ["pending", "in_progress", "completed"], "description": "Current status"}
                            },
                            "required": ["text", "status"]
                        }
                    }
                },
                "required": ["todos"]
            }),
        },
        Tool {
            name: "csvkit",
            description: "csvkit subcommand runner: csvcut, csvstat, csvgrep, csvjson, csvlook, csvsort, csvjoin, in2csv, csvsql. Stdout/stderr are capped for large tables.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "subcommand": {"type": "string", "enum": ["csvcut","csvstat","csvgrep","csvjson","csvlook","csvsort","csvjoin","in2csv","csvsql","csvformat","csvclean","csvstack"]},
                    "args": {"type": "array", "items": {"type": "string"}},
                    "stdin": {"type": "string"}
                },
                "required": ["subcommand", "args"]
            }),
        },
    ]
}

pub(crate) fn needs_permission(name: &str) -> bool {
    matches!(
        name,
        "bash"
            | "browser"
            | "write_file"
            | "edit_file"
            | "multi_edit"
            | "http"
            | "awk"
            | "csvkit"
            | "git_commit"
            | "todo_write"
    )
}

pub(crate) fn is_external_process_tool(name: &str) -> bool {
    matches!(
        name,
        "fd" | "rg" | "jq" | "fzf" | "awk" | "csvkit" | "git_diff" | "git_log" | "browser"
    )
}

pub(crate) fn is_parallel_safe_tool(name: &str) -> bool {
    matches!(
        name,
        "read_file"
            | "read_symbol"
            | "fd"
            | "rg"
            | "jq"
            | "fzf"
            | "git_diff"
            | "git_log"
            | "todo_read"
    )
}

pub(crate) fn should_parallelize_builtin_tools(names: &[&str]) -> bool {
    !names.is_empty() && names.iter().all(|name| is_parallel_safe_tool(name))
}

fn lean_description(name: &str, fallback: &str) -> String {
    match name {
        "read_file" => "Read capped line-numbered file window. Prefer offset+limit.",
        "read_symbol" => "Read source symbol or block around line.",
        "write_file" => "Write file content.",
        "edit_file" => "Replace one exact string in a file.",
        "multi_edit" => "Apply atomic exact-string edits to one file.",
        "bash" => "Run atomic bash command; pipefail; stdout/stderr capped. Use supervised dext- service for persistence.",
        "fd" => "Find files by regex pattern.",
        "rg" => "Search text with ripgrep.",
        "jq" => "Run jq on JSON text or file.",
        "fzf" => "Rank provided lines by fuzzy query.",
        "http" => "HTTPie-style request; response capped.",
        "browser" => "Run optional browser automation.",
        "awk" => "Run awk with optional stdin.",
        "git_diff" => "Show capped git diff or stat; prefer stat first.",
        "git_log" => "Show recent git log.",
        "git_commit" => "Stage files and create git commit.",
        "todo_read" => "Read project todo list.",
        "todo_write" => "Replace project todo list for nontrivial work.",
        "csvkit" => "Run csvkit subcommand.",
        _ => fallback,
    }
    .to_string()
}

fn slim_schema(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = serde_json::Map::new();
            for (key, child) in map {
                if key == "description" {
                    continue;
                }
                out.insert(key.clone(), slim_schema(child));
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(slim_schema).collect()),
        _ => value.clone(),
    }
}

fn tool_description(tool: &Tool, profile: ToolProfile) -> String {
    match profile {
        ToolProfile::Full => tool.description.to_string(),
        ToolProfile::Lean => lean_description(tool.name, tool.description),
    }
}

fn tool_schema(tool: &Tool, profile: ToolProfile) -> Value {
    match profile {
        ToolProfile::Full => tool.input_schema.clone(),
        ToolProfile::Lean => slim_schema(&tool.input_schema),
    }
}

pub(crate) fn wire_tools(tools: &[Tool], profile: ToolProfile) -> Vec<WireTool> {
    let mut wt: Vec<WireTool> = tools
        .iter()
        .map(|t| WireTool {
            name: t.name.to_string(),
            description: tool_description(t, profile),
            input_schema: tool_schema(t, profile),
            cache_control: None,
        })
        .collect();
    if let Some(last) = wt.last_mut() {
        last.cache_control = Some(CacheControl::EPHEMERAL);
    }
    wt
}

pub(crate) fn wire_tools_oai(tools: &[Tool], profile: ToolProfile) -> Vec<OaiTool> {
    tools
        .iter()
        .map(|t| OaiTool {
            r#type: "function".to_string(),
            function: OaiFunctionDef {
                name: t.name.to_string(),
                description: tool_description(t, profile),
                parameters: tool_schema(t, profile),
            },
        })
        .collect()
}

pub(crate) fn wire_tools_chatgpt(tools: &[Tool], profile: ToolProfile) -> Vec<Value> {
    tools
        .iter()
        .map(|t| {
            json!({
                "type": "function",
                "name": t.name,
                "description": tool_description(t, profile),
                "parameters": tool_schema(t, profile),
                "strict": Value::Null,
            })
        })
        .collect()
}
