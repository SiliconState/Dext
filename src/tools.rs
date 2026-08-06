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
#[cfg(test)]
pub(crate) struct SlashCommand {
    pub(crate) name: &'static str,
    pub(crate) usage: &'static str,
    pub(crate) description: &'static str,
}

#[cfg(test)]
pub(crate) fn slash_command_definitions() -> Vec<SlashCommand> {
    vec![
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
            description: "Read a file from disk. Output is line-numbered (1-indexed) and capped for safety. May inspect absolute paths outside the sandbox read-only; writes remain confined. Explicit offset+limit reads get a larger cap and are cached for overlapping follow-up reads; still prefer rg first and focused windows.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or relative file path"},
                    "offset": {"type": "integer", "minimum": 1, "description": "1-indexed start line (default 1)"},
                    "limit": {"type": "integer", "minimum": 1, "description": "max lines to return"}
                },
                "required": ["path"]
            }),
        },
        Tool {
            name: "read_symbol",
            description: "Read a source symbol by name, or the enclosing block around a 1-indexed line number. Returns a line-numbered block plus context. May inspect absolute paths outside the sandbox read-only; writes remain confined. Source input is capped at 8 MiB and observes cancellation while loading. Lightweight fast text/range heuristic; use rg first for exact symbols or line hits.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "path": {"type": "string", "description": "Absolute or relative source file path"},
                    "symbol": {"type": "string", "description": "Function/type/impl/constant name to locate. Mutually exclusive with line."},
                    "line": {"type": "integer", "minimum": 1, "description": "1-indexed line number; returns the enclosing block or paragraph. Mutually exclusive with symbol."},
                    "context": {"type": "integer", "minimum": 0, "maximum": 50, "description": "lines of context before/after the block (default 5, max 50)"}
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
            description: "Execute a shell command via bash -c and return exit code, stdout, and stderr. Last-resort tool: do not use for ordinary file reads/search/discovery, git diff/log, JSON filtering, or HTTP when the corresponding native tool is exposed and fits (read_file/read_symbol/fd/rg/git_diff/git_log/jq/http). Use bash for shell-only orchestration, build/test/install commands, and tool-catalog gaps. Commands run with pipefail enabled. Stdout/stderr are capped, and the process is timed out if it runs too long; set timeout for legitimate long tasks. Bash calls are atomic: Dext cleans the tool process group after the shell exits, so shell backgrounding, nohup, or disown are not persistent; setsid-style detaches are unsupported because they escape Dext cleanup. For user-requested persistent local services, prefer an OS supervisor (on Linux with systemd: systemd-run --user with a dext- unit, then inspect/stop it with systemctl --user). Prefer heredocs/arrays for complex quoting, and treat unexpected stderr on exit 0 as suspicious. The unsafe pip flag '--break-system-packages' is blocked unless explicitly overridden.",
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
            description: "Fast file finder (fd). Pattern is a regex by default. Path may be absolute for read-only outside-sandbox inspection. Stdout/stderr are capped; add a path or flags to narrow results.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Root directory, defaults to '.'"},
                    "extra_args": {"type": "array", "items": {"type": "string"}, "description": "Extra non-executing fd flags, e.g. ['-t','f','-H']; exec flags (-x/-X/--exec*) are rejected"}
                },
                "required": ["pattern"]
            }),
        },
        Tool {
            name: "rg",
            description: "ripgrep: fast regex content search for files and code. Path may be absolute for read-only outside-sandbox inspection. Stdout/stderr are capped; narrow the pattern or scope if needed.",
            input_schema: json!({
                "type": "object",
                "properties": {
                    "pattern": {"type": "string"},
                    "path": {"type": "string", "description": "Path to search, defaults to '.'"},
                    "extra_args": {"type": "array", "items": {"type": "string"}, "description": "Extra non-executing rg flags, e.g. ['-i','--glob','*.rs']; preprocessor/archive/hostname command flags are rejected"}
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
            description: "HTTP request via Dext's built-in client. Args are HTTPie-ish, e.g. ['GET','https://api.x','Auth:Bearer abc'] or ['POST','url','name=john']. Raw output is capped; decoded response reads stop at exact safety ceilings. Add --extract-text (or --text) to read a bounded source head, strip HTML/script/style noise, and pretty-print JSON for research pages. Duplicate, transport/framing, and method-override headers plus URL credentials are rejected. Headerless/bodyless GET or HEAD may follow validated cross-origin redirects; sensitive requests remain same-origin.",
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

#[derive(Debug, Clone, Copy)]
struct ToolSpec {
    name: &'static str,
    required_fields: &'static [&'static str],
    flags: u8,
}

const PERMISSION_REQUIRED: u8 = 1;
const PARALLEL_SAFE: u8 = 1 << 1;
const EXTERNAL_PROCESS: u8 = 1 << 2;
const DEFAULT_PROFILE: u8 = 1 << 3;

const fn tool(name: &'static str, required_fields: &'static [&'static str], flags: u8) -> ToolSpec {
    ToolSpec {
        name,
        required_fields,
        flags,
    }
}

const TOOL_SPECS: &[ToolSpec] = &[
    tool("read_file", &["path"], PARALLEL_SAFE | DEFAULT_PROFILE),
    tool("read_symbol", &["path"], PARALLEL_SAFE | DEFAULT_PROFILE),
    tool(
        "write_file",
        &["path", "content"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool(
        "edit_file",
        &["path", "old_string", "new_string"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool(
        "multi_edit",
        &["path", "edits"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool("bash", &["command"], PERMISSION_REQUIRED | DEFAULT_PROFILE),
    tool(
        "fd",
        &["pattern"],
        PARALLEL_SAFE | EXTERNAL_PROCESS | DEFAULT_PROFILE,
    ),
    tool(
        "rg",
        &["pattern"],
        PARALLEL_SAFE | EXTERNAL_PROCESS | DEFAULT_PROFILE,
    ),
    tool("jq", &["filter"], PARALLEL_SAFE | EXTERNAL_PROCESS),
    tool("fzf", &["query", "items"], PARALLEL_SAFE | EXTERNAL_PROCESS),
    tool("http", &["args"], PERMISSION_REQUIRED | DEFAULT_PROFILE),
    tool("awk", &["args"], PERMISSION_REQUIRED | EXTERNAL_PROCESS),
    tool(
        "git_diff",
        &[],
        PARALLEL_SAFE | EXTERNAL_PROCESS | DEFAULT_PROFILE,
    ),
    tool("git_log", &[], PARALLEL_SAFE | EXTERNAL_PROCESS),
    tool(
        "git_commit",
        &["message"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool("todo_read", &[], PARALLEL_SAFE | DEFAULT_PROFILE),
    tool(
        "todo_write",
        &["todos"],
        PERMISSION_REQUIRED | DEFAULT_PROFILE,
    ),
    tool(
        "csvkit",
        &["subcommand", "args"],
        PERMISSION_REQUIRED | EXTERNAL_PROCESS,
    ),
];

fn tool_spec(name: &str) -> Option<&'static ToolSpec> {
    TOOL_SPECS.iter().find(|spec| spec.name == name)
}

pub(crate) fn required_fields(name: &str) -> &'static [&'static str] {
    tool_spec(name).map_or(&[], |spec| spec.required_fields)
}

pub(crate) fn is_default_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & DEFAULT_PROFILE != 0)
}

pub(crate) fn registered_tool_names() -> impl Iterator<Item = &'static str> {
    TOOL_SPECS.iter().map(|spec| spec.name)
}

pub(crate) fn specialized_tool_names() -> impl Iterator<Item = &'static str> {
    TOOL_SPECS
        .iter()
        .filter(|spec| spec.flags & DEFAULT_PROFILE == 0)
        .map(|spec| spec.name)
}

pub(crate) fn needs_permission(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & PERMISSION_REQUIRED != 0)
}

pub(crate) fn is_side_effect_capable_tool(name: &str) -> bool {
    needs_permission(name)
}

pub(crate) fn is_external_process_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & EXTERNAL_PROCESS != 0)
}

pub(crate) fn is_parallel_safe_tool(name: &str) -> bool {
    tool_spec(name).is_some_and(|spec| spec.flags & PARALLEL_SAFE != 0)
}

pub(crate) fn should_parallelize_builtin_tools(names: &[&str]) -> bool {
    !names.is_empty() && names.iter().all(|name| is_parallel_safe_tool(name))
}

fn lean_description(name: &str, fallback: &str) -> String {
    match name {
        "read_file" => "Read capped line-numbered file window; absolute paths read-only.",
        "read_symbol" => {
            "Read symbol/enclosing line block; selectors exclusive; absolute paths read-only."
        }
        "write_file" => "Create/overwrite file.",
        "edit_file" => "Replace one unique exact string.",
        "multi_edit" => "Apply atomic exact replacements in one file.",
        "bash" => {
            "Atomic shell fallback for build/test/install/gaps; timeout in seconds; pipefail; capped; no persistence; prefer arrays/heredocs for quoting."
        }
        "fd" => "Find files by regex; absolute paths read-only.",
        "rg" => "Search text by regex; absolute paths read-only.",
        "jq" => "Run jq on JSON text or file.",
        "fzf" => "Rank provided lines by fuzzy query.",
        "http" => "HTTPie-style request; response capped.",
        "awk" => "Run awk with optional stdin.",
        "git_diff" => "Read capped Git diff/stat; use stat first when broad.",
        "git_log" => "Show recent git log.",
        "git_commit" => "Stage and commit files.",
        "todo_read" => "Read project todos.",
        "todo_write" => "Replace project todos.",
        "csvkit" => "Run csvkit subcommand.",
        _ => fallback,
    }
    .to_string()
}

fn slim_schema(value: &Value) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };

    let mut out = serde_json::Map::new();
    for (key, child) in map {
        if key == "description" {
            continue;
        }
        let child = match key.as_str() {
            "properties" | "patternProperties" | "dependentSchemas" | "$defs" | "definitions" => {
                slim_schema_map(child)
            }
            "dependencies" => slim_schema_dependencies(child),
            "additionalItems"
            | "additionalProperties"
            | "contains"
            | "contentSchema"
            | "else"
            | "if"
            | "items"
            | "not"
            | "propertyNames"
            | "then"
            | "unevaluatedItems"
            | "unevaluatedProperties" => slim_schema_or_array(child),
            "allOf" | "anyOf" | "oneOf" | "prefixItems" => slim_schema_array(child),
            _ => child.clone(),
        };
        out.insert(key.clone(), child);
    }
    Value::Object(out)
}

fn slim_schema_map(value: &Value) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    Value::Object(
        map.iter()
            .map(|(name, schema)| (name.clone(), slim_schema(schema)))
            .collect(),
    )
}

fn slim_schema_dependencies(value: &Value) -> Value {
    let Value::Object(map) = value else {
        return value.clone();
    };
    Value::Object(
        map.iter()
            .map(|(name, dependency)| {
                let value = if dependency.is_object() {
                    slim_schema(dependency)
                } else {
                    dependency.clone()
                };
                (name.clone(), value)
            })
            .collect(),
    )
}

fn slim_schema_or_array(value: &Value) -> Value {
    if value.is_array() {
        slim_schema_array(value)
    } else {
        slim_schema(value)
    }
}

fn slim_schema_array(value: &Value) -> Value {
    let Value::Array(items) = value else {
        return value.clone();
    };
    Value::Array(items.iter().map(slim_schema).collect())
}

fn tool_description(tool: &Tool, profile: ToolProfile) -> String {
    match profile {
        ToolProfile::Full => tool.description.to_string(),
        ToolProfile::Lean => lean_description(tool.name, tool.description),
    }
}

pub(crate) fn schema_for_profile(schema: &Value, profile: ToolProfile) -> Value {
    match profile {
        ToolProfile::Full => schema.clone(),
        ToolProfile::Lean => slim_schema(schema),
    }
}

fn tool_schema(tool: &Tool, profile: ToolProfile) -> Value {
    schema_for_profile(&tool.input_schema, profile)
}

#[derive(Clone, Serialize)]
pub(crate) struct ProviderNeutralTool {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) schema: Value,
}

pub(crate) fn provider_neutral_tools(
    tools: &[Tool],
    profile: ToolProfile,
) -> Vec<ProviderNeutralTool> {
    tools
        .iter()
        .map(|tool| ProviderNeutralTool {
            name: tool.name.to_string(),
            description: tool_description(tool, profile),
            schema: tool_schema(tool, profile),
        })
        .collect()
}

pub(crate) fn wire_tools_from_neutral(tools: Vec<ProviderNeutralTool>) -> Vec<WireTool> {
    let mut wire = tools
        .into_iter()
        .map(|tool| WireTool {
            name: tool.name,
            description: tool.description,
            input_schema: tool.schema,
            cache_control: None,
        })
        .collect::<Vec<_>>();
    if let Some(last) = wire.last_mut() {
        last.cache_control = Some(CacheControl::for_prompt());
    }
    wire
}

pub(crate) fn wire_oai_tools_from_neutral(tools: Vec<ProviderNeutralTool>) -> Vec<OaiTool> {
    tools
        .into_iter()
        .map(|tool| OaiTool {
            r#type: "function".to_string(),
            function: OaiFunctionDef {
                name: tool.name,
                description: tool.description,
                parameters: tool.schema,
            },
        })
        .collect()
}

pub(crate) fn wire_responses_tools_from_neutral(tools: Vec<ProviderNeutralTool>) -> Vec<Value> {
    tools
        .into_iter()
        .map(|tool| {
            json!({
                "type": "function",
                "name": tool.name,
                "description": tool.description,
                "parameters": tool.schema,
                "strict": Value::Null,
            })
        })
        .collect()
}

#[cfg(test)]
pub(crate) fn wire_tools(tools: &[Tool], profile: ToolProfile) -> Vec<WireTool> {
    wire_tools_from_neutral(provider_neutral_tools(tools, profile))
}
