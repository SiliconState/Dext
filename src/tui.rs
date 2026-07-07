use anyhow::Result;
use crossterm::event::{
    self as cterm_event, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
    KeyModifiers, KeyboardEnhancementFlags, MouseEvent, MouseEventKind,
    PopKeyboardEnhancementFlags, PushKeyboardEnhancementFlags,
};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, enable_raw_mode};
use ratatui::Terminal;
use ratatui::backend::{Backend, CrosstermBackend};
use ratatui::layout::{Alignment, Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Widget, Wrap};
use ratatui::{TerminalOptions, Viewport};
use ratatui_core::layout::Alignment as MdAlignment;
use ratatui_core::style::{Color as MdColor, Modifier as MdModifier, Style as MdStyle};
use ratatui_core::text::Text as MdText;
use serde_json::Value;
use std::borrow::Cow;
use std::collections::{HashMap, HashSet, VecDeque};
use std::hash::{DefaultHasher, Hash, Hasher};
use std::io::{self, Write as IoWrite};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tui_markdown::{
    Options as MarkdownOptions, StyleSheet as MarkdownStyleSheet, from_str_with_options,
};

use crate::provider::{curated_provider_models, provider_has_available_credentials};
use crate::{
    Agent, AgentEvent, ApprovalProfile, Choice, ContextMode, EventSink,
    HISTORY_CHAR_BUDGET_END_TURN_PERCENT, LocalAuthSecret, ThinkingEffort, Usage, WorkMapEventKind,
    canonical_provider_id, clear_secret_string, git_summary, handle_slash,
    history_char_budget_with_window, load_auth_store, load_provider_catalog, model_context_window,
    orchestrator::ExternalTelemetry, parse_active_runtime_control_sequence, parse_compact_slash,
    provider_auth_status, pseudo_tool_redaction_marker, redact_pseudo_tool_protocol_text,
    resolve_active_provider_id, summarize_call, text_line_looks_like_pseudo_tool_syntax,
};

const INPUT_HISTORY_MAX: usize = 200;
const RENDER_CACHE_MAX_ENTRIES: usize = 2048;
const VIEWPORT_HEIGHT: u16 = 10;
const COLLAPSED_PREVIEW_LINES: usize = 4;
const TOOL_DENSITY_SEPARATOR_EVERY: usize = 10;
const RG_LINE_TRUNCATE_CELLS: usize = 220;
const TRANSCRIPT_WRAP_GUARD_COLS: u16 = 1;
const INPUT_MAX_PANEL_ROWS: u16 = 24;
const PASTE_WORD_THRESHOLD: usize = 50;
const CONTEXT_BAR_CELLS: usize = 10;
const WORK_MAP_DRAWER_MAX_ROWS: usize = 20;
const WORK_MAP_DRAWER_MAX_BODY_ROWS: usize = 18;
const WORK_MAP_DRAWER_MIN_EDITOR_ROWS: usize = 1;
const THINKING_BG: Color = Color::Indexed(235);
const STEERING_BG: Color = Color::Indexed(236);
const TRUST_INPUT_BORDER: Color = Color::Indexed(66);
const SPINNER_FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
const LIVE_BACKEND_RING_CAP: usize = 256_000;
const LIVE_BACKEND_MAX_TOOLS: usize = 8;
const LIVE_OUTPUT_DRAIN_BATCH: usize = 32;

#[derive(Clone, PartialEq, Eq, Hash)]
struct ToolChunk {
    call_tag: String,
    summary: String,
    content: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct InputPreviewSpan {
    start: usize,
    end: usize,
    words: usize,
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

struct PendingLocalAuth {
    tool: String,
    message: String,
    responder: std::sync::mpsc::SyncSender<LocalAuthSecret>,
}

#[derive(Clone)]
struct WorkMapDrawer {
    text: String,
    waypoint_ids: Vec<String>,
    selector: Option<String>,
    selected: usize,
    scroll: usize,
    filter_input: bool,
}

#[derive(Clone, PartialEq, Eq, Hash)]
struct ActiveFocusLabel {
    selection: String,
    mode: String,
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
    LocalAuthSecretRequest {
        tool: String,
        message: String,
        responder: std::sync::mpsc::SyncSender<LocalAuthSecret>,
    },
}

enum FromTui {
    Submit(String),
    CycleEffort(i8),
    Quit,
}

struct TuiSink {
    tx: tokio::sync::mpsc::UnboundedSender<ToTui>,
    live_tx: tokio::sync::mpsc::Sender<AgentEvent>,
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

    fn live_output_sender(&self) -> Option<tokio::sync::mpsc::Sender<AgentEvent>> {
        Some(self.live_tx.clone())
    }

    fn request_local_auth_secret(&mut self, tool: &str, message: &str) -> LocalAuthSecret {
        let (resp_tx, resp_rx) = std::sync::mpsc::sync_channel(0);
        if self
            .tx
            .send(ToTui::LocalAuthSecretRequest {
                tool: tool.to_string(),
                message: message.to_string(),
                responder: resp_tx,
            })
            .is_err()
        {
            return LocalAuthSecret::Unavailable;
        }
        resp_rx.recv().unwrap_or(LocalAuthSecret::Canceled)
    }
}

#[derive(Clone)]
struct BackendOutput {
    call_id: String,
    call_tag: String,
    name: String,
    summary: String,
    text: String,
    dropped_bytes: usize,
    running: bool,
    partial_stream: Option<String>,
}

#[derive(Clone)]
struct LiveTool {
    call_id: String,
    call_tag: String,
    name: String,
    summary: String,
    running: bool,
    started: Option<Instant>,
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
        args: "[default|full|status]",
        help: "list/switch tools",
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
        help: "auto-approve all privileged tools",
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
        help: "off|low|medium|high|xhigh|max",
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
        help: "open session map drawer",
    },
    SlashCmd {
        name: "/focus",
        args: "@wNN [--branch|--exact]",
        help: "inspect or branch from a moment",
    },
    SlashCmd {
        name: "/branches",
        args: "",
        help: "list branches",
    },
    SlashCmd {
        name: "/sessions",
        args: "",
        help: "list latest + autosaved/named",
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
static TOOLS_ARGS: &[&str] = &["default", "full", "status"];
static EFFORT_ARGS: &[&str] = &["off", "low", "medium", "high", "xhigh", "max"];
static WORK_MAP_SESSION_ARGS: &[&str] = &["current", "latest"];
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
            "tools" => Some(TOOLS_ARGS),
            "effort" => Some(EFFORT_ARGS),
            "map" | "focus" => Some(WORK_MAP_SESSION_ARGS),
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

struct TuiState {
    pending_insert: Vec<Line_>,
    transcript: Vec<Line_>,
    render_cache: HashMap<u64, CachedTranscriptRender>,
    transcript_rendered_width: u16,
    transcript_scroll_offset: usize,
    transcript_hover_expandable: Option<usize>,
    transcript_area: Rect,
    input_area: Rect,
    inspector_area: Rect,
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
    input_preview_spans: Vec<InputPreviewSpan>,
    history: VecDeque<String>,
    history_idx: Option<usize>,
    status: String,
    usage: Usage,
    // Tokens the most recent request actually carried (live request-level context).
    // Distinct from `usage` which is cumulative session billing.
    last_turn_context_tokens: u64,
    history_chars: u64,
    model: String,
    context_window_tokens: u64,
    sandbox: String,
    streaming_text: String,
    streaming_thinking: String,
    stream_started_at: Option<Instant>,
    stream_chars: u64,
    pending_perm: Option<PendingPermission>,
    pending_local_auth: Option<PendingLocalAuth>,
    local_auth_input: String,
    // Secret-looking input the user was warned about; sending requires an
    // identical second Enter so credentials never reach the model by accident.
    pending_secret_send: Option<String>,
    agent_busy: bool,
    quit: bool,
    frame_count: u64,
    approval_profile: ApprovalProfile,
    thinking_effort: ThinkingEffort,
    last_expandable: Option<ExpandableBlock>,
    show_help: bool,
    show_status_details: bool,
    show_inspector: bool,
    slash_acomp_sel: Option<usize>,
    slash_acomp_scroll: usize,
    git_branch: Option<String>,
    git_branch_refreshed: Option<Instant>,
    tool_tint_parity: bool,
    transcript_needs_rebuild: bool,
    call_tag_seq: usize,
    turn_seq: usize,
    call_tags: HashMap<String, String>,
    verbose: bool,
    input_display_override: Option<String>,
    external_telemetry: ExternalTelemetry,
    retry_status: Option<String>,
    compacting: bool,
    compacting_resume_busy: bool,
    provider_label: String,
    api_family: String,
    auth_source: String,
    context_mode: ContextMode,
    last_retry_reason: Option<String>,
    workaround_fired: bool,
    assistant_prefix_seen: bool,
    turn_tool_counts: HashMap<String, usize>,
    turn_error_count: usize,
    turn_start_at: Option<Instant>,
    todo_progress: Option<TodoProgress>,
    work_map: Option<WorkMapDrawer>,
    active_focus: Option<ActiveFocusLabel>,
    debug_events: VecDeque<String>,
    backend_outputs: VecDeque<BackendOutput>,
    backend_viewer_open: bool,
    backend_viewer_call_id: Option<String>,
    backend_viewer_scroll_from_bottom: usize,
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
        context_window_tokens: u64,
        sandbox: String,
        approval_profile: ApprovalProfile,
        thinking_effort: ThinkingEffort,
    ) -> Self {
        Self {
            pending_insert: Vec::new(),
            transcript: Vec::new(),
            render_cache: HashMap::new(),
            transcript_rendered_width: 0,
            transcript_scroll_offset: 0,
            transcript_hover_expandable: None,
            transcript_area: Rect::default(),
            input_area: Rect::default(),
            inspector_area: Rect::default(),
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
            input_preview_spans: Vec::new(),
            history: VecDeque::new(),
            history_idx: None,
            status: "ready".into(),
            usage: Usage::default(),
            last_turn_context_tokens: 0,
            history_chars: 0,
            model,
            context_window_tokens,
            sandbox,
            streaming_text: String::new(),
            streaming_thinking: String::new(),
            stream_started_at: None,
            stream_chars: 0,
            pending_perm: None,
            pending_local_auth: None,
            local_auth_input: String::new(),
            pending_secret_send: None,
            agent_busy: false,
            quit: false,
            frame_count: 0,
            approval_profile,
            thinking_effort,
            context_mode: ContextMode::Standard,
            last_expandable: None,
            show_help: false,
            show_status_details: false,
            show_inspector: false,
            slash_acomp_sel: None,
            slash_acomp_scroll: 0,
            git_branch: None,
            git_branch_refreshed: None,
            tool_tint_parity: false,
            transcript_needs_rebuild: false,
            call_tag_seq: 0,
            turn_seq: 0,
            call_tags: HashMap::new(),
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
            active_focus: None,
            debug_events: VecDeque::new(),
            backend_outputs: VecDeque::new(),
            backend_viewer_open: false,
            backend_viewer_call_id: None,
            backend_viewer_scroll_from_bottom: 0,
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

    fn push_debug_event(&mut self, event: impl Into<String>) {
        const MAX_DEBUG_EVENTS: usize = 80;
        self.debug_events.push_back(event.into());
        while self.debug_events.len() > MAX_DEBUG_EVENTS {
            self.debug_events.pop_front();
        }
    }

    fn selected_backend_index(&self) -> Option<usize> {
        if let Some(call_id) = self.backend_viewer_call_id.as_deref()
            && let Some(idx) = self
                .backend_outputs
                .iter()
                .position(|output| output.call_id == call_id)
        {
            return Some(idx);
        }
        self.backend_outputs.len().checked_sub(1)
    }

    fn selected_backend_output(&self) -> Option<&BackendOutput> {
        self.selected_backend_index()
            .and_then(|idx| self.backend_outputs.get(idx))
    }

    fn set_backend_selection_to_latest(&mut self) {
        self.backend_viewer_call_id = self
            .backend_outputs
            .back()
            .map(|output| output.call_id.clone());
        self.backend_viewer_scroll_from_bottom = 0;
    }

    fn open_backend_viewer(&mut self) -> bool {
        if self.backend_outputs.is_empty() {
            self.status = "backend output unavailable".to_string();
            return false;
        }
        self.backend_viewer_open = true;
        if self.backend_viewer_call_id.as_ref().is_none_or(|call_id| {
            !self
                .backend_outputs
                .iter()
                .any(|output| &output.call_id == call_id)
        }) {
            self.set_backend_selection_to_latest();
        }
        self.status = "backend viewer open".to_string();
        true
    }

    fn close_backend_viewer(&mut self) {
        let was_open = self.backend_viewer_open;
        self.backend_viewer_open = false;
        if was_open {
            self.status = "backend viewer closed".to_string();
        }
    }

    fn cycle_backend_selection(&mut self, delta: isize) {
        if self.backend_outputs.is_empty() {
            self.backend_viewer_call_id = None;
            self.backend_viewer_scroll_from_bottom = 0;
            return;
        }
        let len = self.backend_outputs.len();
        let current = self
            .selected_backend_index()
            .unwrap_or(len.saturating_sub(1));
        let next = ((current as isize + delta).rem_euclid(len as isize)) as usize;
        self.backend_viewer_call_id = self
            .backend_outputs
            .get(next)
            .map(|output| output.call_id.clone());
        self.backend_viewer_scroll_from_bottom = 0;
    }

    fn append_backend_output_text(output: &mut BackendOutput, text: String) {
        output.text.push_str(&text);
        if output.text.len() <= LIVE_BACKEND_RING_CAP {
            return;
        }
        let mut drain_to = output.text.len() - LIVE_BACKEND_RING_CAP;
        while drain_to < output.text.len() && !output.text.is_char_boundary(drain_to) {
            drain_to += 1;
        }
        output.text.drain(..drain_to);
        output.dropped_bytes = output.dropped_bytes.saturating_add(drain_to);
    }

    fn append_backend_labeled_text(output: &mut BackendOutput, stream: &str, text: &str) {
        let normalized = text.replace('\r', "\n");
        if normalized.is_empty() {
            return;
        }
        let label = if stream == "stderr" {
            "stderr"
        } else {
            "stdout"
        };
        let mut line_open_stream = if output.text.ends_with('\n') {
            None
        } else {
            output.partial_stream.clone()
        };
        let mut ends_with_newline = output.text.is_empty() || output.text.ends_with('\n');
        let mut chunk = String::new();

        for piece in normalized.split_inclusive('\n') {
            if piece.is_empty() {
                continue;
            }
            let continues_same_line =
                line_open_stream.as_deref() == Some(label) && !ends_with_newline;
            if !continues_same_line {
                if !ends_with_newline {
                    chunk.push('\n');
                }
                chunk.push_str(label);
                chunk.push_str(" │ ");
            }
            chunk.push_str(piece);
            if piece.ends_with('\n') {
                line_open_stream = None;
                ends_with_newline = true;
            } else {
                line_open_stream = Some(label.to_string());
                ends_with_newline = false;
            }
        }

        output.partial_stream = if ends_with_newline {
            None
        } else {
            line_open_stream
        };
        Self::append_backend_output_text(output, chunk);
    }

    fn trim_backend_outputs(&mut self) {
        while self.backend_outputs.len() > LIVE_BACKEND_MAX_TOOLS {
            let removed_selected = self
                .backend_outputs
                .front()
                .zip(self.backend_viewer_call_id.as_ref())
                .is_some_and(|(output, selected)| output.call_id == *selected);
            self.backend_outputs.pop_front();
            if removed_selected {
                self.set_backend_selection_to_latest();
            }
        }
    }

    fn upsert_backend_output(
        &mut self,
        call_id: &str,
        name: &str,
        summary: &str,
        running: bool,
        create: bool,
    ) {
        if name != "bash" {
            return;
        }
        let call_tag = self.tool_tag_for(call_id);
        if let Some(output) = self
            .backend_outputs
            .iter_mut()
            .find(|output| output.call_id == call_id)
        {
            output.call_tag = call_tag;
            output.name = name.to_string();
            if !summary.is_empty() {
                output.summary = summary.to_string();
            }
            output.running = running;
        } else if create {
            self.backend_outputs.push_back(BackendOutput {
                call_id: call_id.to_string(),
                call_tag,
                name: name.to_string(),
                summary: summary.to_string(),
                text: String::new(),
                dropped_bytes: 0,
                running,
                partial_stream: None,
            });
            if self.backend_viewer_call_id.is_none() {
                self.set_backend_selection_to_latest();
            }
        }
        self.trim_backend_outputs();
    }

    fn apply_backend_output_delta(
        &mut self,
        call_id: String,
        name: String,
        stream: String,
        text: String,
    ) {
        if !self
            .backend_outputs
            .iter()
            .any(|output| output.call_id == call_id)
        {
            self.upsert_backend_output(&call_id, &name, "", true, true);
        }
        if let Some(output) = self
            .backend_outputs
            .iter_mut()
            .find(|output| output.call_id == call_id)
        {
            Self::append_backend_labeled_text(output, &stream, &text);
        }
    }

    fn backend_body_rows_for_width(&self, width: u16) -> Vec<String> {
        let Some(output) = self.selected_backend_output() else {
            return vec!["No backend output captured yet.".to_string()];
        };
        let inner = width.max(1) as usize;
        let mut rows = Vec::new();
        if output.dropped_bytes > 0 {
            rows.push(clamp_chars(
                &format!(
                    "…[dropped oldest {} bytes from live ring buffer]",
                    output.dropped_bytes
                ),
                inner,
            ));
        }
        if output.text.is_empty() {
            rows.push("(waiting for bash output…)".to_string());
        } else {
            for line in sanitize_display_text(&output.text).lines() {
                rows.extend(wrap_plain_visual(line, inner));
            }
        }
        rows
    }

    fn clamp_backend_scroll(&mut self, body_rows: usize, visible_rows: usize) {
        let max_scroll = body_rows.saturating_sub(visible_rows);
        self.backend_viewer_scroll_from_bottom =
            self.backend_viewer_scroll_from_bottom.min(max_scroll);
    }

    fn scroll_backend_viewer(&mut self, delta_from_bottom: isize) {
        if delta_from_bottom >= 0 {
            self.backend_viewer_scroll_from_bottom = self
                .backend_viewer_scroll_from_bottom
                .saturating_add(delta_from_bottom as usize);
        } else {
            self.backend_viewer_scroll_from_bottom = self
                .backend_viewer_scroll_from_bottom
                .saturating_sub(delta_from_bottom.unsigned_abs());
        }
    }

    fn queue(&mut self, line: Line_) {
        let is_transcript_block = Self::line_needs_history_spacing(&line);
        let is_steering = matches!(line, Line_::Steering(_));
        if ((is_transcript_block && self.last_line_needs_history_spacing()) || is_steering)
            && !self.pending_insert.ends_with(&[Line_::Blank])
        {
            self.pending_insert.push(Line_::Blank);
        }
        self.pending_insert.push(line);
        if is_steering {
            self.pending_insert.push(Line_::Blank);
        }
    }

    fn line_needs_history_spacing(line: &Line_) -> bool {
        matches!(
            line,
            Line_::Assistant { .. } | Line_::Tool { .. } | Line_::Thinking(_)
        )
    }

    fn last_line_needs_history_spacing(&self) -> bool {
        self.pending_insert
            .last()
            .or_else(|| self.transcript.last())
            .is_some_and(Self::line_needs_history_spacing)
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
        self.replace_input(completions[idx].text.clone());
        self.reset_slash_completion_selection();
        true
    }

    fn replace_input(&mut self, input: String) {
        self.input = input;
        self.cursor = self.input.len();
        self.input_preview_spans.clear();
        self.input_display_override = None;
    }

    fn clear_input(&mut self) {
        self.replace_input(String::new());
    }

    fn insert_input_str(&mut self, text: &str) {
        let at = self.cursor;
        self.input.insert_str(at, text);
        let len = text.len();
        for span in &mut self.input_preview_spans {
            if at <= span.start {
                span.start += len;
                span.end += len;
            } else if at < span.end {
                span.end += len;
                span.words = count_words(&self.input[span.start..span.end]);
            }
        }
        self.cursor += len;
    }

    fn insert_input_char(&mut self, c: char) {
        let mut buf = [0u8; 4];
        self.insert_input_str(c.encode_utf8(&mut buf));
    }

    fn remove_input_range(&mut self, start: usize, end: usize) {
        self.input.replace_range(start..end, "");
        if start < end {
            let len = end - start;
            for span in &mut self.input_preview_spans {
                if end <= span.start {
                    span.start -= len;
                    span.end -= len;
                } else if start >= span.end {
                    continue;
                } else if start <= span.start && end >= span.end {
                    span.end = span.start;
                    span.words = 0;
                } else {
                    let overlap_start = start.max(span.start);
                    let overlap_end = end.min(span.end);
                    span.end -= overlap_end.saturating_sub(overlap_start);
                    if start < span.start {
                        let prefix_removed = span.start - start;
                        span.start -= prefix_removed;
                        span.end -= prefix_removed;
                    }
                    if span.start < span.end && span.end <= self.input.len() {
                        span.words = count_words(&self.input[span.start..span.end]);
                    }
                }
            }
        }
        self.cursor = self.cursor.min(self.input.len());
        self.refresh_input_display_override();
    }

    fn refresh_input_display_override(&mut self) {
        let input_len = self.input.len();
        self.input_preview_spans.retain(|span| {
            span.start < span.end
                && span.end <= input_len
                && self.input.is_char_boundary(span.start)
                && self.input.is_char_boundary(span.end)
                && span.words > PASTE_WORD_THRESHOLD
        });
        self.input_display_override =
            abstract_input_with_spans(&self.input, &self.input_preview_spans);
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
        let inspector = self.inspector_area;
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
        let inspector_contains = inspector.width > 0
            && inspector.height > 0
            && column >= inspector.x
            && column < inspector.x.saturating_add(inspector.width)
            && row >= inspector.y
            && row < inspector.y.saturating_add(inspector.height);
        transcript_contains || input_contains || inspector_contains
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

    fn pseudo_tool_display_text(&self, text: &str) -> String {
        pseudo_tool_protocol_text_for_context(text, self.context_mode)
    }

    fn apply_event(&mut self, ev: AgentEvent) {
        match ev {
            AgentEvent::TurnStart => {
                self.push_debug_event("turn start");
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
            }
            AgentEvent::HistoryContextUpdated { chars, tokens } => {
                self.history_chars = chars as u64;
                self.last_turn_context_tokens =
                    tokens.unwrap_or_else(|| ((self.history_chars.saturating_add(3)) / 4).max(1));
            }
            AgentEvent::TextDelta(t) => {
                self.push_debug_event(format!("text delta · {} chars", t.chars().count()));
                self.retry_status = None;
                if self.stream_started_at.is_none() {
                    self.stream_started_at = Some(Instant::now());
                    if self.agent_busy {
                        self.status = "scroll: live".into();
                    }
                }
                let visible = if self.context_mode.is_frugal() {
                    t
                } else {
                    legacy_pseudo_tool_protocol_redact_lines(&t)
                };
                let char_count = visible.chars().count() as u64;
                self.stream_chars = self.stream_chars.saturating_add(char_count);
                self.history_chars = self.history_chars.saturating_add(char_count);
                self.streaming_text.push_str(&visible);
            }
            AgentEvent::TextBlockComplete(full) => {
                self.push_debug_event(format!(
                    "assistant block complete · {} chars",
                    full.chars().count()
                ));
                let rendered = self.pseudo_tool_display_text(&full);
                if !rendered.is_empty() {
                    let dim_prefix = self.assistant_prefix_seen;
                    self.assistant_prefix_seen = true;
                    self.queue(Line_::Assistant {
                        text: rendered,
                        dim_prefix,
                    });
                }
                self.streaming_text.clear();
                self.stream_started_at = None;
                self.stream_chars = 0;
            }
            AgentEvent::ThinkingDelta(t) => {
                self.push_debug_event(format!("thinking delta · {} chars", t.chars().count()));
                self.retry_status = None;
                let visible = if self.context_mode.is_frugal() {
                    t
                } else {
                    legacy_pseudo_tool_protocol_redact_lines(&t)
                };
                self.streaming_thinking.push_str(&visible);
                if self.streaming_text.is_empty() && self.stream_started_at.is_none() {
                    self.status = "scroll: live".into();
                }
            }
            AgentEvent::ThinkingBlockComplete(full) => {
                self.push_debug_event(format!(
                    "thinking block complete · {} words",
                    full.split_whitespace().count()
                ));
                let rendered = if self.context_mode.is_frugal() {
                    self.pseudo_tool_display_text(&full)
                } else {
                    full
                };
                if !rendered.is_empty() {
                    let word_count = rendered.split_whitespace().count();
                    if self.verbose {
                        self.queue(Line_::Thinking(rendered));
                    }
                    self.status = if self.verbose {
                        format!("thinking visible ({} words)", word_count)
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
                self.push_debug_event(format!("tool preview · {name} · {call_id}"));
                let call_tag = self.tool_tag_for(&call_id);
                if let Some(existing) = self.live_tools.iter_mut().find(|t| t.call_id == call_id) {
                    if existing.summary.is_empty() {
                        existing.summary = summary;
                    }
                    existing.name = name;
                } else {
                    self.live_tools.push(LiveTool {
                        call_id,
                        call_tag,
                        name,
                        summary,
                        running: false,
                        started: None,
                    });
                }
            }
            AgentEvent::ToolCallStart {
                call_id,
                name,
                summary,
            } => {
                self.push_debug_event(format!("tool start · {name} · {call_id}"));
                self.upsert_backend_output(&call_id, &name, &summary, true, true);
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
                self.push_debug_event(format!(
                    "tool result · {name} · {} · {} lines",
                    if ok { "ok" } else { "error" },
                    content.lines().count()
                ));
                self.upsert_backend_output(&call_id, &name, &preview, false, false);
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
                let (n, mut summary, started_at) = if let Some(i) = idx {
                    let t = self.live_tools.remove(i);
                    (t.name, t.summary, t.started)
                } else {
                    (name.clone(), String::new(), None)
                };
                let duration_secs = started_at.map(|t| t.elapsed().as_secs()).unwrap_or(0);
                let denied = !ok && content.contains("permission denied");
                if !preview.is_empty() {
                    summary = preview;
                }

                self.retry_status = None;

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
                self.status = "thinking".into();
            }
            AgentEvent::ToolOutputDelta {
                call_id,
                name,
                stream,
                text,
            } => {
                self.apply_backend_output_delta(call_id, name, stream, text);
            }
            AgentEvent::LocalAuthPrompt { tool, message } => {
                self.push_debug_event(format!("local auth prompt · {tool}"));
                self.status = "local sudo prompt".into();
                self.queue(Line_::LocalAuth { tool, message });
            }
            AgentEvent::ToolBatchStart {
                batch_id,
                call_ids,
                labels: _,
            } => {
                self.push_debug_event(format!(
                    "tool batch start · {batch_id} · {} calls",
                    call_ids.len()
                ));
            }
            AgentEvent::ToolBatchEnd {
                batch_id,
                call_ids,
                labels: _,
                failed,
            } => {
                self.push_debug_event(format!(
                    "tool batch end · {batch_id} · {} calls · {failed} failed",
                    call_ids.len()
                ));
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
                self.push_debug_event(format!(
                    "http retry · attempt {attempt}/4 · wait {wait_secs}s · {reason}"
                ));
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
                context_window,
                last_retry_reason,
                workaround_fired,
                context_mode,
                tool_profile: _,
                compacted: _,
                ..
            } => {
                self.push_debug_event(format!(
                    "turn diagnostics · {provider}/{model} · {api_family} · auth {auth_source}"
                ));
                self.provider_label = provider;
                self.api_family = api_family;
                self.auth_source = auth_source;
                self.model = model;
                if let Some(context_window) = context_window.filter(|tokens| *tokens > 0) {
                    self.context_window_tokens = context_window;
                }
                self.last_retry_reason = last_retry_reason;
                if let Some(context_mode) = context_mode {
                    self.context_mode = context_mode;
                }
                self.workaround_fired = workaround_fired;
            }
            AgentEvent::ThinkingEffortChanged { effort } => {
                self.push_debug_event(format!("thinking effort changed · {}", effort.as_str()));
                self.thinking_effort = effort;
            }
            AgentEvent::ApprovalProfileChanged { profile } => {
                self.push_debug_event(format!("approval profile changed · {}", profile.as_str()));
                self.approval_profile = profile;
            }
            AgentEvent::RuntimeControl(s) => {
                self.push_debug_event(format!("runtime control · {}", sanitize_display_text(&s)));
                self.status = "runtime control applied".into();
                self.queue(Line_::Info(s));
            }
            AgentEvent::RuntimeControlApplied {
                commands,
                stream_aborted,
                ..
            } => {
                self.push_debug_event(format!(
                    "runtime control applied · commands={commands} abort={stream_aborted}"
                ));
                if stream_aborted {
                    self.streaming_text.clear();
                    self.streaming_thinking.clear();
                    self.stream_started_at = None;
                    self.stream_chars = 0;
                }
                self.status = if stream_aborted {
                    "runtime changed; restarting request".into()
                } else {
                    "runtime control applied".into()
                };
            }
            AgentEvent::Info(s) => {
                self.push_debug_event(format!("info · {}", sanitize_display_text(&s)));
                if let Some(status) = phase_status_text(&s) {
                    self.status = status;
                }
                self.queue(Line_::Info(s));
            }
            AgentEvent::Warn(s) => {
                self.push_debug_event(format!("warn · {}", sanitize_display_text(&s)));
                self.queue(Line_::Warn(s));
            }
            AgentEvent::Error(s) => {
                self.push_debug_event(format!("error · {}", sanitize_display_text(&s)));
                self.queue(Line_::Error(s));
            }
            AgentEvent::Slash(s) => {
                self.push_debug_event(format!("slash/system · {}", sanitize_display_text(&s)));
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.stream_started_at = None;
                self.stream_chars = 0;
                self.live_tools.clear();
                self.agent_busy = false;
                if s.starts_with("focus cleared;") || s.starts_with("cleared ") {
                    self.active_focus = None;
                }
                self.status = phase_status_text(&s).unwrap_or_else(|| "ready".into());
                self.queue(Line_::Info(s));
            }
            AgentEvent::WorkMap {
                kind,
                text,
                waypoint_ids,
                selector,
            } => {
                self.push_debug_event(format!(
                    "work map · {kind:?} · {} waypoints",
                    waypoint_ids.len()
                ));
                self.compacting = false;
                self.compacting_resume_busy = false;
                self.streaming_text.clear();
                self.streaming_thinking.clear();
                self.stream_started_at = None;
                self.stream_chars = 0;
                self.live_tools.clear();
                self.agent_busy = false;
                if matches!(kind, WorkMapEventKind::Focus)
                    && let Some(focus) = parse_work_map_focus_label(&text)
                {
                    self.active_focus = Some(focus);
                }
                let visible_ids = visible_work_map_ids(&text, &waypoint_ids);
                if matches!(kind, WorkMapEventKind::Map) && !visible_ids.is_empty() {
                    self.work_map = Some(WorkMapDrawer {
                        text,
                        waypoint_ids: visible_ids,
                        selector,
                        selected: 0,
                        scroll: 0,
                        filter_input: false,
                    });
                    self.status = "session map open: ↑/↓/PgUp/PgDn navigate · Enter inspect · f edit · b branch · z filter · Esc close"
                        .into();
                } else {
                    self.work_map = None;
                    self.status = "session map ready".into();
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
                self.push_debug_event("turn end");
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
                        "{} tools · {elapsed}{error_note} · {tool_summary}",
                        tool_total,
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
            }
            AgentEvent::CompactStart => {
                self.push_debug_event("compact start");
                self.compacting_resume_busy = self.agent_busy;
                self.compacting = true;
                self.agent_busy = true;
                self.status = "compacting history".into();
            }

            AgentEvent::CompactEnd { before, after } => {
                self.push_debug_event(format!("compact end · {before} → {after}"));
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
                self.push_debug_event(format!("compact failed · {message}"));
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
                self.push_debug_event("interrupted");
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
            }
            AgentEvent::SteeringReceived { messages, preview } => {
                self.push_debug_event(format!("steering received · {messages} updates"));
                let noun = if messages == 1 { "update" } else { "updates" };
                self.status = format!("queued {messages} {noun} for next response");
                self.queue(Line_::SteeringDelivered { messages, preview });
            }
        }
    }
}

fn phase_status_text(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let rest = trimmed.strip_prefix("[phase:")?;
    let (phase, msg) = rest.split_once(']')?;
    let phase = phase.trim();
    let msg = msg.trim();
    let phase_label = match phase {
        "discover" => "Discovery",
        "tool" | "tools" => "Tools",
        "fix" => "Applying changes",
        "synthesize" => "Final response",
        other if !other.is_empty() => other,
        _ => "Progress",
    };
    if msg.is_empty() {
        Some(phase_label.to_string())
    } else {
        Some(format!("{phase_label}: {msg}"))
    }
}

fn objective_status_text(line: &str) -> Option<String> {
    let trimmed = line.trim_start();
    let body = trimmed
        .strip_prefix("[objective:")?
        .strip_suffix(']')?
        .trim();
    if body.is_empty() {
        return None;
    }
    Some(format!("Objective: {body}"))
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
    let running_tools: Vec<&LiveTool> = state
        .live_tools
        .iter()
        .filter(|tool| tool.running)
        .collect();
    if !running_tools.is_empty() {
        if running_tools.len() == 1 {
            return format!("running {}", running_tools[0].name);
        }
        return format!("running {} tools", running_tools.len());
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

fn legacy_pseudo_tool_protocol_redact_lines(text: &str) -> String {
    let mut redacted = false;
    let mut lines = Vec::new();
    for line in text.lines() {
        if text_line_looks_like_pseudo_tool_syntax(line) {
            redacted = true;
            lines.push(pseudo_tool_redaction_marker().to_string());
        } else {
            lines.push(line.to_string());
        }
    }
    if !redacted {
        return text.to_string();
    }
    let mut out = lines.join("\n");
    if text.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn pseudo_tool_protocol_text_for_context(text: &str, context_mode: ContextMode) -> String {
    if context_mode.is_frugal() {
        redact_pseudo_tool_protocol_text(text)
    } else {
        legacy_pseudo_tool_protocol_redact_lines(text)
    }
}

fn sanitize_live_indicator_detail(detail: &str, context_mode: ContextMode) -> String {
    let sanitized = sanitize_display_text(detail);
    pseudo_tool_protocol_text_for_context(&sanitized, context_mode)
}

fn live_detail_line(
    detail: String,
    color: Color,
    max_cells: usize,
    context_mode: ContextMode,
) -> Line<'static> {
    Line::from(vec![
        Span::styled("  ↳ ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            clamp_chars(
                &sanitize_live_indicator_detail(&detail, context_mode),
                max_cells,
            ),
            Style::default().fg(color),
        ),
    ])
}

fn live_thinking_detail_line(
    detail: String,
    max_cells: usize,
    context_mode: ContextMode,
) -> Line<'static> {
    let style = Style::default().fg(Color::Gray).bg(THINKING_BG);
    Line::from(vec![
        Span::styled(
            "│ ",
            Style::default().fg(Color::Indexed(244)).bg(THINKING_BG),
        ),
        Span::styled(
            clamp_chars(
                &sanitize_live_indicator_detail(&detail, context_mode),
                max_cells,
            ),
            style,
        ),
    ])
}

fn live_indicator_todo_detail(state: &TuiState, max_cells: usize) -> Option<Line<'static>> {
    state
        .todo_progress
        .as_ref()
        .map(todo_progress_label)
        .map(|detail| live_detail_line(detail, Color::Green, max_cells, state.context_mode))
}

fn live_indicator_detail(state: &TuiState, width: u16) -> Option<Line<'static>> {
    if width == 0 || state.pending_perm.is_some() || state.pending_local_auth.is_some() {
        return None;
    }
    let max_cells = width.saturating_sub(4) as usize;
    if !state.streaming_text.is_empty() {
        let rendered_text =
            pseudo_tool_protocol_text_for_context(&state.streaming_text, state.context_mode);
        let tail = rendered_text
            .lines()
            .last()
            .unwrap_or(rendered_text.as_str())
            .trim();
        if !tail.is_empty() {
            return Some(Line::from(vec![
                Span::styled("  ▸ ", Style::default().fg(Color::Blue)),
                Span::styled(
                    clamp_chars(
                        &sanitize_live_indicator_detail(tail, state.context_mode),
                        max_cells,
                    ),
                    Style::default(),
                ),
            ]));
        }
    }
    let running_tools: Vec<&LiveTool> = state
        .live_tools
        .iter()
        .filter(|tool| tool.running)
        .collect();
    if let Some(tool) = running_tools.iter().find(|tool| !tool.summary.is_empty()) {
        let mut detail = tool.summary.clone();
        if tool.name == "bash"
            && state
                .backend_outputs
                .iter()
                .any(|output| output.call_id == tool.call_id)
        {
            detail.push_str(" · Ctrl+B backend");
        }
        return Some(live_detail_line(
            detail,
            Color::Reset,
            max_cells,
            state.context_mode,
        ));
    }
    if state.verbose && !state.streaming_thinking.is_empty() {
        let rendered_thinking =
            pseudo_tool_protocol_text_for_context(&state.streaming_thinking, state.context_mode);
        let tail = rendered_thinking
            .lines()
            .last()
            .unwrap_or(rendered_thinking.as_str())
            .trim();
        if !tail.is_empty() {
            let max_cells = width.saturating_sub(2) as usize;
            return Some(live_thinking_detail_line(
                tail.to_string(),
                max_cells,
                state.context_mode,
            ));
        }
    }
    live_indicator_todo_detail(state, max_cells)
}

fn display_busy_status(status: String) -> String {
    match status.as_str() {
        "thinking" => "Thinking".to_string(),
        "responding" => "Responding".to_string(),
        s if s.starts_with("running ") => format!("Running {}", s.trim_start_matches("running ")),
        s if s.starts_with("retry ") => "Waiting to retry".to_string(),
        s if s.starts_with("compacting") => "Summarizing".to_string(),
        _ => status,
    }
}

fn context_usage(state: &TuiState) -> Option<(u64, u64, u64, Color)> {
    let window = if state.context_window_tokens > 0 {
        state.context_window_tokens
    } else {
        model_context_window(&state.model)
    };
    if window == 0 {
        return None;
    }
    let ctx_used = if state.last_turn_context_tokens > 0 {
        state.last_turn_context_tokens
    } else if state.history_chars > 0 {
        (state.history_chars / 4).max(1)
    } else {
        0
    };
    let pct = ((ctx_used as f64 / window as f64) * 100.0)
        .clamp(0.0, 100.0)
        .round() as u64;
    let color = if pct >= 90 {
        Color::Red
    } else if pct >= 70 {
        Color::Yellow
    } else {
        Color::Cyan
    };
    Some((ctx_used, window, pct, color))
}

fn context_bar(pct: u64, cells: usize) -> String {
    let filled = (((pct.min(100) as usize) * cells + 50) / 100).min(cells);
    format!(
        "{}{}",
        "█".repeat(filled),
        "░".repeat(cells.saturating_sub(filled))
    )
}

fn status_spans(state: &TuiState) -> Vec<Span<'_>> {
    let (marker, marker_color) = if state.agent_busy {
        let c = SPINNER_FRAMES[(state.frame_count % SPINNER_FRAMES.len() as u64) as usize];
        (format!("{c} "), Color::Yellow)
    } else {
        ("● ".to_string(), Color::Green)
    };
    let mut spans = vec![Span::styled(marker, Style::default().fg(marker_color))];

    spans.push(Span::styled(
        home_tilde(&state.sandbox),
        Style::default().fg(Color::Green),
    ));

    if let Some(branch) = &state.git_branch {
        spans.push(Span::styled(" | ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            format!("Branch({})", clamp_chars(branch, 28)),
            Style::default().fg(Color::Magenta),
        ));
    }

    let model_label = status_model_label(&state.model);
    spans.push(Span::styled(" │ ", Style::default().fg(Color::DarkGray)));
    spans.push(Span::styled(model_label, Style::default().fg(Color::Cyan)));
    spans.push(Span::raw(" "));
    spans.push(Span::styled(
        state.thinking_effort.as_str().to_string(),
        Style::default().fg(Color::Magenta),
    ));

    match state.approval_profile {
        ApprovalProfile::Ask | ApprovalProfile::Always => {}
        profile => {
            spans.push(Span::styled(
                " │ approval: ",
                Style::default().fg(Color::DarkGray),
            ));
            spans.push(Span::styled(
                profile.as_str().to_string(),
                Style::default().fg(Color::Yellow),
            ));
        }
    }

    if let Some(focus) = &state.active_focus {
        spans.push(Span::styled(
            " │ focus ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!("{} {}", clamp_chars(&focus.selection, 18), focus.mode),
            Style::default().fg(Color::Yellow),
        ));
    }

    if let Some((_, _, pct, color)) = context_usage(state) {
        spans.push(Span::styled(
            " │ Ctx ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled("[", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(
            context_bar(pct, CONTEXT_BAR_CELLS),
            Style::default().fg(color),
        ));
        spans.push(Span::styled("] ", Style::default().fg(Color::DarkGray)));
        spans.push(Span::styled(format!("{pct}%"), Style::default().fg(color)));
    }

    if state.show_status_details {
        spans.push(Span::styled(
            " │ details",
            Style::default().fg(Color::DarkGray),
        ));
    }
    if state.show_inspector {
        spans.push(Span::styled(
            " │ inspector",
            Style::default().fg(Color::Yellow),
        ));
    }

    spans
}

fn status_detail_spans(state: &TuiState) -> Vec<Span<'_>> {
    let usage = &state.usage;
    let mut spans = vec![Span::styled(
        "Tokens: ",
        Style::default().fg(Color::DarkGray),
    )];
    let detail = if usage.cached_input_tokens() > 0 {
        format!(
            "input {} (new {} · cache r {} w {}) · out {} · ${:.4}",
            format_count(usage.total_input_tokens()),
            format_count(usage.actual_input_tokens()),
            format_count(usage.cache_read),
            format_count(usage.cache_create),
            format_count(usage.output),
            usage.estimated_cost_usd(),
        )
    } else {
        format!(
            "input {} · out {} · ${:.4}",
            format_count(usage.total_input_tokens()),
            format_count(usage.output),
            usage.estimated_cost_usd(),
        )
    };
    spans.push(Span::styled(detail, Style::default().fg(Color::Gray)));

    if let Some((ctx_used, window, pct, color)) = context_usage(state) {
        spans.push(Span::styled(
            " · ctx ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            format!(
                "{}/{} ({pct}%)",
                format_count(ctx_used),
                format_count(window)
            ),
            Style::default().fg(color),
        ));
    }

    let ext = state.external_telemetry;
    let ext_total = ext.dedupe_hits
        + ext.similarity_blocks
        + ext.circuit_breaker_trips
        + ext.partial_delivery_hints
        + ext.http_retries
        + ext.empty_tool_call_hints;
    if ext_total > 0 {
        spans.push(Span::styled(
            " · ext",
            Style::default().fg(Color::LightBlue),
        ));
        let counters = [
            (ext.dedupe_hits, "d", Color::Cyan),
            (ext.circuit_breaker_trips, "cb", Color::Yellow),
            (ext.similarity_blocks, "sg", Color::Magenta),
            (ext.partial_delivery_hints, "ph", Color::Green),
            (ext.http_retries, "rt", Color::DarkGray),
            (ext.empty_tool_call_hints, "et", Color::Red),
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

    if !state.auth_source.is_empty() {
        spans.push(Span::styled(
            " · auth ",
            Style::default().fg(Color::DarkGray),
        ));
        spans.push(Span::styled(
            state.auth_source.clone(),
            Style::default().fg(Color::DarkGray),
        ));
    }

    spans
}

fn status_model_label(model: &str) -> String {
    if let Some(rest) = model.strip_prefix("gpt-") {
        format!("GPT-{rest}")
    } else if let Some(rest) = model.strip_prefix("glm-") {
        format!("GLM-{rest}")
    } else {
        model.to_string()
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
    (state.pending_perm.is_some() && !matches!(item, Line_::PermissionPrompt { .. }))
        || state.pending_local_auth.is_some()
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

fn parse_work_map_focus_label(text: &str) -> Option<ActiveFocusLabel> {
    let first = text.lines().next()?.trim();
    let body = first.strip_prefix("[dext focus ")?.strip_suffix(']')?;
    let (selection, mode) = body.split_once(" mode=")?;
    Some(ActiveFocusLabel {
        selection: selection.trim().to_string(),
        mode: mode.trim().to_string(),
    })
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

fn strip_ansi_escapes(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] == 0x1b {
            i += 1;
            if i < bytes.len() && bytes[i] == b'[' {
                i += 1;
                while i < bytes.len() {
                    let b = bytes[i];
                    i += 1;
                    if (0x40..=0x7e).contains(&b) {
                        break;
                    }
                }
                continue;
            }
            continue;
        }
        if let Some(ch) = text[i..].chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

/// Parse basic ANSI SGR codes into ratatui spans. Recognizes bold (`1`),
/// dim/faint (`2`), and foreground colors (`30`-`37`, esp. cyan `36`). Unset
/// attributes use `Color::Reset`. Lines with no escapes produce a single plain
/// span. This lets ANSI-styled list output render correctly in the TUI.
fn ansi_to_spans(text: &str) -> Vec<Span<'static>> {
    let bytes = text.as_bytes();
    let mut spans: Vec<Span<'static>> = Vec::new();
    let mut buf = String::new();
    let mut style = Style::default();
    let mut i = 0usize;

    macro_rules! flush {
        () => {
            if !buf.is_empty() {
                spans.push(Span::styled(std::mem::take(&mut buf), style));
            }
        };
    }

    while i < bytes.len() {
        if bytes[i] == 0x1b {
            if i + 1 < bytes.len() && bytes[i + 1] == b'[' {
                flush!();
                i += 2;
                let start = i;
                while i < bytes.len() && !((0x40..=0x7e).contains(&bytes[i])) {
                    i += 1;
                }
                if i < bytes.len() && bytes[i] == b'm' {
                    let params = &text[start..i];
                    for param in params.split(';') {
                        match param.trim() {
                            "" | "0" => style = Style::default(),
                            "1" => style = style.add_modifier(Modifier::BOLD),
                            "2" => style = style.add_modifier(Modifier::DIM),
                            "30" => style = style.fg(Color::Black),
                            "31" => style = style.fg(Color::Red),
                            "32" => style = style.fg(Color::Green),
                            "33" => style = style.fg(Color::Yellow),
                            "34" => style = style.fg(Color::Blue),
                            "35" => style = style.fg(Color::Magenta),
                            "36" => style = style.fg(Color::Cyan),
                            "37" => style = style.fg(Color::Gray),
                            "90" => style = style.fg(Color::DarkGray),
                            _ => {}
                        }
                    }
                }
                if i < bytes.len() {
                    i += 1;
                }
            } else {
                i += 1;
            }
        } else {
            let ch = text[i..].chars().next().unwrap_or('\0');
            buf.push(ch);
            i += ch.len_utf8();
        }
    }
    flush!();
    if spans.is_empty() {
        spans.push(Span::raw(strip_ansi_escapes(text)));
    }
    spans
}

/// True when text contains ANSI CSI escape sequences.
fn has_ansi(text: &str) -> bool {
    text.as_bytes().windows(2).any(|w| w == b"\x1b[")
}

fn sanitize_display_text(text: &str) -> String {
    let text = strip_ansi_escapes(text);
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

/// A single base char plus any trailing zero-width continuation chars (VS16,
/// ZWJ, combining marks). Terminal display width is measured on the whole slice
/// via `UnicodeWidthStr`, which is correct for every multi-codepoint symbol
/// class — unlike per-char `UnicodeWidthChar::width`, which misses sequences
/// like `⚙️` (⚙=1 + VS16) and would split a cluster across a clip boundary.
#[derive(Debug)]
pub(crate) struct DisplayCluster {
    pub byte_start: usize,
    pub byte_len: usize,
    pub width: usize,
}

pub(crate) fn display_clusters(s: &str) -> Vec<DisplayCluster> {
    let mut out = Vec::new();
    let mut start: Option<usize> = None;
    let mut prev_was_zwj = false;
    let mut ri_run = 0u32;
    for (i, ch) in s.char_indices() {
        let w = unicode_width::UnicodeWidthChar::width(ch).unwrap_or(0);
        let is_zwj = ch == '\u{200D}';
        let is_ri = ('\u{1F1E6}'..='\u{1F1FF}').contains(&ch);
        // This char continues the cluster at `start` when it is a zero-width
        // continuation (combining mark / VS16 / ZWJ), a base char extending a
        // ZWJ sequence (👨‍👩‍👧), or the second regional indicator of a flag pair.
        let continues = start.is_some()
            && ((w == 0 && !ch.is_control()) || prev_was_zwj || (is_ri && ri_run % 2 == 1));
        if !continues {
            if let Some(bs) = start.take() {
                let slice = &s[bs..i];
                out.push(DisplayCluster {
                    byte_start: bs,
                    byte_len: i - bs,
                    width: unicode_width::UnicodeWidthStr::width(slice),
                });
            }
            start = Some(i);
        }
        ri_run = if is_ri { ri_run + 1 } else { 0 };
        prev_was_zwj = is_zwj;
    }
    if let Some(bs) = start {
        let slice = &s[bs..];
        out.push(DisplayCluster {
            byte_start: bs,
            byte_len: slice.len(),
            width: unicode_width::UnicodeWidthStr::width(slice),
        });
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
    let clusters = display_clusters(s);
    let mut cells = 0usize;
    let mut byte_end = 0usize;
    for c in &clusters {
        if cells + c.width > max_cells {
            break;
        }
        cells += c.width;
        byte_end = c.byte_start + c.byte_len;
    }
    if byte_end >= s.len() {
        return s.to_string();
    }
    if max_cells == 1 {
        return "…".to_string();
    }
    let mut out = String::new();
    let mut truncated_cells = 0usize;
    for c in &clusters {
        if truncated_cells + c.width + 1 > max_cells {
            break;
        }
        out.push_str(&s[c.byte_start..c.byte_start + c.byte_len]);
        truncated_cells += c.width;
    }
    out.push('…');
    out
}

fn clamp_chars_with_indicator(s: &str, max_cells: usize) -> String {
    if max_cells == 0 {
        return String::new();
    }

    if text_width(s) <= max_cells {
        return s.to_string();
    }
    clamp_chars_plain(s, max_cells)
}

#[cfg(test)]
fn clamp_chars_with_hint(s: &str, max_cells: usize) -> String {
    clamp_chars_with_indicator(s, max_cells)
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
    let mut cursor_set = false;

    for c in display_clusters(input) {
        let end = c.byte_start + c.byte_len;
        if !cursor_set && clamped <= end {
            cursor_row = row;
            cursor_col = if clamped <= c.byte_start {
                col
            } else {
                col + c.width
            };
            cursor_set = true;
        }
        let cluster = &input[c.byte_start..end];
        if cluster.starts_with('\n') {
            lines.push(String::new());
            row += 1;
            col = 0;
            continue;
        }
        if col + c.width > cols {
            lines.push(String::new());
            row += 1;
            col = 0;
        }
        lines[row].push_str(cluster);
        col += c.width;
    }

    if !cursor_set {
        cursor_row = row;
        cursor_col = col;
    }

    (lines, cursor_row, cursor_col)
}

fn input_editor_text(state: &TuiState) -> Cow<'_, str> {
    if let Some(preview) = &state.input_display_override {
        Cow::Borrowed(preview.as_str())
    } else if state.input.is_empty() {
        Cow::Borrowed("Type a request…")
    } else {
        Cow::Borrowed(state.input.as_str())
    }
}

fn input_display_cursor(state: &TuiState) -> usize {
    if state.input_display_override.is_some() {
        input_editor_text(state).len()
    } else {
        state.cursor
    }
}

fn input_panel_height(state: &TuiState, area_height: u16, area_width: u16) -> u16 {
    let available = area_height.saturating_sub(1).max(1);
    let min_panel = 3.min(available);
    let ratio_cap = ((area_height as f32) * 0.5).round() as u16;
    let max_panel = INPUT_MAX_PANEL_ROWS
        .min(ratio_cap.max(min_panel))
        .min(available);

    let cols = area_width.saturating_sub(2).max(1) as usize;
    let editor_text = input_editor_text(state);
    let (wrapped, _, _) =
        wrap_input_visual(editor_text.as_ref(), input_display_cursor(state), cols);
    let text_rows = wrapped.len().max(1) as u16;
    let drawer_rows = work_map_drawer_height(state, area_width) as u16;
    let desired = text_rows.saturating_add(2).saturating_add(drawer_rows);
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

#[cfg(test)]
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

#[cfg(test)]
fn abstract_input_for_display(input: &str) -> Option<String> {
    let paragraphs = split_display_paragraphs(input);
    let mut spans = Vec::new();
    let mut offset = 0usize;

    for para in paragraphs {
        let leading = input[offset..]
            .find(para)
            .map(|relative| offset + relative)
            .unwrap_or(offset);
        let end = leading.saturating_add(para.len());
        let words = count_words(para);
        if words > PASTE_WORD_THRESHOLD {
            spans.push(InputPreviewSpan {
                start: leading,
                end,
                words,
            });
        }
        offset = end;
    }

    abstract_input_with_spans(input, &spans)
}

fn abstract_input_with_spans(input: &str, spans: &[InputPreviewSpan]) -> Option<String> {
    if spans.is_empty() {
        return None;
    }

    let mut sorted = spans
        .iter()
        .filter(|span| {
            span.start < span.end
                && span.end <= input.len()
                && input.is_char_boundary(span.start)
                && input.is_char_boundary(span.end)
        })
        .cloned()
        .collect::<Vec<_>>();
    sorted.sort_by_key(|span| span.start);

    let mut out = String::with_capacity(input.len().min(512));
    let mut cursor = 0usize;
    let mut paste_idx = 0usize;
    for span in sorted {
        if span.start < cursor {
            continue;
        }
        out.push_str(&input[cursor..span.start]);
        paste_idx += 1;
        out.push_str(&format!(
            "[paste #{paste_idx} +{} words hidden]",
            span.words
        ));
        cursor = span.end;
    }
    out.push_str(&input[cursor..]);
    Some(out)
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

fn strip_markdown_markers(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut i = 0usize;
    while i < s.len() {
        let rest = &s[i..];
        if rest.starts_with("**") || rest.starts_with("__") {
            i += 2;
            continue;
        }
        if rest.starts_with('`') {
            i += 1;
            continue;
        }
        if let Some(ch) = rest.chars().next() {
            out.push(ch);
            i += ch.len_utf8();
        } else {
            break;
        }
    }
    out
}

fn clean_markdown_text(mut text: Text<'static>) -> Text<'static> {
    for line in &mut text.lines {
        for span in &mut line.spans {
            let cleaned = strip_markdown_markers(span.content.as_ref());
            span.content = cleaned.into();
        }
        let rendered = rendered_line_text(line);
        let trimmed = rendered.trim_start();
        let mut split_idx = 0usize;
        let mut heading_marks = 0usize;
        for (idx, ch) in trimmed.char_indices() {
            if ch == '#' {
                heading_marks += 1;
                split_idx = idx + ch.len_utf8();
            } else {
                break;
            }
        }
        if (1..=6).contains(&heading_marks) && trimmed[split_idx..].starts_with(' ') {
            let cleaned = trimmed[split_idx..].trim_start().to_string();
            line.spans = vec![Span::styled(
                cleaned,
                line.style
                    .patch(Style::default().add_modifier(Modifier::BOLD)),
            )];
        }
    }
    text
}

fn is_plain_text_code_fence_opener(line: &Line<'_>) -> bool {
    let text = rendered_line_text(line);
    let trimmed = text.trim();
    if trimmed == "```" || trimmed == "~~~" {
        return true;
    }
    let Some(rest) = trimmed
        .strip_prefix("```")
        .or_else(|| trimmed.strip_prefix("~~~"))
    else {
        return false;
    };
    !rest.trim().is_empty()
}

fn is_code_fence_closer(line: &Line<'_>) -> bool {
    let trimmed = rendered_line_text(line).trim().to_string();
    trimmed == "```" || trimmed == "~~~"
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

fn normalized_table_cell_text(cell: &str) -> String {
    let no_ansi = strip_ansi_escapes(cell);
    strip_markdown_markers(&no_ansi)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

fn status_word_value(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "yes" | "y" | "true" | "pass" | "passed" | "ok" | "success" | "succeeded" => Some(true),
        "no" | "n" | "false" | "fail" | "failed" | "error" | "errored" => Some(false),
        _ => None,
    }
}

fn status_icon_value(text: &str) -> Option<(bool, &str)> {
    let trimmed = text.trim_start();
    if let Some(rest) = trimmed
        .strip_prefix("✅")
        .or_else(|| trimmed.strip_prefix('✓'))
        .or_else(|| trimmed.strip_prefix('✔'))
    {
        Some((true, rest.trim_start()))
    } else if let Some(rest) = trimmed
        .strip_prefix("❌")
        .or_else(|| trimmed.strip_prefix('✗'))
        .or_else(|| trimmed.strip_prefix('✘'))
        .or_else(|| trimmed.strip_prefix('×'))
    {
        Some((false, rest.trim_start()))
    } else {
        None
    }
}

fn status_token(value: bool) -> String {
    if value { "PASS" } else { "FAIL" }.to_string()
}

fn clean_table_status_cell(cell: &str) -> String {
    let text = normalized_table_cell_text(cell);
    if let Some(value) = status_word_value(&text) {
        return status_token(value);
    }
    if let Some((icon_value, rest)) = status_icon_value(&text) {
        let value = status_word_value(rest).unwrap_or(icon_value);
        if value != icon_value {
            return status_token(false);
        }
        return status_token(value);
    }
    text
}

fn normalize_table_status_cells(table: &mut ParsedTable) {
    if table.header_rows == 0 {
        return;
    }
    let Some(header) = table.rows.first() else {
        return;
    };
    let status_cols = header
        .iter()
        .enumerate()
        .filter_map(|(idx, cell)| {
            let lower = normalized_table_cell_text(cell).to_ascii_lowercase();
            matches!(lower.as_str(), "result" | "status" | "pass" | "ok").then_some(idx)
        })
        .collect::<Vec<_>>();
    for row in table.rows.iter_mut().skip(table.header_rows) {
        for idx in &status_cols {
            if let Some(cell) = row.get_mut(*idx) {
                *cell = clean_table_status_cell(cell);
            }
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

fn table_header_style(base_style: Style) -> Style {
    base_style.patch(Style::default().add_modifier(Modifier::BOLD))
}

fn buffer_to_lines(buffer: &ratatui::buffer::Buffer, area: Rect) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for y in 0..area.height {
        let mut spans = Vec::new();
        let mut text_line = String::new();
        let mut current_style: Option<Style> = None;
        let mut hidden_by_wide_symbol = 0usize;
        for x in 0..area.width {
            let cell = &buffer[(x, y)];
            if hidden_by_wide_symbol > 0 {
                hidden_by_wide_symbol -= 1;
                continue;
            }
            let symbol = cell.symbol();
            if symbol.is_empty() {
                continue;
            }
            hidden_by_wide_symbol = unicode_width::UnicodeWidthStr::width(symbol).saturating_sub(1);
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

const TABLE_CELL_PADDING: usize = 1;
const TABLE_CELL_MIN_WIDTH: usize = 3;
const TABLE_CELL_SOFT_MAX_WIDTH: usize = 42;
const TABLE_RECORD_FALLBACK_MIN_WIDTH: usize = 8;

fn table_cell_text(cell: &str) -> String {
    normalized_table_cell_text(cell)
}

fn table_grid_total_width(widths: &[usize]) -> usize {
    if widths.is_empty() {
        return 0;
    }
    widths.iter().sum::<usize>() + widths.len() * (TABLE_CELL_PADDING * 2 + 1) + 1
}

fn table_uncapped_widths(table: &ParsedTable) -> Vec<usize> {
    let col_count = table_column_count(table);
    let mut widths = vec![TABLE_CELL_MIN_WIDTH; col_count];
    for row in &table.rows {
        for (ci, cell) in row.iter().enumerate().take(col_count) {
            widths[ci] = widths[ci].max(text_width(&table_cell_text(cell)));
        }
    }
    widths
}

fn table_grid_widths(
    table: &ParsedTable,
    max_total_width: usize,
) -> Option<(Vec<usize>, Vec<usize>)> {
    let col_count = table_column_count(table);
    if col_count == 0 {
        return None;
    }
    let min_total = table_grid_total_width(&vec![TABLE_CELL_MIN_WIDTH; col_count]);
    if max_total_width < min_total {
        return None;
    }

    let uncapped = table_uncapped_widths(table);
    let content_budget =
        max_total_width.saturating_sub(col_count * (TABLE_CELL_PADDING * 2 + 1) + 1);
    let soft_max = match col_count {
        0 => TABLE_CELL_MIN_WIDTH,
        1 => content_budget.max(TABLE_CELL_MIN_WIDTH),
        2 => TABLE_CELL_SOFT_MAX_WIDTH
            .max(24)
            .min(content_budget.max(TABLE_CELL_MIN_WIDTH)),
        _ => TABLE_CELL_SOFT_MAX_WIDTH.min(content_budget.max(TABLE_CELL_MIN_WIDTH)),
    };
    let mut widths = uncapped
        .iter()
        .map(|width| (*width).clamp(TABLE_CELL_MIN_WIDTH, soft_max))
        .collect::<Vec<_>>();

    while widths.iter().sum::<usize>() > content_budget {
        let Some((idx, _)) = widths
            .iter()
            .enumerate()
            .filter(|(_, width)| **width > TABLE_CELL_MIN_WIDTH)
            .max_by_key(|(_, width)| **width)
        else {
            break;
        };
        widths[idx] -= 1;
    }

    (widths.iter().sum::<usize>() <= content_budget).then_some((widths, uncapped))
}

fn table_should_render_records(
    table: &ParsedTable,
    widths: &[usize],
    uncapped: &[usize],
    max_total_width: usize,
) -> bool {
    let col_count = widths.len();
    let body_rows = table
        .rows
        .len()
        .saturating_sub(table.header_rows.min(table.rows.len()));
    if body_rows == 0 {
        return false;
    }
    if col_count >= 5 && max_total_width < 96 {
        return true;
    }
    if col_count >= 3 && max_total_width < 40 {
        return true;
    }
    widths
        .iter()
        .zip(uncapped.iter())
        .any(|(width, natural)| *width <= 6 && *natural > width.saturating_mul(3))
}

fn table_border_line(
    widths: &[usize],
    left: char,
    join: char,
    right: char,
    border_style: Style,
) -> Line<'static> {
    let mut text = String::new();
    text.push(left);
    for (idx, width) in widths.iter().enumerate() {
        text.push_str(&"─".repeat(width.saturating_add(TABLE_CELL_PADDING * 2)));
        text.push(if idx + 1 == widths.len() { right } else { join });
    }
    Line::from(Span::styled(text, border_style))
}

fn table_wrap_cell(cell: &str, width: usize) -> Vec<String> {
    let text = table_cell_text(cell);
    if text.is_empty() {
        vec![String::new()]
    } else {
        wrap_plain_words_visual(&text, width.max(1))
    }
}

fn table_alignment_padding(
    alignment: TableColumnAlignment,
    content_width: usize,
    target_width: usize,
) -> (usize, usize) {
    let remaining = target_width.saturating_sub(content_width);
    match alignment {
        TableColumnAlignment::Left => (0, remaining),
        TableColumnAlignment::Center => (remaining / 2, remaining.saturating_sub(remaining / 2)),
        TableColumnAlignment::Right => (remaining, 0),
    }
}

fn render_table_grid_row(
    row: &[String],
    widths: &[usize],
    alignments: &[TableColumnAlignment],
    row_style: Style,
    border_style: Style,
) -> Vec<Line<'static>> {
    let wrapped = widths
        .iter()
        .enumerate()
        .map(|(ci, width)| table_wrap_cell(row.get(ci).map(String::as_str).unwrap_or(""), *width))
        .collect::<Vec<_>>();
    let row_height = wrapped.iter().map(Vec::len).max().unwrap_or(1).max(1);
    let mut lines = Vec::with_capacity(row_height);

    for line_idx in 0..row_height {
        let mut spans = vec![Span::styled("│".to_string(), border_style)];
        for (ci, width) in widths.iter().enumerate() {
            let content = wrapped[ci].get(line_idx).map(String::as_str).unwrap_or("");
            let alignment = alignments
                .get(ci)
                .copied()
                .unwrap_or(TableColumnAlignment::Left);
            let (left_pad, right_pad) =
                table_alignment_padding(alignment, text_width(content), *width);
            spans.push(Span::raw(" ".repeat(TABLE_CELL_PADDING + left_pad)));
            if !content.is_empty() {
                spans.push(Span::styled(content.to_string(), row_style));
            }
            spans.push(Span::raw(" ".repeat(right_pad + TABLE_CELL_PADDING)));
            spans.push(Span::styled("│".to_string(), border_style));
        }
        lines.push(Line::from(spans));
    }

    lines
}

fn table_record_rule(width: usize, left: char, right: char, border_style: Style) -> Line<'static> {
    let inner = width.saturating_sub(2);
    Line::from(Span::styled(
        format!("{left}{}{right}", "─".repeat(inner)),
        border_style,
    ))
}

fn table_record_line(
    content: &str,
    content_style: Style,
    width: usize,
    border_style: Style,
) -> Line<'static> {
    let inner = width.saturating_sub(4).max(1);
    let content_width = text_width(content).min(inner);
    let right_pad = inner.saturating_sub(content_width);
    Line::from(vec![
        Span::styled("│ ".to_string(), border_style),
        Span::styled(content.to_string(), content_style),
        Span::raw(" ".repeat(right_pad)),
        Span::styled(" │".to_string(), border_style),
    ])
}

fn render_table_records(
    table: &ParsedTable,
    base_style: Style,
    max_total_width: usize,
) -> Vec<Line<'static>> {
    let col_count = table_column_count(table);
    if col_count == 0 {
        return Vec::new();
    }
    if max_total_width < TABLE_RECORD_FALLBACK_MIN_WIDTH {
        return table
            .rows
            .iter()
            .flat_map(|row| {
                row.iter()
                    .map(|cell| Line::from(Span::styled(table_cell_text(cell), base_style)))
                    .collect::<Vec<_>>()
            })
            .collect();
    }

    let header_rows = table.header_rows.min(table.rows.len());
    let labels = if header_rows > 0 {
        (0..col_count)
            .map(|ci| {
                let label =
                    table_cell_text(table.rows[0].get(ci).map(String::as_str).unwrap_or(""));
                if label.is_empty() {
                    format!("Column {}", ci + 1)
                } else {
                    label
                }
            })
            .collect::<Vec<_>>()
    } else {
        (0..col_count)
            .map(|ci| format!("Column {}", ci + 1))
            .collect::<Vec<_>>()
    };

    let width = max_total_width;
    let inner = width.saturating_sub(4).max(1);
    let border_style = Style::default().fg(Color::DarkGray);
    let label_style = base_style.patch(Style::default().fg(Color::Gray));
    let mut lines = vec![table_record_rule(width, '┌', '┐', border_style)];
    let mut wrote_record = false;

    for row in table.rows.iter().skip(header_rows) {
        if wrote_record {
            lines.push(table_record_rule(width, '├', '┤', border_style));
        }
        wrote_record = true;
        let mut wrote_field = false;
        for ci in 0..col_count {
            let value = table_cell_text(row.get(ci).map(String::as_str).unwrap_or(""));
            if value.is_empty() {
                continue;
            }
            let label = labels.get(ci).map(String::as_str).unwrap_or("item");
            let text = format!("{label}: {value}");
            for (line_idx, wrapped) in wrap_plain_words_visual(&text, inner)
                .into_iter()
                .enumerate()
            {
                let style = if line_idx == 0 {
                    label_style
                } else {
                    base_style
                };
                lines.push(table_record_line(&wrapped, style, width, border_style));
            }
            wrote_field = true;
        }
        if !wrote_field {
            lines.push(table_record_line("—", base_style, width, border_style));
        }
    }

    if !wrote_record {
        let header = table
            .rows
            .first()
            .map(|row| {
                row.iter()
                    .map(|cell| table_cell_text(cell))
                    .collect::<Vec<_>>()
                    .join(" · ")
            })
            .unwrap_or_default();
        lines.push(table_record_line(
            &header,
            table_header_style(base_style),
            width,
            border_style,
        ));
    }
    lines.push(table_record_rule(width, '└', '┘', border_style));
    lines
}

fn render_table_lines(
    table: &ParsedTable,
    base_style: Style,
    max_total_width: usize,
) -> Vec<Line<'static>> {
    let Some((widths, uncapped)) = table_grid_widths(table, max_total_width) else {
        return render_table_records(table, base_style, max_total_width.max(1));
    };
    if table_should_render_records(table, &widths, &uncapped, max_total_width) {
        return render_table_records(table, base_style, max_total_width.max(1));
    }

    let border_style = Style::default().fg(Color::DarkGray);
    let header_rows = table.header_rows.min(table.rows.len());
    let mut lines = vec![table_border_line(&widths, '┌', '┬', '┐', border_style)];

    for (row_idx, row) in table.rows.iter().enumerate() {
        let row_style = if row_idx < header_rows {
            table_header_style(base_style)
        } else {
            base_style
        };
        lines.extend(render_table_grid_row(
            row,
            &widths,
            &table.alignments,
            row_style,
            border_style,
        ));
        if row_idx + 1 < table.rows.len() {
            lines.push(table_border_line(&widths, '├', '┼', '┤', border_style));
        }
    }

    lines.push(table_border_line(&widths, '└', '┴', '┘', border_style));
    lines
}

fn text_width(s: &str) -> usize {
    unicode_width::UnicodeWidthStr::width(strip_markdown_markers(&strip_ansi_escapes(s)).as_str())
}

fn transcript_content_rect(area: Rect) -> Rect {
    area
}

fn transcript_render_width(width: u16) -> u16 {
    width.saturating_sub(TRANSCRIPT_WRAP_GUARD_COLS).max(1)
}

fn is_table_separator_line(line: &str) -> bool {
    is_md_separator_row(line) || is_ascii_border_row(line)
}

fn has_table_marker(text: &str) -> bool {
    text.lines()
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

fn fence_info(line: &str) -> Option<(char, &str)> {
    let trimmed = line.trim_start();
    if let Some(rest) = trimmed.strip_prefix("```") {
        Some(('`', rest.trim()))
    } else {
        trimmed.strip_prefix("~~~").map(|rest| ('~', rest.trim()))
    }
}

fn markdown_table_fence_delimiter(line: &str) -> Option<char> {
    let (delimiter, info) = fence_info(line)?;
    let lower = info.to_ascii_lowercase();
    let lang = lower
        .trim_start_matches('{')
        .trim_start_matches('.')
        .split(|ch: char| ch.is_whitespace() || ch == '}' || ch == ',')
        .next()
        .unwrap_or_default();
    matches!(lang, "md" | "markdown").then_some(delimiter)
}

fn find_fence_close(lines: &[&str], start: usize, delimiter: char) -> Option<usize> {
    lines
        .iter()
        .enumerate()
        .skip(start.saturating_add(1))
        .find_map(|(idx, line)| (fence_delimiter(line) == Some(delimiter)).then_some(idx))
}

fn has_parseable_table(lines: &[&str], start: usize, end: usize) -> bool {
    let body = &lines[start..end];
    let mut i = 0usize;
    while i < body.len() {
        if parse_markdown_table_block(body, i)
            .or_else(|| parse_ascii_table_block(body, i))
            .is_some()
        {
            return true;
        }
        i += 1;
    }
    false
}

fn push_table_blocks<'a>(
    blocks: &mut Vec<EitherBlock<'a>>,
    raw_lines: &'a [&'a str],
    start: usize,
    end: usize,
) {
    let mut markdown_start = start;
    let mut i = start;
    while i < end {
        let parsed = parse_markdown_table_block(raw_lines, i)
            .or_else(|| parse_ascii_table_block(raw_lines, i));
        if let Some((mut table, consumed)) = parsed
            && i + consumed <= end
        {
            normalize_table_status_cells(&mut table);
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
    if markdown_start < end {
        blocks.push(EitherBlock::Markdown(&raw_lines[markdown_start..end]));
    }
}

fn markdown_text(body: &str, base_style: Style, max_total_width: u16) -> Text<'static> {
    let sanitized = sanitize_display_text(body);
    let options = MarkdownOptions::new(DextMarkdownStyleSheet);
    if !has_table_marker(&sanitized) {
        return clean_markdown_text(hide_plain_text_code_fence_lines(
            text_to_static(from_str_with_options(&sanitized, &options)).style(base_style),
        ));
    }

    let raw_lines: Vec<&str> = sanitized.lines().collect();
    let mut blocks: Vec<EitherBlock> = Vec::new();
    let mut markdown_start = 0usize;
    let mut i = 0usize;

    while i < raw_lines.len() {
        if let Some(delim) = fence_delimiter(raw_lines[i]) {
            if let Some(close) = find_fence_close(&raw_lines, i, delim) {
                if markdown_table_fence_delimiter(raw_lines[i]).is_some()
                    && has_parseable_table(&raw_lines, i + 1, close)
                {
                    push_table_blocks(&mut blocks, &raw_lines, markdown_start, i);
                    push_table_blocks(&mut blocks, &raw_lines, i + 1, close);
                } else {
                    push_table_blocks(&mut blocks, &raw_lines, markdown_start, i);
                    blocks.push(EitherBlock::Markdown(&raw_lines[i..=close]));
                }
                i = close + 1;
                markdown_start = i;
                continue;
            }
            push_table_blocks(&mut blocks, &raw_lines, markdown_start, i);
            blocks.push(EitherBlock::Markdown(&raw_lines[i..]));
            markdown_start = raw_lines.len();
            break;
        }

        i += 1;
    }

    push_table_blocks(&mut blocks, &raw_lines, markdown_start, raw_lines.len());

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

    clean_markdown_text(hide_plain_text_code_fence_lines(Text::from(result_lines)))
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
    let prefix_w = text_width(prefix);
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
            let content = strip_markdown_markers(span.content.as_ref());
            let width = text_width(&content);
            if width <= remaining {
                remaining = remaining.saturating_sub(width);
                spans.push(Span::styled(content, span.style));
            } else {
                let clipped = clamp_chars_with_indicator(&content, remaining);
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
        .saturating_sub(text_width(prefix))
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
    let sanitized = strip_ansi_escapes(s);
    let s = sanitized.as_str();
    if max_cells == 0 || s.is_empty() {
        return (String::new(), s.to_string());
    }

    let mut cells = 0usize;
    let mut end = 0usize;
    for c in display_clusters(s) {
        if cells + c.width > max_cells {
            if end == 0 {
                let next = c.byte_start + c.byte_len;
                return ("…".to_string(), s[next..].to_string());
            }
            break;
        }
        cells += c.width;
        end = c.byte_start + c.byte_len;
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
        let content = strip_markdown_markers(span.content.as_ref());
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
            let content = strip_markdown_markers(span.content.as_ref());
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
        let cleaned = strip_markdown_markers(raw);
        for row in wrap_plain_words_visual(&cleaned, body_width) {
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), border_style),
                Span::styled(row, thinking_style),
            ]));
        }
    }
}

fn welcome_context_description(mode: ContextMode) -> &'static str {
    match mode {
        ContextMode::Standard => "default schemas and caps",
        ContextMode::Frugal => {
            "lean schemas, smaller toolset, smaller caps, deterministic compaction"
        }
        ContextMode::Tiny => "lean schemas, smallest caps, deterministic compaction",
    }
}

fn welcome_approval_value(profile: ApprovalProfile, auto_approved_count: usize) -> String {
    let label = match profile {
        ApprovalProfile::Always => "trust mode",
        ApprovalProfile::Ask => "ask",
        ApprovalProfile::AutoRead => "auto-read",
        ApprovalProfile::AutoWrite => "auto-write",
        ApprovalProfile::Never => "never",
    };
    let suffix = if auto_approved_count == 1 {
        "privileged tool runs without confirmation"
    } else {
        "privileged tools run without confirmation"
    };
    if profile == ApprovalProfile::Always {
        format!("⚠ {label} · {auto_approved_count} {suffix}")
    } else if auto_approved_count == 0 {
        label.to_string()
    } else {
        format!("{label} · {auto_approved_count} {suffix}")
    }
}

fn welcome_banner(
    sandbox: &str,
    model: &str,
    thinking_effort: ThinkingEffort,
    context_mode: ContextMode,
    approval_profile: ApprovalProfile,
    auto_approved_count: usize,
) -> String {
    format!(
        "🐺 Dext v{}\nsandbox\t{}\nmodel\t{} · {} reasoning\ncontext\t{} · {}\napproval\t{}\nkeys\tCtrl+C/Esc interrupt · Ctrl+D quit · ? help",
        env!("CARGO_PKG_VERSION"),
        clamp_chars(&home_tilde(sandbox), 96),
        clamp_chars(model, 40),
        thinking_effort.as_str(),
        context_mode.as_str(),
        welcome_context_description(context_mode),
        welcome_approval_value(approval_profile, auto_approved_count),
    )
}

fn push_welcome_banner_lines(lines: &mut Vec<Line<'static>>, banner: &str) {
    const LABEL_WIDTH: usize = 10;
    let mut raw_lines = banner.split('\n');
    let title = raw_lines.next().unwrap_or("🐺 Dext");
    let rows: Vec<(&str, &str)> = raw_lines.filter_map(|raw| raw.split_once('\t')).collect();
    let row_width = rows
        .iter()
        .map(|(label, value)| text_width(&format!("{label:<LABEL_WIDTH$}{value}")))
        .max()
        .unwrap_or(0)
        .max(68);
    let top_static_width = text_width("── ") + text_width(title) + 1;
    let rule_width = row_width.max(top_static_width + 12);
    let rule_style = Style::default().fg(Color::DarkGray);
    let title_style = Style::default();
    let top_fill = "─".repeat(rule_width.saturating_sub(top_static_width));
    lines.push(Line::from(vec![
        Span::styled("── ".to_string(), rule_style),
        Span::styled(title.to_string(), title_style),
        Span::styled(format!(" {top_fill}"), rule_style),
    ]));
    for (label, value) in rows {
        let warning = label == "approval" && value.contains('⚠');
        let row_style = if warning {
            Style::default().fg(Color::Yellow)
        } else {
            Style::default()
        };
        let label_style = if warning {
            row_style
        } else {
            Style::default().fg(Color::Green)
        };
        lines.push(Line::from(vec![
            Span::styled(format!("{label:<LABEL_WIDTH$}"), label_style),
            Span::styled(value.to_string(), row_style),
        ]));
    }
    lines.push(Line::from(vec![Span::styled(
        "─".repeat(rule_width),
        rule_style,
    )]));
}

fn line_to_text(item: &Line_, width: u16) -> Text<'static> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    match item {
        Line_::Banner(s) => {
            push_welcome_banner_lines(&mut lines, s);
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

            let summary_body = normalized_table_cell_text(tool_summary_body(name, summary));
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
                if let Some(label) = phase_status_text(trimmed) {
                    lines.push(Line::from(vec![Span::styled(
                        label,
                        Style::default()
                            .fg(Color::Indexed(242))
                            .add_modifier(Modifier::ITALIC),
                    )]));
                }
            } else if trimmed.starts_with("[objective:") {
                if let Some(label) = objective_status_text(trimmed) {
                    lines.push(Line::from(vec![Span::styled(
                        label,
                        Style::default()
                            .fg(Color::Indexed(242))
                            .add_modifier(Modifier::ITALIC),
                    )]));
                }
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
            } else if has_ansi(s) {
                // List output (packs/sessions/shelves) styled with ANSI codes:
                // render as styled spans without the dim-italic bullet treatment.
                for seg in s.split('\n') {
                    lines.push(Line::from(ansi_to_spans(seg)));
                }
            } else {
                let sanitized = sanitize_display_text(s);
                for seg in sanitized.split('\n') {
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
                .fg(Color::Indexed(215))
                .bg(STEERING_BG)
                .add_modifier(Modifier::ITALIC);
            let gutter_style = Style::default()
                .fg(Color::Indexed(214))
                .bg(STEERING_BG)
                .add_modifier(Modifier::BOLD);
            push_prefixed_wrapped_line(
                &mut lines,
                "┃ ",
                gutter_style,
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
    let mut style = if trimmed.starts_with("Session map")
        || trimmed.starts_with("Work map")
        || trimmed.starts_with("[dext")
    {
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
        WorkMapEventKind::Map => "Session map",
        WorkMapEventKind::Packet => "Packet",
        WorkMapEventKind::Focus => "Focus",
        WorkMapEventKind::Tracks => "Branches",
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
                "(static text)".to_string()
            } else {
                "↑/↓/PgUp/PgDn navigate · Enter inspect · f edit · b branch · z filter · Esc close"
                    .to_string()
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
    let clusters = display_clusters(line);
    let mut left = String::new();
    let mut left_cells = 0usize;
    for c in &clusters {
        if left_cells + c.width > left_target {
            break;
        }
        left.push_str(&line[c.byte_start..c.byte_start + c.byte_len]);
        left_cells += c.width;
    }
    let mut right_parts: Vec<&str> = Vec::new();
    let mut right_cells = 0usize;
    for c in clusters.iter().rev() {
        if right_cells + c.width > right_target {
            break;
        }
        right_parts.push(&line[c.byte_start..c.byte_start + c.byte_len]);
        right_cells += c.width;
    }
    right_parts.reverse();
    let right: String = right_parts.join("");
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
            .map(|s| text_width(s.content.as_ref()))
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
    let render_width = transcript_render_width(width);
    if let Line_::PermissionPrompt {
        tool,
        command,
        risk,
        ..
    } = item
    {
        let text = permission_prompt_text(tool, command, *risk, render_width);
        let height = text_visual_height(&text, render_width);
        return (text, height);
    }

    let key = line_cache_key(item);
    // The cache is keyed by content hash and the transcript only grows, so without a cap it
    // retains a rendered copy of every line ever inserted. A full clear is safe: this is pure
    // memoization and rebuilds repopulate lazily, touching each item at most once per pass.
    if state.render_cache.len() >= RENDER_CACHE_MAX_ENTRIES
        && !state.render_cache.contains_key(&key)
    {
        state.render_cache.clear();
    }
    let entry = state
        .render_cache
        .entry(key)
        .or_insert_with(|| CachedTranscriptRender {
            renders: HashMap::new(),
            heights: HashMap::new(),
        });

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
    let render_width = transcript_render_width(width);
    let (text, height) = cached_transcript_render(state, item, width);
    let tint_bg = match item {
        Line_::Thinking(_) => Some(THINKING_BG),
        Line_::Steering(_) => Some(STEERING_BG),
        _ => next_transcript_tint(item, tool_tint_parity),
    };
    terminal.insert_before(height, |buf| {
        let area = Rect {
            width: render_width.min(buf.area.width),
            ..buf.area
        };
        let para = Paragraph::new(text).wrap(Wrap { trim: false });
        Widget::render(para, area, buf);
        if let Some(bg) = tint_bg {
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
    width: u16,
) -> io::Result<()> {
    terminal.clear()?;
    // mem::take instead of clone: rebuilds fire on resize/expand and the transcript can be
    // large. Nothing in the insert path reads state.transcript, so loaning it out is safe.
    let items = std::mem::take(&mut state.transcript);
    sync_last_expandable(state, &items);
    let mut tool_tint_parity = false;
    for item in &items {
        insert_transcript_item(terminal, state, item, width, &mut tool_tint_parity)?;
    }
    state.transcript = items;
    state.tool_tint_parity = tool_tint_parity;
    state.transcript_rendered_width = width;
    state.transcript_needs_rebuild = false;
    Ok(())
}

fn transcript_pane_width(area_width: u16, area_height: u16, state: &TuiState) -> u16 {
    compute_layout(Rect::new(0, 0, area_width, area_height), state)
        .transcript_area
        .width
}

fn current_transcript_pane_width<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &TuiState,
) -> io::Result<u16> {
    terminal.autoresize()?;
    let size = terminal.size()?;
    Ok(transcript_pane_width(size.width, size.height, state))
}

fn flush_pending_insert<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    width: u16,
) -> io::Result<()> {
    if state.transcript_rendered_width != width && !state.transcript.is_empty() {
        state.transcript_needs_rebuild = true;
    }
    if state.transcript_needs_rebuild {
        rebuild_transcript(terminal, state, width)?;
    }

    let raw: Vec<Line_> = std::mem::take(&mut state.pending_insert);
    let mut items: Vec<Line_> = merge_consecutive_tools(raw);
    flush_prepared_items(terminal, state, &mut items, width)
}

fn flush_prepared_items<B: Backend>(
    terminal: &mut Terminal<B>,
    state: &mut TuiState,
    items: &mut Vec<Line_>,
    width: u16,
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

    let mut tool_tint_parity = state.tool_tint_parity;
    for item in items.iter() {
        insert_transcript_item(terminal, state, item, width, &mut tool_tint_parity)?;
    }
    state.tool_tint_parity = tool_tint_parity;
    state.transcript.append(items);
    state.transcript_rendered_width = width;
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
    let color = if state.pending_local_auth.is_some() {
        Color::Yellow
    } else {
        match state.approval_profile {
            ApprovalProfile::Always => TRUST_INPUT_BORDER,
            _ => Color::DarkGray,
        }
    };
    Style::default().fg(color)
}

fn input_hint_text(state: &TuiState) -> &'static str {
    if state.pending_local_auth.is_some() {
        "local auth prompt active · Enter submit · Esc cancel"
    } else if state.pending_perm.is_some() {
        "y once · a always · n deny"
    } else if state.work_map_is_active() {
        if state
            .work_map
            .as_ref()
            .is_some_and(|drawer| drawer.filter_input)
        {
            "type filter · Enter opens filtered map · Esc cancel"
        } else {
            "Enter inspect · f edit · b branch · z filter · Esc close"
        }
    } else if state.input_display_override.is_some() {
        "paste preview · Enter sends full input"
    } else {
        ""
    }
}

fn transcript_live_indicator_text(state: &TuiState, width: u16) -> Option<Text<'static>> {
    if width == 0
        || !state.agent_busy
        || state.pending_perm.is_some()
        || state.pending_local_auth.is_some()
    {
        return None;
    }
    let status = display_busy_status(derived_busy_status(state));
    let mut top = vec![
        Span::styled(
            SPINNER_FRAMES[(state.frame_count % SPINNER_FRAMES.len() as u64) as usize].to_string(),
            Style::default().fg(Color::Yellow),
        ),
        Span::raw("  "),
        Span::styled(status, Style::default().fg(Color::DarkGray)),
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
    let content_area = transcript_content_rect(transcript_area);
    let content_width = transcript_render_width(content_area.width);
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
            transcript_area: content_area,
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
        content_area.x,
        content_area
            .y
            .saturating_add(transcript_area.height.saturating_sub(live_height)),
        content_area.width,
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
        transcript_area: content_area,
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

fn queue_local_auth_secret_request(
    state: &mut TuiState,
    tool: String,
    message: String,
    responder: std::sync::mpsc::SyncSender<LocalAuthSecret>,
) {
    let previous = state.pending_local_auth.take();
    clear_secret_string(&mut state.local_auth_input);
    if let Some(pending) = previous {
        let _ = pending.responder.send(LocalAuthSecret::Canceled);
    }
    state.status = format!("local auth for {tool}");
    state.push_debug_event(format!("local auth secret prompt · {tool}"));
    state.pending_local_auth = Some(PendingLocalAuth {
        tool,
        message,
        responder,
    });
}

fn help_overlay_text() -> Text<'static> {
    let keymap_rows: &[(&str, &str)] = &[
        ("Enter", "submit prompt"),
        ("Shift+Enter / Alt+Enter", "insert newline"),
        ("Ctrl+B", "open backend output viewer while bash runs"),
        ("Ctrl+O", "toggle last tool output"),
        ("Ctrl+T", "toggle token/status details"),
        ("Paste", "multi-line paste is inserted without auto-submit"),
        ("Esc", "clear input / close this help"),
        ("Ctrl+C", "interrupt agent (twice = quit)"),
        (
            "Auth secrets",
            "never type sudo passwords here; use local auth prompts",
        ),
        ("Ctrl+D", "quit"),
        ("Ctrl+V", "toggle thinking detail"),
        ("Ctrl+I", "toggle inspector/debug pane"),
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
        ("input/new/cache/out", "exact token counters in details"),
        ("Ctx [████░░░░░░]", "last request context window usage"),
        ("Ctrl+T", "show exact token counters"),
        ("● / ⠋", "ready / busy spinner"),
        ("Branch(name)", "git branch in sandbox"),
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
    inspector_area: ratatui::layout::Rect,
}

fn inspector_width(area_width: u16) -> u16 {
    if area_width < 120 {
        0
    } else {
        (area_width / 3).clamp(34, 56)
    }
}

fn compute_layout(area: ratatui::layout::Rect, state: &TuiState) -> TuiLayout {
    let status_height = if state.show_status_details { 2 } else { 1 }.min(area.height);
    let input_height = input_panel_height(state, area.height, area.width);
    let chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Min(0),
            Constraint::Length(input_height),
            Constraint::Length(status_height),
        ])
        .split(area);

    let main_area = chunks[0];
    let inspector_width = if state.show_inspector {
        inspector_width(area.width).min(main_area.width.saturating_sub(40))
    } else {
        0
    };
    let (transcript_rect, inspector_rect) = if inspector_width > 0 {
        let horizontal = Layout::default()
            .direction(Direction::Horizontal)
            .constraints([Constraint::Min(40), Constraint::Length(inspector_width)])
            .split(main_area);
        (horizontal[0], horizontal[1])
    } else {
        (main_area, empty_rect(main_area))
    };

    TuiLayout {
        transcript_area: clip_rect(transcript_rect, area).unwrap_or_else(|| empty_rect(area)),
        input_area: clip_rect(chunks[1], area).unwrap_or_else(|| empty_rect(area)),
        status_area: clip_rect(chunks[2], area).unwrap_or_else(|| empty_rect(area)),
        inspector_area: clip_rect(inspector_rect, area).unwrap_or_else(|| empty_rect(area)),
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
        .map(|c| text_width(c.text.as_str()))
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
    let title = format!("▌ Session map  {selected_id}  {}/{}", selected + 1, total);
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
            &format!("  {scroll_hint}Enter inspect · f edit · b branch · z filter · Esc close"),
            inner_width,
        ),
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

fn inspector_lines(state: &TuiState, width: u16, height: u16) -> Text<'static> {
    let inner = width.saturating_sub(2).max(1) as usize;
    let mut lines = Vec::new();
    let status = display_busy_status(derived_busy_status(state));
    lines.push(Line::from(vec![
        Span::styled("Status ", Style::default().fg(Color::DarkGray)),
        Span::styled(status, Style::default().fg(Color::Yellow)),
    ]));
    lines.push(Line::from(vec![
        Span::styled("Model  ", Style::default().fg(Color::DarkGray)),
        Span::styled(
            clamp_chars(&state.model, inner.saturating_sub(7)),
            Style::default().fg(Color::Cyan),
        ),
    ]));
    if let Some(focus) = &state.active_focus {
        lines.push(Line::from(vec![
            Span::styled("Focus  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                clamp_chars(
                    &format!("{} {}", focus.selection, focus.mode),
                    inner.saturating_sub(7),
                ),
                Style::default().fg(Color::Yellow),
            ),
        ]));
    }
    if let Some((ctx_used, window, pct, color)) = context_usage(state) {
        lines.push(Line::from(vec![
            Span::styled("Context ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                format!(
                    "{} / {} ({pct}%)",
                    format_count(ctx_used),
                    format_count(window)
                ),
                Style::default().fg(color),
            ),
        ]));
    }
    if let Some((total, summary)) = turn_tool_summary(&state.turn_tool_counts) {
        lines.push(Line::from(vec![
            Span::styled("Tools  ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                clamp_chars(&format!("{total}: {summary}"), inner.saturating_sub(7)),
                Style::default().fg(Color::Green),
            ),
        ]));
    }
    if state.turn_error_count > 0 {
        lines.push(Line::from(vec![
            Span::styled("Errors ", Style::default().fg(Color::DarkGray)),
            Span::styled(
                state.turn_error_count.to_string(),
                Style::default().fg(Color::Red),
            ),
        ]));
    }
    if !state.streaming_thinking.is_empty() {
        let rendered_thinking = if state.context_mode.is_frugal() {
            pseudo_tool_protocol_text_for_context(&state.streaming_thinking, state.context_mode)
        } else {
            state.streaming_thinking.clone()
        };
        lines.push(Line::from(""));
        lines.push(Line::from(Span::styled(
            "Thinking",
            Style::default()
                .fg(Color::Magenta)
                .add_modifier(Modifier::BOLD),
        )));
        let tail = rendered_thinking
            .lines()
            .rev()
            .take(3)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect::<Vec<_>>()
            .join(" ");
        for row in wrap_plain_words_visual(&tail, inner.saturating_sub(2).max(1))
            .into_iter()
            .take(4)
        {
            lines.push(Line::from(vec![
                Span::styled("│ ", Style::default().fg(Color::DarkGray)),
                Span::styled(
                    row,
                    Style::default()
                        .fg(Color::Gray)
                        .add_modifier(Modifier::ITALIC),
                ),
            ]));
        }
    }
    lines.push(Line::from(""));
    lines.push(Line::from(Span::styled(
        "Debug events",
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));
    let reserved = lines.len().saturating_add(2);
    let event_take = (height as usize).saturating_sub(reserved).max(1);
    for event in state
        .debug_events
        .iter()
        .rev()
        .take(event_take)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
    {
        let event = sanitize_display_text(event);
        for (idx, row) in wrap_plain_words_visual(&event, inner.saturating_sub(2).max(1))
            .into_iter()
            .enumerate()
        {
            let prefix = if idx == 0 { "• " } else { "  " };
            lines.push(Line::from(vec![
                Span::styled(prefix.to_string(), Style::default().fg(Color::DarkGray)),
                Span::styled(row, Style::default().fg(Color::DarkGray)),
            ]));
            if lines.len() >= height.saturating_sub(1) as usize {
                break;
            }
        }
        if lines.len() >= height.saturating_sub(1) as usize {
            break;
        }
    }
    lines.push(Line::from(Span::styled(
        "Ctrl+I hide",
        Style::default().fg(Color::DarkGray),
    )));
    Text::from(lines)
}

fn local_auth_overlay_text(state: &TuiState) -> Text<'static> {
    let Some(pending) = state.pending_local_auth.as_ref() else {
        return Text::from("");
    };
    let bullets = "•".repeat(state.local_auth_input.chars().count().min(32));
    let shown = if bullets.is_empty() {
        "<hidden>".to_string()
    } else {
        bullets
    };
    let (header, input_label) = match pending.tool.as_str() {
        "git" => ("git credential for ".to_string(), "Token/password: "),
        _ => ("sudo password for ".to_string(), "Password: "),
    };
    Text::from(vec![
        Line::from(vec![
            Span::styled(header, Style::default().fg(Color::Yellow)),
            Span::styled(
                pending.tool.clone(),
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ),
        ]),
        Line::from(Span::styled(
            pending.message.clone(),
            Style::default().fg(Color::DarkGray),
        )),
        Line::from(vec![
            Span::styled(input_label, Style::default().fg(Color::Yellow)),
            Span::styled(shown, Style::default().fg(Color::Yellow)),
        ]),
        Line::from(Span::styled(
            "Enter submit · Esc cancel · never sent to model/logs",
            Style::default().fg(Color::DarkGray),
        )),
    ])
}

fn render_local_auth_overlay(frame: &mut ratatui::Frame, state: &TuiState, area: Rect) {
    if state.pending_local_auth.is_none() {
        return;
    }
    let text = local_auth_overlay_text(state);
    let width = area.width.clamp(20, 72);
    let height = (text.lines.len() as u16)
        .saturating_add(2)
        .min(area.height.max(1));
    let rect = centered_rect(area, width, height);
    let widget = Paragraph::new(text).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                " local auth ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::Yellow)),
    );
    render_widget_safe(frame, Clear, rect);
    render_widget_safe(frame, widget, rect);
}

fn apply_tui_message(state: &mut TuiState, msg: ToTui) {
    match msg {
        ToTui::Event(ev) => state.apply_event(ev),
        ToTui::PermissionRequest {
            name,
            input,
            responder,
        } => {
            state.close_backend_viewer();
            queue_permission_request(state, name, input, responder);
        }
        ToTui::LocalAuthSecretRequest {
            tool,
            message,
            responder,
        } => {
            state.close_backend_viewer();
            queue_local_auth_secret_request(state, tool, message, responder);
        }
    }
}

fn drain_live_output_events(
    state: &mut TuiState,
    live_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    first: Option<AgentEvent>,
) {
    let mut applied = 0usize;
    if let Some(event) = first {
        state.apply_event(event);
        applied = 1;
    }
    while applied < LIVE_OUTPUT_DRAIN_BATCH {
        match live_rx.try_recv() {
            Ok(event) => {
                state.apply_event(event);
                applied = applied.saturating_add(1);
            }
            Err(_) => break,
        }
    }
}

fn backend_viewer_text(state: &mut TuiState, area: Rect) -> (Text<'static>, String, String) {
    let inner_width = area.width.saturating_sub(2).max(1);
    let visible_rows = area.height.saturating_sub(2).max(1) as usize;
    let rows = state.backend_body_rows_for_width(inner_width);

    let total = state.backend_outputs.len();
    let selected = state
        .selected_backend_index()
        .map(|idx| idx.saturating_add(1))
        .unwrap_or(0);
    let (call_tag, status, summary) = state
        .selected_backend_output()
        .map(|output| {
            (
                output.call_tag.clone(),
                if output.running { "running" } else { "done" }.to_string(),
                output.summary.clone(),
            )
        })
        .unwrap_or_else(|| ("—".to_string(), "idle".to_string(), String::new()));
    let summary_rows = usize::from(!summary.is_empty()).min(visible_rows);
    let body_visible_rows = visible_rows.saturating_sub(summary_rows);
    state.clamp_backend_scroll(rows.len(), body_visible_rows);

    let title = clamp_chars(
        &format!(" backend {selected}/{total} {call_tag} · {status} "),
        area.width.max(1) as usize,
    );
    let footer = clamp_chars(
        " Esc/q close · ↑↓/Pg scroll · Tab next · Shift+Tab prev ",
        area.width.max(1) as usize,
    );

    let end = rows
        .len()
        .saturating_sub(state.backend_viewer_scroll_from_bottom);
    let start = end.saturating_sub(body_visible_rows);
    let mut lines: Vec<Line<'static>> = Vec::new();
    if !summary.is_empty() {
        lines.push(Line::from(Span::styled(
            clamp_chars(&summary, inner_width as usize),
            Style::default().fg(Color::Cyan),
        )));
    }
    for row in rows.iter().skip(start).take(end.saturating_sub(start)) {
        lines.push(Line::from(row.clone()));
    }
    while lines.len() < visible_rows {
        lines.push(Line::from(""));
    }
    (Text::from(lines), title, footer)
}

fn render_backend_viewer(frame: &mut ratatui::Frame, state: &mut TuiState) {
    let area = frame.area();
    render_widget_safe(frame, Clear, area);
    let (text, title, footer) = backend_viewer_text(state, area);
    let widget = Paragraph::new(text).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::ALL)
            .title(Span::styled(
                title,
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .title_bottom(Line::from(Span::styled(
                footer,
                Style::default().fg(Color::DarkGray),
            )))
            .border_style(Style::default().fg(Color::Yellow)),
    );
    render_widget_safe(frame, widget, area);
}

fn render_inspector(frame: &mut ratatui::Frame, state: &TuiState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let text = inspector_lines(state, area.width, area.height);
    let widget = Paragraph::new(text).wrap(Wrap { trim: false }).block(
        Block::default()
            .borders(Borders::LEFT)
            .title(Span::styled(
                " inspector ",
                Style::default()
                    .fg(Color::Yellow)
                    .add_modifier(Modifier::BOLD),
            ))
            .border_style(Style::default().fg(Color::DarkGray)),
    );
    render_widget_safe(frame, Clear, area);
    render_widget_safe(frame, widget, area);
}

fn draw(frame: &mut ratatui::Frame, state: &mut TuiState) {
    let area = frame.area();
    render_widget_safe(frame, Clear, area);

    let layout = compute_layout(area, state);
    let transcript_area = layout.transcript_area;
    let input_area = layout.input_area;
    let status_area = layout.status_area;
    let inspector_area = layout.inspector_area;
    state.input_area = input_area;
    state.inspector_area = inspector_area;

    render_transcript(frame, state, transcript_area);
    if state.show_inspector {
        render_inspector(frame, state, inspector_area);
    }

    let status = if state.show_status_details {
        Paragraph::new(Text::from(vec![
            Line::from(status_spans(state)),
            Line::from(status_detail_spans(state)),
        ]))
    } else {
        Paragraph::new(Line::from(status_spans(state)))
    };
    render_widget_safe(frame, status, status_area);

    let prompt_style = if (state.input.is_empty() && state.input_display_override.is_none())
        || (state.agent_busy && state.pending_perm.is_none())
    {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
    };

    let wrap_cols = input_area.width.saturating_sub(2).max(1) as usize;
    let editor_text = input_editor_text(state);
    let display_cursor = input_display_cursor(state);
    let (wrapped, cursor_row, cursor_col) =
        wrap_input_visual(editor_text.as_ref(), display_cursor, wrap_cols);
    let inner_rows = input_area.height.saturating_sub(2).max(1) as usize;
    let drawer_height = work_map_drawer_height(state, input_area.width).min(inner_rows);
    let hint_rows = 0usize;
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

    let hint = input_hint_text(state);
    let mut block = Block::default()
        .borders(Borders::ALL)
        .border_style(input_border_style(state));
    if !hint.is_empty() {
        block = block.title_bottom(
            Line::from(Span::styled(hint, Style::default().fg(Color::DarkGray))).right_aligned(),
        );
    }
    let input = Paragraph::new(lines).block(block);
    render_widget_safe(frame, input, input_area);

    if state.pending_local_auth.is_none()
        && state.pending_perm.is_none()
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

    if state.pending_local_auth.is_some() {
        render_local_auth_overlay(frame, state, area);
    }

    if !state.show_help
        && state.pending_local_auth.is_none()
        && state.pending_perm.is_none()
        && !state.work_map_is_active()
        && !state.show_inspector
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
                    .saturating_sub(text_width(comp.text.as_str()));
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

fn handle_paste(state: &mut TuiState, mut pasted: String) {
    if pasted.is_empty() {
        return;
    }
    if state.backend_viewer_open {
        clear_secret_string(&mut pasted);
        state.status = "backend viewer open; paste ignored".to_string();
        return;
    }
    if state.pending_local_auth.is_some() {
        let secret = pasted.trim_end_matches(['\r', '\n']);
        state.local_auth_input.push_str(secret);
        clear_secret_string(&mut pasted);
        state.status = "local auth input updated".to_string();
        return;
    }
    if state.agent_busy && crate::text_is_potential_local_secret(&pasted) {
        state.queue(Line_::Warn(
            "paste withheld: do not enter sudo passwords or local auth secrets in chat; use the local auth prompt".to_string(),
        ));
        clear_secret_string(&mut pasted);
        state.status = "local secret paste withheld".to_string();
        return;
    }
    let secret_looking = crate::text_is_potential_local_secret(&pasted);
    let start = state.cursor;
    state.insert_input_str(&pasted);
    let end = state.cursor;
    if secret_looking {
        state.status =
            "pasted text looks like a credential; submitting will ask for confirmation".to_string();
    }
    let words = count_words(&pasted);
    if words > PASTE_WORD_THRESHOLD {
        state
            .input_preview_spans
            .push(InputPreviewSpan { start, end, words });
    }
    state.refresh_input_display_override();
    if state.input_display_override.is_some() {
        state.status = "large paste collapsed in editor".to_string();
    }
    state.reset_slash_completion_selection();
}

fn handle_mouse(state: &mut TuiState, mouse: MouseEvent) {
    if state.backend_viewer_open {
        match mouse.kind {
            MouseEventKind::ScrollUp => state.scroll_backend_viewer(1),
            MouseEventKind::ScrollDown => state.scroll_backend_viewer(-1),
            _ => {}
        }
        return;
    }

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
    state.replace_input(command);
    state.clear_slash_completion_selection();
}

fn work_map_command_prefix(drawer: &WorkMapDrawer) -> String {
    if let Some(selector) = drawer.selector.as_deref().filter(|s| !s.trim().is_empty()) {
        format!("/map {} ", selector.trim())
    } else {
        "/map ".to_string()
    }
}

fn handle_work_map_key(
    state: &mut TuiState,
    key: KeyEvent,
    agent_input: &tokio::sync::mpsc::UnboundedSender<FromTui>,
) -> bool {
    if !state.work_map_is_active() {
        return false;
    }
    if state
        .work_map
        .as_ref()
        .is_some_and(|drawer| drawer.filter_input)
    {
        match key.code {
            KeyCode::Esc => {
                if let Some(drawer) = state.work_map.as_mut() {
                    drawer.filter_input = false;
                }
                state.status = "session map filter canceled".to_string();
                true
            }
            KeyCode::Enter => {
                let command = state.input.trim().to_string();
                if command.is_empty() {
                    return true;
                }
                state.work_map = None;
                state.clear_slash_completion_selection();
                state.status = "opening filtered session map".to_string();
                let _ = agent_input.send(FromTui::Submit(command));
                state.clear_input();
                true
            }
            _ => false,
        }
    } else {
        match key.code {
            KeyCode::Esc => {
                state.work_map = None;
                state.status = "session map drawer closed".to_string();
                true
            }
            KeyCode::Up => {
                if state.move_work_map_selection(-1) {
                    state.status = "session map selection moved".to_string();
                }
                true
            }
            KeyCode::Down => {
                if state.move_work_map_selection(1) {
                    state.status = "session map selection moved".to_string();
                }
                true
            }
            KeyCode::PageUp => {
                let step = work_map_drawer_body_rows(state).saturating_sub(1).max(1) as isize;
                if state.move_work_map_selection_for_rows(-step, WORK_MAP_DRAWER_MAX_BODY_ROWS) {
                    state.status = "session map selection moved".to_string();
                }
                true
            }
            KeyCode::PageDown => {
                let step = work_map_drawer_body_rows(state).saturating_sub(1).max(1) as isize;
                if state.move_work_map_selection_for_rows(step, WORK_MAP_DRAWER_MAX_BODY_ROWS) {
                    state.status = "session map selection moved".to_string();
                }
                true
            }
            KeyCode::Home => {
                state.set_work_map_selection(0);
                state.status = "session map selection moved".to_string();
                true
            }
            KeyCode::End => {
                if let Some(last) = state
                    .work_map
                    .as_ref()
                    .map(|drawer| drawer.waypoint_ids.len().saturating_sub(1))
                {
                    state.set_work_map_selection(last);
                    state.status = "session map selection moved".to_string();
                }
                true
            }
            KeyCode::Enter => {
                if let Some(arg) = state.selected_work_map_command_arg() {
                    state.work_map = None;
                    state.clear_slash_completion_selection();
                    state.status = "inspecting moment".to_string();
                    let _ = agent_input.send(FromTui::Submit(format!("/focus {arg}")));
                }
                true
            }
            KeyCode::Char('f') | KeyCode::Char('F') | KeyCode::Char('i') | KeyCode::Char('I') => {
                if let Some(arg) = state.selected_work_map_command_arg() {
                    insert_command_into_input(state, format!("/focus {arg}"));
                    state.work_map = None;
                    state.status = "inserted /focus command".to_string();
                }
                true
            }
            KeyCode::Char('b') | KeyCode::Char('B') => {
                if let Some(arg) = state.selected_work_map_command_arg() {
                    insert_command_into_input(state, format!("/focus {arg} --branch"));
                    state.work_map = None;
                    state.status = "inserted branch command".to_string();
                }
                true
            }
            KeyCode::Char('z') | KeyCode::Char('Z') | KeyCode::Char('/') => {
                let prefix = state
                    .work_map
                    .as_ref()
                    .map(work_map_command_prefix)
                    .unwrap_or_else(|| "/map ".to_string());
                if let Some(drawer) = state.work_map.as_mut() {
                    drawer.filter_input = true;
                }
                state.replace_input(prefix);
                state.status =
                    "session map filter: type failures|changes|verify|file <path>|query <text>"
                        .to_string();
                true
            }
            _ => false,
        }
    }
}

fn queue_runtime_effort_control(
    state: &mut TuiState,
    runtime_control_input: &tokio::sync::mpsc::UnboundedSender<String>,
    step: i8,
) {
    let command = if step < 0 {
        "/effort prev"
    } else {
        "/effort next"
    };
    state.status = if runtime_control_input.send(command.to_string()).is_ok() {
        "runtime control queued".to_string()
    } else {
        "runtime control unavailable".to_string()
    };
}

fn submit_local_auth_secret(state: &mut TuiState) {
    let Some(pending) = state.pending_local_auth.take() else {
        return;
    };
    if state.local_auth_input.is_empty() {
        state.pending_local_auth = Some(pending);
        state.status = "enter the secret or Esc cancel".to_string();
        return;
    }
    let secret = std::mem::take(&mut state.local_auth_input);
    match pending.responder.send(LocalAuthSecret::Secret(secret)) {
        Ok(()) => {
            state.status = format!("local auth submitted for {}", pending.tool);
        }
        Err(err) => {
            if let LocalAuthSecret::Secret(mut unsent) = err.0 {
                clear_secret_string(&mut unsent);
            }
            state.status = format!("local auth unavailable for {}", pending.tool);
        }
    }
}

fn cancel_local_auth_secret(state: &mut TuiState) {
    if let Some(pending) = state.pending_local_auth.take() {
        clear_secret_string(&mut state.local_auth_input);
        let _ = pending.responder.send(LocalAuthSecret::Canceled);
        state.status = format!("local auth canceled for {}", pending.tool);
    }
}

fn handle_local_auth_key(state: &mut TuiState, key: KeyEvent) -> bool {
    if state.pending_local_auth.is_none() {
        return false;
    }
    let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::META);
    match (key.code, key.modifiers) {
        (KeyCode::Enter, _) => submit_local_auth_secret(state),
        (KeyCode::Esc, _) => cancel_local_auth_secret(state),
        (KeyCode::Char('c'), _) if is_ctrl => cancel_local_auth_secret(state),
        (KeyCode::Char('u'), _) if is_ctrl => {
            clear_secret_string(&mut state.local_auth_input);
            state.status = "local auth input cleared".to_string();
        }
        (KeyCode::Backspace, _) => {
            state.local_auth_input.pop();
            state.status = "local auth input updated".to_string();
        }
        (KeyCode::Delete, _) => {
            state.status = "local auth input hidden".to_string();
        }
        (KeyCode::Char(c), m)
            if !m.intersects(
                KeyModifiers::CONTROL
                    | KeyModifiers::ALT
                    | KeyModifiers::SUPER
                    | KeyModifiers::META,
            ) =>
        {
            state.local_auth_input.push(c);
            state.status = "local auth input updated".to_string();
        }
        _ => {}
    }
    true
}

fn handle_backend_viewer_key(state: &mut TuiState, key: KeyEvent) -> bool {
    if !state.backend_viewer_open {
        return false;
    }
    let is_ctrl = key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER | KeyModifiers::META);
    match (key.code, key.modifiers) {
        (KeyCode::Char('c'), _) | (KeyCode::Char('d'), _) if is_ctrl => return false,
        (KeyCode::Esc, _) | (KeyCode::Char('q'), _) | (KeyCode::Char('Q'), _) => {
            state.close_backend_viewer()
        }
        (KeyCode::Char('b'), _) if is_ctrl => state.close_backend_viewer(),
        (KeyCode::Up, _) => state.scroll_backend_viewer(1),
        (KeyCode::Down, _) => state.scroll_backend_viewer(-1),
        (KeyCode::PageUp, _) => state.scroll_backend_viewer(10),
        (KeyCode::PageDown, _) => state.scroll_backend_viewer(-10),
        (KeyCode::Home, _) => state.scroll_backend_viewer(isize::MAX / 4),
        (KeyCode::End, _) => state.backend_viewer_scroll_from_bottom = 0,
        (KeyCode::Tab, _) | (KeyCode::Char('n'), _) | (KeyCode::Char('N'), _) => {
            state.cycle_backend_selection(1)
        }
        (KeyCode::BackTab, _) | (KeyCode::Char('p'), _) | (KeyCode::Char('P'), _) => {
            state.cycle_backend_selection(-1)
        }
        _ => {}
    }
    true
}

fn handle_key(
    state: &mut TuiState,
    key: KeyEvent,
    agent_input: &tokio::sync::mpsc::UnboundedSender<FromTui>,
    runtime_control_input: &tokio::sync::mpsc::UnboundedSender<String>,
    steering_input: &tokio::sync::mpsc::UnboundedSender<String>,
    interrupt: &Arc<std::sync::atomic::AtomicBool>,
) {
    if key.kind != KeyEventKind::Press {
        return;
    }
    if handle_backend_viewer_key(state, key) {
        return;
    }
    if handle_local_auth_key(state, key) {
        return;
    }
    if state.work_map_is_active() && handle_work_map_key(state, key, agent_input) {
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
                state.clear_input();
                state.clear_slash_completion_selection();
                state.status = "input cleared (Ctrl+C again to quit)".to_string();
            }
        }
        (KeyCode::Char('d'), m) if is_ctrl(m) => {
            state.quit = true;
            let _ = agent_input.send(FromTui::Quit);
        }
        (KeyCode::Char('e'), m) if is_ctrl(m) => {
            if state.agent_busy {
                queue_runtime_effort_control(state, runtime_control_input, 1);
            } else {
                let _ = agent_input.send(FromTui::CycleEffort(1));
            }
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
        (KeyCode::Char('b'), m) if is_ctrl(m) => {
            state.open_backend_viewer();
        }
        (KeyCode::Char('t'), m) if is_ctrl(m) => {
            state.show_status_details = !state.show_status_details;
            state.status = if state.show_status_details {
                "status details visible".to_string()
            } else {
                "status details hidden".to_string()
            };
        }
        (KeyCode::Char('i'), m) if is_ctrl(m) => {
            state.show_inspector = !state.show_inspector;
            state.status = if state.show_inspector {
                "inspector visible".to_string()
            } else {
                "inspector hidden".to_string()
            };
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
                state.status = "session map drawer closed".to_string();
            } else if state.show_help {
                state.show_help = false;
            } else if state.show_inspector {
                state.show_inspector = false;
                state.status = "inspector hidden".to_string();
            } else if state.agent_busy {
                if state.input.trim().is_empty() {
                    if interrupt.swap(true, Ordering::SeqCst) {
                        state.quit = true;
                        let _ = agent_input.send(FromTui::Quit);
                    } else {
                        state.status = "interrupting… Esc again quits".to_string();
                    }
                } else {
                    state.clear_input();
                    state.clear_slash_completion_selection();
                    state.status = "input cleared; Esc again interrupts".to_string();
                }
            } else if !state.input.is_empty() {
                state.clear_input();
                state.clear_slash_completion_selection();
                state.status = "input cleared".to_string();
            }
        }
        (KeyCode::Tab, _) => {
            if !state.accept_slash_completion() {
                if state.agent_busy {
                    queue_runtime_effort_control(state, runtime_control_input, 1);
                } else {
                    let _ = agent_input.send(FromTui::CycleEffort(1));
                }
            }
        }
        (KeyCode::BackTab, _) => {
            if state.move_slash_completion_selection(-1) {
                let _ = state.accept_slash_completion();
            } else if state.agent_busy {
                queue_runtime_effort_control(state, runtime_control_input, -1);
            } else {
                let _ = agent_input.send(FromTui::CycleEffort(-1));
            }
        }
        (KeyCode::Enter, m) if m.contains(KeyModifiers::SHIFT) || m.contains(KeyModifiers::ALT) => {
            state.insert_input_char('\n');
            state.refresh_input_display_override();
        }
        (KeyCode::Enter, _) => {
            state.work_map = None;
            let text = state.input.clone();
            if text.trim().is_empty() {
                return;
            }
            if state.agent_busy && state.pending_perm.is_none() {
                if let Some(commands) = parse_active_runtime_control_sequence(&text) {
                    let mut queued = false;
                    for command in commands {
                        if runtime_control_input.send(command).is_ok() {
                            queued = true;
                        }
                    }
                    state.status = if queued {
                        "runtime control queued".to_string()
                    } else {
                        "runtime control unavailable".to_string()
                    };
                    state.clear_input();
                    state.clear_slash_completion_selection();
                    return;
                }
                if crate::text_is_potential_local_secret(&text) {
                    state.queue(Line_::Warn(
                        "input withheld: do not enter sudo passwords or local auth secrets in chat; use the local auth prompt".to_string(),
                    ));
                    state.clear_input();
                    state.clear_slash_completion_selection();
                    state.status = "local secret withheld from provider".to_string();
                    return;
                }
                if crate::is_slash_command(&text) {
                    state.queue(Line_::Warn(crate::unsupported_busy_slash_message(&text)));
                    state.clear_input();
                    state.clear_slash_completion_selection();
                    state.status = "slash command waits until idle".to_string();
                    return;
                }
                state.queue(Line_::Steering(text.clone()));
                if steering_input.send(text).is_ok() {
                    state.status = "queued for next safe boundary".to_string();
                } else {
                    state.status = "queue unavailable".to_string();
                }
                state.clear_input();
                state.clear_slash_completion_selection();
                return;
            }
            if crate::text_is_potential_local_secret(&text) {
                if state.pending_secret_send.as_deref() != Some(text.as_str()) {
                    state.pending_secret_send = Some(text.clone());
                    state.queue(Line_::Warn(
                        "input looks like it contains a credential and was NOT sent. If a command is waiting for auth, cancel and let Dext open its masked local prompt instead — anything typed here goes into the model transcript. Press Enter again to send anyway.".to_string(),
                    ));
                    state.status =
                        "secret-looking input withheld · Enter again to send".to_string();
                    return;
                }
                state.pending_secret_send = None;
            } else {
                state.pending_secret_send = None;
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
            state.clear_input();
            state.clear_slash_completion_selection();
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
                let start = state.cursor - prev;
                state.remove_input_range(start, state.cursor);
                state.cursor = start;
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
                state.remove_input_range(state.cursor, state.cursor + next);
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
                state.replace_input(prev.clone());
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
                state.replace_input(state.history[i + 1].clone());
                state.history_idx = Some(i + 1);
                state.reset_slash_completion_selection();
            } else {
                state.clear_input();
                state.history_idx = None;
                state.clear_slash_completion_selection();
            }
        }
        (KeyCode::Char(c), m)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            state.insert_input_char(c);
            state.refresh_input_display_override();
            state.reset_slash_completion_selection();
        }
        _ => {}
    }
}

const KEYBOARD_ENHANCEMENT_ENV: &str = "DEXT_KEYBOARD_ENHANCEMENT";

#[cfg(unix)]
fn parse_bool_env(raw: &str) -> Option<bool> {
    match raw.trim().to_ascii_lowercase().as_str() {
        "1" | "true" | "yes" | "on" | "force" => Some(true),
        "" | "0" | "false" | "no" | "off" | "disabled" => Some(false),
        _ => None,
    }
}

#[cfg(unix)]
fn terminal_identity_supports_keyboard_enhancement(
    term: &str,
    term_program: &str,
    marker_env_present: bool,
) -> bool {
    if marker_env_present {
        return true;
    }
    let term = term.to_ascii_lowercase();
    let term_program = term_program.to_ascii_lowercase();
    ["kitty", "wezterm", "alacritty", "foot", "ghostty", "rio"]
        .iter()
        .any(|needle| term.contains(needle) || term_program.contains(needle))
}

#[cfg(unix)]
fn terminal_env_advertises_keyboard_enhancement() -> bool {
    let marker_env_present = [
        "KITTY_WINDOW_ID",
        "WEZTERM_EXECUTABLE",
        "ALACRITTY_LOG",
        "GHOSTTY_RESOURCES_DIR",
    ]
    .iter()
    .any(|key| std::env::var_os(key).is_some());
    terminal_identity_supports_keyboard_enhancement(
        &std::env::var("TERM").unwrap_or_default(),
        &std::env::var("TERM_PROGRAM").unwrap_or_default(),
        marker_env_present,
    )
}

#[cfg(unix)]
fn terminal_supports_keyboard_enhancement() -> bool {
    if let Ok(raw) = std::env::var(KEYBOARD_ENHANCEMENT_ENV)
        && let Some(on) = parse_bool_env(&raw)
    {
        return on;
    }
    if !terminal_env_advertises_keyboard_enhancement() {
        return false;
    }
    crossterm::terminal::supports_keyboard_enhancement().unwrap_or(false)
}

#[cfg(not(unix))]
fn terminal_supports_keyboard_enhancement() -> bool {
    false
}

struct AlternateScreenGuard {
    active: bool,
}

impl AlternateScreenGuard {
    fn new() -> io::Result<Self> {
        let mut out = io::stdout();
        crossterm::execute!(out, EnterAlternateScreen)?;
        let guard = Self { active: true };
        if let Err(err) =
            crossterm::execute!(out, crossterm::cursor::Hide).and_then(|_| out.flush())
        {
            drop(guard);
            return Err(err);
        }
        Ok(guard)
    }
}

impl Drop for AlternateScreenGuard {
    fn drop(&mut self) {
        if self.active {
            let mut out = io::stdout();
            let _ = crossterm::execute!(out, LeaveAlternateScreen, crossterm::cursor::Show);
            let _ = out.flush();
            self.active = false;
        }
    }
}

struct TerminalGuard {
    active: bool,
    keyboard_enhancement_enabled: bool,
}

impl TerminalGuard {
    fn new() -> io::Result<Self> {
        enable_raw_mode()?;
        crate::session::set_tui_active(true);

        let keyboard_enhancement_enabled = terminal_supports_keyboard_enhancement();
        let keyboard_flags = KeyboardEnhancementFlags::DISAMBIGUATE_ESCAPE_CODES;

        let mut out = io::stdout();
        let execute_result = if keyboard_enhancement_enabled {
            crossterm::execute!(
                out,
                EnableBracketedPaste,
                PushKeyboardEnhancementFlags(keyboard_flags),
                crossterm::cursor::SetCursorStyle::SteadyBlock,
                crossterm::cursor::Show
            )
        } else {
            crossterm::execute!(
                out,
                EnableBracketedPaste,
                crossterm::cursor::SetCursorStyle::SteadyBlock,
                crossterm::cursor::Show
            )
        };
        if let Err(err) = execute_result {
            crate::session::restore_terminal_if_tui();
            return Err(err);
        }

        out.flush()?;
        Ok(Self {
            active: true,
            keyboard_enhancement_enabled,
        })
    }
}

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if self.active {
            if self.keyboard_enhancement_enabled {
                let _ = crossterm::execute!(io::stdout(), PopKeyboardEnhancementFlags);
            }
            crate::session::restore_terminal_if_tui();
            self.active = false;
        }
    }
}

struct BackendViewerIo<'a> {
    agent_input: &'a tokio::sync::mpsc::UnboundedSender<FromTui>,
    runtime_control_input: &'a tokio::sync::mpsc::UnboundedSender<String>,
    steering_input: &'a tokio::sync::mpsc::UnboundedSender<String>,
    interrupt: &'a Arc<std::sync::atomic::AtomicBool>,
}

async fn run_backend_viewer(
    state: &mut TuiState,
    live_rx: &mut tokio::sync::mpsc::Receiver<AgentEvent>,
    ev_rx: &mut tokio::sync::mpsc::UnboundedReceiver<ToTui>,
    key_rx: &mut tokio::sync::mpsc::UnboundedReceiver<Event>,
    channels: BackendViewerIo<'_>,
) -> Result<()> {
    let _guard = AlternateScreenGuard::new()?;
    let backend = CrosstermBackend::new(io::stdout());
    let mut terminal = Terminal::new(backend)?;
    terminal.clear()?;
    let tick = Duration::from_millis(80);
    let mut last_tick = Instant::now();

    while state.backend_viewer_open && !state.quit {
        terminal.draw(|f| render_backend_viewer(f, state))?;
        let timeout = tick
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        tokio::select! {
            biased;
            maybe_ev = ev_rx.recv() => {
                if let Some(msg) = maybe_ev {
                    apply_tui_message(state, msg);
                }
            }
            maybe_key = key_rx.recv() => {
                if let Some(ev) = maybe_key {
                    match ev {
                        Event::Key(k) => handle_key(
                            state,
                            k,
                            channels.agent_input,
                            channels.runtime_control_input,
                            channels.steering_input,
                            channels.interrupt,
                        ),
                        Event::Mouse(mouse) => handle_mouse(state, mouse),
                        Event::Paste(pasted) => handle_paste(state, pasted),
                        _ => {}
                    }
                }
            }
            maybe_live = live_rx.recv() => {
                drain_live_output_events(state, live_rx, maybe_live);
            }
            _ = tokio::time::sleep(timeout) => {
                last_tick = Instant::now();
            }
        }
    }

    let _ = terminal.clear();
    Ok(())
}

pub async fn run(mut agent: Agent, initial_task: Option<String>) -> Result<()> {
    let model = agent.model.clone();
    let context_window_tokens = agent.context_window_tokens();
    let sandbox = agent.sandbox_root.display().to_string();
    let approval_profile = agent.approval_profile();
    let thinking_effort = agent.thinking_effort();
    let context_mode = agent.context_mode;
    let auto_approved_count = agent.auto_approved_privileged_tool_count();
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
    let (live_tx, mut live_rx) =
        tokio::sync::mpsc::channel::<AgentEvent>(crate::LIVE_OUTPUT_EVENT_QUEUE_CAP);
    let (in_tx, mut in_rx) = tokio::sync::mpsc::unbounded_channel::<FromTui>();
    let (key_tx, mut key_rx) = tokio::sync::mpsc::unbounded_channel::<Event>();

    let interrupt = agent.interrupt.clone();
    agent.set_sink(Box::new(TuiSink { tx: ev_tx, live_tx }));
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

    let mut state = TuiState::new(
        model,
        context_window_tokens,
        sandbox,
        approval_profile,
        thinking_effort,
    );
    state.context_mode = context_mode;
    let banner = welcome_banner(
        &state.sandbox,
        &state.model,
        state.thinking_effort,
        context_mode,
        state.approval_profile,
        auto_approved_count,
    );
    state.queue(Line_::Banner(banner));
    if let Some(task) = initial_task {
        state.queue(Line_::User(task.clone()));
        let _ = in_tx.send(FromTui::Submit(task));
    }

    // Move agent into a task; communicate via channels
    let (cmd_tx, mut cmd_rx) = tokio::sync::mpsc::unbounded_channel::<FromTui>();
    let (runtime_control_tx, runtime_control_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let direct_runtime_control_tx = runtime_control_tx.clone();
    let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel::<String>();
    let direct_steer_tx = steer_tx.clone();
    agent.install_runtime_controls(runtime_control_rx, runtime_control_tx.clone());
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
                                    let base = history_char_budget_with_window(
                                        agent.context_window_tokens(),
                                        None,
                                        agent.context_mode,
                                        HISTORY_CHAR_BUDGET_END_TURN_PERCENT,
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
        if state.backend_viewer_open {
            Backend::flush(terminal.backend_mut())?;
            run_backend_viewer(
                &mut state,
                &mut live_rx,
                &mut ev_rx,
                &mut key_rx,
                BackendViewerIo {
                    agent_input: &in_tx,
                    runtime_control_input: &direct_runtime_control_tx,
                    steering_input: &direct_steer_tx,
                    interrupt: &interrupt,
                },
            )
            .await?;
            continue;
        }

        state.refresh_git_branch();
        let width = current_transcript_pane_width(&mut terminal, &state)?;
        flush_pending_insert(&mut terminal, &mut state, width)?;
        terminal.draw(|f| draw(f, &mut state))?;
        state.frame_count = state.frame_count.wrapping_add(1);
        let timeout = tick
            .checked_sub(last_tick.elapsed())
            .unwrap_or(Duration::ZERO);

        tokio::select! {
            biased;
            maybe_ev = ev_rx.recv() => {
                if let Some(msg) = maybe_ev {
                    apply_tui_message(&mut state, msg);
                }
            }
            maybe_key = key_rx.recv() => {
                if let Some(ev) = maybe_key {
                    match ev {
                        Event::Key(k) => handle_key(
                            &mut state,
                            k,
                            &in_tx,
                            &direct_runtime_control_tx,
                            &direct_steer_tx,
                            &interrupt,
                        ),
                        Event::Mouse(mouse) => handle_mouse(&mut state, mouse),
                        Event::Paste(pasted) => handle_paste(&mut state, pasted),
                        _ => {}
                    }
                }
            }
            maybe_live = live_rx.recv() => {
                drain_live_output_events(&mut state, &mut live_rx, maybe_live);
            }
            _ = tokio::time::sleep(timeout) => {
                last_tick = Instant::now();
            }
        }
    }

    interrupt.store(true, Ordering::SeqCst);
    cancel_local_auth_secret(&mut state);

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
    while let Ok(ev) = live_rx.try_recv() {
        state.apply_event(ev);
    }
    while let Ok(ev) = ev_rx.try_recv() {
        if let ToTui::Event(e) = ev {
            state.apply_event(e);
        }
    }
    if let Ok(width) = current_transcript_pane_width(&mut terminal, &state) {
        let _ = flush_pending_insert(&mut terminal, &mut state, width);
    }
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
    use std::sync::atomic::AtomicBool;

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

    #[test]
    fn welcome_banner_renders_titled_card_with_inline_warning() {
        let banner = welcome_banner(
            "~/Documents/Projects/Screener",
            "gpt-5.5",
            ThinkingEffort::Medium,
            ContextMode::Tiny,
            ApprovalProfile::Always,
            5,
        );
        let text = line_to_text(&Line_::Banner(banner), 120);
        let lines = flatten_lines(&text);

        assert_eq!(lines.len(), 7);
        assert!(lines[0].starts_with("── 🐺 Dext v"));
        assert!(!lines.iter().any(|line| line == "🐺 Dext"));
        assert_eq!(
            text_width(lines[0].trim_start()),
            text_width(lines[6].trim_start())
        );
        assert!(lines[1].starts_with("sandbox   ~/Documents/Projects/Screener"));
        assert_eq!(lines[2], "model     gpt-5.5 · medium reasoning");
        assert_eq!(
            lines[3],
            "context   tiny · lean schemas, smallest caps, deterministic compaction"
        );
        assert_eq!(
            lines[4],
            "approval  ⚠ trust mode · 5 privileged tools run without confirmation"
        );
        assert_eq!(
            lines[5],
            "keys      Ctrl+C/Esc interrupt · Ctrl+D quit · ? help"
        );
        let approval_style = span_style_for(&text, "⚠ trust mode").expect("approval style");
        assert_eq!(approval_style.fg, Some(Color::Yellow));
        let title_style = span_style_for(&text, "🐺 Dext").expect("title style");
        assert_ne!(title_style.fg, Some(Color::DarkGray));
    }

    #[test]
    fn welcome_banner_renders_frugal_context_description() {
        let banner = welcome_banner(
            "~/Documents/Projects/Screener",
            "gpt-5.5",
            ThinkingEffort::Medium,
            ContextMode::Frugal,
            ApprovalProfile::Always,
            5,
        );
        let lines = flatten_lines(&line_to_text(&Line_::Banner(banner), 120));

        assert_eq!(
            lines[3],
            "context   frugal · lean schemas, smaller toolset, smaller caps, deterministic compaction"
        );
        assert!(!lines.iter().any(|line| line.starts_with("[context]")));
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

    fn draw_to_lines(width: u16, height: u16, state: &mut TuiState) -> Vec<String> {
        let backend = ratatui::backend::TestBackend::new(width, height);
        let mut terminal = Terminal::new(backend).expect("terminal");
        terminal.draw(|frame| draw(frame, state)).expect("draw");
        let buffer = terminal.backend().buffer();
        (0..height)
            .map(|y| {
                (0..width)
                    .map(|x| buffer[(x, y)].symbol())
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
            model_context_window("test-model"),
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

        assert_eq!(input_hint_text(&state), "y once · a always · n deny");
    }

    #[test]
    fn busy_input_hint_is_empty() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;

        assert_eq!(input_hint_text(&state), "");
    }

    #[test]
    fn backend_output_delta_stays_out_of_transcript_and_marks_done() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.apply_event(AgentEvent::ToolCallStart {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            summary: "bash: long job".to_string(),
        });
        state.apply_event(AgentEvent::ToolOutputDelta {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            stream: "stdout".to_string(),
            text: "phase 1\n".to_string(),
        });
        state.apply_event(AgentEvent::ToolOutputDelta {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            stream: "stderr".to_string(),
            text: "warn\n".to_string(),
        });

        assert!(state.pending_insert.is_empty());
        let output = state.backend_outputs.back().expect("backend output");
        assert!(output.running);
        assert_eq!(output.summary, "bash: long job");
        assert!(output.text.contains("stdout │ phase 1"), "{}", output.text);
        assert!(output.text.contains("stderr │ warn"), "{}", output.text);

        state.apply_event(AgentEvent::ToolOutputDelta {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            stream: "stdout".to_string(),
            text: "partial ".to_string(),
        });
        state.apply_event(AgentEvent::ToolOutputDelta {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            stream: "stdout".to_string(),
            text: "line\n".to_string(),
        });
        let output = state.backend_outputs.back().expect("backend output");
        assert!(
            output.text.contains("stdout │ partial line"),
            "{}",
            output.text
        );

        state.apply_event(AgentEvent::ToolCallResult {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            ok: true,
            preview: "bash: long job".to_string(),
            content: "exit: 0".to_string(),
        });

        assert!(!state.backend_outputs.back().unwrap().running);
        state.apply_event(AgentEvent::ToolOutputDelta {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            stream: "stdout".to_string(),
            text: "late tail\n".to_string(),
        });
        let output = state.backend_outputs.back().unwrap();
        assert!(!output.running);
        assert!(
            output.text.contains("stdout │ late tail"),
            "{}",
            output.text
        );
        assert!(
            matches!(state.pending_insert.as_slice(), [Line_::Tool { name, .. }] if name == "bash")
        );
    }

    #[test]
    fn ctrl_b_opens_and_esc_closes_backend_viewer() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::ToolCallStart {
            call_id: "call_backend".to_string(),
            name: "bash".to_string(),
            summary: "bash: long job".to_string(),
        });

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::CONTROL),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );
        assert!(state.backend_viewer_open);

        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Esc, KeyModifiers::empty()),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );
        assert!(!state.backend_viewer_open);
    }

    #[test]
    fn ready_input_hint_is_empty_and_help_lists_keymap() {
        let state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        assert_eq!(input_hint_text(&state), "");

        let help = flatten_lines(&help_overlay_text()).join("\n");
        assert!(help.contains("Enter"), "{help}");
        assert!(help.contains("Shift+Enter / Alt+Enter"), "{help}");
        assert!(help.contains("Ctrl+O"), "{help}");
        assert!(help.contains("Ctrl+I"), "{help}");
        assert!(help.contains("Ctrl+T"), "{help}");
    }

    #[test]
    fn pending_permission_does_not_render_live_indicator_or_busy_status() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
    fn status_spans_render_branch_between_sandbox_and_model() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.provider_label = "chatgpt".to_string();
        state.api_family = "chatgpt-responses".to_string();
        state.git_branch = Some("status-branch (dirty)".to_string());

        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();

        assert!(
            rendered.contains(". | Branch(status-branch (dirty)) │ test-model"),
            "{rendered}"
        );
        let sandbox_idx = rendered.find('.').expect("sandbox marker");
        let branch_idx = rendered
            .find("Branch(status-branch (dirty))")
            .expect("branch");
        let model_idx = rendered.find("test-model").expect("model");
        assert!(sandbox_idx < branch_idx, "{rendered}");
        assert!(branch_idx < model_idx, "{rendered}");
        assert!(!rendered.contains("branch:"), "{rendered}");
    }

    #[test]
    fn status_bar_keeps_full_requested_sandbox_and_branch_visible() {
        let mut state = TuiState::new(
            "gpt-5.5".to_string(),
            model_context_window("gpt-5.5"),
            "/home/abaka/Documents/Projects/Learn/Finance".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.provider_label = "chatgpt".to_string();
        state.api_family = "chatgpt-responses".to_string();
        state.git_branch = Some("main".to_string());

        let sandbox = "Documents/Projects/Learn/Finance";
        let lines = draw_to_lines(100, 20, &mut state);
        let line = lines
            .iter()
            .find(|line| line.contains(sandbox))
            .unwrap_or_else(|| panic!("sandbox not visible: {lines:?}"));
        assert!(line.contains(sandbox), "{line}");
        assert!(!line.contains("~/Documents/Projects/Le…"), "{line}");
        assert!(line.contains("Branch(main)"), "{line}");
        assert!(
            line.find("Branch(main)").unwrap() < line.find("GPT-5.5").unwrap(),
            "{line}"
        );
    }

    #[test]
    fn approval_profile_changed_updates_trust_input_border() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
        assert!(lines[0].contains("Responding"));
        assert!(lines[0].contains("12s"));
        assert!(lines[1].contains("final streamed line"));
    }

    #[test]
    fn live_indicator_keeps_standard_legacy_redaction_and_enhances_frugal() {
        let mut standard = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        standard.agent_busy = true;
        standard.apply_event(AgentEvent::TextDelta("to=".to_string()));
        standard.apply_event(AgentEvent::TextDelta(
            "functions.bash {\"command\":\"cargo test\"}".to_string(),
        ));
        let text = transcript_live_indicator_text(&standard, 80).expect("live indicator");
        let lines = flatten_lines(&text);
        assert!(lines[1].contains("to="), "{lines:?}");
        assert!(lines[1].contains("tool call redacted"), "{lines:?}");
        assert!(!lines[1].contains("functions.bash"), "{lines:?}");
        assert!(!lines[1].contains("cargo test"), "{lines:?}");

        let mut frugal = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        frugal.agent_busy = true;
        frugal.context_mode = ContextMode::Frugal;
        frugal.apply_event(AgentEvent::TextDelta("to=".to_string()));
        frugal.apply_event(AgentEvent::TextDelta(
            "functions.bash {\"command\":\"cargo test\"}".to_string(),
        ));
        let text = transcript_live_indicator_text(&frugal, 80).expect("live indicator");
        let lines = flatten_lines(&text);
        assert!(
            lines[1].contains("tool call redacted"),
            "raw protocol should be hidden from frugal live UI: {lines:?}"
        );
        assert!(!lines[1].contains("to="), "{lines:?}");
        assert!(!lines[1].contains("functions.bash"), "{lines:?}");
        assert!(!lines[1].contains("cargo test"), "{lines:?}");
    }

    #[test]
    fn assistant_text_block_keeps_standard_legacy_payload_and_redacts_frugal_payload() {
        let mut standard = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        standard.apply_event(AgentEvent::TextBlockComplete(
            "to=functions.bash\n{\n  \"command\": \"cargo test\"\n}\nDone".to_string(),
        ));

        let Line_::Assistant { text, .. } = standard.pending_insert.last().expect("assistant line")
        else {
            panic!("expected assistant line");
        };
        assert!(text.contains("tool call redacted"), "{text}");
        assert!(text.contains("\"command\""), "{text}");
        assert!(text.contains("cargo test"), "{text}");

        let mut frugal = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        frugal.context_mode = ContextMode::Frugal;
        frugal.apply_event(AgentEvent::TextBlockComplete(
            "to=functions.bash\n{\n  \"command\": \"cargo test\"\n}\nDone".to_string(),
        ));

        let Line_::Assistant { text, .. } = frugal.pending_insert.last().expect("assistant line")
        else {
            panic!("expected assistant line");
        };
        assert!(text.contains("tool call redacted"), "{text}");
        assert!(text.contains("Done"), "{text}");
        assert!(!text.contains("to=functions"), "{text}");
        assert!(!text.contains("\"command\""), "{text}");
        assert!(!text.contains("cargo test"), "{text}");
    }

    #[test]
    fn assistant_text_block_redacts_multiline_raw_tool_protocol_payload_in_tiny() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.context_mode = ContextMode::Tiny;

        state.apply_event(AgentEvent::TextBlockComplete(
            "to=functions.bash\n{\n  \"command\": \"cargo test\"\n}\nDone".to_string(),
        ));

        let Line_::Assistant { text, .. } = state.pending_insert.last().expect("assistant line")
        else {
            panic!("expected assistant line");
        };
        assert!(text.contains("tool call redacted"), "{text}");
        assert!(text.contains("Done"), "{text}");
        assert!(!text.contains("to=functions"), "{text}");
        assert!(!text.contains("\"command\""), "{text}");
        assert!(!text.contains("cargo test"), "{text}");
    }

    #[test]
    fn inspector_keeps_standard_legacy_payload_and_redacts_tiny_payload() {
        let mut standard = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        standard.streaming_thinking =
            "to=functions.bash\n{\n  \"command\": \"cargo test\"\n}".to_string();

        let text = inspector_lines(&standard, 100, 20);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains("command"), "{joined}");
        assert!(joined.contains("cargo test"), "{joined}");

        let mut tiny = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        tiny.context_mode = ContextMode::Tiny;
        tiny.streaming_thinking =
            "to=functions.bash\n{\n  \"command\": \"cargo test\"\n}".to_string();
        let text = inspector_lines(&tiny, 100, 20);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains("tool call redacted"), "{joined}");
        assert!(!joined.contains("to=functions"), "{joined}");
        assert!(!joined.contains("cargo test"), "{joined}");
    }

    #[test]
    fn status_spans_hide_frugal_and_tiny_context_mode_labels() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.context_mode = ContextMode::Frugal;
        let joined = flatten_lines(&Text::from(Line::from(status_spans(&state)))).join("\n");
        assert!(!joined.contains("frugal"), "{joined}");

        state.context_mode = ContextMode::Tiny;
        let joined = flatten_lines(&Text::from(Line::from(status_spans(&state)))).join("\n");
        assert!(!joined.contains("tiny"), "{joined}");
    }

    #[test]
    fn todo_progress_surfaces_active_task_when_idle() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
        });

        let text = transcript_live_indicator_text(&state, 80).expect("live indicator");
        let lines = flatten_lines(&text);
        assert_eq!(lines.len(), 2);
        assert!(lines[0].contains("Running bash"));
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
            model_context_window("test-model"),
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
                Line_::Info(msg) if msg.starts_with("18 tools · ") => Some(msg.as_str()),
                _ => None,
            })
            .expect("turn summary");
        assert!(summary.starts_with("18 tools · "), "{summary}");
        assert!(summary.contains("1 search"), "{summary}");
        assert!(summary.contains("1 find"), "{summary}");
        assert!(summary.contains("1 write"), "{summary}");
        assert!(summary.contains("3 git ops"), "{summary}");
        assert!(summary.contains("2 todo ops"), "{summary}");
        assert!(summary.contains("3 data ops"), "{summary}");
        assert!(summary.contains("1 request"), "{summary}");
        assert!(summary.contains("1 other call"), "{summary}");
        assert!(summary.contains(" · no errors"), "{summary}");
    }

    #[test]
    fn completed_thinking_is_visible_by_default_before_next_event() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
    fn transcript_blocks_insert_single_blank_rows() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::ThinkingBlockComplete("one".to_string()));
        state.apply_event(AgentEvent::ThinkingBlockComplete("two".to_string()));
        state.apply_event(AgentEvent::ToolCallResult {
            call_id: "call_1".to_string(),
            name: "rg".to_string(),
            ok: true,
            preview: "rg: needle".to_string(),
            content: "match".to_string(),
        });
        state.apply_event(AgentEvent::TextBlockComplete("done".to_string()));

        assert!(matches!(
            state.pending_insert.as_slice(),
            [
                Line_::Thinking(_),
                Line_::Blank,
                Line_::Thinking(_),
                Line_::Blank,
                Line_::Tool { .. },
                Line_::Blank,
                Line_::Assistant { .. }
            ]
        ));
        assert_eq!(
            state
                .pending_insert
                .iter()
                .filter(|line| matches!(line, Line_::Blank))
                .count(),
            3
        );
    }

    #[test]
    fn work_map_packet_still_renders_in_transcript() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
        assert!(state.active_focus.is_none());
        assert!(matches!(
            state.pending_insert.last(),
            Some(Line_::WorkMap { .. })
        ));
        let lines = flatten_lines(&line_to_text(state.pending_insert.last().unwrap(), 80));
        assert!(
            lines.iter().any(|line| line.contains("Packet")),
            "{lines:?}"
        );
    }

    #[test]
    fn work_map_event_opens_input_drawer_and_keyboard_inserts_commands() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Map,
            text: "Session map — current\n@w01 intent #1  first\n@w02 change #2  second\ncommands: /focus @wNN".to_string(),
            waypoint_ids: vec!["@w01".to_string(), "@w02".to_string(), "@w99".to_string()],
            selector: None,
        });

        assert!(state.work_map_is_active());
        assert!(state.pending_insert.is_empty());
        let lines = flatten_lines(&Text::from(work_map_drawer_lines(&mut state, 80, 4)));
        assert!(
            lines.iter().any(|line| line.contains("Session map")),
            "{lines:?}"
        );
        assert!(
            lines.iter().any(|line| line.starts_with("▶ @w01")),
            "{lines:?}"
        );

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<FromTui>();
        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            &tx,
        );
        let lines = flatten_lines(&Text::from(work_map_drawer_lines(&mut state, 80, 4)));
        assert!(
            lines.iter().any(|line| line.starts_with("▶ @w02")),
            "{lines:?}"
        );
        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('b'), KeyModifiers::empty()),
            &tx,
        );

        assert_eq!(state.input, "/focus @w02 --branch");
        assert!(!state.work_map_is_active());

        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Focus,
            text: "[dext focus @w02 mode=exact]\nSafety: focus changes model context only"
                .to_string(),
            waypoint_ids: vec!["@w02".to_string()],
            selector: None,
        });
        assert_eq!(
            state
                .active_focus
                .as_ref()
                .map(|focus| (focus.selection.as_str(), focus.mode.as_str())),
            Some(("@w02", "exact"))
        );
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(rendered.contains("focus @w02 exact"), "{rendered}");
        state.apply_event(AgentEvent::Slash(
            "focus cleared; full session history is active again".to_string(),
        ));
        assert!(state.active_focus.is_none());

        state.input.clear();
        state.cursor = 0;
        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Map,
            text: "Session map — old-session\n@w01 intent #1  first".to_string(),
            waypoint_ids: vec!["@w01".to_string()],
            selector: Some("old-session".to_string()),
        });
        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('z'), KeyModifiers::empty()),
            &tx,
        );
        assert_eq!(state.input, "/map old-session ");
        assert!(
            state
                .work_map
                .as_ref()
                .is_some_and(|drawer| drawer.filter_input)
        );
        state.insert_input_str("failures");
        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &tx,
        );
        assert!(!state.work_map_is_active());
        assert_eq!(state.input, "");
        match rx.try_recv() {
            Ok(FromTui::Submit(text)) => assert_eq!(text, "/map old-session failures"),
            _ => panic!("expected filtered map submit"),
        }

        state.apply_event(AgentEvent::WorkMap {
            kind: WorkMapEventKind::Map,
            text: "Session map — old-session\n@w01 intent #1  first".to_string(),
            waypoint_ids: vec!["@w01".to_string()],
            selector: Some("old-session".to_string()),
        });
        handle_work_map_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &tx,
        );
        assert_eq!(state.input, "");
        match rx.try_recv() {
            Ok(FromTui::Submit(text)) => assert_eq!(text, "/focus old-session @w01"),
            _ => panic!("expected focus submit"),
        }
    }

    #[test]
    fn completed_thinking_can_be_hidden() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
    fn steering_history_has_distinct_highlight_and_gutter() {
        let text = line_to_text(
            &Line_::Steering("wolf = dext my bad. old names.".to_string()),
            80,
        );
        let lines = flatten_lines(&text);
        let body_style = span_style_for(&text, "wolf = dext").expect("steering body");
        let gutter_style = span_style_for(&text, "┃").expect("steering gutter");

        assert_eq!(
            lines.first().map(String::as_str),
            Some("┃ wolf = dext my bad. old names.")
        );
        assert!(lines.iter().all(|line| !line.contains(">>")));
        assert_eq!(body_style.bg, Some(STEERING_BG));
        assert_eq!(gutter_style.bg, Some(STEERING_BG));
        assert_ne!(body_style.bg, Some(THINKING_BG));
        assert_eq!(body_style.fg, Some(Color::Indexed(215)));
        assert_eq!(gutter_style.fg, Some(Color::Indexed(214)));
        assert!(gutter_style.add_modifier.contains(Modifier::BOLD));
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
            model_context_window("test-model"),
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
                Line_::Blank,
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
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
            model_context_window("test-model"),
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
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('o'), KeyModifiers::CONTROL),
            &tx,
            &runtime_control_tx,
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
            model_context_window("test-model"),
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
            model_context_window("gpt-5.4"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let turn = Usage {
            input: 120_000,
            output: 5_400,
            cache_create: 0,
            cache_read: 40_000,
            cost_usd: None,
        };
        let session = Usage {
            input: 828_300,
            output: 5_400,
            cache_create: 0,
            cache_read: 40_000,
            cost_usd: None,
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
            model_context_window("gpt-5.4"),
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
            cost_usd: None,
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
            model_context_window("gpt-5.4"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.usage = Usage {
            input: 47_000,
            output: 5_300,
            cache_create: 0,
            cache_read: 268_000,
            cost_usd: None,
        };
        let line = status_detail_spans(&state)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(line.contains("input 315.0k"), "{line}");
        assert!(line.contains("new 47.0k"), "{line}");
        assert!(line.contains("cache r 268.0k w 0"), "{line}");
        assert!(line.contains("out 5.3k"), "{line}");

        state.usage = Usage {
            input: 47_000,
            output: 5_300,
            cache_create: 0,
            cache_read: 0,
            cost_usd: None,
        };
        let line = status_detail_spans(&state)
            .into_iter()
            .map(|span| span.content.into_owned())
            .collect::<String>();
        assert!(line.contains("input 47.0k"), "{line}");
        assert!(!line.contains("cache r"), "{line}");
        assert!(line.contains("out 5.3k"), "{line}");
    }

    #[test]
    fn ctx_meter_sums_anthropic_cache_create_read_and_output() {
        // Native usage totals include output tokens; context pressure includes
        // input, output, cache reads, and cache writes.
        let mut state = TuiState::new(
            "claude-opus-4-6".to_string(),
            model_context_window("claude-opus-4-6"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let turn = Usage {
            input: 8_000,
            output: 500,
            cache_create: 12_000,
            cache_read: 40_000,
            cost_usd: None,
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
            model_context_window("gpt-5.4"),
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

        assert!(rendered.contains("Ctx ["), "{rendered}");
        assert!(rendered.contains("59%"), "{rendered}");
        let details = status_detail_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(details.contains("160.0k/272.0k"), "{details}");
    }

    #[test]
    fn status_detail_spans_render_external_telemetry_counters() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
            empty_tool_call_hints: 6,
        };
        let rendered = status_detail_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(
            rendered.contains("ext d2 cb3 sg1 ph1 rt4 et6"),
            "{rendered}"
        );
    }

    #[test]
    fn status_detail_spans_render_only_nonzero_external_telemetry_counters() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
            empty_tool_call_hints: 0,
        };
        let rendered = status_detail_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(rendered.contains("ext sg5"), "{rendered}");
        assert!(!rendered.contains("d0"), "{rendered}");
        assert!(!rendered.contains("cb0"), "{rendered}");
        assert!(!rendered.contains("ph0"), "{rendered}");
        assert!(!rendered.contains("rt0"), "{rendered}");
        assert!(!rendered.contains("et0"), "{rendered}");
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
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::TurnDiagnostics {
            provider: "chatgpt".to_string(),
            api_family: "chatgpt-responses".to_string(),
            auth_source: "auth:chatgpt".to_string(),
            model: "gpt-4o".to_string(),
            context_window: Some(128_000),
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
        assert!(rendered.contains(" │ GPT-4o "), "{rendered}");
        assert!(!rendered.contains("chatgpt/"), "{rendered}");
        assert!(!rendered.contains("chatgpt:chatgpt"), "{rendered}");
        assert!(!rendered.contains("chatgpt-responses"), "{rendered}");
        assert!(rendered.contains("GPT-4o"), "{rendered}");
    }

    #[test]
    fn status_spans_do_not_render_context_mode_label() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.context_mode = ContextMode::Tiny;

        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();

        assert!(!rendered.contains("tiny"), "{rendered}");
        assert!(!rendered.contains("frugal"), "{rendered}");
    }

    #[test]
    fn turn_diagnostics_updates_context_mode_without_status_label() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );

        state.apply_event(AgentEvent::TurnDiagnostics {
            provider: "test".to_string(),
            api_family: "test".to_string(),
            auth_source: "test".to_string(),
            model: "test-model".to_string(),
            context_window: None,
            last_retry_reason: None,
            workaround_fired: false,
            turn_duration_ms: None,
            context_mode: Some(ContextMode::Tiny),
            tool_profile: None,
            compacted: None,
        });

        assert_eq!(state.context_mode, ContextMode::Tiny);
        let rendered = status_spans(&state)
            .into_iter()
            .map(|span| span.content)
            .collect::<String>();
        assert!(!rendered.contains("tiny"), "{rendered}");
        assert!(!rendered.contains("frugal"), "{rendered}");
    }

    #[test]
    fn derived_busy_status_prefers_live_tool_graph() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
        });
        assert_eq!(derived_busy_status(&state), "running bash");
    }

    #[test]
    fn derived_busy_status_shows_retry_when_no_active_tools() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
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
        assert!(result.contains("[paste #1 +60 words hidden]"));
        assert!(!result.contains("full content preserved"));
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
        assert!(result.contains("[paste #1 +60 words hidden]"));
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
        assert!(result.contains("[paste #1 +55 words hidden]"));
        assert!(result.contains("[paste #2 +80 words hidden]"));
        assert!(result.contains("do stuff"));
        assert!(result.contains("middle text"));
    }

    #[test]
    fn paste_preview_survives_typing_after_collapsed_paste() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let paste = (0..60).map(|i| format!("word{i}\n")).collect::<String>();
        handle_paste(&mut state, paste.clone());
        let initial_preview = state.input_display_override.clone().expect("paste preview");
        assert!(initial_preview.contains("[paste #1 +60 words hidden]"));
        assert!(!initial_preview.contains("full content preserved"));
        assert_eq!(state.status, "large paste collapsed in editor");
        assert!(!initial_preview.contains("word30"));

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(AtomicBool::new(false));
        for c in " done".chars() {
            let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Char(c), KeyModifiers::empty()),
                &tx,
                &runtime_control_tx,
                &steering_tx,
                &interrupt,
            );
        }

        let preview = state.input_display_override.expect("preview after typing");
        assert!(preview.contains("[paste #1 +60 words hidden]"), "{preview}");
        assert!(preview.ends_with(" done"), "{preview}");
        assert!(!preview.contains("word30"), "{preview}");
        assert_eq!(state.input, format!("{} done", paste));
    }

    #[test]
    fn shift_or_alt_enter_inserts_newline_without_submit() {
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut state = TuiState::new(
                "test-model".to_string(),
                model_context_window("test-model"),
                ".".to_string(),
                ApprovalProfile::Ask,
                ThinkingEffort::Medium,
            );
            state.replace_input("hello".to_string());

            let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel();
            let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
            let interrupt = Arc::new(AtomicBool::new(false));
            let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, modifiers),
                &submit_tx,
                &runtime_control_tx,
                &steering_tx,
                &interrupt,
            );

            assert_eq!(state.input, "hello\n");
            assert!(submit_rx.try_recv().is_err());
            assert!(steering_rx.try_recv().is_err());
        }
    }

    #[test]
    #[cfg(unix)]
    fn keyboard_enhancement_skips_apple_terminal_unless_forced() {
        assert!(!terminal_identity_supports_keyboard_enhancement(
            "xterm-256color",
            "Apple_Terminal",
            false
        ));
        assert!(terminal_identity_supports_keyboard_enhancement(
            "xterm-kitty",
            "",
            false
        ));
        assert!(terminal_identity_supports_keyboard_enhancement(
            "xterm-256color",
            "WezTerm",
            true
        ));
    }

    #[test]
    #[cfg(not(unix))]
    fn keyboard_enhancement_is_off_for_non_unix() {
        assert!(!terminal_supports_keyboard_enhancement());
    }

    #[test]
    fn slash_completion_arrows_select_without_replacing_input_or_history() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
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
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            &tx,
            &runtime_control_tx,
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
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );
        assert_eq!(state.slash_acomp_sel, Some(0));
    }

    #[test]
    fn busy_enter_routes_runtime_controls_separately_from_steering() {
        let cases = [
            (
                "please adjust the current fix",
                false,
                vec!["please adjust the current fix"],
            ),
            (
                "/model chatgpt/gpt-5.3-codex",
                true,
                vec!["/model chatgpt/gpt-5.3-codex"],
            ),
            ("/effort high", true, vec!["/effort high"]),
            (
                "/model chatgpt/gpt-5.3-codex, /effort xhigh",
                true,
                vec!["/model chatgpt/gpt-5.3-codex", "/effort xhigh"],
            ),
            ("/compact status", false, Vec::new()),
        ];
        for (input, runtime_control, expected_messages) in cases {
            let mut state = TuiState::new(
                "glm-5.1".to_string(),
                model_context_window("glm-5.1"),
                ".".to_string(),
                ApprovalProfile::Ask,
                ThinkingEffort::Medium,
            );
            state.agent_busy = true;
            state.input = input.to_string();
            state.cursor = state.input.len();

            let (submit_tx, mut submit_rx) = tokio::sync::mpsc::unbounded_channel();
            let (runtime_control_tx, mut runtime_control_rx) =
                tokio::sync::mpsc::unbounded_channel();
            let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
            let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
                &submit_tx,
                &runtime_control_tx,
                &steering_tx,
                &interrupt,
            );

            if runtime_control {
                for expected in expected_messages {
                    assert_eq!(
                        runtime_control_rx.try_recv().ok().as_deref(),
                        Some(expected)
                    );
                }
                assert!(runtime_control_rx.try_recv().is_err());
                assert!(steering_rx.try_recv().is_err());
                assert_eq!(state.status, "runtime control queued");
                assert!(state.pending_insert.is_empty());
            } else if expected_messages.is_empty() {
                assert!(steering_rx.try_recv().is_err());
                assert!(runtime_control_rx.try_recv().is_err());
                assert_eq!(state.status, "slash command waits until idle");
                assert!(matches!(
                    state.pending_insert.as_slice(),
                    [Line_::Warn(s)] if s.contains("not run while agent is busy")
                ));
            } else {
                assert_eq!(
                    steering_rx.try_recv().ok().as_deref(),
                    expected_messages.first().copied()
                );
                assert!(runtime_control_rx.try_recv().is_err());
                assert_eq!(state.status, "queued for next safe boundary");
                assert!(matches!(
                    state.pending_insert.as_slice(),
                    [Line_::Blank, Line_::Steering(s), Line_::Blank] if s == input
                ));
            }
            assert!(submit_rx.try_recv().is_err());
            assert!(state.input.is_empty());
        }
    }

    #[test]
    fn busy_input_withholds_potential_local_secret_from_steering() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
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
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Enter, KeyModifiers::empty()),
            &submit_tx,
            &runtime_control_tx,
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
            model_context_window("glm-5.1"),
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
    fn local_auth_submit_sends_secret_only_to_responder() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        queue_local_auth_secret_request(
            &mut state,
            "bash".to_string(),
            "sudo auth".to_string(),
            tx,
        );
        state.local_auth_input = "hunter2".to_string();

        submit_local_auth_secret(&mut state);

        match rx.try_recv().expect("local auth response") {
            LocalAuthSecret::Secret(secret) => assert_eq!(secret, "hunter2"),
            LocalAuthSecret::Canceled | LocalAuthSecret::Unavailable => {
                panic!("expected submitted secret")
            }
        }
        assert!(state.local_auth_input.is_empty());
        assert!(state.pending_local_auth.is_none());
        assert!(state.input.is_empty());
        assert_eq!(state.status, "local auth submitted for bash");
    }

    #[test]
    fn local_auth_replacement_cancels_previous_and_clears_input() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let (first_tx, first_rx) = std::sync::mpsc::sync_channel(1);
        let (second_tx, _second_rx) = std::sync::mpsc::sync_channel(1);
        queue_local_auth_secret_request(
            &mut state,
            "bash".to_string(),
            "first".to_string(),
            first_tx,
        );
        state.local_auth_input = "typed-secret".to_string();

        queue_local_auth_secret_request(
            &mut state,
            "bash".to_string(),
            "second".to_string(),
            second_tx,
        );

        assert!(matches!(
            first_rx.try_recv().expect("first prompt canceled"),
            LocalAuthSecret::Canceled
        ));
        assert!(state.local_auth_input.is_empty());
        assert_eq!(
            state
                .pending_local_auth
                .as_ref()
                .map(|pending| pending.message.as_str()),
            Some("second")
        );
    }

    #[test]
    fn local_auth_submit_failure_clears_input_and_unblocks_overlay() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        drop(rx);
        queue_local_auth_secret_request(
            &mut state,
            "bash".to_string(),
            "sudo auth".to_string(),
            tx,
        );
        state.local_auth_input = "hunter2".to_string();

        submit_local_auth_secret(&mut state);

        assert!(state.local_auth_input.is_empty());
        assert!(state.pending_local_auth.is_none());
        assert_eq!(state.status, "local auth unavailable for bash");
    }

    #[test]
    fn slash_completion_arrows_scroll_past_visible_window() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/".to_string();
        state.cursor = state.input.len();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        for _ in 0..SLASH_COMPLETION_MAX_VISIBLE {
            handle_key(
                &mut state,
                KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
                &tx,
                &runtime_control_tx,
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
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/".to_string();
        state.cursor = state.input.len();

        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Down, KeyModifiers::empty()),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &tx,
            &runtime_control_tx,
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
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "regular prompt".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );

        assert!(matches!(rx.try_recv(), Ok(FromTui::CycleEffort(1))));
        assert_eq!(state.input, "regular prompt");
    }

    #[test]
    fn tab_queues_runtime_effort_when_busy_and_not_completing_slash_command() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.input = "regular prompt".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, mut runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(
            runtime_control_rx.try_recv().ok().as_deref(),
            Some("/effort next")
        );
        assert_eq!(state.status, "runtime control queued");
        assert_eq!(state.input, "regular prompt");
    }

    #[test]
    fn backtab_cycles_effort_when_not_completing_slash_command() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "regular prompt".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );

        assert!(matches!(rx.try_recv(), Ok(FromTui::CycleEffort(-1))));
        assert_eq!(state.input, "regular prompt");
    }

    #[test]
    fn backtab_queues_runtime_effort_when_busy_and_not_completing_slash_command() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;
        state.input = "regular prompt".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, mut runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::BackTab, KeyModifiers::SHIFT),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(
            runtime_control_rx.try_recv().ok().as_deref(),
            Some("/effort prev")
        );
        assert_eq!(state.status, "runtime control queued");
        assert_eq!(state.input, "regular prompt");
    }

    #[test]
    fn ctrl_e_queues_runtime_effort_when_busy() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.agent_busy = true;

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, mut runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('e'), KeyModifiers::CONTROL),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );

        assert!(rx.try_recv().is_err());
        assert_eq!(
            runtime_control_rx.try_recv().ok().as_deref(),
            Some("/effort next")
        );
        assert_eq!(state.status, "runtime control queued");
    }

    #[test]
    fn tab_falls_back_to_effort_when_slash_has_no_completion() {
        let mut state = TuiState::new(
            "glm-5.1".to_string(),
            model_context_window("glm-5.1"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.input = "/definitely-no-such-command".to_string();
        state.cursor = state.input.len();

        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Tab, KeyModifiers::empty()),
            &tx,
            &runtime_control_tx,
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
            model_context_window("glm-5.1"),
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
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let banner = welcome_banner(
            "/home/abaka/Documents/Projects/dext",
            "test-model",
            ThinkingEffort::Medium,
            ContextMode::Tiny,
            ApprovalProfile::Always,
            5,
        );
        state.queue(Line_::Banner(banner));
        let size = terminal.size().expect("terminal size");
        let width = transcript_pane_width(size.width, size.height, &state);
        flush_pending_insert(&mut terminal, &mut state, width).expect("flush banner");

        let mut first = vec![tool_line(
            "#1.44",
            "read_file",
            "read_file: src/main.rs (offset=6410, limit=2)",
            Some(true),
            "6410\talpha\n6411\tbravo\n",
        )];
        flush_prepared_items(&mut terminal, &mut state, &mut first, width).expect("flush first");
        let mut second = vec![tool_line(
            "#1.45",
            "read_file",
            "read_file: src/main.rs (offset=6412, limit=2)",
            Some(true),
            "6412\tcharlie\n6413\tdelta\n",
        )];
        flush_prepared_items(&mut terminal, &mut state, &mut second, width).expect("flush second");

        assert_eq!(state.transcript.len(), 3);
        assert!(matches!(state.transcript[0], Line_::Banner(_)));
        assert!(matches!(state.transcript[1], Line_::Tool { .. }));
        assert!(matches!(state.transcript[2], Line_::Tool { .. }));
        assert!(!state.transcript_needs_rebuild);
    }

    #[test]
    fn resize_rebuilds_existing_transcript_for_current_pane_width() {
        use ratatui::backend::TestBackend;
        use ratatui::{Terminal, TerminalOptions, Viewport};

        let backend = TestBackend::new(90, 20);
        let mut terminal = Terminal::with_options(
            backend,
            TerminalOptions {
                viewport: Viewport::Inline(8),
            },
        )
        .expect("terminal");
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.queue(Line_::Assistant {
            text: "alpha beta gamma delta epsilon zeta eta theta iota kappa lambda mu ".repeat(5),
            dim_prefix: false,
        });

        let wide = current_transcript_pane_width(&mut terminal, &state).expect("wide width");
        flush_pending_insert(&mut terminal, &mut state, wide).expect("wide flush");
        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript_rendered_width, wide);

        terminal.backend_mut().resize(38, 20);
        let narrow = current_transcript_pane_width(&mut terminal, &state).expect("narrow width");
        assert!(narrow < wide);
        flush_pending_insert(&mut terminal, &mut state, narrow).expect("narrow flush");
        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript_rendered_width, narrow);
        assert!(!state.transcript_needs_rebuild);

        let item = state.transcript[0].clone();
        let key = line_cache_key(&item);
        let wide_render_width = transcript_render_width(wide);
        let narrow_render_width = transcript_render_width(narrow);
        {
            let entry = state.render_cache.get(&key).expect("cache entry");
            assert!(entry.renders.contains_key(&wide_render_width));
            assert!(entry.renders.contains_key(&narrow_render_width));
            assert!(entry.heights[&narrow_render_width] >= entry.heights[&wide_render_width]);
        }

        terminal.backend_mut().resize(110, 20);
        let wider = current_transcript_pane_width(&mut terminal, &state).expect("wider width");
        assert!(wider > narrow);
        flush_pending_insert(&mut terminal, &mut state, wider).expect("wider flush");
        assert_eq!(state.transcript.len(), 1);
        assert_eq!(state.transcript_rendered_width, wider);
        assert!(!state.transcript_needs_rebuild);

        let wider_render_width = transcript_render_width(wider);
        let entry = state.render_cache.get(&key).expect("cache entry");
        assert!(entry.renders.contains_key(&wider_render_width));
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
            model_context_window("glm-5.1"),
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
            model_context_window("glm-5.1"),
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
            model_context_window("glm-5.1"),
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
    fn table_grid_widths_require_room_for_true_borders() {
        let table = ParsedTable {
            rows: vec![vec!["a".into(), "b".into(), "c".into()]],
            header_rows: 0,
            alignments: vec![
                TableColumnAlignment::Left,
                TableColumnAlignment::Left,
                TableColumnAlignment::Left,
            ],
        };
        assert!(table_grid_widths(&table, 20).is_some());
        assert!(table_grid_widths(&table, 18).is_none());
    }

    #[test]
    fn render_table_text_produces_bold_header_with_borders() {
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
        assert!(!flat.is_empty(), "should have rows");
        assert!(
            flat[0].starts_with('┌') && flat[0].ends_with('┐'),
            "table should start with a top border: {flat:?}"
        );
        assert!(
            flat.last()
                .is_some_and(|line| line.starts_with('└') && line.ends_with('┘')),
            "table should end with a bottom border: {flat:?}"
        );
        let header_line = flat
            .iter()
            .find(|line| line.contains("Key") && line.contains("Value"))
            .expect("header row");
        assert!(
            header_line.starts_with('│') && header_line.ends_with('│'),
            "header row should be inside side borders: {flat:?}"
        );
        assert!(
            flat.iter()
                .any(|line| line.contains("name") && line.contains("dext")),
            "data row should contain name/dext: {flat:?}"
        );

        let header_style = rendered
            .iter()
            .find(|line| line.spans.iter().any(|span| span.content.contains("Key")))
            .and_then(|line| {
                line.spans
                    .iter()
                    .find(|span| span.content.contains("Key"))
                    .map(|span| span.style)
            })
            .expect("header span");
        assert!(
            header_style.fg.is_none() || header_style.fg == Some(Color::Reset),
            "header fg should inherit terminal default/reset: {header_style:?}"
        );
        assert!(header_style.add_modifier.contains(Modifier::BOLD));
    }

    #[test]
    fn markdown_text_renders_generic_status_table_with_borders_and_surrounding_prose() {
        let input =
            "## Results\n\n| Tool | Status |\n| --- | ------ |\n| rg | ok |\n| fd | ok |\n\nDone.";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(
            flat.iter()
                .any(|line| line.starts_with('┌') && line.ends_with('┐')),
            "should render table with a top border: {joined}"
        );
        assert!(
            flat.iter()
                .any(|line| line.contains("Tool") && line.contains("Status")),
            "should keep table header: {joined}"
        );
        assert!(
            flat.iter()
                .any(|line| line.contains("rg") && line.contains("PASS")),
            "{joined}"
        );
        assert!(
            flat.iter()
                .any(|line| line.contains("fd") && line.contains("PASS")),
            "{joined}"
        );
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
        assert!(flat.iter().any(|line| line.starts_with('┌')), "{joined}");
        assert!(
            flat.iter()
                .any(|line| line.contains("rg") && line.contains("PASS")),
            "{joined}"
        );
        assert!(!joined.contains('\r'));
    }

    #[test]
    fn markdown_text_unwraps_markdown_fenced_tables() {
        let input = "```md\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```";
        let text = markdown_text(input, Style::default(), 120);
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(joined.contains('┌'), "{joined}");
        assert!(joined.contains("A") && joined.contains("B"), "{joined}");
        assert!(joined.contains("1") && joined.contains("2"), "{joined}");
        assert!(!joined.contains("```"), "{joined}");
    }

    #[test]
    fn markdown_text_unwraps_markdown_fenced_tables_with_attributes() {
        let input = "```{.markdown}\n| A | B |\n| --- | --- |\n| 1 | 2 |\n```";
        let text = markdown_text(input, Style::default(), 120);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains('┌'), "{joined}");
        assert!(joined.contains("A") && joined.contains("B"), "{joined}");
        assert!(joined.contains("1") && joined.contains("2"), "{joined}");
        assert!(!joined.contains("```"), "{joined}");
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
    fn status_normalization_respects_header_and_preserves_regular_yes_no() {
        let input = "| Question | Answer |\n| --- | --- |\n| Enabled | Yes |\n| Disabled | No |";
        let text = markdown_text(input, Style::default(), 120);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains("Enabled"), "{joined}");
        assert!(joined.contains("Yes"), "{joined}");
        assert!(joined.contains("Disabled"), "{joined}");
        assert!(joined.contains("No"), "{joined}");
        assert!(!joined.contains("PASS"), "{joined}");
        assert!(!joined.contains("FAIL"), "{joined}");
    }

    #[test]
    fn status_normalization_handles_marked_up_status_header() {
        let input = "| Check | **Status** |\n| --- | --- |\n| Server | yes |\n| Cache | no |";
        let text = markdown_text(input, Style::default(), 120);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains("Server"), "{joined}");
        assert!(joined.contains("PASS"), "{joined}");
        assert!(joined.contains("Cache"), "{joined}");
        assert!(joined.contains("FAIL"), "{joined}");
        assert!(!joined.contains("yes"), "{joined}");
        assert!(!joined.contains("no"), "{joined}");
    }

    #[test]
    fn status_cells_use_single_terminal_safe_token() {
        let input = "| Check | Result |\n| --- | --- |\n| Server | ✅ Yes |\n| Cache | ✓ No |";
        let text = markdown_text(input, Style::default(), 120);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains('┌'), "{joined}");
        assert!(joined.contains("Server"), "{joined}");
        assert!(joined.contains("PASS"), "{joined}");
        assert!(joined.contains("Cache"), "{joined}");
        assert!(joined.contains("FAIL"), "{joined}");
        assert!(!joined.contains("✅"), "{joined}");
        assert!(!joined.contains("Yes"), "{joined}");
        assert!(!joined.contains("No"), "{joined}");
    }

    #[test]
    fn leading_emoji_cells_do_not_shift_right_table_border() {
        let input = "| Dimension | My best guess | Confirm? |\n| --- | --- | --- |\n| Degree | PhD | ✅ clear |\n| Field | CS / Cybersecurity / AI | ⚠️ confirm |\n| GRE | Optional/waived | ⚠️ need this |";
        let text = line_to_text(
            &Line_::Assistant {
                text: input.to_string(),
                dim_prefix: false,
            },
            120,
        );
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(joined.contains('┌'), "{joined}");
        assert!(joined.contains("✅ clear"), "{joined}");
        assert!(joined.contains("⚠️ confirm"), "{joined}");
        assert!(joined.contains("⚠️ need this"), "{joined}");
        assert!(!joined.contains("✅  clear"), "{joined}");
        assert!(!joined.contains("⚠️  confirm"), "{joined}");
        let table_edge = |line: &str| {
            let idx = line.rfind(['┐', '┤', '┘', '│']).expect("table right edge");
            let edge = line[idx..].chars().next().expect("right edge marker");
            text_width(&line[..idx]) + text_width(&line[idx..idx + edge.len_utf8()])
        };
        let border_width = flat
            .iter()
            .find(|line| line.starts_with("│ ┌"))
            .map(|line| table_edge(line))
            .expect("top border");
        let emoji_rows = flat
            .iter()
            .filter(|line| line.starts_with("│ │") && (line.contains('✅') || line.contains('⚠')))
            .collect::<Vec<_>>();
        assert_eq!(emoji_rows.len(), 3, "{flat:?}");
        assert!(
            emoji_rows
                .iter()
                .all(|line| table_edge(line) == border_width),
            "emoji rows should keep separators and right border aligned with the table edge: {flat:?}"
        );
    }

    #[test]
    fn mixed_emoji_table_keeps_borders_aligned_across_widths() {
        // Exercises the symbol classes the user reported (🪙🧬 natural-wide emoji,
        // ⚙️☢️ VS16-presentation, 🛒). The table must keep every data row's right
        // border aligned regardless of terminal width; clip/split paths must never
        // split a multi-codepoint cluster across a boundary.
        let input = "| Theme | Tickers | Sidebar nav |\n| --- | --- | --- |\n\
            | 🪙 Commodity | FSM, KGC, WPM, AG | Commodity |\n\
            | 🧬 Biotech | VEEV, REGN, GILD, BMRN | Biotech |\n\
            | ⚙️ Reshoring | BMI, KAI, AOS, MSA | Reshoring |\n\
            | ☢️ Nuclear | CCJ, LEU, BWXT | Nuclear |\n\
            | 🛒 China | PDD, YMM, EDU, TME, JD | China |";
        for w in [40usize, 50, 58, 60, 80, 120] {
            let text = line_to_text(
                &Line_::Assistant {
                    text: input.to_string(),
                    dim_prefix: false,
                },
                w as u16,
            );
            let flat = flatten_lines(&text);
            let joined = flat.join("\n");
            // No cluster ever split: VS16 must stay glued to its base.
            assert!(
                !joined.contains("⚙ \u{fe0f}"),
                "split ⚙/VS16 at w={w}: {joined}"
            );
            assert!(
                !joined.contains("☢ \u{fe0f}"),
                "split ☢/VS16 at w={w}: {joined}"
            );
            // Emoji preserved intact.
            assert!(joined.contains('🪙'), "lost 🪙 at w={w}: {joined}");
            assert!(joined.contains("⚙️"), "lost ⚙️ at w={w}: {joined}");
            // Every line with a box edge must be no wider than the widest line.
            let max_w = flat.iter().map(|l| text_width(l)).max().unwrap_or(0);
            assert!(
                flat.iter().all(|l| text_width(l) <= max_w),
                "row exceeds table edge at w={w}: {flat:?}"
            );
        }
    }

    #[test]
    fn display_clusters_groups_vs16_and_keeps_width_terminal_correct() {
        let clusters = display_clusters("⚙️🪙☢️ab");
        let widths: Vec<usize> = clusters.iter().map(|c| c.width).collect();
        assert_eq!(widths, vec![2, 2, 2, 1, 1], "{clusters:?}");
        // VS16 is glued to its base, never its own cluster.
        assert_eq!(clusters[0].byte_len, "⚙️".len());
        assert_eq!(clusters[2].byte_len, "☢️".len());
    }

    #[test]
    fn display_clusters_unifies_zwj_family_emoji() {
        // 👨‍👩‍👧 (man + ZWJ + woman + ZWJ + girl) renders as one 2-cell family
        // glyph. Per-char width would over-count as 6; the cluster must stay whole.
        let clusters = display_clusters("👨‍👩‍👧");
        assert_eq!(
            clusters.len(),
            1,
            "family must be a single cluster: {clusters:?}"
        );
        assert_eq!(clusters[0].width, 2, "{clusters:?}");
        assert_eq!(clusters[0].byte_len, "👨‍👩‍👧".len());
    }

    #[test]
    fn display_clusters_pairs_regional_indicator_flags() {
        // 🇺🇸 = two regional indicators forming one 2-cell flag.
        let clusters = display_clusters("🇺🇸");
        assert_eq!(clusters.len(), 1, "flag must be one cluster: {clusters:?}");
        assert_eq!(clusters[0].width, 2, "{clusters:?}");
        // Two adjacent flags = two clusters, not four half-flags.
        let two = display_clusters("🇺🇸🇬🇧");
        assert_eq!(two.len(), 2, "{two:?}");
    }

    #[test]
    fn clamp_chars_plain_never_splits_emoji_cluster() {
        // Clipping must keep ⚙️ whole (drop the whole cluster, never split base
        // from its VS16), so the ellipsis lands on a cluster boundary.
        let out = clamp_chars_plain("⚙️xyz", 3);
        assert!(!out.contains('\u{fe0f}') || out.contains("⚙️"), "{out}");
        assert!(out.ends_with('…'), "{out}");
        // A cluster that fits entirely stays intact.
        assert_eq!(clamp_chars_plain("⚙️", 5), "⚙️");
    }

    #[test]
    fn split_display_cells_keeps_emoji_whole_at_boundary() {
        // Width-2 emoji at the exact boundary must not be split mid-sequence.
        let (head, tail) = split_display_cells("ab🪙cd", 3);
        assert!(!head.contains('🪙'), "split emoji into head: {head}|{tail}");
        assert_eq!(head, "ab");
    }

    #[test]
    fn clipped_emoji_bullet_lines_keep_swim_lane_border() {
        let input = "- That prevents the hidden blank cell after ✅ / ⚠️ from being emitted again and shifting the separator/right border.\n- Added regression: leading_emoji_cells_do_not_shift_right_table_border\n    - exercises actual assistant render path\n    - keeps ✅ clear, ⚠️ confirm, ⚠️ need this\n    - asserts table edge alignment stays stable on emoji rows.";
        let text = line_to_text(
            &Line_::Assistant {
                text: input.to_string(),
                dim_prefix: false,
            },
            80,
        );
        let flat = flatten_lines(&text);
        let joined = flat.join("\n");
        assert!(joined.contains("✅ / ⚠️"), "{joined}");
        assert!(joined.contains("✅ clear"), "{joined}");
        assert!(!joined.contains("✅  / ⚠️"), "{joined}");
        assert!(!joined.contains("⚠️  from"), "{joined}");
        assert!(!joined.contains("✅  clear"), "{joined}");
        assert!(!joined.contains("⚠️  confirm"), "{joined}");
        assert!(
            flat.iter()
                .filter(|line| !line.is_empty() && !line.starts_with("┌") && !line.starts_with("└"))
                .all(|line| line.starts_with("│ ") && text_width(line) <= 80),
            "body lines should keep the left swim-lane border and stay inside width: {flat:?}"
        );
    }

    #[test]
    fn overloaded_table_renders_as_bordered_table() {
        let input = "| Area | Item | Result | Value | Recommendation |\n| --- | --- | --- | --- | --- |\n| Check | Model installed | ✅ Yes | | |\n| Performance | Best observed generation | | 10.87 tok/s | |\n| Constraint | WSL currently exposes | | ~15 GiB | |\n| Next step | Increase WSL memory | | | edit ~/.wslconfig and restart WSL |";
        let text = markdown_text(input, Style::default(), 120);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains('┌'), "{joined}");
        assert!(joined.contains("Area"), "{joined}");
        assert!(joined.contains("Item"), "{joined}");
        assert!(joined.contains("Result"), "{joined}");
        assert!(joined.contains("Recommendation"), "{joined}");
        assert!(joined.contains("Model installed"), "{joined}");
        assert!(joined.contains("PASS"), "{joined}");
        assert!(joined.contains("Best observed generation"), "{joined}");
        assert!(joined.contains("10.87 tok/s"), "{joined}");
        assert!(joined.contains("~15 GiB"), "{joined}");
        assert!(joined.contains("edit ~/.wslconfig"), "{joined}");
    }

    #[test]
    fn markdown_text_removes_common_raw_markers() {
        let input =
            "## Bottom Line\n\n**10.87 tok/s** and `inline` code\n\n```rust\nfn main() {}\n```";
        let text = markdown_text(input, Style::default(), 120);
        let joined = flatten_lines(&text).join("\n");
        assert!(joined.contains("Bottom Line"), "{joined}");
        assert!(joined.contains("10.87 tok/s"), "{joined}");
        assert!(joined.contains("inline"), "{joined}");
        assert!(joined.contains("fn main()"), "{joined}");
        assert!(!joined.contains("##"), "{joined}");
        assert!(!joined.contains("**"), "{joined}");
        assert!(!joined.contains("```"), "{joined}");
    }

    #[test]
    fn phase_info_renders_friendly_status_without_internal_label() {
        let text = line_to_text(
            &Line_::Info("[phase:synthesize] preparing final response".to_string()),
            80,
        );
        let joined = flatten_lines(&text).join("\n");
        assert!(
            joined.contains("Final response: preparing final response"),
            "{joined}"
        );
        assert!(!joined.contains("phase:"), "{joined}");
    }

    #[test]
    fn objective_info_renders_actual_objective() {
        let text = line_to_text(
            &Line_::Info("[objective: fix status bar | checkpoints: branch visible]".to_string()),
            100,
        );
        let joined = flatten_lines(&text).join("\n");
        assert!(
            joined.contains("Objective: fix status bar | checkpoints: branch visible"),
            "{joined}"
        );
        assert!(!joined.contains("agent objective noted"), "{joined}");
    }

    #[test]
    fn draw_empty_composer_is_compact_and_clears_debug_artifacts() {
        let mut state = TuiState::new(
            "gpt-5.4".to_string(),
            model_context_window("gpt-5.4"),
            "/home/abaka/Documents/Projects/Dext".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.last_turn_context_tokens = 80_000;
        let lines = draw_to_lines(120, 24, &mut state);
        let joined = lines.join("\n");
        assert!(!joined.contains("+12 chars"), "{joined}");
        assert!(joined.contains("Type a request…"), "{joined}");
        assert!(joined.contains("Ctx ["), "{joined}");
        assert!(
            compute_layout(Rect::new(0, 0, 120, 24), &state)
                .input_area
                .height
                <= 3
        );
    }

    #[test]
    fn transcript_width_uses_available_terminal_space() {
        assert_eq!(transcript_render_width(80), 79);
        assert_eq!(transcript_render_width(160), 159);
        let rect = transcript_content_rect(Rect::new(0, 0, 160, 20));
        assert_eq!(rect.width, 160);
        assert_eq!(rect.x, 0);
    }

    #[test]
    fn wide_layout_only_splits_when_inspector_is_visible() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let wide = Rect::new(0, 0, 180, 32);
        let normal = compute_layout(wide, &state);
        assert_eq!(normal.inspector_area.width, 0);
        assert_eq!(normal.transcript_area.width, 180);

        state.show_inspector = true;
        let inspected = compute_layout(wide, &state);
        assert!(inspected.inspector_area.width >= 34, "{inspected:?}");
        assert_eq!(
            inspected.transcript_area.width + inspected.inspector_area.width,
            180
        );
    }

    #[test]
    fn ansi_and_wide_text_width_are_display_safe() {
        assert_eq!(text_width("\u{1b}[31mPASS\u{1b}[0m"), 4);
        assert_eq!(text_width("界"), 2);
        assert_eq!(text_width("✓"), 1);
    }

    #[test]
    fn clean_table_status_cell_normalizes_status_signals_safely() {
        assert_eq!(clean_table_status_cell("✅ Yes"), "PASS");
        assert_eq!(clean_table_status_cell("✓ No"), "FAIL");
        assert_eq!(clean_table_status_cell("❌ Yes"), "FAIL");
        assert_eq!(clean_table_status_cell("✅ No"), "FAIL");
    }

    #[test]
    fn empty_input_panel_defaults_to_three_rows() {
        let state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        assert_eq!(input_panel_height(&state, 24, 80), 3);
    }

    #[test]
    fn status_details_are_expanded_by_ctrl_t() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('t'), KeyModifiers::CONTROL),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );
        assert!(state.show_status_details);
        assert_eq!(
            compute_layout(Rect::new(0, 0, 80, 20), &state)
                .status_area
                .height,
            2
        );
    }

    #[test]
    fn inspector_is_toggled_by_ctrl_i_and_renders_debug_events() {
        let mut state = TuiState::new(
            "test-model".to_string(),
            model_context_window("test-model"),
            ".".to_string(),
            ApprovalProfile::Ask,
            ThinkingEffort::Medium,
        );
        state.apply_event(AgentEvent::Info(
            "[phase:synthesize] preparing final response".to_string(),
        ));
        let (tx, _rx) = tokio::sync::mpsc::unbounded_channel();
        let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
        let interrupt = Arc::new(AtomicBool::new(false));
        let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
        handle_key(
            &mut state,
            KeyEvent::new(KeyCode::Char('i'), KeyModifiers::CONTROL),
            &tx,
            &runtime_control_tx,
            &steering_tx,
            &interrupt,
        );
        assert!(state.show_inspector);
        let lines = draw_to_lines(160, 28, &mut state);
        let joined = lines.join("\n");
        assert!(joined.contains("inspector"), "{joined}");
        assert!(joined.contains("Debug events"), "{joined}");
        assert!(joined.contains("phase:synthesize"), "{joined}");
    }

    #[test]
    fn clamp_chars_with_indicator_omits_debug_count() {
        let clipped = clamp_chars_with_hint("abcdefghijklmnopqrstuvwxyz", 20);
        assert_eq!(clipped, "abcdefghijklmnopqrs…");
        assert!(!clipped.contains("chars"), "{clipped}");
        assert!(unicode_width::UnicodeWidthStr::width(clipped.as_str()) <= 20);
    }

    #[test]
    fn table_wrap_cell_respects_width() {
        assert_eq!(table_wrap_cell("hello world", 5), vec!["hello", "world"]);
        assert_eq!(table_wrap_cell("hi", 5), vec!["hi"]);
        assert_eq!(table_wrap_cell("", 5), vec![""]);
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

    #[test]
    fn ansi_info_lines_strip_escape_codes_without_reemitting_raw_escapes() {
        let text = line_to_text(&Line_::Info("\x1b[1mBold\x1b[0m plain".to_string()), 80);
        let flat = flatten_lines(&text);
        assert_eq!(flat, vec!["Bold plain"]);
        assert!(!flat.join("\n").contains('\x1b'));
    }

    #[test]
    fn non_csi_escape_in_info_line_is_sanitized_with_normal_info_style() {
        let text = line_to_text(&Line_::Info("hello \x1bworld".to_string()), 80);
        let flat = flatten_lines(&text);
        assert_eq!(flat, vec!["• hello world"]);
        assert!(!flat.join("\n").contains('\x1b'));
    }

    #[test]
    fn wrap_input_visual_counts_emoji_presentation_as_two_cells() {
        // ⚠️ = U+26A0 (width 1) + U+FE0F (VS16, width 0).
        // Per-char unicode-width counts ⚠ as 1, but terminals render ⚠️ as 2.
        // The wrapper must account for this so tool-call borders don't overflow.
        let text = "ab⚠️cd";
        let (lines, _, _) = wrap_input_visual(text, text.len(), 4);
        // "ab" = 2 cells, "⚠️" = 2 cells → total 4, fits on line 0.
        // "cd" = 2 cells → line 1.
        assert_eq!(lines.len(), 2, "⚠️ should count as 2 cells: {lines:?}");
        assert_eq!(lines[0], "ab⚠️");
        assert_eq!(lines[1], "cd");
    }

    #[test]
    fn wrap_input_visual_keeps_vs16_with_base_char() {
        // The VS16 must stay on the same line as its base char, not wrap separately.
        let text = "aaa⚠️bbb";
        let (lines, _, _) = wrap_input_visual(text, text.len(), 5);
        // "aaa" = 3, "⚠️" = 2 → total 5, fits. "bbb" = 3 → next line.
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert!(
            lines[0].ends_with("⚠️"),
            "VS16 must stay with base: {lines:?}"
        );
    }

    #[test]
    fn wrap_input_visual_vs16_at_exact_boundary() {
        // Emoji at the very end of the line width — should not overflow.
        let text = "abcd⚠️ef";
        let (lines, _, _) = wrap_input_visual(text, text.len(), 6);
        // "abcd" = 4, "⚠️" = 2 → total 6, exactly fits. "ef" → next line.
        assert_eq!(lines.len(), 2, "{lines:?}");
        assert_eq!(lines[0], "abcd⚠️");
        assert_eq!(lines[1], "ef");
    }
}
