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
        _ => None,
    }
}

pub(crate) fn validate_tool_input(name: &str, input: &Value) -> std::result::Result<(), String> {
    if let Some(issue) = tool_input_issue(name, input) {
        return Err(format!("invalid tool args for {name}: {issue}"));
    }
    Ok(())
}

pub(crate) fn tool_input_advisory(name: &str, input: &Value) -> Option<String> {
    match name {
        "bash" => bash_command_advisory(input["command"].as_str().unwrap_or("")),
        "read_symbol" => read_symbol_advisory(input["symbol"].as_str().unwrap_or("")),
        _ => None,
    }
}

pub(crate) fn command_requests_sudo_password(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    if !lower.contains("sudo") {
        return false;
    }
    let words = shell_words(command);
    let mut idx = 0usize;
    let mut command_position = true;
    while idx < words.len() {
        let word = &words[idx];
        if shell_command_separator(word) {
            command_position = true;
            idx += 1;
            continue;
        }
        if !command_position {
            idx += 1;
            continue;
        }
        if shell_assignment_word(word) || shell_command_prefix_word(word) {
            idx += 1;
            continue;
        }
        if word == "sudo" {
            let has_noninteractive = words[idx..]
                .iter()
                .take_while(|arg| !shell_command_separator(arg))
                .any(|arg| {
                    arg == "-n"
                        || arg == "--non-interactive"
                        || arg.strip_prefix('-').is_some_and(|flags| {
                            !flags.starts_with('-') && flags.chars().any(|ch| ch == 'n')
                        })
                });
            if !has_noninteractive {
                return true;
            }
        }
        command_position = false;
        idx += 1;
    }
    false
}

fn shell_command_separator(word: &str) -> bool {
    matches!(word, "&&" | ";" | "|" | "&")
}

fn shell_command_prefix_word(word: &str) -> bool {
    matches!(
        word,
        "command" | "env" | "time" | "if" | "then" | "do" | "while" | "until"
    )
}

fn shell_assignment_word(word: &str) -> bool {
    let Some((name, _)) = word.split_once('=') else {
        return false;
    };
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    (first == '_' || first.is_ascii_alphabetic())
        && chars.all(|ch| ch == '_' || ch.is_ascii_alphanumeric())
}

fn read_symbol_advisory(symbol: &str) -> Option<String> {
    let trimmed = symbol.trim();
    let token = trimmed
        .strip_prefix("struct ")
        .or_else(|| trimmed.strip_prefix("fn "))
        .or_else(|| trimmed.strip_prefix("impl "))
        .or_else(|| trimmed.strip_prefix("enum "))
        .or_else(|| trimmed.strip_prefix("trait "))?
        .split_whitespace()
        .next()
        .unwrap_or("")
        .trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    if token.is_empty() {
        None
    } else {
        Some(format!(
            "read_symbol expects the exact symbol name, not a declaration. Try rg -n '{token}' first, then read_symbol with symbol='{token}' or a focused line window."
        ))
    }
}

fn bash_command_advisory(command: &str) -> Option<String> {
    if let Some(msg) = cargo_test_multi_filter_advisory(command) {
        return Some(msg);
    }
    if let Some(msg) = slow_shell_search_advisory(command) {
        return Some(msg);
    }
    if bare_python_without_probe(command) {
        return Some(
            "bash advisory: this environment may not provide bare `python`; prefer `python3` or probe with `command -v python`.".to_string(),
        );
    }
    if let Some(tool) = api_tool_used_as_shell_filter(command) {
        return Some(format!(
            "bash advisory: `{tool}` is available as a Dext API tool but may not be installed as a shell binary. Use the native {tool} tool, use grep/awk, or probe with `command -v {tool}`."
        ));
    }
    if command_requests_sudo_password(command) {
        return Some(
            "bash advisory: sudo auth is local only. Dext will open a local password prompt if sudo needs authentication; never type sudo passwords into chat or steering input."
                .to_string(),
        );
    }
    if background_process_without_supervisor(command) {
        return Some(
            "bash advisory: bash calls are atomic and Dext cleans the process group after the shell exits; &, nohup, and disown will not persist, and setsid-style detaches are unsupported because they escape Dext cleanup. For a user-requested persistent local service, use an OS supervisor instead (Linux systemd example: systemd-run --user --unit=dext-<name> --same-dir <cmd>, inspect with systemctl --user status dext-<name>, stop with systemctl --user stop dext-<name>)."
                .to_string(),
        );
    }
    if let Some(msg) = avoidable_shell_tool_advisory(command) {
        return Some(msg);
    }
    None
}

fn background_process_without_supervisor(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    let words = shell_words(&lower);
    command_has_background_ampersand(&lower)
        || words.iter().any(|word| {
            matches!(word.as_str(), "nohup" | "disown" | "setsid")
                || word.ends_with("/nohup")
                || word.ends_with("/setsid")
        })
}

fn command_has_background_ampersand(command: &str) -> bool {
    let chars: Vec<char> = command.chars().collect();
    let mut in_single = false;
    let mut in_double = false;
    for (idx, ch) in chars.iter().enumerate() {
        match *ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '&' if !in_single && !in_double => {
                let prev = idx.checked_sub(1).and_then(|pos| chars.get(pos)).copied();
                let next = chars.get(idx + 1).copied();
                if matches!(prev, Some('&' | '>' | '<')) || matches!(next, Some('&' | '>')) {
                    continue;
                }
                return true;
            }
            _ => {}
        }
    }
    false
}

fn cargo_test_multi_filter_advisory(command: &str) -> Option<String> {
    for segment in command_segments(command) {
        let words = shell_words(&segment);
        for (idx, word) in words.iter().enumerate() {
            if word == "cargo" && words.get(idx + 1).is_some_and(|next| next == "test") {
                let mut filters = 0usize;
                let mut skip_next = false;
                for arg in words.iter().skip(idx + 2) {
                    if skip_next {
                        skip_next = false;
                        continue;
                    }
                    if arg == "--" || matches!(arg.as_str(), "&&" | ";" | "|") {
                        break;
                    }
                    if matches!(arg.as_str(), "--release" | "--debug") {
                        continue;
                    }
                    if matches!(arg.as_str(), "--test" | "--package" | "-p") {
                        skip_next = true;
                        continue;
                    }
                    if arg.starts_with('-') {
                        continue;
                    }
                    filters += 1;
                }
                if filters > 1 {
                    return Some(
                        "bash advisory: `cargo test` accepts one test filter before `--`; run separate tests or use one broader filter.".to_string(),
                    );
                }
            }
        }
    }
    None
}

fn slow_shell_search_advisory(command: &str) -> Option<String> {
    let words = shell_words(command);
    for (idx, word) in words.iter().enumerate() {
        match word.as_str() {
            "grep" => {
                if command.contains("command -v grep") || command.contains("which grep") {
                    continue;
                }
                let recursive = words
                    .iter()
                    .skip(idx + 1)
                    .take_while(|arg| !matches!(arg.as_str(), "&&" | ";" | "|" | "&"))
                    .any(|arg| {
                        if matches!(
                            arg.as_str(),
                            "-r" | "-R" | "--recursive" | "--dereference-recursive"
                        ) {
                            return true;
                        }
                        arg.starts_with('-')
                            && !arg.starts_with("--")
                            && arg.chars().skip(1).any(|ch| ch == 'r' || ch == 'R')
                    });
                if recursive {
                    return Some(
                        "bash advisory: prefer the native rg tool over recursive grep; it is faster, structured, capped, and respects ignore files."
                            .to_string(),
                    );
                }
            }
            "find" => {
                if command.contains("command -v find") || command.contains("which find") {
                    continue;
                }
                return Some(
                    "bash advisory: prefer the native fd tool over shell find for repo file discovery; it is faster, capped, and easier to narrow."
                        .to_string(),
                );
            }
            _ => {}
        }
    }
    None
}

fn avoidable_shell_tool_advisory(command: &str) -> Option<String> {
    let lower = command.to_ascii_lowercase();
    if command.trim().is_empty() || command.contains('>') || lower.contains("<<") {
        return None;
    }
    for segment in command_segments(&lower) {
        let segment = segment.trim();
        if segment.is_empty() || shell_probe_segment(segment) {
            continue;
        }
        if let Some((label, replacement)) = avoidable_shell_segment_replacement(segment) {
            return Some(format!(
                "bash advisory: `{label}` is avoidable shell usage here. Prefer native {replacement}; reserve bash for shell-only orchestration, build/test/install commands, or true tool-catalog gaps."
            ));
        }
    }
    None
}

fn shell_probe_segment(segment: &str) -> bool {
    let words = shell_words(segment);
    match words.as_slice() {
        [first, second, ..] if first == "command" && second == "-v" => true,
        [first, ..] if first == "which" => true,
        _ => false,
    }
}

fn avoidable_shell_segment_replacement(segment: &str) -> Option<(String, &'static str)> {
    let words = shell_words(segment);
    let mut idx = first_command_word_index(&words)?;
    let command = words.get(idx)?.as_str();
    if command == "git" {
        let subcommand_idx = git_subcommand_index(&words, idx)?;
        let subcommand = words.get(subcommand_idx)?.as_str();
        return match subcommand {
            "diff" if git_diff_args_have_native_equivalent(&words[subcommand_idx + 1..]) => {
                Some(("git diff".to_string(), "git_diff"))
            }
            _ => None,
        };
    }
    if command == "env" || command == "time" || command == "command" {
        idx = idx.saturating_add(1);
    }
    let command = words.get(idx)?.as_str();
    let replacement = match command {
        "cat" | "head" | "tail" | "less" | "more" | "nl" => "read_file/read_symbol",
        "sed"
            if !words
                .iter()
                .any(|word| word == "-i" || word.starts_with("-i")) =>
        {
            "read_file/read_symbol or rg"
        }
        "grep" | "egrep" | "fgrep" => "rg",
        "ls" | "tree" => "fd",
        "curl" | "wget" | "http" | "xh" => "http when exposed",
        _ => return None,
    };
    Some((command.to_string(), replacement))
}

fn first_command_word_index(words: &[String]) -> Option<usize> {
    let mut idx = 0usize;
    while idx < words.len() {
        let word = words[idx].as_str();
        if shell_assignment_word(word) || matches!(word, "env" | "time" | "command") {
            idx += 1;
            continue;
        }
        return Some(idx);
    }
    None
}

fn git_subcommand_index(words: &[String], git_idx: usize) -> Option<usize> {
    let mut idx = git_idx.saturating_add(1);
    while idx < words.len() {
        let word = words[idx].as_str();
        if matches!(word, "-c" | "-C" | "--git-dir" | "--work-tree") {
            idx = idx.saturating_add(2);
            continue;
        }
        if word.starts_with("--git-dir=") || word.starts_with("--work-tree=") {
            idx += 1;
            continue;
        }
        if word.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some(idx);
    }
    None
}

fn git_diff_args_have_native_equivalent(args: &[String]) -> bool {
    let mut after_pathspec_separator = false;
    let mut revisions_or_paths = 0usize;
    let mut pathspecs = 0usize;
    for arg in args {
        if after_pathspec_separator {
            pathspecs = pathspecs.saturating_add(1);
            if pathspecs > 1 {
                return false;
            }
            continue;
        }
        match arg.as_str() {
            "--" => after_pathspec_separator = true,
            "--stat" | "--cached" | "--staged" => {}
            _ if !arg.starts_with('-') => {
                revisions_or_paths = revisions_or_paths.saturating_add(1);
                if revisions_or_paths > 1 {
                    return false;
                }
            }
            _ => return false,
        }
    }
    true
}

fn bare_python_without_probe(command: &str) -> bool {
    let lower = command.to_ascii_lowercase();
    if lower.contains("command -v python") || lower.contains("which python") {
        return false;
    }
    command_segments(command)
        .iter()
        .any(|segment| segment.split_whitespace().next() == Some("python"))
}

fn api_tool_used_as_shell_filter(command: &str) -> Option<&'static str> {
    for segment in command_segments(command) {
        let mut words = segment.split_whitespace();
        let Some(first) = words.next() else {
            continue;
        };
        if !matches!(first, "rg" | "fd" | "jq") {
            continue;
        }
        if command.contains(&format!("command -v {first}"))
            || command.contains(&format!("which {first}"))
        {
            continue;
        }
        return match first {
            "rg" => Some("rg"),
            "fd" => Some("fd"),
            "jq" => Some("jq"),
            _ => None,
        };
    }
    None
}

const SHELL_PRELUDE_LINES: &[&str] = &[
    "set -euo pipefail",
    "set -eo pipefail",
    "set -o pipefail",
    "set -eu",
    "set -e",
];

fn strip_shell_prelude_prefix(mut line: &str) -> Option<&str> {
    let mut stripped = false;
    loop {
        let trimmed = line.trim_start();
        let mut matched = false;
        for prelude in SHELL_PRELUDE_LINES {
            let Some(suffix) = trimmed.strip_prefix(prelude) else {
                continue;
            };
            if !suffix.is_empty()
                && !suffix.chars().next().is_some_and(char::is_whitespace)
                && !suffix.starts_with("&&")
                && !suffix.starts_with(';')
            {
                continue;
            }
            let suffix = suffix.trim_start();
            if suffix.starts_with('#') {
                line = "";
                stripped = true;
                matched = true;
                break;
            }
            let rest = suffix
                .strip_prefix("&&")
                .or_else(|| suffix.strip_prefix(';'))
                .unwrap_or(suffix);
            line = rest;
            stripped = true;
            matched = true;
            break;
        }
        if !matched {
            break;
        }
    }
    stripped.then(|| line.trim_start())
}

fn command_segments(command: &str) -> Vec<String> {
    let mut segments: Vec<String> = command
        .split(['|', ';', '\n', '&'])
        .map(str::trim)
        .map(str::to_string)
        .collect();
    while let Some(rest) = segments
        .first()
        .and_then(|first| strip_shell_prelude_prefix(first).map(str::to_string))
    {
        segments.remove(0);
        if !rest.is_empty() {
            segments.insert(0, rest);
            break;
        }
    }
    segments
}

fn shell_words(command: &str) -> Vec<String> {
    let mut words = Vec::new();
    let mut current = String::new();
    let mut in_single = false;
    let mut in_double = false;
    for ch in command.chars() {
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            c if c.is_whitespace() && !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
            }
            ';' | '|' | '&' if !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                words.push(ch.to_string());
            }
            _ => current.push(ch),
        }
    }
    if !current.is_empty() {
        words.push(current);
    }
    words
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
        "awk" => classify_argv_tool_risk(&input["args"]),
        "csvkit" => classify_csvkit_risk(input),
        _ => CommandRisk::Write,
    }
}

fn args_contain_write_or_escape(args: &[String]) -> bool {
    args.iter().any(|arg| {
        let trimmed = arg.trim().to_ascii_lowercase();
        trimmed == "--in-place"
            || trimmed.contains('>')
            || trimmed.contains("system(")
            || trimmed.contains("| sh")
            || trimmed.contains("|sh")
            || trimmed.contains("| bash")
            || trimmed.contains("|bash")
    })
}

fn classify_argv_tool_risk(args_value: &Value) -> CommandRisk {
    let args: Vec<String> = args_value
        .as_array()
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();
    if args_contain_write_or_escape(&args) {
        CommandRisk::Write
    } else {
        CommandRisk::Read
    }
}

fn classify_csvkit_risk(input: &Value) -> CommandRisk {
    classify_argv_tool_risk(&input["args"])
}

fn shell_chunk_is_read_only(chunk: &str) -> bool {
    let mut words = chunk.split_whitespace();
    let first = words.next().unwrap_or("");
    match first {
        "echo" | "ls" | "cat" | "pwd" | "whoami" | "id" | "env" | "printenv" | "rg" | "fd"
        | "grep" | "head" | "tail" | "stat" | "which" | "basename" | "dirname" | "realpath"
        | "readlink" | "wc" | "sort" | "uniq" | "cut" | "tr" | "jq" => true,
        // These are read-only as filters but have in-place/exec escape hatches.
        // `-i` is the only short sed flag containing 'i', so any short-flag
        // cluster with 'i' (e.g. -i, -i.bak, -ni) means an in-place edit.
        "sed" => !words.any(|w| {
            w == "--in-place"
                || w.starts_with("--in-place=")
                || (w.starts_with('-') && !w.starts_with("--") && w.contains('i'))
        }),
        "find" => !words.any(|w| {
            matches!(
                w,
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls"
            ) || w.starts_with("-fprint")
        }),
        "awk" => !chunk.contains("system("),
        "git" => match words.next().unwrap_or("") {
            "status" | "diff" | "log" | "show" | "rev-parse" => true,
            // `git branch` only lists when every remaining arg is a list-style
            // flag; positional args create branches and -d/-D/-m/-M/-c/-C/-f
            // mutate them.
            "branch" => words.all(|w| {
                if !w.starts_with('-') {
                    return false;
                }
                if let Some(flags) = w.strip_prefix('-')
                    && !flags.starts_with('-')
                {
                    // Short flags may combine (-dr deletes a remote-tracking
                    // branch), so any cluster containing a mutating letter is
                    // a write; -a/-r/-v style listing flags stay read-only.
                    return !flags
                        .chars()
                        .any(|ch| matches!(ch, 'd' | 'D' | 'm' | 'M' | 'c' | 'C' | 'f' | 'u'));
                }
                !matches!(
                    w,
                    "--force"
                        | "--delete"
                        | "--move"
                        | "--copy"
                        | "--edit-description"
                        | "--unset-upstream"
                ) && !w.starts_with("--set-upstream-to")
            }),
            _ => false,
        },
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
        "\nrm ",
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
    // Catch rm/rmdir in command position (e.g. `rm file` at the start of the
    // command or after a separator), which the substring needles above miss.
    if command_segments(&lower)
        .iter()
        .any(|segment| matches!(segment.split_whitespace().next(), Some("rm" | "rmdir")))
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
                "blocked unsafe flag '--break-system-packages'. Use a virtualenv instead (python3 -m venv .venv && . .venv/bin/activate). Set DEXT_ALLOW_BREAK_SYSTEM_PACKAGES=1 to override."
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
    fn tool_input_advisories_catch_common_misuse_without_blocking() {
        let cargo = tool_input_advisory(
            "bash",
            &json!({"command": "cargo test --release one_test another_test -- --nocapture"}),
        )
        .expect("cargo multi-filter should warn");
        assert!(cargo.contains("one test filter"), "{cargo}");
        assert!(
            tool_input_advisory("bash", &json!({"command": "cargo test --release"})).is_none(),
            "cargo test --release has no test filter"
        );

        let python =
            tool_input_advisory("bash", &json!({"command": "python - <<'PY'\nprint(1)\nPY"}))
                .expect("bare python should warn");
        assert!(python.contains("python3"), "{python}");

        let recursive_grep = tool_input_advisory("bash", &json!({"command": "grep -R needle src"}))
            .expect("recursive grep should warn");
        assert!(
            recursive_grep.contains("native rg tool"),
            "{recursive_grep}"
        );

        let find = tool_input_advisory("bash", &json!({"command": "find . -name '*.rs'"}))
            .expect("find should warn");
        assert!(find.contains("native fd tool"), "{find}");

        let cat = tool_input_advisory("bash", &json!({"command": "cat src/main.rs"}))
            .expect("plain cat should warn");
        assert!(cat.contains("read_file/read_symbol"), "{cat}");

        let git_diff = tool_input_advisory(
            "bash",
            &json!({"command": "git diff --stat -- src/main.rs"}),
        )
        .expect("native git_diff should be preferred");
        assert!(git_diff.contains("git_diff"), "{git_diff}");

        assert!(
            tool_input_advisory(
                "bash",
                &json!({"command": "git diff -- src/main.rs src/tools.rs"}),
            )
            .is_none(),
            "multi-path git diff is not exactly representable by git_diff"
        );

        let curl = tool_input_advisory("bash", &json!({"command": "curl https://example.com"}))
            .expect("curl should prefer native http when exposed");
        assert!(curl.contains("avoidable shell usage"), "{curl}");
        assert!(!curl.contains("read-only shell"), "{curl}");

        let sed_after_prelude = tool_input_advisory(
            "bash",
            &json!({"command": "set -euo pipefail\ngit show HEAD:src/main.rs | sed -n '1,10p'"}),
        )
        .expect("sed pipe should warn after ignoring shell prelude");
        assert!(sed_after_prelude.contains("`sed`"), "{sed_after_prelude}");
        assert!(!sed_after_prelude.contains("`set`"), "{sed_after_prelude}");

        let inline_prelude = tool_input_advisory(
            "bash",
            &json!({"command": "set -euo pipefail git show HEAD:src/main.rs | sed -n '1,10p'"}),
        )
        .expect("inline prelude should not be advised as set");
        assert!(inline_prelude.contains("`sed`"), "{inline_prelude}");
        assert!(!inline_prelude.contains("`set`"), "{inline_prelude}");

        assert!(
            tool_input_advisory("bash", &json!({"command": "git diff --check"})).is_none(),
            "git diff --check has no native equivalent and is useful verification"
        );

        assert!(
            tool_input_advisory(
                "bash",
                &json!({"command": "command -v grep >/dev/null && grep -R needle src"}),
            )
            .is_none()
        );

        let rg = tool_input_advisory(
            "bash",
            &json!({"command": "git show HEAD:file | rg needle"}),
        )
        .expect("shell rg should warn");
        assert!(rg.contains("Dext API tool"), "{rg}");

        assert!(
            tool_input_advisory(
                "bash",
                &json!({"command": "command -v rg >/dev/null && git show HEAD:file | rg needle"}),
            )
            .is_none()
        );

        let background_amp = tool_input_advisory(
            "bash",
            &json!({"command": "python3 -m http.server 8000 >/tmp/dext.log 2>&1 &"}),
        )
        .expect("background ampersand should warn");
        assert!(
            background_amp.contains("bash calls are atomic"),
            "{background_amp}"
        );

        let background = tool_input_advisory(
            "bash",
            &json!({"command": "nohup python3 -m http.server 8000 >/tmp/dext.log 2>&1 &"}),
        )
        .expect("background process should warn");
        assert!(background.contains("bash calls are atomic"), "{background}");
        assert!(background.contains("systemd-run --user"), "{background}");

        let detached = tool_input_advisory(
            "bash",
            &json!({"command": "setsid python3 -m http.server 8000 >/tmp/dext.log 2>&1"}),
        )
        .expect("setsid detach should warn");
        assert!(detached.contains("unsupported"), "{detached}");

        assert!(
            tool_input_advisory(
                "bash",
                &json!({"command": "systemd-run --user --unit=dext-preview --same-dir python3 -m http.server 8000"}),
            )
            .is_none()
        );
        assert!(tool_input_advisory("bash", &json!({"command": "echo ok && echo done"})).is_none());
        assert!(tool_input_advisory("bash", &json!({"command": "printf '%s' ok 2>&1"})).is_none());

        let symbol = tool_input_advisory(
            "read_symbol",
            &json!({"path": "src/main.rs", "symbol": "struct Usage"}),
        )
        .expect("declaration-shaped symbol should warn");
        assert!(symbol.contains("symbol='Usage'"), "{symbol}");
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
        // rm in command position is dangerous even without flags or a leading space.
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "rm stale.txt"})),
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "echo ok\nrm stale.txt"})),
            CommandRisk::Danger
        );
        // git branch listing is read-only; branch mutation/creation is not.
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git branch"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git branch -a -v"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git branch --show-current"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git branch -D feature"})),
            CommandRisk::Write
        );
        // Combined short flags still mutate (-dr deletes a remote-tracking branch).
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git branch -dr origin/x"})),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git branch new-feature"})),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk(
                "bash",
                &json!({"command": "git branch --set-upstream-to=origin/main"})
            ),
            CommandRisk::Write
        );
        assert!(command_requests_sudo_password("sudo apt update"));
        assert!(command_requests_sudo_password("sudo -v"));
        assert!(command_requests_sudo_password("echo ok && sudo apt update"));
        assert!(command_requests_sudo_password(
            "sudo -n true; sudo apt update"
        ));
        assert!(command_requests_sudo_password(
            "echo sudo && sudo apt update"
        ));
        assert!(!command_requests_sudo_password("grep sudo README.md"));
        assert!(!command_requests_sudo_password("sudo -n true"));
        assert!(!command_requests_sudo_password("sudo -nv"));
        assert!(!command_requests_sudo_password(
            "sudo --non-interactive true"
        ));
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "sed -n '1,10p' src/main.rs"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "sed -i 's/a/b/' src/main.rs"})),
            CommandRisk::Write
        );
        // Combined short-flag cluster with 'i' is still an in-place edit.
        assert_eq!(
            classify_command_risk(
                "bash",
                &json!({"command": "sed -ni 's/a/b/w out' src/main.rs"})
            ),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk(
                "bash",
                &json!({"command": "sed --in-place 's/a/b/' src/main.rs"})
            ),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "find . -name '*.rs'"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "find . -name '*.tmp' -delete"})),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk(
                "bash",
                &json!({"command": "find . -name '*.tmp' -exec touch {} +"})
            ),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "awk '{print $1}' data.txt"})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk(
                "bash",
                &json!({"command": "awk '{system(\"touch pwned\")}' data.txt"})
            ),
            CommandRisk::Write
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
            classify_command_risk("awk", &json!({"args": ["{print $1}"]})),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk("awk", &json!({"args": ["{system(\"touch pwned\")}"]})),
            CommandRisk::Write
        );
        assert_eq!(
            classify_command_risk(
                "csvkit",
                &json!({"subcommand": "csvcut", "args": ["-c", "1"]})
            ),
            CommandRisk::Read
        );
        assert_eq!(
            classify_command_risk(
                "csvkit",
                &json!({"subcommand": "csvformat", "args": ["--in-place", "data.csv"]})
            ),
            CommandRisk::Write
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
