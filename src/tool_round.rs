use crate::*;

pub(crate) enum Plan {
    Immediate {
        content: String,
        is_error: Option<bool>,
    },
    Builtin,
    Runtime,
}

pub(crate) struct PlannedCall {
    pub(crate) tool_use_id: String,
    pub(crate) event_call_id: String,
    pub(crate) name: String,
    pub(crate) input: Value,
    pub(crate) journal_input: Value,
    pub(crate) input_str: String,
    pub(crate) summary: String,
    pub(crate) hosts: Vec<String>,
    pub(crate) bulk_network: bool,
    pub(crate) local_sudo_auth_needed: bool,
    pub(crate) cache_key: Option<String>,
    pub(crate) bash_similarity_key: Option<String>,
    pub(crate) verification_spec: Option<verification::CommandSpec>,
    pub(crate) verification_fingerprint: Option<String>,
    pub(crate) verification_post_fingerprint: Option<String>,
    pub(crate) prepared_mutation: Option<mutation_preview::PreparedMutation>,
    pub(crate) image_disclosure_target: Option<String>,
    pub(crate) journal_record_id: Option<String>,
    pub(crate) plan: Plan,
}

pub(crate) struct ToolRoundContext<'a> {
    pub(crate) tool_calls: Vec<(String, String, Value)>,
    pub(crate) iterations: u32,
    pub(crate) turn_id: String,
    pub(crate) objective_apply_fixes_allowed: bool,
    pub(crate) turn_state: &'a mut orchestrator::TurnRuntimeState,
    pub(crate) denied_signatures: HashSet<String>,
    pub(crate) hooks_approval_decided: bool,
    pub(crate) hooks_approved: bool,
}

pub(crate) struct ToolRoundOutcome {
    pub(crate) mutation_succeeded: bool,
    pub(crate) external_failures: usize,
    pub(crate) denied_signatures: HashSet<String>,
    pub(crate) hooks_approval_decided: bool,
    pub(crate) hooks_approved: bool,
}

pub(crate) fn command_installs_dext(command: &str, root: &Path) -> bool {
    command_install_target_status(command, root).is_some_and(|status| status != Some(false))
}

fn command_install_target_status(command: &str, root: &Path) -> Option<Option<bool>> {
    if !tool_policy::command_invokes_cargo_install(command) {
        return None;
    }
    let paths = tool_policy::cargo_install_path_args(command);
    if paths.is_empty() {
        return None;
    }
    let working_directory = if tool_policy::command_has_wrapper_directory_change(command) {
        None
    } else {
        verification::install_working_directory(command, root)
    };
    let mut status = Some(false);
    for path in paths {
        let path = Path::new(&path);
        let package = if path.is_absolute() {
            verification::dext_package_status(path)
        } else if let Some(cwd) = working_directory.as_deref() {
            verification::dext_package_status(&cwd.join(path))
        } else if verification::is_dext_checkout(root) {
            None
        } else {
            Some(false)
        };
        match package {
            Some(true) => return Some(Some(true)),
            None => status = None,
            Some(false) => {}
        }
    }
    Some(status)
}

impl Agent {
    pub(crate) async fn execute_tool_round(
        &mut self,
        context: ToolRoundContext<'_>,
    ) -> Result<ToolRoundOutcome> {
        let ToolRoundContext {
            tool_calls,
            iterations,
            turn_id,
            objective_apply_fixes_allowed,
            turn_state,
            mut denied_signatures,
            mut hooks_approval_decided,
            mut hooks_approved,
        } = context;
        let read_cache = self.read_cache.clone();
        let context_mode = self.context_mode;
        if self.session_enabled
            && tool_calls.iter().any(|(_, name, _)| {
                self.active_runtime_tool(name).is_some() || self.tool_is_side_effect_capable(name)
            })
        {
            self.save_latest_session()
                .context("persisting assistant tool calls before runtime/stateful or side-effect-capable execution")?;
            self.last_checkpoint_at = Some(std::time::Instant::now());
            self.last_checkpoint_signature = Some((self.history.len(), self.history_chars()));
        }

        if self.interrupt.load(Ordering::SeqCst) {
            self.append_latest_log("tool_round_interrupted", "before tool execution");
            let results: Vec<Block> = tool_calls
                .into_iter()
                .map(|(id, _, _)| Block::ToolResult {
                    tool_use_id: id,
                    content: "interrupted by user before tool execution".to_string(),
                    is_error: Some(true),
                    metadata: ToolResultMetadata {
                        status: Some("interrupted".to_string()),
                        ..ToolResultMetadata::default()
                    },
                })
                .collect();
            self.history.push(Message {
                role: "user".to_string(),
                content: results,
            });
            anyhow::bail!("interrupted by user");
        }

        let batch_id = format!("batch-{iterations}");
        let classified_verifications = tool_calls
            .iter()
            .filter(|(_, name, input)| {
                name == "bash"
                    && input["command"]
                        .as_str()
                        .and_then(verification::classify)
                        .is_some()
            })
            .count();
        let batch_has_dext_install = tool_calls.iter().any(|(_, name, input)| {
            name == "bash"
                && input["command"].as_str().is_some_and(|command| {
                    command_installs_dext(command, &self.sandbox_root)
                        || tool_policy::command_has_ambiguous_cargo_install(command)
                })
        });
        let multi_verification_batch =
            classified_verifications > 1 && classified_verifications == tool_calls.len();
        let mixed_verification_batch = (classified_verifications > 0
            && classified_verifications != tool_calls.len())
            || (batch_has_dext_install && tool_calls.len() != 1)
            || (multi_verification_batch && hooks_approved && !self.hooks.is_empty());
        let verification_cache_allowed =
            tool_calls.len() == 1 && (!hooks_approved || self.hooks.is_empty());
        let batch_policy_error = mixed_verification_batch.then(|| {
            "verification batch blocked: verification commands may be batched only with other verification commands and without active project hooks; Dext cargo install must be a standalone call"
                .to_string()
        });
        let dext_checkout = batch_has_dext_install;
        let mut plans: Vec<PlannedCall> = Vec::new();
        let mut journal_terminal_errors: Vec<String> = Vec::new();
        for (ordinal, (id, name, mut input)) in tool_calls.into_iter().enumerate() {
            let mut journal_input = input.clone();
            self.privacy
                .redact_recall_tool_input(&name, &mut journal_input, &self.sandbox_root);
            let event_call_id = normalize_tool_call_id(&id, 0, ordinal);
            let input_str = journal_input.to_string();
            let summary = summarize_call(&name, &input);
            let call_sig = format!("{name}\n{input}");
            let hosts = tool_policy::hosts_for_tool_call(&name, &input);
            let bulk_network = tool_policy::looks_like_bulk_network_call(&name, &input);
            let cache_key = orchestrator::network_cache_key(&name, &input);
            let bash_similarity_key = if name == "bash" {
                Some(orchestrator::normalize_bash_similarity_key(
                    input["command"].as_str().unwrap_or(""),
                ))
            } else if matches!(name.as_str(), "write_file" | "edit_file") {
                input["path"].as_str().map(|p| format!("{name}:{p}"))
            } else {
                None
            };

            let bash_mutates = name == "bash"
                && input["command"]
                    .as_str()
                    .is_some_and(orchestrator::bash_command_likely_mutates_files);
            let verification_spec = (name == "bash")
                .then(|| input["command"].as_str().and_then(verification::classify))
                .flatten();
            let verification_directory = verification_spec
                .as_ref()
                .and_then(|spec| spec.effective_directory(&self.sandbox_root));
            let verification_fingerprint =
                verification_directory.as_deref().and_then(|directory| {
                    verification::workspace_fingerprint_for_tool(
                        directory,
                        &self.pack_hook_env,
                        self.sandbox_profile.as_str(),
                    )
                });
            let verification_gate_eligible = verification_directory
                .as_deref()
                .is_some_and(verification::is_git_toplevel);
            let noncanonical_dext_install = name == "bash"
                && dext_checkout
                && input["command"].as_str().is_some_and(|command| {
                    command_installs_dext(command, &self.sandbox_root)
                        || tool_policy::command_has_ambiguous_cargo_install(command)
                })
                && !verification_spec
                    .as_ref()
                    .is_some_and(verification::CommandSpec::is_install);

            let mut plan: Option<Plan> =
                batch_policy_error.as_ref().map(|content| Plan::Immediate {
                    content: content.clone(),
                    is_error: Some(true),
                });
            let mut local_sudo_auth_needed = false;
            let mut prepared_mutation: Option<mutation_preview::PreparedMutation> = None;
            let mut image_disclosure_target: Option<String> = None;
            let journal_record_id: Option<String> = None;

            if plan.is_none() && noncanonical_dext_install {
                plan = Some(Plan::Immediate {
                    content: "installation blocked: installing Dext requires the standalone canonical command `cargo install --path . --force --locked` from the checkout directory"
                        .to_string(),
                    is_error: Some(true),
                });
            }

            if plan.is_none()
                && let Some(spec) = verification_spec.as_ref()
            {
                if spec.is_install() && dext_checkout && !verification_gate_eligible {
                    plan = Some(Plan::Immediate {
                        content: "installation blocked: Dext must be installed from the Git checkout top-level"
                            .to_string(),
                        is_error: Some(true),
                    });
                }
                if plan.is_none()
                    && spec.is_install()
                    && dext_checkout
                    && verification_fingerprint.is_none()
                {
                    plan = Some(Plan::Immediate {
                        content: "installation blocked: current workspace fingerprint is unavailable; inspect repository state and retry before installing Dext".to_string(),
                        is_error: Some(true),
                    });
                }
                if plan.is_none()
                    && let Some(fingerprint) = verification_fingerprint.as_deref()
                {
                    let full_ready = self.work_ledger.required_gates_passed(fingerprint);
                    if spec.is_install() && dext_checkout && !full_ready {
                        plan = Some(Plan::Immediate {
                            content: "installation blocked: the required Dext verification gates have not all passed for the current workspace fingerprint".to_string(),
                            is_error: Some(true),
                        });
                    } else if verification_cache_allowed
                        && !spec.is_install()
                        && let Some(record) =
                            self.work_ledger.cached_verification(spec, fingerprint)
                    {
                        plan = Some(Plan::Immediate {
                            content: format!(
                                "verification cache hit: {} already {} for workspace {}{}",
                                record.name,
                                record.status,
                                &fingerprint[..12.min(fingerprint.len())],
                                record
                                    .artifact
                                    .as_ref()
                                    .map(|path| format!("; artifact={path}"))
                                    .unwrap_or_default()
                            ),
                            is_error: Some(record.status != "passed"),
                        });
                    }
                }
                if plan.is_none()
                    && let Some(message) = self.work_ledger.code_loop.guard("V")
                {
                    plan = Some(Plan::Immediate {
                        content: message,
                        is_error: Some(true),
                    });
                }
            }

            let planned_mutation =
                matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit")
                    || bash_mutates && verification_spec.is_none();
            if plan.is_none()
                && planned_mutation
                && let Some(message) = self.work_ledger.code_loop.guard("M")
            {
                plan = Some(Plan::Immediate {
                    content: message,
                    is_error: Some(true),
                });
            }

            if let Err(msg) = self.validate_active_tool_input(&name, &input) {
                if let Some(budget_msg) = turn_state.tool_retry_guard(&name, &msg) {
                    emit_external_telemetry(self.sink.as_mut(), turn_state);
                    plan = Some(Plan::Immediate {
                        content: budget_msg,
                        is_error: Some(true),
                    });
                } else {
                    plan = Some(Plan::Immediate {
                        content: msg,
                        is_error: Some(true),
                    });
                }
            }

            if plan.is_none() && name == "read_image" && !self.model_supports_image_input() {
                plan = Some(Plan::Immediate {
                    content: format!(
                        "the active model '{}' does not advertise image input; switch to a vision-capable model or use explicit OCR/conversion",
                        self.model
                    ),
                    is_error: Some(true),
                });
            }

            if plan.is_none() && name == "read_image" && self.successful_images_this_turn >= 2 {
                plan = Some(Plan::Immediate {
                    content: "read_image allows at most two successful images per user turn; ask the user to start another turn for additional images".to_string(),
                    is_error: Some(true),
                });
            }

            if plan.is_none()
                && let Some((cached_content, cached_error)) =
                    turn_state.dedupe_guard(cache_key.as_deref())
            {
                emit_external_telemetry(self.sink.as_mut(), turn_state);
                plan = Some(Plan::Immediate {
                    content: cached_content,
                    is_error: cached_error,
                });
            }

            if plan.is_none()
                && let Some(msg) = turn_state.bash_similarity_guard(bash_similarity_key.as_deref())
            {
                emit_external_telemetry(self.sink.as_mut(), turn_state);
                plan = Some(Plan::Immediate {
                    content: msg,
                    is_error: Some(true),
                });
            }

            if plan.is_none()
                && let Some(msg) = turn_state.blocked_host_guard(&hosts)
            {
                plan = Some(Plan::Immediate {
                    content: msg,
                    is_error: Some(true),
                });
            }

            if plan.is_none()
                && let Some(msg) = turn_state.feasibility_guard(&hosts, bulk_network)
            {
                plan = Some(Plan::Immediate {
                    content: msg,
                    is_error: Some(true),
                });
            }

            if plan.is_none()
                && let Some(msg) = self.sandbox_policy_denial(&name, &input)
            {
                plan = Some(Plan::Immediate {
                    content: msg,
                    is_error: Some(true),
                });
            }

            if plan.is_none()
                && let Some(reason) = self.shelf_tool_denial(&name, &input)
            {
                plan = Some(Plan::Immediate {
                    content: format!("shelf policy blocked {name}: {reason}"),
                    is_error: Some(true),
                });
            }

            if plan.is_none()
                && let Some(msg) = self.privacy.path_denial(&name, &input, &self.sandbox_root)
            {
                plan = Some(Plan::Immediate {
                    content: msg,
                    is_error: Some(true),
                });
            }

            if plan.is_none() {
                match mutation_preview::prepare_tool_mutation(&name, &input, &self.sandbox_root) {
                    Ok(mut prepared) => {
                        if let Some(mutation) = prepared.as_mut()
                            && privacy::is_recall_memory_file(mutation.path())
                        {
                            mutation.rewrite_after_text(|text| self.privacy.redact_text(text).text);
                            if mutation.after_text().len() > privacy::RECALL_MEMORY_MAX_BYTES {
                                plan = Some(Plan::Immediate {
                                    content: format!(
                                        "recall.md would exceed the {}-byte working-memory cap; prune lower-value entries in the same edit",
                                        privacy::RECALL_MEMORY_MAX_BYTES
                                    ),
                                    is_error: Some(true),
                                });
                                prepared = None;
                            }
                        }
                        prepared_mutation = prepared;
                    }
                    Err(message) => {
                        plan = Some(Plan::Immediate {
                            content: message,
                            is_error: Some(true),
                        });
                    }
                }
            }

            if plan.is_none()
                && name == "read_image"
                && let Err(content) = image::validate_workspace_path(&self.sandbox_root, &input)
            {
                plan = Some(Plan::Immediate {
                    content,
                    is_error: Some(true),
                });
            }

            if plan.is_none() {
                let approved = if self.deny_tools.contains(&name)
                    || denied_signatures.contains(&call_sig)
                    || (self.tool_needs_permission(&name)
                        && self.approval_profile == ApprovalProfile::Never)
                {
                    false
                } else if self.tool_needs_permission(&name)
                    && !self.tool_auto_approved(&name, &input)
                {
                    // The preview and executor share the same prepared mutation.
                    if self.preview_mode != MutationPreviewMode::Off
                        && let Some(prepared) = prepared_mutation.as_ref()
                    {
                        self.sink
                            .emit(AgentEvent::Info(format_preview(&prepared.preview())));
                    }
                    match self.sink.request_permission(&name, &input) {
                        Choice::Once => true,
                        Choice::Always => {
                            self.allowed.insert(name.clone());
                            true
                        }
                        Choice::Deny => false,
                    }
                } else {
                    true
                };

                if approved && name == "git_commit" && !hooks_approval_decided {
                    hooks_approved = git_commit_hooks_approved(self);
                    hooks_approval_decided = true;
                }

                if !approved {
                    denied_signatures.insert(call_sig.clone());
                    plan = Some(Plan::Immediate {
                        content: "permission denied by user — do not retry this tool call; ask the user instead".to_string(),
                        is_error: Some(true),
                    });
                } else {
                    if name == "read_image" {
                        image_disclosure_target = Some(self.image_disclosure_target());
                        match image::approval_digest(&self.sandbox_root, &input) {
                            Ok(digest) => {
                                if let Some(fields) = input.as_object_mut() {
                                    fields.insert(
                                        image::APPROVED_SOURCE_SHA256_FIELD.to_string(),
                                        Value::String(digest),
                                    );
                                } else {
                                    plan = Some(Plan::Immediate {
                                        content:
                                            "invalid tool args for read_image: expected an object"
                                                .to_string(),
                                        is_error: Some(true),
                                    });
                                }
                            }
                            Err(content) => {
                                plan = Some(Plan::Immediate {
                                    content,
                                    is_error: Some(true),
                                });
                            }
                        }
                    }
                    if plan.is_none() {
                        let input_redacted = self.privacy.redact_text(&input_str).text;
                        let pre_env = [
                            ("DEXT_TOOL_NAME", name.as_str()),
                            ("DEXT_TOOL_INPUT", input_redacted.as_str()),
                        ];
                        let mut blocked: Option<String> = None;
                        if hooks_approved {
                            for (out, code) in self.hooks.fire(
                                "pre_tool",
                                &name,
                                &pre_env,
                                &self.pack_hook_env,
                                &self.sandbox_root,
                                self.sandbox_profile(),
                            ) {
                                if code != 0 {
                                    blocked = Some(format!(
                                        "pre_tool hook blocked (exit {code}):\n{}",
                                        out.trim()
                                    ));
                                    break;
                                }
                            }
                        }
                        plan = Some(match blocked {
                            Some(msg) => Plan::Immediate {
                                content: msg,
                                is_error: Some(true),
                            },
                            None => {
                                local_sudo_auth_needed = name == "bash"
                                    && tool_policy::command_invokes_sudo(
                                        input["command"].as_str().unwrap_or(""),
                                    );
                                if self.active_runtime_tool(&name).is_some() {
                                    Plan::Runtime
                                } else {
                                    Plan::Builtin
                                }
                            }
                        });
                    }
                }
            }

            let plan = plan.expect("plan must be set");

            if matches!(plan, Plan::Builtin)
                && bulk_network
                && let Some((_, msg)) =
                    turn_state.advance_phase(orchestrator::PhaseTrigger::ScaleCollection)
            {
                self.set_work_phase(turn_state.phase().label());
                self.sink.emit(AgentEvent::Info(format!(
                    "[phase:{}] {msg}",
                    turn_state.phase().label()
                )));
            }
            if matches!(plan, Plan::Builtin | Plan::Runtime)
                && self.tool_is_side_effect_capable(&name)
                && let Some((_, msg)) = turn_state.advance_phase(if objective_apply_fixes_allowed {
                    orchestrator::PhaseTrigger::Fix
                } else {
                    orchestrator::PhaseTrigger::DeliverableWrite
                })
            {
                self.set_work_phase(turn_state.phase().label());
                self.sink.emit(AgentEvent::Info(format!(
                    "[phase:{}] {msg}",
                    turn_state.phase().label()
                )));
            }

            plans.push(PlannedCall {
                tool_use_id: id,
                event_call_id,
                name,
                input,
                journal_input,
                input_str,
                summary,
                hosts,
                bulk_network,
                local_sudo_auth_needed,
                cache_key,
                bash_similarity_key,
                verification_spec,
                verification_fingerprint,
                verification_post_fingerprint: None,
                prepared_mutation,
                image_disclosure_target,
                journal_record_id,
                plan,
            });
        }

        let durable_result_round = self.session_enabled
            && plans.iter().any(|plan| {
                matches!(plan.plan, Plan::Runtime) || self.tool_is_side_effect_capable(&plan.name)
            });

        let runnable_indices: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| {
                if matches!(p.plan, Plan::Builtin | Plan::Runtime) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect();
        let runnable_set: HashSet<usize> = runnable_indices.iter().copied().collect();
        if runnable_indices.len() > 1 {
            let call_ids: Vec<String> = runnable_indices
                .iter()
                .map(|idx| plans[*idx].event_call_id.clone())
                .collect();
            let labels: Vec<String> = runnable_indices
                .iter()
                .map(|idx| plans[*idx].summary.clone())
                .collect();
            self.sink.emit(AgentEvent::ToolBatchStart {
                batch_id: batch_id.clone(),
                call_ids,
                labels,
            });
        }

        let builtin_indices: Vec<usize> = plans
            .iter()
            .enumerate()
            .filter_map(|(idx, p)| {
                if matches!(p.plan, Plan::Builtin | Plan::Runtime) {
                    Some(idx)
                } else {
                    None
                }
            })
            .collect();
        let mut builtin_started_at: HashMap<usize, std::time::Instant> = HashMap::new();
        // Calls that executed while a git credential was installed: only
        // their auth failures mean the credential itself was rejected.
        let mut builtin_git_cred_used: HashSet<usize> = HashSet::new();
        let builtin_names: Vec<&str> = builtin_indices
            .iter()
            .map(|idx| plans[*idx].name.as_str())
            .collect();
        let parallel_builtin_round = builtin_indices
            .iter()
            .all(|idx| matches!(plans[*idx].plan, Plan::Builtin))
            && should_parallelize_builtin_tools(&builtin_names);
        debug_assert!(
            !parallel_builtin_round
                || builtin_indices
                    .iter()
                    .all(|idx| !self.tool_is_side_effect_capable(&plans[*idx].name))
        );

        let mut builtin_outputs: HashMap<usize, std::result::Result<String, String>> =
            HashMap::new();
        if parallel_builtin_round {
            let mut builtin_tasks: tokio::task::JoinSet<(
                usize,
                std::result::Result<String, String>,
            )> = tokio::task::JoinSet::new();
            let mut pending: HashSet<usize> = builtin_indices.iter().copied().collect();
            for idx in builtin_indices.iter().copied() {
                let root = self.sandbox_root.clone();
                let n = plans[idx].name.clone();
                let inp = plans[idx].input.clone();
                let summary = plans[idx].summary.clone();
                self.sink.emit(AgentEvent::ToolCallStart {
                    call_id: plans[idx].event_call_id.clone(),
                    name: n.clone(),
                    summary: summary.clone(),
                });
                self.append_latest_log("tool_start", &summary);
                builtin_started_at.insert(idx, std::time::Instant::now());
                let interrupt = self.interrupt.clone();
                let sem = self.builtin_semaphore.clone();
                let read_cache = read_cache.clone();
                let session_id = self.session_id.clone();
                let sandbox_profile = self.sandbox_profile;
                let pack_env = self.pack_hook_env.clone();
                let git_credential =
                    stored_git_credential_for_bash_call(&n, &inp, self.git_credential.as_ref());
                if git_credential.is_some() {
                    builtin_git_cred_used.insert(idx);
                }
                builtin_tasks.spawn(async move {
                    let _permit = match sem.acquire_owned().await {
                        Ok(p) => p,
                        Err(e) => return (idx, Err(format!("builtin semaphore closed: {e}"))),
                    };
                    // The permit can be granted long after an interrupt
                    // arrived; `execute_builtin_call` re-reads the flag before
                    // it does anything, so a queued call short-circuits here
                    // rather than starting work the user already cancelled.
                    let outcome = execute_builtin_call_for_context(
                        n,
                        inp,
                        root,
                        interrupt,
                        Some(read_cache),
                        Some(session_id),
                        None,
                        git_credential,
                        None,
                        sandbox_profile,
                        hooks_approved,
                        None,
                        pack_env,
                        context_mode,
                    )
                    .await;
                    (idx, outcome)
                });
            }

            let mut task_panic: Option<String> = None;
            while !builtin_tasks.is_empty() {
                let next = tokio::select! {
                    joined = builtin_tasks.join_next() => Some(joined),
                    _ = tokio::time::sleep(std::time::Duration::from_millis(25)) => None,
                };
                match next {
                    Some(Some(Ok((idx, outcome)))) => {
                        pending.remove(&idx);
                        builtin_outputs.insert(idx, outcome);
                    }
                    // A JoinError carries no index, so the backfill below is
                    // what keeps every call paired with a result.
                    Some(Some(Err(e))) if !e.is_cancelled() => {
                        let message = format!("task panic: {e}");
                        self.append_latest_log("tool_task_panic", &message);
                        task_panic = Some(message);
                    }
                    Some(Some(Err(_))) => {}
                    Some(None) => break,
                    None if !self.interrupt.load(Ordering::SeqCst) => continue,
                    None => {
                        builtin_tasks.abort_all();
                        break;
                    }
                }
                if self.interrupt.load(Ordering::SeqCst) {
                    builtin_tasks.abort_all();
                    break;
                }
            }
            if self.interrupt.load(Ordering::SeqCst) {
                builtin_tasks.abort_all();
                // Calls that already finished did real work; keep their
                // results instead of reporting them as abandoned. This
                // never blocks: it takes only what is already complete.
                while let Some(joined) = builtin_tasks.try_join_next() {
                    if let Ok((idx, outcome)) = joined {
                        pending.remove(&idx);
                        builtin_outputs.insert(idx, outcome);
                    }
                }
            }
            drop(builtin_tasks);

            // Every tool_use id must receive a matching tool_result block, so
            // calls abandoned by an interrupt or a panicked task still get an
            // explicit outcome rather than silently going missing.
            if !pending.is_empty() {
                let interrupted = self.interrupt.load(Ordering::SeqCst);
                if interrupted {
                    self.append_latest_log("tool_round_interrupted", "during tool execution");
                }
                for idx in std::mem::take(&mut pending) {
                    let name = &plans[idx].name;
                    let message = if interrupted {
                        format!("{name} did not complete: interrupted by user")
                    } else if let Some(panic) = task_panic.as_deref() {
                        format!("{name} did not produce a result: {panic}")
                    } else {
                        format!("{name} did not produce a result: the tool task ended unexpectedly")
                    };
                    builtin_outputs.insert(idx, Err(message));
                }
            }
        } else {
            for idx in builtin_indices {
                let root = self.sandbox_root.clone();
                let n = plans[idx].name.clone();
                let inp = plans[idx].input.clone();
                let summary = plans[idx].summary.clone();
                let session_id = self.session_id.clone();
                let local_sudo_auth_needed = plans[idx].local_sudo_auth_needed;
                if let Some(spec) = plans[idx].verification_spec.clone() {
                    let directory = spec.effective_directory(&root);
                    let fingerprint = directory.as_deref().and_then(|directory| {
                        verification::workspace_fingerprint_for_tool(
                            directory,
                            &self.pack_hook_env,
                            self.sandbox_profile.as_str(),
                        )
                    });
                    plans[idx].verification_fingerprint = fingerprint.clone();
                    let install_error = spec
                        .is_install()
                        .then(|| {
                            if command_installs_dext(
                                inp["command"].as_str().unwrap_or(""),
                                &root,
                            ) {
                                if !directory
                                    .as_deref()
                                    .is_some_and(verification::is_git_toplevel)
                                {
                                    return Some(
                                        "installation blocked: Dext must be installed from the Git checkout top-level"
                                            .to_string(),
                                    );
                                }
                                match fingerprint.as_deref() {
                                    None => Some("installation blocked: current workspace fingerprint is unavailable; inspect repository state and retry before installing Dext".to_string()),
                                    Some(value) if !self.work_ledger.required_gates_passed(value) => Some("installation blocked: the required Dext verification gates have not all passed for the current workspace fingerprint".to_string()),
                                    Some(_) => None,
                                }
                            } else {
                                None
                            }
                        })
                        .flatten();
                    if let Some(message) = install_error {
                        builtin_outputs.insert(idx, Err(message));
                        continue;
                    }
                }
                if !journal_terminal_errors.is_empty() {
                    builtin_outputs.insert(
                        idx,
                        Err(format!(
                            "{n} was not executed because a prior side-effect outcome could not be durably journaled"
                        )),
                    );
                    continue;
                }
                // Sequential dispatch is the mutation boundary: a later call in
                // the same round must checkpoint state produced by earlier calls.
                if self.tool_is_side_effect_capable(&n)
                    && let Err(error) = self.maybe_create_tool_checkpoint(&n, &inp)
                {
                    builtin_outputs.insert(
                        idx,
                        Err(format!(
                            "{n} was not executed because its recovery checkpoint failed: {error}"
                        )),
                    );
                    continue;
                }
                if self.session_enabled && self.tool_is_side_effect_capable(&n) {
                    match tool_journal::start(
                        &root,
                        &session_id,
                        tool_journal::StartSpec {
                            turn_id: &turn_id,
                            batch_id: &batch_id,
                            call_id: &plans[idx].tool_use_id,
                            tool_name: &n,
                            summary: &tool_journal_summary(&n, &inp),
                            input: &plans[idx].journal_input,
                        },
                    ) {
                        Ok(record_id) => plans[idx].journal_record_id = Some(record_id),
                        Err(error) => {
                            builtin_outputs.insert(
                                idx,
                                Err(format!(
                                    "tool journal start fence failed; {n} was not executed: {error:#}"
                                )),
                            );
                            continue;
                        }
                    }
                }
                if n == "read_image"
                    && plans[idx].image_disclosure_target.as_deref()
                        != Some(self.image_disclosure_target().as_str())
                {
                    builtin_outputs.insert(
                        idx,
                        Err("the workspace, provider, endpoint, contract, or model changed after read_image approval; call read_image again".to_string()),
                    );
                    continue;
                }
                if n == "read_image" && self.successful_images_this_turn >= 2 {
                    builtin_outputs.insert(
                        idx,
                        Err("read_image allows at most two successful images per user turn; ask the user to start another turn for additional images".to_string()),
                    );
                    continue;
                }
                self.sink.emit(AgentEvent::ToolCallStart {
                    call_id: plans[idx].event_call_id.clone(),
                    name: n.clone(),
                    summary: summary.clone(),
                });
                self.append_latest_log("tool_start", &summary);
                builtin_started_at.insert(idx, std::time::Instant::now());
                let mut local_sudo_auth = if local_sudo_auth_needed {
                    match prepare_local_sudo_auth(&root, &session_id).await {
                        Ok(auth) => auth,
                        Err(e) => {
                            let outcome = Err(e);
                            if let Some(error) = persist_tool_journal_terminal(
                                &root,
                                &session_id,
                                plans[idx].journal_record_id.as_deref(),
                                &n,
                                &outcome,
                            ) {
                                journal_terminal_errors.push(error);
                            }
                            builtin_outputs.insert(idx, outcome);
                            continue;
                        }
                    }
                } else {
                    None
                };
                if let Some(auth) = local_sudo_auth.as_mut()
                    && auth.preauth_required
                {
                    let message = SUDO_AUTH_GUIDANCE.to_string();
                    match self.sink.request_local_auth_secret("bash", &message) {
                        LocalAuthSecret::Secret(password) => {
                            match create_sudo_password_fifo(&root, &session_id) {
                                Ok(fifo) => {
                                    auth.password_fifo = Some(fifo);
                                    auth.password = Some(password);
                                }
                                Err(e) => {
                                    let mut password = password;
                                    clear_secret_string(&mut password);
                                    let outcome = Err(e);
                                    if let Some(error) = persist_tool_journal_terminal(
                                        &root,
                                        &session_id,
                                        plans[idx].journal_record_id.as_deref(),
                                        &n,
                                        &outcome,
                                    ) {
                                        journal_terminal_errors.push(error);
                                    }
                                    builtin_outputs.insert(idx, outcome);
                                    continue;
                                }
                            }
                        }
                        LocalAuthSecret::Canceled => {
                            let outcome = Err("sudo authentication canceled by user".to_string());
                            if let Some(error) = persist_tool_journal_terminal(
                                &root,
                                &session_id,
                                plans[idx].journal_record_id.as_deref(),
                                &n,
                                &outcome,
                            ) {
                                journal_terminal_errors.push(error);
                            }
                            builtin_outputs.insert(idx, outcome);
                            continue;
                        }
                        LocalAuthSecret::Unavailable => {
                            self.sink.local_auth_prompt("bash", SUDO_AUTH_GUIDANCE);
                        }
                    }
                }
                let live_output =
                    live_output_for_tool(self.sink.as_ref(), &plans[idx].event_call_id, &n, &inp);
                let git_credential_for_call =
                    stored_git_credential_for_bash_call(&n, &inp, self.git_credential.as_ref());
                if git_credential_for_call.is_some() {
                    builtin_git_cred_used.insert(idx);
                }
                let prepared_mutation = plans[idx].prepared_mutation.take();
                let r = if matches!(plans[idx].plan, Plan::Runtime) {
                    self.execute_pack_runtime_tool(&n, &inp, &turn_id, iterations)
                        .await
                } else {
                    execute_builtin_call_for_context(
                        n.clone(),
                        inp,
                        root.clone(),
                        self.interrupt.clone(),
                        Some(read_cache.clone()),
                        Some(session_id.clone()),
                        local_sudo_auth,
                        git_credential_for_call,
                        prepared_mutation,
                        self.sandbox_profile,
                        hooks_approved,
                        live_output,
                        self.pack_hook_env.clone(),
                        context_mode,
                    )
                    .await
                };
                if n == "read_image"
                    && r.as_ref().is_ok_and(|content| {
                        serde_json::from_str::<image::ImageReference>(content).is_ok()
                    })
                {
                    self.successful_images_this_turn =
                        self.successful_images_this_turn.saturating_add(1);
                }
                if multi_verification_batch
                    && let Some(spec) = plans[idx].verification_spec.as_ref()
                {
                    plans[idx].verification_post_fingerprint = spec
                        .effective_directory(&root)
                        .as_deref()
                        .and_then(|directory| {
                            verification::workspace_fingerprint_for_tool(
                                directory,
                                &self.pack_hook_env,
                                self.sandbox_profile.as_str(),
                            )
                        });
                }
                if let Some(error) = persist_tool_journal_terminal(
                    &root,
                    &session_id,
                    plans[idx].journal_record_id.as_deref(),
                    &n,
                    &r,
                ) {
                    journal_terminal_errors.push(error);
                }
                builtin_outputs.insert(idx, r);
            }
        }

        let mut batch_failed = 0usize;
        let mut batch_call_ids: Vec<String> = Vec::new();
        let mut batch_labels: Vec<String> = Vec::new();
        let mut results = Vec::new();
        let mut round_external_failures: usize = 0;
        let mut mutation_succeeded = false;
        let mut extension_state_may_have_changed = hooks_approved && !self.hooks.is_empty();
        for (idx, p) in plans.into_iter().enumerate() {
            let PlannedCall {
                tool_use_id,
                event_call_id,
                name,
                input,
                journal_input: _journal_input,
                input_str,
                summary,
                hosts,
                bulk_network: _bulk_network,
                local_sudo_auth_needed: _local_sudo_auth_needed,
                cache_key,
                bash_similarity_key,
                verification_spec,
                verification_fingerprint,
                verification_post_fingerprint,
                prepared_mutation: _prepared_mutation,
                image_disclosure_target,
                journal_record_id: _journal_record_id,
                plan,
            } = p;

            let started_at = builtin_started_at.remove(&idx);
            let ran_tool = matches!(plan, Plan::Builtin | Plan::Runtime);
            let ran_runtime = matches!(plan, Plan::Runtime);
            let ui_summary = summary.clone();
            let mut followup_warnings: Vec<String> = Vec::new();
            let mut provider_runtime_notes: Vec<String> = Vec::new();
            if let Some(advisory) = tool_policy::tool_input_advisory(&name, &input) {
                followup_warnings.push(advisory.clone());
                provider_runtime_notes.push(advisory);
            }

            let (mut content, mut is_error) = match plan {
                Plan::Immediate { content, is_error } => (content, is_error),
                Plan::Builtin | Plan::Runtime => match builtin_outputs.remove(&idx).unwrap_or_else(|| {
                    Err(format!(
                        "internal tool runner omitted the result for {name}; the tool outcome is unknown"
                    ))
                }) {
                    Ok(s) => {
                        if name == "bash" {
                            let failed = parse_bash_exit_code(&s).is_some_and(|code| code != 0)
                                && !bash_sigpipe_with_output(&s);
                            (s, failed.then_some(true))
                        } else {
                            (s, None)
                        }
                    }
                    Err(e) => (e, Some(true)),
                },
            };
            let mut image_reference = None;
            if !is_error.unwrap_or(false) && ran_tool && name == "read_image" {
                match (
                    serde_json::from_str::<image::ImageReference>(&content),
                    image_disclosure_target,
                ) {
                    (Ok(reference), Some(disclosure_target)) => {
                        content = format!(
                            "approved image ({}x{}, sanitized as {}); pixels are available to the model only in this turn",
                            reference.width, reference.height, reference.media_type
                        );
                        image_reference = Some(image_reference_block(reference, disclosure_target));
                    }
                    (Ok(_), None) => {
                        content = "missing internal image disclosure target".to_string();
                        is_error = Some(true);
                    }
                    (Err(error), _) => {
                        content = format!("invalid internal image reference: {error}");
                        is_error = Some(true);
                    }
                }
            }
            let ok = !is_error.unwrap_or(false);

            if name == "bash" && output_indicates_git_credential_failure(&content) {
                let ran_with_credential = builtin_git_cred_used.contains(&idx);
                let failure_hosts = git_credential_hosts_for_failure(&hosts, &content);
                content.push_str(
                    &self.handle_git_credential_failure(ran_with_credential, failure_hosts),
                );
            }

            let post_tool_result = self.privacy.redact_text(&content).text;
            let post_tool_input = self.privacy.redact_text(&input_str).text;
            let post_env = [
                ("DEXT_TOOL_NAME", name.as_str()),
                ("DEXT_TOOL_INPUT", post_tool_input.as_str()),
                ("DEXT_TOOL_RESULT", post_tool_result.as_str()),
            ];
            if hooks_approved {
                for (out, _code) in self.hooks.fire(
                    "post_tool",
                    &name,
                    &post_env,
                    &self.pack_hook_env,
                    &self.sandbox_root,
                    self.sandbox_profile(),
                ) {
                    let t = out.trim();
                    if !t.is_empty() {
                        content.push_str(&format!("\n\n[hook:post_tool]\n{t}"));
                    }
                }
            }

            if ran_tool
                && (ran_runtime && self.tool_is_side_effect_capable(&name)
                    || matches!(
                        name.as_str(),
                        "write_file"
                            | "edit_file"
                            | "multi_edit"
                            | "bash"
                            | "awk"
                            | "csvkit"
                            | "git_commit"
                    ))
            {
                // Even a failed process may have changed extension files before
                // reporting its error.
                extension_state_may_have_changed = true;
            }
            if ok
                && ran_tool
                && name == "git_diff"
                && input["stat"].as_bool() != Some(true)
                && !content.trim().is_empty()
            {
                self.work_ledger.code_loop.note_review();
            }
            if ok && ran_tool && matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit")
            {
                self.work_ledger_note_file_change(&input);
            }
            if ok
                && ran_tool
                && (ran_runtime && self.tool_is_side_effect_capable(&name)
                    || ACTION_CONTRACT_MUTATING_TOOL_NAMES.contains(&name.as_str())
                    || name == "bash"
                        && input["command"]
                            .as_str()
                            .is_some_and(orchestrator::bash_command_likely_mutates_files))
            {
                mutation_succeeded = true;
                turn_state.mark_mutation_succeeded();
                self.work_ledger.code_loop.note_mutation();
            }
            let privacy_redaction = self.privacy.apply_tool_output(&name, &input, content);
            content = privacy_redaction.text;
            let observation =
                turn_state.record_external_outcome(orchestrator::ExternalOutcomeInput {
                    tool_name: &name,
                    hosts: &hosts,
                    cache_key: cache_key.as_deref(),
                    bash_similarity_key: bash_similarity_key.as_deref(),
                    command: input["command"].as_str(),
                    content: &mut content,
                    is_error,
                });
            emit_external_telemetry(self.sink.as_mut(), turn_state);
            round_external_failures =
                round_external_failures.saturating_add(observation.round_external_failures);
            followup_warnings.extend(observation.followup_warnings);
            insert_runtime_notes(&mut content, &provider_runtime_notes);

            if runnable_set.contains(&idx) {
                if !ok {
                    batch_failed = batch_failed.saturating_add(1);
                }
                batch_call_ids.push(event_call_id.clone());
                batch_labels.push(ui_summary.clone());
            }

            let ui_cap = orchestrator::adaptive_tool_ui_cap_for_window(
                &self.last_request_usage,
                self.context_window_tokens(),
                TOOL_UI_CONTENT_CAP,
            );
            let ui_content = orchestrator::compress_tool_ui_content(&content, ui_cap);
            self.sink.emit(AgentEvent::ToolCallResult {
                call_id: event_call_id.clone(),
                name: name.clone(),
                ok,
                preview: ui_summary.clone(),
                content: ui_content,
            });
            for warning in followup_warnings {
                self.sink.emit(AgentEvent::Warn(warning));
            }
            self.append_latest_log(
                if ok { "tool_ok" } else { "tool_error" },
                &format!("{} :: {}", ui_summary, content),
            );
            let verification_command = if !ran_runtime && matches!(name.as_str(), "bash" | "csvkit")
            {
                if name == "bash" {
                    serde_json::from_str::<Value>(&input_str)
                        .ok()
                        .and_then(|v| v["command"].as_str().map(String::from))
                } else {
                    Some(ui_summary.clone())
                }
            } else {
                None
            };
            let is_verification_result = ran_tool && verification_spec.is_some();
            let mut artifact_display: Option<String> = None;
            let mut verification_status: Option<String> = None;
            if is_verification_result {
                let command = verification_command.clone().unwrap_or_default();
                let spec = verification_spec.as_ref().expect("verification classified");
                let post_fingerprint = if multi_verification_batch {
                    verification_post_fingerprint
                } else {
                    spec.effective_directory(&self.sandbox_root)
                        .as_deref()
                        .and_then(|directory| {
                            verification::workspace_fingerprint_for_tool(
                                directory,
                                &self.pack_hook_env,
                                self.sandbox_profile.as_str(),
                            )
                        })
                };
                let fingerprint = verification_fingerprint.clone().unwrap_or_default();
                let duration = started_at
                    .map(|t| t.elapsed())
                    .unwrap_or_else(|| std::time::Duration::from_millis(0));
                let exit_code = parse_tool_exit_code(&name, ok, &content);
                let status = verification::evidence_status(
                    ok && exit_code.unwrap_or(0) == 0,
                    verification_fingerprint.as_deref(),
                    post_fingerprint.as_deref(),
                );
                let command_cwd = spec
                    .effective_directory(&self.sandbox_root)
                    .unwrap_or_else(|| self.sandbox_root.clone());
                let artifact = write_verification_artifact(
                    &self.sandbox_root,
                    &self.session_id,
                    VerificationArtifactSpec {
                        name: &ui_summary,
                        command: &command,
                        command_cwd: &command_cwd,
                        output: &content,
                        exit_code,
                        duration,
                        status,
                        privacy: &self.privacy,
                    },
                );
                artifact_display = artifact.as_ref().map(|p| p.display().to_string());
                verification_status = Some(status.to_string());
                let scope = spec.scope_label();
                self.work_ledger.code_loop.note_verification(
                    if spec
                        .scopes
                        .iter()
                        .any(|scope| verification::REQUIRED_GATES.contains(scope))
                    {
                        "full"
                    } else {
                        "focused"
                    },
                );
                self.work_ledger.verification.push(VerificationRecord {
                    evidence_version: verification::EVIDENCE_VERSION,
                    reusable: true,
                    gate_eligible: spec
                        .scopes
                        .iter()
                        .any(|scope| verification::REQUIRED_GATES.contains(scope))
                        && spec
                            .effective_directory(&self.sandbox_root)
                            .as_deref()
                            .is_some_and(verification::is_git_toplevel),
                    name: ui_summary.clone(),
                    command: command.clone(),
                    command_key: spec.key.clone(),
                    workspace_fingerprint: fingerprint.clone(),
                    runtime_id: verification::runtime_id().to_string(),
                    completed_at_secs: unix_timestamp_secs(),
                    mutation_sequence: self.work_ledger.code_loop.mutation_sequence,
                    scope,
                    status: status.to_string(),
                    exit_code,
                    duration_ms: millis_u64(duration),
                    artifact: artifact_display.clone(),
                    validates: Vec::new(),
                });
                if self.work_ledger.verification.len() > 24 {
                    let excess = self.work_ledger.verification.len() - 24;
                    self.work_ledger.verification.drain(0..excess);
                }
                if let Some(path) = artifact_display.as_deref() {
                    self.append_latest_log(
                        "verification",
                        &format!(
                            "{status} {ui_summary} fingerprint={} artifact={path}",
                            &fingerprint[..12.min(fingerprint.len())]
                        ),
                    );
                }
            }
            let dynamic_result_cap = tool_result_context_cap_with_window(
                &name,
                &input,
                &self.last_request_usage,
                &self.model,
                Some(self.context_window_tokens()),
                self.context_mode,
            );
            let result_status =
                verification_status.unwrap_or_else(|| if ok { "ok" } else { "error" }.to_string());
            let exit_code = parse_tool_exit_code(&name, ok, &content);
            let result_duration_ms = started_at.map(|t| millis_u64(t.elapsed()));
            let result_artifact = artifact_display.clone();
            let result_hint = if let Some(path) = result_artifact.as_deref() {
                format!("Full verification output saved as a structured artifact: {path}")
            } else {
                "Full verification output saved as a structured artifact; see verification ledger."
                    .to_string()
            };
            results.push(Block::ToolResult {
                tool_use_id,
                content: if is_verification_result {
                    cap_bytes_head_tail_with_hint(content, dynamic_result_cap, &result_hint)
                } else if matches!(name.as_str(), "bash" | "awk" | "csvkit") {
                    cap_bytes_head_tail_with_hint(
                        content,
                        dynamic_result_cap,
                        TOOL_OUTPUT_NARROW_HINT,
                    )
                } else {
                    cap_tool_output_with_cap(content, dynamic_result_cap)
                },
                is_error,
                metadata: ToolResultMetadata {
                    status: Some(result_status),
                    exit_code,
                    duration_ms: result_duration_ms,
                    artifact: result_artifact,
                },
            });
            if let Some(reference) = image_reference {
                results.push(reference);
            }
        }

        if runnable_indices.len() > 1 {
            self.sink.emit(AgentEvent::ToolBatchEnd {
                batch_id,
                call_ids: batch_call_ids,
                labels: batch_labels,
                failed: batch_failed,
            });
        }

        let squashed_results = squash_identical_error_result_content(results);
        self.history.push(Message {
            role: "user".to_string(),
            content: squashed_results,
        });
        if durable_result_round {
            let result_checkpoint = self
                .save_latest_session()
                .context("persisting tool results and runtime state after execution");
            if let Err(error) = result_checkpoint {
                journal_terminal_errors.push(format!("tool result checkpoint failed: {error:#}"));
            } else {
                self.last_checkpoint_at = Some(std::time::Instant::now());
                self.last_checkpoint_signature = Some((self.history.len(), self.history_chars()));
                if let Err(error) = tool_journal::compact(&self.sandbox_root, &self.session_id) {
                    self.sink.emit(AgentEvent::Warn(format!(
                        "[warn] tool journal compaction failed: {error:#}"
                    )));
                }
            }
        } else {
            self.checkpoint_latest_session("after_tool_results");
        }
        if !journal_terminal_errors.is_empty() {
            let detail = journal_terminal_errors.join("; ");
            self.append_latest_log("tool_journal_hard_error", &detail);
            self.sink.emit(AgentEvent::Warn(format!(
                "[hard error] tool outcome/state recovery is unresolved: {detail}"
            )));
            anyhow::bail!(
                "tool outcome/state recovery is unresolved; inspect the session and tool journal before retrying: {detail}"
            );
        }

        if extension_state_may_have_changed {
            // Pack discovery and the typed shelf registry are cached across
            // provider requests in one turn. Tools and approved hooks can
            // create, remove, or edit PACK.md/shelf.json even when their
            // eventual outcome is an error.
            self.shelf_registry = shelves::ShelfRegistry::discover(&self.sandbox_root);
            if let Ok(mut cache) = self.prompt_scan_cache.lock() {
                *cache = None;
            }
        }

        Ok(ToolRoundOutcome {
            mutation_succeeded,
            external_failures: round_external_failures,
            denied_signatures,
            hooks_approval_decided,
            hooks_approved,
        })
    }
}
