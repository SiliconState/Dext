//! Compact terminal-first rendering based on the established `/sessions` layout
//! and shared by `/pack`, `/shelves`, `/help`, `/tools`, and `/system`.
//!
//! All structured renderers use the session layout: a bold count header, bold
//! section labels, two-space entry names, four-space details, separated blocks,
//! and a detached `Use:` footer. Styling is emitted as ANSI escapes only when
//! color is enabled (interactive TTY, not piped, `NO_COLOR` unset,
//! `TERM != dumb`); the TUI translates these escapes back into styled spans.

use std::fmt::Write as _;
use std::path::Path;

use crate::session::user_home_dir;

const DEFAULT_WIDTH: usize = 100;
const MAX_WIDTH: usize = 120;
// Keep wrapped text off the terminal's right edge and cap the measure for
// readability on wide terminals.
const RIGHT_GUTTER: usize = 2;

/// Rendering knobs for list views. `color` should already account for TTY,
/// `NO_COLOR`, and `TERM=dumb`; `width` is the wrapped column budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ListOptions {
    pub(crate) color: bool,
    pub(crate) width: usize,
    pub(crate) verbose: bool,
}

impl ListOptions {
    pub(crate) fn detect_with_width(verbose: bool, width: Option<usize>) -> Self {
        Self {
            color: use_color(),
            width: width.map_or_else(terminal_width, width_for_terminal_cols),
            verbose,
        }
    }

    /// Forced, colorless, fixed-width variant for tests and machine-readable callers.
    #[allow(dead_code)]
    pub(crate) fn fixed(verbose: bool, width: usize) -> Self {
        Self {
            color: false,
            width,
            verbose,
        }
    }

    pub(crate) fn effective_width(&self) -> usize {
        self.width.max(1)
    }
}

/// Color is enabled only for an interactive stdout TTY without an explicit
/// opt-out. Respects `NO_COLOR` and a `TERM=dumb` environment.
pub(crate) fn use_color() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").is_none_or(|t| t != "dumb")
        && std::io::stdout().is_terminal()
}

/// Terminal width from the controlling TTY minus a small right gutter,
/// clamped to a readable maximum measure. Non-TTY output (pipes, redirects,
/// tests) keeps the fixed default.
pub(crate) fn width_for_terminal_cols(cols: usize) -> usize {
    cols.saturating_sub(RIGHT_GUTTER).clamp(1, MAX_WIDTH)
}

pub(crate) fn terminal_width() -> usize {
    if let Ok((cols, _)) = crossterm::terminal::size() {
        return width_for_terminal_cols(cols as usize);
    }
    DEFAULT_WIDTH
}

// --- styling primitives -----------------------------------------------------

fn terminal_escape_end(text: &str, start: usize) -> usize {
    let bytes = text.as_bytes();
    let mut i = start.saturating_add(1);
    if i >= bytes.len() {
        return i;
    }
    match bytes[i] {
        b'[' => {
            i += 1;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if (0x40..=0x7e).contains(&byte) {
                    break;
                }
            }
        }
        b']' | b'P' | b'^' | b'_' => {
            i += 1;
            while i < bytes.len() {
                if bytes[i] == 0x07 {
                    return i + 1;
                }
                if bytes[i] == 0x1b && bytes.get(i + 1) == Some(&b'\\') {
                    return i + 2;
                }
                i += 1;
            }
        }
        _ => {
            i += text[i..].chars().next().map_or(0, char::len_utf8);
        }
    }
    i.min(bytes.len())
}

fn is_bidi_format_control(ch: char) -> bool {
    matches!(
        ch,
        '\u{061c}'
            | '\u{200e}'
            | '\u{200f}'
            | '\u{202a}'..='\u{202e}'
            | '\u{2066}'..='\u{2069}'
    )
}

fn terminal_safe_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut i = 0usize;
    while i < text.len() {
        if text.as_bytes()[i] == 0x1b {
            i = terminal_escape_end(text, i);
            continue;
        }
        let Some(ch) = text[i..].chars().next() else {
            break;
        };
        i += ch.len_utf8();
        match ch {
            '\r' => {
                if !text[i..].starts_with('\n') {
                    out.push('\n');
                }
            }
            '\n' | '\u{2028}' | '\u{2029}' => out.push('\n'),
            '\t' => out.push_str("    "),
            _ if ch.is_control() || is_bidi_format_control(ch) => {}
            _ => out.push(ch),
        }
    }
    out
}

/// Bold (`\x1b[1m`) when color is enabled; identity otherwise.
pub(crate) fn bold(s: &str, color: bool) -> String {
    let s = terminal_safe_text(s);
    if color {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s
    }
}

/// Dim/faint (`\x1b[2m`) when color is enabled.
pub(crate) fn dim(s: &str, color: bool) -> String {
    let s = terminal_safe_text(s);
    if color {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s
    }
}

/// Inline styled label such as `source:` keys.
pub(crate) fn label(key: &str, value: &str, color: bool) -> String {
    let key = terminal_safe_text(key);
    let value = terminal_safe_text(value);
    if color {
        format!("\x1b[36m{key}\x1b[0m {value}")
    } else {
        format!("{key} {value}")
    }
}

// --- path shortening --------------------------------------------------------

/// Shorten an absolute path for display: project-root relative (`./...`),
/// home-relative (`~/...`), or the absolute path when no anchor matches. When
/// `verbose` is set the full absolute path is returned unchanged.
pub(crate) fn shorten_path(path: &Path, root: &Path, home: &Path, verbose: bool) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if verbose {
        return canon.display().to_string();
    }
    let root_c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Ok(rel) = canon.strip_prefix(&root_c) {
        return if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            format!("./{}", rel.display())
        };
    }
    if let Ok(rel) = canon.strip_prefix(home) {
        return if rel.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rel.display())
        };
    }
    canon.display().to_string()
}

/// Convenience for renderers that only know the project root.
pub(crate) fn display_path(path: &Path, opts: &ListOptions, root: &Path) -> String {
    shorten_path(path, root, &user_home_dir(), opts.verbose)
}

// --- word wrap --------------------------------------------------------------

fn word_chunks(word: &str, width: usize) -> Vec<String> {
    let mut chunks = Vec::new();
    let mut chunk = String::new();
    let mut cells = 0usize;
    for cluster in crate::tui::display_clusters(word) {
        let source = &word[cluster.byte_start..cluster.byte_start + cluster.byte_len];
        let (text, cluster_width) = if cluster.width > width {
            ("?", 1)
        } else {
            (source, cluster.width)
        };
        if cells > 0 && cells + cluster_width > width {
            chunks.push(std::mem::take(&mut chunk));
            cells = 0;
        }
        chunk.push_str(text);
        cells += cluster_width;
    }
    if !chunk.is_empty() {
        chunks.push(chunk);
    }
    if chunks.is_empty() {
        chunks.push(word.to_string());
    }
    chunks
}

/// Word-wrap `text` to `width` columns, returning lines. Each line is at most
/// `width` display columns except for a single glyph wider than `width`.
pub(crate) fn wrap_lines(text: &str, width: usize) -> Vec<String> {
    let sanitized = terminal_safe_text(text);
    let text = sanitized.as_str();
    let width = width.max(1);
    let mut out = Vec::new();
    for paragraph in text.split('\n') {
        let words: Vec<&str> = paragraph.split_whitespace().collect();
        if words.is_empty() {
            out.push(String::new());
            continue;
        }
        let mut line = String::new();
        let mut line_w = 0usize;
        for word in words {
            for chunk in word_chunks(word, width) {
                let w = unicode_width::UnicodeWidthStr::width(chunk.as_str());
                if line.is_empty() {
                    line.push_str(&chunk);
                    line_w = w;
                } else if line_w + 1 + w > width {
                    out.push(std::mem::take(&mut line));
                    line.push_str(&chunk);
                    line_w = w;
                } else {
                    line.push(' ');
                    line.push_str(&chunk);
                    line_w += 1 + w;
                }
            }
        }
        if !line.is_empty() {
            out.push(line);
        }
    }
    if out.is_empty() {
        out.push(String::new());
    }
    out
}

/// Append `text` wrapped to `width` with a `hang`-column hanging indent.
pub(crate) fn write_wrapped(out: &mut String, text: &str, hang: usize, width: usize) {
    let width = width.max(1);
    let hang = hang.min(width.saturating_sub(1));
    let body_w = width.saturating_sub(hang).max(1);
    let pad = " ".repeat(hang);
    for line in wrap_lines(text, body_w) {
        let _ = writeln!(out, "{pad}{line}");
    }
}

/// Preserve input line and indentation boundaries while word-wrapping prose.
/// Oversized tokens are split only when they cannot fit on an otherwise empty line.
pub(crate) fn write_preformatted_wrapped(out: &mut String, text: &str, width: usize) {
    let safe = terminal_safe_text(text);
    let width = width.max(1);
    for line in safe.split('\n') {
        if line.trim().is_empty() {
            out.push('\n');
            continue;
        }
        let body = line.trim_start_matches(' ');
        let indent = line
            .len()
            .saturating_sub(body.len())
            .min(width.saturating_sub(1));
        let padding = " ".repeat(indent);
        for wrapped in wrap_lines(body, width.saturating_sub(indent).max(1)) {
            let _ = writeln!(out, "{padding}{wrapped}");
        }
    }
}

fn write_indented_preformatted(
    out: &mut String,
    text: &str,
    indent: usize,
    width: usize,
    color: bool,
) {
    let width = width.max(1);
    let indent = indent.min(width.saturating_sub(1));
    let body_width = width.saturating_sub(indent).max(1);
    let padding = " ".repeat(indent);
    for source_line in text.split('\n') {
        for chunk in word_chunks(source_line, body_width) {
            let _ = writeln!(out, "{padding}{}", bold(&chunk, color));
        }
    }
}

// --- layout helpers ---------------------------------------------------------

/// Render one compact four-space-indented metadata line. Embedded line breaks
/// remain indented so untrusted values cannot escape the detail column.
pub(crate) fn render_metadata(meta: &[(&str, String)], opts: &ListOptions) -> String {
    let width = opts.effective_width();
    let safe_pairs: Vec<(String, String)> = meta
        .iter()
        .map(|(key, value)| {
            (
                terminal_safe_text(key).trim().to_string(),
                terminal_safe_text(value),
            )
        })
        .collect();
    if safe_pairs.is_empty() {
        return String::new();
    }
    let metadata_indent = 4.min(width.saturating_sub(1));
    let metadata_padding = " ".repeat(metadata_indent);
    let inline_plain = safe_pairs
        .iter()
        .map(|(key, value)| format!("{key}: {value}"))
        .collect::<Vec<_>>()
        .join("    ");
    let mut out = String::new();
    if !inline_plain.contains('\n')
        && metadata_indent
            .saturating_add(unicode_width::UnicodeWidthStr::width(inline_plain.as_str()))
            <= width
    {
        let inline_styled = safe_pairs
            .iter()
            .map(|(key, value)| label(&format!("{key}:"), value, opts.color))
            .collect::<Vec<_>>()
            .join("    ");
        let _ = writeln!(out, "{metadata_padding}{inline_styled}");
        return out;
    }

    let body_width = width.saturating_sub(metadata_indent).max(1);
    for (key, value) in safe_pairs {
        let key = format!("{key}:");
        let pair = if value.is_empty() {
            key.clone()
        } else {
            format!("{key} {value}")
        };
        for (index, row) in wrap_lines(&pair, body_width).into_iter().enumerate() {
            if index == 0
                && let Some(rest) = row.strip_prefix(&key)
            {
                let _ = writeln!(
                    out,
                    "{metadata_padding}{}",
                    label(&key, rest.trim_start(), opts.color)
                );
            } else {
                let _ = writeln!(out, "{metadata_padding}{row}");
            }
        }
    }
    out
}

/// Render compact name/description rows. Wide views align descriptions into a
/// shared column; narrow views retain the familiar stacked session hierarchy.
pub(crate) fn render_entry_rows(entries: &[(&str, &str)], opts: &ListOptions) -> String {
    const MIN_COLUMNS_WIDTH: usize = 64;
    const MAX_NAME_WIDTH: usize = 38;
    const DESCRIPTION_GAP: usize = 3;

    let width = opts.effective_width();
    let safe_entries: Vec<(String, String)> = entries
        .iter()
        .map(|(name, description)| {
            (
                terminal_safe_text(name).trim().to_string(),
                terminal_safe_text(description).trim().to_string(),
            )
        })
        .collect();
    let name_width = safe_entries
        .iter()
        .map(|(name, _)| unicode_width::UnicodeWidthStr::width(name.as_str()))
        .max()
        .unwrap_or(0)
        .min(MAX_NAME_WIDTH);
    let description_indent = 2 + name_width + DESCRIPTION_GAP;
    let use_columns = width >= MIN_COLUMNS_WIDTH
        && description_indent.saturating_add(20) <= width
        && safe_entries.iter().all(|(name, _)| {
            !name.contains('\n')
                && unicode_width::UnicodeWidthStr::width(name.as_str()) <= name_width
        });

    let mut out = String::new();
    for (name, description) in safe_entries {
        if !use_columns {
            write_indented_preformatted(&mut out, &name, 2, width, opts.color);
            if !description.is_empty() {
                write_wrapped(&mut out, &description, 4, width);
            }
            continue;
        }

        let display_width = unicode_width::UnicodeWidthStr::width(name.as_str());
        let padding = " ".repeat(name_width.saturating_sub(display_width) + DESCRIPTION_GAP);
        let wrapped = wrap_lines(
            &description,
            width.saturating_sub(description_indent).max(1),
        );
        let first = wrapped.first().map_or("", String::as_str);
        let _ = writeln!(out, "  {}{padding}{first}", bold(&name, opts.color));
        let continuation_padding = " ".repeat(description_indent);
        for line in wrapped.iter().skip(1) {
            let _ = writeln!(out, "{continuation_padding}{line}");
        }
    }
    out
}

/// Render a single list entry block: a bold name line, an indented description
/// (wrapped), and indented metadata pairs. Ends with a trailing blank line so
/// consecutive entries are visually separated.
pub(crate) fn render_entry(
    name: &str,
    description: &str,
    meta: &[(&str, String)],
    opts: &ListOptions,
) -> String {
    let mut out = String::new();
    let hang = 4;
    let safe_name = terminal_safe_text(name);
    write_indented_preformatted(&mut out, &safe_name, 2, opts.effective_width(), opts.color);
    let safe_description = terminal_safe_text(description);
    if !safe_description.trim().is_empty() {
        write_wrapped(
            &mut out,
            safe_description.trim(),
            hang,
            opts.effective_width(),
        );
    }
    if !meta.is_empty() {
        out.push_str(&render_metadata(meta, opts));
    }
    out.push('\n');
    out
}

/// Render a bold section heading in the established session layout.
pub(crate) fn render_section_header(title: &str, opts: &ListOptions) -> String {
    let mut out = String::new();
    let safe_title = terminal_safe_text(title);
    write_indented_preformatted(&mut out, &safe_title, 0, opts.effective_width(), opts.color);
    out
}

/// Standard `Use:` footer block.
pub(crate) fn render_footer(commands: &[&str], opts: &ListOptions) -> String {
    let mut out = render_section_header("Use:", opts);
    for cmd in commands {
        let safe_cmd = terminal_safe_text(cmd);
        write_indented_preformatted(&mut out, &safe_cmd, 2, opts.effective_width(), false);
    }
    out
}

/// Standard list header line: `Title  <count> <noun>`.
pub(crate) fn render_count_header(
    title: &str,
    count: usize,
    noun: &str,
    opts: &ListOptions,
) -> String {
    let safe_title = terminal_safe_text(title);
    let count_label = terminal_safe_text(&format!("{count} {noun}"));
    let compact = format!("{safe_title}  {count_label}");
    if !compact.contains('\n')
        && unicode_width::UnicodeWidthStr::width(compact.as_str()) <= opts.effective_width()
    {
        return format!(
            "{}  {}\n",
            bold(&safe_title, opts.color),
            dim(&count_label, opts.color),
        );
    }

    let mut out = String::new();
    write_indented_preformatted(&mut out, &safe_title, 0, opts.effective_width(), opts.color);
    for source_line in count_label.split('\n') {
        for chunk in word_chunks(source_line, opts.effective_width()) {
            let _ = writeln!(out, "{}", dim(&chunk, opts.color));
        }
    }
    out
}

/// Discovery-list header line: `Title  <count> found`.
pub(crate) fn render_header(title: &str, count: usize, opts: &ListOptions) -> String {
    render_count_header(title, count, "found", opts)
}

/// Parse `--verbose` / `-v` / `--paths` out of a slash argument, returning
/// (remainder, verbose).
pub(crate) fn take_verbose(arg: &str) -> (String, bool) {
    let mut verbose = false;
    let mut kept = Vec::new();
    for tok in arg.split_whitespace() {
        if tok == "--verbose" || tok == "-v" || tok == "--paths" {
            verbose = true;
        } else {
            kept.push(tok);
        }
    }
    (kept.join(" "), verbose)
}
