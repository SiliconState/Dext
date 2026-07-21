use crate::provider::RequestContract;
use crate::{Block, Usage, normalize_reasoning_summary_text, reasoning_summary_stream_delta};
use anyhow::{Result, anyhow};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

pub(crate) use crate::sse::{SseDecoder, SseFrame};

const TOOL_ARGUMENT_BUFFER_CAP: usize = 256_000;

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StreamUpdate {
    TextDelta(String),
    TextBlockComplete(String),
    ThinkingDelta(String),
    ThinkingBlockComplete(String),
}

#[derive(Debug)]
pub(crate) struct ParsedStream {
    pub(crate) blocks: Vec<Block>,
    pub(crate) stop_reason: Option<String>,
    pub(crate) usage: Usage,
    pub(crate) unknown_events: usize,
}

pub(crate) struct ProviderStreamParser {
    contract: RequestContract,
    state: ProviderState,
}

enum ProviderState {
    Anthropic(AnthropicState),
    OpenAi(OpenAiState),
    ChatGpt(ChatGptState),
}

#[derive(Default)]
struct AnthropicState {
    blocks: BTreeMap<usize, AnthropicBlock>,
    stop_reason: Option<String>,
    usage: Usage,
    message_started: bool,
    message_stopped: bool,
    unknown_events: usize,
}

#[derive(Default)]
struct AnthropicBlock {
    kind: String,
    text: String,
    id: String,
    name: String,
    input_json: Option<String>,
    thinking_signature: Option<String>,
    redacted_data: String,
    stopped: bool,
}

#[derive(Default)]
struct OpenAiState {
    text: String,
    tool_calls: BTreeMap<usize, ToolCallParts>,
    stop_reason: Option<String>,
    usage: Usage,
    unknown_events: usize,
    preserve_timing_cache: bool,
}

#[derive(Default)]
struct ChatGptState {
    text: String,
    reasoning: String,
    reasoning_emitted: String,
    text_in_progress: bool,
    reasoning_in_progress: bool,
    tool_calls: BTreeMap<String, ToolCallParts>,
    tool_call_order: Vec<String>,
    stop_reason: Option<String>,
    usage: Usage,
    unknown_events: usize,
    completed: bool,
    response_reconciled: bool,
}

#[derive(Default)]
struct ToolCallParts {
    id: String,
    name: String,
    arguments: String,
    done: bool,
}

impl ProviderStreamParser {
    pub(crate) fn new(contract: RequestContract, preserve_timing_cache: bool) -> Self {
        let state = match contract {
            RequestContract::AnthropicMessages => {
                ProviderState::Anthropic(AnthropicState::default())
            }
            RequestContract::OpenAiChatCompletions => ProviderState::OpenAi(OpenAiState {
                preserve_timing_cache,
                ..OpenAiState::default()
            }),
            RequestContract::ChatGptResponses => ProviderState::ChatGpt(ChatGptState::default()),
        };
        Self { contract, state }
    }

    pub(crate) fn push_frame(&mut self, frame: SseFrame) -> Result<Vec<StreamUpdate>> {
        let contract = self.contract;
        match &mut self.state {
            ProviderState::Anthropic(state) => parse_anthropic_frame(contract, state, frame),
            ProviderState::OpenAi(state) => parse_openai_frame(contract, state, frame),
            ProviderState::ChatGpt(state) => parse_chatgpt_frame(contract, state, frame),
        }
    }

    pub(crate) fn finish(self) -> Result<ParsedStream> {
        match self.state {
            ProviderState::Anthropic(state) => finish_anthropic(self.contract, state),
            ProviderState::OpenAi(state) => finish_openai(self.contract, state),
            ProviderState::ChatGpt(state) => finish_chatgpt(self.contract, state),
        }
    }
}

fn parse_json_data(contract: RequestContract, event: &str, data: &str) -> Result<Value> {
    serde_json::from_str(data)
        .map_err(|_| protocol_error(contract, event, "recognized event data is not valid JSON"))
}

fn protocol_error(contract: RequestContract, event: &str, detail: &str) -> anyhow::Error {
    anyhow!(
        "stream protocol error [{}/{}]: {}",
        contract.as_str(),
        bounded_label(event),
        detail
    )
}

fn bounded_label(raw: &str) -> String {
    let mut label = raw
        .chars()
        .take(80)
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
                ch
            } else {
                '_'
            }
        })
        .collect::<String>();
    if label.is_empty() {
        label.push_str("unknown");
    }
    label
}

fn bounded_provider_message(raw: &str) -> String {
    const CAP: usize = 512;
    let mut message = raw
        .chars()
        .take(CAP + 1)
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect::<String>();
    if message.chars().count() > CAP {
        message = message.chars().take(CAP).collect();
        message.push('…');
    }
    message
}

fn object<'a>(
    contract: RequestContract,
    event: &str,
    value: &'a Value,
    field: &str,
) -> Result<&'a serde_json::Map<String, Value>> {
    value
        .as_object()
        .ok_or_else(|| protocol_error(contract, event, &format!("{field} must be an object")))
}

fn array<'a>(
    contract: RequestContract,
    event: &str,
    value: &'a Value,
    field: &str,
) -> Result<&'a Vec<Value>> {
    value
        .as_array()
        .ok_or_else(|| protocol_error(contract, event, &format!("{field} must be an array")))
}

fn string<'a>(
    contract: RequestContract,
    event: &str,
    value: &'a Value,
    field: &str,
) -> Result<&'a str> {
    value
        .as_str()
        .ok_or_else(|| protocol_error(contract, event, &format!("{field} must be a string")))
}

fn nonempty_string<'a>(
    contract: RequestContract,
    event: &str,
    value: &'a Value,
    field: &str,
) -> Result<&'a str> {
    let value = string(contract, event, value, field)?;
    if value.trim().is_empty() {
        return Err(protocol_error(
            contract,
            event,
            &format!("{field} must not be empty"),
        ));
    }
    Ok(value)
}

fn index(contract: RequestContract, event: &str, value: &Value, field: &str) -> Result<usize> {
    let raw = value
        .as_u64()
        .ok_or_else(|| protocol_error(contract, event, &format!("{field} must be an integer")))?;
    usize::try_from(raw)
        .map_err(|_| protocol_error(contract, event, &format!("{field} is out of range")))
}

fn optional_string(
    contract: RequestContract,
    event: &str,
    value: Option<&Value>,
    field: &str,
) -> Result<Option<String>> {
    match value {
        None | Some(Value::Null) => Ok(None),
        Some(value) => Ok(Some(string(contract, event, value, field)?.to_string())),
    }
}

fn append_capped(
    contract: RequestContract,
    event: &str,
    field: &str,
    target: &mut String,
    fragment: &str,
) -> Result<()> {
    if target.len().saturating_add(fragment.len()) > TOOL_ARGUMENT_BUFFER_CAP {
        return Err(protocol_error(
            contract,
            event,
            &format!("{field} exceeded {TOOL_ARGUMENT_BUFFER_CAP} bytes"),
        ));
    }
    target.push_str(fragment);
    Ok(())
}

fn parse_tool_arguments(
    contract: RequestContract,
    event: &str,
    call_label: &str,
    raw: &str,
) -> Result<Value> {
    if raw.trim().is_empty() {
        return Err(protocol_error(
            contract,
            event,
            &format!("tool call {call_label} has empty arguments"),
        ));
    }
    let value: Value = serde_json::from_str(raw).map_err(|_| {
        protocol_error(
            contract,
            event,
            &format!("tool call {call_label} has malformed arguments"),
        )
    })?;
    if !value.is_object() {
        return Err(protocol_error(
            contract,
            event,
            &format!("tool call {call_label} arguments must be a JSON object"),
        ));
    }
    Ok(value)
}

fn parse_anthropic_frame(
    contract: RequestContract,
    state: &mut AnthropicState,
    frame: SseFrame,
) -> Result<Vec<StreamUpdate>> {
    let Some(data_str) = frame.data else {
        return Ok(Vec::new());
    };
    let hint = frame.event.as_deref().unwrap_or("event");
    let data = parse_json_data(contract, hint, &data_str)?;
    let data_object = object(contract, hint, &data, "event data")?;
    let payload_type = data_object
        .get("type")
        .map(|value| string(contract, hint, value, "type"))
        .transpose()?;
    let event = frame.event.as_deref().or(payload_type).unwrap_or("unknown");
    if let (Some(frame_event), Some(payload_type)) = (frame.event.as_deref(), payload_type)
        && frame_event != payload_type
    {
        return Err(protocol_error(
            contract,
            frame_event,
            "event field does not match payload type",
        ));
    }

    match event {
        "message_start" => {
            if state.message_started {
                return Err(protocol_error(contract, event, "duplicate message start"));
            }
            let message = data_object
                .get("message")
                .ok_or_else(|| protocol_error(contract, event, "missing message"))?;
            let message = object(contract, event, message, "message")?;
            if let Some(usage) = message.get("usage") {
                object(contract, event, usage, "message.usage")?;
                state.usage = Usage::parse(usage);
            }
            state.message_started = true;
        }
        "content_block_start" => {
            require_anthropic_message_open(contract, state, event)?;
            let idx = index(
                contract,
                event,
                data_object
                    .get("index")
                    .ok_or_else(|| protocol_error(contract, event, "missing index"))?,
                "index",
            )?;
            if state.blocks.contains_key(&idx) {
                return Err(protocol_error(contract, event, "duplicate block index"));
            }
            let content = data_object
                .get("content_block")
                .ok_or_else(|| protocol_error(contract, event, "missing content_block"))?;
            let content = object(contract, event, content, "content_block")?;
            let kind = nonempty_string(
                contract,
                event,
                content
                    .get("type")
                    .ok_or_else(|| protocol_error(contract, event, "missing content_block.type"))?,
                "content_block.type",
            )?
            .to_string();
            let mut block = AnthropicBlock {
                kind: kind.clone(),
                ..AnthropicBlock::default()
            };
            match kind.as_str() {
                "text" => {
                    block.text = string(
                        contract,
                        event,
                        content
                            .get("text")
                            .ok_or_else(|| protocol_error(contract, event, "missing text"))?,
                        "content_block.text",
                    )?
                    .to_string();
                }
                "thinking" => {
                    block.text = string(
                        contract,
                        event,
                        content
                            .get("thinking")
                            .ok_or_else(|| protocol_error(contract, event, "missing thinking"))?,
                        "content_block.thinking",
                    )?
                    .to_string();
                    block.thinking_signature = optional_string(
                        contract,
                        event,
                        content.get("signature"),
                        "content_block.signature",
                    )?;
                }
                "redacted_thinking" => {
                    block.redacted_data = string(
                        contract,
                        event,
                        content.get("data").ok_or_else(|| {
                            protocol_error(contract, event, "missing redacted data")
                        })?,
                        "content_block.data",
                    )?
                    .to_string();
                }
                "tool_use" => {
                    block.id = nonempty_string(
                        contract,
                        event,
                        content
                            .get("id")
                            .ok_or_else(|| protocol_error(contract, event, "missing tool id"))?,
                        "content_block.id",
                    )?
                    .to_string();
                    block.name = nonempty_string(
                        contract,
                        event,
                        content
                            .get("name")
                            .ok_or_else(|| protocol_error(contract, event, "missing tool name"))?,
                        "content_block.name",
                    )?
                    .to_string();
                    let input = content
                        .get("input")
                        .ok_or_else(|| protocol_error(contract, event, "missing tool input"))?;
                    object(contract, event, input, "content_block.input")?;
                    block.input_json = Some(input.to_string());
                }
                _ => {}
            }
            state.blocks.insert(idx, block);
        }
        "content_block_delta" => {
            require_anthropic_message_open(contract, state, event)?;
            let idx = index(
                contract,
                event,
                data_object
                    .get("index")
                    .ok_or_else(|| protocol_error(contract, event, "missing index"))?,
                "index",
            )?;
            let block = state.blocks.get_mut(&idx).ok_or_else(|| {
                protocol_error(contract, event, "delta references unknown block index")
            })?;
            if block.stopped {
                return Err(protocol_error(contract, event, "delta follows block stop"));
            }
            let delta = data_object
                .get("delta")
                .ok_or_else(|| protocol_error(contract, event, "missing delta"))?;
            let delta = object(contract, event, delta, "delta")?;
            let kind = nonempty_string(
                contract,
                event,
                delta
                    .get("type")
                    .ok_or_else(|| protocol_error(contract, event, "missing delta.type"))?,
                "delta.type",
            )?;
            let mut updates = Vec::new();
            match kind {
                "text_delta" => {
                    require_block_kind(contract, event, block, "text")?;
                    let text = string(
                        contract,
                        event,
                        delta
                            .get("text")
                            .ok_or_else(|| protocol_error(contract, event, "missing delta.text"))?,
                        "delta.text",
                    )?;
                    block.text.push_str(text);
                    updates.push(StreamUpdate::TextDelta(text.to_string()));
                }
                "thinking_delta" => {
                    require_block_kind(contract, event, block, "thinking")?;
                    let text = string(
                        contract,
                        event,
                        delta.get("thinking").ok_or_else(|| {
                            protocol_error(contract, event, "missing delta.thinking")
                        })?,
                        "delta.thinking",
                    )?;
                    block.text.push_str(text);
                    updates.push(StreamUpdate::ThinkingDelta(text.to_string()));
                }
                "signature_delta" => {
                    require_block_kind(contract, event, block, "thinking")?;
                    block.thinking_signature = Some(
                        string(
                            contract,
                            event,
                            delta.get("signature").ok_or_else(|| {
                                protocol_error(contract, event, "missing delta.signature")
                            })?,
                            "delta.signature",
                        )?
                        .to_string(),
                    );
                }
                "redacted_thinking_delta" | "data_delta" => {
                    require_block_kind(contract, event, block, "redacted_thinking")?;
                    let value = string(
                        contract,
                        event,
                        delta
                            .get("data")
                            .ok_or_else(|| protocol_error(contract, event, "missing delta.data"))?,
                        "delta.data",
                    )?;
                    block.redacted_data.push_str(value);
                }
                "input_json_delta" => {
                    require_block_kind(contract, event, block, "tool_use")?;
                    let fragment = string(
                        contract,
                        event,
                        delta.get("partial_json").ok_or_else(|| {
                            protocol_error(contract, event, "missing delta.partial_json")
                        })?,
                        "delta.partial_json",
                    )?;
                    let target = block.input_json.get_or_insert_default();
                    if target.trim() == "{}" {
                        target.clear();
                    }
                    append_capped(contract, event, "tool arguments", target, fragment)?;
                }
                _ => state.unknown_events = state.unknown_events.saturating_add(1),
            }
            return Ok(updates);
        }
        "content_block_stop" => {
            require_anthropic_message_open(contract, state, event)?;
            let idx = index(
                contract,
                event,
                data_object
                    .get("index")
                    .ok_or_else(|| protocol_error(contract, event, "missing index"))?,
                "index",
            )?;
            let block = state.blocks.get_mut(&idx).ok_or_else(|| {
                protocol_error(contract, event, "stop references unknown block index")
            })?;
            if block.stopped {
                return Err(protocol_error(contract, event, "duplicate block stop"));
            }
            block.stopped = true;
            return Ok(match block.kind.as_str() {
                "text" => vec![StreamUpdate::TextBlockComplete(block.text.clone())],
                "thinking" => vec![StreamUpdate::ThinkingBlockComplete(block.text.clone())],
                _ => Vec::new(),
            });
        }
        "message_delta" => {
            require_anthropic_message_open(contract, state, event)?;
            let delta = data_object
                .get("delta")
                .ok_or_else(|| protocol_error(contract, event, "missing delta"))?;
            let delta = object(contract, event, delta, "delta")?;
            if let Some(reason) = optional_string(
                contract,
                event,
                delta.get("stop_reason"),
                "delta.stop_reason",
            )? {
                state.stop_reason = Some(reason);
            }
            if let Some(usage) = data_object.get("usage") {
                object(contract, event, usage, "usage")?;
                let parsed = Usage::parse(usage);
                if parsed.output > 0 {
                    state.usage.output = parsed.output;
                }
                if parsed.input > 0 || parsed.cache_create > 0 || parsed.cache_read > 0 {
                    state.usage.input = parsed.input;
                    state.usage.cache_create = parsed.cache_create;
                    state.usage.cache_read = parsed.cache_read;
                }
                if parsed.cost_usd.is_some() {
                    state.usage.cost_usd = parsed.cost_usd;
                }
            }
        }
        "message_stop" => {
            require_anthropic_message_open(contract, state, event)?;
            if state.blocks.values().any(|block| !block.stopped) {
                return Err(protocol_error(
                    contract,
                    event,
                    "message stopped with an open content block",
                ));
            }
            state.message_stopped = true;
        }
        "error" => {
            let error_type = data
                .pointer("/error/type")
                .and_then(Value::as_str)
                .unwrap_or("provider_error");
            return Err(anyhow!(
                "provider stream error [{}]: {}",
                contract.as_str(),
                bounded_label(error_type)
            ));
        }
        _ => state.unknown_events = state.unknown_events.saturating_add(1),
    }
    Ok(Vec::new())
}

fn require_anthropic_message_open(
    contract: RequestContract,
    state: &AnthropicState,
    event: &str,
) -> Result<()> {
    if !state.message_started {
        return Err(protocol_error(
            contract,
            event,
            "event precedes message start",
        ));
    }
    if state.message_stopped {
        return Err(protocol_error(
            contract,
            event,
            "event follows message stop",
        ));
    }
    Ok(())
}

fn require_block_kind(
    contract: RequestContract,
    event: &str,
    block: &AnthropicBlock,
    expected: &str,
) -> Result<()> {
    if block.kind != expected {
        return Err(protocol_error(
            contract,
            event,
            &format!("delta type does not match {expected} block"),
        ));
    }
    Ok(())
}

fn finish_anthropic(contract: RequestContract, state: AnthropicState) -> Result<ParsedStream> {
    if !state.message_started || !state.message_stopped {
        return Err(protocol_error(
            contract,
            "finalize",
            "stream ended before message_stop",
        ));
    }
    let mut blocks = Vec::new();
    for (idx, block) in state.blocks {
        if !block.stopped {
            return Err(protocol_error(
                contract,
                "finalize",
                &format!("block {idx} did not stop"),
            ));
        }
        match block.kind.as_str() {
            "text" => blocks.push(Block::Text { text: block.text }),
            "thinking" => {
                let signature = block.thinking_signature.filter(|value| !value.is_empty());
                if !block.text.is_empty() || signature.is_some() {
                    blocks.push(Block::Thinking {
                        text: block.text,
                        signature,
                    });
                }
            }
            "redacted_thinking" if !block.redacted_data.is_empty() => {
                blocks.push(Block::RedactedThinking {
                    data: block.redacted_data,
                });
            }
            "tool_use" => {
                let input = parse_tool_arguments(
                    contract,
                    "finalize",
                    &format!("block {idx}"),
                    block.input_json.as_deref().unwrap_or(""),
                )?;
                blocks.push(Block::ToolUse {
                    id: block.id,
                    name: block.name,
                    input,
                });
            }
            _ => {}
        }
    }
    Ok(ParsedStream {
        blocks,
        stop_reason: state.stop_reason,
        usage: state.usage,
        unknown_events: state.unknown_events,
    })
}

fn parse_openai_frame(
    contract: RequestContract,
    state: &mut OpenAiState,
    frame: SseFrame,
) -> Result<Vec<StreamUpdate>> {
    let Some(data_str) = frame.data else {
        return Ok(Vec::new());
    };
    if data_str.trim() == "[DONE]" {
        return Ok(Vec::new());
    }
    let data = parse_json_data(contract, "chunk", &data_str)?;
    let data_object = object(contract, "chunk", &data, "chunk")?;
    if let Some(error) = data_object.get("error") {
        let error = object(contract, "error", error, "error")?;
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .or_else(|| error.get("type").and_then(Value::as_str))
            .unwrap_or("provider_error");
        return Err(anyhow!(
            "provider stream error [{}]: {}",
            contract.as_str(),
            bounded_label(code)
        ));
    }
    let mut recognized = false;
    let mut updates = Vec::new();
    if let Some(choices) = data_object.get("choices") {
        recognized = true;
        for choice in array(contract, "chunk", choices, "choices")? {
            let choice = object(contract, "chunk", choice, "choice")?;
            if let Some(reason) = optional_string(
                contract,
                "chunk",
                choice.get("finish_reason"),
                "finish_reason",
            )? {
                state.stop_reason = Some(reason);
            }
            let delta = choice
                .get("delta")
                .ok_or_else(|| protocol_error(contract, "chunk", "missing choice.delta"))?;
            let delta = object(contract, "chunk", delta, "choice.delta")?;
            if let Some(content) = delta.get("content")
                && !content.is_null()
            {
                let content = string(contract, "chunk", content, "delta.content")?;
                state.text.push_str(content);
                updates.push(StreamUpdate::TextDelta(content.to_string()));
            }
            if let Some(tool_calls) = delta.get("tool_calls") {
                for tool_call in array(contract, "chunk", tool_calls, "delta.tool_calls")? {
                    let tool_call = object(contract, "chunk", tool_call, "tool call")?;
                    let idx = index(
                        contract,
                        "chunk",
                        tool_call.get("index").ok_or_else(|| {
                            protocol_error(contract, "chunk", "missing tool call index")
                        })?,
                        "tool call index",
                    )?;
                    let entry = state.tool_calls.entry(idx).or_default();
                    if let Some(id) = tool_call.get("id") {
                        entry.id = string(contract, "chunk", id, "tool call id")?.to_string();
                    }
                    if let Some(function) = tool_call.get("function") {
                        let function = object(contract, "chunk", function, "tool call function")?;
                        if let Some(name) = function.get("name") {
                            entry.name = string(contract, "chunk", name, "tool name")?.to_string();
                        }
                        if let Some(arguments) = function.get("arguments") {
                            let arguments = string(contract, "chunk", arguments, "tool arguments")?;
                            append_capped(
                                contract,
                                "chunk",
                                "tool arguments",
                                &mut entry.arguments,
                                arguments,
                            )?;
                        }
                    }
                }
            }
        }
    }
    if let Some(timings) = data_object.get("timings") {
        recognized = true;
        object(contract, "chunk", timings, "timings")?;
        if let Some(usage) = Usage::parse_openai_timings(timings) {
            state.usage = usage;
        }
    }
    if let Some(usage) = data_object.get("usage") {
        recognized = true;
        object(contract, "chunk", usage, "usage")?;
        let parsed = Usage::parse_openai(usage);
        if !(state.preserve_timing_cache && state.usage.cache_read > 0 && parsed.cache_read == 0) {
            state.usage = parsed;
        }
    }
    if !recognized {
        state.unknown_events = state.unknown_events.saturating_add(1);
    }
    Ok(updates)
}

fn finish_openai(contract: RequestContract, state: OpenAiState) -> Result<ParsedStream> {
    let mut blocks = Vec::new();
    if !state.text.is_empty() {
        blocks.push(Block::Text { text: state.text });
    }
    for (idx, call) in state.tool_calls {
        if call.id.trim().is_empty() {
            return Err(protocol_error(
                contract,
                "finalize",
                &format!("tool call {idx} has no id"),
            ));
        }
        if call.name.trim().is_empty() {
            return Err(protocol_error(
                contract,
                "finalize",
                &format!("tool call {idx} has no name"),
            ));
        }
        let input = parse_tool_arguments(contract, "finalize", &idx.to_string(), &call.arguments)?;
        blocks.push(Block::ToolUse {
            id: call.id,
            name: call.name,
            input,
        });
    }
    Ok(ParsedStream {
        blocks,
        stop_reason: state.stop_reason,
        usage: state.usage,
        unknown_events: state.unknown_events,
    })
}

fn parse_chatgpt_frame(
    contract: RequestContract,
    state: &mut ChatGptState,
    frame: SseFrame,
) -> Result<Vec<StreamUpdate>> {
    let Some(data_str) = frame.data else {
        return Ok(Vec::new());
    };
    if data_str.trim() == "[DONE]" {
        state
            .stop_reason
            .get_or_insert_with(|| "completed".to_string());
        state.completed = true;
        return Ok(Vec::new());
    }
    if state.completed {
        return Err(protocol_error(
            contract,
            "event",
            "payload event arrived after stream completion",
        ));
    }
    let data = parse_json_data(contract, "event", &data_str)?;
    let data_object = object(contract, "event", &data, "event data")?;
    let event = nonempty_string(
        contract,
        "event",
        data_object
            .get("type")
            .ok_or_else(|| protocol_error(contract, "event", "missing type"))?,
        "type",
    )?;
    if let Some(frame_event) = frame.event.as_deref()
        && frame_event != event
    {
        return Err(protocol_error(
            contract,
            frame_event,
            "event field does not match payload type",
        ));
    }
    let mut updates = Vec::new();
    match event {
        "error" | "response.failed" => {
            let code = data
                .pointer("/error/code")
                .and_then(Value::as_str)
                .or_else(|| data.pointer("/response/error/code").and_then(Value::as_str))
                .unwrap_or("provider_error");
            let message = data
                .pointer("/error/message")
                .and_then(Value::as_str)
                .or_else(|| {
                    data.pointer("/response/error/message")
                        .and_then(Value::as_str)
                })
                .or_else(|| data.get("message").and_then(Value::as_str))
                .filter(|message| !message.trim().is_empty());
            let code = bounded_label(code);
            let detail = message
                .map(|message| format!("{code}: {}", bounded_provider_message(message)))
                .unwrap_or(code);
            return Err(anyhow!(
                "provider stream error [{}]: {}",
                contract.as_str(),
                detail
            ));
        }
        "response.output_text.delta" => {
            let delta = string(
                contract,
                event,
                data_object
                    .get("delta")
                    .ok_or_else(|| protocol_error(contract, event, "missing delta"))?,
                "delta",
            )?;
            state.text.push_str(delta);
            state.text_in_progress = true;
            updates.push(StreamUpdate::TextDelta(delta.to_string()));
        }
        "response.output_text.done" => {
            if state.text.is_empty() {
                let text = string(
                    contract,
                    event,
                    data_object
                        .get("text")
                        .ok_or_else(|| protocol_error(contract, event, "missing text"))?,
                    "text",
                )?;
                state.text.push_str(text);
                updates.push(StreamUpdate::TextDelta(text.to_string()));
            } else if let Some(text) = data_object.get("text") {
                string(contract, event, text, "text")?;
            }
            state.text_in_progress = false;
        }
        "response.reasoning_summary_text.delta" | "response.reasoning_text.delta" => {
            let delta = string(
                contract,
                event,
                data_object
                    .get("delta")
                    .ok_or_else(|| protocol_error(contract, event, "missing delta"))?,
                "delta",
            )?;
            state.reasoning.push_str(delta);
            state.reasoning_in_progress = true;
            if let Some(visible) =
                reasoning_summary_stream_delta(&state.reasoning, &mut state.reasoning_emitted)
            {
                updates.push(StreamUpdate::ThinkingDelta(visible));
            }
        }
        "response.reasoning_summary_text.done" | "response.reasoning_text.done" => {
            if state.reasoning.is_empty() {
                let text = string(
                    contract,
                    event,
                    data_object
                        .get("text")
                        .ok_or_else(|| protocol_error(contract, event, "missing text"))?,
                    "text",
                )?;
                state.reasoning.push_str(text);
                if let Some(visible) =
                    reasoning_summary_stream_delta(&state.reasoning, &mut state.reasoning_emitted)
                {
                    updates.push(StreamUpdate::ThinkingDelta(visible));
                }
            } else if let Some(text) = data_object.get("text") {
                string(contract, event, text, "text")?;
            }
            state.reasoning_in_progress = false;
        }
        "response.output_item.added" => {
            let item = data_object
                .get("item")
                .ok_or_else(|| protocol_error(contract, event, "missing item"))?;
            let item = object(contract, event, item, "item")?;
            let item_type = nonempty_string(
                contract,
                event,
                item.get("type")
                    .ok_or_else(|| protocol_error(contract, event, "missing item.type"))?,
                "item.type",
            )?;
            if item_type == "function_call" {
                let item_id = nonempty_string(
                    contract,
                    event,
                    item.get("id")
                        .ok_or_else(|| protocol_error(contract, event, "missing item.id"))?,
                    "item.id",
                )?
                .to_string();
                if state.tool_calls.contains_key(&item_id) {
                    return Err(protocol_error(contract, event, "duplicate function item"));
                }
                let call = ToolCallParts {
                    id: optional_string(contract, event, item.get("call_id"), "item.call_id")?
                        .unwrap_or_default(),
                    name: optional_string(contract, event, item.get("name"), "item.name")?
                        .unwrap_or_default(),
                    arguments: optional_string(
                        contract,
                        event,
                        item.get("arguments"),
                        "item.arguments",
                    )?
                    .unwrap_or_default(),
                    done: false,
                };
                state.tool_call_order.push(item_id.clone());
                state.tool_calls.insert(item_id, call);
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = chatgpt_item_id(contract, event, data_object)?;
            let call = state.tool_calls.get_mut(item_id).ok_or_else(|| {
                protocol_error(contract, event, "delta references unknown function item")
            })?;
            if call.done {
                return Err(protocol_error(
                    contract,
                    event,
                    "delta follows arguments done",
                ));
            }
            let delta = string(
                contract,
                event,
                data_object
                    .get("delta")
                    .ok_or_else(|| protocol_error(contract, event, "missing delta"))?,
                "delta",
            )?;
            append_capped(
                contract,
                event,
                "tool arguments",
                &mut call.arguments,
                delta,
            )?;
        }
        "response.output_item.done" => {
            let item = data_object
                .get("item")
                .ok_or_else(|| protocol_error(contract, event, "missing item"))?;
            let item = object(contract, event, item, "item")?;
            let item_type = nonempty_string(
                contract,
                event,
                item.get("type")
                    .ok_or_else(|| protocol_error(contract, event, "missing item.type"))?,
                "item.type",
            )?;
            if item_type == "function_call" {
                let item_id = nonempty_string(
                    contract,
                    event,
                    item.get("id")
                        .ok_or_else(|| protocol_error(contract, event, "missing item.id"))?,
                    "item.id",
                )?;
                let id = optional_string(contract, event, item.get("call_id"), "item.call_id")?
                    .unwrap_or_default();
                let name = optional_string(contract, event, item.get("name"), "item.name")?
                    .unwrap_or_default();
                let arguments =
                    optional_string(contract, event, item.get("arguments"), "item.arguments")?;
                let call = state.tool_calls.get_mut(item_id).ok_or_else(|| {
                    protocol_error(
                        contract,
                        event,
                        "final function item was not announced by output_item.added",
                    )
                })?;
                let identity_conflicts = (!id.is_empty() && !call.id.is_empty() && call.id != id)
                    || (!name.is_empty() && !call.name.is_empty() && call.name != name);
                if identity_conflicts {
                    return Err(protocol_error(
                        contract,
                        event,
                        "final function item conflicts with streamed state",
                    ));
                }
                if !id.is_empty() {
                    call.id = id;
                }
                if !name.is_empty() {
                    call.name = name;
                }
                if let Some(arguments) = arguments {
                    let arguments_conflict = if call.done {
                        call.arguments != arguments
                    } else {
                        !arguments.starts_with(&call.arguments)
                    };
                    if arguments_conflict {
                        return Err(protocol_error(
                            contract,
                            event,
                            "final function item conflicts with streamed state",
                        ));
                    }
                    if arguments.len() > TOOL_ARGUMENT_BUFFER_CAP {
                        return Err(protocol_error(
                            contract,
                            event,
                            "tool arguments exceeded buffer cap",
                        ));
                    }
                    call.arguments = arguments.to_string();
                    call.done = true;
                }
            }
        }
        "response.function_call_arguments.done" => {
            let item_id = chatgpt_item_id(contract, event, data_object)?;
            let call = state.tool_calls.get_mut(item_id).ok_or_else(|| {
                protocol_error(contract, event, "done references unknown function item")
            })?;
            if call.done {
                return Err(protocol_error(contract, event, "duplicate arguments done"));
            }
            let arguments = string(
                contract,
                event,
                data_object
                    .get("arguments")
                    .ok_or_else(|| protocol_error(contract, event, "missing arguments"))?,
                "arguments",
            )?;
            if !call.arguments.is_empty() && !arguments.starts_with(&call.arguments) {
                return Err(protocol_error(
                    contract,
                    event,
                    "final arguments conflict with streamed prefix",
                ));
            }
            call.arguments = arguments.to_string();
            if call.arguments.len() > TOOL_ARGUMENT_BUFFER_CAP {
                return Err(protocol_error(
                    contract,
                    event,
                    "tool arguments exceeded buffer cap",
                ));
            }
            call.done = true;
        }
        "response.completed" | "response.done" | "response.incomplete" => {
            let response = data_object
                .get("response")
                .ok_or_else(|| protocol_error(contract, event, "missing response"))?;
            let response = object(contract, event, response, "response")?;
            let status =
                optional_string(contract, event, response.get("status"), "response.status")?;
            let response_incomplete =
                event == "response.incomplete" || status.as_deref() == Some("incomplete");
            if event == "response.incomplete"
                && status
                    .as_deref()
                    .is_some_and(|status| status != "incomplete")
            {
                return Err(protocol_error(
                    contract,
                    event,
                    "response.incomplete conflicted with response status",
                ));
            }
            if status
                .as_deref()
                .is_some_and(|status| !matches!(status, "completed" | "done" | "incomplete"))
            {
                return Err(protocol_error(
                    contract,
                    event,
                    "response terminal status was not usable; function calls were not accepted",
                ));
            }
            if response_incomplete {
                let incomplete_reason = match response.get("incomplete_details") {
                    None | Some(Value::Null) => None,
                    Some(details) => {
                        let details =
                            object(contract, event, details, "response.incomplete_details")?;
                        optional_string(
                            contract,
                            event,
                            details.get("reason"),
                            "response.incomplete_details.reason",
                        )?
                    }
                };
                state.stop_reason = Some(
                    incomplete_reason
                        .filter(|reason| !reason.trim().is_empty())
                        .map(|reason| {
                            format!("incomplete:{}", bounded_label(&reason.to_ascii_lowercase()))
                        })
                        .unwrap_or_else(|| "incomplete".to_string()),
                );
            } else if let Some(status) = status {
                state.stop_reason = Some(status);
            }
            if let Some(usage) = response.get("usage") {
                object(contract, event, usage, "response.usage")?;
                state.usage = Usage::parse_openai(usage);
            }
            let mut terminal_function_items = BTreeSet::new();
            let terminal_outputs = response
                .get("output")
                .map(|outputs| array(contract, event, outputs, "response.output"))
                .transpose()?;
            if !response_incomplete && let Some(outputs) = terminal_outputs {
                for output in outputs {
                    let output = object(contract, event, output, "response output item")?;
                    let output_type = nonempty_string(
                        contract,
                        event,
                        output.get("type").ok_or_else(|| {
                            protocol_error(contract, event, "missing output item type")
                        })?,
                        "output item type",
                    )?;
                    if output_type != "function_call" {
                        continue;
                    }
                    let item_id = nonempty_string(
                        contract,
                        event,
                        output.get("id").ok_or_else(|| {
                            protocol_error(contract, event, "missing output item id")
                        })?,
                        "output item id",
                    )?
                    .to_string();
                    if !terminal_function_items.insert(item_id.clone()) {
                        return Err(protocol_error(
                            contract,
                            event,
                            "duplicate final function item",
                        ));
                    }
                    let call = ToolCallParts {
                        id: nonempty_string(
                            contract,
                            event,
                            output.get("call_id").ok_or_else(|| {
                                protocol_error(contract, event, "missing output call_id")
                            })?,
                            "output call_id",
                        )?
                        .to_string(),
                        name: nonempty_string(
                            contract,
                            event,
                            output.get("name").ok_or_else(|| {
                                protocol_error(contract, event, "missing output name")
                            })?,
                            "output name",
                        )?
                        .to_string(),
                        arguments: string(
                            contract,
                            event,
                            output.get("arguments").ok_or_else(|| {
                                protocol_error(contract, event, "missing output arguments")
                            })?,
                            "output arguments",
                        )?
                        .to_string(),
                        done: true,
                    };
                    if call.arguments.len() > TOOL_ARGUMENT_BUFFER_CAP {
                        return Err(protocol_error(
                            contract,
                            event,
                            "tool arguments exceeded buffer cap",
                        ));
                    }
                    let Some(streamed) = state.tool_calls.get(&item_id) else {
                        return Err(protocol_error(
                            contract,
                            event,
                            "final function item was not announced by output_item.added",
                        ));
                    };
                    let identity_conflicts = (!streamed.id.is_empty() && streamed.id != call.id)
                        || (!streamed.name.is_empty() && streamed.name != call.name);
                    let arguments_conflict = if streamed.done {
                        streamed.arguments != call.arguments
                    } else {
                        !call.arguments.starts_with(&streamed.arguments)
                    };
                    if identity_conflicts || arguments_conflict {
                        return Err(protocol_error(
                            contract,
                            event,
                            "final function item conflicts with streamed state",
                        ));
                    }
                    state.tool_calls.insert(item_id, call);
                }
            }
            if response_incomplete {
                let discard_all_response_content =
                    state.stop_reason.as_deref() == Some("incomplete:content_filter");
                let accepted_function_items = state
                    .tool_call_order
                    .iter()
                    .filter(|item_id| {
                        !discard_all_response_content
                            && state.tool_calls.get(*item_id).is_some_and(|call| {
                                call.done
                                    && !call.id.trim().is_empty()
                                    && !call.name.trim().is_empty()
                            })
                    })
                    .cloned()
                    .collect::<BTreeSet<_>>();
                state
                    .tool_call_order
                    .retain(|item_id| accepted_function_items.contains(item_id));
                state
                    .tool_calls
                    .retain(|item_id, _| accepted_function_items.contains(item_id));
                if state.text_in_progress
                    || (discard_all_response_content && !state.text.is_empty())
                {
                    state.text.clear();
                    state.text_in_progress = false;
                    updates.push(StreamUpdate::TextBlockComplete(String::new()));
                }
                if state.reasoning_in_progress
                    || (discard_all_response_content && !state.reasoning.is_empty())
                {
                    state.reasoning.clear();
                    state.reasoning_emitted.clear();
                    state.reasoning_in_progress = false;
                    updates.push(StreamUpdate::ThinkingBlockComplete(String::new()));
                }
            }
            // Completed responses must reconcile every unfinished streamed call
            // against the terminal output array. Incomplete responses instead
            // keep only calls finalized by streamed *.done events above.
            if !response_incomplete
                && state.tool_call_order.iter().any(|item_id| {
                    !terminal_function_items.contains(item_id)
                        && !state.tool_calls.get(item_id).is_some_and(|call| call.done)
                })
            {
                return Err(protocol_error(
                    contract,
                    event,
                    "completed response omitted an unfinished streamed function item",
                ));
            }
            state.completed = true;
            state.response_reconciled = true;
        }
        _ => state.unknown_events = state.unknown_events.saturating_add(1),
    }
    Ok(updates)
}

fn chatgpt_item_id<'a>(
    contract: RequestContract,
    event: &str,
    data: &'a serde_json::Map<String, Value>,
) -> Result<&'a str> {
    let value = data
        .get("item_id")
        .or_else(|| data.get("id"))
        .ok_or_else(|| protocol_error(contract, event, "missing item_id"))?;
    nonempty_string(contract, event, value, "item_id")
}

fn finish_chatgpt(contract: RequestContract, mut state: ChatGptState) -> Result<ParsedStream> {
    if !state.completed {
        return Err(protocol_error(
            contract,
            "finalize",
            "unexpected EOF before response completed; function calls were not accepted",
        ));
    }
    if !state.tool_call_order.is_empty() && !state.response_reconciled {
        return Err(protocol_error(
            contract,
            "finalize",
            "function calls require a completed response terminal event",
        ));
    }
    let mut blocks = Vec::new();
    if !state.reasoning.is_empty() {
        let reasoning = normalize_reasoning_summary_text(&state.reasoning);
        if !reasoning.is_empty() {
            blocks.push(Block::Thinking {
                text: reasoning,
                signature: None,
            });
        }
    }
    if !state.text.is_empty() {
        blocks.push(Block::Text { text: state.text });
    }
    for (idx, item_id) in state.tool_call_order.iter().enumerate() {
        let call = state
            .tool_calls
            .remove(item_id)
            .ok_or_else(|| protocol_error(contract, "finalize", "function item disappeared"))?;
        if !call.done {
            return Err(protocol_error(
                contract,
                "finalize",
                &format!("function item {idx} did not complete"),
            ));
        }
        if call.id.trim().is_empty() || call.name.trim().is_empty() {
            return Err(protocol_error(
                contract,
                "finalize",
                &format!("function item {idx} has incomplete identity"),
            ));
        }
        let input = parse_tool_arguments(
            contract,
            "finalize",
            &format!("function item {idx}"),
            &call.arguments,
        )?;
        blocks.push(Block::ToolUse {
            id: call.id,
            name: call.name,
            input,
        });
    }
    Ok(ParsedStream {
        blocks,
        stop_reason: state.stop_reason,
        usage: state.usage,
        unknown_events: state.unknown_events,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn anthropic_validates_order_identity_and_object_arguments() {
        let contract = RequestContract::AnthropicMessages;
        let mut parser = ProviderStreamParser::new(contract, false);
        for (event, data) in [
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{"input_tokens":2}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"{\"path\":\"README.md\"}"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ] {
            parser
                .push_frame(SseFrame {
                    event: Some(event.to_string()),
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert!(matches!(
            parsed.blocks.as_slice(),
            [Block::ToolUse { id, name, input }]
                if id == "call_1" && name == "read_file" && input["path"] == "README.md"
        ));

        let mut parser = ProviderStreamParser::new(contract, false);
        parser
            .push_frame(SseFrame {
                event: Some("message_start".to_string()),
                data: Some(r#"{"type":"message_start","message":{"usage":{}}}"#.to_string()),
            })
            .unwrap();
        let error = parser
            .push_frame(SseFrame {
                event: Some("content_block_delta".to_string()),
                data: Some(
                    r#"{"type":"content_block_delta","index":4,"delta":{"type":"text_delta","text":"no"}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown block index"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        for (event, data) in [
            (
                "message_start",
                r#"{"type":"message_start","message":{"usage":{}}}"#,
            ),
            (
                "content_block_start",
                r#"{"type":"content_block_start","index":0,"content_block":{"type":"tool_use","id":"call_1","name":"read_file","input":{}}}"#,
            ),
            (
                "content_block_delta",
                r#"{"type":"content_block_delta","index":0,"delta":{"type":"input_json_delta","partial_json":"[1,2]"}}"#,
            ),
            (
                "content_block_stop",
                r#"{"type":"content_block_stop","index":0}"#,
            ),
            ("message_stop", r#"{"type":"message_stop"}"#),
        ] {
            parser
                .push_frame(SseFrame {
                    event: Some(event.to_string()),
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser.finish().unwrap_err().to_string();
        assert!(error.contains("arguments must be a JSON object"), "{error}");
    }

    #[test]
    fn openai_rejects_malformed_json_incomplete_identity_and_bad_arguments() {
        let contract = RequestContract::OpenAiChatCompletions;
        let mut parser = ProviderStreamParser::new(contract, false);
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some("{not-json".to_string()),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid JSON"), "{error}");
        assert!(!error.contains("not-json"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"name":"read_file","arguments":"{\"path\":"}}]},"finish_reason":null}]}"#,
            r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"README.md\"}"}}]},"finish_reason":"tool_calls"}]}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser.finish().unwrap_err().to_string();
        assert!(error.contains("has no id"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"read_file","arguments":"[]"}}]},"finish_reason":"tool_calls"}]}"#
                        .to_string(),
                ),
            })
            .unwrap();
        let error = parser.finish().unwrap_err().to_string();
        assert!(error.contains("arguments must be a JSON object"), "{error}");
    }

    #[test]
    fn chatgpt_failed_response_preserves_bounded_retry_message() {
        let contract = RequestContract::ChatGptResponses;
        let mut parser = ProviderStreamParser::new(contract, false);
        let error = parser
            .push_frame(SseFrame {
                event: Some("response.failed".to_string()),
                data: Some(
                    r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"An error occurred while processing your request. You can retry your request, or contact us through our help center."}}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("server_error"), "{error}");
        assert!(error.contains("retry your request"), "{error}");
        assert!(error.len() < 700, "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        let error = parser
            .push_frame(SseFrame {
                event: Some("response.failed".to_string()),
                data: Some(
                    r#"{"type":"response.failed","response":{"error":{"code":"server_error","message":"The request could not be completed."}}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("server_error"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        let long_message = "x".repeat(2_000);
        let error = parser
            .push_frame(SseFrame {
                event: Some("response.failed".to_string()),
                data: Some(
                    serde_json::json!({
                        "type": "response.failed",
                        "response": {"error": {"message": long_message}}
                    })
                    .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.len() < 700, "{}", error.len());
        assert!(error.ends_with('…'), "{error}");
    }

    #[test]
    fn chatgpt_requires_added_item_before_deltas_and_complete_final_call() {
        let contract = RequestContract::ChatGptResponses;
        let mut parser = ProviderStreamParser::new(contract, false);
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{}"}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("unknown function item"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.created","response":{"id":"r_1"}}"#,
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":"}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","usage":{"input_tokens":2,"output_tokens":1},"output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}]}}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert_eq!(parsed.unknown_events, 1);
        assert!(matches!(
            parsed.blocks.as_slice(),
            [Block::ToolUse { id, name, input }]
                if id == "call_1" && name == "read_file" && input["path"] == "README.md"
        ));

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{}"}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"fc_1","name":"read_file","arguments":"{}"}]}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("missing output call_id"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"README"}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"Cargo.toml\"}"}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflict with streamed prefix"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.completed","response":{"status":"completed","output":[{"type":"function_call","id":"fc_1","call_id":"call_2","name":"write_file","arguments":"{\"path\":\"README.md\"}"}]}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicts with streamed state"), "{error}");

        // A terminal event that omits an already finalized function item is
        // accepted — observed with gpt-5.6 models on the ChatGPT backend.
        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{}"}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert!(matches!(
            parsed.blocks.as_slice(),
            [Block::ToolUse { id, name, .. }] if id == "call_1" && name == "read_file"
        ));

        // output_item.done alone finalizes a call when arguments.done never
        // arrives and the terminal output omits the item.
        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":"}"#,
            r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"README.md\"}"}}"#,
            r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert!(matches!(
            parsed.blocks.as_slice(),
            [Block::ToolUse { id, name, input }]
                if id == "call_1" && name == "read_file" && input["path"] == "README.md"
        ));

        // output_item.done that contradicts streamed argument prefixes stays fatal.
        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"README"}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.output_item.done","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file","arguments":"{\"path\":\"Cargo.toml\"}"}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicts with streamed state"), "{error}");

        // An item that never finished anywhere is still a protocol violation.
        let mut parser = ProviderStreamParser::new(contract, false);
        parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"read_file"}}"#
                        .to_string(),
                ),
            })
            .unwrap();
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.completed","response":{"status":"completed","output":[]}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("omitted an unfinished streamed function item"),
            "{error}"
        );

        for terminal in [
            r#"{"type":"response.incomplete","response":{"status":"incomplete","output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"README.md\"}"}]}}"#,
            r#"{"type":"response.incomplete","response":{"output":[]}}"#,
            r#"{"type":"response.completed","response":{"status":"incomplete","output":[]}}"#,
        ] {
            let mut parser = ProviderStreamParser::new(contract, false);
            for data in [
                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
                r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
                terminal,
            ] {
                parser
                    .push_frame(SseFrame {
                        event: None,
                        data: Some(data.to_string()),
                    })
                    .unwrap();
            }
            let parsed = parser.finish().unwrap();
            assert_eq!(parsed.stop_reason.as_deref(), Some("incomplete"));
            assert!(matches!(
                parsed.blocks.as_slice(),
                [Block::ToolUse { id, name, input }]
                    if id == "call_1"
                        && name == "write_file"
                        && input["path"] == "README.md"
            ));
        }

        // A function call truncated by an incomplete response is not executable,
        // even if the terminal snapshot contains a syntactically complete-looking
        // version. Discard it and let the agent issue a fresh request.
        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
            r#"{"type":"response.function_call_arguments.delta","item_id":"fc_1","delta":"{\"path\":\"README"}"#,
            r#"{"type":"response.incomplete","response":{"status":"incomplete","output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"README.md\"}"}]}}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert_eq!(parsed.stop_reason.as_deref(), Some("incomplete"));
        assert!(parsed.blocks.is_empty(), "{:?}", parsed.blocks);

        // The provider can also stop before producing any output item. This is
        // a recoverable terminal response, not a malformed SSE stream.
        let mut parser = ProviderStreamParser::new(contract, false);
        parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"},"output":[]}}"#
                        .to_string(),
                ),
            })
            .unwrap();
        let parsed = parser.finish().unwrap();
        assert_eq!(
            parsed.stop_reason.as_deref(),
            Some("incomplete:max_output_tokens")
        );
        assert!(parsed.blocks.is_empty());

        let mut parser = ProviderStreamParser::new(contract, false);
        parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[]}}"#
                        .to_string(),
                ),
            })
            .unwrap();
        let parsed = parser.finish().unwrap();
        assert_eq!(
            parsed.stop_reason.as_deref(),
            Some("incomplete:content_filter")
        );
        assert!(parsed.blocks.is_empty());

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_text.delta","delta":"filtered draft"}"#,
            r#"{"type":"response.output_text.done","text":"filtered draft"}"#,
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[]}}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert_eq!(
            parsed.stop_reason.as_deref(),
            Some("incomplete:content_filter")
        );
        assert!(parsed.blocks.is_empty());

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
            r#"{"type":"response.incomplete","response":{"status":"incomplete","incomplete_details":{"reason":"content_filter"},"output":[{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file","arguments":"{\"path\":\"README.md\"}"}]}}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert_eq!(
            parsed.stop_reason.as_deref(),
            Some("incomplete:content_filter")
        );
        assert!(parsed.blocks.is_empty());

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.completed","response":{"status":"failed","output":[]}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("terminal status was not usable"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.incomplete","response":{"status":"completed","output":[]}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("conflicted with response status"), "{error}");

        for unfinished_event in [
            r#"{"type":"response.output_text.delta","delta":"partial"}"#,
            r#"{"type":"response.reasoning_summary_text.delta","delta":"partial"}"#,
        ] {
            let mut parser = ProviderStreamParser::new(contract, false);
            for data in [
                unfinished_event,
                r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
                r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
                r#"{"type":"response.incomplete","response":{"status":"incomplete","output":[]}}"#,
            ] {
                parser
                    .push_frame(SseFrame {
                        event: None,
                        data: Some(data.to_string()),
                    })
                    .unwrap();
            }
            let parsed = parser.finish().unwrap();
            assert_eq!(parsed.stop_reason.as_deref(), Some("incomplete"));
            assert!(matches!(
                parsed.blocks.as_slice(),
                [Block::ToolUse { id, name, input }]
                    if id == "call_1"
                        && name == "write_file"
                        && input["path"] == "README.md"
            ));
        }

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser.finish().unwrap_err().to_string();
        assert!(error.contains("unexpected EOF"), "{error}");

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#,
            r#"{"type":"response.function_call_arguments.done","item_id":"fc_1","arguments":"{\"path\":\"README.md\"}"}"#,
            "[DONE]",
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let error = parser.finish().unwrap_err().to_string();
        assert!(
            error.contains("completed response terminal event"),
            "{error}"
        );

        let mut parser = ProviderStreamParser::new(contract, false);
        for data in [
            r#"{"type":"response.output_text.delta","delta":"complete text"}"#,
            "[DONE]",
        ] {
            parser
                .push_frame(SseFrame {
                    event: None,
                    data: Some(data.to_string()),
                })
                .unwrap();
        }
        let parsed = parser.finish().unwrap();
        assert!(matches!(
            parsed.blocks.as_slice(),
            [Block::Text { text }] if text == "complete text"
        ));

        let mut parser = ProviderStreamParser::new(contract, false);
        parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.completed","response":{"status":"completed"}}"#
                        .to_string(),
                ),
            })
            .unwrap();
        let error = parser
            .push_frame(SseFrame {
                event: None,
                data: Some(
                    r#"{"type":"response.output_item.added","item":{"type":"function_call","id":"fc_1","call_id":"call_1","name":"write_file"}}"#
                        .to_string(),
                ),
            })
            .unwrap_err()
            .to_string();
        assert!(error.contains("after stream completion"), "{error}");
    }

    #[test]
    fn protocol_error_labels_are_bounded_and_do_not_echo_payloads() {
        let event = format!("{} secret-token", "x".repeat(200));
        let mut parser = ProviderStreamParser::new(RequestContract::ChatGptResponses, false);
        let error = parser
            .push_frame(SseFrame {
                event: Some(event),
                data: Some(r#"{"type":"different"}"#.to_string()),
            })
            .unwrap_err()
            .to_string();
        assert!(error.len() < 180, "{error}");
        assert!(!error.contains("secret-token"), "{error}");
    }

    #[test]
    fn decodes_split_mixed_delimiters_comments_and_multiline_data() {
        let mut decoder = SseDecoder::new(1024);
        assert!(
            decoder
                .push(b": keep-alive\r\nevent: update\r\nda")
                .unwrap()
                .is_empty()
        );
        let frames = decoder
            .push(b"ta: first\r\ndata: second\r\n\ndata: third\n\n")
            .unwrap();
        assert_eq!(
            frames,
            vec![
                SseFrame {
                    event: Some("update".to_string()),
                    data: Some("first\nsecond".to_string()),
                },
                SseFrame {
                    event: None,
                    data: Some("third".to_string()),
                },
            ]
        );
    }

    #[test]
    fn accepts_comment_keepalive_and_final_unterminated_frame() {
        let mut decoder = SseDecoder::new(1024);
        let frames = decoder.push(b": ping\n\ndata: final").unwrap();
        assert_eq!(
            frames,
            vec![SseFrame {
                event: None,
                data: None
            }]
        );
        assert_eq!(
            decoder.finish().unwrap(),
            vec![SseFrame {
                event: None,
                data: Some("final".to_string()),
            }]
        );
    }

    #[test]
    fn rejects_oversized_and_non_utf8_frames_without_echoing_payload() {
        let mut decoder = SseDecoder::new(4);
        let error = decoder.push(b"data: secret").unwrap_err().to_string();
        assert!(error.contains("exceeded 4 bytes"), "{error}");
        assert!(!error.contains("secret"), "{error}");

        let mut decoder = SseDecoder::new(32);
        let error = decoder
            .push(&[b'd', b'a', b't', b'a', b':', 0xff, b'\n', b'\n'])
            .unwrap_err()
            .to_string();
        assert!(error.contains("not valid UTF-8"), "{error}");
    }
}
