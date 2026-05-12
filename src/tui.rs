use anyhow::Result;
use crossterm::event::{
    self as cterm_event, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, MouseEvent, MouseEventKind,
};
use crossterm::terminal::enable_raw_mode;
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row, Table, Widget, Wrap};
use ratatui::{TerminalOptions, Viewport};
use ratatui_core::layout::Alignment as MdAlignment;
use ratatui_core::style::{Color as MdColor, Modifier as MdModifier, Style as MdStyle};
use ratatui_core::text::Text as MdText;
use serde_json::Value;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write as IoWrite};
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tui_markdown::{
    Options as MarkdownOptions, StyleSheet as MarkdownStyleSheet, from_str_with_options,
};

use crate::provider::{curated_provider_models, provider_has_available_credentials};
use crate::{
    Agent, AgentEvent, ApprovalProfile, Choice, EventSink, ThinkingEffort, Usage, WorkMapEventKind,
    canonical_provider_id, git_summary, handle_slash, history_char_budget_with_override,
    load_auth_store, load_provider_catalog, model_context_window, orchestrator::ExternalTelemetry,
    parse_compact_slash, provider_auth_status, resolve_active_provider_id, summarize_call,
};

const INPUT_HISTORY_MAX: usize = 200;
const VIEWPORT_HEIGHT: u16 = 10;
const COLLAPSED_PREVIEW_LINES: usize = 4;
const TOOL_DENSITY_SEPARATOR_EVERY: usize = 10;
const RG_LINE_TRUNCATE_CELLS: usize = 220;
const TRANSCRIPT_WRAP_GUARD_COLS: u16 = 1;
const WORK_MAP_DRAWER_MAX_ROWS: usize = 10;
const WORK_MAP_DRAWER_MAX_BODY_ROWS: usize = 8;
const WORK_MAP_DRAWER_MIN_EDITOR_ROWS: usize = 1;
const THINKING_BG: Color = Color::Indexed(235);
const STEERING_BG: Color = Color::Indexed(237);
const TRUST_INPUT_BORDER: Color = Color::Indexed(66);
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];

fn is_subagent_call_id(call_id: &str) -> bool {
    call_id.starts_with("sub.")
}

fn is_subagent_batch_id(batch_id: &str) -> bool {
    batch_id.starts_with("sub.")
}

fn parse_detached_subagent_launch(s: &str) -> Option<DetachedSubagent> {
    if !s.contains("▶ subagent detached:") {
        return None;
    }
    let output_path = s
        .lines()
        .find(|l| l.starts_with("output:"))
        .map(|l| l.trim_start_matches("output:").trim())?;
    let task = s
        .lines()
        .find(|l| l.starts_with("▶ subagent detached:"))
        .and_then(|l| {
            let between = l.split('"').nth(1)?;
            Some(between.to_string())
        })
        .unwrap_or_default();
    Some(DetachedSubagent {
        task,
        output_path: PathBuf::from(output_path),
        file_offset: 0,
        tail: Vec::new(),
        completed: false,
    })
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ToolChunk {
    call_tag: String,
    summary: String,
    content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
enum PermissionTier {
    Read,
    Write,
    Danger,
}

impl PermissionTier {
    fn from_risk(risk: crate::tool_policy::CommandRisk) -> Self {
        match risk {
            crate::tool_policy::CommandRisk::Read => Self::Read,
            crate::tool_policy::CommandRisk::Write => Self::Write,
            crate::tool_policy::CommandRisk::Danger => Self::Danger,
        }
    }

    fn accent(self) -> Color {
        match self {
            Self::Read => Color::Yellow,
            Self::Write => Color::Yellow,
            Self::Danger => Color::Red,
        }
    }

    fn default_choice(self) -> Choice {
        match self {
            Self::Read | Self::Write => Choice::Once,
            Self::Danger => Choice::Deny,
        }
    }
}

struct PendingPermission {
    tool: String,
    audit_label: String,
    tier: PermissionTier,
    responder: std::sync::mpsc::SyncSender<Choice>,
}

#[derive(Clone)]
struct WorkMapDrawer {
    text: String,
    waypoint_ids: Vec<String>,
    selector: Option<String>,
    selected: usize,
    scroll: usize,
}

#[derive(Clone, PartialEq, Eq, Hash)]
enum Line_ {
    Banner(String),
    User(String),
    Assistant {
        text: String,
        dim_prefix: bool,
    },
    Tool {
        call_tag: String,
        name: String,
        summary: String,
        ok: Option<bool>,
        content: String,
        group_count: usize,
        group_lines: usize,
        group_chunks: Vec<ToolChunk>,
        duration_secs: u64,
        denied: bool,
        dim: bool,
        density_rank: usize,
        expanded: bool,
    },
    PermissionPrompt {
        tool: String,
        command: String,
        tier: PermissionTier,
        risk: crate::tool_policy::CommandRisk,
    },
    PermissionResult {
        command: String,
        approved: bool,
        always: bool,
    },
    LocalAuth {
        tool: String,
        message: String,
    },
    Info(String),
    Warn(String),
    Error(String),
    Retry(String),
    Steering(String),
    SteeringDelivered {
        messages: usize,
        preview: String,
    },
    Thinking(String),
    WorkMap {
        kind: WorkMapEventKind,
        text: String,
        waypoint_ids: Vec<String>,
        selector: Option<String>,
        selected: usize,
    },
    Blank,
    TurnSep,
}

enum ToTui {
    Event(AgentEvent),
    PermissionRequest {
        name: String,
        input: Value,
        responder: std::sync::mpsc::SyncSender<Choice>,
    },
}

enum FromTui {
    Submit(String),
    CycleEffort(i8),
    Quit,
}

struct TuiSink {
    tx: tokio::sync::mpsc::UnboundedSender<ToTui>,
}

impl EventSink for TuiSink {
    fn emit(&mut self, event: AgentEvent) {
        crate::record_crash_event(&event);
        let _ = self.tx.send(ToTui::Event(event));
    }
    fn request_permission(&mut self, name: &str, input: &Value) -> Choice {
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel(0);
        if self
            .tx
            .send(ToTui::PermissionRequest {
                name: name.to_string(),
                input: input.clone(),
                responder: resp_tx,
            })
            .is_err()
        {
            return Choice::Deny;
        }
        resp_rx.recv().unwrap_or(Choice::Deny)
    }

    fn local_auth_prompt(&mut self, tool: &str, message: &str) {
        let _ = self.tx.send(ToTui::Event(AgentEvent::LocalAuthPrompt {
            tool: tool.to_string(),
            message: message.to_string(),
        }));
    }
}

#[derive(Clone)]
struct LiveTool {
    call_id: String,
    call_tag: String,
    name: String,
    summary: String,
    running: bool,
    started: Option<Instant>,
    is_subagent: bool,
}

#[derive(Clone)]
struct LiveBatch {
    entries: Vec<String>,
    failed: usize,
    done: bool,
}

struct ExpandableBlock {
    name: String,
    expanded: bool,
}

struct SlashCmd {
    name: &'static str,
    args: &'static str,
    help: &'static str,
}

static SLASH_COMMANDS: &[SlashCmd] = &[
    SlashCmd {
        name: "/help",
        args: "",
        help: "show commands",
    },
    SlashCmd {
        name: "/quit",
        args: "",
        help: "exit dext",
    },
    SlashCmd {
        name: "/exit",
        args: "",
        help: "exit dext",
    },
    SlashCmd {
        name: "/reset",
        args: "",
        help: "clear conversation",
    },
    SlashCmd {
        name: "/tools",
        args: "",
        help: "list tools",
    },
    SlashCmd {
        name: "/history",
        args: "",
        help: "turn count + last 5",
    },
    SlashCmd {
        name: "/system",
        args: "[text]",
        help: "show/replace system prompt",
    },
    SlashCmd {
        name: "/allow",
        args: "<tool>",
        help: "auto-approve tool",
    },
    SlashCmd {
        name: "/revoke",
        args: "<tool>",
        help: "remove auto-approval",
    },
    SlashCmd {
        name: "/allowed",
        args: "",
        help: "list auto-approved tools",
    },
    SlashCmd {
        name: "/trust",
        args: "[on|off|status]",
        help: "auto-approve all gated tools",
    },
    SlashCmd {
        name: "/approval",
        args: "[profile]",
        help: "ask|auto-read|auto-write|never|always",
    },
    SlashCmd {
        name: "/sandbox-profile",
        args: "[profile]",
        help: "read-only|workspace-write|danger-full-access",
    },
    SlashCmd {
        name: "/budget",
        args: "[cap|off]",
        help: "show/set spend cap",
    },
    SlashCmd {
        name: "/browser",
        args: "[off|agent-browser|agentbrowser]",
        help: "optional browser recipe",
    },
    SlashCmd {
        name: "/sandbox",
        args: "[path]",
        help: "show/change sandbox root",
    },
    SlashCmd {
        name: "/model",
        args: "[id]",
        help: "show/change model",
    },
    SlashCmd {
        name: "/providers",
        args: "",
        help: "list providers + auth",
    },
    SlashCmd {
        name: "/provider",
        args: "[id|#]",
        help: "show/switch provider",
    },
    SlashCmd {
        name: "/models",
        args: "[provider|#|all]",
        help: "list active/authenticated models",
    },
    SlashCmd {
        name: "/login",
        args: "[provider|#] [token|web|import]",
        help: "open web login (Enter shows providers)",
    },
    SlashCmd {
        name: "/logout",
        args: "[provider|#]",
        help: "remove stored key",
    },
    SlashCmd {
        name: "/effort",
        args: "[level]",
        help: "off|low|medium|high|xhigh",
    },
    SlashCmd {
        name: "/compact",
        args: "",
        help: "summarize older history",
    },
    SlashCmd {
        name: "/usage",
        args: "",
        help: "token usage this session",
    },
    SlashCmd {
        name: "/status",
        args: "",
        help: "runtime diagnostics",
    },
    SlashCmd {
        name: "/tokens",
        args: "",
        help: "tokens per message",
    },
    SlashCmd {
        name: "/save",
        args: "<name>",
        help: "save session as JSONL",
    },
    SlashCmd {
        name: "/export",
        args: "[html|jsonl] [path]",
        help: "export session",
    },
    SlashCmd {
        name: "/resume",
        args: "[name]",
        help: "load session",
    },
    SlashCmd {
        name: "/map",
        args: "[session]",
        help: "open Work Map drawer",
    },
    SlashCmd {
        name: "/packet",
        args: "@wNN [session]",
        help: "waypoint packet",
    },
    SlashCmd {
        name: "/focus",
        args: "@wNN [--exact]",
        help: "load focus packet",
    },
    SlashCmd {
        name: "/track",
        args: "open @wNN [name]",
        help: "create continuation",
    },
    SlashCmd {
        name: "/tracks",
        args: "",
        help: "list continuations",
    },
    SlashCmd {
        name: "/sessions",
        args: "",
        help: "list project latest + named",
    },
    SlashCmd {
        name: "/session",
        args: "",
        help: "alias for /sessions",
    },
    SlashCmd {
        name: "/plan",
        args: "<task>",
        help: "run read-only planner",
    },
    SlashCmd {
        name: "/subagent",
        args: "<task> [opts]",
        help: "run detached subagent",
    },
    SlashCmd {
        name: "/pack",
        args: "[list|inspect|run]",
        help: "discover/invoke packs",
    },
    SlashCmd {
        name: "/hooks",
        args: "[reload]",
        help: "show/reload hooks",
    },
    SlashCmd {
        name: "/version",
        args: "",
        help: "show version",
    },
];

static TRUST_ARGS: &[&str] = &["on", "off", "status"];
static EFFORT_ARGS: &[&str] = &["off", "low", "medium", "high", "xhigh"];
static WORK_MAP_SESSION_ARGS: &[&str] = &["current", "latest"];
static TRACK_ARGS: &[&str] = &["open", "list"];
const SLASH_COMPLETION_MAX_VISIBLE: usize = 7;

struct SlashCompletion {
    text: String,
    hint: String,
}

fn provider_arg_completions(cmd: &str, arg_part: &str) -> Vec<SlashCompletion> {
    let Ok(catalog) = load_provider_catalog() else {
        return Vec::new();
    };
    let store = load_auth_store().ok();
    let active = resolve_active_provider_id(&catalog);
    let needle = arg_part.trim().to_ascii_lowercase();

    let mut out = Vec::new();
    for (idx, profile) in catalog.providers.iter().enumerate() {
        let index = (idx + 1).to_string();
        let id = profile.id.to_ascii_lowercase();
        if !needle.is_empty() && !id.starts_with(&needle) && !index.starts_with(&needle) {
            continue;
        }

        let marker = if canonical_provider_id(&profile.id) == canonical_provider_id(&active) {
            "*"
        } else {
            " "
        };

        let hint = if cmd == "login" {
            let auth = store
                .as_ref()
                .map(|s| provider_auth_status(profile, s))
                .unwrap_or_else(|| "unknown".to_string());
            format!(
                "{marker} {} model={} auth={}",
                profile.id, profile.default_model, auth
            )
        } else {
            format!("{marker} {} model={}", profile.id, profile.default_model)
        };

        out.push(SlashCompletion {
            text: format!("/{cmd} {}", profile.id),
            hint: hint.clone(),
        });
        out.push(SlashCompletion {
            text: format!("/{cmd} {}", index),
            hint,
        });
    }

    out
}

fn model_arg_completions(arg_part: &str) -> Vec<SlashCompletion> {
    let Ok(catalog) = load_provider_catalog() else {
        return Vec::new();
    };
    let Ok(store) = load_auth_store() else {
        return Vec::new();
    };
    let active = resolve_active_provider_id(&catalog);
    let needle = arg_part.trim().to_ascii_lowercase();
    let has_explicit_provider = arg_part.contains('/') || arg_part.contains(':');
    let mut out = Vec::new();
    let mut seen_texts: HashSet<String> = HashSet::new();
    for profile in &catalog.providers {
        if !provider_has_available_credentials(profile, &store) {
            continue;
        }
        let is_active = canonical_provider_id(&profile.id) == canonical_provider_id(&active);
        for model in curated_provider_models(profile).into_iter() {
            let plain = model.to_ascii_lowercase();
            let qualified = format!("{}/{}", profile.id, model);
            let qualified_colon = format!("{}:{}", profile.id, model);
            if !needle.is_empty()
                && !plain.starts_with(&needle)
                && !qualified.to_ascii_lowercase().starts_with(&needle)
                && !qualified_colon.to_ascii_lowercase().starts_with(&needle)
            {
                continue;
            }
            let hint = format!(
                "{} {} · {} · {}",
                if is_active { "active" } else { "switch" },
                profile.id,
                model,
                provider_auth_status(profile, &store)
            );
            let mut push = |text: String, hint: String, out: &mut Vec<SlashCompletion>| {
                if seen_texts.insert(text.clone()) {
                    out.push(SlashCompletion { text, hint });
                }
            };
            if has_explicit_provider {
                push(format!("/model {qualified}"), hint.clone(), &mut out);
                push(format!("/model {qualified_colon}"), hint, &mut out);
            } else if is_active {
                push(format!("/model {model}"), hint, &mut out);
            } else {
                push(format!("/model {qualified}"), hint, &mut out);
            }
        }
    }
    out
}

fn slash_completions(input: &str) -> Vec<SlashCompletion> {
    let trimmed_start = input.trim_start();
    if !trimmed_start.starts_with('/') {
        return Vec::new();
    }
    let no_prefix = &trimmed_start[1..];
    if no_prefix.is_empty() {
        return SLASH_COMMANDS
            .iter()
            .map(|c| SlashCompletion {
                text: c.name.to_string(),
                hint: if c.args.is_empty() {
                    c.help.to_string()
                } else {
                    format!("{} — {}", c.args, c.help)
                },
            })
            .collect();
    }

    let mut parts = no_prefix.splitn(2, char::is_whitespace);
    let cmd_part = parts.next().unwrap_or("");
    let arg_raw = parts.next().unwrap_or("");
    let arg_part = arg_raw.trim();
    let has_arg_context = no_prefix.chars().any(char::is_whitespace);

    // If there's a space, we're completing arguments for a known command.
    if has_arg_context {
        if matches!(cmd_part, "login" | "provider" | "logout") {
            let provider_comps = provider_arg_completions(cmd_part, arg_part);
            if !provider_comps.is_empty() {
                return provider_comps;
            }
        }
        if cmd_part == "models" {
            let scope = arg_part.to_ascii_lowercase();
            if let Ok(catalog) = load_provider_catalog()
                && let Ok(store) = load_auth_store()
            {
                let active = resolve_active_provider_id(&catalog);
                if arg_part.is_empty() || scope == "all" {
                    let mut out = Vec::new();
                    out.push(SlashCompletion {
                        text: "/models all".to_string(),
                        hint: "list models for all authenticated providers".to_string(),
                    });
                    for profile in &catalog.providers {
                        if !provider_has_available_credentials(profile, &store) {
                            continue;
                        }
                        let marker = if canonical_provider_id(&profile.id)
                            == canonical_provider_id(&active)
                        {
                            "*"
                        } else {
                            " "
                        };
                        out.push(SlashCompletion {
                            text: format!("/models {}", profile.id),
                            hint: format!(
                                "{marker} {} models={} auth={}",
                                profile.id,
                                curated_provider_models(profile).len(),
                                provider_auth_status(profile, &store)
                            ),
                        });
                    }
                    return out;
                }
            }
            let provider_comps = provider_arg_completions(cmd_part, arg_part);
            if !provider_comps.is_empty() {
                return provider_comps;
            }
        }
        if cmd_part == "model" {
            let model_comps = model_arg_completions(arg_part);
            if !model_comps.is_empty() {
                return model_comps;
            }
        }

        let sub_args = match cmd_part {
            "trust" => Some(TRUST_ARGS),
            "effort" => Some(EFFORT_ARGS),
            "map" | "packet" | "focus" => Some(WORK_MAP_SESSION_ARGS),
            "track" => Some(TRACK_ARGS),
            _ => None,
        };
        if let Some(args) = sub_args {
            return args
                .iter()
                .filter(|a| a.starts_with(arg_part) && !a.is_empty())
                .map(|a| SlashCompletion {
                    text: format!("/{} {}", cmd_part, a),
                    hint: String::new(),
                })
                .collect();
        }

        if let Some(cmd) = SLASH_COMMANDS.iter().find(|c| &c.name[1..] == cmd_part)
            && !cmd.args.is_empty()
        {
            return vec![SlashCompletion {
                text: trimmed_start.trim_end().to_string(),
                hint: format!("{} — {}", cmd.args, cmd.help),
            }];
        }
        return Vec::new();
    }

    // Completing the command name itself.
    SLASH_COMMANDS
        .iter()
        .filter(|c| c.name[1..].starts_with(cmd_part))
        .map(|c| SlashCompletion {
            text: c.name.to_string(),
            hint: if c.args.is_empty() {
                c.help.to_string()
            } else {
                format!("{} — {}", c.args, c.help)
            },
        })
        .collect()
}

struct CachedTranscriptRender {
    renders: HashMap<u16, Text<'static>>,
    heights: HashMap<u16, u16>,
}

struct TranscriptLayoutState {
    transcript_area: Rect,
    input_area: Rect,
    total_lines: usize,
    visible_lines: usize,
    live_indicator_lines: usize,
    live_indicator_top_padding: usize,
    live_indicator_visible: bool,
    live_indicator_scroll_start: usize,
    live_indicator_scroll_end: usize,
    transcript_line_layout: Vec<(usize, usize)>,
    live_indicator_line_layout: Option<(usize, usize)>,
    live_indicator_text: Option<Text<'static>>,
}

struct DetachedSubagent {
    task: String,
    output_path: PathBuf,
    file_offset: u64,
    tail: Vec<String>,
    completed: bool,
}

struct TuiState {
    pending_insert: Vec<Line_>,
    transcript: Vec<Line_>,
    render_cache: HashMap<u64, CachedTranscriptRender>,
    transcript_scroll_offset: usize,
    transcript_hover_expandable: Option<usize>,
    transcript_area: Rect,
    input_area: Rect,
    live_indicator_height: u16,
    live_indicator_visible: bool,
    live_indicator_text: Option<Text<'static>>,
    transcript_total_lines: usize,
    transcript_visible_lines: usize,
    live_indicator_lines: usize,
    live_indicator_top_padding: usize,
    transcript_scroll_max: usize,
    live_indicator_scroll_start: usize,
    live_indicator_scroll_end: usize,
    transcript_line_layout: Vec<(usize, usize)>,
    live_indicator_line_layout: Option<(usize, usize)>,
    live_tools: Vec<LiveTool>,
    input: String,
    cursor: usize,
    history: VecDeque<String>,
    history_idx: Option<usize>,
    status: String,
    usage: Usage,
    // Tokens the most recent request actually carried (live request-level context).
    // Distinct from `usage` which is cumulative session billing.
    last_turn_context_tokens: u64,
    history_chars: u64,
    model: String,
    sandbox: String,
    streaming_text: String,
    streaming_thinking: String,
    stream_started_at: Option<Instant>,
    stream_chars: u64,
    pending_perm: Option<PendingPermission>,
    agent_busy: bool,
    quit: bool,
    frame_count: u64,
    approval_profile: ApprovalProfile,
    thinking_effort: ThinkingEffort,
    last_expandable: Option<ExpandableBlock>,
    show_help: bool,
    slash_acomp_sel: Option<usize>,
    slash_acomp_scroll: usize,
    git_branch: Option<String>,
    git_branch_refreshed: Option<Instant>,
    tool_tint_parity: bool,
    transcript_needs_rebuild: bool,
    call_tag_seq: usize,
    turn_seq: usize,
    call_tags: HashMap<String, String>,
    sub_batch: Option<LiveBatch>,
    verbose: bool,
    input_display_override: Option<String>,
    external_telemetry: ExternalTelemetry,
    retry_status: Option<String>,
    compacting: bool,
    compacting_resume_busy: bool,
    provider_label: String,
    api_family: String,
    auth_source: String,
    last_retry_reason: Option<String>,
    workaround_fired: bool,
    assistant_prefix_seen: bool,
    turn_tool_counts: HashMap<String, usize>,
    turn_error_count: usize,
    turn_start_at: Option<Instant>,
    todo_progress: Option<TodoProgress>,
    work_map: Option<WorkMapDrawer>,
    detached_subagent: Option<DetachedSubagent>,
}

#[derive(Clone, Debug, PartialEq, Eq, Hash)]
struct TodoProgress {
    total: usize,
    completed: usize,
    in_progress: usize,
    active: Option<String>,
}

impl TuiState {
    fn new(
        model: String,
        sandbox: String,
        approval_profile: ApprovalProfile,
        thinking_effort: ThinkingEffort,
    ) -> Self {
        Self {
            pending_insert: Vec::new(),
            transcript: Vec::new(),
            render_cache: HashMap::new(),
            transcript_scroll_offset: 0,
            transcript_hover_expandable: None,
            transcript_area: Rect::default(),
            input_area: Rect::default(),
            live_indicator_height: 0,
            live_indicator_visible: false,
            live_indicator_text: None,
            transcript_total_lines: 0,
            transcript_visible_lines: 0,
            live_indicator_lines: 0,
            live_indicator_top_padding: 0,
            transcript_scroll_max: 0,
            live_indicator_scroll_start: 0,
            live_indicator_scroll_end: 0,
            transcript_line_layout: Vec::new(),
            live_indicator_line_layout: None,
            live_tools: Vec::new(),
            input: String::new(),
            cursor: 0,
            history: VecDeque::new(),
            history_idx: None,
            status: "ready".into(),
            usage: Usage::default(),
            last_turn_context_tokens: 0,
            history_chars: 0,
            model,
            sandbox,
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            stream_started_at: None,
            stream_chars: 0,
            pending_perm: None,
            agent_busy: false,
            quit: false,
            frame_count: 0,
            approval_profile,
            thinking_effort,
            last_expandable: None,
            show_help: false,
            slash_acomp_sel: None,
            slash_acomp_scroll: 0,
            git_branch: None,
            git_branch_refreshed: None,
            tool_tint_parity: false,
            transcript_needs_rebuild: false,
            call_tag_seq: 0,
            turn_seq: 0,
            call_tags: HashMap::new(),
            sub_batch: None,
            verbose: true,
            input_display_override: None,
            external_telemetry: ExternalTelemetry::default(),
            retry_status: None,
            compacting: false,
            compacting_resume_busy: false,
            provider_label: String::new(),
            api_family: String::new(),
            auth_source: String::new(),
            last_retry_reason: None,
            workaround_fired: false,
            assistant_prefix_seen: false,
            turn_tool_counts: HashMap::new(),
            turn_error_count: 0,
            turn_start_at: None,
            todo_progress: None,
            work_map: None,
            detached_subagent: None,
        }
    }

    fn refresh_git_branch(&mut self) {
        let stale = match self.git_branch_refreshed {
            Some(t) => t.elapsed() > Duration::from_secs(2),
            None => true,
        };
        if !stale {
            return;
        }
        self.git_branch = git_summary(std::path::Path::new(&self.sandbox));
        self.git_branch_refreshed = Some(Instant::now());
    }

    fn poll_detached_subagent(&mut self) {
        let ds = match self.detached_subagent.as_mut() {
            Some(ds) if !ds.completed => ds,
            _ => return,
        };
        let mut file = match std::fs::File::open(&ds.output_path) {
            Ok(f) => f,
            Err(_) => return,
        };
        use std::io::{Read, Seek, SeekFrom};
        if let Ok(meta) = file.metadata() {
            if meta.len() <= ds.file_offset {
                return;
            }
        }
        if file.seek(SeekFrom::Start(ds.file_offset)).is_err() {
            return;
        }
        let mut buf = String::new();
        if file.read_to_string(&mut buf).is_err() {
            return;
        }
        ds.file_offset += buf.len() as u64;
        let new_lines: Vec<&str> = buf.lines().collect();
        const MAX_TAIL: usize = 8;
        for line in new_lines {
            let trimmed = line.trim();
            if trimmed.is_empty() || trimmed.starts_with('#') {
                continue;
            }
            ds.tail.push(trimmed.to_string());
        }
        while ds.tail.len() > MAX_TAIL {
            ds.tail.remove(0);
        }
        if buf.contains("## Status\ncomplete") {
            ds.completed = true;
        }
    }

    fn queue(&mut self, line: Line_) {
        if matches!(line, Line_::Tool { .. }) && self.last_line_is_completed_thinking() {
            self.pending_insert.push(Line_::Blank);
        }
        if matches!(line, Line_::Thinking(_)) && self.last_line_is_tool() {
            self.pending_insert.push(Line_::Blank);
        }
        if matches!(line, Line_::Steering(_)) && !self.pending_insert.ends_with(&[Line_::Blank]) {
            self.pending_insert.push(Line_::Blank);
        }
        let is_steering = matches!(line, Line_::Steering(_));
        self.pending_insert.push(line);
        if is_steering {
            self.pending_insert.push(Line_::Blank);
        }
    }

    fn last_line_is_completed_thinking(&self) -> bool {
        self.pending_insert
            .last()
            .or_else(|| self.transcript.last())
            .is_some_and(|line| matches!(line, Line_::Thinking(_)))
    }

    fn last_line_is_tool(&self) -> bool {
        self.pending_insert
            .last()
            .or_else(|| self.transcript.last())
            .is_some_and(|line| matches!(line, Line_::Tool { .. }))
    }

    fn scroll_transcript_by(&mut self, delta: isize) {
        if delta >= 0 {
            self.transcript_scroll_offset = self
                .transcript_scroll_offset
                .saturating_add(delta as usize)
                .min(self.transcript_scroll_max);
        } else {
            self.transcript_scroll_offset = self
                .transcript_scroll_offset
                .saturating_sub(delta.unsigned_abs())
                .min(self.transcript_scroll_max);
        }
        self.update_transcript_scroll_status();
    }

    fn jump_transcript_to_top(&mut self) {
        self.transcript_scroll_offset = self.transcript_scroll_max;
        self.update_transcript_scroll_status();
    }

    fn jump_transcript_to_bottom(&mut self) {
        self.transcript_scroll_offset = 0;
        self.update_transcript_scroll_status();
    }

    fn clamp_transcript_scroll(&mut self) {
        self.transcript_scroll_offset = self
            .transcript_scroll_offset
            .min(self.transcript_scroll_max);
    }

    fn update_transcript_scroll_status(&mut self) {
        self.clamp_transcript_scroll();
        self.status = if self.transcript_scroll_offset == 0 {
            "scroll: live".to_string()
        } else {
            format!("scroll: +{} lines", self.transcript_scroll_offset)
        };
    }

    fn reset_slash_completion_selection(&mut self) {
        if self.input.trim_start().starts_with('/') {
            self.slash_acomp_sel = Some(0);
        } else {
            self.slash_acomp_sel = None;
        }
        self.slash_acomp_scroll = 0;
    }

    fn clear_slash_completion_selection(&mut self) {
        self.slash_acomp_sel = None;
        self.slash_acomp_scroll = 0;
    }

    fn sync_slash_completion_window(
        &mut self,
        completions_len: usize,
        visible_count: usize,
    ) -> Option<(usize, usize)> {
        if completions_len == 0 || visible_count == 0 {
            self.clear_slash_completion_selection();
            return None;
        }
        let visible = completions_len
            .min(visible_count)
            .min(SLASH_COMPLETION_MAX_VISIBLE);
        let selected = self
            .slash_acomp_sel
            .unwrap_or(0)
            .min(completions_len.saturating_sub(1));
        self.slash_acomp_sel = Some(selected);
        if selected < self.slash_acomp_scroll {
            self.slash_acomp_scroll = selected;
        } else if selected >= self.slash_acomp_scroll.saturating_add(visible) {
            self.slash_acomp_scroll = selected.saturating_add(1).saturating_sub(visible);
        }
        let max_scroll = completions_len.saturating_sub(visible);
        self.slash_acomp_scroll = self.slash_acomp_scroll.min(max_scroll);
        Some((selected, self.slash_acomp_scroll))
    }

    fn move_slash_completion_selection(&mut self, delta: isize) -> bool {
        let completions_len = slash_completions(&self.input).len();
        if completions_len == 0 {
            self.clear_slash_completion_selection();
            return false;
        }
        let current = self
            .slash_acomp_sel
            .unwrap_or(0)
            .min(completions_len.saturating_sub(1));
        let next = ((current as isize + delta).rem_euclid(completions_len as isize)) as usize;
        self.slash_acomp_sel = Some(next);
        self.sync_slash_completion_window(completions_len, SLASH_COMPLETION_MAX_VISIBLE);
        true
    }

    fn accept_slash_completion(&mut self) -> bool {
        let completions = slash_completions(&self.input);
        if completions.is_empty() {
            return false;
        }
        let idx = self
            .slash_acomp_sel
            .unwrap_or(0)
            .min(completions.len().saturating_sub(1));
        self.input = completions[idx].text.clone();
        self.cursor = self.input.len();
        self.reset_slash_completion_selection();
        true
    }

    fn work_map_is_active(&self) -> bool {
        self.work_map
            .as_ref()
            .is_some_and(|drawer| !drawer.waypoint_ids.is_empty())
    }

    fn set_work_map_selection(&mut self, selected: usize) {
        if let Some(drawer) = self.work_map.as_mut() {
            let max = drawer.waypoint_ids.len().saturating_sub(1);
            drawer.selected = selected.min(max);
            let visible_rows = drawer
                .waypoint_ids
                .len()
                .clamp(1, WORK_MAP_DRAWER_MAX_BODY_ROWS);
            sync_work_map_scroll(drawer, visible_rows);
        }
    }

    fn move_work_map_selection(&mut self, delta: isize) -> bool {
        self.move_work_map_selection_for_rows(delta, WORK_MAP_DRAWER_MAX_BODY_ROWS)
    }

    fn move_work_map_selection_for_rows(&mut self, delta: isize, visible_rows: usize) -> bool {
        let Some(drawer) = self.work_map.as_mut() else {
            return false;
        };
        if drawer.waypoint_ids.is_empty() {
            return false;
        }
        let current = drawer
            .selected
            .min(drawer.waypoint_ids.len().saturating_sub(1));
        let next = if delta < 0 {
            current.saturating_sub(delta.unsigned_abs())
        } else {
            current
                .saturating_add(delta as usize)
                .min(drawer.waypoint_ids.len().saturating_sub(1))
        };
        if next == current {
            return false;
        }
        drawer.selected = next;
        sync_work_map_scroll(drawer, visible_rows);
        true
    }

    fn selected_work_map_command_arg(&self) -> Option<String> {
        let drawer = self.work_map.as_ref()?;
        let id = drawer.waypoint_ids.get(drawer.selected)?;
        if let Some(selector) = drawer.selector.as_deref().filter(|s| !s.trim().is_empty()) {
            Some(format!("{} {id}", selector.trim()))
        } else {
            Some(id.clone())
        }
    }

    fn managed_region_contains(&self, column: u16, row: u16) -> bool {
        let transcript = self.transcript_area;
        let input = self.input_area;
        let transcript_contains = transcript.width > 0
            && transcript.height > 0
            && column >= transcript.x
            && column < transcript.x.saturating_add(transcript.width)
            && row >= transcript.y
            && row < transcript.y.saturating_add(transcript.height);
        let input_contains = input.width > 0
            && input.height > 0
            && column >= input.x
            && column < input.x.saturating_add(input.width)
            && row >= input.y
            && row < input.y.saturating_add(input.height);
        transcript_contains || input_contains
    }

    fn set_transcript_layout(&mut self, layout: TranscriptLayoutState) {
        self.transcript_area = layout.transcript_area;
        self.input_area = layout.input_area;
        self.transcript_total_lines = layout.total_lines;
        self.transcript_visible_lines = layout.visible_lines;
        self.live_indicator_lines = layout.live_indicator_lines;
        self.live_indicator_top_padding = layout.live_indicator_top_padding;
        self.live_indicator_visible = layout.live_indicator_visible;
        self.live_indicator_scroll_start = layout.live_indicator_scroll_start;
        self.live_indicator_scroll_end = layout.live_indicator_scroll_end;
        self.transcript_line_layout = layout.transcript_line_layout;
        self.live_indicator_line_layout = layout.live_indicator_line_layout;
        self.live_indicator_text = layout.live_indicator_text;
        self.transcript_scroll_max = layout.total_lines.saturating_sub(layout.visible_lines);
        self.live_indicator_height = layout.live_indicator_lines.min(u16::MAX as usize) as u16;
        self.transcript_hover_expandable = None;
        self.clamp_transcript_scroll();
    }

    fn tool_tag_for(&mut self, call_id: &str) -> String {
        if let Some(tag) = self.call_tags.get(call_id) {
            return tag.clone();
        }
        self.call_tag_seq = self.call_tag_seq.saturating_add(1);
        let tag = format!("#{}.{}", self.turn_seq, self.call_tag_seq);
        self.call_tags.insert(call_id.to_string(), tag.clone());
        tag
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TurnStart => {
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.agent_busy = true;
                self.status = "scroll: live".into();
                self.external_telemetry = ExternalTelemetry::default();
                self.retry_status = None;
                self.turn_seq = self.turn_seq.saturating_add(1);
                self.call_tag_seq = 0;
                self.turn_tool_counts.clear();
                self.turn_error_count = 0;
                self.turn_start_at = Some(Instant::now());
                if self
                    .detached_subagent
                    .as_ref()
                    .is_some_and(|ds| ds.completed)
                {
                    self.detached_subagent = None;
                }
            }
            AgentEvent::HistoryContextUpdated { chars, tokens } => {
                self.history_chars = chars as u64;
                self.last_turn_context_tokens =
                    tokens.unwrap_or_else(|| ((self.history_chars.saturating_add(3)) / 4).max(1));
            }
            AgentEvent::TextDelta(t) => {
                self.retry_status = None;
                if self.stream_started_at.is_none() {
                    self.stream_started_at = Some(Instant::now());
                    if self.agent_busy {
                        self.status = "scroll: live".into();
                    }
                }
                let char_count = t.chars().count() as u64;
                self.stream_chars = self.stream_chars.saturating_add(char_count);
                self.history_chars = self.history_chars.saturating_add(char_count);
                self.streaming_text.push_str(&t);
            }
            AgentEvent::TextBlockComplete(full) => {
                if !full.is_empty() {
                    let dim_prefix = self.assistant_prefix_seen;
                    self.assistant_prefix_seen = true;
                    self.queue(Line_::Assistant {
                        text: full,
                        dim_prefix,
                    });
                }
                self.streaming_text.clear();
                self.stream_started_at = None;
                self.stream_chars = 0;
            }
            AgentEvent::ThinkingDelta(t) => {
                self.retry_status = None;
                self.streaming_thinking.push_str(&t);
                if self.streaming_text.is_empty() && self.stream_started_at.is_none() {
                    self.status = "scroll: live".into();
                }
            }
            AgentEvent::ThinkingBlockComplete(full) => {
                if !full.is_empty() {
                    let word_count = full.split_whitespace().count();
                    if self.verbose {
                        self.queue(Line_::Thinking(full));
                    }
                    self.status = if self.verbose {
                        format!("thinking done ({} words)", word_count)
                    } else {
                        format!("thinking hidden ({} words)", word_count)
                    };
                }
                self.streaming_thinking.clear();
            }
            AgentEvent::ToolCallPreview {
                call_id,
                name,
                summary,
            } => {
                let is_subagent = is_subagent_call_id(&call_id);
                let call_tag = self.tool_tag_for(&call_id);
                if let Some(existing) = self.live_tools.iter_mut().find(|t| t.call_id == call_id) {
                    if existing.summary.is_empty() {
                        existing.summary = summary;
                    }
                    existing.name = name;
                    existing.is_subagent = is_subagent;
                } else {
                    self.live_tools.push(LiveTool {
                        call_id,
                        call_tag,
                        name,
                        summary,
                        running: false,
                        started: None,
                        is_subagent,
                    });
                }
            }
            AgentEvent::ToolCallStart {
                call_id,
                name,
                summary,
            } => {
                let is_subagent = is_subagent_call_id(&call_id);
                if !is_subagent && self.sub_batch.as_ref().is_some_and(|b| b.done) {
                    self.sub_batch = None;
                }
                let preferred_tag = self.tool_tag_for(&call_id);
                let match_idx = self
                    .live_tools
                    .iter()
                    .position(|entry| entry.call_id == call_id)
                    .or_else(|| {
                        self.live_tools
                            .iter()
                            .position(|entry| !entry.running && entry.name == name)
                    });

                if let Some(i) = match_idx {
                    let mut final_tag = preferred_tag;
                    if self.live_tools[i].call_id != call_id {
                        final_tag = self.live_tools[i].call_tag.clone();
                        self.call_tags.insert(call_id.clone(), final_tag.clone());
                    }
                    let entry = &mut self.live_tools[i];
                    entry.call_id = call_id.clone();
                    entry.running = true;
                    entry.started = Some(Instant::now());
                    entry.call_tag = final_tag;
                    entry.name = name.clone();
                    entry.is_subagent = is_subagent;
                    if !summary.is_empty() {
                        entry.summary = summary.clone();
                    }
                } else {
                    self.live_tools.push(LiveTool {
                        call_id,
                        call_tag: preferred_tag,
                        name: name.clone(),
                        summary,
                        running: true,
                        started: Some(Instant::now()),
                        is_subagent,
                    });
                }
                self.retry_status = None;
                self.status = format!("running {name}");
            }
            AgentEvent::ToolCallResult {
                call_id,
                name,
                ok,
                preview,
                content,
            } => {
                let is_subagent = is_subagent_call_id(&call_id);
                let call_tag = self.tool_tag_for(&call_id);
                let idx = self
                    .live_tools
                    .iter()
                    .position(|t| t.call_id == call_id && t.running)
                    .or_else(|| {
                        self.live_tools
                            .iter()
                            .position(|t| t.call_id == call_id || (t.name == name && t.running))
                    })
                    .or_else(|| self.live_tools.iter().position(|t| t.name == name));
                let (n, mut summary, tool_is_subagent, started_at) = if let Some(i) = idx {
                    let t = self.live_tools.remove(i);
                    (t.name, t.summary, t.is_subagent, t.started)
                } else {
                    (name.clone(), String::new(), is_subagent, None)
                };
                let duration_secs = started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let denied = !ok && content.contains("permission denied");
                if !preview.is_empty() {
                    summary = preview;
                }

                self.retry_status = None;
                if tool_is_subagent {
                    self.status = "subagent running".into();
                    return;
                }

                if ok && !content.is_empty() && content_has_more_than_preview(&content) {
                    self.last_expandable = Some(ExpandableBlock {
                        name: n.clone(),
                        expanded: false,
                    });
                } else {
                    self.last_expandable = None;
                }
                *self.turn_tool_counts.entry(n.clone()).or_insert(0) += 1;
                if !ok {
                    self.turn_error_count += 1;
                }
                let lines_count = content.lines().count();
                if matches!(n.as_str(), "todo_read" | "todo_write")
                    && let Some(progress) = todo_progress_from_content(&content)
                {
                    self.todo_progress = Some(progress);
                }
                let chunk = ToolChunk {
                    call_tag: call_tag.clone(),
                    summary: summary.clone(),
                    content: content.clone(),
                };
                self.queue(Line_::Tool {
                    call_tag,
                    name: n.clone(),
                    summary,
                    ok: Some(ok),
                    content,
                    group_count: 1,
                    group_lines: lines_count,
                    group_chunks: vec![chunk],
                    duration_secs,
                    denied,
                    dim: false,
                    density_rank: 1,
                    expanded: false,
                });
                if n == "subagent" {
                    self.live_tools.retain(|t| !t.is_subagent);
                }
                self.status = "thinking".into();
            }
            AgentEvent::LocalAuthPrompt { tool, message } => {
                self.status = "local sudo prompt".into();
                self.queue(Line_::LocalAuth { tool, message });
            }
            AgentEvent::ToolBatchStart {
                batch_id,
                call_ids,
                labels,
            } => {
                if is_subagent_batch_id(&batch_id) {
                    let mut entries: Vec<String> = Vec::new();
                    for (i, call_id) in call_ids.into_iter().enumerate() {
                        let call_tag = self.tool_tag_for(&call_id);
                        let label = labels.get(i).cloned().unwrap_or_else(|| "tool".to_string());
                        entries.push(format!("{call_tag} {label}"));
                    }
                    self.sub_batch = Some(LiveBatch {
                        entries,
                        failed: 0,
                        done: false,
                    });
                }
            }
            AgentEvent::ToolBatchEnd {
                batch_id,
                call_ids,
                labels,
                failed,
            } => {
                if is_subagent_batch_id(&batch_id) {
                    let mut entries: Vec<String> = Vec::new();
                    for (i, call_id) in call_ids.into_iter().enumerate() {
                        let call_tag = self.tool_tag_for(&call_id);
                        let label = labels.get(i).cloned().unwrap_or_else(|| "tool".to_string());
                        entries.push(format!("{call_tag} {label}"));
                    }
                    self.sub_batch = Some(LiveBatch {
                        entries,
                        failed,
                        done: true,
                    });
                }
            }
            AgentEvent::UsageUpdate { turn, session } => {
                self.usage = session;
                // `turn` is the single request that just completed. After the
                // provider-side normalization, input/cache_read/cache_create
                // are disjoint sets across every supported provider, so the
                // actual context the model saw is their sum.
                let turn_ctx = turn.context_tokens();
                if turn_ctx > 0 {
                    self.last_turn_context_tokens = turn_ctx;
                    self.history_chars = turn_ctx.saturating_mul(4);
                } else {
                    self.last_turn_context_tokens =
                        ((self.history_chars.saturating_add(3)) / 4).max(1);
                }
            }
            AgentEvent::HttpRetry {
                attempt,
                wait_secs,
                reason,
            } => {
                self.retry_status = Some(format!("retry backoff {attempt}/4"));
                self.status = format!("retry backoff {attempt}/4");
                if attempt == 1 {
                    self.queue(Line_::Retry(format!(
                        "retry {attempt}/4 ({reason}) in {wait_secs}s"
                    )));
                }
            }
            AgentEvent::ExternalTelemetry { telemetry } => {
                self.external_telemetry = telemetry;
            }
            AgentEvent::TurnDiagnostics {
                provider,
                api_family,
                auth_source,
                model,
                last_retry_reason,
                workaround_fired,
                ..
            } => {
                self.provider_label = provider;
                self.api_family = api_family;
                self.auth_source = auth_source;
                self.model = model;
                self.last_retry_reason = last_retry_reason;
                self.workaround_fired = workaround_fired;
            }
            AgentEvent::ThinkingEffortChanged { effort } => {
                self.thinking_effort = effort;
            }
            AgentEvent::ApprovalProfileChanged { profile } => {
                self.approval_profile = profile;
            }
            AgentEvent::Info(s) => self.queue(Line_::Info(s)),
            AgentEvent::Warn(s) => self.queue(Line_::Warn(s)),
            AgentEvent::Error(s) => self.queue(Line_::Error(s)),
            AgentEvent::Slash(s) => {
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.stream_started_at = None;
                self.stream_chars = 0;
                self.live_tools.clear();
                self.sub_batch = None;
                self.agent_busy = false;
                self.status = "ready".into();
                if let Some(ds) = parse_detached_subagent_launch(&s) {
                    self.detached_subagent = Some(ds);
                }
                self.queue(Line_::Info(s));
            }
            AgentEvent::WorkMap {
                kind,
                text,
                waypoint_ids,
                selector,
            } => {
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.stream_started_at = None;
                self.stream_chars = 0;
                self.live_tools.clear();
                self.sub_batch = None;
                self.agent_busy = false;
                let visible_ids = visible_work_map_ids(&text, &waypoint_ids);
                if matches!(kind, WorkMapEventKind::Map) && !visible_ids.is_empty() {
                    self.work_map = Some(WorkMapDrawer {
                        text,
                        waypoint_ids: visible_ids,
                        selector,
                        selected: 0,
                        scroll: 0,
                    });
                    self.status = "work map open in composer: ↑/↓ select · Enter focus · p packet · t track · Esc close"
                        .into();
                } else {
                    self.work_map = None;
                    self.status = "work map ready".into();
                    self.queue(Line_::WorkMap {
                        kind,
                        text,
                        waypoint_ids: visible_ids,
                        selector,
                        selected: 0,
                    });
                }
            }
            AgentEvent::TurnEnd { .. } => {
                self.compacting = false;
                self.compacting_resume_busy = false;
                if let Some((tool_total, tool_summary)) = turn_tool_summary(&self.turn_tool_counts)
                {
                    let elapsed = self
                        .turn_start_at
                        .map(|t| format_duration(t.elapsed().as_secs()))
                        .unwrap_or_default();
                    let error_note = if self.turn_error_count > 0 {
                        format!(
                            " · {} error{}",
                            self.turn_error_count,
                            if self.turn_error_count > 1 { "s" } else { "" }
                        )
                    } else {
                        " · no errors".to_string()
                    };
                    self.queue(Line_::Info(format!(
                        "Turn used {} tool call{} ({}) · {elapsed}{error_note}",
                        tool_total,
                        if tool_total == 1 { "" } else { "s" },
                        tool_summary,
                    )));
                }
                self.agent_busy = false;
                self.status = "ready".into();
                self.retry_status = None;
                self.stream_started_at = None;
                self.stream_chars = 0;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.live_tools.clear();
                self.sub_batch = None;
            }
            AgentEvent::CompactStart => {
                self.compacting_resume_busy = self.agent_busy;
                self.compacting = true;
                self.agent_busy = true;
                self.status = "compacting history".into();
            }

            AgentEvent::CompactEnd { before, after } => {
                let resume_busy = self.compacting_resume_busy;
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.agent_busy = resume_busy;
                self.last_turn_context_tokens = self
                    .last_turn_context_tokens
                    .max(((self.history_chars.saturating_add(3)) / 4).max(1));
                self.queue(Line_::Info(format!(
                    "compacted {before} → {after} messages"
                )));
                self.status = if self.agent_busy {
                    "thinking".into()
                } else {
                    "ready".into()
                };
            }
            AgentEvent::CompactFailed { message } => {
                let resume_busy = self.compacting_resume_busy;
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.agent_busy = resume_busy;
                self.queue(Line_::Warn(format!("compact failed: {message}")));
                self.status = if self.agent_busy {
                    "thinking".into()
                } else {
                    "ready".into()
                };
            }
            AgentEvent::Interrupted => {
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.queue(Line_::Warn("interrupted".into()));
                self.agent_busy = false;
                self.status = "ready".into();
                self.stream_started_at = None;
                self.stream_chars = 0;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.live_tools.clear();
                self.sub_batch = None;
            }
            AgentEvent::SteeringReceived { messages, preview } => {
                let noun = if messages == 1 { "update" } else { "updates" };
                self.status = format!("queued {messages} {noun} for next response");
                self.queue(Line_::SteeringDelivered { messages, preview });
            }
        }
    }
}

fn todo_text_from_line(line: &str) -> String {
    let without_mark = line
        .trim_start()
        .trim_start_matches(['✓', '►', '○'])
        .trim_start();
    without_mark
        .strip_suffix(" [in_progress]")
        .or_else(|| without_mark.strip_suffix(" [completed]"))
        .or_else(|| without_mark.strip_suffix(" [pending]"))
        .unwrap_or(without_mark)
        .trim()
        .to_string()
}

fn todo_progress_from_content(content: &str) -> Option<TodoProgress> {
    let mut total = 0usize;
    let mut completed = 0usize;
    let mut in_progress = 0usize;
    let mut active = None;
    for line in content.lines() {
        let trimmed = line.trim_start();
        let Some(mark) = trimmed.chars().next() else {
            continue;
        };
        match mark {
            '✓' => {
                total += 1;
                completed += 1;
            }
            '►' => {
                total += 1;
                in_progress += 1;
                if active.is_none() {
                    let text = todo_text_from_line(trimmed);
                    if !text.is_empty() {
                        active = Some(text);
                    }
                }
            }
            '○' => total += 1,
            _ => {}
        }
    }
    (total > 0).then_some(TodoProgress {
        total,
        completed,
        in_progress,
        active,
    })
}

fn todo_progress_label(progress: &TodoProgress) -> String {
    let active = match (progress.in_progress, progress.active.as_deref()) {
        (0, _) => String::new(),
        (1, Some(text)) => format!(" · active: {text}"),
        (n, Some(text)) => format!(" · {n} active: {text}"),
        (n, None) => format!(" · {n} active"),
    };
    format!(
        "[{}/{} todos done{}]",
        progress.completed, progress.total, active
    )
}

fn derived_busy_status(state: &TuiState) -> String {
    if state.compacting {
        return "compacting history".to_string();
    }
    if let Some(retry) = &state.retry_status {
        return retry.clone();
    }
    if let Some(ds) = &state.detached_subagent {
        if !ds.completed {
            return "subagent running".to_string();
        }
    }
    let running_tools: Vec<&LiveTool> = state
        .live_tools
        .iter()
        .filter(|tool| tool.running)
        .collect();
    if !running_tools.is_empty() {
        let subagents = running_tools.iter().filter(|tool| tool.is_subagent).count();
        let regular = running_tools.len().saturating_sub(subagents);
        if regular == 1
            && let Some(tool) = running_tools.iter().find(|tool| !tool.is_subagent)
        {
            return format!("running {}", tool.name);
        }
        if regular > 1 {
            return format!("running {regular} tools");
        }
        if subagents == 1 {
            return "subagent running".to_string();
        }
        if subagents > 1 {
            return format!("{subagents} subagents running");
        }
    }
    if state.sub_batch.as_ref().is_some_and(|batch| !batch.done) {
        return "subagent batch running".to_string();
    }
    if !state.streaming_text.is_empty() {
        return "responding".to_string();
    }
    if !state.streaming_thinking.is_empty() {
        return "thinking".to_string();
    }
    state.status.clone()
}

fn format_elapsed(elapsed: Duration) -> String {
    let secs = elapsed.as_secs();
    if secs < 60 {
        format!("{secs}s")
    } else {
        format!("{}m {:02}s", secs / 60, secs % 60)
    }
}

fn live_indicator_elapsed(state: &TuiState) -> Option<String> {
    let started = state
        .live_tools
        .iter()
        .filter(|tool| tool.running)
        .filter_map(|tool| tool.started)
        .min()
        .or(state.stream_started_at);
    started.map(|started| format_elapsed(started.elapsed()))
}

fn live_detail_line(detail: String, color: Color, max_cells: usize) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ↳ ", Style::default().fg(Color::DarkGray)),
        Span::styled(clamp_chars(&detail, max_cells), Style::default().fg(color)),
    ])
}

fn live_thinking_detail_line(detail: String, max_cells: usize) -> Line<'static> {
    let style = Style::default().fg(Color::Gray).bg(THINKING_BG);
    Line::from(vec![
        Span::styled(
            "  ↳ ",
            Style::default().fg(Color::Indexed(244)).bg(THINKING_BG),
        ),
        Span::styled(clamp_chars(&detail, max_cells), style),
    ])
}

fn live_indicator_todo_detail(state: &TuiState, max_cells: usize) -> Option<Line<'static>> {
    state
        .todo_progress
        .as_ref()
        .map(todo_progress_label)
        .map(|detail| live_detail_line(detail, Color::Green, max_cells))
}

fn live_indicator_detail(state: &TuiState, width: u16) -> Option<Line<'static>> {
    if width == 0 || state.pending_perm.is_some() {
        return None;
    }
    let max_cells = width.saturating_sub(4) as usize;
    if !state.streaming_text.is_empty() {
        let tail = state
            .streaming_text
            .lines()
            .last()
            .unwrap_or(&state.streaming_text)
            .trim();
        if !tail.is_empty() {
            return Some(Line::from(vec![
                Span::styled("  ▸ ", Style::default().fg(Color::Blue)),
                Span::styled(clamp_chars(tail, max_cells), Style::default()),
            ]));
        }
    }
    if let Some(ds) = &state.detached_subagent {
        if !ds.completed {
            let last_tool = ds.tail.iter().rev().find(|l| l.starts_with("▶"));
            let last_line = ds.tail.last().map(|s| s.as_str()).unwrap_or("launched");
            let label = if let Some(tool) = last_tool {
                format!("⟨sub⟩ {}", tool.trim_start_matches("▶ ").trim())
            } else if ds.task.is_empty() {
                last_line.to_string()
            } else {
                let task_head = ds.task.chars().take(30).collect::<String>();
                format!("⟨sub⟩ {task_head}")
            };
            return Some(live_detail_line(label, Color::Cyan, max_cells));
        } else {
            return Some(live_detail_line(
                "⟨sub⟩ complete".to_string(),
                Color::Green,
                max_cells,
            ));
        }
    }
    let running_tools: Vec<&LiveTool> = state
        .live_tools
        .iter()
        .filter(|tool| tool.running)
        .collect();
    if let Some(tool) = running_tools.iter().find(|tool| !tool.summary.is_empty()) {
        return Some(live_detail_line(
            tool.summary.clone(),
            Color::Reset,
            max_cells,
        ));
    }
    if let Some(batch) = state
        .sub_batch
        .as_ref()
        .filter(|batch| !batch.entries.is_empty())
    {
        let detail = if batch.done {
            format!(
                "batch complete · {} entries · {} failed",
                batch.entries.len(),
                batch.failed
            )
        } else {
            format!("batch active · {} entries", batch.entries.len())
        };
        return Some(live_detail_line(detail, Color::Reset, max_cells));
    }
    if state.verbose && !state.streaming_thinking.is_empty() {
        let tail = state
            .streaming_thinking
            .lines()
            .last()
            .unwrap_or(&state.streaming_thinking)
            .trim();
        if !tail.is_empty() {
            return Some(live_thinking_detail_line(tail.to_string(), max_cells));
        }
    }
    live_indicator_todo_detail(state, max_cells)
}

fn display_busy_status(status: String) -> String {
    if status == "thinking" {
        "Thinking".to_string()
    } else {
        status
    }
}

fn status_spans(state: &TuiState) -> Vec<Span<'_>> {
    let (marker, marker_color) = if state.agent_busy {
        let c = SPINNER_FRAMES[(state.frame_count % SPINNER_FRAMES.len() as u64) as usize];
        (format!("{c} "), Color::Yellow)
    } else {
        ("● ".to_string(), Color::Green)
    };
    let mut spans = vec![Span::styled(marker, Style::default().fg(marker_color))];

    let cwd = home_tilde(&state.sandbox);
    spans.push(Span::styled(
        clamp_chars(&cwd, 40),
        Style::default().fg(Color::Green),
    ));

    if let Some(branch) = &state.git_branch {
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("({branch})"),
            Style::default().fg(Color::Magenta),
        ));
    }

    if !state.provider_label.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            status_provider_label(&state.provider_label, &state.api_family),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let usage = &state.usage;
    let actual_in = usage.actual_input_tokens();
    let cached_in = usage.cached_input_tokens();
    if actual_in + cached_in + usage.output > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("↑{}", format_count(actual_in)),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("↻ {}", format_count(cached_in)),
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("↓{}", format_count(usage.output)),
            Style::default().fg(Color::DarkGray),
        ));
    }

    let window = model_context_window(&state.model);
    let ctx_used = if state.last_turn_context_tokens > 0 {
        state.last_turn_context_tokens
    } else if state.history_chars > 0 {
        (state.history_chars / 4).max(1)
    } else {
        0
    };
    if window > 0 {
        let pct = ((ctx_used as f64 / window as f64) * 100.0)
            .clamp(0.0, 100.0)
            .round() as u64;
        let bar_color = if pct >= 90 {
            Color::Red
        } else if pct >= 70 {
            Color::Yellow
        } else {
            Color::Cyan
        };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("{}/{}", format_count(ctx_used), format_count(window)),
            Style::default().fg(bar_color),
        ));
        spans.push(Span::styled(" ctx", Style::default().fg(Color::DarkGray)));
        spans.push(Span::raw(" "));
        spans.push(Span::styled(
            format!("{pct}%"),
            Style::default().fg(bar_color),
        ));
    }

    let ext = state.external_telemetry;
    let ext_total = ext.dedupe_hits
        + ext.similarity_blocks
        + ext.circuit_breaker_trips
        + ext.partial_delivery_hints
        + ext.http_retries;
    if ext_total > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled("ext", Style::default().fg(Color::LightBlue)));
        let counters = [
            (ext.dedupe_hits, "d", Color::Cyan),
            (ext.circuit_breaker_trips, "cb", Color::Yellow),
            (ext.similarity_blocks, "sg", Color::Magenta),
            (ext.partial_delivery_hints, "ph", Color::Green),
            (ext.http_retries, "rt", Color::DarkGray),
        ];
        for (value, label, color) in counters {
            if value > 0 {
                spans.push(Span::raw(" "));
                spans.push(Span::styled(
                    format!("{label}{value}"),
                    Style::default().fg(color),
                ));
            }
        }
    }

    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        state.model.clone(),
        Style::default().fg(Color::Cyan),
    ));
    spans.push(Span::raw("  "));
    spans.push(Span::styled(
        format!("reasoning:{}", state.thinking_effort.as_str()),
        Style::default().fg(Color::Magenta),
    ));

    match state.approval_profile {
        ApprovalProfile::Ask | ApprovalProfile::Always => {}
        profile => {
            spans.insert(1, Span::styled("  ", Style::default().fg(Color::DarkGray)));
            spans.insert(
                2,
                Span::styled(
                    format!("approval:{}  ", profile.as_str()),
                    Style::default().fg(Color::Yellow),
                ),
            );
        }
    }

    spans
}

fn status_provider_label(provider: &str, api_family: &str) -> String {
    match (provider, api_family) {
        (provider, "") => provider.to_string(),
        ("chatgpt", "chatgpt-responses" | "chatgpt") => "chatgpt".to_string(),
        ("openai", "openai-chat-completions" | "openai") => "openai".to_string(),
        ("anthropic", "anthropic-messages" | "anthropic") => "anthropic".to_string(),
        (provider, "chatgpt-responses") => format!("{provider}:chatgpt"),
        (provider, "openai-chat-completions") => format!("{provider}:openai"),
        (provider, "anthropic-messages") => format!("{provider}:anthropic"),
        (provider, api_family) if provider == api_family => provider.to_string(),
        (provider, api_family) => format!("{provider}:{api_family}"),
    }
}

fn home_tilde(path: &str) -> String {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    if !home.is_empty() {
        let norm_path = path.replace('\\', "/");
        let norm_home = home.replace('\\', "/");
        if let Some(rest) = norm_path.strip_prefix(&norm_home) {
            let mut out = String::from("~");
            if !rest.starts_with('/') && !rest.is_empty() {
                out.push('/');
            }
            out.push_str(rest);
            return out;
        }
    }
    path.to_string()
}

fn format_count(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}k", n as f64 / 1_000.0)
    } else {
        n.to_string()
    }
}

fn content_has_more_than_preview(s: &str) -> bool {
    s.lines().count() > COLLAPSED_PREVIEW_LINES
}

fn turn_tool_summary(counts: &HashMap<String, usize>) -> Option<(usize, String)> {
    let total = counts.values().copied().sum::<usize>();
    if total == 0 {
        return None;
    }

    let categories: &[(&[&str], &str, &str)] = &[
        (&["read_file", "read_symbol"], "read", "reads"),
        (&["rg", "fzf"], "search", "searches"),
        (&["fd"], "find", "finds"),
        (&["edit_file", "multi_edit"], "edit", "edits"),
        (&["bash"], "command", "commands"),
        (&["write_file"], "write", "writes"),
        (&["git_diff", "git_log", "git_commit"], "git op", "git ops"),
        (&["todo_read", "todo_write"], "todo op", "todo ops"),
        (&["jq", "awk", "csvkit"], "data op", "data ops"),
        (&["http"], "request", "requests"),
    ];

    let mut accounted = 0usize;
    let mut parts = Vec::new();
    for (tools, singular, plural) in categories {
        let count = tools
            .iter()
            .filter_map(|tool| counts.get(*tool))
            .copied()
            .sum::<usize>();
        if count > 0 {
            accounted = accounted.saturating_add(count);
            parts.push(format!(
                "{count} {}",
                if count == 1 { *singular } else { *plural }
            ));
        }
    }

    let other = total.saturating_sub(accounted);
    if other > 0 {
        parts.push(format!(
            "{other} other call{}",
            if other == 1 { "" } else { "s" }
        ));
    }

    Some((total, parts.join(", ")))
}

fn format_duration(secs: u64) -> String {
    if secs >= 60 {
        let m = secs / 60;
        let s = secs % 60;
        if s == 0 {
            format!("{m}m")
        } else {
            format!("{m}m{s:02}s")
        }
    } else {
        format!("{secs}s")
    }
}

fn tool_summary_body<'a>(name: &str, summary: &'a str) -> &'a str {
    summary
        .strip_prefix(&format!("{name}: "))
        .unwrap_or(summary)
}

fn permission_command_text(name: &str, input: &Value) -> String {
    match name {
        "bash" => input["command"]
            .as_str()
            .unwrap_or("?")
            .trim_end()
            .to_string(),
        "write_file" => {
            let path = input["path"].as_str().unwrap_or("?");
            let bytes = input["content"].as_str().map(|s| s.len()).unwrap_or(0);
            format!("{path} ({bytes} bytes)")
        }
        "edit_file" => input["path"].as_str().unwrap_or("?").to_string(),
        "multi_edit" => {
            let path = input["path"].as_str().unwrap_or("?");
            let edits = input["edits"].as_array().map(|a| a.len()).unwrap_or(0);
            format!("{path} ({edits} edits)")
        }
        "git_commit" => {
            let message = clamp_chars(input["message"].as_str().unwrap_or("?").trim(), 90);
            let paths = input["paths"].as_array().map(|a| a.len()).unwrap_or(0);
            if paths > 0 {
                format!(
                    "message: {message} · {paths} path{}",
                    if paths == 1 { "" } else { "s" }
                )
            } else {
                format!("message: {message}")
            }
        }
        "todo_write" => {
            let count = input["todos"].as_array().map(|a| a.len()).unwrap_or(0);
            format!("{count} todo entr{}", if count == 1 { "y" } else { "ies" })
        }
        _ => {
            let summary = summarize_call(name, input);
            tool_summary_body(name, &summary).to_string()
        }
    }
}

fn permission_audit_label(name: &str, input: &Value) -> String {
    let collapsed = permission_command_text(name, input)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ");
    clamp_chars(&collapsed, 120)
}

fn wrap_plain_visual(text: &str, max_cols: usize) -> Vec<String> {
    wrap_input_visual(&sanitize_display_text(text), text.len(), max_cols.max(1)).0
}

fn wrap_plain_words_visual(text: &str, max_cols: usize) -> Vec<String> {
    let max_cols = max_cols.max(1);
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut current_width = 0usize;

    for word in text.split_whitespace() {
        let mut rest = word.to_string();
        while !rest.is_empty() {
            let rest_width = text_width(&rest);
            if current_width == 0 {
                if rest_width <= max_cols {
                    current_width = rest_width;
                    current = rest;
                    break;
                }
                let (chunk, tail) = split_display_cells(&rest, max_cols);
                if !chunk.is_empty() {
                    lines.push(chunk);
                }
                rest = tail;
            } else if current_width + 1 + rest_width <= max_cols {
                current.push(' ');
                current.push_str(&rest);
                current_width += 1 + rest_width;
                break;
            } else {
                lines.push(std::mem::take(&mut current));
                current_width = 0;
            }
        }
    }

    if !current.is_empty() {
        lines.push(current);
    }
    if lines.is_empty() {
        lines.push(String::new());
    }
    lines
}

fn permission_prompt_text(
    tool: &str,
    command: &str,
    risk: crate::tool_policy::CommandRisk,
    width: u16,
) -> Text<'static> {
    let tier = PermissionTier::from_risk(risk);
    let accent = tier.accent();
    let prefix_style = Style::default().fg(accent).add_modifier(Modifier::BOLD);
    let body_width = width.saturating_sub(2).max(1) as usize;
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut push_line = |body: String, style: Style| {
        let first_prefix = "▌ ".to_string();
        let cont_prefix = "▌ ".to_string();
        let text_width = body_width.saturating_sub(first_prefix.len()).max(1);
        let wrapped = wrap_plain_visual(&body, text_width);
        for (idx, row) in wrapped.into_iter().enumerate() {
            let prefix = if idx == 0 {
                &first_prefix
            } else {
                &cont_prefix
            };
            lines.push(Line::from(vec![
                Span::styled(prefix.clone(), prefix_style),
                Span::styled(row, style),
            ]));
        }
    };

    push_line(
        format!("ask {tool} · risk={} · {command}", risk.label()),
        Style::default().fg(accent).add_modifier(Modifier::BOLD),
    );
    push_line(
        "[y] once   [a] always   [n] deny".to_string(),
        Style::default().fg(Color::DarkGray),
    );
    Text::from(lines)
}

fn dim_text(text: &mut Text<'static>) {
    for line in &mut text.lines {
        for span in &mut line.spans {
            span.style = span.style.add_modifier(Modifier::DIM);
        }
    }
}

fn transcript_item_should_dim(item: &Line_, state: &TuiState) -> bool {
    state.pending_perm.is_some() && !matches!(item, Line_::PermissionPrompt { .. })
}

fn replace_last_permission_entry(items: &mut [Line_], replacement: Line_) -> bool {
    if let Some(idx) = items
        .iter()
        .rposition(|item| matches!(item, Line_::PermissionPrompt { .. }))
    {
        items[idx] = replacement;
        true
    } else {
        false
    }
}

fn visible_work_map_ids(text: &str, waypoint_ids: &[String]) -> Vec<String> {
    waypoint_ids
        .iter()
        .filter(|id| {
            text.lines()
                .any(|line| line.trim_start().starts_with(id.as_str()))
        })
        .cloned()
        .collect()
}

fn extract_path_from_summary(summary: &str) -> Option<String> {
    let after_colon = summary.split_once(": ").map(|(_, r)| r).unwrap_or(summary);
    let path_part = after_colon
        .split_once(" (")
        .map(|(p, _)| p)
        .unwrap_or(after_colon);
    let trimmed = path_part.trim();
    if trimmed.is_empty() || trimmed == "?" {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn short_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(idx) = normalized.rfind("/src/") {
        normalized[idx + 1..].to_string()
    } else if let Some(slash) = normalized.rfind('/') {
        normalized[slash + 1..].to_string()
    } else {
        normalized
    }
}

fn display_read_file_path(path: &str) -> String {
    let normalized = path.replace('\\', "/");
    if let Some(idx) = normalized.find("src/") {
        normalized[idx..].to_string()
    } else {
        short_path(path)
    }
}

fn count_diff_stats(content: &str) -> (usize, usize) {
    let mut added = 0usize;
    let mut removed = 0usize;
    for line in content.lines() {
        let stripped = strip_line_numbers(line);
        if stripped.starts_with('+') && !stripped.starts_with("+++") {
            added += 1;
        } else if stripped.starts_with('-') && !stripped.starts_with("---") {
            removed += 1;
        }
    }
    (added, removed)
}

fn extract_hunk_function_names(content: &str) -> Vec<String> {
    let mut names = Vec::new();
    for line in content.lines() {
        let stripped = strip_line_numbers(line);
        if stripped.starts_with("@@")
            && let Some(func) = stripped.rfind("@@ ").and_then(|at| {
                let rest = &stripped[at + 3..].trim();
                if rest.is_empty() {
                    None
                } else {
                    Some(rest.to_string())
                }
            })
            && names.len() < 3
            && !names.contains(&func)
        {
            names.push(func);
        }
    }
    names
}

/// Return the target key used to match consecutive tool calls for grouping.
/// For read_file: the path. For bash: the first few words of the command.
/// For search tools: the pattern.
fn tool_target_key(name: &str, summary: &str) -> Option<String> {
    let prefix = format!("{name}: ");
    let body = summary.strip_prefix(&prefix).unwrap_or(summary);

    if matches!(name, "bash" | "http") {
        for token in body.split_whitespace() {
            let t = token.trim_matches('"').trim_matches('\'').trim_matches(',');
            let rest = t
                .strip_prefix("https://")
                .or_else(|| t.strip_prefix("http://"));
            if let Some(rest) = rest {
                let host = rest
                    .split('/')
                    .next()
                    .unwrap_or("?")
                    .trim()
                    .to_ascii_lowercase();
                if !host.is_empty() {
                    return Some(format!("host:{host}"));
                }
            }
        }
    }

    let head = match body.split_once(" (") {
        Some((h, _)) => h,
        None => body,
    };
    let trimmed = head.trim();
    if trimmed.is_empty() {
        None
    } else {
        Some(trimmed.to_string())
    }
}

fn mark_retry_cycles(items: &mut [Line_]) {
    let mut failed_cmd_summary: Option<String> = None;
    let mut files_in_error: Vec<String> = Vec::new();
    let mut i = 0;
    while i < items.len() {
        if let Line_::Tool {
            name,
            ok,
            content,
            summary,
            dim,
            ..
        } = &mut items[i]
        {
            if *name == "bash" && *ok == Some(false) && !content.is_empty() {
                failed_cmd_summary = Some(summary.clone());
                files_in_error = content
                    .lines()
                    .filter_map(|l| {
                        let s = l.trim();
                        if (s.contains("error") || s.contains("→"))
                            && let Some(col) = s.find("src/")
                        {
                            let path: String = s[col..].split(':').next().unwrap_or("").to_string();
                            if !path.is_empty() && !files_in_error.contains(&path) {
                                return Some(path);
                            }
                        }
                        None
                    })
                    .take(10)
                    .collect();
                i += 1;
                continue;
            }
            if let Some(ref failed_cmd) = failed_cmd_summary {
                let is_edit_to_failed_file =
                    matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit")
                        && files_in_error.iter().any(|f| summary.contains(f));
                let is_same_command = *name == "bash"
                    && summary.contains(failed_cmd.split_once(": ").map(|(_, c)| c).unwrap_or(""));
                if is_edit_to_failed_file || is_same_command {
                    *dim = true;
                }
                if is_same_command {
                    failed_cmd_summary = None;
                    files_in_error.clear();
                }
            }
        }
        i += 1;
    }
}

fn grouped_tool_summary(
    name: &str,
    ok: Option<bool>,
    group_count: usize,
    group_lines: usize,
    group_chunks: &[ToolChunk],
) -> String {
    let is_read = matches!(name, "read_file" | "rg" | "fd");
    let is_edit = matches!(name, "write_file" | "edit_file" | "multi_edit");

    let paths: Vec<String> = group_chunks
        .iter()
        .filter_map(|c| extract_path_from_summary(&c.summary))
        .map(|p| {
            if name == "read_file" {
                display_read_file_path(&p)
            } else {
                short_path(&p)
            }
        })
        .collect();
    let unique_paths: Vec<&str> = {
        let mut seen: Vec<&str> = vec![];
        for p in &paths {
            if !seen.contains(&p.as_str()) {
                seen.push(p.as_str());
            }
        }
        seen
    };

    let path_suffix = if unique_paths.len() <= 3 && !unique_paths.is_empty() {
        format!(" on {}", unique_paths.join(", "))
    } else if !unique_paths.is_empty() {
        format!(" on {} +{} more", unique_paths[0], unique_paths.len() - 1)
    } else {
        String::new()
    };

    if name == "bash" && ok == Some(false) {
        format!("× {group_count} {path_suffix}")
    } else if is_edit && ok == Some(true) {
        let (added, removed) = group_chunks.iter().fold((0usize, 0usize), |(a, r), c| {
            let (ca, cr) = count_diff_stats(&c.content);
            (a + ca, r + cr)
        });
        format!("× {group_count}{path_suffix}  (+{added} −{removed})")
    } else if name == "read_file" && ok == Some(true) {
        let target = if unique_paths.len() == 1 {
            unique_paths[0].to_string()
        } else if !unique_paths.is_empty() {
            format!("{} +{} more", unique_paths[0], unique_paths.len() - 1)
        } else {
            "files".to_string()
        };
        let range = read_file_line_span(group_chunks)
            .map(|(start, end)| format!(", lines {start}-{end}"))
            .unwrap_or_default();
        let coverage = read_file_coverage_hint(group_chunks)
            .map(|pct| format!(", ~{pct}% span"))
            .unwrap_or_default();
        format!("{target} ({group_count} reads, {group_lines} lines inspected{range}{coverage})")
    } else if is_read && ok == Some(true) {
        format!("× {group_count}{path_suffix}  ({group_lines} lines inspected)")
    } else if ok == Some(true) {
        format!("× {group_count}{path_suffix}  ({group_lines} lines total)")
    } else {
        format!("× {group_count} {path_suffix}")
    }
}

fn read_file_line_span(group_chunks: &[ToolChunk]) -> Option<(usize, usize)> {
    let mut start: Option<usize> = None;
    let mut end: Option<usize> = None;
    for chunk in group_chunks {
        for raw in chunk.content.lines() {
            let Some(tab) = raw.find('\t') else {
                continue;
            };
            let Ok(line_no) = raw[..tab].parse::<usize>() else {
                continue;
            };
            start = Some(start.map_or(line_no, |current| current.min(line_no)));
            end = Some(end.map_or(line_no, |current| current.max(line_no)));
        }
    }
    start.zip(end)
}

fn read_file_coverage_hint(group_chunks: &[ToolChunk]) -> Option<usize> {
    let (start, end) = read_file_line_span(group_chunks)?;
    if start == 0 || end < start {
        return None;
    }
    let inspected = group_chunks
        .iter()
        .map(|chunk| chunk.content.lines().count())
        .sum::<usize>();
    let span = end.saturating_sub(start).saturating_add(1);
    if span == 0 || inspected < 20 {
        return None;
    }
    Some(
        ((inspected as f64 / span as f64) * 100.0)
            .clamp(1.0, 100.0)
            .round() as usize,
    )
}

fn set_tool_density_ranks(items: &mut [Line_], start_rank: usize) -> usize {
    let mut rank = start_rank;
    for item in items {
        if let Line_::Tool {
            group_count,
            density_rank,
            ..
        } = item
        {
            rank = rank.saturating_add(1);
            *density_rank = rank;
            rank = rank.saturating_add(group_count.saturating_sub(1));
        }
    }
    rank
}

fn merge_consecutive_tools(items: Vec<Line_>) -> Vec<Line_> {
    let mut out: Vec<Line_> = Vec::with_capacity(items.len());
    for item in items {
        let Line_::Tool { .. } = &item else {
            out.push(item);
            continue;
        };
        let last = out.last_mut();
        let Some(Line_::Tool {
            call_tag: _lcall_tag,
            name: ln,
            summary: ls,
            ok: lok,
            content: lcontent,
            group_count,
            group_lines,
            group_chunks,
            duration_secs: l_duration,
            denied: l_denied,
            dim: l_dim,
            density_rank: _l_density_rank,
            expanded: l_expanded,
        }) = last
        else {
            out.push(item);
            continue;
        };
        let Line_::Tool {
            call_tag,
            name,
            summary,
            ok,
            content,
            group_chunks: new_chunks,
            group_lines: new_lines,
            duration_secs: new_duration,
            denied: new_denied,
            dim: new_dim,
            ..
        } = item
        else {
            unreachable!()
        };
        let same_target = ln == &name
            && tool_target_key(ln, ls).as_deref() == tool_target_key(&name, &summary).as_deref();
        let failed_bash_chain =
            name == "bash" && *lok == Some(false) && ok == Some(false) && ln == &name;
        if !(failed_bash_chain || same_target && *lok == ok) {
            out.push(Line_::Tool {
                call_tag,
                name,
                summary,
                ok,
                content,
                group_count: 1,
                group_lines: new_lines,
                group_chunks: new_chunks,
                duration_secs: new_duration,
                denied: new_denied,
                dim: false,
                density_rank: 1,
                expanded: false,
            });
            continue;
        }
        *l_expanded = false;
        *l_duration = l_duration.saturating_add(new_duration);
        *l_denied = *l_denied || new_denied;
        *l_dim = *l_dim || new_dim;
        *group_count += 1;
        *group_lines += new_lines;
        group_chunks.extend(new_chunks);
        *ls = grouped_tool_summary(ln, *lok, *group_count, *group_lines, group_chunks);
        // Keep the first chunk as the preview content; expansion shows all.
        let _ = lcontent;
    }
    out
}

fn strip_line_numbers(line: &str) -> &str {
    // read_file emits "<lineno>\t<content>"; render without the prefix so previews look like code.
    if let Some(tab) = line.find('\t') {
        let head = &line[..tab];
        if !head.is_empty() && head.chars().all(|c| c.is_ascii_digit()) {
            return &line[tab + 1..];
        }
    }
    line
}

fn strip_content_line_numbers(content: &str) -> String {
    let sanitized = sanitize_display_text(content);
    let mut out = String::new();
    for (idx, raw) in sanitized.lines().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(strip_line_numbers(raw));
    }
    if sanitized.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn is_markdown_ordered_list(line: &str) -> bool {
    let digits = line.chars().take_while(|c| c.is_ascii_digit()).count();
    if digits == 0 {
        return false;
    }
    matches!(line[digits..].chars().next(), Some('.') | Some(')'))
        && line[digits + 1..].starts_with(' ')
}

fn looks_like_markdownish_tool_content(_name: &str, content: &str) -> bool {
    let mut markers = 0usize;
    let mut saw_heading = false;
    let mut saw_list = false;
    let mut saw_table_sep = false;
    for raw in content.lines().take(64) {
        let line = strip_line_numbers(raw).trim_start();
        if line.is_empty() {
            continue;
        }
        if line.starts_with("```") || line.starts_with("~~~") {
            return true;
        }
        if is_table_separator_line(line) {
            saw_table_sep = true;
            markers += 1;
            continue;
        }
        let heading_marks = line.chars().take_while(|&c| c == '#').count();
        if heading_marks > 0 && line[heading_marks..].starts_with(' ') {
            saw_heading = true;
            markers += 1;
            continue;
        }
        if line.starts_with("- ")
            || line.starts_with("* ")
            || line.starts_with("+ ")
            || line.starts_with("> ")
            || is_markdown_ordered_list(line)
        {
            saw_list = true;
            markers += 1;
        }
    }

    (saw_heading && saw_list) || markers >= 2 || saw_table_sep
}

fn line_cache_key(item: &Line_) -> u64 {
    let mut hasher = DefaultHasher::new();
    item.hash(&mut hasher);
    hasher.finish()
}

fn collect_diff_preview_lines(content: &str) -> Vec<String> {
    let mut picked = Vec::new();
    for raw in content.lines() {
        let text = strip_line_numbers(raw);
        if text.starts_with("diff --git")
            || text.starts_with("index ")
            || text.starts_with("+++")
            || text.starts_with("---")
        {
            continue;
        }
        if text.starts_with("@@")
            || (text.starts_with('+') && !text.starts_with("+++"))
            || (text.starts_with('-') && !text.starts_with("---"))
        {
            picked.push(text.to_string());
        }
    }
    if picked.is_empty() {
        content
            .lines()
            .map(strip_line_numbers)
            .take(COLLAPSED_PREVIEW_LINES)
            .map(ToString::to_string)
            .collect()
    } else {
        picked
    }
}

fn push_diff_preview(
    lines: &mut Vec<Line<'static>>,
    content: &str,
    max_lines: usize,
    width: u16,
) -> usize {
    let preview_lines = collect_diff_preview_lines(content);
    let take = preview_lines.len().min(max_lines);
    for text in preview_lines.iter().take(take) {
        let style = if text.starts_with('+') && !text.starts_with("+++") {
            Style::default().fg(Color::Green)
        } else if text.starts_with('-') && !text.starts_with("---") {
            Style::default().fg(Color::Red)
        } else if text.starts_with("@@") {
            Style::default().fg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        push_prefixed_wrapped_line(
            lines,
            "│ ",
            Style::default().fg(Color::DarkGray),
            text,
            style,
            width,
        );
    }
    preview_lines.len().saturating_sub(take)
}

fn sanitize_display_text(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    continue;
                }
                out.push('\n');
            }
            '\n' | '\t' => out.push(ch),
            _ if ch.is_control() => {}
            _ => out.push(ch),
        }
    }
    out
}

fn clamp_chars(s: &str, max_cells: usize) -> String {
    clamp_chars_plain(s, max_cells)
}

fn clamp_chars_plain(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }
    let mut cells = 0usize;
    let mut char_end = 0usize;
    for (i, c) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if cells + w > max_cells {
            break;
        }
        cells += w;
        char_end = i + c.len_utf8();
    }
    if char_end >= s.len() {
        return s.to_string();
    }
    if max_cells == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut truncated_cells = 0usize;
    for c in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
        if truncated_cells + w + 1 > max_cells {
            break;
        }
        out.push(c);
        truncated_cells += w;
    }
    out.push('…');
    out
}

fn clamp_chars_with_hint(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }

    let total_cells = text_width(s);
    if total_cells <= max_cells {
        return s.to_string();
    }

    let omitted_chars = s.chars().count();
    if max_cells == 1 {
        return "…".to_string();
    }

    let hint = format!("… +{omitted_chars} chars");
    if text_width(&hint) >= max_cells {
        let mut out = String::new();
        let mut cells = 0usize;
        for ch in hint.chars() {
            let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
            if cells + w > max_cells {
                break;
            }
            out.push(ch);
            cells += w;
        }
        return out;
    }

    let suffix_width = text_width(&hint);
    let prefix_limit = max_cells.saturating_sub(suffix_width);
    let mut out = String::new();
    let mut cells = 0usize;
    let mut kept_chars = 0usize;
    for ch in s.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + w > prefix_limit {
            break;
        }
        out.push(ch);
        cells += w;
        kept_chars += 1;
    }
    let omitted_chars = s.chars().count().saturating_sub(kept_chars);
    out.push_str(&format!("… +{omitted_chars} chars"));
    out
}

fn wrap_input_visual(
    input: &str,
    cursor_byte: usize,
    max_cols: usize,
) -> (Vec<String>, usize, usize) {
    let cols = max_cols.max(1);
    let clamped = cursor_byte.min(input.len());

    let mut lines = vec![String::new()];
    let mut row = 0usize;
    let mut col = 0usize;
    let mut cursor_row = 0usize;
    let mut cursor_col = 0usize;

    for (idx, ch) in input.char_indices() {
        if idx == clamped {
            cursor_row = row;
            cursor_col = col;
        }

        if ch == '\n' {
            lines.push(String::new());
            row += 1;
            col = 0;
            continue;
        }

        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if col + w > cols {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        lines[row].push(ch);
        col += w;
    }

    if clamped == input.len() {
        cursor_row = row;
        cursor_col = col;
    }

    (lines, cursor_row, cursor_col)
}

fn input_panel_height(state: &TuiState, area_height: u16, area_width: u16) -> u16 {
    let available = area_height.saturating_sub(2).max(1);
    let base_min = if state.pending_perm.is_some() || state.agent_busy {
        5
    } else {
        7
    };
    let min_panel = base_min.min(available);
    let ratio_cap = ((area_height as f32) * 0.5).round() as u16;
    let max_panel = ratio_cap.max(min_panel).min(available);

    let cols = area_width.saturating_sub(2).max(1) as usize;
    let (wrapped, _, _) = wrap_input_visual(&state.input, state.cursor, cols);
    let text_rows = wrapped.len().max(1) as u16;
    let drawer_rows = work_map_drawer_height(state, area_width) as u16;
    let desired = text_rows.saturating_add(3).saturating_add(drawer_rows);
    desired.clamp(min_panel, max_panel)
}

fn work_map_drawer_body_rows(state: &TuiState) -> usize {
    state
        .work_map
        .as_ref()
        .map(|drawer| {
            drawer
                .waypoint_ids
                .len()
                .clamp(1, WORK_MAP_DRAWER_MAX_BODY_ROWS)
        })
        .unwrap_or(0)
}

fn work_map_drawer_height(state: &TuiState, area_width: u16) -> usize {
    if !state.work_map_is_active() || area_width == 0 {
        return 0;
    }
    let body_rows = work_map_drawer_body_rows(state);
    (body_rows + 2).min(WORK_MAP_DRAWER_MAX_ROWS)
}

fn count_words(s: &str) -> usize {
    s.split_whitespace().count()
}

fn split_display_paragraphs(s: &str) -> Vec<&str> {
    let mut result = Vec::new();
    let mut start = 0;
    let mut in_blank = false;
    let mut blank_start = 0;
    for (i, ch) in s.char_indices() {
        if ch == '\n' {
            let rest = &s[i + 1..];
            if (rest.starts_with('\n') || rest.is_empty()) && !in_blank && start < i {
                result.push(&s[start..i]);
                in_blank = true;
                blank_start = i;
            }
        } else if in_blank {
            result.push(s[blank_start..i].trim_end_matches('\n'));
            start = i;
            in_blank = false;
        }
    }
    if start < s.len() {
        result.push(&s[start..]);
    }
    if result.is_empty() {
        result.push(s);
    }
    result
}

fn abstract_input_for_display(input: &str) -> Option<String> {
    const PASTE_WORD_THRESHOLD: usize = 50;

    let paragraphs = split_display_paragraphs(input);
    let mut out = String::with_capacity(input.len().min(512));
    let mut changed = false;
    let mut paste_idx = 0usize;
    let mut first = true;
    for para in paragraphs {
        if !first {
            out.push_str("\n\n");
        }
        first = false;
        let words = count_words(para);
        if words > PASTE_WORD_THRESHOLD {
            paste_idx += 1;
            changed = true;
            out.push_str(&format!(
                "[paste #{paste_idx} +{words} words hidden — full content preserved for Enter]"
            ));
        } else {
            out.push_str(para);
        }
    }
    changed.then_some(out)
}

#[derive(Clone, Copy, Debug, Default)]
struct DextMarkdownStyleSheet;

impl MarkdownStyleSheet for DextMarkdownStyleSheet {
    fn heading(&self, level: u8) -> MdStyle {
        match level {
            1 => MdStyle::default()
                .fg(MdColor::LightCyan)
                .add_modifier(MdModifier::BOLD),
            2 => MdStyle::default()
                .fg(MdColor::Cyan)
                .add_modifier(MdModifier::BOLD),
            3 => MdStyle::default()
                .fg(MdColor::LightCyan)
                .add_modifier(MdModifier::BOLD),
            _ => MdStyle::default()
                .fg(MdColor::Gray)
                .add_modifier(MdModifier::BOLD),
        }
    }

    fn code(&self) -> MdStyle {
        MdStyle::default()
    }

    fn link(&self) -> MdStyle {
        MdStyle::default()
            .fg(MdColor::LightBlue)
            .add_modifier(MdModifier::UNDERLINED)
    }

    fn blockquote(&self) -> MdStyle {
        MdStyle::default().fg(MdColor::Green)
    }

    fn heading_meta(&self) -> MdStyle {
        MdStyle::default().fg(MdColor::DarkGray)
    }

    fn metadata_block(&self) -> MdStyle {
        MdStyle::default().fg(MdColor::Gray)
    }
}

fn md_color_to_color(color: MdColor) -> Color {
    match color {
        MdColor::Reset => Color::Reset,
        MdColor::Black => Color::Black,
        MdColor::Red => Color::Red,
        MdColor::Green => Color::Green,
        MdColor::Yellow => Color::Yellow,
        MdColor::Blue => Color::Blue,
        MdColor::Magenta => Color::Magenta,
        MdColor::Cyan => Color::Cyan,
        MdColor::Gray => Color::Gray,
        MdColor::DarkGray => Color::DarkGray,
        MdColor::LightRed => Color::LightRed,
        MdColor::LightGreen => Color::LightGreen,
        MdColor::LightYellow => Color::LightYellow,
        MdColor::LightBlue => Color::LightBlue,
        MdColor::LightMagenta => Color::LightMagenta,
        MdColor::LightCyan => Color::LightCyan,
        MdColor::White => Color::Reset,
        MdColor::Rgb(r, g, b) => Color::Rgb(r, g, b),
        MdColor::Indexed(i) => Color::Indexed(i),
    }
}

fn md_modifier_to_modifier(modifier: MdModifier) -> Modifier {
    let mut out = Modifier::empty();
    if modifier.contains(MdModifier::BOLD) {
        out |= Modifier::BOLD;
    }
    if modifier.contains(MdModifier::DIM) {
        out |= Modifier::DIM;
    }
    if modifier.contains(MdModifier::ITALIC) {
        out |= Modifier::ITALIC;
    }
    if modifier.contains(MdModifier::UNDERLINED) {
        out |= Modifier::UNDERLINED;
    }
    if modifier.contains(MdModifier::SLOW_BLINK) {
        out |= Modifier::SLOW_BLINK;
    }
    if modifier.contains(MdModifier::RAPID_BLINK) {
        out |= Modifier::RAPID_BLINK;
    }
    if modifier.contains(MdModifier::REVERSED) {
        out |= Modifier::REVERSED;
    }
    if modifier.contains(MdModifier::HIDDEN) {
        out |= Modifier::HIDDEN;
    }
    if modifier.contains(MdModifier::CROSSED_OUT) {
        out |= Modifier::CROSSED_OUT;
    }
    out
}

fn md_style_to_style(style: MdStyle) -> Style {
    Style {
        fg: style.fg.map(md_color_to_color),
        bg: style.bg.map(md_color_to_color),
        add_modifier: md_modifier_to_modifier(style.add_modifier),
        sub_modifier: md_modifier_to_modifier(style.sub_modifier),
    }
}

fn md_alignment_to_alignment(alignment: MdAlignment) -> Alignment {
    match alignment {
        MdAlignment::Left => Alignment::Left,
        MdAlignment::Center => Alignment::Center,
        MdAlignment::Right => Alignment::Right,
    }
}

fn text_to_static(text: MdText<'_>) -> Text<'static> {
    Text {
        alignment: text.alignment.map(md_alignment_to_alignment),
        style: md_style_to_style(text.style),
        lines: text
            .lines
            .into_iter()
            .map(|line| Line {
                style: md_style_to_style(line.style),
                alignment: line.alignment.map(md_alignment_to_alignment),
                spans: line
                    .spans
                    .into_iter()
                    .map(|span| {
                        Span::styled(span.content.into_owned(), md_style_to_style(span.style))
                    })
                    .collect(),
            })
            .collect(),
    }
}

fn rendered_line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn is_plain_text_code_fence_opener(line: &Line<'_>) -> bool {
    let text = rendered_line_text(line);
    let trimmed = text.trim();
    let Some(info) = trimmed.strip_prefix("```") else {
        return false;
    };
    let lang = info.split_whitespace().next().unwrap_or("");
    matches!(
        lang.to_ascii_lowercase().as_str(),
        "text" | "txt" | "plain" | "plaintext"
    )
}

fn is_code_fence_closer(line: &Line<'_>) -> bool {
    rendered_line_text(line).trim() == "```"
}

fn hide_plain_text_code_fence_lines(mut text: Text<'static>) -> Text<'static> {
    let mut lines = Vec::with_capacity(text.lines.len());
    let mut in_plain_text_fence = false;
    for line in text.lines {
        if !in_plain_text_fence && is_plain_text_code_fence_opener(&line) {
            in_plain_text_fence = true;
            continue;
        }
        if in_plain_text_fence && is_code_fence_closer(&line) {
            in_plain_text_fence = false;
            continue;
        }
        lines.push(line);
    }
    text.lines = lines;
    text
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum TableColumnAlignment {
    Left,
    Center,
    Right,
}

struct ParsedTable {
    rows: Vec<Vec<String>>,
    header_rows: usize,
    alignments: Vec<TableColumnAlignment>,
}

fn parse_md_separator_alignments(line: &str) -> Option<Vec<TableColumnAlignment>> {
    let mut trimmed = line.trim();
    if trimmed.is_empty() || !trimmed.contains('|') {
        return None;
    }
    if let Some(stripped) = trimmed.strip_prefix('|') {
        trimmed = stripped;
    }
    if let Some(stripped) = trimmed.strip_suffix('|') {
        trimmed = stripped;
    }

    let mut alignments = Vec::new();
    for raw in trimmed.split('|') {
        let part = raw.trim();
        if part.is_empty() {
            return None;
        }
        let left = part.starts_with(':');
        let right = part.ends_with(':');
        let core = part.trim_matches(':').trim();
        if core.chars().count() < 3 || !core.chars().all(|c| c == '-') {
            return None;
        }
        let alignment = match (left, right) {
            (true, true) => TableColumnAlignment::Center,
            (false, true) => TableColumnAlignment::Right,
            _ => TableColumnAlignment::Left,
        };
        alignments.push(alignment);
    }

    if alignments.is_empty() {
        None
    } else {
        Some(alignments)
    }
}

fn is_md_separator_row(line: &str) -> bool {
    parse_md_separator_alignments(line).is_some()
}

fn is_ascii_border_row(line: &str) -> bool {
    let trimmed = line.trim();
    if !trimmed.starts_with('+') || !trimmed.ends_with('+') {
        return false;
    }
    if !trimmed.chars().all(|c| matches!(c, '+' | '-' | '=')) {
        return false;
    }
    trimmed.chars().any(|c| c == '-' || c == '=')
}

fn is_ascii_data_row(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.starts_with('|') && trimmed.ends_with('|') && trimmed.matches('|').count() >= 2
}

fn split_markdown_cells(line: &str) -> Vec<String> {
    let mut trimmed = line.trim();
    if let Some(stripped) = trimmed.strip_prefix('|') {
        trimmed = stripped;
    }
    if let Some(stripped) = trimmed.strip_suffix('|') {
        trimmed = stripped;
    }

    let mut cells = Vec::new();
    let mut current = String::new();
    let mut chars = trimmed.chars().peekable();
    while let Some(ch) = chars.next() {
        if ch == '\\' {
            if matches!(chars.peek(), Some('|')) {
                current.push('|');
                chars.next();
            } else {
                current.push(ch);
            }
            continue;
        }
        if ch == '|' {
            cells.push(current.trim().to_string());
            current.clear();
            continue;
        }
        current.push(ch);
    }
    cells.push(current.trim().to_string());
    cells
}

fn split_ascii_cells(line: &str) -> Vec<String> {
    let mut trimmed = line.trim();
    if let Some(stripped) = trimmed.strip_prefix('|') {
        trimmed = stripped;
    }
    if let Some(stripped) = trimmed.strip_suffix('|') {
        trimmed = stripped;
    }
    trimmed
        .split('|')
        .map(|cell| cell.trim().to_string())
        .collect()
}

fn normalize_row(mut cells: Vec<String>, col_count: usize) -> Vec<String> {
    cells.truncate(col_count);
    while cells.len() < col_count {
        cells.push(String::new());
    }
    cells
}

fn parse_markdown_table_block(lines: &[&str], start: usize) -> Option<(ParsedTable, usize)> {
    let header_line = *lines.get(start)?;
    let separator_line = *lines.get(start + 1)?;
    if !header_line.contains('|') {
        return None;
    }

    let alignments = parse_md_separator_alignments(separator_line)?;
    let col_count = alignments.len();
    if col_count == 0 {
        return None;
    }

    let mut rows = Vec::new();
    let header = normalize_row(split_markdown_cells(header_line), col_count);
    if header.iter().all(|cell| cell.is_empty()) {
        return None;
    }
    rows.push(header);

    let mut consumed = 2;
    while let Some(line) = lines.get(start + consumed).copied() {
        if line.trim().is_empty() || !line.contains('|') {
            break;
        }
        if parse_md_separator_alignments(line).is_some() {
            break;
        }
        rows.push(normalize_row(split_markdown_cells(line), col_count));
        consumed += 1;
    }

    Some((
        ParsedTable {
            rows,
            header_rows: 1,
            alignments,
        },
        consumed,
    ))
}

fn parse_ascii_table_block(lines: &[&str], start: usize) -> Option<(ParsedTable, usize)> {
    if !is_ascii_border_row(lines.get(start).copied()?) {
        return None;
    }

    let mut rows = Vec::new();
    let mut consumed = 1;
    let mut saw_border_after_rows = false;
    let mut saw_header_separator = false;

    while let Some(line) = lines.get(start + consumed).copied() {
        if line.trim().is_empty() {
            break;
        }
        if is_ascii_border_row(line) {
            if rows.is_empty() {
                return None;
            }
            saw_border_after_rows = true;
            if rows.len() == 1 {
                let next_is_data = lines
                    .get(start + consumed + 1)
                    .copied()
                    .is_some_and(is_ascii_data_row);
                if line.contains('=') || next_is_data {
                    saw_header_separator = true;
                }
            }
            consumed += 1;
            continue;
        }
        if !is_ascii_data_row(line) {
            break;
        }
        rows.push(split_ascii_cells(line));
        consumed += 1;
    }

    if rows.is_empty() || !saw_border_after_rows {
        return None;
    }

    let col_count = rows.iter().map(|row| row.len()).max().unwrap_or(0);
    if col_count == 0 {
        return None;
    }

    let rows = rows
        .into_iter()
        .map(|row| normalize_row(row, col_count))
        .collect();

    Some((
        ParsedTable {
            rows,
            header_rows: usize::from(saw_header_separator),
            alignments: vec![TableColumnAlignment::Left; col_count],
        },
        consumed,
    ))
}

#[cfg(test)]
fn parse_table_lines(lines: &[&str]) -> Option<ParsedTable> {
    parse_markdown_table_block(lines, 0)
        .filter(|(_, consumed)| *consumed == lines.len())
        .map(|(table, _)| table)
        .or_else(|| {
            parse_ascii_table_block(lines, 0)
                .filter(|(_, consumed)| *consumed == lines.len())
                .map(|(table, _)| table)
        })
}

fn table_area_width(widths: &[usize], spacing: usize) -> usize {
    if widths.is_empty() {
        return 0;
    }
    widths.iter().sum::<usize>() + widths.len().saturating_sub(1) * spacing + 2
}

fn shrink_table_widths(widths: &mut [usize], spacing: usize, max_total: usize) {
    if widths.is_empty() {
        return;
    }

    let mut total = table_area_width(widths, spacing);
    while total > max_total {
        let mut changed = false;
        for width in widths.iter_mut() {
            if total <= max_total {
                break;
            }
            if *width > 1 {
                *width -= 1;
                total = total.saturating_sub(1);
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

fn table_column_count(table: &ParsedTable) -> usize {
    table
        .rows
        .iter()
        .map(|row| row.len())
        .max()
        .unwrap_or(0)
        .max(table.alignments.len())
}

fn table_spacing(table: &ParsedTable, max_total: usize) -> usize {
    let col_count = table_column_count(table);
    if col_count <= 1 {
        return 0;
    }
    let min_with_spacing = col_count + (col_count - 1) + 2;
    if max_total < min_with_spacing { 0 } else { 1 }
}

fn table_header_style(base_style: Style) -> Style {
    base_style.patch(Style::default().add_modifier(Modifier::BOLD))
}

fn table_column_widths(table: &ParsedTable, spacing: usize, max_total: usize) -> Vec<usize> {
    let col_count = table_column_count(table);
    if col_count == 0 {
        return Vec::new();
    }

    let mut widths = vec![1usize; col_count];
    for row in &table.rows {
        for (ci, cell) in row.iter().enumerate() {
            widths[ci] = widths[ci].max(text_width(cell));
        }
    }

    shrink_table_widths(&mut widths, spacing, max_total);
    widths
}

fn table_alignment(alignment: TableColumnAlignment) -> Alignment {
    match alignment {
        TableColumnAlignment::Left => Alignment::Left,
        TableColumnAlignment::Center => Alignment::Center,
        TableColumnAlignment::Right => Alignment::Right,
    }
}

fn buffer_to_lines(buffer: &ratatui::buffer::Buffer, area: Rect) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for y in 0..area.height {
        let mut spans = Vec::new();
        let mut text_line = String::new();
        let mut current_style: Option<Style> = None;
        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            let symbol = cell.symbol();
            if symbol.is_empty() {
                continue;
            }
            let style = cell.style();
            if current_style == Some(style) {
                text_line.push_str(symbol);
            } else {
                if !text_line.is_empty() {
                    spans.push(Span::styled(
                        std::mem::take(&mut text_line),
                        current_style.unwrap_or_default(),
                    ));
                }
                text_line.push_str(symbol);
                current_style = Some(style);
            }
        }
        if !text_line.is_empty() {
            spans.push(Span::styled(text_line, current_style.unwrap_or_default()));
        }
        if spans.is_empty() {
            out.push(Line::from(""));
        } else {
            out.push(Line::from(spans));
        }
    }
    out
}

fn table_visual_height(table: &ParsedTable, max_total_width: usize) -> u16 {
    let spacing = table_spacing(table, max_total_width);
    let widths = table_column_widths(table, spacing, max_total_width);
    if widths.is_empty() {
        return 0;
    }
    let header_rows = table.header_rows.min(table.rows.len());
    let body_rows = table.rows.len().saturating_sub(header_rows);
    header_rows
        .saturating_add(body_rows)
        .saturating_add(2)
        .min(u16::MAX as usize) as u16
}

fn render_table_lines(
    table: &ParsedTable,
    base_style: Style,
    max_total_width: usize,
) -> Vec<Line<'static>> {
    let spacing = table_spacing(table, max_total_width);
    let widths = table_column_widths(table, spacing, max_total_width);
    if widths.is_empty() {
        return Vec::new();
    }

    let col_count = widths.len();
    let header_rows = table.header_rows.min(table.rows.len());
    let make_cells = |row: &[String], is_header: bool| {
        (0..col_count)
            .map(|ci| {
                let raw = row.get(ci).map(String::as_str).unwrap_or("");
                let truncated = truncate_cell(raw, widths[ci]);
                let align = table
                    .alignments
                    .get(ci)
                    .copied()
                    .unwrap_or(TableColumnAlignment::Left);
                let text = Text::from(truncated).alignment(table_alignment(align));
                let style = if is_header {
                    table_header_style(base_style)
                } else {
                    base_style
                };
                Cell::from(text).style(style)
            })
            .collect::<Vec<Cell<'static>>>()
    };

    let rows = table
        .rows
        .iter()
        .enumerate()
        .skip(header_rows)
        .map(|(_, row)| Row::new(make_cells(row, false)).style(base_style))
        .collect::<Vec<Row<'static>>>();

    let width_constraints = widths
        .iter()
        .map(|w| Constraint::Length((*w).min(u16::MAX as usize) as u16))
        .collect::<Vec<Constraint>>();

    let mut widget = Table::new(rows, width_constraints)
        .column_spacing(spacing as u16)
        .style(base_style)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::DarkGray)),
        );

    if header_rows > 0 {
        let header =
            Row::new(make_cells(&table.rows[0], true)).style(table_header_style(base_style));
        widget = widget.header(header);
    }

    let area_width = table_area_width(&widths, spacing)
        .max(3)
        .min(u16::MAX as usize) as u16;
    let area_height = table_visual_height(table, max_total_width).max(3);
    let area = Rect::new(0, 0, area_width, area_height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    Widget::render(widget, area, &mut buffer);
    buffer_to_lines(&buffer, area)
}

fn truncate_cell(cell: &str, max_width: usize) -> String {
    if max_width == 0 {
        return String::new();
    }

    if unicode_width::UnicodeWidthStr::width(cell) <= max_width {
        return cell.to_string();
    }
    if max_width == 1 {
        return "…".to_string();
    }

    let mut out = String::new();
    let mut cells = 0usize;
    for ch in cell.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + w + 1 > max_width {
            break;
        }
        out.push(ch);
        cells += w;
    }
    out.push('…');
    out
}

fn text_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(s)
}

fn transcript_render_width(width: u16) -> u16 {
    width.saturating_sub(TRANSCRIPT_WRAP_GUARD_COLS).max(1)
}

fn is_table_separator_line(line: &str) -> bool {
    is_md_separator_row(line) || is_ascii_border_row(line)
}

fn has_table_marker(text: &str) -> bool {
    text.lines()
        .take(256)
        .any(|line| line.contains('|') || is_ascii_border_row(line))
}

fn fence_delimiter(line: &str) -> Option<char> {
    let trimmed = line.trim_start();
    if trimmed.starts_with("```") {
        Some('`')
    } else if trimmed.starts_with("~~~") {
        Some('~')
    } else {
        None
    }
}

fn markdown_text(body: &str, base_style: Style, max_total_width: u16) -> Text<'static> {
    let sanitized = sanitize_display_text(body);
    let options = MarkdownOptions::new(DextMarkdownStyleSheet);
    if !has_table_marker(&sanitized) {
        return hide_plain_text_code_fence_lines(
            text_to_static(from_str_with_options(&sanitized, &options)).style(base_style),
        );
    }

    let raw_lines: Vec<&str> = sanitized.lines().collect();
    let mut blocks: Vec<EitherBlock> = Vec::new();
    let mut markdown_start = 0usize;
    let mut i = 0usize;
    let mut open_fence: Option<char> = None;

    while i < raw_lines.len() {
        if let Some(delim) = fence_delimiter(raw_lines[i]) {
            match open_fence {
                Some(open) if open == delim => open_fence = None,
                None => open_fence = Some(delim),
                _ => {}
            }
            i += 1;
            continue;
        }

        if open_fence.is_some() {
            i += 1;
            continue;
        }

        let parsed = parse_markdown_table_block(&raw_lines, i)
            .or_else(|| parse_ascii_table_block(&raw_lines, i));

        if let Some((table, consumed)) = parsed {
            if markdown_start < i {
                blocks.push(EitherBlock::Markdown(&raw_lines[markdown_start..i]));
            }
            blocks.push(EitherBlock::Table(table));
            i += consumed;
            markdown_start = i;
            continue;
        }

        i += 1;
    }

    if markdown_start < raw_lines.len() {
        blocks.push(EitherBlock::Markdown(&raw_lines[markdown_start..]));
    }

    let mut result_lines: Vec<Line<'static>> = Vec::new();
    for block in blocks {
        match block {
            EitherBlock::Markdown(lines) => {
                let joined = lines.join("\n");
                let rendered = text_to_static(from_str_with_options(&joined, &options));
                result_lines.extend(rendered.style(base_style).lines);
            }
            EitherBlock::Table(table) => {
                result_lines.extend(render_table_lines(
                    &table,
                    base_style,
                    max_total_width as usize,
                ));
            }
        }
    }

    hide_plain_text_code_fence_lines(Text::from(result_lines))
}

enum EitherBlock<'a> {
    Markdown(&'a [&'a str]),
    Table(ParsedTable),
}

fn push_prefixed_text(
    lines: &mut Vec<Line<'static>>,
    text: Text<'static>,
    prefix: &str,
    prefix_style: Style,
    target_width: u16,
) {
    let prefix_w = unicode_width::UnicodeWidthStr::width(prefix);
    let available_content_width = (target_width as usize).saturating_sub(prefix_w);
    if text.lines.is_empty() {
        lines.push(Line::from(vec![Span::styled(
            prefix.to_string(),
            prefix_style,
        )]));
        return;
    }
    for line in text.lines {
        let mut spans = Vec::with_capacity(line.spans.len().saturating_add(1));
        spans.push(Span::styled(prefix.to_string(), prefix_style));
        let mut remaining = available_content_width;
        for span in line.spans {
            if remaining == 0 {
                break;
            }
            let content = span.content.into_owned();
            let width = unicode_width::UnicodeWidthStr::width(content.as_str());
            if width <= remaining {
                remaining = remaining.saturating_sub(width);
                spans.push(Span::styled(content, span.style));
            } else {
                let clipped = clamp_chars_with_hint(&content, remaining);
                if !clipped.is_empty() {
                    spans.push(Span::styled(clipped, span.style));
                }
                break;
            }
        }
        lines.push(Line {
            style: line.style,
            alignment: line.alignment,
            spans,
        });
    }
}

fn push_prefixed_wrapped_line(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    prefix_style: Style,
    body: &str,
    body_style: Style,
    target_width: u16,
) {
    let content_width = (target_width as usize)
        .saturating_sub(unicode_width::UnicodeWidthStr::width(prefix))
        .max(1);
    let wrapped = wrap_plain_visual(body, content_width)
        .into_iter()
        .map(|row| Line::from(Span::styled(row, body_style)))
        .collect::<Vec<_>>();
    push_prefixed_text(
        lines,
        Text::from(wrapped),
        prefix,
        prefix_style,
        target_width,
    );
}

fn push_prefixed_wrapped_spans(
    lines: &mut Vec<Line<'static>>,
    prefix: &str,
    prefix_style: Style,
    body_spans: Vec<Span<'static>>,
    target_width: u16,
) {
    let prefix_span = Span::styled(prefix.to_string(), prefix_style);
    push_wrapped_spans_with_prefix(
        lines,
        vec![prefix_span.clone()],
        vec![prefix_span],
        body_spans,
        target_width,
    );
}

fn split_display_cells(s: &str, max_cells: usize) -> (String, String) {
    if max_cells == 0 || s.is_empty() {
        return (String::new(), s.to_string());
    }

    let mut cells = 0usize;
    let mut end = 0usize;
    for (idx, ch) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if cells + w > max_cells {
            if end == 0 {
                let next = idx + ch.len_utf8();
                return ("…".to_string(), s[next..].to_string());
            }
            break;
        }
        cells += w;
        end = idx + ch.len_utf8();
    }

    if end >= s.len() {
        (s.to_string(), String::new())
    } else {
        (s[..end].to_string(), s[end..].to_string())
    }
}

fn spans_display_width(spans: &[Span<'static>]) -> usize {
    spans
        .iter()
        .map(|span| text_width(span.content.as_ref()))
        .sum()
}

fn clip_spans_to_width(spans: Vec<Span<'static>>, max_width: usize) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut remaining = max_width;
    for span in spans {
        if remaining == 0 {
            break;
        }
        let style = span.style;
        let content = span.content.into_owned();
        let width = text_width(&content);
        if width <= remaining {
            remaining = remaining.saturating_sub(width);
            out.push(Span::styled(content, style));
        } else {
            let (chunk, _) = split_display_cells(&content, remaining);
            if !chunk.is_empty() {
                out.push(Span::styled(chunk, style));
            }
            break;
        }
    }
    out
}

fn push_wrapped_spans_with_prefix(
    lines: &mut Vec<Line<'static>>,
    first_prefix: Vec<Span<'static>>,
    continuation_prefix: Vec<Span<'static>>,
    body_spans: Vec<Span<'static>>,
    width: u16,
) {
    let max_width = width.max(1) as usize;
    let continuation_prefix = if spans_display_width(&continuation_prefix) < max_width {
        continuation_prefix
    } else {
        Vec::new()
    };
    let mut pending = body_spans
        .into_iter()
        .filter(|span| !span.content.is_empty())
        .collect::<VecDeque<_>>();

    if pending.is_empty() {
        lines.push(Line::from(clip_spans_to_width(first_prefix, max_width)));
        return;
    }

    let mut first = true;
    while first || !pending.is_empty() {
        let prefix = if first {
            first_prefix.clone()
        } else {
            continuation_prefix.clone()
        };
        let mut line_spans = if spans_display_width(&prefix) > max_width {
            clip_spans_to_width(prefix, max_width)
        } else {
            prefix
        };
        let mut available = max_width.saturating_sub(spans_display_width(&line_spans));

        while available > 0 {
            let Some(span) = pending.pop_front() else {
                break;
            };
            let style = span.style;
            let content = span.content.into_owned();
            let width = text_width(&content);
            if width <= available {
                available = available.saturating_sub(width);
                line_spans.push(Span::styled(content, style));
            } else {
                let (chunk, rest) = split_display_cells(&content, available);
                if !chunk.is_empty() {
                    line_spans.push(Span::styled(chunk, style));
                }
                if !rest.is_empty() {
                    pending.push_front(Span::styled(rest, style));
                }
                break;
            }
        }

        lines.push(Line::from(line_spans));
        first = false;
    }
}

fn push_swim_markdown_card(
    lines: &mut Vec<Line<'static>>,
    lane: &str,
    lane_color: Color,
    body: &str,
    body_style: Style,
    width: u16,
) {
    lines.push(Line::from(vec![
        Span::styled("┌─ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            lane.to_string(),
            Style::default().fg(lane_color).add_modifier(Modifier::BOLD),
        ),
    ]));
    let content_width = width.saturating_sub(2).max(1);
    let wrapped = collect_wrapped_lines(
        &markdown_text(body, body_style, content_width),
        content_width,
    );
    push_prefixed_text(
        lines,
        Text::from(wrapped),
        "│ ",
        Style::default().fg(Color::DarkGray),
        width,
    );
    lines.push(Line::from(Span::styled(
        "└",
        Style::default().fg(Color::DarkGray),
    )));
}

fn push_density_separator(
    lines: &mut Vec<Line<'static>>,
    density_rank: usize,
    group_count: usize,
    width: u16,
) {
    if density_rank == 0 {
        return;
    }
    let last_rank = density_rank.saturating_add(group_count.saturating_sub(1));
    let Some(boundary_rank) =
        (density_rank..=last_rank).find(|rank| rank % TOOL_DENSITY_SEPARATOR_EVERY == 0)
    else {
        return;
    };
    let label = format!(" tool call {boundary_rank} ");
    let label_width = text_width(&label);
    let rule_width = (width as usize).saturating_sub(label_width).max(4);
    let left = rule_width / 2;
    let right = rule_width.saturating_sub(left);
    lines.push(Line::from(vec![
        Span::styled("─".repeat(left), Style::default().fg(Color::Indexed(236))),
        Span::styled(label, Style::default().fg(Color::Indexed(242))),
        Span::styled("─".repeat(right), Style::default().fg(Color::Indexed(236))),
    ]));
}

fn push_thinking_body_lines(
    lines: &mut Vec<Line<'static>>,
    body: &str,
    border_style: Style,
    thinking_style: Style,
    width: u16,
) {
    let max_width = width.max(1) as usize;
    let prefix = "│ ";
    let prefix_width = text_width(prefix);
    let body_width = max_width.saturating_sub(prefix_width).max(1);
    for raw in sanitize_display_text(body).lines().take(20) {
        for row in wrap_plain_words_visual(raw, body_width) {
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), border_style),
                Span::styled(row, thinking_style),
            ]));
        }
    }
}

fn line_to_text(item: &Line_, width: u16) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match item {
        Line_::Banner(s) => {
            let rule = "─".repeat(62);
            lines.push(Line::from(Span::styled(
                rule.clone(),
                Style::default().fg(Color::DarkGray),
            )));
            for raw in s.split('\n') {
                if raw.starts_with("🐺") {
                    lines.push(Line::from(Span::styled(
                        raw.to_string(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )));
                } else if let Some((label, value)) = raw.split_once("  ") {
                    lines.push(Line::from(vec![
                        Span::styled(format!("{label:<9}"), Style::default().fg(Color::Green)),
                        Span::styled(value.trim_start().to_string(), Style::default()),
                    ]));
                } else {
                    lines.push(Line::from(Span::styled(
                        raw.to_string(),
                        Style::default().fg(Color::DarkGray),
                    )));
                }
            }
            lines.push(Line::from(Span::styled(
                rule,
                Style::default().fg(Color::DarkGray),
            )));
        }
        Line_::TurnSep => {
            let sep: String = "─".repeat(60);
            lines.push(Line::from(Span::styled(
                sep,
                Style::default().fg(Color::Indexed(236)),
            )));
        }
        Line_::User(s) => {
            push_swim_markdown_card(
                &mut lines,
                "you",
                Color::Magenta,
                s,
                Style::default(),
                width,
            );
        }
        Line_::Assistant { text, dim_prefix } => {
            push_swim_markdown_card(
                &mut lines,
                "dext",
                if *dim_prefix {
                    Color::DarkGray
                } else {
                    Color::Blue
                },
                text,
                Style::default(),
                width,
            );
        }
        Line_::Tool {
            call_tag,
            name,
            summary,
            ok,
            content,
            group_count,
            group_chunks,
            group_lines,
            duration_secs,
            denied,
            dim,
            density_rank,
            expanded,
        } => {
            push_density_separator(&mut lines, *density_rank, *group_count, width);
            let grouped = *group_count > 1;
            let repeated_read_dim = matches!(name.as_str(), "read_file") && grouped;
            let dim_style = if *dim || repeated_read_dim {
                Style::default().add_modifier(Modifier::DIM)
            } else {
                Style::default()
            };
            let (marker_text, marker_color) = if *denied {
                ("⊘ ", Color::DarkGray)
            } else {
                match ok {
                    None => ("◦ ", Color::DarkGray),
                    Some(true) => ("✓ ", Color::Green),
                    Some(false) => ("✗ ", Color::Red),
                }
            };
            let marker = Span::styled(
                marker_text.to_string(),
                Style::default().fg(marker_color).patch(dim_style),
            );
            let name_style = match ok {
                Some(false) => Style::default().fg(Color::Red),
                _ => Style::default().fg(Color::Cyan),
            }
            .patch(dim_style);

            let header_spans = vec![
                marker,
                Span::styled(
                    format!("{} ", call_tag),
                    Style::default().fg(Color::DarkGray).patch(dim_style),
                ),
                Span::styled(name.clone(), name_style),
            ];

            let summary_body = tool_summary_body(name, summary);
            let mut body_spans = Vec::new();
            if !summary_body.is_empty() {
                body_spans.push(Span::raw(": "));
                body_spans.push(Span::styled(
                    summary_body.to_string(),
                    Style::default().fg(Color::DarkGray).patch(dim_style),
                ));
            }

            if matches!(ok, Some(true)) && !content.is_empty() && !grouped {
                let content_lines = content.lines().count();
                let result_tag = match name.as_str() {
                    "read_file" | "rg" | "fd" => format!(" ({} lines)", content_lines),
                    "write_file" | "edit_file" | "multi_edit" => {
                        let (added, removed) = count_diff_stats(content);
                        let hunks = content.lines().filter(|l| l.starts_with("@@")).count();
                        let mut tag = String::new();
                        if added > 0 || removed > 0 {
                            tag = format!(" +{} −{}", added, removed);
                        }
                        if hunks > 0 {
                            tag = format!(
                                "{} ({} hunk{})",
                                tag,
                                hunks,
                                if hunks > 1 { "s" } else { "" }
                            );
                        }
                        tag
                    }
                    _ => String::new(),
                };
                if !result_tag.is_empty() {
                    body_spans.push(Span::styled(
                        result_tag,
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }

            if *denied {
                body_spans.push(Span::styled(
                    " denied".to_string(),
                    Style::default().fg(Color::DarkGray),
                ));
            } else if *duration_secs > 0 {
                let dur_str = format_duration(*duration_secs);
                let is_timeout = matches!(name.as_str(), "bash")
                    && matches!(ok, Some(false))
                    && content.contains("timed out");
                if is_timeout {
                    body_spans.push(Span::styled(
                        format!(" ⏱ timed out at {dur_str}"),
                        Style::default().fg(Color::Yellow),
                    ));
                } else {
                    body_spans.push(Span::styled(
                        format!("  ·  {dur_str}"),
                        Style::default().fg(Color::DarkGray),
                    ));
                }
            }

            push_wrapped_spans_with_prefix(
                &mut lines,
                header_spans,
                vec![Span::styled(
                    "  ".to_string(),
                    Style::default().patch(dim_style),
                )],
                body_spans,
                width,
            );

            let is_mutating_diff =
                matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit");
            if is_mutating_diff
                && !grouped
                && let Some(path) = extract_path_from_summary(summary)
            {
                lines.push(Line::from(vec![
                    Span::styled("  ↳ ", Style::default().fg(Color::DarkGray)),
                    Span::styled(short_path(&path), Style::default().fg(Color::DarkGray)),
                ]));
            }
            if is_mutating_diff && matches!(ok, Some(true)) && !content.is_empty() && !grouped {
                let funcs = extract_hunk_function_names(content);
                for func in funcs.iter().take(2) {
                    lines.push(Line::from(vec![
                        Span::styled("  └ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            func.clone(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
            }

            if grouped {
                let repeated_note = if repeated_read_dim {
                    " · repeated read dimmed"
                } else {
                    ""
                };
                lines.push(Line::from(vec![Span::styled(
                    format!(
                        "  ↳ {group_count} calls · {group_lines} lines{repeated_note} · Ctrl+O"
                    ),
                    Style::default().fg(Color::DarkGray),
                )]));
            }
            if *expanded {
                render_tool_content_body(
                    &mut lines,
                    name,
                    content,
                    group_chunks,
                    grouped,
                    usize::MAX,
                    width,
                );
                lines.push(Line::from(Span::styled(
                    "  Ctrl+O collapse",
                    Style::default().fg(Color::DarkGray),
                )));
            } else if is_mutating_diff
                && matches!(ok, Some(true) | Some(false))
                && !content.is_empty()
            {
                let remaining = push_diff_preview(&mut lines, content, 8, width);
                if remaining > 0 {
                    lines.push(Line::from(vec![Span::styled(
                        format!("  +{remaining} lines hidden · Ctrl+O"),
                        Style::default().fg(Color::DarkGray),
                    )]));
                }
            } else if matches!(ok, Some(false)) && !content.is_empty() {
                let stripped = strip_content_line_numbers(content);
                let error_lines: Vec<&str> = stripped
                    .lines()
                    .filter(|l| {
                        l.starts_with("error")
                            || l.contains("error[E")
                            || l.starts_with("Error")
                            || l.contains("→ ")
                    })
                    .take(COLLAPSED_PREVIEW_LINES + 2)
                    .collect();
                if error_lines.is_empty() {
                    for raw in stripped.lines().take(COLLAPSED_PREVIEW_LINES) {
                        push_prefixed_wrapped_line(
                            &mut lines,
                            "│ ",
                            Style::default().fg(Color::DarkGray),
                            raw,
                            Style::default().fg(Color::Red),
                            width,
                        );
                    }
                } else {
                    for raw in &error_lines {
                        let style = if raw.starts_with("error") || raw.contains("error[E") {
                            Style::default().fg(Color::Red)
                        } else if raw.contains("→ ") {
                            Style::default().fg(Color::Yellow)
                        } else {
                            Style::default().fg(Color::Red)
                        };
                        push_prefixed_wrapped_line(
                            &mut lines,
                            "   ",
                            Style::default(),
                            raw,
                            style,
                            width,
                        );
                    }
                    let remaining = stripped.lines().count().saturating_sub(error_lines.len());
                    if remaining > 0 {
                        lines.push(Line::from(vec![Span::styled(
                            format!("  ⎯ {remaining} more lines hidden · Ctrl+O"),
                            Style::default().fg(Color::DarkGray),
                        )]));
                    }
                }
            } else {
                let stripped = if name == "rg" {
                    rg_display_content(content)
                } else {
                    strip_content_line_numbers(content)
                };
                let total_lines = stripped.lines().count();
                if total_lines > COLLAPSED_PREVIEW_LINES {
                    if looks_like_markdownish_tool_content(name, &stripped) {
                        let preview = stripped
                            .lines()
                            .take(COLLAPSED_PREVIEW_LINES)
                            .collect::<Vec<_>>()
                            .join("\n");
                        push_prefixed_text(
                            &mut lines,
                            Text::from(collect_wrapped_lines(
                                &markdown_text(
                                    &preview,
                                    Style::default().fg(Color::Gray),
                                    width.saturating_sub(2).max(1),
                                ),
                                width.saturating_sub(2).max(1),
                            )),
                            "│ ",
                            Style::default().fg(Color::DarkGray),
                            width,
                        );
                    } else {
                        for raw in stripped.lines().take(COLLAPSED_PREVIEW_LINES) {
                            push_prefixed_wrapped_line(
                                &mut lines,
                                "│ ",
                                Style::default().fg(Color::DarkGray),
                                raw,
                                Style::default().fg(Color::DarkGray),
                                width,
                            );
                        }
                    }
                    let remaining = total_lines.saturating_sub(COLLAPSED_PREVIEW_LINES);
                    lines.push(Line::from(vec![Span::styled(
                        format!("  +{remaining} lines hidden · Ctrl+O"),
                        Style::default().fg(Color::DarkGray),
                    )]));
                }
            }
        }
        Line_::PermissionPrompt {
            tool,
            command,
            risk,
            ..
        } => {
            return permission_prompt_text(tool, command, *risk, width);
        }
        Line_::PermissionResult {
            command,
            approved,
            always,
        } => {
            let marker = if *approved { "✓" } else { "✗" };
            let color = if *approved { Color::Green } else { Color::Red };
            let verdict = if *approved {
                if *always {
                    "approved (always)"
                } else {
                    "approved (once)"
                }
            } else {
                "denied"
            };
            lines.push(Line::from(vec![
                Span::styled(format!("{marker} "), Style::default().fg(color)),
                Span::styled(verdict.to_string(), Style::default().fg(color)),
                Span::styled(" · ".to_string(), Style::default().fg(Color::DarkGray)),
                Span::styled(command.clone(), Style::default().fg(Color::DarkGray)),
            ]));
        }
        Line_::LocalAuth { tool, message } => {
            push_prefixed_wrapped_line(
                &mut lines,
                "🔐 ",
                Style::default().fg(Color::Yellow),
                &format!("local auth for {tool}: {message}"),
                Style::default().fg(Color::Yellow),
                width,
            );
        }
        Line_::Info(s) => {
            let trimmed = s.trim_start();
            if let Some(rest) = trimmed.strip_prefix("[sub]") {
                for seg in rest.trim_start().split('\n') {
                    lines.push(Line::from(vec![
                        Span::styled("⋮ ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            "sub ",
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                        Span::styled(
                            seg.to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
            } else if trimmed.starts_with("[phase:") {
                let phase_label = trimmed
                    .strip_prefix("[phase:")
                    .and_then(|r| r.strip_suffix(']'))
                    .unwrap_or(trimmed);
                lines.push(Line::from(vec![Span::styled(
                    format!("▫ {phase_label}"),
                    Style::default()
                        .fg(Color::Indexed(242))
                        .add_modifier(Modifier::ITALIC),
                )]));
            } else if trimmed.starts_with("[objective:") {
                lines.push(Line::from(vec![Span::styled(
                    trimmed
                        .trim_start_matches('[')
                        .trim_end_matches(']')
                        .to_string(),
                    Style::default()
                        .fg(Color::Indexed(242))
                        .add_modifier(Modifier::ITALIC),
                )]));
            } else if trimmed.starts_with("* provider '") && trimmed.contains(" models:") {
                let body = trimmed.trim_start_matches("* ");
                lines.push(Line::from(vec![
                    Span::styled("★ ", Style::default().fg(Color::Green)),
                    Span::styled(
                        body.to_string(),
                        Style::default()
                            .fg(Color::LightBlue)
                            .add_modifier(Modifier::BOLD),
                    ),
                ]));
            } else if trimmed.starts_with("provider '") && trimmed.contains(" models:") {
                lines.push(Line::from(vec![Span::styled(
                    format!("• {trimmed}"),
                    Style::default()
                        .fg(Color::LightBlue)
                        .add_modifier(Modifier::BOLD),
                )]));
            } else {
                for seg in s.split('\n') {
                    lines.push(Line::from(vec![
                        Span::styled("• ", Style::default().fg(Color::DarkGray)),
                        Span::styled(
                            seg.to_string(),
                            Style::default()
                                .fg(Color::DarkGray)
                                .add_modifier(Modifier::ITALIC),
                        ),
                    ]));
                }
            }
        }
        Line_::Warn(s) => {
            let sanitized = sanitize_display_text(s);
            let friendlier = if sanitized.contains("runtime guidance:")
                && sanitized.contains("objective checkpoints still look unresolved")
            {
                let items = sanitized.rsplit(": ").next().unwrap_or(sanitized.as_str());
                format!("objective not verified yet — run tests? ({})", items.trim())
            } else {
                sanitized
            };
            lines.push(Line::from(vec![
                Span::styled("⚠ ", Style::default().fg(Color::Yellow)),
                Span::styled(friendlier, Style::default().fg(Color::Yellow)),
            ]));
        }
        Line_::Error(s) => {
            lines.push(Line::from(vec![
                Span::styled("✗ ", Style::default().fg(Color::Red)),
                Span::styled(sanitize_display_text(s), Style::default().fg(Color::Red)),
            ]));
        }
        Line_::Retry(s) => {
            lines.push(Line::from(vec![
                Span::styled("↺ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    sanitize_display_text(s),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        Line_::Steering(s) => {
            let sanitized = sanitize_display_text(s);
            let body_style = Style::default()
                .fg(Color::LightMagenta)
                .bg(STEERING_BG)
                .add_modifier(Modifier::ITALIC);
            let border_style = Style::default().fg(Color::Indexed(177)).bg(STEERING_BG);
            push_prefixed_wrapped_line(
                &mut lines,
                ">> ",
                border_style,
                &sanitized,
                body_style,
                width,
            );
        }
        Line_::SteeringDelivered { messages, preview } => {
            let noun = if *messages == 1 {
                "message"
            } else {
                "messages"
            };
            lines.push(Line::from(vec![
                Span::styled("↳ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!(
                        "queued for next response: {messages} {noun} — {}",
                        sanitize_display_text(preview)
                    ),
                    Style::default()
                        .fg(Color::Indexed(242))
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
        Line_::Thinking(s) => {
            let sanitized = sanitize_display_text(s);
            let thinking_style = Style::default()
                .fg(Color::Gray)
                .bg(THINKING_BG)
                .add_modifier(Modifier::ITALIC);
            let border_style = Style::default().fg(Color::Indexed(244)).bg(THINKING_BG);
            push_thinking_body_lines(&mut lines, &sanitized, border_style, thinking_style, width);
            let remaining = sanitized.lines().count().saturating_sub(20);
            if remaining > 0 {
                push_prefixed_wrapped_spans(
                    &mut lines,
                    "│ ",
                    border_style,
                    vec![Span::styled(
                        format!("… ({remaining} more lines)"),
                        thinking_style,
                    )],
                    width,
                );
            }
        }
        Line_::WorkMap {
            kind,
            text,
            waypoint_ids,
            selected,
            ..
        } => push_work_map_lines(&mut lines, *kind, text, waypoint_ids, *selected, width),
        Line_::Blank => lines.push(Line::from("")),
    }
    Text::from(lines)
}

fn work_map_line_style(raw: &str, is_selected: bool) -> Style {
    let trimmed = raw.trim_start();
    let mut style = if trimmed.starts_with("Work map") || trimmed.starts_with("[dext") {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else if trimmed.starts_with("commands:") {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };
    if is_selected {
        style = style
            .fg(Color::Black)
            .bg(Color::Cyan)
            .add_modifier(Modifier::BOLD);
    }
    style
}

fn push_work_map_lines(
    lines: &mut Vec<Line<'static>>,
    kind: WorkMapEventKind,
    text: &str,
    waypoint_ids: &[String],
    selected: usize,
    width: u16,
) {
    let title = match kind {
        WorkMapEventKind::Map => "work map",
        WorkMapEventKind::Packet => "work packet",
        WorkMapEventKind::Focus => "work focus",
        WorkMapEventKind::Tracks => "work tracks",
    };
    let selected_id = waypoint_ids.get(selected).map(String::as_str);
    let border_style = Style::default().fg(Color::Cyan);
    let title_style = Style::default()
        .fg(Color::Cyan)
        .add_modifier(Modifier::BOLD);
    lines.push(Line::from(vec![
        Span::styled("▌ ", border_style),
        Span::styled(title.to_string(), title_style),
        Span::styled("  ".to_string(), Style::default()),
        Span::styled(
            if waypoint_ids.is_empty() {
                "printed in transcript".to_string()
            } else {
                "↑/↓ select · Enter focus · p packet · t track · Esc close".to_string()
            },
            Style::default().fg(Color::DarkGray),
        ),
    ]));

    let body_width = width.saturating_sub(2).max(1);
    for raw in sanitize_display_text(text).lines() {
        let trimmed = raw.trim_start();
        let is_selected = selected_id.is_some_and(|id| trimmed.starts_with(id));
        let body_style = work_map_line_style(raw, is_selected);
        let prefix_style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::Cyan)
        };
        let prefix = if is_selected { "▶ " } else { "│ " };
        for (idx, wrapped) in wrap_plain_visual(raw, body_width as usize)
            .into_iter()
            .enumerate()
        {
            let line_prefix = if idx == 0 { prefix } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(line_prefix.to_string(), prefix_style),
                Span::styled(wrapped, body_style),
            ]));
        }
    }
}

fn rg_display_content(content: &str) -> String {
    let sanitized = sanitize_display_text(content);
    let mut out = String::new();
    for (idx, raw) in sanitized.lines().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        out.push_str(&middle_truncate_rg_line(raw, RG_LINE_TRUNCATE_CELLS));
    }
    if sanitized.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn middle_truncate_rg_line(line: &str, max_cells: usize) -> String {
    if text_width(line) <= max_cells || max_cells < 32 {
        return line.to_string();
    }
    let marker = " … ";
    let marker_width = text_width(marker);
    let target = max_cells.saturating_sub(marker_width);
    let left_target = target / 2;
    let right_target = target.saturating_sub(left_target);
    let mut left = String::new();
    let mut left_cells = 0usize;
    for ch in line.chars() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if left_cells + w > left_target {
            break;
        }
        left.push(ch);
        left_cells += w;
    }
    let mut right_rev = Vec::new();
    let mut right_cells = 0usize;
    for ch in line.chars().rev() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        if right_cells + w > right_target {
            break;
        }
        right_rev.push(ch);
        right_cells += w;
    }
    let right: String = right_rev.into_iter().rev().collect();
    format!("{left}{marker}{right}")
}

fn render_tool_content_body(
    lines: &mut Vec<Line<'static>>,
    name: &str,
    content: &str,
    group_chunks: &[ToolChunk],
    grouped: bool,
    max_diff_lines: usize,
    width: u16,
) {
    if grouped {
        for (i, chunk) in group_chunks.iter().enumerate() {
            if i > 0 {
                lines.push(Line::from(""));
            }
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    format!("{} chunk {}: {}", chunk.call_tag, i + 1, chunk.summary),
                    Style::default().fg(Color::DarkGray),
                ),
            ]));
            render_tool_content_body(
                lines,
                name,
                &chunk.content,
                &[],
                false,
                max_diff_lines,
                width,
            );
        }
        return;
    }

    let stripped = if name == "rg" {
        rg_display_content(content)
    } else {
        strip_content_line_numbers(content)
    };
    if matches!(name, "write_file" | "edit_file" | "multi_edit") {
        let remaining = push_diff_preview(lines, &stripped, max_diff_lines, width);
        let _ = remaining;
    } else if looks_like_markdownish_tool_content(name, &stripped) {
        push_prefixed_text(
            lines,
            Text::from(collect_wrapped_lines(
                &markdown_text(
                    &stripped,
                    Style::default().fg(Color::Gray),
                    width.saturating_sub(2).max(1),
                ),
                width.saturating_sub(2).max(1),
            )),
            "│ ",
            Style::default().fg(Color::DarkGray),
            width,
        );
    } else {
        for raw in stripped.lines() {
            push_prefixed_wrapped_line(
                lines,
                "│ ",
                Style::default().fg(Color::DarkGray),
                raw,
                Style::default().fg(Color::DarkGray),
                width,
            );
        }
    }
}

fn text_visual_height(text: &Text, width: u16) -> u16 {
    let w = width.max(1) as usize;
    let mut h: u16 = 0;
    for line in &text.lines {
        let cells: usize = line
            .spans
            .iter()
            .map(|s| {
                s.content
                    .chars()
                    .map(|c| unicode_width::UnicodeWidthChar::width(c).unwrap_or(0))
                    .sum::<usize>()
            })
            .sum();
        let rows = if cells == 0 { 1 } else { cells.div_ceil(w) };
        h = h.saturating_add(rows as u16);
    }
    h.max(1)
}

fn cached_transcript_render(
    state: &mut TuiState,
    item: &Line_,
    width: u16,
) -> (Text<'static>, u16) {
    if let Line_::PermissionPrompt {
        tool,
        command,
        risk,
        ..
    } = item
    {
        let text = permission_prompt_text(tool, command, *risk, width);
        let height = text.lines.len().max(1).min(u16::MAX as usize) as u16;
        return (text, height);
    }

    let key = line_cache_key(item);
    let entry = state
        .render_cache
        .entry(key)
        .or_insert_with(|| CachedTranscriptRender {
            renders: HashMap::new(),
            heights: HashMap::new(),
        });

    let render_width = transcript_render_width(width);
    let text = entry
        .renders
        .entry(render_width)
        .or_insert_with(|| line_to_text(item, render_width))
        .clone();
    let height = *entry
        .heights
        .entry(render_width)
        .or_insert_with(|| text_visual_height(&text, render_width));

    let mut text = text;
    if transcript_item_should_dim(item, state) {
        dim_text(&mut text);
    }
    (text, height)
}

fn sync_last_expandable(state: &mut TuiState, items: &[Line_]) {
    // Sync last_expandable with the final Tool block — groups expand into
    // per-chunk output; singles keep their single-content expansion.
    for item in items.iter().rev() {
        if let Line_::Tool {
            name,
            content,
            group_count,
            expanded,
            ..
        } = item
        {
            state.last_expandable = if *group_count > 1 || content_has_more_than_preview(content) {
                Some(ExpandableBlock {
                    name: name.clone(),
                    expanded: *expanded,
                })
            } else {
                None
            };
            break;
        }
    }
}

fn next_transcript_tint(item: &Line_, tool_tint_parity: &mut bool) -> Option<Color> {
    match item {
        Line_::Tool {
            name, group_count, ..
        } => {
            let on = *tool_tint_parity;
            *tool_tint_parity = !*tool_tint_parity;
            if *name == "read_file" && *group_count > 1 {
                return Some(Color::Indexed(235));
            }
            if on { Some(Color::Indexed(236)) } else { None }
        }
        _ => None,
    }
}

fn insert_transcript_item<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    item: &Line_,
    width: u16,
    tool_tint_parity: &mut bool,
) -> io::Result<()> {
    let (text, height) = cached_transcript_render(state, item, width);
    let tint_bg = match item {
        Line_::Thinking(_) => Some(THINKING_BG),
        Line_::Steering(_) => Some(STEERING_BG),
        _ => next_transcript_tint(item, tool_tint_parity),
    };
    terminal.insert_before(height, |buf| {
        let para = Paragraph::new(text).wrap(Wrap { trim: false });
        Widget::render(para, buf.area, buf);
        if let Some(bg) = tint_bg {
            let area = buf.area;
            for y in area.top()..area.bottom() {
                for x in area.left()..area.right() {
                    let cell = &mut buf[(x, y)];
                    if cell.bg == Color::Reset {
                        cell.bg = bg;
                    }
                }
            }
        }
    })?;
    Ok(())
}

fn rebuild_transcript<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
) -> io::Result<()> {
    terminal.clear()?;
    let width = terminal.size()?.width;
    let items = state.transcript.clone();
    sync_last_expandable(state, &items);
    let mut tool_tint_parity = false;
    for item in &items {
        insert_transcript_item(terminal, state, item, width, &mut tool_tint_parity)?;
    }
    state.tool_tint_parity = tool_tint_parity;
    state.transcript_needs_rebuild = false;
    Ok(())
}

fn flush_pending_insert<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
) -> io::Result<()> {
    if state.transcript_needs_rebuild {
        rebuild_transcript(terminal, state)?;
    }

    let raw: Vec<Line_> = std::mem::take(&mut state.pending_insert);
    let mut items: Vec<Line_> = merge_consecutive_tools(raw);
    flush_prepared_items(terminal, state, &mut items)
}

fn flush_prepared_items<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    items: &mut Vec<Line_>,
) -> io::Result<()> {
    mark_retry_cycles(items);
    // Inline viewport output is real terminal scrollback: already-inserted lines cannot be
    // rewritten without appending another copy of the transcript. Keep grouping within the
    // current flush batch only; never merge new tool output into historical scrollback.
    let start_rank = state
        .transcript
        .iter()
        .rev()
        .find_map(|item| match item {
            Line_::Tool {
                density_rank,
                group_count,
                ..
            } => Some(density_rank.saturating_add(group_count.saturating_sub(1))),
            _ => None,
        })
        .unwrap_or(0);
    set_tool_density_ranks(items, start_rank);
    let expansion_active = state.last_expandable.as_ref().is_some_and(|b| b.expanded);
    if !expansion_active {
        sync_last_expandable(state, items);
    }

    if items.is_empty() {
        return Ok(());
    }

    let width = terminal.size()?.width;
    let mut tool_tint_parity = state.tool_tint_parity;
    for item in items.iter() {
        insert_transcript_item(terminal, state, item, width, &mut tool_tint_parity)?;
    }
    state.tool_tint_parity = tool_tint_parity;
    state.transcript.append(items);
    Ok(())
}

fn set_last_tool_expanded(items: &mut [Line_], name: &str, expanded: bool) -> bool {
    if let Some(Line_::Tool {
        expanded: tool_expanded,
        ..
    }) = items
        .iter_mut()
        .rev()
        .find(|item| matches!(item, Line_::Tool { name: tool_name, .. } if tool_name == name))
    {
        *tool_expanded = expanded;
        true
    } else {
        false
    }
}

fn input_border_style(state: &TuiState) -> Style {
    let color = match state.approval_profile {
        ApprovalProfile::Always => TRUST_INPUT_BORDER,
        _ => Color::DarkGray,
    };
    Style::default().fg(color)
}

fn input_hint_text(state: &TuiState) -> &'static str {
    if state.pending_perm.is_some() {
        "  press y / a / n to respond"
    } else if state.work_map_is_active() {
        "  Work Map drawer  ·  Enter focuses selection  ·  p packet  ·  t track  ·  Esc close"
    } else if state.input_display_override.is_some() {
        "  large paste collapsed visually  ·  Enter sends full input  ·  any edit reveals full text"
    } else if state.agent_busy {
        "  agent busy: chat input becomes steering; local auth secrets are withheld — use sudo/auth prompts only"
    } else {
        "  Enter submit  ·  Shift+Enter newline  ·  Ctrl+O expand/collapse  ·  PgUp/PgDn scroll"
    }
}

fn transcript_live_indicator_text(state: &TuiState, width: u16) -> Option<Text<'static>> {
    if width == 0 || !state.agent_busy || state.pending_perm.is_some() {
        return None;
    }
    let mut top = vec![
        Span::styled(
            SPINNER_FRAMES[(state.frame_count % SPINNER_FRAMES.len() as u64) as usize].to_string(),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(
            display_busy_status(derived_busy_status(state)),
            Style::default().fg(Color::DarkGray),
        ),
    ];
    if let Some(elapsed) = live_indicator_elapsed(state) {
        top.push(Span::styled("  ·  ", Style::default().fg(Color::DarkGray)));
        top.push(Span::styled(elapsed, Style::default().fg(Color::Yellow)));
    }
    let mut lines = vec![Line::from(top)];
    if let Some(detail) = live_indicator_detail(state, width) {
        lines.push(detail);
    } else if let Some(detail) = live_indicator_todo_detail(state, width.saturating_sub(4) as usize)
    {
        lines.push(detail);
    }
    Some(Text::from(lines))
}

fn cap_live_indicator_lines(mut lines: Vec<Line<'static>>) -> Vec<Line<'static>> {
    const LIVE_INDICATOR_MAX_LINES: usize = 2;
    if lines.len() > LIVE_INDICATOR_MAX_LINES {
        lines.drain(..lines.len() - LIVE_INDICATOR_MAX_LINES);
    }
    lines
}

fn count_lines_by_width(text: &Text<'_>, width: u16) -> usize {
    text_visual_height(text, width) as usize
}

fn collect_wrapped_lines(text: &Text<'static>, width: u16) -> Vec<Line<'static>> {
    if width == 0 {
        return Vec::new();
    }
    let height = count_lines_by_width(text, width)
        .max(1)
        .min(u16::MAX as usize) as u16;
    let paragraph = Paragraph::new(text.clone()).wrap(Wrap { trim: false });
    let area = Rect::new(0, 0, width, height);
    let mut buffer = ratatui::buffer::Buffer::empty(area);
    Widget::render(paragraph, area, &mut buffer);
    buffer_to_lines(&buffer, area)
}

fn render_transcript(frame: &mut ratatui::Frame, state: &mut TuiState, transcript_area: Rect) {
    let content_width = transcript_render_width(transcript_area.width);
    let live_text = transcript_live_indicator_text(state, content_width);
    let live_lines = live_text
        .as_ref()
        .map(|text| cap_live_indicator_lines(collect_wrapped_lines(text, content_width)))
        .unwrap_or_default();
    let live_indicator_lines = live_lines.len();
    let viewport_height = transcript_area.height as usize;

    state.transcript_scroll_max = 0;
    state.transcript_scroll_offset = 0;

    if transcript_area.width == 0 || transcript_area.height == 0 || live_indicator_lines == 0 {
        state.set_transcript_layout(TranscriptLayoutState {
            transcript_area,
            input_area: state.input_area,
            total_lines: 0,
            visible_lines: viewport_height,
            live_indicator_lines: 0,
            live_indicator_top_padding: 0,
            live_indicator_visible: false,
            live_indicator_scroll_start: 0,
            live_indicator_scroll_end: 0,
            transcript_line_layout: Vec::new(),
            live_indicator_line_layout: None,
            live_indicator_text: live_text,
        });
        return;
    }

    let live_height = live_indicator_lines.min(transcript_area.height as usize) as u16;
    let live_area = Rect::new(
        transcript_area.x,
        transcript_area
            .y
            .saturating_add(transcript_area.height.saturating_sub(live_height)),
        transcript_area.width,
        live_height,
    );
    let text = Text::from(live_lines.clone());
    render_widget_safe(
        frame,
        Paragraph::new(text).wrap(Wrap { trim: false }),
        live_area,
    );

    let live_start = transcript_area.height.saturating_sub(live_height) as usize;
    state.set_transcript_layout(TranscriptLayoutState {
        transcript_area,
        input_area: state.input_area,
        total_lines: live_indicator_lines,
        visible_lines: viewport_height,
        live_indicator_lines,
        live_indicator_top_padding: live_start,
        live_indicator_visible: true,
        live_indicator_scroll_start: live_start,
        live_indicator_scroll_end: live_start.saturating_add(live_indicator_lines),
        transcript_line_layout: Vec::new(),
        live_indicator_line_layout: Some((
            live_start,
            live_start.saturating_add(live_indicator_lines),
        )),
        live_indicator_text: live_text,
    });
}

fn queue_permission_request(
    state: &mut TuiState,
    name: String,
    input: Value,
    responder: std::sync::mpsc::SyncSender<Choice>,
) {
    let risk = crate::tool_policy::classify_command_risk(&name, &input);
    let tier = PermissionTier::from_risk(risk);
    let command = permission_command_text(&name, &input);
    let audit_label = permission_audit_label(&name, &input);
    state.status = "thinking".to_string();
    let prompt = Line_::PermissionPrompt {
        tool: name.clone(),
        command: command.clone(),
        tier,
        risk,
    };
    if !replace_last_permission_entry(&mut state.pending_insert, prompt.clone())
        && !replace_last_permission_entry(&mut state.transcript, prompt.clone())
    {
        state.queue(prompt);
    }
    state.transcript_needs_rebuild = true;
    state.pending_perm = Some(PendingPermission {
        tool: name,
        audit_label,
        tier,
        responder,
    });
}

fn help_overlay_text() -> Text<'static> {
    let keymap_rows: &[(&str, &str)] = &[
        ("Enter", "submit prompt"),
        ("Shift+Enter / Alt+Enter", "insert newline"),
        ("Paste", "multi-line paste is inserted without auto-submit"),
        ("Esc", "clear input / close this help"),
        ("Ctrl+C", "interrupt agent (twice = quit)"),
        (
            "Auth secrets",
            "never type sudo passwords here; use local auth prompts",
        ),
        ("Ctrl+D", "quit"),
        ("Ctrl+O", "toggle last tool output"),
        ("Ctrl+V", "toggle thinking visibility"),
        (
            "Tab / Shift+Tab",
            "cycle reasoning depth (slash completion when typing /)",
        ),
        ("Ctrl+E", "cycle reasoning depth"),
        ("PgUp / PgDn", "scroll transcript"),
        ("Up / Down", "history (single-line input only)"),
        ("?", "toggle this help"),
    ];
    let legend_rows: &[(&str, &str)] = &[
        ("↑N ↻ N ↓N", "actual input / cached input / output tokens"),
        ("% [████░░░░░░]", "last request context window usage"),
        ("● / ⠋", "ready / busy spinner"),
        ("(branch)", "git branch in sandbox"),
    ];
    let mut lines: Vec<Line<'static>> = Vec::new();
    lines.push(Line::from(Span::styled(
        "keymap",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for (key, desc) in keymap_rows {
        lines.push(Line::from(vec![
            Span::styled(
                format!("  {:<26}", key),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled((*desc).to_string(), Style::default()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "status line",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    lines.push(Line::from(""));
    for (sym, desc) in legend_rows {
        lines.push(Line::from(vec![
            Span::styled(format!("  {:<20}", sym), Style::default().fg(Color::Green)),
            Span::styled((*desc).to_string(), Style::default()),
        ]));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "  press ? or Esc to dismiss",
        Style::default().fg(Color::DarkGray),
    )));
    Text::from(lines)
}

fn centered_rect(area: ratatui::layout::Rect, width: u16, height: u16) -> ratatui::layout::Rect {
    let w = width.min(area.width);
    let h = height.min(area.height);
    let x = area.x + (area.width.saturating_sub(w)) / 2;
    let y = area.y + (area.height.saturating_sub(h)) / 2;
    ratatui::layout::Rect::new(x, y, w, h)
}

fn clip_rect(
    rect: ratatui::layout::Rect,
    bounds: ratatui::layout::Rect,
) -> Option<ratatui::layout::Rect> {
    let left = rect.left().max(bounds.left());
    let top = rect.top().max(bounds.top());
    let right = rect.right().min(bounds.right());
    let bottom = rect.bottom().min(bounds.bottom());
    if right <= left || bottom <= top {
        return None;
    }
    Some(ratatui::layout::Rect::new(
        left,
        top,
        right.saturating_sub(left),
        bottom.saturating_sub(top),
    ))
}

fn empty_rect(origin: ratatui::layout::Rect) -> ratatui::layout::Rect {
    ratatui::layout::Rect::new(origin.x, origin.y, 0, 0)
}

fn render_widget_safe<W: Widget>(
    frame: &mut ratatui::Frame,
    widget: W,
    rect: ratatui::layout::Rect,
) {
    if let Some(rect) = clip_rect(rect, frame.area()) {
        frame.render_widget(widget, rect);
    }
}

struct SlashPopupLayout {
    rect: ratatui::layout::Rect,
    visible_count: usize,
    name_width: usize,
}

#[derive(Clone, Copy, Debug)]
struct TuiLayout {
    transcript_area: ratatui::layout::Rect,
    input_area: ratatui::layout::Rect,
    status_area: ratatui::layout::Rect,
}

fn compute_layout(area: ratatui::layout::Rect, state: &TuiState) -> TuiLayout {
    let input_height = input_panel_height(state, area.height, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(input_height),
            Constraint::Length(area.height.min(1)),
        ])
        .split(area);

    TuiLayout {
        transcript_area: clip_rect(chunks[0], area).unwrap_or_else(|| empty_rect(area)),
        input_area: clip_rect(chunks[1], area).unwrap_or_else(|| empty_rect(area)),
        status_area: clip_rect(chunks[2], area).unwrap_or_else(|| empty_rect(area)),
    }
}

fn slash_popup_layout(
    area: ratatui::layout::Rect,
    input_area: ratatui::layout::Rect,
    completions: &[SlashCompletion],
) -> Option<SlashPopupLayout> {
    if completions.is_empty() || area.width == 0 || area.height < 3 {
        return None;
    }

    let visible_count = completions
        .len()
        .min(SLASH_COMPLETION_MAX_VISIBLE)
        .min(area.height.saturating_sub(2) as usize);
    if visible_count == 0 {
        return None;
    }

    let name_width = completions
        .iter()
        .take(visible_count)
        .map(|c| unicode_width::UnicodeWidthStr::width(c.text.as_str()))
        .max()
        .unwrap_or(0);

    let area_right = area.x.saturating_add(area.width);
    let popup_x = input_area
        .x
        .saturating_add(1)
        .min(area_right.saturating_sub(1));
    let max_popup_w = area_right.saturating_sub(popup_x);
    if max_popup_w == 0 {
        return None;
    }

    let desired_w = (name_width as u16).saturating_add(28).max(30);
    let popup_w = desired_w.min(max_popup_w);
    let popup_h = (visible_count as u16).saturating_add(2).min(area.height);
    let popup_y = input_area.y.saturating_sub(popup_h).max(area.y);

    Some(SlashPopupLayout {
        rect: ratatui::layout::Rect::new(popup_x, popup_y, popup_w, popup_h),
        visible_count,
        name_width,
    })
}

fn work_map_drawer_rows(drawer: &WorkMapDrawer) -> Vec<String> {
    let sanitized = sanitize_display_text(&drawer.text);
    let text_lines = sanitized.lines().collect::<Vec<_>>();
    drawer
        .waypoint_ids
        .iter()
        .map(|id| {
            text_lines
                .iter()
                .find(|line| line.trim_start().starts_with(id.as_str()))
                .map(|line| line.trim_start().to_string())
                .unwrap_or_else(|| id.clone())
        })
        .collect()
}

fn sync_work_map_scroll(drawer: &mut WorkMapDrawer, visible_rows: usize) {
    if visible_rows == 0 || drawer.waypoint_ids.is_empty() {
        drawer.scroll = 0;
        return;
    }
    let max_selected = drawer.waypoint_ids.len().saturating_sub(1);
    drawer.selected = drawer.selected.min(max_selected);
    if drawer.selected < drawer.scroll {
        drawer.scroll = drawer.selected;
    } else if drawer.selected >= drawer.scroll.saturating_add(visible_rows) {
        drawer.scroll = drawer
            .selected
            .saturating_add(1)
            .saturating_sub(visible_rows);
    }
    let max_scroll = drawer.waypoint_ids.len().saturating_sub(visible_rows);
    drawer.scroll = drawer.scroll.min(max_scroll);
}

fn work_map_drawer_lines(state: &mut TuiState, width: u16, height: usize) -> Vec<Line<'static>> {
    if height < 3 || width == 0 {
        return Vec::new();
    }
    let Some(drawer) = state.work_map.as_mut() else {
        return Vec::new();
    };
    if drawer.waypoint_ids.is_empty() {
        return Vec::new();
    }

    let body_rows = height.saturating_sub(2).min(WORK_MAP_DRAWER_MAX_BODY_ROWS);
    if body_rows == 0 {
        return Vec::new();
    }
    sync_work_map_scroll(drawer, body_rows);

    let rows = work_map_drawer_rows(drawer);
    let total = drawer.waypoint_ids.len();
    let selected = drawer.selected.min(total.saturating_sub(1));
    let selected_id = drawer
        .waypoint_ids
        .get(selected)
        .map(String::as_str)
        .unwrap_or("?");
    let inner_width = width.max(1) as usize;
    let mut lines = Vec::with_capacity(height);
    let title = format!("▌ Work Map  {selected_id}  {}/{}", selected + 1, total);
    lines.push(Line::from(vec![Span::styled(
        clamp_chars(&title, inner_width),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )]));

    for idx in drawer.scroll..drawer.scroll.saturating_add(body_rows).min(total) {
        let raw = rows.get(idx).map(String::as_str).unwrap_or_else(|| {
            drawer
                .waypoint_ids
                .get(idx)
                .map(String::as_str)
                .unwrap_or("?")
        });
        let is_selected = idx == selected;
        let prefix = if is_selected { "▶ " } else { "  " };
        let prefix_style = if is_selected {
            Style::default().fg(Color::Black).bg(Color::Cyan)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        let body_width = inner_width.saturating_sub(text_width(prefix)).max(1);
        lines.push(Line::from(vec![
            Span::styled(prefix.to_string(), prefix_style),
            Span::styled(
                clamp_chars(raw, body_width),
                work_map_line_style(raw, is_selected),
            ),
        ]));
    }

    while lines.len() < height.saturating_sub(1) {
        lines.push(Line::from(""));
    }

    let first_visible = drawer.scroll.saturating_add(1).min(total);
    let last_visible = drawer.scroll.saturating_add(body_rows).min(total);
    let scroll_hint = if total > body_rows {
        format!("showing {first_visible}-{last_visible}/{total} · ")
    } else {
        String::new()
    };
    lines.push(Line::from(Span::styled(
        clamp_chars(
            &format!("  {scroll_hint}Enter focus · p packet · t track · Esc close"),
            inner_width,
        ),
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

fn draw(frame: &mut ratatui::Frame, state: &mut TuiState) {
    let area = frame.area();
    render_widget_safe(frame, Clear, area);

    let layout = compute_layout(area, state);
    let transcript_area = layout.transcript_area;
    let input_area = layout.input_area;
    let status_area = layout.status_area;
    state.input_area = input_area;

    render_transcript(frame, state, transcript_area);

    let status = Paragraph::new(Line::from(status_spans(state)));
    render_widget_safe(frame, status, status_area);

    let prompt_style = if state.agent_busy && state.pending_perm.is_none() {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let wrap_cols = input_area.width.saturating_sub(2).max(1) as usize;
    let (wrapped, cursor_row, cursor_col) =
        wrap_input_visual(&state.input, state.cursor, wrap_cols);
    let inner_rows = input_area.height.saturating_sub(2).max(1) as usize;
    let drawer_height = work_map_drawer_height(state, input_area.width).min(inner_rows);
    let hint_rows = 1usize;
    let mut text_rows_visible = inner_rows
        .saturating_sub(drawer_height)
        .saturating_sub(hint_rows)
        .max(WORK_MAP_DRAWER_MIN_EDITOR_ROWS);
    let drawer_rows = drawer_height.min(inner_rows.saturating_sub(text_rows_visible + hint_rows));
    text_rows_visible = inner_rows
        .saturating_sub(drawer_rows)
        .saturating_sub(hint_rows)
        .max(WORK_MAP_DRAWER_MIN_EDITOR_ROWS);

    let mut start_row = 0usize;
    if wrapped.len() > text_rows_visible {
        start_row = cursor_row.saturating_sub(text_rows_visible.saturating_sub(1));
        let max_start = wrapped.len().saturating_sub(text_rows_visible);
        start_row = start_row.min(max_start);
    }
    let end_row = (start_row + text_rows_visible).min(wrapped.len());

    let mut lines: Vec<Line> = wrapped[start_row..end_row]
        .iter()
        .map(|line| Line::from(Span::styled(line.clone(), prompt_style)))
        .collect();
    while lines.len() < text_rows_visible {
        lines.push(Line::from(""));
    }
    if drawer_rows > 0 {
        lines.extend(work_map_drawer_lines(state, wrap_cols as u16, drawer_rows));
    }
    lines.push(Line::from(Span::styled(
        input_hint_text(state),
        Style::default().fg(Color::DarkGray),
    )));

    let input = Paragraph::new(lines).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(input_border_style(state)),
    );
    render_widget_safe(frame, input, input_area);

    if state.pending_perm.is_none()
        && !state.show_help
        && !state.work_map_is_active()
        && input_area.width > 0
        && input_area.height > 0
    {
        let cursor_row_vis = cursor_row.saturating_sub(start_row) as u16;
        let max_cx = input_area.x + input_area.width.saturating_sub(2);
        let max_cy = input_area.y + input_area.height.saturating_sub(2);
        let cx = (input_area.x + 1 + cursor_col as u16).min(max_cx);
        let cy = (input_area.y + cursor_row_vis + 1).min(max_cy);
        frame.set_cursor_position((cx, cy));
    }

    if state.show_help {
        let help = help_overlay_text();
        let desired_w = 56u16;
        let desired_h = (help.lines.len() as u16).saturating_add(2);
        let rect = centered_rect(area, desired_w, desired_h);
        let widget = Paragraph::new(help).wrap(Wrap { trim: false }).block(
            Block::default()
                .borders(Borders::ALL)
                .title(Span::styled(
                    " help ",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ))
                .border_style(Style::default().fg(Color::Cyan)),
        );
        render_widget_safe(frame, Clear, rect);
        render_widget_safe(frame, widget, rect);
    }

    if !state.show_help
        && state.pending_perm.is_none()
        && !state.work_map_is_active()
        && (!state.agent_busy || state.input.starts_with('/'))
    {
        let completions = slash_completions(&state.input);
        if let Some(layout) = slash_popup_layout(area, input_area, &completions) {
            let (sel, scroll) = state
                .sync_slash_completion_window(completions.len(), layout.visible_count)
                .unwrap_or((0, 0));
            let visible = completions
                .iter()
                .enumerate()
                .skip(scroll)
                .take(layout.visible_count);
            let mut popup_lines: Vec<Line> = Vec::new();
            for (i, comp) in visible {
                let is_sel = i == sel;
                let cmd_style = if is_sel {
                    Style::default()
                        .fg(Color::Black)
                        .bg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::Cyan)
                };
                let hint_style = if is_sel {
                    Style::default().fg(Color::Black).bg(Color::Cyan)
                } else {
                    Style::default().fg(Color::DarkGray)
                };
                let padding = layout
                    .name_width
                    .saturating_add(1)
                    .saturating_sub(unicode_width::UnicodeWidthStr::width(comp.text.as_str()));
                let padded = format!("{}{}", comp.text, " ".repeat(padding));
                let mut spans = vec![
                    Span::styled(padded, cmd_style),
                    Span::styled(&comp.hint, hint_style),
                ];
                if comp.hint.is_empty() {
                    spans.pop();
                }
                popup_lines.push(Line::from(spans));
            }
            let hint = if completions.len() > layout.visible_count {
                format!(
                    " ↑↓ select · Tab accept · {}/{} ",
                    sel + 1,
                    completions.len()
                )
            } else {
                " ↑↓ select · Tab accept ".to_string()
            };
            let popup_widget = Paragraph::new(popup_lines).block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(Span::styled(hint, Style::default().fg(Color::DarkGray)))
                    .border_style(Style::default().fg(Color::DarkGray)),
            );
            render_widget_safe(frame, Clear, layout.rect);
            render_widget_safe(frame, popup_widget, layout.rect);
        }
    }
}

fn handle_paste(state: &mut TuiState, pasted: String) {
    if pasted.is_empty() {
        return;
    }
    if state.agent_busy && crate::text_is_potential_local_secret(&pasted) {
        state.queue(Line_::Warn(
            "paste withheld: do not enter sudo passwords or local auth secrets in chat; use the local auth prompt".to_string(),
        ));
        state.status = "local secret paste withheld".to_string();
        return;
    }
    state.input.insert_str(state.cursor, &pasted);
    state.cursor += pasted.len();
    state.input_display_override = abstract_input_for_display(&state.input);
    if state.input_display_override.is_some() {
        state.status =
            "large paste collapsed in editor — full content preserved for Enter".to_string();
    }
    state.reset_slash_completion_selection();
}

fn handle_mouse(state: &mut TuiState, mouse: MouseEvent) {
    let column = mouse.column;
    let row = mouse.row;

    match mouse.kind {
        MouseEventKind::ScrollUp => {
            if state.managed_region_contains(column, row) {
                state.scroll_transcript_by(1);
            }
        }
        MouseEventKind::ScrollDown => {
            if state.managed_region_contains(column, row) {
                state.scroll_transcript_by(-1);
            }
        }
        _ => {}
    }
}

fn insert_command_into_input(state: &mut TuiState, command: String) {
    state.input = command;
    state.cursor = state.input.len();
    state.clear_slash_completion_selection();
    state.input_display_override = None;
}

fn handle_work_map_key(state: &mut TuiState, key: KeyEvent) -> bool {
    if !state.work_map_is_active() {
        return false;
    }
    match key.code {
        KeyCode::Esc => {
            state.work_map = None;
            state.status = "work map drawer closed".to_string();
            true
        }
        KeyCode::Up => {
            if state.move_work_map_selection(-1) {
                state.status = "work map selection moved".to_string();
            }
            true
        }
        KeyCode::Down => {
            if state.move_work_map_selection(1) {
                state.status = "work map selection moved".to_string();
            }
            true
        }
        KeyCode::PageUp => {
            let step = work_map_drawer_body_rows(state).saturating_sub(1).max(1) as isize;
            if state.move_work_map_selection_for_rows(-step, WORK_MAP_DRAWER_MAX_BODY_ROWS) {
                state.status = "work map selection moved".to_string();
            }
            true
        }
        KeyCode::PageDown => {
            let step = work_map_drawer_body_rows(state).saturating_sub(1).max(1) as isize;
            if state.move_work_map_selection_for_rows(step, WORK_MAP_DRAWER_MAX_BODY_ROWS) {
                state.status = "work map selection moved".to_string();
            }
            true
        }
        KeyCode::Home => {
            state.set_work_map_selection(0);
            state.status = "work map selection moved".to_string();
            true
        }
        KeyCode::End => {
            if let Some(last) = state
                .work_map
                .as_ref()
                .map(|drawer| drawer.waypoint_ids.len().saturating_sub(1))
            {
                state.set_work_map_selection(last);
                state.status = "work map selection moved".to_string();
            }
            true
        }
        KeyCode::Enter | KeyCode::Char('f') | KeyCode::Char('F') => {
            if let Some(arg) = state.selected_work_map_command_arg() {
                insert_command_into_input(state, format!("/focus {arg}"));
                state.work_map = None;
                state.status = "inserted /focus command".to_string();
            }
            true
        }
        KeyCode::Char('p') | KeyCode::Char('P') => {
            if let Some(arg) = state.selected_work_map_command_arg() {
                insert_command_into_input(state, format!("/packet {arg}"));
                state.work_map = None;
                state.status = "inserted /packet command".to_string();
            }
            true
        }
        KeyCode::Char('t') | KeyCode::Char('T') => {
            if let Some(arg) = state.selected_work_map_command_arg() {
                insert_command_into_input(state, format!("/track open {arg}"));
                state.work_map = None;
                state.status = "inserted /track command".to_string();
            }
            true
        }
        _ => false,
    }
}

fn handle_key(
    state: &mut TuiState,
    key: KeyEvent,
    agent_input: &tokio::sync::mpsc::UnboundedSender<FromTui>,
    steering_input: &tokio::sync::mpsc::UnboundedSender<String>,
    interrupt: &Arc<std::sync::atomic::AtomicBool>,
) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if state.work_map_is_active() && handle_work_map_key(state, key) {
        return;
    }
    if state.pending_perm.is_some() {
        let default_choice = state
            .pending_perm
            .as_ref()
            .map(|pending| pending.tier.default_choice())
            .unwrap_or(Choice::Deny);
        let choice = match (key.code, key.modifiers) {
            (KeyCode::Char('y'), _) | (KeyCode::Char('Y'), _) => Some(Choice::Once),
            (KeyCode::Char('a'), _) | (KeyCode::Char('A'), _) => Some(Choice::Always),
            (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) | (KeyCode::Esc, _) => {
                Some(Choice::Deny)
            }
            (KeyCode::Enter, _) => Some(default_choice),
            _ => None,
        };
        if let Some(choice) = choice
            && let Some(pending) = state.pending_perm.take()
        {
            let result = Line_::PermissionResult {
                command: pending.audit_label.clone(),
                approved: !matches!(choice, Choice::Deny),
                always: matches!(choice, Choice::Always),
            };
            if !replace_last_permission_entry(&mut state.pending_insert, result.clone())
                && !replace_last_permission_entry(&mut state.transcript, result.clone())
            {
                state.queue(result);
            }
            state.transcript_needs_rebuild = true;
            let _ = pending.responder.send(choice);
            match choice {
                Choice::Deny => {
                    state.status = "thinking".to_string();
                }
                Choice::Once | Choice::Always => {
                    state.status = format!("running {}", pending.tool);
                }
            }
        }
        return;
    }
    let is_ctrl = |m: KeyModifiers| {
        m.contains(KeyModifiers::CONTROL)
            && !m.intersects(KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::META)
    };
    let editing_key = matches!(
        key.code,
        KeyCode::Char(_)
            | KeyCode::Backspace
            | KeyCode::Delete
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Home
            | KeyCode::End
            | KeyCode::Up
            | KeyCode::Down
            | KeyCode::PageUp
            | KeyCode::PageDown
            | KeyCode::Enter
    );
    if state.input_display_override.is_some() && editing_key && key.code != KeyCode::Enter {
        state.input_display_override = None;
        state.status = "paste preview cleared — editing full input".to_string();
    }
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), m) if is_ctrl(m) => {
            if state.agent_busy {
                if interrupt.swap(true, Ordering::SeqCst) {
                    state.quit = true;
                    let _ = agent_input.send(FromTui::Quit);
                } else {
                    state.status = "interrupting… Ctrl+C again quits".to_string();
                }
            } else if state.input.is_empty() {
                state.quit = true;
                let _ = agent_input.send(FromTui::Quit);
            } else {
                state.input.clear();
                state.cursor = 0;
                state.clear_slash_completion_selection();
                state.input_display_override = None;
                state.status = "input cleared (Ctrl+C again to quit)".to_string();
            }
        }
        (KeyCode::Char('d'), m) if is_ctrl(m) => {
            state.quit = true;
            let _ = agent_input.send(FromTui::Quit);
        }
        (KeyCode::Char('e'), m) if is_ctrl(m) => {
            let _ = agent_input.send(FromTui::CycleEffort(1));
        }
        (KeyCode::Char('?'), m)
            if !m.contains(KeyModifiers::CONTROL)
                && !m.contains(KeyModifiers::ALT)
                && state.input.is_empty() =>
        {
            state.show_help = !state.show_help;
        }
        (KeyCode::Char('o'), m) if is_ctrl(m) => {
            if let Some(block) = state.last_expandable.as_ref() {
                let name = block.name.clone();
                let next_expanded = !block.expanded;
                let updated_pending =
                    set_last_tool_expanded(&mut state.pending_insert, &name, next_expanded);
                let updated_transcript =
                    set_last_tool_expanded(&mut state.transcript, &name, next_expanded);
                if updated_pending || updated_transcript {
                    state.transcript_needs_rebuild = true;
                    state.jump_transcript_to_bottom();
                    state.status = if next_expanded {
                        "expanded".to_string()
                    } else {
                        "collapsed".to_string()
                    };
                    if let Some(block) = state.last_expandable.as_mut() {
                        block.expanded = next_expanded;
                    }
                }
            }
        }
        (KeyCode::Char('v'), m) if is_ctrl(m) => {
            state.verbose = !state.verbose;
            state.status = if state.verbose {
                "thinking visible".to_string()
            } else {
                "thinking hidden".to_string()
            };
        }
        (KeyCode::Esc, _) => {
            if state.work_map_is_active() {
                state.work_map = None;
                state.status = "work map drawer closed".to_string();
            } else if state.show_help {
                state.show_help = false;
            } else if state.agent_busy {
                if state.input.trim().is_empty() {
                    if interrupt.swap(true, Ordering::SeqCst) {
                        state.quit = true;
                        let _ = agent_input.send(FromTui::Quit);
                    } else {
                        state.status = "interrupting… Esc again quits".to_string();
                    }
                } else {
                    state.input.clear();
                    state.cursor = 0;
                    state.clear_slash_completion_selection();
                    state.input_display_override = None;
                    state.status = "input cleared; Esc again interrupts".to_string();
                }
            } else if !state.input.is_empty() {
                state.input.clear();
                state.cursor = 0;
                state.clear_slash_completion_selection();
                state.status = "input cleared".to_string();
            }
        }
        (KeyCode::Tab, _) if !state.agent_busy => {
            if !state.accept_slash_completion() {
                let _ = agent_input.send(FromTui::CycleEffort(1));
            }
        }
        (KeyCode::BackTab, _) if !state.agent_busy => {
            if state.move_slash_completion_selection(-1) {
                let _ = state.accept_slash_completion();
            } else {
                let _ = agent_input.send(FromTui::CycleEffort(-1));
            }
        }
        (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) => {
            state.input.insert(state.cursor, '\n');
            state.cursor += 1;
            state.input_display_override = None;
        }
        (KeyCode::Enter, _) => {
            state.work_map = None;
            let text = state.input.clone();
            if text.trim().is_empty() {
                return;
            }
            if state.agent_busy && state.pending_perm.is_none() {
                if crate::text_is_potential_local_secret(&text) {
                    state.queue(Line_::Warn(
                        "input withheld: do not enter sudo passwords or local auth secrets in chat; use the local auth prompt".to_string(),
                    ));
                    state.input.clear();
                    state.cursor = 0;
                    state.clear_slash_completion_selection();
                    state.input_display_override = None;
                    state.status = "local secret withheld from provider".to_string();
                    return;
                }
                state.queue(Line_::Steering(text.clone()));
                if steering_input.send(text).is_ok() {
                    state.status = "queued for next safe boundary".to_string();
                } else {
                    state.status = "queue unavailable".to_string();
                }
                state.input.clear();
                state.cursor = 0;
                state.clear_slash_completion_selection();
                state.input_display_override = None;
                return;
            }
            state.queue(Line_::TurnSep);
            state.queue(Line_::User(text.clone()));
            if state.history.back().map(|s| s.as_str()) != Some(text.as_str()) {
                if state.history.len() >= INPUT_HISTORY_MAX {
                    state.history.pop_front();
                }
                state.history.push_back(text.clone());
            }
            state.history_idx = None;
            state.input.clear();
            state.cursor = 0;
            state.clear_slash_completion_selection();
            state.input_display_override = None;
            let _ = agent_input.send(FromTui::Submit(text));
        }
        (KeyCode::PageUp, _) => {
            let step = state.transcript_visible_lines.saturating_sub(2).max(1) as isize;
            state.scroll_transcript_by(step);
        }
        (KeyCode::PageDown, _) => {
            let step = state.transcript_visible_lines.saturating_sub(2).max(1) as isize;
            state.scroll_transcript_by(-step);
        }
        (KeyCode::Up, m) if m.contains(KeyModifiers::SHIFT) => {
            state.scroll_transcript_by(1);
        }
        (KeyCode::Down, m) if m.contains(KeyModifiers::SHIFT) => {
            state.scroll_transcript_by(-1);
        }
        (KeyCode::Home, m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.jump_transcript_to_top();
        }
        (KeyCode::End, m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.jump_transcript_to_bottom();
        }
        (KeyCode::Backspace, _) => {
            if state.cursor > 0 {
                let prev = state.input[..state.cursor]
                    .chars()
                    .last()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state
                    .input
                    .replace_range(state.cursor - prev..state.cursor, "");
                state.cursor -= prev;
                state.reset_slash_completion_selection();
            }
        }
        (KeyCode::Delete, _) => {
            if state.cursor < state.input.len() {
                let next = state.input[state.cursor..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state
                    .input
                    .replace_range(state.cursor..state.cursor + next, "");
                state.reset_slash_completion_selection();
            }
        }
        (KeyCode::Left, _) => {
            if state.cursor > 0 {
                let prev = state.input[..state.cursor]
                    .chars()
                    .last()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state.cursor -= prev;
            }
        }
        (KeyCode::Right, _) => {
            if state.cursor < state.input.len() {
                let next = state.input[state.cursor..]
                    .chars()
                    .next()
                    .map(|c| c.len_utf8())
                    .unwrap_or(0);
                state.cursor += next;
            }
        }
        (KeyCode::Up, m)
            if !m.contains(KeyModifiers::SHIFT) && state.input.trim_start().starts_with('/') =>
        {
            if state.move_slash_completion_selection(-1) {
                state.history_idx = None;
            }
        }
        (KeyCode::Down, m)
            if !m.contains(KeyModifiers::SHIFT) && state.input.trim_start().starts_with('/') =>
        {
            if state.move_slash_completion_selection(1) {
                state.history_idx = None;
            }
        }
        (KeyCode::Up, _) => {
            if state.input.contains('\n') || state.history.is_empty() {
                return;
            }
            let idx = match state.history_idx {
                None => state.history.len().saturating_sub(1),
                Some(i) => i.saturating_sub(1),
            };
            if let Some(prev) = state.history.get(idx) {
                state.input = prev.clone();
                state.cursor = state.input.len();
                state.history_idx = Some(idx);
                state.reset_slash_completion_selection();
            }
        }
        (KeyCode::Down, _) => {
            if state.input.contains('\n') {
                return;
            }
            let Some(i) = state.history_idx else {
                return;
            };
            if i + 1 < state.history.len() {
                state.input = state.history[i + 1].clone();
                state.cursor = state.input.len();
                state.history_idx = Some(i + 1);
                state.reset_slash_completion_selection();
            } else {
                state.input.clear();
                state.cursor = 0;
                state.history_idx = None;
                state.clear_slash_completion_selection();
                state.input_display_override = None;
            }
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.input.insert(state.cursor, c);
            state.cursor += c.len_utf8();
            state.reset_slash_completion_selection();
        }
        _ => {}
    }
}

struct TerminalGuard {
    active: bool,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        crate::session::set_tui_active(true);

        let mut out = io::stdout();
        if let Err(err) = crossterm::execute!(
            out,
            EnableBracketedPaste,
            crossterm::cursor::SetCursorStyle::SteadyBlock,
            crossterm::cursor::Show
        ) {
            crate::session::restore_terminal_if_tui();
            return Err(err);
        }

        out.flush()?;
        Ok(Self { active: true })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            crate::session::restore_terminal_if_tui();
            self.active = false;
        }
    }
}

pub async fn run(mut agent: Agent, initial_task: Option<String>) -> Result<()> {
    let model = agent.model.clone();
    let sandbox = agent.sandbox_root.display().to_string();
    let approval_profile = agent.approval_profile();
    let thinking_effort = agent.thinking_effort();
    let _guard = TerminalGuard::new()?;
    let stdout = io::stdout();
    let backend = CrosstermBackend::new(stdout);
    let mut terminal = Terminal::with_options(
        backend,
        TerminalOptions {
            viewport: Viewport::Inline(VIEWPORT_HEIGHT),
        },
    )?;

    let (ev_tx, mut ev_rx) = tokio::sync::mpsc::unbounded_channel::<ToTui>();
    let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<FromTui>();
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

    let interrupt = agent.interrupt.clone();
    agent.set_sink(Box::new(TuiSink { tx: ev_tx }));
    agent.pretty = false;
    agent.silent = false;

    // Keyboard reader thread — store handle so we can signal shutdown
    let (key_kill_tx, key_kill_rx) = std::sync::mpsc::channel::<()>();
    let key_handle = std::thread::spawn(move || {
        loop {
            if key_kill_rx.try_recv().is_ok() {
                break;
            }
            if cterm_event::poll(Duration::from_millis(80)).unwrap_or(false)
                && let Ok(ev) = cterm_event::read()
                && key_tx.send(ev).is_err()
            {
                break;
            }
        }
    });

    let mut state = TuiState::new(model, sandbox, approval_profile, thinking_effort);
    let mode_label = match state.approval_profile {
        ApprovalProfile::Always => "trust",
        ApprovalProfile::Ask => "guarded",
        profile => profile.as_str(),
    };
    let banner = format!(
        "🐺  Dext v{}\nsandbox  {}\nmodel    {}\nmode     {}\nreason   {}\nkeys     Ctrl+C/Esc interrupt · Ctrl+D quit · ? help",
        env!("CARGO_PKG_VERSION"),
        clamp_chars(&state.sandbox, 96),
        clamp_chars(&state.model, 40),
        mode_label,
        state.thinking_effort.as_str(),
    );
    state.queue(Line_::Banner(banner));
    if state.approval_profile == ApprovalProfile::Always {
        state.queue(Line_::Warn(
            "trust mode is active: privileged tools skip confirmation prompts".to_string(),
        ));
    }
    if let Some(task) = initial_task {
        state.queue(Line_::User(task.clone()));
        let _ = in_tx.send(FromTui::Submit(task));
    }

    // Move agent into a task; communicate via channels
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<FromTui>();
    let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let direct_steer_tx = steer_tx.clone();
    agent.install_steering(steer_rx, steer_tx);
    let handle = tokio::spawn(async move {
        while let Some(cmd) = cmd_rx.recv().await {
            match cmd {
                FromTui::Submit(text) => {
                    if text.starts_with('/') {
                        let trimmed = text.trim();
                        if let Some(parsed) = parse_compact_slash(trimmed) {
                            match parsed {
                                Ok(crate::CompactSlash::RunNow) => {
                                    let _ = agent.compact().await;
                                }
                                Ok(crate::CompactSlash::Status) => {
                                    let current = agent.compact_threshold_chars();
                                    let base = history_char_budget_with_override(
                                        &agent.model,
                                        None,
                                        agent.context_mode,
                                    );
                                    match agent.compact_threshold_override_percent() {
                                        Some(percent) => agent.sink.emit(AgentEvent::Slash(format!(
                                            "compact threshold: {current} chars ({percent}% of model context window; auto baseline {base})"
                                        ))),
                                        None => agent.sink.emit(AgentEvent::Slash(format!(
                                            "compact threshold: {current} chars (auto: {} mode)",
                                            agent.context_mode.as_str()
                                        ))),
                                    }
                                }
                                Ok(crate::CompactSlash::Auto) => {
                                    agent.set_compact_threshold_auto();
                                    agent.sink.emit(AgentEvent::Slash(format!(
                                        "compact threshold reset to auto {} ({})",
                                        agent.context_mode.as_str(),
                                        agent.compact_threshold_chars()
                                    )));
                                }
                                Ok(crate::CompactSlash::SetPercent(percent)) => {
                                    let chars = agent.set_compact_threshold_percent(percent);
                                    agent.sink.emit(AgentEvent::Slash(format!(
                                        "compact threshold set to {percent}% -> {chars} chars"
                                    )));
                                }
                                Err(msg) => agent.sink.emit(AgentEvent::Slash(msg.to_string())),
                            }
                        } else if trimmed == "/plan" || trimmed.starts_with("/plan ") {
                            let task = trimmed.strip_prefix("/plan").unwrap_or("").trim();
                            if task.is_empty() {
                                agent
                                    .sink
                                    .emit(AgentEvent::Slash("usage: /plan <task>".into()));
                            } else if let Err(e) = agent.run_plan(task.to_string()).await {
                                agent
                                    .sink
                                    .emit(AgentEvent::Error(format!("[plan error] {e:#}")));
                            }
                        } else if trimmed == "/subagent" || trimmed.starts_with("/subagent ") {
                            let raw = trimmed.strip_prefix("/subagent").unwrap_or("").trim();
                            if raw.is_empty() {
                                agent.sink.emit(AgentEvent::Slash(
                                    "usage: /subagent <task> [--tools t1,t2] [--max-iter N] [--system PROMPT] [--readonly] [--inline|--detached]\n       /subagent steer <message>".into(),
                                ));
                            } else if raw.starts_with("steer ") || raw == "steer" {
                                let msg = raw.strip_prefix("steer").unwrap_or("").trim();
                                if msg.is_empty() {
                                    agent.sink.emit(AgentEvent::Slash(
                                        "usage: /subagent steer <message>".into(),
                                    ));
                                } else if let Err(e) =
                                    agent.steer_detached_subagent(msg.to_string()).await
                                {
                                    agent
                                        .sink
                                        .emit(AgentEvent::Error(format!("[steer error] {e:#}")));
                                }
                            } else if let Err(e) = agent.run_subagent_cmd(raw.to_string()).await {
                                agent
                                    .sink
                                    .emit(AgentEvent::Error(format!("[subagent error] {e:#}")));
                            }
                        } else if trimmed == "/pack"
                            || trimmed.starts_with("/pack ")
                            || trimmed == "/packs"
                            || trimmed.starts_with("/packs ")
                        {
                            let raw = trimmed
                                .trim_start_matches("/packs")
                                .trim_start_matches("/pack")
                                .trim();
                            let mut parts = raw.splitn(3, char::is_whitespace);
                            let sub = parts.next().unwrap_or("");
                            if matches!(sub, "run" | "use" | "start") {
                                let selector = parts.next().unwrap_or("").trim();
                                let task = parts.next().unwrap_or("").trim();
                                if selector.is_empty() || task.is_empty() {
                                    agent.sink.emit(AgentEvent::Slash(
                                        "usage: /pack run <name> <task>".into(),
                                    ));
                                } else if let Err(e) = agent.run_pack(selector, task).await {
                                    agent
                                        .sink
                                        .emit(AgentEvent::Error(format!("[pack error] {e:#}")));
                                }
                            } else if !run_slash(&text, &mut agent) {
                                break;
                            }
                        } else if !run_slash(&text, &mut agent) {
                            break;
                        }
                    } else {
                        match agent.try_consume_pending_login_input(&text) {
                            Ok(Some(msg)) => agent.sink.emit(AgentEvent::Slash(msg)),
                            Ok(None) => {
                                if let Err(e) = agent.chat(text).await {
                                    agent.sink.emit(AgentEvent::Error(format!("[error] {e:#}")));
                                }
                            }
                            Err(e) => {
                                agent
                                    .sink
                                    .emit(AgentEvent::Error(format!("[login error] {e:#}")));
                            }
                        }
                    }
                    agent.checkpoint_latest_session("outer_loop_autosave");
                }
                FromTui::CycleEffort(step) => {
                    let effort = agent.cycle_thinking_effort(step);
                    agent
                        .sink
                        .emit(AgentEvent::ThinkingEffortChanged { effort });
                    agent.sink.emit(AgentEvent::Slash(format!(
                        "thinking effort -> {} (model reasoning depth/tool persistence)",
                        effort.as_str()
                    )));
                }
                FromTui::Quit => break,
            }
        }
    });

    // Bridge: relay in_rx → cmd_tx
    let bridge_cmd_tx = cmd_tx.clone();
    let bridge = tokio::spawn(async move {
        while let Some(c) = in_rx.recv().await {
            if bridge_cmd_tx.send(c).is_err() {
                break;
            }
        }
    });

    let tick = Duration::from_millis(80);
    let mut last_tick = Instant::now();

    while !state.quit {
        state.refresh_git_branch();
        flush_pending_insert(&mut terminal, &mut state)?;
        terminal.draw(|f| draw(f, &mut state))?;
        state.frame_count = state.frame_count.wrapping_add(1);
        let timeout = tick
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        tokio::select! {
            biased;
            maybe_ev = ev_rx.recv() => {
                match maybe_ev {
                    Some(ToTui::Event(ev)) => state.apply_event(ev),
                    Some(ToTui::PermissionRequest { name, input, responder }) => {
                        queue_permission_request(&mut state, name, input, responder);
                    }
                    None => {}
                }
            }
            maybe_key = key_rx.recv() => {
                if let Some(ev) = maybe_key {
                    match ev {
                        Event::Key(k) => handle_key(&mut state, k, &in_tx, &direct_steer_tx, &interrupt),
                        Event::Mouse(mouse) => handle_mouse(&mut state, mouse),
                        Event::Paste(pasted) => handle_paste(&mut state, pasted),
                        _ => {}
                    }
                }
            }
            _ = tokio::time::sleep(timeout) => {
                last_tick = Instant::now();
                state.poll_detached_subagent();
            }
        }
    }

    interrupt.store(true, Ordering::SeqCst);

    // Shut down channels in the right order to avoid deadlock:
    // 1. Drop in_tx so the bridge's in_rx.recv() returns None
    // 2. Send Quit to agent via cmd_tx so it can break out of chat()
    // 3. Drop cmd_tx so cmd_rx sees channel closed
    drop(in_tx);
    let _ = cmd_tx.send(FromTui::Quit);
    drop(cmd_tx);

    // Graceful shutdown with a hard deadline.  If anything hangs, the
    // deadline fires and we still restore the terminal before exiting.
    let mut handle = handle;
    let graceful = tokio::time::timeout(Duration::from_secs(2), async {
        let _ = bridge.await;
        tokio::select! {
            _ = &mut handle => {}
            _ = tokio::time::sleep(Duration::from_millis(800)) => {
                handle.abort();
                let _ = handle.await;
            }
        }
    })
    .await;
    if graceful.is_err() {
        // timeout fired; handle was consumed by the async block so we
        // can't abort it here, but the process is exiting anyway.
    }

    // Drain remaining events quickly so final output renders
    while let Ok(ev) = ev_rx.try_recv() {
        if let ToTui::Event(e) = ev {
            state.apply_event(e);
        }
    }
    let _ = flush_pending_insert(&mut terminal, &mut state);
    let _ = terminal.draw(|f| draw(f, &mut state));

    drop(key_rx);
    let _ = key_kill_tx.send(());
    // key reader thread should exit within one poll cycle (80 ms)
    let _ = key_handle.join();

    let _ = terminal.clear();
    {
        let mut out = io::stdout();
        let _ = crossterm::execute!(
            out,
            crossterm::cursor::MoveToColumn(0),
            crossterm::cursor::Show
        );
        let _ = out.flush();
    }

    drop(terminal);
    drop(_guard);
    Ok(())
}

fn run_slash(line: &str, agent: &mut Agent) -> bool {
    let trimmed = line.trim();
    if trimmed == "/quit" || trimmed == "/exit" {
        return false;
    }
    handle_slash(line, agent).unwrap_or(true)
}

#[cfg(test)]
pub(crate) fn steering_delivered_text_for_test(
    messages: usize,
    preview: &str,
    width: u16,
) -> Text<'static> {
    let item = Line_::SteeringDelivered {
        messages,
        preview: preview.to_string(),
    };
    line_to_text(&item, width)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::StoredCredential;

    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        crate::test_env_lock().lock().expect("env lock")
    }

    fn tool_line(
        call_tag: &str,
        name: &str,
        summary: &str,
        ok: Option<bool>,
        content: &str,
    ) -> Line_ {
        Line_::Tool {
            call_tag: call_tag.to_string(),
            name: name.to_string(),
            summary: summary.to_string(),
            ok,
            content: content.to_string(),
            group_count: 1,
            group_lines: content.lines().count(),
            group_chunks: vec![ToolChunk {
                call_tag: call_tag.to_string(),
                summary: summary.to_string(),
                content: content.to_string(),
            }],
            duration_secs: 0,
            denied: false,
            dim: false,
            density_rank: 1,
            expanded: false,
        }
    }

    fn flatten_lines(text: &Text<'_>) -> Vec<String> {
        text.lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect()
    }

    fn span_style_for<'a>(text: &'a Text<'a>, needle: &str) -> Option<Style> {
        text.lines
            .iter()
            .flat_map(|line| line.spans.iter())
            .find(|span| span.content.contains(needle))
            .map(|span| span.style)
    }

    #[test]
    fn sanitize_display_text_normalizes_carriage_returns_and_controls() {
        let sanitized = sanitize_display_text("alpha\rbravo\r\ncharlie\u{0007}\tend");
        assert_eq!(sanitized, "alpha\nbravo\ncharlie\tend");
    }

    #[test]
    fn user_cards_pad_body_lines_so_right_border_stays_clear() {
        let text = line_to_text(&Line_::User("- line one\n- line two".to_string()), 120);
        let lines = flatten_lines(&text);
        let body = lines
            .iter()
            .filter(|line| line.starts_with("│ "))
            .cloned()
            .collect::<Vec<_>>();
        assert!(body.len() >= 2, "body lines: {body:?}");
        assert!(
            body.iter().all(|line| line.starts_with("│ ")),
            "body lines: {body:?}"
        );
        assert!(
            body.iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 120),
            "body lines: {body:?}"
        );
    }

    #[test]
    fn permission_prompt_renders_two_border_lines() {
        let text = permission_prompt_text(
            "bash",
            "echo $DEXT_MODEL",
            crate::tool_policy::CommandRisk::Read,
            80,
        );
        let lines = flatten_lines(&text);
        assert_eq!(lines.len(), 2);
        assert!(lines.iter().all(|line| line.starts_with("▌ ")));
        assert!(lines[0].contains("ask bash"));
        assert!(lines[0].contains("risk=read"));
        assert!(lines[0].contains("echo $DEXT_MODEL"));
        assert!(lines[1].contains("[y] once"));
        assert!(lines[1].contains("[a] always"));
        assert!(lines[1].contains("[n] deny"));
        assert!(!lines.iter().any(|line| line.contains("why")));
    }

    #[test]
    fn permission_prompt_border_color_tracks_tier() {
        let read = permission_prompt_text("bash", "pwd", crate::tool_policy::CommandRisk::Read, 80);
        let write = permission_prompt_text(
            "write_file",
            "src/tui.rs",
            crate::tool_policy::CommandRisk::Write,
            80,
        );
        let danger = permission_prompt_text(
            "bash",
            "sudo rm -rf /tmp/x",
            crate::tool_policy::CommandRisk::Danger,
            80,
        );

        let read_style = span_style_for(&read, "▌ ").expect("read border");
        let write_style = span_style_for(&write, "▌ ").expect("write border");
        let danger_style = span_style_for(&danger, "▌ ").expect("danger border");

        assert_eq!(read_style.fg, Some(Color::Yellow));
        assert_eq!(write_style.fg, Some(Color::Yellow));
        assert_eq!(danger_style.fg, Some(Color::Red));
    }

    #[test]
    fn input_hint_switches_to_permission_response_text() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.pending_perm = Some(PendingPermission {
            tool: "bash".to_string(),
            audit_label: "echo $DEXT_MODEL".to_string(),
            tier: PermissionTier::Read,
            responder: std::sync::mpsc::sync_channel(0).0,
        });

        assert_eq!(input_hint_text(&state), "  press y / a / n to respond");
    }

    #[test]
    fn busy_input_hint_warns_about_local_auth_secrets() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;

        assert!(input_hint_text(&state).contains("local auth secrets are withheld"));
    }

    #[test]
    fn pending_permission_does_not_render_live_indicator_or_busy_status() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.pending_perm = Some(PendingPermission {
            tool: "bash".to_string(),
            audit_label: "echo $DEXT_MODEL".to_string(),
            tier: PermissionTier::Read,
            responder: std::sync::mpsc::sync_channel(0).0,
        });

        assert!(transcript_live_indicator_text(&state, 80).is_none());
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(!rendered.contains("awaiting permission"));
    }

    #[test]
    fn status_spans_do_not_render_trust_indicator() {
        let state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Always,
            ThinkingEffort::Medium,
        );
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();

        assert!(rendered.starts_with("● ."), "{rendered}");
        assert!(!rendered.contains("trust●"), "{rendered}");
        assert_eq!(input_border_style(&state).fg, Some(TRUST_INPUT_BORDER));
    }

    #[test]
    fn approval_profile_changed_updates_trust_input_border() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(!rendered.contains("trust●"), "{rendered}");
        assert_eq!(input_border_style(&state).fg, Some(Color::DarkGray));

        state.apply_event(AgentEvent::ApprovalProfileChanged {
            profile: ApprovalProfile::Always,
        });
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(!rendered.contains("trust●"), "{rendered}");
        assert_eq!(input_border_style(&state).fg, Some(TRUST_INPUT_BORDER));

        state.apply_event(AgentEvent::ApprovalProfileChanged {
            profile: ApprovalProfile::Ask,
        });
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(!rendered.contains("trust●"), "{rendered}");
        assert_eq!(input_border_style(&state).fg, Some(Color::DarkGray));
    }

    #[test]
    fn live_indicator_is_capped_to_two_lines() {
        let lines = cap_live_indicator_lines(vec![
            Line::from("one"),
            Line::from("two"),
            Line::from("three"),
        ]);
        let rendered = Text::from(lines);
        assert_eq!(flatten_lines(&rendered), vec!["two", "three"]);
    }

    #[test]
    fn transcript_live_indicator_shows_status_elapsed_and_stream_tail() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.frame_count = 1;
        state.stream_started_at = Some(Instant::now() - Duration::from_secs(12));
        state.streaming_text = "first line\nfinal streamed line".to_string();

        let text = transcript_live_indicator_text(&state, 80).expect("live indicator");
        let lines = flatten_lines(&text);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("responding"));
        assert!(lines[0].contains("12s"));
        assert!(lines[1].contains("final streamed line"));
    }

    #[test]
    fn todo_progress_surfaces_active_task_when_idle() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.todo_progress = todo_progress_from_content(
            "✓ done [completed]\n► improve live indicator [in_progress]\n○ wait [pending]\n",
        );

        let text = transcript_live_indicator_text(&state, 80).expect("live indicator");
        let lines = flatten_lines(&text);
        assert!(
            lines[1].contains("[1/3 todos done · active: improve live indicator]"),
            "{lines:?}"
        );
    }

    #[test]
    fn live_indicator_prefers_rolling_activity_over_todo_count() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.todo_progress = todo_progress_from_content(
            "► improve live indicator [in_progress]\n○ verify [pending]\n",
        );
        state.streaming_thinking = "considering options\nchoosing display priority".to_string();

        let text = transcript_live_indicator_text(&state, 80).expect("live indicator");
        let lines = flatten_lines(&text);
        assert!(lines[1].contains("choosing display priority"), "{lines:?}");
        assert!(!lines[1].contains("todos done"), "{lines:?}");
    }

    #[test]
    fn transcript_live_indicator_shows_tool_summary_when_no_stream_text() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.live_tools.push(LiveTool {
            call_id: "call-1".to_string(),
            call_tag: "#1.1".to_string(),
            name: "bash".to_string(),
            summary: "bash: cargo test --release".to_string(),
            running: true,
            started: Some(Instant::now() - Duration::from_secs(5)),
            is_subagent: false,
        });

        let text = transcript_live_indicator_text(&state, 80).expect("live indicator");
        let lines = flatten_lines(&text);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("running bash"));
        assert!(lines[0].contains("5s"));
        assert!(lines[1].contains("cargo test --release"));
    }

    #[test]
    fn rg_long_lines_are_middle_truncated_before_wrapping() {
        let line = format!("src/app.css:12:{}MATCH{}", "a".repeat(300), "z".repeat(300));
        let truncated = middle_truncate_rg_line(&line, 120);
        assert!(truncated.starts_with("src/app.css:12:"), "{truncated}");
        assert!(truncated.contains(" … "), "{truncated}");
        assert!(truncated.ends_with(&"z".repeat(58)), "{truncated}");
        assert!(text_width(&truncated) <= 120, "{truncated}");
    }

    #[test]
    fn turn_tool_summary_breakdown_is_exhaustive_and_singularizes() {
        let counts = HashMap::from([
            ("read_file".to_string(), 40),
            ("read_symbol".to_string(), 3),
            ("rg".to_string(), 20),
            ("fd".to_string(), 4),
            ("edit_file".to_string(), 8),
            ("multi_edit".to_string(), 1),
            ("bash".to_string(), 20),
            ("write_file".to_string(), 1),
            ("git_diff".to_string(), 6),
            ("git_log".to_string(), 2),
            ("git_commit".to_string(), 1),
            ("todo_read".to_string(), 2),
            ("todo_write".to_string(), 3),
            ("jq".to_string(), 1),
            ("http".to_string(), 2),
            ("mystery".to_string(), 1),
        ]);

        let (total, summary) = turn_tool_summary(&counts).expect("summary");

        assert_eq!(total, 115);
        assert!(summary.contains("43 reads"), "{summary}");
        assert!(summary.contains("20 searches"), "{summary}");
        assert!(summary.contains("4 finds"), "{summary}");
        assert!(summary.contains("9 edits"), "{summary}");
        assert!(summary.contains("20 commands"), "{summary}");
        assert!(summary.contains("1 write"), "{summary}");
        assert!(summary.contains("9 git ops"), "{summary}");
        assert!(summary.contains("5 todo ops"), "{summary}");
        assert!(summary.contains("1 data op"), "{summary}");
        assert!(summary.contains("2 requests"), "{summary}");
        assert!(summary.contains("1 other call"), "{summary}");
    }

    #[test]
    fn turn_end_uses_same_total_as_displayed_breakdown() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::TurnStart);
        for (idx, tool) in [
            "read_file",
            "read_symbol",
            "rg",
            "fd",
            "edit_file",
            "multi_edit",
            "bash",
            "write_file",
            "git_diff",
            "git_log",
            "git_commit",
            "todo_read",
            "todo_write",
            "jq",
            "awk",
            "csvkit",
            "http",
            "unknown_tool",
        ]
        .iter()
        .enumerate()
        {
            state.apply_event(AgentEvent::ToolCallResult {
                call_id: format!("call-{idx}"),
                name: tool.to_string(),
                ok: true,
                preview: String::new(),
                content: String::new(),
            });
        }

        state.apply_event(AgentEvent::TurnEnd {
            usage: Usage::default(),
        });

        let summary = state
            .pending_insert
            .iter()
            .rev()
            .find_map(|line| match line {
                Line_::Info(msg) if msg.starts_with("Turn used ") => Some(msg.as_str()),
                _ => None,
            })
            .expect("turn summary");
        assert!(
            summary.starts_with("Turn used 18 tool calls ("),
            "{summary}"
        );
        assert!(summary.contains("2 reads"), "{summary}");
        assert!(summary.contains("1 write"), "{summary}");
        assert!(summary.contains("3 git ops"), "{summary}");
        assert!(summary.contains("2 todo ops"), "{summary}");
        assert!(summary.contains("3 data ops"), "{summary}");
        assert!(summary.contains("1 request"), "{summary}");
        assert!(summary.contains("1 other call"), "{summary}");
        assert!(summary.ends_with(" · no errors"), "{summary}");
    }

    #[test]
    fn completed_thinking_is_visible_by_default_before_next_event() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.apply_event(AgentEvent::ThinkingDelta(
            "I need to inspect the implementation".to_string(),
        ));
        state.apply_event(AgentEvent::ThinkingBlockComplete(
            "I need to inspect the implementation before using tools".to_string(),
        ));
        state.apply_event(AgentEvent::ToolCallResult {
            call_id: "call_1".to_string(),
            name: "rg".to_string(),
            ok: true,
            preview: "rg: thinking".to_string(),
            content: "matched line".to_string(),
        });

        assert!(matches!(
            state.pending_insert.as_slice(),
            [Line_::Thinking(_), Line_::Blank, Line_::Tool { name, .. }] if name == "rg"
        ));
    }

    #[test]
    fn inserts_blank_between_thinking_history_and_next_tool() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.apply_event(AgentEvent::ThinkingBlockComplete(
            "thinking history".to_string(),
        ));
        state.apply_event(AgentEvent::ToolCallResult {
            call_id: "call_1".to_string(),
            name: "read_symbol".to_string(),
            ok: true,
            preview: "read_symbol: src/main.rs".to_string(),
            content: "line".to_string(),
        });

        assert!(matches!(
            state.pending_insert.as_slice(),
            [Line_::Thinking(_), Line_::Blank, Line_::Tool { name, .. }] if name == "read_symbol"
        ));
        assert_eq!(
            flatten_lines(&line_to_text(&Line_::Blank, 80)),
            vec![String::new()]
        );
    }

    #[test]
    fn inserts_blank_between_tool_and_next_completed_thinking() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.apply_event(AgentEvent::ToolCallResult {
            call_id: "call_1".to_string(),
            name: "bash".to_string(),
            ok: true,
            preview: "bash: echo ok".to_string(),
            content: "exit: 0".to_string(),
        });
        state.apply_event(AgentEvent::ThinkingBlockComplete(
            "Considering the next step".to_string(),
        ));

        assert!(matches!(
            state.pending_insert.as_slice(),
            [Line_::Tool { name, .. }, Line_::Blank, Line_::Thinking(_)] if name == "bash"
        ));
    }

    #[test]
    fn work_map_packet_still_renders_in_transcript() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Packet,
            text: "[dext packet @w01]\nsource: current".to_string(),
            waypoint_ids: vec!["@w01".to_string()],
            selector: None,
        });

        assert!(!state.work_map_is_active());
        assert!(matches!(
            state.pending_insert.last(),
            Some(Line_::WorkMap { .. })
        ));
        let lines = flatten_lines(&line_to_text(state.pending_insert.last().unwrap(), 80));
        assert!(
            lines.iter().any(|line| line.contains("work packet")),
            "{lines:?}"
        );
    }

    #[test]
    fn work_map_event_opens_input_drawer_and_keyboard_inserts_commands() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Map,
            text: "Work map — current\n@w01 intent #1  first\n@w02 change #2  second\ncommands: /packet @wNN".to_string(),
            waypoint_ids: vec!["@w01".to_string(), "@w02".to_string(), "@w99".to_string()],
            selector: None,
        });

        assert!(state.work_map_is_active());
        assert!(state.pending_insert.is_empty());
        let lines = flatten_lines(&Text::from(work_map_drawer_lines(&mut state, 80, 4)));
        assert!(
            lines.iter().any(|line| line.contains("Work Map")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("▶ @w01")),
            "{lines:?}"
        );

        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
        );
        let lines = flatten_lines(&Text::from(work_map_drawer_lines(&mut state, 80, 4)));
        assert!(
            lines.iter().any(|line| line.starts_with("▶ @w02")),
            "{lines:?}"
        );
        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('p'), KeyModifiers::empty()),
        );

        assert_eq!(state.input, "/packet @w02");
        assert!(!state.work_map_is_active());

        state.input.clear();
        state.cursor = 0;
        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Map,
            text: "Work map — old-session\n@w01 intent #1  first".to_string(),
            waypoint_ids: vec!["@w01".to_string()],
            selector: Some("old-session".to_string()),
        });
        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
        );
        assert_eq!(state.input, "/focus old-session @w01");
    }

    #[test]
    fn completed_thinking_can_be_hidden() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.verbose = false;

        state.apply_event(AgentEvent::ThinkingDelta(
            "I need to inspect the implementation".to_string(),
        ));
        state.apply_event(AgentEvent::ThinkingBlockComplete(
            "I need to inspect the implementation before using tools".to_string(),
        ));
        state.apply_event(AgentEvent::ToolCallResult {
            call_id: "call_1".to_string(),
            name: "rg".to_string(),
            ok: true,
            preview: "rg: thinking".to_string(),
            content: "matched line".to_string(),
        });

        assert!(matches!(
            state.pending_insert.as_slice(),
            [Line_::Tool { name, .. }] if name == "rg"
        ));
        assert!(state.streaming_thinking.is_empty());
    }

    #[test]
    fn thinking_block_wraps_inside_available_width() {
        let text = line_to_text(
            &Line_::Thinking("**Logging decisions and findings** ".repeat(12)),
            32,
        );
        let lines = flatten_lines(&text);
        assert!(lines.len() > 1, "thinking block should wrap: {lines:?}");
        assert!(
            lines
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 32),
            "thinking block lines must stay inside transcript width: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .skip(1)
                .all(|line| line.starts_with("│ ") || line.starts_with("  ")),
            "wrapped thinking lines should keep an internal lane prefix: {lines:?}"
        );
    }

    #[test]
    fn transcript_render_width_reserves_guard_column() {
        assert_eq!(transcript_render_width(0), 1);
        assert_eq!(transcript_render_width(1), 1);
        assert_eq!(transcript_render_width(80), 79);
    }

    #[test]
    fn live_thinking_indicator_uses_full_width() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.streaming_thinking = "x".repeat(80);

        let text = transcript_live_indicator_text(&state, 80).expect("live indicator");
        let lines = flatten_lines(&text);

        assert!(lines[0].contains("Thinking"), "live status: {lines:?}");
        assert!(
            !lines[0].contains("  thinking"),
            "live thinking status should be capitalized: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .any(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) == 80),
            "live indicator should be allowed to fill/break at terminal border: {lines:?}"
        );
        let style = span_style_for(&text, "xxx").expect("streaming thinking detail");
        assert_eq!(style.bg, Some(THINKING_BG));
    }

    #[test]
    fn cached_thinking_render_reserves_terminal_wrap_guard() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let item = Line_::Thinking("**Addressing user frustration** ".repeat(8));
        let (text, _) = cached_transcript_render(&mut state, &item, 32);
        let lines = flatten_lines(&text);
        assert!(
            lines
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 31),
            "thinking render must leave a guard column for terminal auto-wrap: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| line.starts_with("│ ")),
            "wrapped thinking lines should stay in the internal lane: {lines:?}"
        );
    }

    #[test]
    fn thinking_history_has_muted_background_without_header_spacer() {
        let text = line_to_text(&Line_::Thinking("checking the next step".to_string()), 80);
        let body_style = span_style_for(&text, "checking").expect("thinking body");
        let lines = flatten_lines(&text);

        assert_eq!(body_style.bg, Some(THINKING_BG));
        assert_eq!(body_style.fg, Some(Color::Gray));
        assert_eq!(
            lines.first().map(String::as_str),
            Some("│ checking the next step")
        );
        assert!(
            lines.iter().all(|line| !line.contains('▸')),
            "thinking marker should be hidden without adding a spacer row: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.contains("thinking (")),
            "thinking word-count title should stay hidden without adding a spacer row: {lines:?}"
        );
    }

    #[test]
    fn steering_history_has_distinct_highlight_and_lane() {
        let text = line_to_text(
            &Line_::Steering("wolf = dext my bad. old names.".to_string()),
            80,
        );
        let lines = flatten_lines(&text);
        let body_style = span_style_for(&text, "wolf = dext").expect("steering body");
        let prefix_style = span_style_for(&text, ">>").expect("steering prefix");

        assert_eq!(
            lines.first().map(String::as_str),
            Some(">> wolf = dext my bad. old names.")
        );
        assert_eq!(body_style.bg, Some(STEERING_BG));
        assert_eq!(prefix_style.bg, Some(STEERING_BG));
        assert_ne!(body_style.bg, Some(THINKING_BG));
        assert_eq!(body_style.fg, Some(Color::LightMagenta));
    }

    #[test]
    fn thinking_body_wraps_on_word_boundaries_with_stable_lane_prefix() {
        let text = line_to_text(
            &Line_::Thinking("I need to keep working on fixing Clippy. It seems like I should read the SessionHeader struct and check its default nested provenance. I might be able to patch it directly, but I should inspect the imports too.".to_string()),
            74,
        );
        let lines = flatten_lines(&text);

        assert!(
            lines.iter().all(|line| line.starts_with("│ ")),
            "thinking body lines should all stay in the vertical lane: {lines:?}"
        );
        assert!(
            lines.iter().all(|line| !line.starts_with("  ")),
            "thinking body should not create hanging indent continuation rows: {lines:?}"
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.starts_with("│ ruct") || line.starts_with("│ d ")),
            "thinking body should avoid mid-word wrap fragments like the reported UX issue: {lines:?}"
        );
    }

    #[test]
    fn repeated_assistant_prefix_is_dimmed_after_first_response() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.apply_event(AgentEvent::TextBlockComplete("first".to_string()));
        state.apply_event(AgentEvent::TextBlockComplete("second".to_string()));

        assert!(matches!(
            state.pending_insert.as_slice(),
            [
                Line_::Assistant {
                    dim_prefix: false,
                    ..
                },
                Line_::Assistant {
                    dim_prefix: true,
                    ..
                }
            ]
        ));
        let text = line_to_text(state.pending_insert.last().unwrap(), 80);
        let style = span_style_for(&text, "dext").expect("dext label");
        assert_eq!(style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn merge_consecutive_read_file_tracks_line_span_and_coverage() {
        let items = merge_consecutive_tools(vec![
            tool_line(
                "#1.44",
                "read_file",
                "read_file: src/main.rs (offset=6410, limit=2)",
                Some(true),
                "6410\talpha\n6411\tbravo\n",
            ),
            tool_line(
                "#1.45",
                "read_file",
                "read_file: src/main.rs (offset=6412, limit=2)",
                Some(true),
                "6412\tcharlie\n6413\tdelta\n",
            ),
        ]);

        let Line_::Tool {
            summary,
            group_count,
            group_lines,
            ..
        } = &items[0]
        else {
            panic!("expected grouped tool");
        };
        assert_eq!(*group_count, 2);
        assert_eq!(*group_lines, 4);
        assert!(
            summary.contains("src/main.rs (2 reads, 4 lines inspected"),
            "{summary}"
        );
        assert!(summary.contains("lines 6410-6413"), "{summary}");
    }

    #[test]
    fn density_separator_renders_every_tenth_tool_call() {
        let mut item = tool_line("#1.10", "bash", "bash: true", Some(true), "ok");
        if let Line_::Tool { density_rank, .. } = &mut item {
            *density_rank = 10;
        }
        let lines = flatten_lines(&line_to_text(&item, 100));
        assert!(
            lines
                .first()
                .is_some_and(|line| line.contains("tool call 10")),
            "{lines:?}"
        );
    }

    #[test]
    fn tool_row_dedupes_summary_name_prefix() {
        let item = tool_line(
            "#1.2",
            "read_file",
            "read_file: src/tui.rs (limit=20)",
            Some(true),
            "1\tfn main() {}\n",
        );
        let text = line_to_text(&item, 120);
        let lines = flatten_lines(&text);
        assert!(
            lines
                .iter()
                .any(|line| line.contains("#1.2 read_file: src/tui.rs (limit=20)"))
        );
        assert!(
            !lines
                .iter()
                .any(|line| line.contains("read_file read_file:"))
        );
    }

    #[test]
    fn compact_end_after_idle_manual_compact_marks_ready() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.apply_event(AgentEvent::CompactStart);
        assert!(state.agent_busy);
        assert_eq!(derived_busy_status(&state), "compacting history");

        state.apply_event(AgentEvent::CompactEnd {
            before: 20,
            after: 4,
        });

        assert!(!state.agent_busy);
        assert_eq!(state.status, "ready");
        assert!(transcript_live_indicator_text(&state, 80).is_none());
    }

    #[test]
    fn compact_end_during_turn_resumes_busy_status() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;

        state.apply_event(AgentEvent::CompactStart);
        state.apply_event(AgentEvent::CompactEnd {
            before: 20,
            after: 4,
        });

        assert!(state.agent_busy);
        assert_eq!(derived_busy_status(&state), "thinking");
    }

    #[test]
    fn slash_event_clears_live_preview_state() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.status = "scroll: live".to_string();
        state.stream_started_at = Some(Instant::now());
        state.stream_chars = 42;
        state.streaming_text = "stale answer".to_string();
        state.streaming_thinking = "thinking".to_string();
        state.live_tools.push(LiveTool {
            call_id: "call-1".to_string(),
            call_tag: "#1.1".to_string(),
            name: "bash".to_string(),
            summary: "bash: pwd".to_string(),
            running: true,
            started: Some(Instant::now()),
            is_subagent: false,
        });

        state.apply_event(AgentEvent::Slash("ok".to_string()));

        assert!(!state.agent_busy);
        assert_eq!(state.status, "ready");
        assert!(state.streaming_text.is_empty());
        assert!(state.streaming_thinking.is_empty());
        assert!(state.stream_started_at.is_none());
        assert_eq!(state.stream_chars, 0);
        assert!(state.live_tools.is_empty());
        assert!(
            state
                .pending_insert
                .last()
                .is_some_and(|line| matches!(line, Line_::Info(msg) if msg == "ok"))
        );
    }

    #[test]
    fn user_cards_render_markdown() {
        let text = line_to_text(&Line_::User("# Heading\n- item".to_string()), 120);
        let lines = flatten_lines(&text);
        assert!(lines.iter().any(|line| line.contains("you")));
        let heading_style = span_style_for(&text, "Heading").expect("heading span");
        assert_eq!(heading_style.fg, Some(Color::LightCyan));
        assert!(matches!(heading_style.bg, None | Some(Color::Reset)));
        assert!(heading_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn set_last_tool_expanded_toggles_matching_tool_in_place() {
        let mut items = vec![tool_line(
            "#1.1",
            "bash",
            "bash: cargo test",
            Some(true),
            "ok",
        )];

        assert!(set_last_tool_expanded(&mut items, "bash", true));
        assert!(matches!(
            items.as_slice(),
            [Line_::Tool { expanded: true, .. }]
        ));
    }

    #[test]
    fn collapse_marks_transcript_for_rebuild() {
        use std::sync::atomic::AtomicBool;

        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let mut item = tool_line(
            "#1.1",
            "bash",
            "bash: cargo test",
            Some(true),
            "line 1\nline 2\nline 3\nline 4\nline 5\n",
        );
        if let Line_::Tool { expanded, .. } = &mut item {
            *expanded = true;
        }
        state.transcript.push(item);

        state.last_expandable = Some(ExpandableBlock {
            name: "bash".to_string(),
            expanded: true,
        });

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(AtomicBool::new(false));
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &tx,
            &steering_tx,
            &interrupt,
        );

        assert!(state.transcript_needs_rebuild);
        assert!(matches!(
            state.transcript.as_slice(),
            [Line_::Tool {
                expanded: false,
                ..
            }]
        ));
        assert!(
            state
                .last_expandable
                .as_ref()
                .is_some_and(|block| !block.expanded)
        );
        assert_eq!(state.status, "collapsed");
    }

    #[test]
    fn diff_preview_prioritizes_changed_lines_over_diff_boilerplate() {
        let mut lines = Vec::new();
        let diff = "diff --git a/src/tui.rs b/src/tui.rs\nindex 123..456 100644\n--- a/src/tui.rs\n+++ b/src/tui.rs\n@@ -1,3 +1,4 @@ fn demo()\n-old\n+new\n context\n";

        let remaining = push_diff_preview(&mut lines, diff, 8, 120);
        let rendered: Vec<String> = lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect()
            })
            .collect();

        assert_eq!(remaining, 0);
        assert_eq!(rendered.len(), 3);
        assert!(rendered[0].contains("@@ -1,3 +1,4 @@ fn demo()"));
        assert!(rendered[1].contains("-old"));
        assert!(rendered[2].contains("+new"));
        assert!(!rendered.iter().any(|line| line.contains("diff --git")));
        assert!(
            !rendered
                .iter()
                .any(|line| line.contains("+++ b/src/tui.rs"))
        );
    }

    #[test]
    fn edit_tool_renders_path_chip_and_diff_preview() {
        let item = tool_line(
            "#1.2",
            "edit_file",
            "edit_file: src/tui.rs (+2 −1)",
            Some(true),
            "--- a/src/tui.rs\n+++ b/src/tui.rs\n@@ -10,2 +10,3 @@ fn demo()\n-old\n+new\n+more\n",
        );
        let text = line_to_text(&item, 120);
        let lines = flatten_lines(&text);

        assert!(lines.iter().any(|line| line.contains("↳ tui.rs")));
        assert!(
            lines
                .iter()
                .any(|line| line.contains("@@ -10,2 +10,3 @@ fn demo()"))
        );
        assert!(lines.iter().any(|line| line.contains("-old")));
        assert!(lines.iter().any(|line| line.contains("+new")));
        assert!(!lines.iter().any(|line| line.contains("+++ b/src/tui.rs")));
    }

    #[test]
    fn write_tool_renders_path_chip_and_colored_diff_preview() {
        let item = tool_line(
            "#1.3w",
            "write_file",
            "write_file: src/new.rs (32 bytes)",
            Some(true),
            "@@ -0,0 +1,2 @@\n+fn main() {}\n+// done\n",
        );
        let text = line_to_text(&item, 120);
        let lines = flatten_lines(&text);

        assert!(lines.iter().any(|line| line.contains("↳ new.rs")));
        assert!(lines.iter().any(|line| line.contains("+fn main() {}")));
        let added_style = span_style_for(&text, "+fn main() {}").expect("added line span");
        assert_eq!(added_style.fg, Some(Color::Green));
    }

    #[test]
    fn expanded_tool_renders_full_output_inline_without_secondary_block() {
        let mut item = tool_line(
            "#1.3",
            "bash",
            "bash: printf lines",
            Some(true),
            "line 1\nline 2\nline 3\nline 4\nline 5\n",
        );
        if let Line_::Tool { expanded, .. } = &mut item {
            *expanded = true;
        }
        let text = line_to_text(&item, 120);
        let lines = flatten_lines(&text);

        assert!(
            lines.iter().any(|line| line.contains("line 5")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.contains("Ctrl+O collapse")),
            "{lines:?}"
        );
        assert!(
            !lines.iter().any(|line| line.contains("bash (expanded)")),
            "{lines:?}"
        );
    }

    #[test]
    fn expanded_markdownish_tool_renders_markdown() {
        let mut item = tool_line("#1.4", "rg", "rg: markdown", Some(true), "# Plan\n- step");
        if let Line_::Tool { expanded, .. } = &mut item {
            *expanded = true;
        }
        let text = line_to_text(&item, 120);
        let heading_style = span_style_for(&text, "Plan").expect("plan span");
        assert_eq!(heading_style.fg, Some(Color::LightCyan));
        assert!(matches!(heading_style.bg, None | Some(Color::Reset)));
        assert!(heading_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn prefixed_expanded_tool_lines_wrap_inside_available_width() {
        let repeated = "long token ".repeat(32);
        let mut item = tool_line("#1.5", "rg", "rg: long token", Some(true), &repeated);
        if let Line_::Tool { expanded, .. } = &mut item {
            *expanded = true;
        }
        let text = line_to_text(&item, 40);
        let lines = flatten_lines(&text);
        let body = lines
            .iter()
            .filter(|line| line.starts_with("│ "))
            .collect::<Vec<_>>();
        assert!(body.len() > 1, "body lines: {body:?}");
        assert!(
            body.iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 40),
            "body lines: {body:?}"
        );
    }

    #[test]
    fn tool_header_wraps_long_rg_summary_inside_available_width() {
        let item = tool_line(
            "#2.71",
            "rg",
            "rg: /build_streaming_request|history_to_oai_messages|anthropic|tool_result|is_error|provider_tool_result|chatgpt_input_serializes_function_call_without_id_field/ in src/main_tests.rs (+1 args)",
            Some(true),
            "src/main_tests.rs:1:match\n",
        );
        let text = line_to_text(&item, 74);
        let lines = flatten_lines(&text);

        assert!(
            lines.len() > 1,
            "long rg header should wrap instead of relying on terminal overflow: {lines:?}"
        );
        assert!(
            lines[0].starts_with("✓ #2.71 rg"),
            "first header line should keep the check mark/tag/tool: {lines:?}"
        );
        assert!(
            lines.iter().skip(1).any(|line| line.starts_with("  ")),
            "wrapped rg arguments should continue inside the content lane: {lines:?}"
        );
        assert!(
            lines
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 74),
            "rg header lines must not exceed transcript width: {lines:?}"
        );
    }

    #[test]
    fn narrow_tool_header_still_stays_inside_width() {
        let item = tool_line(
            "#123.456",
            "read_symbol",
            "read_symbol: extraordinarily_long_symbol_name_that_needs_wrapping @ src/really/deep/path/file.rs",
            Some(true),
            "ok\n",
        );
        let lines = flatten_lines(&line_to_text(&item, 12));
        assert!(
            lines
                .iter()
                .all(|line| unicode_width::UnicodeWidthStr::width(line.as_str()) <= 12),
            "narrow tool header lines must stay inside transcript width: {lines:?}"
        );
    }

    #[test]
    fn non_markdown_expanded_tool_stays_raw() {
        let mut item = tool_line(
            "#1.6",
            "bash",
            "bash: echo hello",
            Some(true),
            "echo hello\ncargo test",
        );
        if let Line_::Tool { expanded, .. } = &mut item {
            *expanded = true;
        }
        let text = line_to_text(&item, 120);
        let lines = flatten_lines(&text);
        assert!(lines.iter().any(|line| line.contains("echo hello")));
        assert!(lines.iter().any(|line| line.contains("cargo test")));
        let raw_style = span_style_for(&text, "echo hello").expect("raw span");
        assert_eq!(raw_style.fg, Some(Color::DarkGray));
        assert_eq!(raw_style.bg, None);
    }

    #[test]
    fn markdownish_detection_handles_line_numbered_markdown() {
        let content = strip_content_line_numbers("1\t# Heading\n2\t- item\n");
        assert!(looks_like_markdownish_tool_content("read_file", &content));
    }

    #[test]
    fn transcript_render_cache_tracks_multiple_widths() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let item = Line_::Assistant {
            text: "# Heading\n- item".to_string(),
            dim_prefix: false,
        };

        let (_, h40) = cached_transcript_render(&mut state, &item, 40);
        let (_, h80) = cached_transcript_render(&mut state, &item, 80);

        assert!(h40 >= 1);
        assert!(h80 >= 1);

        let key = line_cache_key(&item);
        let entry = state.render_cache.get(&key).expect("cache entry");
        assert_eq!(entry.heights.len(), 2);
        assert_eq!(entry.renders.len(), 2);
    }

    #[test]
    fn usage_update_sets_session_usage_without_turn_end_double_counting() {
        let mut state = TuiState::new(
            "gpt-5.4".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let turn = Usage {
            input: 120_000,
            output: 5_400,
            cache_create: 0,
            cache_read: 40_000,
        };
        let session = Usage {
            input: 828_300,
            output: 5_400,
            cache_create: 0,
            cache_read: 40_000,
        };

        state.apply_event(AgentEvent::UsageUpdate { turn, session });
        state.apply_event(AgentEvent::TurnEnd { usage: turn });

        assert_eq!(state.usage.input, 828_300);
        assert_eq!(state.usage.output, 5_400);
        assert_eq!(state.usage.cache_read, 40_000);
        assert_eq!(state.usage.actual_input_tokens(), 828_300);
        assert_eq!(state.usage.cached_input_tokens(), 40_000);
        assert_eq!(state.last_turn_context_tokens, 165_400);
    }

    #[test]
    fn ctx_meter_tracks_last_request_not_sum_of_iterations() {
        // Regression: the emitter used to pass the accumulated per-turn
        // usage, so a 15-iteration tool-calling turn reported 15× the real
        // context. The TUI must reflect only the most recent request.
        let mut state = TuiState::new(
            "gpt-5.4".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let mut session = Usage::default();
        let per_iter = Usage {
            input: 6_000,
            output: 400,
            cache_create: 0,
            cache_read: 60_000,
        };
        for _ in 0..10 {
            session.add(per_iter);
            state.apply_event(AgentEvent::UsageUpdate {
                turn: per_iter,
                session,
            });
        }
        assert_eq!(state.last_turn_context_tokens, 66_400);
        assert_eq!(state.usage.input, 60_000);
        assert_eq!(state.usage.cache_read, 600_000);
    }

    #[test]
    fn status_splits_actual_and_cached_input() {
        let mut state = TuiState::new(
            "gpt-5.4".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.usage = Usage {
            input: 47_000,
            output: 5_300,
            cache_create: 0,
            cache_read: 268_000,
        };
        let line = status_spans(&state)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(line.contains("↑47.0k"), "{line}");
        assert!(line.contains("↻ 268.0k"), "{line}");
        assert!(line.contains("↓5.3k"), "{line}");

        state.usage = Usage {
            input: 47_000,
            output: 5_300,
            cache_create: 0,
            cache_read: 0,
        };
        let line = status_spans(&state)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(line.contains("↑47.0k"), "{line}");
        assert!(line.contains("↻ 0"), "{line}");
        assert!(line.contains("↓5.3k"), "{line}");
    }

    #[test]
    fn ctx_meter_sums_anthropic_cache_create_read_and_output() {
        // Native usage totals include output tokens; Pi's context-token helper
        // uses input + output + cacheRead + cacheWrite.
        let mut state = TuiState::new(
            "claude-opus-4-6".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let turn = Usage {
            input: 8_000,
            output: 500,
            cache_create: 12_000,
            cache_read: 40_000,
        };
        state.apply_event(AgentEvent::UsageUpdate {
            turn,
            session: turn,
        });
        assert_eq!(state.last_turn_context_tokens, 60_500);
    }

    #[test]
    fn context_meter_uses_chatgpt_catalog_window() {
        let _guard = env_lock();
        let root = std::env::temp_dir().join(format!(
            "dext-tui-ctx-window-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock drift")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create temp dir");
        unsafe {
            std::env::set_var("DEXT_HOME", &root);
            std::env::remove_var("DEXT_CONTEXT_WINDOW");
            std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
        }

        let mut state = TuiState::new(
            "gpt-5.4".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.last_turn_context_tokens = 160_000;

        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();

        unsafe {
            std::env::remove_var("DEXT_HOME");
            std::env::remove_var("DEXT_CONTEXT_WINDOW");
            std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
        }
        let _ = std::fs::remove_dir_all(&root);

        assert!(rendered.contains("160.0k/272.0k"), "{rendered}");
    }

    #[test]
    fn status_spans_render_external_telemetry_counters() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.external_telemetry = ExternalTelemetry {
            dedupe_hits: 2,
            similarity_blocks: 1,
            circuit_breaker_trips: 3,
            partial_delivery_hints: 1,
            http_retries: 4,
        };
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(rendered.contains("ext d2 cb3 sg1 ph1 rt4"), "{rendered}");
    }

    #[test]
    fn status_spans_render_only_nonzero_external_telemetry_counters() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.external_telemetry = ExternalTelemetry {
            dedupe_hits: 0,
            similarity_blocks: 5,
            circuit_breaker_trips: 0,
            partial_delivery_hints: 0,
            http_retries: 0,
        };
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(rendered.contains("ext sg5"), "{rendered}");
        assert!(!rendered.contains("d0"), "{rendered}");
        assert!(!rendered.contains("cb0"), "{rendered}");
        assert!(!rendered.contains("ph0"), "{rendered}");
        assert!(!rendered.contains("rt0"), "{rendered}");
    }

    #[test]
    fn model_arg_completions_lists_authenticated_provider_models() {
        let _guard = env_lock();
        let root = std::env::temp_dir().join(format!(
            "dext-tui-model-completions-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("clock drift")
                .as_nanos()
        ));
        std::fs::create_dir_all(&root).expect("create temp dir");
        unsafe {
            std::env::set_var("DEXT_HOME", &root);
        }

        let result = (|| -> Result<()> {
            let mut store = load_auth_store()?;
            store.providers.insert(
                "glm".to_string(),
                StoredCredential::ApiKey {
                    key: "glm-test-key".to_string(),
                },
            );
            store.providers.insert(
                "chatgpt".to_string(),
                StoredCredential::ApiKey {
                    key: "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string(),
                },
            );
            crate::save_auth_store(&store)?;

            let completions = model_arg_completions("");
            let texts: Vec<String> = completions.into_iter().map(|c| c.text).collect();
            assert!(
                texts.iter().any(|t| t == "/model chatgpt/gpt-4o"),
                "{texts:?}"
            );
            assert!(
                texts.iter().any(|t| t == "/model chatgpt/gpt-5.4"),
                "{texts:?}"
            );
            assert!(texts.iter().any(|t| t == "/model glm-5.1"), "{texts:?}");
            Ok(())
        })();

        unsafe {
            std::env::remove_var("DEXT_HOME");
        }
        let _ = std::fs::remove_dir_all(&root);
        result.expect("model completions should load auth-backed providers");
    }

    #[test]
    fn status_spans_render_updated_provider_and_model() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::TurnDiagnostics {
            provider: "chatgpt".to_string(),
            api_family: "chatgpt-responses".to_string(),
            auth_source: "auth:chatgpt".to_string(),
            model: "gpt-4o".to_string(),
            last_retry_reason: None,
            workaround_fired: false,
            turn_duration_ms: None,
            context_mode: None,
            tool_profile: None,
            compacted: None,
        });
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(rendered.contains("  chatgpt  "), "{rendered}");
        assert!(!rendered.contains("chatgpt:chatgpt"), "{rendered}");
        assert!(!rendered.contains("chatgpt-responses"), "{rendered}");
        assert!(rendered.contains("gpt-4o"), "{rendered}");
    }

    #[test]
    fn status_provider_label_keeps_distinct_provider_and_api_family() {
        assert_eq!(
            status_provider_label("openrouter", "openai-chat-completions"),
            "openrouter:openai"
        );
        assert_eq!(
            status_provider_label("glm", "anthropic-messages"),
            "glm:anthropic"
        );
        assert_eq!(status_provider_label("custom", "custom"), "custom");
    }

    #[test]
    fn derived_busy_status_prefers_live_tool_graph() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.status = "thinking".to_string();
        state.live_tools.push(LiveTool {
            call_id: "call-1".to_string(),
            call_tag: "#1.1".to_string(),
            name: "bash".to_string(),
            summary: "bash: curl".to_string(),
            running: true,
            started: None,
            is_subagent: false,
        });
        assert_eq!(derived_busy_status(&state), "running bash");
    }

    #[test]
    fn derived_busy_status_shows_retry_when_no_active_tools() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.retry_status = Some("retry backoff 2/4".to_string());
        assert_eq!(derived_busy_status(&state), "retry backoff 2/4");
    }

    #[test]
    fn abstract_input_for_display_short_input_unchanged() {
        let input = "hello\nworld\nfoo";
        assert_eq!(abstract_input_for_display(input), None);
    }

    #[test]
    fn abstract_input_for_display_single_long_paragraph_is_collapsed() {
        let long: String = (0..60).map(|i| format!("w{i}\n")).collect();
        let long = long.trim_end();
        let result = abstract_input_for_display(long).expect("collapsed paste preview");
        assert!(result.contains("[paste #1 +60 words hidden"));
        assert!(!result.contains("w30"));
    }

    #[test]
    fn abstract_input_for_display_preserves_short_paragraphs() {
        let first: String = (0..3).map(|i| format!("instruction {i}\n")).collect();
        let second: String = (0..60).map(|i| format!("paste_line_{i}\n")).collect();
        let input = format!("{}\n\n{}", first.trim_end(), second.trim_end());
        let result = abstract_input_for_display(&input).expect("collapsed second paragraph");
        assert!(result.contains("instruction 0"));
        assert!(result.contains("instruction 2"));
        assert!(result.contains("[paste #1 +60 words hidden"));
        assert!(!result.contains("paste_line_30"));
    }

    #[test]
    fn abstract_input_for_display_numbers_multiple_large_pastes() {
        let first = "do stuff";
        let second: String = (0..55).map(|i| format!("p1_{i}\n")).collect();
        let third = "middle text";
        let fourth: String = (0..80).map(|i| format!("p2_{i}\n")).collect();
        let input = format!(
            "{}\n\n{}\n\n{}\n\n{}",
            first,
            second.trim_end(),
            third,
            fourth.trim_end()
        );
        let result = abstract_input_for_display(&input).expect("multiple collapsed pastes");
        assert!(result.contains("[paste #1 +55 words hidden"));
        assert!(result.contains("[paste #2 +80 words hidden"));
        assert!(result.contains("do stuff"));
        assert!(result.contains("middle text"));
    }

    #[test]
    fn slash_completion_arrows_select_without_replacing_input_or_history() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/".to_string();
        state.cursor = state.input.len();
        state.history.push_back("previous prompt".to_string());

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            &tx,
            &steering_tx,
            &interrupt,
        );

        assert_eq!(state.input, "/");
        assert_eq!(state.cursor, 1);
        assert_eq!(state.history_idx, None);
        assert_eq!(state.slash_acomp_sel, Some(1));
        assert!(rx.try_recv().is_err());

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Up, KeyModifiers::empty()),
            &tx,
            &steering_tx,
            &interrupt,
        );
        assert_eq!(state.slash_acomp_sel, Some(0));
    }

    #[test]
    fn busy_enter_sends_steering_directly_without_command_queue_delay() {
        for input in [
            "please adjust the current fix",
            "/model chatgpt/gpt-5.3-codex",
            "/effort high",
        ] {
            let mut state = TuiState::new(
                "glm-5.1".to_string(),
                ".".to_string(),
                ApprovalProfile::Ask,
                ThinkingEffort::Medium,
            );
            state.agent_busy = true;
            state.input = input.to_string();
            state.cursor = state.input.len();

            let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel();
            let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
            let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
                &submit_tx,
                &steering_tx,
                &interrupt,
            );

            assert_eq!(steering_rx.try_recv().ok().as_deref(), Some(input));
            assert!(submit_rx.try_recv().is_err());
            assert!(state.input.is_empty());
            assert_eq!(state.status, "queued for next safe boundary");
            assert!(matches!(
                state.pending_insert.as_slice(),
                [Line_::Blank, Line_::Steering(s), Line_::Blank] if s == input
            ));
        }
    }

    #[test]
    fn busy_input_withholds_potential_local_secret_from_steering() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.input = "/login chatgpt sk-secret-token-that-should-stay-local".to_string();
        state.cursor = state.input.len();

        let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &submit_tx,
            &steering_tx,
            &interrupt,
        );

        assert!(steering_rx.try_recv().is_err());
        assert!(submit_rx.try_recv().is_err());
        assert!(state.input.is_empty());
        assert_eq!(state.status, "local secret withheld from provider");
        assert!(matches!(
            state.pending_insert.as_slice(),
            [Line_::Warn(s)] if s.contains("input withheld")
        ));
    }

    #[test]
    fn busy_paste_withholds_potential_local_secret() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        handle_paste(&mut state, "token=abcdefghijklmnopqrstuvwxyz".to_string());

        assert!(state.input.is_empty());
        assert_eq!(state.status, "local secret paste withheld");
        assert!(matches!(
            state.pending_insert.as_slice(),
            [Line_::Warn(s)] if s.contains("paste withheld")
        ));
    }

    #[test]
    fn slash_completion_arrows_scroll_past_visible_window() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/".to_string();
        state.cursor = state.input.len();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        for _ in 0..SLASH_COMPLETION_MAX_VISIBLE {
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
                &tx,
                &steering_tx,
                &interrupt,
            );
        }

        assert_eq!(state.slash_acomp_sel, Some(SLASH_COMPLETION_MAX_VISIBLE));
        assert_eq!(state.slash_acomp_scroll, 1);
    }

    #[test]
    fn slash_completion_tab_accepts_arrow_selection() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/".to_string();
        state.cursor = state.input.len();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            &tx,
            &steering_tx,
            &interrupt,
        );
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &tx,
            &steering_tx,
            &interrupt,
        );

        assert_eq!(state.input, SLASH_COMMANDS[1].name);
        assert_eq!(state.cursor, state.input.len());
        assert_eq!(state.slash_acomp_sel, Some(0));
        assert_eq!(state.slash_acomp_scroll, 0);
    }

    #[test]
    fn tab_cycles_effort_when_not_completing_slash_command() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "regular prompt".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &tx,
            &steering_tx,
            &interrupt,
        );

        assert!(matches!(rx.try_recv(), Ok(FromTui::CycleEffort(1))));
        assert_eq!(state.input, "regular prompt");
    }

    #[test]
    fn backtab_cycles_effort_when_not_completing_slash_command() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "regular prompt".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &tx,
            &steering_tx,
            &interrupt,
        );

        assert!(matches!(rx.try_recv(), Ok(FromTui::CycleEffort(-1))));
        assert_eq!(state.input, "regular prompt");
    }

    #[test]
    fn tab_falls_back_to_effort_when_slash_has_no_completion() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/definitely-no-such-command".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &tx,
            &steering_tx,
            &interrupt,
        );

        assert!(matches!(rx.try_recv(), Ok(FromTui::CycleEffort(1))));
        assert_eq!(state.input, "/definitely-no-such-command");
    }

    #[test]
    fn slash_popup_layout_stays_inside_inline_viewport() {
        let area = ratatui::layout::Rect::new(0, 10, 92, 7);
        let input_area = ratatui::layout::Rect::new(0, 10, 92, 6);
        let completions: Vec<SlashCompletion> = SLASH_COMMANDS
            .iter()
            .map(|cmd| SlashCompletion {
                text: cmd.name.to_string(),
                hint: cmd.help.to_string(),
            })
            .collect();

        let layout = slash_popup_layout(area, input_area, &completions).expect("popup layout");

        assert!(layout.visible_count > 0);
        assert!(layout.rect.x >= area.x);
        assert!(layout.rect.y >= area.y);
        assert!(layout.rect.x.saturating_add(layout.rect.width) <= area.x + area.width);
        assert!(layout.rect.y.saturating_add(layout.rect.height) <= area.y + area.height);
    }

    #[test]
    fn slash_popup_layout_clamps_width_when_input_is_indented() {
        let area = ratatui::layout::Rect::new(0, 0, 30, 8);
        let input_area = ratatui::layout::Rect::new(4, 2, 26, 5);
        let completions = vec![SlashCompletion {
            text: "/provider anthropic/claude-opus-4-5".to_string(),
            hint: "very long provider/model hint".to_string(),
        }];

        let layout = slash_popup_layout(area, input_area, &completions).expect("popup layout");

        assert_eq!(layout.rect.x, 5);
        assert!(layout.rect.width <= area.width.saturating_sub(5));
    }

    #[test]
    fn slash_input_draw_survives_offset_inline_viewport() {
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let backend = TestBackend::new(92, 40);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(VIEWPORT_HEIGHT),
            },
        )
        .expect("terminal");

        terminal
            .insert_before(30, |buf| {
                let filler = Paragraph::new(Text::from(vec![Line::from(" "); 30]));
                Widget::render(filler, buf.area, buf);
            })
            .expect("insert_before");

        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            "/home/abaka/Documents/Projects/dext".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/".to_string();
        state.cursor = state.input.len();
        state.reset_slash_completion_selection();

        let mut seen_area = None;
        terminal
            .draw(|f| {
                seen_area = Some(f.area());
                draw(f, &mut state);
            })
            .expect("draw slash input");

        let area = seen_area.expect("frame area");
        assert_eq!(area.width, 92);
        assert_eq!(area.height, VIEWPORT_HEIGHT);
        assert!(area.y > 0);
    }

    #[test]
    fn flush_does_not_rebuild_when_later_read_file_matches_previous_batch() {
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let backend = TestBackend::new(120, 20);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("terminal");
        let mut state = TuiState::new(
            "test-model".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.queue(Line_::Banner("🐺  Dext vtest".to_string()));
        flush_pending_insert(&mut terminal, &mut state).expect("flush banner");

        let mut first = vec![tool_line(
            "#1.44",
            "read_file",
            "read_file: src/main.rs (offset=6410, limit=2)",
            Some(true),
            "6410\talpha\n6411\tbravo\n",
        )];
        flush_prepared_items(&mut terminal, &mut state, &mut first).expect("flush first");
        let mut second = vec![tool_line(
            "#1.45",
            "read_file",
            "read_file: src/main.rs (offset=6412, limit=2)",
            Some(true),
            "6412\tcharlie\n6413\tdelta\n",
        )];
        flush_prepared_items(&mut terminal, &mut state, &mut second).expect("flush second");

        assert_eq!(state.transcript.len(), 3);
        assert!(matches!(state.transcript[0], Line_::Banner(_)));
        assert!(matches!(state.transcript[1], Line_::Tool { .. }));
        assert!(matches!(state.transcript[2], Line_::Tool { .. }));
        assert!(!state.transcript_needs_rebuild);
    }

    #[test]
    fn render_transcript_bottom_aligns_live_indicator() {
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let backend = TestBackend::new(40, 12);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(6),
            },
        )
        .expect("terminal");

        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            "/home/abaka/Documents/Projects/dext".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.status = "waiting".to_string();

        terminal
            .draw(|f| {
                let transcript_area = Rect::new(0, 0, 40, 6);
                render_transcript(f, &mut state, transcript_area);
            })
            .expect("draw transcript");

        assert!(state.live_indicator_visible);
        assert_eq!(state.live_indicator_top_padding, 5);
        assert_eq!(state.live_indicator_line_layout, Some((5, 6)));
    }

    #[test]
    fn work_map_drawer_expands_input_without_exceeding_half_viewport() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let area = ratatui::layout::Rect::new(0, 0, 80, 30);
        let baseline = compute_layout(area, &state);
        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Map,
            text: (1..=12)
                .map(|n| format!("@w{n:02} change #{n}  waypoint {n}"))
                .collect::<Vec<_>>()
                .join("\n"),
            waypoint_ids: (1..=12).map(|n| format!("@w{n:02}")).collect(),
            selector: None,
        });
        let with_drawer = compute_layout(area, &state);

        assert!(with_drawer.input_area.height > baseline.input_area.height);
        assert!(with_drawer.input_area.height <= area.height / 2);
        assert_eq!(
            with_drawer.status_area.y + with_drawer.status_area.height,
            area.height
        );
    }

    #[test]
    fn sticky_footer_regression_tall_viewport() {
        let state = TuiState::new(
            "glm-5.1".to_string(),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let area = ratatui::layout::Rect::new(0, 0, 60, 40);
        let layout = compute_layout(area, &state);

        assert_eq!(
            layout.input_area.y + layout.input_area.height,
            layout.status_area.y,
            "input area must sit directly above status"
        );
        assert_eq!(
            layout.status_area.y + layout.status_area.height,
            area.y + area.height,
            "status must sit at the bottom edge"
        );
        assert!(
            layout.input_area.height >= 3,
            "input needs at least 3 rows (border + content + hint)"
        );
    }

    #[test]
    fn md_separator_row_detects_standard() {
        assert!(is_md_separator_row("| --- | --- |"));
        assert!(is_md_separator_row("|-----|-----|"));
        assert!(is_md_separator_row("|:---|---:|"));
        assert!(is_md_separator_row("| :---: | ---: | --- |"));
        assert!(!is_md_separator_row("| Header | Value |"));
        assert!(!is_md_separator_row("no pipes here"));
    }

    #[test]
    fn ascii_border_row_detects_standard() {
        assert!(is_ascii_border_row("+---+---+"));
        assert!(is_ascii_border_row("+------+-------+"));
        assert!(!is_ascii_border_row("|---+---|"));
        assert!(!is_ascii_border_row("| data | more |"));
        assert!(!is_ascii_border_row("+ab+cd+"));
    }

    #[test]
    fn parse_md_table_extracts_header_and_rows() {
        let input = "| Name | Age |\n| --- | --- |\n| Alice | 30 |\n| Bob | 25 |";
        let lines: Vec<&str> = input.lines().collect();
        let table = parse_table_lines(&lines).expect("should parse");
        assert_eq!(table.rows.len(), 3);
        assert_eq!(table.header_rows, 1);
        assert_eq!(table.alignments.len(), 2);
        assert_eq!(table.rows[0], vec!["Name", "Age"]);
        assert_eq!(table.rows[1], vec!["Alice", "30"]);
        assert_eq!(table.rows[2], vec!["Bob", "25"]);
    }

    #[test]
    fn parse_md_table_supports_escaped_pipes() {
        let input = "| Name | Note |\n| --- | --- |\n| dext | uses \\| safely |";
        let lines: Vec<&str> = input.lines().collect();
        let table = parse_table_lines(&lines).expect("should parse");
        assert_eq!(table.rows[1], vec!["dext", "uses | safely"]);
    }

    #[test]
    fn parse_ascii_table_extracts_rows() {
        let input =
            "+------+-----+\n| Name | Age |\n+------+-----+\n| Eve  | 28  |\n+------+-----+";
        let lines: Vec<&str> = input.lines().collect();
        let table = parse_table_lines(&lines).expect("should parse");
        assert_eq!(table.rows.len(), 2);
        assert_eq!(table.header_rows, 1);
        assert_eq!(table.rows[0], vec!["Name", "Age"]);
        assert_eq!(table.rows[1], vec!["Eve", "28"]);
    }

    #[test]
    fn parse_ascii_table_without_header_separator_has_no_header_row() {
        let input = "+-----+\n| a |\n+-----+";
        let lines: Vec<&str> = input.lines().collect();
        let table = parse_table_lines(&lines).expect("should parse");
        assert_eq!(table.header_rows, 0);
    }

    #[test]
    fn table_spacing_adapts_on_narrow_width() {
        let table = ParsedTable {
            rows: vec![vec!["a".into(), "b".into(), "c".into()]],
            header_rows: 0,
            alignments: vec![
                TableColumnAlignment::Left,
                TableColumnAlignment::Left,
                TableColumnAlignment::Left,
            ],
        };
        assert_eq!(table_spacing(&table, 20), 1);
        assert_eq!(table_spacing(&table, 6), 0);
    }

    #[test]
    fn render_table_text_produces_borders_and_bold_header() {
        let table = ParsedTable {
            rows: vec![
                vec!["Key".into(), "Value".into()],
                vec!["name".into(), "dext".into()],
            ],
            header_rows: 1,
            alignments: vec![TableColumnAlignment::Left, TableColumnAlignment::Left],
        };
        let rendered = render_table_lines(&table, Style::default(), 120);
        let flat = rendered
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>();
        assert!(flat[0].starts_with('┌'), "top border: {:?}", flat[0]);
        assert!(flat[1].contains("Key"), "header row: {:?}", flat[1]);
        assert!(flat[1].contains("Value"), "header row: {:?}", flat[1]);
        assert!(flat[2].starts_with('│'), "data row: {:?}", flat[2]);
        assert!(flat[3].starts_with('└'), "bottom border: {:?}", flat[3]);

        let header_style = rendered[1]
            .spans
            .iter()
            .find(|span| span.content.contains("Key"))
            .map(|span| span.style)
            .expect("header span");
        assert!(
            header_style.fg.is_none() || header_style.fg == Some(Color::Reset),
            "header fg should inherit terminal default/reset: {header_style:?}"
        );
        assert!(header_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn markdown_text_renders_table_with_surrounding_prose() {
        let input =
            "## Results\n\n| Tool | Status |\n| --- | ------ |\n| rg | ok |\n| fd | ok |\n\nDone.";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(joined.contains("┌"), "should have table borders");
        assert!(joined.contains("Tool"), "should have header");
        assert!(joined.contains("rg"), "should have data");
        assert!(joined.contains("Done"), "should have trailing text");
    }

    #[test]
    fn markdown_text_sanitizes_carriage_returns_before_rendering() {
        let input = "line one\rline two\r\n| Tool | Status |\n| --- | --- |\n| rg | ok |";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(joined.contains("line one"));
        assert!(joined.contains("line two"));
        assert!(joined.contains("┌"));
        assert!(!joined.contains('\r'));
    }

    #[test]
    fn markdown_text_skips_tables_inside_fenced_code_blocks() {
        let input = "```md\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        assert!(
            !flat.iter().any(|line| line.starts_with('┌')),
            "fenced code should stay code, not table"
        );
        assert!(flat.iter().any(|line| line.trim() == "```md"), "{flat:?}");
        assert!(flat.iter().any(|line| line.trim() == "```"), "{flat:?}");
    }

    #[test]
    fn markdown_text_hides_plain_text_fence_lines() {
        let input = "```text\nball\n```";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        assert_eq!(flat, vec!["ball"]);
    }

    #[test]
    fn markdown_text_hides_plain_text_fences_around_tables() {
        let input = "Before\n\n```text\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```\n\nAfter";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(joined.contains("Before"), "{flat:?}");
        assert!(joined.contains("| A | B |"), "{flat:?}");
        assert!(joined.contains("After"), "{flat:?}");
        assert!(
            !flat.iter().any(|line| line.trim().starts_with("```")),
            "{flat:?}"
        );
        assert!(
            !flat.iter().any(|line| line.starts_with('┌')),
            "plain text fenced table should stay raw: {flat:?}"
        );
    }

    #[test]
    fn markdown_text_falls_back_on_non_table_pipe_lines() {
        let input = "some | pipe | text\nno separator here";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        assert!(
            !flat.iter().any(|l| l.starts_with('┌')),
            "should not render as table: {flat:?}"
        );
    }

    #[test]
    fn clamp_chars_reports_omitted_character_count() {
        let clipped = clamp_chars_with_hint("abcdefghijklmnopqrstuvwxyz", 20);
        assert!(clipped.ends_with("+17 chars"), "{clipped}");
        assert!(unicode_width::UnicodeWidthStr::width(clipped.as_str()) <= 20);
    }

    #[test]
    fn truncate_cell_respects_width() {
        assert_eq!(truncate_cell("hello", 3), "he…");
        assert_eq!(truncate_cell("hi", 5), "hi");
        assert_eq!(truncate_cell("abc", 0), "");
        assert_eq!(truncate_cell("abcdef", 1), "…");
    }

    #[test]
    fn markdown_alignment_applies_right_and_center_cells() {
        let alignments =
            parse_md_separator_alignments("| --- | :---: | ---: |").expect("separator row");
        assert_eq!(alignments[0], TableColumnAlignment::Left);
        assert_eq!(alignments[1], TableColumnAlignment::Center);
        assert_eq!(alignments[2], TableColumnAlignment::Right);

        let input = "| L | C | R |\n| --- | :---: | ---: |\n| a | b | 7 |";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(joined.contains("7"));
    }
}
