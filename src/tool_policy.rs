use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashSet;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CommandRisk {
    Read,
    Write,
    Danger,
}

impl CommandRisk {
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Danger => "danger",
        }
    }
}

fn str_array(v: &Value) -> Vec<String> {
    v.as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

pub(crate) fn required_fields_for_tool(name: &str) -> &'static [&'static str] {
    match name {
        "read_file" => &["path"],
        "read_symbol" => &["path"],
        "write_file" => &["path", "content"],
        "edit_file" => &["path", "old_string", "new_string"],
        "multi_edit" => &["path", "edits"],
        "bash" => &["command"],
        "fd" => &["pattern"],
        "rg" => &["pattern"],
        "jq" => &["filter"],
        "fzf" => &["query", "items"],
        "http" => &["args"],
        "browser" => &["args"],
        "awk" => &["args"],
        "csvkit" => &["subcommand", "args"],
        "subagent" => &["task"],
        "git_commit" => &["message"],
        "todo_write" => &["todos"],
        _ => &[],
    }
}

pub(crate) fn missing_required_tool_fields(name: &str, input: &Value) -> Vec<&'static str> {
    required_fields_for_tool(name)
        .iter()
        .copied()
        .filter(|field| {
            let value = &input[*field];
            if value.is_null() {
                return true;
            }
            // String-shaped fields where whitespace-only = missing.
            // Fields that must be a non-empty, non-whitespace string.
            // Excludes `new_string` (empty string legitimately means "delete text")
            // and `content` (empty file is valid).
            if matches!(
                *field,
                "path"
                    | "pattern"
                    | "command"
                    | "filter"
                    | "query"
                    | "old_string"
                    | "subcommand"
                    | "task"
                    | "message"
            ) {
                return value.as_str().is_none_or(|s| s.trim().is_empty());
            }
            false
        })
        .collect()
}

pub(crate) fn tool_input_issue(name: &str, input: &Value) -> Option<String> {
    let missing = missing_required_tool_fields(name, input);
    if !missing.is_empty() {
        return Some(format!("missing {}", missing.join(", ")));
    }

    match name {
        "http" | "awk" | "fd" | "rg" | "fzf" | "csvkit" | "browser" => {
            if !input["args"].is_null() && !input["args"].is_array() {
                return Some("args must be an array".to_string());
            }
            if name == "fzf" && !input["items"].is_array() {
                return Some("items must be an array".to_string());
            }
            None
        }
        "read_symbol" => {
            let has_symbol = input["symbol"]
                .as_str()
                .is_some_and(|s| !s.trim().is_empty());
            let has_line = input["line"].as_u64().is_some_and(|line| line > 0);
            if input["line"].as_i64().is_some_and(|line| line <= 0) {
                return Some("line must be a positive integer".to_string());
            }
            match (has_symbol, has_line) {
                (true, false) | (false, true) => None,
                (false, false) => Some("provide symbol or line".to_string()),
                (true, true) => Some("provide only one of symbol or line".to_string()),
            }
        }
        "todo_write" => {
            if !input["todos"].is_array() {
                Some("todos must be an array".to_string())
            } else {
                None
            }
        }
        "subagent" => {
            if let Some(task) = input["task"].as_str()
                && task.trim().is_empty()
            {
                return Some("task cannot be empty".to_string());
            }
            None
        }
        _ => None,
    }
}

pub(crate) fn validate_tool_input(name: &str, input: &Value) -> std::result::Result<(), String> {
    if let Some(issue) = tool_input_issue(name, input) {
        return Err(format!("invalid tool args for {name}: {issue}"));
    }
    Ok(())
}

pub(crate) fn extract_url_hosts(text: &str) -> Vec<String> {
    let mut seen: HashSet<String> = HashSet::new();
    let mut out: Vec<String> = Vec::new();

    for token in text.split(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '<' | '>' | '(' | ')' | '[' | ']')
    }) {
        let cleaned = token.trim_matches(|c: char| matches!(c, ',' | ';' | '.' | ')' | ']' | '}'));
        let rest = if let Some(r) = cleaned.strip_prefix("https://") {
            r
        } else if let Some(r) = cleaned.strip_prefix("http://") {
            r
        } else {
            continue;
        };

        let host = rest
            .split('/')
            .next()
            .unwrap_or("")
            .split(':')
            .next()
            .unwrap_or("")
            .trim_matches('.')
            .to_ascii_lowercase();
        if host.is_empty() {
            continue;
        }
        if seen.insert(host.clone()) {
            out.push(host);
        }
    }

    out
}

pub(crate) fn hosts_for_tool_call(name: &str, input: &Value) -> Vec<String> {
    match name {
        "bash" => extract_url_hosts(input["command"].as_str().unwrap_or("")),
        "http" | "browser" => {
            let joined = str_array(&input["args"]).join(" ");
            extract_url_hosts(&joined)
        }
        _ => Vec::new(),
    }
}

pub(crate) fn looks_like_bulk_network_call(name: &str, input: &Value) -> bool {
    match name {
        "bash" => {
            let command = input["command"].as_str().unwrap_or("");
            let lower = command.to_ascii_lowercase();
            let has_url = lower.contains("http://") || lower.contains("https://");
            let loopish = has_url
                && (lower.contains("for ")
                    || lower.contains("while ")
                    || lower.contains("xargs")
                    || lower.contains("parallel"));
            let bulk_query = lower.contains("size=")
                || lower.contains("limit=")
                || lower.contains("offset=")
                || lower.contains("symbols=")
                || lower.contains("ids=")
                || lower.contains("batch")
                || lower.contains("bulk");
            loopish || (has_url && bulk_query)
        }
        "http" | "browser" => {
            let joined = str_array(&input["args"]).join(" ");
            let lower = joined.to_ascii_lowercase();
            lower.contains("size=")
                || lower.contains("limit=")
                || lower.contains("offset=")
                || lower.contains("symbols=")
                || lower.contains("ids=")
                || lower.contains("batch")
                || lower.contains("bulk")
        }
        _ => false,
    }
}

pub(crate) fn output_has_auth_failure_markers(output: &str) -> bool {
    let lower = output.to_ascii_lowercase();
    [
        "unauthorized",
        "invalid crumb",
        "invalid api key",
        "api key is invalid",
        "api key required",
        "unable to access this feature",
        "http 401",
        "http 403",
        "status 401",
        "status 403",
        "forbidden",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

pub(crate) fn classify_command_risk(name: &str, input: &Value) -> CommandRisk {
    match name {
        "bash" => {
            let command = input["command"].as_str().unwrap_or("");
            classify_bash_command_risk(command)
        }
        "http" => classify_http_risk(input),
        "browser" => CommandRisk::Write,
        "write_file" | "edit_file" | "multi_edit" | "git_commit" | "todo_write" => {
            CommandRisk::Write
        }
        "read_file" | "read_symbol" | "fd" | "rg" | "jq" | "fzf" | "git_diff" | "git_log"
        | "todo_read" => CommandRisk::Read,
        "awk" | "csvkit" => CommandRisk::Read,
        _ => CommandRisk::Write,
    }
}

fn shell_chunk_is_read_only(chunk: &str) -> bool {
    let mut words = chunk.split_whitespace();
    let first = words.next().unwrap_or("");
    match first {
        "echo" | "ls" | "cat" | "pwd" | "whoami" | "id" | "env" | "printenv" | "rg" | "fd"
        | "find" | "grep" | "head" | "tail" | "stat" | "which" | "basename" | "dirname"
        | "realpath" | "readlink" | "wc" | "sort" | "uniq" | "cut" | "tr" | "sed" | "awk"
        | "jq" => true,
        "git" => matches!(
            words.next().unwrap_or(""),
            "status" | "diff" | "log" | "show" | "branch" | "rev-parse"
        ),
        _ => false,
    }
}

fn shell_command_is_read_only(command: &str) -> bool {
    let lower = command.trim().to_ascii_lowercase();
    if lower.is_empty() || lower.contains('>') {
        return false;
    }
    let mut saw_chunk = false;
    for chunk in lower.split(['|', ';', '\n', '&']) {
        let trimmed = chunk.trim();
        if trimmed.is_empty() {
            continue;
        }
        saw_chunk = true;
        if !shell_chunk_is_read_only(trimmed) {
            return false;
        }
    }
    saw_chunk
}

fn shell_command_is_dangerous(command: &str) -> bool {
    let lower = command.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return true;
    }
    let dangerous_needles = [
        "sudo ",
        "rm -",
        " rm ",
        "\nr m ",
        "rmdir ",
        "mkfs",
        "dd if=",
        "shutdown",
        "reboot",
        "poweroff",
        "git push",
        "docker system prune",
    ];
    if dangerous_needles
        .iter()
        .any(|needle| lower.contains(needle))
    {
        return true;
    }
    if (lower.contains("curl") || lower.contains("wget"))
        && (lower.contains("| sh")
            || lower.contains("|sh")
            || lower.contains("| bash")
            || lower.contains("|bash"))
    {
        return true;
    }
    ["post", "put", "patch", "delete"].iter().any(|verb| {
        lower.contains(&format!("-x {verb}"))
            || lower.contains(&format!("-x{verb}"))
            || lower.contains(&format!("--request {verb}"))
            || lower.contains(&format!("http {verb}"))
            || lower.contains(&format!("xh {verb}"))
    })
}

fn classify_bash_command_risk(command: &str) -> CommandRisk {
    if shell_command_is_dangerous(command) {
        CommandRisk::Danger
    } else if shell_command_is_read_only(command) {
        CommandRisk::Read
    } else {
        CommandRisk::Write
    }
}

fn classify_http_risk(input: &Value) -> CommandRisk {
    let method = input["args"]
        .as_array()
        .and_then(|args| args.first())
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_ascii_uppercase();
    match method.as_str() {
        "GET" | "HEAD" | "OPTIONS" => CommandRisk::Read,
        "POST" | "PUT" | "PATCH" | "DELETE" => CommandRisk::Danger,
        _ => CommandRisk::Danger,
    }
}

pub(crate) fn apply_bash_guardrails(command: &str) -> std::result::Result<String, String> {
    let lower = command.to_ascii_lowercase();
    if lower.contains("--break-system-packages") {
        let allow = std::env::var(crate::BASH_UNSAFE_FLAG_OVERRIDE_ENV)
            .unwrap_or_default()
            .to_ascii_lowercase();
        let allow_unsafe = matches!(allow.as_str(), "1" | "true" | "yes");
        if !allow_unsafe {
            return Err(
                "blocked unsafe flag '--break-system-packages'. Use a virtualenv instead (python3 -m venv .venv && . .venv/bin/activate). Set WOLF_ALLOW_BREAK_SYSTEM_PACKAGES=1 to override."
                    .to_string(),
            );
        }
    }

    if lower.contains("pipefail") {
        return Ok(command.to_string());
    }
    Ok(format!("set -o pipefail\n{command}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock().lock().expect("env lock")
    }

    #[test]
    fn bash_guardrails_add_pipefail_and_block_unsafe_pip_flag() {
        let _guard = env_lock();
        unsafe {
            std::env::remove_var(crate::BASH_UNSAFE_FLAG_OVERRIDE_ENV);
        }

        let guarded = apply_bash_guardrails("echo ok | cat").expect("guarded command");
        assert!(guarded.starts_with("set -o pipefail"), "{guarded}");

        let err = apply_bash_guardrails("pip install --break-system-packages requests")
            .expect_err("unsafe flag should be blocked");
        assert!(err.contains("blocked unsafe flag"), "{err}");
    }

    #[test]
    fn auth_failure_marker_detector_catches_known_signatures() {
        assert!(output_has_auth_failure_markers(
            "{\"error\":{\"code\":\"Unauthorized\",\"description\":\"Invalid Crumb\"}}"
        ));
        assert!(!output_has_auth_failure_markers("{\"result\":\"ok\"}"));
    }

    #[test]
    fn extract_url_hosts_handles_punctuation_ports_and_duplicates() {
        let hosts = extract_url_hosts(
            "curl \"https://API.EXAMPLE.com:443/v1?q=1\", http://foo.example.com/path https://api.example.com/v2",
        );
        assert_eq!(hosts, vec!["api.example.com", "foo.example.com"]);
    }

    #[test]
    fn hosts_for_tool_call_supports_bash_and_http_args() {
        let bash_hosts = hosts_for_tool_call(
            "bash",
            &json!({"command": "curl -s https://a.example.com/x && curl -s https://b.example.com/y"}),
        );
        assert_eq!(bash_hosts, vec!["a.example.com", "b.example.com"]);

        let http_hosts = hosts_for_tool_call(
            "http",
            &json!({"args": ["GET", "https://query.example.org/data?limit=1"]}),
        );
        assert_eq!(http_hosts, vec!["query.example.org"]);
    }

    #[test]
    fn bulk_network_detection_boundaries_are_reasonable() {
        assert!(looks_like_bulk_network_call(
            "bash",
            &json!({"command": "for t in a b c; do curl -s https://api.example.com/v1/item/$t; done"})
        ));
        assert!(looks_like_bulk_network_call(
            "http",
            &json!({"args": ["GET", "https://api.example.com/v1/items?limit=200"]})
        ));
        assert!(!looks_like_bulk_network_call(
            "http",
            &json!({"args": ["GET", "https://api.example.com/v1/items?id=abc"]})
        ));
        assert!(!looks_like_bulk_network_call(
            "bash",
            &json!({"command": "for t in a b c; do echo $t; done"})
        ));
    }

    #[test]
    fn command_risk_classifies_common_tool_calls() {
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git status && rg foo src"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "cargo test --release"})),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "sudo rm -rf /tmp/demo"})),
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk("read_file", &json!({"path": "/outside"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("git_diff", &json!({"stat": true})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("todo_read", &json!({})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("http", &json!({"args": ["GET", "https://example.com"]})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("http", &json!({"args": ["POST", "https://example.com"]})),
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk("edit_file", &json!({"path": "src/main.rs"})),
            CommandRisk::Write
        );
    }
}
