//! Privacy policy and redaction: deciding whether text may leave the machine
//! (clipboard, transcripts, provider payloads) and scrubbing what may not.

use std::path::{Component, Path};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{byte_suffix_at_char_boundary, canonicalize_read_tool_path, provider, str_array};

pub(crate) fn text_is_potential_local_secret(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    if trimmed.contains("accessToken") || trimmed.contains("access_token") {
        return true;
    }
    if slash_login_contains_secret(trimmed) {
        return true;
    }
    if contains_secretish_assignment(trimmed) {
        return true;
    }
    if let Some(token) = strip_bearer_token(trimmed) {
        return token.chars().count() >= 8;
    }
    if contains_known_secret_token(trimmed) {
        return true;
    }
    if trimmed.starts_with('/') || trimmed.starts_with('{') {
        return false;
    }
    if looks_like_public_clipboard_reference(trimmed) {
        return false;
    }
    false
}

pub(crate) fn contains_secretish_assignment(text: &str) -> bool {
    text.split(|c: char| c.is_whitespace() || matches!(c, '&' | '?' | ';' | ','))
        .chain(text.lines())
        .any(secretish_assignment_has_value)
}

pub(crate) fn secretish_assignment_has_value(segment: &str) -> bool {
    let trimmed = segment.trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '{' | '}' | '[' | ']' | '(' | ')')
    });
    let Some((key, value)) = trimmed.split_once('=').or_else(|| trimmed.split_once(':')) else {
        return false;
    };
    let key = key
        .trim()
        .trim_matches(|c: char| !c.is_ascii_alphanumeric() && c != '_' && c != '-')
        .rsplit(['/', '.'])
        .next()
        .unwrap_or(key);
    let value = value.trim().trim_matches(|c: char| {
        c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '{' | '}' | '[' | ']' | '(' | ')')
    });
    let value_len = value.chars().count();
    (secretish_key_name(key) && value_len >= 6) || (secretish_code_key(key) && value_len >= 12)
}

pub(crate) fn compact_key_name(key: &str) -> String {
    key.chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .flat_map(|c| c.to_lowercase())
        .collect()
}

pub(crate) fn secretish_key_name(key: &str) -> bool {
    let compact = compact_key_name(key);
    compact == "auth"
        || compact == "authorization"
        || compact == "passwd"
        || compact == "pwd"
        || compact == "privatekey"
        || compact.ends_with("apikey")
        || compact.ends_with("token")
        || compact.contains("password")
        || compact.contains("secret")
}

pub(crate) fn secretish_code_key(key: &str) -> bool {
    matches!(
        compact_key_name(key).as_str(),
        "code" | "oauthcode" | "authorizationcode"
    )
}

pub(crate) fn strip_bearer_token(text: &str) -> Option<&str> {
    let prefix_len = "bearer".len();
    let prefix = text.get(..prefix_len)?;
    let rest = text.get(prefix_len..)?;
    if prefix.eq_ignore_ascii_case("bearer") && rest.chars().next().is_some_and(char::is_whitespace)
    {
        Some(rest.trim_start()).filter(|token| !token.is_empty())
    } else {
        None
    }
}

pub(crate) fn contains_known_secret_token(text: &str) -> bool {
    text.split(|c: char| {
        c.is_whitespace() || matches!(c, '/' | '\\' | '?' | '&' | '=' | ':' | ';' | ',' | '#')
    })
    .any(|raw| {
        let token = raw.trim_matches(|c: char| {
            c.is_whitespace()
                || matches!(
                    c,
                    '"' | '\'' | '`' | '<' | '>' | '(' | ')' | '[' | ']' | '{' | '}'
                )
        });
        let len = token.chars().count();
        if len < 8 {
            return false;
        }
        let lower = token.to_ascii_lowercase();
        lower.starts_with("sk-")
            || lower.starts_with("sk_")
            || lower.starts_with("xoxb-")
            || lower.starts_with("xoxp-")
            || lower.starts_with("ghp_")
            || lower.starts_with("github_pat_")
            || lower.starts_with("glpat-")
            || lower.starts_with("ya29.")
            || (lower.starts_with("ac_") && len >= 16)
            || (token.starts_with("AIza") && len >= 20)
    })
}

pub(crate) fn looks_like_public_clipboard_reference(text: &str) -> bool {
    let mut saw_reference = false;
    for token in text.split_whitespace().map(|token| {
        token.trim_matches(|c: char| c.is_whitespace() || matches!(c, '"' | '\'' | '`' | '<' | '>'))
    }) {
        if token.is_empty() {
            continue;
        }
        if !(looks_like_url_reference(token)
            || looks_like_social_handle(token)
            || looks_like_git_sha(token))
        {
            return false;
        }
        saw_reference = true;
    }
    saw_reference
}

pub(crate) fn looks_like_url_reference(token: &str) -> bool {
    let lower = token.to_ascii_lowercase();
    if lower.contains("://") {
        return !url_authority_has_userinfo(token);
    }
    if lower.starts_with("www.") {
        return true;
    }
    token.split_once('/').is_some_and(|(host, rest)| {
        if host.contains('@') {
            return false;
        }
        let host = host.split(':').next().unwrap_or(host);
        !rest.is_empty() && looks_like_domain_name(host)
    })
}

pub(crate) fn url_authority_has_userinfo(token: &str) -> bool {
    let Some((_, rest)) = token.split_once("://") else {
        return false;
    };
    let authority = rest.split(['/', '?', '#']).next().unwrap_or(rest);
    authority.contains('@')
}

pub(crate) fn looks_like_domain_name(host: &str) -> bool {
    if !host.contains('.') || host.starts_with('.') || host.ends_with('.') {
        return false;
    }
    let Some(tld) = host.rsplit('.').next() else {
        return false;
    };
    (2..=24).contains(&tld.len())
        && tld.chars().all(|c| c.is_ascii_alphabetic())
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.'))
}

pub(crate) fn looks_like_social_handle(token: &str) -> bool {
    let Some(handle) = token.strip_prefix('@') else {
        return (3..=15).contains(&token.len())
            && token.contains('_')
            && token.chars().any(|c| c.is_ascii_digit())
            && token.chars().any(|c| c.is_ascii_lowercase())
            && token
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    };
    !handle.is_empty()
        && handle.len() <= 15
        && handle
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_')
}

pub(crate) fn looks_like_git_sha(token: &str) -> bool {
    let len = token.len();
    matches!(len, 7..=12 | 40) && token.chars().all(|c| c.is_ascii_hexdigit())
}

pub(crate) fn slash_login_contains_secret(trimmed: &str) -> bool {
    let mut parts = trimmed.split_whitespace();
    let Some(cmd) = parts.next() else {
        return false;
    };
    if cmd != "/login" {
        return false;
    }
    let args: Vec<&str> = parts.collect();
    if args.len() < 2 {
        return false;
    }
    let secret = args[1..].join(" ");
    let lowered = secret.trim().to_ascii_lowercase();
    if provider::login_arg_requests_web_flow(&lowered)
        || provider::login_arg_requests_import(&lowered)
    {
        return false;
    }
    true
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct PrivacyPolicy {
    pub(crate) enabled: bool,
    pub(crate) strict_paths: bool,
    pub(crate) findings: PrivacyFindingCounts,
}

#[derive(Serialize, Deserialize, Clone, Debug, Default, PartialEq, Eq)]
#[serde(default)]
pub(crate) struct PrivacyFindingCounts {
    pub(crate) ssn: u64,
    pub(crate) credit_card: u64,
    pub(crate) api_key: u64,
    pub(crate) private_key: u64,
    pub(crate) account_number: u64,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct PrivacyRedaction {
    pub(crate) text: String,
    pub(crate) counts: PrivacyFindingCounts,
}

impl Default for PrivacyPolicy {
    fn default() -> Self {
        Self {
            enabled: true,
            strict_paths: false,
            findings: PrivacyFindingCounts::default(),
        }
    }
}

impl PrivacyPolicy {
    pub(crate) fn from_env() -> Self {
        let mut policy = Self::default();
        if let Ok(v) = std::env::var("DEXT_PRIVACY") {
            let normalized = v.trim().to_ascii_lowercase();
            policy.enabled = !matches!(normalized.as_str(), "0" | "false" | "no" | "off");
            if normalized == "strict" {
                policy.enabled = true;
                policy.strict_paths = true;
            }
        }
        policy
    }

    pub(crate) fn mode_label(&self) -> &'static str {
        if !self.enabled {
            "off"
        } else if self.strict_paths {
            "strict"
        } else {
            "redact"
        }
    }

    pub(crate) fn prompt_status_line(&self) -> String {
        if !self.enabled {
            "privacy=off".to_string()
        } else if self.strict_paths {
            "privacy=strict (sensitive-looking native read paths are blocked; other tool output is redacted before model context/session logs)".to_string()
        } else {
            "privacy=redact (user-readable files remain readable; private keys, secret assignments, and labeled SSNs/cards/accounts are redacted before model context/session logs)".to_string()
        }
    }

    pub(crate) fn status_text(&self) -> String {
        let mut out = format!(
            "privacy: {}\nstrict path guard: {}\nredacts: private keys, secret assignments, explicitly labeled SSNs/payment-card/account identifiers",
            self.mode_label(),
            if self.strict_paths { "on" } else { "off" }
        );
        if self.findings.total() > 0 {
            out.push_str(&format!(
                "\nredacted this session: {}",
                self.findings.summary()
            ));
        }
        out
    }

    pub(crate) fn redact_text(&self, text: &str) -> PrivacyRedaction {
        if !self.enabled || text.is_empty() {
            return PrivacyRedaction {
                text: text.to_string(),
                counts: PrivacyFindingCounts::default(),
            };
        }
        redact_sensitive_text(text)
    }

    pub(crate) fn redact_log_detail(&self, text: &str) -> String {
        if !self.enabled || text.is_empty() {
            return text.to_string();
        }
        redact_sensitive_text(text).text
    }

    pub(crate) fn apply_tool_output(
        &mut self,
        _tool_name: &str,
        _input: &Value,
        content: String,
    ) -> PrivacyRedaction {
        let mut redacted = self.redact_text(&content);
        if self.enabled && redacted.counts.total() > 0 {
            let summary = redacted.counts.summary();
            self.findings.add(&redacted.counts);
            redacted.text.push_str(&format!(
                "\n\n[privacy: redacted {summary}; raw values withheld]"
            ));
        }
        redacted
    }

    pub(crate) fn path_denial(
        &mut self,
        tool_name: &str,
        input: &Value,
        root: &Path,
    ) -> Option<String> {
        if !(self.enabled
            && self.strict_paths
            && matches!(
                tool_name,
                "read_file" | "read_symbol" | "fd" | "rg" | "jq" | "git_diff" | "git_log"
            ))
        {
            return None;
        }
        let path = match tool_name {
            "fd" | "rg" => input["path"].as_str().unwrap_or("."),
            _ => input["path"].as_str()?,
        };
        let resolved_sensitive = canonicalize_read_tool_path(root, path)
            .ok()
            .is_some_and(|resolved| privacy_sensitive_path(&resolved.to_string_lossy()));
        let sensitive_search_scope =
            matches!(tool_name, "fd" | "rg") && privacy_sensitive_search_scope(tool_name, input);
        if !privacy_sensitive_path(path) && !resolved_sensitive && !sensitive_search_scope {
            return None;
        }
        self.findings.private_key = self.findings.private_key.saturating_add(1);
        if sensitive_search_scope && !privacy_sensitive_path(path) && !resolved_sensitive {
            Some(format!(
                "[privacy] blocked {tool_name} because strict path mode does not allow hidden, ignored, symlink-following, or sensitive-glob search scope. Raw file content and sensitive paths withheld. Use `/privacy on` for redaction-only reads, or `/privacy off` for raw reads."
            ))
        } else {
            Some(format!(
                "[privacy] blocked {tool_name} for sensitive-looking path `{path}` because strict path mode is enabled. Raw file content withheld. Use `/privacy on` for redaction-only reads, or `/privacy off` for raw reads."
            ))
        }
    }
}

pub(crate) fn privacy_sensitive_search_scope(tool_name: &str, input: &Value) -> bool {
    if tool_name == "fd"
        && input["pattern"]
            .as_str()
            .is_some_and(privacy_sensitive_path)
    {
        return true;
    }
    let args = str_array(&input["extra_args"]);
    let mut expect_glob = false;
    for arg in args {
        if expect_glob {
            if privacy_sensitive_path(&arg) {
                return true;
            }
            expect_glob = false;
            continue;
        }
        if matches!(arg.as_str(), "-g" | "--glob" | "--iglob") {
            expect_glob = true;
            continue;
        }
        if let Some(glob) = arg
            .strip_prefix("--glob=")
            .or_else(|| arg.strip_prefix("--iglob="))
            && privacy_sensitive_path(glob)
        {
            return true;
        }
        if matches!(
            arg.as_str(),
            "-H" | "--hidden"
                | "-L"
                | "--follow"
                | "-u"
                | "-uu"
                | "-uuu"
                | "--no-ignore"
                | "--no-ignore-vcs"
                | "--no-ignore-global"
                | "--no-ignore-parent"
                | "--no-ignore-dot"
                | "--no-ignore-exclude"
                | "--no-ignore-files"
        ) {
            return true;
        }
        if arg.starts_with('-')
            && !arg.starts_with("--")
            && arg[1..].chars().any(|flag| matches!(flag, 'H' | 'L' | 'u'))
        {
            return true;
        }
    }
    false
}

impl PrivacyFindingCounts {
    pub(crate) fn add(&mut self, other: &Self) {
        self.ssn = self.ssn.saturating_add(other.ssn);
        self.credit_card = self.credit_card.saturating_add(other.credit_card);
        self.api_key = self.api_key.saturating_add(other.api_key);
        self.private_key = self.private_key.saturating_add(other.private_key);
        self.account_number = self.account_number.saturating_add(other.account_number);
    }

    pub(crate) fn total(&self) -> u64 {
        self.ssn
            .saturating_add(self.credit_card)
            .saturating_add(self.api_key)
            .saturating_add(self.private_key)
            .saturating_add(self.account_number)
    }

    pub(crate) fn summary(&self) -> String {
        let mut parts = Vec::new();
        if self.ssn > 0 {
            parts.push(format!("{} SSN", self.ssn));
        }
        if self.credit_card > 0 {
            parts.push(format!("{} payment-card", self.credit_card));
        }
        if self.api_key > 0 {
            parts.push(format!("{} API/token", self.api_key));
        }
        if self.private_key > 0 {
            parts.push(format!("{} private-key/path", self.private_key));
        }
        if self.account_number > 0 {
            parts.push(format!("{} account identifier", self.account_number));
        }
        if parts.is_empty() {
            "0 items".to_string()
        } else {
            parts.join(", ")
        }
    }
}

pub(crate) fn redact_sensitive_text(text: &str) -> PrivacyRedaction {
    let mut counts = PrivacyFindingCounts::default();
    let mut out = redact_private_key_blocks(text, &mut counts);
    out = redact_secret_assignments(&out, &mut counts);
    out = redact_digit_sequences(&out, &mut counts);
    PrivacyRedaction { text: out, counts }
}

pub(crate) fn redact_private_key_blocks(text: &str, counts: &mut PrivacyFindingCounts) -> String {
    if !text.contains("PRIVATE KEY-----") {
        return text.to_string();
    }
    let mut out = String::with_capacity(text.len());
    let mut in_key = false;
    for segment in text.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let line_ending = &segment[line.len()..];
        let trimmed = line.trim();
        if !in_key && trimmed.starts_with("-----BEGIN ") && trimmed.contains("PRIVATE KEY-----") {
            counts.private_key = counts.private_key.saturating_add(1);
            out.push_str("[REDACTED_PRIVATE_KEY]");
            out.push_str(line_ending);
            in_key = true;
            continue;
        }
        if in_key {
            if trimmed.starts_with("-----END ") && trimmed.contains("PRIVATE KEY-----") {
                in_key = false;
            }
            continue;
        }
        out.push_str(segment);
    }
    out
}

pub(crate) fn redact_secret_assignments(text: &str, counts: &mut PrivacyFindingCounts) -> String {
    let mut spans = Vec::new();
    let mut line_start = 0usize;
    for segment in text.split_inclusive('\n') {
        let line = segment.strip_suffix('\n').unwrap_or(segment);
        let line = line.strip_suffix('\r').unwrap_or(line);
        for (start, end) in secret_assignment_value_spans(line) {
            counts.api_key = counts.api_key.saturating_add(1);
            spans.push((
                line_start.saturating_add(start),
                line_start.saturating_add(end),
                "[REDACTED_SECRET]",
            ));
        }
        line_start = line_start.saturating_add(segment.len());
    }
    redact_by_labeled_spans(text, spans)
}

pub(crate) fn secret_assignment_value_spans(line: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut scan_from = 0usize;
    while scan_from < line.len() {
        let Some((delimiter_offset, ch)) = line[scan_from..]
            .char_indices()
            .find(|(_, ch)| matches!(ch, '=' | ':'))
        else {
            break;
        };
        let delimiter = scan_from.saturating_add(delimiter_offset);
        scan_from = delimiter.saturating_add(ch.len_utf8());
        if ch == '=' && line[delimiter..].starts_with("==") {
            continue;
        }
        let prefix = line[..delimiter].trim_end();
        let key = prefix
            .rsplit(|ch: char| ch.is_whitespace() || matches!(ch, '{' | '[' | '(' | ',' | ';'))
            .next()
            .unwrap_or(prefix)
            .trim_matches(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_' && ch != '-');
        if !redaction_secret_key_name(key) {
            continue;
        }
        let Some((start, end)) = secret_assignment_candidate_span(line, scan_from) else {
            continue;
        };
        scan_from = end.max(scan_from);
        if secret_assignment_value_looks_real(&line[start..end]) {
            spans.push((start, end));
        }
    }
    spans
}

pub(crate) fn secret_assignment_candidate_span(
    line: &str,
    value_start: usize,
) -> Option<(usize, usize)> {
    let value = line.get(value_start..)?;
    let leading_ws = value.len().saturating_sub(value.trim_start().len());
    let mut start = value_start.saturating_add(leading_ws);
    let first = line.get(start..)?.chars().next()?;
    if matches!(first, '"' | '\'' | '`') {
        start = start.saturating_add(first.len_utf8());
        let mut escaped = false;
        for (offset, ch) in line.get(start..)?.char_indices() {
            if escaped {
                escaped = false;
            } else if ch == '\\' {
                escaped = true;
            } else if ch == first {
                return (offset > 0).then_some((start, start.saturating_add(offset)));
            }
        }
        return None;
    }

    if line
        .get(start..start.saturating_add("bearer".len()))
        .is_some_and(|prefix| prefix.eq_ignore_ascii_case("bearer"))
        && line
            .get(start.saturating_add("bearer".len())..)
            .and_then(|rest| rest.chars().next())
            .is_some_and(char::is_whitespace)
    {
        start = start.saturating_add("bearer".len());
        let rest = line.get(start..)?;
        start = start.saturating_add(rest.len().saturating_sub(rest.trim_start().len()));
    }

    let end = line
        .get(start..)?
        .char_indices()
        .find_map(|(offset, ch)| {
            (ch.is_whitespace() || matches!(ch, ',' | ';' | '&' | '}' | ']' | ')'))
                .then_some(start.saturating_add(offset))
        })
        .unwrap_or(line.len());
    (end > start).then_some((start, end))
}

pub(crate) fn redaction_secret_key_name(key: &str) -> bool {
    let compact = compact_key_name(key);
    matches!(
        compact.as_str(),
        "auth"
            | "authorization"
            | "passwd"
            | "pwd"
            | "password"
            | "privatekey"
            | "apikey"
            | "accesstoken"
            | "authtoken"
            | "bearertoken"
            | "clientsecret"
            | "consumersecret"
            | "secretkey"
            | "awssecretaccesskey"
    ) || compact.ends_with("apikey")
        || compact.ends_with("accesstoken")
        || compact.ends_with("authtoken")
        || compact.ends_with("password")
        || compact.ends_with("token")
}

pub(crate) fn secret_assignment_value_looks_real(value: &str) -> bool {
    let value = value
        .trim()
        .trim_end_matches([',', ';'])
        .trim_matches(|ch| matches!(ch, '"' | '\'' | '`'));
    let bearer = strip_bearer_token(value);
    let candidate = bearer.unwrap_or(value);
    if candidate.len() < 6 || candidate.chars().any(char::is_whitespace) {
        return false;
    }
    let lower = candidate.to_ascii_lowercase();
    !matches!(
        lower.as_str(),
        "string"
            | "str"
            | "none"
            | "null"
            | "true"
            | "false"
            | "secret"
            | "password"
            | "token"
            | "api-key"
            | "apikey"
            | "env_var_name"
            | "!command"
    ) && !lower.starts_with("[redacted")
        && !lower.starts_with("<redacted")
        && !lower.starts_with("example")
        && !candidate.starts_with('$')
        && !candidate.starts_with('<')
        && !candidate.starts_with("env::")
        && !candidate.starts_with("std::env")
}

pub(crate) fn redact_digit_sequences(text: &str, counts: &mut PrivacyFindingCounts) -> String {
    let mut spans = Vec::new();
    let mut start: Option<usize> = None;
    let mut digits = String::new();
    let mut digit_count = 0usize;
    for (idx, ch) in text.char_indices() {
        if ch.is_ascii_digit() {
            if start.is_none() {
                start = Some(idx);
                digits.clear();
                digit_count = 0;
            }
            digits.push(ch);
            digit_count += 1;
        } else if start.is_some() && matches!(ch, ' ' | '-') {
            digits.push(ch);
        } else if let Some(s) = start.take() {
            classify_digit_span(text, s, idx, &digits, digit_count, &mut spans, counts);
            digits.clear();
            digit_count = 0;
        }
    }
    if let Some(s) = start {
        classify_digit_span(
            text,
            s,
            text.len(),
            &digits,
            digit_count,
            &mut spans,
            counts,
        );
    }
    redact_by_labeled_spans(text, spans)
}

pub(crate) fn classify_digit_span(
    text: &str,
    start: usize,
    _end: usize,
    raw_digits: &str,
    digit_count: usize,
    spans: &mut Vec<(usize, usize, &'static str)>,
    counts: &mut PrivacyFindingCounts,
) {
    let raw_digits = raw_digits.trim_end_matches([' ', '-']);
    let end = start.saturating_add(raw_digits.len());
    if digit_count < 9
        || !byte_boundary_ok(text.as_bytes(), start, end)
        || numeric_span_touches_decimal(text, start, end)
    {
        return;
    }
    let digits: String = raw_digits.chars().filter(|c| c.is_ascii_digit()).collect();
    if digit_count == 9 && looks_like_ssn_context(text, start) && valid_ssn_digits(&digits) {
        counts.ssn = counts.ssn.saturating_add(1);
        spans.push((start, end, "[REDACTED_SSN]"));
    } else if (13..=19).contains(&digit_count)
        && luhn_valid(&digits)
        && looks_like_card_context(text, start)
    {
        counts.credit_card = counts.credit_card.saturating_add(1);
        spans.push((start, end, "[REDACTED_CARD]"));
    } else if (9..=34).contains(&digit_count) && looks_like_account_context(text, start) {
        counts.account_number = counts.account_number.saturating_add(1);
        spans.push((start, end, "[REDACTED_ACCOUNT]"));
    }
}

pub(crate) fn byte_boundary_ok(bytes: &[u8], start: usize, end: usize) -> bool {
    let before = start.checked_sub(1).and_then(|i| bytes.get(i)).copied();
    let after = bytes.get(end).copied();
    !before.is_some_and(|b| b.is_ascii_alphanumeric())
        && !after.is_some_and(|b| b.is_ascii_alphanumeric())
}

pub(crate) fn redact_by_labeled_spans(
    text: &str,
    mut spans: Vec<(usize, usize, &'static str)>,
) -> String {
    if spans.is_empty() {
        return text.to_string();
    }
    spans.sort_by_key(|(s, _, _)| *s);
    let mut out = String::with_capacity(text.len());
    let mut last = 0usize;
    for (start, end, replacement) in spans {
        if start < last {
            continue;
        }
        out.push_str(&text[last..start]);
        out.push_str(replacement);
        last = end;
    }
    out.push_str(&text[last..]);
    out
}

pub(crate) fn normalized_numeric_label_before(text: &str, start: usize) -> String {
    let line_prefix = text[..start]
        .rsplit_once('\n')
        .map_or(&text[..start], |(_, line)| line);
    let suffix = byte_suffix_at_char_boundary(line_prefix, 64);
    let normalized: String = suffix
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() {
                ch.to_ascii_lowercase()
            } else {
                ' '
            }
        })
        .collect();
    normalized.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(crate) fn numeric_label_matches(text: &str, start: usize, labels: &[&str]) -> bool {
    let label = normalized_numeric_label_before(text, start);
    labels.iter().any(|candidate| {
        label == *candidate
            || label
                .strip_suffix(candidate)
                .is_some_and(|prefix| prefix.ends_with(' '))
    })
}

pub(crate) fn looks_like_ssn_context(text: &str, start: usize) -> bool {
    numeric_label_matches(
        text,
        start,
        &["ssn", "social security", "social security number"],
    )
}

pub(crate) fn looks_like_card_context(text: &str, start: usize) -> bool {
    numeric_label_matches(
        text,
        start,
        &[
            "card",
            "card number",
            "cardnumber",
            "credit card",
            "credit card number",
            "creditcard",
            "creditcardnumber",
            "debit card",
            "payment card",
            "pan",
        ],
    )
}

pub(crate) fn looks_like_account_context(text: &str, start: usize) -> bool {
    numeric_label_matches(
        text,
        start,
        &[
            "account",
            "account number",
            "accountnumber",
            "acct",
            "acct number",
            "acctnumber",
            "routing",
            "routing number",
            "routingnumber",
            "iban",
            "member id",
            "memberid",
            "customer id",
            "customerid",
        ],
    )
}

pub(crate) fn numeric_span_touches_decimal(text: &str, start: usize, end: usize) -> bool {
    text[..start]
        .chars()
        .next_back()
        .is_some_and(|ch| matches!(ch, '.' | '_'))
        || text[end..]
            .chars()
            .next()
            .is_some_and(|ch| matches!(ch, '.' | '_'))
}

pub(crate) fn valid_ssn_digits(digits: &str) -> bool {
    if digits.len() != 9 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return false;
    }
    let area = digits[..3].parse::<u16>().unwrap_or(0);
    let group = digits[3..5].parse::<u8>().unwrap_or(0);
    let serial = digits[5..].parse::<u16>().unwrap_or(0);
    (1..=899).contains(&area) && area != 666 && group != 0 && serial != 0
}

pub(crate) fn luhn_valid(digits: &str) -> bool {
    let mut sum = 0u32;
    let mut double = false;
    for ch in digits.chars().rev() {
        let Some(mut n) = ch.to_digit(10) else {
            return false;
        };
        if double {
            n *= 2;
            if n > 9 {
                n -= 9;
            }
        }
        sum += n;
        double = !double;
    }
    sum > 0 && sum.is_multiple_of(10)
}

pub(crate) fn privacy_sensitive_path(path: &str) -> bool {
    let path = Path::new(path);
    let file_name = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    if matches!(
        file_name.as_str(),
        ".env"
            | ".git-credentials"
            | ".netrc"
            | ".npmrc"
            | ".pypirc"
            | "auth.json"
            | "credentials"
            | "credentials.json"
            | "id_dsa"
            | "id_ecdsa"
            | "id_ed25519"
            | "id_rsa"
            | "providers.json"
    ) || file_name.ends_with(".pem")
        || file_name.ends_with(".key")
        || file_name.ends_with(".p12")
        || file_name.ends_with(".pfx")
    {
        return true;
    }
    path.components().any(|component| match component {
        Component::Normal(name) => matches!(
            name.to_string_lossy().to_ascii_lowercase().as_str(),
            ".aws" | ".gnupg" | ".ssh" | "secrets"
        ),
        _ => false,
    })
}
