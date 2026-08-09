//! The agent event stream: every observable thing a turn does, plus the sink
//! trait each front end (console, JSON, TUI channel) implements to consume it.

use serde::Serialize;
use serde_json::Value;

use crate::{
    ApprovalProfile, Choice, ContextMode, LocalAuthSecret, ReasoningMode, ThinkingEffort, Usage,
    orchestrator,
};

#[derive(Serialize, Clone)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub(crate) enum AgentEvent {
    TurnStart,
    HistoryContextUpdated {
        chars: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        tokens: Option<u64>,
    },
    TextDelta(String),
    TextBlockComplete(String),
    ThinkingDelta(String),
    ThinkingBlockComplete(String),
    ToolCallPreview {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolCallStart {
        call_id: String,
        name: String,
        summary: String,
    },
    ToolCallResult {
        call_id: String,
        name: String,
        ok: bool,
        preview: String,
        content: String,
    },
    RuntimeView {
        pack: String,
        title: String,
        markdown: String,
    },
    ToolOutputDelta {
        call_id: String,
        name: String,
        stream: String,
        text: String,
    },
    LocalAuthPrompt {
        tool: String,
        message: String,
    },
    LoginInputMode {
        provider: Option<String>,
    },
    ToolBatchStart {
        batch_id: String,
        call_ids: Vec<String>,
        labels: Vec<String>,
    },
    ToolBatchEnd {
        batch_id: String,
        call_ids: Vec<String>,
        labels: Vec<String>,
        failed: usize,
    },
    UsageUpdate {
        turn: Usage,
        session: Usage,
    },
    HttpRetry {
        attempt: u32,
        wait_secs: u64,
        reason: String,
    },
    ExternalTelemetry {
        telemetry: orchestrator::ExternalTelemetry,
    },
    TurnDiagnostics {
        provider: String,
        api_family: String,
        auth_source: String,
        model: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        last_retry_reason: Option<String>,
        workaround_fired: bool,
        #[serde(skip_serializing_if = "Option::is_none")]
        turn_duration_ms: Option<u64>,
        #[serde(skip_serializing_if = "Option::is_none")]
        context_mode: Option<ContextMode>,
        #[serde(skip_serializing_if = "Option::is_none")]
        tool_profile: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        compacted: Option<bool>,
    },
    ThinkingEffortChanged {
        effort: ThinkingEffort,
    },
    ReasoningModeChanged {
        mode: ReasoningMode,
    },
    ApprovalProfileChanged {
        profile: ApprovalProfile,
    },
    RuntimeControl(String),
    RuntimeControlApplied {
        commands: usize,
        model_changed: bool,
        effort_changed: bool,
        mode_changed: bool,
        stream_aborted: bool,
    },
    Info(String),
    Warn(String),
    Error(String),
    Slash(String),
    TurnEnd {
        usage: Usage,
        failed: bool,
    },
    CompactStart,
    CompactEnd {
        before: usize,
        after: usize,
        summary: String,
    },
    CompactFailed {
        message: String,
    },
    Interrupted,
    SteeringReceived {
        messages: usize,
        preview: String,
    },
}

pub(crate) trait EventSink: Send + Sync {
    fn emit(&mut self, event: AgentEvent);
    fn request_permission(&mut self, name: &str, input: &Value) -> Choice;
    fn local_auth_prompt(&mut self, tool: &str, message: &str);
    fn live_output_sender(&self) -> Option<tokio::sync::mpsc::Sender<AgentEvent>> {
        None
    }
    fn request_local_auth_secret(&mut self, tool: &str, message: &str) -> LocalAuthSecret {
        self.local_auth_prompt(tool, message);
        LocalAuthSecret::Unavailable
    }
}
