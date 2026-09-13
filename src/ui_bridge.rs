use serde_json::Value;
use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, RwLock, mpsc};
use std::time::{Duration, Instant};

use crate::events::UiResponse;

const UI_PAYLOAD_CAP: usize = 64 * 1024;
const UI_TOKEN_CAP: usize = 64;
const UI_ERROR_MESSAGE_CAP: usize = 2_000;

fn valid_token(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= UI_TOKEN_CAP
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn value_contains_unsafe_control(value: &Value) -> bool {
    match value {
        Value::String(text) => text
            .chars()
            .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t')),
        Value::Array(items) => items.iter().any(value_contains_unsafe_control),
        Value::Object(fields) => fields.iter().any(|(key, value)| {
            key.chars().any(char::is_control) || value_contains_unsafe_control(value)
        }),
        _ => false,
    }
}

fn validate_value(value: &Value, label: &str) -> Result<(), String> {
    let bytes =
        serde_json::to_vec(value).map_err(|error| format!("serializing {label}: {error}"))?;
    if bytes.len() > UI_PAYLOAD_CAP {
        return Err(format!("{label} exceeds {UI_PAYLOAD_CAP} bytes"));
    }
    if value_contains_unsafe_control(value) {
        return Err(format!(
            "{label} contains unsafe terminal control characters"
        ));
    }
    Ok(())
}

pub(crate) fn validate_request(request: &crate::events::UiRequest) -> Result<(), String> {
    if !valid_token(&request.id) {
        return Err(
            "pack runtime UI request id must be 1-64 ASCII letters, digits, '.', '_', or '-'"
                .to_string(),
        );
    }
    if !valid_token(&request.method) {
        return Err(
            "pack runtime UI request method must be 1-64 ASCII letters, digits, '.', '_', or '-'"
                .to_string(),
        );
    }
    validate_value(&request.params, "pack runtime UI params")
}

pub(crate) fn validate_response(response: &UiResponse) -> Result<(), String> {
    match response {
        UiResponse::Ok { value } => validate_value(value, "pack UI response value"),
        UiResponse::Cancelled => Ok(()),
        UiResponse::Error { code, message } => {
            if !valid_token(code) {
                return Err("pack UI response error code is invalid".to_string());
            }
            if message.trim().is_empty()
                || message.len() > UI_ERROR_MESSAGE_CAP
                || message
                    .chars()
                    .any(|ch| ch.is_control() && !matches!(ch, '\n' | '\t'))
            {
                return Err("pack UI response error message exceeds its limit".to_string());
            }
            Ok(())
        }
    }
}

pub(crate) const REPLY_TIMEOUT: Duration = Duration::from_secs(30 * 60);

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct UiReply {
    pub(crate) id: String,
    pub(crate) response: UiResponse,
}

#[derive(Default)]
pub(crate) struct UiCapabilities {
    methods: HashSet<String>,
}

impl UiCapabilities {
    pub(crate) fn supports(&self, method: &str) -> bool {
        self.methods.contains("*") || self.methods.contains(method)
    }

    pub(crate) fn methods(&self) -> Vec<String> {
        let mut methods = self.methods.iter().cloned().collect::<Vec<_>>();
        methods.sort();
        methods
    }

    pub(crate) fn replace_from_frame(&mut self, frame: &Value) -> Result<(), String> {
        *self = parse_capabilities(frame)?;
        Ok(())
    }
}

pub(crate) fn parse_capabilities(frame: &Value) -> Result<UiCapabilities, String> {
    let methods = frame["methods"]
        .as_array()
        .ok_or_else(|| "ui.capabilities: needs methods array".to_string())?;
    if methods.len() > 64 {
        return Err("ui.capabilities: at most 64 methods".to_string());
    }
    let methods = methods
        .iter()
        .map(Value::as_str)
        .collect::<Option<Vec<_>>>()
        .ok_or_else(|| "ui.capabilities: methods must be strings".to_string())?;
    if methods.iter().any(|method| {
        *method != "*"
            && (method.is_empty()
                || method.len() > 64
                || !method
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.')))
    }) {
        return Err("ui.capabilities: invalid method name".to_string());
    }
    Ok(UiCapabilities {
        methods: methods.into_iter().map(str::to_string).collect(),
    })
}

pub(crate) fn parse_response(frame: &Value) -> Result<UiReply, String> {
    let id = frame["id"]
        .as_str()
        .map(str::trim)
        .filter(|id| valid_token(id))
        .ok_or_else(|| "ui.response: id must be 1-64 safe ASCII characters".to_string())?
        .to_string();
    let response = match frame["status"].as_str() {
        Some("ok") => UiResponse::Ok {
            value: frame.get("value").cloned().unwrap_or(Value::Null),
        },
        Some("cancelled") => UiResponse::Cancelled,
        Some("error") => {
            let field = |key: &str| {
                frame[key]
                    .as_str()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(str::to_string)
            };
            UiResponse::Error {
                code: field("code")
                    .ok_or_else(|| "ui.response error: needs code and message".to_string())?,
                message: field("message")
                    .ok_or_else(|| "ui.response error: needs code and message".to_string())?,
            }
        }
        _ => return Err("ui.response: needs status ok|cancelled|error".to_string()),
    };
    Ok(UiReply { id, response })
}

#[derive(Default)]
pub(crate) struct PendingUiRequest {
    state: Mutex<Option<(String, bool)>>,
}

impl PendingUiRequest {
    pub(crate) fn begin(&self, id: String) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "host UI request state is unavailable".to_string())?;
        if state.is_some() {
            return Err("another host UI request is already pending".to_string());
        }
        *state = Some((id, false));
        Ok(())
    }

    pub(crate) fn claim(&self, id: &str) -> bool {
        let Ok(mut state) = self.state.lock() else {
            return false;
        };
        match state.as_mut() {
            Some((pending, claimed)) if pending == id && !*claimed => {
                *claimed = true;
                true
            }
            _ => false,
        }
    }

    pub(crate) fn unclaim(&self, id: &str) {
        if let Ok(mut state) = self.state.lock()
            && let Some((pending, claimed)) = state.as_mut()
            && pending == id
        {
            *claimed = false;
        }
    }

    fn finish(&self, id: &str) {
        if let Ok(mut state) = self.state.lock()
            && state.as_ref().is_some_and(|(pending, _)| pending == id)
        {
            *state = None;
        }
    }
}

pub(crate) struct UiBridge {
    rx: Mutex<mpsc::Receiver<UiReply>>,
    interrupt: Arc<AtomicBool>,
    pub(crate) next_id: u64,
    pub(crate) ready: Arc<AtomicBool>,
    pub(crate) capabilities: Arc<RwLock<UiCapabilities>>,
    pub(crate) pending: Arc<PendingUiRequest>,
}

impl UiBridge {
    pub(crate) fn new(rx: mpsc::Receiver<UiReply>, interrupt: Arc<AtomicBool>) -> Self {
        Self {
            rx: Mutex::new(rx),
            interrupt,
            next_id: 0,
            ready: Arc::new(AtomicBool::new(true)),
            capabilities: Arc::new(RwLock::new(UiCapabilities::default())),
            pending: Arc::new(PendingUiRequest::default()),
        }
    }

    pub(crate) fn supports(&self, method: &str) -> bool {
        self.ready.load(Ordering::SeqCst)
            && self
                .capabilities
                .read()
                .is_ok_and(|capabilities| capabilities.supports(method))
    }

    pub(crate) fn begin(&self, id: &str) -> Result<(), String> {
        if !self.ready.load(Ordering::SeqCst) {
            return Err("the host input bridge is not ready".to_string());
        }
        self.pending.begin(id.to_string())
    }

    pub(crate) fn wait(&self, id: &str) -> UiResponse {
        let response = self.wait_inner(id);
        self.pending.finish(id);
        response
    }

    fn wait_inner(&self, id: &str) -> UiResponse {
        let deadline = Instant::now() + REPLY_TIMEOUT;
        let Ok(rx) = self.rx.lock() else {
            return UiResponse::error("unavailable", "the host UI response channel is unavailable");
        };
        while !self.interrupt.load(Ordering::SeqCst) && Instant::now() < deadline {
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(reply) if reply.id == id => return reply.response,
                Ok(reply) => self.pending.unclaim(&reply.id),
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    return UiResponse::error("disconnected", "the host input bridge closed");
                }
            }
        }
        if self.interrupt.load(Ordering::SeqCst) {
            UiResponse::Cancelled
        } else {
            UiResponse::error("timeout", "the host did not answer within 30 minutes")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn pending_request_accepts_only_one_matching_response() {
        let interrupt = Arc::new(AtomicBool::new(false));
        let (tx, rx) = mpsc::sync_channel(1);
        let bridge = UiBridge::new(rx, interrupt);
        bridge.begin("ui-1").unwrap();
        assert!(!bridge.pending.claim("stale"));
        assert!(bridge.pending.claim("ui-1"));
        assert!(!bridge.pending.claim("ui-1"));
        tx.send(UiReply {
            id: "ui-1".to_string(),
            response: UiResponse::Ok { value: json!(1) },
        })
        .unwrap();
        assert_eq!(bridge.wait("ui-1"), UiResponse::Ok { value: json!(1) });
        assert!(!bridge.pending.claim("ui-1"));
        bridge.begin("ui-2").unwrap();
    }

    #[test]
    fn capability_replacement_is_atomic_on_invalid_input() {
        let mut capabilities = parse_capabilities(&json!({"methods": ["form"]})).unwrap();
        assert!(
            capabilities
                .replace_from_frame(&json!({"methods": ["bad method"]}))
                .is_err()
        );
        assert!(capabilities.supports("form"));
    }
}
