use crate::provider::RequestContract;
use crate::streaming::{ProviderStreamParser, SseDecoder};
use crate::{
    Agent, AgentEvent, Block, Usage, build_compacted_history, millis_u64,
    render_compaction_evidence, sha256_hex_str, summarize_inline, tool_journal,
};
use anyhow::{Result, bail};
use futures_util::StreamExt as _;
use serde_json::json;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

#[derive(Default)]
pub(crate) struct BackgroundState {
    pub(crate) enabled: bool,
    pub(crate) history_epoch: u64,
    pub(crate) config_epoch: u64,
    pub(crate) job: Option<BackgroundJob>,
    pub(crate) cooldown: Option<Instant>,
    pub(crate) last_prefix: Option<String>,
    pub(crate) next_job: u64,
}

pub(crate) struct BackgroundJob {
    pub(crate) worker: SummaryWorker,
    pub(crate) id: String,
    pub(crate) session: String,
    pub(crate) turn: String,
    pub(crate) history_epoch: u64,
    pub(crate) config_epoch: u64,
    pub(crate) config_digest: String,
    pub(crate) prefix_digest: String,
    pub(crate) split: usize,
    pub(crate) preserved: Vec<crate::Message>,
    pub(crate) before_chars: usize,
}

pub(crate) struct SummaryRequest {
    pub(crate) client: reqwest::Client,
    pub(crate) request: reqwest::Request,
    pub(crate) contract: RequestContract,
    pub(crate) first_byte: Duration,
    pub(crate) idle: Duration,
    pub(crate) provider: String,
    pub(crate) model: String,
    pub(crate) pricing: crate::UsagePricing,
    pub(crate) pricing_override: bool,
    pub(crate) override_wire_cost: bool,
    pub(crate) input_tokens: u64,
    pub(crate) speculative: bool,
}

#[derive(Default)]
pub(crate) struct SummaryAccounting {
    pub(crate) usage: Usage,
    pub(crate) inflight: Usage,
    pub(crate) inflight_open: bool,
    pub(crate) accounted: bool,
    pub(crate) unknown: bool,
    pub(crate) retries: Vec<(u32, u64, String)>,
}

pub(crate) struct SummaryWorker {
    pub(crate) task: tokio::task::JoinHandle<Result<String>>,
    pub(crate) cancel: Arc<AtomicBool>,
    pub(crate) accounting: Arc<Mutex<SummaryAccounting>>,
    pub(crate) started: Instant,
}

impl Drop for SummaryWorker {
    fn drop(&mut self) {
        self.cancel.store(true, Ordering::SeqCst);
        self.task.abort();
    }
}

impl SummaryWorker {
    pub(crate) fn spawn(request: SummaryRequest, deadline: Option<Duration>) -> Self {
        let cancel = Arc::new(AtomicBool::new(false));
        let accounting = Arc::new(Mutex::new(SummaryAccounting::default()));
        let stop = cancel.clone();
        let counts = accounting.clone();
        let task = tokio::spawn(async move {
            let compute = request.compute(&counts);
            tokio::pin!(compute);
            let mut tick = tokio::time::interval(Duration::from_millis(25));
            let timeout = async {
                match deadline {
                    Some(deadline) => tokio::time::sleep(deadline).await,
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::pin!(timeout);
            loop {
                if stop.load(Ordering::SeqCst) {
                    counts.lock().unwrap_or_else(|e| e.into_inner()).unknown = true;
                    bail!("summary cancelled");
                }
                tokio::select! {
                    result = &mut compute => return result,
                    _ = &mut timeout => {
                        counts.lock().unwrap_or_else(|e| e.into_inner()).unknown = true;
                        bail!("summary deadline exceeded");
                    }
                    _ = tick.tick() => {}
                }
            }
        });
        Self {
            task,
            cancel,
            accounting,
            started: Instant::now(),
        }
    }
}

fn summary_rate_limited(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    message.contains("429") || message.contains("rate_limit") || message.contains("rate limit")
}

impl SummaryRequest {
    async fn compute(&self, accounting: &Mutex<SummaryAccounting>) -> Result<String> {
        for attempt in 1..=crate::MAX_STREAM_ATTEMPTS {
            let result = self.attempt(accounting).await;
            match result {
                Ok((text, stop)) => {
                    if let Some(reason) =
                        crate::chatgpt_incomplete_reason(self.contract, stop.as_deref())
                    {
                        if reason == "content_filter" || attempt == crate::MAX_STREAM_ATTEMPTS {
                            bail!(
                                "summary response was incomplete ({reason}) after {attempt} attempts"
                            );
                        }
                        accounting
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .retries
                            .push((
                                attempt,
                                0,
                                format!("incomplete summary response ({reason})"),
                            ));
                        continue;
                    }
                    if text.trim().is_empty() {
                        bail!("summary response had no text blocks");
                    }
                    return Ok(text);
                }
                Err(error) => {
                    let message = crate::stream_error_body(&error);
                    let plan = crate::orchestrator::classify_stream_error(&message);
                    // Speculation must not compete with a rate-limited foreground request.
                    if !plan.retry
                        || !self.contract.is_responses()
                        || attempt == crate::MAX_STREAM_ATTEMPTS
                        || (self.speculative && summary_rate_limited(&message))
                    {
                        return Err(error);
                    }
                    let wait = 1u64 << (attempt - 1);
                    accounting
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .retries
                        .push((
                            attempt,
                            wait,
                            format!("{} summary stream error", plan.label()),
                        ));
                    tokio::time::sleep(Duration::from_secs(wait)).await;
                }
            }
        }
        unreachable!()
    }

    fn price(&self, mut usage: Usage) -> Usage {
        if usage.total_tokens() > 0 && (self.override_wire_cost || usage.cost_usd.is_none()) {
            let pricing = crate::openai_long_context_pricing_with_override_state(
                &self.provider,
                &self.model,
                usage,
                self.pricing,
                self.pricing_override,
            );
            usage.cost_usd = Some(pricing.estimate(usage));
        }
        usage
    }

    fn record_usage(&self, usage: Usage, accounting: &Mutex<SummaryAccounting>) {
        let usage = self.price(usage);
        let mut accounting = accounting.lock().unwrap_or_else(|e| e.into_inner());
        if !accounting.accounted {
            accounting.usage.add(usage);
            accounting.inflight = Usage::default();
            accounting.inflight_open = false;
        }
    }

    fn observe_usage(&self, usage: Usage, accounting: &Mutex<SummaryAccounting>) {
        let usage = self.price(usage);
        let mut accounting = accounting.lock().unwrap_or_else(|e| e.into_inner());
        if !accounting.accounted {
            accounting.inflight = usage;
        }
    }

    async fn attempt(
        &self,
        accounting: &Mutex<SummaryAccounting>,
    ) -> Result<(String, Option<String>)> {
        accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .inflight_open = true;
        let request = self
            .request
            .try_clone()
            .ok_or_else(|| anyhow::anyhow!("summary request body is not cloneable"))?;
        let response =
            tokio::time::timeout(self.first_byte, self.client.execute(request)).await??;
        let status = response.status();
        if !status.is_success() {
            let text = crate::read_provider_error_body(response, self.idle)
                .await
                .unwrap_or_default();
            bail!("summary {}", crate::http_status_error(status, &text));
        }
        if !self.contract.is_responses() {
            let value = crate::read_provider_json_body(response, self.idle).await?;
            let usage = if self.contract == RequestContract::AnthropicMessages {
                Usage::parse(&value["usage"])
            } else {
                Usage::parse_openai(&value["usage"])
            };
            if usage.total_tokens() == 0 {
                accounting.lock().unwrap_or_else(|e| e.into_inner()).unknown = true;
            }
            self.record_usage(usage, accounting);
            let text = if self.contract == RequestContract::AnthropicMessages {
                value["content"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(|block| {
                        (block["type"] == "text")
                            .then(|| block["text"].as_str())
                            .flatten()
                    })
                    .collect::<String>()
            } else {
                crate::openai_summary_text_from_response(&value)?
            };
            return Ok((text, None));
        }
        let mut decoder = SseDecoder::new(crate::streaming::sse_event_cap(self.contract));
        let mut parser = ProviderStreamParser::new(self.contract, false);
        let mut stream = response.bytes_stream();
        let mut bytes = 0usize;
        let mut observed = Usage::default();
        let result: Result<_> = async {
            while let Some(chunk) = tokio::time::timeout(self.idle, stream.next()).await? {
                let chunk = chunk?;
                bytes = bytes.saturating_add(chunk.len());
                if bytes > crate::PROVIDER_JSON_BODY_CAP {
                    bail!("summary stream exceeded byte limit");
                }
                for frame in decoder.push(&chunk)? {
                    parser.push_frame(frame)?;
                    observed = parser.known_usage();
                    self.observe_usage(observed, accounting);
                }
            }
            for frame in decoder.finish()? {
                parser.push_frame(frame)?;
                observed = parser.known_usage();
                self.observe_usage(observed, accounting);
            }
            parser.finish()
        }
        .await;
        if result.is_err() {
            self.record_usage(observed, accounting);
            accounting.lock().unwrap_or_else(|e| e.into_inner()).unknown = true;
        }
        let parsed = result?;
        let mut usage = parsed.usage;
        if usage.total_tokens() == 0 {
            accounting.lock().unwrap_or_else(|e| e.into_inner()).unknown = true;
        }
        crate::Agent::fill_missing_usage_metrics(&mut usage, self.input_tokens, &parsed.blocks);
        self.record_usage(usage, accounting);
        let text = parsed
            .blocks
            .into_iter()
            .filter_map(|block| match block {
                Block::Text { text } | Block::PartialStream { text } => Some(text),
                _ => None,
            })
            .collect::<String>();
        Ok((text, parsed.stop_reason))
    }
}

impl Agent {
    pub(crate) fn history_pairs_closed(history: &[crate::Message]) -> bool {
        let mut uses = std::collections::HashMap::new();
        let mut results = std::collections::HashMap::new();
        for block in history.iter().flat_map(|message| &message.content) {
            match block {
                Block::ToolUse { id, .. } => *uses.entry(id).or_insert(0usize) += 1,
                Block::ToolResult { tool_use_id, .. } => {
                    *results.entry(tool_use_id).or_insert(0usize) += 1
                }
                _ => {}
            }
        }
        uses == results
    }

    pub(crate) fn background_config_digest(&self) -> String {
        sha256_hex_str(&json!({
            "route": self.provider_route_identity(), "root": self.sandbox_root,
            "model": self.compact_summary_model(), "context": self.context_mode,
            "effort": self.thinking_effort, "reasoning": self.reasoning_mode,
            "privacy": self.privacy.mode_label(), "threshold": self.active_compact_threshold_chars(),
            "budget": self.budget_cap, "approval": self.approval_profile,
            "sandbox": self.sandbox_profile,
        }).to_string())
    }

    pub(crate) fn background_event(
        &mut self,
        job: &BackgroundJob,
        phase: &str,
        reason: &str,
        wait_ms: u64,
        after_chars: Option<usize>,
    ) {
        let known = {
            let accounting = job
                .worker
                .accounting
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            !(accounting.unknown || accounting.inflight_open)
        };
        self.sink.emit(AgentEvent::BackgroundCompaction {
            version: 1,
            session_id: job.session.clone(),
            session_epoch: job.history_epoch,
            job_id: job.id.clone(),
            origin_turn_id: job.turn.clone(),
            phase: phase.into(),
            blocking: phase == "waiting",
            reason: reason.into(),
            elapsed_ms: millis_u64(job.worker.started.elapsed()),
            wait_ms,
            before_chars: job.before_chars,
            after_chars,
            usage_known: known,
        });
        self.append_latest_log("background_compaction", &format!("job={} phase={phase} reason={reason} elapsed_ms={} wait_ms={wait_ms} before={} after={after_chars:?}", job.id, millis_u64(job.worker.started.elapsed()), job.before_chars));
    }

    pub(crate) fn account_background(&mut self, job: &BackgroundJob) {
        let accounting = job
            .worker
            .accounting
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let mut accounting = accounting;
        if accounting.accounted {
            return;
        }
        if !job.worker.task.is_finished() {
            accounting.unknown = true;
        }
        let mut known = accounting.usage;
        known.add(accounting.inflight);
        accounting.accounted = true;
        self.session_usage.add(known);
        self.sink.emit(AgentEvent::UsageUpdate {
            turn: Usage::default(),
            session: self.session_usage,
        });
        if accounting.unknown || accounting.inflight_open {
            self.append_latest_log("summary_usage_unknown", &job.id);
        }
    }

    pub(crate) fn invalidate_background(&mut self, reason: &str) {
        if self.background.job.is_some() {
            self.append_latest_log("background_invalidate", reason);
        }
        self.background.history_epoch = self.background.history_epoch.wrapping_add(1);
        self.background.config_epoch = self.background.config_epoch.wrapping_add(1);
        if let Some(job) = self.background.job.take() {
            job.worker.cancel.store(true, Ordering::SeqCst);
            job.worker.task.abort();
            self.background_event(&job, "cancelled", reason, 0, None);
            self.background.job = Some(job);
            self.background.cooldown = Some(std::time::Instant::now());
        }
    }

    pub(crate) fn retire_background_session(&mut self, reason: &str) {
        self.invalidate_background(reason);
        if let Some(job) = self.background.job.take() {
            self.account_background(&job);
        }
    }

    pub(crate) async fn settle_background(&mut self, reason: &str) {
        if let Some(mut job) = self.background.job.take() {
            job.worker.cancel.store(true, Ordering::SeqCst);
            if tokio::time::timeout(std::time::Duration::from_millis(100), &mut job.worker.task)
                .await
                .is_err()
            {
                job.worker.task.abort();
                let _ = (&mut job.worker.task).await;
            }
            self.account_background(&job);
            self.background_event(&job, "cancelled", reason, 0, None);
        }
    }

    pub(crate) async fn background_wakeup(&self) {
        let Some(job) = self.background.job.as_ref() else {
            std::future::pending::<()>().await;
            return;
        };
        while !job.worker.task.is_finished() && !self.interrupt.load(Ordering::SeqCst) {
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
    }

    pub(crate) fn background_eligible(&self, job: &BackgroundJob) -> bool {
        job.session == self.session_id
            && job.history_epoch == self.background.history_epoch
            && job.config_epoch == self.background.config_epoch
            && job.config_digest == self.background_config_digest()
            && self.history.len() >= job.split
            && tool_journal::input_sha256(&json!(&self.history[..job.split]))
                .is_ok_and(|digest| digest == job.prefix_digest)
            && Self::compact_split_is_pair_safe(&self.history, job.split)
    }

    pub(crate) async fn service_background(&mut self, wait: bool) -> bool {
        let Some(mut job) = self.background.job.take() else {
            return false;
        };
        if self.interrupt.load(Ordering::SeqCst) || !self.background_eligible(&job) {
            job.worker.cancel.store(true, Ordering::SeqCst);
            let _ =
                tokio::time::timeout(std::time::Duration::from_millis(100), &mut job.worker.task)
                    .await;
            if !job.worker.task.is_finished() {
                job.worker.task.abort();
                let _ = (&mut job.worker.task).await;
            }
            self.account_background(&job);
            self.background_event(&job, "discarded", "invalidated", 0, None);
            self.background.cooldown = Some(std::time::Instant::now());
            return false;
        }
        if !wait && !job.worker.task.is_finished() {
            self.background.job = Some(job);
            return false;
        }
        let waiting = std::time::Instant::now();
        if wait && !job.worker.task.is_finished() {
            self.background_event(&job, "waiting", "headroom", 0, None);
        }
        let mut tick = tokio::time::interval(std::time::Duration::from_millis(25));
        let deadline = tokio::time::sleep(std::time::Duration::from_secs(10));
        tokio::pin!(deadline);
        let result = loop {
            if self.interrupt.load(Ordering::SeqCst) {
                job.worker.cancel.store(true, Ordering::SeqCst);
                if tokio::time::timeout(std::time::Duration::from_millis(100), &mut job.worker.task)
                    .await
                    .is_err()
                {
                    job.worker.task.abort();
                    let _ = (&mut job.worker.task).await;
                }
                break Err(anyhow::anyhow!("summary wait interrupted"));
            }
            tokio::select! {
                result = &mut job.worker.task => break result.map_err(anyhow::Error::from).and_then(|value| value),
                _ = &mut deadline => { job.worker.task.abort(); let _ = (&mut job.worker.task).await; break Err(anyhow::anyhow!("headroom wait timed out")); }
                _ = tick.tick() => {}
            }
        };
        self.account_background(&job);
        let wait_ms = if wait {
            millis_u64(waiting.elapsed())
        } else {
            0
        };
        self.background.cooldown = Some(std::time::Instant::now());
        let summary = match result {
            Ok(summary) => self.privacy.redact_text(&summary).text,
            Err(error) => {
                self.background_event(
                    &job,
                    "failed",
                    &summarize_inline(&self.privacy.redact_text(&error.to_string()).text, 160),
                    wait_ms,
                    None,
                );
                return false;
            }
        };
        self.background_event(&job, "ready", "computed", wait_ms, None);
        if !self.background_eligible(&job) || self.interrupt.load(Ordering::SeqCst) {
            self.background_event(&job, "discarded", "stale", wait_ms, None);
            return false;
        }
        let candidate =
            build_compacted_history(&summary, job.preserved.clone(), &self.history[job.split..]);
        let chars = candidate
            .iter()
            .map(Self::message_compaction_bytes)
            .sum::<usize>();
        if chars >= self.history_chars() || !Self::history_pairs_closed(&candidate) {
            self.background_event(&job, "discarded", "not_useful_or_unpaired", wait_ms, None);
            return false;
        }
        let before = self.history.len();
        let old = std::mem::replace(&mut self.history, candidate);
        if self.session_enabled
            && let Err(error) = self.save_session_to_path(&self.latest_session_path)
        {
            self.history = old;
            self.background_event(
                &job,
                "failed",
                &format!("persist: {}", summarize_inline(&error.to_string(), 120)),
                wait_ms,
                None,
            );
            return false;
        }
        self.background.history_epoch = self.background.history_epoch.wrapping_add(1);
        self.sink.emit(AgentEvent::HistoryContextUpdated {
            chars: self.history_chars(),
            tokens: Some(self.estimated_context_tokens_from_history()),
        });
        self.sink.emit(AgentEvent::CompactEnd {
            before,
            after: self.history.len(),
            summary: summary.clone(),
            job_id: Some(job.id.clone()),
            background: true,
        });
        self.background_event(
            &job,
            "applied",
            "installed",
            wait_ms,
            Some(self.history_chars()),
        );
        self.post_compact_hooks(&summary);
        true
    }

    pub(crate) fn start_background(&mut self, turn: &str) -> Result<()> {
        let threshold = self.active_compact_threshold_chars();
        if !self.background.enabled
            || self.background.job.is_some()
            || !self.hooks.pre_request.is_empty()
            || self.interrupt.load(Ordering::SeqCst)
            || self.history_chars() < threshold.saturating_mul(4) / 5
            || self.history_chars() >= threshold
            || self
                .background
                .cooldown
                .is_some_and(|at| at.elapsed() < std::time::Duration::from_secs(30))
            || self.budget_cap.is_some()
        {
            return Ok(());
        }
        let Some(split) = self.find_compact_split() else {
            return Ok(());
        };
        if !Self::history_pairs_closed(&self.history[..split]) {
            return Ok(());
        }
        let prefix = json!(&self.history[..split]);
        if serde_json::to_vec(&prefix)?.len() > 4 * 1024 * 1024 {
            return Ok(());
        }
        let prefix_digest = tool_journal::input_sha256(&prefix)?;
        if self.background.last_prefix.as_ref() == Some(&prefix_digest) {
            return Ok(());
        }
        let (input, preserved) = self.split_compaction_inputs(&self.history[..split]);
        if input.is_empty() {
            return Ok(());
        }
        let evidence = render_compaction_evidence(
            &self.history[..split],
            &self.work_ledger,
            &self.provider_health,
        );
        let mut request = self.prepare_summary_request(&input, &evidence)?;
        request.speculative = true;
        self.background.next_job = self.background.next_job.wrapping_add(1);
        let job = BackgroundJob {
            worker: SummaryWorker::spawn(request, Some(std::time::Duration::from_secs(60))),
            id: format!(
                "bg-{}-{}-{}",
                self.session_id, self.background.history_epoch, self.background.next_job
            ),
            session: self.session_id.clone(),
            turn: turn.into(),
            history_epoch: self.background.history_epoch,
            config_epoch: self.background.config_epoch,
            config_digest: self.background_config_digest(),
            prefix_digest: prefix_digest.clone(),
            split,
            preserved,
            before_chars: self.history_chars(),
        };
        self.background.last_prefix = Some(prefix_digest);
        self.background_event(&job, "running", "soft_threshold", 0, None);
        self.background.job = Some(job);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn speculative_rate_limit_detection_covers_http_and_stream_errors() {
        for message in ["HTTP 429", "RATE_LIMIT_EXCEEDED", "rate limit reached"] {
            assert!(summary_rate_limited(message));
        }
        assert!(!summary_rate_limited("server_error"));
    }
}
