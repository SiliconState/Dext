use super::*;
use crate::provider::{
    DEFAULT_LOCAL_CONTEXT_WINDOW_TOKENS, clear_cached_local_llama_context_windows,
    list_models_for_available_providers, merge_provider_profile, normalize_chatgpt_model_slug,
    parse_llama_context_window, refresh_local_llama_context_window,
    resolve_provider_model_selection,
};
#[cfg(target_os = "linux")]
use crate::session::process_exists;
use crate::session::{
    append_log_line, canonicalize_read_tool_path, canonicalize_tool_path, cap_latest_log_buffer,
    render_limited_lines, validate_session_name,
};
use crate::tools::{self, is_parallel_safe_tool};
use serde_json::json;
use std::net::TcpListener;
#[cfg(target_os = "linux")]
use std::process::Command;
use std::sync::OnceLock;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::test_env_lock().lock().expect("env lock")
}

fn temp_test_dir(label: &str) -> PathBuf {
    let unique = format!(
        "dext-{label}-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos()
    );
    let dir = std::env::temp_dir().join(unique);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn prepend_env_path(path: &Path) -> String {
    let existing = std::env::var_os("PATH").unwrap_or_default();
    let mut parts = vec![path.to_path_buf()];
    parts.extend(std::env::split_paths(&existing));
    std::env::join_paths(parts)
        .expect("join PATH")
        .to_string_lossy()
        .into_owned()
}

fn test_agent(root: &Path) -> Agent {
    Agent {
        client: Arc::new(OnceLock::new()),
        provider_id: "test".to_string(),
        api_key: "test-key".to_string(),
        key_source: "test".to_string(),
        provider_requires_api_key: true,
        base_url: "http://127.0.0.1".to_string(),
        model: "test-model".to_string(),
        api_provider: ApiProvider::Anthropic,
        thinking_effort: ThinkingEffort::Medium,
        system: "test-system".to_string(),
        history: Vec::new(),
        tools: provider_tool_definitions()
            .into_iter()
            .filter(|t| t.name != "browser")
            .filter(|t| tool_name_allowed_in_profile(t.name, ToolContextProfile::Default))
            .collect(),
        allowed: HashSet::new(),
        deny_tools: HashSet::new(),
        sandbox_root: root.to_path_buf(),
        git_context: None,
        silent: true,
        pretty: false,
        max_iterations: Some(1),
        session_usage: Usage::default(),
        interrupt: Arc::new(AtomicBool::new(false)),
        shelf_registry: shelves::ShelfRegistry::discover(root),
        hooks: Hooks::default(),
        pack_hook_env: Vec::new(),
        state_lock: None,
        session_enabled: true,
        latest_session_path: latest_session_path(root),
        latest_log_path: latest_log_path(root),
        pending_login_provider: None,
        suppress_checkpoints: false,
        last_checkpoint_at: None,
        session_model_pins: HashMap::new(),
        partial_stream_text: None,
        compact_threshold_chars: None,
        compact_threshold_percent: None,
        context_window_tokens: model_context_window("test-model"),
        approval_profile: ApprovalProfile::default(),
        sandbox_profile: SandboxProfile::default(),
        browser_recipe: BrowserRecipe::default(),
        context_mode: ContextMode::default(),
        tool_context_profile: ToolContextProfile::default(),
        tool_profile: ToolProfile::default(),
        preview_mode: MutationPreviewMode::default(),
        budget_cap: None,
        budget_exhausted: false,
        builtin_semaphore: Arc::new(tokio::sync::Semaphore::new(max_concurrent_builtins())),
        sink: Box::new(NullSink),
        runtime_control_rx: None,
        runtime_control_tx: Agent::noop_text_tx(),
        steering_rx: None,
        steering_tx: Agent::noop_steering_tx(),
        read_cache: Arc::new(Mutex::new(ReadFileCache::default())),
        work_ledger: WorkLedger::default(),
        provider_health: ProviderHealthLedger::default(),
        track_origin: None,
        privacy: PrivacyPolicy::default(),
        detached_subagent_steer_path: None,
        checkpoint_cache: git_checkpoints::RepoRootCache::new(),
        checkpoint_ordinal: 0,
    }
}

fn drain_events(rx: &mut tokio::sync::mpsc::UnboundedReceiver<AgentEvent>) -> Vec<AgentEvent> {
    let mut events = Vec::new();
    while let Ok(event) = rx.try_recv() {
        events.push(event);
    }
    events
}

fn git_ok(root: &Path, args: &[&str]) {
    let output = std::process::Command::new("git")
        .args(args)
        .current_dir(root)
        .output()
        .expect("run git");
    assert!(
        output.status.success(),
        "git {:?} failed: {}",
        args,
        String::from_utf8_lossy(&output.stderr)
    );
}

#[test]
fn git_checkpoint_non_git_is_noop() {
    let root = temp_test_dir("checkpoint-non-git");
    let checkpoint = git_checkpoints::create_checkpoint(&root, "write_file", &[], 1)
        .expect("checkpoint non-git");
    assert!(checkpoint.is_none());
}

#[test]
fn git_checkpoint_sidecar_restores_untracked_file() {
    let root = temp_test_dir("checkpoint-sidecar");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    std::fs::write(root.join("note.txt"), "before\n").expect("write untracked");
    let cp = git_checkpoints::create_checkpoint(&root, "write_file", &["note.txt".to_string()], 1)
        .expect("create checkpoint")
        .expect("checkpoint exists");
    std::fs::write(root.join("note.txt"), "after\n").expect("mutate untracked");

    git_checkpoints::restore_worktree(&root, &cp, git_checkpoints::RestoreMode::Worktree)
        .expect("restore checkpoint");
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("read restored"),
        "before\n"
    );
}

#[test]
fn memory_merge_recall_prefers_ours_and_dedupes_theirs() {
    let base = "# Dext recall\n- keep\n- remove\n";
    let ours = "# Dext recall\n- keep\n- ours\n";
    let theirs = "# Dext recall\n- keep\n- remove\n- theirs\n";
    let merged = memory_merge::merge_recall(base, ours, theirs);
    assert!(merged.clean);
    assert!(merged.content.contains("- ours"));
    assert!(merged.content.contains("- theirs"));
    assert!(!merged.content.contains("- remove\n"));
    assert_eq!(merged.content.matches("- keep").count(), 1);
}

#[test]
fn mutation_preview_new_file_does_not_duplicate_added_lines() {
    let root = temp_test_dir("mutation-preview-new");
    let preview =
        mutation_preview::preview_write_file(&root, "new.txt", "a\nb\n").expect("preview new file");
    assert_eq!(preview.added, 2);
    assert_eq!(preview.diff.matches("+a").count(), 1);
    assert_eq!(preview.diff.matches("+b").count(), 1);
}

#[test]
fn tool_paths_allow_user_global_pack_roots_outside_project() {
    let _guard = env_lock();
    let root = temp_test_dir("tool-path-global-pack-root");
    let home = temp_test_dir("tool-path-global-pack-home");
    let pack_dir = home.join("packs/demo");
    std::fs::create_dir_all(&pack_dir).expect("create global pack dir");
    let pack_md = pack_dir.join("PACK.md");
    std::fs::write(&pack_md, "---\nname: demo\n---\n# Demo\n").expect("write PACK.md");
    let notes = pack_dir.join("notes.md");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
    }

    let canonical = canonicalize_tool_path(&root, &pack_md.display().to_string())
        .expect("allow global pack path");
    assert_eq!(
        canonical,
        std::fs::canonicalize(&pack_md).expect("canonical pack path")
    );

    let preview = mutation_preview::preview_write_file(&root, &notes.display().to_string(), "hi\n")
        .expect("preview global pack write");
    assert_eq!(preview.path, notes);
    assert!(preview.is_new_file);

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn tool_paths_allow_external_reads_but_reject_external_writes() {
    let _guard = env_lock();
    let root = temp_test_dir("tool-path-reject-root");
    let home = temp_test_dir("tool-path-reject-home");
    let outside = temp_test_dir("tool-path-reject-outside");
    let outside_file = outside.join("deny.txt");
    std::fs::write(&outside_file, "nope\n").expect("write outside file");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
    }

    let read_canonical = canonicalize_read_tool_path(&root, &outside_file.display().to_string())
        .expect("read tools may inspect outside sandbox");
    assert_eq!(
        read_canonical,
        std::fs::canonicalize(&outside_file).unwrap()
    );
    let read_output = execute_tool("read_file", &json!({"path": outside_file}), &root)
        .expect("read_file may inspect outside sandbox");
    assert!(read_output.contains("1\tnope"), "{read_output}");

    let err = canonicalize_tool_path(&root, &outside_file.display().to_string())
        .expect_err("reject outside write path");
    assert!(
        err.contains("outside sandbox or Dext global pack roots"),
        "{err}"
    );
    let write_err = execute_tool(
        "write_file",
        &json!({"path": outside_file, "content": "write\n"}),
        &root,
    )
    .expect_err("write_file must stay confined");
    assert!(
        write_err.contains("outside sandbox or Dext global pack roots"),
        "{write_err}"
    );

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn memory_merge_cli_skips_merge_subcommand_for_positionals() {
    let root = temp_test_dir("memory-merge-cli");
    let base = root.join("base.md");
    let ours = root.join("ours.md");
    let theirs = root.join("theirs.md");
    std::fs::write(&base, "# Memory\n\n## A\nbase\n").expect("write base");
    std::fs::write(&ours, "# Memory\n\n## A\nours\n").expect("write ours");
    std::fs::write(&theirs, "# Memory\n\n## A\nbase\n").expect("write theirs");

    let args = vec![
        "merge".to_string(),
        base.display().to_string(),
        ours.display().to_string(),
        theirs.display().to_string(),
    ];
    assert_eq!(handle_memory_cli(&args, &root), 0);
    let merged = std::fs::read_to_string(&ours).expect("read merged");
    assert!(merged.contains("ours"), "{merged}");
    assert!(!merged.contains("theirs"), "{merged}");
}

struct SessionReplayFixture {
    header: SessionHeader,
    history: Vec<Message>,
}

impl SessionReplayFixture {
    fn load(name: &str) -> Result<Self> {
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/session-replays")
            .join(format!("{name}.jsonl"));
        let content = std::fs::read_to_string(&path)
            .with_context(|| format!("reading replay fixture {}", path.display()))?;
        let mut lines = content.lines();
        let header = parse_session_header(lines.next().context("empty replay fixture")?)?;
        let mut history = Vec::new();
        for (i, line) in lines.enumerate() {
            if line.trim().is_empty() {
                continue;
            }
            history.push(
                serde_json::from_str::<Message>(line)
                    .with_context(|| format!("bad replay message on line {}", i + 2))?,
            );
        }
        Ok(Self { header, history })
    }

    fn assistant_text(&self) -> String {
        assistant_text(&self.history)
    }

    fn tool_call_count(&self, name: &str) -> usize {
        history_tool_count(&self.history, name)
    }

    fn tool_names_after_marker(&self, marker: &str) -> Vec<String> {
        let mut after = false;
        let mut names = Vec::new();
        for msg in &self.history {
            for block in &msg.content {
                if block_contains_marker(block, marker) {
                    after = true;
                    continue;
                }
                if after && let Block::ToolUse { name, .. } = block {
                    names.push(name.clone());
                }
            }
        }
        names
    }

    fn tool_inputs_after_marker(&self, marker: &str, tool_name: &str) -> Vec<String> {
        let mut after = false;
        let mut inputs = Vec::new();
        for msg in &self.history {
            for block in &msg.content {
                if block_contains_marker(block, marker) {
                    after = true;
                    continue;
                }
                if after
                    && let Block::ToolUse { name, input, .. } = block
                    && name == tool_name
                {
                    inputs.push(input.to_string());
                }
            }
        }
        inputs
    }

    fn tool_results_containing(&self, needle: &str) -> Vec<(String, Option<bool>)> {
        let mut out = Vec::new();
        for msg in &self.history {
            for block in &msg.content {
                if let Block::ToolResult {
                    content, is_error, ..
                } = block
                    && content.contains(needle)
                {
                    out.push((content.clone(), *is_error));
                }
            }
        }
        out
    }

    fn text_blocks_containing(&self, needle: &str) -> Vec<String> {
        let mut out = Vec::new();
        for msg in &self.history {
            for block in &msg.content {
                if let Block::Text { text } = block
                    && text.contains(needle)
                {
                    out.push(text.clone());
                }
            }
        }
        out
    }
}

fn block_contains_marker(block: &Block, marker: &str) -> bool {
    match block {
        Block::Text { text } | Block::PartialStream { text } | Block::Thinking { text } => {
            text.contains(marker)
        }
        Block::ToolUse { name, input, .. } => {
            name.contains(marker) || input.to_string().contains(marker)
        }
        Block::ToolResult { content, .. } => content.contains(marker),
    }
}

fn tool_result_block(tool_use_id: &str, content: &str, is_error: Option<bool>) -> Block {
    Block::ToolResult {
        tool_use_id: tool_use_id.to_string(),
        content: content.to_string(),
        is_error,
        metadata: ToolResultMetadata::default(),
    }
}

#[test]
fn injected_steering_is_ledger_visible_and_final_required() {
    let root = temp_test_dir("steering-ledger-visible");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let (tx_events, mut rx_events) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx: tx_events }));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    agent.install_steering(rx, tx.clone());
    tx.send("fix the rg border overflow and tell me what happened".to_string())
        .expect("send steering");
    let mut turn_state = orchestrator::TurnRuntimeState::new();

    assert!(agent.inject_queued_steering(&mut turn_state, 3, 7, true));
    let ledger = agent.work_ledger_prompt();
    assert!(ledger.contains("queued_user_updates:"), "{ledger}");
    assert!(ledger.contains("rg border overflow"), "{ledger}");
    assert!(ledger.contains("respond to queued user update"), "{ledger}");
    let injected = agent
        .history
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            Block::Text { text } if text.contains("[queued-user-update]") => Some(text.clone()),
            _ => None,
        })
        .expect("queued update prompt injected");
    assert!(
        injected.contains("must explicitly address it"),
        "{injected}"
    );
    assert!(
        drain_events(&mut rx_events).iter().any(|event| matches!(
            event,
            AgentEvent::SteeringReceived { messages, preview }
                if *messages == 1 && preview.contains("rg border overflow")
        )),
        "expected compact queued-update event"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn busy_queued_slash_commands_are_not_injected_as_steering() {
    let root = temp_test_dir("busy-slash-not-steering");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let (tx_events, mut rx_events) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx: tx_events }));
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    agent.install_steering(rx, tx.clone());
    tx.send("/compact status".to_string()).expect("queue slash");
    tx.send("please adjust the current fix".to_string())
        .expect("queue steering");
    let mut turn_state = orchestrator::TurnRuntimeState::new();

    assert!(agent.inject_queued_steering(&mut turn_state, 1, 0, false));
    assert!(
        agent
            .history
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .any(|text| text.contains("please adjust the current fix"))
    );
    assert!(
        !agent
            .history
            .iter()
            .flat_map(|m| m.content.iter())
            .filter_map(|b| match b {
                Block::Text { text } => Some(text.as_str()),
                _ => None,
            })
            .any(|text| text.contains("/compact status"))
    );
    assert!(drain_events(&mut rx_events).iter().any(|event| matches!(
        event,
        AgentEvent::Warn(msg) if msg.contains("not run while agent is busy")
    )));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn channel_sink_emits_local_auth_only_via_explicit_method() {
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut sink = ChannelSink { tx };

    sink.emit(AgentEvent::Info("visible".to_string()));
    assert!(matches!(rx.try_recv(), Ok(AgentEvent::Info(_))));
    assert!(rx.try_recv().is_err());

    sink.local_auth_prompt("bash", SUDO_AUTH_GUIDANCE);
    assert!(matches!(
        rx.try_recv(),
        Ok(AgentEvent::LocalAuthPrompt { tool, message })
            if tool == "bash" && message == SUDO_AUTH_GUIDANCE
    ));
}

#[test]
fn local_auth_prompt_is_not_recorded_in_crash_or_latest_logs() {
    let root = temp_test_dir("local-auth-no-logs");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    let mut agent = test_agent(&root);
    let log_path = agent.latest_log_path.clone();
    agent.set_sink(Box::new(ChannelSink { tx }));

    if let Ok(mut state) = crash_runtime_state().lock() {
        state.last_event_ids.clear();
    }
    agent.sink.local_auth_prompt("bash", SUDO_AUTH_GUIDANCE);

    assert!(matches!(
        rx.try_recv(),
        Ok(AgentEvent::LocalAuthPrompt { tool, message })
            if tool == "bash" && message == SUDO_AUTH_GUIDANCE
    ));
    let crash_labels = crash_runtime_state()
        .lock()
        .map(|state| state.last_event_ids.clone())
        .unwrap_or_default();
    assert!(
        crash_labels
            .iter()
            .all(|label| !label.contains("local_auth")),
        "{crash_labels:?}"
    );
    assert!(!log_path.exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn potential_login_secret_is_not_serialized_to_provider_request() {
    let root = temp_test_dir("local-login-secret-provider");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));
    let (steer_tx, steer_rx) = tokio::sync::mpsc::unbounded_channel();
    agent.install_steering(steer_rx, steer_tx.clone());
    let secret = "/login chatgpt sk-secret-token-that-should-stay-local";

    assert!(text_is_potential_local_secret(secret));
    steer_tx.send(secret.to_string()).expect("queue steering");
    let mut turn_state = orchestrator::TurnRuntimeState::new();
    assert!(!agent.inject_queued_steering(&mut turn_state, 1, 0, false));
    assert!(agent.history.is_empty());
    assert!(drain_events(&mut rx).iter().any(|event| matches!(
        event,
        AgentEvent::Warn(msg) if msg.contains("withheld")
    )));

    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "safe steering note".to_string(),
        }],
    });
    let (_url, body) = agent
        .build_streaming_request("sys", "env", &[], &[], "sess-local")
        .expect("build request");
    let body_text = String::from_utf8(body).expect("utf8 request");
    assert!(!body_text.contains("sk-secret-token-that-should-stay-local"));

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn sudo_askpass_script_uses_local_tty_and_never_echoes_chat_guidance() {
    let script = sudo_askpass_script_content_with_paths(
        "/tmp/zenity'bad",
        "/tmp/kdialog",
        "/usr/bin/osascript",
    );
    assert!(script.contains("/dev/tty"), "{script}");
    assert!(script.contains("osascript"), "{script}");
    assert!(
        script.contains("Dext local sudo prompt requires a TTY"),
        "{script}"
    );
    assert!(!script.contains("chat/steering"), "{script}");
    assert!(script.contains("'\\''"), "{script}");
}

#[test]
fn busy_console_input_withholds_potential_local_secret_from_steering() {
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (runtime_control_tx, mut runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
    let busy = AtomicBool::new(true);

    let route = route_interactive_input_line(
        "token=abcdefghijklmnopqrstuvwxyz\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );

    assert_eq!(route, InteractiveInputRoute::Dropped);
    assert!(runtime_control_rx.try_recv().is_err());
    assert!(steering_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());

    let route = route_interactive_input_line(
        "{\"accessToken\":\"secret-token-that-should-go-to-login\"}\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );
    assert_eq!(route, InteractiveInputRoute::Dropped);
    assert!(runtime_control_rx.try_recv().is_err());
    assert!(steering_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());

    let route = route_interactive_input_line(
        "/login chatgpt sk-secret-token-that-should-stay-local\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );
    assert_eq!(route, InteractiveInputRoute::Dropped);
    assert!(runtime_control_rx.try_recv().is_err());
    assert!(steering_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());

    let route = route_interactive_input_line(
        "/compact status\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );
    let InteractiveInputRoute::UnsupportedBusySlash(warning) = route else {
        panic!("unexpected route: {route:?}");
    };
    assert_eq!(
        unsupported_busy_slash_message(&warning),
        "queued slash command /compact not run while agent is busy; only /model and /effort (/think) are active runtime controls"
    );
    assert!(runtime_control_rx.try_recv().is_err());
    assert!(steering_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());

    let route = route_interactive_input_line(
        "/model gpt-5.5, /effort high\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );
    assert_eq!(route, InteractiveInputRoute::RuntimeControlQueued);
    assert_eq!(
        runtime_control_rx.try_recv().ok().as_deref(),
        Some("/model gpt-5.5")
    );
    assert_eq!(
        runtime_control_rx.try_recv().ok().as_deref(),
        Some("/effort high")
    );
    assert!(steering_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());
}

#[test]
fn active_runtime_control_detection_accepts_only_model_and_effort_sequences() {
    assert!(is_active_runtime_control_command("/model gpt-5.5"));
    assert!(is_active_runtime_control_command(
        "/model gpt-5.5, /think high"
    ));
    assert!(is_active_runtime_control_command("/think status"));
    assert!(!is_active_runtime_control_command("/tools full"));
    assert!(!is_active_runtime_control_command("/compact 25%"));
    assert!(!is_active_runtime_control_command(
        "/model gpt-5.5, adjust this"
    ));
    assert_eq!(
        parse_active_runtime_control_sequence(" /model chatgpt/gpt-5.4 , /effort xhigh ")
            .expect("parse sequence"),
        vec!["/model chatgpt/gpt-5.4", "/effort xhigh"]
    );
}

#[test]
fn runtime_control_events_serialize_for_stream_json() {
    let ev = AgentEvent::RuntimeControl("thinking effort -> high".to_string());
    let value = serde_json::to_value(ev).expect("serialize event");
    assert_eq!(value["event"], "runtime_control");
    assert_eq!(value["data"], "thinking effort -> high");

    let ev = AgentEvent::RuntimeControlApplied {
        commands: 2,
        model_changed: true,
        effort_changed: true,
        stream_aborted: true,
    };
    let value = serde_json::to_value(ev).expect("serialize event");
    assert_eq!(value["event"], "runtime_control_applied");
    assert_eq!(value["data"]["commands"], 2);
    assert_eq!(value["data"]["model_changed"], true);
    assert_eq!(value["data"]["effort_changed"], true);
    assert_eq!(value["data"]["stream_aborted"], true);
}

#[test]
fn steering_received_event_serializes_preview() {
    let ev = AgentEvent::SteeringReceived {
        messages: 1,
        preview: "fix rg border overflow".to_string(),
    };
    let value = serde_json::to_value(ev).expect("serialize event");
    assert_eq!(value["event"], "steering_received");
    assert_eq!(value["data"]["messages"], 1);
    assert_eq!(value["data"]["preview"], "fix rg border overflow");
}

#[test]
fn non_tui_busy_input_routes_steering_and_runtime_controls_immediately() {
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (runtime_control_tx, mut runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
    let busy = AtomicBool::new(true);

    let route = route_interactive_input_line(
        "adjust the current fix\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );

    assert_eq!(route, InteractiveInputRoute::SteeringQueued);
    assert_eq!(
        steering_rx.try_recv().ok().as_deref(),
        Some("adjust the current fix")
    );
    assert!(runtime_control_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());

    let route = route_interactive_input_line(
        "/effort high\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );
    assert_eq!(route, InteractiveInputRoute::RuntimeControlQueued);
    assert_eq!(
        runtime_control_rx.try_recv().ok().as_deref(),
        Some("/effort high")
    );
    assert!(steering_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());

    let route = route_interactive_input_line(
        "/model gpt-5.5\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );
    assert_eq!(route, InteractiveInputRoute::RuntimeControlQueued);
    assert_eq!(
        runtime_control_rx.try_recv().ok().as_deref(),
        Some("/model gpt-5.5")
    );
    assert!(steering_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());

    let route = route_interactive_input_line(
        "/compact status\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
    );
    let InteractiveInputRoute::UnsupportedBusySlash(warning) = route else {
        panic!("unexpected route: {route:?}");
    };
    assert_eq!(
        unsupported_busy_slash_message(&warning),
        "queued slash command /compact not run while agent is busy; only /model and /effort (/think) are active runtime controls"
    );
    assert!(steering_rx.try_recv().is_err());
    assert!(runtime_control_rx.try_recv().is_err());
    assert!(input_rx.try_recv().is_err());
}

#[tokio::test]
async fn non_aborting_runtime_control_keeps_pending_provider_response() {
    let root = temp_test_dir("runtime-control-pending-response");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let request_count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let server_count = request_count.clone();
    let server = std::thread::spawn(move || {
        fn respond(stream: &mut std::net::TcpStream) {
            let body = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
                body.len(),
                body
            );
            let _ = std::io::Write::write_all(stream, response.as_bytes());
        }

        let (mut stream, _) = listener.accept().expect("accept first request");
        server_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(200)));
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        std::thread::sleep(std::time::Duration::from_millis(200));
        respond(&mut stream);

        listener
            .set_nonblocking(true)
            .expect("nonblocking listener");
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        while std::time::Instant::now() < deadline {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    server_count.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(50)));
                    let mut request = [0u8; 4096];
                    let _ = stream.read(&mut request);
                    respond(&mut stream);
                    break;
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                }
                Err(_) => break,
            }
        }
    });

    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::OpenAi;
    agent.provider_id = "local".to_string();
    agent.provider_requires_api_key = false;
    agent.api_key.clear();
    agent.base_url = format!("http://{addr}");
    agent.model = "qwen2.5-coder-7b".to_string();
    let (runtime_control_tx, runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
    agent.install_runtime_controls(runtime_control_rx, runtime_control_tx.clone());

    let control_count = request_count.clone();
    let control = tokio::spawn(async move {
        for _ in 0..50 {
            if control_count.load(std::sync::atomic::Ordering::SeqCst) > 0 {
                let _ = runtime_control_tx.send("/effort status".to_string());
                return;
            }
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }
    });

    agent
        .chat("hello".to_string())
        .await
        .expect("chat completes");
    control.await.expect("control task");
    server.join().expect("server thread");
    assert_eq!(
        request_count.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "non-aborting runtime controls must not drop and restart the pending HTTP request"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn steering_delivery_line_includes_preview_and_queue_status() {
    let text = crate::tui::steering_delivered_text_for_test(1, "fix rg border overflow", 80);
    let rendered = text
        .lines
        .iter()
        .flat_map(|line| line.spans.iter())
        .map(|span| span.content.as_ref())
        .collect::<String>();
    assert!(rendered.contains("queued for next response"), "{rendered}");
    assert!(rendered.contains("fix rg border overflow"), "{rendered}");
}

#[test]
fn steering_acknowledgement_detects_final_that_mentions_steered_scope() {
    let item = "queued during active turn (1 message): fix the rg border overflow and tell me what happened";
    let missing = vec![Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "Done.".to_string(),
        }],
    }];
    assert!(!steering_item_acknowledged(item, &missing));

    let acknowledged = vec![
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "I fixed the rg border overflow in an earlier turn.".to_string(),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "[queued-user-update] queued during active turn (1 message): fix the rg border overflow and tell me what happened".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "I fixed the rg border overflow and explained what happened.".to_string(),
            }],
        },
    ];
    assert!(steering_item_acknowledged(item, &acknowledged));

    let stale_ack_only = vec![
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "I fixed the rg border overflow before any queued update.".to_string(),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "[queued-user-update] queued during active turn (1 message): fix the rg border overflow and tell me what happened".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "Done.".to_string(),
            }],
        },
    ];
    assert!(!steering_item_acknowledged(item, &stale_ack_only));

    let web_recipe_item = "queued during active turn (1 message): i can give you a web recipe";
    let web_recipe_acknowledged = vec![Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "You mentioned a web recipe; please share it and I will use it next.".to_string(),
        }],
    }];
    assert!(steering_item_acknowledged(
        web_recipe_item,
        &web_recipe_acknowledged
    ));
}

#[test]
fn queued_steering_done_only_after_acknowledged_final() {
    let root = temp_test_dir("steering-done-after-ack");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent
        .work_ledger
        .steering
        .push("queued during active turn (1 message): add a space between thinking history and next tool call".to_string());
    agent
        .work_ledger
        .pending
        .push("respond to queued user update".to_string());
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "Done.".to_string(),
        }],
    });

    let unresolved = agent
        .work_ledger
        .steering
        .iter()
        .filter(|item| !steering_item_acknowledged(item, &agent.history))
        .cloned()
        .collect::<Vec<_>>();
    assert_eq!(unresolved.len(), 1);
    assert!(
        agent
            .work_ledger
            .pending
            .contains(&"respond to queued user update".to_string())
    );

    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "I addressed your steering by adding a space between thinking history and the next tool call.".to_string(),
        }],
    });
    let unresolved = agent
        .work_ledger
        .steering
        .iter()
        .filter(|item| !steering_item_acknowledged(item, &agent.history))
        .cloned()
        .collect::<Vec<_>>();
    assert!(unresolved.is_empty());
    agent.mark_work_done("respond to queued user update");
    assert!(
        agent
            .work_ledger
            .done
            .contains(&"respond to queued user update".to_string())
    );
    assert!(
        !agent
            .work_ledger
            .pending
            .contains(&"respond to queued user update".to_string())
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn queued_steering_reopens_pending_after_prior_ack_and_dedupes_entry() {
    let root = temp_test_dir("steering-reopens-pending");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent
        .work_ledger
        .done
        .push("respond to queued user update".to_string());

    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "I addressed the old queued change.".to_string(),
        }],
    });
    let old_messages = vec!["old queued change".to_string()];
    agent.note_steering_messages(&old_messages);
    assert_eq!(agent.work_ledger.steering.len(), 1);
    agent.mark_work_done("respond to queued user update");

    let messages = vec!["please revisit the queued change".to_string()];
    agent.note_steering_messages(&messages);
    agent.note_steering_messages(&messages);

    assert_eq!(agent.work_ledger.steering.len(), 1);
    assert!(
        !agent
            .work_ledger
            .done
            .contains(&"respond to queued user update".to_string())
    );
    assert!(
        agent
            .work_ledger
            .pending
            .contains(&"respond to queued user update".to_string())
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn normalize_login_secret_extracts_access_token_from_multiline_json() {
    let raw = "{\n  \"accessToken\": \"abc123xyz456\",\n  \"expires\": \"later\"\n}";
    assert_eq!(normalize_login_secret(raw).as_deref(), Some("abc123xyz456"));
}

#[test]
fn looks_like_login_secret_input_accepts_multiline_json() {
    let raw = "{\n  \"accessToken\": \"abc123xyz456\",\n  \"expires\": \"later\"\n}";
    assert!(looks_like_login_secret_input(raw));
}

fn build_test_work_map() -> WorkMap {
    let header = SessionHeader {
        model: "test-model".to_string(),
        provenance: SessionProvenance {
            provider: "test".to_string(),
            ..Default::default()
        },
        work_ledger: WorkLedger {
            objective: "fix failing tests".to_string(),
            decisions: vec!["keep map non-tree".to_string()],
            ..Default::default()
        },
        ..Default::default()
    };
    let history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "Fix failing tests".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_write".to_string(),
                name: "write_file".to_string(),
                input: json!({"path":"src/lib.rs","content":"x"}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::ToolResult {
                tool_use_id: "call_write".to_string(),
                content: "wrote file".to_string(),
                is_error: None,
                metadata: ToolResultMetadata::default(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_test".to_string(),
                name: "bash".to_string(),
                input: json!({"command":"cargo test --release"}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::ToolResult {
                tool_use_id: "call_test".to_string(),
                content: "exit: 1\nfailed".to_string(),
                is_error: Some(true),
                metadata: ToolResultMetadata {
                    status: Some("failed".to_string()),
                    exit_code: Some(1),
                    duration_ms: Some(10),
                    artifact: Some("artifact.json".to_string()),
                },
            }],
        },
    ];
    build_session_work_map(Path::new("test-session.jsonl"), &header, &history)
}

#[test]
fn work_map_derives_waypoints_and_packets() {
    let map = build_test_work_map();
    let rendered = render_work_map(&map, &[]);
    assert!(rendered.contains("Work map"), "{rendered}");
    assert!(rendered.contains("@w"), "{rendered}");
    assert!(
        map.waypoints
            .iter()
            .any(|wp| wp.kind == WorkMapKind::Change)
    );
    assert!(
        map.waypoints
            .iter()
            .any(|wp| wp.kind == WorkMapKind::Failure)
    );
    let failure = map
        .waypoints
        .iter()
        .find(|wp| wp.kind == WorkMapKind::Failure)
        .expect("failure waypoint");
    let selection = parse_work_map_selection(&failure.id, &map).expect("select waypoint");
    let packet = render_work_map_packet(&map, &selection);
    assert!(packet.contains("[dext packet"), "{packet}");
    assert!(packet.contains("Failures/blockers"), "{packet}");
    assert!(
        packet.contains("Focus changes model context only"),
        "{packet}"
    );
}

#[test]
fn work_map_focus_includes_safety_and_exact_mode() {
    let map = build_test_work_map();
    let selection = parse_work_map_selection("@w01", &map).expect("select waypoint");
    let focus = render_work_map_focus(&map, &selection, &FocusMode::Exact);
    assert!(focus.contains("mode=exact"), "{focus}");
    assert!(focus.contains("does not rewind files"), "{focus}");
}

#[test]
fn work_map_args_accept_waypoint_before_or_after_selector() -> Result<()> {
    let args = parse_work_map_command_args("@w02 latest --exact");
    let (id, selector, mode_args) = parse_work_map_operation_args(&args, "current")?;
    assert_eq!(id, "@w02");
    assert_eq!(selector, "latest");
    assert_eq!(mode_args, vec!["--exact".to_string()]);

    let args = parse_work_map_command_args("@w02 --carry=failures,files latest");
    let (id, selector, mode_args) = parse_work_map_operation_args(&args, "current")?;
    assert_eq!(id, "@w02");
    assert_eq!(selector, "latest");
    assert_eq!(mode_args, vec!["--carry=failures,files".to_string()]);

    let args = parse_work_map_command_args("latest @w03 exact");
    let (id, selector, mode_args) = parse_work_map_operation_args(&args, "current")?;
    assert_eq!(id, "@w03");
    assert_eq!(selector, "latest");
    assert_eq!(mode_args, vec!["exact".to_string()]);

    let args = parse_work_map_command_args("latest @w04 track-name --exact");
    let (id, selector, name, mode_args) = parse_track_open_args(&args, "current")?;
    assert_eq!(id, "@w04");
    assert_eq!(selector, "latest");
    assert_eq!(name, Some("track-name"));
    assert_eq!(mode_args, vec!["--exact".to_string()]);
    Ok(())
}

#[test]
fn work_map_filters_multiple_kinds_as_union_and_narrow_by_query() -> Result<()> {
    let map = build_test_work_map();
    let args = parse_work_map_command_args("changes failures");
    let (selector, filters) = parse_work_map_filter_args(&args)?;
    assert_eq!(selector, "current");

    let visible = map
        .waypoints
        .iter()
        .filter(|wp| work_map_filter_matches(&map, wp, &filters))
        .collect::<Vec<_>>();
    assert!(
        visible.iter().any(|wp| wp.kind == WorkMapKind::Change),
        "{visible:?}"
    );
    assert!(
        visible.iter().any(|wp| wp.kind == WorkMapKind::Failure),
        "{visible:?}"
    );
    assert!(
        visible
            .iter()
            .all(|wp| matches!(wp.kind, WorkMapKind::Change | WorkMapKind::Failure)),
        "{visible:?}"
    );
    let rendered = render_work_map(&map, &filters);
    assert!(rendered.contains("filter change,failure"), "{rendered}");

    let args = parse_work_map_command_args("failures query cargo");
    let (_, filters) = parse_work_map_filter_args(&args)?;
    let visible = map
        .waypoints
        .iter()
        .filter(|wp| work_map_filter_matches(&map, wp, &filters))
        .collect::<Vec<_>>();
    assert!(!visible.is_empty(), "expected cargo failure waypoint");
    assert!(
        visible.iter().all(|wp| wp.kind == WorkMapKind::Failure),
        "{visible:?}"
    );
    Ok(())
}

#[test]
fn work_map_active_focus_limits_future_provider_context() -> Result<()> {
    let root = temp_test_dir("work-map-active-focus");
    let mut agent = test_agent(&root);
    agent.suppress_checkpoints = true;
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "older request outside focus".to_string(),
        }],
    });
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "older answer outside focus".to_string(),
        }],
    });

    let map = build_test_work_map();
    let selection = parse_work_map_selection("@w01", &map)?;
    let focus = activate_work_map_focus(&mut agent, &map, &selection, &FocusMode::Exact);
    assert!(focus.contains("mode=exact"), "{focus}");
    assert!(agent.work_ledger.active_focus.is_some());
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "new request inside focus".to_string(),
        }],
    });

    let context = agent.provider_context_history();
    assert_eq!(context.len(), 2);
    let first = match &context[0].content[0] {
        Block::Text { text } => text,
        other => panic!("unexpected focus context block: {other:?}"),
    };
    assert!(first.starts_with("[dext focus packet loaded]"), "{first}");
    let rendered = agent
        .history_to_chatgpt_input()
        .into_iter()
        .map(|item| item.to_string())
        .collect::<String>();
    assert!(
        !rendered.contains("older request outside focus"),
        "{rendered}"
    );
    assert!(rendered.contains("new request inside focus"), "{rendered}");
    Ok(())
}

#[test]
fn track_origin_serializes_in_session_header() -> Result<()> {
    let root = temp_test_dir("work-map-track-origin");
    let _guard = env_lock();
    let sessions_dir = root.join("sessions");
    std::fs::create_dir_all(&sessions_dir)?;
    unsafe { std::env::set_var("DEXT_SESSIONS_DIR", &sessions_dir) };
    let result = (|| -> Result<()> {
        let agent = test_agent(&root);
        let map = build_test_work_map();
        let selection = parse_work_map_selection("@w01", &map)?;
        let path = create_track_from_work_map(
            &agent,
            &map,
            &selection,
            Some("work-map-track"),
            &FocusMode::Exact,
        )?;
        let (header, history) = read_session_jsonl(&path)?;
        let origin = header.track_origin.expect("track origin");
        assert_eq!(origin.source_waypoint, "@w01");
        assert_eq!(origin.mode, "exact");
        assert_eq!(history.len(), 2);
        Ok(())
    })();
    unsafe { std::env::remove_var("DEXT_SESSIONS_DIR") };
    result
}

#[test]
fn session_replay_fixture_circuit_breaker_stops_retrying_blocked_host() -> Result<()> {
    let replay = SessionReplayFixture::load("circuit_breaker")?;
    assert_eq!(replay.header.version, 2);
    assert_eq!(replay.header.model, "fixture-model");
    assert_eq!(replay.tool_call_count("bash"), 2);

    let breaker_hits = replay.tool_results_containing("[circuit-breaker]");
    assert_eq!(breaker_hits.len(), 1, "{:?}", breaker_hits);
    assert!(
        replay
            .tool_names_after_marker("[circuit-breaker]")
            .is_empty(),
        "no more tool calls should happen after the breaker trips"
    );
    assert!(
        replay.assistant_text().contains("need credentials"),
        "{}",
        replay.assistant_text()
    );
    Ok(())
}

#[test]
fn session_replay_fixture_feasibility_gate_requires_probe_before_scale() -> Result<()> {
    let replay = SessionReplayFixture::load("feasibility_gate")?;
    let gate_hits = replay.tool_results_containing("source feasibility gate");
    assert_eq!(gate_hits.len(), 1, "{:?}", gate_hits);
    let gate_text = &gate_hits[0].0;
    assert!(
        gate_text.contains("Bulk external collection is blocked"),
        "{}",
        gate_text
    );
    assert!(
        gate_text.contains("retry the original bulk request"),
        "{}",
        gate_text
    );

    let probe_inputs = replay.tool_inputs_after_marker("source feasibility gate", "bash");
    assert!(
        probe_inputs.iter().any(|input| input.contains("/items/1")),
        "expected a single-item probe after the gate, got {:?}",
        probe_inputs
    );
    assert!(
        replay.assistant_text().contains("probe succeeded"),
        "{}",
        replay.assistant_text()
    );
    Ok(())
}

#[test]
fn session_replay_fixture_partial_delivery_hint_avoids_write_fallback() -> Result<()> {
    let replay = SessionReplayFixture::load("partial_delivery_hint")?;
    let hint = orchestrator::partial_delivery_hint();
    assert_eq!(replay.text_blocks_containing(hint).len(), 1);

    let later_tools = replay.tool_names_after_marker(hint);
    assert!(
        !later_tools
            .iter()
            .any(|name| { matches!(name.as_str(), "write_file" | "edit_file" | "multi_edit") }),
        "partial-delivery hint should prevent a weak file-write fallback, got {:?}",
        later_tools
    );
    let assistant = replay.assistant_text();
    assert!(assistant.contains("Partial deliverable"), "{assistant}");
    assert!(assistant.contains("credentials"), "{assistant}");
    Ok(())
}

#[test]
fn session_replay_fixture_dedupe_cache_preserves_success_and_error_hits() -> Result<()> {
    let replay = SessionReplayFixture::load("dedupe_cache")?;
    assert_eq!(replay.tool_call_count("bash"), 4);

    let hits = replay.tool_results_containing("request dedupe cache hit");
    assert_eq!(hits.len(), 2, "{:?}", hits);
    assert!(
        hits.iter().any(|(_, is_error)| is_error.is_none()),
        "expected a cached success hit: {:?}",
        hits
    );
    assert!(
        hits.iter().any(|(_, is_error)| *is_error == Some(true)),
        "expected a cached error hit: {:?}",
        hits
    );
    assert!(
        replay.assistant_text().contains("deduped"),
        "{}",
        replay.assistant_text()
    );
    Ok(())
}

#[test]
fn session_replay_fixture_ignores_irrelevant_subagent_and_handles_queued_update() -> Result<()> {
    let replay = SessionReplayFixture::load("irrelevant_subagent_queued_update")?;
    assert!(
        !replay
            .header
            .exposed_tools
            .contains(&"subagent".to_string())
    );
    assert_eq!(replay.tool_call_count("subagent"), 0);
    assert_eq!(replay.tool_call_count("rg"), 1);
    assert_eq!(replay.tool_call_count("read_file"), 1);
    assert!(
        replay.tool_names_after_marker("wealthtrak subagent is irrelevant")
            == vec!["read_file".to_string()],
        "expected Dext-source work after queued update, got {:?}",
        replay.tool_names_after_marker("wealthtrak subagent is irrelevant")
    );
    let assistant = replay.assistant_text();
    assert!(
        assistant.contains("ignored the detached wealthtrak subagent"),
        "{assistant}"
    );
    assert!(
        assistant.contains("provider-visible tools exclude subagent"),
        "{assistant}"
    );
    Ok(())
}

#[test]
fn latest_session_roundtrip_restores_history_usage_and_sandbox() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("resume-roundtrip");
    let sessions_dir = root.join("sessions");
    let sandbox = root.join("sandbox");
    let other = root.join("other");
    std::fs::create_dir_all(&sandbox)?;
    std::fs::create_dir_all(&other)?;

    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::set_var("DEXT_SESSIONS_DIR", &sessions_dir) };
    let result = (|| -> Result<()> {
        let sandbox = std::fs::canonicalize(&sandbox)?;

        let mut saved = test_agent(&sandbox);
        saved.model = "saved-model".to_string();
        saved.thinking_effort = ThinkingEffort::XHigh;
        saved.context_mode = ContextMode::Standard;
        saved.tool_context_profile = ToolContextProfile::Full;
        saved.tool_profile = ToolProfile::Full;
        saved.refresh_tools_for_context();
        saved.system = "saved-system".to_string();
        saved.allowed.insert("read_file".to_string());
        saved.allowed.insert("write_file".to_string());
        saved.work_ledger.objective = "preserve session metadata".to_string();
        saved.work_ledger.current_phase = "synthesize".to_string();
        saved
            .work_ledger
            .done
            .push("run verification checks".to_string());
        saved
            .work_ledger
            .next_actions
            .push("deliver requested outcome with verifiable steps".to_string());
        saved
            .work_ledger
            .files_changed
            .push("src/main.rs".to_string());
        saved
            .work_ledger
            .files_changed
            .push("/tmp/scratch.py".to_string());
        saved.work_ledger.verification.push(VerificationRecord {
            name: "cargo test focused".to_string(),
            command: "cargo test focused".to_string(),
            status: "passed".to_string(),
            exit_code: Some(0),
            duration_ms: 123,
            artifact: Some(".dext/artifacts/verify.json".to_string()),
            validates: vec!["session roundtrip".to_string()],
        });
        saved.provider_health.providers.insert(
            "chatgpt".to_string(),
            ProviderHealthState {
                auth: "present".to_string(),
                last_error: Some("HTTP 429".to_string()),
                retry_after: Some(10),
                mode: Some("chatgpt-responses".to_string()),
                disabled_for_turn: true,
                consecutive_server_errors: 0,
            },
        );
        saved.session_usage = Usage {
            input: 11,
            output: 7,
            cache_create: 3,
            cache_read: 5,
            cost_usd: None,
        };
        saved.history = vec![
            Message {
                role: "user".to_string(),
                content: vec![Block::Text {
                    text: "hello from disk".to_string(),
                }],
            },
            Message {
                role: "assistant".to_string(),
                content: vec![Block::Text {
                    text: "saved reply".to_string(),
                }],
            },
        ];
        saved.save_latest_session()?;
        let saved_header_line = std::fs::read_to_string(latest_session_path(&sandbox))?
            .lines()
            .next()
            .unwrap_or_default()
            .to_string();
        assert!(
            saved_header_line.contains("\"work_ledger\""),
            "{saved_header_line}"
        );
        assert!(
            saved_header_line.contains("\"exposed_tools\""),
            "{saved_header_line}"
        );
        assert!(
            saved_header_line.contains("\"approval_required_tools\""),
            "{saved_header_line}"
        );
        assert!(
            saved_header_line.contains("\"auto_approved_tools\""),
            "{saved_header_line}"
        );
        assert!(
            saved_header_line.contains("\"provider_health\""),
            "{saved_header_line}"
        );
        assert!(
            saved_header_line.contains("\"provenance\""),
            "{saved_header_line}"
        );
        assert!(
            saved_header_line.contains("\"tool_context_profile\":\"full\""),
            "{saved_header_line}"
        );

        let saved_header: SessionHeader = serde_json::from_str(&saved_header_line)?;
        assert_eq!(saved_header.system, "saved-system");
        assert!(
            saved_header
                .composed_system
                .as_deref()
                .unwrap_or_default()
                .contains("saved-system"),
            "{:?}",
            saved_header.composed_system
        );
        assert!(
            saved_header
                .exposed_tools
                .contains(&"read_file".to_string())
        );
        assert!(saved_header.exposed_tools.contains(&"rg".to_string()));
        assert!(
            saved_header
                .approval_required_tools
                .contains(&"write_file".to_string())
        );
        assert!(
            saved_header
                .auto_approved_tools
                .contains(&"read_file".to_string())
        );
        assert!(saved_header.auto_approved_tools.contains(&"rg".to_string()));
        assert!(
            saved_header
                .auto_approved_tools
                .contains(&"write_file".to_string())
        );
        assert_eq!(saved_header.context_mode, ContextMode::Standard);
        assert_eq!(saved_header.tool_context_profile, ToolContextProfile::Full);
        assert_eq!(saved_header.tool_profile, ToolProfile::Full);
        assert!(saved_header.exposed_tools.contains(&"jq".to_string()));
        assert_eq!(saved_header.work_ledger.current_phase, "done");
        assert!(saved_header.work_ledger.next_actions.is_empty());
        assert_eq!(
            saved_header.work_ledger.files_changed,
            vec!["src/main.rs".to_string()]
        );

        let mut loaded = test_agent(&other);
        loaded.model = "other-model".to_string();
        loaded.system = "other-system".to_string();
        loaded.load_latest_session()?;

        assert_eq!(loaded.model, "saved-model");
        assert_eq!(loaded.thinking_effort, ThinkingEffort::XHigh);
        assert_eq!(loaded.context_mode, ContextMode::Standard);
        assert_eq!(loaded.tool_context_profile(), ToolContextProfile::Full);
        assert_eq!(loaded.tool_profile, ToolProfile::Full);
        assert!(loaded.tools.iter().any(|t| t.name == "jq"));
        assert_eq!(loaded.system, "saved-system");
        assert_eq!(loaded.sandbox_root, sandbox);
        assert_eq!(loaded.session_usage.input, 11);
        assert_eq!(loaded.session_usage.output, 7);
        assert!(loaded.allowed.contains("read_file"));
        assert!(loaded.allowed.contains("write_file"));
        assert_eq!(loaded.work_ledger.objective, "preserve session metadata");
        assert_eq!(loaded.work_ledger.verification[0].status, "passed");
        assert!(
            loaded
                .work_ledger
                .files_changed
                .contains(&"src/main.rs".to_string())
        );
        assert!(
            !loaded
                .work_ledger
                .files_changed
                .contains(&"/tmp/scratch.py".to_string())
        );
        let header = saved.session_header();
        assert_eq!(
            loaded.provider_health.providers["chatgpt"].retry_after,
            Some(10)
        );
        assert_eq!(header.version, SESSION_FORMAT_VERSION);
        assert!(header.composed_system.is_some());
        assert_eq!(header.provenance.thinking_effort, ThinkingEffort::XHigh);
        assert_eq!(header.provenance.tool_catalog_version, TOOL_CATALOG_VERSION);
        assert!(!header.provenance.system_prompt_hash.is_empty());
        assert_eq!(loaded.history.len(), 2);
        assert_eq!(assistant_text(&loaded.history), "saved reply");
        Ok(())
    })();
    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::remove_var("DEXT_SESSIONS_DIR") };
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn session_html_export_renders_and_escapes_transcript() -> Result<()> {
    let root = temp_test_dir("session-html-export");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "hello <script>alert(1)</script> & bye".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![
                Block::ToolUse {
                    id: "tool-1".to_string(),
                    name: "bash".to_string(),
                    input: json!({"command":"echo '<ok>'"}),
                },
                Block::Text {
                    text: "done".to_string(),
                },
            ],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("tool-1", "line <ok> & done", Some(false))],
        },
    ];

    std::fs::write(root.join("DEXT.md"), "## Local\n- html export context")?;

    let out = root.join("session.html");
    agent.export_session_html_to_path(&out)?;
    let html = std::fs::read_to_string(&out)?;

    assert!(
        html.contains("hello &lt;script&gt;alert(1)&lt;/script&gt; &amp; bye"),
        "{html}"
    );
    assert!(!html.contains("<script>alert(1)</script>"), "{html}");
    assert!(html.contains("tool_use bash"), "{html}");
    assert!(html.contains("tool_result"), "{html}");
    assert!(html.contains("line &lt;ok&gt; &amp; done"), "{html}");
    assert!(html.contains("system prompt"), "{html}");
    assert!(html.contains("Project context (DEXT.md"), "{html}");
    assert!(html.contains("html export context"), "{html}");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn sessions_listing_includes_project_latest_without_named_sessions() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("session-listing-latest");
    let dext_home = root.join("dext-home");
    let project = root.join("project");
    std::fs::create_dir_all(&dext_home)?;
    std::fs::create_dir_all(&project)?;

    unsafe {
        std::env::set_var("DEXT_HOME", &dext_home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }
    let result = (|| -> Result<()> {
        let project = std::fs::canonicalize(&project)?;
        let mut agent = test_agent(&project);
        agent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "hello latest".to_string(),
            }],
        });
        agent.save_latest_session()?;

        let listing = render_session_listing(&project);
        assert!(listing.contains("project latest:"), "{listing}");
        assert!(listing.contains("latest: 1 messages"), "{listing}");
        assert!(listing.contains("named sessions:"), "{listing}");
        let project_named_dir = named_sessions_dir_for_root(&project);
        assert!(
            listing.contains(&format!("none in {}", project_named_dir.display())),
            "{listing}"
        );
        assert!(listing.contains("use /save <name>"), "{listing}");
        assert!(!listing.contains("(no sessions"), "{listing}");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn named_sessions_are_project_scoped_by_default() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("project-scoped-named-sessions");
    let dext_home = root.join("dext-home");
    let alpha = root.join("alpha");
    let beta = root.join("beta");
    std::fs::create_dir_all(&dext_home)?;
    std::fs::create_dir_all(&alpha)?;
    std::fs::create_dir_all(&beta)?;

    unsafe {
        std::env::set_var("DEXT_HOME", &dext_home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }
    let result = (|| -> Result<()> {
        let alpha = std::fs::canonicalize(&alpha)?;
        let beta = std::fs::canonicalize(&beta)?;
        let mut alpha_agent = test_agent(&alpha);
        alpha_agent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "alpha".to_string(),
            }],
        });
        let alpha_path = alpha_agent.save_session("shared")?;

        let mut beta_agent = test_agent(&beta);
        beta_agent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "beta".to_string(),
            }],
        });
        let beta_path = beta_agent.save_session("shared")?;

        assert_ne!(alpha_path, beta_path);
        assert_eq!(resolve_session_selector(&alpha, "shared")?, alpha_path);
        assert_eq!(resolve_session_selector(&beta, "shared")?, beta_path);
        assert!(render_session_listing(&alpha).contains(&alpha_path.display().to_string()));
        assert!(render_session_listing(&beta).contains(&beta_path.display().to_string()));
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn session_export_target_parses_html_and_jsonl() {
    let (format, path) = parse_session_export_target("html report.html");
    assert_eq!(format, SessionExportFormat::Html);
    assert_eq!(path, PathBuf::from("report.html"));

    let (format, path) = parse_session_export_target("report.html");
    assert_eq!(format, SessionExportFormat::Html);
    assert_eq!(path, PathBuf::from("report.html"));

    let (format, path) = parse_session_export_target("archive.jsonl");
    assert_eq!(format, SessionExportFormat::Jsonl);
    assert_eq!(path, PathBuf::from("archive.jsonl"));
}

#[test]
fn session_jsonl_reads_tool_metadata_duration() -> Result<()> {
    let root = temp_test_dir("session-jsonl-metadata-duration");
    let path = root.join("session.jsonl");
    let header = SessionHeader {
        model: "test-model".to_string(),
        ..SessionHeader::default()
    };
    let message = Message {
        role: "user".to_string(),
        content: vec![Block::ToolResult {
            tool_use_id: "call_1".to_string(),
            content: "ok".to_string(),
            is_error: None,
            metadata: ToolResultMetadata {
                status: Some("ok".to_string()),
                exit_code: Some(0),
                duration_ms: Some(25),
                artifact: None,
            },
        }],
    };
    let data = format!(
        "{}\n{}\n",
        serde_json::to_string(&header)?,
        serde_json::to_string(&message)?
    );
    std::fs::write(&path, data)?;

    let (_header, history) = read_session_jsonl(&path)?;
    let Block::ToolResult { metadata, .. } = &history[0].content[0] else {
        panic!("expected tool result")
    };
    assert_eq!(metadata.duration_ms, Some(25));

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn latest_state_defaults_are_project_scoped() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("project-scoped-state");
    let dext_home = root.join("dext-home");
    let alpha = root.join("alpha");
    let beta = root.join("beta");
    std::fs::create_dir_all(&alpha)?;
    std::fs::create_dir_all(&beta)?;

    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::set_var("DEXT_HOME", &dext_home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let result = (|| -> Result<()> {
        let alpha = std::fs::canonicalize(&alpha)?;
        let beta = std::fs::canonicalize(&beta)?;
        let alpha_session = latest_session_path(&alpha);
        let beta_session = latest_session_path(&beta);
        let alpha_log = latest_log_path(&alpha);
        let beta_log = latest_log_path(&beta);

        assert_ne!(alpha_session, beta_session);
        assert_ne!(alpha_log, beta_log);
        assert!(alpha_session.starts_with(dext_home.join("projects")));
        assert!(alpha_log.starts_with(dext_home.join("projects")));
        Ok(())
    })();

    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn project_state_lock_blocks_second_owner_until_release() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("state-lock-owner");
    let root = std::fs::canonicalize(&root)?;
    let first = ProjectStateLock::acquire(&root)?;
    let err = ProjectStateLock::acquire(&root).expect_err("second owner should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("another dext process already owns project state"),
        "{msg}"
    );
    drop(first);
    let second = ProjectStateLock::acquire(&root)?;
    drop(second);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
#[cfg(target_os = "linux")]
fn process_exists_treats_zombies_as_dead_for_stale_lock_recovery() -> Result<()> {
    let mut child = Command::new("sh").arg("-c").arg("exit 0").spawn()?;
    let pid = child.id();
    for _ in 0..100 {
        if std::fs::read_to_string(format!("/proc/{pid}/stat"))
            .ok()
            .and_then(|stat| stat.rsplit_once(") ").map(|(_, rest)| rest.to_string()))
            .and_then(|rest| rest.split_whitespace().next().map(str::to_string))
            .is_some_and(|state| state == "Z")
        {
            assert!(!process_exists(pid));
            let _ = child.wait();
            return Ok(());
        }
        std::thread::sleep(std::time::Duration::from_millis(10));
    }
    let _ = child.wait();
    anyhow::bail!("child did not become zombie before wait")
}

#[test]
fn set_sandbox_root_rejects_locked_project() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("sandbox-lock-switch");
    let alpha = std::fs::canonicalize(
        std::fs::create_dir_all(root.join("alpha")).map(|_| root.join("alpha"))?,
    )?;
    let beta = std::fs::canonicalize(
        std::fs::create_dir_all(root.join("beta")).map(|_| root.join("beta"))?,
    )?;

    let mut agent = test_agent(&alpha);
    let held = ProjectStateLock::acquire(&beta)?;
    let before = agent.sandbox_root.clone();
    let err = agent
        .set_sandbox_root(beta.clone())
        .expect_err("locked target project should fail");
    let msg = format!("{err:#}");
    assert!(
        msg.contains("another dext process already owns project state"),
        "{msg}"
    );
    assert_eq!(agent.sandbox_root, before);
    drop(held);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn session_names_reject_path_traversal() {
    assert!(validate_session_name("../escape").is_err());
    assert!(validate_session_name("..\\escape").is_err());
    assert!(validate_session_name("safe-name_01").is_ok());
}

#[test]
fn builtin_parallel_policy_only_allows_read_only_rounds() {
    assert!(should_parallelize_builtin_tools(&["read_file", "rg", "fd"]));
    assert!(!should_parallelize_builtin_tools(&[
        "read_file",
        "write_file"
    ]));
    assert!(!should_parallelize_builtin_tools(&["bash"]));
    assert!(!should_parallelize_builtin_tools(&["http", "rg"]));
    assert!(!should_parallelize_builtin_tools(&[]));
}

#[test]
fn trust_mode_toggle_controls_gated_allowlist() {
    let root = temp_test_dir("trust-toggle");
    let mut agent = test_agent(&root);
    assert!(!agent.trust_mode_active());

    let enabled = agent.set_trust_mode(true);
    assert!(enabled > 0, "expected gated tools to be added");
    assert!(agent.trust_mode_active());
    assert_eq!(agent.approval_profile(), ApprovalProfile::Always);

    let disabled = agent.set_trust_mode(false);
    assert!(disabled > 0, "expected gated tools to be removed");
    assert!(!agent.trust_mode_active());
    assert_eq!(agent.approval_profile(), ApprovalProfile::Ask);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn slash_trust_emits_profile_update_for_tui_status() {
    let root = temp_test_dir("trust-slash-status");
    let mut agent = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    assert_eq!(handle_slash("/trust on", &mut agent), Some(true));
    let mut saw_profile = false;
    while let Ok(event) = rx.try_recv() {
        if let AgentEvent::ApprovalProfileChanged { profile } = event {
            saw_profile = true;
            assert_eq!(profile, ApprovalProfile::Always);
        }
    }
    assert!(
        saw_profile,
        "expected ApprovalProfileChanged after /trust on"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lsp_diagnostics_parser_extracts_publish_diagnostics() {
    let root = temp_test_dir("lsp-diagnostics-parser");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let uri = format!("file://{}/src/lib.rs", root.display());
    let line = json!({
        "method": "textDocument/publishDiagnostics",
        "params": {
            "uri": uri,
            "diagnostics": [{
                "range": {"start": {"line": 2, "character": 4}},
                "severity": 1,
                "code": "E0425",
                "message": "cannot find value `x` in this scope\nextra detail"
            }]
        }
    })
    .to_string();

    let diagnostics = parse_lsp_diagnostics_from_json_lines(&line, &root);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].file, "src/lib.rs");
    assert_eq!(diagnostics[0].line, Some(3));
    assert_eq!(diagnostics[0].character, Some(5));
    assert_eq!(diagnostics[0].severity, "error");
    assert_eq!(diagnostics[0].code.as_deref(), Some("E0425"));
    assert_eq!(
        diagnostics[0].message,
        "cannot find value `x` in this scope"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cargo_json_diagnostics_parser_extracts_primary_span() {
    let root = temp_test_dir("cargo-diagnostics-parser");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let line = json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "mismatched types",
            "code": {"code": "E0308"},
            "spans": [{
                "file_name": "src/main.rs",
                "line_start": 10,
                "column_start": 7,
                "is_primary": true
            }]
        }
    })
    .to_string();

    let diagnostics = parse_cargo_json_diagnostics(&line, &root);
    assert_eq!(diagnostics.len(), 1);
    assert_eq!(diagnostics[0].file, "src/main.rs");
    assert_eq!(diagnostics[0].line, Some(10));
    assert_eq!(diagnostics[0].character, Some(7));
    assert_eq!(diagnostics[0].severity, "error");
    assert_eq!(diagnostics[0].code.as_deref(), Some("E0308"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn cargo_json_diagnostics_summary_ranks_and_dedupes() {
    let root = temp_test_dir("cargo-diagnostics-summary");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let warning = json!({
        "reason": "compiler-message",
        "message": {
            "level": "warning",
            "message": "unused variable",
            "code": {"code": "unused_variables"},
            "spans": [{
                "file_name": "src/lib.rs",
                "line_start": 20,
                "column_start": 9,
                "is_primary": true
            }]
        }
    })
    .to_string();
    let error = json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "mismatched types",
            "code": {"code": "E0308"},
            "spans": [{
                "file_name": "src/main.rs",
                "line_start": 10,
                "column_start": 7,
                "is_primary": true
            }]
        }
    })
    .to_string();
    let output = format!("{warning}\n{error}\n{error}\n");

    let summary = render_cargo_json_diagnostics_summary(&output, &root).expect("summary");
    assert!(summary.contains("errors=1"), "{summary}");
    assert!(summary.contains("warnings=1"), "{summary}");
    assert!(summary.contains("total=2"), "{summary}");
    assert!(
        summary.find("- error [E0308]").unwrap()
            < summary.find("- warning [unused_variables]").unwrap(),
        "{summary}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sync_work_ledger_keeps_only_unresolved_objective_checkpoints_pending() {
    let root = temp_test_dir("ledger-objective-sync");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let objective = orchestrator::ObjectiveTracker::from_user_prompt("Implement it and test it");
    agent.update_work_ledger_from_objective(&objective);
    agent
        .work_ledger
        .done
        .push("run verification checks".to_string());
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call-edit".to_string(),
            name: "edit_file".to_string(),
            input: json!({"path": "src/lib.rs", "old_string": "a", "new_string": "b"}),
        }],
    });

    let coverage = objective.assess_history(&agent.history);
    agent.sync_work_ledger_with_objective_coverage(&coverage);

    assert!(
        agent
            .work_ledger
            .done
            .contains(&"implement requested changes".to_string())
    );
    assert!(
        agent
            .work_ledger
            .pending
            .contains(&"run verification checks".to_string())
    );
    assert!(
        !agent
            .work_ledger
            .done
            .contains(&"run verification checks".to_string())
    );
    assert!(
        !agent
            .work_ledger
            .pending
            .contains(&"implement requested changes".to_string())
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn action_contract_violation_notes_loop_and_fallback_after_repeated_no_mutation() {
    let root = temp_test_dir("action-contract-noop-notes");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.provider_id = "chatgpt".to_string();
    agent.model = "gpt-5.3-codex".to_string();
    let mut fallback_emitted = false;

    let notes = agent.action_contract_violation_runtime_notes(1, &mut fallback_emitted);
    assert!(!fallback_emitted);
    assert_eq!(agent.model, "gpt-5.3-codex");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("action contract active")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("file-mutating tool_use")),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|note| note.contains("git_commit")),
        "{notes:?}"
    );

    let notes = agent.action_contract_violation_runtime_notes(2, &mut fallback_emitted);
    assert!(fallback_emitted);
    assert_eq!(agent.model, "gpt-5.4");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("switched model to gpt-5.4")),
        "{notes:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn workflow_diagnostics_render_and_ledger_are_prompt_visible() {
    let report = WorkflowDiagnosticsReport {
        source: "rust-analyzer".to_string(),
        status: "failed".to_string(),
        diagnostics: vec![WorkflowDiagnostic {
            file: "src/lib.rs".to_string(),
            line: Some(1),
            character: Some(2),
            severity: "error".to_string(),
            code: Some("E0001".to_string()),
            message: "bad thing".to_string(),
        }],
        raw_output: String::new(),
        duration: std::time::Duration::from_millis(12),
    };
    let rendered = render_workflow_diagnostics(&report, 2_000);
    assert!(rendered.contains("via rust-analyzer"), "{rendered}");
    assert!(rendered.contains("src/lib.rs:1:2"), "{rendered}");

    let mut ledger = WorkLedger::default();
    ledger.diagnostics.push(WorkflowDiagnosticRecord {
        source: report.source.clone(),
        status: report.status.clone(),
        summary: workflow_diagnostic_summary(&report),
        errors: 1,
        warnings: 0,
        duration_ms: millis_u64(report.duration),
    });
    let prompt = render_work_ledger_prompt(&ledger);
    assert!(prompt.contains("diagnostics:"), "{prompt}");
    assert!(prompt.contains("errors=1"), "{prompt}");
}

#[test]
fn privacy_redacts_sensitive_tool_output_and_blocks_secret_paths() {
    let root = temp_test_dir("privacy-policy");
    let mut agent = test_agent(&root);
    agent.privacy.enabled = true;

    let content = "ssn=123-45-6789\ncard 4111 1111 1111 1111\naccount number: 123456789012\nAPI_KEY=abcdef123456";
    let redacted = agent.privacy.apply_tool_output(
        "read_file",
        &json!({"path": "data.txt"}),
        content.to_string(),
    );
    assert!(!redacted.text.contains("123-45-6789"));
    assert!(!redacted.text.contains("4111 1111 1111 1111"));
    assert!(!redacted.text.contains("123456789012"));
    assert!(!redacted.text.contains("abcdef123456"));
    assert!(
        redacted.text.contains("[REDACTED_SSN]"),
        "{}",
        redacted.text
    );
    assert!(
        redacted.text.contains("[REDACTED_CARD]"),
        "{}",
        redacted.text
    );
    assert!(
        redacted.text.contains("[REDACTED_ACCOUNT]"),
        "{}",
        redacted.text
    );
    assert!(
        redacted.text.contains("[REDACTED_SECRET]"),
        "{}",
        redacted.text
    );
    assert!(
        redacted.text.contains("[privacy] Redacted"),
        "{}",
        redacted.text
    );

    let denial = agent
        .privacy
        .path_denial("read_file", &json!({"path": ".env"}))
        .expect("secret path blocked");
    assert!(denial.contains("blocked read_file"), "{denial}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn slash_privacy_toggles_runtime_policy() {
    let root = temp_test_dir("privacy-slash");
    let mut agent = test_agent(&root);

    assert_eq!(handle_slash("/privacy on", &mut agent), Some(true));
    assert!(agent.privacy.enabled);
    assert_eq!(handle_slash("/privacy status", &mut agent), Some(true));
    assert_eq!(handle_slash("/privacy off", &mut agent), Some(true));
    assert!(!agent.privacy.enabled);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn approval_and_sandbox_profiles_enforce_policy() {
    let root = temp_test_dir("approval-sandbox-profiles");
    let mut agent = test_agent(&root);
    let read = json!({"command": "pwd"});
    let write = json!({"path": "out.txt", "content": "x"});

    agent.set_approval_profile(ApprovalProfile::AutoRead);
    assert!(agent.tool_auto_approved("bash", &read));
    assert!(!agent.tool_auto_approved("write_file", &write));

    agent.set_approval_profile(ApprovalProfile::AutoWrite);
    assert!(agent.tool_auto_approved("write_file", &write));
    assert!(!agent.tool_auto_approved("bash", &json!({"command": "sudo reboot"})));

    agent.set_sandbox_profile(SandboxProfile::ReadOnly);
    assert!(agent.sandbox_policy_denial("write_file", &write).is_some());
    assert!(agent.sandbox_policy_denial("bash", &read).is_none());
    assert!(
        agent
            .sandbox_policy_denial("read_file", &json!({"path": "/tmp/outside"}))
            .is_none()
    );
    assert!(
        agent
            .sandbox_policy_denial("git_diff", &json!({"stat": true}))
            .is_none()
    );
    assert!(
        agent
            .sandbox_policy_denial("todo_read", &json!({}))
            .is_none()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn default_toolset_hides_specialized_tools_and_frugal_is_smaller() {
    let root = temp_test_dir("toolset-default-frugal");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.refresh_tools_for_context();
    let default_names: HashSet<&str> = agent.tools.iter().map(|t| t.name).collect();

    for name in [
        "read_file",
        "read_symbol",
        "write_file",
        "edit_file",
        "multi_edit",
        "bash",
        "fd",
        "rg",
        "http",
        "git_diff",
        "git_commit",
        "todo_read",
        "todo_write",
    ] {
        assert!(default_names.contains(name), "default should expose {name}");
    }
    for name in ["jq", "fzf", "awk", "git_log", "csvkit", "browser"] {
        assert!(!default_names.contains(name), "default should hide {name}");
    }

    agent.tool_context_profile = ToolContextProfile::Full;
    agent.context_mode = ContextMode::Frugal;
    agent.refresh_tools_for_context();
    let frugal_names: HashSet<&str> = agent.tools.iter().map(|t| t.name).collect();
    assert_eq!(agent.tool_context_profile(), ToolContextProfile::Frugal);
    assert!(frugal_names.len() < default_names.len());
    assert!(!frugal_names.contains("http"));
    assert!(frugal_names.contains("bash"));
    assert!(frugal_names.contains("git_diff"));

    agent.allowed.insert("jq".to_string());
    agent.allowed.insert("bash".to_string());
    agent.deny_tools.insert("csvkit".to_string());
    agent.deny_tools.insert("rg".to_string());
    agent.refresh_tools_for_context();
    assert!(!agent.allowed.contains("jq"));
    assert!(agent.allowed.contains("bash"));
    assert!(!agent.deny_tools.contains("csvkit"));
    assert!(agent.deny_tools.contains("rg"));

    agent.context_mode = ContextMode::Standard;
    agent.tool_context_profile = ToolContextProfile::Default;
    agent.set_browser_recipe(BrowserRecipe::AgentBrowser);
    assert!(agent.tools.iter().any(|t| t.name == "browser"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn full_toolset_env_exposes_specialized_tools_without_browser_by_default() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("toolset-full-env");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    unsafe { std::env::set_var("DEXT_TOOLSET", "full") };
    let result = (|| -> Result<()> {
        let mut agent = test_agent(&root);
        agent.tool_context_profile = ToolContextProfile::from_env();
        agent.refresh_tools_for_context();
        let names: HashSet<&str> = agent.tools.iter().map(|t| t.name).collect();
        for name in ["jq", "fzf", "awk", "git_log", "csvkit"] {
            assert!(names.contains(name), "full toolset should expose {name}");
        }
        assert!(!names.contains("browser"));
        assert_eq!(agent.tool_context_profile(), ToolContextProfile::Full);
        Ok(())
    })();
    unsafe { std::env::remove_var("DEXT_TOOLSET") };
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn browser_recipe_toggles_browser_tool() {
    let root = temp_test_dir("browser-recipe");
    let mut agent = test_agent(&root);
    assert!(agent.tools.iter().all(|t| t.name != "browser"));

    agent.set_browser_recipe(BrowserRecipe::AgentBrowser);
    assert!(agent.tools.iter().any(|t| t.name == "browser"));
    assert_eq!(agent.browser_recipe(), BrowserRecipe::AgentBrowser);

    agent.set_approval_profile(ApprovalProfile::Always);
    assert!(agent.allowed.contains("browser"));
    agent.set_browser_recipe(BrowserRecipe::Disabled);
    assert!(agent.tools.iter().all(|t| t.name != "browser"));
    assert_eq!(agent.browser_recipe(), BrowserRecipe::Disabled);
    assert!(!agent.allowed.contains("browser"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn thinking_effort_parse_and_cycle() {
    assert_eq!(ThinkingEffort::parse("off"), Some(ThinkingEffort::Off));
    assert_eq!(ThinkingEffort::parse("none"), Some(ThinkingEffort::Off));
    assert_eq!(ThinkingEffort::parse("low"), Some(ThinkingEffort::Low));
    assert_eq!(ThinkingEffort::parse("MED"), Some(ThinkingEffort::Medium));
    assert_eq!(ThinkingEffort::parse("x-high"), Some(ThinkingEffort::XHigh));
    assert_eq!(ThinkingEffort::parse("unknown"), None);
    assert_eq!(ThinkingEffort::Off.cycle(-1), ThinkingEffort::XHigh);
    assert_eq!(ThinkingEffort::Low.cycle(-1), ThinkingEffort::Off);
    assert_eq!(ThinkingEffort::XHigh.cycle(1), ThinkingEffort::Off);
}

#[test]
fn provider_tool_result_id_bug_detects_claude_and_chatgpt_patterns() {
    assert!(is_provider_tool_result_id_bug(
        "ClaudeContentBlockToolResult has no attribute 'id'"
    ));
    assert!(is_provider_tool_result_id_bug(
        r#"No tool call found for function call output with call_id call_abc123."#
    ));
    assert!(!is_provider_tool_result_id_bug("rate limited"));
    assert!(!is_provider_tool_result_id_bug(
        "ClaudeContentBlockToolResult but no id substring"
    ));
}

#[test]
fn provider_bug_fallback_flattens_latest_tool_results() {
    let root = temp_test_dir("provider-workaround");
    let mut agent = test_agent(&root);
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block("call_123", "missing path", Some(true))],
    });

    assert!(agent.rewrite_latest_tool_results_as_text_fallback());
    match &agent.history.last().expect("history").content[..] {
        [Block::Text { text }] => {
            assert!(text.contains("provider rejected structured tool_result blocks"));
            assert!(text.contains("call_123"));
        }
        other => panic!("expected flattened text block, got: {other:?}"),
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn parse_tool_input_json_handles_stringified_json() {
    let parsed = parse_tool_input_json("\"{\\\"task\\\":\\\"hello\\\",\\\"max_iterations\\\":1}\"");
    assert_eq!(parsed["task"], "hello");
    assert_eq!(parsed["max_iterations"], 1);
}

#[test]
fn append_tool_input_json_fragment_replaces_empty_placeholder() {
    let mut raw = "{}".to_string();
    append_tool_input_json_fragment(&mut raw, "{\"task\":\"hello\"}");
    let parsed = parse_tool_input_json(&raw);
    assert_eq!(parsed["task"], "hello");
}

#[tokio::test]
async fn external_runner_times_out() {
    let root = temp_test_dir("external-timeout");
    let args = vec!["-lc".to_string(), "sleep 2".to_string()];
    let err = execute_external_async(
        "bash",
        &args,
        None,
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_millis(150),
    )
    .await
    .expect_err("expected timeout");
    assert!(err.contains("timed out after"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn external_runner_honors_interrupts() {
    let root = temp_test_dir("external-interrupt");
    let args = vec!["-lc".to_string(), "sleep 5".to_string()];
    let interrupt = Arc::new(AtomicBool::new(false));
    let trigger = interrupt.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;
        trigger.store(true, Ordering::SeqCst);
    });

    let err = execute_external_async(
        "bash",
        &args,
        None,
        &root,
        interrupt,
        std::time::Duration::from_secs(10),
    )
    .await
    .expect_err("expected interrupt");
    assert!(err.contains("killed by interrupt"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn fast_bash_command_returns_without_100ms_poll_tail() {
    let root = temp_test_dir("bash-fastpath");
    let start = std::time::Instant::now();
    let out = execute_bash_async_with_timeout(
        "true",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("expected success");
    let elapsed = start.elapsed();
    assert!(out.contains("exit: 0"), "{out}");
    assert!(
        elapsed < std::time::Duration::from_millis(90),
        "expected <90ms (old busy-poll capped at 100ms); got {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn bash_runner_times_out() {
    let root = temp_test_dir("bash-timeout");
    let err = execute_bash_async_with_timeout(
        "sleep 2",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_millis(150),
    )
    .await
    .expect_err("expected timeout");
    assert!(err.contains("timed out after"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sync_runner_times_out() {
    let root = temp_test_dir("sync-timeout");
    let mut cmd = Command::new("bash");
    cmd.arg("-lc").arg("sleep 2").current_dir(&root);
    let err = run_sync_command_limited(
        cmd,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "bash",
        std::time::Duration::from_millis(150),
    )
    .expect_err("expected timeout");
    assert!(err.contains("timed out after"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn bash_runner_reaps_background_children_after_shell_exit() {
    let root = temp_test_dir("bash-grandchild-reap");
    let start = std::time::Instant::now();
    let out = execute_bash_async_with_timeout(
        "sleep 2 & echo done",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("expected success");
    let elapsed = start.elapsed();
    assert!(out.contains("exit: 0"), "{out}");
    assert!(out.contains("done"), "{out}");
    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "background child kept pipe open for {elapsed:?}; output={out}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sync_runner_reaps_background_children_after_shell_exit() {
    let root = temp_test_dir("sync-grandchild-reap");
    let mut cmd = Command::new("bash");
    cmd.arg("-lc").arg("sleep 2 & echo done").current_dir(&root);
    let start = std::time::Instant::now();
    let (out, _, status) = run_sync_command_limited(
        cmd,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "bash",
        std::time::Duration::from_secs(5),
    )
    .expect("expected success");
    let elapsed = start.elapsed();
    assert_eq!(status, 0);
    assert!(out.render("stdout").contains("done"));
    assert!(
        elapsed < std::time::Duration::from_millis(900),
        "background child kept pipe open for {elapsed:?}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tool_result_metadata_parses_status_and_artifact_hints() {
    let failed = "exit: 101\n--- stdout ---\n--- stderr ---\ncompile failed";
    assert_eq!(parse_tool_exit_code("bash", false, failed), Some(101));
    assert_eq!(parse_tool_exit_code("rg", true, "match\n"), None);
    assert!(looks_like_verification_command("cargo nextest run ui"));

    let mut noted = failed.to_string();
    insert_runtime_notes(&mut noted, &["prefer native rg".to_string()]);
    assert_eq!(parse_tool_exit_code("bash", false, &noted), Some(101));
    assert!(
        noted.starts_with("exit: 101\n\n[runtime-note] prefer native rg"),
        "{noted}"
    );

    let capped = cap_bytes_with_hint(
        "x".repeat(128),
        8,
        "Full verification output saved as a structured artifact: /tmp/verify.json",
    );
    assert!(capped.contains("/tmp/verify.json"), "{capped}");
}

#[test]
fn read_file_explicit_window_uses_larger_model_context_cap_and_cache() {
    let root = temp_test_dir("read-file-explicit-cache");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut content = String::new();
    for i in 1..=120 {
        content.push_str(&format!("line-{i:03}-{}\n", "x".repeat(120)));
    }
    std::fs::write(root.join("big.txt"), content).expect("write big file");
    let metadata = std::fs::metadata(root.join("big.txt")).expect("metadata");
    let signature = file_signature_from_metadata(&metadata);
    let cache_path = std::fs::canonicalize(root.join("big.txt")).expect("canonical file");
    let cache = Arc::new(Mutex::new(ReadFileCache::default()));

    let out = execute_tool_with_cache(
        "read_file",
        &json!({"path": "big.txt", "offset": 1, "limit": 110}),
        &root,
        Some(&cache),
    )
    .expect("explicit read should succeed");
    assert!(
        out.len() > TOOL_RESULT_CAP,
        "explicit window should exceed generic cap: {}",
        out.len()
    );
    assert!(
        out.len() <= READ_FILE_EXPLICIT_CAPTURE_CAP + 140,
        "{}",
        out.len()
    );
    assert_eq!(
        tool_result_context_cap(
            "read_file",
            &json!({"path": "big.txt", "offset": 1, "limit": 110}),
            &Usage::default(),
            "test-model",
            ContextMode::Standard,
        ),
        READ_FILE_EXPLICIT_CAPTURE_CAP
    );

    let cached = cache
        .lock()
        .expect("cache lock")
        .get_window(
            &cache_path,
            signature,
            10,
            10,
            READ_FILE_EXPLICIT_CAPTURE_CAP,
        )
        .expect("overlap should be served from cached union");
    assert!(cached.contains("10\tline-010"), "{cached}");
    assert!(cached.contains("19\tline-019"), "{cached}");

    std::fs::remove_file(root.join("big.txt")).expect("remove source after cache fill");
    let cached = execute_tool_with_cache(
        "read_file",
        &json!({"path": "big.txt", "offset": 10, "limit": 10}),
        &root,
        Some(&cache),
    );
    assert!(
        cached.is_err(),
        "metadata signature lookup should prevent serving stale cache after file removal"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_symbol_returns_symbol_block_with_context() {
    let root = temp_test_dir("read-symbol");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(
        root.join("lib.rs"),
        "fn helper() {}\n\nfn target() {\n    let value = 1;\n    println!(\"{}\", value);\n}\n\nfn tail() {}\n",
    )
    .expect("write source");

    let out = execute_tool(
        "read_symbol",
        &json!({"path": "lib.rs", "symbol": "target", "context": 1}),
        &root,
    )
    .expect("read_symbol should succeed");
    assert!(out.contains("3\tfn target()"), "{out}");
    assert!(out.contains("6\t}"), "{out}");
    assert!(out.contains("7\t"), "{out}");
    assert!(!out.contains("1\tfn helper"), "{out}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_symbol_line_mode_returns_enclosing_block_without_extra_allocations() {
    let root = temp_test_dir("read-symbol-line");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(
        root.join("lib.rs"),
        "fn before() {}\n\nfn target() {\n    if true {\n        println!(\"hit\");\n    }\n}\n\nfn after() {}\n",
    )
    .expect("write source");

    let out = execute_tool(
        "read_symbol",
        &json!({"path": "lib.rs", "line": 5, "context": 0}),
        &root,
    )
    .expect("line mode should succeed");
    assert!(out.contains("3\tfn target()"), "{out}");
    assert!(out.contains("5\t        println!"), "{out}");
    assert!(out.contains("7\t}"), "{out}");
    assert!(!out.contains("1\tfn before"), "{out}");
    assert!(!out.contains("9\tfn after"), "{out}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_symbol_not_found_suggests_nearby_symbols() {
    let root = temp_test_dir("read-symbol-suggestions");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(
        root.join("lib.rs"),
        "fn load_provider_catalog() {}\nfn render_provider_picker() {}\nstruct ProviderState;\n",
    )
    .expect("write source");

    let err = execute_tool(
        "read_symbol",
        &json!({"path": "lib.rs", "symbol": "load_provider_catlog"}),
        &root,
    )
    .expect_err("missing symbol should suggest neighbors");
    assert!(err.contains("Did you mean:"), "{err}");
    assert!(err.contains("load_provider_catalog @ lib.rs:1"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_symbol_requires_exactly_one_selector() {
    assert_eq!(
        tool_policy::tool_input_issue("read_symbol", &json!({"path": "lib.rs", "line": 12})),
        None
    );
    let neither = tool_policy::tool_input_issue("read_symbol", &json!({"path": "lib.rs"}))
        .expect("missing selector should be rejected");
    assert!(neither.contains("provide symbol or line"), "{neither}");
    let both = tool_policy::tool_input_issue(
        "read_symbol",
        &json!({"path": "lib.rs", "symbol": "target", "line": 12}),
    )
    .expect("both selectors should be rejected");
    assert!(both.contains("only one"), "{both}");

    let root = temp_test_dir("read-symbol-selector-validation");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(root.join("lib.rs"), "fn target() {}\n").expect("write source");
    let err = execute_tool(
        "read_symbol",
        &json!({"path": "lib.rs", "symbol": "target", "line": 1}),
        &root,
    )
    .expect_err("runtime should reject ambiguous selector");
    assert!(err.contains("exactly one"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn read_file_caps_large_output_and_suggests_resume_offset() {
    let root = temp_test_dir("read-file-cap");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let path = root.join("big.txt");
    let mut content = String::new();
    for i in 1..=300 {
        content.push_str(&format!("line-{i:03}-{}\n", "x".repeat(60)));
    }
    std::fs::write(&path, content).expect("write big file");

    let out = execute_tool("read_file", &json!({"path": "big.txt"}), &root)
        .expect("read_file should succeed");
    assert!(out.contains("output capped after"), "{out}");
    assert!(out.contains("Pass offset="), "{out}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn compaction_preserves_recent_tool_messages_and_summarizes_older_context() {
    let root = temp_test_dir("compact-preserve-tools");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let agent = test_agent(&root);
    let mut old = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "Find where startup loads config".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "I will inspect main.rs first".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_1".to_string(),
                name: "read_file".to_string(),
                input: json!({"path": "src/main.rs"}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("call_1", "1\tfn main() {}", Some(false))],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "Now wire in memory loading".to_string(),
            }],
        },
    ];
    for i in 0..5 {
        old.push(Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: format!("call_recent_{i}"),
                name: "read_file".to_string(),
                input: json!({"path": format!("src/recent_{i}.rs")}),
            }],
        });
        old.push(Message {
            role: "user".to_string(),
            content: vec![tool_result_block(
                &format!("call_recent_{i}"),
                &format!("{i}\trecent output"),
                Some(false),
            )],
        });
    }

    let (summary_msgs, preserved) = agent.split_compaction_inputs(&old);
    assert_eq!(
        preserved.len(),
        10,
        "expected recent tool use/result pairs to be kept"
    );
    assert!(preserved.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { .. }))
    }));
    assert!(
        summary_msgs.len() >= 3,
        "older text/tool turns should remain for summarization; got {}",
        summary_msgs.len()
    );
    assert!(summary_msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, Block::Text { text } if text.contains("Find where startup")))
    }));
    assert!(summary_msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, Block::ToolUse { id, .. } if id == "call_1"))
    }));
    assert!(summary_msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|b| matches!(b, Block::ToolResult { tool_use_id, .. } if tool_use_id == "call_1"))
    }));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn find_compact_split_stays_near_tail_even_without_recent_user_boundary() {
    let root = temp_test_dir("compact-tail-split");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);

    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "start a long code investigation".to_string(),
        }],
    });
    for i in 0..40 {
        agent.history.push(Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: format!("call_{i}"),
                name: "read_file".to_string(),
                input: json!({"path": format!("src/{i}.rs")}),
            }],
        });
        agent.history.push(Message {
            role: "user".to_string(),
            content: vec![tool_result_block(
                &format!("call_{i}"),
                &"x".repeat(2048),
                Some(false),
            )],
        });
    }

    let split = agent
        .find_compact_split()
        .expect("tool-heavy history should still compact");
    assert!(
        split
            >= agent
                .history
                .len()
                .saturating_sub(COMPACT_KEEP_MESSAGES + 8),
        "split={split}, len={}",
        agent.history.len()
    );
    assert!(Agent::compact_split_is_pair_safe(&agent.history, split));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn compaction_summary_caps_old_tool_results() {
    let root = temp_test_dir("compact-summary-tool-cap");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let agent = test_agent(&root);
    let mut old = vec![
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_old".to_string(),
                name: "bash".to_string(),
                input: json!({"command": "cat huge.log"}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block(
                "call_old",
                &"x".repeat(COMPACT_SUMMARY_TOOL_RESULT_CAP + 500),
                Some(false),
            )],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "after old tool".to_string(),
            }],
        },
    ];
    for i in 0..5 {
        old.push(Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: format!("call_new_{i}"),
                name: "read_file".to_string(),
                input: json!({"path": format!("src/new_{i}.rs")}),
            }],
        });
        old.push(Message {
            role: "user".to_string(),
            content: vec![tool_result_block(
                &format!("call_new_{i}"),
                "small recent output",
                Some(false),
            )],
        });
    }

    let (summary_msgs, preserved) = agent.split_compaction_inputs(&old);
    assert_eq!(preserved.len(), 10, "recent pairs should be retained");
    let tool_result = summary_msgs
        .iter()
        .flat_map(|m| m.content.iter())
        .find_map(|b| match b {
            Block::ToolResult { content, .. } => Some(content),
            _ => None,
        })
        .expect("tool result included in summary input");
    assert!(
        tool_result.contains("Tool result truncated before compaction summary"),
        "{tool_result}"
    );
    assert!(
        tool_result.len() < COMPACT_SUMMARY_TOOL_RESULT_CAP + 200,
        "{}",
        tool_result.len()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn run_external_caps_streamed_stdout() {
    let root = temp_test_dir("external-cap");
    let args = vec![
        "-lc".to_string(),
        "for i in {1..7000}; do printf x; done".to_string(),
    ];

    let out = run_external("bash", &args, None, &root).expect("external run should succeed");
    assert!(out.contains("stdout capped after"), "{out}");
    assert!(out.contains("kept first 3000 and last 3000"), "{out}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn limited_byte_capture_preserves_head_and_tail() {
    let mut capture = LimitedByteCapture::new(10);
    capture.push(b"abcdefgh");
    capture.push(b"ijklmnop");
    let rendered = capture.render("stdout");

    assert!(rendered.starts_with("abcde"), "{rendered}");
    assert!(rendered.contains("kept first 5 and last 5"), "{rendered}");
    assert!(rendered.contains("lmnop"), "{rendered}");
}

#[test]
fn project_todo_summary_prioritizes_active_work_for_prompt() {
    let root = temp_test_dir("todo-summary");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let todos = json!([
        {"text": "done task", "status": "completed"},
        {"text": "active task", "status": "in_progress"},
        {"text": "next task", "status": "pending"}
    ]);
    std::fs::write(
        root.join("DEXT.todo.json"),
        serde_json::to_string_pretty(&todos).unwrap(),
    )
    .unwrap();

    let summary = read_project_todo_summary(&root, 2).expect("todo summary");
    assert!(
        summary.contains("todo_status: 1 pending, 1 in_progress, 1 completed"),
        "{summary}"
    );
    assert!(summary.contains("- in_progress: active task"), "{summary}");
    assert!(summary.contains("- pending: next task"), "{summary}");
    assert!(!summary.contains("done task"), "{summary}");

    let mut agent = test_agent(&root);
    agent.work_ledger = WorkLedger::default();
    let (_stable, env) = agent.compose_system_parts();
    assert!(env.contains("## Project todos"), "{env}");
    assert!(env.contains("active task"), "{env}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prepare_http_tool_request_parses_httpie_style_args() {
    let request = prepare_http_tool_request(
        &json!({
            "args": [
                "POST",
                "https://example.com/api?existing=1",
                "Accept: application/json",
                "page==2",
                "name=john",
                "count:=3"
            ]
        }),
        std::time::Duration::from_secs(30),
    )
    .expect("prepare http tool request");

    assert_eq!(request.method, reqwest::Method::POST);
    assert_eq!(request.output_mode, HttpOutputMode::Raw);
    assert_eq!(
        request.url.as_str(),
        "https://example.com/api?existing=1&page=2"
    );
    assert_eq!(
        request.headers,
        vec![("Accept".to_string(), "application/json".to_string())]
    );
    match request.body.expect("json body") {
        HttpToolBody::Json(Value::Object(map)) => {
            assert_eq!(map.get("name"), Some(&Value::String("john".to_string())));
            assert_eq!(map.get("count"), Some(&Value::from(3)));
        }
        other => panic!("expected json body, got {other:?}"),
    }
}

#[test]
fn http_extract_html_text_strips_script_and_decodes_entities() {
    let html = r#"
        <!doctype html><html><head><title>Docs &amp; Research</title><style>.x{}</style></head>
        <body><h1>Session Trees</h1><script>bad_noise()</script><p>Jump &amp; teleport&nbsp;ideas</p></body></html>
    "#;
    let text = extract_response_text(html.to_string(), Some("text/html; charset=utf-8"));

    assert!(text.contains("Docs & Research"), "{text}");
    assert!(text.contains("Session Trees"), "{text}");
    assert!(text.contains("Jump & teleport"), "{text}");
    assert!(!text.contains("bad_noise"), "{text}");
    assert!(!text.contains(".x"), "{text}");
}

#[test]
fn prepare_http_tool_request_accepts_extract_text_flag() {
    let request = prepare_http_tool_request(
        &json!({"args": ["GET", "https://example.com", "--extract-text"]}),
        std::time::Duration::from_secs(30),
    )
    .expect("prepare http tool request");

    assert_eq!(request.method, reqwest::Method::GET);
    assert_eq!(request.output_mode, HttpOutputMode::Text);
}

#[tokio::test]
async fn builtin_http_tool_executes_without_xh_dependency() {
    let root = temp_test_dir("http-built-in");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set read timeout");

        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        let mut header_end = None;
        while header_end.is_none() {
            let n = stream.read(&mut buf).expect("read request headers");
            assert!(n > 0, "client closed before sending headers");
            request.extend_from_slice(&buf[..n]);
            header_end = request.windows(4).position(|w| w == b"\r\n\r\n");
        }

        let header_end = header_end.expect("header terminator") + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                let lower = line.to_ascii_lowercase();
                lower
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let n = stream.read(&mut buf).expect("read request body");
            assert!(n > 0, "client closed before sending body");
            request.extend_from_slice(&buf[..n]);
        }

        let body = String::from_utf8_lossy(&request[header_end..header_end + content_length]);
        assert!(
            headers.starts_with("POST /submit HTTP/1.1\r\n"),
            "{headers}"
        );
        assert!(
            headers.contains("accept: application/json\r\n"),
            "{headers}"
        );
        assert!(
            headers.contains("content-type: application/json"),
            "{headers}"
        );
        assert!(body.contains(r#""name":"john""#), "{body}");
        assert!(body.contains(r#""count":3"#), "{body}");

        stream
            .write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}",
            )
            .expect("write response");
    });

    let out = execute_builtin_call(
        "http".to_string(),
        json!({
            "args": [
                "POST",
                format!("http://{addr}/submit"),
                "Accept: application/json",
                "name=john",
                "count:=3"
            ]
        }),
        root.clone(),
        Arc::new(AtomicBool::new(false)),
        None,
        None,
    )
    .await
    .expect("http request should succeed without xh installed");

    assert!(out.contains("{\"ok\":true}"), "{out}");
    server.join().expect("server thread");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rg_falls_back_to_grep_when_not_on_path() {
    // When rg is missing binary_on_path returns false; grep is always present.
    // Directly test the fallback output by preparing the tool and checking
    // the result matches what we'd get with no rg on PATH.
    let root = temp_test_dir("rg-fallback");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");

    // If rg IS on PATH we get rg; if not we get grep. Either is valid.
    let (bin, args, stdin) = prepare_external_tool(
        "rg",
        &json!({"pattern": "fn main", "path": root.to_str().unwrap()}),
        &root,
    )
    .expect("prepare rg tool");

    if bin == "grep" {
        assert!(args.contains(&"-rn".to_string()));
        assert!(args.contains(&"-E".to_string()));
        assert!(args.contains(&"fn main".to_string()));
    } else {
        assert_eq!(bin, "rg");
        assert!(args.contains(&"--line-number".to_string()));
        assert!(args.contains(&"fn main".to_string()));
    }
    assert!(stdin.is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fd_falls_back_to_find_when_not_on_path() {
    let root = temp_test_dir("fd-fallback");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");

    let (bin, args, stdin) = prepare_external_tool(
        "fd",
        &json!({"pattern": ".*\\.rs$", "path": root.to_str().unwrap()}),
        &root,
    )
    .expect("prepare fd tool");

    if bin == "find" {
        let type_idx = args
            .iter()
            .position(|arg| arg == "-type")
            .expect("find args include -type");
        assert_eq!(args[type_idx + 1], "f");
        assert!(args.iter().any(|a| a.contains(".rs")));
    } else {
        assert_eq!(bin, "fd");
        assert!(args.iter().any(|a| a.contains(".rs")));
    }
    assert!(stdin.is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_log_builds_separate_flag_and_count_args() {
    // Regression: `git log "--oneline -8"` is a single arg, rejected by git.
    // Must be two separate argv entries: ["log", "--oneline", "-8"].
    let root = temp_test_dir("git-log-argv");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, _) =
        prepare_external_tool("git_log", &json!({"count": 8, "oneline": true}), &root)
            .expect("prepare git_log");
    assert_eq!(bin, "git");
    assert_eq!(args, vec!["log", "--oneline", "-8"]);

    let (_, args_no_oneline, _) =
        prepare_external_tool("git_log", &json!({"count": 5, "oneline": false}), &root)
            .expect("prepare git_log without oneline");
    assert_eq!(args_no_oneline, vec!["log", "-5"]);

    let (_, args_with_path, _) = prepare_external_tool(
        "git_log",
        &json!({"count": 3, "oneline": true, "path": "src/main.rs"}),
        &root,
    )
    .expect("prepare git_log with path");
    assert_eq!(
        args_with_path,
        vec!["log", "--oneline", "-3", "--", "src/main.rs"]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fd_rejects_empty_pattern_early() {
    let root = temp_test_dir("fd-empty-pattern");
    let root = std::fs::canonicalize(&root).unwrap();

    let err = prepare_external_tool(
        "fd",
        &json!({"pattern": "", "path": root.to_str().unwrap()}),
        &root,
    )
    .expect_err("empty pattern should be rejected");
    assert!(err.contains("empty"), "{err}");

    let err_ws = prepare_external_tool(
        "fd",
        &json!({"pattern": "   ", "path": root.to_str().unwrap()}),
        &root,
    )
    .expect_err("whitespace-only pattern should be rejected");
    assert!(err_ws.contains("empty"), "{err_ws}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn jq_tool_builds_file_mode_argv() {
    let root = temp_test_dir("jq-file-argv");
    let root = std::fs::canonicalize(&root).unwrap();
    let file = root.join("data.json");
    std::fs::write(&file, b"{}").unwrap();

    let (bin, args, stdin) =
        prepare_external_tool("jq", &json!({"filter": ".foo", "path": "data.json"}), &root)
            .expect("prepare jq file mode");
    assert_eq!(bin, "jq");
    assert_eq!(args.len(), 2);
    assert_eq!(args[0], ".foo");
    assert!(args[1].ends_with("data.json"), "{}", args[1]);
    assert!(stdin.is_none());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn jq_tool_builds_inline_json_mode_argv() {
    let root = temp_test_dir("jq-inline-argv");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, stdin) = prepare_external_tool(
        "jq",
        &json!({"filter": ".name", "json": "{\"name\":\"dext\"}"}),
        &root,
    )
    .expect("prepare jq inline mode");
    assert_eq!(bin, "jq");
    assert_eq!(args, vec![".name"]);
    assert_eq!(stdin.as_deref(), Some("{\"name\":\"dext\"}"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn jq_tool_errors_without_path_or_json() {
    let root = temp_test_dir("jq-missing-source");
    let root = std::fs::canonicalize(&root).unwrap();

    let err = prepare_external_tool("jq", &json!({"filter": "."}), &root)
        .expect_err("jq with neither path nor json should error");
    assert!(
        err.contains("path") && err.contains("json"),
        "unexpected error: {err}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fzf_tool_pipes_items_as_stdin() {
    let root = temp_test_dir("fzf-argv");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, stdin) = prepare_external_tool(
        "fzf",
        &json!({"query": "foo", "items": ["foo.rs", "bar.rs", "foobar.rs"]}),
        &root,
    )
    .expect("prepare fzf");
    assert_eq!(bin, "fzf");
    assert_eq!(args, vec!["--filter", "foo"]);
    assert_eq!(stdin.as_deref(), Some("foo.rs\nbar.rs\nfoobar.rs"));

    let err = prepare_external_tool("fzf", &json!({"query": "foo", "items": []}), &root)
        .expect_err("fzf with empty items should error");
    assert!(err.contains("items"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn awk_tool_passes_args_and_stdin_verbatim() {
    let root = temp_test_dir("awk-argv");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, stdin) = prepare_external_tool(
        "awk",
        &json!({
            "args": ["-F,", "{print $2}"],
            "stdin": "a,b,c\nd,e,f"
        }),
        &root,
    )
    .expect("prepare awk");
    assert_eq!(bin, "awk");
    assert_eq!(args, vec!["-F,", "{print $2}"]);
    assert_eq!(stdin.as_deref(), Some("a,b,c\nd,e,f"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn csvkit_tool_uses_subcommand_as_binary() {
    let root = temp_test_dir("csvkit-argv");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, stdin) = prepare_external_tool(
        "csvkit",
        &json!({
            "subcommand": "csvcut",
            "args": ["-c", "1,3"],
            "stdin": "a,b,c\n1,2,3"
        }),
        &root,
    )
    .expect("prepare csvkit");
    assert_eq!(bin, "csvcut");
    assert_eq!(args, vec!["-c", "1,3"]);
    assert_eq!(stdin.as_deref(), Some("a,b,c\n1,2,3"));

    let err = prepare_external_tool("csvkit", &json!({"args": ["-c", "1"]}), &root)
        .expect_err("csvkit without subcommand should error");
    assert!(err.contains("subcommand"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rg_tool_threads_extra_args_on_happy_path() {
    // Only meaningful when rg is on PATH. Skip otherwise rather than assert.
    if !binary_on_path("rg") {
        return;
    }
    let root = temp_test_dir("rg-extra-args");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, _) = prepare_external_tool(
        "rg",
        &json!({
            "pattern": "fn main",
            "path": root.to_str().unwrap(),
            "extra_args": ["-i", "--glob=*.rs", "--glob", "!node_modules"]
        }),
        &root,
    )
    .expect("prepare rg with extras");
    assert_eq!(bin, "rg");
    // First two are always base flags, then extras in order, then pattern + path.
    assert_eq!(&args[0..2], &["--line-number", "--no-heading"]);
    assert!(args.contains(&"-i".to_string()), "{args:?}");
    assert!(args.contains(&"--glob=*.rs".to_string()), "{args:?}");
    assert!(
        args.contains(&"!**/node_modules/**".to_string()),
        "{args:?}"
    );
    let pattern_idx = args.iter().position(|a| a == "fn main").expect("pattern");
    let path_idx = pattern_idx + 1;
    assert_eq!(args.get(path_idx).map(String::as_str), root.to_str());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn rg_grep_fallback_translates_glob_flag() {
    // Simulate the grep fallback branch by asserting the documented translation:
    // --glob=X becomes --include=X, -i passes through, others are dropped.
    // This matches prepare_external_tool's grep arm when rg isn't on PATH.
    // We can't easily simulate rg-missing inside a unit test, so we verify the
    // translation directly via the prepare_external_tool_fallback helper which
    // shares the same translation logic.
    let root = temp_test_dir("rg-grep-fallback-argv");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, _) = prepare_external_tool_fallback(
        "rg",
        &json!({
            "pattern": "needle",
            "path": root.to_str().unwrap(),
            "extra_args": ["-i", "--glob=*.rs", "--glob", "!node_modules", "--unsupported-flag"]
        }),
        &root,
    );
    assert_eq!(bin, "grep");
    assert_eq!(&args[0..3], &["-rn", "-E", "--color=never"]);
    assert!(args.contains(&"-i".to_string()), "{args:?}");
    assert!(
        args.contains(&"--exclude-dir=node_modules".to_string()),
        "{args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "--unsupported-flag"),
        "unknown flags must be dropped in grep fallback: {args:?}"
    );
    assert_eq!(args[args.len() - 2], "needle");
    assert_eq!(args.last().map(String::as_str), root.to_str(), "{args:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fd_find_fallback_translates_type_and_consumes_value() {
    let root = temp_test_dir("fd-find-fallback-type");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, _) = prepare_external_tool_fallback(
        "fd",
        &json!({
            "pattern": "^DEXT(\\.memory)?\\.md$",
            "path": root.to_str().unwrap(),
            "extra_args": ["-H", "--type", "f"]
        }),
        &root,
    );
    assert_eq!(bin, "find");
    let type_idx = args
        .iter()
        .position(|arg| arg == "-type")
        .expect("find args include -type");
    assert_eq!(args[type_idx + 1], "f");
    assert!(args.iter().any(|arg| arg == "-regex"), "{args:?}");
    assert!(
        args.iter().any(|arg| arg == "(.*/)?DEXT(\\.memory)?\\.md$"),
        "{args:?}"
    );
    assert!(
        !args.iter().any(|a| a == "--type" || a == "-H"),
        "fd-only flags should not leak into find argv: {args:?}"
    );

    let args_with_glob = build_fd_find_fallback_args(
        &root,
        "table|tui",
        &[
            "-t".to_string(),
            "f".to_string(),
            "--glob=*.rs".to_string(),
            "--exclude".to_string(),
            ".turbo".to_string(),
            "--glob".to_string(),
            "!node_modules".to_string(),
            "--hidden".to_string(),
        ],
    );
    assert!(
        args_with_glob.iter().any(|a| a == "-name"),
        "{args_with_glob:?}"
    );
    assert!(
        args_with_glob.iter().any(|a| a == "*.rs"),
        "{args_with_glob:?}"
    );
    assert!(
        args_with_glob.iter().any(|a| a == "*/.turbo/*"),
        "{args_with_glob:?}"
    );
    assert!(
        args_with_glob.iter().any(|a| a == "*/node_modules/*"),
        "{args_with_glob:?}"
    );
    assert!(
        !args_with_glob
            .iter()
            .any(|a| a == "--glob=*.rs" || a == "--hidden" || a == "--exclude"),
        "fd-only flags should not leak into find argv: {args_with_glob:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fd_runtime_find_error_triggers_fallback_policy() {
    assert!(should_retry_external_tool_with_fallback(
        "fd",
        "fd",
        "exit 1: find: paths must precede expression: `.'\nfind: possible unquoted pattern after predicate `-regex'?"
    ));
    assert!(!should_retry_external_tool_with_fallback(
        "fd",
        "find",
        "exit 1: find: paths must precede expression"
    ));
    assert!(should_retry_external_tool_with_fallback(
        "rg",
        "rg",
        "failed to spawn rg: No such file or directory"
    ));
}

#[test]
fn fd_find_fallback_maps_directory_type() {
    let root = temp_test_dir("fd-find-fallback-dir");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, _) = prepare_external_tool_fallback(
        "fd",
        &json!({
            "pattern": "src",
            "path": root.to_str().unwrap(),
            "extra_args": ["--type", "d"]
        }),
        &root,
    );
    assert_eq!(bin, "find");
    let type_idx = args
        .iter()
        .position(|arg| arg == "-type")
        .expect("find args include -type");
    assert_eq!(args[type_idx + 1], "d");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fd_tool_threads_extra_args_on_happy_path() {
    if !binary_on_path("fd") {
        return;
    }
    let root = temp_test_dir("fd-extra-args");
    let root = std::fs::canonicalize(&root).unwrap();

    let (bin, args, _) = prepare_external_tool(
        "fd",
        &json!({
            "pattern": "\\.rs$",
            "path": root.to_str().unwrap(),
            "extra_args": ["-H", "--type", "f"]
        }),
        &root,
    )
    .expect("prepare fd with extras");
    assert_eq!(bin, "fd");
    // extras prepended, then pattern, then path (per prepare_external_tool).
    assert_eq!(&args[0..3], &["-H", "--type", "f"]);
    assert_eq!(args[3], "\\.rs$");
    assert_eq!(args.get(4).map(String::as_str), root.to_str());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_diff_builds_expected_argv() {
    let root = temp_test_dir("git-diff-argv");
    let root = std::fs::canonicalize(&root).unwrap();

    let (_, args, _) = prepare_external_tool(
        "git_diff",
        &json!({"staged": true, "path": "src/main.rs"}),
        &root,
    )
    .expect("prepare git_diff");
    assert_eq!(args, vec!["diff", "--cached", "--", "src/main.rs"]);

    let (_, args_commit, _) =
        prepare_external_tool("git_diff", &json!({"commit": "HEAD~1"}), &root)
            .expect("prepare git_diff commit");
    assert_eq!(args_commit, vec!["diff", "HEAD~1"]);

    let (_, args_stat, _) = prepare_external_tool(
        "git_diff",
        &json!({"stat": true, "staged": true, "path": "src/main.rs"}),
        &root,
    )
    .expect("prepare git_diff stat");
    assert_eq!(
        args_stat,
        vec!["diff", "--stat", "--cached", "--", "src/main.rs"]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn stream_error_classification_retries_chunked_eof() {
    let plan = orchestrator::classify_stream_error(
        "error decoding response body: error reading a body from connection: unexpected EOF during chunk size line",
    );
    assert!(plan.retry);
}

#[test]
fn partial_stream_preserve_only_text_blocks() {
    let blocks = vec![Block::Text {
        text: "partial".to_string(),
    }];
    let mut history = Vec::new();
    assert!(maybe_preserve_partial_stream(
        &blocks,
        &mut history,
        ContextMode::Standard
    ));
    assert_eq!(history.len(), 1);
    assert!(!maybe_preserve_partial_stream(
        &blocks,
        &mut history,
        ContextMode::Standard
    ));
    assert_eq!(history.len(), 1);

    let raw_tool_text = vec![Block::Text {
        text: "to=functions.bash {\"command\":\"cargo test\"}".to_string(),
    }];
    let mut history = Vec::new();
    assert!(maybe_preserve_partial_stream(
        &raw_tool_text,
        &mut history,
        ContextMode::Standard
    ));
    assert_eq!(history.len(), 1);
    assert!(matches!(
        &history[0].content[0],
        Block::Text { text } if text.contains("to=functions.bash") && text.contains("cargo test")
    ));

    let mut history = Vec::new();
    assert!(maybe_preserve_partial_stream(
        &raw_tool_text,
        &mut history,
        ContextMode::Frugal
    ));
    assert_eq!(history.len(), 1);
    assert!(matches!(
        &history[0].content[0],
        Block::Text { text } if text.contains("tool call redacted") && !text.contains("cargo test")
    ));

    let multiline_raw_tool_text = vec![Block::Text {
        text: "to=functions.bash\n{\n  \"command\": \"cargo test\"\n}".to_string(),
    }];
    let mut history = Vec::new();
    assert!(maybe_preserve_partial_stream(
        &multiline_raw_tool_text,
        &mut history,
        ContextMode::Tiny
    ));
    assert_eq!(history.len(), 1);
    assert!(matches!(
        &history[0].content[0],
        Block::Text { text } if text.contains("tool call redacted") && !text.contains("command") && !text.contains("cargo test")
    ));

    let tool_only = vec![Block::ToolUse {
        id: "call_1".to_string(),
        name: "todo_read".to_string(),
        input: json!({}),
    }];
    assert!(!maybe_preserve_partial_stream(
        &tool_only,
        &mut history,
        ContextMode::Tiny
    ));
}

#[test]
fn parse_compact_slash_accepts_status_auto_and_percentage_override() {
    assert!(matches!(
        parse_compact_slash("/compact"),
        Some(Ok(CompactSlash::RunNow))
    ));
    assert!(matches!(
        parse_compact_slash(" /compact  "),
        Some(Ok(CompactSlash::RunNow))
    ));
    assert!(matches!(
        parse_compact_slash("/compact status"),
        Some(Ok(CompactSlash::Status))
    ));
    assert!(matches!(
        parse_compact_slash("/compact auto"),
        Some(Ok(CompactSlash::Auto))
    ));
    assert!(matches!(
        parse_compact_slash("/compact 20"),
        Some(Ok(CompactSlash::SetPercent(20)))
    ));
    assert!(matches!(
        parse_compact_slash("/compact 20%"),
        Some(Ok(CompactSlash::SetPercent(20)))
    ));
    assert!(matches!(
        parse_compact_slash("/compact now"),
        Some(Err("usage: /compact [status|auto|<percent>|<percent>%]"))
    ));
    assert!(matches!(
        parse_compact_slash("/compacted"),
        Some(Err("usage: /compact [status|auto|<percent>|<percent>%]"))
    ));
}

#[test]
fn auto_compact_depends_on_history_size_not_cumulative_output_usage() {
    let root = temp_test_dir("compact-threshold");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.model = "demo-128k".to_string();
    agent.context_window_tokens = model_context_window(&agent.model);
    agent.session_usage.output = 1_000_000;
    assert_eq!(agent.compact_threshold_chars(), 460_800);
    assert_eq!(agent.active_compact_threshold_chars(), 409_600);
    assert!(
        !agent.should_auto_compact(),
        "high cumulative output usage alone should not force compaction"
    );

    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "x".repeat(agent.active_compact_threshold_chars() + 10),
        }],
    });
    assert!(
        agent.should_active_compact(),
        "history beyond the active budget should trigger mid-run compaction"
    );
    assert!(
        !agent.should_auto_compact(),
        "history below the end-turn budget should not trigger end-turn compaction"
    );

    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "y".repeat(agent.compact_threshold_chars() - agent.history_chars() + 10),
        }],
    });
    assert!(
        agent.should_auto_compact(),
        "history beyond the end-turn budget should trigger compaction"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn manual_compact_threshold_percent_override_beats_model_default() {
    let root = temp_test_dir("compact-threshold-override");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let default_budget = agent.compact_threshold_chars();
    assert!(default_budget > 64, "{default_budget}");

    let chars = agent.set_compact_threshold_percent(20);
    assert_eq!(agent.compact_threshold_override_percent(), Some(20));
    assert_eq!(agent.compact_threshold_chars(), chars);
    assert_eq!(chars, compact_threshold_chars_for_percent(&agent.model, 20));

    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "x".repeat(chars + 8),
        }],
    });
    assert!(agent.should_auto_compact());

    agent.set_compact_threshold_auto();
    assert_eq!(agent.compact_threshold_override(), None);
    assert_eq!(agent.compact_threshold_chars(), default_budget);
    assert!(agent.active_compact_threshold_chars() < agent.compact_threshold_chars());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn runtime_control_command_updates_effort_mid_run() {
    let root = temp_test_dir("runtime-control-effort");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let mut out = Vec::new();

    let handled = apply_runtime_control_command(&mut agent, "/effort high", |msg| out.push(msg));
    assert!(handled);
    assert_eq!(agent.thinking_effort(), ThinkingEffort::High);
    assert!(
        out.iter().any(|msg| msg.contains("thinking effort -> high")
            && msg.contains("applies immediately")
            && msg.contains("next model request")),
        "{out:?}"
    );

    out.clear();
    let handled = apply_runtime_control_command(&mut agent, "/effort status", |msg| out.push(msg));
    assert!(handled);
    assert_eq!(agent.thinking_effort(), ThinkingEffort::High);
    assert!(
        out.iter().any(
            |msg| msg.contains("thinking effort: high") && !msg.contains("applies immediately")
        ),
        "{out:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn runtime_control_model_switch_updates_next_request_material() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("runtime-control-model-provider");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let mut store = load_auth_store()?;
        store.providers.insert(
            "chatgpt".to_string(),
            StoredCredential::ApiKey {
                key: "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string(),
            },
        );
        store.providers.insert(
            "deepseek".to_string(),
            StoredCredential::ApiKey {
                key: "deepseek-key".to_string(),
            },
        );
        save_auth_store(&store)?;

        let mut agent = test_agent(&root);
        agent.reload_provider(Some("glm"), false)?;
        let mut out = Vec::new();

        let handled =
            apply_runtime_control_command(&mut agent, "/model deepseek/deepseek-reasoner", |msg| {
                out.push(msg)
            });
        assert!(handled);
        assert_eq!(agent.provider_id, "deepseek");
        assert_eq!(agent.model, "deepseek-reasoner");
        assert_eq!(agent.api_provider, ApiProvider::OpenAi);
        assert!(
            out.iter().any(
                |msg| msg.contains("applies immediately") && msg.contains("next model request")
            ),
            "{out:?}"
        );

        let chatgpt_session_id = "test-session";
        let (url, body) =
            agent.build_streaming_request("sys", "env", &[], &[], chatgpt_session_id)?;
        assert!(url.contains("api.deepseek.com"), "{url}");
        let body_json: Value = serde_json::from_slice(&body)?;
        assert_eq!(body_json["model"], "deepseek-reasoner");
        assert_eq!(body_json["max_tokens"], 8192);
        assert_eq!(body_json["stream_options"]["include_usage"], true);
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn runtime_control_command_rejects_non_runtime_slash_commands() {
    let root = temp_test_dir("runtime-control-rejects-non-runtime");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let starting_profile = agent.tool_context_profile();
    let starting_threshold = agent.compact_threshold_chars();
    let mut out = Vec::new();

    assert!(!apply_runtime_control_command(
        &mut agent,
        "/tools full",
        |msg| { out.push(msg) }
    ));
    assert!(!apply_runtime_control_command(
        &mut agent,
        "/compact 25%",
        |msg| { out.push(msg) }
    ));
    assert_eq!(agent.tool_context_profile(), starting_profile);
    assert_eq!(agent.compact_threshold_chars(), starting_threshold);
    assert!(out.is_empty(), "{out:?}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reasoning_effort_off_omits_provider_reasoning_controls() -> Result<()> {
    let root = temp_test_dir("reasoning-effort-off");
    let root = std::fs::canonicalize(root)?;
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::OpenAi;
    agent.base_url = "http://127.0.0.1:8080".to_string();
    agent.model = "qwen2.5-coder-7b".to_string();
    agent.thinking_effort = ThinkingEffort::Off;

    let (_url, body) = agent.build_streaming_request("sys", "env", &[], &[], "unused")?;
    let body_json: Value = serde_json::from_slice(&body)?;
    assert!(body_json.get("reasoning_effort").is_none(), "{body_json}");
    assert!(body_json.get("stream_options").is_none(), "{body_json}");
    assert_eq!(body_json["max_tokens"], 8192);

    let chatgpt = build_chatgpt_request(
        "gpt-5.4",
        ThinkingEffort::Off,
        "sys",
        "sess-1",
        vec![json!({"type":"message","role":"user","content":[]} )],
        Vec::new(),
    );
    assert!(chatgpt.get("reasoning").is_none(), "{chatgpt}");

    assert!(openai_reasoning_effort(ThinkingEffort::Off).is_none());
    assert!(anthropic_thinking_budget_tokens(ThinkingEffort::Off).is_none());
    assert_eq!(clamp_thinking_budget_below_max(8_192, 8_192), Some(6_144));
    assert_eq!(clamp_thinking_budget_below_max(4_096, 4_096), Some(3_072));
    assert_eq!(clamp_thinking_budget_below_max(1_024, 2), Some(1));
    assert_eq!(clamp_thinking_budget_below_max(1_024, 1), None);

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn compaction_evidence_includes_ledger_verification_provider_health_and_tool_refs() {
    let mut ledger = WorkLedger {
        objective: "fix session discovery".to_string(),
        files_changed: vec!["src/main.rs".to_string()],
        ..Default::default()
    };
    ledger.verification.push(VerificationRecord {
        name: "focused tests".to_string(),
        command: "cargo test session_discovery".to_string(),
        status: "passed".to_string(),
        exit_code: Some(0),
        duration_ms: 42,
        artifact: Some(".dext/artifacts/verify.json".to_string()),
        validates: vec!["session discovery".to_string()],
    });
    let mut health = ProviderHealthLedger::default();
    health.providers.insert(
        "chatgpt".to_string(),
        ProviderHealthState {
            auth: "failed".to_string(),
            last_error: Some("HTTP 401 unauthorized".to_string()),
            retry_after: None,
            mode: Some("chatgpt-responses".to_string()),
            disabled_for_turn: true,
            consecutive_server_errors: 0,
        },
    );
    let msgs = vec![
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call-read".to_string(),
                name: "read_file".to_string(),
                input: json!({"path": "src/main.rs", "offset": 10, "limit": 20}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("call-read", "10\tfn main()", Some(false))],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "continue implementing recommendations".to_string(),
            }],
        },
    ];

    let evidence = render_compaction_evidence(&msgs, &ledger, &health);
    assert!(evidence.contains("[ledger:active]"), "{evidence}");
    assert!(evidence.contains("focused tests: passed"), "{evidence}");
    assert!(evidence.contains("[provider_health:active]"), "{evidence}");
    assert!(evidence.contains("HTTP 401"), "{evidence}");
    assert!(
        evidence.contains("[tool:call-read] ok read_file"),
        "{evidence}"
    );
    assert!(
        evidence.contains("[intent:latest] continue implementing recommendations"),
        "{evidence}"
    );
}

#[test]
fn compaction_prompt_requests_structured_resume_packet() {
    let prompt = compaction_user_text("[user] fix compaction\n");
    assert!(prompt.contains("Task"), "{prompt}");
    assert!(prompt.contains("Decisions"), "{prompt}");
    assert!(prompt.contains("Files"), "{prompt}");
    assert!(prompt.contains("Open work"), "{prompt}");
    assert!(prompt.contains("Recent state"), "{prompt}");
    let evidence_prompt =
        compaction_user_text_with_evidence("[user] hi\n", "[ledger:active]\nobjective: test");
    assert!(
        evidence_prompt.contains("Deterministic evidence packet"),
        "{evidence_prompt}"
    );
    assert!(
        evidence_prompt.contains("[ledger:active]"),
        "{evidence_prompt}"
    );
}

#[test]
fn format_compacted_summary_mentions_retained_tool_context() {
    let out = format_compacted_summary(
        "Task\n- Fix compaction\n\nDecisions\n- Keep summaries structured",
        2,
    );
    assert!(
        out.contains("[prior conversation, summarized for resume]"),
        "{out}"
    );
    assert!(
        out.contains("retained 2 recent tool message(s) verbatim"),
        "{out}"
    );
}

#[test]
fn build_compacted_history_keeps_resume_packet_then_retained_context_then_tail() {
    let preserved = vec![Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call_1".to_string(),
            name: "read_file".to_string(),
            input: json!({"path": "src/main.rs"}),
        }],
    }];
    let tail = vec![Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "continue now".to_string(),
        }],
    }];

    let history = build_compacted_history(
        "Task\n- Fix compaction\n\nDecisions\n- Preserve continuity",
        preserved,
        &tail,
    );

    assert_eq!(history.len(), 4);
    match &history[0].content[0] {
        Block::Text { text } => {
            assert!(text.contains("Task"), "{text}");
            assert!(text.contains("Decisions"), "{text}");
        }
        other => panic!("expected text summary, got {other:?}"),
    }
    match &history[1].content[0] {
        Block::Text { text } => {
            assert!(text.contains("resume packet"), "{text}");
        }
        other => panic!("expected assistant ack, got {other:?}"),
    }
    assert!(matches!(history[2].content[0], Block::ToolUse { .. }));
    match &history[3].content[0] {
        Block::Text { text } => assert_eq!(text, "continue now"),
        other => panic!("expected tail text message, got {other:?}"),
    }
}

#[test]
fn transcript_summary_is_capped() {
    let msgs = vec![Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "x".repeat(SUMMARY_TRANSCRIPT_CAP + 5_000),
        }],
    }];

    let out = render_transcript_for_summary(&msgs, ContextMode::Standard);
    assert!(out.contains("Older transcript content omitted"), "{out}");
    assert!(out.len() < SUMMARY_TRANSCRIPT_CAP + 500, "{}", out.len());
}

#[test]
fn provenance_aliases_legacy_dext_memory_hash_to_recall_hash() {
    let original = SessionProvenance {
        recall_hash: Some("abc123".to_string()),
        ..Default::default()
    };
    let serialized = serde_json::to_string(&original).expect("serialize provenance");
    assert!(serialized.contains("recall_hash"), "{serialized}");
    assert!(!serialized.contains("dext_memory_hash"), "{serialized}");

    let mut legacy = serde_json::to_value(&original).expect("serialize legacy provenance");
    let legacy_object = legacy.as_object_mut().expect("provenance object");
    let hash = legacy_object
        .remove("recall_hash")
        .expect("recall hash field");
    legacy_object.insert("dext_memory_hash".to_string(), hash);

    let parsed: SessionProvenance =
        serde_json::from_value(legacy).expect("parse legacy provenance");
    assert_eq!(parsed.recall_hash.as_deref(), Some("abc123"));
}

#[test]
fn session_analysis_surfaces_provenance_and_verification() {
    let header = SessionHeader {
        version: SESSION_FORMAT_VERSION,
        model: "gpt-5.4".to_string(),
        system: "sys".to_string(),
        provenance: SessionProvenance {
            dext_version: "test-version".to_string(),
            provider: "chatgpt".to_string(),
            api_provider: ApiProvider::ChatGpt,
            model: "gpt-5.4".to_string(),
            thinking_effort: ThinkingEffort::XHigh,
            system_prompt_hash: "abcdef1234567890".to_string(),
            tool_catalog_version: TOOL_CATALOG_VERSION,
            ..Default::default()
        },
        work_ledger: WorkLedger {
            verification: vec![VerificationRecord {
                name: "focused tests".to_string(),
                command: "cargo test focused".to_string(),
                status: "passed".to_string(),
                exit_code: Some(0),
                duration_ms: 10,
                artifact: Some("artifact.json".to_string()),
                validates: Vec::new(),
            }],
            ..Default::default()
        },
        ..Default::default()
    };
    let history = vec![Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "Fix auth retry".to_string(),
        }],
    }];
    let analysis = analyze_session_history(&header, &history);
    let rendered = render_session_analysis(Path::new("session.jsonl"), &header, &analysis);
    assert!(rendered.contains("provider: chatgpt"), "{rendered}");
    assert!(rendered.contains("provenance:"), "{rendered}");
    assert!(rendered.contains("thinking_effort: xhigh"), "{rendered}");
    assert!(rendered.contains("focused tests: passed"), "{rendered}");
}

#[test]
fn latest_log_buffer_is_capped_and_marks_truncation() {
    let input = format!("[1] tool_ok {}", "x".repeat(LATEST_LOG_CAP + 5_000));
    let out = cap_latest_log_buffer(input);
    assert!(out.len() <= LATEST_LOG_CAP, "{}", out.len());
    assert!(out.contains("log_truncated kept latest"), "{out}");
    assert!(out.ends_with('x'), "{out}");
}

#[test]
fn render_limited_lines_shows_latest_entries_with_notice() {
    let items: Vec<String> = (1..=60).map(|i| format!("session-{i:02}")).collect();
    let out = render_limited_lines(&items, 5, true, "sessions");
    assert!(out.contains("session-56"), "{out}");
    assert!(out.contains("session-60"), "{out}");
    assert!(!out.contains("session-01"), "{out}");
    assert!(out.contains("earlier sessions omitted"), "{out}");
}

#[test]
fn render_limited_csv_truncates_with_notice() {
    let items: Vec<String> = (1..=60).map(|i| format!("tool-{i:02}")).collect();
    let out = render_limited_csv(&items, 3, "(none)", "tools");
    assert!(out.starts_with("tool-01, tool-02, tool-03"), "{out}");
    assert!(out.contains("more tools"), "{out}");
}

#[test]
fn summarize_call_bash_collapses_newlines() {
    let summary = summarize_call("bash", &json!({"command": "echo one\n&& echo two"}));
    assert!(!summary.contains('\n'), "{summary}");
    assert!(
        summary.starts_with("bash: echo one && echo two"),
        "{summary}"
    );

    let summary = summarize_call(
        "bash",
        &json!({"command": "set -euo pipefail\ngit show --no-ext-diff --unified=80 e896250 -- src/tui.rs | sed -n '260,620p'"}),
    );
    assert!(
        summary.starts_with("bash: git show --no-ext-diff"),
        "{summary}"
    );
    assert!(!summary.contains("set -euo pipefail"), "{summary}");

    let summary = summarize_call(
        "bash",
        &json!({"command": "set -euo pipefail git show --no-ext-diff --unified=80 e896250 -- src/tui.rs | sed -n '260,620p'"}),
    );
    assert!(
        summary.starts_with("bash: git show --no-ext-diff"),
        "{summary}"
    );
    assert!(!summary.contains("set -euo pipefail"), "{summary}");

    let summary = summarize_call("bash", &json!({"command": "set -euo pipefail"}));
    assert!(summary.starts_with("bash: set -euo pipefail"), "{summary}");
}

#[test]
fn summarize_call_surfaces_invalid_tool_args() {
    let summary = summarize_call("write_file", &json!({}));
    assert!(summary.contains("invalid args"), "{summary}");
    assert!(summary.contains("missing path, content"), "{summary}");
}

#[test]
fn partial_delivery_hint_triggers_only_once_per_turn() {
    let should_emit = orchestrator::should_emit_partial_delivery_hint(false, 1, 2, 0);
    assert!(should_emit, "first qualifying check should emit hint");

    let should_emit_again = orchestrator::should_emit_partial_delivery_hint(true, 1, 2, 0);
    assert!(
        !should_emit_again,
        "once emitted in a turn, hint must not re-emit"
    );
}

#[test]
fn external_failure_counting_does_not_double_count_auth_failures() {
    let mut round_external_failures = 0usize;

    round_external_failures = round_external_failures
        .saturating_add(orchestrator::external_failure_increment(false, true));

    assert_eq!(
        round_external_failures, 1,
        "failed+auth attempt should count as exactly one external failure"
    );
}

#[test]
fn dedupe_cache_short_circuit_preserves_cached_error_semantics_regression() {
    let mut cache: HashMap<String, (String, Option<bool>)> = HashMap::new();
    cache.insert("ok-key".to_string(), ("cached success".to_string(), None));
    cache.insert(
        "err-key".to_string(),
        ("cached failure".to_string(), Some(true)),
    );

    let ok = orchestrator::dedupe_cache_short_circuit(&cache, Some("ok-key"))
        .expect("expected dedupe hit");
    assert_eq!(ok.1, None, "success cache hit should not be marked error");

    let err = orchestrator::dedupe_cache_short_circuit(&cache, Some("err-key"))
        .expect("expected dedupe hit");
    assert_eq!(
        err.1,
        Some(true),
        "error cache hit should preserve error semantics"
    );
}

#[test]
fn subagent_request_inherits_parent_capability_profile() {
    let root = temp_test_dir("subagent-request-profile");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.set_approval_profile(ApprovalProfile::Always);
    agent.set_sandbox_profile(SandboxProfile::DangerFullAccess);
    agent.set_browser_recipe(BrowserRecipe::AgentBrowser);
    agent.set_thinking_effort(ThinkingEffort::High);
    agent.tool_context_profile = ToolContextProfile::Full;
    agent.context_mode = ContextMode::Frugal;
    agent.tool_profile = ToolProfile::Lean;
    agent.set_budget_cap(BudgetCap::parse("25k tokens"));
    agent.privacy.enabled = false;

    let input = json!({
        "task": "survey repo",
        "allowed_tools": ["rg", "read_file"],
        "max_iterations": 7,
    });
    let request = SubagentRequest::from_input(&agent, &input).expect("request");
    assert_eq!(request.approval_profile, ApprovalProfile::Always);
    assert_eq!(request.sandbox_profile, SandboxProfile::DangerFullAccess);
    assert_eq!(request.browser_recipe, BrowserRecipe::AgentBrowser);
    assert_eq!(request.thinking_effort, ThinkingEffort::High);
    assert_eq!(request.context_mode, ContextMode::Frugal);
    assert_eq!(request.tool_context_profile, ToolContextProfile::Frugal);
    assert_eq!(request.tool_profile, ToolProfile::Lean);
    assert!(!tool_name_allowed_in_profile(
        "http",
        ToolContextProfile::Frugal
    ));
    assert_eq!(request.max_iterations, Some(7));
    assert_eq!(
        request.allowed_tools,
        Some(vec!["rg".into(), "read_file".into()])
    );
    assert!(!request.privacy_enabled);
    assert_eq!(request.to_tool_input()["task"], "survey repo");
    assert_eq!(
        request.to_tool_input()["tool_context_profile"],
        json!("frugal")
    );
    assert_eq!(request.to_tool_input()["tool_profile"], json!("lean"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn subagent_detached_artifacts_are_project_scoped() {
    let _guard = env_lock();
    let root = temp_test_dir("subagent-artifacts");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let dext_home = root.join("dext-home");
    unsafe {
        std::env::set_var("DEXT_HOME", &dext_home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }
    let request = SubagentRequest {
        task: "noop".to_string(),
        ..SubagentRequest::default()
    };
    let (input_path, output_path, steer_path) =
        write_subagent_input(&root, &request).expect("write subagent input");
    assert!(
        input_path.starts_with(project_state_dir(&root)),
        "{}",
        input_path.display()
    );
    assert!(
        output_path.starts_with(project_state_dir(&root)),
        "{}",
        output_path.display()
    );
    assert!(input_path.exists(), "{}", input_path.display());
    assert!(output_path.exists(), "{}", output_path.display());
    assert!(steer_path.exists(), "{}", steer_path.display());
    assert_eq!(
        input_path
            .extension()
            .and_then(|e: &std::ffi::OsStr| e.to_str()),
        Some("json")
    );
    assert_eq!(
        output_path
            .extension()
            .and_then(|e: &std::ffi::OsStr| e.to_str()),
        Some("md")
    );
    assert_eq!(
        steer_path
            .extension()
            .and_then(|e: &std::ffi::OsStr| e.to_str()),
        Some("steer")
    );
    let parsed: SubagentRequest =
        serde_json::from_slice(&std::fs::read(&input_path).unwrap()).unwrap();
    assert_eq!(parsed.task, "noop");
    let output = std::fs::read_to_string(&output_path).unwrap();
    assert!(output.contains("# Dext subagent output bundle"), "{output}");
    assert!(output.contains("## Logs"), "{output}");

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(subagent_requests_dir(&root));
    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
}

#[test]
fn current_dext_executable_falls_back_to_path_when_current_exe_missing() {
    let _guard = env_lock();
    let root = temp_test_dir("dext-exe-path-fallback");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create bin dir");
    let dext_path = bin_dir.join("dext");
    std::fs::write(&dext_path, "#!/bin/sh\nexit 0\n").expect("write fake dext");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dext_path).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dext_path, perms).unwrap();
    }
    let old_path = std::env::var_os("PATH");
    unsafe {
        std::env::set_var("PATH", prepend_env_path(&bin_dir));
    }

    let missing_exe = root.join("missing-current-exe");
    let found = current_dext_executable_from(missing_exe).expect("PATH dext fallback");
    assert_eq!(found, dext_path);

    unsafe {
        if let Some(old_path) = old_path {
            std::env::set_var("PATH", old_path);
        } else {
            std::env::remove_var("PATH");
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn subagent_runtime_renders_report_with_quality_gate() {
    let report = SubagentRunReport {
        task: "research".to_string(),
        max_iterations: Some(3),
        iterations: 1,
        calls: 0,
        failed_calls: 0,
        elapsed: std::time::Duration::from_millis(12),
        halted_reason: None,
        traces: Vec::new(),
        final_text: "source inspected: docs\nverification run: none\nfiles touched: none\nuncertainty/open questions: none\nconfidence: medium\nexact recommended edits: none\nremaining risks: none"
            .to_string(),
    };
    let rendered = render_subagent_report(&report);
    assert!(rendered.contains("=== SUBAGENT RESULT ==="), "{rendered}");
    assert!(rendered.contains("(no tool calls)"), "{rendered}");
    assert!(!rendered.contains("quality gate"), "{rendered}");
}

#[test]
fn shell_single_quote_handles_spaces_and_quotes() {
    #[cfg(unix)]
    assert_eq!(shell_single_quote("a b'c"), "'a b'\\''c'");
}

#[test]
fn subagent_quality_gate_requires_structured_handoff_headings() {
    let good = "source inspected: src/main.rs\nverification run: cargo test\nfiles touched: src/main.rs\nuncertainty/open questions: none\nconfidence: high\nexact recommended edits: applied\nremaining risks: TUI manual check";
    assert!(subagent_quality_gate_missing(good).is_empty());

    let bad = "I looked around and it seems fine. No remaining risks were found.";
    let missing = subagent_quality_gate_missing(bad);
    assert!(missing.contains(&"source inspected"), "{missing:?}");
    assert!(missing.contains(&"verification run"), "{missing:?}");
    assert!(missing.contains(&"exact recommended edits"), "{missing:?}");
    assert!(missing.contains(&"remaining risks"), "{missing:?}");
}

#[test]
fn slash_subagent_command_usage_is_registered() {
    let cmd = slash_command_definitions()
        .into_iter()
        .find(|cmd| cmd.name == "subagent")
        .expect("subagent slash command");
    assert!(cmd.usage.starts_with("/subagent <task>"));
    assert!(cmd.description.contains("provider-visible tools"));
}

#[test]
fn hooks_loads_project_hooks_json_by_default() {
    let root = temp_test_dir("hooks-json-default");
    std::fs::write(
        root.join("hooks.json"),
        r#"{"user_prompt":[{"command":"printf pack"}]}"#,
    )
    .expect("write hooks.json");

    let hooks = Hooks::load(&root);
    let out = hooks.fire("user_prompt", "", &[], &[], &root);
    assert_eq!(out.len(), 1);
    assert!(out[0].0.contains("pack"), "{}", out[0].0);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn hooks_capture_is_capped() {
    let root = temp_test_dir("hook-cap");
    let hooks = Hooks {
        pre_tool: vec![Hook {
            tool_match: Some("*".to_string()),
            command: "for i in {1..5000}; do printf x; done".to_string(),
        }],
        ..Default::default()
    };

    let out = hooks.fire("pre_tool", "read_file", &[], &[], &root);
    assert_eq!(out.len(), 1);
    assert!(
        out[0].0.contains("hook stdout capped after"),
        "{}",
        out[0].0
    );
    assert!(
        out[0].0.contains("kept first 2000 and last 2000"),
        "{}",
        out[0].0
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn compose_system_parts_caps_dext_md() {
    let root = temp_test_dir("dext-md-cap");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(
        root.join("DEXT.md"),
        "x".repeat(PROJECT_CONTEXT_CAP + 5_000),
    )
    .expect("write DEXT.md");

    let agent = test_agent(&root);
    let (stable, _env) = agent.compose_system_parts();
    assert!(stable.contains("DEXT.md truncated"), "{stable}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tiny_mode_uses_condensed_prompt_and_slim_env() {
    let root = temp_test_dir("tiny-system-prompt");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(root.join("DEXT.md"), "project guidance".repeat(500)).expect("write DEXT.md");
    let mut agent = test_agent(&root);
    agent.context_mode = ContextMode::Tiny;
    agent.system = TINY_SYSTEM.to_string();
    agent.work_ledger.objective = "keep it tiny".to_string();

    let (stable, env) = agent.compose_system_parts();
    assert!(stable.starts_with(TINY_SYSTEM), "{stable}");
    assert!(stable.contains("Native tools before bash"), "{stable}");
    assert!(stable.len() < 2_500, "{}", stable.len());
    assert!(env.contains("context=tiny"), "{env}");
    assert!(env.contains("compact="), "{env}");
    assert!(!env.contains("## Project todos"), "{env}");
    assert!(env.len() < 1_200, "{}", env.len());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn compose_system_parts_keeps_standard_env_compact_and_caps_ledger() {
    let root = temp_test_dir("compact-env-ledger");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.work_ledger.objective = "keep prompt small".to_string();
    agent.work_ledger.pending = (0..8)
        .map(|idx| {
            format!(
                "pending item with enough detail to grow the ledger {idx}: {}",
                "x".repeat(500)
            )
        })
        .collect();
    agent.provider_health.providers.insert(
        "chatgpt".to_string(),
        ProviderHealthState {
            auth: "present".to_string(),
            mode: Some("chatgpt-responses".to_string()),
            last_error: Some("temporary upstream error ".repeat(2000)),
            retry_after: None,
            disabled_for_turn: false,
            consecutive_server_errors: 0,
        },
    );

    let (_stable, env) = agent.compose_system_parts();
    assert!(env.starts_with("## Environment\ncwd="), "{env}");
    assert!(
        env.contains("active_history_compact_threshold_chars="),
        "{env}"
    );
    assert!(!env.contains("session_event_refs"), "{env}");
    assert!(!env.contains("auth_source"), "{env}");
    assert!(
        env.contains("work ledger trimmed for prompt budget"),
        "{env}"
    );
    assert!(env.contains("last_error="), "{env}");
    assert!(
        env.len() < 4_000,
        "standard environment should stay compact, got {} bytes: {env}",
        env.len()
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn compose_system_parts_includes_typed_shelf_registry_summary() {
    let _guard = env_lock();
    let root = temp_test_dir("typed-shelf-system-summary");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let shelf_dir = root.join(".dext/shelves/community");
    std::fs::create_dir_all(&shelf_dir).expect("create shelf dir");
    std::fs::write(
        shelf_dir.join("shelf.json"),
        r#"{
  "id": "community",
  "name": "Community",
  "description": "shared typed abilities",
  "mode": "always",
  "packs": [{
    "id": "research",
    "name": "Research",
    "version": "0.1.0",
    "description": "research helpers",
    "abilities": [{"ability": "tool", "name": "search", "description": "project search", "schema": {"type": "object"}, "grants": ["read"], "exposure": "on_demand"}, {"ability": "context", "name": "notes", "description": "curated notes", "budget": 1024}]
  }]
}"#,
    )
    .expect("write shelf manifest");
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let agent = test_agent(&root);
    let (_stable, env) = agent.compose_system_parts();
    assert!(env.contains("## Dext shelves"), "{env}");
    assert!(
        env.contains("Typed shelf registry: 1 shelf(s), 2 resolved ability metadata entries."),
        "{env}"
    );
    assert!(
        env.contains("tool:search (community/research, project search)"),
        "{env}"
    );
    assert!(
        env.contains("context:notes (community/research, curated notes, budget 1024)"),
        "{env}"
    );
    assert!(env.contains("not extra provider-visible tools"), "{env}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn default_tool_profile_is_lean_for_prompt_budget() {
    assert_eq!(ToolProfile::default(), ToolProfile::Lean);
    assert_eq!(ToolProfile::parse("default"), Some(ToolProfile::Lean));
}

#[test]
fn slash_tools_switches_specialized_tool_visibility() {
    let root = temp_test_dir("slash-tools");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    assert!(agent.tools.iter().all(|t| t.name != "jq"));
    assert_eq!(handle_slash("/tools full", &mut agent), Some(true));
    assert_eq!(agent.tool_context_profile(), ToolContextProfile::Full);
    assert!(agent.tools.iter().any(|t| t.name == "jq"));

    assert_eq!(handle_slash("/context frugal", &mut agent), Some(true));
    assert_eq!(handle_slash("/tools default", &mut agent), Some(true));
    assert_eq!(agent.tool_context_profile(), ToolContextProfile::Frugal);
    assert!(agent.tools.iter().all(|t| t.name != "jq"));
    let slash = drain_events(&mut rx)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    assert!(slash.contains("tools -> full"), "{slash}");
    assert!(slash.contains("context mode -> frugal"), "{slash}");
    assert!(slash.contains("pins tools frugal"), "{slash}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn slash_system_displays_composed_prompt_with_project_context() {
    let root = temp_test_dir("slash-system-composed");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(root.join("DEXT.md"), "## Local\n- slash system context")
        .expect("write DEXT.md");
    let mut agent = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    assert_eq!(handle_slash("/system", &mut agent), Some(true));
    let slash = drain_events(&mut rx)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .unwrap_or_default();
    assert!(slash.contains("test-system"), "{slash}");
    assert!(slash.contains("Project context (DEXT.md"), "{slash}");
    assert!(slash.contains("slash system context"), "{slash}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn compose_system_parts_includes_recall_md() {
    let root = temp_test_dir("recall-md");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    std::fs::write(
        root.join("recall.md"),
        "## Decisions\n- keep tool evidence concise",
    )
    .expect("write recall.md");

    let agent = test_agent(&root);
    let (stable, _env) = agent.compose_system_parts();
    assert!(stable.contains("Recall (recall.md"), "{stable}");
    assert!(stable.contains("keep tool evidence concise"), "{stable}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn context_mode_parse_includes_tiny_without_aliasing_frugal() {
    assert_eq!(ContextMode::parse("tiny"), Some(ContextMode::Tiny));
    assert_eq!(ContextMode::parse("skinny"), Some(ContextMode::Tiny));
    assert_eq!(ContextMode::parse("frugal"), Some(ContextMode::Frugal));
    assert_eq!(ContextMode::Tiny.as_str(), "tiny");
    assert!(ContextMode::Tiny.is_frugal());
    assert!(ContextMode::Tiny.is_tiny());
    assert!(!ContextMode::Frugal.is_tiny());
    assert_eq!(
        ToolContextProfile::parse("full"),
        Some(ToolContextProfile::Full)
    );
    assert_eq!(
        ToolContextProfile::parse("standard"),
        Some(ToolContextProfile::Default)
    );
    assert_eq!(
        ToolContextProfile::parse("frugal"),
        Some(ToolContextProfile::Frugal)
    );
    assert_eq!(ToolContextProfile::parse_selectable("frugal"), None);
    assert_eq!(
        ToolContextProfile::Full.effective(ContextMode::Frugal),
        ToolContextProfile::Frugal
    );
    assert_eq!(
        ToolContextProfile::Frugal.effective(ContextMode::Standard),
        ToolContextProfile::Default
    );
}

#[test]
fn tiny_and_frugal_systems_preserve_tool_protocol_guardrails_without_standard_prompt_change() {
    assert!(
        !DEFAULT_SYSTEM.contains("Never print raw tool syntax"),
        "standard prompt should stay unchanged: {DEFAULT_SYSTEM}"
    );

    let root = temp_test_dir("frugal-tool-protocol-note");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.context_mode = ContextMode::Frugal;
    let (frugal_stable, _env) = agent.compose_system_parts();
    assert!(
        frugal_stable.contains("actual provider tool calls"),
        "{frugal_stable}"
    );
    assert!(
        frugal_stable.contains("Never print raw tool syntax"),
        "{frugal_stable}"
    );
    assert!(
        TINY_SYSTEM.contains("real tool calls only"),
        "{TINY_SYSTEM}"
    );
    assert!(
        TINY_SYSTEM.contains("prefill the TUI input"),
        "{TINY_SYSTEM}"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn context_window_and_history_budget_are_model_aware_and_overridable() {
    let _guard = env_lock();
    clear_cached_local_llama_context_windows();
    unsafe {
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
        std::env::remove_var("DEXT_MAX_HISTORY_CHARS");
    }

    assert_eq!(model_context_window("demo-128k"), 128_000);
    assert_eq!(
        history_char_budget_with_override("demo-128k", None, ContextMode::Standard),
        460_800
    );
    assert_eq!(
        active_history_char_budget_with_override("demo-128k", None, ContextMode::Standard),
        409_600
    );
    assert_eq!(
        history_char_budget_with_override("tiny-1k", None, ContextMode::Standard),
        HISTORY_CHAR_BUDGET_MIN
    );
    assert_eq!(
        active_history_char_budget_with_override("tiny-1k", None, ContextMode::Standard),
        HISTORY_CHAR_BUDGET_MIN
    );
    assert_eq!(
        history_char_budget_with_override("huge-1m", None, ContextMode::Standard),
        3_600_000
    );
    assert_eq!(
        active_history_char_budget_with_override("huge-1m", None, ContextMode::Standard),
        3_200_000
    );

    assert_eq!(
        history_char_budget_with_override("demo-128k", None, ContextMode::Frugal),
        60_000
    );
    assert_eq!(
        active_history_char_budget_with_override("demo-128k", None, ContextMode::Frugal),
        60_000
    );
    assert_eq!(
        history_char_budget_with_override("qwen2.5-coder-7b", None, ContextMode::Tiny),
        32_000
    );
    assert_eq!(model_context_window("qwen2.5-coder-7b"), 32_000);
    assert_eq!(
        active_history_char_budget_with_override("qwen2.5-coder-7b", None, ContextMode::Tiny),
        32_000
    );
    assert_eq!(
        history_char_budget_with_override("tiny-1k", None, ContextMode::Tiny),
        8_000
    );
    assert_eq!(
        active_history_char_budget_with_override("tiny-1k", None, ContextMode::Tiny),
        8_000
    );

    unsafe {
        std::env::set_var("DEXT_CONTEXT_WINDOW_TOKENS", "64000");
    }
    assert_eq!(model_context_window("demo-128k"), 64_000);

    unsafe {
        std::env::set_var("DEXT_MAX_HISTORY_CHARS", "77777");
    }
    assert_eq!(
        history_char_budget_with_override("any-model", None, ContextMode::Standard),
        77_777
    );
    assert_eq!(
        active_history_char_budget_with_override("any-model", None, ContextMode::Standard),
        77_777
    );

    unsafe {
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
        std::env::remove_var("DEXT_MAX_HISTORY_CHARS");
    }
}

#[test]
fn context_window_reads_from_provider_catalog() -> Result<()> {
    // Resolution order: env > runtime cache > catalog override > built-in catalog > family heuristic > 200k fallback.
    // This test isolates the catalog path: write a providers.json with a custom
    // provider and per-model override, then confirm model_context_window reads it.
    let _guard = env_lock();
    clear_cached_local_llama_context_windows();
    let root = temp_test_dir("ctx-window-catalog");
    let root = std::fs::canonicalize(&root)?;
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }

    let result = (|| -> Result<()> {
        let mut per_model = std::collections::HashMap::new();
        per_model.insert("special-1m-model".to_string(), 1_000_000u64);
        let catalog = ProviderCatalog {
            version: 1,
            active_provider: "custom".to_string(),
            providers: vec![ProviderProfile {
                id: "custom".to_string(),
                display_name: "Custom".to_string(),
                api_provider: ApiProvider::OpenAi,
                base_url: "https://example.invalid".to_string(),
                default_model: "custom-default".to_string(),
                models: vec!["custom-default".to_string(), "special-1m-model".to_string()],
                env_vars: Vec::new(),
                requires_api_key: false,
                login_url: None,
                oauth_flow: None,
                notes: None,
                context_window: Some(333_000),
                model_context_windows: per_model,
            }],
        };
        save_provider_catalog(&catalog)?;

        // Provider-default applies for a model listed but not per-model-overridden.
        assert_eq!(model_context_window("custom-default"), 333_000);
        // Per-model override wins over provider default.
        assert_eq!(model_context_window("special-1m-model"), 1_000_000);
        // Family heuristics beat a catalog provider default when a foreign built-in model
        // was accidentally saved under the wrong provider.
        let mut polluted = catalog.clone();
        polluted.providers[0].models.push("gpt-4o".to_string());
        save_provider_catalog(&polluted)?;
        assert_eq!(model_context_window("gpt-4o"), 128_000);
        save_provider_catalog(&catalog)?;
        // Env var beats the catalog.
        unsafe {
            std::env::set_var("DEXT_CONTEXT_WINDOW_TOKENS", "42000");
        }
        assert_eq!(model_context_window("custom-default"), 42_000);
        unsafe {
            std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
        }
        // Model unknown to any provider falls through to heuristic + default.
        assert_eq!(model_context_window("unknown-model"), 200_000);
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn builtin_chatgpt_profile_declares_codex_context_window() {
    // The 272k window for Codex must come from provider profile data, not hardcoded
    // in model_context_window(). This pins the data-driven default.
    let profile = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "chatgpt")
        .expect("chatgpt profile");
    assert_eq!(
        profile.context_window,
        Some(272_000),
        "ChatGPT default context window must come from the catalog as 272k (Codex)"
    );
    assert_eq!(profile.default_model, "gpt-5.4");
    // gpt-4.1 override should still be 1M via model_context_windows.
    assert_eq!(
        profile.model_context_windows.get("gpt-4.1").copied(),
        Some(1_000_000)
    );
}

#[test]
fn model_context_window_uses_builtin_chatgpt_profile_when_catalog_isolated() -> Result<()> {
    let _guard = env_lock();
    clear_cached_local_llama_context_windows();
    let root = temp_test_dir("ctx-window-builtin-chatgpt");
    let root = std::fs::canonicalize(&root)?;
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }

    let result = {
        assert_eq!(model_context_window("gpt-5.4"), 272_000);
        Ok(())
    };

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn llama_context_parser_prefers_runtime_ctx_fields() {
    assert_eq!(
        parse_llama_context_window(&json!({
            "model": {"n_ctx_train": 262144},
            "default_generation_settings": {"n_ctx": 30000}
        })),
        Some(30_000)
    );
    assert_eq!(
        parse_llama_context_window(&json!({
            "data": [{"id": "qwen", "context_length": 32000}]
        })),
        Some(32_000)
    );
}

#[test]
fn local_llama_context_cache_overrides_builtin_local_default() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }
    clear_cached_local_llama_context_windows();
    assert_eq!(
        model_context_window("qwen3.5-9b"),
        crate::provider::DEFAULT_LOCAL_CONTEXT_WINDOW_TOKENS
    );
    crate::provider::set_cached_local_llama_context_window("qwen3.5-9b", 30_000);
    assert_eq!(model_context_window("qwen3.5-9b"), 30_000);
    unsafe {
        std::env::set_var("DEXT_CONTEXT_WINDOW", "64000");
    }
    assert_eq!(model_context_window("qwen3.5-9b"), 64_000);
    unsafe {
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }
    clear_cached_local_llama_context_windows();
}

#[test]
fn local_llama_runtime_probe_updates_model_context_window() -> Result<()> {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }
    clear_cached_local_llama_context_windows();
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = std::thread::spawn(move || {
        for _ in 0..2 {
            let (mut stream, _) = listener.accept().expect("accept");
            let mut request = [0u8; 1024];
            let n = stream.read(&mut request).expect("read request");
            let request = String::from_utf8_lossy(&request[..n]);
            let (status, body) = if request.starts_with("GET /props ") {
                (
                    "200 OK",
                    r#"{"default_generation_settings":{"n_ctx":30000},"model":{"n_ctx_train":262144}}"#,
                )
            } else {
                ("404 Not Found", r#"{}"#)
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
    });

    let tokens = refresh_local_llama_context_window(
        "local",
        ApiProvider::OpenAi,
        &format!("http://{addr}"),
        "qwen3.5-9b",
    );
    assert_eq!(tokens, Some(30_000));
    assert_eq!(model_context_window("qwen3.5-9b"), 30_000);
    let _ = refresh_local_llama_context_window(
        "local",
        ApiProvider::OpenAi,
        &format!("http://{addr}"),
        "qwen2.5-coder-7b",
    );
    server.join().expect("server thread");
    clear_cached_local_llama_context_windows();
    Ok(())
}

#[test]
fn builtin_provider_merge_preserves_context_window_overrides() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "chatgpt")
        .expect("chatgpt profile");
    let mut stored = builtin.clone();
    stored.default_model = "gpt-5-4".to_string();
    stored.context_window = Some(300_000);
    stored
        .model_context_windows
        .insert("gpt-5-4".to_string(), 350_000);
    stored
        .model_context_windows
        .insert("gpt-5.4-mini".to_string(), 180_000);
    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(merged.default_model, "gpt-5.4");
    assert_eq!(merged.context_window, Some(300_000));
    assert_eq!(merged.model_context_windows.get("gpt-5.4"), Some(&350_000));
    assert_eq!(
        merged.model_context_windows.get("gpt-5.4-mini"),
        Some(&180_000)
    );
}

#[test]
fn builtin_provider_merge_drops_foreign_builtin_models_from_non_chatgpt_profiles() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "glm")
        .expect("glm profile");
    let mut stored = builtin.clone();
    stored.models.push("gpt-5.4".to_string());
    let merged = merge_provider_profile(builtin, stored);
    assert!(!merged.models.iter().any(|m| m == "gpt-5.4"));
}

#[test]
fn release_registered_locks_drops_files_and_registry() -> Result<()> {
    // Hold the env_lock so parallel tests' locks aren't in the shared registry
    // when we sweep it.
    let _guard = env_lock();
    let root = temp_test_dir("lock-cleanup-registry");
    let root = std::fs::canonicalize(&root)?;
    let lock = ProjectStateLock::acquire(&root)?;
    let lock_path = lock.path.clone();
    assert!(lock_path.exists());

    std::mem::forget(lock);
    release_registered_locks();
    assert!(
        !lock_path.exists(),
        "lock file should be removed by registry"
    );

    let fresh = ProjectStateLock::acquire(&root)?;
    drop(fresh);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn log_rotation_archives_when_enabled() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("log-rotation");
    let path = root.join("latest.log");
    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::set_var("DEXT_LOG_ARCHIVES", "3") };
    let result = (|| -> Result<()> {
        let big = "y".repeat(LATEST_LOG_CAP);
        std::fs::write(&path, &big)?;
        append_log_line(&path, "fresh-line");
        let archive1 = path.with_extension("log.1");
        assert!(archive1.exists(), "archive .1 should exist");
        let after = std::fs::read_to_string(&path)?;
        assert!(after.contains("fresh-line"), "{after}");
        assert!(after.len() < LATEST_LOG_CAP, "current log should be small");
        Ok(())
    })();
    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::remove_var("DEXT_LOG_ARCHIVES") };
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn log_rotation_defaults_to_truncation_only() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("log-no-rotation");
    let path = root.join("latest.log");
    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::remove_var("DEXT_LOG_ARCHIVES") };
    std::fs::create_dir_all(&root)?;
    let big = "y".repeat(LATEST_LOG_CAP);
    std::fs::write(&path, &big)?;
    append_log_line(&path, "fresh-line");
    assert!(!path.with_extension("log.1").exists());
    let after = std::fs::read_to_string(&path)?;
    assert!(after.contains("log_truncated kept latest"), "{after}");
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn checkpoint_latest_session_writes_session_and_log() {
    let _guard = env_lock();
    let root = temp_test_dir("checkpoint-session");
    let sessions = root.join("sessions");
    let logs = root.join("logs");

    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::set_var("DEXT_SESSIONS_DIR", &sessions);
        std::env::set_var("DEXT_LOGS_DIR", &logs);
    }

    let result = (|| -> Result<()> {
        let root = std::fs::canonicalize(&root)?;
        let mut agent = test_agent(&root);
        agent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "hello".to_string(),
            }],
        });
        agent.checkpoint_latest_session("test_reason");

        let session_path = sessions.join("_latest.jsonl");
        let log_path = logs.join("latest.log");
        assert!(session_path.exists(), "missing {}", session_path.display());
        assert!(log_path.exists(), "missing {}", log_path.display());
        let log = std::fs::read_to_string(&log_path)?;
        assert!(log.contains("session_checkpoint"), "{log}");

        let mut session_entries: Vec<String> = std::fs::read_dir(&sessions)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        let mut log_entries: Vec<String> = std::fs::read_dir(&logs)?
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .collect();
        session_entries.sort();
        log_entries.sort();
        assert_eq!(session_entries, vec!["_latest.jsonl"]);
        assert_eq!(log_entries, vec!["latest.log"]);
        Ok(())
    })();

    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result.expect("checkpoint latest session");
}

#[test]
fn max_concurrent_builtins_reads_env_and_defaults() {
    let _guard = env_lock();
    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::remove_var("DEXT_MAX_CONCURRENT_BUILTINS");
    }
    assert_eq!(max_concurrent_builtins(), 8);
    // Safe: guarded by env_lock.
    unsafe {
        std::env::set_var("DEXT_MAX_CONCURRENT_BUILTINS", "3");
    }
    assert_eq!(max_concurrent_builtins(), 3);
    // Safe: guarded by env_lock.
    unsafe {
        std::env::set_var("DEXT_MAX_CONCURRENT_BUILTINS", "0");
    }
    assert_eq!(
        max_concurrent_builtins(),
        8,
        "zero should be rejected and fall back to default"
    );
    // Safe: guarded by env_lock.
    unsafe {
        std::env::set_var("DEXT_MAX_CONCURRENT_BUILTINS", "garbage");
    }
    assert_eq!(
        max_concurrent_builtins(),
        8,
        "non-numeric should fall back to default"
    );
    // Safe: guarded by env_lock.
    unsafe {
        std::env::remove_var("DEXT_MAX_CONCURRENT_BUILTINS");
    }
}

#[test]
fn parse_cli_options_supports_no_session_cd_output_and_file_args() -> Result<()> {
    let root = temp_test_dir("cli-parse");
    let task_file = root.join("task.txt");
    std::fs::write(&task_file, "from file")?;

    let opts = parse_cli_options(vec![
        "--no-session".to_string(),
        "--cd".to_string(),
        root.display().to_string(),
        "--output=stream-json".to_string(),
        "--no-trust".to_string(),
        "--fork".to_string(),
        "--budget=250k tokens".to_string(),
        "--approval=auto-read".to_string(),
        "--sandbox-profile=read-only".to_string(),
        "--browser=agent-browser".to_string(),
        "--frugal".to_string(),
        "--tiny".to_string(),
        "--tool-context-profile=full".to_string(),
        "--tool-profile=default".to_string(),
        format!("@{}", task_file.display()),
        "tail".to_string(),
    ])?;

    assert!(opts.no_session);
    assert!(opts.no_trust_mode);
    assert!(!opts.trust_mode);
    assert!(opts.fork);
    assert_eq!(opts.cd, Some(root.clone()));
    assert_eq!(opts.output, OutputMode::StreamJson);
    assert_eq!(opts.budget_cap.and_then(|cap| cap.tokens), Some(250_000));
    assert_eq!(opts.approval_profile, Some(ApprovalProfile::AutoRead));
    assert_eq!(opts.sandbox_profile, Some(SandboxProfile::ReadOnly));
    assert_eq!(opts.browser_recipe, Some(BrowserRecipe::AgentBrowser));
    assert_eq!(opts.context_mode, Some(ContextMode::Tiny));
    assert_eq!(opts.tool_context_profile, Some(ToolContextProfile::Full));
    assert_eq!(opts.tool_profile, Some(ToolProfile::Lean));
    assert_eq!(
        opts.positional,
        vec!["from file".to_string(), "tail".to_string()]
    );
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn tiny_context_mode_sets_distinct_banner_and_system_prompt() {
    let root = temp_test_dir("tiny-mode-banner");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.system = DEFAULT_SYSTEM.to_string();

    agent.set_context_mode(ContextMode::Tiny);

    assert!(agent.context_mode.is_tiny());
    assert_eq!(agent.system, TINY_SYSTEM);
    let tiny_line = context_mode_startup_line(agent.context_mode).expect("tiny context line");
    assert!(tiny_line.contains("tiny mode"), "{tiny_line}");
    assert!(!tiny_line.contains("frugal mode"), "{tiny_line}");

    agent.set_context_mode(ContextMode::Frugal);
    let frugal_line = context_mode_startup_line(agent.context_mode).expect("frugal context line");
    assert!(frugal_line.contains("frugal mode"), "{frugal_line}");
    assert!(!frugal_line.contains("tiny mode"), "{frugal_line}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn parse_cli_options_accepts_tiny_alias_without_positional_leak() -> Result<()> {
    let opts = parse_cli_options(vec!["--tiny".to_string(), "do task".to_string()])?;

    assert_eq!(opts.context_mode, Some(ContextMode::Tiny));
    assert_eq!(opts.tool_profile, Some(ToolProfile::Lean));
    assert_eq!(opts.thinking_effort, Some(ThinkingEffort::Medium));
    assert_eq!(opts.positional, vec!["do task".to_string()]);
    Ok(())
}

#[test]
fn parse_cli_options_still_accepts_context_mode_tiny() -> Result<()> {
    let opts = parse_cli_options(vec!["--context-mode=tiny".to_string()])?;

    assert_eq!(opts.context_mode, Some(ContextMode::Tiny));
    assert!(opts.positional.is_empty());
    Ok(())
}

#[test]
fn parse_cli_options_trust_flags_last_one_wins() -> Result<()> {
    let opts = parse_cli_options(vec!["--no-trust".to_string(), "--trust".to_string()])?;
    assert!(opts.trust_mode);
    assert!(!opts.no_trust_mode);

    let opts = parse_cli_options(vec!["--trust".to_string(), "--no-trust".to_string()])?;
    assert!(!opts.trust_mode);
    assert!(opts.no_trust_mode);
    Ok(())
}

#[test]
fn env_flag_default_defaults_and_honors_false_values() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_TEST_FLAG_DEFAULT");
    }
    assert!(env_flag_default("DEXT_TEST_FLAG_DEFAULT", true));
    assert!(!env_flag_default("DEXT_TEST_FLAG_DEFAULT", false));

    unsafe {
        std::env::set_var("DEXT_TEST_FLAG_DEFAULT", "0");
    }
    assert!(!env_flag_default("DEXT_TEST_FLAG_DEFAULT", true));

    unsafe {
        std::env::set_var("DEXT_TEST_FLAG_DEFAULT", "yes");
    }
    assert!(env_flag_default("DEXT_TEST_FLAG_DEFAULT", false));

    unsafe {
        std::env::remove_var("DEXT_TEST_FLAG_DEFAULT");
    }
}

#[test]
fn no_session_agent_skips_checkpoints_and_state_lock() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("no-session");
    let root = std::fs::canonicalize(&root)?;
    let sessions = root.join("sessions");
    let logs = root.join("logs");
    unsafe {
        std::env::set_var("DEXT_SESSIONS_DIR", &sessions);
        std::env::set_var("DEXT_LOGS_DIR", &logs);
    }

    let result = {
        let mut agent = test_agent(&root);
        agent.session_enabled = false;
        agent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "hello".to_string(),
            }],
        });
        agent.checkpoint_latest_session("no_session_test");
        assert!(!agent.latest_session_path.exists());
        assert!(!agent.latest_log_path.exists());
        Ok(())
    };

    unsafe {
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn final_objective_warning_lists_unresolved_checkpoints() {
    let objective = orchestrator::ObjectiveTracker::from_user_prompt("Implement it and test it");
    let history = vec![Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "Implemented the change.".to_string(),
        }],
    }];
    let warning = final_objective_warning(&objective, &history).expect("warning");
    assert!(warning.contains("run verification checks"), "{warning}");
}

#[test]
fn final_objective_warning_is_suppressed_until_runtime_reminder_was_used() {
    let root = temp_test_dir("final-warning-suppressed");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let objective = orchestrator::ObjectiveTracker::from_user_prompt("Implement it and test it");
    agent.update_work_ledger_from_objective(&objective);
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "Implemented the change.".to_string(),
        }],
    });

    let mut objective_warning_emitted = false;
    let coverage = objective.assess_history(&agent.history);
    agent.sync_work_ledger_with_objective_coverage(&coverage);
    if !coverage.unresolved.is_empty() && objective_warning_emitted {
        agent
            .work_ledger
            .blocked
            .push(final_objective_warning_from_coverage(&coverage));
    }
    assert!(agent.work_ledger.blocked.is_empty());

    objective_warning_emitted = true;
    let coverage = objective.assess_history(&agent.history);
    agent.sync_work_ledger_with_objective_coverage(&coverage);
    if !coverage.unresolved.is_empty() && objective_warning_emitted {
        agent
            .work_ledger
            .blocked
            .push(final_objective_warning_from_coverage(&coverage));
    }
    assert_eq!(agent.work_ledger.blocked.len(), 1);
    assert!(agent.work_ledger.blocked[0].contains("run verification checks"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn json_sink_serializes_agent_events() {
    let ev = AgentEvent::ToolCallStart {
        call_id: "call-1".to_string(),
        name: "bash".to_string(),
        summary: "bash: echo ok".to_string(),
    };
    let value = serde_json::to_value(ev).expect("serialize event");
    assert_eq!(value["event"], "tool_call_start");
    assert_eq!(value["data"]["name"], "bash");
}

#[test]
fn turn_diagnostics_serializes_runtime_observability() {
    let ev = AgentEvent::TurnDiagnostics {
        provider: "chatgpt".to_string(),
        api_family: "chatgpt-responses".to_string(),
        auth_source: "auth:chatgpt".to_string(),
        model: "gpt-5".to_string(),
        context_window: Some(272_000),
        last_retry_reason: Some("429".to_string()),
        workaround_fired: true,
        turn_duration_ms: Some(123),
        context_mode: Some(ContextMode::Frugal),
        tool_profile: Some("frugal:lean".to_string()),
        compacted: Some(true),
    };
    let value = serde_json::to_value(ev).expect("serialize event");
    assert_eq!(value["event"], "turn_diagnostics");
    assert_eq!(value["data"]["context_window"], 272_000);
    assert_eq!(value["data"]["turn_duration_ms"], 123);
    assert_eq!(value["data"]["context_mode"], "Frugal");
    assert_eq!(value["data"]["tool_profile"], "frugal:lean");
    assert_eq!(value["data"]["compacted"], true);
}

#[tokio::test]
async fn builtin_semaphore_caps_concurrency() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let sem = Arc::new(tokio::sync::Semaphore::new(3));
    let live = Arc::new(AtomicUsize::new(0));
    let peak = Arc::new(AtomicUsize::new(0));

    let mut handles = Vec::new();
    for _ in 0..20 {
        let sem = sem.clone();
        let live = live.clone();
        let peak = peak.clone();
        handles.push(tokio::spawn(async move {
            let _p = sem.acquire_owned().await.unwrap();
            let now = live.fetch_add(1, Ordering::SeqCst) + 1;
            peak.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(30)).await;
            live.fetch_sub(1, Ordering::SeqCst);
        }));
    }
    for h in handles {
        h.await.unwrap();
    }
    assert!(
        peak.load(Ordering::SeqCst) <= 3,
        "peak concurrency {} exceeded cap 3",
        peak.load(Ordering::SeqCst)
    );
}

#[test]
fn usage_total_and_input_tokens_match_status_semantics() {
    let usage = Usage {
        input: 120_000,
        output: 5_400,
        cache_create: 2_000,
        cache_read: 40_000,
        cost_usd: None,
    };

    assert_eq!(usage.actual_input_tokens(), 120_000);
    assert_eq!(usage.cached_input_tokens(), 42_000);
    assert_eq!(usage.context_tokens(), 167_400);
    assert_eq!(usage.total_tokens(), 167_400);
    assert!(usage.line().contains("in=120000"));
    assert!(usage.line().contains("cached=42000"));
    assert!(usage.line().contains("total=167400"));
}

#[test]
fn anthropic_usage_parse_keeps_native_uncached_input() {
    let usage = Usage::parse(&json!({
        "input_tokens": 1000,
        "output_tokens": 50,
        "cache_creation_input_tokens": 100,
        "cache_read_input_tokens": 300
    }));

    assert_eq!(usage.actual_input_tokens(), 1000);
    assert_eq!(usage.cached_input_tokens(), 400);
    assert_eq!(usage.context_tokens(), 1450);
}

#[test]
fn anthropic_compatible_prompt_usage_subtracts_cache_from_total_prompt() {
    let usage = Usage::parse(&json!({
        "prompt_tokens": 1000,
        "completion_tokens": 50,
        "cache_creation_input_tokens": 100,
        "cache_read_input_tokens": 300
    }));

    assert_eq!(usage.actual_input_tokens(), 600);
    assert_eq!(usage.cached_input_tokens(), 400);
    assert_eq!(usage.context_tokens(), 1050);
}

#[test]
fn openai_usage_parse_splits_cached_prompt_tokens() {
    let usage = Usage::parse_openai(&json!({
        "prompt_tokens": 1000,
        "completion_tokens": 50,
        "prompt_tokens_details": {"cached_tokens": 300}
    }));

    assert_eq!(usage.actual_input_tokens(), 700);
    assert_eq!(usage.cache_read, 300);
    assert_eq!(usage.context_tokens(), 1050);
}

#[test]
fn openai_usage_parse_splits_prompt_cache_hit_and_miss_tokens() {
    let usage = Usage::parse_openai(&json!({
        "prompt_cache_hit_tokens": 300,
        "prompt_cache_miss_tokens": 700,
        "completion_tokens": 50
    }));

    assert_eq!(usage.actual_input_tokens(), 700);
    assert_eq!(usage.cache_read, 300);
    assert_eq!(usage.context_tokens(), 1050);
}

#[test]
fn openai_usage_parse_accepts_direct_cost_and_cache_create() {
    let usage = Usage::parse_openai(&json!({
        "input_tokens": 1000,
        "output_tokens": 50,
        "cache_creation_input_tokens": 100,
        "cache_read_input_tokens": 300,
        "cost_usd": 0.0123
    }));

    assert_eq!(usage.actual_input_tokens(), 600);
    assert_eq!(usage.cache_create, 100);
    assert_eq!(usage.cache_read, 300);
    assert_eq!(usage.cost_usd, Some(0.0123));
    assert_eq!(usage.estimated_cost_usd(), 0.0123);
}

#[test]
fn local_llama_timings_parse_cached_prefix_and_delta_prompt() {
    let usage = Usage::parse_openai_timings(&json!({
        "cache_n": 25869,
        "prompt_n": 32,
        "predicted_n": 64
    }))
    .expect("timings usage");

    assert_eq!(usage.actual_input_tokens(), 32);
    assert_eq!(usage.cache_read, 25_869);
    assert_eq!(usage.output, 64);
    assert_eq!(usage.context_tokens(), 25_965);
}

#[test]
fn usage_pricing_for_local_provider_is_zero_cost() {
    let pricing = usage_pricing_for(
        "local",
        ApiProvider::OpenAi,
        "http://127.0.0.1:8080",
        "qwen3.5-9b",
    );
    let usage = Usage {
        input: 32,
        output: 64,
        cache_create: 0,
        cache_read: 25_869,
        cost_usd: None,
    };

    assert_eq!(pricing.estimate(usage), 0.0);
}

#[test]
fn usage_pricing_env_override_controls_budget_estimate() {
    let _guard = env_lock();
    unsafe {
        std::env::set_var("DEXT_INPUT_USD_PER_MTOK", "2");
        std::env::set_var("DEXT_OUTPUT_USD_PER_MTOK", "4");
        std::env::set_var("DEXT_CACHE_READ_USD_PER_MTOK", "0.5");
        std::env::set_var("DEXT_CACHE_CREATE_USD_PER_MTOK", "1");
    }
    let pricing = usage_pricing_for(
        "openai",
        ApiProvider::OpenAi,
        "https://api.openai.com",
        "unknown",
    );
    let usage = Usage {
        input: 1_000_000,
        output: 2_000_000,
        cache_create: 3_000_000,
        cache_read: 4_000_000,
        cost_usd: None,
    };
    assert_eq!(pricing.estimate(usage), 15.0);

    unsafe {
        std::env::remove_var("DEXT_INPUT_USD_PER_MTOK");
        std::env::remove_var("DEXT_OUTPUT_USD_PER_MTOK");
        std::env::remove_var("DEXT_CACHE_READ_USD_PER_MTOK");
        std::env::remove_var("DEXT_CACHE_CREATE_USD_PER_MTOK");
    }
}

#[test]
fn usage_add_drops_exact_cost_when_mixing_unpriced_nonzero_usage() {
    let mut usage = Usage {
        input: 1_000,
        output: 100,
        cache_create: 0,
        cache_read: 0,
        cost_usd: Some(0.01),
    };
    usage.add(Usage {
        input: 1_000,
        output: 100,
        cache_create: 0,
        cache_read: 0,
        cost_usd: None,
    });

    assert_eq!(usage.cost_usd, None);
}

#[test]
fn usage_fallback_estimates_missing_output_when_input_usage_is_present() {
    let root = temp_test_dir("usage-fallback-output");
    let agent = test_agent(&root);
    let blocks = vec![Block::Text {
        text: "abcdefgh".to_string(),
    }];
    let mut usage = Usage {
        input: 10,
        output: 0,
        cache_create: 0,
        cache_read: 0,
        cost_usd: None,
    };

    agent.finalize_turn_usage_metrics(&mut usage, &blocks);

    assert_eq!(usage.actual_input_tokens(), 10);
    assert_eq!(usage.output, 2);
    assert!(usage.cost_usd.is_some());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn message_approx_tokens_uses_ceil_char_quarter() {
    let message = Message {
        role: "user".to_string(),
        content: vec![
            Block::Text {
                text: "12345".to_string(),
            },
            tool_result_block("id1", "123", None),
        ],
    };

    assert_eq!(message_approx_tokens(&message), 2);
}

#[test]
fn tokens_report_highlights_largest_messages() {
    let hist = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "short".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "x".repeat(8000),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("id1", &"y".repeat(4000), None)],
        },
    ];
    let report = render_tokens_report(&hist);
    assert!(report.contains("approx tokens in history:"), "{report}");
    assert!(report.contains("top hogs:"), "{report}");
    assert!(report.contains("[1] assistant"), "{report}");
    assert!(report.contains("[2] user"), "{report}");
    let big_idx = report.find("[1] assistant").unwrap();
    let mid_idx = report.find("[2] user").unwrap();
    assert!(
        big_idx < mid_idx,
        "largest message should come first: {report}"
    );
}

#[test]
fn jittered_backoff_respects_range_and_varies() {
    for base in [1u64, 2, 4, 8, 30] {
        let half = base / 2;
        let samples: Vec<u64> = (0..64).map(|_| jittered_backoff_secs(base)).collect();
        for &w in &samples {
            assert!(
                w >= half.max(1) || (base == 1 && w >= 1),
                "base={base} wait={w} below half={half}"
            );
            assert!(w <= base, "base={base} wait={w} exceeds base");
        }
        if base > 2 {
            let min_seen = samples.iter().copied().min().unwrap();
            let max_seen = samples.iter().copied().max().unwrap();
            assert!(
                max_seen > min_seen,
                "jitter produced constant value for base={base}: {samples:?}"
            );
        }
    }
}

#[test]
fn provider_runtime_and_slash_registries_are_split() {
    let provider_names: HashSet<&str> = provider_tool_definitions()
        .iter()
        .map(|tool| tool.name)
        .collect();
    assert!(!provider_names.contains("subagent"));
    assert!(provider_names.contains("read_file"));
    assert!(provider_names.contains("jq"));
    assert!(provider_names.contains("csvkit"));
    assert!(provider_names.contains("browser"));
    assert!(tools::is_external_process_tool("browser"));
    assert_eq!(
        prepare_external_tool("browser", &json!({"args": ["snapshot"]}), Path::new("."))
            .expect("prepare browser tool")
            .0,
        "agent-browser"
    );

    let default_names: HashSet<&str> = provider_tool_definitions()
        .iter()
        .filter(|tool| tool_name_allowed_in_profile(tool.name, ToolContextProfile::Default))
        .map(|tool| tool.name)
        .collect();
    assert!(default_names.contains("read_file"));
    assert!(!default_names.contains("jq"));
    assert!(!default_names.contains("csvkit"));
    assert!(!default_names.contains("git_log"));

    let runtime_names: HashSet<&str> = runtime_tool_definitions()
        .iter()
        .map(|tool| tool.name)
        .collect();
    assert!(runtime_names.contains("subagent-runtime"));
    assert!(!runtime_names.contains("read_file"));

    assert_eq!(
        BrowserRecipe::parse("agentbrowser"),
        Some(BrowserRecipe::AgentBrowser)
    );

    let slash_names: HashSet<&str> = slash_command_definitions()
        .iter()
        .map(|cmd| cmd.name)
        .collect();
    assert!(slash_names.contains("subagent"));
    assert!(slash_names.contains("browser"));
    assert!(slash_names.contains("tools"));
    assert!(!slash_names.contains("toolset"));
    assert!(slash_names.contains("pack"));
    assert!(slash_names.contains("shelves"));
    assert!(!slash_names.contains("read_file"));
    assert!(provider_names.is_disjoint(&runtime_names));
}

#[test]
fn subagent_is_hidden_from_model_tool_state() {
    let root = temp_test_dir("subagent-hidden");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.allowed.insert("subagent".to_string());
    assert!(!needs_permission("subagent"));
    assert!(!agent.tools.iter().any(|t| t.name == "subagent"));
    let header = agent.session_header();
    assert!(!header.allowed.contains(&"subagent".to_string()));
    assert!(!header.exposed_tools.contains(&"subagent".to_string()));
    assert!(!header.auto_approved_tools.contains(&"subagent".to_string()));
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));
    assert_eq!(handle_slash("/allowed", &mut agent), Some(true));
    let slash = drain_events(&mut rx)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .unwrap_or_default();
    assert!(slash.contains("(none)"), "{slash}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_debounce_skips_rapid_non_critical_writes() {
    let _guard = env_lock();
    let root = temp_test_dir("checkpoint-debounce");
    let sessions = root.join("sessions");
    let logs = root.join("logs");

    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::set_var("DEXT_SESSIONS_DIR", &sessions);
        std::env::set_var("DEXT_LOGS_DIR", &logs);
        std::env::set_var("DEXT_CHECKPOINT_DEBOUNCE_MS", "10000");
    }

    let result = (|| -> Result<()> {
        let root = std::fs::canonicalize(&root)?;
        let mut agent = test_agent(&root);
        agent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "first".to_string(),
            }],
        });
        agent.checkpoint_latest_session("after_tool_results");

        let session_path = sessions.join("_latest.jsonl");
        let after_first = std::fs::read_to_string(&session_path)?;
        assert!(after_first.contains("first"), "{after_first}");

        agent.history.push(Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "second".to_string(),
            }],
        });
        agent.checkpoint_latest_session("after_tool_results");
        let after_second = std::fs::read_to_string(&session_path)?;
        assert!(
            !after_second.contains("second"),
            "rapid non-critical checkpoint should have been debounced: {after_second}"
        );

        agent.checkpoint_latest_session("after_compact");
        let after_critical = std::fs::read_to_string(&session_path)?;
        assert!(
            after_critical.contains("second"),
            "critical reason must bypass debounce: {after_critical}"
        );
        Ok(())
    })();

    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
        std::env::remove_var("DEXT_CHECKPOINT_DEBOUNCE_MS");
    }
    let _ = std::fs::remove_dir_all(&root);
    result.expect("checkpoint debounce");
}

#[test]
fn suppressed_checkpoints_do_not_clobber_parent_session() {
    let _guard = env_lock();
    let root = temp_test_dir("subagent-no-clobber");
    let sessions = root.join("sessions");
    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::set_var("DEXT_SESSIONS_DIR", &sessions) };

    let result = (|| -> Result<()> {
        let root_canon = std::fs::canonicalize(&root)?;
        let session_path = sessions.join("_latest.jsonl");

        let mut parent = test_agent(&root_canon);
        parent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "parent-message".to_string(),
            }],
        });
        parent.checkpoint_latest_session("parent_write");
        let written = std::fs::read_to_string(&session_path)?;
        assert!(written.contains("parent-message"), "{written}");

        let mut sub = test_agent(&root_canon);
        sub.suppress_checkpoints = true;
        sub.latest_session_path = parent.latest_session_path.clone();
        sub.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "subagent-message".to_string(),
            }],
        });
        sub.checkpoint_latest_session("sub_should_noop");

        let after_sub = std::fs::read_to_string(&session_path)?;
        assert!(
            after_sub.contains("parent-message"),
            "parent session was clobbered by subagent: {after_sub}"
        );
        assert!(
            !after_sub.contains("subagent-message"),
            "subagent leaked into parent session: {after_sub}"
        );
        Ok(())
    })();

    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::remove_var("DEXT_SESSIONS_DIR") };
    let _ = std::fs::remove_dir_all(&root);
    result.expect("subagent checkpoint suppression");
}

#[test]
fn sse_scan_handles_mixed_lf_and_crlf_delimiters() {
    let mut stream = Vec::new();
    stream.extend_from_slice(b"event: content_block_delta\n");
    stream.extend_from_slice(b"data: {\"n\":1}\n\n");
    stream.extend_from_slice(b"event: content_block_delta\r\n");
    stream.extend_from_slice(b"data: {\"n\":2}\r\n\r\n");
    stream.extend_from_slice(b"event: message_stop\n");
    stream.extend_from_slice(b"data: {}\n\n");

    let mut recovered: Vec<Value> = Vec::new();
    let mut buf: Vec<u8> = stream;
    while let Some((event_text, consumed)) = next_sse_event(&buf) {
        for line in event_text.lines() {
            if let Some(rest) = line.strip_prefix("data:") {
                let rest = rest.trim_start();
                if let Ok(v) = serde_json::from_str::<Value>(rest) {
                    recovered.push(v);
                }
            }
        }
        buf.drain(..consumed);
    }
    assert_eq!(recovered.len(), 3, "got {recovered:?}");
    assert_eq!(recovered[0]["n"], 1);
    assert_eq!(recovered[1]["n"], 2);
    assert!(buf.is_empty(), "trailing {:?}", buf);
}

#[tokio::test]
async fn bash_tool_honors_input_timeout() {
    let root = temp_test_dir("bash-input-timeout");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let err = execute_builtin_call(
        "bash".to_string(),
        json!({"command": "sleep 2", "timeout": 1}),
        root.clone(),
        Arc::new(AtomicBool::new(false)),
        None,
        None,
    )
    .await
    .expect_err("expected input timeout");
    assert!(err.contains("timed out after 1s running bash"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn model_side_subagent_tool_call_does_not_launch_runtime() {
    let root = temp_test_dir("model-subagent-blocked");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let out = execute_builtin_call(
        "subagent".to_string(),
        json!({"task": "do a thing"}),
        root.clone(),
        Arc::new(AtomicBool::new(false)),
        None,
        None,
    )
    .await
    .expect("model-side subagent guidance");
    assert!(out.contains("not a provider-visible tool"), "{out}");
    assert!(
        !subagent_requests_dir(&root).exists(),
        "model-side subagent tool call must not create artifacts"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn fd_and_rg_discovery_ignore_heavy_dirs_by_default() {
    let root = temp_test_dir("discovery-default-excludes");
    let root = std::fs::canonicalize(&root).unwrap();

    let (fd_bin, fd_args, _) = prepare_external_tool(
        "fd",
        &json!({"pattern": "\\.rs$", "path": root.to_str().unwrap()}),
        &root,
    )
    .expect("prepare fd");
    if fd_bin == "fd" {
        assert!(
            fd_args.windows(2).any(|w| w == ["--exclude", "target"]),
            "{fd_args:?}"
        );
        assert!(
            fd_args
                .windows(2)
                .any(|w| w == ["--exclude", "node_modules"]),
            "{fd_args:?}"
        );
    } else {
        assert!(
            fd_args.windows(2).any(|w| w == ["-path", "*/target/*"]),
            "{fd_args:?}"
        );
        assert!(
            fd_args
                .windows(2)
                .any(|w| w == ["-path", "*/node_modules/*"]),
            "{fd_args:?}"
        );
    }

    let (_rg_bin, rg_args, _) = prepare_external_tool(
        "rg",
        &json!({"pattern": "needle", "path": root.to_str().unwrap()}),
        &root,
    )
    .expect("prepare rg");
    if rg_args.iter().any(|arg| arg == "--glob") {
        assert!(
            rg_args.windows(2).any(|w| w == ["--glob", "!**/target/**"]),
            "{rg_args:?}"
        );
        assert!(
            rg_args
                .windows(2)
                .any(|w| w == ["--glob", "!**/node_modules/**"]),
            "{rg_args:?}"
        );
    } else {
        assert!(
            rg_args.contains(&"--exclude-dir=target".to_string()),
            "{rg_args:?}"
        );
        assert!(
            rg_args.contains(&"--exclude-dir=node_modules".to_string()),
            "{rg_args:?}"
        );
    }

    let (_bin, unrestricted_args, _) = prepare_external_tool(
        "fd",
        &json!({"pattern": "\\.rs$", "path": root.to_str().unwrap(), "extra_args": ["--no-ignore"]}),
        &root,
    )
    .expect("prepare unrestricted fd");
    assert!(
        !unrestricted_args
            .windows(2)
            .any(|w| w == ["--exclude", "target"] || w == ["-path", "*/target/*"]),
        "{unrestricted_args:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn process_output_warns_on_suspicious_stderr_with_success_status() {
    let out = format_process_output(
        "ok\n".to_string(),
        "bash: line 1: nope: command not found\n".to_string(),
        0,
    )
    .expect("status 0 still returns output");
    assert!(
        out.contains("command exited 0 but stderr contains"),
        "{out}"
    );
    assert!(out.contains("command not found"), "{out}");
}

#[tokio::test]
async fn bash_prepends_cargo_json_diagnostics_summary() {
    let root = temp_test_dir("bash-cargo-json-summary");
    let root = std::fs::canonicalize(&root).unwrap();
    let line = json!({
        "reason": "compiler-message",
        "message": {
            "level": "error",
            "message": "mismatched types",
            "code": {"code": "E0308"},
            "spans": [{
                "file_name": "src/main.rs",
                "line_start": 10,
                "column_start": 7,
                "is_primary": true
            }]
        }
    })
    .to_string();
    let escaped = serde_json::to_string(&line).unwrap();

    let out = execute_bash_async_with_timeout(
        &format!("printf '%s\\n' {escaped}"),
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
    )
    .await
    .expect("bash should succeed");

    assert!(out.starts_with("cargo diagnostics summary"), "{out}");
    assert!(out.contains("- error [E0308]: src/main.rs:10:7"), "{out}");
    assert!(out.contains("exit: 0"), "{out}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_diff_tool_builds_correct_args() {
    let root = temp_test_dir("git-diff-args");
    let root = std::fs::canonicalize(&root).unwrap();
    let out = execute_tool(
        "git_diff",
        &json!({"staged": true, "path": "src/main.rs"}),
        &root,
    );
    let err = out.expect_err("expected git to fail in temp dir without repo");
    assert!(err.contains("git"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_log_tool_builds_correct_args() {
    let root = temp_test_dir("git-log-args");
    let root = std::fs::canonicalize(&root).unwrap();
    let out = execute_tool("git_log", &json!({"count": 5, "oneline": true}), &root);
    let err = out.expect_err("expected git to fail in temp dir without repo");
    assert!(err.contains("git"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn write_file_returns_diff_preview_and_summary() {
    let root = temp_test_dir("write-file-diff");
    let root = std::fs::canonicalize(&root).unwrap();

    let out = execute_tool(
        "write_file",
        &json!({"path": "note.txt", "content": "hello\nworld\n"}),
        &root,
    )
    .expect("write_file should succeed");

    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).unwrap(),
        "hello\nworld\n"
    );
    assert!(out.contains("@@"), "{out}");
    assert!(out.contains("+hello"), "{out}");
    assert!(out.contains("+world"), "{out}");
    assert!(out.contains("wrote 12 bytes to"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn edit_file_returns_diff_and_summary() {
    let root = temp_test_dir("edit-file-diff");
    let root = std::fs::canonicalize(&root).unwrap();
    let path = root.join("note.txt");
    std::fs::write(&path, "hello\nworld\n").unwrap();

    let out = execute_tool(
        "edit_file",
        &json!({"path": "note.txt", "old_string": "world", "new_string": "dext"}),
        &root,
    )
    .expect("edit_file should succeed");

    assert!(out.contains("@@"), "{out}");
    assert!(out.contains("-world"), "{out}");
    assert!(out.contains("+dext"), "{out}");
    assert!(out.contains("edited "), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn edit_file_non_unique_error_returns_match_locations() {
    let root = temp_test_dir("edit-file-non-unique");
    let root = std::fs::canonicalize(&root).unwrap();
    let path = root.join("note.txt");
    std::fs::write(&path, "alpha\nneedle\nbeta\nneedle\ngamma\n").unwrap();

    let err = execute_tool(
        "edit_file",
        &json!({"path": "note.txt", "old_string": "needle", "new_string": "dext"}),
        &root,
    )
    .expect_err("edit_file should reject non-unique old_string");

    assert!(err.contains("old_string appears 2 times"), "{err}");
    assert!(err.contains("match 1: note.txt:2:1"), "{err}");
    assert!(err.contains("match 2: note.txt:4:1"), "{err}");
    assert!(err.contains("> 2\tneedle"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn multi_edit_non_unique_error_returns_match_locations() {
    let root = temp_test_dir("multi-edit-non-unique");
    let root = std::fs::canonicalize(&root).unwrap();
    let path = root.join("note.txt");
    std::fs::write(&path, "alpha\nneedle\nbeta\nneedle\ngamma\n").unwrap();

    let err = execute_tool(
        "multi_edit",
        &json!({
            "path": "note.txt",
            "edits": [{"old_string": "needle", "new_string": "dext"}]
        }),
        &root,
    )
    .expect_err("multi_edit should reject non-unique old_string");

    assert!(err.contains("edit[0]: old_string appears 2 times"), "{err}");
    assert!(err.contains("match 1: note.txt:2:1"), "{err}");
    assert!(err.contains("match 2: note.txt:4:1"), "{err}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn multi_edit_returns_diff_and_summary() {
    let root = temp_test_dir("multi-edit-diff");
    let root = std::fs::canonicalize(&root).unwrap();
    let path = root.join("note.txt");
    std::fs::write(&path, "alpha\nbeta\ngamma\n").unwrap();

    let out = execute_tool(
        "multi_edit",
        &json!({
            "path": "note.txt",
            "edits": [
                {"old_string": "alpha", "new_string": "ALPHA"},
                {"old_string": "gamma", "new_string": "GAMMA"}
            ]
        }),
        &root,
    )
    .expect("multi_edit should succeed");

    assert!(out.contains("@@"), "{out}");
    assert!(out.contains("-alpha"), "{out}");
    assert!(out.contains("+ALPHA"), "{out}");
    assert!(out.contains("-gamma"), "{out}");
    assert!(out.contains("+GAMMA"), "{out}");
    assert!(out.contains("applied 2 edits to"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn todo_write_creates_and_todo_read_returns() {
    let root = temp_test_dir("todo-roundtrip");
    let root = std::fs::canonicalize(&root).unwrap();

    let write_result = execute_tool(
        "todo_write",
        &json!({
            "todos": [
                {"text": "Implement feature X", "status": "in_progress"},
                {"text": "Write tests", "status": "pending"},
                {"text": "Update docs", "status": "completed"}
            ]
        }),
        &root,
    );
    assert!(
        write_result.is_ok(),
        "todo_write failed: {:?}",
        write_result
    );
    let msg = write_result.unwrap();
    assert!(msg.contains("► Implement feature X [in_progress]"), "{msg}");
    assert!(msg.contains("○ Write tests [pending]"), "{msg}");
    assert!(msg.contains("✓ Update docs [completed]"), "{msg}");
    assert!(
        msg.contains("1 pending, 1 in progress, 1 completed"),
        "{msg}"
    );
    assert!(
        msg.contains("delta: +1 pending · +1 in_progress · +1 completed"),
        "{msg}"
    );

    let read_result = execute_tool("todo_read", &json!({}), &root);
    assert!(read_result.is_ok(), "todo_read failed: {:?}", read_result);
    let content = read_result.unwrap();
    assert!(content.contains("Implement feature X"), "{content}");
    assert!(content.contains("Write tests"), "{content}");
    assert!(content.contains("Update docs"), "{content}");
    assert!(content.contains("1 pending"), "{content}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn todo_write_reports_status_delta_against_existing_list() {
    let root = temp_test_dir("todo-delta");
    let root = std::fs::canonicalize(&root).unwrap();

    execute_tool(
        "todo_write",
        &json!({
            "todos": [
                {"text": "Plan", "status": "pending"},
                {"text": "Code", "status": "pending"}
            ]
        }),
        &root,
    )
    .unwrap();

    let msg = execute_tool(
        "todo_write",
        &json!({
            "todos": [
                {"text": "Plan", "status": "completed"},
                {"text": "Code", "status": "in_progress"},
                {"text": "Verify", "status": "pending"}
            ]
        }),
        &root,
    )
    .unwrap();

    assert!(
        msg.contains("1 pending, 1 in progress, 1 completed"),
        "{msg}"
    );
    assert!(
        msg.contains("delta: -1 pending · +1 in_progress · +1 completed"),
        "{msg}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn todo_read_returns_empty_when_no_file() {
    let root = temp_test_dir("todo-empty");
    let root = std::fs::canonicalize(&root).unwrap();
    let result = execute_tool("todo_read", &json!({}), &root).unwrap();
    assert!(result.contains("no todos"), "{result}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn todo_write_rejects_invalid_status() {
    let root = temp_test_dir("todo-bad-status");
    let root = std::fs::canonicalize(&root).unwrap();
    let result = execute_tool(
        "todo_write",
        &json!({
            "todos": [{"text": "test", "status": "bogus"}]
        }),
        &root,
    );
    assert!(result.is_err(), "expected error for invalid status");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn api_provider_detects_openai_base_url() {
    let _guard = env_lock();
    unsafe {
        std::env::set_var("DEXT_API_PROVIDER", "");
    }

    unsafe {
        std::env::set_var("ANTHROPIC_BASE_URL", "http://localhost:11434/v1");
    }
    assert_eq!(ApiProvider::from_env(), ApiProvider::OpenAi);

    unsafe {
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.openai.com/v1");
    }
    assert_eq!(ApiProvider::from_env(), ApiProvider::OpenAi);

    unsafe {
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
    }
    assert_eq!(ApiProvider::from_env(), ApiProvider::Anthropic);

    unsafe {
        std::env::set_var("DEXT_API_PROVIDER", "openai");
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
    }
    assert_eq!(ApiProvider::from_env(), ApiProvider::OpenAi);

    unsafe {
        std::env::set_var("DEXT_API_PROVIDER", "deepseek");
        std::env::set_var("ANTHROPIC_BASE_URL", "https://api.anthropic.com");
    }
    assert_eq!(ApiProvider::from_env(), ApiProvider::OpenAi);

    unsafe {
        std::env::set_var("DEXT_API_PROVIDER", "codex");
    }
    assert_eq!(ApiProvider::from_env(), ApiProvider::ChatGpt);

    unsafe {
        std::env::remove_var("DEXT_API_PROVIDER");
        std::env::remove_var("ANTHROPIC_BASE_URL");
    }
}

#[test]
fn git_tools_are_parallel_safe_and_read_only() {
    assert!(is_parallel_safe_tool("git_diff"));
    assert!(is_parallel_safe_tool("git_log"));
    assert!(is_parallel_safe_tool("todo_read"));
    assert!(!is_parallel_safe_tool("git_commit"));
    assert!(!is_parallel_safe_tool("todo_write"));
}

#[test]
fn new_tools_in_correct_permission_category() {
    assert!(needs_permission("git_commit"));
    assert!(needs_permission("todo_write"));
    assert!(!needs_permission("git_diff"));
    assert!(!needs_permission("git_log"));
    assert!(!needs_permission("todo_read"));
}

#[test]
fn runtime_provider_reroutes_away_from_stale_cross_provider_default_model() -> Result<()> {
    // Reproduces the live bug: an older build saved `glm-5.1` as chatgpt's
    // default_model (via `/model glm-5.1` without provider switch). On startup
    // with chatgpt active, resolve_runtime_provider used to return
    // (chatgpt, glm-5.1), and the first turn 400'd with "The 'glm-5.1' model
    // is not supported when using Codex with a ChatGPT account." After the
    // fix, resolve_runtime_provider must auto-reroute to the glm provider
    // (authenticated) since it owns glm-5.1 in its models[] list.
    let _guard = env_lock();
    let root = temp_test_dir("provider-stale-default-reroute");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::remove_var("DEXT_PROVIDER");
        std::env::remove_var("DEXT_MODEL");
        std::env::remove_var("DEXT_MODEL_CHATGPT");
        std::env::remove_var("DEXT_MODEL_GLM");
        std::env::remove_var("DEXT_MODEL_FORCE");
    }

    let result = (|| -> Result<()> {
        // Seed: both providers authenticated, chatgpt active, chatgpt's
        // default_model stale-set to a glm model.
        let mut catalog = load_provider_catalog()?;
        catalog.active_provider = "chatgpt".to_string();
        for profile in &mut catalog.providers {
            if canonical_provider_id(&profile.id) == "chatgpt" {
                profile.default_model = "glm-5.1".to_string();
            }
        }
        save_provider_catalog(&catalog)?;

        let mut store = load_auth_store()?;
        store.providers.insert(
            "chatgpt".to_string(),
            StoredCredential::ApiKey {
                key: "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string(),
            },
        );
        store.providers.insert(
            "glm".to_string(),
            StoredCredential::ApiKey {
                key: "glm-test-key".to_string(),
            },
        );
        save_auth_store(&store)?;

        let resolved = resolve_runtime_provider(None, false)?;
        assert_eq!(
            resolved.profile.id, "glm",
            "expected auto-reroute to glm when stale default_model belongs there"
        );
        assert_eq!(resolved.model, "glm-5.1");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn runtime_provider_allows_missing_key_when_requested() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-missing-key");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("DEXT_PROVIDER", "glm");
        std::env::remove_var("DEXT_API_KEY");
        std::env::remove_var("ZAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }

    let result = (|| -> Result<()> {
        let loose = resolve_runtime_provider(None, false)?;
        assert_eq!(loose.profile.id, "glm");
        assert!(loose.requires_api_key);
        assert!(loose.api_key.is_empty());
        assert!(resolve_runtime_provider(None, true).is_err());
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_PROVIDER");
        std::env::remove_var("ZAI_API_KEY");
        std::env::remove_var("ANTHROPIC_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn global_model_override_respects_provider_compatibility() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-model-compat");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("DEXT_PROVIDER", "chatgpt");
        std::env::set_var("DEXT_MODEL", "glm-5.1");
        std::env::remove_var("DEXT_MODEL_CHATGPT");
        std::env::remove_var("DEXT_MODEL_FORCE");
    }

    let result = (|| -> Result<()> {
        let resolved = resolve_runtime_provider(None, false)?;
        assert_eq!(resolved.profile.id, "chatgpt");
        assert_eq!(resolved.model, "gpt-5.4");

        unsafe {
            std::env::set_var("DEXT_MODEL_CHATGPT", "gpt-4o");
        }
        let resolved_with_provider_override = resolve_runtime_provider(None, false)?;
        assert_eq!(resolved_with_provider_override.model, "gpt-4o");

        unsafe {
            std::env::remove_var("DEXT_MODEL_CHATGPT");
            std::env::set_var("DEXT_MODEL_FORCE", "1");
        }
        let resolved_forced = resolve_runtime_provider(None, false)?;
        assert_eq!(resolved_forced.model, "glm-5.1");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_PROVIDER");
        std::env::remove_var("DEXT_MODEL");
        std::env::remove_var("DEXT_MODEL_CHATGPT");
        std::env::remove_var("DEXT_MODEL_FORCE");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn provider_default_model_persists_and_is_listed() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-default-model");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        set_provider_default_model_in_catalog("glm", "glm-5.9")?;
        let catalog = load_provider_catalog()?;
        let glm = find_provider_profile(&catalog, "glm").context("glm profile")?;
        assert_eq!(glm.default_model, "glm-5.9");
        assert!(glm.models.iter().any(|m| m == "glm-5.9"));
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn chatgpt_default_model_is_normalized_when_saved() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-chatgpt-model-normalize");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        set_provider_default_model_in_catalog("chatgpt", "gpt-4o")?;
        let catalog = load_provider_catalog()?;
        let chatgpt = find_provider_profile(&catalog, "chatgpt").context("chatgpt profile")?;
        assert_eq!(chatgpt.default_model, "gpt-4o");
        assert!(chatgpt.models.iter().any(|m| m == "gpt-4o"));
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn stale_chatgpt_catalog_entry_is_upgraded_on_load() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-chatgpt-catalog-upgrade");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let path = provider_catalog_path();
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "active_provider": "chatgpt",
  "providers": [
{
  "id": "chatgpt",
  "display_name": "ChatGPT",
  "api_provider": "chatgpt",
  "base_url": "https://chatgpt.com/backend-api",
  "default_model": "gpt-4o",
  "models": ["gpt-4o"],
  "env_vars": ["CHATGPT_ACCESS_TOKEN"],
  "requires_api_key": true,
  "login_url": "https://chatgpt.com/auth/login",
  "notes": "old note"
}
  ]
}"#,
        )?;

        let catalog = load_provider_catalog()?;
        let chatgpt = find_provider_profile(&catalog, "chatgpt").context("chatgpt profile")?;
        assert_eq!(resolve_active_provider_id(&catalog), "chatgpt");
        assert_eq!(chatgpt.default_model, "gpt-4o");
        assert!(chatgpt.models.iter().any(|m| m == "gpt-4o"));
        assert_eq!(chatgpt.login_url.as_deref(), Some("https://chatgpt.com"));
        assert!(
            chatgpt
                .notes
                .as_deref()
                .is_some_and(|notes| notes.contains("OAuth"))
        );
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn legacy_bundled_providers_are_pruned_from_catalog() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-prune-legacy-builtin");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let path = provider_catalog_path();
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "active_provider": "openai",
  "providers": [
{"id":"openai","display_name":"OpenAI","api_provider":"openai","base_url":"https://api.openai.com","default_model":"gpt-5","models":["gpt-5"],"env_vars":["OPENAI_API_KEY"],"requires_api_key":true},
{"id":"anthropic","display_name":"Anthropic","api_provider":"anthropic","base_url":"https://api.anthropic.com","default_model":"claude-sonnet-4-5","models":["claude-sonnet-4-5"],"env_vars":["ANTHROPIC_API_KEY"],"requires_api_key":true},
{"id":"openrouter","display_name":"OpenRouter","api_provider":"openai","base_url":"https://openrouter.ai/api/v1","default_model":"openai/gpt-4.1-mini","models":["openai/gpt-4.1-mini"],"env_vars":["OPENROUTER_API_KEY"],"requires_api_key":true},
{"id":"ollama","display_name":"Ollama","api_provider":"openai","base_url":"http://localhost:11434/v1","default_model":"qwen2.5-coder:latest","models":["qwen2.5-coder:latest"],"env_vars":[],"requires_api_key":false}
  ]
}"#,
        )?;

        let catalog = load_provider_catalog()?;
        let ids = catalog
            .providers
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["glm", "chatgpt", "openai", "anthropic", "deepseek", "local"]
        );

        let glm = find_provider_profile(&catalog, "glm").context("glm")?;
        assert_eq!(glm.env_vars, vec!["ZAI_API_KEY"]);

        let chatgpt = find_provider_profile(&catalog, "chatgpt").context("chatgpt")?;
        assert_eq!(chatgpt.env_vars, vec!["CHATGPT_ACCESS_TOKEN"]);
        assert!(chatgpt.oauth_flow.is_some());

        let openai = find_provider_profile(&catalog, "openai").context("openai")?;
        assert_eq!(openai.api_provider, ApiProvider::OpenAi);
        assert_eq!(openai.env_vars, vec!["OPENAI_API_KEY"]);
        assert!(openai.oauth_flow.is_none());

        let anthropic = find_provider_profile(&catalog, "anthropic").context("anthropic")?;
        assert_eq!(anthropic.api_provider, ApiProvider::Anthropic);
        assert_eq!(anthropic.env_vars, vec!["ANTHROPIC_API_KEY"]);

        let deepseek = find_provider_profile(&catalog, "deepseek").context("deepseek")?;
        assert_eq!(deepseek.api_provider, ApiProvider::OpenAi);
        assert_eq!(deepseek.env_vars, vec!["DEEPSEEK_API_KEY"]);
        assert_eq!(deepseek.default_model, "deepseek-chat");

        let local = find_provider_profile(&catalog, "local").context("local")?;
        assert_eq!(local.api_provider, ApiProvider::OpenAi);
        assert!(!local.requires_api_key);
        assert_eq!(local.default_model, "qwen2.5-coder-7b");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn local_provider_merge_drops_retired_catalog_artifacts() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "local")
        .expect("local profile");
    let mut stored = builtin.clone();
    stored.default_model = "qwen-local".to_string();
    stored.models.push("qwen-local".to_string());
    stored
        .models
        .push("Qwen3.6-35B-A3B-Q4_K_M.gguf".to_string());
    stored.models.push("custom-local-model".to_string());
    stored.context_window = Some(4_096);
    stored
        .model_context_windows
        .insert("qwen-local".to_string(), 4_096);
    stored
        .model_context_windows
        .insert("custom-local-model".to_string(), 12_345);

    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(merged.default_model, "qwen2.5-coder-7b");
    assert_eq!(
        merged.context_window,
        Some(DEFAULT_LOCAL_CONTEXT_WINDOW_TOKENS)
    );
    assert!(!merged.models.iter().any(|m| m == "qwen-local"));
    assert!(merged.models.iter().any(|m| m == "custom-local-model"));
    assert!(merged.model_context_windows.get("qwen-local").is_none());
    assert_eq!(
        merged.model_context_windows.get("custom-local-model"),
        Some(&12_345)
    );
}

#[test]
fn local_provider_merge_drops_stale_local_4k_context_without_retired_models() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "local")
        .expect("local profile");
    let mut stored = builtin.clone();
    stored.context_window = Some(4_096);

    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(
        merged.context_window,
        Some(DEFAULT_LOCAL_CONTEXT_WINDOW_TOKENS)
    );
}

#[test]
fn local_provider_merge_drops_stale_local_16k_context_without_retired_models() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "local")
        .expect("local profile");
    let mut stored = builtin.clone();
    stored.context_window = Some(16_384);
    stored.models.push("custom-local-model".to_string());

    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(
        merged.context_window,
        Some(DEFAULT_LOCAL_CONTEXT_WINDOW_TOKENS)
    );
    assert!(merged.models.iter().any(|m| m == "custom-local-model"));
}

#[test]
fn local_provider_merge_preserves_user_local_context_override_without_retired_artifacts() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "local")
        .expect("local profile");
    let mut stored = builtin.clone();
    stored.context_window = Some(65_536);
    stored.models.push("custom-local-model".to_string());

    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(merged.context_window, Some(65_536));
    assert!(merged.models.iter().any(|m| m == "custom-local-model"));
}

#[test]
fn auth_store_reads_legacy_provider_map() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("auth-legacy-map");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let path = auth_store_path();
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(&path, r#"{"openai":{"type":"api_key","key":"legacy-key"}}"#)?;

        let store = load_auth_store()?;
        let profile = ProviderProfile {
            id: "openai".to_string(),
            display_name: "OpenAI API".to_string(),
            api_provider: ApiProvider::OpenAi,
            base_url: "https://api.openai.com".to_string(),
            default_model: "gpt-5".to_string(),
            models: vec!["gpt-5".to_string()],
            env_vars: vec!["OPENAI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: None,
            oauth_flow: None,
            notes: None,
            context_window: None,
            model_context_windows: HashMap::new(),
        };
        let (key, source) = resolve_provider_api_key(&profile, &store).context("resolve key")?;
        assert_eq!(key, "legacy-key");
        assert_eq!(source, "auth:openai");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn login_imports_env_key_and_reuses_stored_key() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-import-env");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("ZAI_API_KEY", "env-glm-key");
        std::env::remove_var("DEXT_API_KEY");
    }

    let result = (|| -> Result<()> {
        let msg = login_provider_with_key(Some("glm"), None, false)?;
        assert!(msg.contains("imported credentials"), "{msg}");

        let store = load_auth_store()?;
        let entry = store
            .providers
            .get("glm")
            .context("missing glm credentials in auth store")?;
        let key = entry
            .resolve_secret()
            .context("unresolved glm credential")?;
        assert_eq!(key, "env-glm-key");

        unsafe {
            std::env::remove_var("ZAI_API_KEY");
        }
        let msg2 = login_provider_with_key(Some("glm"), None, false)?;
        assert!(msg2.contains("already authenticated"), "{msg2}");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("ZAI_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn provider_selector_accepts_index_and_id() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-selector");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let catalog = load_provider_catalog()?;
        let first = provider_id_from_selector(&catalog, "1")?;
        assert_eq!(first, canonical_provider_id(&catalog.providers[0].id));
        assert_eq!(provider_id_from_selector(&catalog, "chatgpt")?, "chatgpt");
        assert!(provider_id_from_selector(&catalog, "999").is_err());
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn resolve_provider_model_selection_prefers_authenticated_provider_matches() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-model-selection-auth");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let catalog = load_provider_catalog()?;
        let mut store = load_auth_store()?;
        store.providers.insert(
            "chatgpt".to_string(),
            StoredCredential::ApiKey {
                key: "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string(),
            },
        );
        store.providers.insert(
            "glm".to_string(),
            StoredCredential::ApiKey {
                key: "glm-test-key".to_string(),
            },
        );
        save_auth_store(&store)?;

        let selection = resolve_provider_model_selection(&catalog, &store, "glm", "gpt-4o")?;
        assert_eq!(selection.provider_id, "chatgpt");
        assert_eq!(selection.model, "gpt-4o");

        let compact = resolve_provider_model_selection(&catalog, &store, "glm", "gpt5.3codex")?;
        assert_eq!(compact.provider_id, "chatgpt");
        assert_eq!(compact.model, "gpt-5.3-codex");

        let glm = resolve_provider_model_selection(&catalog, &store, "chatgpt", "glm 5.1")?;
        assert_eq!(glm.provider_id, "glm");
        assert_eq!(glm.model, "glm-5.1");

        let local =
            resolve_provider_model_selection(&catalog, &store, "glm", "local/qwen2.5-coder-7b")?;
        assert_eq!(local.provider_id, "local");
        assert_eq!(local.model, "qwen2.5-coder-7b");

        let qwen_alias =
            resolve_provider_model_selection(&catalog, &store, "glm", "qwen/qwen3.5-9b")?;
        assert_eq!(qwen_alias.provider_id, "local");
        assert_eq!(qwen_alias.model, "qwen3.5-9b");

        let explicit =
            resolve_provider_model_selection(&catalog, &store, "glm", "chatgpt/gpt-5-4")?;
        assert_eq!(explicit.provider_id, "chatgpt");
        assert_eq!(explicit.model, "gpt-5.4");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn default_provider_catalog_includes_core_multi_provider_set() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("provider-core-set");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let catalog = load_provider_catalog()?;
        let ids = catalog
            .providers
            .iter()
            .map(|p| p.id.as_str())
            .collect::<Vec<_>>();
        assert_eq!(
            ids,
            vec!["glm", "chatgpt", "openai", "anthropic", "deepseek", "local"]
        );
        let local = catalog
            .providers
            .iter()
            .find(|p| p.id == "local")
            .expect("local provider");
        assert_eq!(local.api_provider, ApiProvider::OpenAi);
        assert!(!local.requires_api_key);
        assert_eq!(local.default_model, "qwen2.5-coder-7b");
        assert_eq!(
            local.context_window,
            Some(crate::provider::DEFAULT_LOCAL_CONTEXT_WINDOW_TOKENS)
        );
        assert_eq!(resolve_active_provider_id(&catalog), "glm");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn auth_store_normalizes_canonical_provider_ids_on_load_and_logout() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("auth-canonical-normalize");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let path = auth_store_path();
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(
            &path,
            r#"{
  "version": 1,
  "providers": {
    "codex": {"type":"api_key","key":"chatgpt-token"},
    "zai": {"type":"api_key","key":"glm-token"},
    "claude": {"type":"api_key","key":"anthropic-token"}
  }
}"#,
        )?;

        let store = load_auth_store()?;
        assert!(store.providers.contains_key("chatgpt"), "{store:?}");
        assert!(store.providers.contains_key("glm"), "{store:?}");
        assert!(store.providers.contains_key("anthropic"), "{store:?}");
        assert!(!store.providers.contains_key("codex"), "{store:?}");
        assert!(!store.providers.contains_key("zai"), "{store:?}");
        assert!(!store.providers.contains_key("claude"), "{store:?}");

        let catalog = load_provider_catalog()?;
        let chatgpt = find_provider_profile(&catalog, "chatgpt").context("chatgpt")?;
        assert_eq!(provider_auth_status(&chatgpt, &store), "auth");

        let logout = logout_provider(Some("codex"))?;
        assert!(
            logout.contains("removed stored credentials for provider 'chatgpt'"),
            "{logout}"
        );
        let store = load_auth_store()?;
        assert!(!store.providers.contains_key("chatgpt"));
        assert!(store.providers.contains_key("glm"));
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn chatgpt_login_does_not_import_openai_platform_key() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("chatgpt-no-openai-platform-key");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("OPENAI_API_KEY", "sk-openai-platform-key");
        std::env::set_var("DEXT_SKIP_BROWSER_OPEN", "1");
        std::env::remove_var("CHATGPT_ACCESS_TOKEN");
        std::env::remove_var("DEXT_API_KEY");
    }

    let result = (|| -> Result<()> {
        let chatgpt_login = login_provider(Some("chatgpt"), None, false)?;
        assert!(
            chatgpt_login.awaiting_credentials,
            "{}",
            chatgpt_login.message
        );
        assert!(
            !chatgpt_login.message.contains("imported credentials"),
            "{}",
            chatgpt_login.message
        );

        let openai_login = login_provider(Some("openai"), None, false)?;
        assert!(
            !openai_login.awaiting_credentials,
            "{}",
            openai_login.message
        );

        assert!(
            openai_login.message.contains("imported credentials"),
            "{}",
            openai_login.message
        );
        let store = load_auth_store()?;
        assert!(!store.providers.contains_key("chatgpt"));
        assert!(store.providers.contains_key("openai"));
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DEXT_SKIP_BROWSER_OPEN");
        std::env::remove_var("CHATGPT_ACCESS_TOKEN");
        std::env::remove_var("DEXT_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn list_models_for_available_providers_shows_authenticated_sections() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("models-all-authenticated");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let catalog = load_provider_catalog()?;
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
        save_auth_store(&store)?;

        let rendered = list_models_for_available_providers(&catalog, &store, "glm")?;
        assert!(rendered.contains("* provider 'glm' models:"), "{rendered}");
        assert!(
            rendered.contains("provider 'chatgpt' models:"),
            "{rendered}"
        );
        assert!(rendered.contains("- glm-4.6"), "{rendered}");
        assert!(rendered.contains("- gpt-4o"), "{rendered}");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn auth_store_parses_oauth_access_shape() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("auth-oauth-shape");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let path = auth_store_path();
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(
            &path,
            r#"{"chatgpt":{"type":"oauth","access":"access-token","refresh":"refresh-token","expires":4102444800}}"#,
        )?;

        let store = load_auth_store()?;
        let profile = find_provider_profile(&load_provider_catalog()?, "chatgpt")
            .context("missing chatgpt profile")?;
        let (secret, source) =
            resolve_provider_api_key(&profile, &store).context("resolve oauth")?;
        assert_eq!(secret, "access-token");
        assert_eq!(source, "auth:chatgpt");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn login_chatgpt_import_mode_uses_external_auth() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-chatgpt-external-auth");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("HOME", &root);
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DEXT_API_KEY");
    }

    let result = (|| -> Result<()> {
        let ext_auth = PathBuf::from(&root).join(".dext/external-auth.json");
        std::fs::create_dir_all(ext_auth.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(
            &ext_auth,
            r#"{"openai-codex":{"type":"oauth","access":"ext-codex-token","refresh":"rr","expires":4102444800}}"#,
        )?;

        let login = login_provider(Some("chatgpt"), Some("import"), false)?;
        assert!(!login.awaiting_credentials);
        assert!(
            login.message.contains("imported credentials"),
            "{}",
            login.message
        );
        assert!(
            login.message.contains("external-auth:openai-codex"),
            "{}",
            login.message
        );

        let store = load_auth_store()?;
        let entry = store
            .providers
            .get("chatgpt")
            .context("missing chatgpt credential")?;
        let secret = entry.resolve_secret().context("missing secret")?;
        assert_eq!(secret, "ext-codex-token");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("HOME");
        std::env::remove_var("OPENAI_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn login_chatgpt_default_mode_ignores_external_auth_import() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-chatgpt-default-mode");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("HOME", &root);
        std::env::set_var("DEXT_SKIP_BROWSER_OPEN", "1");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DEXT_API_KEY");
    }

    let result = (|| -> Result<()> {
        let ext_auth = PathBuf::from(&root).join(".dext/external-auth.json");
        std::fs::create_dir_all(ext_auth.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(
            &ext_auth,
            r#"{"openai-codex":{"type":"oauth","access":"ext-codex-token","refresh":"rr","expires":4102444800}}"#,
        )?;

        let login = login_provider(Some("chatgpt"), None, false)?;
        assert!(login.awaiting_credentials);
        assert!(
            !login.message.contains("imported credentials"),
            "{}",
            login.message
        );

        let store = load_auth_store()?;
        assert!(!store.providers.contains_key("chatgpt"));

        let catalog = load_provider_catalog()?;
        assert_eq!(resolve_active_provider_id(&catalog), "chatgpt");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("HOME");
        std::env::remove_var("DEXT_SKIP_BROWSER_OPEN");
        std::env::remove_var("OPENAI_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn login_web_mode_skips_external_auth_import() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-chatgpt-web-mode");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("HOME", &root);
        std::env::set_var("DEXT_SKIP_BROWSER_OPEN", "1");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DEXT_API_KEY");
    }

    let result = (|| -> Result<()> {
        let ext_auth = PathBuf::from(&root).join(".dext/external-auth.json");
        std::fs::create_dir_all(ext_auth.parent().unwrap_or(Path::new(".")))?;
        std::fs::write(
            &ext_auth,
            r#"{"openai-codex":{"type":"oauth","access":"ext-codex-token","refresh":"rr","expires":4102444800}}"#,
        )?;

        let login = login_provider(Some("chatgpt"), Some("web"), false)?;
        assert!(login.awaiting_credentials);
        assert!(
            login.message.contains("browser open disabled")
                || login.message.contains("opened ChatGPT in your browser"),
            "{}",
            login.message
        );
        assert!(
            !login.message.contains("imported credentials"),
            "{}",
            login.message
        );

        let store = load_auth_store()?;
        assert!(!store.providers.contains_key("chatgpt"));

        let catalog = load_provider_catalog()?;
        assert_eq!(resolve_active_provider_id(&catalog), "chatgpt");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("HOME");
        std::env::remove_var("DEXT_SKIP_BROWSER_OPEN");
        std::env::remove_var("OPENAI_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn chatgpt_oauth_authorize_url_uses_dext_originator_by_default() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-chatgpt-originator-default");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("HOME", &root);
        std::env::set_var("DEXT_SKIP_BROWSER_OPEN", "1");
        std::env::remove_var("DEXT_OAUTH_ORIGINATOR");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DEXT_API_KEY");
    }

    let result = (|| -> Result<()> {
        let login = login_provider(Some("chatgpt"), Some("web"), false)?;
        assert!(login.awaiting_credentials);
        assert!(
            login.message.contains("originator=dext"),
            "{}",
            login.message
        );
        assert!(
            !login.message.contains("originator=codex"),
            "{}",
            login.message
        );
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("HOME");
        std::env::remove_var("DEXT_SKIP_BROWSER_OPEN");
        std::env::remove_var("DEXT_OAUTH_ORIGINATOR");
        std::env::remove_var("OPENAI_API_KEY");
        std::env::remove_var("DEXT_API_KEY");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn oauth_code_parser_accepts_callback_url_and_plain_authorization_code() {
    let callback = "http://localhost:1455/auth/callback?code=ac_test_code.abc123&state=deadbeef";
    assert_eq!(
        extract_oauth_code_from_callback(callback).as_deref(),
        Some("ac_test_code.abc123")
    );

    assert_eq!(
        extract_oauth_code_from_callback("ac_test_code.abc123").as_deref(),
        Some("ac_test_code.abc123")
    );

    assert!(extract_oauth_code_from_callback("sk-proj-123").is_none());
}

#[test]
fn login_chatgpt_authorization_code_uses_oauth_completion_path() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-chatgpt-authorization-code");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = {
        for code in ["ac_test_code.abc123", "tmp_manual_code_123456"] {
            let err = login_provider(Some("chatgpt"), Some(code), false)
                .expect_err("authorization code should require pending OAuth state");
            let msg = format!("{err:#}");
            assert!(
                msg.contains("no pending OAuth session found"),
                "unexpected error for {code}: {msg}"
            );
            assert!(
                !msg.contains("invalid ChatGPT access token format"),
                "authorization code must not be validated as JWT for {code}: {msg}"
            );
        }
        Ok(())
    };

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn oauth_exchange_failure_stays_retryable() {
    let message = oauth_exchange_failure_result_message(
        "chatgpt",
        "If the browser callback doesn't auto-complete, paste the callback URL.",
    );
    assert!(message.contains("OAuth token exchange failed"), "{message}");
    assert!(
        message.contains("paste the callback URL or authorization code into dext to retry"),
        "{message}"
    );
}

#[test]
fn chatgpt_oauth_defaults_match_pi_flow() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("chatgpt-oauth-defaults");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let catalog = load_provider_catalog()?;
        let chatgpt = find_provider_profile(&catalog, "chatgpt").context("chatgpt profile")?;
        let oauth = chatgpt.oauth_flow.context("missing chatgpt oauth flow")?;
        assert_eq!(oauth.scope, "openid profile email offline_access");
        assert_eq!(
            oauth.redirect_uri.as_deref(),
            Some("http://localhost:1455/auth/callback")
        );
        assert_eq!(oauth.callback_host.as_deref(), Some("127.0.0.1"));
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn chatgpt_oauth_callback_host_env_override_is_respected() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("chatgpt-oauth-callback-host-override");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("DEXT_SKIP_BROWSER_OPEN", "1");
        std::env::set_var("DEXT_OAUTH_CALLBACK_HOST", "256.0.0.1");
    }

    let result = (|| -> Result<()> {
        let login = login_provider(Some("chatgpt"), Some("web"), false)?;
        assert!(login.awaiting_credentials);
        assert!(
            login.message.contains("256.0.0.1:1455"),
            "{}",
            login.message
        );
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SKIP_BROWSER_OPEN");
        std::env::remove_var("DEXT_OAUTH_CALLBACK_HOST");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn login_chatgpt_accepts_access_token_json_and_rejects_api_key() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-chatgpt-access-token");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let invalid = login_provider(Some("chatgpt"), Some("sk-test-key-1234567890abcdef"), false)
            .expect_err("platform api key should be rejected for chatgpt provider");
        assert!(
            format!("{invalid:#}").contains("invalid ChatGPT access token format"),
            "{invalid:#}"
        );

        let jwt = r#"{"accessToken":"eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature"}"#;
        let login = login_provider(Some("chatgpt"), Some(jwt), false)?;
        assert!(!login.awaiting_credentials);
        assert!(
            login.message.contains("provider 'chatgpt'"),
            "{}",
            login.message
        );

        let store = load_auth_store()?;
        let entry = store
            .providers
            .get("chatgpt")
            .context("missing chatgpt credential")?;
        let secret = entry.resolve_secret().context("missing secret")?;
        assert_eq!(
            secret,
            "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature"
        );
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn slash_logout_ignores_extra_words_after_provider() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-logout-chatgpt-web");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let mut catalog = load_provider_catalog()?;
        catalog.active_provider = "chatgpt".to_string();
        save_provider_catalog(&catalog)?;

        let mut store = load_auth_store()?;
        store.providers.insert(
            "chatgpt".to_string(),
            StoredCredential::ApiKey {
                key: "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string(),
            },
        );
        save_auth_store(&store)?;

        let mut agent = test_agent(&root);
        agent.reload_provider(Some("chatgpt"), false)?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.set_sink(Box::new(ChannelSink { tx }));

        assert_eq!(handle_slash("/logout chatgpt web", &mut agent), Some(true));

        let mut slash = String::new();
        while let Ok(event) = rx.try_recv() {
            if let AgentEvent::Slash(msg) = event {
                slash = msg;
            }
        }
        assert!(
            slash.contains("removed stored credentials for provider 'chatgpt'"),
            "{slash}"
        );
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn handle_slash_model_switches_provider_when_model_belongs_elsewhere() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-model-cross-provider");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let mut store = load_auth_store()?;
        store.providers.insert(
            "chatgpt".to_string(),
            StoredCredential::ApiKey {
                key: "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string(),
            },
        );
        store.providers.insert(
            "glm".to_string(),
            StoredCredential::ApiKey {
                key: "glm-test-key".to_string(),
            },
        );
        save_auth_store(&store)?;

        let mut agent = test_agent(&root);
        agent.reload_provider(Some("glm"), false)?;
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        agent.set_sink(Box::new(ChannelSink { tx }));

        assert_eq!(handle_slash("/model gpt-4o", &mut agent), Some(true));
        assert_eq!(agent.provider_id, "chatgpt");
        assert_eq!(agent.model, "gpt-4o");

        let mut slash = String::new();
        let mut diagnostics = None;
        while let Ok(event) = rx.try_recv() {
            match event {
                AgentEvent::Slash(msg) => slash = msg,
                AgentEvent::TurnDiagnostics {
                    provider, model, ..
                } => {
                    diagnostics = Some((provider, model));
                }
                _ => {}
            }
        }
        assert!(slash.contains("provider -> chatgpt"), "{slash}");
        assert_eq!(
            diagnostics,
            Some(("chatgpt".to_string(), "gpt-4o".to_string()))
        );

        assert_eq!(handle_slash("/model glm 5.1", &mut agent), Some(true));
        assert_eq!(agent.provider_id, "glm");
        assert_eq!(agent.model, "glm-5.1");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn slash_model_pins_runtime_model_against_global_override() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-model-runtime-pin");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("DEXT_PROVIDER", "chatgpt");
        std::env::set_var("DEXT_MODEL", "glm-5.1");
        std::env::set_var("DEXT_MODEL_FORCE", "1");
    }

    let result = (|| -> Result<()> {
        let mut store = load_auth_store()?;
        store.providers.insert(
            "chatgpt".to_string(),
            StoredCredential::ApiKey {
                key: "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string(),
            },
        );
        save_auth_store(&store)?;

        let mut agent = test_agent(&root);
        agent.reload_provider(Some("chatgpt"), false)?;
        assert_eq!(agent.model, "glm-5.1");

        assert_eq!(handle_slash("/model gpt5.3codex", &mut agent), Some(true));
        assert_eq!(agent.provider_id, "chatgpt");
        assert_eq!(agent.model, "gpt-5.3-codex");

        agent.reload_provider(None, false)?;
        assert_eq!(agent.model, "gpt-5.3-codex");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_PROVIDER");
        std::env::remove_var("DEXT_MODEL");
        std::env::remove_var("DEXT_MODEL_FORCE");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn normalize_chatgpt_model_slug_accepts_compact_aliases() {
    assert_eq!(normalize_chatgpt_model_slug("gpt5.3codex"), "gpt-5.3-codex");
    assert_eq!(
        normalize_chatgpt_model_slug("GPT 5 3 CODEX"),
        "gpt-5.3-codex"
    );
    assert_eq!(normalize_chatgpt_model_slug("gpt5codex"), "gpt-5-codex");
}

#[test]
fn implementation_model_mitigation_lowers_codex_53_xhigh() {
    let root = temp_test_dir("codex-53-implementation-effort-mitigation");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.provider_id = "chatgpt".to_string();
    agent.model = "gpt-5.3-codex".to_string();
    agent.thinking_effort = ThinkingEffort::XHigh;

    let note = agent
        .apply_implementation_phase_model_mitigation()
        .expect("mitigation note");
    assert_eq!(agent.thinking_effort(), ThinkingEffort::Medium);
    assert!(note.contains("gpt-5.3-codex"), "{note}");

    agent.thinking_effort = ThinkingEffort::XHigh;
    agent.model = "gpt-5.5".to_string();
    assert!(
        agent
            .apply_implementation_phase_model_mitigation()
            .is_none()
    );
    assert_eq!(agent.thinking_effort(), ThinkingEffort::XHigh);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn implementation_model_fallback_switches_codex_53_to_default() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_IMPL_FALLBACK_MODEL");
    }
    let root = temp_test_dir("codex-53-implementation-fallback");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.provider_id = "chatgpt".to_string();
    agent.model = "gpt-5.3-codex".to_string();

    let mut fallback_emitted = false;
    let notes = agent.action_contract_violation_runtime_notes(2, &mut fallback_emitted);
    assert!(fallback_emitted);
    assert_eq!(agent.model, "gpt-5.4");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("switched model to gpt-5.4")),
        "{notes:?}"
    );
    assert!(
        notes
            .iter()
            .any(|note| note.contains("file-mutating tool_use")),
        "{notes:?}"
    );
    assert!(
        !notes.iter().any(|note| note.contains("git_commit")),
        "{notes:?}"
    );
    assert_eq!(
        agent
            .session_model_pins
            .get(&canonical_provider_id("chatgpt"))
            .map(String::as_str),
        Some("gpt-5.4")
    );

    let _ = std::fs::remove_dir_all(&root);
    unsafe {
        std::env::remove_var("DEXT_IMPL_FALLBACK_MODEL");
    }
}

#[test]
fn implementation_model_fallback_honors_non_codex_env_override() {
    let _guard = env_lock();
    unsafe {
        std::env::set_var("DEXT_IMPL_FALLBACK_MODEL", "gpt-5.5");
    }
    let root = temp_test_dir("codex-implementation-fallback-env");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.provider_id = "chatgpt".to_string();
    agent.model = "gpt-5-codex".to_string();

    let mut fallback_emitted = false;
    let notes = agent.action_contract_violation_runtime_notes(2, &mut fallback_emitted);
    assert!(fallback_emitted);
    assert_eq!(agent.model, "gpt-5.5");
    assert!(
        notes
            .iter()
            .any(|note| note.contains("switched model to gpt-5.5")),
        "{notes:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    unsafe {
        std::env::remove_var("DEXT_IMPL_FALLBACK_MODEL");
    }
}

#[test]
fn implementation_model_fallback_ignores_codex_env_override() {
    let _guard = env_lock();
    unsafe {
        std::env::set_var("DEXT_IMPL_FALLBACK_MODEL", "gpt-5-codex");
    }
    let root = temp_test_dir("codex-implementation-fallback-codex-env");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.provider_id = "chatgpt".to_string();
    agent.model = "gpt-5.3-codex-spark".to_string();

    let mut fallback_emitted = false;
    let notes = agent.action_contract_violation_runtime_notes(2, &mut fallback_emitted);
    assert!(!fallback_emitted);
    assert_eq!(agent.model, "gpt-5.3-codex-spark");
    assert!(
        !notes.iter().any(|note| note.contains("switched model")),
        "{notes:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    unsafe {
        std::env::remove_var("DEXT_IMPL_FALLBACK_MODEL");
    }
}

#[test]
fn anthropic_streaming_request_clamps_thinking_below_max_tokens() -> Result<()> {
    let _guard = env_lock();
    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "2");
    }
    let root = temp_test_dir("anthropic-thinking-clamp");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "claude-sonnet-4-5".to_string();
    agent.thinking_effort = ThinkingEffort::XHigh;
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "sys",
        cache_control: None,
    }];

    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["max_tokens"], 2);
    assert_eq!(value["thinking"]["budget_tokens"], 1);

    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "1");
    }
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["max_tokens"], 1);
    assert!(value.get("thinking").is_none(), "{value}");

    let _ = std::fs::remove_dir_all(&root);
    unsafe {
        std::env::remove_var("DEXT_MAX_OUTPUT_TOKENS");
    }
    Ok(())
}

#[test]
fn max_output_tokens_reads_positive_env_override() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_MAX_OUTPUT_TOKENS");
    }
    assert_eq!(max_output_tokens(), 8_192);

    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "1234");
    }
    assert_eq!(max_output_tokens(), 1_234);

    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "0");
    }
    assert_eq!(max_output_tokens(), 8_192);

    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "not-a-number");
    }
    assert_eq!(max_output_tokens(), 8_192);

    unsafe {
        std::env::remove_var("DEXT_MAX_OUTPUT_TOKENS");
    }
}

#[test]
fn pseudo_tool_syntax_detection_marks_plain_text_invalid() {
    assert!(text_contains_pseudo_tool_syntax(
        "I will run this now:\nto=functions.edit_file {\"path\":\"src/main.rs\"}"
    ));
    assert!(text_contains_pseudo_tool_syntax(
        r#"{"recipient_name":"functions.bash","parameters":{"command":"cargo test"}}"#
    ));
    assert!(text_contains_pseudo_tool_syntax(
        r#"{"type":"function_call","name":"bash","arguments":{"command":"cargo test"}}"#
    ));
    assert!(blocks_contain_pseudo_tool_syntax(&[Block::Text {
        text: "tool call: functions.write_file".to_string(),
    }]));
    assert!(!text_contains_pseudo_tool_syntax(
        "I will use the edit_file tool if needed."
    ));
    assert!(!text_contains_pseudo_tool_syntax("to="));
    assert!(text_contains_pseudo_tool_syntax_for_context(
        "to=",
        ContextMode::Frugal
    ));
    assert!(text_line_looks_like_pseudo_tool_start("to="));
    assert!(text_line_looks_like_pseudo_tool_start(
        r#"{"recipient_name":"#
    ));
    assert!(!text_contains_pseudo_tool_syntax("today=functions maybe"));
    assert!(!text_line_looks_like_pseudo_tool_start(
        "today=functions maybe"
    ));
    assert!(!text_line_looks_like_pseudo_tool_start("to=day plan"));
}

#[test]
fn action_contract_note_requires_mutation_tool_use() {
    let note = action_contract_runtime_note(2);
    assert!(note.contains("edit_file|multi_edit|write_file"), "{note}");
    assert!(note.contains("bash command that mutates files"), "{note}");
    assert!(!note.contains("git_commit"), "{note}");
    assert!(
        note.contains("Text-only blocked statements do not clear"),
        "{note}"
    );
    assert!(assistant_text_has_implementation_commitment(
        "I will implement this next."
    ));
    assert!(assistant_text_has_implementation_commitment(
        "Applying patch now."
    ));
    assert!(!assistant_text_has_implementation_commitment(
        "I found the root cause."
    ));
}

#[test]
fn anthropic_request_strips_dext_only_tool_result_metadata() -> Result<()> {
    let root = temp_test_dir("anthropic-strip-tool-metadata");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::Anthropic;
    agent.history = vec![
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "tool-1".to_string(),
                name: "bash".to_string(),
                input: json!({"command": "false"}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::ToolResult {
                tool_use_id: "tool-1".to_string(),
                content: "exit: 1".to_string(),
                is_error: Some(true),
                metadata: ToolResultMetadata {
                    status: Some("failed".to_string()),
                    exit_code: Some(1),
                    duration_ms: Some(25),
                    artifact: Some("artifact.json".to_string()),
                },
            }],
        },
    ];
    let stable = "sys";
    let env = "env";
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: stable,
        cache_control: None,
    }];
    let (_, body) = agent.build_streaming_request(stable, env, &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    let result = &value["messages"][1]["content"][0];
    assert_eq!(result["type"], "tool_result");
    assert!(result.get("metadata").is_none(), "{result}");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn chatgpt_input_serializes_function_call_without_id_field() {
    // Regression: Codex rejects `input[].id` that doesn't start with `fc_`.
    // Dext stores the server's call_id on Block::ToolUse.id and must not echo it
    // as `id` on replay — only `call_id` (optional `id` gets auto-assigned).
    let root = temp_test_dir("chatgpt-input-no-id");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call_abc123".to_string(),
            name: "todo_read".to_string(),
            input: json!({}),
        }],
    });
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block("call_abc123", "(no todos)", None)],
    });

    let items = agent.history_to_chatgpt_input();
    let fc = items
        .iter()
        .find(|i| i["type"] == "function_call")
        .expect("function_call item missing");
    assert!(
        fc.get("id").is_none(),
        "function_call must not include id field (server rejects non-fc_ ids): {fc}"
    );
    assert_eq!(fc["call_id"], "call_abc123");

    let fco = items
        .iter()
        .find(|i| i["type"] == "function_call_output")
        .expect("function_call_output item missing");
    assert_eq!(fco["call_id"], "call_abc123");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn chatgpt_input_strips_orphaned_tool_results_without_matching_tool_use() {
    // Regression: ChatGPT Responses API returns HTTP 400 "No tool call found for
    // function call output with call_id ..." when a function_call_output references
    // a call_id whose function_call was compacted away or lost. The serialization
    // layer must silently drop such orphaned ToolResult blocks.
    let root = temp_test_dir("chatgpt-orphan-strip");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;

    // ToolUse + ToolResult pair that is valid — must survive.
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call_valid".to_string(),
            name: "read_file".to_string(),
            input: json!({ "path": "a.rs" }),
        }],
    });
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block("call_valid", "fn main() {}", None)],
    });

    // Orphaned ToolResult — no matching ToolUse in history.
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block("call_orphan", "stale result", None)],
    });

    let items = agent.history_to_chatgpt_input();

    let call_ids: Vec<&str> = items
        .iter()
        .filter(|i| i["type"] == "function_call")
        .map(|i| i["call_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(call_ids, vec!["call_valid"]);

    let output_ids: Vec<&str> = items
        .iter()
        .filter(|i| i["type"] == "function_call_output")
        .map(|i| i["call_id"].as_str().unwrap_or(""))
        .collect();
    assert_eq!(
        output_ids,
        vec!["call_valid"],
        "orphaned tool result must be stripped"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn chatgpt_summary_request_body_matches_current_responses_api_expectations() {
    let root = temp_test_dir("chatgpt-summary-body-fields");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.model = "gpt-5.4".to_string();

    let body = build_chatgpt_summary_request(&agent.model, COMPACT_SYSTEM, "resume this work");
    assert_eq!(body["store"], false, "store must be false");
    assert_eq!(body["stream"], true, "summary requests must stream");
    assert_eq!(body["tool_choice"], "none", "summary should disable tools");
    assert_eq!(
        body["parallel_tool_calls"], false,
        "summary should disable parallel tools"
    );
    assert_eq!(
        body["include"][0], "reasoning.encrypted_content",
        "missing include"
    );
    assert_eq!(body["text"]["verbosity"], "low", "missing low verbosity");
    assert_eq!(body["reasoning"]["effort"], "low", "missing low effort");
    assert_eq!(body["reasoning"]["summary"], "auto", "missing summary mode");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn chatgpt_request_body_maps_xhigh_to_actual_reasoning_effort_and_summary() {
    // Regression: ChatGPT/Codex models silently refuse to emit function_call items unless the
    // request body contains tool_choice, parallel_tool_calls, include (reasoning
    // encrypted_content), text.verbosity, and reasoning.effort.
    let root = temp_test_dir("chatgpt-body-fields");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    agent.model = "gpt-5.4".to_string();
    agent.thinking_effort = ThinkingEffort::XHigh;

    let body = build_chatgpt_request(
        &agent.model,
        agent.thinking_effort,
        "sys",
        "sess-1",
        agent.history_to_chatgpt_input(),
        agent.wire_tools_chatgpt(),
    );
    assert_eq!(body["tool_choice"], "auto", "missing tool_choice");
    assert_eq!(
        body["parallel_tool_calls"], true,
        "missing parallel_tool_calls"
    );
    assert_eq!(
        body["include"][0], "reasoning.encrypted_content",
        "missing include"
    );
    assert_eq!(
        body["text"]["verbosity"], "medium",
        "missing text.verbosity"
    );
    assert_eq!(
        body["reasoning"]["effort"], "xhigh",
        "xhigh must be sent to the provider, not just shown in UI"
    );
    assert_eq!(
        body["reasoning"]["summary"], "auto",
        "missing reasoning.summary"
    );
    assert_eq!(body["store"], false, "store must be false");
    assert_eq!(body["stream"], true, "stream must be true");
    assert_eq!(body["prompt_cache_key"], "sess-1");
    let body_json = body.to_string();
    assert!(
        body_json.contains("\"reasoning\":{\"effort\":\"xhigh\",\"summary\":\"auto\"}"),
        "{body_json}"
    );
    assert!(
        body_json.contains("\"include\":[\"reasoning.encrypted_content\"]"),
        "{body_json}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn chatgpt_stream_reasoning_summary_is_rendered_and_stored_as_thinking() {
    let root = temp_test_dir("chatgpt-reasoning-stream");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request);
        let body = "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking \"}\n\ndata: {\"type\":\"response.reasoning_summary_text.done\",\"text\":\"ignored because delta already populated\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
    });

    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    let resp = reqwest::get(format!("http://{addr}/stream"))
        .await
        .expect("response");
    let (blocks, _stop, _usage) = agent.read_stream_chatgpt(resp).await.expect("parse stream");
    assert!(
        matches!(
            blocks.first(),
            Some(Block::Thinking { text }) if text == "thinking "
        ),
        "{blocks:?}"
    );
    assert!(
        matches!(
            blocks.get(1),
            Some(Block::Text { text }) if text == "answer"
        ),
        "{blocks:?}"
    );
    server.join().expect("server thread");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lean_tool_profile_keeps_descriptions_useful_and_schemas_slim() {
    let read_file = provider_tool_definitions()
        .into_iter()
        .find(|tool| tool.name == "read_file")
        .expect("read_file tool");
    let wired = tools::wire_tools(&[read_file], ToolProfile::Lean);
    assert_eq!(
        wired[0].description,
        "Read capped line-numbered file window. Absolute paths ok read-only; prefer offset+limit."
    );
    assert!(
        wired[0].input_schema.to_string().contains("\"offset\""),
        "{}",
        wired[0].input_schema
    );
    assert!(
        !wired[0].input_schema.to_string().contains("description"),
        "{}",
        wired[0].input_schema
    );

    let bash = provider_tool_definitions()
        .into_iter()
        .find(|tool| tool.name == "bash")
        .expect("bash tool");
    let wired = tools::wire_tools(&[bash], ToolProfile::Lean);
    assert!(wired[0].description.contains("last-resort"));
    assert!(wired[0].description.contains("native tool"));
}

#[test]
fn chatgpt_tools_are_responses_api_shape() {
    let root = temp_test_dir("chatgpt-tools-shape");
    let root = std::fs::canonicalize(&root).unwrap();
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    let tools = agent.wire_tools_chatgpt();
    assert!(!tools.is_empty(), "test_agent should have default tools");
    for t in &tools {
        assert_eq!(t["type"], "function");
        assert!(t["name"].is_string(), "tool missing name: {t}");
        assert!(t["parameters"].is_object(), "tool missing parameters: {t}");
        assert!(
            t["strict"].is_null(),
            "tool strict must be explicit null for Codex: {t}"
        );
        // Chat-completions nested shape would have `function: {...}`. Responses API
        // is flat — guard against accidental regression.
        assert!(
            t.get("function").is_none(),
            "tool must not be chat-completions shape: {t}"
        );
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn every_schema_backed_tool_validates_happy_and_missing_paths() {
    // Every tool with a non-empty required_fields list must:
    //   (a) surface a clear issue when fields are missing,
    //   (b) pass validation when minimal required fields are present.
    // Prevents regressions like todo_write silently accepting no args.
    let cases: &[(&str, Value)] = &[
        ("read_file", json!({"path": "foo.txt"})),
        ("read_symbol", json!({"path": "foo.txt", "symbol": "foo"})),
        ("write_file", json!({"path": "foo.txt", "content": "x"})),
        (
            "edit_file",
            json!({"path": "foo.txt", "old_string": "a", "new_string": "b"}),
        ),
        ("multi_edit", json!({"path": "foo.txt", "edits": []})),
        ("bash", json!({"command": "echo hi"})),
        ("fd", json!({"pattern": "*.rs"})),
        ("rg", json!({"pattern": "foo"})),
        ("jq", json!({"filter": "."})),
        ("fzf", json!({"query": "x", "items": []})),
        ("http", json!({"args": ["GET", "https://x"]})),
        ("awk", json!({"args": ["{print}"]})),
        ("csvkit", json!({"subcommand": "csvcut", "args": ["-n"]})),
        ("git_commit", json!({"message": "c"})),
        (
            "todo_write",
            json!({"todos": [{"text": "t", "status": "pending"}]}),
        ),
    ];

    for (name, good) in cases {
        assert!(
            tool_policy::tool_input_issue(name, good).is_none(),
            "{name}: valid input flagged as invalid: {good}"
        );
        let empty = json!({});
        let issue = tool_policy::tool_input_issue(name, &empty);
        assert!(
            issue.is_some(),
            "{name}: empty input should surface a required-field issue"
        );
    }
}

#[test]
fn split_compaction_inputs_keeps_tool_pairs_intact_when_budget_would_orphan() {
    // Regression: when the reverse-budget loop keeps a ToolResult whose paired
    // ToolUse is too large to fit, the ChatGPT Responses API rejects the
    // compacted request with "No tool call found for function call output with
    // call_id ...". The pair-closure pass must pull the ToolUse back in so
    // preserved_tool_msgs never contains an orphan half.
    let root = temp_test_dir("compact-pair-close");
    let mut agent = test_agent(&root);

    // A small earlier pair that will not survive the budget cut-off.
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call_A".to_string(),
            name: "read_file".to_string(),
            input: json!({ "path": "a.rs" }),
        }],
    });
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block("call_A", "file contents", None)],
    });

    // The pair under test: a massively oversized ToolUse (over the byte budget
    // on its own) followed by a tiny ToolResult. The reverse-budget loop will
    // keep the ToolResult first, then reject the ToolUse for exceeding budget.
    let huge_payload = "x".repeat(COMPACT_PRESERVE_TOOL_BYTES);
    agent.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call_B".to_string(),
            name: "bash".to_string(),
            input: json!({ "payload": huge_payload }),
        }],
    });
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block("call_B", "ok", None)],
    });

    let old = agent.history.clone();
    let (_summary, preserved) = agent.split_compaction_inputs(&old);

    let mut uses: HashSet<String> = HashSet::new();
    let mut results: HashSet<String> = HashSet::new();
    for msg in &preserved {
        for block in &msg.content {
            match block {
                Block::ToolUse { id, .. } => {
                    uses.insert(id.clone());
                }
                Block::ToolResult { tool_use_id, .. } => {
                    results.insert(tool_use_id.clone());
                }
                _ => {}
            }
        }
    }
    for call_id in &results {
        assert!(
            uses.contains(call_id),
            "orphan tool_result with call_id {call_id}: preserved call_ids with tool_use = {uses:?}"
        );
    }
    for call_id in &uses {
        assert!(
            results.contains(call_id),
            "orphan tool_use with call_id {call_id}: preserved call_ids with tool_result = {results:?}"
        );
    }
    assert!(
        results.contains("call_B"),
        "expected call_B tool_result in preserved set"
    );
    assert!(
        uses.contains("call_B"),
        "expected call_B tool_use to be pulled back in via pair closure"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn pair_close_drops_tool_result_whose_tool_use_is_missing_entirely() {
    // Defensive: if history has a ToolResult whose paired ToolUse is nowhere
    // in `old` (corruption or partial-checkpoint replay), the orphan must be
    // dropped from preserved_tool_msgs rather than sent to the API.
    let root = temp_test_dir("compact-orphan-drop");
    let mut agent = test_agent(&root);

    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "kick off".into(),
        }],
    });
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block("call_missing", "ghost result", None)],
    });

    let old = agent.history.clone();
    let (_summary, preserved) = agent.split_compaction_inputs(&old);

    for msg in &preserved {
        for block in &msg.content {
            if let Block::ToolResult { tool_use_id, .. } = block {
                assert_ne!(
                    tool_use_id, "call_missing",
                    "orphan tool_result with no matching tool_use must be dropped"
                );
            }
        }
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn active_compaction_checkpoint_runs_when_history_crosses_active_threshold() {
    let root = temp_test_dir("active-compact-checkpoint");
    let mut agent = test_agent(&root);
    agent.model = "demo-128k".to_string();
    agent.context_window_tokens = model_context_window(&agent.model);
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "x".repeat(agent.active_compact_threshold_chars() + 1),
        }],
    });
    assert!(
        agent.history_chars() > agent.active_compact_threshold_chars(),
        "active checkpoint should run before the next provider request"
    );
    assert!(
        agent.history_chars() <= agent.compact_threshold_chars(),
        "same history should remain below the 90% end-turn threshold"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn active_compaction_runs_after_tool_results_when_history_crosses_active_threshold() {
    let root = temp_test_dir("active-compact-tool-results");
    let mut agent = test_agent(&root);
    agent.work_ledger.objective = "preserve active compaction evidence".to_string();
    agent.compact_threshold_chars = Some(20_000);
    agent.history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "start".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "ack".to_string(),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "old context ".repeat(1_800),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "noted".to_string(),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "more context ".repeat(1_800),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_active".to_string(),
                name: "read_file".to_string(),
                input: json!({"path": "src/main.rs", "offset": 1, "limit": 1}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("call_active", "line evidence", None)],
        },
    ];
    assert!(agent.should_active_compact());

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));
    let compacted = agent
        .compact_if_over_threshold(
            agent.active_compact_threshold_chars(),
            "after_active_compact_attempt",
        )
        .await;
    assert!(compacted, "active compaction should shrink the history");

    let mut saw_start = false;
    let mut saw_end = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::CompactStart => saw_start = true,
            AgentEvent::CompactEnd { .. } => saw_end = true,
            _ => {}
        }
    }
    assert!(saw_start && saw_end, "expected compact start/end events");
    assert!(
        agent
            .history
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, Block::ToolUse { id, .. } if id == "call_active")),
        "recent tool_use should remain paired after active compaction"
    );
    assert!(
        agent.history.iter().flat_map(|m| m.content.iter()).any(
            |b| matches!(b, Block::ToolResult { tool_use_id, .. } if tool_use_id == "call_active")
        ),
        "recent tool_result should remain paired after active compaction"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn compact_uses_deterministic_evidence_fallback_when_summary_request_errors() {
    // Regression: if the summary HTTP call fails mid-compact, deterministic evidence
    // should still allow compaction to finish so the TUI clears its spinner.
    let root = temp_test_dir("compact-failed-event");
    let mut agent = test_agent(&root);
    agent.work_ledger.objective = "keep deterministic evidence".to_string();

    // Build enough history for find_compact_split to return Some. Must be more
    // than COMPACT_KEEP_MESSAGES, with a user text message at the split point.
    for i in 0..10 {
        let role = if i % 2 == 0 { "user" } else { "assistant" };
        agent.history.push(Message {
            role: role.to_string(),
            content: vec![Block::Text {
                text: format!("msg {i}"),
            }],
        });
    }

    // Sink recording: capture every event emitted during compact().
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    // base_url 127.0.0.1 with no listener makes one_shot_summary fail.
    let result = agent.compact().await;
    assert!(
        result.is_ok(),
        "expected deterministic evidence fallback to compact"
    );

    let mut saw_start = false;
    let mut saw_failed = false;
    let mut saw_end = false;
    let mut saw_fallback = false;
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::CompactStart => saw_start = true,
            AgentEvent::CompactFailed { message } => {
                saw_failed = true;
                assert!(!message.is_empty(), "CompactFailed must carry a message");
            }
            AgentEvent::CompactEnd { .. } => saw_end = true,
            AgentEvent::Warn(message) if message.contains("compact fallback") => {
                saw_fallback = true;
            }
            _ => {}
        }
    }
    assert!(saw_start, "CompactStart must fire before fallback");
    assert!(
        !saw_failed,
        "fallback compaction should not emit CompactFailed"
    );
    assert!(
        saw_fallback,
        "summary failure should be visible as a fallback warning"
    );
    assert!(
        saw_end,
        "CompactEnd must fire when deterministic fallback compaction succeeds"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn packs_discover_user_global_pack_from_dext_home() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("user-pack-discovery-root");
    let home = temp_test_dir("user-pack-discovery-home");
    let pack_dir = home.join("packs/globaldemo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: globaldemo\ndescription: User-global workflow\n---\n# Global demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_PACKS_DIR");
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", &home);
    }

    let pack = packs::find_pack(&root, "globaldemo")?;
    assert_eq!(pack.name, "globaldemo");
    assert_eq!(pack.description, "User-global workflow");
    assert_eq!(pack.source, "user:~/.dext/packs");
    assert_eq!(pack.path, pack_dir);

    let listing = packs::render_pack_listing(&root);
    assert!(
        listing.contains("globaldemo — User-global workflow"),
        "{listing}"
    );
    assert!(listing.contains("source: user:~/.dext/packs"), "{listing}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
    Ok(())
}

#[test]
fn packs_discover_project_pack_and_build_prompt() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-discovery");
    let pack_dir = root.join(".dext/packs/demo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: demo\ndescription: Demo workflow\n---\n# Demo pack\n\nDo the demo workflow.\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_PACKS_DIR");
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let pack = packs::find_pack(&root, "demo")?;
    assert_eq!(pack.name, "demo");
    assert_eq!(pack.description, "Demo workflow");
    assert_eq!(pack.env_var_name(), "DEXT_PACK_DEMO_DIR");
    assert!(pack.pack_md_path.ends_with("PACK.md"));

    let listing = packs::render_pack_listing(&root);
    assert!(listing.contains("demo — Demo workflow"), "{listing}");
    assert!(listing.contains("/pack run <name> <task>"), "{listing}");

    let prompt = packs::pack_prompt(&pack, "ship it")?;
    assert!(prompt.contains("[dext pack invocation]"), "{prompt}");
    assert!(prompt.contains("Do the demo workflow."), "{prompt}");
    assert!(
        prompt.contains("User task for this pack:\nship it"),
        "{prompt}"
    );

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn packs_discovery_is_deterministic_and_dedupes_symlinked_roots() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-deterministic-discovery");
    let pack_root = root.join("shared-packs");
    let alpha = pack_root.join("alpha");
    let beta = pack_root.join("beta");
    std::fs::create_dir_all(&alpha)?;
    std::fs::create_dir_all(&beta)?;
    std::fs::write(
        alpha.join("PACK.md"),
        "---\nname: alpha\ndescription: Alpha workflow\n---\n# Alpha\n",
    )?;
    std::fs::write(
        beta.join("PACK.md"),
        "---\nname: beta\ndescription: Beta workflow\n---\n# Beta\n",
    )?;
    let alias = root.join("alias-packs");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&pack_root, &alias)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&pack_root, &alias)?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var(
            "DEXT_PACKS_DIR",
            std::env::join_paths([&alias, &pack_root])?,
        );
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let packs = packs::discover_packs(&root);
    let names = packs
        .iter()
        .filter(|pack| pack.source == "env:DEXT_PACKS_DIR")
        .map(|pack| pack.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["alpha", "beta"], "{names:?}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_PACKS_DIR");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn packs_discover_shelf_pack_and_apply_precedence() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("shelf-pack-discovery");
    let shelf_pack = root.join(".dext/shelves/community/packs/demo");
    let legacy_pack = root.join("packs/demo");
    let env_pack = root.join("external-shelf/packs/envpack");
    std::fs::create_dir_all(&shelf_pack)?;
    std::fs::create_dir_all(&legacy_pack)?;
    std::fs::create_dir_all(&env_pack)?;
    std::fs::write(
        shelf_pack.join("PACK.md"),
        "---\nname: demo\ndescription: Shelf workflow\n---\n# Shelf demo\n",
    )?;
    std::fs::write(
        legacy_pack.join("PACK.md"),
        "---\nname: demo\ndescription: Legacy workflow\n---\n# Legacy demo\n",
    )?;
    std::fs::write(
        env_pack.join("PACK.md"),
        "---\nname: envpack\ndescription: Env shelf workflow\n---\n# Env demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_PACKS_DIR");
        std::env::set_var("DEXT_SHELVES_DIR", root.join("external-shelf"));
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let pack = packs::find_pack(&root, "demo")?;
    assert_eq!(pack.description, "Shelf workflow");
    assert_eq!(pack.shelf.as_deref(), Some("community"));
    assert!(pack.source.contains("project:.dext/shelves/community"));

    let env = packs::find_pack(&root, "envpack")?;
    assert_eq!(env.shelf.as_deref(), Some("external-shelf"));
    assert!(env.source.contains("env:DEXT_SHELVES_DIR/external-shelf"));

    let project_names = packs::discover_packs(&root)
        .into_iter()
        .map(|pack| pack.name)
        .collect::<Vec<_>>();
    let demo_count = project_names.iter().filter(|name| *name == "demo").count();
    assert_eq!(demo_count, 1, "{project_names:?}");

    let listing = packs::render_pack_listing(&root);
    assert!(listing.contains("demo — Shelf workflow"), "{listing}");
    assert!(listing.contains("shelf: community"), "{listing}");
    assert!(
        listing.contains("source: project:.dext/shelves/community"),
        "{listing}"
    );

    let inspect = packs::render_pack_inspect(&root, "demo")?;
    assert!(inspect.contains("shelf: community"), "{inspect}");

    let prompt = packs::pack_prompt(&pack, "ship it")?;
    assert!(prompt.contains("Shelf: community"), "{prompt}");

    let summary = packs::pack_summary_for_prompt(&root).unwrap_or_default();
    assert!(summary.contains("demo[community]"), "{summary}");

    let registry = shelves::ShelfRegistry::discover(&root);
    assert!(registry.is_empty());

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn slash_shelves_lists_typed_manifest_registry() {
    let _guard = env_lock();
    let root = temp_test_dir("slash-shelves");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let shelf_dir = root.join(".dext/shelves/community");
    std::fs::create_dir_all(&shelf_dir).expect("create shelf dir");
    std::fs::write(
        shelf_dir.join("shelf.json"),
        r#"{
  "id": "community",
  "name": "Community",
  "description": "shared typed abilities",
  "packs": [{
    "id": "research",
    "name": "Research",
    "version": "0.1.0",
    "description": "research helpers",
    "abilities": [{"ability": "command", "name": "scan", "usage": "scan <target>", "description": "scan target"}]
  }]
}"#,
    )
    .expect("write shelf manifest");
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let mut agent = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    assert_eq!(handle_slash("/shelves", &mut agent), Some(true));
    let slash = drain_events(&mut rx)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .unwrap_or_default();
    assert!(slash.contains("shelves:"), "{slash}");
    assert!(
        slash.contains("Community — shared typed abilities"),
        "{slash}"
    );
    assert!(slash.contains("command:scan — scan target"), "{slash}");
    assert!(slash.contains("scope: project"), "{slash}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn slash_pack_list_and_inspect_use_discovered_packs() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-pack");
    let pack_dir = root.join("packs/demo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: demo\ndescription: Slash demo\n---\n# Demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_PACKS_DIR");
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let mut agent = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    assert_eq!(handle_slash("/pack list", &mut agent), Some(true));
    assert_eq!(handle_slash("/pack inspect demo", &mut agent), Some(true));
    let slash_text = drain_events(&mut rx)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n---\n");
    assert!(slash_text.contains("demo — Slash demo"), "{slash_text}");
    assert!(slash_text.contains("pack: demo"), "{slash_text}");
    assert!(slash_text.contains("workflow:"), "{slash_text}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn conversational_pack_inference_requires_invocation_intent() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-inference");
    let pack_dir = root.join("packs/demo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(pack_dir.join("PACK.md"), "---\nname: demo\n---\n# Demo\n")?;
    unsafe {
        std::env::remove_var("DEXT_PACKS_DIR");
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let invocation = packs::infer_pack_invocation(&root, "run demo on faster tests")
        .expect("expected pack invocation");
    assert_eq!(invocation.pack.name, "demo");
    assert_eq!(invocation.task, "run demo on faster tests");
    assert!(packs::infer_pack_invocation(&root, "how do I run demo?").is_none());
    assert!(packs::infer_pack_invocation(&root, "explain demo").is_none());

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn parse_cli_options_accepts_pack_flag() -> Result<()> {
    let opts = parse_cli_options(vec![
        "--pack".to_string(),
        "demo".to_string(),
        "do".to_string(),
        "thing".to_string(),
    ])?;
    assert_eq!(opts.pack.as_deref(), Some("demo"));
    assert_eq!(opts.positional, vec!["do".to_string(), "thing".to_string()]);

    let opts = parse_cli_options(vec!["--pack=demo".to_string(), "do thing".to_string()])?;
    assert_eq!(opts.pack.as_deref(), Some("demo"));
    assert_eq!(opts.positional, vec!["do thing".to_string()]);
    Ok(())
}

#[test]
fn squash_identical_error_result_content_preserves_tool_result_ids() {
    let results = vec![
        Block::ToolResult {
            tool_use_id: "call_1".to_string(),
            content: "missing command".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
        Block::ToolResult {
            tool_use_id: "call_2".to_string(),
            content: "missing command".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
        Block::ToolResult {
            tool_use_id: "call_3".to_string(),
            content: "missing command".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
    ];
    let squashed = squash_identical_error_result_content(results);
    assert_eq!(squashed.len(), 3, "must preserve one result per tool_use");
    let ids: Vec<&str> = squashed
        .iter()
        .map(|block| match block {
            Block::ToolResult { tool_use_id, .. } => tool_use_id.as_str(),
            _ => panic!("expected ToolResult"),
        })
        .collect();
    assert_eq!(ids, vec!["call_1", "call_2", "call_3"]);
    let Block::ToolResult { content, .. } = &squashed[0] else {
        panic!("expected ToolResult");
    };
    assert!(
        content.contains("squashed: 3 identical error results"),
        "{content}"
    );
    assert!(content.contains("missing command"), "{content}");
    let Block::ToolResult { content, .. } = &squashed[1] else {
        panic!("expected ToolResult");
    };
    assert!(content.contains("duplicate error elided"), "{content}");
}

#[test]
fn squash_preserves_non_error_results() {
    let results = vec![
        Block::ToolResult {
            tool_use_id: "call_1".to_string(),
            content: "ok output".to_string(),
            is_error: None,
            metadata: ToolResultMetadata::default(),
        },
        Block::ToolResult {
            tool_use_id: "call_2".to_string(),
            content: "ok output".to_string(),
            is_error: None,
            metadata: ToolResultMetadata::default(),
        },
    ];
    let squashed = squash_identical_error_result_content(results);
    assert_eq!(squashed.len(), 2, "non-error results should not collapse");
}

#[test]
fn squash_handles_mixed_error_types() {
    let results = vec![
        Block::ToolResult {
            tool_use_id: "call_1".to_string(),
            content: "error A".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
        Block::ToolResult {
            tool_use_id: "call_2".to_string(),
            content: "error B".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
        Block::ToolResult {
            tool_use_id: "call_3".to_string(),
            content: "error A".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
    ];
    let squashed = squash_identical_error_result_content(results);
    // error A run, then error B (standalone), then error A (different run)
    assert_eq!(
        squashed.len(),
        3,
        "different error messages should not merge"
    );
}

#[test]
fn squash_small_batches_unchanged() {
    let results = vec![
        Block::ToolResult {
            tool_use_id: "call_1".to_string(),
            content: "err".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
        Block::ToolResult {
            tool_use_id: "call_2".to_string(),
            content: "err".to_string(),
            is_error: Some(true),
            metadata: ToolResultMetadata::default(),
        },
    ];
    // len <= 2: returned as-is
    let squashed = squash_identical_error_result_content(results);
    assert_eq!(squashed.len(), 2);
}
