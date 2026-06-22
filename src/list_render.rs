//! Compact terminal-first list rendering shared by `/pack`, `/sessions`, `/shelves`, and `/help`.
//!
//! All list renderers share the same look: a bold header, separated per-entry
//! blocks with a prominent name, hanging-indent wrapped descriptions, and a
//! detached `Use:` footer. Styling is emitted as ANSI escapes only when color is
//! enabled (interactive TTY, not piped, `NO_COLOR` unset, `TERM != dumb`); the
//! TUI translates these escapes back into styled spans.

use std::fmt::Write as _;
use std::path::Path;

use crate::session::user_home_dir;

const MIN_WIDTH: usize = 20;
const DEFAULT_WIDTH: usize = 100;

/// Rendering knobs for list views. `color` should already account for TTY,
/// `NO_COLOR`, and `TERM=dumb`; `width` is the wrapped column budget.
#[derive(Debug, Clone, Copy)]
pub(crate) struct ListOptions {
    pub(crate) color: bool,
    pub(crate) width: usize,
    pub(crate) verbose: bool,
}

impl ListOptions {
    pub(crate) fn detect(verbose: bool) -> Self {
        Self {
            color: use_color(),
            width: terminal_width(),
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
        self.width.max(MIN_WIDTH)
    }
}

/// Color is enabled only for an interactive stdout TTY without an explicit
/// opt-out. Respects `NO_COLOR` and a `TERM=dumb` environment.
pub(crate) fn use_color() -> bool {
    use std::io::IsTerminal;
    std::env::var_os("NO_COLOR").is_none()
        && std::env::var_os("TERM").map_or(true, |t| t != "dumb")
        && std::io::stdout().is_terminal()
}

/// Terminal width from the controlling TTY, clamped to a sane default for
/// non-TTY output (pipes, redirects, tests).
pub(crate) fn terminal_width() -> usize {
    if let Some((cols, _)) = crossterm::terminal::size().ok() {
        if cols >= MIN_WIDTH as u16 {
            return cols as usize;
        }
    }
    DEFAULT_WIDTH
}

// --- styling primitives -----------------------------------------------------

/// Bold (`\x1b[1m`) when color is enabled; identity otherwise.
pub(crate) fn bold(s: &str, color: bool) -> String {
    if color {
        format!("\x1b[1m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Dim/faint (`\x1b[2m`) when color is enabled.
pub(crate) fn dim(s: &str, color: bool) -> String {
    if color {
        format!("\x1b[2m{s}\x1b[0m")
    } else {
        s.to_string()
    }
}

/// Inline styled label such as `source:` keys.
pub(crate) fn label(key: &str, value: &str, color: bool) -> String {
    if color {
        format!("\x1b[36m{key}\x1b[0m {value}")
    } else {
        format!("{key} {value}")
    }
}

// --- path shortening --------------------------------------------------------

/// Shorten an absolute path for display: home-relative (`~/...`), project-root
/// relative (`./...`), or the absolute path when no anchor matches. When
/// `verbose` is set the full absolute path is returned unchanged.
pub(crate) fn shorten_path(path: &Path, root: &Path, home: &Path, verbose: bool) -> String {
    let canon = std::fs::canonicalize(path).unwrap_or_else(|_| path.to_path_buf());
    if verbose {
        return canon.display().to_string();
    }
    if let Ok(rel) = canon.strip_prefix(home) {
        return if rel.as_os_str().is_empty() {
            "~".to_string()
        } else {
            format!("~/{}", rel.display())
        };
    }
    let root_c = std::fs::canonicalize(root).unwrap_or_else(|_| root.to_path_buf());
    if let Ok(rel) = canon.strip_prefix(&root_c) {
        return if rel.as_os_str().is_empty() {
            ".".to_string()
        } else {
            format!("./{}", rel.display())
        };
    }
    canon.display().to_string()
}

/// Convenience for renderers that only know the project root.
pub(crate) fn display_path(path: &Path, opts: &ListOptions, root: &Path) -> String {
    shorten_path(path, root, &user_home_dir(), opts.verbose)
}

// --- word wrap --------------------------------------------------------------

/// Word-wrap `text` to `width` columns, returning lines. Each line is at most
/// `width` display columns; an over-long single word fills a line.
pub(crate) fn wrap_lines(text: &str, width: usize) -> Vec<String> {
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
            let w = unicode_width::UnicodeWidthStr::width(word);
            if line.is_empty() {
                line.push_str(word);
                line_w = w;
            } else if line_w + 1 + w > width {
                out.push(std::mem::take(&mut line));
                line.push_str(word);
                line_w = w;
            } else {
                line.push(' ');
                line.push_str(word);
                line_w += 1 + w;
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
    let body_w = width.saturating_sub(hang).max(1);
    let pad = " ".repeat(hang);
    for line in wrap_lines(text, body_w) {
        let _ = writeln!(out, "{pad}{line}");
    }
}

// --- layout helpers ---------------------------------------------------------

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
    let _ = writeln!(out, "  {}", bold(name, opts.color));
    if !description.trim().is_empty() {
        write_wrapped(&mut out, description.trim(), hang, opts.effective_width());
    }
    if !meta.is_empty() {
        let pairs: Vec<String> = meta
            .iter()
            .map(|(k, v)| label(&format!("{k}:"), v, opts.color))
            .collect();
        let _ = writeln!(out, "{}{}", " ".repeat(hang), pairs.join("    "));
    }
    out.push('\n');
    out
}

/// Standard `Use:` footer block.
pub(crate) fn render_footer(commands: &[&str], opts: &ListOptions) -> String {
    let mut out = String::new();
    let _ = writeln!(out, "{}", bold("Use:", opts.color));
    for cmd in commands {
        let _ = writeln!(out, "  {cmd}");
    }
    out
}

/// Standard list header line: `Title  <count> found`.
pub(crate) fn render_header(title: &str, count: usize, opts: &ListOptions) -> String {
    format!(
        "{}  {} {}\n",
        bold(title, opts.color),
        dim(&count.to_string(), opts.color),
        dim("found", opts.color),
    )
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
