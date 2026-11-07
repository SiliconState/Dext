//! The agent event stream: every observable thing a turn does, plus the sink
//! trait each front end (console, JSON, TUI channel) implements to consume it.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::sync::Mutex;

use crate::{
    ApprovalProfile, Choice, ContextMode, LocalAuthSecret, ReasoningMode, ThinkingEffort, Usage,
    orchestrator,
};

// Lower-level diagnostics can originate on blocking workers without an EventSink.
type RuntimeWarningSink = Box<dyn Fn(String) + Send + Sync>;

#[derive(Default)]
struct RuntimeWarningRouter {
    sink: Mutex<Option<RuntimeWarningSink>>,
}

impl RuntimeWarningRouter {
    fn install(&self, sink: RuntimeWarningSink) -> RuntimeWarningGuard<'_> {
        let mut current = self.sink.lock().unwrap_or_else(|error| error.into_inner());
        assert!(current.is_none(), "runtime warning route already installed");
        *current = Some(sink);
        RuntimeWarningGuard { router: self }
    }

    fn emit(&self, message: String, stderr: impl FnOnce(String)) {
        let current = self.sink.lock().unwrap_or_else(|error| error.into_inner());
        // Keep fallback output inside the lock so installing the TUI route
        // cannot overtake a warning that already selected stderr.
        if let Some(sink) = current.as_ref() {
            sink(message);
        } else {
            stderr(message);
        }
    }
}

pub(crate) struct RuntimeWarningGuard<'a> {
    router: &'a RuntimeWarningRouter,
}

impl Drop for RuntimeWarningGuard<'_> {
    fn drop(&mut self) {
        self.router
            .sink
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .take();
    }
}

static RUNTIME_WARNINGS: RuntimeWarningRouter = RuntimeWarningRouter {
    sink: Mutex::new(None),
};

pub(crate) fn route_runtime_warnings(
    sink: impl Fn(String) + Send + Sync + 'static,
) -> RuntimeWarningGuard<'static> {
    RUNTIME_WARNINGS.install(Box::new(sink))
}

pub(crate) fn runtime_warning(message: String) {
    RUNTIME_WARNINGS.emit(message, |message| eprintln!("{message}"));
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub(crate) struct UiRequest {
    pub(crate) id: String,
    pub(crate) method: String,
    #[serde(default)]
    pub(crate) params: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "status", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum UiResponse {
    Ok {
        #[serde(default)]
        value: Value,
    },
    Cancelled,
    Error {
        code: String,
        message: String,
    },
}

impl UiResponse {
    pub(crate) fn error(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Error {
            code: code.into(),
            message: message.into(),
        }
    }
}

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
    ThinkingPreviewDiscarded,
    ThinkingPreviewCommitted,
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
    PackStart {
        name: String,
        task_preview: String,
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
    StructuredSlash(String),
    TurnEnd {
        usage: Usage,
        failed: bool,
    },
    BackgroundCompactionSetting {
        enabled: bool,
    },
    BackgroundCompaction {
        version: u32,
        session_id: String,
        session_epoch: u64,
        job_id: String,
        origin_turn_id: String,
        phase: String,
        blocking: bool,
        reason: String,
        elapsed_ms: u64,
        wait_ms: u64,
        before_chars: usize,
        #[serde(skip_serializing_if = "Option::is_none")]
        after_chars: Option<usize>,
        usage_known: bool,
    },
    CompactStart,
    CompactEnd {
        before: usize,
        after: usize,
        summary: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        job_id: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        background: bool,
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
    fn request_ui(&mut self, _pack: &str, _request: &UiRequest) -> UiResponse {
        UiResponse::error(
            "unsupported",
            "this frontend does not provide the pack UI channel",
        )
    }
    fn ui_methods(&self) -> Vec<String> {
        Vec::new()
    }
    fn local_auth_prompt(&mut self, tool: &str, message: &str);
    fn machine_live_output(&self) -> bool {
        false
    }
    fn live_output_sender(&self) -> Option<tokio::sync::mpsc::Sender<AgentEvent>> {
        None
    }
    fn request_local_auth_secret_async<'a>(
        &'a mut self,
        tool: &'a str,
        message: &'a str,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = LocalAuthSecret> + Send + 'a>> {
        Box::pin(async move { self.request_local_auth_secret(tool, message) })
    }
    fn request_local_auth_secret(&mut self, tool: &str, message: &str) -> LocalAuthSecret {
        self.local_auth_prompt(tool, message);
        LocalAuthSecret::Unavailable
    }
}

#[cfg(test)]
mod tests {
    use super::RuntimeWarningRouter;

    #[test]
    fn runtime_warnings_route_across_threads_until_guard_drops() {
        let router = RuntimeWarningRouter::default();
        router.emit("console".into(), |message| assert_eq!(message, "console"));
        let (tx, rx) = std::sync::mpsc::channel();
        let guard = router.install(Box::new(move |message| {
            let _ = tx.send(message);
        }));
        std::thread::scope(|scope| {
            scope.spawn(|| {
                router.emit("checkpoint warning".into(), |_| panic!("unexpected stderr"));
            });
        });
        assert_eq!(rx.try_recv().unwrap(), "checkpoint warning");
        drop(guard);
        router.emit("console again".into(), |message| {
            assert_eq!(message, "console again")
        });
    }

    #[test]
    fn runtime_warnings_serialize_stderr_with_route_installation() {
        let router = RuntimeWarningRouter::default();
        router.emit("startup warning".into(), |message| {
            assert_eq!(message, "startup warning");
            assert!(matches!(
                router.sink.try_lock(),
                Err(std::sync::TryLockError::WouldBlock)
            ));
        });
        assert!(router.sink.try_lock().is_ok());
    }

    #[test]
    fn runtime_warnings_remove_route_during_unwind() {
        let router = RuntimeWarningRouter::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = router.install(Box::new(move |message| {
                let _ = tx.send(message);
            }));
            router.emit("before failure".into(), |_| panic!("unexpected stderr"));
            panic!("injected setup failure");
        }));
        assert!(result.is_err());
        assert_eq!(rx.try_recv().unwrap(), "before failure");
        router.emit("after failure".into(), |message| {
            assert_eq!(message, "after failure")
        });
    }

    #[test]
    fn runtime_warnings_recover_after_sink_panics() {
        let router = RuntimeWarningRouter::default();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _guard = router.install(Box::new(|_| panic!("injected sink failure")));
            router.emit("sink failure".into(), |_| panic!("unexpected stderr"));
        }));
        assert!(result.is_err());
        router.emit("console recovered".into(), |message| {
            assert_eq!(message, "console recovered");
        });
        let (tx, rx) = std::sync::mpsc::channel();
        let _guard = router.install(Box::new(move |message| {
            let _ = tx.send(message);
        }));
        router.emit("route recovered".into(), |_| panic!("unexpected stderr"));
        assert_eq!(rx.try_recv().unwrap(), "route recovered");
    }

    #[test]
    fn runtime_warnings_do_not_fall_back_when_ui_receiver_is_closed() {
        let router = RuntimeWarningRouter::default();
        let (tx, rx) = std::sync::mpsc::channel();
        let _guard = router.install(Box::new(move |message| {
            let _ = tx.send(message);
        }));
        drop(rx);
        router.emit("late warning".into(), |_| panic!("unexpected stderr"));
    }
}
