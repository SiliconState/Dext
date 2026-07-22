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

const CSVKIT_SUBCOMMANDS: &[&str] = &[
    "csvcut",
    "csvstat",
    "csvgrep",
    "csvjson",
    "csvlook",
    "csvsort",
    "csvjoin",
    "in2csv",
    "csvsql",
    "csvformat",
    "csvclean",
    "csvstack",
];

pub(crate) fn csvkit_subcommand_allowed(subcommand: &str) -> bool {
    CSVKIT_SUBCOMMANDS.contains(&subcommand)
}

fn string_array_issue(field: &str, value: &Value) -> Option<String> {
    let values = value.as_array()?;
    values
        .iter()
        .any(|item| !item.is_string())
        .then(|| format!("{field} must contain only strings"))
}

fn awk_external_program_option(arg: &str) -> bool {
    matches!(
        arg,
        "-f" | "--file" | "-e" | "--exec" | "-i" | "--include" | "-l" | "--load"
    ) || arg.starts_with("--file=")
        || arg.starts_with("--exec=")
        || arg.starts_with("--include=")
        || arg.starts_with("--load=")
        || (arg.starts_with("-f") && arg.len() > 2)
        || (arg.starts_with("-e") && arg.len() > 2)
        || (arg.starts_with("-i") && arg.len() > 2)
        || (arg.starts_with("-l") && arg.len() > 2)
}

pub(crate) fn awk_args_issue(args: &[String]) -> Option<String> {
    if args.iter().any(|arg| awk_external_program_option(arg)) {
        return Some(
            "external or alternate awk program and extension loading is not allowed; pass one inline program or use bash so command risk and approval policy apply"
                .to_string(),
        );
    }
    let mut idx = 0usize;
    while idx < args.len() {
        let arg = args[idx].as_str();
        if arg == "--" {
            idx += 1;
            break;
        }
        if matches!(arg, "-F" | "-v" | "--field-separator" | "--assign") {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg.starts_with("-F")
            || (arg.starts_with("-v") && arg.len() > 2)
            || arg.starts_with("--field-separator=")
            || arg.starts_with("--assign=")
            || matches!(
                arg,
                "--posix"
                    | "--traditional"
                    | "--characters-as-bytes"
                    | "--sandbox"
                    | "--lint"
                    | "--lint=fatal"
                    | "--lint=invalid"
                    | "--lint=no-ext"
                    | "--lint=old"
            )
        {
            idx += 1;
            continue;
        }
        if arg.starts_with('-') {
            return Some(format!(
                "unsupported awk option '{arg}'; use the inline, non-destructive awk subset or bash so command risk and approval policy apply"
            ));
        }
        break;
    }
    let Some(program) = args.get(idx) else {
        return Some("args must include an inline awk program".to_string());
    };
    let lower = program.to_ascii_lowercase();
    let compact: String = lower.chars().filter(|ch| !ch.is_whitespace()).collect();
    if compact.contains("system(")
        || lower.contains('@')
        || lower.contains("getline")
        || lower.contains("/inet/")
        || awk_program_has_output_redirection(program)
    {
        return Some(
            "awk subprocess, file-redirection, network, getline, and dynamic-loading constructs are not allowed; use bash so command risk and approval policy apply"
                .to_string(),
        );
    }
    None
}

fn awk_program_has_output_redirection(program: &str) -> bool {
    for keyword in ["print", "printf"] {
        let mut search_from = 0usize;
        while let Some(offset) = program[search_from..].find(keyword) {
            let start = search_from.saturating_add(offset);
            let before = program[..start].chars().next_back();
            let end = start.saturating_add(keyword.len());
            let after = program[end..].chars().next();
            if before.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
                && after.is_none_or(|ch| !ch.is_ascii_alphanumeric() && ch != '_')
                && awk_statement_has_top_level_redirection(&program[end..])
            {
                return true;
            }
            search_from = end;
        }
    }
    false
}

fn awk_statement_has_top_level_redirection(statement: &str) -> bool {
    let mut paren_depth = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    for ch in statement.chars() {
        if escaped {
            escaped = false;
            continue;
        }
        if ch == '\\' {
            escaped = true;
            continue;
        }
        if ch == '\'' && !in_double {
            in_single = !in_single;
            continue;
        }
        if ch == '"' && !in_single {
            in_double = !in_double;
            continue;
        }
        if in_single || in_double {
            continue;
        }
        match ch {
            '(' => paren_depth = paren_depth.saturating_add(1),
            ')' => paren_depth = paren_depth.saturating_sub(1),
            ';' | '\n' | '}' if paren_depth == 0 => break,
            '>' if paren_depth == 0 => return true,
            '|' if paren_depth == 0 => return true,
            _ => {}
        }
    }
    false
}

pub(crate) fn required_fields_for_tool(name: &str) -> &'static [&'static str] {
    crate::tools::required_fields(name)
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
        "http" => {
            if !input["args"].is_null() && !input["args"].is_array() {
                return Some("args must be an array".to_string());
            }
            string_array_issue("args", &input["args"])
        }
        "awk" => {
            if !input["args"].is_array() {
                return Some("args must be an array".to_string());
            }
            if let Some(issue) = string_array_issue("args", &input["args"]) {
                return Some(issue);
            }
            awk_args_issue(&str_array(&input["args"]))
        }
        "csvkit" => {
            if !input["args"].is_array() {
                return Some("args must be an array".to_string());
            }
            if let Some(issue) = string_array_issue("args", &input["args"]) {
                return Some(issue);
            }
            let subcommand = input["subcommand"].as_str().unwrap_or_default();
            if !csvkit_subcommand_allowed(subcommand) {
                return Some(format!("unsupported csvkit subcommand '{subcommand}'"));
            }
            None
        }
        "fd" | "rg" => {
            if !input["extra_args"].is_null() && !input["extra_args"].is_array() {
                return Some("extra_args must be an array".to_string());
            }
            if let Some(issue) =
                search_tool_extra_args_issue(name, &str_array(&input["extra_args"]))
            {
                return Some(issue);
            }
            None
        }
        "fzf" => {
            if !input["items"].is_array() {
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
        "git_commit" => {
            if !input["paths"].is_null() {
                if !input["paths"].is_array() {
                    return Some("paths must be an array".to_string());
                }
                if let Some(issue) = string_array_issue("paths", &input["paths"]) {
                    return Some(issue);
                }
            }
            if !input["all"].is_null() && !input["all"].is_boolean() {
                return Some("all must be a boolean".to_string());
            }
            None
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

pub(crate) fn command_invokes_sudo(command: &str) -> bool {
    shell_command_invocations(command)
        .iter()
        .any(|words| words.first().is_some_and(|word| is_sudo_command_word(word)))
}

fn shell_command_invocations(command: &str) -> Vec<Vec<String>> {
    let mut invocations = Vec::new();
    collect_shell_command_invocations(command, 0, &mut invocations);
    invocations
}

fn collect_shell_command_invocations(
    command: &str,
    depth: usize,
    invocations: &mut Vec<Vec<String>>,
) {
    if depth > 6 {
        return;
    }
    let words = shell_words(command);
    let mut start = 0usize;
    while start < words.len() {
        while start < words.len() && shell_command_separator(&words[start]) {
            start += 1;
        }
        let mut end = start;
        while end < words.len() && !shell_command_separator(&words[end]) {
            end += 1;
        }
        collect_shell_segment_invocation(&words[start..end], depth, invocations);
        start = end.saturating_add(1);
    }
    for nested in embedded_shell_commands(command) {
        collect_shell_command_invocations(&nested, depth.saturating_add(1), invocations);
    }
}

fn collect_shell_segment_invocation(
    segment: &[String],
    depth: usize,
    invocations: &mut Vec<Vec<String>>,
) {
    if depth > 6 {
        return;
    }
    let mut idx = 0usize;
    while idx < segment.len()
        && (shell_assignment_word(&segment[idx]) || shell_command_keyword(&segment[idx]))
    {
        idx += 1;
    }
    loop {
        let Some(word) = segment.get(idx) else {
            return;
        };
        match shell_command_basename(word) {
            "env" => {
                idx += 1;
                skip_env_command_prefix(segment, &mut idx);
            }
            "command" | "builtin" => {
                idx += 1;
                skip_command_builtin_prefix(segment, &mut idx);
            }
            "time" => {
                idx += 1;
                skip_time_command_prefix(segment, &mut idx);
            }
            _ => break,
        }
    }
    if idx >= segment.len() {
        return;
    }
    let invocation = segment[idx..].to_vec();
    let command = shell_command_basename(&invocation[0]);
    invocations.push(invocation.clone());

    if matches!(command, "sh" | "bash" | "dash" | "ksh" | "zsh") {
        if let Some(payload) = shell_c_payload(&invocation) {
            collect_shell_command_invocations(payload, depth.saturating_add(1), invocations);
        }
    } else if command == "eval" && invocation.len() > 1 {
        collect_shell_command_invocations(
            &invocation[1..].join(" "),
            depth.saturating_add(1),
            invocations,
        );
    } else if command == "xargs"
        && let Some(command_idx) = xargs_command_index(&invocation)
    {
        collect_shell_segment_invocation(
            &invocation[command_idx..],
            depth.saturating_add(1),
            invocations,
        );
    } else if command == "find" {
        for exec_idx in invocation.iter().enumerate().filter_map(|(idx, word)| {
            matches!(word.as_str(), "-exec" | "-execdir" | "-ok" | "-okdir").then_some(idx + 1)
        }) {
            if exec_idx < invocation.len() {
                collect_shell_segment_invocation(
                    &invocation[exec_idx..],
                    depth.saturating_add(1),
                    invocations,
                );
            }
        }
    }
}

fn shell_c_payload(invocation: &[String]) -> Option<&str> {
    let mut idx = 1usize;
    while idx < invocation.len() {
        let arg = invocation[idx].as_str();
        if arg == "--" {
            return None;
        }
        if arg == "-c"
            || (arg.starts_with('-')
                && !arg.starts_with("--")
                && arg[1..].chars().any(|flag| flag == 'c'))
        {
            return invocation.get(idx + 1).map(String::as_str);
        }
        if !arg.starts_with('-') {
            return None;
        }
        idx += 1;
    }
    None
}

fn xargs_command_index(invocation: &[String]) -> Option<usize> {
    let mut idx = 1usize;
    while idx < invocation.len() {
        let arg = invocation[idx].as_str();
        if arg == "--" {
            return (idx + 1 < invocation.len()).then_some(idx + 1);
        }
        if matches!(
            arg,
            "-a" | "--arg-file"
                | "-d"
                | "--delimiter"
                | "-E"
                | "-e"
                | "--eof"
                | "-I"
                | "--replace"
                | "-L"
                | "-l"
                | "--max-lines"
                | "-n"
                | "--max-args"
                | "-P"
                | "--max-procs"
                | "-s"
                | "--max-chars"
        ) {
            idx = idx.saturating_add(2);
            continue;
        }
        if arg.starts_with('-') {
            idx += 1;
            continue;
        }
        return Some(idx);
    }
    None
}

fn embedded_shell_commands(command: &str) -> Vec<String> {
    let bytes = command.as_bytes();
    let mut nested = Vec::new();
    let mut idx = 0usize;
    let mut in_single = false;
    let mut in_double = false;
    let mut escaped = false;
    while idx < bytes.len() {
        let byte = bytes[idx];
        if escaped {
            escaped = false;
            idx += 1;
            continue;
        }
        if byte == b'\\' && !in_single {
            escaped = true;
            idx += 1;
            continue;
        }
        if byte == b'\'' && !in_double {
            in_single = !in_single;
            idx += 1;
            continue;
        }
        if byte == b'"' && !in_single {
            in_double = !in_double;
            idx += 1;
            continue;
        }
        if in_single {
            idx += 1;
            continue;
        }
        let substitution = (byte == b'$' || byte == b'<' || byte == b'>')
            && bytes.get(idx + 1) == Some(&b'(')
            && !(byte == b'$' && bytes.get(idx + 2) == Some(&b'('));
        if substitution {
            let content_start = idx + 2;
            let mut cursor = content_start;
            let mut level = 1usize;
            let mut sub_single = false;
            let mut sub_double = false;
            let mut sub_escaped = false;
            while cursor < bytes.len() {
                let current = bytes[cursor];
                if sub_escaped {
                    sub_escaped = false;
                } else if current == b'\\' && !sub_single {
                    sub_escaped = true;
                } else if current == b'\'' && !sub_double {
                    sub_single = !sub_single;
                } else if current == b'"' && !sub_single {
                    sub_double = !sub_double;
                } else if !sub_single && !sub_double {
                    if current == b'(' {
                        level = level.saturating_add(1);
                    } else if current == b')' {
                        level = level.saturating_sub(1);
                        if level == 0 {
                            nested.push(command[content_start..cursor].to_string());
                            idx = cursor + 1;
                            break;
                        }
                    }
                }
                cursor += 1;
            }
            if level == 0 {
                continue;
            }
        } else if byte == b'`' {
            let content_start = idx + 1;
            let mut cursor = content_start;
            let mut tick_escaped = false;
            while cursor < bytes.len() {
                let current = bytes[cursor];
                if tick_escaped {
                    tick_escaped = false;
                } else if current == b'\\' {
                    tick_escaped = true;
                } else if current == b'`' {
                    nested.push(command[content_start..cursor].to_string());
                    idx = cursor + 1;
                    break;
                }
                cursor += 1;
            }
            if idx == cursor + 1 {
                continue;
            }
        }
        idx += 1;
    }
    nested
}

fn is_sudo_command_word(word: &str) -> bool {
    shell_command_basename(word) == "sudo"
}

fn shell_command_separator(word: &str) -> bool {
    matches!(word, "&&" | ";" | "|" | "&")
}

fn skip_env_command_prefix(words: &[String], idx: &mut usize) {
    while *idx < words.len() {
        let word = &words[*idx];
        if shell_command_separator(word) {
            return;
        }
        if word == "--" {
            *idx += 1;
            break;
        }
        if shell_assignment_word(word)
            || matches!(
                word.as_str(),
                "-" | "-i" | "--ignore-environment" | "-0" | "--null"
            )
        {
            *idx += 1;
            continue;
        }
        if matches!(word.as_str(), "-u" | "--unset" | "-C" | "--chdir") {
            *idx = (*idx).saturating_add(2);
            continue;
        }
        if word.starts_with("--unset=") || word.starts_with("--chdir=") {
            *idx += 1;
            continue;
        }
        break;
    }
}

fn shell_command_basename(word: &str) -> &str {
    let basename = word
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(word)
        .trim_start_matches(['(', '{'])
        .trim_end_matches([')', '}']);
    basename
        .strip_suffix(".exe")
        .or_else(|| basename.strip_suffix(".com"))
        .unwrap_or(basename)
}

fn skip_command_builtin_prefix(words: &[String], idx: &mut usize) {
    while *idx < words.len() {
        match words[*idx].as_str() {
            "-p" => *idx += 1,
            "--" => {
                *idx += 1;
                break;
            }
            _ => break,
        }
    }
}

fn skip_time_command_prefix(words: &[String], idx: &mut usize) {
    while *idx < words.len() {
        let word = words[*idx].as_str();
        if matches!(
            word,
            "-p" | "--portability" | "-v" | "--verbose" | "--quiet"
        ) {
            *idx += 1;
        } else if matches!(word, "-o" | "--output" | "-f" | "--format") {
            *idx = (*idx).saturating_add(2);
        } else if word.starts_with("--output=") || word.starts_with("--format=") {
            *idx += 1;
        } else if word == "--" {
            *idx += 1;
            break;
        } else {
            break;
        }
    }
}

fn shell_command_keyword(word: &str) -> bool {
    matches!(
        word,
        "if" | "then" | "do" | "while" | "until" | "!" | "{" | "}" | "(" | ")"
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
    if command_invokes_sudo(command) {
        return Some(
            "bash advisory: sudo auth is local only. Dext will run sudo through its local preauth path; never type sudo passwords into chat or steering input."
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
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' && !in_single {
            let Some(next) = chars.peek().copied() else {
                current.push(ch);
                break;
            };
            if !in_double || matches!(next, '$' | '`' | '"' | '\\' | '\n') {
                let escaped = chars.next().expect("peeked escaped shell character");
                if escaped != '\n' {
                    current.push(escaped);
                }
                continue;
            }
            current.push(ch);
            continue;
        }
        match ch {
            '\'' if !in_double => in_single = !in_single,
            '"' if !in_single => in_double = !in_double,
            '\n' if !in_single && !in_double => {
                if !current.is_empty() {
                    words.push(std::mem::take(&mut current));
                }
                words.push(";".to_string());
            }
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
        "http" => {
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
            let shell_loop = (lower.contains("for ") || lower.contains("while "))
                && (lower.contains("; do")
                    || lower.lines().any(|line| {
                        let line = line.trim_start();
                        line == "do" || line.starts_with("do ")
                    }));
            let loopish =
                has_url && (shell_loop || lower.contains("xargs") || lower.contains("parallel"));
            let bulk_query = lower.contains("size=")
                || lower.contains("limit=")
                || lower.contains("offset=")
                || lower.contains("symbols=")
                || lower.contains("ids=")
                || lower.contains("batch")
                || lower.contains("bulk");
            loopish || (has_url && bulk_query)
        }
        "http" => {
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

pub(crate) fn search_tool_arg_exec_escape(name: &str, arg: &str) -> bool {
    match name {
        "fd" => {
            arg.starts_with("--exec")
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg[1..].chars().any(|flag| matches!(flag, 'x' | 'X')))
        }
        "rg" => {
            matches!(
                arg,
                "--pre" | "--pre-glob" | "--search-zip" | "--hostname-bin"
            ) || arg.starts_with("--pre=")
                || arg.starts_with("--pre-glob=")
                || arg.starts_with("--search-zip=")
                || arg.starts_with("--hostname-bin=")
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg[1..].chars().any(|flag| flag == 'z'))
        }
        _ => false,
    }
}

fn search_tool_arg_changes_operands(name: &str, arg: &str) -> bool {
    match name {
        "fd" => {
            matches!(arg, "--and" | "--base-directory" | "--search-path")
                || arg.starts_with("--and=")
                || arg.starts_with("--base-directory=")
                || arg.starts_with("--search-path=")
        }
        "rg" => {
            matches!(
                arg,
                "-e" | "--regexp" | "-f" | "--file" | "--files" | "--config-path"
            ) || arg.starts_with("--regexp=")
                || arg.starts_with("--file=")
                || arg.starts_with("--config-path=")
        }
        _ => false,
    }
}

fn search_tool_option_takes_value(name: &str, arg: &str) -> bool {
    match name {
        "fd" => matches!(
            arg,
            "-d" | "--max-depth"
                | "--min-depth"
                | "--exact-depth"
                | "-t"
                | "--type"
                | "-e"
                | "--extension"
                | "-E"
                | "--exclude"
                | "-c"
                | "--color"
                | "-j"
                | "--threads"
                | "--size"
                | "--owner"
                | "--ignore-file"
                | "--max-results"
                | "--path-separator"
                | "--format"
                | "--changed-within"
                | "--changed-before"
                | "--change-newer-than"
                | "--change-older-than"
        ),
        "rg" => matches!(
            arg,
            "-A" | "--after-context"
                | "-B"
                | "--before-context"
                | "-C"
                | "--context"
                | "-E"
                | "--encoding"
                | "-g"
                | "--glob"
                | "--iglob"
                | "-j"
                | "--threads"
                | "-m"
                | "--max-count"
                | "-M"
                | "--max-columns"
                | "--max-filesize"
                | "-r"
                | "--replace"
                | "-t"
                | "--type"
                | "-T"
                | "--type-not"
                | "--type-add"
                | "--type-clear"
                | "--ignore-file"
                | "--sort"
                | "--sortr"
                | "--color"
                | "--colors"
                | "--engine"
                | "--dfa-size-limit"
                | "--regex-size-limit"
                | "--path-separator"
                | "--context-separator"
                | "--field-context-separator"
                | "--field-match-separator"
                | "--hyperlink-format"
        ),
        _ => false,
    }
}

pub(crate) fn search_tool_extra_args_issue(name: &str, extra: &[String]) -> Option<String> {
    let mut value_for: Option<&str> = None;
    for arg in extra {
        if let Some(option) = value_for.take() {
            if arg == "--" {
                return Some(format!("option '{option}' cannot use '--' as its value"));
            }
            continue;
        }
        if search_tool_arg_exec_escape(name, arg) {
            return Some(format!(
                "blocked subprocess-execution flag '{arg}'; use bash so command risk and approval policy apply"
            ));
        }
        if search_tool_arg_changes_operands(name, arg) {
            return Some(format!(
                "extra_args flag '{arg}' can replace or add search operands; provide search data only through pattern/path"
            ));
        }
        if arg == "-" || arg == "--" || !arg.starts_with('-') {
            return Some(format!(
                "extra_args token '{arg}' is positional; provide search data through pattern/path and keep extra_args to options and their values"
            ));
        }
        if search_tool_option_takes_value(name, arg) {
            value_for = Some(arg);
        }
    }
    value_for.map(|option| format!("option '{option}' requires a value"))
}

fn search_tool_input_exec_escape(name: &str, input: &Value) -> bool {
    str_array(&input["extra_args"])
        .iter()
        .any(|arg| search_tool_arg_exec_escape(name, arg))
}

pub(crate) fn classify_command_risk(name: &str, input: &Value) -> CommandRisk {
    match name {
        "bash" => {
            let command = input["command"].as_str().unwrap_or("");
            classify_bash_command_risk(command)
        }
        "http" => classify_http_risk(input),
        "git_commit" => CommandRisk::Danger,
        "write_file" | "edit_file" | "multi_edit" | "todo_write" => CommandRisk::Write,
        "fd" | "rg" if search_tool_input_exec_escape(name, input) => CommandRisk::Danger,
        "read_file" | "read_symbol" | "fd" | "rg" | "jq" | "fzf" | "git_diff" | "git_log"
        | "todo_read" => CommandRisk::Read,
        "awk" => {
            let Some(raw_args) = input["args"].as_array() else {
                return CommandRisk::Danger;
            };
            if raw_args.iter().any(|arg| !arg.is_string()) {
                return CommandRisk::Danger;
            }
            let args = str_array(&input["args"]);
            if awk_args_issue(&args).is_some() {
                CommandRisk::Danger
            } else {
                CommandRisk::Read
            }
        }
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

fn classify_csvkit_risk(input: &Value) -> CommandRisk {
    let subcommand = input["subcommand"].as_str().unwrap_or_default();
    if !csvkit_subcommand_allowed(subcommand) {
        return CommandRisk::Danger;
    }
    let args = str_array(&input["args"]);
    if subcommand == "csvsql"
        || args.iter().any(|arg| {
            arg == "--in-place"
                || arg.starts_with("--in-place=")
                || arg == "--insert"
                || arg.starts_with("--insert=")
                || arg == "--db"
                || arg.starts_with("--db=")
        })
    {
        CommandRisk::Danger
    } else if args_contain_write_or_escape(&args) {
        CommandRisk::Write
    } else {
        CommandRisk::Read
    }
}

fn shell_search_tool_exec_escape(chunk: &str) -> bool {
    let words = shell_words(chunk);
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
        if shell_assignment_word(word) || shell_command_keyword(word) {
            idx += 1;
            continue;
        }
        match shell_command_basename(word) {
            "env" => {
                idx += 1;
                skip_env_command_prefix(&words, &mut idx);
                continue;
            }
            "command" | "builtin" => {
                idx += 1;
                skip_command_builtin_prefix(&words, &mut idx);
                continue;
            }
            "time" => {
                idx += 1;
                skip_time_command_prefix(&words, &mut idx);
                continue;
            }
            _ => {}
        }
        let command = shell_command_basename(word);
        if matches!(command, "fd" | "rg")
            && words[idx.saturating_add(1)..]
                .iter()
                .take_while(|arg| !shell_command_separator(arg) && arg.as_str() != "--")
                .any(|arg| search_tool_arg_exec_escape(command, arg))
        {
            return true;
        }
        command_position = false;
        idx += 1;
    }
    false
}

fn shell_chunk_is_read_only(chunk: &str) -> bool {
    let words = shell_words(chunk);
    let first = words.first().map(String::as_str).unwrap_or("");
    match first {
        "echo" | "ls" | "cat" | "pwd" | "whoami" | "id" | "printenv" | "grep" | "head" | "tail"
        | "stat" | "which" | "basename" | "dirname" | "realpath" | "readlink" | "wc" | "sort"
        | "uniq" | "cut" | "tr" | "jq" => true,
        "env" => {
            let mut command_idx = 1usize;
            skip_env_command_prefix(&words, &mut command_idx);
            if command_idx >= words.len() {
                true
            } else {
                shell_chunk_is_read_only(&words[command_idx..].join(" "))
            }
        }
        "fd" | "rg" => !shell_search_tool_exec_escape(chunk),
        // These are read-only as filters but have in-place/exec escape hatches.
        // `-i` is the only short sed flag containing 'i', so any short-flag
        // cluster with 'i' (e.g. -i, -i.bak, -ni) means an in-place edit.
        "sed" => !words.iter().skip(1).any(|w| {
            w == "--in-place"
                || w.starts_with("--in-place=")
                || (w.starts_with('-') && !w.starts_with("--") && w.contains('i'))
        }),
        "find" => !words.iter().skip(1).any(|w| {
            matches!(
                w.as_str(),
                "-delete" | "-exec" | "-execdir" | "-ok" | "-okdir" | "-fls"
            ) || w.starts_with("-fprint")
        }),
        "awk" => !chunk.contains("system("),
        "git" => match words.get(1).map(String::as_str).unwrap_or("") {
            "status" | "diff" | "log" | "show" | "rev-parse" => true,
            // `git branch` only lists when every remaining arg is a list-style
            // flag; positional args create branches and -d/-D/-m/-M/-c/-C/-f
            // mutate them.
            "branch" => words.iter().skip(2).all(|w| {
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
                    w.as_str(),
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
    if shell_command_invocations(&lower)
        .iter()
        .any(|invocation| shell_invocation_is_dangerous(invocation))
    {
        return true;
    }
    if command_segments(&lower)
        .iter()
        .any(|segment| shell_search_tool_exec_escape(segment))
    {
        return true;
    }
    (lower.contains("curl") || lower.contains("wget"))
        && (lower.contains("| sh")
            || lower.contains("|sh")
            || lower.contains("| bash")
            || lower.contains("|bash"))
}

fn shell_invocation_is_dangerous(invocation: &[String]) -> bool {
    let Some(first) = invocation.first() else {
        return false;
    };
    let command = shell_command_basename(first);
    if is_sudo_command_word(first)
        || matches!(
            command,
            "rm" | "rmdir"
                | "unlink"
                | "shred"
                | "wipefs"
                | "truncate"
                | "shutdown"
                | "reboot"
                | "poweroff"
                | "halt"
                | "eval"
        )
        || command == "dd"
        || command.starts_with("mkfs")
    {
        return true;
    }

    match command {
        "find" => invocation.iter().any(|arg| arg == "-delete"),
        "git" => git_invocation_is_dangerous(invocation),
        "python" | "python3" | "perl" | "node" | "nodejs" => {
            interpreter_inline_code_is_dangerous(command, invocation)
        }
        "docker" | "podman" => container_invocation_is_dangerous(invocation),
        "curl" => curl_invocation_is_dangerous(invocation),
        "wget" => wget_invocation_is_dangerous(invocation),
        "http" | "xh" => invocation
            .iter()
            .skip(1)
            .any(|arg| http_method_is_dangerous(arg)),
        "terraform" | "tofu" => invocation
            .iter()
            .skip(1)
            .any(|arg| matches!(arg.as_str(), "apply" | "destroy" | "import" | "taint")),
        "kubectl" => invocation.iter().skip(1).any(|arg| {
            matches!(
                arg.as_str(),
                "apply"
                    | "create"
                    | "delete"
                    | "edit"
                    | "exec"
                    | "patch"
                    | "replace"
                    | "rollout"
                    | "run"
                    | "scale"
                    | "set"
            )
        }),
        _ => false,
    }
}

fn interpreter_inline_code_is_dangerous(command: &str, invocation: &[String]) -> bool {
    let args = &invocation[1..];
    if args.is_empty() {
        return true;
    }
    let inline = args.iter().any(|arg| match command {
        "python" | "python3" => arg == "-" || arg == "-c" || arg.starts_with("-c") && arg.len() > 2,
        "perl" => {
            arg == "-"
                || arg == "-e"
                || arg == "-E"
                || arg.starts_with("-e")
                || arg.starts_with("-E")
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && !arg.starts_with("-I")
                    && !arg.starts_with("-F")
                    && !arg.starts_with("-M")
                    && !arg.starts_with("-m")
                    && arg[1..].chars().any(|flag| matches!(flag, 'e' | 'E')))
        }
        "node" | "nodejs" => {
            arg == "-"
                || matches!(arg.as_str(), "-e" | "--eval" | "-p" | "--print")
                || arg.starts_with("-e")
                || arg.starts_with("-p")
                || arg.starts_with("--eval=")
                || arg.starts_with("--print=")
        }
        _ => false,
    });
    if inline
        || args
            .iter()
            .any(|arg| matches!(arg.as_str(), "-h" | "--help" | "-v" | "-V" | "--version"))
    {
        return inline;
    }
    !interpreter_has_explicit_program(command, args)
}

fn interpreter_has_explicit_program(command: &str, args: &[String]) -> bool {
    let mut index = 0usize;
    while index < args.len() {
        let arg = args[index].as_str();
        if arg == "--" {
            return args.get(index + 1).is_some_and(|next| next != "-");
        }
        match command {
            "python" | "python3" => {
                if arg == "-m" {
                    return args.get(index + 1).is_some();
                }
                if arg.starts_with("-m") && arg.len() > 2 {
                    return true;
                }
                if matches!(arg, "-W" | "-X" | "--check-hash-based-pycs") {
                    index = index.saturating_add(2);
                    continue;
                }
            }
            "perl" if matches!(arg, "-I" | "-F") => {
                index = index.saturating_add(2);
                continue;
            }
            "node" | "nodejs"
                if matches!(
                    arg,
                    "-r" | "--require"
                        | "--loader"
                        | "--import"
                        | "--conditions"
                        | "--input-type"
                        | "--inspect-port"
                ) =>
            {
                index = index.saturating_add(2);
                continue;
            }
            _ => {}
        }
        if arg.starts_with('-') {
            index += 1;
            continue;
        }
        return true;
    }
    false
}

fn git_invocation_is_dangerous(invocation: &[String]) -> bool {
    let Some(subcommand_idx) = git_subcommand_index(invocation, 0) else {
        return false;
    };
    let subcommand = invocation[subcommand_idx].as_str();
    let args = &invocation[subcommand_idx.saturating_add(1)..];
    match subcommand {
        "push" | "clean" | "checkout" => true,
        "stash" => args
            .iter()
            .find(|arg| !arg.starts_with('-'))
            .is_some_and(|action| matches!(action.as_str(), "drop" | "clear" | "pop")),
        "reset" => args
            .iter()
            .any(|arg| arg == "--hard" || arg.starts_with("--hard=")),
        "restore" => args
            .iter()
            .any(|arg| !arg.starts_with('-') || arg == "--worktree"),
        "branch" => args.iter().any(|arg| {
            matches!(arg.as_str(), "-d" | "-D" | "--delete")
                || (arg.starts_with('-')
                    && !arg.starts_with("--")
                    && arg[1..].chars().any(|flag| matches!(flag, 'd' | 'D')))
        }),
        _ => false,
    }
}

fn container_invocation_is_dangerous(invocation: &[String]) -> bool {
    invocation.iter().skip(1).any(|arg| {
        matches!(
            arg.as_str(),
            "attach"
                | "build"
                | "commit"
                | "compose"
                | "container"
                | "cp"
                | "create"
                | "exec"
                | "image"
                | "kill"
                | "login"
                | "logout"
                | "network"
                | "node"
                | "pause"
                | "plugin"
                | "push"
                | "rename"
                | "restart"
                | "rm"
                | "rmi"
                | "run"
                | "secret"
                | "service"
                | "stack"
                | "start"
                | "stop"
                | "swarm"
                | "system"
                | "tag"
                | "trust"
                | "unpause"
                | "update"
                | "volume"
        )
    })
}

fn curl_invocation_is_dangerous(invocation: &[String]) -> bool {
    let mut idx = 1usize;
    while idx < invocation.len() {
        let arg = invocation[idx].as_str();
        if let Some(method) = arg.strip_prefix("--request=") {
            if http_method_is_dangerous(method) {
                return true;
            }
        } else if arg == "--request" || arg == "-X" {
            if invocation
                .get(idx + 1)
                .is_some_and(|method| http_method_is_dangerous(method))
            {
                return true;
            }
            idx = idx.saturating_add(1);
        } else if let Some(method) = arg.strip_prefix("-X") {
            if !method.is_empty() && http_method_is_dangerous(method) {
                return true;
            }
        } else if matches!(
            arg,
            "-d" | "--data"
                | "--data-ascii"
                | "--data-binary"
                | "--data-raw"
                | "--data-urlencode"
                | "-F"
                | "--form"
                | "--form-string"
                | "-T"
                | "--upload-file"
                | "--json"
        ) || [
            "--data=",
            "--data-ascii=",
            "--data-binary=",
            "--data-raw=",
            "--data-urlencode=",
            "--form=",
            "--form-string=",
            "--upload-file=",
            "--json=",
        ]
        .iter()
        .any(|prefix| arg.starts_with(prefix))
        {
            return true;
        }
        idx += 1;
    }
    false
}

fn wget_invocation_is_dangerous(invocation: &[String]) -> bool {
    let mut idx = 1usize;
    while idx < invocation.len() {
        let arg = invocation[idx].as_str();
        if matches!(
            arg,
            "--post-data" | "--post-file" | "--body-data" | "--body-file"
        ) || [
            "--post-data=",
            "--post-file=",
            "--body-data=",
            "--body-file=",
        ]
        .iter()
        .any(|prefix| arg.starts_with(prefix))
        {
            return true;
        }
        if arg == "--method" {
            if invocation
                .get(idx + 1)
                .is_some_and(|method| http_method_is_dangerous(method))
            {
                return true;
            }
            idx = idx.saturating_add(1);
        } else if let Some(method) = arg.strip_prefix("--method=")
            && http_method_is_dangerous(method)
        {
            return true;
        }
        idx += 1;
    }
    false
}

fn http_method_is_dangerous(method: &str) -> bool {
    matches!(
        method.to_ascii_uppercase().as_str(),
        "POST" | "PUT" | "PATCH" | "DELETE" | "CONNECT"
    )
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

pub(crate) const BASH_PIPEFAIL_PREFIX: &str = "set -o pipefail\n";

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

    Ok(format!("{BASH_PIPEFAIL_PREFIX}{command}"))
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

        let guarded = apply_bash_guardrails("echo pipefail | cat").expect("guarded command");
        assert_eq!(guarded, "set -o pipefail\necho pipefail | cat");
        let guarded = apply_bash_guardrails("set -euo pipefail\necho ok").expect("guarded command");
        assert_eq!(
            guarded, "set -o pipefail\nset -euo pipefail\necho ok",
            "an incidental or existing pipefail token must not suppress the guard"
        );

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
            &json!({"command": "python3 - <<'PY'\nurl = 'https://api.example.com/v1/item/abc'\nwarnings = [warning for warning in []]\nPY"})
        ));
        assert!(looks_like_bulk_network_call(
            "bash",
            &json!({"command": "for t in a b c\ndo\n  curl -s https://api.example.com/v1/item/$t\ndone"})
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
        for command in [
            "rg --pre 'sh -c evil' needle .",
            "rg -z needle archives/",
            "fd needle . -x sh -c evil",
            "env FOO=bar fd needle . --exec-batch touch",
            "/usr/bin/env FOO=bar fd needle . --exec-batch touch",
            "command -- rg --pre sh needle .",
            "time -p /usr/bin/rg --hostname-bin sh --debug needle .",
            "/usr/bin/rg --hostname-bin sh --debug needle .",
            "env FOO=bar rm stale.txt",
            "/bin/rm stale.txt",
            "command -- rm stale.txt",
            "time -p unlink stale.txt",
            "bash -c 'rm stale.txt'",
            "sh -eu -c 'git -C repo push origin main'",
            "eval 'rm stale.txt'",
            "echo $(rm stale.txt)",
            "cat <(rm stale.txt)",
            "echo `rm stale.txt`",
            "find . -exec rm {} +",
            "printf '%s\\n' stale.txt | xargs rm",
            "git -C repo push origin main",
            "git --git-dir=.git push origin main",
            "git reset --hard HEAD~1",
            "git checkout -- .",
            "git.exe checkout -- .",
            "git stash drop",
            "git stash clear",
            "git stash pop",
            "python3 -c 'import shutil; shutil.rmtree(\"build\")'",
            "C:/Python/python.exe -c 'import shutil; shutil.rmtree(\"build\")'",
            "python3 -c'import os; os.unlink(\"stale\")'",
            "printf 'import os; os.unlink(\"stale\")' | python3",
            "perl -e 'unlink \"stale.txt\"'",
            "perl -E'unlink \"stale.txt\"'",
            "node -e 'require(\"fs\").rmSync(\"build\", {recursive:true})'",
            "node -p 'require(\"fs\").rmSync(\"build\", {recursive:true})'",
            "docker system prune",
            "curl --request=DELETE https://example.invalid/item/1",
            "curl --data=x=1 https://example.invalid/items",
            "curl --json={} https://example.invalid/items",
            "wget --method=PATCH --body-data=x https://example.invalid/items",
            "http POST https://example.invalid/items",
            "kubectl delete pod example",
            "terraform destroy -auto-approve",
        ] {
            assert_eq!(
                classify_command_risk("bash", &json!({"command": command})),
                CommandRisk::Danger,
                "{command}"
            );
        }
        assert_eq!(
            classify_command_risk(
                "rg",
                &json!({"pattern": "needle", "extra_args": ["--pre", "sh"]})
            ),
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk(
                "fd",
                &json!({"pattern": "needle", "extra_args": ["-HIx", "touch"]})
            ),
            CommandRisk::Danger
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
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "echo 'rm stale.txt'"})),
            CommandRisk::Read,
            "quoted prose must not look like an rm invocation"
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "bash -c 'printf ok'"})),
            CommandRisk::Write,
            "nested shell remains write-risk but must not be mislabeled danger"
        );
        assert_ne!(
            classify_command_risk("bash", &json!({"command": "git -C . status"})),
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "python3 script.py"})),
            CommandRisk::Write,
            "script execution remains ordinary write-risk"
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "python.exe script.py"})),
            CommandRisk::Write,
            "Windows-suffixed script execution remains ordinary write-risk"
        );
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git stash list"})),
            CommandRisk::Write,
            "non-destructive stash operations are not promoted to danger"
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
            CommandRisk::Danger
        );
        // Combined short flags still mutate (-dr deletes a remote-tracking branch).
        assert_eq!(
            classify_command_risk("bash", &json!({"command": "git branch -dr origin/x"})),
            CommandRisk::Danger
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
        assert!(command_invokes_sudo("sudo apt update"));
        assert!(command_invokes_sudo("sudo -v"));
        assert!(command_invokes_sudo("echo ok && sudo apt update"));
        assert!(command_invokes_sudo("sudo -n true; sudo apt update"));
        assert!(command_invokes_sudo("echo sudo && sudo apt update"));
        assert!(command_invokes_sudo("/usr/bin/sudo apt update"));
        assert!(command_invokes_sudo("/usr/bin/env FOO=bar sudo apt update"));
        assert!(command_invokes_sudo("command -- sudo apt update"));
        assert!(command_invokes_sudo("time -p sudo apt update"));
        assert!(command_invokes_sudo("set -euo pipefail\nsudo apt update"));
        assert!(command_invokes_sudo(
            "set -euo pipefail\nsudo -n true\nsudo apt update"
        ));
        assert!(command_invokes_sudo("env FOO=bar sudo apt update"));
        assert!(command_invokes_sudo("env -- sudo apt update"));
        assert!(command_invokes_sudo("sudo -u nobody true"));
        assert!(command_invokes_sudo("sudo -unobody true"));
        assert!(command_invokes_sudo("sudo -p -n true"));
        assert!(command_invokes_sudo("sudo --prompt -n true"));
        assert!(command_invokes_sudo("sudo -n true"));
        assert!(command_invokes_sudo("sudo -nv"));
        assert!(command_invokes_sudo("sudo -u nobody -n true"));
        assert!(command_invokes_sudo(
            "sudo --user=nobody --non-interactive true"
        ));
        assert!(command_invokes_sudo("sudo --prompt=Password -n true"));
        assert!(command_invokes_sudo("sudo --non-interactive true"));
        assert!(command_invokes_sudo("bash -c 'sudo apt update'"));
        assert!(command_invokes_sudo("echo $(sudo -n true)"));
        assert!(command_invokes_sudo("find . -exec sudo -n true \\;"));
        assert!(!command_invokes_sudo("grep sudo README.md"));
        assert!(!command_invokes_sudo("echo 'sudo apt update'"));
        assert!(!command_invokes_sudo("printf '%s' sudo"));
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
            CommandRisk::Danger
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
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk("awk", &json!({"args": ["-f", "program.awk"]})),
            CommandRisk::Danger
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
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk(
                "csvkit",
                &json!({"subcommand": "csvsql", "args": ["--db", "sqlite:///data.db", "data.csv"]})
            ),
            CommandRisk::Danger
        );
        assert_eq!(
            classify_command_risk(
                "csvkit",
                &json!({"subcommand": "sh", "args": ["-c", "touch pwned"]})
            ),
            CommandRisk::Danger
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
