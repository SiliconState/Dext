use super::*;
use crate::provider::{
    ANTHROPIC_API_VERSION, DEFAULT_LOCAL_MODEL, ModelCapabilities, ModelPricing, ModelSpec,
    chatgpt_reasoning_effort, clear_cached_local_llama_context_windows,
    list_models_for_available_providers, merge_provider_profile, normalize_chatgpt_model_slug,
    parse_llama_context_window, refresh_local_llama_context_window,
    resolve_provider_model_selection,
};
use crate::session::{
    append_log_line, canonicalize_mutation_path, canonicalize_read_tool_path,
    cap_latest_log_buffer, latest_log_path, render_limited_lines, validate_session_name,
};
use crate::tools::{self, is_parallel_safe_tool};
use serde_json::json;
use std::net::TcpListener;
use std::process::Command;
use std::sync::OnceLock;

fn env_lock() -> std::sync::MutexGuard<'static, ()> {
    crate::test_env_lock().lock().expect("env lock")
}

fn restore_env_var(name: &str, old_value: Option<std::ffi::OsString>) {
    unsafe {
        match old_value {
            Some(value) => std::env::set_var(name, value),
            None => std::env::remove_var(name),
        }
    }
}

struct RemoveDirOnDrop(PathBuf);

impl Drop for RemoveDirOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
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
    #[cfg(unix)]
    let temp_root = PathBuf::from("/tmp");
    #[cfg(not(unix))]
    let temp_root = std::env::temp_dir();
    let dir = temp_root.join(unique);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    std::fs::canonicalize(&dir).expect("canonical temp dir")
}

fn mutation_ok<T>(result: std::result::Result<T, String>) -> Result<T> {
    result.map_err(anyhow::Error::msg)
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
    let session_id = new_session_id();
    Agent {
        client: Arc::new(OnceLock::new()),
        provider_id: "test".to_string(),
        provider_profile: None,
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
        last_request_usage: Usage::default(),
        interrupt: Arc::new(AtomicBool::new(false)),
        shelf_registry: shelves::ShelfRegistry::discover(root),
        hooks: Hooks::default(),
        pack_hook_env: Vec::new(),
        active_pack_hook_paths: HashSet::new(),
        suppress_pack_activation: false,
        state_lock: None,
        session_enabled: true,
        session_id: session_id.clone(),
        latest_session_path: session_latest_session_path(root, &session_id),
        latest_log_path: session_latest_log_path(root, &session_id),
        pending_login_provider: None,
        suppress_checkpoints: false,
        last_checkpoint_at: None,
        session_model_pins: HashMap::new(),
        partial_stream_text: None,
        compact_threshold_chars: None,
        compact_threshold_percent: None,
        context_window_tokens: model_context_window("test-model"),
        approval_profile: ApprovalProfile::default(),
        approval_policy_source: ApprovalPolicySource::default(),
        sandbox_profile: SandboxProfile::default(),
        context_mode: ContextMode::default(),
        context_mode_explicit: false,
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
        git_credential: None,
        checkpoint_cache: git_checkpoints::RepoRootCache::new(),
        checkpoint_ordinal: 0,
        prompt_scan_cache: Mutex::new(None),
        prompt_scan_epoch: 0,
        last_checkpoint_signature: None,
    }
}

struct FixedPermissionSink {
    choice: Choice,
    requests: Arc<std::sync::atomic::AtomicUsize>,
}

impl EventSink for FixedPermissionSink {
    fn emit(&mut self, _event: AgentEvent) {}

    fn request_permission(&mut self, _name: &str, _input: &Value) -> Choice {
        self.requests
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.choice
    }

    fn local_auth_prompt(&mut self, _tool: &str, _message: &str) {}
}

fn spawn_openai_tool_call_server(
    tool_call_id: &str,
    tool_name: &str,
    input: &Value,
) -> (String, std::thread::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind tool-call test server");
    let addr = listener.local_addr().expect("tool-call test server addr");
    let arguments = serde_json::to_string(input).expect("serialize tool input");
    let chunk = json!({
        "choices": [{
            "delta": {
                "tool_calls": [{
                    "index": 0,
                    "id": tool_call_id,
                    "function": {"name": tool_name, "arguments": arguments}
                }]
            },
            "finish_reason": "tool_calls"
        }]
    });
    let body = format!("data: {chunk}\n\ndata: [DONE]\n\n");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept tool-call request");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("set tool-call request timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        let header_end = loop {
            let read = stream.read(&mut buf).expect("read tool-call request");
            assert!(read > 0, "client closed before sending request headers");
            request.extend_from_slice(&buf[..read]);
            if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                break end + 4;
            }
        };
        let headers = String::from_utf8_lossy(&request[..header_end]);
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        assert!(
            content_length <= 1024 * 1024,
            "tool-call request body too large"
        );
        while request.len() < header_end + content_length {
            let read = stream.read(&mut buf).expect("read tool-call request body");
            assert!(read > 0, "client closed before request body completed");
            request.extend_from_slice(&buf[..read]);
        }
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write tool-call response");
    });
    (format!("http://{addr}"), server)
}

fn configure_local_openai_agent(agent: &mut Agent, base_url: String) {
    agent.api_provider = ApiProvider::OpenAi;
    agent.provider_id = "local".to_string();
    agent.provider_requires_api_key = false;
    agent.api_key.clear();
    agent.base_url = base_url;
    agent.model = DEFAULT_LOCAL_MODEL.to_string();
    agent.set_approval_profile(ApprovalProfile::Always);
}

fn last_tool_result(history: &[Message]) -> Option<(&str, &str)> {
    history.iter().rev().find_map(|message| {
        message.content.iter().rev().find_map(|block| match block {
            Block::ToolResult {
                content, metadata, ..
            } => Some((content.as_str(), metadata.status.as_deref().unwrap_or(""))),
            _ => None,
        })
    })
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
fn git_status_summary_parses_branch_tracking_and_dirty_state() {
    assert_eq!(
        parse_git_status_summary(b"## main...origin/main [ahead 2]\n"),
        Some("main".to_string())
    );
    assert_eq!(
        parse_git_status_summary(b"## feature/welcome...origin/feature/welcome\n M src/tui.rs\n"),
        Some("feature/welcome (dirty)".to_string())
    );
    assert_eq!(
        parse_git_status_summary(b"## No commits yet on main\n?? README.md\n"),
        Some("main (dirty)".to_string())
    );
    assert_eq!(
        parse_git_status_summary(b"fatal: not a git repository\n"),
        None
    );
}

#[cfg(unix)]
fn git_stdout(root: &Path, args: &[&str]) -> String {
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
    String::from_utf8(output.stdout)
        .expect("Git output is UTF-8")
        .trim()
        .to_string()
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
    assert_eq!(cp.paths_hint, vec!["note.txt"]);
    let sidecar = root.join(".dext/checkpoints").join(&cp.id).join("note.txt");
    assert_eq!(
        std::fs::read_to_string(&sidecar).expect("read sidecar"),
        "before\n"
    );
    assert!(
        std::fs::read_to_string(root.join(".git/info/exclude"))
            .expect("read local exclude")
            .lines()
            .any(|line| line.trim() == "/.dext/"),
        "checkpoint state must be locally excluded from Git"
    );
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let sidecar_mode = std::fs::metadata(&sidecar)
            .expect("sidecar metadata")
            .permissions()
            .mode()
            & 0o777;
        let manifest_mode = std::fs::metadata(root.join(".dext/checkpoints/manifest.txt"))
            .expect("manifest metadata")
            .permissions()
            .mode()
            & 0o777;
        let checkpoint_dir_mode = std::fs::metadata(root.join(".dext/checkpoints"))
            .expect("checkpoint dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(sidecar_mode, 0o600, "sidecar mode {sidecar_mode:o}");
        assert_eq!(manifest_mode, 0o600, "manifest mode {manifest_mode:o}");
        assert_eq!(
            checkpoint_dir_mode, 0o700,
            "checkpoint directory mode {checkpoint_dir_mode:o}"
        );
    }
    std::fs::write(root.join("note.txt"), "after\n").expect("mutate untracked");

    git_checkpoints::restore_worktree(&root, &cp, git_checkpoints::RestoreMode::Worktree)
        .expect("restore checkpoint");
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("read restored"),
        "before\n"
    );
}

#[test]
fn checkpoint_worktree_restore_preserves_newer_index_state() {
    let root = temp_test_dir("checkpoint-preserve-index");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    std::fs::write(root.join("tracked.txt"), "checkpoint-state\n").expect("write checkpoint state");
    git_ok(&root, &["add", "tracked.txt"]);
    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["tracked.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");

    std::fs::write(root.join("tracked.txt"), "new-index-state\n").expect("write index state");
    git_ok(&root, &["add", "tracked.txt"]);
    std::fs::write(root.join("tracked.txt"), "new-worktree-state\n").expect("write worktree state");

    git_checkpoints::restore_worktree(&root, &checkpoint, git_checkpoints::RestoreMode::Worktree)
        .expect("restore worktree only");
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).expect("read restored worktree"),
        "checkpoint-state\n"
    );
    let index = Command::new("git")
        .args(["show", ":tracked.txt"])
        .current_dir(&root)
        .output()
        .expect("read index");
    assert!(index.status.success());
    assert_eq!(String::from_utf8_lossy(&index.stdout), "new-index-state\n");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_checkpoint_unborn_head_is_noop_before_and_after_mutation() {
    let root = temp_test_dir("checkpoint-unborn-head");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);

    let before = git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("checkpoint before mutation");
    assert!(before.is_none());

    std::fs::write(root.join("created.txt"), "new\n").expect("write created");
    let after =
        git_checkpoints::create_checkpoint(&root, "write_file", &["created.txt".to_string()], 2)
            .expect("checkpoint after mutation");
    assert!(after.is_none());
    assert!(!root.join(".dext/checkpoints/manifest.txt").exists());
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_symlinked_storage_root() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("checkpoint-storage-symlink");
    let outside = temp_test_dir("checkpoint-storage-symlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    symlink(&outside, root.join(".dext")).expect("symlink checkpoint root");

    let error = match git_checkpoints::create_checkpoint(&root, "bash", &[], 1) {
        Err(error) => error,
        Ok(_) => panic!("symlinked checkpoint storage must be rejected"),
    };
    assert!(error.contains("not a real directory"), "{error}");
    assert!(!outside.join("checkpoints").exists());
    let refs = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/dext/checkpoints",
        ])
        .current_dir(&root)
        .output()
        .expect("list checkpoint refs");
    assert!(refs.status.success());
    assert!(refs.stdout.is_empty(), "checkpoint ref must not be created");

    let _ = std::fs::remove_file(root.join(".dext"));
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn checkpoint_skips_external_hints_and_restore_ignores_them() {
    let root = temp_test_dir("checkpoint-external-path");
    let outside = temp_test_dir("checkpoint-external-target");
    let outside_file = outside.join("keep.txt");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(&outside_file, "outside\n").expect("write outside");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let skipped = git_checkpoints::create_checkpoint(
        &root,
        "write_file",
        &[outside_file.display().to_string()],
        1,
    )
    .expect("external checkpoint hint is handled");
    assert!(skipped.is_none(), "external paths are not Git-restorable");

    let mut checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["tracked.txt".to_string()], 2)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    checkpoint.paths_hint = vec![outside_file.display().to_string()];
    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("unsafe manifest path must fail before restore");
    assert!(error.contains("unsafe checkpoint path"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&outside_file).expect("read outside"),
        "outside\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_symlinked_git_exclude() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("checkpoint-exclude-symlink");
    let outside = temp_test_dir("checkpoint-exclude-symlink-target");
    let outside_file = outside.join("exclude.txt");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(&outside_file, "keep\n").expect("write outside exclude");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    std::fs::remove_file(root.join(".git/info/exclude")).expect("remove local exclude");
    symlink(&outside_file, root.join(".git/info/exclude")).expect("symlink local exclude");

    let error = match git_checkpoints::create_checkpoint(&root, "bash", &[], 1) {
        Err(error) => error,
        Ok(_) => panic!("symlinked Git exclude must be rejected"),
    };
    assert!(error.contains("not a real file"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&outside_file).expect("read outside exclude"),
        "keep\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_hardlinked_git_exclude() {
    let root = temp_test_dir("checkpoint-exclude-hardlink");
    let outside = temp_test_dir("checkpoint-exclude-hardlink-target");
    let outside_file = outside.join("exclude.txt");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(&outside_file, "keep\n").expect("write outside exclude");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    std::fs::remove_file(root.join(".git/info/exclude")).expect("remove local exclude");
    std::fs::hard_link(&outside_file, root.join(".git/info/exclude"))
        .expect("hardlink local exclude");

    let error = match git_checkpoints::create_checkpoint(&root, "bash", &[], 1) {
        Err(error) => error,
        Ok(_) => panic!("hardlinked Git exclude must be rejected"),
    };
    assert!(error.contains("single link"), "{error}");
    assert_eq!(
        std::fs::read_to_string(&outside_file).expect("read outside exclude"),
        "keep\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_symlinked_git_info_directory() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("checkpoint-info-symlink");
    let outside = temp_test_dir("checkpoint-info-symlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    std::fs::remove_dir_all(root.join(".git/info")).expect("remove Git info directory");
    symlink(&outside, root.join(".git/info")).expect("symlink Git info directory");

    let error = match git_checkpoints::create_checkpoint(&root, "bash", &[], 1) {
        Err(error) => error,
        Ok(_) => panic!("symlinked Git info directory must be rejected"),
    };
    assert!(error.contains("escapes repository metadata"), "{error}");
    assert!(!outside.join("exclude").exists());

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_restore_rejects_symlinked_sidecar_files() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("checkpoint-sidecar-symlink");
    let outside = temp_test_dir("checkpoint-sidecar-symlink-target");
    let outside_file = outside.join("secret.txt");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(root.join("note.txt"), "before\n").expect("write untracked");
    std::fs::write(&outside_file, "outside-secret\n").expect("write outside");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["note.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    let sidecar = root
        .join(".dext/checkpoints")
        .join(&checkpoint.id)
        .join("note.txt");
    std::fs::remove_file(&sidecar).expect("remove real sidecar");
    symlink(&outside_file, &sidecar).expect("symlink sidecar");
    std::fs::write(root.join("note.txt"), "after\n").expect("mutate note");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("unsafe sidecar must fail before restore");
    assert!(error.contains("unsafe sidecar symlink"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("read unchanged note"),
        "after\n"
    );
    assert_eq!(
        std::fs::read_to_string(&outside_file).expect("read outside"),
        "outside-secret\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn checkpoint_restore_fails_closed_when_required_sidecar_is_missing() {
    let root = temp_test_dir("checkpoint-sidecar-missing");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(root.join("note.txt"), "before\n").expect("write untracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["note.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    assert!(checkpoint.includes_untracked_sidecar);
    std::fs::remove_dir_all(root.join(".dext/checkpoints").join(&checkpoint.id))
        .expect("remove sidecar");
    std::fs::write(root.join("note.txt"), "after\n").expect("mutate note");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("missing sidecar must fail before restore");
    assert!(error.contains("sidecar is missing"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("read unchanged note"),
        "after\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_restore_rejects_ref_oid_mismatch_before_mutation() {
    let root = temp_test_dir("checkpoint-ref-mismatch");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["tracked.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    std::fs::write(root.join("tracked.txt"), "second\n").expect("write second");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "second"]);
    git_ok(&root, &["update-ref", &checkpoint.ref_name, "HEAD"]);
    std::fs::write(root.join("tracked.txt"), "current\n").expect("write current");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("tampered checkpoint ref must fail before restore");
    assert!(error.contains("no longer matches"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).expect("read unchanged tracked file"),
        "current\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_restore_refuses_to_replace_a_directory_with_absent_file_state() {
    let root = temp_test_dir("checkpoint-file-became-directory");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["new.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    std::fs::create_dir(root.join("new.txt")).expect("replace target with directory");
    std::fs::write(root.join("new.txt/keep.txt"), "keep\n").expect("write nested file");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("directory replacement must fail before restore");
    assert!(
        error.contains("refusing to recursively remove")
            || error.contains("refusing to replace directory"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("new.txt/keep.txt")).expect("read nested file"),
        "keep\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn internal_checkpoint_git_never_executes_repository_configured_helpers() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let old_git_config_count = std::env::var_os("GIT_CONFIG_COUNT");
    let old_git_config_key = std::env::var_os("GIT_CONFIG_KEY_0");
    let old_git_config_value = std::env::var_os("GIT_CONFIG_VALUE_0");
    let old_external_diff = std::env::var_os("GIT_EXTERNAL_DIFF");
    let old_timeout = std::env::var_os("DEXT_INTERNAL_GIT_TIMEOUT_SECS");
    let root = temp_test_dir("checkpoint-hostile-git-config");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join(".gitattributes"), "*.txt filter=evil diff=evil\n")?;
    std::fs::write(root.join("tracked.txt"), "base\n")?;
    git_ok(&root, &["add", ".gitattributes", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let marker = root.join("repository-helper-ran");
    let helper = root.join("hostile-helper.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$0 $*\" >> {}\nexit 1\n",
            shell_single_quote(&marker.display().to_string())
        ),
    )?;
    let mut permissions = std::fs::metadata(&helper)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions)?;
    let helper_text = helper
        .to_str()
        .ok_or_else(|| anyhow::anyhow!("non-UTF-8 helper path"))?;

    let hooks = root.join("hostile-hooks");
    std::fs::create_dir(&hooks)?;
    let reference_hook = hooks.join("reference-transaction");
    std::fs::write(
        &reference_hook,
        format!(
            "#!/bin/sh\nprintf hook >> {}\nexit 1\n",
            shell_single_quote(&marker.display().to_string())
        ),
    )?;
    let mut permissions = std::fs::metadata(&reference_hook)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&reference_hook, permissions)?;

    git_ok(&root, &["config", "core.fsmonitor", helper_text]);
    git_ok(
        &root,
        &[
            "config",
            "core.hooksPath",
            hooks
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("non-UTF-8 hooks path"))?,
        ],
    );
    for key in [
        "filter.evil.clean",
        "filter.evil.smudge",
        "filter.evil.process",
        "diff.evil.command",
        "diff.evil.textconv",
        "diff.external",
    ] {
        git_ok(&root, &["config", key, helper_text]);
    }
    git_ok(&root, &["config", "filter.evil.required", "true"]);
    let _ = std::fs::remove_file(&marker);

    unsafe {
        std::env::set_var("GIT_CONFIG_COUNT", "1");
        std::env::set_var("GIT_CONFIG_KEY_0", "core.fsmonitor");
        std::env::set_var("GIT_CONFIG_VALUE_0", helper_text);
        std::env::set_var("GIT_EXTERNAL_DIFF", helper_text);
        std::env::set_var("DEXT_INTERNAL_GIT_TIMEOUT_SECS", "2");
    }

    let result = (|| -> Result<()> {
        std::fs::write(root.join("tracked.txt"), "checkpoint-state\n")?;
        let summary = tui_git_summary(&root);
        assert!(
            summary
                .as_deref()
                .is_some_and(|summary| summary.ends_with(" (dirty)")),
            "unexpected TUI Git summary: {summary:?}"
        );
        assert!(
            !marker.exists(),
            "TUI Git summary executed a repository-configured helper: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
        let checkpoint = git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
            .map_err(anyhow::Error::msg)?
            .ok_or_else(|| anyhow::anyhow!("dirty repository must create a checkpoint"))?;
        assert!(
            !marker.exists(),
            "checkpoint creation executed a repository-configured helper: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );

        std::fs::write(root.join("tracked.txt"), "current-state\n")?;
        git_checkpoints::preview_restore(&root, &checkpoint).map_err(anyhow::Error::msg)?;
        assert!(
            !marker.exists(),
            "checkpoint preview executed a repository-configured helper: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );

        git_checkpoints::restore_worktree(
            &root,
            &checkpoint,
            git_checkpoints::RestoreMode::Worktree,
        )
        .map_err(anyhow::Error::msg)?;
        assert_eq!(
            std::fs::read_to_string(root.join("tracked.txt"))?,
            "checkpoint-state\n"
        );
        assert!(
            !marker.exists(),
            "checkpoint restore executed a repository-configured helper: {}",
            std::fs::read_to_string(&marker).unwrap_or_default()
        );
        Ok(())
    })();

    restore_env_var("GIT_CONFIG_COUNT", old_git_config_count);
    restore_env_var("GIT_CONFIG_KEY_0", old_git_config_key);
    restore_env_var("GIT_CONFIG_VALUE_0", old_git_config_value);
    restore_env_var("GIT_EXTERNAL_DIFF", old_external_diff);
    restore_env_var("DEXT_INTERNAL_GIT_TIMEOUT_SECS", old_timeout);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn checkpoint_prune_creates_private_ignored_storage() {
    let root = temp_test_dir("checkpoint-prune-storage");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    git_checkpoints::prune(&root, None, None).expect("prune empty checkpoint namespace");
    assert!(
        std::fs::read_to_string(root.join(".git/info/exclude"))
            .expect("read exclude")
            .lines()
            .any(|line| line.trim() == "/.dext/")
    );
    assert!(root.join(".dext/checkpoints/manifest.txt").is_file());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_snapshots_untracked_and_preview_names_created_files() {
    let root = temp_test_dir("checkpoint-untracked-delta");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    // A pre-existing untracked file is part of the snapshot.
    std::fs::write(root.join("existing.txt"), "x\n").expect("write existing untracked");

    // bash is write-risk → checkpointed, with an untracked snapshot.
    let cp = git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("create checkpoint")
        .expect("checkpoint exists");
    assert!(
        cp.untracked_snapshot.iter().any(|p| p == "existing.txt"),
        "snapshot should record pre-existing untracked file: {:?}",
        cp.untracked_snapshot
    );

    // Simulate the command creating a new untracked file and removing the old one.
    std::fs::write(root.join("created.txt"), "new\n").expect("write created untracked");
    std::fs::remove_file(root.join("existing.txt")).expect("remove existing untracked");

    let preview = git_checkpoints::preview_restore(&root, &cp).expect("preview");
    assert!(
        preview.contains("created.txt"),
        "preview should name created untracked file:\n{preview}"
    );
    assert!(
        preview.contains("existing.txt"),
        "preview should name removed untracked file:\n{preview}"
    );
}

#[test]
fn checkpoint_creation_enforces_retention_ceiling() {
    let root = temp_test_dir("checkpoint-retention");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    for ordinal in 1..=25 {
        git_checkpoints::create_checkpoint(&root, "bash", &[], ordinal)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    }

    let checkpoints =
        git_checkpoints::list_checkpoints(&root, usize::MAX).expect("list checkpoints");
    assert_eq!(checkpoints.len(), 20, "checkpoint retention ceiling");
    let refs = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/dext/checkpoints",
        ])
        .current_dir(&root)
        .output()
        .expect("list checkpoint refs");
    assert!(
        refs.status.success(),
        "{}",
        String::from_utf8_lossy(&refs.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&refs.stdout).lines().count(), 20);
    assert_eq!(
        std::fs::read_to_string(root.join(".dext/checkpoints/manifest.txt"))
            .expect("read bounded checkpoint manifest")
            .lines()
            .count(),
        20,
        "automatic retention must compact the private manifest too"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_prune_removes_refs_missing_from_private_manifest() {
    let root = temp_test_dir("checkpoint-orphan-ref");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint = git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("create checkpoint")
        .expect("checkpoint exists");
    let orphan = "refs/dext/checkpoints/orphan";
    git_ok(&root, &["update-ref", orphan, "HEAD"]);
    let sibling = "refs/dext/checkpoints-sibling/keep";
    git_ok(&root, &["update-ref", sibling, "HEAD"]);
    let orphan_sidecar = root.join(".dext/checkpoints/orphan_sidecar");
    std::fs::create_dir(&orphan_sidecar).expect("create orphan sidecar");
    std::fs::write(orphan_sidecar.join("data"), "orphan\n").expect("write orphan sidecar");

    let result = git_checkpoints::prune(&root, None, None).expect("prune checkpoints");
    assert!(result.contains("pruned 1 checkpoint"), "{result}");
    assert!(result.contains("1 orphan sidecar entry"), "{result}");

    assert!(!orphan_sidecar.exists(), "orphan sidecar should be removed");
    assert!(
        Command::new("git")
            .args(["show-ref", "--verify", "--quiet", orphan])
            .current_dir(&root)
            .status()
            .is_ok_and(|status| !status.success()),
        "orphan checkpoint ref should be removed"
    );
    assert!(
        Command::new("git")
            .args(["show-ref", "--verify", "--quiet", sibling])
            .current_dir(&root)
            .status()
            .is_ok_and(|status| status.success()),
        "manual prune must preserve sibling ref namespaces"
    );
    assert!(
        Command::new("git")
            .args(["show-ref", "--verify", "--quiet", &checkpoint.ref_name])
            .current_dir(&root)
            .status()
            .is_ok_and(|status| status.success()),
        "manifest-backed checkpoint should remain"
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_lookup_rejects_ambiguous_or_empty_prefixes() {
    let root = temp_test_dir("checkpoint-lookup-ambiguity");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let first = git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("create first checkpoint")
        .expect("first checkpoint exists");
    let second = git_checkpoints::create_checkpoint(&root, "bash", &[], 2)
        .expect("create second checkpoint")
        .expect("second checkpoint exists");
    let common_len = first
        .id
        .bytes()
        .zip(second.id.bytes())
        .take_while(|(left, right)| left == right)
        .count();
    assert!(
        common_len > 0,
        "checkpoint IDs should share a timestamp prefix"
    );
    let ambiguous = &first.id[..common_len];
    let error = git_checkpoints::find_checkpoint(&root, ambiguous)
        .expect_err("ambiguous checkpoint selector must fail closed");
    assert!(error.contains("ambiguous"), "{error}");
    assert!(
        git_checkpoints::find_checkpoint(&root, "")
            .expect_err("empty checkpoint selector must fail")
            .contains("cannot be empty")
    );
    assert_eq!(
        git_checkpoints::find_checkpoint(&root, &first.ref_name)
            .expect("exact ref lookup")
            .expect("exact ref found")
            .id,
        first.id
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_ref_creation_never_overwrites_an_existing_ref() {
    let _guard = env_lock();
    let root = temp_test_dir("checkpoint-ref-cas");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "first\n").expect("write first revision");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "first"]);
    let first_oid = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("read first OID")
            .stdout,
    )
    .expect("UTF-8 OID")
    .trim()
    .to_string();

    std::fs::write(root.join("tracked.txt"), "second\n").expect("write second revision");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "second"]);
    let second_oid = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", "HEAD"])
            .current_dir(&root)
            .output()
            .expect("read second OID")
            .stdout,
    )
    .expect("UTF-8 OID")
    .trim()
    .to_string();
    assert_ne!(first_oid, second_oid);

    let ref_name = "refs/dext/checkpoints/test/collision-id";
    git_checkpoints::create_checkpoint_ref(&root, ref_name, &first_oid)
        .expect("create initial checkpoint ref");
    let error = git_checkpoints::create_checkpoint_ref(&root, ref_name, &second_oid)
        .expect_err("existing checkpoint ref must not be overwritten");
    assert!(
        error.contains("cannot lock ref") || error.contains("reference already exists"),
        "{error}"
    );
    let stored_oid = String::from_utf8(
        Command::new("git")
            .args(["rev-parse", ref_name])
            .current_dir(&root)
            .output()
            .expect("read stored ref")
            .stdout,
    )
    .expect("UTF-8 stored OID")
    .trim()
    .to_string();
    assert_eq!(stored_oid, first_oid);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_missing_ref_fixture_fails_closed_without_project_mutation() {
    let root = temp_test_dir("checkpoint-missing-ref");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("initialize checkpoint storage")
        .expect("checkpoint exists");
    let manifest = root.join(".dext/checkpoints/manifest.txt");
    let manifest_before = std::fs::read(state_fixture_path("checkpoints", "missing-ref.manifest"))
        .expect("read missing-ref fixture");
    std::fs::write(&manifest, &manifest_before).expect("install missing-ref fixture");
    let tracked_before = std::fs::read(root.join("tracked.txt")).expect("read tracked");

    let error = git_checkpoints::list_checkpoints(&root, usize::MAX)
        .expect_err("missing checkpoint ref must fail inspection");
    assert!(
        error.contains("manifest references a missing ref"),
        "{error}"
    );
    assert_eq!(
        std::fs::read(&manifest).expect("reread manifest"),
        manifest_before
    );
    assert_eq!(
        std::fs::read(root.join("tracked.txt")).expect("reread tracked"),
        tracked_before
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_corrupt_manifest_fails_closed_without_leaking_new_ref() {
    use std::io::Write as _;

    let root = temp_test_dir("checkpoint-corrupt-manifest");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("create checkpoint")
        .expect("checkpoint exists");

    let manifest = root.join(".dext/checkpoints/manifest.txt");
    std::fs::OpenOptions::new()
        .append(true)
        .open(&manifest)
        .expect("open manifest")
        .write_all(b"corrupt manifest line\n")
        .expect("corrupt manifest");
    let error = git_checkpoints::list_checkpoints(&root, usize::MAX)
        .expect_err("corrupt manifest must not be partially accepted");
    assert!(
        error.contains("invalid checkpoint manifest entry"),
        "{error}"
    );
    assert!(
        git_checkpoints::create_checkpoint(&root, "bash", &[], 2)
            .expect_err("checkpoint append must reject corrupt manifest")
            .contains("invalid checkpoint manifest entry")
    );
    let refs = Command::new("git")
        .args([
            "for-each-ref",
            "--format=%(refname)",
            "refs/dext/checkpoints/",
        ])
        .current_dir(&root)
        .output()
        .expect("list refs");
    assert!(refs.status.success());
    assert_eq!(
        String::from_utf8_lossy(&refs.stdout).lines().count(),
        1,
        "failed append must roll its ref back"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn checkpoint_rejects_hardlinked_private_manifest() {
    let root = temp_test_dir("checkpoint-manifest-hardlink");
    let outside = temp_test_dir("checkpoint-manifest-hardlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("create checkpoint")
        .expect("checkpoint exists");

    let manifest = root.join(".dext/checkpoints/manifest.txt");
    let outside_manifest = outside.join("outside-manifest.txt");
    std::fs::copy(&manifest, &outside_manifest).expect("copy manifest outside");
    let outside_before = std::fs::read(&outside_manifest).expect("read outside manifest");
    std::fs::remove_file(&manifest).expect("remove private manifest");
    std::fs::hard_link(&outside_manifest, &manifest).expect("hardlink manifest");

    let error = git_checkpoints::list_checkpoints(&root, usize::MAX)
        .expect_err("hardlinked manifest must be rejected");
    assert!(error.contains("safe regular file"), "{error}");
    assert_eq!(
        std::fs::read(&outside_manifest).expect("read unchanged outside manifest"),
        outside_before
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_prune_unlinks_orphan_sidecar_symlink_without_following_it() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("checkpoint-prune-sidecar-symlink");
    let outside = temp_test_dir("checkpoint-prune-sidecar-symlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(outside.join("keep.txt"), "keep\n").expect("write outside file");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);
    git_checkpoints::prune(&root, None, None).expect("initialize checkpoint storage");

    let orphan = root.join(".dext/checkpoints/orphan_symlink");
    symlink(&outside, &orphan).expect("create orphan sidecar symlink");
    let result = git_checkpoints::prune(&root, None, None).expect("prune orphan symlink");
    assert!(result.contains("1 orphan sidecar entry"), "{result}");
    assert!(!orphan.exists(), "orphan symlink should be unlinked");
    assert_eq!(
        std::fs::read_to_string(outside.join("keep.txt")).expect("read outside file"),
        "keep\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn checkpoint_rejects_git_tracked_private_storage() {
    let root = temp_test_dir("checkpoint-tracked-storage");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::create_dir_all(root.join(".dext/checkpoints")).expect("create checkpoint storage");
    std::fs::write(
        root.join(".dext/checkpoints/tracked.txt"),
        "must not commit\n",
    )
    .expect("write tracked checkpoint file");
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked file");
    git_ok(
        &root,
        &["add", ".dext/checkpoints/tracked.txt", "tracked.txt"],
    );
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let error = git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect_err("Git-tracked checkpoint storage must be rejected");
    assert!(
        error.contains("checkpoint storage is tracked by Git"),
        "{error}"
    );
    assert!(!root.join(".dext/checkpoints/manifest.txt").exists());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_restore_accepts_tracked_directory_hint() {
    let root = temp_test_dir("checkpoint-directory-hint");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::create_dir(root.join("nested")).expect("create tracked directory");
    std::fs::write(root.join("nested/tracked.txt"), "base\n").expect("write tracked");
    git_ok(&root, &["add", "nested/tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    std::fs::write(root.join("nested/tracked.txt"), "checkpoint\n").expect("checkpoint state");
    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["nested".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    std::fs::write(root.join("nested/tracked.txt"), "later\n").expect("later state");

    git_checkpoints::restore_worktree(&root, &checkpoint, git_checkpoints::RestoreMode::Worktree)
        .expect("restore tracked directory hint");
    assert_eq!(
        std::fs::read_to_string(root.join("nested/tracked.txt")).expect("read restored file"),
        "checkpoint\n"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn checkpoint_restore_rejects_symlinked_parent_before_any_mutation() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("checkpoint-destination-parent-symlink");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::create_dir(root.join("nested")).expect("create tracked directory");
    std::fs::write(root.join("a-safe.txt"), "base safe\n").expect("write safe tracked");
    std::fs::write(root.join("nested/tracked.txt"), "base nested\n").expect("write nested tracked");
    git_ok(&root, &["add", "a-safe.txt", "nested/tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    std::fs::write(root.join("a-safe.txt"), "checkpoint safe\n").expect("checkpoint safe");
    std::fs::write(root.join("nested/tracked.txt"), "checkpoint nested\n")
        .expect("checkpoint nested");
    let checkpoint = git_checkpoints::create_checkpoint(
        &root,
        "write_file",
        &["a-safe.txt".to_string(), "nested/tracked.txt".to_string()],
        1,
    )
    .expect("create checkpoint")
    .expect("checkpoint exists");

    std::fs::write(root.join("a-safe.txt"), "later safe\n").expect("mutate safe");
    std::fs::write(root.join("nested/tracked.txt"), "later nested\n").expect("mutate nested");
    std::fs::rename(root.join("nested"), root.join("relocated"))
        .expect("relocate tracked directory");
    symlink("relocated", root.join("nested")).expect("alias tracked directory");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("symlinked restore parent must fail closed");
    assert!(error.contains("parent is not a real directory"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("a-safe.txt")).expect("read safe after rejection"),
        "later safe\n",
        "parent preflight must fail before restoring an earlier path"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("relocated/tracked.txt"))
            .expect("read aliased destination after rejection"),
        "later nested\n"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn checkpoint_reset_head_rejects_hardlinked_destination_before_any_mutation() {
    let root = temp_test_dir("checkpoint-reset-destination-hardlink");
    let outside = temp_test_dir("checkpoint-reset-destination-hardlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a-safe.txt"), "base safe\n").expect("write safe tracked");
    std::fs::write(root.join("z-linked.txt"), "base linked\n").expect("write linked tracked");
    git_ok(&root, &["add", "a-safe.txt", "z-linked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    std::fs::write(root.join("a-safe.txt"), "checkpoint safe\n").expect("checkpoint safe");
    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["a-safe.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    std::fs::write(root.join("a-safe.txt"), "later safe\n").expect("mutate safe");
    std::fs::write(root.join("z-linked.txt"), "later linked\n").expect("mutate linked");
    git_ok(&root, &["add", "a-safe.txt", "z-linked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "later"]);
    let later_head = git_stdout(&root, &["rev-parse", "HEAD"]);

    std::fs::remove_file(root.join("z-linked.txt")).expect("remove linked tracked");
    let victim = outside.join("victim.txt");
    std::fs::write(&victim, "external must survive\n").expect("write outside victim");
    std::fs::hard_link(&victim, root.join("z-linked.txt")).expect("hardlink reset destination");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::ResetHead,
    )
    .expect_err("reset-hard must reject multiply linked destinations");
    assert!(error.contains("multiply linked"), "{error}");
    assert_eq!(git_stdout(&root, &["rev-parse", "HEAD"]), later_head);
    assert_eq!(
        std::fs::read_to_string(root.join("a-safe.txt")).expect("read safe after rejection"),
        "later safe\n",
        "reset preflight must fail before changing HEAD or an earlier path"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read outside victim"),
        "external must survive\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_restore_rejects_hardlinked_destination_before_any_mutation() {
    let root = temp_test_dir("checkpoint-destination-hardlink");
    let outside = temp_test_dir("checkpoint-destination-hardlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(root.join("note.txt"), "checkpoint note\n").expect("write untracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint = git_checkpoints::create_checkpoint(
        &root,
        "write_file",
        &["tracked.txt".to_string(), "note.txt".to_string()],
        1,
    )
    .expect("create checkpoint")
    .expect("checkpoint exists");

    std::fs::write(root.join("tracked.txt"), "new tracked state\n").expect("modify tracked");
    std::fs::remove_file(root.join("note.txt")).expect("remove untracked target");
    let victim = outside.join("victim.txt");
    std::fs::write(&victim, "external must survive\n").expect("write outside victim");
    std::fs::hard_link(&victim, root.join("note.txt")).expect("hardlink restore destination");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("multiply linked restore destination must fail closed");
    assert!(
        error.contains("multiply linked") || error.contains("unsafe"),
        "{error}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).expect("read tracked after rejection"),
        "new tracked state\n",
        "preflight failure must occur before restoring any tracked path"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read outside victim"),
        "external must survive\n",
        "checkpoint restore must never truncate a multiply linked external inode"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_restore_rejects_hardlinked_tracked_destination_before_any_mutation() {
    let root = temp_test_dir("checkpoint-tracked-destination-hardlink");
    let outside = temp_test_dir("checkpoint-tracked-destination-hardlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a-safe.txt"), "base safe\n").expect("write safe tracked");
    std::fs::write(root.join("z-linked.txt"), "base linked\n").expect("write linked tracked");
    git_ok(&root, &["add", "a-safe.txt", "z-linked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint = git_checkpoints::create_checkpoint(
        &root,
        "write_file",
        &["a-safe.txt".to_string(), "z-linked.txt".to_string()],
        1,
    )
    .expect("create checkpoint")
    .expect("checkpoint exists");
    assert!(!checkpoint.includes_untracked_sidecar);

    std::fs::write(root.join("a-safe.txt"), "new safe state\n").expect("modify safe tracked");
    std::fs::remove_file(root.join("z-linked.txt")).expect("remove linked tracked");
    let victim = outside.join("victim.txt");
    std::fs::write(&victim, "external must survive\n").expect("write outside victim");
    std::fs::hard_link(&victim, root.join("z-linked.txt"))
        .expect("hardlink tracked restore destination");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("multiply linked tracked destination must fail closed");
    assert!(error.contains("multiply linked"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("a-safe.txt")).expect("read safe after rejection"),
        "new safe state\n",
        "tracked-only preflight failure must occur before restoring any earlier path"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read outside victim"),
        "external must survive\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_full_restore_rejects_hardlinked_destination_before_any_mutation() {
    let root = temp_test_dir("checkpoint-full-destination-hardlink");
    let outside = temp_test_dir("checkpoint-full-destination-hardlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("a-safe.txt"), "base safe\n").expect("write safe tracked");
    std::fs::write(root.join("z-linked.txt"), "base linked\n").expect("write linked tracked");
    git_ok(&root, &["add", "a-safe.txt", "z-linked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint = git_checkpoints::create_checkpoint(&root, "bash", &[], 1)
        .expect("create checkpoint")
        .expect("checkpoint exists");
    assert!(checkpoint.paths_hint.is_empty());

    std::fs::write(root.join("a-safe.txt"), "new safe state\n").expect("modify safe tracked");
    std::fs::remove_file(root.join("z-linked.txt")).expect("remove linked tracked");
    let victim = outside.join("victim.txt");
    std::fs::write(&victim, "external must survive\n").expect("write outside victim");
    std::fs::hard_link(&victim, root.join("z-linked.txt"))
        .expect("hardlink full restore destination");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("full restore must reject multiply linked destinations");
    assert!(error.contains("multiply linked"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("a-safe.txt")).expect("read safe after rejection"),
        "new safe state\n",
        "full-worktree preflight failure must occur before git restore mutates any path"
    );
    assert_eq!(
        std::fs::read_to_string(&victim).expect("read outside victim"),
        "external must survive\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[cfg(unix)]
#[test]
fn checkpoint_restore_rejects_hardlinked_sidecar_before_mutation() {
    let root = temp_test_dir("checkpoint-sidecar-hardlink");
    let outside = temp_test_dir("checkpoint-sidecar-hardlink-target");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(root.join("note.txt"), "before\n").expect("write untracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["note.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    let sidecar = root
        .join(".dext/checkpoints")
        .join(&checkpoint.id)
        .join("note.txt");
    let outside_link = outside.join("linked-sidecar.txt");
    std::fs::hard_link(&sidecar, &outside_link).expect("hardlink sidecar outside");
    std::fs::write(root.join("note.txt"), "after\n").expect("mutate note");

    let error = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::Worktree,
    )
    .expect_err("hardlinked sidecar must fail before restore");
    assert!(error.contains("unsafe sidecar entry"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("note.txt")).expect("read unchanged note"),
        "after\n"
    );
    assert_eq!(
        std::fs::read_to_string(&outside_link).expect("read outside hardlink"),
        "before\n"
    );

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&outside);
}

#[test]
fn checkpoint_automatic_retention_removes_expired_sidecars() {
    let root = temp_test_dir("checkpoint-retention-sidecars");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write tracked");
    std::fs::write(root.join("note.txt"), "untracked\n").expect("write untracked");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let mut first_id = String::new();
    for ordinal in 0..21 {
        let checkpoint = git_checkpoints::create_checkpoint(
            &root,
            "write_file",
            &["note.txt".to_string()],
            ordinal,
        )
        .expect("create checkpoint")
        .expect("checkpoint exists");
        if ordinal == 0 {
            first_id = checkpoint.id;
        }
    }

    assert!(
        !root.join(".dext/checkpoints").join(&first_id).exists(),
        "automatic retention must remove the expired sidecar directory"
    );
    let checkpoints =
        git_checkpoints::list_checkpoints(&root, usize::MAX).expect("list retained checkpoints");
    assert_eq!(checkpoints.len(), 20);
    assert!(
        checkpoints
            .iter()
            .all(|checkpoint| checkpoint.id != first_id)
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn checkpoint_reset_head_reports_commit_state_and_preserves_snapshot_ref() {
    let root = temp_test_dir("checkpoint-reset-head-message");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n").expect("write base");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    std::fs::write(root.join("tracked.txt"), "checkpoint-state\n").expect("write checkpoint state");
    let checkpoint =
        git_checkpoints::create_checkpoint(&root, "write_file", &["tracked.txt".to_string()], 1)
            .expect("create checkpoint")
            .expect("checkpoint exists");
    std::fs::write(root.join("tracked.txt"), "later-commit\n").expect("write later state");
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "later"]);

    let message = git_checkpoints::restore_worktree(
        &root,
        &checkpoint,
        git_checkpoints::RestoreMode::ResetHead,
    )
    .expect("reset HEAD");
    assert!(message.contains("now match that commit"), "{message}");
    assert!(
        message.contains("captured uncommitted snapshot"),
        "{message}"
    );
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).expect("read reset worktree"),
        "base\n"
    );

    git_checkpoints::restore_worktree(&root, &checkpoint, git_checkpoints::RestoreMode::Worktree)
        .expect("restore preserved uncommitted snapshot");
    assert_eq!(
        std::fs::read_to_string(root.join("tracked.txt")).expect("read snapshot worktree"),
        "checkpoint-state\n"
    );

    let _ = std::fs::remove_dir_all(&root);
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
fn prepared_mutation_replaces_atomically_preserves_mode_and_creates_parents() -> Result<()> {
    let root = temp_test_dir("prepared-mutation-success");
    let path = root.join("note.txt");
    std::fs::write(&path, "before\n")?;

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
    }
    #[cfg(unix)]
    let original_inode = {
        use std::os::unix::fs::MetadataExt;
        std::fs::metadata(&path)?.ino()
    };

    let prepared = mutation_ok(mutation_preview::prepare_write_file(
        &root, "note.txt", "after\n",
    ))?;
    assert_eq!(prepared.preview().diff, "-before\n+after\n");
    mutation_ok(mutation_preview::apply_prepared_mutation(&root, &prepared))?;
    assert_eq!(std::fs::read_to_string(&path)?, "after\n");

    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let metadata = std::fs::metadata(&path)?;
        assert_ne!(metadata.ino(), original_inode, "replacement must be atomic");
        assert_eq!(metadata.permissions().mode() & 0o777, 0o640);
    }

    let nested = mutation_ok(mutation_preview::prepare_write_file(
        &root,
        "new/deep/file.txt",
        "created through prepared state\n",
    ))?;
    mutation_ok(mutation_preview::apply_prepared_mutation(&root, &nested))?;
    assert_eq!(
        std::fs::read_to_string(root.join("new/deep/file.txt"))?,
        "created through prepared state\n"
    );

    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn prepared_mutation_rejects_stale_overwrite_and_edit() -> Result<()> {
    let root = temp_test_dir("prepared-mutation-stale-content");
    let path = root.join("note.txt");
    std::fs::write(&path, "before\n")?;

    let overwrite = mutation_ok(mutation_preview::prepare_write_file(
        &root,
        "note.txt",
        "planned\n",
    ))?;
    std::fs::write(&path, "raced overwrite\n")?;
    let error = mutation_preview::apply_prepared_mutation(&root, &overwrite)
        .expect_err("stale overwrite must fail");
    assert!(error.contains("stale file state"), "{error}");
    assert!(error.contains("Re-read the file and retry"), "{error}");
    assert_eq!(std::fs::read_to_string(&path)?, "raced overwrite\n");

    std::fs::write(&path, "alpha beta gamma\n")?;
    let edit = mutation_ok(mutation_preview::prepare_edit_file(
        &root, "note.txt", "beta", "BETA",
    ))?;
    std::fs::write(&path, "alpha raced gamma\n")?;
    let error =
        mutation_preview::apply_prepared_mutation(&root, &edit).expect_err("stale edit must fail");
    assert!(error.contains("stale file state"), "{error}");
    assert_eq!(std::fs::read_to_string(&path)?, "alpha raced gamma\n");

    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn prepared_mutation_rejects_path_appearance_disappearance_and_type_swap() -> Result<()> {
    let root = temp_test_dir("prepared-mutation-stale-path");

    let appeared = mutation_ok(mutation_preview::prepare_write_file(
        &root,
        "appeared.txt",
        "planned\n",
    ))?;
    std::fs::write(root.join("appeared.txt"), "intruder\n")?;
    let error = mutation_preview::apply_prepared_mutation(&root, &appeared)
        .expect_err("appearing destination must fail");
    assert!(error.contains("stale file state"), "{error}");
    assert_eq!(
        std::fs::read_to_string(root.join("appeared.txt"))?,
        "intruder\n"
    );

    let vanished_path = root.join("vanished.txt");
    std::fs::write(&vanished_path, "before\n")?;
    let vanished = mutation_ok(mutation_preview::prepare_write_file(
        &root,
        "vanished.txt",
        "planned\n",
    ))?;
    std::fs::remove_file(&vanished_path)?;
    let error = mutation_preview::apply_prepared_mutation(&root, &vanished)
        .expect_err("disappearing destination must fail");
    assert!(error.contains("stale file state"), "{error}");
    assert!(!vanished_path.exists());

    let swapped_path = root.join("swapped.txt");
    std::fs::write(&swapped_path, "before\n")?;
    let swapped = mutation_ok(mutation_preview::prepare_write_file(
        &root,
        "swapped.txt",
        "planned\n",
    ))?;
    std::fs::remove_file(&swapped_path)?;
    std::fs::create_dir(&swapped_path)?;
    let error = mutation_preview::apply_prepared_mutation(&root, &swapped)
        .expect_err("type-swapped destination must fail");
    assert!(error.contains("stale file state"), "{error}");
    assert!(swapped_path.is_dir());

    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[cfg(unix)]
#[test]
fn prepared_mutation_rejects_symlink_swap_without_touching_target() -> Result<()> {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("prepared-mutation-symlink-swap");
    let outside = temp_test_dir("prepared-mutation-symlink-target");
    let path = root.join("note.txt");
    let outside_path = outside.join("outside.txt");
    std::fs::write(&path, "before\n")?;
    std::fs::write(&outside_path, "outside\n")?;

    let prepared = mutation_ok(mutation_preview::prepare_write_file(
        &root,
        "note.txt",
        "planned\n",
    ))?;
    std::fs::remove_file(&path)?;
    symlink(&outside_path, &path)?;
    let error = mutation_preview::apply_prepared_mutation(&root, &prepared)
        .expect_err("symlink-swapped destination must fail");
    assert!(error.contains("stale file state"), "{error}");
    assert!(error.contains("Re-read the file and retry"), "{error}");
    assert_eq!(std::fs::read_to_string(&outside_path)?, "outside\n");
    assert!(std::fs::symlink_metadata(&path)?.file_type().is_symlink());

    std::fs::remove_dir_all(root)?;
    std::fs::remove_dir_all(outside)?;
    Ok(())
}

#[test]
fn prepared_mutation_write_failure_cleans_temp_and_preserves_original() -> Result<()> {
    let root = temp_test_dir("prepared-mutation-write-failure");
    let path = root.join("note.txt");
    std::fs::write(&path, "before\n")?;
    let prepared = mutation_ok(mutation_preview::prepare_write_file(
        &root,
        "note.txt",
        "planned\n",
    ))?;

    let error = mutation_preview::fail_prepared_mutation_before_replace(&root, &prepared)
        .expect_err("injected write failure must fail");
    assert!(
        error.contains("injected pre-replacement failure"),
        "{error}"
    );
    assert_eq!(std::fs::read_to_string(&path)?, "before\n");
    let leaked_temp = std::fs::read_dir(&root)?
        .flatten()
        .any(|entry| entry.file_name().to_string_lossy().contains(".dext-tmp-"));
    assert!(!leaked_temp, "failed mutation leaked a temp file");

    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn multi_edit_preparation_is_all_or_nothing() -> Result<()> {
    let root = temp_test_dir("prepared-multi-edit-all-or-nothing");
    let path = root.join("note.txt");
    std::fs::write(&path, "alpha beta gamma\n")?;
    let edits = [
        mutation_preview::MultiEdit {
            old_string: "alpha".to_string(),
            new_string: "ALPHA".to_string(),
            replace_all: false,
        },
        mutation_preview::MultiEdit {
            old_string: "missing".to_string(),
            new_string: "MISSING".to_string(),
            replace_all: false,
        },
    ];

    let error = mutation_preview::prepare_multi_edit(&root, "note.txt", &edits)
        .expect_err("invalid later edit must reject the full mutation");
    assert!(error.contains("edit[1]: old_string not found"), "{error}");
    assert_eq!(std::fs::read_to_string(&path)?, "alpha beta gamma\n");

    std::fs::remove_dir_all(root)?;
    Ok(())
}

#[test]
fn tool_paths_allow_only_user_shelf_pack_content_outside_project() {
    let _guard = env_lock();
    let root = temp_test_dir("tool-path-global-pack-root");
    let home = temp_test_dir("tool-path-global-pack-home");
    let shelf_dir = home.join("shelves/community");
    let shelf_pack_dir = shelf_dir.join("packs/demo");
    std::fs::create_dir_all(&shelf_pack_dir).expect("create shelf pack dir");
    let shelf_pack_md = shelf_pack_dir.join("PACK.md");
    let shelf_manifest = shelf_dir.join("shelf.json");
    std::fs::write(&shelf_pack_md, "---\nname: shelf-demo\n---\n# Demo\n")
        .expect("write shelf PACK.md");
    std::fs::write(&shelf_manifest, "{}\n").expect("write shelf manifest");
    let shelf_notes = shelf_pack_dir.join("notes.md");
    let shelf_metadata = shelf_dir.join("metadata.json");
    let loose_packs_file = shelf_dir.join("packs/loose.md");
    let fake_pack_file = shelf_dir.join("packs/not-a-pack/notes.md");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
    }

    let canonical = canonicalize_mutation_path(&root, &shelf_pack_md.display().to_string())
        .expect("allow user shelf pack path");
    assert_eq!(
        canonical,
        std::fs::canonicalize(&shelf_pack_md).expect("canonical shelf pack path")
    );

    let preview =
        mutation_preview::preview_write_file(&root, &shelf_notes.display().to_string(), "hi\n")
            .expect("preview user shelf pack write");
    assert_eq!(preview.path, shelf_notes);
    assert!(preview.is_new_file);

    for denied in [
        &shelf_manifest,
        &shelf_metadata,
        &loose_packs_file,
        &fake_pack_file,
    ] {
        let error = canonicalize_mutation_path(&root, &denied.display().to_string())
            .expect_err("shelf metadata is outside pack content");
        assert!(
            error.contains("outside sandbox or Dext global pack roots"),
            "{error}"
        );
    }

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
}

#[test]
fn prepared_user_pack_mutation_rechecks_pack_marker_before_apply() {
    let _guard = env_lock();
    let root = temp_test_dir("prepared-pack-marker-root");
    let home = temp_test_dir("prepared-pack-marker-home");
    let pack_dir = home.join("shelves/community/packs/demo");
    std::fs::create_dir_all(&pack_dir).expect("create pack directory");
    let marker = pack_dir.join("PACK.md");
    std::fs::write(&marker, "---\nname: demo\n---\n# Demo\n").expect("write pack marker");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
    }

    let destination = pack_dir.join("new/deep/notes.md");
    let prepared =
        mutation_preview::prepare_write_file(&root, &destination.display().to_string(), "notes\n")
            .expect("prepare authenticated pack mutation");
    std::fs::remove_file(&marker).expect("remove pack marker before apply");
    let error = mutation_preview::apply_prepared_mutation(&root, &prepared)
        .expect_err("missing marker must invalidate prepared pack mutation");
    assert!(error.contains("stale file state"), "{error}");
    assert!(
        error.contains("outside sandbox or Dext global pack roots"),
        "{error}"
    );
    assert!(!destination.exists());
    assert!(!pack_dir.join("new").exists());

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
}

#[cfg(unix)]
#[test]
fn tool_paths_reject_symlinked_user_pack_marker() {
    use std::os::unix::fs::symlink;

    let _guard = env_lock();
    let root = temp_test_dir("tool-path-symlinked-pack-marker-root");
    let home = temp_test_dir("tool-path-symlinked-pack-marker-home");
    let outside = temp_test_dir("tool-path-symlinked-pack-marker-outside");
    let pack_dir = home.join("shelves/community/packs/demo");
    std::fs::create_dir_all(&pack_dir).expect("create shelf pack dir");
    let outside_marker = outside.join("PACK.md");
    std::fs::write(&outside_marker, "---\nname: outside\n---\n# Outside\n")
        .expect("write outside marker");
    symlink(&outside_marker, pack_dir.join("PACK.md")).expect("symlink pack marker");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
    }

    let notes = pack_dir.join("notes.md");
    let error = canonicalize_mutation_path(&root, &notes.display().to_string())
        .expect_err("symlinked PACK.md must not authorize external mutation");
    assert!(
        error.contains("outside sandbox or Dext global pack roots"),
        "{error}"
    );

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&outside);
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

    let err = canonicalize_mutation_path(&root, &outside_file.display().to_string())
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

#[cfg(unix)]
#[test]
fn tool_paths_reject_dangling_symlink_write_escapes() {
    use std::os::unix::fs::symlink;

    let _guard = env_lock();
    let root = temp_test_dir("tool-path-dangling-root");
    let home = temp_test_dir("tool-path-dangling-home");
    let outside = temp_test_dir("tool-path-dangling-outside");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
    }

    let outside_file = outside.join("created-by-escape.txt");
    symlink(&outside_file, root.join("file-alias")).expect("create dangling file alias");
    let error = canonicalize_mutation_path(&root, "file-alias")
        .expect_err("dangling file symlink must fail closed");
    assert!(
        error.contains("cannot resolve existing path component"),
        "{error}"
    );
    let error = execute_tool(
        "write_file",
        &json!({"path": "file-alias", "content": "escaped\n"}),
        &root,
    )
    .expect_err("write_file must reject dangling file alias");
    assert!(
        error.contains("cannot resolve existing path component"),
        "{error}"
    );
    assert!(!outside_file.exists(), "outside target must not be created");

    let outside_dir = outside.join("created-directory");
    symlink(&outside_dir, root.join("dir-alias")).expect("create dangling directory alias");
    let error = canonicalize_mutation_path(&root, "dir-alias/escaped.txt")
        .expect_err("dangling directory symlink must fail closed");
    assert!(
        error.contains("cannot resolve existing path component"),
        "{error}"
    );
    let error = execute_tool(
        "write_file",
        &json!({"path": "dir-alias/escaped.txt", "content": "escaped\n"}),
        &root,
    )
    .expect_err("write_file must reject dangling directory alias");
    assert!(
        error.contains("cannot resolve existing path component"),
        "{error}"
    );
    assert!(
        !outside_dir.exists(),
        "outside directory must not be created through a dangling alias"
    );

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&home);
    let _ = std::fs::remove_dir_all(&outside);
}

struct SessionReplayFixture {
    header: SessionHeader,
    history: Vec<Message>,
}

fn state_fixture_path(area: &str, name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/state")
        .join(area)
        .join(name)
}

fn load_session_state_fixture(name: &str) -> Result<(SessionHeader, Vec<Message>)> {
    let path = state_fixture_path("sessions", name);
    let content = std::fs::read_to_string(&path)
        .with_context(|| format!("reading state fixture {}", path.display()))?;
    let mut lines = content.lines();
    let header = parse_session_header(lines.next().context("empty session state fixture")?)?;
    let history = lines
        .enumerate()
        .filter(|(_, line)| !line.trim().is_empty())
        .map(|(index, line)| {
            serde_json::from_str::<Message>(line)
                .with_context(|| format!("bad fixture message on line {}", index + 2))
        })
        .collect::<Result<Vec<_>>>()?;
    Ok((header, history))
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
        Block::Text { text } | Block::PartialStream { text } | Block::Thinking { text, .. } => {
            text.contains(marker)
        }
        Block::RedactedThinking { data } => data.contains(marker),
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
    // Synthesized objective/checkpoint fields (objective/done/pending/
    // next_actions) are intentionally NOT rendered into the runtime status
    // block; the surfaced [queued-user-update] history message carries the
    // call to action instead.
    assert!(!ledger.contains("objective:"), "{ledger}");
    assert!(!ledger.contains("pending:"), "{ledger}");
    assert!(!ledger.contains("next_actions:"), "{ledger}");
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
fn queued_path_steering_is_preserved_as_literal_task_input() {
    let root = temp_test_dir("steering-path-literal");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
    agent.install_steering(rx, tx.clone());
    let supplied_path = "/home/fixture-user/.dext/projects/stocks-test/sessions";
    tx.send(supplied_path.to_string())
        .expect("send path steering");
    let mut turn_state = orchestrator::TurnRuntimeState::new();

    assert!(agent.inject_queued_steering(&mut turn_state, 3, 7, false));
    let injected = agent
        .history
        .iter()
        .flat_map(|message| message.content.iter())
        .find_map(|block| match block {
            Block::Text { text } if text.contains("[queued-user-update]") => Some(text),
            _ => None,
        })
        .expect("queued path update injected");
    assert!(injected.contains(supplied_path), "{injected}");
    assert!(
        injected.contains("literal user-authored task input"),
        "{injected}"
    );
    assert!(
        injected.contains("never dismiss or reinterpret"),
        "{injected}"
    );
    assert!(injected.contains("inspect that path first"), "{injected}");
    assert!(injected.contains("`read_file` for a file"), "{injected}");
    assert!(injected.contains("`fd`/`rg` for a directory"), "{injected}");
    assert!(
        injected.contains("not guessed alternatives, bash discovery, or sudo"),
        "{injected}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn busy_queued_slash_commands_are_not_injected_as_steering() {
    let root = temp_test_dir("busy-slash-not-steering");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let _guard = env_lock();
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };
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

    restore_env_var("DEXT_HOME", old_dext_home);
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
fn sudo_askpass_script_reads_fifo_and_never_echoes_chat_guidance() {
    let script = sudo_askpass_script_content_with_paths(
        "/tmp/zenity'bad",
        "/tmp/kdialog",
        "/usr/bin/osascript",
    );
    // Password flows only through the Dext-provided fifo, never a raw /dev/tty
    // echo path inside the child, so it can't leak into the TUI scrollback.
    assert!(script.contains("DEXT_SUDO_PASSWORD_FIFO"), "{script}");
    assert!(script.contains("osascript"), "{script}");
    assert!(
        script.contains("Dext local sudo prompt requires Dext local auth"),
        "{script}"
    );
    assert!(!script.contains("/dev/tty"), "{script}");
    assert!(!script.contains("chat/steering"), "{script}");
    assert!(script.contains("'\\''"), "{script}");
}

#[cfg(unix)]
#[test]
fn sudo_password_fifo_is_0600_and_unlinkable() {
    let root = temp_test_dir("sudo-fifo-perms");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    // session_sudo_dir resolves through DEXT_HOME; hold the env lock so a
    // concurrent test mutating it can't redirect the fifo into a temp root
    // that gets deleted mid-test.
    let _guard = env_lock();
    let path = create_sudo_password_fifo(&root, "test-session").expect("create fifo");
    use std::os::unix::fs::PermissionsExt;
    let mode = std::fs::metadata(&path)
        .expect("stat fifo")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "fifo mode {mode:o}");
    assert!(path.exists());
    std::fs::remove_file(&path).expect("unlink fifo");
    assert!(!path.exists());
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_credential_input_parses_token_and_username_forms() {
    let cred = parse_git_credential_input("ghp_abcdef1234567890\n");
    assert_eq!(cred.username, "x-access-token");
    assert_eq!(cred.secret, "ghp_abcdef1234567890");

    let cred = parse_git_credential_input("oauth2:glpat-abc123");
    assert_eq!(cred.username, "oauth2");
    assert_eq!(cred.secret, "glpat-abc123");

    // URL-shaped and colon-bearing passwords must not be mis-split.
    let cred = parse_git_credential_input("https://example.com/secret");
    assert_eq!(cred.username, "x-access-token");
    assert_eq!(cred.secret, "https://example.com/secret");
}

#[test]
fn git_credential_failure_detection_matches_prompt_and_auth_errors() {
    assert!(output_indicates_git_credential_failure(
        "fatal: could not read Username for 'https://github.com': terminal prompts disabled"
    ));
    assert!(output_indicates_git_credential_failure(
        "fatal: could not read Password for 'https://x@github.com': No such device or address"
    ));
    assert!(output_indicates_git_credential_failure(
        "remote: Invalid username or token.\nfatal: Authentication failed for 'https://github.com/x/y.git/'"
    ));
    assert!(!output_indicates_git_credential_failure(
        "exit: 0\n--- stdout ---\nEverything up-to-date"
    ));
    assert!(!output_indicates_git_credential_failure(
        "npm ERR! terminal prompts disabled while installing dependency"
    ));
    assert!(!output_indicates_git_credential_failure(
        "fatal: could not read Username for 'http://insecure.example': terminal prompts disabled"
    ));
}

#[test]
fn git_credential_scope_limits_install_and_rejection_to_matching_hosts() {
    let hosts = git_credential_hosts_for_failure(
        &[],
        "fatal: Authentication failed for 'https://user@GitHub.COM/owner/repo.git/'",
    );
    assert_eq!(hosts, vec!["github.com"]);

    let cred = LocalGitCredential {
        username: "x-access-token".to_string(),
        secret: "test-token".to_string(),
        hosts: vec!["github.com".to_string()],
    };
    assert!(bash_command_should_install_git_credential(
        "set -euo pipefail\ngit -C repo push https://github.com/owner/repo.git",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "git push https://gitlab.com/owner/repo.git",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "printf '%s\\n' hi; git push https://github.com/owner/repo.git",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "git push http://github.com/owner/repo.git",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "cd repo && git push https://github.com/owner/repo.git",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "git push https://github.com/owner/repo.git > /tmp/out",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "git push https://github.com/owner/repo.git | cat",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "GIT_CONFIG_COUNT=1 git push https://github.com/owner/repo.git",
        &cred
    ));
    assert!(!bash_command_should_install_git_credential(
        "git -c credential.helper='!cat /tmp/leak' push https://github.com/owner/repo.git",
        &cred
    ));
    assert!(
        stored_git_credential_for_bash_call(
            "bash",
            &json!({"command": "command git ls-remote https://github.com/owner/repo.git"}),
            Some(&cred),
        )
        .is_some()
    );
    assert!(
        stored_git_credential_for_bash_call(
            "read_file",
            &json!({"path": "src/main.rs"}),
            Some(&cred),
        )
        .is_none()
    );
}

#[cfg(unix)]
#[test]
fn git_credential_helper_script_answers_get_from_fifo_only() {
    let script = git_credential_helper_script_content();
    assert!(script.contains("DEXT_GIT_CRED_FIFO"), "{script}");
    assert!(script.contains("DEXT_GIT_CRED_HOSTS"), "{script}");
    assert!(script.contains("[ \"$protocol\" = https ]"), "{script}");
    assert!(!script.contains("/dev/tty"), "{script}");
    // Only `get` is answered; store/erase silently succeed without state.
    assert!(
        script.contains("if [ \"$op\" != get ]; then exit 0; fi"),
        "{script}"
    );
}

#[cfg(unix)]
#[test]
fn git_credential_helper_feeds_git_credential_fill() {
    let root = temp_test_dir("git-cred-helper");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    // Isolate the session state dir (where the helper script and FIFO live)
    // from the shared ./.dext tree so concurrent tests' state cleanup can't
    // unlink the FIFO mid-test.
    let _guard = env_lock();
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };
    let cred = LocalGitCredential {
        username: "x-access-token".to_string(),
        secret: "test-token-not-a-real-secret".to_string(),
        hosts: vec!["example.invalid".to_string()],
    };
    let runtime = prepare_git_credential_helper(&root, "test-session", cred);
    restore_env_var("DEXT_HOME", old_dext_home);
    let runtime = runtime.expect("prepare helper");

    assert!(
        runtime
            .env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_KEY_0" && v == "credential.helper")
    );
    assert!(
        runtime
            .env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_VALUE_0" && v.is_empty())
    );
    assert!(
        runtime
            .env
            .iter()
            .any(|(k, v)| k == "GIT_CONFIG_VALUE_1" && v.starts_with('!'))
    );

    let mut cmd = std::process::Command::new("git");
    cmd.arg("credential").arg("fill").current_dir(&root);
    cmd.env("GIT_TERMINAL_PROMPT", "0");
    // Isolate from the developer's real credential helpers.
    cmd.env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", "/dev/null");
    for (k, v) in &runtime.env {
        cmd.env(k, v);
    }
    cmd.stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().expect("spawn git credential fill");
    {
        use std::io::Write as _;
        child
            .stdin
            .take()
            .expect("stdin")
            .write_all(b"protocol=https\nhost=example.invalid\n\n")
            .expect("write request");
    }
    // Hard deadline so a broken helper can never wedge the whole suite.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    let status = loop {
        match child.try_wait().expect("poll git credential fill") {
            Some(status) => break status,
            None if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                panic!("git credential fill did not finish within 30s");
            }
            None => std::thread::sleep(std::time::Duration::from_millis(25)),
        }
    };
    let mut stdout = String::new();
    let mut stderr = String::new();
    {
        use std::io::Read as _;
        if let Some(mut so) = child.stdout.take() {
            let _ = so.read_to_string(&mut stdout);
        }
        if let Some(mut se) = child.stderr.take() {
            let _ = se.read_to_string(&mut stderr);
        }
    }
    assert!(status.success(), "status {status:?}: {stderr}");
    assert!(stdout.contains("username=x-access-token"), "{stdout}");
    assert!(
        stdout.contains("password=test-token-not-a-real-secret"),
        "{stdout}"
    );
    drop(runtime);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn session_transcript_files_are_written_0600() {
    use std::os::unix::fs::PermissionsExt;
    let root = temp_test_dir("session-transcript-perms");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let _guard = env_lock();
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };
    let agent = test_agent(&root);
    let path = root.join("exported-session.jsonl");
    let saved = agent.save_session_to_path(&path);
    restore_env_var("DEXT_HOME", old_dext_home);
    saved.expect("save session");
    let mode = std::fs::metadata(&path)
        .expect("stat session file")
        .permissions()
        .mode()
        & 0o777;
    assert_eq!(mode, 0o600, "session transcript mode {mode:o}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn clear_secret_string_zeroes_then_empties() {
    let mut secret = "hunter2-hunter2".to_string();
    clear_secret_string(&mut secret);
    assert!(secret.is_empty());
    assert!(secret.capacity() == 0 || secret.as_bytes().iter().all(|&b| b == 0));
}

#[cfg(unix)]
#[test]
fn sudo_wrapper_prefix_uses_noninteractive_and_no_askpass_env() {
    let auth = LocalSudoAuth {
        askpass: Some(PathBuf::from("/nonexistent/askpass.sh")),
        sudo_path: PathBuf::from("/usr/bin/sudo"),
        sudo_shim_dir: PathBuf::from("/tmp/dext-sudo/bin"),
        password_fifo: None,
        password: None,
        preauth_required: false,
    };
    let prefix = sudo_wrapper_prefix(&auth);
    assert!(prefix.contains("-n \"$@\""), "{prefix}");
    assert!(prefix.contains("sudo()"), "{prefix}");
    assert!(prefix.contains("function /usr/bin/sudo"), "{prefix}");
    // After pre-auth, the wrapper must NOT export askpass env vars into the
    // bash child (pre-auth already established the sudo timestamp; leaking
    // askpass would risk re-prompting via the script's fallback paths).
    assert!(!prefix.contains("SUDO_ASKPASS"), "{prefix}");
    assert!(!prefix.contains("DEXT_SUDO_ASKPASS"), "{prefix}");
    assert!(!prefix.contains("DEXT_SUDO_PASSWORD_FIFO"), "{prefix}");
    assert!(sudo_shell_function_name_is_safe("/usr/bin/sudo"));
    assert!(!sudo_shell_function_name_is_safe("/tmp/sudo;evil"));
}

#[cfg(unix)]
#[test]
fn sudo_command_shim_invokes_real_sudo_noninteractive_and_is_private() {
    let root = temp_test_dir("sudo-command-shim");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let real_sudo = root.join("real-sudo");
    std::fs::write(&real_sudo, "#!/bin/sh\nprintf '%s\\n' \"$@\"\n").expect("write fake sudo");
    use std::os::unix::fs::PermissionsExt;
    let mut real_perms = std::fs::metadata(&real_sudo)
        .expect("stat fake sudo")
        .permissions();
    real_perms.set_mode(0o700);
    std::fs::set_permissions(&real_sudo, real_perms).expect("chmod fake sudo");

    // session_sudo_dir resolves through DEXT_HOME. Isolate the generated shim
    // from shared user-global session state while serializing the env override.
    let _guard = env_lock();
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };
    let bin_dir = write_sudo_command_shim(&root, "test-session", &real_sudo).expect("write shim");
    restore_env_var("DEXT_HOME", old_dext_home);
    let shim = bin_dir.join("sudo");
    let output = Command::new(&shim).arg("true").output().expect("run shim");
    assert!(
        output.status.success(),
        "status {}: {}",
        output.status,
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(String::from_utf8_lossy(&output.stdout), "-n\ntrue\n");
    assert_eq!(
        std::fs::metadata(&bin_dir)
            .expect("stat shim dir")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&shim)
            .expect("stat shim")
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    let content = std::fs::read_to_string(&shim).expect("read shim");
    assert!(content.contains(" -n \"$@\""), "{content}");
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test]
async fn sudo_preauth_runs_inside_bash_session_before_noninteractive_sudo() {
    let root = temp_test_dir("sudo-preauth-noninteractive");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let fake_sudo = root.join("fake-sudo");
    let state_dir = root.join("sudo-state");
    std::fs::create_dir_all(&state_dir).expect("create sudo state dir");
    let state_dir = shell_single_quote(&state_dir.to_string_lossy());
    std::fs::write(
        &fake_sudo,
        format!(
            "#!/bin/sh\nset -eu\nstate_dir={state_dir}\nstate=\"$state_dir/authenticated\"\nif [ \"${{1:-}}\" = -n ] && [ \"${{2:-}}\" = -v ]; then\n  test -f \"$state\"\n  exit $?\nfi\nif [ \"${{1:-}}\" = -A ] && [ \"${{2:-}}\" = -v ]; then\n  ask=\"${{SUDO_ASKPASS:-}}\"\n  if [ -z \"$ask\" ]; then ask=\"${{DEXT_SUDO_ASKPASS:-}}\"; fi\n  [ -n \"$ask\" ] || exit 1\n  pass=$(\"$ask\" 'fake sudo prompt') || exit 1\n  [ \"$pass\" = hunter2 ] || exit 1\n  : > \"$state\"\n  exit 0\nfi\nwhile [ \"${{1:-}}\" = -n ]; do shift; done\nif [ -f \"$state\" ]; then exec \"$@\"; fi\nprintf '%s\\n' 'sudo: interactive authentication is required' >&2\nexit 1\n"
        ),
    )
    .expect("write fake sudo");
    use std::os::unix::fs::PermissionsExt;
    let mut fake_perms = std::fs::metadata(&fake_sudo)
        .expect("stat fake sudo")
        .permissions();
    fake_perms.set_mode(0o700);
    std::fs::set_permissions(&fake_sudo, fake_perms).expect("chmod fake sudo");

    let target_dir = temp_test_dir("sudo-preauth-target");
    let target = target_dir.join("out.txt");

    let (sudo_shim_dir, askpass, fifo) = {
        let _guard = env_lock();
        let old_sessions_dir = std::env::var_os("DEXT_SESSIONS_DIR");
        unsafe {
            std::env::set_var("DEXT_SESSIONS_DIR", root.join("sessions"));
        }
        let result = (
            write_sudo_command_shim(&root, "test-session", &fake_sudo).expect("write sudo shim"),
            write_sudo_askpass_script(&root, "test-session").expect("write askpass"),
            create_sudo_password_fifo(&root, "test-session").expect("create sudo fifo"),
        );
        restore_env_var("DEXT_SESSIONS_DIR", old_sessions_dir);
        result
    };
    let auth = LocalSudoAuth {
        askpass: Some(askpass),
        sudo_path: fake_sudo,
        sudo_shim_dir,
        password_fifo: Some(fifo),
        password: Some("hunter2".to_string()),
        preauth_required: true,
    };
    let command = format!(
        "printf ok | sudo -n tee {} >/dev/null",
        shell_single_quote(&target.to_string_lossy())
    );
    let out = execute_bash_async_prepared(
        &command,
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(10),
        Some(auth),
        SandboxProfile::ReadOnly,
        None,
        &[],
        &[],
    )
    .await
    .expect("preauthed sudo command runs");

    assert!(out.contains("exit: 0"), "{out}");
    assert_eq!(std::fs::read_to_string(&target).expect("read target"), "ok");
    let _ = std::fs::remove_file(&target);
    if let Some(dir) = target.parent() {
        let _ = std::fs::remove_dir(dir);
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn tool_credential_env_key_matches_only_secret_variables() {
    assert!(tool_credential_env_key("DEXT_API_KEY"));
    assert!(tool_credential_env_key("acme_api_key"));
    assert!(tool_credential_env_key("CHATGPT_ACCESS_TOKEN"));
    assert!(tool_credential_env_key("AWS_SECRET_ACCESS_KEY"));
    assert!(tool_credential_env_key("SECRET_KEY"));
    assert!(tool_credential_env_key("TOKEN"));
    assert!(tool_credential_env_key("X_CT0"));
    assert!(tool_credential_env_key("X_CONSUMER_KEY"));
    assert!(!tool_credential_env_key("DEXT_BASE_URL"));
    assert!(!tool_credential_env_key("DEXT_PACK_DEMO_DIR"));
}

#[test]
fn sync_tool_children_scrub_credentials_unless_explicitly_enabled() {
    let _guard = env_lock();
    let old_api_key = std::env::var_os("SECURITY_TEST_API_KEY");
    let old_access_token = std::env::var_os("CHATGPT_ACCESS_TOKEN");
    let old_safe = std::env::var_os("SECURITY_TEST_SAFE");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    unsafe {
        std::env::set_var("SECURITY_TEST_API_KEY", "fake-api-key-fixture");
        std::env::set_var("CHATGPT_ACCESS_TOKEN", "fake-access-token-fixture");
        std::env::set_var("SECURITY_TEST_SAFE", "visible");
        std::env::remove_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    }

    let mut scrubbed = Command::new(bash_executable_path());
    scrubbed.arg("-c").arg(
        "printf '%s|%s|%s' \"${SECURITY_TEST_API_KEY-unset}\" \"${CHATGPT_ACCESS_TOKEN-unset}\" \"${SECURITY_TEST_SAFE-unset}\"",
    );
    let (stdout, _, code) = run_sync_command_limited(
        scrubbed,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "credential scrub test",
        std::time::Duration::from_secs(5),
    )
    .expect("run scrubbed child");
    assert_eq!(code, 0);
    assert_eq!(stdout.render("stdout"), "unset|unset|visible");

    unsafe {
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
    }
    let mut inherited = Command::new(bash_executable_path());
    inherited
        .arg("-c")
        .arg("printf '%s|%s' \"${SECURITY_TEST_API_KEY-unset}\" \"${CHATGPT_ACCESS_TOKEN-unset}\"");
    let (stdout, _, code) = run_sync_command_limited(
        inherited,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "credential inheritance test",
        std::time::Duration::from_secs(5),
    )
    .expect("run inherited child");
    assert_eq!(code, 0);
    assert_eq!(
        stdout.render("stdout"),
        "fake-api-key-fixture|fake-access-token-fixture"
    );

    restore_env_var("SECURITY_TEST_API_KEY", old_api_key);
    restore_env_var("CHATGPT_ACCESS_TOKEN", old_access_token);
    restore_env_var("SECURITY_TEST_SAFE", old_safe);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn async_tool_children_scrub_credentials_by_default() {
    let _guard = env_lock();
    let old_api_key = std::env::var_os("SECURITY_ASYNC_API_KEY");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    unsafe {
        std::env::set_var("SECURITY_ASYNC_API_KEY", "fake-async-api-key-fixture");
        std::env::remove_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    }
    let mut command = tokio::process::Command::new(bash_executable_path());
    command
        .arg("-c")
        .arg("printf '%s' \"${SECURITY_ASYNC_API_KEY-unset}\"");
    scrub_tool_credentials_from_tokio_command(&mut command);
    let output = command.output().await.expect("run async scrubbed child");
    assert!(output.status.success());
    assert_eq!(String::from_utf8_lossy(&output.stdout), "unset");

    restore_env_var("SECURITY_ASYNC_API_KEY", old_api_key);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn async_tool_children_scrub_injected_only_credentials_and_prompt_overrides() {
    let _guard = env_lock();
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    unsafe {
        std::env::remove_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    }
    let root = temp_test_dir("credential-injected-extra-env");
    let root = std::fs::canonicalize(&root).expect("canonical root");
    let output = execute_bash_async_prepared(
        "printf '%s|%s' \"${INJECTED_ONLY_SECRET_KEY-unset}\" \"${GIT_TERMINAL_PROMPT-unset}\"",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        None,
        SandboxProfile::DangerFullAccess,
        None,
        &[
            (
                "INJECTED_ONLY_SECRET_KEY".to_string(),
                "fake-injected-secret".to_string(),
            ),
            ("GIT_TERMINAL_PROMPT".to_string(), "1".to_string()),
        ],
        &[],
    )
    .await
    .expect("run bash with injected environment");
    assert!(output.contains("unset|0"), "{output}");

    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn declared_pack_credentials_reach_direct_helper_but_not_arbitrary_bash() {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let old_auth = std::env::var_os("X_AUTH_TOKEN");
    let old_provider = std::env::var_os("OPENAI_API_KEY");
    let old_unrelated = std::env::var_os("UNRELATED_SECRET_KEY");
    let old_bash_env = std::env::var_os("BASH_ENV");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    unsafe {
        std::env::set_var("X_AUTH_TOKEN", "fake-pack-auth-token");
        std::env::set_var("OPENAI_API_KEY", "fake-provider-key");
        std::env::set_var("UNRELATED_SECRET_KEY", "fake-unrelated-key");
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
    }
    let root = temp_test_dir("pack credential env with spaces");
    let root = std::fs::canonicalize(&root).expect("canonical root");
    let startup = root.join("bash-env.sh");
    std::fs::write(
        &startup,
        "export PACK_STARTUP_LEAK=loaded\nprintf '%s' \"${X_AUTH_TOKEN-unset}\" > startup-leak.txt\n",
    )
    .expect("write hostile bash startup file");
    unsafe {
        std::env::set_var("BASH_ENV", &startup);
    }
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(&bin_dir).expect("create pack bin");
    let helper = bin_dir.join("credential-probe");
    std::fs::write(
        &helper,
        "#!/bin/sh\nif [ \"${X_AUTH_TOKEN-unset}\" = unset ]; then status=absent; else status=available; fi\nprintf '%s|%s|%s|%s|%s|%s' \"$status\" \"${X_AUTH_TOKEN-unset}\" \"${OPENAI_API_KEY-unset}\" \"${UNRELATED_SECRET_KEY-unset}\" \"${PACK_STARTUP_LEAK-unset}\" \"${1-unset}\"\n",
    )
    .expect("write credential probe");
    let mut permissions = std::fs::metadata(&helper)
        .expect("credential probe metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions).expect("make credential probe executable");
    let pack_env = [
        ("DEXT_PACK_DIR".to_string(), root.display().to_string()),
        (
            "DEXT_PACK_CREDENTIAL_ENV".to_string(),
            "X_AUTH_TOKEN,OPENAI_API_KEY".to_string(),
        ),
    ];
    let (live_tx, mut live_rx) = tokio::sync::mpsc::channel(8);
    let live = LiveToolOutput {
        call_id: "credential-helper".to_string(),
        name: "bash".to_string(),
        tx: live_tx,
    };
    let guarded_helper = tool_policy::apply_bash_guardrails(
        "$DEXT_PACK_DIR/bin/credential-probe 'literal argument with spaces'",
    )
    .expect("guard direct helper");
    let output = execute_bash_async_prepared(
        &guarded_helper,
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        None,
        SandboxProfile::DangerFullAccess,
        Some(live),
        &pack_env,
        &[],
    )
    .await
    .expect("run direct pack helper with declared credential");
    assert!(
        output.contains(
            "available|[REDACTED_PACK_CREDENTIAL]|unset|unset|unset|literal argument with spaces"
        ),
        "{output}"
    );
    assert!(!output.contains("fake-pack-auth-token"), "{output}");
    assert!(!output.contains("fake-provider-key"), "{output}");
    assert!(!output.contains("fake-unrelated-key"), "{output}");
    assert!(
        live_rx.try_recv().is_err(),
        "credential-bearing helper output must not bypass redaction via live deltas"
    );
    assert!(
        !root.join("startup-leak.txt").exists(),
        "BASH_ENV must not run before credential-bearing pack helpers"
    );

    let helper_without_declarations = [("DEXT_PACK_DIR".to_string(), root.display().to_string())];
    let output = execute_bash_async_prepared(
        &guarded_helper,
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        None,
        SandboxProfile::DangerFullAccess,
        None,
        &helper_without_declarations,
        &[],
    )
    .await
    .expect("run direct pack helper without declarations");
    assert!(
        output.contains("absent|unset|unset|unset|unset|literal argument with spaces"),
        "{output}"
    );
    assert!(!output.contains("fake-pack-auth-token"), "{output}");
    assert!(!output.contains("fake-provider-key"), "{output}");
    assert!(!output.contains("fake-unrelated-key"), "{output}");

    let outside_helper = root.join("credential-probe-outside-bin");
    std::fs::write(
        &outside_helper,
        "#!/bin/sh\nprintf '%s' \"${X_AUTH_TOKEN-unset}\"\n",
    )
    .expect("write outside credential probe");
    let mut permissions = std::fs::metadata(&outside_helper)
        .expect("outside credential probe metadata")
        .permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&outside_helper, permissions)
        .expect("make outside credential probe executable");
    let linked_helper = bin_dir.join("linked-credential-probe");
    std::os::unix::fs::symlink(&outside_helper, &linked_helper)
        .expect("link outside helper into pack bin");
    let linked_helper_command = shell_single_quote(&linked_helper.display().to_string());
    let output = execute_bash_async_prepared(
        &linked_helper_command,
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        None,
        SandboxProfile::DangerFullAccess,
        None,
        &pack_env,
        &[],
    )
    .await
    .expect("run linked helper without declared credential");
    assert!(output.contains("unset"), "{output}");
    assert!(!output.contains("fake-pack-auth-token"), "{output}");

    let output = execute_bash_async_prepared(
        "printf '%s' \"${X_AUTH_TOKEN-unset}\"",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        None,
        SandboxProfile::DangerFullAccess,
        None,
        &pack_env,
        &[],
    )
    .await
    .expect("run arbitrary bash with pack environment");
    assert!(output.contains("unset"), "{output}");
    assert!(!output.contains("fake-pack-auth-token"), "{output}");

    restore_env_var("X_AUTH_TOKEN", old_auth);
    restore_env_var("OPENAI_API_KEY", old_provider);
    restore_env_var("UNRELATED_SECRET_KEY", old_unrelated);
    restore_env_var("BASH_ENV", old_bash_env);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(unix)]
#[test]
fn provider_secret_commands_are_bounded_cached_and_credential_isolated() -> Result<()> {
    let _guard = env_lock();
    let old_api_key = std::env::var_os("SECRET_COMMAND_TEST_API_KEY");
    let old_bash_env = std::env::var_os("BASH_ENV");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    let old_timeout = std::env::var_os("DEXT_SECRET_COMMAND_TIMEOUT_SECS");
    let root = temp_test_dir("provider-secret-command");
    let startup_marker = root.join("startup-ran");
    let startup = root.join("bash-env.sh");
    std::fs::write(
        &startup,
        format!(
            "printf startup > {}\nexport SECRET_COMMAND_STARTUP=loaded\n",
            shell_single_quote(&startup_marker.display().to_string())
        ),
    )?;
    unsafe {
        std::env::set_var("SECRET_COMMAND_TEST_API_KEY", "secret-command-ambient-key");
        std::env::set_var("BASH_ENV", &startup);
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
        std::env::set_var("DEXT_SECRET_COMMAND_TIMEOUT_SECS", "1");
    }
    crate::provider::command_secret_cache()
        .lock()
        .expect("secret command cache")
        .clear();

    let result = (|| -> Result<()> {
        let isolated = crate::provider::resolve_secret_reference(
            "!printf '%s|%s|%s|%s|%s' \"${SECRET_COMMAND_TEST_API_KEY-unset}\" \"${SECRET_COMMAND_STARTUP-unset}\" \"${GIT_TERMINAL_PROMPT-unset}\" \"${SSH_ASKPASS_REQUIRE-unset}\" \"${GCM_INTERACTIVE-unset}\"",
        );
        assert_eq!(isolated.as_deref(), Some("unset|unset|0|never|never"));
        assert!(!startup_marker.exists(), "BASH_ENV startup file ran");

        unsafe {
            std::env::set_var("SECRET_COMMAND_TEST_API_KEY", "changed-after-cache");
        }
        let cached = crate::provider::resolve_secret_reference(
            "!printf '%s|%s|%s|%s|%s' \"${SECRET_COMMAND_TEST_API_KEY-unset}\" \"${SECRET_COMMAND_STARTUP-unset}\" \"${GIT_TERMINAL_PROMPT-unset}\" \"${SSH_ASKPASS_REQUIRE-unset}\" \"${GCM_INTERACTIVE-unset}\"",
        );
        assert_eq!(cached, isolated, "successful result must be cached");

        let failure_marker = root.join("failure-count");
        let failing = format!(
            "!printf x >> {}; exit 7",
            shell_single_quote(&failure_marker.display().to_string())
        );
        assert_eq!(crate::provider::resolve_secret_reference(&failing), None);
        assert_eq!(crate::provider::resolve_secret_reference(&failing), None);
        assert_eq!(std::fs::read_to_string(&failure_marker)?, "x");

        assert_eq!(
            crate::provider::resolve_secret_reference("!head -c 20000 /dev/zero | tr '\\0' x"),
            None,
            "oversized secret output must be rejected"
        );

        let timeout_marker = root.join("timeout-descendant");
        let timed = format!(
            "!(sleep 2; printf late > {}) & wait",
            shell_single_quote(&timeout_marker.display().to_string())
        );
        let started = std::time::Instant::now();
        assert_eq!(crate::provider::resolve_secret_reference(&timed), None);
        assert!(
            started.elapsed() < std::time::Duration::from_secs(2),
            "secret command timeout was not enforced"
        );
        std::thread::sleep(std::time::Duration::from_millis(1200));
        assert!(
            !timeout_marker.exists(),
            "timed-out secret command descendant survived process-group cleanup"
        );
        Ok(())
    })();

    crate::provider::command_secret_cache()
        .lock()
        .expect("secret command cache")
        .clear();
    restore_env_var("SECRET_COMMAND_TEST_API_KEY", old_api_key);
    restore_env_var("BASH_ENV", old_bash_env);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
    restore_env_var("DEXT_SECRET_COMMAND_TIMEOUT_SECS", old_timeout);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(unix)]
#[test]
fn internal_commands_ignore_tool_credential_opt_in_and_use_private_temp() -> Result<()> {
    let _guard = env_lock();
    let old_secret = std::env::var_os("INTERNAL_TEST_API_KEY");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    let old_bash_env = std::env::var_os("BASH_ENV");
    let old_zdotdir = std::env::var_os("ZDOTDIR");
    let old_node_options = std::env::var_os("NODE_OPTIONS");
    let root = temp_test_dir("internal-command-env");
    let inherited_temp = root.join("inherited-temp");
    let startup_marker = root.join("startup-ran");
    let startup = root.join("startup.sh");
    std::fs::create_dir(&inherited_temp)?;
    std::fs::write(
        &startup,
        format!(
            "printf startup > {}\nexport INTERNAL_STARTUP_RAN=yes\n",
            shell_single_quote(&startup_marker.display().to_string())
        ),
    )?;
    let old_tmpdir = std::env::var_os("TMPDIR");
    let old_tmp = std::env::var_os("TMP");
    let old_temp = std::env::var_os("TEMP");
    unsafe {
        std::env::set_var("INTERNAL_TEST_API_KEY", "internal-secret-fixture");
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
        std::env::set_var("BASH_ENV", &startup);
        std::env::set_var("ZDOTDIR", &root);
        std::env::set_var("NODE_OPTIONS", "--require=/definitely/not/loaded.js");
        std::env::set_var("TMPDIR", &inherited_temp);
        std::env::set_var("TMP", &inherited_temp);
        std::env::set_var("TEMP", &inherited_temp);
    }

    let result = (|| -> Result<()> {
        let mut command = Command::new(bash_executable_path());
        command.arg("-c").arg(
            r#"printf '%s|%s|%s|%s|%s|%s|%s|%s|%s' \
             "${INTERNAL_TEST_API_KEY-unset}" \
             "${INTERNAL_STARTUP_RAN-unset}" \
             "${BASH_ENV-unset}" \
             "${ZDOTDIR-unset}" \
             "${NODE_OPTIONS-unset}" \
             "${GIT_TERMINAL_PROMPT-unset}" \
             "${SSH_ASKPASS_REQUIRE-unset}" \
             "$(stat -c %a "$TMPDIR" 2>/dev/null || stat -f %Lp "$TMPDIR")" \
             "$TMPDIR,$TMP,$TEMP""#,
        );
        let output = run_internal_command_limited(
            command,
            "internal environment probe",
            std::time::Duration::from_secs(5),
        )
        .map_err(anyhow::Error::msg)?;
        assert!(output.success());
        let rendered = String::from_utf8(output.stdout)?;
        let fields = rendered.split('|').collect::<Vec<_>>();
        assert_eq!(fields.len(), 9, "{rendered:?}");
        assert_eq!(
            &fields[..7],
            &["unset", "unset", "unset", "unset", "unset", "0", "never"]
        );
        assert_eq!(fields[7], "700");
        let temps = fields[8].split(',').collect::<Vec<_>>();
        assert_eq!(temps.len(), 3, "{rendered:?}");
        assert_eq!(temps[0], temps[1]);
        assert_eq!(temps[0], temps[2]);
        assert_ne!(Path::new(temps[0]), inherited_temp.as_path());
        assert!(
            !Path::new(temps[0]).exists(),
            "private temp must be removed after the child exits"
        );
        assert!(!startup_marker.exists(), "BASH_ENV startup file ran");
        Ok(())
    })();

    restore_env_var("INTERNAL_TEST_API_KEY", old_secret);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
    restore_env_var("BASH_ENV", old_bash_env);
    restore_env_var("ZDOTDIR", old_zdotdir);
    restore_env_var("NODE_OPTIONS", old_node_options);
    restore_env_var("TMPDIR", old_tmpdir);
    restore_env_var("TMP", old_tmp);
    restore_env_var("TEMP", old_temp);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn browser_launchers_are_bounded_credential_isolated_and_use_private_temp() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let root = temp_test_dir("browser-launcher-isolation");
    let bin_dir = root.join("bin");
    let marker = root.join("launcher-env.txt");
    let startup_marker = root.join("startup-ran.txt");
    let startup = root.join("bash-env.sh");
    std::fs::create_dir_all(&bin_dir)?;
    std::fs::write(
        &startup,
        format!(
            "printf startup > {}\n",
            shell_single_quote(&startup_marker.display().to_string())
        ),
    )?;
    let launcher = bin_dir.join("xdg-open");
    for earlier in ["wslview", "powershell.exe"] {
        let path = bin_dir.join(earlier);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n")?;
        let mut permissions = std::fs::metadata(&path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions)?;
    }
    std::fs::write(
        &launcher,
        r#"#!/bin/bash
printf '%s|%s|%s|%s|%s|%s|%s' \
  "${BROWSER_TEST_API_KEY-unset}" \
  "${BASH_ENV-unset}" \
  "${PYTHONPATH-unset}" \
  "${GIT_TERMINAL_PROMPT-unset}" \
  "$(stat -c %a "$TMPDIR")" \
  "$TMPDIR,$TMP,$TEMP" \
  "$1" > "$BROWSER_TEST_MARKER"
"#,
    )?;
    let mut permissions = std::fs::metadata(&launcher)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&launcher, permissions)?;

    let names = [
        "PATH",
        "BROWSER_TEST_MARKER",
        "BROWSER_TEST_API_KEY",
        "BASH_ENV",
        "PYTHONPATH",
        TOOL_CREDENTIAL_ENV_INHERIT_FLAG,
    ];
    let old = names
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect::<Vec<_>>();
    unsafe {
        std::env::set_var("PATH", prepend_env_path(&bin_dir));
        std::env::set_var("BROWSER_TEST_MARKER", &marker);
        std::env::set_var("BROWSER_TEST_API_KEY", "browser-secret-fixture");
        std::env::set_var("BASH_ENV", &startup);
        std::env::set_var("PYTHONPATH", "/hostile/python/path");
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
    }

    let result = (|| -> Result<()> {
        let launcher_name =
            crate::provider::open_url_in_browser("https://example.invalid/callback?code=fixture")?;
        assert_eq!(launcher_name, "xdg-open");
        let rendered = std::fs::read_to_string(&marker)?;
        let fields = rendered.split('|').collect::<Vec<_>>();
        assert_eq!(fields.len(), 7, "{rendered:?}");
        assert_eq!(&fields[..4], &["unset", "unset", "unset", "0"]);
        assert_eq!(fields[4], "700");
        let temps = fields[5].split(',').collect::<Vec<_>>();
        assert_eq!(temps.len(), 3, "{rendered:?}");
        assert_eq!(temps[0], temps[1]);
        assert_eq!(temps[0], temps[2]);
        assert!(
            !Path::new(temps[0]).exists(),
            "browser launcher scratch must be removed"
        );
        assert_eq!(fields[6], "https://example.invalid/callback?code=fixture");
        assert!(!startup_marker.exists(), "BASH_ENV startup file ran");
        Ok(())
    })();

    for (name, value) in old {
        restore_env_var(name, value);
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(all(unix, not(target_os = "macos")))]
#[test]
fn browser_launcher_timeout_kills_descendants() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let root = temp_test_dir("browser-launcher-timeout");
    let bin_dir = root.join("bin");
    let descendant_marker = root.join("descendant-survived.txt");
    std::fs::create_dir_all(&bin_dir)?;
    let launcher = bin_dir.join("xdg-open");
    std::fs::write(
        &launcher,
        r#"#!/bin/bash
(/bin/sleep 2; printf late > "$BROWSER_DESCENDANT_MARKER") &
wait
"#,
    )?;
    let mut permissions = std::fs::metadata(&launcher)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&launcher, permissions)?;
    for fallback in [
        "wslview",
        "powershell.exe",
        "gio",
        "sensible-browser",
        "x-www-browser",
    ] {
        let path = bin_dir.join(fallback);
        std::fs::write(&path, "#!/bin/sh\nexit 1\n")?;
        let mut permissions = std::fs::metadata(&path)?.permissions();
        permissions.set_mode(0o700);
        std::fs::set_permissions(&path, permissions)?;
    }

    let old_path = std::env::var_os("PATH");
    let old_marker = std::env::var_os("BROWSER_DESCENDANT_MARKER");
    let old_timeout = std::env::var_os("DEXT_BROWSER_OPEN_TIMEOUT_SECS");
    unsafe {
        std::env::set_var("PATH", prepend_env_path(&bin_dir));
        std::env::set_var("BROWSER_DESCENDANT_MARKER", &descendant_marker);
        std::env::set_var("DEXT_BROWSER_OPEN_TIMEOUT_SECS", "1");
    }

    let started = std::time::Instant::now();
    let result = crate::provider::open_url_in_browser("https://example.invalid/timeout");
    let elapsed = started.elapsed();
    restore_env_var("PATH", old_path);
    restore_env_var("BROWSER_DESCENDANT_MARKER", old_marker);
    restore_env_var("DEXT_BROWSER_OPEN_TIMEOUT_SECS", old_timeout);

    assert!(result.is_err(), "hanging launcher unexpectedly succeeded");
    assert!(
        elapsed < std::time::Duration::from_secs(2),
        "browser launcher timeout was not enforced: {elapsed:?}"
    );
    std::thread::sleep(std::time::Duration::from_millis(1200));
    assert!(
        !descendant_marker.exists(),
        "timed-out browser launcher descendant survived process-group cleanup"
    );
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[cfg(unix)]
#[test]
fn eval_shell_ignores_login_and_noninteractive_startup_files() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("eval-shell-startup-isolation");
    let home = root.join("home");
    let login_marker = root.join("login-profile-ran.txt");
    let bash_env_marker = root.join("bash-env-ran.txt");
    let bash_env = root.join("bash-env.sh");
    std::fs::create_dir_all(&home)?;
    std::fs::write(
        home.join(".bash_profile"),
        format!(
            "printf login > {}\nexport EVAL_LOGIN_PROFILE_RAN=yes\n",
            shell_single_quote(&login_marker.display().to_string())
        ),
    )?;
    std::fs::write(
        &bash_env,
        format!(
            "printf bash-env > {}\nexport EVAL_BASH_ENV_RAN=yes\n",
            shell_single_quote(&bash_env_marker.display().to_string())
        ),
    )?;

    let names = [
        "HOME",
        "BASH_ENV",
        "EVAL_TEST_API_KEY",
        TOOL_CREDENTIAL_ENV_INHERIT_FLAG,
    ];
    let old = names
        .iter()
        .map(|name| (*name, std::env::var_os(name)))
        .collect::<Vec<_>>();
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("BASH_ENV", &bash_env);
        std::env::set_var("EVAL_TEST_API_KEY", "eval-secret-fixture");
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
    }

    let result = (|| -> Result<()> {
        let (code, stdout, stderr) = run_eval_shell_command(
            &root,
            "printf '%s|%s|%s|%s' \"${EVAL_TEST_API_KEY-unset}\" \"${EVAL_LOGIN_PROFILE_RAN-unset}\" \"${EVAL_BASH_ENV_RAN-unset}\" \"${BASH_ENV-unset}\"",
        )
        .map_err(anyhow::Error::msg)?;
        assert_eq!(code, 0, "{stderr}");
        assert_eq!(stdout, "unset|unset|unset|unset");
        assert!(!login_marker.exists(), "login profile executed");
        assert!(!bash_env_marker.exists(), "BASH_ENV startup file executed");
        Ok(())
    })();

    for (name, value) in old {
        restore_env_var(name, value);
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(target_os = "linux")]
#[test]
fn eval_shell_confines_project_and_external_writes() -> Result<()> {
    if !sandbox::is_enforced() {
        return Ok(());
    }

    let root = temp_test_dir("eval-shell-write-confinement");
    let outside = root
        .parent()
        .expect("temp root parent")
        .join(format!("dext-eval-outside-{}", std::process::id()));
    let inside = root.join("mutated-by-eval.txt");
    let command = format!(
        "printf inside > {} || true; printf outside > {} || true; test ! -e {} && test ! -e {}",
        shell_single_quote(&inside.to_string_lossy()),
        shell_single_quote(&outside.to_string_lossy()),
        shell_single_quote(&inside.to_string_lossy()),
        shell_single_quote(&outside.to_string_lossy()),
    );

    let (code, _stdout, stderr) =
        run_eval_shell_command(&root, &command).map_err(anyhow::Error::msg)?;
    assert_eq!(code, 0, "{stderr}");
    assert!(!inside.exists(), "eval assertion mutated its fixture");
    assert!(!outside.exists(), "eval assertion escaped its fixture");

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_file(outside);
    Ok(())
}

#[test]
fn pack_credential_redactor_handles_split_and_overlapping_values() {
    let mut redactor = SecretByteRedactor::new(vec![b"token-value".to_vec(), b"token".to_vec()]);
    let mut output = Vec::new();
    redactor.push(b"before token-", |bytes| output.extend_from_slice(bytes));
    redactor.push(b"value middle token after", |bytes| {
        output.extend_from_slice(bytes)
    });
    redactor.finish(|bytes| output.extend_from_slice(bytes));

    assert_eq!(
        String::from_utf8(output).expect("redacted UTF-8"),
        "before [REDACTED_PACK_CREDENTIAL] middle [REDACTED_PACK_CREDENTIAL] after"
    );
}

#[test]
fn hooks_never_receive_declared_pack_credential_values() {
    let _guard = env_lock();
    let old_auth = std::env::var_os("X_AUTH_TOKEN");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    unsafe {
        std::env::set_var("X_AUTH_TOKEN", "fake-hook-auth-token");
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
    }
    let root = temp_test_dir("pack-hook-credential-scrub");
    let hooks = Hooks {
        pre_tool: vec![Hook {
            tool_match: Some("bash".to_string()),
            command: "printf '%s' \"${X_AUTH_TOKEN-unset}\"".to_string(),
        }],
        ..Default::default()
    };
    let extra_env = vec![(
        "DEXT_PACK_CREDENTIAL_ENV".to_string(),
        "X_AUTH_TOKEN".to_string(),
    )];
    let output = hooks.fire(
        "pre_tool",
        "bash",
        &[],
        &extra_env,
        &root,
        SandboxProfile::WorkspaceWrite,
    );
    assert_eq!(output.len(), 1);
    assert!(output[0].0.contains("unset"), "{}", output[0].0);
    assert!(
        !output[0].0.contains("fake-hook-auth-token"),
        "{}",
        output[0].0
    );

    restore_env_var("X_AUTH_TOKEN", old_auth);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sync_tool_children_scrub_command_only_credentials() {
    let _guard = env_lock();
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    unsafe {
        std::env::remove_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    }
    let mut command = Command::new(bash_executable_path());
    command
        .arg("-c")
        .arg("printf '%s' \"${INJECTED_ONLY_SECRET_KEY-unset}\"")
        .env("INJECTED_ONLY_SECRET_KEY", "fake-command-only-secret");
    let (stdout, _, code) = run_sync_command_limited(
        command,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "command-only credential scrub test",
        std::time::Duration::from_secs(5),
    )
    .expect("run scrubbed command");
    assert_eq!(code, 0);
    assert_eq!(stdout.render("stdout"), "unset");
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
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
        &mut None,
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
        &mut None,
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
        &mut None,
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
        &mut None,
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
        &mut None,
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
fn busy_console_input_allows_public_copied_text_as_steering() {
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (runtime_control_tx, mut runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
    let busy = AtomicBool::new(true);

    for pasted in [
        "https://x.com/zephyr_z9/status/1234567890123456789\n",
        "zephyr_z9\n",
        "@zephyr_z9\n",
        "d6280ad878e3\n",
        "grad-programs-ai-security-2027.md\n",
        "08_synthesis/grad-programs-ai-security-2027.md\n",
        "/home/fixture-user/.dext/shelves/finance/packs/stock_deepdive/PACK.md\n",
        "/compact-report/output.md\n",
        "- 08_synthesis/grad-programs-ai-security-2027.md\n",
        "fix the narrow emoji status column\n",
    ] {
        let route = route_interactive_input_line(
            pasted.to_string(),
            &busy,
            &input_tx,
            &runtime_control_tx,
            &steering_tx,
            &mut None,
        );

        assert_eq!(route, InteractiveInputRoute::SteeringQueued);
        assert_eq!(steering_rx.try_recv().ok().as_deref(), Some(pasted.trim()));
        assert!(runtime_control_rx.try_recv().is_err());
        assert!(input_rx.try_recv().is_err());
    }
}

#[test]
fn slash_routing_distinguishes_commands_from_absolute_and_wsl_paths() {
    for command in [
        "/help",
        "/compact status",
        "/pack run demo task",
        "/privacy strict",
    ] {
        assert!(is_slash_command(command), "{command}");
    }
    for path in [
        "/home/fixture-user/.dext/shelves/finance/packs/stock_deepdive/PACK.md",
        "/mnt/c/Users/fixture-user/report.md",
        "/compact-report/output.md",
        r"\\wsl.localhost\Ubuntu\home\fixture-user\.dext\shelves",
        r"\\wsl$\Ubuntu\home\fixture-user\.dext\shelves",
    ] {
        assert!(!is_slash_command(path), "{path}");
    }

    assert_eq!(
        normalize_user_input_path(r"\\wsl.localhost\Ubuntu\home\fixture-user\.dext\shelves"),
        "/home/fixture-user/.dext/shelves"
    );
    assert_eq!(
        normalize_user_input_path(r"\\wsl$\Ubuntu\mnt\c\Users\fixture-user\report.md"),
        "/mnt/c/Users/fixture-user/report.md"
    );
    assert_eq!(
        normalize_user_input_path("/wsl.localhost/Ubuntu/home/fixture-user/report.md"),
        "/wsl.localhost/Ubuntu/home/fixture-user/report.md"
    );
}

#[test]
fn busy_console_input_routes_absolute_and_wsl_paths_as_active_steering() {
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (runtime_control_tx, mut runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (steering_tx, mut steering_rx) = tokio::sync::mpsc::unbounded_channel();
    let busy = AtomicBool::new(true);

    for (input, expected) in [
        (
            "/home/fixture-user/.dext/shelves/finance/packs/stock_deepdive/PACK.md\n",
            "/home/fixture-user/.dext/shelves/finance/packs/stock_deepdive/PACK.md",
        ),
        ("/compact-report/output.md\n", "/compact-report/output.md"),
        (
            "\\\\wsl.localhost\\Ubuntu\\home\\fixture-user\\.dext\\shelves\n",
            "/home/fixture-user/.dext/shelves",
        ),
        (
            "\\\\wsl$\\Ubuntu\\mnt\\c\\Users\\fixture-user\\report.md\n",
            "/mnt/c/Users/fixture-user/report.md",
        ),
    ] {
        let route = route_interactive_input_line(
            input.to_string(),
            &busy,
            &input_tx,
            &runtime_control_tx,
            &steering_tx,
            &mut None,
        );
        assert_eq!(route, InteractiveInputRoute::SteeringQueued);
        assert_eq!(steering_rx.try_recv().ok().as_deref(), Some(expected));
        assert!(runtime_control_rx.try_recv().is_err());
        assert!(input_rx.try_recv().is_err());
    }
}

#[test]
fn idle_console_input_normalizes_wsl_path_before_submission() {
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
    let busy = AtomicBool::new(false);

    let route = route_interactive_input_line(
        "\\\\wsl.localhost\\Ubuntu\\home\\fixture-user\\Dext\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
        &mut None,
    );
    assert_eq!(route, InteractiveInputRoute::Submitted);
    assert_eq!(
        input_rx.try_recv().ok().as_deref(),
        Some("/home/fixture-user/Dext")
    );
}

#[test]
fn idle_console_input_withholds_secret_until_repeated() {
    let (input_tx, mut input_rx) = tokio::sync::mpsc::unbounded_channel();
    let (runtime_control_tx, _runtime_control_rx) = tokio::sync::mpsc::unbounded_channel();
    let (steering_tx, _steering_rx) = tokio::sync::mpsc::unbounded_channel();
    let busy = AtomicBool::new(false);
    let mut pending_secret = None;

    // First Enter with a token-shaped line is withheld even while idle — this
    // is exactly how a pasted GitHub PAT once reached the model transcript.
    let route = route_interactive_input_line(
        "\"ghp_0123456789abcdefghijABCDEFGHIJ012345\"\r\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
        &mut pending_secret,
    );
    assert_eq!(route, InteractiveInputRoute::SecretWithheld);
    assert!(input_rx.try_recv().is_err());

    // An identical repeat is an explicit confirmation and goes through.
    let route = route_interactive_input_line(
        "\"ghp_0123456789abcdefghijABCDEFGHIJ012345\"\r\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
        &mut pending_secret,
    );
    assert_eq!(route, InteractiveInputRoute::Submitted);
    assert!(input_rx.try_recv().is_ok());
    assert_eq!(pending_secret, None);

    // Ordinary prose is unaffected.
    let route = route_interactive_input_line(
        "clean up repo and push to remote\n".to_string(),
        &busy,
        &input_tx,
        &runtime_control_tx,
        &steering_tx,
        &mut pending_secret,
    );
    assert_eq!(route, InteractiveInputRoute::Submitted);
    assert!(input_rx.try_recv().is_ok());
}

#[cfg(unix)]
#[tokio::test]
async fn bash_children_get_no_terminal_and_no_git_prompts() {
    let root = temp_test_dir("bash-no-tty");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let out = execute_bash_async_prepared(
        "printf 'GTP=%s\\n' \"${GIT_TERMINAL_PROMPT:-unset}\"; \
         printf 'ASKPASS=%s\\n' \"${SSH_ASKPASS_REQUIRE:-unset}\"; \
         if read -r -t 1 _line < /dev/tty 2>/dev/null; then echo TTY=open; else echo TTY=closed; fi",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(10),
        None,
        SandboxProfile::WorkspaceWrite,
        None,
        &[],
        &[],
    )
    .await
    .expect("bash runs");
    assert!(out.contains("GTP=0"), "{out}");
    assert!(out.contains("ASKPASS=never"), "{out}");
    // setsid detached the child from the controlling terminal, so /dev/tty
    // cannot be opened and a would-be credential prompt fails instantly
    // instead of hanging or scribbling over the TUI.
    assert!(out.contains("TTY=closed"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn potential_local_secret_detection_keeps_targeted_secret_coverage() {
    assert!(text_is_potential_local_secret(
        "token=abcdefghijklmnopqrstuvwxyz"
    ));
    assert!(text_is_potential_local_secret(
        "Bearer sk-secret-token-that-should-stay-local"
    ));
    assert!(text_is_potential_local_secret(
        "{\"accessToken\":\"secret-token-that-should-go-to-login\"}"
    ));
    assert!(text_is_potential_local_secret(
        "/login chatgpt sk-secret-token-that-should-stay-local"
    ));
    assert!(text_is_potential_local_secret(
        "sk-secret-token-that-should-stay-local"
    ));
    assert!(!text_is_potential_local_secret(
        "d6280ad878e35256aa76aaf02d6bc62c3d850ab1"
    ));
    assert!(!text_is_potential_local_secret("https://x.com/zephyr_z9"));
    assert!(!text_is_potential_local_secret("zephyr_z9"));
    assert!(!text_is_potential_local_secret("@zephyr_z9"));
    assert!(!text_is_potential_local_secret("d6280ad878e3"));
    assert!(!text_is_potential_local_secret(
        "grad-programs-ai-security-2027.md"
    ));
    assert!(!text_is_potential_local_secret(
        "08_synthesis/grad-programs-ai-security-2027.md"
    ));
    assert!(!text_is_potential_local_secret(
        "- 08_synthesis/grad-programs-ai-security-2027.md"
    ));
    assert!(!text_is_potential_local_secret(
        "fix the narrow emoji status column"
    ));
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
fn crash_broken_pipe_detection_does_not_suppress_unrelated_panics() {
    assert!(panic_message_is_broken_pipe(
        "failed printing to stdout: Broken pipe (os error 32)"
    ));
    assert!(panic_message_is_broken_pipe(
        "failed printing to stderr: Broken pipe (os error 32)"
    ));
    assert!(!panic_message_is_broken_pipe(
        "database invariant failed after Broken pipe"
    ));
    assert!(!panic_message_is_broken_pipe(
        "failed printing to stdout: permission denied"
    ));
}

#[test]
fn crash_ids_and_notices_never_expose_storage_paths() {
    let first = new_crash_id().expect("generate first crash id");
    let second = new_crash_id().expect("generate second crash id");
    assert_ne!(first, second);
    for id in [&first, &second] {
        let parts = id.split('-').collect::<Vec<_>>();
        assert_eq!(parts.len(), 4, "unexpected crash id: {id}");
        assert_eq!(parts[0], "crash");
        assert!(parts[1].parse::<u64>().is_ok());
        assert!(parts[2].parse::<u32>().is_ok());
        assert_eq!(parts[3].len(), 12);
        assert!(parts[3].bytes().all(|byte| byte.is_ascii_hexdigit()));

        let notice = crash_snapshot_notice(id);
        assert_eq!(notice, format!("[dext crash snapshot id: {id}]"));
        assert!(!notice.contains('/'));
        assert!(!notice.contains('\\'));
        assert!(!notice.contains(".dext"));
        assert!(!notice.contains("crashes"));
    }
}

#[test]
fn crash_snapshot_body_does_not_block_on_locked_runtime_state() {
    let _state = crash_runtime_state().lock().expect("lock crash state");
    let body = crash_snapshot_body("crash-safe-id", None);
    assert_eq!(body["runtime"], Value::Null);
}

#[test]
fn crash_session_ids_accept_only_generated_non_path_tokens() {
    let valid = PathBuf::from(format!(
        "/private/project/sessions/1784175000-4242-abcdef012345/{LATEST_SESSION_NAME}.jsonl"
    ));
    assert_eq!(
        generated_session_id_from_path(&valid).as_deref(),
        Some("1784175000-4242-abcdef012345")
    );

    for invalid in [
        format!("/private/project/sessions/{LATEST_SESSION_NAME}.jsonl"),
        format!(
            "/private/project/sessions/1784175000-4242-ABCDEF012345/{LATEST_SESSION_NAME}.jsonl"
        ),
        format!(
            "/private/project/sessions/1784175000-4242-abcdef01234g/{LATEST_SESSION_NAME}.jsonl"
        ),
        "/private/project/sessions/1784175000-4242-abcdef012345/transcript.jsonl".to_string(),
        format!(
            "/private/project/sessions/1784175000-4242-abcdef012345/{LATEST_SESSION_NAME}.json"
        ),
    ] {
        assert_eq!(
            generated_session_id_from_path(Path::new(&invalid)),
            None,
            "accepted invalid session path {invalid}"
        );
    }
}

#[test]
fn crash_breadcrumbs_omit_all_free_form_event_fields() {
    let secret = "CRASH_EVENT_SECRET_SENTINEL";
    let events = [
        AgentEvent::ToolCallPreview {
            call_id: secret.to_string(),
            name: secret.to_string(),
            summary: secret.to_string(),
        },
        AgentEvent::ToolCallStart {
            call_id: secret.to_string(),
            name: secret.to_string(),
            summary: secret.to_string(),
        },
        AgentEvent::ToolCallResult {
            call_id: secret.to_string(),
            name: secret.to_string(),
            ok: false,
            preview: secret.to_string(),
            content: secret.to_string(),
        },
        AgentEvent::HttpRetry {
            attempt: 2,
            wait_secs: 1,
            reason: secret.to_string(),
        },
        AgentEvent::CompactFailed {
            message: secret.to_string(),
        },
        AgentEvent::RuntimeControl(secret.to_string()),
        AgentEvent::SteeringReceived {
            messages: 1,
            preview: secret.to_string(),
        },
        AgentEvent::ToolBatchStart {
            batch_id: secret.to_string(),
            call_ids: vec![secret.to_string()],
            labels: vec![secret.to_string()],
        },
        AgentEvent::ToolBatchEnd {
            batch_id: secret.to_string(),
            call_ids: vec![secret.to_string()],
            labels: vec![secret.to_string()],
            failed: 1,
        },
    ];

    for event in events {
        let breadcrumb = crash_event_breadcrumb(&event).expect("structural breadcrumb");
        assert!(
            !breadcrumb.contains(secret),
            "leaked breadcrumb: {breadcrumb}"
        );
    }
    assert!(crash_event_breadcrumb(&AgentEvent::Error(secret.to_string())).is_none());
    assert!(
        crash_event_breadcrumb(&AgentEvent::ToolOutputDelta {
            call_id: secret.to_string(),
            name: secret.to_string(),
            stream: secret.to_string(),
            text: secret.to_string(),
        })
        .is_none()
    );
}

#[test]
fn crash_snapshot_schema_omits_raw_paths_and_environment_text() {
    let _guard = env_lock();
    let old_columns = std::env::var_os("COLUMNS");
    let old_lines = std::env::var_os("LINES");
    let old_backtrace = std::env::var_os("RUST_BACKTRACE");
    unsafe {
        std::env::set_var("COLUMNS", "CRASH_COLUMNS_SECRET");
        std::env::set_var("LINES", "42");
        std::env::set_var("RUST_BACKTRACE", "CRASH_BACKTRACE_SECRET");
    }

    let source_path = "/private/source/CRASH_SOURCE_SECRET.rs";
    let body = crash_snapshot_body("crash-safe-id", Some((source_path, 7, 9)));
    let encoded = serde_json::to_string(&body).expect("serialize crash body");

    restore_env_var("COLUMNS", old_columns);
    restore_env_var("LINES", old_lines);
    restore_env_var("RUST_BACKTRACE", old_backtrace);

    assert_eq!(body["id"], "crash-safe-id");
    assert_eq!(body["panic"], "panic captured; free-form payload omitted");
    assert_eq!(body["location"]["line"], 7);
    assert_eq!(body["location"]["column"], 9);
    assert_eq!(body["location"]["file_sha256"], sha256_hex_str(source_path));
    assert!(body.get("cwd").is_none());
    assert!(body.get("cwd_sha256").is_none());
    assert!(body.get("backtrace").is_none());
    assert_eq!(body["terminal"]["columns"], Value::Null);
    assert_eq!(body["terminal"]["lines"], 42);
    assert_eq!(body["backtrace_enabled"], false);
    for secret in [
        source_path,
        "CRASH_SOURCE_SECRET",
        "CRASH_COLUMNS_SECRET",
        "CRASH_BACKTRACE_SECRET",
    ] {
        assert!(
            !encoded.contains(secret),
            "snapshot leaked {secret}: {encoded}"
        );
    }
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
        &mut None,
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
        &mut None,
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
        &mut None,
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
        &mut None,
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
    agent.model = DEFAULT_LOCAL_MODEL.to_string();
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
    assert!(rendered.contains("Queued for next response"), "{rendered}");
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
    assert!(rendered.contains("Session map"), "{rendered}");
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
    assert!(packet.contains("does not rewind files"), "{packet}");
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
fn work_map_query_does_not_match_hidden_objective_text() -> Result<()> {
    let header = SessionHeader {
        work_ledger: WorkLedger {
            objective: "unique hidden objective text".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    let history = vec![Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "visible unrelated request".to_string(),
        }],
    }];
    let map = build_session_work_map(Path::new("test-session.jsonl"), &header, &history);
    let args = parse_work_map_command_args("query unique hidden objective");
    let (_, filters) = parse_work_map_filter_args(&args)?;
    let visible = map
        .waypoints
        .iter()
        .filter(|wp| work_map_filter_matches(&map, wp, &filters))
        .collect::<Vec<_>>();
    assert!(visible.is_empty(), "{visible:?}");
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
    assert_eq!(replay.header.version, SESSION_FORMAT_VERSION);
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
                last_error: Some("HTTP 520 <unknown status code>: error code: 520".to_string()),
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
        let saved_header_line = std::fs::read_to_string(&saved.latest_session_path)?
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
        assert_eq!(
            loaded.provider_health.providers["chatgpt"]
                .last_error
                .as_deref(),
            Some("HTTP 520")
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
        assert!(listing.contains("Latest"), "{listing}");
        assert!(listing.contains("latest"), "{listing}");
        assert!(listing.contains("Named"), "{listing}");
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
fn latest_state_defaults_are_project_scoped_with_session_overlays() -> Result<()> {
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

        let alpha_session_scoped = session_latest_session_path(&alpha, "s1");
        let beta_session_scoped = session_latest_session_path(&beta, "s1");
        let alpha_log_scoped = session_latest_log_path(&alpha, "s1");
        let beta_log_scoped = session_latest_log_path(&beta, "s1");

        assert_ne!(alpha_session, beta_session);
        assert_ne!(alpha_log, beta_log);
        assert_ne!(alpha_session_scoped, beta_session_scoped);
        assert_ne!(alpha_log_scoped, beta_log_scoped);
        assert!(alpha_session.starts_with(dext_home.join("projects")));
        assert!(alpha_log.starts_with(dext_home.join("projects")));
        assert!(alpha_session_scoped.starts_with(dext_home.join("projects")));
        assert!(alpha_session_scoped.ends_with("sessions/s1/_latest.jsonl"));
        assert!(alpha_log_scoped.ends_with("sessions/s1/latest.log"));
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
fn session_state_dirs_are_session_scoped_and_concurrent() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("session-scoped-state");
    let dext_home = root.join("dext-home");

    // Safe: test holds a global lock around env mutation.
    unsafe {
        std::env::set_var("DEXT_HOME", &dext_home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let result = (|| -> Result<()> {
        let root = std::fs::canonicalize(&root)?;
        let sid1 = "s1";
        let sid2 = "s2";
        let first = SessionStateLock::acquire(&root, sid1)?;
        let second = SessionStateLock::acquire(&root, sid2)?;
        assert_ne!(first.path, second.path);
        assert!(first.path.starts_with(dext_home.join("projects")));
        assert!(first.path.ends_with("sessions/s1/session.lock.json"));
        assert!(second.path.ends_with("sessions/s2/session.lock.json"));
        let err =
            SessionStateLock::acquire(&root, sid1).expect_err("same session id should be locked");
        assert!(
            format!("{err:#}").contains("dext session s1 is already open"),
            "{err:#}"
        );
        drop(first);
        let reacquired = SessionStateLock::acquire(&root, sid1)?;
        drop(reacquired);
        drop(second);
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
fn set_sandbox_root_allows_other_live_sessions() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("sandbox-session-switch");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    let old_sessions_dir = std::env::var_os("DEXT_SESSIONS_DIR");
    let old_logs_dir = std::env::var_os("DEXT_LOGS_DIR");
    unsafe {
        std::env::set_var("DEXT_HOME", root.join("dext-home"));
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }

    let result = (|| -> Result<()> {
        let alpha = std::fs::canonicalize(
            std::fs::create_dir_all(root.join("alpha")).map(|_| root.join("alpha"))?,
        )?;
        let beta = std::fs::canonicalize(
            std::fs::create_dir_all(root.join("beta")).map(|_| root.join("beta"))?,
        )?;

        let mut agent = test_agent(&alpha);
        let other = SessionStateLock::acquire(&beta, "other-session")?;
        agent.set_sandbox_root(beta.clone())?;
        assert_eq!(agent.sandbox_root, beta);
        assert!(agent.state_lock.as_ref().is_some_and(|lock| {
            lock.path
                .ends_with(format!("sessions/{}/session.lock.json", agent.session_id))
        }));
        drop(other);
        Ok(())
    })();

    restore_env_var("DEXT_HOME", old_dext_home);
    restore_env_var("DEXT_SESSIONS_DIR", old_sessions_dir);
    restore_env_var("DEXT_LOGS_DIR", old_logs_dir);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn same_session_id_lock_blocks_double_open() -> Result<()> {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let root = temp_test_dir("same-session-lock");
    let root = std::fs::canonicalize(&root)?;
    let first = SessionStateLock::acquire(&root, "same")?;
    let err = SessionStateLock::acquire(&root, "same").expect_err("same session should fail");
    assert!(
        format!("{err:#}").contains("dext session same is already open"),
        "{err:#}"
    );
    drop(first);
    let second = SessionStateLock::acquire(&root, "same")?;
    drop(second);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn stale_session_lock_is_reaped_on_acquire() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("stale-session-lock");
    let home = root.join("home");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }

    let result = (|| -> Result<()> {
        let project = std::fs::canonicalize(&root)?;
        let lock_path = session_state_lock_path(&project, "same");
        std::fs::create_dir_all(lock_path.parent().unwrap())?;
        std::fs::write(
            &lock_path,
            serde_json::to_vec(&json!({
                "token": "stale",
                "pid": 0,
                "acquired_at": 1,
                "project_key": project_key(&project),
                "sandbox_root": project.display().to_string(),
                "session_id": "same"
            }))?,
        )?;

        let lock = SessionStateLock::acquire(&project, "same")?;
        assert!(lock.path.exists());
        drop(lock);
        assert!(!lock_path.exists(), "fresh lock should clean up on drop");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn session_prune_removes_stale_locks_but_preserves_sessions() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("prune-stale-locks");
    let home = root.join("home");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }

    let result = (|| -> Result<()> {
        let project = std::fs::canonicalize(&root)?;
        let session_path = session_latest_session_path(&project, "old-session");
        std::fs::create_dir_all(session_path.parent().unwrap())?;
        std::fs::write(&session_path, b"{}\n")?;
        let lock_path = session_state_lock_path(&project, "old-session");
        std::fs::write(
            &lock_path,
            serde_json::to_vec(&json!({
                "token": "stale",
                "pid": 0,
                "acquired_at": 1,
                "project_key": project_key(&project),
                "sandbox_root": project.display().to_string(),
                "session_id": "old-session"
            }))?,
        )?;

        prune_project_dirs(&project, false, 0)?;
        assert!(session_path.exists(), "session JSONL must be preserved");
        assert!(!lock_path.exists(), "stale lock should be pruned");
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
        std::env::remove_var("DEXT_LOGS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn session_prune_does_not_follow_symlinked_session_directories() -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let _guard = env_lock();
        let root = temp_test_dir("prune-symlinked-session");
        let home = root.join("home");
        unsafe {
            std::env::set_var("DEXT_HOME", &home);
            std::env::remove_var("DEXT_SESSIONS_DIR");
            std::env::remove_var("DEXT_LOGS_DIR");
        }

        let result = (|| -> Result<()> {
            let project = std::fs::canonicalize(&root)?;
            let linked_lock = session_state_lock_path(&project, "linked-session");
            std::fs::create_dir_all(linked_lock.parent().unwrap().parent().unwrap())?;
            let outside = root.join("outside-session");
            std::fs::create_dir_all(&outside)?;
            let outside_lock = outside.join(SESSION_STATE_LOCK_NAME);
            let lock_bytes = serde_json::to_vec(&json!({
                "token": "external-stale",
                "pid": 0,
                "acquired_at": 1,
                "project_key": "outside",
                "sandbox_root": outside.display().to_string(),
                "session_id": "outside"
            }))?;
            std::fs::write(&outside_lock, &lock_bytes)?;
            symlink(&outside, linked_lock.parent().unwrap())?;

            prune_project_dirs(&project, false, 0)?;
            assert_eq!(std::fs::read(&outside_lock)?, lock_bytes);
            Ok(())
        })();

        unsafe {
            std::env::remove_var("DEXT_HOME");
            std::env::remove_var("DEXT_SESSIONS_DIR");
            std::env::remove_var("DEXT_LOGS_DIR");
        }
        let _ = std::fs::remove_dir_all(&root);
        result?;
    }
    Ok(())
}

#[test]
fn session_latest_prefers_newest_session_dir_but_supports_legacy_latest() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("latest-session-selector");
    let dext_home = root.join("dext-home");
    unsafe {
        std::env::set_var("DEXT_HOME", &dext_home);
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }
    let result = (|| -> Result<()> {
        let root = std::fs::canonicalize(&root)?;
        let legacy = project_latest_session_path(&root);
        atomic_write_bytes(&legacy, b"{}\n")?;
        assert_eq!(latest_session_path(&root), legacy);
        let session_latest = session_latest_session_path(&root, "newer-session");
        std::thread::sleep(std::time::Duration::from_millis(2));
        atomic_write_bytes(&session_latest, b"{}\n")?;
        assert_eq!(latest_session_path(&root), session_latest);
        let listing = render_session_listing(&root);
        assert!(listing.contains("Autosaved"), "{listing}");
        assert_eq!(
            resolve_session_selector(&root, "newer-session")?,
            session_latest
        );
        assert!(listing.contains("Latest"), "{listing}");
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
fn slash_resume_selector_loads_latest_autosaved_and_paths() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-resume-selector");
    let home = root.join("home");
    let project = root.join("project");
    let old_home = std::env::var_os("HOME");
    let old_userprofile = std::env::var_os("USERPROFILE");
    std::fs::create_dir_all(&home)?;
    std::fs::create_dir_all(&project)?;
    unsafe {
        std::env::set_var("HOME", &home);
        std::env::set_var("USERPROFILE", &home);
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }

    let result = (|| -> Result<()> {
        let project = std::fs::canonicalize(&project)?;
        let mut agent = test_agent(&project);
        let session_id = agent.session_id.clone();
        agent.history.push(Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "resume me".to_string(),
            }],
        });
        let saved = agent.save_latest_session()?;

        assert_eq!(resolve_session_selector(&project, "latest")?, saved);
        assert_eq!(resolve_session_selector(&project, &session_id)?, saved);
        assert_eq!(
            resolve_session_selector(&project, saved.parent().unwrap().to_str().unwrap())?,
            saved
        );
        let tilde = saved.strip_prefix(&home)?;
        let bracketed_tilde = format!("[~/{}]", tilde.display());
        assert_eq!(resolve_session_selector(&project, &bracketed_tilde)?, saved);

        let mut loaded = test_agent(&project);
        loaded.load_session("latest")?;
        assert_eq!(loaded.history.len(), 1);
        loaded.history.clear();
        loaded.load_session(&session_id)?;
        assert_eq!(loaded.history.len(), 1);
        loaded.history.clear();
        loaded.load_session(&bracketed_tilde)?;
        assert_eq!(loaded.history.len(), 1);
        Ok(())
    })();

    restore_env_var("HOME", old_home);
    restore_env_var("USERPROFILE", old_userprofile);
    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SESSIONS_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn malformed_session_load_is_transactional() -> Result<()> {
    let root = temp_test_dir("session-transactional-root");
    let restored_root = temp_test_dir("session-transactional-restored-root");
    let mut agent = test_agent(&root);
    agent.model = "live-model".to_string();
    agent.privacy.enabled = false;
    agent.privacy.strict_paths = false;
    agent.approval_profile = ApprovalProfile::Ask;
    agent.pack_hook_env = vec![("LIVE_PACK_STATE".to_string(), "preserved".to_string())];
    agent.suppress_pack_activation = true;
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "live history".to_string(),
        }],
    });

    let mut header = serde_json::to_value(agent.session_header())?;
    header["model"] = json!("restored-model");
    header["sandbox"] = json!(restored_root.display().to_string());
    header["approval_profile"] = json!("always");
    header["privacy"] = json!({
        "enabled": true,
        "strict_paths": true,
        "findings": {}
    });
    let valid_message = Message {
        role: "assistant".to_string(),
        content: vec![Block::Text {
            text: "restored history".to_string(),
        }],
    };
    let session_path = root.join("malformed-session.jsonl");
    std::fs::write(
        &session_path,
        format!(
            "{}\n{}\n{{malformed trailing message\n",
            serde_json::to_string(&header)?,
            serde_json::to_string(&valid_message)?
        ),
    )?;

    let error = agent
        .load_session_from_path(&session_path)
        .expect_err("malformed trailing message must reject the whole session");
    assert!(
        error.to_string().contains("bad message on line 3"),
        "{error:#}"
    );
    assert_eq!(agent.sandbox_root, root);
    assert_eq!(agent.model, "live-model");
    assert_eq!(agent.approval_profile, ApprovalProfile::Ask);
    assert!(!agent.privacy.enabled);
    assert!(!agent.privacy.strict_paths);
    assert_eq!(agent.pack_hook_env.len(), 1);
    assert_eq!(agent.pack_hook_env[0].0, "LIVE_PACK_STATE");
    assert!(agent.suppress_pack_activation);
    assert_eq!(agent.history.len(), 1);
    assert!(matches!(
        &agent.history[0].content[0],
        Block::Text { text } if text == "live history"
    ));

    let _ = std::fs::remove_dir_all(&root);
    let _ = std::fs::remove_dir_all(&restored_root);
    Ok(())
}

#[test]
fn invalid_saved_sandbox_load_is_transactional() -> Result<()> {
    let root = temp_test_dir("session-invalid-sandbox-root");
    let missing = root.join("missing-sandbox");
    let file = root.join("sandbox-file");
    std::fs::write(&file, "not a directory\n")?;

    let mut agent = test_agent(&root);
    agent.model = "live-model".to_string();
    agent.allowed.insert("write_file".to_string());
    agent.pack_hook_env = vec![("LIVE_PACK_STATE".to_string(), "preserved".to_string())];
    agent.suppress_pack_activation = true;
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "live history".to_string(),
        }],
    });

    for (index, invalid_root) in [&missing, &file].into_iter().enumerate() {
        let mut header = serde_json::to_value(agent.session_header())?;
        header["model"] = json!(format!("restored-model-{index}"));
        header["sandbox"] = json!(invalid_root.display().to_string());
        header["allowed"] = json!(["bash"]);
        let restored_message = Message {
            role: "assistant".to_string(),
            content: vec![Block::Text {
                text: "restored history".to_string(),
            }],
        };
        let session_path = root.join(format!("invalid-sandbox-{index}.jsonl"));
        std::fs::write(
            &session_path,
            format!(
                "{}\n{}\n",
                serde_json::to_string(&header)?,
                serde_json::to_string(&restored_message)?
            ),
        )?;

        let error = agent
            .load_session_from_path(&session_path)
            .expect_err("invalid saved sandbox must reject the whole session");
        if index == 0 {
            assert!(
                error.to_string().contains("restoring saved sandbox"),
                "{error:#}"
            );
        } else {
            assert!(
                error
                    .to_string()
                    .contains("sandbox root is not a directory"),
                "{error:#}"
            );
        }
        assert_eq!(agent.sandbox_root, root);
        assert_eq!(agent.model, "live-model");
        assert_eq!(agent.allowed, HashSet::from(["write_file".to_string()]));
        assert_eq!(
            agent.pack_hook_env,
            vec![("LIVE_PACK_STATE".to_string(), "preserved".to_string())]
        );
        assert!(agent.suppress_pack_activation);
        assert_eq!(agent.history.len(), 1);
        assert!(matches!(
            &agent.history[0].content[0],
            Block::Text { text } if text == "live history"
        ));
    }

    let slash_result = agent.set_sandbox_root(file.clone());
    assert!(
        slash_result.is_err(),
        "regular file must not become a sandbox root"
    );
    assert_eq!(agent.sandbox_root, root);

    let _ = std::fs::remove_dir_all(root);
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
fn side_effect_capability_covers_every_permission_required_tool() {
    for tool in provider_tool_definitions() {
        if needs_permission(tool.name) {
            assert!(
                is_side_effect_capable_tool(tool.name),
                "permission-required tool {} must remain journaled",
                tool.name
            );
        }
    }
    for read_only in [
        "read_file",
        "read_symbol",
        "fd",
        "rg",
        "jq",
        "fzf",
        "git_diff",
        "git_log",
        "todo_read",
    ] {
        assert!(
            !is_side_effect_capable_tool(read_only),
            "read-only tool {read_only} must remain journal-free"
        );
    }
}

#[test]
fn trust_mode_toggle_controls_privileged_allowlist() {
    let root = temp_test_dir("trust-toggle");
    let mut agent = test_agent(&root);
    assert!(!agent.trust_mode_active());

    let enabled = agent.set_trust_mode(true);
    assert!(enabled > 0, "expected privileged tools to be added");
    assert!(agent.trust_mode_active());
    assert_eq!(agent.approval_profile(), ApprovalProfile::Always);

    let disabled = agent.set_trust_mode(false);
    assert!(disabled > 0, "expected privileged tools to be removed");
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
fn hook_execution_requires_explicit_non_persistent_approval() {
    let root = temp_test_dir("hook-approval-policy");
    let mut agent = test_agent(&root);
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Deny,
        requests: requests.clone(),
    }));

    assert!(
        !hooks_approved(&mut agent),
        "empty hook sets need no approval"
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 0);

    agent.hooks.user_prompt.push(Hook {
        tool_match: None,
        command: "printf hook".to_string(),
    });
    assert!(!hooks_approved(&mut agent), "denied hooks must not run");
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!agent.allowed.contains(HOOKS_APPROVAL_NAME));

    agent.set_approval_profile(ApprovalProfile::Never);
    assert!(!hooks_approved(&mut agent));
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "approval=never must reject hooks without prompting"
    );

    agent.set_approval_profile(ApprovalProfile::Always);
    assert!(
        !hooks_approved(&mut agent),
        "global approval=always must not implicitly authorize project hooks"
    );
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 2);

    agent.set_approval_profile(ApprovalProfile::Ask);
    let once_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Once,
        requests: once_requests.clone(),
    }));
    assert!(hooks_approved(&mut agent));
    assert!(!agent.allowed.contains(HOOKS_APPROVAL_NAME));
    assert!(hooks_approved(&mut agent));
    assert_eq!(
        once_requests.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "Once is intentionally scoped by the caller to one turn"
    );

    let always_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Always,
        requests: always_requests.clone(),
    }));
    assert!(hooks_approved(&mut agent));
    assert!(agent.allowed.contains(HOOKS_APPROVAL_NAME));
    assert!(hooks_approved(&mut agent));
    assert_eq!(always_requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(
        !agent
            .session_header()
            .allowed
            .contains(&HOOKS_APPROVAL_NAME.to_string()),
        "hook trust must not be serialized into sessions"
    );
    agent.set_sandbox_profile(SandboxProfile::ReadOnly);
    assert!(
        !agent.allowed.contains(HOOKS_APPROVAL_NAME),
        "hook trust must not survive sandbox-profile changes"
    );

    agent.set_approval_profile(ApprovalProfile::AutoRead);
    assert!(!agent.allowed.contains(HOOKS_APPROVAL_NAME));
    agent.allowed.insert(HOOKS_APPROVAL_NAME.to_string());
    agent.allowed.insert(DIAGNOSTICS_APPROVAL_NAME.to_string());
    agent.allowed.insert("bash".to_string());
    agent.session_enabled = false;
    let next_root = temp_test_dir("hook-approval-next-root");
    agent
        .set_sandbox_root(next_root.clone())
        .expect("switch sandbox root");
    assert!(
        agent.allowed.is_empty(),
        "tool and operation grants must not cross sandbox roots: {:?}",
        agent.allowed
    );

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(next_root);
}

#[test]
fn git_commit_hook_execution_requires_explicit_non_persistent_approval() {
    let root = temp_test_dir("git-commit-hook-approval-policy");
    let mut agent = test_agent(&root);
    agent.set_approval_profile(ApprovalProfile::Always);
    let deny_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Deny,
        requests: deny_requests.clone(),
    }));

    assert!(
        !git_commit_hooks_approved(&mut agent),
        "global approval=always must not authorize repository Git hooks"
    );
    assert_eq!(deny_requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(!agent.allowed.contains(HOOKS_APPROVAL_NAME));

    agent.set_approval_profile(ApprovalProfile::Ask);
    let once_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Once,
        requests: once_requests.clone(),
    }));
    assert!(git_commit_hooks_approved(&mut agent));
    assert!(git_commit_hooks_approved(&mut agent));
    assert_eq!(
        once_requests.load(std::sync::atomic::Ordering::SeqCst),
        2,
        "Once must be scoped by the caller to one turn"
    );
    assert!(!agent.allowed.contains(HOOKS_APPROVAL_NAME));

    let always_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Always,
        requests: always_requests.clone(),
    }));
    assert!(git_commit_hooks_approved(&mut agent));
    assert!(git_commit_hooks_approved(&mut agent));
    assert_eq!(always_requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(agent.allowed.contains(HOOKS_APPROVAL_NAME));
    assert!(
        !agent
            .session_header()
            .allowed
            .contains(&HOOKS_APPROVAL_NAME.to_string()),
        "native Git hook trust must not be serialized"
    );

    let _ = std::fs::remove_dir_all(root);
}

#[test]
fn diagnostics_requires_danger_approval_before_execution() {
    let root = temp_test_dir("diagnostics-approval-denied");
    let mut agent = test_agent(&root);
    agent.set_approval_profile(ApprovalProfile::AutoWrite);
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Deny,
        requests: requests.clone(),
    }));

    assert_eq!(handle_slash("/diagnostics", &mut agent), Some(true));
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(agent.work_ledger.diagnostics.is_empty());
    assert!(!agent.allowed.contains(DIAGNOSTICS_APPROVAL_NAME));

    agent.set_approval_profile(ApprovalProfile::Never);
    assert_eq!(handle_slash("/diag", &mut agent), Some(true));
    assert_eq!(
        requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "approval=never must deny without prompting"
    );
    assert!(agent.work_ledger.diagnostics.is_empty());

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn diagnostics_always_approval_is_persisted_visible_and_reset_with_profile() {
    let root = temp_test_dir("diagnostics-approval-always");
    let mut agent = test_agent(&root);
    let requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Always,
        requests: requests.clone(),
    }));

    assert!(diagnostics_approved(&mut agent));
    assert_eq!(requests.load(std::sync::atomic::Ordering::SeqCst), 1);
    assert!(agent.allowed.contains(DIAGNOSTICS_APPROVAL_NAME));
    assert!(
        agent
            .session_header()
            .allowed
            .contains(&DIAGNOSTICS_APPROVAL_NAME.to_string())
    );

    let denied_requests = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    agent.set_sink(Box::new(FixedPermissionSink {
        choice: Choice::Deny,
        requests: denied_requests.clone(),
    }));
    assert!(diagnostics_approved(&mut agent));
    assert_eq!(
        denied_requests.load(std::sync::atomic::Ordering::SeqCst),
        0,
        "persisted diagnostics approval must not prompt again"
    );

    agent.set_approval_profile(ApprovalProfile::AutoRead);
    assert!(!agent.allowed.contains(DIAGNOSTICS_APPROVAL_NAME));
    assert!(!diagnostics_approved(&mut agent));
    assert_eq!(
        denied_requests.load(std::sync::atomic::Ordering::SeqCst),
        1,
        "auto-read must not authorize project code execution"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn lsp_message_reader_bounds_headers_and_body_before_allocation() {
    let oversized_body = format!("Content-Length: {}\r\n\r\n", LSP_MESSAGE_BODY_CAP + 1);
    let error = read_lsp_message(&mut std::io::Cursor::new(oversized_body))
        .expect_err("oversized body must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    let oversized_line = format!("X-Header: {}\r\n\r\n", "x".repeat(LSP_HEADER_LINE_CAP));
    let error = read_lsp_message(&mut std::io::Cursor::new(oversized_line))
        .expect_err("oversized header line must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    let mut aggregate = String::new();
    while aggregate.len() <= LSP_HEADER_TOTAL_CAP {
        aggregate.push_str("X: 1234567890\r\n");
    }
    aggregate.push_str("\r\n");
    let error = read_lsp_message(&mut std::io::Cursor::new(aggregate))
        .expect_err("oversized aggregate headers must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    let duplicate = "Content-Length: 2\r\nContent-Length: 2\r\n\r\n{}";
    let error = read_lsp_message(&mut std::io::Cursor::new(duplicate))
        .expect_err("duplicate content length must be rejected");
    assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);

    let valid = "Content-Length: 2\r\n\r\n{}";
    assert_eq!(
        read_lsp_message(&mut std::io::Cursor::new(valid)).expect("valid LSP frame"),
        Some("{}".to_string())
    );
}

#[cfg(unix)]
#[test]
fn rust_analyzer_diagnostics_bounds_frame_queue_and_cleanup() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let root = temp_test_dir("diagnostics-lsp-frame-flood");
    let bin_dir = root.join("bin");
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::create_dir(&bin_dir)?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"lsp-frame-flood\"\nversion = \"0.1.0\"\nedition = \"2024\"\n",
    )?;
    std::fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n")?;
    let analyzer = bin_dir.join("rust-analyzer");
    std::fs::write(
        &analyzer,
        "#!/bin/sh\nwhile :; do printf 'Content-Length: 2\\r\\n\\r\\n{}'; done\n",
    )?;
    let mut permissions = std::fs::metadata(&analyzer)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&analyzer, permissions)?;

    let old_path = std::env::var_os("PATH");
    let old_timeout = std::env::var_os("DEXT_LSP_DIAGNOSTICS_TIMEOUT_SECS");
    unsafe {
        std::env::set_var("PATH", prepend_env_path(&bin_dir));
        std::env::set_var("DEXT_LSP_DIAGNOSTICS_TIMEOUT_SECS", "1");
    }

    let started = std::time::Instant::now();
    let report = run_rust_analyzer_diagnostics(&root);
    let elapsed = started.elapsed();

    restore_env_var("PATH", old_path);
    restore_env_var("DEXT_LSP_DIAGNOSTICS_TIMEOUT_SECS", old_timeout);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        report.is_some(),
        "fake analyzer should start and emit frames"
    );
    assert!(
        elapsed < std::time::Duration::from_secs(5),
        "bounded LSP cleanup took {elapsed:?}"
    );
    Ok(())
}

#[cfg(unix)]
#[test]
fn rust_diagnostics_sources_reject_symlinks_and_hardlinks() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("diagnostics-source-aliases");
    let outside = temp_test_dir("diagnostics-source-aliases-outside");
    std::fs::create_dir_all(root.join("src/nested")).expect("create source directories");
    std::fs::write(root.join("src/lib.rs"), "pub fn local() {}\n").expect("write local source");
    std::fs::write(
        outside.join("secret.rs"),
        "const SECRET: &str = \"outside\";\n",
    )
    .expect("write outside source");
    symlink(outside.join("secret.rs"), root.join("src/symlink.rs")).expect("create source symlink");
    symlink(&outside, root.join("src/nested/external")).expect("create source directory symlink");
    std::fs::hard_link(outside.join("secret.rs"), root.join("src/hardlink.rs"))
        .expect("create source hardlink");

    let sources = rust_files_for_diagnostics(&root);
    assert_eq!(sources.len(), 1);
    assert_eq!(
        sources[0].0.strip_prefix(&root).expect("project source"),
        Path::new("src/lib.rs")
    );
    assert_eq!(sources[0].1, "pub fn local() {}\n");

    let _ = std::fs::remove_dir_all(root);
    let _ = std::fs::remove_dir_all(outside);
}

#[test]
fn rust_diagnostics_sources_enforce_file_count_size_and_total_budgets() {
    let file_count_root = temp_test_dir("diagnostics-source-file-count");
    std::fs::create_dir_all(file_count_root.join("src")).expect("create source directory");
    for index in 0..(LSP_DIAGNOSTIC_FILE_LIMIT + 6) {
        std::fs::write(
            file_count_root.join("src").join(format!("f{index:03}.rs")),
            format!("pub const V{index}: usize = {index};\n"),
        )
        .expect("write counted source");
    }
    let counted = rust_files_for_diagnostics(&file_count_root);
    assert_eq!(counted.len(), LSP_DIAGNOSTIC_FILE_LIMIT);

    let byte_root = temp_test_dir("diagnostics-source-byte-budgets");
    std::fs::create_dir_all(byte_root.join("src")).expect("create source directory");
    std::fs::write(
        byte_root.join("src/oversized.rs"),
        vec![b'x'; LSP_DIAGNOSTIC_FILE_BYTE_CAP as usize + 1],
    )
    .expect("write oversized source");
    let bounded_size = LSP_DIAGNOSTIC_FILE_BYTE_CAP as usize - 1;
    for index in 0..9 {
        std::fs::write(
            byte_root.join("src").join(format!("bounded{index}.rs")),
            vec![b'x'; bounded_size],
        )
        .expect("write bounded source");
    }
    let bounded = rust_files_for_diagnostics(&byte_root);
    assert_eq!(
        bounded.len(),
        8,
        "aggregate budget must exclude the ninth file"
    );
    assert!(
        bounded.iter().all(
            |(path, _)| path.file_name().and_then(|name| name.to_str()) != Some("oversized.rs")
        )
    );
    assert!(
        bounded
            .iter()
            .map(|(_, text)| text.len() as u64)
            .sum::<u64>()
            <= LSP_DIAGNOSTIC_TOTAL_BYTE_CAP
    );

    let _ = std::fs::remove_dir_all(file_count_root);
    let _ = std::fs::remove_dir_all(byte_root);
}

#[cfg(target_os = "linux")]
#[test]
fn cargo_diagnostics_confine_hostile_build_script_writes_to_private_scratch() -> Result<()> {
    if !sandbox::is_enforced() {
        return Ok(());
    }

    let root = temp_test_dir("diagnostics-hostile-build-script");
    std::fs::create_dir_all(root.join("src"))?;
    std::fs::write(
        root.join("Cargo.toml"),
        "[package]\nname = \"hostile-diagnostics\"\nversion = \"0.1.0\"\nedition = \"2024\"\nbuild = \"build.rs\"\n",
    )?;
    std::fs::write(
        root.join("Cargo.lock"),
        "# This file is automatically @generated by Cargo.\n# It is not intended for manual editing.\nversion = 4\n\n[[package]]\nname = \"hostile-diagnostics\"\nversion = \"0.1.0\"\n",
    )?;
    std::fs::write(root.join("src/lib.rs"), "pub fn value() -> u8 { 1 }\n")?;
    std::fs::write(
        root.join("build.rs"),
        r#"fn main() {
    let marker = std::path::Path::new(&std::env::var("CARGO_MANIFEST_DIR").unwrap())
        .join("escaped-from-build-script.txt");
    match std::fs::write(&marker, "escaped") {
        Ok(()) => panic!("diagnostics sandbox unexpectedly allowed project write"),
        Err(error) => panic!("diagnostics sandbox blocked project write: {error}"),
    }
}
"#,
    )?;

    let report = run_cargo_check_diagnostics(&root);
    assert_eq!(report.status, "failed", "{}", report.raw_output);
    assert!(
        report
            .raw_output
            .contains("diagnostics sandbox blocked project write"),
        "hostile build script did not reach the confined write attempt:\n{}",
        report.raw_output
    );
    assert!(!root.join("escaped-from-build-script.txt").exists());
    assert!(
        !root.join("target").exists(),
        "diagnostic build artifacts must stay in private scratch"
    );

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn lsp_diagnostics_parser_extracts_publish_diagnostics() {
    let root = temp_test_dir("lsp-diagnostics-parser");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let uri = file_uri_from_path(&root.join("src/lib.rs"));
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
    agent.history.push(Message {
        role: "user".to_string(),
        content: vec![tool_result_block(
            "call-edit",
            "updated src/lib.rs",
            Some(false),
        )],
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

    assert!(action_contract_should_retry(1));
    assert!(action_contract_should_retry(2));
    assert!(!action_contract_should_retry(3));
    assert!(!action_contract_should_retry(u32::MAX));
    let halted = action_contract_retry_halted_note(3);
    assert!(halted.contains("after 3 assistant responses"), "{halted}");
    assert!(halted.contains("unbounded provider loop"), "{halted}");

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
fn privacy_redacts_sensitive_tool_output_and_strict_mode_blocks_secret_paths() {
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
        redacted.text.contains("[privacy: redacted"),
        "{}",
        redacted.text
    );

    let default_denial = agent
        .privacy
        .path_denial("read_file", &json!({"path": ".env"}), &root);
    assert!(
        default_denial.is_none(),
        "default redaction mode must not block user-readable files"
    );

    agent.privacy.strict_paths = true;
    let denial = agent
        .privacy
        .path_denial("read_file", &json!({"path": ".env"}), &root)
        .expect("strict mode blocks secret path");
    assert!(denial.contains("strict path mode"), "{denial}");

    for (tool, input) in [
        ("fd", json!({"pattern": ".*", "path": ".ssh"})),
        ("rg", json!({"pattern": "token", "path": ".env"})),
        ("jq", json!({"filter": ".", "path": "credentials.json"})),
        ("git_diff", json!({"path": "private.key"})),
        ("git_log", json!({"path": "secrets/history.txt"})),
    ] {
        let denial = agent
            .privacy
            .path_denial(tool, &input, &root)
            .unwrap_or_else(|| panic!("strict mode must block {tool}: {input}"));
        assert!(denial.contains(tool), "{denial}");
    }
    for (tool, input) in [
        ("fd", json!({"pattern": ".*", "extra_args": ["--hidden"]})),
        ("fd", json!({"pattern": ".*", "extra_args": ["-uuu"]})),
        (
            "rg",
            json!({"pattern": "token", "extra_args": ["--follow"]}),
        ),
        (
            "rg",
            json!({"pattern": "token", "extra_args": ["--glob=.env"]}),
        ),
    ] {
        let denial = agent
            .privacy
            .path_denial(tool, &input, &root)
            .unwrap_or_else(|| panic!("strict mode must block sensitive {tool} scope: {input}"));
        assert!(denial.contains("search scope"), "{denial}");
    }
    assert!(
        agent
            .privacy
            .path_denial(
                "rg",
                &json!({"pattern": "needle", "path": ".", "extra_args": ["-i"]}),
                &root,
            )
            .is_none(),
        "ordinary strict-mode code search must remain available"
    );
    assert!(
        agent
            .privacy
            .path_denial("fd", &json!({"pattern": "\\.rs$", "path": "."}), &root)
            .is_none(),
        "ordinary strict-mode file discovery must remain available"
    );

    assert!(privacy_sensitive_path("/tmp/.ssh/config"));
    assert!(privacy_sensitive_path("config/providers.json"));
    assert!(privacy_sensitive_path("private.key"));
    assert!(!privacy_sensitive_path("src/private_api.rs"));
    assert!(!privacy_sensitive_path(
        "docs/credential-safe-subprocesses.md"
    ));
    assert!(!privacy_sensitive_path("notes/secretary.md"));

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        std::fs::create_dir_all(root.join(".ssh")).expect("create sensitive directory");
        std::fs::write(root.join(".ssh/config"), "Host example\n").expect("write sensitive file");
        symlink(root.join(".ssh/config"), root.join("notes.txt")).expect("create benign alias");
        let denial = agent
            .privacy
            .path_denial("read_file", &json!({"path": "notes.txt"}), &root)
            .expect("symlink alias to sensitive path blocked");
        assert!(denial.contains("notes.txt"), "{denial}");
        for tool in ["fd", "rg", "jq", "git_diff", "git_log"] {
            let input = match tool {
                "fd" => json!({"pattern": ".*", "path": "notes.txt"}),
                "rg" => json!({"pattern": "Host", "path": "notes.txt"}),
                "jq" => json!({"filter": ".", "path": "notes.txt"}),
                _ => json!({"path": "notes.txt"}),
            };
            let denial = agent
                .privacy
                .path_denial(tool, &input, &root)
                .unwrap_or_else(|| panic!("strict mode must resolve sensitive alias for {tool}"));
            assert!(denial.contains("notes.txt"), "{denial}");
        }
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn privacy_env_defaults_to_redaction_and_requires_explicit_strict_paths() {
    let _guard = env_lock();
    let old_privacy = std::env::var_os("DEXT_PRIVACY");
    let root = temp_test_dir("privacy-env-modes");

    unsafe { std::env::remove_var("DEXT_PRIVACY") };
    let default_policy = PrivacyPolicy::from_env();
    assert!(default_policy.enabled);
    assert!(!default_policy.strict_paths);

    unsafe { std::env::set_var("DEXT_PRIVACY", "1") };
    let redaction_policy = PrivacyPolicy::from_env();
    assert!(redaction_policy.enabled);
    assert!(!redaction_policy.strict_paths);

    unsafe { std::env::set_var("DEXT_PRIVACY", "strict") };
    let mut strict_policy = PrivacyPolicy::from_env();
    assert!(strict_policy.enabled);
    assert!(strict_policy.strict_paths);
    assert!(
        strict_policy
            .path_denial("read_file", &json!({"path": ".env"}), &root)
            .is_some()
    );

    unsafe { std::env::set_var("DEXT_PRIVACY", "0") };
    let disabled_policy = PrivacyPolicy::from_env();
    assert!(!disabled_policy.enabled);
    assert!(!disabled_policy.strict_paths);

    restore_env_var("DEXT_PRIVACY", old_privacy);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn privacy_ignores_decimal_and_unlabeled_http_numeric_values() {
    let mut policy = PrivacyPolicy::default();
    let body = r#"HTTP/2 200
content-type: application/json
x-request-id: 49210000000008

{"regularMarketPrice":278.10000000001,"previousClose":123.10000000009,"marketCap":8510000000000,"period1":1704067200,"period2":1735689600}"#;

    let redacted = policy.apply_tool_output("http", &json!({}), body.to_string());

    assert_eq!(redacted.text, body);
    assert_eq!(redacted.counts.total(), 0);
    assert_eq!(policy.findings.total(), 0);
    assert!(!redacted.text.contains("[privacy:"));
}

#[test]
fn privacy_keeps_labeled_identity_payment_and_secret_protection() {
    let mut policy = PrivacyPolicy::default();
    let ssn = ["123", "45", "6789"].join("-");
    let card = ["4111", "1111", "1111", "1111"].join(" ");
    let account = ["123456", "789012"].concat();
    let routing = ["123", "456", "780"].concat();
    let api_key = ["sk-test-", "A1b2C3d4E5f6"].concat();
    let bearer = ["bearer-test-", "Z9y8X7w6"].concat();
    let private_key = format!(
        "-----BEGIN {}-----\nlocal-test-key-material\n-----END {}-----",
        "PRIVATE KEY", "PRIVATE KEY"
    );
    let body = format!(
        "ssn: {ssn}\ncard number: {card}\naccount number: {account}\nrouting number: {routing}\napi_key={api_key}\nAuthorization: Bearer {bearer}\n{private_key}"
    );

    let redacted = policy.apply_tool_output("http", &json!({}), body);

    assert_eq!(redacted.counts.ssn, 1);
    assert_eq!(redacted.counts.credit_card, 1);
    assert_eq!(redacted.counts.account_number, 2);
    assert_eq!(redacted.counts.api_key, 2);
    assert_eq!(redacted.counts.private_key, 1);
    assert!(redacted.text.contains("[REDACTED_SSN]"));
    assert!(redacted.text.contains("[REDACTED_CARD]"));
    assert!(redacted.text.contains("[REDACTED_ACCOUNT]"));
    assert_eq!(redacted.text.matches("[REDACTED_SECRET]").count(), 2);
    assert!(redacted.text.contains("[REDACTED_PRIVATE_KEY]"));
    assert!(redacted.text.contains("raw values withheld"));
    assert!(!redacted.text.contains(&ssn));
    assert!(!redacted.text.contains(&card));
    assert!(!redacted.text.contains(&account));
    assert!(!redacted.text.contains(&routing));
    assert!(!redacted.text.contains(&api_key));
    assert!(!redacted.text.contains(&bearer));
}

#[test]
fn privacy_ignores_secret_placeholders_and_code_expressions() {
    let mut policy = PrivacyPolicy::default();
    let text = r#"api_key: String
password = std::env::var("PASSWORD")?
auth_token=$TOKEN
client_secret: example-value
secret_key: <redacted>
assert api_key == expected"#;

    let redacted = policy.apply_tool_output("read_file", &json!({}), text.to_string());

    assert_eq!(redacted.text, text);
    assert_eq!(redacted.counts.total(), 0);
    assert_eq!(policy.findings.total(), 0);
    assert!(!redacted.text.contains("[privacy:"));
}

#[test]
fn slash_privacy_toggles_runtime_policy() {
    let root = temp_test_dir("privacy-slash");
    let mut agent = test_agent(&root);

    assert_eq!(handle_slash("/privacy on", &mut agent), Some(true));
    assert!(agent.privacy.enabled);
    assert!(!agent.privacy.strict_paths);

    assert_eq!(handle_slash("/privacy strict", &mut agent), Some(true));
    assert!(agent.privacy.enabled);
    assert!(agent.privacy.strict_paths);

    assert_eq!(handle_slash("/privacy on", &mut agent), Some(true));
    assert!(agent.privacy.enabled);
    assert!(!agent.privacy.strict_paths);
    assert_eq!(handle_slash("/privacy status", &mut agent), Some(true));

    assert_eq!(handle_slash("/privacy off", &mut agent), Some(true));
    assert!(!agent.privacy.enabled);
    assert!(!agent.privacy.strict_paths);

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
fn default_and_frugal_toolsets_keep_core_capabilities() {
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
    for name in ["jq", "fzf", "awk", "git_log", "csvkit"] {
        assert!(!default_names.contains(name), "default should hide {name}");
    }

    agent.tool_context_profile = ToolContextProfile::Full;
    agent.context_mode = ContextMode::Frugal;
    agent.refresh_tools_for_context();
    let frugal_names: HashSet<&str> = agent.tools.iter().map(|t| t.name).collect();
    assert_eq!(agent.tool_context_profile(), ToolContextProfile::Full);
    assert!(frugal_names.is_superset(&default_names));
    for name in ["jq", "fzf", "awk", "git_log", "csvkit"] {
        assert!(
            frugal_names.contains(name),
            "full toolset should retain {name}"
        );
    }
    assert!(frugal_names.contains("http"));
    assert!(frugal_names.contains("git_commit"));
    assert!(frugal_names.contains("bash"));
    assert!(frugal_names.contains("git_diff"));

    agent.tool_context_profile = ToolContextProfile::Default;
    agent.allowed.insert("jq".to_string());
    agent.allowed.insert("bash".to_string());
    agent.deny_tools.insert("csvkit".to_string());
    agent.deny_tools.insert("rg".to_string());
    agent.refresh_tools_for_context();
    assert!(!agent.allowed.contains("jq"));
    assert!(agent.allowed.contains("bash"));
    assert!(!agent.deny_tools.contains("csvkit"));
    assert!(agent.deny_tools.contains("rg"));

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn full_toolset_env_exposes_specialized_tools() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("toolset-full-env");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    unsafe { std::env::set_var("DEXT_TOOLSET", "full") };
    let check = || -> Result<()> {
        let mut agent = test_agent(&root);
        agent.tool_context_profile = ToolContextProfile::from_env();
        agent.refresh_tools_for_context();
        let names: HashSet<&str> = agent.tools.iter().map(|t| t.name).collect();
        for name in ["jq", "fzf", "awk", "git_log", "csvkit"] {
            assert!(names.contains(name), "full toolset should expose {name}");
        }
        assert_eq!(agent.tool_context_profile(), ToolContextProfile::Full);
        Ok(())
    };
    let result = check();
    unsafe { std::env::remove_var("DEXT_TOOLSET") };
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn thinking_effort_parse_and_cycle() {
    assert_eq!(ThinkingEffort::parse("off"), Some(ThinkingEffort::Off));
    assert_eq!(ThinkingEffort::parse("none"), Some(ThinkingEffort::Off));
    assert_eq!(ThinkingEffort::parse("low"), Some(ThinkingEffort::Low));
    assert_eq!(ThinkingEffort::parse("MED"), Some(ThinkingEffort::Medium));
    assert_eq!(ThinkingEffort::parse("x-high"), Some(ThinkingEffort::XHigh));
    assert_eq!(ThinkingEffort::parse("maximum"), Some(ThinkingEffort::Max));
    assert_eq!(ThinkingEffort::parse("unknown"), None);
    assert_eq!(ThinkingEffort::Off.cycle(-1), ThinkingEffort::Max);
    assert_eq!(ThinkingEffort::Low.cycle(-1), ThinkingEffort::Off);
    assert_eq!(ThinkingEffort::XHigh.cycle(1), ThinkingEffort::Max);
    assert_eq!(ThinkingEffort::Max.cycle(1), ThinkingEffort::Off);
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

#[tokio::test(flavor = "current_thread")]
async fn provider_bug_fallback_rebuilds_request_body_before_retry() {
    let root = temp_test_dir("provider-workaround-request-rebuild");
    std::fs::write(root.join("input.txt"), "hello").expect("write fixture");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        fn read_request_body(stream: &mut std::net::TcpStream) -> Vec<u8> {
            stream
                .set_read_timeout(Some(std::time::Duration::from_secs(5)))
                .expect("set read timeout");
            let mut request = Vec::new();
            let mut buf = [0u8; 1024];
            let header_end = loop {
                let read = stream.read(&mut buf).expect("read request");
                assert!(read > 0, "client closed before request completed");
                request.extend_from_slice(&buf[..read]);
                if let Some(end) = request.windows(4).position(|window| window == b"\r\n\r\n") {
                    break end + 4;
                }
            };
            let headers = String::from_utf8_lossy(&request[..header_end]);
            let content_length = headers
                .lines()
                .find_map(|line| {
                    line.to_ascii_lowercase()
                        .strip_prefix("content-length:")
                        .and_then(|value| value.trim().parse::<usize>().ok())
                })
                .unwrap_or(0);
            while request.len() < header_end + content_length {
                let read = stream.read(&mut buf).expect("read request body");
                assert!(read > 0, "client closed before request body completed");
                request.extend_from_slice(&buf[..read]);
            }
            request[header_end..header_end + content_length].to_vec()
        }

        let mut bodies = Vec::new();
        for response_index in 0..3 {
            let (mut stream, _) = listener.accept().expect("accept request");
            bodies.push(read_request_body(&mut stream));
            let (status, content_type, body) = match response_index {
                0 => (
                    "200 OK",
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_retry\",\"function\":{\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"input.txt\\\"}\"}}]},\"finish_reason\":\"tool_calls\"}]}\n\ndata: [DONE]\n\n",
                ),
                1 => (
                    "400 Bad Request",
                    "application/json",
                    "No tool call found for function call output with call_id call_retry.",
                ),
                _ => (
                    "200 OK",
                    "text/event-stream",
                    "data: {\"choices\":[{\"delta\":{\"content\":\"Recovered.\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n",
                ),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                body.len()
            );
            stream
                .write_all(response.as_bytes())
                .expect("write response");
        }
        bodies
    });

    let mut agent = test_agent(&root);
    configure_local_openai_agent(&mut agent, format!("http://{addr}"));
    agent.max_iterations = Some(2);
    agent
        .chat("Read input.txt once, then report the result.".to_string())
        .await
        .expect("provider workaround should recover");

    let bodies = server.join().expect("server thread");
    let rejected: Value = serde_json::from_slice(&bodies[1]).expect("rejected request JSON");
    let retried: Value = serde_json::from_slice(&bodies[2]).expect("retried request JSON");
    assert!(
        rejected["messages"]
            .as_array()
            .is_some_and(|messages| messages.iter().any(|message| message["role"] == "tool")),
        "{rejected}"
    );
    assert!(
        retried["messages"].as_array().is_some_and(|messages| {
            messages.iter().all(|message| message["role"] != "tool")
                && messages.iter().any(|message| {
                    message["content"].as_str().is_some_and(|text| {
                        text.contains("provider rejected structured tool_result")
                    })
                })
        }),
        "retry reused the stale request body: {retried}"
    );

    let _ = std::fs::remove_dir_all(&root);
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
        ExternalExecutionPolicy {
            timeout: std::time::Duration::from_millis(150),
            sandbox_profile: SandboxProfile::WorkspaceWrite,
            allow_tool_credentials: true,
        },
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
        ExternalExecutionPolicy {
            timeout: std::time::Duration::from_secs(10),
            sandbox_profile: SandboxProfile::WorkspaceWrite,
            allow_tool_credentials: true,
        },
    )
    .await
    .expect_err("expected interrupt");
    assert!(err.contains("killed by interrupt"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn doctor_report_covers_core_checks() {
    let _guard = env_lock();
    let root = temp_test_dir("doctor-report");
    let old_sandbox_profile = std::env::var_os("DEXT_SANDBOX_PROFILE");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::remove_var("DEXT_SANDBOX_PROFILE");
    }
    let (report, _warnings) = doctor_report(&root);
    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    restore_env_var("DEXT_SANDBOX_PROFILE", old_sandbox_profile);
    let _ = std::fs::remove_dir_all(&root);

    for needle in [
        "dext doctor",
        "version:",
        "sandbox:",
        "terminal:",
        "git repo:",
        "tool bash:",
        "active provider:",
        "session locks:",
    ] {
        assert!(report.contains(needle), "missing '{needle}' in:\n{report}");
    }
    // The sandbox line must reflect the real platform status, never be empty.
    assert!(
        report.contains(&crate::sandbox::describe()),
        "sandbox line should embed the platform description:\n{report}"
    );
}

#[test]
fn doctor_findings_render_levels_and_effective_policy_source() {
    let (rendered, warnings) = render_doctor_findings(&[
        DoctorFinding::ok("healthy", "ready"),
        DoctorFinding::info("optional", "absent"),
        DoctorFinding::warn("repair", "required"),
    ]);
    assert_eq!(warnings, 1);
    assert!(rendered.contains("[ok  ] healthy: ready"), "{rendered}");
    assert!(rendered.contains("[info] optional: absent"), "{rendered}");
    assert!(rendered.contains("[warn] repair: required"), "{rendered}");

    let _guard = env_lock();
    let root = temp_test_dir("doctor-policy-source");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    let old_sessions = std::env::var_os("DEXT_SESSIONS_DIR");
    let old_approval = std::env::var_os("DEXT_APPROVAL");
    let old_trust = std::env::var_os("DEXT_TRUST");
    unsafe {
        std::env::set_var("DEXT_HOME", root.join("home"));
        std::env::set_var("DEXT_SESSIONS_DIR", root.join("sessions"));
        std::env::set_var("DEXT_APPROVAL", "always");
        std::env::set_var("DEXT_TRUST", "1");
    }
    let (report, _) = doctor_report_with_overrides(
        &root,
        Some(ApprovalProfile::AutoRead),
        Some(SandboxProfile::DangerFullAccess),
    );
    restore_env_var("DEXT_HOME", old_dext_home);
    restore_env_var("DEXT_SESSIONS_DIR", old_sessions);
    restore_env_var("DEXT_APPROVAL", old_approval);
    restore_env_var("DEXT_TRUST", old_trust);
    let _ = std::fs::remove_dir_all(&root);

    assert!(
        report.contains("approval policy: auto-read (source CLI)"),
        "{report}"
    );
    assert!(
        report.contains("sandbox: effective profile danger-full-access"),
        "{report}"
    );
    assert!(
        report.contains("sandbox kernel: intentionally disabled"),
        "{report}"
    );
}

#[test]
fn doctor_reports_bounded_latest_state_without_executing_auth_references() -> Result<()> {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let root = temp_test_dir("doctor-latest-state");
    let dext_home = root.join("home");
    let sessions = root.join("sessions");
    let session_dir = sessions.join("session-1");
    std::fs::create_dir_all(&session_dir)?;
    std::fs::create_dir_all(&dext_home)?;

    let old_dext_home = std::env::var_os("DEXT_HOME");
    let old_sessions = std::env::var_os("DEXT_SESSIONS_DIR");
    let old_approval = std::env::var_os("DEXT_APPROVAL");
    let old_trust = std::env::var_os("DEXT_TRUST");
    let old_sandbox = std::env::var_os("DEXT_SANDBOX_PROFILE");
    unsafe {
        std::env::set_var("DEXT_HOME", &dext_home);
        std::env::set_var("DEXT_SESSIONS_DIR", &sessions);
        std::env::set_var("DEXT_APPROVAL", "never");
        std::env::remove_var("DEXT_TRUST");
        std::env::set_var("DEXT_SANDBOX_PROFILE", "workspace-write");
    }

    let result = (|| -> Result<()> {
        let session_path = session_dir.join(format!("{LATEST_SESSION_NAME}.jsonl"));
        let session_bytes = std::fs::read(state_fixture_path("sessions", "v3.jsonl"))?;
        std::fs::write(&session_path, &session_bytes)?;
        let todo_path = session_dir.join("DEXT.todo.json");
        let todo_bytes = std::fs::read(state_fixture_path("todo", "corrupt.json"))?;
        std::fs::write(&todo_path, &todo_bytes)?;
        let settings_path = dext_home.join("settings.json");
        let settings_bytes = std::fs::read(state_fixture_path("settings", "out-of-range.json"))?;
        std::fs::write(&settings_path, &settings_bytes)?;
        let journal_path = session_dir.join("tool-journal.json");
        let journal_bytes =
            std::fs::read(state_fixture_path("tool-journal", "v1-unresolved.json"))?;
        std::fs::write(&journal_path, &journal_bytes)?;
        #[cfg(unix)]
        std::fs::set_permissions(&journal_path, std::fs::Permissions::from_mode(0o600))?;

        let provider_path = dext_home.join("providers.json");
        let provider_bytes = std::fs::read(state_fixture_path("providers", "v2.json"))?;
        std::fs::write(&provider_path, &provider_bytes)?;
        let marker = root.join("auth-command-executed");
        let secret = "doctor-secret-must-not-render";
        let auth_bytes = serde_json::to_vec(&json!({
            "version": 1,
            "providers": {
                "openai": {
                    "type": "api_key",
                    "key": format!("!printf '{secret}' > {}", marker.display())
                }
            }
        }))?;
        let auth_path = dext_home.join("auth.json");
        std::fs::write(&auth_path, &auth_bytes)?;
        #[cfg(unix)]
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o600))?;

        let (report, warnings) = doctor_report_with_overrides(&root, None, None);
        assert!(warnings >= 3, "{report}");
        assert!(
            report.contains("approval policy: never (source DEXT_APPROVAL)"),
            "{report}"
        );
        assert!(report.contains("provider catalog: valid v2"), "{report}");
        assert!(report.contains("active provider: local"), "{report}");
        assert!(report.contains("auth store: valid v1"), "{report}");
        #[cfg(unix)]
        assert!(
            report.contains("auth permissions: owner-only mode 0600"),
            "{report}"
        );
        assert!(report.contains("latest session: valid v3"), "{report}");
        assert!(report.contains("[warn] latest todo:"), "{report}");
        assert!(report.contains("[warn] settings:"), "{report}");
        assert!(
            report.contains("tool journal: 1 unresolved/uncertain call(s)"),
            "{report}"
        );
        assert!(
            report.contains("checkpoints: unavailable outside a Git worktree"),
            "{report}"
        );
        assert!(!report.contains(secret), "{report}");
        assert!(!report.contains("printf"), "{report}");
        assert!(
            !marker.exists(),
            "doctor executed an auth command reference"
        );
        assert_eq!(std::fs::read(&session_path)?, session_bytes);
        assert_eq!(std::fs::read(&todo_path)?, todo_bytes);
        assert_eq!(std::fs::read(&settings_path)?, settings_bytes);
        assert_eq!(std::fs::read(&journal_path)?, journal_bytes);
        assert_eq!(std::fs::read(&provider_path)?, provider_bytes);
        assert_eq!(std::fs::read(&auth_path)?, auth_bytes);
        Ok(())
    })();

    restore_env_var("DEXT_HOME", old_dext_home);
    restore_env_var("DEXT_SESSIONS_DIR", old_sessions);
    restore_env_var("DEXT_APPROVAL", old_approval);
    restore_env_var("DEXT_TRUST", old_trust);
    restore_env_var("DEXT_SANDBOX_PROFILE", old_sandbox);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn doctor_rejects_symlinked_latest_session_entries() -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let _guard = env_lock();
        let root = temp_test_dir("doctor-latest-session-symlink");
        let sessions = root.join("sessions");
        let outside = root.join("outside");
        std::fs::create_dir_all(&sessions)?;
        std::fs::create_dir_all(&outside)?;
        std::fs::write(outside.join("latest.jsonl"), "{}\n")?;
        symlink(&outside, sessions.join("linked-session"))?;
        let old_sessions = std::env::var_os("DEXT_SESSIONS_DIR");
        unsafe {
            std::env::set_var("DEXT_SESSIONS_DIR", &sessions);
        }

        let directory_error = doctor_latest_session_path(&root).expect_err("reject directory link");
        assert!(
            directory_error.contains("symlinked session entry"),
            "{directory_error}"
        );

        std::fs::remove_file(sessions.join("linked-session"))?;
        let regular = sessions.join("regular-session");
        std::fs::create_dir_all(&regular)?;
        symlink(
            outside.join("latest.jsonl"),
            regular.join(format!("{LATEST_SESSION_NAME}.jsonl")),
        )?;
        let file_error = doctor_latest_session_path(&root).expect_err("reject file link");
        assert!(file_error.contains("path is a symlink"), "{file_error}");

        std::fs::remove_file(regular.join(format!("{LATEST_SESSION_NAME}.jsonl")))?;
        let safe_session = session_latest_session_path(&root, "safe-session");
        atomic_write_bytes(&safe_session, b"{}\n")?;
        symlink(
            outside.join("latest.jsonl"),
            project_latest_session_path(&root),
        )?;
        let legacy_error = doctor_latest_session_path(&root)
            .expect_err("reject legacy file link even when a safe session is newer");
        assert!(legacy_error.contains("path is a symlink"), "{legacy_error}");

        std::fs::remove_dir_all(&sessions)?;
        symlink(&outside, &sessions)?;
        let root_error = doctor_latest_session_path(&root).expect_err("reject sessions root link");
        assert!(
            root_error.contains("directory is a symlink"),
            "{root_error}"
        );

        restore_env_var("DEXT_SESSIONS_DIR", old_sessions);
        let _ = std::fs::remove_dir_all(&root);
    }
    Ok(())
}

#[test]
fn doctor_treats_intentional_full_access_as_configured_not_unavailable() {
    let (ok, detail) = sandbox_doctor_status(SandboxProfile::DangerFullAccess);
    assert!(ok);
    assert!(detail.contains("intentionally disabled"), "{detail}");
    assert!(!detail.contains("path-validation"), "{detail}");

    let (ok, detail) = sandbox_doctor_status(SandboxProfile::WorkspaceWrite);
    assert_eq!(ok, crate::sandbox::is_enforced());
    assert!(detail.contains("workspace-write"), "{detail}");
    assert!(detail.contains(&crate::sandbox::describe()), "{detail}");
}

#[tokio::test]
async fn sandbox_never_breaks_benign_commands() {
    // Regardless of platform/enforcement, a read-only command must still run
    // under every profile (graceful degradation, no false failures).
    let root = temp_test_dir("sandbox-benign");
    for profile in [
        SandboxProfile::ReadOnly,
        SandboxProfile::WorkspaceWrite,
        SandboxProfile::DangerFullAccess,
    ] {
        let out = execute_bash_async_with_timeout(
            "echo hello",
            &root,
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(5),
            profile,
        )
        .await
        .expect("benign command must succeed under every profile");
        assert!(out.contains("exit: 0"), "profile {profile:?}: {out}");
        assert!(out.contains("hello"), "profile {profile:?}: {out}");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn read_only_sandbox_does_not_break_git_inspection() {
    // Git inspection stays confined like other external tools. Keep the fixture
    // under the checkout so the parent test remains compatible with an outer
    // workspace-write sandbox; the child still receives the repository itself as
    // its read-only sandbox root.
    let repo = std::env::current_dir()
        .expect("current checkout")
        .join(format!(".dext-sbx-git-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&repo);
    let setup = (|| -> std::result::Result<(), String> {
        std::fs::create_dir_all(&repo).map_err(|e| e.to_string())?;
        git_ok(&repo, &["init", "-q"]);
        git_ok(&repo, &["config", "user.email", "t@e.invalid"]);
        git_ok(&repo, &["config", "user.name", "T"]);
        std::fs::write(repo.join("f.txt"), "one\n").map_err(|e| e.to_string())?;
        git_ok(&repo, &["add", "f.txt"]);
        git_ok(&repo, &["commit", "-q", "-m", "base"]);
        std::fs::write(repo.join("f.txt"), "two\n").map_err(|e| e.to_string())?;
        Ok(())
    })();

    let outcome = match setup {
        Ok(()) => {
            execute_builtin_call(
                "git_diff".to_string(),
                json!({}),
                repo.clone(),
                Arc::new(AtomicBool::new(false)),
                None,
                None,
                None,
                None,
                None,
                SandboxProfile::ReadOnly,
                false,
                None,
                Vec::new(),
            )
            .await
        }
        Err(e) => Err(e),
    };

    let _ = std::fs::remove_dir_all(&repo);
    let body = outcome.expect("git_diff must run under the read-only sandbox");
    assert!(
        body.contains("f.txt") || body.contains("-one") || body.contains("+two"),
        "git_diff should still produce a diff under read-only sandbox: {body}"
    );
}

#[tokio::test]
async fn sandbox_enforces_write_confinement_when_kernel_supports_it() {
    // Only meaningful where the OS actually enforces (Linux Landlock / macOS
    // Seatbelt); elsewhere this asserts the no-break invariant only.
    let root = temp_test_dir("sandbox-confine");

    // A path under the workspace root is always writable under WorkspaceWrite.
    let inside = root.join("inside.txt");
    let out = execute_bash_async_with_timeout(
        "echo ok > inside.txt",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        SandboxProfile::WorkspaceWrite,
    )
    .await
    .expect("workspace write should run");
    assert!(
        out.contains("exit: 0"),
        "workspace write inside root: {out}"
    );
    assert!(inside.exists(), "expected inside-root write to succeed");

    // WorkspaceWrite must allow toolchain-style atomic moves inside writable roots.
    // On Landlock ABI v2+, cross-directory rename/link needs the REFER access.
    let rename_out = execute_bash_async_with_timeout(
        "mkdir -p a b && printf x > a/tmp && mv a/tmp b/out && cat b/out",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        SandboxProfile::WorkspaceWrite,
    )
    .await
    .expect("workspace-write cross-directory rename should run");
    assert!(rename_out.contains("exit: 0"), "{rename_out}");
    assert!(rename_out.contains("x"), "{rename_out}");

    if crate::sandbox::is_enforced() {
        // Target a session-shaped path outside the child workspace and scratch
        // roots, plus a standard toolchain cache. The first fixture lives in
        // the checkout so this control remains writable even when the test
        // process itself inherited an outer Landlock sandbox.
        let external_paths = {
            let _guard = env_lock();
            std::env::var("HOME")
                .or_else(|_| std::env::var("USERPROFILE"))
                .ok()
                .and_then(|home| {
                    let home = std::path::PathBuf::from(home);
                    let checkout = std::env::current_dir().ok()?;
                    let escape_root =
                        checkout.join(format!(".dext-read-autonomy-test-{}", std::process::id()));
                    let escape_dir = escape_root.join("projects/stocks-test/sessions");
                    let cache_dir = home
                        .join(".cache/pip")
                        .join(format!("dext-sbx-test-{}", std::process::id()));
                    let _ = std::fs::create_dir_all(&escape_dir);
                    let _ = std::fs::create_dir_all(&cache_dir);
                    Some((escape_root, escape_dir, cache_dir))
                })
        };
        if let Some((escape_root, escape_dir, cache_dir)) = external_paths {
            let _escape_cleanup = RemoveDirOnDrop(escape_root);
            let _cache_cleanup = RemoveDirOnDrop(cache_dir.clone());
            let escape_file = escape_dir.join("escape.txt");
            let escape_cmd = format!(
                "echo pwned > {} 2>&1",
                shell_single_quote(&escape_file.to_string_lossy())
            );

            // Control: the write succeeds unsandboxed, proving the path is
            // genuinely writable and only the sandbox affects it.
            let _ = std::fs::remove_file(&escape_file);
            let _ = execute_bash_async_with_timeout(
                &escape_cmd,
                &root,
                Arc::new(AtomicBool::new(false)),
                std::time::Duration::from_secs(5),
                SandboxProfile::DangerFullAccess,
            )
            .await
            .expect("unsandboxed write should run");
            assert!(
                escape_file.exists(),
                "control: external path must be writable without an additional sandbox"
            );

            // Control: the file is readable unsandboxed.
            let readable = execute_bash_async_with_timeout(
                &format!("cat {}", shell_single_quote(&escape_file.to_string_lossy())),
                &root,
                Arc::new(AtomicBool::new(false)),
                std::time::Duration::from_secs(5),
                SandboxProfile::DangerFullAccess,
            )
            .await
            .expect("unsandboxed HOME read should run");
            assert!(readable.contains("pwned"), "{readable}");

            // Native reads and every confined subprocess profile must be able to
            // inspect exact files and enumerate user-readable directories outside
            // the child workspace. This also mirrors DEXT_HOME session review.
            let native_read = execute_tool(
                "read_file",
                &json!({"path": escape_file.display().to_string()}),
                &root,
            )
            .expect("native read_file may inspect HOME session path");
            assert!(native_read.contains("pwned"), "{native_read}");

            for profile in [SandboxProfile::ReadOnly, SandboxProfile::WorkspaceWrite] {
                let shell_read = execute_bash_async_with_timeout(
                    &format!(
                        "ls -1 {} && cat {}",
                        shell_single_quote(&escape_dir.to_string_lossy()),
                        shell_single_quote(&escape_file.to_string_lossy())
                    ),
                    &root,
                    Arc::new(AtomicBool::new(false)),
                    std::time::Duration::from_secs(5),
                    profile,
                )
                .await
                .expect("sandboxed external read command should return output");
                assert!(
                    shell_read.contains("escape.txt"),
                    "{profile:?}: {shell_read}"
                );
                assert!(shell_read.contains("pwned"), "{profile:?}: {shell_read}");

                let fd_read = execute_builtin_call(
                    "fd".to_string(),
                    json!({
                        "pattern": "escape\\.txt$",
                        "path": escape_dir.display().to_string(),
                        "extra_args": ["-H"]
                    }),
                    root.clone(),
                    Arc::new(AtomicBool::new(false)),
                    None,
                    None,
                    None,
                    None,
                    None,
                    profile,
                    false,
                    None,
                    Vec::new(),
                )
                .await
                .expect("fd may enumerate HOME session path");
                assert!(fd_read.contains("escape.txt"), "{profile:?}: {fd_read}");

                let rg_read = execute_builtin_call(
                    "rg".to_string(),
                    json!({
                        "pattern": "pwned",
                        "path": escape_dir.display().to_string()
                    }),
                    root.clone(),
                    Arc::new(AtomicBool::new(false)),
                    None,
                    None,
                    None,
                    None,
                    None,
                    profile,
                    false,
                    None,
                    Vec::new(),
                )
                .await
                .expect("rg may search HOME session path");
                assert!(rg_read.contains("pwned"), "{profile:?}: {rg_read}");
            }

            // Read-only denies the external write.
            let _ = std::fs::remove_file(&escape_file);
            let blocked = execute_bash_async_with_timeout(
                &escape_cmd,
                &root,
                Arc::new(AtomicBool::new(false)),
                std::time::Duration::from_secs(5),
                SandboxProfile::ReadOnly,
            )
            .await
            .expect("command runs even when its write is denied");
            assert!(
                !escape_file.exists(),
                "read-only sandbox must block writes outside scratch roots: {blocked}"
            );

            // Workspace-write also denies unrelated external writes, preventing
            // persistence through shell startup files and similar paths.
            let _ = std::fs::remove_file(&escape_file);
            let blocked = execute_bash_async_with_timeout(
                &escape_cmd,
                &root,
                Arc::new(AtomicBool::new(false)),
                std::time::Duration::from_secs(5),
                SandboxProfile::WorkspaceWrite,
            )
            .await
            .expect("workspace-write command runs even when HOME write is denied");
            assert!(
                !escape_file.exists(),
                "workspace-write sandbox must block unrelated external writes: {blocked}"
            );

            // Standard cache roots remain writable for cargo/npm/pip/etc.
            let cache_file = cache_dir.join("cache.txt");
            let cache_cmd = format!(
                "echo cached > {} 2>&1",
                shell_single_quote(&cache_file.to_string_lossy())
            );
            let cache_out = execute_bash_async_with_timeout(
                &cache_cmd,
                &root,
                Arc::new(AtomicBool::new(false)),
                std::time::Duration::from_secs(5),
                SandboxProfile::WorkspaceWrite,
            )
            .await
            .expect("workspace-write cache write should run");
            assert!(
                cache_file.exists(),
                "workspace-write must allow toolchain cache writes: {cache_out}"
            );
        }

        // Arbitrary shared-temp writes are not scratch writes: they would let a
        // read-only tool mutate another process's files or a temp-based workspace.
        let shared_temp_target = PathBuf::from("/tmp").join(format!(
            "dext-sbx-shared-temp-{}-{}.txt",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_nanos()
        ));
        let _ = std::fs::remove_file(&shared_temp_target);
        let blocked = execute_bash_async_with_timeout(
            &format!(
                "echo unsafe > {} 2>&1 || true",
                shell_single_quote(&shared_temp_target.to_string_lossy())
            ),
            &root,
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(5),
            SandboxProfile::ReadOnly,
        )
        .await
        .expect("read-only shared-temp write command should run");
        assert!(
            !shared_temp_target.exists(),
            "read-only sandbox must block arbitrary shared-temp writes: {blocked}"
        );

        // Normal temp APIs still work in one private directory. The command
        // wrapper owns that directory, so it must be gone after the runner returns.
        let private_temp = execute_bash_async_with_timeout(
            "test \"$TMPDIR\" = \"$TMP\" && test \"$TMPDIR\" = \"$TEMP\" && \
             file=$(mktemp \"$TMPDIR/dext.XXXXXX\") && printf data > \"$file\" && test -f \"$file\" && \
             printf 'PRIVATE_TEMP=%s\\n' \"$TMPDIR\"",
            &root,
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(5),
            SandboxProfile::ReadOnly,
        )
        .await
        .expect("read-only private scratch command should run");
        assert!(private_temp.contains("exit: 0"), "{private_temp}");
        let scratch = private_temp
            .lines()
            .find_map(|line| line.strip_prefix("PRIVATE_TEMP="))
            .map(PathBuf::from)
            .expect("private scratch path in command output");
        assert!(
            !scratch.exists(),
            "runner must remove private scratch after child exit: {}",
            scratch.display()
        );
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(target_os = "linux")]
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn sandbox_blocks_parent_environment_and_untrusted_cache_overrides() {
    if !crate::sandbox::is_enforced() {
        return;
    }
    let _guard = env_lock();
    let old_secret = std::env::var_os("SANDBOX_PARENT_ONLY_SECRET");
    let old_pip_cache = std::env::var_os("PIP_CACHE_DIR");
    let old_tmpdir = std::env::var_os("TMPDIR");
    let root = temp_test_dir("sandbox-parent-env");
    let checkout = std::env::current_dir().expect("current checkout");
    let escape_dir = checkout.join(format!(".dext-sbx-env-test-{}", std::process::id()));
    let escape_file = escape_dir.join("escape.txt");
    std::fs::create_dir_all(&escape_dir).expect("create escape directory");
    let _escape_cleanup = RemoveDirOnDrop(escape_dir.clone());
    unsafe {
        std::env::set_var("SANDBOX_PARENT_ONLY_SECRET", "parent-secret-fixture");
        std::env::set_var("PIP_CACHE_DIR", &escape_dir);
        std::env::set_var("TMPDIR", &escape_dir);
    }

    let proc_output = execute_bash_async_with_timeout(
        "tr '\\0' '\\n' < /proc/$PPID/environ 2>/dev/null || true",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        SandboxProfile::ReadOnly,
    )
    .await
    .expect("sandboxed proc read command runs");
    assert!(
        !proc_output.contains("parent-secret-fixture"),
        "sandbox exposed parent environment: {proc_output}"
    );

    let write_output = execute_bash_async_with_timeout(
        &format!(
            "printf escaped > {} 2>&1 || true",
            shell_single_quote(&escape_file.to_string_lossy())
        ),
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        SandboxProfile::WorkspaceWrite,
    )
    .await
    .expect("sandboxed override write command runs");
    assert!(
        !escape_file.exists(),
        "PIP_CACHE_DIR/TMPDIR must not widen writable roots: {write_output}"
    );

    restore_env_var("SANDBOX_PARENT_ONLY_SECRET", old_secret);
    restore_env_var("PIP_CACHE_DIR", old_pip_cache);
    restore_env_var("TMPDIR", old_tmpdir);
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
        SandboxProfile::WorkspaceWrite,
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
        SandboxProfile::WorkspaceWrite,
    )
    .await
    .expect_err("expected timeout");
    assert!(err.contains("timed out after"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
async fn bash_runner_emits_live_output_deltas() {
    let root = temp_test_dir("bash-live-output");
    let (tx, mut rx) = tokio::sync::mpsc::channel(8);
    let live = LiveToolOutput {
        call_id: "call_live".to_string(),
        name: "bash".to_string(),
        tx,
    };
    let out = execute_bash_async_prepared(
        "printf 'out\\n'; printf 'err\\n' >&2",
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(5),
        None,
        SandboxProfile::WorkspaceWrite,
        Some(live),
        &[],
        &[],
    )
    .await
    .expect("expected success");
    assert!(out.contains("exit: 0"), "{out}");

    let mut stdout = String::new();
    let mut stderr = String::new();
    while let Ok(event) = rx.try_recv() {
        match event {
            AgentEvent::ToolOutputDelta {
                call_id,
                name,
                stream,
                text,
            } => {
                assert_eq!(call_id, "call_live");
                assert_eq!(name, "bash");
                match stream.as_str() {
                    "stdout" => stdout.push_str(&text),
                    "stderr" => stderr.push_str(&text),
                    other => panic!("unexpected stream {other}"),
                }
            }
            _ => panic!("unexpected event"),
        }
    }
    assert!(stdout.contains("out"), "stdout={stdout:?}");
    assert!(stderr.contains("err"), "stderr={stderr:?}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn provider_transport_timeouts_use_local_defaults_and_valid_overrides() {
    let _guard = env_lock();
    let vars = [
        "DEXT_PROVIDER_CONNECT_TIMEOUT_SECS",
        "DEXT_PROVIDER_FIRST_BYTE_TIMEOUT_SECS",
        "DEXT_PROVIDER_STREAM_IDLE_TIMEOUT_SECS",
    ];
    let old_values = vars.map(std::env::var_os);
    unsafe {
        for var in vars {
            std::env::remove_var(var);
        }
    }

    assert_eq!(
        provider_connect_timeout(),
        std::time::Duration::from_secs(15)
    );
    assert_eq!(
        provider_first_byte_timeout(false),
        std::time::Duration::from_secs(180)
    );
    assert_eq!(
        provider_first_byte_timeout(true),
        std::time::Duration::from_secs(600)
    );
    assert_eq!(
        provider_stream_idle_timeout(false),
        std::time::Duration::from_secs(90)
    );
    assert_eq!(
        provider_stream_idle_timeout(true),
        std::time::Duration::from_secs(300)
    );

    unsafe {
        std::env::set_var(vars[0], "7");
        std::env::set_var(vars[1], "8");
        std::env::set_var(vars[2], "9");
    }
    assert_eq!(
        provider_connect_timeout(),
        std::time::Duration::from_secs(7)
    );
    assert_eq!(
        provider_first_byte_timeout(true),
        std::time::Duration::from_secs(8)
    );
    assert_eq!(
        provider_stream_idle_timeout(true),
        std::time::Duration::from_secs(9)
    );

    unsafe {
        std::env::set_var(vars[0], "0");
        std::env::set_var(vars[1], "invalid");
        std::env::set_var(vars[2], "0");
    }
    assert_eq!(
        provider_connect_timeout(),
        std::time::Duration::from_secs(15)
    );
    assert_eq!(
        provider_first_byte_timeout(false),
        std::time::Duration::from_secs(180)
    );
    assert_eq!(
        provider_stream_idle_timeout(false),
        std::time::Duration::from_secs(90)
    );

    for (var, old_value) in vars.into_iter().zip(old_values) {
        restore_env_var(var, old_value);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn provider_body_reader_enforces_idle_timeout_after_headers() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("server address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = std::io::Read::read(&mut stream, &mut buffer).expect("read request");
            assert!(read > 0, "client closed before sending request headers");
            request.extend_from_slice(&buffer[..read]);
        }
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\na";
        std::io::Write::write_all(&mut stream, response).expect("write partial response");
        std::thread::sleep(std::time::Duration::from_millis(150));
    });

    let response = reqwest::Client::new()
        .get(format!("http://{addr}"))
        .send()
        .await
        .expect("receive response headers");
    let error = read_provider_body_limited(response, 16, std::time::Duration::from_millis(30))
        .await
        .expect_err("stalled body should time out");
    assert!(
        error.to_string().contains("response body idle timeout"),
        "{error:#}"
    );
    server.join().expect("server thread");
}

#[tokio::test(flavor = "current_thread")]
async fn provider_body_reader_stops_at_cap_without_buffering_the_rest() {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("server address");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        let mut request = Vec::new();
        let mut buffer = [0u8; 1024];
        while !request.windows(4).any(|bytes| bytes == b"\r\n\r\n") {
            let read = std::io::Read::read(&mut stream, &mut buffer).expect("read request");
            assert!(read > 0, "client closed before sending request headers");
            request.extend_from_slice(&buffer[..read]);
        }
        let response = b"HTTP/1.1 200 OK\r\nContent-Length: 8\r\nConnection: close\r\n\r\nabcdefgh";
        std::io::Write::write_all(&mut stream, response).expect("write response");
    });

    let response = reqwest::Client::new()
        .get(format!("http://{addr}"))
        .send()
        .await
        .expect("receive response headers");
    let (body, truncated) =
        read_provider_body_limited(response, 4, std::time::Duration::from_secs(1))
            .await
            .expect("read capped body");
    assert_eq!(body, b"abcd");
    assert!(truncated);
    server.join().expect("server thread");
}

#[test]
fn sync_runner_times_out() {
    let root = temp_test_dir("sync-timeout");
    let mut cmd = Command::new(bash_executable_path());
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

#[cfg(unix)]
fn descendant_pid(output: &str) -> libc::pid_t {
    output
        .lines()
        .find_map(|line| line.strip_prefix("DESCENDANT_PID="))
        .and_then(|pid| pid.parse().ok())
        .unwrap_or_else(|| panic!("missing descendant PID in runner output: {output}"))
}

#[cfg(unix)]
fn assert_process_dead(pid: libc::pid_t, label: &str) {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let status = unsafe { libc::kill(pid, 0) };
        if status == -1 && std::io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
            return;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "{label} descendant PID {pid} survived process-group cleanup"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(unix)]
#[tokio::test]
async fn bash_process_lifecycle_matrix_reaps_background_nohup_and_disown_descendants() {
    let root = temp_test_dir("bash-descendant-lifecycle");
    let launchers = [
        ("background", "sleep 30 >/dev/null 2>&1 & child=$!"),
        (
            "nohup",
            "nohup sleep 30 </dev/null >/dev/null 2>&1 & child=$!",
        ),
        (
            "disown",
            "sleep 30 </dev/null >/dev/null 2>&1 & child=$!; disown \"$child\"",
        ),
    ];

    for (label, launch) in launchers {
        let completion = execute_bash_async_with_timeout(
            &format!("{launch}; printf 'DESCENDANT_PID=%s\\n' \"$child\""),
            &root,
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_secs(5),
            SandboxProfile::WorkspaceWrite,
        )
        .await
        .unwrap_or_else(|error| panic!("{label} completion case failed: {error}"));
        assert_process_dead(descendant_pid(&completion), &format!("{label} completion"));

        let timeout = execute_bash_async_with_timeout(
            &format!("{launch}; printf 'DESCENDANT_PID=%s\\n' \"$child\"; sleep 30"),
            &root,
            Arc::new(AtomicBool::new(false)),
            std::time::Duration::from_millis(150),
            SandboxProfile::WorkspaceWrite,
        )
        .await
        .expect_err("lifecycle timeout case must time out");
        assert!(timeout.contains("timed out after"), "{label}: {timeout}");
        assert_process_dead(descendant_pid(&timeout), &format!("{label} timeout"));

        let interrupt = Arc::new(AtomicBool::new(false));
        let trigger = interrupt.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(150)).await;
            trigger.store(true, Ordering::SeqCst);
        });
        let interrupted = execute_bash_async_with_timeout(
            &format!("{launch}; printf 'DESCENDANT_PID=%s\\n' \"$child\"; sleep 30"),
            &root,
            interrupt,
            std::time::Duration::from_secs(5),
            SandboxProfile::WorkspaceWrite,
        )
        .await
        .expect_err("lifecycle interrupt case must be interrupted");
        assert!(
            interrupted.contains("killed by interrupt"),
            "{label}: {interrupted}"
        );
        assert_process_dead(descendant_pid(&interrupted), &format!("{label} interrupt"));
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[cfg(windows)]
fn windows_descendant_pid(output: &str) -> u32 {
    output
        .lines()
        .find_map(|line| line.strip_prefix("DESCENDANT_PID="))
        .and_then(|pid| pid.trim().parse().ok())
        .unwrap_or_else(|| panic!("missing descendant PID in runner output: {output}"))
}

#[cfg(windows)]
fn assert_windows_process_dead(pid: u32, label: &str) {
    use windows_sys::Win32::{
        Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT},
        System::Threading::{OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject},
    };

    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
    loop {
        let process = unsafe { OpenProcess(PROCESS_SYNCHRONIZE, 0, pid) };
        if process.is_null() {
            return;
        }
        let status = unsafe { WaitForSingleObject(process, 0) };
        unsafe {
            CloseHandle(process);
        }
        if status == WAIT_OBJECT_0 {
            return;
        }
        assert_eq!(status, WAIT_TIMEOUT, "wait failed for {label} PID {pid}");
        assert!(
            std::time::Instant::now() < deadline,
            "{label} descendant PID {pid} survived Job Object cleanup"
        );
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
}

#[cfg(windows)]
#[tokio::test]
async fn bash_process_lifecycle_reaps_windows_descendants() {
    let root = temp_test_dir("bash-windows-descendant-lifecycle");
    let launch = "powershell.exe -NoProfile -NonInteractive -Command '$p = Start-Process powershell.exe -ArgumentList \"-NoProfile\",\"-NonInteractive\",\"-Command\",\"Start-Sleep -Seconds 30\" -PassThru; Write-Output (\"DESCENDANT_PID=\" + $p.Id)'";

    let completion = execute_bash_async_with_timeout(
        launch,
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(10),
        SandboxProfile::WorkspaceWrite,
    )
    .await
    .expect("Windows lifecycle completion case failed");
    assert_windows_process_dead(windows_descendant_pid(&completion), "Windows completion");

    let timeout = execute_bash_async_with_timeout(
        &format!("{launch}; sleep 30"),
        &root,
        Arc::new(AtomicBool::new(false)),
        std::time::Duration::from_secs(3),
        SandboxProfile::WorkspaceWrite,
    )
    .await
    .expect_err("Windows lifecycle timeout case must time out");
    assert!(timeout.contains("timed out after"), "{timeout}");
    assert_windows_process_dead(windows_descendant_pid(&timeout), "Windows timeout");

    let interrupt = Arc::new(AtomicBool::new(false));
    let trigger = interrupt.clone();
    tokio::spawn(async move {
        tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        trigger.store(true, Ordering::SeqCst);
    });
    let interrupted = execute_bash_async_with_timeout(
        &format!("{launch}; sleep 30"),
        &root,
        interrupt,
        std::time::Duration::from_secs(10),
        SandboxProfile::WorkspaceWrite,
    )
    .await
    .expect_err("Windows lifecycle interrupt case must be interrupted");
    assert!(interrupted.contains("killed by interrupt"), "{interrupted}");
    assert_windows_process_dead(windows_descendant_pid(&interrupted), "Windows interrupt");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn sync_runner_reaps_background_children_after_shell_exit() {
    let root = temp_test_dir("sync-grandchild-reap");
    let mut cmd = Command::new(bash_executable_path());
    cmd.arg("-lc").arg("sleep 5 & echo done").current_dir(&root);
    let start = std::time::Instant::now();
    let (out, _, status) = run_sync_command_limited(
        cmd,
        None,
        PROCESS_STREAM_CAPTURE_CAP,
        "bash",
        std::time::Duration::from_secs(10),
    )
    .expect("expected success");
    let elapsed = start.elapsed();
    assert_eq!(status, 0);
    assert!(out.render("stdout").contains("done"));
    assert!(
        elapsed < std::time::Duration::from_secs(3),
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
        None,
        None,
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
        None,
        None,
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
fn http_status_labels_and_errors_omit_reqwest_unknown_status_noise() {
    assert_eq!(
        http_status_label(reqwest::StatusCode::NOT_FOUND),
        "404 Not Found"
    );
    let status_520 = reqwest::StatusCode::from_u16(520).expect("status 520");
    assert_eq!(http_status_label(status_520), "520");
    assert_eq!(http_status_error(status_520, ""), "HTTP 520");
    assert_eq!(http_status_error(status_520, "error code: 520"), "HTTP 520");
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

#[test]
fn http_tool_blocks_local_and_internal_destinations_by_default() {
    let _guard = env_lock();
    let old_link_local = std::env::var_os(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV);
    let old_loopback = std::env::var_os(HTTP_TOOL_ALLOW_LOOPBACK_ENV);
    let old_private = std::env::var_os(HTTP_TOOL_ALLOW_PRIVATE_ENV);
    unsafe {
        std::env::remove_var(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV);
        std::env::remove_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV);
        std::env::remove_var(HTTP_TOOL_ALLOW_PRIVATE_ENV);
    }

    let blocked = [
        ("http://169.254.169.254/latest/meta-data/", "link-local"),
        ("http://[fd00:ec2::254]/latest/meta-data/", "metadata"),
        ("http://[::ffff:169.254.169.254]/", "IPv4-embedded"),
        (
            "http://metadata.google.internal/computeMetadata/v1/",
            "metadata alias",
        ),
        ("http://127.0.0.1:1/", "loopback"),
        ("http://0.0.0.0:1/", "unspecified"),
        ("http://[::1]:1/", "loopback"),
        ("http://[::]:1/", "unspecified"),
        ("http://10.0.0.1/", "private"),
        ("http://172.16.0.1/", "private"),
        ("http://192.168.0.1/", "private"),
        ("http://100.64.0.1/", "CGNAT"),
        ("http://[fd12::1]/", "unique-local"),
    ];
    for (target, reason) in blocked {
        let url = reqwest::Url::parse(target).unwrap();
        let err = validate_http_tool_destination(&url).expect_err(target);
        assert!(err.contains(reason), "{target}: {err}");
    }

    unsafe {
        std::env::set_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, "1");
        std::env::set_var(HTTP_TOOL_ALLOW_PRIVATE_ENV, "1");
        std::env::set_var(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV, "1");
    }
    for target in [
        "http://127.0.0.1:1/",
        "http://0.0.0.0:1/",
        "http://10.0.0.1/",
        "http://100.64.0.1/",
        "http://169.254.169.254/latest/meta-data/",
        "http://metadata.google.internal/computeMetadata/v1/",
    ] {
        let url = reqwest::Url::parse(target).unwrap();
        assert!(validate_http_tool_destination(&url).is_ok(), "{target}");
    }

    restore_env_var(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV, old_link_local);
    restore_env_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, old_loopback);
    restore_env_var(HTTP_TOOL_ALLOW_PRIVATE_ENV, old_private);
}

#[test]
fn http_tool_network_overrides_are_independent() {
    let _guard = env_lock();
    let old_link_local = std::env::var_os(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV);
    let old_loopback = std::env::var_os(HTTP_TOOL_ALLOW_LOOPBACK_ENV);
    let old_private = std::env::var_os(HTTP_TOOL_ALLOW_PRIVATE_ENV);

    let cases = [
        (
            HTTP_TOOL_ALLOW_LOOPBACK_ENV,
            "http://127.0.0.1:1/",
            ["http://10.0.0.1/", "http://169.254.169.254/"],
        ),
        (
            HTTP_TOOL_ALLOW_PRIVATE_ENV,
            "http://10.0.0.1/",
            ["http://127.0.0.1:1/", "http://169.254.169.254/"],
        ),
        (
            HTTP_TOOL_ALLOW_LINK_LOCAL_ENV,
            "http://169.254.169.254/",
            ["http://127.0.0.1:1/", "http://10.0.0.1/"],
        ),
    ];

    for (enabled, allowed, blocked) in cases {
        unsafe {
            std::env::remove_var(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV);
            std::env::remove_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV);
            std::env::remove_var(HTTP_TOOL_ALLOW_PRIVATE_ENV);
            std::env::set_var(enabled, "1");
        }

        let allowed = reqwest::Url::parse(allowed).unwrap();
        assert!(
            validate_http_tool_destination(&allowed).is_ok(),
            "{enabled} should allow {allowed}"
        );
        for target in blocked {
            let target = reqwest::Url::parse(target).unwrap();
            assert!(
                validate_http_tool_destination(&target).is_err(),
                "{enabled} must not allow {target}"
            );
        }
    }

    restore_env_var(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV, old_link_local);
    restore_env_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, old_loopback);
    restore_env_var(HTTP_TOOL_ALLOW_PRIVATE_ENV, old_private);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn builtin_http_client_ignores_proxy_environment() {
    let _guard = env_lock();
    let env_names = [
        "HTTP_PROXY",
        "http_proxy",
        "HTTPS_PROXY",
        "https_proxy",
        "ALL_PROXY",
        "all_proxy",
        "NO_PROXY",
        "no_proxy",
        HTTP_TOOL_ALLOW_LOOPBACK_ENV,
    ];
    let old_env = env_names.map(|name| (name, std::env::var_os(name)));

    let direct = TcpListener::bind("127.0.0.1:0").expect("bind direct test server");
    direct.set_nonblocking(true).expect("nonblocking direct");
    let direct_port = direct.local_addr().expect("direct addr").port();
    let proxy = TcpListener::bind("127.0.0.1:0").expect("bind proxy trap");
    proxy.set_nonblocking(true).expect("nonblocking proxy");
    let proxy_url = format!("http://{}", proxy.local_addr().expect("proxy addr"));
    unsafe {
        for name in ["HTTP_PROXY", "http_proxy", "ALL_PROXY", "all_proxy"] {
            std::env::set_var(name, &proxy_url);
        }
        for name in ["HTTPS_PROXY", "https_proxy", "NO_PROXY", "no_proxy"] {
            std::env::remove_var(name);
        }
        std::env::set_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, "1");
    }

    let stop = Arc::new(AtomicBool::new(false));
    let spawn_server = |listener: TcpListener, body: &'static str, stop: Arc<AtomicBool>| {
        std::thread::spawn(move || {
            while !stop.load(Ordering::SeqCst) {
                match listener.accept() {
                    Ok((mut stream, _)) => {
                        let mut request = [0u8; 1024];
                        let _ = std::io::Read::read(&mut stream, &mut request);
                        let response = format!(
                            "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                            body.len()
                        );
                        stream
                            .write_all(response.as_bytes())
                            .expect("write response");
                        return true;
                    }
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                    }
                    Err(error) => panic!("test listener failed: {error}"),
                }
            }
            false
        })
    };
    let direct_server = spawn_server(direct, "direct", stop.clone());
    let proxy_server = spawn_server(proxy, "proxied", stop.clone());

    let response = build_http_tool_client()
        .get(format!("http://127.0.0.1:{direct_port}/"))
        .timeout(std::time::Duration::from_secs(3))
        .send()
        .await
        .expect("direct HTTP request despite proxy environment")
        .text()
        .await
        .expect("read direct response");
    stop.store(true, Ordering::SeqCst);
    let direct_used = direct_server.join().expect("direct server thread");
    let proxy_used = proxy_server.join().expect("proxy server thread");

    for (name, value) in old_env {
        restore_env_var(name, value);
    }
    assert_eq!(response, "direct");
    assert!(direct_used, "direct listener should receive the request");
    assert!(!proxy_used, "proxy environment must be ignored");
}

#[test]
fn builtin_http_tool_enforces_exact_redirect_limit() {
    let _guard = env_lock();
    let old_loopback = std::env::var_os(HTTP_TOOL_ALLOW_LOOPBACK_ENV);
    unsafe { std::env::set_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, "1") };

    let run_chain = |redirects: usize| {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind redirect test listener");
        listener
            .set_nonblocking(true)
            .expect("set redirect listener nonblocking");
        let addr = listener.local_addr().expect("redirect listener addr");
        let expected_requests = redirects.min(HTTP_TOOL_REDIRECT_LIMIT) + 1;
        let server = std::thread::spawn(move || {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            let mut handled = 0;
            while handled < expected_requests && std::time::Instant::now() < deadline {
                let (mut stream, _) = match listener.accept() {
                    Ok(connection) => connection,
                    Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                        std::thread::sleep(std::time::Duration::from_millis(10));
                        continue;
                    }
                    Err(error) => panic!("redirect listener failed: {error}"),
                };
                stream
                    .set_nonblocking(false)
                    .expect("set redirect stream blocking");
                stream
                    .set_read_timeout(Some(std::time::Duration::from_secs(2)))
                    .expect("set redirect request timeout");
                let mut request = Vec::new();
                let mut buf = [0u8; 1024];
                while !request.windows(4).any(|window| window == b"\r\n\r\n") {
                    let read = stream.read(&mut buf).expect("read redirect request");
                    assert!(read > 0, "client closed before sending redirect headers");
                    request.extend_from_slice(&buf[..read]);
                }

                let response = if handled < redirects {
                    format!(
                        "HTTP/1.1 302 Found\r\nLocation: http://{addr}/hop/{}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                        handled + 1
                    )
                } else {
                    "HTTP/1.1 200 OK\r\nContent-Length: 4\r\nConnection: close\r\n\r\ndone"
                        .to_string()
                };
                stream
                    .write_all(response.as_bytes())
                    .expect("write redirect response");
                handled += 1;
            }
            handled
        });

        let result = tokio::runtime::Runtime::new()
            .expect("runtime")
            .block_on(async {
                build_http_tool_client()
                    .get(format!("http://{addr}/hop/0"))
                    .timeout(std::time::Duration::from_secs(5))
                    .send()
                    .await?
                    .text()
                    .await
            });
        let handled = server.join().expect("redirect server thread");
        (result, handled)
    };

    let (allowed, allowed_requests) = run_chain(HTTP_TOOL_REDIRECT_LIMIT);
    assert_eq!(allowed.expect("redirect limit should be allowed"), "done");
    assert_eq!(allowed_requests, HTTP_TOOL_REDIRECT_LIMIT + 1);

    let (blocked, blocked_requests) = run_chain(HTTP_TOOL_REDIRECT_LIMIT + 1);
    let error = blocked.expect_err("redirect beyond limit should fail");
    assert!(error.is_redirect(), "{error:?}");
    assert!(
        format!("{error:?}").contains("too many redirects"),
        "{error:?}"
    );
    assert_eq!(blocked_requests, HTTP_TOOL_REDIRECT_LIMIT + 1);

    restore_env_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, old_loopback);
}

#[test]
fn builtin_http_tool_blocks_redirect_to_link_local_metadata() {
    let _guard = env_lock();
    let old_link_local = std::env::var_os(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV);
    let old_loopback = std::env::var_os(HTTP_TOOL_ALLOW_LOOPBACK_ENV);
    unsafe {
        std::env::remove_var(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV);
        std::env::set_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, "1");
    }

    let root = temp_test_dir("http-redirect-link-local");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test listener");
    let addr = listener.local_addr().expect("listener addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept request");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .expect("set read timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|w| w == b"\r\n\r\n") {
            let n = stream.read(&mut buf).expect("read request headers");
            assert!(n > 0, "client closed before sending headers");
            request.extend_from_slice(&buf[..n]);
        }
        stream
            .write_all(
                b"HTTP/1.1 302 Found\r\nLocation: http://169.254.169.254/latest/meta-data/\r\nContent-Length: 0\r\n\r\n",
            )
            .expect("write redirect");
    });

    let outcome = tokio::runtime::Runtime::new()
        .expect("runtime")
        .block_on(execute_builtin_call(
            "http".to_string(),
            json!({"args": [format!("http://{addr}/redirect")]}),
            root.clone(),
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            None,
            None,
            None,
            SandboxProfile::WorkspaceWrite,
            false,
            None,
            Vec::new(),
        ));

    restore_env_var(HTTP_TOOL_ALLOW_LINK_LOCAL_ENV, old_link_local);
    restore_env_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, old_loopback);
    let out = outcome.expect_err("redirect to link-local should be blocked");
    assert!(out.contains("blocked http redirect"), "{out}");
    assert!(out.contains("169.254.169.254"), "{out}");
    server.join().expect("server thread");
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test]
#[allow(clippy::await_holding_lock)]
async fn builtin_http_tool_executes_without_xh_dependency() {
    let _guard = env_lock();
    let old_loopback = std::env::var_os(HTTP_TOOL_ALLOW_LOOPBACK_ENV);
    unsafe { std::env::set_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, "1") };
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
        None,
        None,
        None,
        SandboxProfile::WorkspaceWrite,
        false,
        None,
        Vec::new(),
    )
    .await
    .expect("http request should succeed without xh installed");

    assert!(out.contains("{\"ok\":true}"), "{out}");
    server.join().expect("server thread");
    restore_env_var(HTTP_TOOL_ALLOW_LOOPBACK_ENV, old_loopback);
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
    assert_eq!(
        args,
        vec![
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "log",
            "--oneline",
            "-8",
        ]
    );

    let (_, args_no_oneline, _) =
        prepare_external_tool("git_log", &json!({"count": 5, "oneline": false}), &root)
            .expect("prepare git_log without oneline");
    assert_eq!(
        args_no_oneline,
        vec![
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "log",
            "-5",
        ]
    );

    let (_, args_with_path, _) = prepare_external_tool(
        "git_log",
        &json!({"count": 3, "oneline": true, "path": "src/main.rs"}),
        &root,
    )
    .expect("prepare git_log with path");
    assert_eq!(
        args_with_path,
        vec![
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "log",
            "--oneline",
            "-3",
            "--",
            "src/main.rs",
        ]
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
fn awk_tool_allows_inline_filters_and_rejects_process_or_write_capabilities() {
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

    for args in [
        json!(["-F", ",", "-v", "min=2", "$1 > min { print $1 }"]),
        json!(["$0 ~ /warn|error/ { print ($1 > 2) }"]),
        json!(["{ print \"a > b | c\" }"]),
        json!(["$1 > 2 || $2 < 4 { print $1 }"]),
    ] {
        prepare_external_tool("awk", &json!({"args": args}), &root)
            .unwrap_or_else(|error| panic!("safe inline awk rejected: {error}"));
    }

    for args in [
        json!(["-f", "program.awk", "data.csv"]),
        json!(["--load=ext", "{ print $1 }"]),
        json!(["{ system (\"touch pwned\") }"]),
        json!(["{ @fn(\"touch pwned\") }"]),
        json!(["{ print $1 | \"sh\" }"]),
        json!(["{ print $1 > \"out.txt\" }"]),
        json!(["{ \"cat /etc/passwd\" | getline line }"]),
        json!(["BEGIN { print \"x\" > \"/inet/tcp/0/host/80\" }"]),
    ] {
        let error = prepare_external_tool("awk", &json!({"args": args}), &root)
            .expect_err("capability-bearing awk must be blocked");
        assert!(error.contains("blocked awk args"), "{error}");
    }

    let error = prepare_external_tool("awk", &json!({"args": ["{print}", 7]}), &root)
        .expect_err("mixed-type awk argv must not be silently truncated");
    assert!(error.contains("only strings"), "{error}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn csvkit_tool_enforces_subcommand_allowlist_at_execution_boundary() {
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

    let error = prepare_external_tool(
        "csvkit",
        &json!({"subcommand": "sh", "args": ["-c", "touch pwned"]}),
        &root,
    )
    .expect_err("csvkit must not execute arbitrary PATH binaries");
    assert!(error.contains("unsupported csvkit subcommand"), "{error}");
    assert!(
        tool_policy::tool_input_issue(
            "csvkit",
            &json!({"subcommand": "/bin/rm", "args": ["victim"]})
        )
        .is_some(),
        "planner validation must enforce the same executable allowlist"
    );

    let error = prepare_external_tool(
        "csvkit",
        &json!({"subcommand": "csvcut", "args": ["-c", 1]}),
        &root,
    )
    .expect_err("mixed-type csvkit argv must not be silently truncated");
    assert!(error.contains("only strings"), "{error}");

    let error = prepare_external_tool("csvkit", &json!({"args": ["-c", "1"]}), &root)
        .expect_err("csvkit without subcommand should error");
    assert!(error.contains("subcommand"), "{error}");

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_tools_reject_subprocess_exec_flags() {
    let root = temp_test_dir("search-exec-flags");
    let root = std::fs::canonicalize(&root).unwrap();
    let cases = [
        ("fd", vec!["-x", "sh", "-c", "echo pwned"]),
        ("fd", vec!["-HIx", "touch", "pwned"]),
        ("fd", vec!["--exec-batch=touch", "pwned"]),
        ("rg", vec!["--pre", "sh -c 'echo pwned'"]),
        ("rg", vec!["--pre-glob=*.rs"]),
        ("rg", vec!["-z"]),
        ("rg", vec!["-iz"]),
        ("rg", vec!["--search-zip"]),
        ("rg", vec!["--hostname-bin", "sh"]),
    ];

    for (name, extra_args) in cases {
        let err = prepare_external_tool(
            name,
            &json!({
                "pattern": "needle",
                "path": root.to_str().unwrap(),
                "extra_args": extra_args
            }),
            &root,
        )
        .expect_err("exec-capable search flag must be rejected");
        assert!(err.contains("subprocess-execution"), "{name}: {err}");
    }

    for (name, extra_args) in [
        ("fd", vec!["../outside"]),
        ("fd", vec!["--"]),
        ("fd", vec!["--base-directory", "../outside"]),
        ("rg", vec!["../outside"]),
        ("rg", vec!["--files"]),
        ("rg", vec!["--regexp=alternate"]),
    ] {
        let err = prepare_external_tool(
            name,
            &json!({
                "pattern": "needle",
                "path": root.to_str().unwrap(),
                "extra_args": extra_args
            }),
            &root,
        )
        .expect_err("extra_args must not add or replace search operands");
        assert!(
            err.contains("search operands") || err.contains("positional"),
            "{name}: {err}"
        );
    }

    for (name, extra_args) in [
        ("fd", vec!["-H", "-t", "f", "--glob"]),
        ("rg", vec!["-i", "--glob", "*.rs", "--max-count", "3"]),
    ] {
        prepare_external_tool(
            name,
            &json!({
                "pattern": "needle",
                "path": root.to_str().unwrap(),
                "extra_args": extra_args
            }),
            &root,
        )
        .expect("ordinary search flags remain allowed");
    }

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn search_tool_patterns_are_separated_from_options() {
    let root = temp_test_dir("search-option-pattern");
    let root = std::fs::canonicalize(&root).unwrap();

    for name in ["fd", "rg"] {
        let (bin, args, _) = prepare_external_tool(
            name,
            &json!({"pattern": "--exec", "path": root.to_str().unwrap()}),
            &root,
        )
        .expect("option-shaped pattern must remain data");
        if matches!((name, bin.as_str()), ("fd", "fd") | ("rg", "rg")) {
            let pattern_idx = args.iter().position(|arg| arg == "--exec").unwrap();
            assert_eq!(args[pattern_idx - 1], "--", "{name}: {args:?}");
        }
        if name == "rg" && bin == "rg" {
            assert!(args.iter().any(|arg| arg == "--no-config"), "{args:?}");
        }
    }

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
    // Base flags disable config-file injection and preserve stable output.
    assert_eq!(
        &args[0..3],
        &["--no-config", "--line-number", "--no-heading"]
    );
    assert!(args.contains(&"-i".to_string()), "{args:?}");
    assert!(args.contains(&"--glob=*.rs".to_string()), "{args:?}");
    assert!(
        args.contains(&"!**/node_modules/**".to_string()),
        "{args:?}"
    );
    let pattern_idx = args.iter().position(|a| a == "fn main").expect("pattern");
    assert_eq!(
        args.get(pattern_idx.wrapping_sub(1)).map(String::as_str),
        Some("--")
    );
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
    )
    .expect("prepare rg fallback");
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
    assert_eq!(args[args.len() - 3], "--");
    assert_eq!(args[args.len() - 2], "needle");
    assert_eq!(args.last().map(String::as_str), root.to_str(), "{args:?}");

    #[cfg(unix)]
    {
        use std::os::unix::fs::symlink;

        let outside = temp_test_dir("rg-grep-fallback-dangling-target");
        let target = outside.join("missing.txt");
        symlink(&target, root.join("dangling-alias")).expect("create dangling fallback alias");
        let error = prepare_external_tool_fallback(
            "rg",
            &json!({"pattern": "needle", "path": "dangling-alias"}),
            &root,
        )
        .expect_err("fallback must not retarget an invalid path to the workspace root");
        assert!(
            error.contains("cannot resolve existing path component"),
            "{error}"
        );
        let _ = std::fs::remove_dir_all(&outside);
    }

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
    )
    .expect("prepare fd file fallback");
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
    )
    .expect("prepare fd directory fallback");
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
    assert_eq!(
        args,
        vec![
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--cached",
            "--",
            "src/main.rs",
        ]
    );

    let (_, args_commit, _) =
        prepare_external_tool("git_diff", &json!({"commit": "HEAD~1"}), &root)
            .expect("prepare git_diff commit");
    assert_eq!(
        args_commit,
        vec![
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "HEAD~1",
        ]
    );

    let (_, args_stat, _) = prepare_external_tool(
        "git_diff",
        &json!({"stat": true, "staged": true, "path": "src/main.rs"}),
        &root,
    )
    .expect("prepare git_diff stat");
    assert_eq!(
        args_stat,
        vec![
            "--no-pager",
            "--no-optional-locks",
            "--literal-pathspecs",
            "-c",
            "core.fsmonitor=false",
            "-c",
            "credential.helper=",
            "-c",
            "protocol.allow=never",
            "diff",
            "--no-ext-diff",
            "--no-textconv",
            "--stat",
            "--cached",
            "--",
            "src/main.rs",
        ]
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn git_tools_reject_option_injection_malformed_fields_and_outside_paths() {
    let root = temp_test_dir("git-tool-input-validation");
    let outside = root
        .parent()
        .expect("temp root parent")
        .join(format!("outside-{}", std::process::id()));
    std::fs::write(root.join("inside.txt"), "inside\n").expect("write inside fixture");
    std::fs::write(&outside, "outside\n").expect("write outside fixture");

    for revision in ["--ext-diff", "-O/tmp/order", "HEAD\n--output=leak"] {
        let error = prepare_external_tool("git_diff", &json!({"commit": revision}), &root)
            .expect_err("option-like or control-bearing revision must be rejected");
        assert!(error.contains("non-option"), "{revision:?}: {error}");
    }

    for input in [
        json!({"path": 7}),
        json!({"staged": "yes"}),
        json!({"stat": []}),
    ] {
        prepare_external_tool("git_diff", &input, &root)
            .expect_err("malformed git_diff field must fail closed");
    }
    for input in [
        json!({"path": false}),
        json!({"oneline": "yes"}),
        json!({"count": "10"}),
        json!({"count": -1}),
    ] {
        prepare_external_tool("git_log", &input, &root)
            .expect_err("malformed git_log field must fail closed");
    }

    for tool in ["git_diff", "git_log"] {
        let error = prepare_external_tool(tool, &json!({"path": outside.to_string_lossy()}), &root)
            .expect_err("outside Git path must be rejected");
        assert!(error.contains("outside"), "{tool}: {error}");
    }

    let (_, args, _) = prepare_external_tool("git_diff", &json!({"path": ":(top,glob)**"}), &root)
        .expect("pathspec magic must be passed as a literal filename");
    assert!(args.iter().any(|arg| arg == "--literal-pathspecs"));
    assert_eq!(args.last().map(String::as_str), Some(":(top,glob)**"));

    let _ = std::fs::remove_file(outside);
    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[test]
fn git_pathspec_preserves_final_symlink_and_rejects_symlinked_ancestors() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("git-tool-symlink-pathspec");
    std::fs::create_dir(root.join("real-dir")).expect("create real directory");
    std::fs::write(root.join("real-dir/file.txt"), "real\n").expect("write real file");
    std::fs::write(root.join("target.txt"), "target\n").expect("write target");
    symlink("target.txt", root.join("link.txt")).expect("create final symlink");
    symlink("real-dir", root.join("alias-dir")).expect("create directory symlink");

    assert_eq!(
        git_tool_pathspec(&root, "git_diff", "link.txt").expect("preserve final symlink"),
        "link.txt"
    );
    let (_, args, _) = prepare_external_tool("git_diff", &json!({"path": "link.txt"}), &root)
        .expect("prepare final-symlink diff");
    assert_eq!(args.last().map(String::as_str), Some("link.txt"));

    let error = git_tool_pathspec(&root, "git_commit", "alias-dir/file.txt")
        .expect_err("symlinked parent must fail closed");
    assert!(error.contains("symlinked ancestor"), "{error}");

    let _ = std::fs::remove_dir_all(root);
}

#[cfg(unix)]
#[tokio::test]
async fn git_commit_path_stages_the_tracked_symlink_not_its_referent() {
    use std::os::unix::fs::symlink;

    let root = temp_test_dir("git-commit-symlink-pathspec");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("target-a.txt"), "a\n").expect("write first target");
    std::fs::write(root.join("target-b.txt"), "b\n").expect("write second target");
    symlink("target-a.txt", root.join("link.txt")).expect("create tracked symlink");
    git_ok(&root, &["add", "target-a.txt", "target-b.txt", "link.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    std::fs::remove_file(root.join("link.txt")).expect("remove old symlink");
    symlink("target-b.txt", root.join("link.txt")).expect("retarget symlink");
    execute_git_commit_async(
        &json!({"message": "test: retarget link", "paths": ["link.txt"]}),
        &root,
        Arc::new(AtomicBool::new(false)),
        SandboxProfile::WorkspaceWrite,
        false,
    )
    .await
    .expect("commit tracked symlink change");

    assert_eq!(
        git_stdout(&root, &["show", "HEAD:link.txt"]),
        "target-b.txt"
    );
    assert_eq!(git_stdout(&root, &["status", "--porcelain"]), "");

    let _ = std::fs::remove_dir_all(root);
}

#[tokio::test]
async fn git_commit_rejects_malformed_path_arrays_before_staging() {
    let root = temp_test_dir("git-commit-path-array-validation");
    std::fs::write(root.join("tracked.txt"), "unchanged\n").expect("write fixture");

    for paths in [
        json!("tracked.txt"),
        json!([7]),
        json!(["tracked.txt", null]),
    ] {
        let error = execute_git_commit_async(
            &json!({"message": "test", "paths": paths}),
            &root,
            Arc::new(AtomicBool::new(false)),
            SandboxProfile::WorkspaceWrite,
            false,
        )
        .await
        .expect_err("malformed commit paths must fail before invoking Git");
        assert!(error.contains("paths must"), "{error}");
    }

    let _ = std::fs::remove_dir_all(root);
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "current_thread")]
async fn builtin_git_commit_disables_unapproved_hooks_filters_fsmonitor_and_signing() -> Result<()>
{
    use std::os::unix::fs::PermissionsExt as _;

    let root = temp_test_dir("builtin-git-commit-config-executors");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join(".gitattributes"), "*.txt filter=evil\n")?;
    std::fs::write(root.join("tracked.txt"), "base\n")?;
    git_ok(&root, &["add", ".gitattributes", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let marker = root.join("config-helper-ran");
    let helper = root.join("config-helper.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$*\" >> {}\ncat\nexit 97\n",
            shell_single_quote(&marker.to_string_lossy())
        ),
    )?;
    let mut permissions = std::fs::metadata(&helper)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions)?;
    let helper_text = helper.to_string_lossy().into_owned();
    git_ok(&root, &["config", "filter.evil.clean", &helper_text]);
    git_ok(&root, &["config", "filter.evil.smudge", &helper_text]);
    git_ok(&root, &["config", "filter.evil.process", &helper_text]);
    git_ok(&root, &["config", "filter.evil.required", "true"]);
    git_ok(&root, &["config", "core.fsmonitor", &helper_text]);
    git_ok(&root, &["config", "commit.gpgSign", "true"]);
    git_ok(&root, &["config", "gpg.program", &helper_text]);
    std::fs::write(
        root.join(".git/hooks/pre-commit"),
        format!(
            "#!/bin/sh\nprintf hook >> {}\nexit 97\n",
            shell_single_quote(&marker.to_string_lossy())
        ),
    )?;
    let mut hook_permissions = std::fs::metadata(root.join(".git/hooks/pre-commit"))?.permissions();
    hook_permissions.set_mode(0o700);
    std::fs::set_permissions(root.join(".git/hooks/pre-commit"), hook_permissions)?;

    std::fs::write(root.join("tracked.txt"), "changed\n")?;
    execute_git_commit_async(
        &json!({"message": "test: helpers disabled", "paths": ["tracked.txt"]}),
        &root,
        Arc::new(AtomicBool::new(false)),
        SandboxProfile::WorkspaceWrite,
        false,
    )
    .await
    .map_err(anyhow::Error::msg)?;

    anyhow::ensure!(!marker.exists(), "repository-configured executor ran");
    anyhow::ensure!(
        git_stdout(&root, &["show", "HEAD:tracked.txt"]) == "changed",
        "tracked content was not committed"
    );
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn builtin_git_commit_disables_user_global_clean_filters() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let root = temp_test_dir("builtin-git-commit-global-filter");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join(".gitattributes"), "*.txt filter=global-evil\n")?;
    std::fs::write(root.join("tracked.txt"), "base\n")?;
    git_ok(&root, &["add", ".gitattributes", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let marker = root.join("global-filter-ran");
    let helper = root.join("global-filter.sh");
    std::fs::write(
        &helper,
        format!(
            "#!/bin/sh\nprintf helper > {}\ncat\nexit 97\n",
            shell_single_quote(&marker.to_string_lossy())
        ),
    )?;
    let mut permissions = std::fs::metadata(&helper)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions)?;

    let home = root.join("test-home");
    std::fs::create_dir(&home)?;
    let global_config = home.join(".gitconfig");
    let global_config_text = global_config.to_string_lossy().into_owned();
    let helper_text = helper.to_string_lossy().into_owned();
    git_ok(
        &root,
        &[
            "config",
            "--file",
            &global_config_text,
            "filter.global-evil.clean",
            &helper_text,
        ],
    );
    git_ok(
        &root,
        &[
            "config",
            "--file",
            &global_config_text,
            "filter.global-evil.required",
            "true",
        ],
    );
    std::fs::write(root.join("tracked.txt"), "changed\n")?;

    let old_home = std::env::var_os("HOME");
    unsafe {
        std::env::set_var("HOME", &home);
    }
    let result = execute_git_commit_async(
        &json!({"message": "test: global filter disabled", "paths": ["tracked.txt"]}),
        &root,
        Arc::new(AtomicBool::new(false)),
        SandboxProfile::WorkspaceWrite,
        false,
    )
    .await;
    restore_env_var("HOME", old_home);

    result.map_err(anyhow::Error::msg)?;
    anyhow::ensure!(!marker.exists(), "user-global clean filter executed");
    anyhow::ensure!(
        git_stdout(&root, &["show", "HEAD:tracked.txt"]) == "changed",
        "tracked content was not committed"
    );
    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[tokio::test]
async fn git_commit_rejects_malformed_all_and_unsafe_messages_before_staging() {
    let root = temp_test_dir("git-commit-message-validation");
    std::fs::write(root.join("tracked.txt"), "unchanged\n").expect("write fixture");

    for input in [
        json!({"message": "test", "all": "yes"}),
        json!({"message": "   "}),
        json!({"message": "bad\0message"}),
        json!({"message": "x".repeat(64 * 1024 + 1)}),
    ] {
        execute_git_commit_async(
            &input,
            &root,
            Arc::new(AtomicBool::new(false)),
            SandboxProfile::WorkspaceWrite,
            false,
        )
        .await
        .expect_err("invalid commit input must fail before invoking Git");
    }

    let _ = std::fs::remove_dir_all(root);
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn builtin_git_inspection_disables_helpers_and_ignores_ambient_git_state() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let root = temp_test_dir("builtin-git-inspection-isolation");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join(".gitattributes"), "*.txt diff=evil\n")?;
    std::fs::write(root.join("tracked.txt"), "base\n")?;
    git_ok(&root, &["add", ".gitattributes", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let helper = root.join("evil-helper.sh");
    std::fs::write(
        &helper,
        "#!/bin/sh\nprintf 'GIT_HELPER_EXECUTED:%s:%s\\n' \"${GIT_INSPECTION_API_KEY-unset}\" \"${BASH_ENV-unset}\" >&2\nexit 97\n",
    )?;
    let mut permissions = std::fs::metadata(&helper)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&helper, permissions)?;
    let helper_text = helper.to_string_lossy().into_owned();
    git_ok(&root, &["config", "diff.external", &helper_text]);
    git_ok(&root, &["config", "diff.evil.textconv", &helper_text]);
    git_ok(&root, &["config", "core.fsmonitor", &helper_text]);
    std::fs::write(root.join("tracked.txt"), "changed\n")?;

    let alternate = temp_test_dir("builtin-git-inspection-alternate");
    git_ok(&alternate, &["init", "-q"]);
    let startup = root.join("startup.sh");
    let startup_marker = root.join("startup-ran");
    std::fs::write(
        &startup,
        format!(
            "printf startup > {}\n",
            shell_single_quote(&startup_marker.to_string_lossy())
        ),
    )?;

    let old_external_diff = std::env::var_os("GIT_EXTERNAL_DIFF");
    let old_git_dir = std::env::var_os("GIT_DIR");
    let old_secret = std::env::var_os("GIT_INSPECTION_API_KEY");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    let old_bash_env = std::env::var_os("BASH_ENV");
    unsafe {
        std::env::set_var("GIT_EXTERNAL_DIFF", &helper);
        std::env::set_var("GIT_DIR", alternate.join(".git"));
        std::env::set_var("GIT_INSPECTION_API_KEY", "inspection-secret");
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
        std::env::set_var("BASH_ENV", &startup);
    }

    let result = async {
        let output = execute_builtin_call(
            "git_diff".to_string(),
            json!({"path": "tracked.txt"}),
            root.clone(),
            Arc::new(AtomicBool::new(false)),
            None,
            None,
            None,
            None,
            None,
            SandboxProfile::ReadOnly,
            false,
            None,
            Vec::new(),
        )
        .await
        .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(
            output.contains("-base") && output.contains("+changed"),
            "built-in diff did not inspect the active repository: {output}"
        );
        anyhow::ensure!(
            !output.contains("GIT_HELPER_EXECUTED"),
            "repository or ambient Git helper executed: {output}"
        );
        anyhow::ensure!(!startup_marker.exists(), "BASH_ENV startup file executed");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    restore_env_var("GIT_EXTERNAL_DIFF", old_external_diff);
    restore_env_var("GIT_DIR", old_git_dir);
    restore_env_var("GIT_INSPECTION_API_KEY", old_secret);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
    restore_env_var("BASH_ENV", old_bash_env);
    let _ = std::fs::remove_dir_all(alternate);
    let _ = std::fs::remove_dir_all(root);
    result
}

#[cfg(all(unix, not(target_os = "macos")))]
#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn builtin_git_commit_hooks_are_credential_startup_and_temp_isolated() -> Result<()> {
    use std::os::unix::fs::PermissionsExt as _;

    let _guard = env_lock();
    let root = temp_test_dir("builtin-git-commit-hook-isolation");
    git_ok(&root, &["init", "-q"]);
    git_ok(&root, &["config", "user.email", "test@example.invalid"]);
    git_ok(&root, &["config", "user.name", "Test"]);
    std::fs::write(root.join("tracked.txt"), "base\n")?;
    git_ok(&root, &["add", "tracked.txt"]);
    git_ok(&root, &["commit", "-q", "-m", "base"]);

    let report = root.join("hook-report.txt");
    let hook = root.join(".git/hooks/pre-commit");
    std::fs::write(
        &hook,
        format!(
            r#"#!/bin/bash
printf '%s|%s|%s|%s|%s|%s|%s|%s' \
  "${{COMMIT_TEST_API_KEY-unset}}" \
  "${{OPENAI_API_KEY-unset}}" \
  "${{BASH_ENV-unset}}" \
  "${{PYTHONPATH-unset}}" \
  "${{GIT_TERMINAL_PROMPT-unset}}" \
  "$(stat -c %a "$TMPDIR")" \
  "$TMPDIR,$TMP,$TEMP" \
  "${{COMMIT_STARTUP_RAN-unset}}" > {}
"#,
            shell_single_quote(&report.to_string_lossy())
        ),
    )?;
    let mut permissions = std::fs::metadata(&hook)?.permissions();
    permissions.set_mode(0o700);
    std::fs::set_permissions(&hook, permissions)?;

    let startup = root.join("startup.sh");
    let startup_marker = root.join("startup-ran");
    std::fs::write(
        &startup,
        format!(
            "printf startup > {}\nexport COMMIT_STARTUP_RAN=yes\n",
            shell_single_quote(&startup_marker.to_string_lossy())
        ),
    )?;
    let inherited_temp = root.join("inherited-temp");
    std::fs::create_dir(&inherited_temp)?;
    std::fs::write(root.join("tracked.txt"), "changed\n")?;

    let old_secret = std::env::var_os("COMMIT_TEST_API_KEY");
    let old_provider = std::env::var_os("OPENAI_API_KEY");
    let old_opt_in = std::env::var_os(TOOL_CREDENTIAL_ENV_INHERIT_FLAG);
    let old_bash_env = std::env::var_os("BASH_ENV");
    let old_pythonpath = std::env::var_os("PYTHONPATH");
    let old_tmpdir = std::env::var_os("TMPDIR");
    let old_tmp = std::env::var_os("TMP");
    let old_temp = std::env::var_os("TEMP");
    unsafe {
        std::env::set_var("COMMIT_TEST_API_KEY", "commit-secret");
        std::env::set_var("OPENAI_API_KEY", "provider-secret");
        std::env::set_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, "1");
        std::env::set_var("BASH_ENV", &startup);
        std::env::set_var("PYTHONPATH", &root);
        std::env::set_var("TMPDIR", &inherited_temp);
        std::env::set_var("TMP", &inherited_temp);
        std::env::set_var("TEMP", &inherited_temp);
    }

    let result = async {
        let output = execute_git_commit_async(
            &json!({"message": "test: isolated hook", "paths": ["tracked.txt"]}),
            &root,
            Arc::new(AtomicBool::new(false)),
            SandboxProfile::WorkspaceWrite,
            true,
        )
        .await
        .map_err(anyhow::Error::msg)?;
        anyhow::ensure!(output.contains("test: isolated hook"), "{output}");
        let rendered = std::fs::read_to_string(&report)?;
        let fields = rendered.split('|').collect::<Vec<_>>();
        anyhow::ensure!(fields.len() == 8, "unexpected hook report: {rendered:?}");
        anyhow::ensure!(
            fields[..5] == ["unset", "unset", "unset", "unset", "0"],
            "hook inherited a credential, startup vector, or prompt setting: {rendered:?}"
        );
        anyhow::ensure!(
            fields[5] == "700",
            "hook temp was not private: {rendered:?}"
        );
        let temps = fields[6].split(',').collect::<Vec<_>>();
        anyhow::ensure!(
            temps.len() == 3,
            "unexpected hook temp report: {rendered:?}"
        );
        anyhow::ensure!(temps[0] == temps[1] && temps[0] == temps[2]);
        anyhow::ensure!(Path::new(temps[0]) != inherited_temp.as_path());
        anyhow::ensure!(
            !Path::new(temps[0]).exists(),
            "private temp survived child exit: {}",
            temps[0]
        );
        anyhow::ensure!(
            fields[7] == "unset",
            "startup file affected hook: {rendered:?}"
        );
        anyhow::ensure!(!startup_marker.exists(), "BASH_ENV startup file executed");
        Ok::<(), anyhow::Error>(())
    }
    .await;

    restore_env_var("COMMIT_TEST_API_KEY", old_secret);
    restore_env_var("OPENAI_API_KEY", old_provider);
    restore_env_var(TOOL_CREDENTIAL_ENV_INHERIT_FLAG, old_opt_in);
    restore_env_var("BASH_ENV", old_bash_env);
    restore_env_var("PYTHONPATH", old_pythonpath);
    restore_env_var("TMPDIR", old_tmpdir);
    restore_env_var("TMP", old_tmp);
    restore_env_var("TEMP", old_temp);
    let _ = std::fs::remove_dir_all(root);
    result
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

    let xml_raw_tool_text = vec![Block::Text {
        text: "before <tool_call>\n<function=bash>\n<parameter=command>cargo test</parameter>\n</tool_call> after".to_string(),
    }];
    let mut history = Vec::new();
    assert!(maybe_preserve_partial_stream(
        &xml_raw_tool_text,
        &mut history,
        ContextMode::Frugal
    ));
    assert!(matches!(
        &history[0].content[0],
        Block::Text { text } if text.contains("before") && text.contains("tool call redacted") && text.contains("after") && !text.contains("cargo test")
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
    assert!(parse_compact_slash("/compacted").is_none());
    assert!(parse_compact_slash("/compact-report/output.md").is_none());
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
    agent.model = DEFAULT_LOCAL_MODEL.to_string();
    agent.thinking_effort = ThinkingEffort::Off;

    let (_url, body) = agent.build_streaming_request("sys", "env", &[], &[], "unused")?;
    let body_json: Value = serde_json::from_slice(&body)?;
    assert!(body_json.get("reasoning_effort").is_none(), "{body_json}");
    assert!(body_json.get("stream_options").is_none(), "{body_json}");
    assert!(body_json.get("prompt_cache_key").is_none(), "{body_json}");
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
    assert!(chatgpt.get("max_output_tokens").is_none(), "{chatgpt}");

    assert!(openai_reasoning_effort(DEFAULT_LOCAL_MODEL, ThinkingEffort::Off).is_none());
    assert_eq!(
        openai_reasoning_effort("gpt-5.6-terra", ThinkingEffort::Off),
        Some("none")
    );
    assert_eq!(
        openai_reasoning_effort("gpt-5.6-luna", ThinkingEffort::Max),
        Some("xhigh")
    );
    assert!(anthropic_thinking_budget_tokens(ThinkingEffort::Off).is_none());
    assert_eq!(clamp_thinking_budget_below_max(8_192, 8_192), Some(6_144));
    assert_eq!(clamp_thinking_budget_below_max(4_096, 4_096), Some(3_072));
    assert_eq!(clamp_thinking_budget_below_max(1_024, 2), Some(1));
    assert_eq!(clamp_thinking_budget_below_max(1_024, 1), None);

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn gpt_5_6_openai_request_uses_native_reasoning_and_completion_cap() -> Result<()> {
    let root = std::env::current_dir()?.canonicalize()?;
    let profile = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "openai")
        .expect("openai profile");
    let mut agent = test_agent(&root);
    agent.provider_id = "openai".to_string();
    agent.provider_profile = Some(profile);
    agent.api_provider = ApiProvider::OpenAi;
    agent.base_url = "https://api.openai.com".to_string();
    agent.model = "gpt-5.6-terra".to_string();
    agent.thinking_effort = ThinkingEffort::Off;

    let (_url, body) = agent.build_streaming_request("sys", "env", &[], &[], "unused")?;
    let body: Value = serde_json::from_slice(&body)?;
    assert_eq!(body["reasoning_effort"], "none", "{body}");
    assert_eq!(body["max_completion_tokens"], 128_000, "{body}");
    assert!(body.get("max_tokens").is_none(), "{body}");

    let chatgpt = build_chatgpt_request(
        "gpt-5.6-luna",
        ThinkingEffort::Off,
        "sys",
        "sess-1",
        vec![json!({"type":"message","role":"user","content":[]})],
        Vec::new(),
    );
    assert_eq!(chatgpt["reasoning"]["effort"], "none", "{chatgpt}");
    assert_eq!(
        chatgpt_reasoning_effort("gpt-5.6-sol", ThinkingEffort::Max),
        Some("xhigh")
    );

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
fn compaction_evidence_skips_objective_only_ledger_header() {
    let ledger = WorkLedger {
        objective: "raw user prompt that should stay hidden here".to_string(),
        ..Default::default()
    };
    let evidence = render_compaction_evidence(&[], &ledger, &ProviderHealthLedger::default());
    assert!(!evidence.contains("[ledger:active]"), "{evidence}");
    assert!(!evidence.contains("raw user prompt"), "{evidence}");
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
fn cross_platform_paths_normalize_for_tools_and_session_state() {
    assert_eq!(
        normalized_path_text(Path::new(r"\\?\C:\workspace\src\main.rs")),
        "C:/workspace/src/main.rs"
    );
    assert_eq!(
        normalized_path_text(Path::new(r"\\?\UNC\server\share\file.rs")),
        "//server/share/file.rs"
    );
    for path in [
        "/tmp/scratch.py",
        r"C:\workspace\scratch.py",
        r"\\server\share\scratch.py",
    ] {
        assert!(portable_path_is_absolute(path), "{path}");
    }
    assert!(!portable_path_is_absolute("src/main.rs"));
}

#[test]
fn windows_bash_resolver_skips_wsl_aliases() {
    let root = temp_test_dir("windows-bash-resolution");
    let system32 = root.join("Windows/System32");
    let windows_apps = root.join("Microsoft/WindowsApps");
    let git_bin = root.join("Git/bin");
    for dir in [&system32, &windows_apps, &git_bin] {
        std::fs::create_dir_all(dir).expect("create fake PATH directory");
        std::fs::write(dir.join("bash.exe"), b"fixture").expect("write fake bash");
    }
    let path = std::env::join_paths([&system32, &windows_apps, &git_bin]).expect("join fake PATH");

    assert_eq!(
        windows_bash_executable_from_path(&path),
        Some(git_bin.join("bash.exe"))
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn shell_single_quote_handles_spaces_and_quotes() {
    #[cfg(unix)]
    assert_eq!(shell_single_quote("a b'c"), "'a b'\\''c'");
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
    let out = hooks.fire(
        "user_prompt",
        "",
        &[],
        &[],
        &root,
        SandboxProfile::WorkspaceWrite,
    );
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

    let out = hooks.fire(
        "pre_tool",
        "read_file",
        &[],
        &[],
        &root,
        SandboxProfile::WorkspaceWrite,
    );
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

#[cfg(unix)]
#[test]
fn automatic_hooks_never_inherit_danger_full_access() {
    let root = temp_test_dir("hook-sandbox-ceiling");
    let outside = root.parent().expect("temp root parent").join(format!(
        "dext-hook-outside-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock drift")
            .as_nanos()
    ));
    let inside = root.join("inside.txt");
    let hooks = Hooks {
        user_prompt: vec![Hook {
            tool_match: None,
            command: format!(
                "printf inside > {}; printf outside > {}",
                shell_single_quote(&inside.to_string_lossy()),
                shell_single_quote(&outside.to_string_lossy())
            ),
        }],
        ..Default::default()
    };

    let out = hooks.fire(
        "user_prompt",
        "",
        &[],
        &[],
        &root,
        SandboxProfile::DangerFullAccess,
    );
    assert_eq!(out.len(), 1);
    assert_eq!(std::fs::read_to_string(&inside).unwrap(), "inside");
    if crate::sandbox::is_enforced() {
        assert_ne!(out[0].1, 0, "outside hook write unexpectedly succeeded");
        assert!(!outside.exists(), "hook escaped workspace confinement");
    }

    std::fs::remove_file(&inside).expect("remove workspace-write fixture");
    let read_only = hooks.fire("user_prompt", "", &[], &[], &root, SandboxProfile::ReadOnly);
    assert_eq!(read_only.len(), 1);
    if crate::sandbox::is_enforced() {
        assert_ne!(read_only[0].1, 0, "read-only hook mutated workspace");
        assert!(!inside.exists(), "read-only hook wrote inside workspace");
        assert!(!outside.exists(), "read-only hook wrote outside workspace");
    }

    let _ = std::fs::remove_file(outside);
    let _ = std::fs::remove_dir_all(root);
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
    let _guard = env_lock();
    let root = temp_test_dir("compact-env-ledger");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    let old_shelves_dir = std::env::var_os("DEXT_SHELVES_DIR");
    unsafe {
        std::env::set_var("DEXT_HOME", root.join("home"));
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let mut agent = test_agent(&root);
    agent.work_ledger.files_changed = (0..8)
        .map(|idx| format!("src/module_{idx}.rs: {}", "x".repeat(500)))
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

    restore_env_var("DEXT_HOME", old_dext_home);
    restore_env_var("DEXT_SHELVES_DIR", old_shelves_dir);
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
fn shelf_context_ability_injects_into_prompt_when_hook_opts_in() {
    let _guard = env_lock();
    let root = temp_test_dir("shelf-context-injection");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let shelf_dir = root.join(".dext/shelves/house");
    std::fs::create_dir_all(&shelf_dir).expect("create shelf dir");
    // A Context ability plus a load-signal Hook: the live signal→effect loop
    // must inject the context text into the system prompt.
    std::fs::write(
        shelf_dir.join("shelf.json"),
        r#"{
  "id": "house",
  "name": "House",
  "description": "house rules",
  "mode": "always",
  "packs": [{
    "id": "rules",
    "name": "Rules",
    "version": "0.1.0",
    "description": "house rules",
    "abilities": [
      {"ability": "context", "name": "house-rules", "description": "ALWAYS prefer rg over grep in this repo", "budget": 256},
      {"ability": "hook", "name": "loader", "signals": ["load"]}
    ]
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

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);

    assert!(env.contains("## Shelf context"), "{env}");
    assert!(
        env.contains("ALWAYS prefer rg over grep in this repo"),
        "{env}"
    );
}

#[test]
fn session_brief_renders_safe_continuation_packet() {
    let mut header = SessionHeader {
        model: "glm-4.6".to_string(),
        ..SessionHeader::default()
    };
    header.work_ledger.files_changed = vec!["src/parser.rs".to_string()];
    header.work_ledger.verification.push(VerificationRecord {
        name: "cargo test parser".to_string(),
        command: "cargo test parser".to_string(),
        status: "passed".to_string(),
        exit_code: Some(0),
        duration_ms: 12,
        artifact: None,
        validates: Vec::new(),
    });

    let history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "fix the parser bug".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "t1".to_string(),
                name: "edit_file".to_string(),
                input: json!({"path": "src/parser.rs", "old_string": "a", "new_string": "b"}),
            }],
        },
    ];

    let analysis = analyze_session_history(&header, &history);
    let brief = render_session_brief(std::path::Path::new("/tmp/s.jsonl"), &header, &analysis);

    assert!(brief.contains("# Dext session brief"), "{brief}");
    assert!(brief.contains("src/parser.rs"), "{brief}");
    assert!(brief.contains("cargo test parser: passed"), "{brief}");
    assert!(brief.contains("## Continue"), "{brief}");
    // The brief surfaces real state only; synthesized objective/checkpoint
    // placeholders are not echoed from the user prompt.
    assert!(!brief.contains("objective: fix the parser bug"), "{brief}");
    assert!(!brief.contains("deliver requested outcome"), "{brief}");
    assert!(!brief.contains("pending/next-action"), "{brief}");
    // The brief is a distilled packet: it must not embed raw prompt transcript.
    assert!(!brief.contains("## Transcript"), "{brief}");
}

#[test]
fn llama_tool_grammar_is_gated_to_local_and_opt_in() {
    use crate::provider::ApiProvider;
    let tools = ["read_file", "bash"];

    // Cloud OpenAI: never attach a grammar (it rejects unknown fields).
    assert!(
        llama_tool_grammar_for(
            "openai",
            ApiProvider::OpenAi,
            "https://api.openai.com",
            &tools,
            true
        )
        .is_none()
    );
    // Local llama.cpp but opt-in disabled: no grammar (default experience).
    assert!(
        llama_tool_grammar_for(
            "local",
            ApiProvider::OpenAi,
            "http://127.0.0.1:8080",
            &tools,
            false
        )
        .is_none()
    );
    // Local llama.cpp + opt-in: a grammar naming the tools that forces a
    // non-empty arguments object.
    let grammar = llama_tool_grammar_for(
        "local",
        ApiProvider::OpenAi,
        "http://127.0.0.1:8080",
        &tools,
        true,
    )
    .expect("local + opt-in should produce a grammar");
    assert!(grammar.contains("root"), "{grammar}");
    assert!(grammar.contains(r#""\"read_file\"""#), "{grammar}");
    assert!(grammar.contains(r#""\"bash\"""#), "{grammar}");
    // The arguments object requires at least one member, so dropped/empty
    // arguments cannot satisfy the grammar.
    assert!(
        grammar.contains(r#"object ::= "{" ws string ws ":" ws value"#),
        "{grammar}"
    );
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
    assert_eq!(agent.tool_context_profile(), ToolContextProfile::Full);
    assert!(agent.tools.iter().any(|t| t.name == "jq"));
    assert_eq!(handle_slash("/tools default", &mut agent), Some(true));
    assert_eq!(agent.tool_context_profile(), ToolContextProfile::Default);
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
    assert!(slash.contains("tools -> default"), "{slash}");

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
fn local_provider_defaults_to_frugal_without_changing_frontier_default() {
    assert_eq!(
        default_context_mode_for_provider("local", ApiProvider::OpenAi, "http://127.0.0.1:8080"),
        ContextMode::Frugal
    );
    assert_eq!(
        default_context_mode_for_provider("custom", ApiProvider::OpenAi, "http://localhost:9000"),
        ContextMode::Frugal
    );
    assert_eq!(
        default_context_mode_for_provider("openai", ApiProvider::OpenAi, "https://api.openai.com"),
        ContextMode::Standard
    );
}

#[test]
fn provider_switches_update_only_automatic_context_mode() {
    let root = temp_test_dir("automatic-context-provider-switch");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.context_mode_explicit = false;

    let local = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "local")
        .expect("local profile");
    agent.apply_runtime_provider(ResolvedProviderConfig {
        model: local.default_model.clone(),
        profile: local,
        api_key: String::new(),
        key_source: "not-required".to_string(),
        base_url: String::new(),
        requires_api_key: false,
    });
    assert_eq!(agent.context_mode, ContextMode::Frugal);

    let openai = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "openai")
        .expect("openai profile");
    agent.apply_runtime_provider(ResolvedProviderConfig {
        model: openai.default_model.clone(),
        profile: openai,
        api_key: "test".to_string(),
        key_source: "test".to_string(),
        base_url: "https://api.openai.com".to_string(),
        requires_api_key: true,
    });
    assert_eq!(agent.context_mode, ContextMode::Standard);

    agent.set_context_mode(ContextMode::Tiny);
    let local = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "local")
        .expect("local profile");
    agent.apply_runtime_provider(ResolvedProviderConfig {
        model: local.default_model.clone(),
        profile: local,
        api_key: String::new(),
        key_source: "not-required".to_string(),
        base_url: String::new(),
        requires_api_key: false,
    });
    assert_eq!(agent.context_mode, ContextMode::Tiny);
    assert!(agent.context_mode_explicit);

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn session_state_fixtures_migrate_v1_v2_and_preserve_v3_semantics() -> Result<()> {
    let (v1, v1_history) = load_session_state_fixture("v1.jsonl")?;
    assert_eq!(v1.version, SESSION_FORMAT_VERSION);
    assert_eq!(v1.model, "fixture-v1");
    assert_eq!(v1.context_mode, ContextMode::Tiny);
    assert!(v1.context_mode_explicit);
    assert_eq!(v1_history.len(), 1);

    let (v2, v2_history) = load_session_state_fixture("v2.jsonl")?;
    assert_eq!(v2.version, SESSION_FORMAT_VERSION);
    assert_eq!(v2.model, "fixture-v2");
    assert_eq!(v2_history.len(), 2);
    let Block::ToolResult { metadata, .. } = &v2_history[1].content[0] else {
        panic!("v2 fixture must contain a tool result");
    };
    assert!(metadata.is_empty());

    let (v3, v3_history) = load_session_state_fixture("v3.jsonl")?;
    assert_eq!(v3.version, SESSION_FORMAT_VERSION);
    let serialized_v3 = serde_json::to_string(&v3)?;
    assert!(
        !serialized_v3.contains("browser_recipe"),
        "retired browser metadata must not be persisted: {serialized_v3}"
    );
    assert_eq!(v3.work_ledger.objective, "fixture objective");
    assert_eq!(v3.work_ledger.current_phase, "verify");
    assert_eq!(v3.provenance.provider, "local");
    assert!(v3.provider_health.providers.contains_key("local"));
    let Block::ToolResult { metadata, .. } = &v3_history[1].content[0] else {
        panic!("v3 fixture must contain a tool result");
    };
    assert_eq!(metadata.status.as_deref(), Some("completed"));
    assert_eq!(metadata.exit_code, Some(0));
    assert_eq!(metadata.artifact.as_deref(), Some("fixture-artifact.json"));

    let future_path = state_fixture_path("sessions", "future.jsonl");
    let future_before = std::fs::read(&future_path)?;
    let future = std::fs::read_to_string(&future_path)?;
    let error = parse_session_header(future.lines().next().context("future fixture header")?)
        .err()
        .expect("future session fixture must fail")
        .to_string();
    assert!(
        error.contains("unsupported session format version 4"),
        "{error}"
    );
    assert_eq!(std::fs::read(&future_path)?, future_before);

    let corrupt_path = state_fixture_path("sessions", "corrupt.jsonl");
    let corrupt_before = std::fs::read(&corrupt_path)?;
    let error = load_session_state_fixture("corrupt.jsonl")
        .err()
        .expect("truncated session fixture must fail")
        .to_string();
    assert!(error.contains("bad fixture message on line 2"), "{error}");
    assert_eq!(std::fs::read(&corrupt_path)?, corrupt_before);
    Ok(())
}

#[test]
fn session_header_versions_migrate_in_memory_and_future_versions_fail() {
    let legacy = parse_session_header(r#"{"model":"legacy","system":"system"}"#)
        .expect("parse v1 session header");
    assert_eq!(legacy.version, SESSION_FORMAT_VERSION);
    assert_eq!(legacy.model, "legacy");

    let v2 = parse_session_header(r#"{"version":2,"model":"v2","system":"system"}"#)
        .expect("parse v2 session header");
    assert_eq!(v2.version, SESSION_FORMAT_VERSION);
    assert_eq!(v2.model, "v2");

    let current = parse_session_header(
        r#"{"version":3,"model":"v3","system":"system","context_mode":"Tiny"}"#,
    )
    .expect("parse v3 session header");
    assert_eq!(current.version, SESSION_FORMAT_VERSION);
    assert_eq!(current.context_mode, ContextMode::Tiny);
    assert!(current.context_mode_explicit);

    let error = parse_session_header(r#"{"version":4,"model":"future","system":"system"}"#)
        .err()
        .expect("future session format must fail")
        .to_string();
    assert!(
        error.contains("unsupported session format version 4"),
        "{error}"
    );
    assert!(parse_session_header(r#"{"version":"3"}"#).is_err());
    assert!(parse_session_header("[]").is_err());
}

#[test]
fn legacy_session_context_mode_preserves_nonstandard_as_explicit() {
    let mut value = serde_json::to_value(SessionHeader::default()).expect("serialize header");
    value
        .as_object_mut()
        .expect("header object")
        .remove("context_mode_explicit");
    value
        .as_object_mut()
        .expect("header object")
        .insert("context_mode".to_string(), json!("tiny"));
    let header = parse_session_header(&value.to_string()).expect("parse legacy tiny header");
    assert_eq!(header.context_mode, ContextMode::Tiny);
    assert!(header.context_mode_explicit);

    value
        .as_object_mut()
        .expect("header object")
        .insert("context_mode".to_string(), json!("standard"));
    let header = parse_session_header(&value.to_string()).expect("parse legacy standard header");
    assert_eq!(header.context_mode, ContextMode::Standard);
    assert!(!header.context_mode_explicit);
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
        ToolContextProfile::Full
    );
    assert_eq!(
        ToolContextProfile::Frugal.effective(ContextMode::Standard),
        ToolContextProfile::Default
    );
}

#[test]
fn systems_preserve_tool_protocol_guardrails_and_table_guidance() {
    assert!(
        !DEFAULT_SYSTEM.contains("Never print raw tool syntax"),
        "standard prompt should not carry frugal-only tool syntax guardrails: {DEFAULT_SYSTEM}"
    );
    assert!(
        DEFAULT_SYSTEM.contains("consolidate them into one grouped table"),
        "standard prompt should steer related table groups: {DEFAULT_SYSTEM}"
    );
    assert!(
        DEFAULT_SYSTEM.contains("one physical line per row"),
        "standard prompt should avoid renderer-hostile row wrapping: {DEFAULT_SYSTEM}"
    );
    assert!(
        DEFAULT_SYSTEM.contains("emoji verdict icons") && DEFAULT_SYSTEM.contains("unescaped `|`"),
        "standard prompt should call out fragile table-cell content: {DEFAULT_SYSTEM}"
    );
    assert!(
        DEFAULT_SYSTEM.contains("Avoid stacked heading+table blocks"),
        "standard prompt should avoid renderer-hostile table stacks: {DEFAULT_SYSTEM}"
    );

    assert!(
        DEFAULT_SYSTEM.contains("literal active user input")
            && DEFAULT_SYSTEM.contains("Never dismiss terse, path-only")
            && DEFAULT_SYSTEM.contains("inspect it first with native tools")
            && DEFAULT_SYSTEM.contains("using bash/sudo discovery"),
        "standard prompt should preserve path-only queued steering: {DEFAULT_SYSTEM}"
    );
    assert!(
        TINY_SYSTEM.contains("literal active user input")
            && TINY_SYSTEM.contains("never dismiss path-only/context-looking updates")
            && TINY_SYSTEM.contains("inspect exact user paths first")
            && TINY_SYSTEM.contains("not bash/sudo discovery"),
        "tiny prompt should preserve path-only queued steering: {TINY_SYSTEM}"
    );

    let root = temp_test_dir("frugal-tool-protocol-note");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.context_mode = ContextMode::Frugal;
    let (frugal_stable, _env) = agent.compose_system_parts();
    assert!(frugal_stable.contains("Frugal workflow"), "{frugal_stable}");
    assert!(
        frugal_stable.contains("required input and observable output"),
        "{frugal_stable}"
    );
    assert!(
        frugal_stable.contains("repair only the failed step"),
        "{frugal_stable}"
    );
    assert!(
        TINY_SYSTEM.contains("required input and observable output")
            && TINY_SYSTEM.contains("repair only the failed step"),
        "{TINY_SYSTEM}"
    );
    assert!(
        TINY_SYSTEM.contains("prefill the TUI input"),
        "{TINY_SYSTEM}"
    );
    assert!(
        TINY_SYSTEM.contains("related data -> one grouped table"),
        "{TINY_SYSTEM}"
    );
    assert!(
        TINY_SYSTEM.contains("one row/line") && TINY_SYSTEM.contains("no emoji"),
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
        history_char_budget_with_override(DEFAULT_LOCAL_MODEL, None, ContextMode::Tiny),
        32_000
    );
    assert_eq!(model_context_window(DEFAULT_LOCAL_MODEL), 200_000);
    assert_eq!(
        active_history_char_budget_with_override(DEFAULT_LOCAL_MODEL, None, ContextMode::Tiny),
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
    // Resolution order: env > runtime cache > per-model catalog metadata > model-name hint > provider default > family heuristic > fallback.
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
                builtin: None,
                display_name: "Custom".to_string(),
                api_provider: ApiProvider::OpenAi,
                request_contract: Some(RequestContract::OpenAiChatCompletions),
                base_url: "https://example.invalid".to_string(),
                default_model: "custom-default".to_string(),
                models: vec!["custom-default".to_string(), "special-1m-model".to_string()],
                model_aliases: HashMap::new(),
                model_defaults: ModelSpec::default(),
                model_specs: HashMap::new(),
                env_vars: Vec::new(),
                requires_api_key: false,
                login_url: None,
                oauth_flow: None,
                notes: None,
                context_window: Some(333_000),
                model_context_windows: per_model,
                model_effort_levels: HashMap::new(),
            }],
        };
        save_provider_catalog(&catalog)?;

        // Provider-default applies for a model listed but not per-model-overridden.
        assert_eq!(model_context_window("custom-default"), 333_000);
        // An explicit per-model value beats a conflicting name hint.
        assert_eq!(model_context_window("special-1m-model"), 1_000_000);
        // Provider-wide defaults apply only after model-name hints.
        let profile = &catalog.providers[0];
        assert_eq!(
            model_context_window_for_profile(Some(profile), "custom-128k"),
            128_000
        );
        let mut explicit_hint_profile = profile.clone();
        explicit_hint_profile
            .models
            .push("explicit-128k".to_string());
        explicit_hint_profile.model_specs.insert(
            "explicit-128k".to_string(),
            ModelSpec {
                context_window: Some(96_000),
                ..Default::default()
            },
        );
        assert_eq!(
            model_context_window_for_profile(Some(&explicit_hint_profile), "explicit-128k"),
            96_000
        );
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
fn builtin_glm_profile_declares_5_2_context_and_effort_levels() -> Result<()> {
    let _guard = env_lock();
    clear_cached_local_llama_context_windows();
    let root = temp_test_dir("glm-52-profile");
    let root = std::fs::canonicalize(&root)?;
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }

    let result = (|| -> Result<()> {
        let catalog = load_provider_catalog()?;
        let profile = find_provider_profile(&catalog, "glm").context("glm profile")?;
        assert_eq!(profile.default_model, "glm-5.2[1m]");
        assert_eq!(model_context_window("glm-5.2"), 1_000_000);
        assert_eq!(model_context_window("glm-5.2[1m]"), 1_000_000);
        assert_eq!(
            profile.model_effort_levels.get("glm-5.2[1m]"),
            Some(&vec!["high".to_string(), "max".to_string()])
        );
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
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
        for model in ["gpt-5.6", "gpt-5.6-sol", "gpt-5.6-terra", "gpt-5.6-luna"] {
            assert_eq!(model_context_window(model), 1_050_000, "{model}");
        }
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
fn detected_llama_context_does_not_override_explicit_environment_setting() {
    let _guard = env_lock();
    unsafe {
        std::env::set_var("DEXT_CONTEXT_WINDOW", "64000");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }

    assert_eq!(
        runtime_context_window_for_profile(None, "arbitrary-local-model", Some(30_000)),
        64_000
    );

    unsafe {
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }
}

#[tokio::test]
async fn offline_local_llama_probe_is_safe_inside_tokio_runtime() -> Result<()> {
    let _guard = env_lock();
    clear_cached_local_llama_context_windows();
    let tokens = refresh_local_llama_context_window(
        "local",
        ApiProvider::OpenAi,
        "http://127.0.0.1:0",
        "offline-local-model",
    );

    assert_eq!(tokens, None);
    clear_cached_local_llama_context_windows();
    Ok(())
}

#[tokio::test]
async fn local_llama_runtime_context_is_endpoint_scoped_not_model_global() -> Result<()> {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_CONTEXT_WINDOW");
        std::env::remove_var("DEXT_CONTEXT_WINDOW_TOKENS");
    }
    clear_cached_local_llama_context_windows();
    let model = "arbitrary-local-runtime-model";
    assert_eq!(model_context_window(model), 200_000);

    let listener = TcpListener::bind("127.0.0.1:0")?;
    let addr = listener.local_addr()?;
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 1024];
        let _ = stream.read(&mut request).expect("read request");
        let body = r#"{"default_generation_settings":{"n_ctx":30000}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
            body.len()
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });

    let tokens = refresh_local_llama_context_window(
        "local",
        ApiProvider::OpenAi,
        &format!("http://{addr}"),
        model,
    );
    assert_eq!(tokens, Some(30_000));
    assert_eq!(model_context_window(model), 200_000);
    server.join().expect("server thread");
    clear_cached_local_llama_context_windows();
    Ok(())
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

    let default_model_fallback = model_context_window(DEFAULT_LOCAL_MODEL);
    let custom_model_fallback = model_context_window("custom-local-probe");
    let tokens = refresh_local_llama_context_window(
        "local",
        ApiProvider::OpenAi,
        &format!("http://{addr}"),
        DEFAULT_LOCAL_MODEL,
    );
    assert_eq!(tokens, Some(30_000));
    assert_eq!(
        model_context_window(DEFAULT_LOCAL_MODEL),
        default_model_fallback
    );
    let custom_tokens = refresh_local_llama_context_window(
        "local",
        ApiProvider::OpenAi,
        &format!("http://{addr}"),
        "custom-local-probe",
    );
    assert_eq!(custom_tokens, Some(30_000));
    assert_eq!(
        model_context_window("custom-local-probe"),
        custom_model_fallback
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
    let lock = SessionStateLock::acquire(&root, "registry")?;
    let lock_path = lock.path.clone();
    assert!(lock_path.exists());

    std::mem::forget(lock);
    release_registered_locks();
    assert!(
        !lock_path.exists(),
        "lock file should be removed by registry"
    );

    let fresh = SessionStateLock::acquire(&root, "registry")?;
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

        let session_path = agent.latest_session_path.clone();
        let log_path = agent.latest_log_path.clone();
        assert!(session_path.exists(), "missing {}", session_path.display());
        assert!(log_path.exists(), "missing {}", log_path.display());
        let log = std::fs::read_to_string(&log_path)?;
        assert!(log.contains("session_checkpoint"), "{log}");

        assert!(session_path.starts_with(sessions.join(&agent.session_id)));
        assert!(log_path.starts_with(logs.join(&agent.session_id)));
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
        // Containment, not equality: DEXT_SESSIONS_DIR/DEXT_LOGS_DIR are
        // process-global, so a concurrently-running agent test can leak its own
        // session id into this directory. This test only needs to confirm that
        // this agent's session is written under its own id.
        assert!(
            session_entries.contains(&agent.session_id),
            "{session_entries:?} missing {}",
            agent.session_id
        );
        assert!(
            log_entries.contains(&agent.session_id),
            "{log_entries:?} missing {}",
            agent.session_id
        );
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
        "--frugal".to_string(),
        "--tiny".to_string(),
        "--tool-context-profile=full".to_string(),
        "--tool-profile=default".to_string(),
        format!("@{}", task_file.display()),
        "tail".to_string(),
    ])?;

    assert!(opts.no_session);
    assert!(opts.fork);
    assert_eq!(opts.cd, Some(root.clone()));
    assert_eq!(opts.output, OutputMode::StreamJson);
    assert_eq!(opts.budget_cap.and_then(|cap| cap.tokens), Some(250_000));
    assert_eq!(
        opts.approval_policy_override,
        Some(ApprovalProfile::AutoRead)
    );
    assert_eq!(opts.sandbox_profile, Some(SandboxProfile::ReadOnly));
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
fn parse_cli_options_accepts_resume_selector_equals_form() -> Result<()> {
    let opts = parse_cli_options(vec![
        "--resume=branch-w01".to_string(),
        "tail task".to_string(),
    ])?;

    assert!(opts.resume_latest);
    assert_eq!(opts.resume_selector.as_deref(), Some("branch-w01"));
    assert_eq!(opts.positional, vec!["tail task".to_string()]);

    let opts = parse_cli_options(vec!["--resume".to_string()])?;
    assert!(opts.resume_latest);
    assert!(opts.resume_selector.is_none());
    Ok(())
}

#[test]
fn tiny_context_mode_sets_distinct_system_prompt() {
    let root = temp_test_dir("tiny-mode-banner");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.system = DEFAULT_SYSTEM.to_string();

    assert!(agent.session_dir().ends_with(&agent.session_id));

    agent.set_context_mode(ContextMode::Tiny);

    assert!(agent.context_mode_explicit);
    assert!(agent.context_mode.is_tiny());
    assert_eq!(agent.system, TINY_SYSTEM);

    agent.set_context_mode(ContextMode::Frugal);
    assert_eq!(agent.context_mode, ContextMode::Frugal);
    assert_eq!(agent.system, DEFAULT_SYSTEM);

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
fn parse_cli_options_policy_flags_last_one_wins() -> Result<()> {
    let opts = parse_cli_options(vec!["--no-trust".to_string(), "--trust".to_string()])?;
    assert_eq!(opts.approval_policy_override, Some(ApprovalProfile::Always));

    let opts = parse_cli_options(vec![
        "--trust".to_string(),
        "--approval=auto-write".to_string(),
        "--no-trust".to_string(),
        "--approval-profile".to_string(),
        "never".to_string(),
    ])?;
    assert_eq!(opts.approval_policy_override, Some(ApprovalProfile::Never));
    Ok(())
}

#[test]
fn console_permission_denies_non_interactive_and_prompts_only_with_two_terminals() {
    let prompt_calls = std::cell::Cell::new(0);
    let prompt = || {
        prompt_calls.set(prompt_calls.get() + 1);
        Choice::Once
    };

    assert!(matches!(
        resolve_console_permission(false, true, prompt),
        Choice::Deny
    ));
    assert!(matches!(
        resolve_console_permission(true, false, prompt),
        Choice::Deny
    ));
    assert_eq!(prompt_calls.get(), 0);
    assert!(matches!(
        resolve_console_permission(true, true, prompt),
        Choice::Once
    ));
    assert_eq!(prompt_calls.get(), 1);
}

#[test]
fn json_permission_always_denies_without_delegating_to_console() {
    for mode in [OutputMode::Json, OutputMode::StreamJson] {
        let mut sink = JsonSink::new(mode, false, true);
        assert!(matches!(
            sink.request_permission("write_file", &json!({"path": "denied.txt"})),
            Choice::Deny
        ));
    }
}

#[test]
fn approval_policy_resolver_is_fail_safe_and_has_fixed_precedence() {
    let resolved = resolve_approval_policy(None, None, None);
    assert_eq!(resolved.profile, ApprovalProfile::Ask);
    assert_eq!(resolved.source, ApprovalPolicySource::Default);

    let resolved = resolve_approval_policy(None, Some("auto-write"), Some("1"));
    assert_eq!(resolved.profile, ApprovalProfile::AutoWrite);
    assert_eq!(resolved.source, ApprovalPolicySource::DextApproval);

    let resolved = resolve_approval_policy(Some(ApprovalProfile::Never), Some("always"), Some("1"));
    assert_eq!(resolved.profile, ApprovalProfile::Never);
    assert_eq!(resolved.source, ApprovalPolicySource::Cli);

    let resolved = resolve_approval_policy(None, Some("invalid"), Some("yes"));
    assert_eq!(resolved.profile, ApprovalProfile::Always);
    assert_eq!(resolved.source, ApprovalPolicySource::DextTrust);
    assert_eq!(resolved.warnings.len(), 1);

    let resolved = resolve_approval_policy(None, None, Some("invalid"));
    assert_eq!(resolved.profile, ApprovalProfile::Ask);
    assert_eq!(resolved.source, ApprovalPolicySource::Default);
    assert_eq!(resolved.warnings.len(), 1);

    for false_value in ["0", "false", "off", "no"] {
        let resolved = resolve_approval_policy(None, None, Some(false_value));
        assert_eq!(resolved.profile, ApprovalProfile::Ask);
        assert_eq!(resolved.source, ApprovalPolicySource::Default);
    }
}

#[test]
fn resume_reconciles_each_pending_tool_journal_fence_without_replay() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("tool-journal-resume-fences");
    let input = json!({"path": "fenced.txt", "content": "new"});

    let mut no_start_history = vec![Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call-no-start".to_string(),
            name: "write_file".to_string(),
            input: input.clone(),
        }],
    }];
    let recovery = reconcile_pending_tool_calls(&mut no_start_history, None)?;
    assert_eq!(recovery.not_started, 1);
    assert_eq!(
        last_tool_result(&no_start_history),
        Some((
            "[resume recovery] write_file was not started: no durable start fence exists. Review current state before making a new call.",
            "not_started"
        ))
    );

    let session_id = "resume-fences";
    let session_path = session_latest_session_path(&root, session_id);
    let record_id = tool_journal::start(
        &root,
        session_id,
        tool_journal::StartSpec {
            turn_id: "turn-1",
            batch_id: "batch-1",
            call_id: "call-fenced",
            tool_name: "write_file",
            summary: "write_file: approved side-effect-capable call",
            input: &input,
        },
    )?;
    let entries = tool_journal::load_for_session_file(&session_path)?.expect("started journal");
    let mut uncertain_history = vec![Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call-fenced".to_string(),
            name: "write_file".to_string(),
            input: input.clone(),
        }],
    }];
    let recovery = reconcile_pending_tool_calls(&mut uncertain_history, Some(&entries))?;
    assert_eq!(recovery.uncertain, 1);
    assert_eq!(
        last_tool_result(&uncertain_history).map(|(_, status)| status),
        Some("uncertain")
    );

    tool_journal::finish(
        &root,
        session_id,
        &record_id,
        tool_journal::ToolJournalStatus::Completed,
    )?;
    let entries = tool_journal::load_for_session_file(&session_path)?.expect("terminal journal");
    let mut terminal_history = vec![Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call-fenced".to_string(),
            name: "write_file".to_string(),
            input: input.clone(),
        }],
    }];
    let recovery = reconcile_pending_tool_calls(&mut terminal_history, Some(&entries))?;
    assert_eq!(recovery.recovered_terminal, 1);
    assert_eq!(
        last_tool_result(&terminal_history).map(|(_, status)| status),
        Some("recovered_completed")
    );
    assert!(
        !root.join("fenced.txt").exists(),
        "recovery must never replay the tool"
    );

    let before = terminal_history.len();
    let recovery = reconcile_pending_tool_calls(&mut terminal_history, Some(&entries))?;
    assert_eq!(recovery.total(), 0);
    assert_eq!(
        terminal_history.len(),
        before,
        "paired calls must remain unchanged"
    );

    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn loading_session_reconciles_source_journal_and_emits_one_warning() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("tool-journal-load-recovery");
    let mut source = test_agent(&root);
    let input = json!({"path": "uncertain.txt", "content": "new"});
    source.history.push(Message {
        role: "assistant".to_string(),
        content: vec![Block::ToolUse {
            id: "call-uncertain".to_string(),
            name: "write_file".to_string(),
            input: input.clone(),
        }],
    });
    let source_path = source.save_latest_session()?;
    tool_journal::start(
        &root,
        &source.session_id,
        tool_journal::StartSpec {
            turn_id: "turn-1",
            batch_id: "batch-1",
            call_id: "call-uncertain",
            tool_name: "write_file",
            summary: "write_file: approved side-effect-capable call",
            input: &input,
        },
    )?;

    let mut resumed = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    resumed.set_sink(Box::new(ChannelSink { tx }));
    resumed.load_session_from_path(&source_path)?;

    assert_eq!(
        last_tool_result(&resumed.history).map(|(_, status)| status),
        Some("uncertain")
    );
    assert!(
        !root.join("uncertain.txt").exists(),
        "resume must not replay uncertain calls"
    );
    let warnings = drain_events(&mut rx)
        .into_iter()
        .filter(|event| matches!(event, AgentEvent::Warn(text) if text.contains("resume recovery")))
        .count();
    assert_eq!(warnings, 1);

    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[allow(
    clippy::await_holding_lock,
    reason = "the process-global test environment must remain stable across the async operation"
)]
#[tokio::test(flavor = "current_thread")]
async fn side_effect_tool_journal_records_completed_failed_and_interrupted_outcomes() -> Result<()>
{
    let _guard = env_lock();
    async fn run_call(
        root: &Path,
        call_id: &str,
        tool_name: &str,
        input: Value,
        interrupt_after_start: bool,
    ) -> Result<(Agent, std::result::Result<(), anyhow::Error>)> {
        let (base_url, server) = spawn_openai_tool_call_server(call_id, tool_name, &input);
        let mut agent = test_agent(root);
        configure_local_openai_agent(&mut agent, base_url);
        agent.set_sandbox_profile(SandboxProfile::DangerFullAccess);
        let interrupt = agent.interrupt.clone();
        let session_path = agent.latest_session_path.clone();
        let watcher = interrupt_after_start.then(|| {
            tokio::spawn(async move {
                for _ in 0..500 {
                    if tool_journal::load_for_session_file(&session_path)
                        .ok()
                        .flatten()
                        .is_some_and(|entries| {
                            entries.iter().any(|entry| {
                                entry.status == tool_journal::ToolJournalStatus::Started
                            })
                        })
                    {
                        interrupt.store(true, Ordering::SeqCst);
                        return;
                    }
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
        });
        let result = agent.chat("run one tool".to_string()).await;
        if let Some(watcher) = watcher {
            watcher.await.expect("interrupt watcher");
        }
        server.join().expect("tool-call server");
        Ok((agent, result))
    }

    let completed_root = temp_test_dir("tool-journal-completed");
    let (completed, result) = run_call(
        &completed_root,
        "call-completed",
        "write_file",
        json!({"path": "completed.txt", "content": "done"}),
        false,
    )
    .await?;
    result?;
    assert_eq!(
        std::fs::read_to_string(completed_root.join("completed.txt"))?,
        "done"
    );
    let entries = tool_journal::load_for_session_file(&completed.latest_session_path)?
        .expect("completed journal");
    assert_eq!(
        entries.last().map(|entry| entry.status),
        Some(tool_journal::ToolJournalStatus::Completed)
    );

    let failed_root = temp_test_dir("tool-journal-failed");
    let (failed, result) = run_call(
        &failed_root,
        "call-failed",
        "bash",
        json!({"command": "exit 7"}),
        false,
    )
    .await?;
    result?;
    let entries =
        tool_journal::load_for_session_file(&failed.latest_session_path)?.expect("failed journal");
    assert_eq!(
        entries.last().map(|entry| entry.status),
        Some(tool_journal::ToolJournalStatus::Failed)
    );

    let interrupted_root = temp_test_dir("tool-journal-interrupted");
    let (interrupted, result) = run_call(
        &interrupted_root,
        "call-interrupted",
        "bash",
        json!({"command": "sleep 5"}),
        true,
    )
    .await?;
    assert!(result.is_err(), "interrupted turn should return an error");
    let entries = tool_journal::load_for_session_file(&interrupted.latest_session_path)?
        .expect("interrupted journal");
    assert_eq!(
        entries.last().map(|entry| entry.status),
        Some(tool_journal::ToolJournalStatus::Interrupted)
    );

    let _ = std::fs::remove_dir_all(completed_root);
    let _ = std::fs::remove_dir_all(failed_root);
    let _ = std::fs::remove_dir_all(interrupted_root);
    Ok(())
}

#[cfg(unix)]
#[allow(
    clippy::await_holding_lock,
    reason = "the process-global test environment must remain stable across the async operation"
)]
#[tokio::test(flavor = "current_thread")]
async fn side_effect_tool_does_not_execute_when_journal_start_fence_fails() -> Result<()> {
    use std::os::unix::fs::symlink;

    let _guard = env_lock();

    let root = temp_test_dir("tool-journal-start-fail-closed");
    let input = json!({"path": "must-not-exist.txt", "content": "blocked"});
    let (base_url, server) =
        spawn_openai_tool_call_server("call-start-fails", "write_file", &input);
    let mut agent = test_agent(&root);
    configure_local_openai_agent(&mut agent, base_url);
    std::fs::create_dir_all(session::session_state_dir(&root, &agent.session_id))?;
    let target = root.join("unsafe-journal-target");
    std::fs::write(&target, "{}")?;
    symlink(
        &target,
        tool_journal::journal_path(&root, &agent.session_id),
    )?;

    agent.chat("try one tool".to_string()).await?;
    server.join().expect("tool-call server");

    assert!(!root.join("must-not-exist.txt").exists());
    let (content, _) = last_tool_result(&agent.history).expect("start fence tool result");
    assert!(content.contains("start fence failed"), "{content}");

    let _ = std::fs::remove_dir_all(root);
    Ok(())
}

#[test]
fn applying_current_policy_after_resume_clears_saved_privileged_grants() -> Result<()> {
    let root = temp_test_dir("resume-policy");
    let mut source = test_agent(&root);
    source.set_resolved_approval_profile(ApprovalProfile::Always, ApprovalPolicySource::Cli);
    let path = root.join("saved.jsonl");
    source.save_session_to_path(&path)?;

    let mut resumed = test_agent(&root);
    resumed.load_session_from_path(&path)?;
    assert_eq!(resumed.approval_profile, ApprovalProfile::Always);
    assert!(resumed.allowed.contains("write_file"));

    resumed.set_resolved_approval_profile(ApprovalProfile::Ask, ApprovalPolicySource::Default);
    assert_eq!(resumed.approval_profile, ApprovalProfile::Ask);
    assert_eq!(
        resumed.approval_policy_source,
        ApprovalPolicySource::Default
    );
    assert!(!resumed.allowed.contains("write_file"));

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn user_dotenv_loads_only_the_explicit_state_file() {
    let _guard = env_lock();
    let root = temp_test_dir("user-dotenv");
    let state_env = root.join("state.env");
    let project_env = root.join(".env");
    std::fs::write(
        &state_env,
        "DEXT_TEST_USER_DOTENV=trusted\nDEXT_APPROVAL=ask\n",
    )
    .unwrap();
    std::fs::write(
        &project_env,
        "DEXT_TEST_PROJECT_DOTENV=untrusted\nDEXT_APPROVAL=always\n",
    )
    .unwrap();
    let old_user = std::env::var_os("DEXT_TEST_USER_DOTENV");
    let old_project = std::env::var_os("DEXT_TEST_PROJECT_DOTENV");
    let old_approval = std::env::var_os("DEXT_APPROVAL");
    unsafe {
        std::env::remove_var("DEXT_TEST_USER_DOTENV");
        std::env::remove_var("DEXT_TEST_PROJECT_DOTENV");
        std::env::remove_var("DEXT_APPROVAL");
    }

    load_user_dotenv_from(&state_env);

    assert_eq!(
        std::env::var("DEXT_TEST_USER_DOTENV").as_deref(),
        Ok("trusted")
    );
    assert_eq!(std::env::var("DEXT_APPROVAL").as_deref(), Ok("ask"));
    assert!(std::env::var("DEXT_TEST_PROJECT_DOTENV").is_err());

    restore_env_var("DEXT_TEST_USER_DOTENV", old_user);
    restore_env_var("DEXT_TEST_PROJECT_DOTENV", old_project);
    restore_env_var("DEXT_APPROVAL", old_approval);
    let _ = std::fs::remove_dir_all(&root);
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
    assert_eq!(usage.total_input_tokens(), 162_000);
    assert_eq!(usage.context_tokens(), 167_400);
    assert_eq!(usage.total_tokens(), 167_400);
    assert!(usage.line().contains("input=162000"));
    assert!(usage.line().contains("new_in=120000"));
    assert!(usage.line().contains("cache_r=40000"));
    assert!(usage.line().contains("cache_w=2000"));
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
    assert_eq!(usage.total_input_tokens(), 1_400);
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
    assert_eq!(usage.total_input_tokens(), 1_000);
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
    assert_eq!(usage.total_input_tokens(), 1000);
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
    assert_eq!(usage.total_input_tokens(), 1000);
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
    assert_eq!(usage.total_input_tokens(), 1000);
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
        DEFAULT_LOCAL_MODEL,
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
fn gpt_5_6_pricing_applies_documented_long_context_tier() {
    let _guard = env_lock();
    for name in [
        "DEXT_INPUT_USD_PER_MTOK",
        "DEXT_OUTPUT_USD_PER_MTOK",
        "DEXT_CACHE_READ_USD_PER_MTOK",
        "DEXT_CACHE_CREATE_USD_PER_MTOK",
    ] {
        unsafe {
            std::env::remove_var(name);
        }
    }
    let usage = Usage {
        input: 300_000,
        output: 100_000,
        cache_create: 0,
        cache_read: 0,
        cost_usd: None,
    };
    for (model, expected) in [
        ("gpt-5.6-sol", 7.5),
        ("gpt-5.6-terra", 3.75),
        ("gpt-5.6-luna", 1.5),
    ] {
        let priced = usage_with_current_pricing(
            usage,
            "openai",
            ApiProvider::OpenAi,
            "https://api.openai.com",
            model,
            None,
        );
        assert_eq!(priced.cost_usd, Some(expected), "{model}");
    }

    let threshold_usage = Usage {
        input: 272_000,
        output: 100_000,
        ..Usage::default()
    };
    let threshold = usage_with_current_pricing(
        threshold_usage,
        "openai",
        ApiProvider::OpenAi,
        "https://api.openai.com",
        "gpt-5.6-terra",
        None,
    );
    assert!(
        (threshold.cost_usd.expect("threshold price") - 2.18).abs() < 1e-12,
        "{:?}",
        threshold.cost_usd
    );
}

#[test]
fn anthropic_fable_pricing_matches_console_session_cost() {
    let pricing = usage_pricing_for(
        "anthropic",
        ApiProvider::Anthropic,
        "https://api.anthropic.com",
        "claude-fable-5",
    );
    let usage = Usage {
        input: 267_523,
        output: 7_024,
        cache_create: 134_837,
        cache_read: 261_781,
        cost_usd: None,
    };
    let estimate = pricing.estimate(usage);

    assert!(
        (estimate - 5.83).abs() < 0.0001,
        "expected $5.83, got ${estimate:.8}"
    );
    assert!((pricing.output / pricing.input - 5.0).abs() < 0.000001);
    assert!((pricing.cache_read / pricing.input - 0.1).abs() < 0.000001);
    assert!((pricing.cache_create / pricing.input - 1.25).abs() < 0.000001);
}

#[test]
fn anthropic_wire_cost_is_repriced_for_supported_claude_models() {
    let root = temp_test_dir("anthropic-reprice-wire-cost");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.provider_id = "anthropic".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "claude-fable-5".to_string();
    let mut usage = Usage {
        input: 267_523,
        output: 7_024,
        cache_create: 134_837,
        cache_read: 261_781,
        cost_usd: Some(0.49736735),
    };

    agent.finalize_usage_metrics(&mut usage);

    assert!(
        (usage.estimated_cost_usd() - 5.83).abs() < 0.0001,
        "expected Anthropic model pricing to override stale wire/default cost, got ${:.8}",
        usage.estimated_cost_usd()
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn glm_wire_cost_is_preserved_when_provider_reports_it() {
    let root = temp_test_dir("glm-preserve-wire-cost");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.provider_id = "glm".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "glm-5.1".to_string();
    let mut usage = Usage {
        input: 1_000,
        output: 100,
        cache_create: 0,
        cache_read: 0,
        cost_usd: Some(0.123),
    };

    agent.finalize_usage_metrics(&mut usage);

    assert_eq!(usage.cost_usd, Some(0.123));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn anthropic_api_provider_uses_anthropic_model_pricing_for_custom_profiles() {
    let direct = usage_pricing_for(
        "anthropic",
        ApiProvider::Anthropic,
        "https://api.anthropic.com",
        "claude-fable-5",
    );
    let custom = usage_pricing_for(
        "custom-claude",
        ApiProvider::Anthropic,
        "https://api.anthropic.com",
        "claude-fable-5",
    );
    let usage = Usage {
        input: 1_000_000,
        output: 0,
        cache_create: 0,
        cache_read: 0,
        cost_usd: None,
    };

    assert_eq!(custom.estimate(usage), direct.estimate(usage));
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
    assert!(!provider_names.contains("nonexistent_tool"));
    assert!(provider_names.contains("read_file"));
    assert!(provider_names.contains("jq"));
    assert!(provider_names.contains("csvkit"));
    assert!(!provider_names.contains("browser"));
    assert!(!tools::is_external_process_tool("browser"));

    let default_names: HashSet<&str> = provider_tool_definitions()
        .iter()
        .filter(|tool| tool_name_allowed_in_profile(tool.name, ToolContextProfile::Default))
        .map(|tool| tool.name)
        .collect();
    assert!(default_names.contains("read_file"));
    assert!(!default_names.contains("jq"));
    assert!(!default_names.contains("csvkit"));
    assert!(!default_names.contains("git_log"));

    let slash_names: HashSet<&str> = tools::slash_command_definitions()
        .iter()
        .map(|cmd| cmd.name)
        .collect();
    assert!(!slash_names.contains("browser"));
    assert!(slash_names.contains("tools"));
    assert!(!slash_names.contains("toolset"));
    assert!(slash_names.contains("pack"));
    assert!(slash_names.contains("shelves"));
    assert!(!slash_names.contains("read_file"));
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

        let session_path = agent.latest_session_path.clone();
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
    let root = temp_test_dir("checkpoint-no-clobber");
    let sessions = root.join("sessions");
    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::set_var("DEXT_SESSIONS_DIR", &sessions) };

    let result = (|| -> Result<()> {
        let root_canon = std::fs::canonicalize(&root)?;

        let mut parent = test_agent(&root_canon);
        let session_path = parent.latest_session_path.clone();
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
                text: "child-message".to_string(),
            }],
        });
        sub.checkpoint_latest_session("sub_should_noop");

        let after_sub = std::fs::read_to_string(&session_path)?;
        assert!(
            after_sub.contains("parent-message"),
            "parent session was clobbered by child: {after_sub}"
        );
        assert!(
            !after_sub.contains("child-message"),
            "child leaked into parent session: {after_sub}"
        );
        Ok(())
    })();

    // Safe: test holds a global lock around env mutation.
    unsafe { std::env::remove_var("DEXT_SESSIONS_DIR") };
    let _ = std::fs::remove_dir_all(&root);
    result.expect("checkpoint suppression");
}

#[tokio::test]
async fn bash_tool_receives_active_pack_environment() {
    let root = temp_test_dir("bash-pack-env");
    let pack_dir = root.join("pack");
    std::fs::create_dir_all(&pack_dir).expect("pack dir");
    let pack_dir_text = pack_dir.display().to_string();
    let out = execute_builtin_call(
        "bash".to_string(),
        json!({"command": "printf '%s\\n%s\\n' \"$DEXT_PACK_DIR\" \"$DEXT_PACK_DEMO_DIR\""}),
        root.clone(),
        Arc::new(AtomicBool::new(false)),
        None,
        None,
        None,
        None,
        None,
        SandboxProfile::WorkspaceWrite,
        false,
        None,
        vec![
            ("DEXT_PACK_DIR".to_string(), pack_dir_text.clone()),
            ("DEXT_PACK_DEMO_DIR".to_string(), pack_dir_text.clone()),
        ],
    )
    .await
    .expect("bash pack environment");

    assert!(
        out.contains(&format!("{pack_dir_text}\n{pack_dir_text}")),
        "{out}"
    );
    let _ = std::fs::remove_dir_all(&root);
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
        None,
        None,
        None,
        SandboxProfile::WorkspaceWrite,
        false,
        None,
        Vec::new(),
    )
    .await
    .expect_err("expected input timeout");
    assert!(err.contains("timed out after 1s running bash"), "{err}");
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

    let explicit_hidden_root = root.join(".dext/shelves");
    std::fs::create_dir_all(&explicit_hidden_root).expect("create explicit hidden root");
    let (_fd_bin, explicit_fd_args, _) = prepare_external_tool(
        "fd",
        &json!({"pattern": "PACK\\.md$", "path": explicit_hidden_root.to_str().unwrap()}),
        &root,
    )
    .expect("prepare fd for explicit .dext root");
    assert!(
        !explicit_fd_args
            .windows(2)
            .any(|w| w == ["--exclude", ".dext"] || w == ["-path", "*/.dext/*"]),
        "explicit .dext root must not exclude itself: {explicit_fd_args:?}"
    );

    let (_rg_bin, explicit_rg_args, _) = prepare_external_tool(
        "rg",
        &json!({"pattern": "needle", "path": explicit_hidden_root.to_str().unwrap()}),
        &root,
    )
    .expect("prepare rg for explicit .dext root");
    assert!(
        !explicit_rg_args
            .windows(2)
            .any(|w| w == ["--glob", "!**/.dext/**"])
            && !explicit_rg_args.contains(&"--exclude-dir=.dext".to_string()),
        "explicit .dext root must not exclude itself: {explicit_rg_args:?}"
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
        SandboxProfile::WorkspaceWrite,
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
fn todo_and_compact_settings_state_fixtures_validate_without_rewriting_rejections() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("todo-settings-fixtures");
    let root = std::fs::canonicalize(root)?;
    unsafe {
        std::env::set_var("DEXT_HOME", root.join("dext-home"));
    }

    let result = (|| -> Result<()> {
        let todo_path = root.join("DEXT.todo.json");
        let valid_todo = std::fs::read(state_fixture_path("todo", "valid.json"))?;
        std::fs::write(&todo_path, &valid_todo)?;
        let rendered = execute_tool("todo_read", &json!({}), &root).map_err(anyhow::Error::msg)?;
        assert!(rendered.contains("fixture pending"), "{rendered}");
        assert!(rendered.contains("fixture complete"), "{rendered}");
        assert_eq!(std::fs::read(&todo_path)?, valid_todo);

        let corrupt_todo = std::fs::read(state_fixture_path("todo", "corrupt.json"))?;
        std::fs::write(&todo_path, &corrupt_todo)?;
        let error = execute_tool("todo_read", &json!({}), &root)
            .expect_err("non-array todo fixture must fail");
        assert!(error.contains("expected array"), "{error}");
        assert_eq!(std::fs::read(&todo_path)?, corrupt_todo);

        let settings_path = compact_threshold_settings_path();
        std::fs::create_dir_all(settings_path.parent().context("settings parent")?)?;
        let valid_settings = std::fs::read(state_fixture_path("settings", "valid.json"))?;
        std::fs::write(&settings_path, &valid_settings)?;
        assert_eq!(load_compact_threshold_percent_setting()?, Some(42));
        assert_eq!(std::fs::read(&settings_path)?, valid_settings);

        for fixture in ["out-of-range.json", "corrupt.json"] {
            let bytes = std::fs::read(state_fixture_path("settings", fixture))?;
            std::fs::write(&settings_path, &bytes)?;
            assert!(
                load_compact_threshold_percent_setting().is_err(),
                "settings fixture {fixture}"
            );
            assert_eq!(std::fs::read(&settings_path)?, bytes);
        }
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
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
            vec![
                "glm",
                "chatgpt",
                "openai",
                "anthropic",
                "kimi",
                "deepseek",
                "local",
            ]
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

        let kimi = find_provider_profile(&catalog, "kimi").context("kimi")?;
        assert_eq!(kimi.api_provider, ApiProvider::Anthropic);
        assert_eq!(kimi.env_vars, vec!["KIMI_API_KEY"]);
        assert_eq!(kimi.base_url, "https://api.kimi.com/coding");
        assert_eq!(
            kimi.login_url.as_deref(),
            Some("https://www.kimi.com/code/console")
        );
        assert_eq!(kimi.default_model, "k3");
        assert!(kimi.oauth_flow.is_none());

        let deepseek = find_provider_profile(&catalog, "deepseek").context("deepseek")?;
        assert_eq!(deepseek.api_provider, ApiProvider::OpenAi);
        assert_eq!(deepseek.env_vars, vec!["DEEPSEEK_API_KEY"]);
        assert_eq!(deepseek.default_model, "deepseek-chat");

        let local = find_provider_profile(&catalog, "local").context("local")?;
        assert_eq!(local.api_provider, ApiProvider::OpenAi);
        assert!(!local.requires_api_key);
        assert_eq!(local.default_model, DEFAULT_LOCAL_MODEL);
        assert_eq!(local.models, vec![DEFAULT_LOCAL_MODEL.to_string()]);
        assert!(local.model_context_windows.is_empty());
        assert_eq!(local.context_window, None);
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn local_provider_merge_preserves_user_aliases_and_context() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "local")
        .expect("local profile");
    let mut stored = builtin.clone();
    stored.default_model = "custom-local-model".to_string();
    stored.models.push("qwen-local".to_string());
    stored.models.push("qwen2.5-coder-7b".to_string());
    stored.models.push("qwen3.5-9b".to_string());
    stored.models.push("custom-local-model".to_string());
    stored.context_window = Some(4_096);
    stored
        .model_context_windows
        .insert("qwen-local".to_string(), 4_096);
    stored
        .model_context_windows
        .insert("custom-local-model".to_string(), 12_345);

    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(merged.default_model, "custom-local-model");
    assert_eq!(merged.context_window, Some(4_096));
    assert!(merged.models.iter().any(|m| m == "qwen-local"));
    assert!(merged.models.iter().any(|m| m == "qwen2.5-coder-7b"));
    assert!(merged.models.iter().any(|m| m == "qwen3.5-9b"));
    assert!(merged.models.iter().any(|m| m == "custom-local-model"));
    assert_eq!(merged.model_context_windows.get("qwen-local"), Some(&4_096));
    assert_eq!(
        merged.model_context_windows.get("custom-local-model"),
        Some(&12_345)
    );
}

#[test]
fn local_provider_merge_preserves_user_local_context_without_retired_artifacts_even_when_small() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "local")
        .expect("local profile");
    let mut stored = builtin.clone();
    stored.context_window = Some(4_096);
    stored.models.push("custom-local-model".to_string());

    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(merged.context_window, Some(4_096));
    assert!(merged.models.iter().any(|m| m == "custom-local-model"));
}

#[test]
fn local_provider_merge_preserves_context_for_old_local_alias() {
    let builtin = built_in_provider_profiles()
        .into_iter()
        .find(|p| p.id == "local")
        .expect("local profile");
    let mut stored = builtin.clone();
    stored.context_window = Some(123_456);
    stored.models.push("qwen-local".to_string());
    stored
        .model_context_windows
        .insert("qwen-local".to_string(), 123_456);

    let merged = merge_provider_profile(builtin, stored);
    assert_eq!(merged.context_window, Some(123_456));
    assert!(merged.models.iter().any(|m| m == "qwen-local"));
    assert_eq!(
        merged.model_context_windows.get("qwen-local"),
        Some(&123_456)
    );
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

#[cfg(unix)]
#[test]
fn crash_snapshots_are_private_and_reject_symlink_targets() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt, symlink};

    let root = temp_test_dir("crash-snapshot-private");
    let crash_dir = root.join("crashes");
    std::fs::create_dir(&crash_dir)?;
    std::fs::set_permissions(&crash_dir, std::fs::Permissions::from_mode(0o755))?;
    let snapshot = crash_dir.join("crash.json");
    write_private_crash_snapshot(&snapshot, &json!({"safe": true}))?;
    assert_eq!(
        std::fs::metadata(&crash_dir)?.permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        std::fs::metadata(&snapshot)?.permissions().mode() & 0o777,
        0o600
    );

    let victim = root.join("victim.json");
    std::fs::write(&victim, b"unchanged")?;
    let linked_snapshot = crash_dir.join("linked.json");
    symlink(&victim, &linked_snapshot)?;
    assert!(
        write_private_crash_snapshot(&linked_snapshot, &json!({"safe": false})).is_err(),
        "existing snapshot symlink must be rejected"
    );
    assert_eq!(std::fs::read(&victim)?, b"unchanged");

    let target_dir = root.join("target-dir");
    std::fs::create_dir(&target_dir)?;
    let linked_dir = root.join("linked-crashes");
    symlink(&target_dir, &linked_dir)?;
    assert!(
        write_private_crash_snapshot(&linked_dir.join("crash.json"), &json!({"safe": false}))
            .is_err(),
        "crash directory symlink must be rejected"
    );
    assert!(!target_dir.join("crash.json").exists());

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn auth_store_inspection_classifies_integrity_without_rewrite_or_reference_execution() -> Result<()>
{
    let _guard = env_lock();
    let root = temp_test_dir("auth-store-inspection-integrity");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let path = auth_store_path();
        let missing = crate::provider::inspect_auth_store();
        assert_eq!(
            missing.security,
            crate::provider::AuthStoreFileSecurity::Missing
        );
        assert_eq!(
            missing.integrity,
            crate::provider::AuthStoreIntegrity::NotChecked
        );

        let marker = root.join("command-reference-executed");
        let current = serde_json::to_vec(&json!({
            "version": 1,
            "providers": {
                "openai": {
                    "type": "api_key",
                    "key": format!("!printf executed > {}", marker.display())
                }
            }
        }))?;
        std::fs::write(&path, &current)?;
        let inspected = crate::provider::inspect_auth_store();
        assert_eq!(
            inspected.integrity,
            crate::provider::AuthStoreIntegrity::Valid {
                version: 1,
                legacy: false
            }
        );
        assert!(
            !marker.exists(),
            "auth inspection executed a command reference"
        );
        assert_eq!(std::fs::read(&path)?, current);

        let legacy = br#"{"openai":"ENV_REFERENCE"}"#;
        std::fs::write(&path, legacy)?;
        assert_eq!(
            crate::provider::inspect_auth_store().integrity,
            crate::provider::AuthStoreIntegrity::Valid {
                version: 1,
                legacy: true
            }
        );
        assert_eq!(std::fs::read(&path)?, legacy);

        let invalid = br#"{"version":1,"providers":"invalid"}"#;
        std::fs::write(&path, invalid)?;
        assert_eq!(
            crate::provider::inspect_auth_store().integrity,
            crate::provider::AuthStoreIntegrity::InvalidSchema
        );
        assert_eq!(std::fs::read(&path)?, invalid);

        let corrupt = br#"{"version":1"#;
        std::fs::write(&path, corrupt)?;
        assert_eq!(
            crate::provider::inspect_auth_store().integrity,
            crate::provider::AuthStoreIntegrity::InvalidSchema
        );
        assert_eq!(std::fs::read(&path)?, corrupt);

        let future = br#"{"version":2,"providers":{}}"#;
        std::fs::write(&path, future)?;
        assert_eq!(
            crate::provider::inspect_auth_store().integrity,
            crate::provider::AuthStoreIntegrity::UnsupportedVersion { version: 2 }
        );
        assert_eq!(std::fs::read(&path)?, future);

        std::fs::remove_file(&path)?;
        std::fs::create_dir(&path)?;
        assert_eq!(
            crate::provider::inspect_auth_store().security,
            crate::provider::AuthStoreFileSecurity::NonRegular
        );
        Ok(())
    })();

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(unix)]
#[test]
fn auth_store_inspection_reports_unix_modes_symlinks_and_save_repair() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let _guard = env_lock();
    let root = temp_test_dir("auth-store-inspection-unix");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let path = auth_store_path();
        let bytes = br#"{"version":1,"providers":{}}"#;
        std::fs::write(&path, bytes)?;
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;
        assert_eq!(
            crate::provider::inspect_auth_store().security,
            crate::provider::AuthStoreFileSecurity::Secure { mode: 0o600 }
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640))?;
        assert_eq!(
            crate::provider::inspect_auth_store().security,
            crate::provider::AuthStoreFileSecurity::UnsafeMode { mode: 0o640 }
        );

        let store = AuthStore::default();
        save_auth_store(&store)?;
        assert_eq!(
            crate::provider::inspect_auth_store().security,
            crate::provider::AuthStoreFileSecurity::Secure { mode: 0o600 }
        );

        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o000))?;
        let unreadable = crate::provider::inspect_auth_store();
        assert_eq!(
            unreadable.security,
            crate::provider::AuthStoreFileSecurity::Secure { mode: 0o000 }
        );
        assert_eq!(
            unreadable.integrity,
            crate::provider::AuthStoreIntegrity::Unreadable
        );
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600))?;

        let target = root.join("auth-target.json");
        std::fs::write(&target, bytes)?;
        std::fs::remove_file(&path)?;
        symlink(&target, &path)?;
        let linked = crate::provider::inspect_auth_store();
        assert_eq!(
            linked.security,
            crate::provider::AuthStoreFileSecurity::Symlink
        );
        assert_eq!(
            linked.integrity,
            crate::provider::AuthStoreIntegrity::NotChecked
        );
        Ok(())
    })();

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(unix)]
#[test]
fn secret_writes_create_private_auth_and_oauth_files() -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    let _guard = env_lock();
    let root = temp_test_dir("secret-file-modes");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let standalone = PathBuf::from(&root).join("standalone-secret.json");
        crate::session::atomic_write_secret(&standalone, b"secret")?;
        assert_eq!(
            std::fs::metadata(&standalone)?.permissions().mode() & 0o777,
            0o600
        );

        let mut store = AuthStore::default();
        let auth_path = auth_store_path();
        std::fs::write(&auth_path, b"{}")?;
        let mut old_auth_perms = std::fs::metadata(&auth_path)?.permissions();
        old_auth_perms.set_mode(0o644);
        std::fs::set_permissions(&auth_path, old_auth_perms)?;
        store.providers.insert(
            "glm".to_string(),
            StoredCredential::ApiKey {
                key: "secret-key".to_string(),
            },
        );
        save_auth_store(&store)?;
        assert_eq!(
            std::fs::metadata(&auth_path)?.permissions().mode() & 0o777,
            0o600
        );

        let pending_path = crate::provider::pending_oauth_path();
        std::fs::write(&pending_path, b"{}")?;
        let mut old_pending_perms = std::fs::metadata(&pending_path)?.permissions();
        old_pending_perms.set_mode(0o644);
        std::fs::set_permissions(&pending_path, old_pending_perms)?;
        crate::provider::save_pending_oauth(
            "chatgpt",
            "code-verifier-secret",
            "oauth-state",
            "http://localhost:1455/auth/callback",
        )?;
        assert_eq!(
            std::fs::metadata(&pending_path)?.permissions().mode() & 0o777,
            0o600
        );
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[cfg(unix)]
#[test]
fn runtime_state_loading_repairs_auth_mode_and_rejects_unsafe_provider_state() -> Result<()> {
    use std::os::unix::fs::{PermissionsExt as _, symlink};

    let _guard = env_lock();
    let root = temp_test_dir("runtime-state-load-security");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let auth_path = auth_store_path();
        let auth_bytes = br#"{"version":1,"providers":{}}"#;
        std::fs::write(&auth_path, auth_bytes)?;
        std::fs::set_permissions(&auth_path, std::fs::Permissions::from_mode(0o644))?;
        load_auth_store()?;
        assert_eq!(
            crate::provider::inspect_auth_store().security,
            crate::provider::AuthStoreFileSecurity::Secure { mode: 0o600 }
        );
        assert_eq!(
            std::fs::metadata(&auth_path)?.permissions().mode() & 0o777,
            0o600
        );
        assert_eq!(std::fs::read(&auth_path)?, auth_bytes);

        let provider_path = provider_catalog_path();
        let provider_bytes = std::fs::read(state_fixture_path("providers", "v2.json"))?;
        std::fs::write(&provider_path, &provider_bytes)?;
        std::fs::set_permissions(&provider_path, std::fs::Permissions::from_mode(0o666))?;
        let error = load_provider_catalog()
            .expect_err("group/world-writable provider state must be rejected")
            .to_string();
        assert!(error.contains("unsafe writable mode 0666"), "{error}");
        assert_eq!(
            crate::provider::inspect_provider_catalog().integrity,
            crate::provider::ProviderCatalogIntegrity::UnsafeMode { mode: 0o666 }
        );
        assert_eq!(std::fs::read(&provider_path)?, provider_bytes);

        std::fs::set_permissions(&provider_path, std::fs::Permissions::from_mode(0o600))?;
        load_provider_catalog()?;
        assert!(matches!(
            crate::provider::inspect_provider_catalog().integrity,
            crate::provider::ProviderCatalogIntegrity::Valid { version: 2, .. }
        ));
        let target = root.join("provider-target.json");
        std::fs::write(&target, &provider_bytes)?;
        std::fs::remove_file(&provider_path)?;
        symlink(&target, &provider_path)?;
        let error = load_provider_catalog()
            .expect_err("symlinked provider state must be rejected")
            .to_string();
        assert!(error.contains("regular non-symlink file"), "{error}");
        assert_eq!(
            crate::provider::inspect_provider_catalog().integrity,
            crate::provider::ProviderCatalogIntegrity::Symlink
        );
        assert_eq!(std::fs::read(&target)?, provider_bytes);
        Ok(())
    })();

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn provider_and_auth_state_fixtures_normalize_and_reject_without_rewrite() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("state-provider-auth-fixtures");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let provider_path = provider_catalog_path();
        std::fs::create_dir_all(provider_path.parent().unwrap_or(Path::new(".")))?;
        for fixture in ["v1.json", "v2.json"] {
            let source = state_fixture_path("providers", fixture);
            let bytes = std::fs::read(&source)?;
            std::fs::write(&provider_path, &bytes)?;
            let catalog = load_provider_catalog()?;
            assert_eq!(
                catalog.version,
                crate::provider::default_provider_catalog_version()
            );
            assert_eq!(resolve_active_provider_id(&catalog), "local");
            let local =
                find_provider_profile(&catalog, "local").context("local fixture profile")?;
            assert_eq!(local.default_model, "fixture-model");
            assert_eq!(std::fs::read(&provider_path)?, bytes);
        }
        for fixture in ["future.json", "corrupt.json"] {
            let source = state_fixture_path("providers", fixture);
            let bytes = std::fs::read(&source)?;
            std::fs::write(&provider_path, &bytes)?;
            assert!(
                load_provider_catalog().is_err(),
                "provider fixture {fixture}"
            );
            assert_eq!(std::fs::read(&provider_path)?, bytes);
        }

        let auth_path = auth_store_path();
        for fixture in ["legacy-map.json", "v1.json"] {
            let source = state_fixture_path("auth", fixture);
            let bytes = std::fs::read(&source)?;
            std::fs::write(&auth_path, &bytes)?;
            let store = load_auth_store()?;
            assert_eq!(store.version, crate::provider::default_auth_store_version());
            let Some(StoredCredential::ApiKey { key }) = store.providers.get("openai") else {
                anyhow::bail!("auth fixture missing openai API-key reference");
            };
            assert_eq!(key, "FIXTURE_API_KEY");
            assert_eq!(std::fs::read(&auth_path)?, bytes);
        }
        for fixture in ["future.json", "corrupt.json"] {
            let source = state_fixture_path("auth", fixture);
            let bytes = std::fs::read(&source)?;
            std::fs::write(&auth_path, &bytes)?;
            assert!(load_auth_store().is_err(), "auth fixture {fixture}");
            assert_eq!(std::fs::read(&auth_path)?, bytes);
        }
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn provider_and_auth_future_versions_fail_without_rewriting_source() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("state-future-versions");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
    }

    let result = (|| -> Result<()> {
        let provider_path = provider_catalog_path();
        std::fs::create_dir_all(provider_path.parent().unwrap_or(Path::new(".")))?;
        let provider_bytes = br#"{"version":3,"active_provider":"glm","providers":[]}"#;
        std::fs::write(&provider_path, provider_bytes)?;
        let error = load_provider_catalog()
            .expect_err("future provider catalog must fail")
            .to_string();
        assert!(
            error.contains("unsupported provider catalog version 3"),
            "{error}"
        );
        assert_eq!(std::fs::read(&provider_path)?, provider_bytes);

        let auth_path = auth_store_path();
        let auth_bytes = br#"{"version":2,"providers":{}}"#;
        std::fs::write(&auth_path, auth_bytes)?;
        let error = load_auth_store()
            .expect_err("future auth store must fail")
            .to_string();
        assert!(
            error.contains("unsupported auth store version 2"),
            "{error}"
        );
        assert_eq!(std::fs::read(&auth_path)?, auth_bytes);
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    result
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
            builtin: None,
            display_name: "OpenAI API".to_string(),
            api_provider: ApiProvider::OpenAi,
            request_contract: Some(RequestContract::OpenAiChatCompletions),
            base_url: "https://api.openai.com".to_string(),
            default_model: "gpt-5".to_string(),
            models: vec!["gpt-5".to_string()],
            model_aliases: HashMap::new(),
            model_defaults: ModelSpec::default(),
            model_specs: HashMap::new(),
            env_vars: vec!["OPENAI_API_KEY".to_string()],
            requires_api_key: true,
            login_url: None,
            oauth_flow: None,
            notes: None,
            context_window: None,
            model_context_windows: HashMap::new(),
            model_effort_levels: HashMap::new(),
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
fn login_stores_plain_api_key_for_non_oauth_provider() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("login-glm-plain-key");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::remove_var("ZAI_API_KEY");
        std::env::remove_var("DEXT_API_KEY");
    }

    let result = (|| -> Result<()> {
        // ZAI-style `id.secret` key: no sk-/eyJ prefix, >12 chars, no whitespace.
        // Must be stored as an API key, not misrouted to OAuth callback completion.
        let key = "4f8a0e8c2d6b894fdeadbeef.AbCdEfGh123";
        let msg = login_provider_with_key(Some("glm"), Some(key), false)?;
        assert!(msg.contains("stored credentials"), "{msg}");

        let store = load_auth_store()?;
        let entry = store
            .providers
            .get("glm")
            .context("missing glm credentials in auth store")?;
        let resolved = entry
            .resolve_secret()
            .context("unresolved glm credential")?;
        assert_eq!(resolved, key);
        Ok(())
    })();

    unsafe {
        std::env::remove_var("DEXT_HOME");
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

        let local = resolve_provider_model_selection(
            &catalog,
            &store,
            "glm",
            &format!("local/{DEFAULT_LOCAL_MODEL}"),
        )?;
        assert_eq!(local.provider_id, "local");
        assert_eq!(local.model, DEFAULT_LOCAL_MODEL);

        let qwen_alias = resolve_provider_model_selection(
            &catalog,
            &store,
            "glm",
            &format!("qwen/{DEFAULT_LOCAL_MODEL}"),
        )?;
        assert_eq!(qwen_alias.provider_id, "local");
        assert_eq!(qwen_alias.model, DEFAULT_LOCAL_MODEL);

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
            vec![
                "glm",
                "chatgpt",
                "openai",
                "anthropic",
                "kimi",
                "deepseek",
                "local",
            ]
        );
        let kimi = catalog
            .providers
            .iter()
            .find(|p| p.id == "kimi")
            .expect("kimi provider");
        assert_eq!(kimi.base_url, "https://api.kimi.com/coding");
        assert_eq!(kimi.default_model, "k3");
        assert_eq!(
            request_contract_for_profile(kimi),
            RequestContract::AnthropicMessages
        );
        let local = catalog
            .providers
            .iter()
            .find(|p| p.id == "local")
            .expect("local provider");
        assert_eq!(local.api_provider, ApiProvider::OpenAi);
        assert!(!local.requires_api_key);
        assert_eq!(local.default_model, DEFAULT_LOCAL_MODEL);
        assert_eq!(local.models, vec![DEFAULT_LOCAL_MODEL.to_string()]);
        assert!(local.model_context_windows.is_empty());
        assert_eq!(local.context_window, None);
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
    let ext_auth = root.join(".dext/external-auth.json");
    let old_external_auth_file = std::env::var_os("DEXT_EXTERNAL_AUTH_FILE");
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        std::env::set_var("HOME", &root);
        std::env::set_var("DEXT_EXTERNAL_AUTH_FILE", &ext_auth);
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
    restore_env_var("DEXT_EXTERNAL_AUTH_FILE", old_external_auth_file);
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
    assert_eq!(normalize_chatgpt_model_slug("gpt56"), "gpt-5.6-sol");
    assert_eq!(normalize_chatgpt_model_slug("gpt56terra"), "gpt-5.6-terra");
    assert_eq!(normalize_chatgpt_model_slug("GPT 5 6 LUNA"), "gpt-5.6-luna");
}

#[test]
fn provider_catalog_v1_migrates_builtin_metadata_and_v2_overrides_it() {
    let chatgpt = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "chatgpt")
        .expect("chatgpt profile");
    let openai = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "openai")
        .expect("openai profile");
    assert_eq!(
        normalize_provider_model_value(&chatgpt, "gpt-5.6"),
        "gpt-5.6-sol"
    );
    assert_eq!(
        normalize_provider_model_value(&openai, "gpt-5.6"),
        "gpt-5.6"
    );
    for profile in [&chatgpt, &openai] {
        for (model, input, cached, output) in [
            ("gpt-5.6-sol", 5.0, 0.5, 30.0),
            ("gpt-5.6-terra", 2.5, 0.25, 15.0),
            ("gpt-5.6-luna", 1.0, 0.1, 6.0),
        ] {
            assert!(profile.models.iter().any(|candidate| candidate == model));
            let spec = resolve_model_spec(profile, model);
            assert_eq!(spec.context_window, Some(1_050_000), "{model}");
            assert_eq!(spec.max_output_tokens, Some(128_000), "{model}");
            assert!(spec.tools && spec.reasoning && spec.image_input && spec.prompt_cache);
            assert_eq!(
                spec.effort_levels,
                ["none", "low", "medium", "high", "xhigh"],
                "{model}"
            );
            let pricing = spec.pricing.expect("gpt-5.6 pricing");
            assert_eq!(pricing.input_usd_per_mtok, input, "{model}");
            assert_eq!(pricing.cache_read_usd_per_mtok, cached, "{model}");
            assert_eq!(pricing.output_usd_per_mtok, output, "{model}");
            assert_eq!(pricing.cache_create_usd_per_mtok, input * 1.25, "{model}");
        }
    }
    let alias_spec = resolve_model_spec(&openai, "gpt-5.6");
    assert_eq!(alias_spec.context_window, Some(1_050_000));
    assert_eq!(alias_spec.max_output_tokens, Some(128_000));

    let mut legacy = chatgpt.clone();
    legacy.request_contract = Some(RequestContract::OpenAiChatCompletions);
    legacy.model_aliases = HashMap::from([("gpt-5.6".to_string(), "wrong".to_string())]);
    legacy.model_defaults.max_output_tokens = Some(123);
    let migrated = crate::provider::normalize_provider_catalog(ProviderCatalog {
        version: 1,
        active_provider: "chatgpt".to_string(),
        providers: vec![legacy],
    })
    .expect("legacy catalog migration");
    let migrated = find_provider_profile(&migrated, "chatgpt").expect("migrated chatgpt");
    assert_eq!(
        request_contract_for_profile(&migrated),
        RequestContract::ChatGptResponses
    );
    assert_eq!(
        normalize_provider_model_value(&migrated, "gpt-5.6"),
        "gpt-5.6-sol"
    );
    assert_eq!(
        resolve_model_spec(&migrated, "gpt-5.4").max_output_tokens,
        Some(8_192)
    );

    let mut explicit = chatgpt;
    explicit.request_contract = Some(RequestContract::OpenAiChatCompletions);
    explicit
        .model_aliases
        .insert("fast".to_string(), "custom-fast".to_string());
    explicit.model_defaults.max_output_tokens = Some(1_234);
    explicit.model_defaults.capabilities.tools = Some(false);
    explicit.model_specs.insert(
        "gpt-5.4".to_string(),
        ModelSpec {
            pricing: Some(ModelPricing {
                input_usd_per_mtok: 2.0,
                output_usd_per_mtok: 4.0,
                cache_read_usd_per_mtok: 0.5,
                cache_create_usd_per_mtok: 1.0,
            }),
            ..Default::default()
        },
    );
    let normalized = crate::provider::normalize_provider_catalog(ProviderCatalog {
        version: 2,
        active_provider: "chatgpt".to_string(),
        providers: vec![explicit],
    })
    .expect("explicit catalog normalization");
    let explicit = find_provider_profile(&normalized, "chatgpt").expect("explicit chatgpt");
    assert_eq!(
        request_contract_for_profile(&explicit),
        RequestContract::OpenAiChatCompletions
    );
    assert_eq!(explicit.api_provider, ApiProvider::OpenAi);
    assert_eq!(
        normalize_provider_model_value(&explicit, "fast"),
        "custom-fast"
    );
    let spec = resolve_model_spec(&explicit, "gpt-5.4");
    assert_eq!(spec.max_output_tokens, Some(1_234));
    assert!(!spec.tools);
    assert_eq!(
        spec.pricing.expect("explicit pricing").output_usd_per_mtok,
        4.0
    );
}

#[test]
fn request_capabilities_and_pricing_come_from_active_profile_metadata() -> Result<()> {
    let root = temp_test_dir("profile-request-metadata");
    let root = std::fs::canonicalize(&root)?;
    let mut profile = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "chatgpt")
        .expect("chatgpt profile");
    let spec = profile
        .model_specs
        .entry("gpt-5.4".to_string())
        .or_default();
    spec.max_output_tokens = Some(1_234);
    spec.capabilities = ModelCapabilities {
        tools: Some(false),
        reasoning: Some(false),
        image_input: Some(false),
        prompt_cache: Some(false),
    };
    spec.pricing = Some(ModelPricing {
        input_usd_per_mtok: 2.0,
        output_usd_per_mtok: 4.0,
        cache_read_usd_per_mtok: 0.5,
        cache_create_usd_per_mtok: 1.0,
    });

    let mut agent = test_agent(&root);
    agent.provider_id = profile.id.clone();
    agent.base_url = profile.base_url.clone();
    agent.model = "gpt-5.4".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.provider_profile = Some(profile);
    agent.thinking_effort = ThinkingEffort::XHigh;

    let (url, body) = agent.build_streaming_request("sys", "env", &[], &[], "sess")?;
    let body: Value = serde_json::from_slice(&body)?;
    assert!(url.ends_with("/codex/responses"), "{url}");
    // The codex backend rejects max_output_tokens, so the spec cap must not
    // leak into ChatGPT Responses requests.
    assert!(body.get("max_output_tokens").is_none(), "{body}");
    assert!(body.get("reasoning").is_none(), "{body}");
    assert!(body.get("prompt_cache_key").is_none(), "{body}");
    assert!(body.get("tools").is_none(), "{body}");
    assert!(body.get("tool_choice").is_none(), "{body}");
    assert!(body.get("parallel_tool_calls").is_none(), "{body}");

    let mut usage = Usage {
        input: 1_000_000,
        output: 2_000_000,
        cache_read: 4_000_000,
        cache_create: 3_000_000,
        cost_usd: None,
    };
    agent.finalize_usage_metrics(&mut usage);
    assert_eq!(usage.cost_usd, Some(15.0));
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn provider_health_render_removes_legacy_unknown_status_noise() {
    let mut health = ProviderHealthLedger::default();
    health.providers.insert(
        "chatgpt".to_string(),
        ProviderHealthState {
            auth: "present".to_string(),
            last_error: Some("HTTP 520 <unknown status code>: error code: 520".to_string()),
            mode: Some("chatgpt-responses".to_string()),
            ..Default::default()
        },
    );

    let rendered = render_provider_health_prompt(&health);
    assert!(rendered.contains("last_error=HTTP 520"));
    assert!(!rendered.contains("unknown status code"), "{rendered}");
    assert!(!rendered.contains("error code: 520"), "{rendered}");
}

#[test]
fn provider_health_turn_state_resets_and_success_clears_stale_errors() {
    let root = temp_test_dir("provider-health-turn-reset");
    let mut agent = test_agent(&root);
    let key = agent.provider_health_key();
    agent.provider_health.providers.insert(
        key.clone(),
        ProviderHealthState {
            auth: "present".to_string(),
            last_error: Some("HTTP 520: error code: 520".to_string()),
            retry_after: Some(5),
            mode: Some("chatgpt-responses".to_string()),
            disabled_for_turn: true,
            consecutive_server_errors: 2,
        },
    );

    agent.begin_provider_turn();
    let state = &agent.provider_health.providers[&key];
    assert_eq!(state.retry_after, None);
    assert!(!state.disabled_for_turn);
    assert_eq!(state.consecutive_server_errors, 2);
    assert!(state.last_error.is_some());

    agent.record_provider_success();
    assert!(!agent.provider_health.providers.contains_key(&key));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn provider_http_failure_handles_proxy_auth_and_cloudflare_errors() {
    let root = temp_test_dir("provider-health-http-failure");
    let mut agent = test_agent(&root);
    let key = agent.provider_health_key();

    agent.record_provider_http_failure(
        reqwest::StatusCode::PROXY_AUTHENTICATION_REQUIRED,
        "proxy auth required",
        None,
    );
    let state = &agent.provider_health.providers[&key];
    assert_eq!(state.auth, "failed");
    assert!(state.disabled_for_turn);

    agent.record_provider_http_failure(
        reqwest::StatusCode::from_u16(520).expect("status 520"),
        "error code: 520",
        None,
    );
    let state = &agent.provider_health.providers[&key];
    assert_eq!(state.auth, "present");
    assert_eq!(state.last_error.as_deref(), Some("HTTP 520"));
    assert_eq!(state.consecutive_server_errors, 1);
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn provider_health_key_and_picker_include_route_provenance() {
    let root = temp_test_dir("provider-health-route");
    let mut agent = test_agent(&root);
    agent.provider_id = "OPENAI".to_string();
    agent.api_provider = ApiProvider::OpenAi;
    agent.base_url = "HTTPS://EXAMPLE.TEST/v1/".to_string();
    agent.model = "MODEL-A".to_string();
    let original = agent.provider_health_key();
    assert_eq!(
        original,
        "openai|openai-chat-completions|https://example.test/v1|model-a"
    );
    agent.base_url = "https://other.test/v1".to_string();
    assert_ne!(agent.provider_health_key(), original);
    agent.base_url = "https://example.test/v1".to_string();
    agent.model = "model-b".to_string();
    assert_ne!(agent.provider_health_key(), original);
    agent.model = "MODEL-A".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    assert_ne!(agent.provider_health_key(), original);

    let catalog = crate::provider::default_provider_catalog();
    let store = AuthStore::default();
    let list = render_provider_list(&catalog, &store, "chatgpt");
    let picker = render_provider_picker(&catalog, &store, "chatgpt");
    assert!(list.contains("contract=chatgpt-responses"), "{list}");
    assert!(list.contains("api=chatgpt"), "{list}");
    assert!(list.contains("spec=model"), "{list}");
    assert!(picker.contains("contract=chatgpt-responses"), "{picker}");
    assert!(picker.contains("spec=model"), "{picker}");
    let _ = std::fs::remove_dir_all(&root);
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
fn glm_streaming_request_keeps_enabled_thinking_payload() -> Result<()> {
    let root = temp_test_dir("glm-thinking-enabled");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.provider_id = "glm".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "glm-5.1".to_string();
    agent.thinking_effort = ThinkingEffort::XHigh;
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "sys",
        cache_control: None,
    }];

    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["max_tokens"], 8_192);
    assert_eq!(value["thinking"]["type"], "enabled");
    assert_eq!(value["thinking"]["budget_tokens"], 6_144);
    assert!(value.get("output_config").is_none(), "{value}");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn claude_streaming_request_marks_stable_system_and_tools_cacheable() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("claude-prompt-cache-controls");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.provider_id = "anthropic".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "claude-sonnet-4-6".to_string();
    let sys_blocks = [
        SystemBlock {
            kind: "text",
            text: "stable prompt",
            cache_control: Some(CacheControl::EPHEMERAL),
        },
        SystemBlock {
            kind: "text",
            text: "volatile env",
            cache_control: None,
        },
    ];
    let wire_tools = vec![WireTool {
        name: "read_file".to_string(),
        description: "read".to_string(),
        input_schema: json!({"type":"object","properties":{}}),
        cache_control: Some(CacheControl::EPHEMERAL),
    }];

    let (_, body) = agent.build_streaming_request(
        "stable prompt",
        "volatile env",
        &sys_blocks,
        &wire_tools,
        "unused",
    )?;
    let value: Value = serde_json::from_slice(&body)?;
    assert!(value.get("cache_control").is_none(), "{value}");
    assert_eq!(value["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(value["system"][1].get("cache_control").is_none(), "{value}");
    assert_eq!(value["tools"][0]["cache_control"]["type"], "ephemeral");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn claude_request_sets_sliding_message_breakpoint_and_tail_env() -> Result<()> {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_PROMPT_CACHE");
        std::env::remove_var("DEXT_PROMPT_CACHE_TTL");
    }
    let root = temp_test_dir("claude-sliding-breakpoint");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.provider_id = "anthropic".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "claude-sonnet-4-6".to_string();
    agent.history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "find the bug".to_string(),
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
            content: vec![Block::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "1\tfn main() {}".to_string(),
                is_error: Some(false),
                metadata: ToolResultMetadata::default(),
            }],
        },
    ];
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "stable prompt",
        cache_control: Some(CacheControl::for_prompt()),
    }];

    let (_, body) = agent.build_streaming_request(
        "stable prompt",
        "## Environment\ncwd=/x model=demo",
        &sys_blocks,
        &[],
        "unused",
    )?;
    let value: Value = serde_json::from_slice(&body)?;
    let msgs = value["messages"].as_array().expect("messages");
    assert_eq!(msgs.len(), 3, "{value}");

    // The sliding breakpoint lands on the last persisted block (the tool
    // result), so the whole conversation prefix is cacheable.
    let last_blocks = msgs[2]["content"].as_array().expect("blocks");
    assert_eq!(last_blocks[0]["type"], "tool_result");
    assert_eq!(last_blocks[0]["cache_control"]["type"], "ephemeral");
    assert!(
        last_blocks[0]["cache_control"].get("ttl").is_none(),
        "{value}"
    );

    // The volatile env section rides after the breakpoint as a transient text
    // block — outside every cached prefix, never persisted.
    let env_block = &last_blocks[1];
    assert_eq!(env_block["type"], "text");
    let env_text = env_block["text"].as_str().expect("env text");
    assert!(env_text.starts_with("[dext runtime status"), "{env_text}");
    assert!(env_text.contains("## Environment"), "{env_text}");
    assert!(env_block.get("cache_control").is_none(), "{value}");

    // System carries only the stable block; earlier messages stay untouched.
    assert_eq!(value["system"].as_array().map(Vec::len), Some(1), "{value}");
    assert!(msgs[0]["content"][0].get("cache_control").is_none());
    assert!(msgs[1]["content"][0].get("cache_control").is_none());

    // Stored history is never mutated by wire-level injection.
    assert_eq!(agent.history.len(), 3);
    assert_eq!(agent.history[2].content.len(), 1);

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn sliding_breakpoint_skips_thinking_blocks_and_cache_gate_env_works() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_PROMPT_CACHE");
        std::env::remove_var("DEXT_PROMPT_CACHE_TTL");
    }

    // A trailing thinking block cannot carry a breakpoint; the text before it
    // takes the marker instead.
    let messages = vec![Message {
        role: "assistant".to_string(),
        content: vec![
            Block::Text {
                text: "answer".to_string(),
            },
            Block::Thinking {
                text: "chain".to_string(),
                signature: Some("sig".to_string()),
            },
        ],
    }];
    let wire = anthropic_wire_messages(&messages, true).expect("wire");
    assert!(wire[0]["content"][1].get("cache_control").is_none());
    assert_eq!(wire[0]["content"][0]["cache_control"]["type"], "ephemeral");

    // DEXT_PROMPT_CACHE=on opts a non-claude Anthropic-style provider in;
    // =off strips even claude defaults.
    unsafe { std::env::set_var("DEXT_PROMPT_CACHE", "on") };
    assert!(anthropic_prompt_cache_supported("glm", "glm-5.1"));
    unsafe { std::env::set_var("DEXT_PROMPT_CACHE", "off") };
    assert!(!anthropic_prompt_cache_supported(
        "anthropic",
        "claude-sonnet-4-6"
    ));
    unsafe { std::env::remove_var("DEXT_PROMPT_CACHE") };
    assert!(anthropic_prompt_cache_supported(
        "anthropic",
        "claude-sonnet-4-6"
    ));
    assert!(!anthropic_prompt_cache_supported("glm", "glm-5.1"));

    // DEXT_PROMPT_CACHE_TTL=1h flows into the serialized breakpoint.
    unsafe { std::env::set_var("DEXT_PROMPT_CACHE_TTL", "1h") };
    let cc = serde_json::to_value(CacheControl::for_prompt()).expect("cc");
    assert_eq!(cc["ttl"], "1h");
    unsafe { std::env::remove_var("DEXT_PROMPT_CACHE_TTL") };
    let cc = serde_json::to_value(CacheControl::for_prompt()).expect("cc");
    assert!(cc.get("ttl").is_none());
}

#[test]
fn prompt_cache_env_overrides_attached_profile_metadata_in_requests() -> Result<()> {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_PROMPT_CACHE");
        std::env::remove_var("DEXT_PROMPT_CACHE_TTL");
    }
    let root = temp_test_dir("profile-prompt-cache-env");
    let root = std::fs::canonicalize(&root)?;
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "stable prompt",
        cache_control: Some(CacheControl::EPHEMERAL),
    }];

    let mut agent = test_agent(&root);
    let anthropic = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "anthropic")
        .expect("anthropic profile");
    agent.provider_id = anthropic.id.clone();
    agent.api_provider = anthropic.api_provider;
    agent.model = anthropic.default_model.clone();
    agent.provider_profile = Some(anthropic);
    unsafe { std::env::set_var("DEXT_PROMPT_CACHE", "off") };
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let body: Value = serde_json::from_slice(&body)?;
    assert!(body["system"][0].get("cache_control").is_none(), "{body}");
    assert!(!agent.model_supports_prompt_cache());

    let glm = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "glm")
        .expect("glm profile");
    agent.provider_id = glm.id.clone();
    agent.api_provider = glm.api_provider;
    agent.model = "glm-5.1".to_string();
    agent.provider_profile = Some(glm);
    unsafe { std::env::set_var("DEXT_PROMPT_CACHE", "on") };
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let body: Value = serde_json::from_slice(&body)?;
    assert_eq!(body["system"][0]["cache_control"]["type"], "ephemeral");
    assert!(agent.model_supports_prompt_cache());

    unsafe { std::env::remove_var("DEXT_PROMPT_CACHE") };
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn openai_and_chatgpt_requests_keep_system_stable_and_append_tail_env() -> Result<()> {
    let root = temp_test_dir("oai-stable-system");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::OpenAi;
    agent.model = "deepseek-chat".to_string();
    agent.history = vec![Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "hello".to_string(),
        }],
    }];

    let (_, body) =
        agent.build_streaming_request("stable sys", "## Environment\nx=1", &[], &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    let msgs = value["messages"].as_array().expect("messages");
    // System message carries only the stable text so implicit provider prefix
    // caching survives env churn between tool rounds.
    assert_eq!(msgs[0]["role"], "system");
    assert_eq!(msgs[0]["content"], "stable sys");
    let last = msgs.last().expect("tail env");
    assert_eq!(last["role"], "user");
    let tail = last["content"].as_str().expect("tail content");
    assert!(tail.starts_with("[dext runtime status"), "{tail}");
    assert!(tail.contains("## Environment"), "{tail}");

    agent.api_provider = ApiProvider::ChatGpt;
    agent.model = "gpt-5.4".to_string();
    let (_, body) =
        agent.build_streaming_request("stable sys", "## Environment\nx=1", &[], &[], "sess")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["instructions"], "stable sys");
    let input = value["input"].as_array().expect("input");
    let last = input.last().expect("tail env item");
    assert_eq!(last["role"], "user");
    let tail = last["content"][0]["text"].as_str().expect("tail text");
    assert!(tail.starts_with("[dext runtime status"), "{tail}");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn anthropic_compatible_non_claude_request_strips_cache_controls() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("anthropic-compatible-cache-strip");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.provider_id = "glm".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "glm-5.1".to_string();
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "sys",
        cache_control: Some(CacheControl::EPHEMERAL),
    }];
    let wire_tools = vec![WireTool {
        name: "read_file".to_string(),
        description: "read".to_string(),
        input_schema: json!({"type":"object","properties":{}}),
        cache_control: Some(CacheControl::EPHEMERAL),
    }];

    let (_, body) =
        agent.build_streaming_request("sys", "env", &sys_blocks, &wire_tools, "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert!(value["system"][0].get("cache_control").is_none(), "{value}");
    assert!(value["tools"][0].get("cache_control").is_none(), "{value}");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn claude_anthropic_streaming_request_uses_adaptive_thinking_output_config() -> Result<()> {
    let root = temp_test_dir("claude-adaptive-thinking");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.provider_id = "anthropic".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "claude-opus-4-1".to_string();
    agent.thinking_effort = ThinkingEffort::Max;
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "sys",
        cache_control: None,
    }];
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["thinking"]["type"], "enabled");
    assert_eq!(value["thinking"]["budget_tokens"], 6_144);
    assert!(value.get("output_config").is_none(), "{value}");

    agent.model = "claude-sonnet-4-6".to_string();
    agent.thinking_effort = ThinkingEffort::Medium;
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["thinking"]["type"], "adaptive");
    assert!(value["thinking"].get("display").is_none(), "{value}");
    assert!(value["thinking"].get("budget_tokens").is_none(), "{value}");
    assert_eq!(value["output_config"]["effort"], "medium");

    agent.model = "claude-opus-4-8".to_string();
    agent.thinking_effort = ThinkingEffort::XHigh;
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["thinking"]["type"], "adaptive");
    assert_eq!(value["thinking"]["display"], "omitted");
    assert_eq!(value["output_config"]["effort"], "xhigh");

    agent.model = "claude-fable-5".to_string();
    agent.thinking_effort = ThinkingEffort::Max;
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["thinking"]["type"], "adaptive");
    assert_eq!(value["thinking"]["display"], "omitted");
    assert_eq!(value["output_config"]["effort"], "max");

    agent.model = "claude-opus-4-1".to_string();
    agent.thinking_effort = ThinkingEffort::Off;
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert!(value.get("thinking").is_none(), "{value}");
    assert!(value.get("output_config").is_none(), "{value}");

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn kimi_builtin_metadata_is_isolated_from_existing_provider_profiles() {
    let profiles = built_in_provider_profiles();
    for (id, api, contract, base_url, default_model) in [
        (
            "glm",
            ApiProvider::Anthropic,
            RequestContract::AnthropicMessages,
            "https://api.z.ai/api/anthropic",
            "glm-5.2[1m]",
        ),
        (
            "chatgpt",
            ApiProvider::ChatGpt,
            RequestContract::ChatGptResponses,
            "https://chatgpt.com/backend-api/codex",
            "gpt-5.4",
        ),
        (
            "openai",
            ApiProvider::OpenAi,
            RequestContract::OpenAiChatCompletions,
            "https://api.openai.com",
            "gpt-5",
        ),
        (
            "anthropic",
            ApiProvider::Anthropic,
            RequestContract::AnthropicMessages,
            "https://api.anthropic.com",
            "claude-sonnet-4-6",
        ),
        (
            "deepseek",
            ApiProvider::OpenAi,
            RequestContract::OpenAiChatCompletions,
            "https://api.deepseek.com",
            "deepseek-chat",
        ),
        (
            "local",
            ApiProvider::OpenAi,
            RequestContract::OpenAiChatCompletions,
            "http://127.0.0.1:8080",
            DEFAULT_LOCAL_MODEL,
        ),
    ] {
        let profile = profiles
            .iter()
            .find(|profile| profile.id == id)
            .unwrap_or_else(|| panic!("missing {id} profile"));
        assert_eq!(profile.api_provider, api, "{id}");
        assert_eq!(request_contract_for_profile(profile), contract, "{id}");
        assert_eq!(profile.base_url, base_url, "{id}");
        assert_eq!(profile.default_model, default_model, "{id}");
    }

    let kimi = profiles
        .iter()
        .find(|profile| profile.id == "kimi")
        .expect("kimi profile");
    assert_eq!(kimi.base_url, "https://api.kimi.com/coding");
    assert_eq!(kimi.env_vars, ["KIMI_API_KEY"]);
    assert_eq!(kimi.default_model, "k3");
    assert!(kimi.oauth_flow.is_none());
    let k3 = resolve_model_spec(kimi, "k3");
    assert_eq!(k3.context_window, Some(1_048_576));
    assert_eq!(k3.max_output_tokens, Some(131_072));
    assert_eq!(k3.effort_levels, ["max"]);
    let k3_pricing = k3.pricing.expect("Kimi coding-plan pricing");
    assert_eq!(k3_pricing.input_usd_per_mtok, 0.0);
    assert_eq!(k3_pricing.output_usd_per_mtok, 0.0);
    assert_eq!(k3_pricing.cache_read_usd_per_mtok, 0.0);
    assert_eq!(k3_pricing.cache_create_usd_per_mtok, 0.0);
    let legacy = resolve_model_spec(kimi, "k2p7");
    assert_eq!(legacy.context_window, Some(262_144));
    assert_eq!(legacy.max_output_tokens, Some(32_768));
    assert_eq!(
        legacy
            .pricing
            .expect("Kimi coding-plan pricing")
            .output_usd_per_mtok,
        0.0
    );
}

#[test]
fn kimi_custom_profile_collision_requires_rename_without_rewrite() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("kimi-profile-collision");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };

    let result = (|| -> Result<()> {
        let legacy = crate::provider::normalize_provider_catalog(ProviderCatalog {
            version: 1,
            active_provider: "local".to_string(),
            providers: vec![
                built_in_provider_profiles()
                    .into_iter()
                    .find(|profile| profile.id == "local")
                    .expect("local profile"),
            ],
        })?;
        let kimi = find_provider_profile(&legacy, "kimi").expect("migrated Kimi profile");
        assert!(is_official_kimi_profile(&kimi, &kimi.base_url));

        for id in ["kimi", "kimi-code", "kimi-coding", "kimi-membership"] {
            let mut custom = kimi.clone();
            custom.id = id.to_string();
            custom.builtin = None;
            custom.display_name = "Existing custom profile".to_string();
            custom.base_url = "https://example.test/custom".to_string();
            let error = crate::provider::normalize_provider_catalog(ProviderCatalog {
                version: 2,
                active_provider: id.to_string(),
                providers: vec![custom],
            })
            .expect_err("reserved unmarked Kimi id must not be captured");
            assert!(
                error.to_string().contains("rename the custom profile"),
                "{error:#}"
            );
        }

        let path = provider_catalog_path();
        std::fs::create_dir_all(path.parent().unwrap_or(Path::new(".")))?;
        let bytes = br#"{"version":2,"active_provider":"kimi","providers":[{"id":"kimi","display_name":"Existing custom profile","api_provider":"anthropic","request_contract":"anthropic-messages","base_url":"https://example.test/custom","default_model":"custom-k3","models":["custom-k3"],"env_vars":["CUSTOM_KIMI_KEY"],"requires_api_key":true}]}"#;
        std::fs::write(&path, bytes)?;
        let error = load_provider_catalog().expect_err("custom Kimi collision must fail on load");
        assert!(
            error.to_string().contains("rename the custom profile"),
            "{error:#}"
        );
        assert_eq!(std::fs::read(&path)?, bytes);
        Ok(())
    })();

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn kimi_headers_use_plan_api_keys_and_isolate_official_metadata() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("kimi-headers");
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };
    let client = reqwest::Client::new();
    let result = (|| -> Result<()> {
        let api_key = apply_provider_headers(
            client.post("https://api.kimi.com/coding/v1/messages"),
            RequestContract::AnthropicMessages,
            "kimi-key",
            true,
            None,
        )?
        .build()?;
        assert_eq!(api_key.headers()["x-api-key"], "kimi-key");
        assert!(!api_key.headers().contains_key("authorization"));
        assert_eq!(
            api_key.headers()["anthropic-version"],
            ANTHROPIC_API_VERSION
        );
        assert_eq!(api_key.headers()["x-msh-platform"], "kimi_code_cli");
        assert!(api_key.headers().contains_key("x-msh-device-id"));
        assert_eq!(
            api_key.headers()["user-agent"],
            format!("dext/{}", env!("CARGO_PKG_VERSION"))
        );

        let custom = apply_provider_headers(
            client.post("https://example.test/v1/messages"),
            RequestContract::AnthropicMessages,
            "custom-key",
            false,
            None,
        )?
        .build()?;
        assert_eq!(custom.headers()["x-api-key"], "custom-key");
        assert!(!custom.headers().contains_key("authorization"));
        assert!(!custom.headers().contains_key("x-msh-platform"));
        assert!(!custom.headers().contains_key("x-msh-device-id"));
        Ok(())
    })();
    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn kimi_runtime_auth_uses_plan_api_keys_and_ignores_stale_oauth() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("kimi-runtime-auth");
    let env_names = [
        "DEXT_HOME",
        "DEXT_API_KEY",
        "DEXT_BASE_URL",
        "DEXT_MODEL",
        "DEXT_MODEL_KIMI",
        "DEXT_MODEL_FORCE",
        "ANTHROPIC_BASE_URL",
        "KIMI_API_KEY",
    ];
    let previous = env_names.map(|name| (name, std::env::var_os(name)));
    unsafe {
        std::env::set_var("DEXT_HOME", &root);
        for name in &env_names[1..] {
            std::env::remove_var(name);
        }
    }

    let result = (|| -> Result<()> {
        let login = login_provider(Some("kimi"), Some("static-kimi-key"), false)?;
        assert!(!login.awaiting_credentials);
        assert!(
            login.message.contains("stored credentials"),
            "{}",
            login.message
        );
        let resolved = resolve_runtime_provider(Some("kimi"), true)?;
        assert_eq!(resolved.key_source, "auth:kimi");
        assert!(is_official_kimi_profile(
            &resolved.profile,
            &resolved.base_url
        ));

        unsafe { std::env::set_var("DEXT_BASE_URL", "https://example.test/kimi") };
        let custom_key = resolve_runtime_provider(Some("kimi"), true)?;
        assert_eq!(custom_key.base_url, "https://example.test/kimi");

        unsafe { std::env::remove_var("DEXT_BASE_URL") };
        let mut store = load_auth_store()?;
        store.providers.insert(
            "kimi".to_string(),
            StoredCredential::OAuth {
                access_token: "oauth-access".to_string(),
                refresh_token: Some("oauth-refresh".to_string()),
                expires_at: Some(unix_timestamp_secs().saturating_add(3_600)),
            },
        );
        save_auth_store(&store)?;
        let error = resolve_runtime_provider(Some("kimi"), true)
            .expect_err("legacy Kimi OAuth credentials must not be used");
        assert!(
            error.to_string().contains("missing credentials"),
            "{error:#}"
        );
        let store = load_auth_store()?;
        assert_eq!(provider_auth_status(&resolved.profile, &store), "missing");
        Ok(())
    })();

    for (name, value) in previous {
        restore_env_var(name, value);
    }
    let _ = std::fs::remove_dir_all(&root);
    result
}

#[test]
fn official_kimi_k3_request_uses_adaptive_thinking_and_empty_signatures_only_there() -> Result<()> {
    let root = temp_test_dir("kimi-k3-request");
    let root = std::fs::canonicalize(&root)?;
    let kimi = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "kimi")
        .expect("kimi profile");
    let mut agent = test_agent(&root);
    agent.provider_id = kimi.id.clone();
    agent.provider_profile = Some(kimi.clone());
    agent.api_provider = kimi.api_provider;
    agent.base_url = kimi.base_url.clone();
    agent.model = "k3".to_string();
    agent.thinking_effort = ThinkingEffort::Max;
    agent.history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "inspect".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![
                Block::Thinking {
                    text: "reasoning".to_string(),
                    signature: Some(String::new()),
                },
                Block::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path":"src/main.rs"}),
                },
            ],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "ok".to_string(),
                is_error: Some(false),
                metadata: ToolResultMetadata::default(),
            }],
        },
    ];
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "sys",
        cache_control: None,
    }];

    let (url, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    assert_eq!(url, "https://api.kimi.com/coding/v1/messages");
    let body: Value = serde_json::from_slice(&body)?;
    assert_eq!(body["max_tokens"], 131_072);
    assert_eq!(body["thinking"]["type"], "adaptive");
    assert!(body["thinking"].get("budget_tokens").is_none(), "{body}");
    assert_eq!(body["output_config"]["effort"], "max");
    let assistant = body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "assistant")
        })
        .expect("assistant message");
    assert!(assistant["content"].as_array().is_some_and(|blocks| {
        blocks.iter().any(|block| {
            block["type"] == "thinking"
                && block["thinking"] == "reasoning"
                && block["signature"] == ""
        })
    }));

    let mut custom = kimi;
    custom.base_url = "https://example.test/kimi".to_string();
    agent.provider_profile = Some(custom.clone());
    agent.base_url = custom.base_url;
    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let body: Value = serde_json::from_slice(&body)?;
    assert_eq!(body["thinking"]["type"], "enabled");
    assert_eq!(body["output_config"]["effort"], "max");
    let assistant = body["messages"]
        .as_array()
        .and_then(|messages| {
            messages
                .iter()
                .find(|message| message["role"] == "assistant")
        })
        .expect("assistant message");
    assert!(
        !assistant["content"]
            .as_array()
            .is_some_and(|blocks| { blocks.iter().any(|block| block["type"] == "thinking") })
    );

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn max_output_tokens_reads_positive_env_override() {
    let _guard = env_lock();
    unsafe {
        std::env::remove_var("DEXT_MAX_OUTPUT_TOKENS");
    }
    assert_eq!(max_output_tokens_for(None), 8_192);

    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "1234");
    }
    assert_eq!(max_output_tokens_for(None), 1_234);

    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "0");
    }
    assert_eq!(max_output_tokens_for(None), 8_192);

    unsafe {
        std::env::set_var("DEXT_MAX_OUTPUT_TOKENS", "not-a-number");
    }
    assert_eq!(max_output_tokens_for(None), 8_192);

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
fn anthropic_request_roundtrips_signed_and_redacted_thinking_blocks() -> Result<()> {
    let root = temp_test_dir("anthropic-thinking-roundtrip");
    let root = std::fs::canonicalize(&root)?;
    let mut agent = test_agent(&root);
    agent.provider_id = "anthropic".to_string();
    agent.api_provider = ApiProvider::Anthropic;
    agent.model = "claude-sonnet-4-6".to_string();
    agent.thinking_effort = ThinkingEffort::High;
    agent.history = vec![Message {
        role: "assistant".to_string(),
        content: vec![
            Block::Thinking {
                text: String::new(),
                signature: Some("sig-full".to_string()),
            },
            Block::RedactedThinking {
                data: "opaque-redacted".to_string(),
            },
            Block::Thinking {
                text: "legacy unsigned should be stripped".to_string(),
                signature: None,
            },
            Block::Text {
                text: "answer".to_string(),
            },
        ],
    }];
    let sys_blocks = [SystemBlock {
        kind: "text",
        text: "sys",
        cache_control: None,
    }];

    let (_, body) = agent.build_streaming_request("sys", "env", &sys_blocks, &[], "unused")?;
    let value: Value = serde_json::from_slice(&body)?;
    assert_eq!(value["thinking"]["type"], "adaptive");
    let content = value["messages"][0]["content"]
        .as_array()
        .context("content")?;
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "");
    assert_eq!(content[0]["signature"], "sig-full");
    assert!(content[0].get("text").is_none(), "{}", content[0]);
    assert_eq!(content[1]["type"], "redacted_thinking");
    assert_eq!(content[1]["data"], "opaque-redacted");
    assert_eq!(content[2]["type"], "text");
    assert_eq!(
        content.len(),
        3,
        "unsigned legacy thinking must be stripped: {content:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
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

    let body =
        build_chatgpt_summary_request(&agent.model, COMPACT_SYSTEM, "resume this work", true);
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

    let body = build_chatgpt_summary_request("gpt-4o", COMPACT_SYSTEM, "resume this work", false);
    assert!(body.get("reasoning").is_none(), "{body}");

    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn chatgpt_compact_summary_honors_summary_model_reasoning_capability() {
    let _guard = env_lock();
    let old_compact_model = std::env::var_os("DEXT_COMPACT_MODEL");
    unsafe { std::env::set_var("DEXT_COMPACT_MODEL", "gpt-4o") };
    let root = temp_test_dir("chatgpt-summary-non-reasoning-model");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = std::sync::mpsc::channel();
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
            header_end = request.windows(4).position(|window| window == b"\r\n\r\n");
        }
        let header_end = header_end.expect("header terminator") + 4;
        let headers = String::from_utf8_lossy(&request[..header_end]).to_string();
        let content_length = headers
            .lines()
            .find_map(|line| {
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|value| value.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let n = stream.read(&mut buf).expect("read request body");
            assert!(n > 0, "client closed before sending body");
            request.extend_from_slice(&buf[..n]);
        }
        tx.send(request[header_end..header_end + content_length].to_vec())
            .expect("send request body");
        let body = "data: {\"type\":\"response.output_text.delta\",\"delta\":\"Summarized.\"}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });

    let mut agent = test_agent(&root);
    let profile = built_in_provider_profiles()
        .into_iter()
        .find(|profile| profile.id == "chatgpt")
        .expect("chatgpt profile");
    agent.provider_id = profile.id.clone();
    agent.api_provider = profile.api_provider;
    agent.provider_profile = Some(profile);
    agent.base_url = format!("http://{addr}");
    agent.model = "gpt-5.4".to_string();
    agent.api_key = "eyJhbGciOiJIUzI1NiJ9.eyJodHRwczovL2FwaS5vcGVuYWkuY29tL2F1dGgiOnsiY2hhdGdwdF9hY2NvdW50X2lkIjoiYWNjdF90ZXN0In19.signature".to_string();
    let old = vec![Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "old context".to_string(),
        }],
    }];

    let (summary, _) = agent
        .one_shot_summary(&old, "")
        .await
        .expect("summary request should complete");
    assert_eq!(summary, "Summarized.");
    let body: Value = serde_json::from_slice(&rx.recv().expect("request body")).expect("json body");
    assert_eq!(body["model"], "gpt-4o");
    assert!(body.get("reasoning").is_none(), "{body}");

    server.join().expect("server thread");
    restore_env_var("DEXT_COMPACT_MODEL", old_compact_model);
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
async fn anthropic_stream_omitted_thinking_preserves_signature_for_roundtrip() {
    let root = temp_test_dir("anthropic-omitted-thinking-stream");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("set request timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buf).expect("read request headers");
            assert!(read > 0, "client closed before sending request headers");
            request.extend_from_slice(&buf[..read]);
        }
        let body = "event: message_start\ndata: {\"type\":\"message_start\",\"message\":{\"usage\":{}}}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig-full\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":0}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"redacted_thinking\",\"data\":\"opaque-redacted\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":1}\n\nevent: content_block_start\ndata: {\"type\":\"content_block_start\",\"index\":2,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\nevent: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":2,\"delta\":{\"type\":\"text_delta\",\"text\":\"answer\"}}\n\nevent: content_block_stop\ndata: {\"type\":\"content_block_stop\",\"index\":2}\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
    });

    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::Anthropic;
    let resp = reqwest::get(format!("http://{addr}/stream"))
        .await
        .expect("response");
    let (blocks, _stop, _usage) = agent.read_stream(resp).await.expect("parse stream");
    assert!(
        matches!(
            blocks.first(),
            Some(Block::Thinking { text, signature: Some(signature) }) if text.is_empty() && signature == "sig-full"
        ),
        "{blocks:?}"
    );
    assert!(
        matches!(
            blocks.get(1),
            Some(Block::RedactedThinking { data }) if data == "opaque-redacted"
        ),
        "{blocks:?}"
    );
    assert!(matches!(blocks.get(2), Some(Block::Text { text }) if text == "answer"));
    server.join().expect("server thread");
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
        stream
            .set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .expect("set request timeout");
        let mut request = Vec::new();
        let mut buf = [0u8; 1024];
        while !request.windows(4).any(|window| window == b"\r\n\r\n") {
            let read = stream.read(&mut buf).expect("read request headers");
            assert!(read > 0, "client closed before sending request headers");
            request.extend_from_slice(&buf[..read]);
        }
        let body = "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"**Planning removal**\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"**Planning masked login input**\\n\\n<!-- \"}\n\ndata: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"-->**Verifying restored history**\\n\\n<!-- -->\"}\n\ndata: {\"type\":\"response.reasoning_summary_text.done\",\"text\":\"ignored because delta already populated\"}\n\ndata: {\"type\":\"response.output_text.delta\",\"delta\":\"answer\"}\n\ndata: {\"type\":\"response.completed\",\"response\":{\"status\":\"completed\",\"output\":[]}}\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
    });

    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::ChatGpt;
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));
    let resp = reqwest::get(format!("http://{addr}/stream"))
        .await
        .expect("response");
    let (blocks, _stop, _usage) = agent.read_stream_chatgpt(resp).await.expect("parse stream");
    assert!(
        matches!(
            blocks.first(),
            Some(Block::Thinking { text, .. })
                if text == "**Planning removal**\n\n**Planning masked login input**\n\n**Verifying restored history**"
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
    let events = drain_events(&mut rx);
    let streamed_thinking = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ThinkingDelta(text) => Some(text.as_str()),
            _ => None,
        })
        .collect::<String>();
    assert_eq!(
        streamed_thinking,
        "**Planning removal**\n\n**Planning masked login input**\n\n**Verifying restored history**"
    );
    assert!(events.iter().any(|event| matches!(
        event,
        AgentEvent::ThinkingBlockComplete(text)
            if text == "**Planning removal**\n\n**Planning masked login input**\n\n**Verifying restored history**"
    )));
    assert!(!events.iter().any(|event| matches!(
        event,
        AgentEvent::ThinkingBlockComplete(text) if text.contains("<!--")
    )));
    server.join().expect("server thread");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn reasoning_summary_normalization_only_removes_empty_separator_comments() {
    assert_eq!(
        normalize_reasoning_summary_text("first\n\n<!-- -->second\n\n<!--   -->"),
        "first\n\nsecond"
    );
    assert_eq!(
        normalize_reasoning_summary_text("first  \n<!-- keep this -->\n`code  `\n"),
        "first  \n<!-- keep this -->\n`code  `\n"
    );
    assert_eq!(
        normalize_reasoning_summary_text("**first**<!-- -->**second****third**"),
        "**first**\n\n**second**\n\n**third**"
    );
    assert_eq!(
        normalize_reasoning_summary_text("**first****second****third**"),
        "**first**\n\n**second**\n\n**third**"
    );
    assert_eq!(
        normalize_reasoning_summary_text("**first** **second**"),
        "**first** **second**"
    );
    assert_eq!(
        normalize_reasoning_summary_text("**first** and **second**"),
        "**first** and **second**"
    );
    assert_eq!(
        normalize_reasoning_summary_text("first\n\n<!-- -->  indented"),
        "first\n\n  indented"
    );
    assert_eq!(
        normalize_reasoning_summary_text("`**first****second**`"),
        "`**first****second**`"
    );
    assert_eq!(
        normalize_reasoning_summary_text("paragraph\n\n`<!-- -->`"),
        "paragraph\n\n`<!-- -->`"
    );
    assert_eq!(
        normalize_reasoning_summary_text("inline <!-- --> marker\n`<!-- -->`\n"),
        "inline <!-- --> marker\n`<!-- -->`\n"
    );
    assert_eq!(
        normalize_reasoning_summary_text("first\n\n<!-- unfinished"),
        "first\n\n<!-- unfinished"
    );
}

#[test]
fn restored_chatgpt_reasoning_removes_only_empty_separator_comments() -> Result<()> {
    let root = temp_test_dir("restored-chatgpt-reasoning");
    let root = std::fs::canonicalize(&root)?;
    let chatgpt_path = root.join("chatgpt.jsonl");
    let anthropic_path = root.join("anthropic.jsonl");
    let raw_thinking = "**Planning removal**<!-- -->**Planning masked login input****Verifying restored history**\n\n<!-- -->";
    let message = Message {
        role: "assistant".to_string(),
        content: vec![
            Block::Thinking {
                text: raw_thinking.to_string(),
                signature: None,
            },
            Block::Text {
                text: "ordinary <!-- --> marker".to_string(),
            },
        ],
    };

    let mut header = SessionHeader {
        model: "gpt-5.6-sol".to_string(),
        provenance: SessionProvenance {
            provider: "chatgpt".to_string(),
            api_provider: ApiProvider::ChatGpt,
            model: "gpt-5.6-sol".to_string(),
            ..Default::default()
        },
        ..Default::default()
    };
    std::fs::write(
        &chatgpt_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header)?,
            serde_json::to_string(&message)?
        ),
    )?;
    let mut agent = test_agent(&root);
    agent.load_session_from_path(&chatgpt_path)?;
    assert!(matches!(
        &agent.history[0].content[0],
        Block::Thinking { text, .. }
            if text == "**Planning removal**\n\n**Planning masked login input**\n\n**Verifying restored history**"
    ));
    assert!(matches!(
        &agent.history[0].content[1],
        Block::Text { text } if text == "ordinary <!-- --> marker"
    ));

    header.provenance.provider = "anthropic".to_string();
    header.provenance.api_provider = ApiProvider::Anthropic;
    std::fs::write(
        &anthropic_path,
        format!(
            "{}\n{}\n",
            serde_json::to_string(&header)?,
            serde_json::to_string(&message)?
        ),
    )?;
    agent.load_session_from_path(&anthropic_path)?;
    assert!(matches!(
        &agent.history[0].content[0],
        Block::Thinking { text, .. } if text == raw_thinking
    ));
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
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
#[allow(clippy::await_holding_lock)]
async fn active_compaction_runs_after_tool_results_when_history_crosses_active_threshold() {
    let root = temp_test_dir("active-compact-tool-results");
    let _guard = env_lock();
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };
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

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
#[allow(clippy::await_holding_lock)]
async fn compact_uses_deterministic_evidence_fallback_when_summary_request_errors() {
    // Regression: if the summary HTTP call fails mid-compact, deterministic evidence
    // should still allow compaction to finish so the TUI clears its spinner.
    let root = temp_test_dir("compact-failed-event");
    let _guard = env_lock();
    let old_dext_home = std::env::var_os("DEXT_HOME");
    unsafe { std::env::set_var("DEXT_HOME", &root) };
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

    restore_env_var("DEXT_HOME", old_dext_home);
    let _ = std::fs::remove_dir_all(&root);
}

#[tokio::test(flavor = "current_thread")]
async fn read_only_plan_suppresses_internal_planner_events_hooks_and_restores_sink() {
    let root = temp_test_dir("plan-silent-sink");
    let root = std::fs::canonicalize(&root).expect("canonical temp dir");
    std::fs::write(
        root.join("hooks.json"),
        r#"{"user_prompt":[{"match":"*","command":"printf fired > hook-fired"}]}"#,
    )
    .expect("write hooks");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let server = std::thread::spawn(move || {
        let (mut stream, _) = listener.accept().expect("accept");
        let mut request = [0u8; 4096];
        let _ = stream.read(&mut request);
        let body = "data: {\"choices\":[{\"delta\":{\"content\":\"plan text\"},\"finish_reason\":null}]}\n\ndata: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}]}\n\ndata: [DONE]\n\n";
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nContent-Length: {}\r\n\r\n{}",
            body.len(),
            body
        );
        std::io::Write::write_all(&mut stream, response.as_bytes()).expect("write response");
    });

    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::OpenAi;
    agent.provider_id = "local".to_string();
    agent.provider_requires_api_key = false;
    agent.api_key.clear();
    agent.base_url = format!("http://{addr}");
    agent.model = DEFAULT_LOCAL_MODEL.to_string();
    agent.hooks = Hooks::load(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    let plan = agent
        .generate_read_only_plan("write a plan")
        .await
        .expect("plan completes");
    assert_eq!(plan, "plan text");
    assert!(
        drain_events(&mut rx).is_empty(),
        "internal planner events must not leak to the active sink"
    );
    assert!(
        !root.join("hook-fired").exists(),
        "planning must not fire user_prompt hooks"
    );

    agent.sink.emit(AgentEvent::Slash("restored".to_string()));
    assert!(
        drain_events(&mut rx)
            .into_iter()
            .any(|event| matches!(event, AgentEvent::Slash(text) if text == "restored")),
        "original sink should be restored after planning"
    );

    server.join().expect("server thread");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn packs_discover_user_global_pack_from_dext_home() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("user-pack-discovery-root");
    let home = temp_test_dir("user-pack-discovery-home");
    let pack_dir = home.join("shelves/personal/packs/globaldemo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: globaldemo\ndescription: User-global workflow\n---\n# Global demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", &home);
    }

    let pack = packs::find_pack(&root, "globaldemo")?;
    assert_eq!(pack.name, "globaldemo");
    assert_eq!(pack.description, "User-global workflow");
    assert_eq!(pack.source, "user:~/.dext/shelves/personal");
    assert_eq!(pack.shelf.as_deref(), Some("personal"));
    assert_eq!(pack.path, pack_dir);

    let listing = packs::render_pack_listing(&root);
    assert!(listing.contains("globaldemo"), "{listing}");
    assert!(listing.contains("User-global workflow"), "{listing}");
    assert!(listing.contains("source:"), "{listing}");
    assert!(listing.contains("Use:"), "{listing}");

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
    let pack_dir = root.join(".dext/shelves/project/packs/demo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: demo\ndescription: Demo workflow\ncredential-env: [X_AUTH_TOKEN, X_CT0, OPENAI_API_KEY, invalid-name]\n---\n# Demo pack\n\nDo the demo workflow.\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let pack = packs::find_pack(&root, "demo")?;
    assert_eq!(pack.name, "demo");
    assert_eq!(pack.description, "Demo workflow");
    assert_eq!(pack.env_var_name(), "DEXT_PACK_DEMO_DIR");
    assert!(
        pack.credential_env.is_empty(),
        "project packs cannot enable inherited credentials"
    );
    assert!(pack.credential_env_ignored);
    let inspect = packs::render_pack_inspect(&root, "demo")?;
    assert!(
        inspect.contains(
            "credential env: (ignored: project-local packs cannot inherit parent credentials)"
        ),
        "{inspect}"
    );
    assert!(pack.pack_md_path.ends_with("PACK.md"));

    let listing = packs::render_pack_listing(&root);
    assert!(listing.contains("demo"), "{listing}");
    assert!(listing.contains("Demo workflow"), "{listing}");
    assert!(listing.contains("Use:"), "{listing}");
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
fn user_global_pack_preserves_helper_credential_declarations() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("user-pack-credential-declaration");
    let home = root.join("home");
    let pack_dir = home.join("shelves/trusted/packs/trusted-helper");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: trusted-helper\ncredential-env: [SERVICE_TOKEN]\n---\n# Trusted helper\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", &home);
    }

    let pack = packs::find_pack(&root, "trusted-helper")?;
    assert_eq!(pack.source, "user:~/.dext/shelves/trusted");
    assert_eq!(pack.shelf.as_deref(), Some("trusted"));
    assert_eq!(pack.credential_env, vec!["SERVICE_TOKEN"]);
    assert!(!pack.credential_env_ignored);

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn project_pack_shadowing_user_pack_never_inherits_user_pack_credentials() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("project-pack-shadows-user-credential-pack");
    let home = root.join("home");
    let project_pack = root.join(".dext/shelves/project/packs/demo");
    let user_pack = home.join("shelves/user/packs/demo");
    std::fs::create_dir_all(&project_pack)?;
    std::fs::create_dir_all(&user_pack)?;
    std::fs::write(
        project_pack.join("PACK.md"),
        "---\nname: demo\ncredential-env: [SERVICE_TOKEN]\n---\n# Project demo\n",
    )?;
    std::fs::write(
        user_pack.join("PACK.md"),
        "---\nname: demo\ncredential-env: [SERVICE_TOKEN]\n---\n# User demo\n",
    )?;
    let old_dext_home = std::env::var_os("DEXT_HOME");
    let old_shelves_dir = std::env::var_os("DEXT_SHELVES_DIR");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
        std::env::remove_var("DEXT_SHELVES_DIR");
    }

    let pack = packs::find_pack(&root, "demo")?;
    assert_eq!(pack.path, project_pack);
    assert!(pack.source.starts_with("project:"), "{}", pack.source);
    assert!(pack.credential_env.is_empty());
    assert!(pack.credential_env_ignored);

    restore_env_var("DEXT_HOME", old_dext_home);
    restore_env_var("DEXT_SHELVES_DIR", old_shelves_dir);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn packs_discovery_is_deterministic_and_dedupes_symlinked_shelf_roots() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-deterministic-discovery");
    let shelves_root = root.join("shared-shelves");
    let pack_root = shelves_root.join("community/packs");
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
    let alias = root.join("alias-shelves");
    #[cfg(unix)]
    std::os::unix::fs::symlink(&shelves_root, &alias)?;
    #[cfg(windows)]
    std::os::windows::fs::symlink_dir(&shelves_root, &alias)?;
    unsafe {
        std::env::set_var(
            "DEXT_SHELVES_DIR",
            std::env::join_paths([&alias, &shelves_root])?,
        );
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let packs = packs::discover_packs(&root);
    let names = packs
        .iter()
        .filter(|pack| pack.source.starts_with("env:DEXT_SHELVES_DIR"))
        .map(|pack| pack.name.as_str())
        .collect::<Vec<_>>();
    assert_eq!(names, vec!["alpha", "beta"], "{names:?}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn pack_create_scaffolds_user_and_project_shelf_packs() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-create");
    let home = root.join("home");
    let old_home = std::env::var_os("DEXT_HOME");
    let old_shelves = std::env::var_os("DEXT_SHELVES_DIR");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
        std::env::remove_var("DEXT_SHELVES_DIR");
    }

    let user_pack = packs::create_pack(&root, "personal/code-review", false)?;
    assert_eq!(user_pack, home.join("shelves/personal/packs/code-review"));
    let user_workflow = std::fs::read_to_string(user_pack.join("PACK.md"))?;
    assert!(
        user_workflow.contains("name: code-review"),
        "{user_workflow}"
    );
    assert!(user_workflow.contains("# Code Review"), "{user_workflow}");
    let discovered = packs::find_pack(&root, "code-review")?;
    assert_eq!(discovered.shelf.as_deref(), Some("personal"));
    assert_eq!(discovered.path, user_pack);

    let project_pack = packs::create_pack(&root, "local/release-check", true)?;
    assert_eq!(
        project_pack,
        root.join(".dext/shelves/local/packs/release-check")
    );
    let discovered = packs::find_pack(&root, "release-check")?;
    assert_eq!(discovered.shelf.as_deref(), Some("local"));
    assert!(discovered.source.starts_with("project:"));

    let overwrite = packs::create_pack(&root, "personal/code-review", false)
        .expect_err("existing pack must not be overwritten");
    assert!(
        overwrite.to_string().contains("already exists"),
        "{overwrite:#}"
    );
    for invalid in ["missing-shelf", "Bad/name", "shelf/name/extra", "../escape"] {
        assert!(
            packs::create_pack(&root, invalid, false).is_err(),
            "invalid location accepted: {invalid}"
        );
    }

    restore_env_var("DEXT_HOME", old_home);
    restore_env_var("DEXT_SHELVES_DIR", old_shelves);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn direct_pack_roots_and_overrides_are_not_discovered() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("direct-pack-roots-ignored");
    let home = root.join("home");
    let env_root = root.join("env-packs");
    let direct_override = root.join("direct-override");
    let paths = [
        root.join("packs/project-root"),
        root.join(".dext/packs/project-hidden"),
        home.join("packs/user-root"),
        env_root.join("env-root"),
        direct_override.clone(),
    ];
    for (index, path) in paths.iter().enumerate() {
        std::fs::create_dir_all(path)?;
        std::fs::write(
            path.join("PACK.md"),
            format!("---\nname: direct-{index}\n---\n# Direct\n"),
        )?;
    }
    let old_home = std::env::var_os("DEXT_HOME");
    let old_shelves = std::env::var_os("DEXT_SHELVES_DIR");
    let old_packs = std::env::var_os("DEXT_PACKS_DIR");
    let old_direct = std::env::var_os("DEXT_PACK_DIRECT_4_DIR");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_PACKS_DIR", &env_root);
        std::env::set_var("DEXT_PACK_DIRECT_4_DIR", &direct_override);
    }

    let discovered = packs::discover_packs(&root);
    assert!(
        discovered
            .iter()
            .all(|pack| !pack.name.starts_with("direct-")),
        "{discovered:?}"
    );

    restore_env_var("DEXT_HOME", old_home);
    restore_env_var("DEXT_SHELVES_DIR", old_shelves);
    restore_env_var("DEXT_PACKS_DIR", old_packs);
    restore_env_var("DEXT_PACK_DIRECT_4_DIR", old_direct);
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
    assert!(listing.contains("demo"), "{listing}");
    assert!(listing.contains("Shelf workflow"), "{listing}");
    assert!(listing.contains("shelf: community"), "{listing}");
    assert!(listing.contains("source:"), "{listing}");

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
    assert!(slash.contains("Shelves"), "{slash}");
    assert!(slash.contains("Community"), "{slash}");
    assert!(slash.contains("shared typed abilities"), "{slash}");
    assert!(slash.contains("command:scan"), "{slash}");
    assert!(slash.contains("scan target"), "{slash}");
    assert!(slash.contains("scope:"), "{slash}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn slash_pack_create_scaffolds_project_pack() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-pack-create");
    let home = root.join("home");
    let old_home = std::env::var_os("DEXT_HOME");
    let old_shelves = std::env::var_os("DEXT_SHELVES_DIR");
    unsafe {
        std::env::set_var("DEXT_HOME", &home);
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let mut agent = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    assert_eq!(
        handle_slash("/pack create local/slash-pack --project", &mut agent),
        Some(true)
    );
    let output = drain_events(&mut rx)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .unwrap_or_default();
    assert!(output.contains("created pack:"), "{output}");
    let pack = packs::find_pack(&root, "slash-pack")?;
    assert_eq!(pack.shelf.as_deref(), Some("local"));
    assert!(pack.path.join("PACK.md").is_file());

    restore_env_var("DEXT_HOME", old_home);
    restore_env_var("DEXT_SHELVES_DIR", old_shelves);
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn slash_pack_list_and_inspect_use_discovered_packs() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-pack");
    let pack_dir = root.join(".dext/shelves/test/packs/demo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: demo\ndescription: Slash demo\n---\n# Demo\n",
    )?;
    unsafe {
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
    assert!(slash_text.contains("Slash demo"), "{slash_text}");
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
fn slash_pack_verbose_flag_lists_with_paths() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("slash-pack-verbose");
    let pack_dir = root.join(".dext/shelves/test/packs/demo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(
        pack_dir.join("PACK.md"),
        "---\nname: demo\ndescription: Verbose slash demo\n---\n# Demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let mut agent = test_agent(&root);
    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
    agent.set_sink(Box::new(ChannelSink { tx }));

    assert_eq!(handle_slash("/pack -v", &mut agent), Some(true));
    let slash_text = drain_events(&mut rx)
        .into_iter()
        .find_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .unwrap_or_default();
    assert!(slash_text.contains("Verbose slash demo"), "{slash_text}");
    assert!(slash_text.contains("path:"), "{slash_text}");
    assert!(!slash_text.contains("usage: /pack"), "{slash_text}");

    assert_eq!(
        handle_slash("/pack -v inspect demo", &mut agent),
        Some(true)
    );
    assert_eq!(
        handle_slash("/pack run demo -v task", &mut agent),
        Some(true)
    );
    let followup = drain_events(&mut rx)
        .into_iter()
        .filter_map(|event| match event {
            AgentEvent::Slash(text) => Some(text),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n---\n");
    assert!(followup.contains("pack: demo"), "{followup}");
    assert!(followup.contains("dext pack demo -v task"), "{followup}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
        std::env::remove_var("DEXT_SHELVES_DIR");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn pack_list_renders_compact_blocks_with_header_and_footer() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-list-normal");
    std::fs::create_dir_all(root.join(".dext/shelves/test/packs/demo"))?;
    std::fs::write(
        root.join(".dext/shelves/test/packs/demo/PACK.md"),
        "---\nname: demo\ndescription: Short demo\n---\n# Demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let packs = packs::discover_packs(&root);
    let opts = list_render::ListOptions::fixed(false, 80);
    let out = packs::render_pack_list(&packs, &opts, &root);

    assert!(out.starts_with("Packs"), "{out}");
    assert!(out.contains("found"), "{out}");
    assert!(out.contains("demo"), "{out}");
    assert!(out.contains("Short demo"), "{out}");
    assert!(out.contains("source:"), "{out}");
    assert!(out.contains("shelf:"), "{out}");
    assert!(out.contains("Use:"), "{out}");
    assert!(out.contains("/pack inspect <name>"), "{out}");
    assert!(!out.contains("path:"), "{out}"); // path hidden by default
    assert!(!out.contains('\x1b'), "{out}"); // no ANSI in fixed mode

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn pack_list_verbose_shows_paths() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-list-verbose");
    let root = std::fs::canonicalize(&root)?;
    std::fs::create_dir_all(root.join(".dext/shelves/test/packs/demo"))?;
    std::fs::write(
        root.join(".dext/shelves/test/packs/demo/PACK.md"),
        "---\nname: demo\ndescription: Demo\n---\n# Demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let packs = packs::discover_packs(&root);
    let opts = list_render::ListOptions::fixed(true, 80);
    let out = packs::render_pack_list(&packs, &opts, &root);

    assert!(out.contains("path:"), "{out}");
    assert!(out.contains("demo"), "{out}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn pack_list_narrow_terminal_wraps_description() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-list-narrow");
    std::fs::create_dir_all(root.join(".dext/shelves/test/packs/demo"))?;
    std::fs::write(
        root.join(".dext/shelves/test/packs/demo/PACK.md"),
        "---\nname: demo\ndescription: The quick brown fox jumps over the lazy dog repeatedly\n---\n# Demo\n",
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let packs = packs::discover_packs(&root);
    let opts = list_render::ListOptions::fixed(false, 30);
    let out = packs::render_pack_list(&packs, &opts, &root);

    let desc_lines: Vec<&str> = out
        .lines()
        .filter(|l| l.starts_with("    ") && !l.contains("source:") && !l.contains("shelf:"))
        .collect();
    assert!(desc_lines.iter().all(|l| l.len() <= 30), "{out}");
    assert!(out.contains("The quick brown fox"), "{out}");
    assert!(out.contains("lazy dog"), "{out}");

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn pack_list_long_description_wraps_within_width() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-list-longdesc");
    std::fs::create_dir_all(root.join(".dext/shelves/test/packs/demo"))?;
    let long_desc = "word ".repeat(50);
    std::fs::write(
        root.join(".dext/shelves/test/packs/demo/PACK.md"),
        format!("---\nname: demo\ndescription: {long_desc}---\n# Demo\n"),
    )?;
    unsafe {
        std::env::remove_var("DEXT_SHELVES_DIR");
        std::env::set_var("DEXT_HOME", root.join("home"));
    }

    let packs = packs::discover_packs(&root);
    let opts = list_render::ListOptions::fixed(false, 60);
    let out = packs::render_pack_list(&packs, &opts, &root);

    for line in out.lines() {
        assert!(line.len() <= 60, "line too long: {line}");
    }

    unsafe {
        std::env::remove_var("DEXT_HOME");
    }
    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn list_render_shorten_path_uses_project_relative() {
    let home = std::path::Path::new("/tmp/dext-test-home");
    let root = std::path::Path::new("/tmp/dext-test-home/work/project");
    let pack_path =
        std::path::Path::new("/tmp/dext-test-home/work/project/.dext/shelves/local/packs/demo");

    let shortened = list_render::shorten_path(pack_path, root, home, false);
    assert_eq!(shortened, "./.dext/shelves/local/packs/demo");

    let home_pack = std::path::Path::new("/tmp/dext-test-home/.dext/shelves/personal/packs/demo");
    let shortened2 = list_render::shorten_path(home_pack, root, home, false);
    assert_eq!(shortened2, "~/.dext/shelves/personal/packs/demo");
}

#[test]
fn list_render_wrap_honors_hanging_indent() {
    let lines = list_render::wrap_lines("aaa bbb ccc ddd eee", 10);
    assert!(lines.iter().all(|l| l.len() <= 10), "{lines:?}");
    assert_eq!(
        lines.join(" ").split_whitespace().collect::<Vec<_>>(),
        vec!["aaa", "bbb", "ccc", "ddd", "eee"]
    );
}

#[test]
fn list_render_wrap_splits_long_words() {
    let lines = list_render::wrap_lines("supercalifragilistic", 6);
    assert!(lines.iter().all(|l| l.len() <= 6), "{lines:?}");
    assert_eq!(lines.join(""), "supercalifragilistic");
}

#[test]
fn list_render_bold_only_with_color() {
    assert_eq!(list_render::bold("x", false), "x");
    assert_eq!(list_render::bold("x", true), "\x1b[1mx\x1b[0m");
}

#[test]
fn session_listing_shows_header_and_footer() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("session-list-header");
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
                text: "hello".to_string(),
            }],
        });
        agent.save_latest_session()?;

        let listing = render_session_listing(&project);
        assert!(listing.contains("Sessions"), "{listing}");
        assert!(listing.contains("Latest"), "{listing}");
        assert!(listing.contains("Named"), "{listing}");
        assert!(listing.contains("Autosaved"), "{listing}");
        assert!(listing.contains("Use:"), "{listing}");
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
fn pack_helper_direct_spawn_policy_is_platform_safe() {
    assert!(pack_helper_supports_direct_spawn_on(
        Path::new("helper"),
        false
    ));
    assert!(pack_helper_supports_direct_spawn_on(
        Path::new("helper.py"),
        false
    ));
    assert!(pack_helper_supports_direct_spawn_on(
        Path::new("helper.exe"),
        true
    ));
    assert!(pack_helper_supports_direct_spawn_on(
        Path::new("helper.COM"),
        true
    ));
    assert!(!pack_helper_supports_direct_spawn_on(
        Path::new("helper"),
        true
    ));
    assert!(!pack_helper_supports_direct_spawn_on(
        Path::new("helper.py"),
        true
    ));
}

#[test]
fn pack_invocation_args_preserve_complete_tasks() {
    assert_eq!(
        packs::pack_invocation_args("run agent-browser inspect https://example.com now"),
        Some(("agent-browser", "inspect https://example.com now"))
    );
    assert_eq!(
        packs::pack_invocation_args("agent-browser inspect https://example.com now"),
        Some(("agent-browser", "inspect https://example.com now"))
    );
    assert_eq!(packs::pack_invocation_args("inspect agent-browser"), None);
    assert_eq!(packs::pack_invocation_args("run agent-browser"), None);
    assert_eq!(packs::pack_invocation_args("agent-browser"), None);
}

#[test]
fn conversational_pack_inference_requires_invocation_intent() -> Result<()> {
    let _guard = env_lock();
    let root = temp_test_dir("pack-inference");
    let pack_dir = root.join(".dext/shelves/test/packs/demo");
    std::fs::create_dir_all(&pack_dir)?;
    std::fs::write(pack_dir.join("PACK.md"), "---\nname: demo\n---\n# Demo\n")?;
    unsafe {
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
fn pack_auto_invocation_disabled_by_env_globs_and_specific_names() {
    let _guard = env_lock();
    let pack = packs::PackInfo {
        name: "crew".to_string(),
        description: String::new(),
        path: PathBuf::from("/tmp/crew"),
        pack_md_path: PathBuf::from("/tmp/crew/PACK.md"),
        phooks_path: None,
        credential_env: Vec::new(),
        credential_env_ignored: false,
        source: "test".to_string(),
        shelf: Some("orchestration".to_string()),
    };

    unsafe {
        std::env::remove_var("DEXT_NO_PACK");
    }
    assert!(
        !pack_auto_invocation_disabled_by_env(&pack),
        "unset → enabled"
    );

    for val in ["*", "all", "true", "1"] {
        unsafe {
            std::env::set_var("DEXT_NO_PACK", val);
        }
        assert!(
            pack_auto_invocation_disabled_by_env(&pack),
            "{val} → disabled"
        );
    }

    for val in [
        "crew",
        "crew,autoresearch",
        "orchestration",
        "crew autoresearch",
    ] {
        unsafe {
            std::env::set_var("DEXT_NO_PACK", val);
        }
        assert!(
            pack_auto_invocation_disabled_by_env(&pack),
            "{val} → disabled (matches crew)"
        );
    }

    unsafe {
        std::env::set_var("DEXT_NO_PACK", "autoresearch");
    }
    assert!(
        !pack_auto_invocation_disabled_by_env(&pack),
        "autoresearch ≠ crew → enabled"
    );

    for val in ["0", "false", "off", "no", ""] {
        unsafe {
            std::env::set_var("DEXT_NO_PACK", val);
        }
        assert!(
            !pack_auto_invocation_disabled_by_env(&pack),
            "{val:?} → enabled"
        );
    }

    unsafe {
        std::env::remove_var("DEXT_NO_PACK");
    }
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

#[test]
fn html_entity_decode_preserves_multibyte_utf8() {
    assert_eq!(
        html_entity_decode_minimal("caf\u{e9} \u{2014} r\u{e9}sum\u{e9} &amp; t\u{e9}l\u{e9}"),
        "caf\u{e9} \u{2014} r\u{e9}sum\u{e9} & t\u{e9}l\u{e9}"
    );
    assert_eq!(html_entity_decode_minimal("&lt;a&gt;&quot;&#39;"), "<a>\"'");
    assert_eq!(
        html_entity_decode_minimal("&#x2014;&#8212;"),
        "\u{2014}\u{2014}"
    );
    // Unterminated/unknown entities pass through without panicking.
    assert_eq!(
        html_entity_decode_minimal("a&b &unknown; &"),
        "a&b &unknown; &"
    );

    let text = extract_html_text("<p>caf\u{e9} &amp; cr\u{e8}me</p>");
    assert!(text.contains("caf\u{e9} & cr\u{e8}me"), "{text}");
}

#[test]
fn sanitize_strips_prior_turn_thinking_but_keeps_current_tool_loop() {
    let thinking = |text: &str| Block::Thinking {
        text: text.to_string(),
        signature: Some("sig".to_string()),
    };
    let user_text = |text: &str| Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: text.to_string(),
        }],
    };
    let history = vec![
        user_text("first task"),
        Message {
            role: "assistant".to_string(),
            content: vec![
                thinking("old reasoning"),
                Block::Text {
                    text: "done".to_string(),
                },
            ],
        },
        user_text("second task"),
        Message {
            role: "assistant".to_string(),
            content: vec![
                thinking("current reasoning"),
                Block::ToolUse {
                    id: "call_1".to_string(),
                    name: "read_file".to_string(),
                    input: json!({"path": "x"}),
                },
            ],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::ToolResult {
                tool_use_id: "call_1".to_string(),
                content: "ok".to_string(),
                is_error: Some(false),
                metadata: ToolResultMetadata::default(),
            }],
        },
        // Runtime notes do not start a new turn; the tool loop above must keep
        // its thinking block even with a note in between.
        user_text("[runtime-note] keep output small"),
    ];

    let sanitized = sanitize_anthropic_messages(&history, true, false);
    assert!(
        !sanitized[1]
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. })),
        "prior-turn thinking should be stripped"
    );
    assert!(
        sanitized[3]
            .content
            .iter()
            .any(|b| matches!(b, Block::Thinking { .. })),
        "current-turn tool-loop thinking must be preserved"
    );

    // With thinking disabled everything is stripped, as before.
    let sanitized = sanitize_anthropic_messages(&history, false, false);
    assert!(
        !sanitized
            .iter()
            .flat_map(|m| m.content.iter())
            .any(|b| matches!(b, Block::Thinking { .. }))
    );
}

#[test]
fn head_tail_cap_preserves_process_output_verdict() {
    let mut output = String::new();
    output.push_str("compiling step one\n");
    for i in 0..2000 {
        output.push_str(&format!("warning line {i}\n"));
    }
    output.push_str("test result: FAILED. 3 passed; 2 failed\n");
    let capped = cap_bytes_head_tail_with_hint(output.clone(), 2_000, "see artifact");
    assert!(capped.len() < output.len());
    assert!(capped.starts_with("compiling step one"), "{capped}");
    assert!(
        capped.contains("test result: FAILED. 3 passed; 2 failed"),
        "tail verdict must survive capping"
    );
    assert!(capped.contains("see artifact"), "{capped}");

    // Under-cap content passes through untouched.
    let short = "all good".to_string();
    assert_eq!(
        cap_bytes_head_tail_with_hint(short.clone(), 2_000, "x"),
        short
    );
}

#[test]
fn compact_summary_model_env_override() {
    let _guard = env_lock();
    let root = temp_test_dir("compact-model-override");
    let agent = test_agent(&root);
    unsafe { std::env::remove_var("DEXT_COMPACT_MODEL") };
    assert_eq!(agent.compact_summary_model(), agent.model);
    unsafe { std::env::set_var("DEXT_COMPACT_MODEL", "claude-haiku-4-5") };
    assert_eq!(agent.compact_summary_model(), "claude-haiku-4-5");
    unsafe { std::env::set_var("DEXT_COMPACT_MODEL", "   ") };
    assert_eq!(agent.compact_summary_model(), agent.model);
    unsafe { std::env::remove_var("DEXT_COMPACT_MODEL") };
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn context_state_warns_on_repeated_actions_and_strategy_budget() {
    let history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "inspect the repo".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_status_1".to_string(),
                name: "git_diff".to_string(),
                input: json!({"stat": true}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("call_status_1", "clean", Some(false))],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_status_2".to_string(),
                name: "git_diff".to_string(),
                input: json!({"stat": true}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("call_status_2", "clean", Some(false))],
        },
    ];

    let state = render_context_state_prompt(&history, &WorkLedger::default());
    assert!(state.contains("Recent actions (last 5):"), "{state}");
    assert!(
        state.contains("git_status: 2/1 used · PIVOT REQUIRED"),
        "{state}"
    );
    assert!(
        state.contains("PATTERN: same action repeated 2x"),
        "{state}"
    );
}

#[test]
fn context_state_resets_git_status_budget_after_mutation() {
    let history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "make a change".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_status".to_string(),
                name: "git_diff".to_string(),
                input: json!({"stat": true}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("call_status", "clean", Some(false))],
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
            content: vec![tool_result_block("call_write", "wrote file", Some(false))],
        },
    ];

    let state = render_context_state_prompt(&history, &WorkLedger::default());
    assert!(state.contains("git_status: 0/1 used · OK"), "{state}");
}

#[test]
fn compose_system_parts_includes_context_state_section() {
    let root = temp_test_dir("context-state-env");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let mut agent = test_agent(&root);
    agent.history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "check status".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::ToolUse {
                id: "call_status".to_string(),
                name: "git_diff".to_string(),
                input: json!({"stat": true}),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![tool_result_block("call_status", "dirty", Some(false))],
        },
    ];

    let (_stable, env) = agent.compose_system_parts();
    assert!(env.contains("## Context State"), "{env}");
    assert!(
        env.contains("git_status: 1/1 used · PIVOT REQUIRED"),
        "{env}"
    );

    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn compact_summary_thinking_budget_and_reasoning_fallbacks() {
    assert_eq!(
        compact_summary_max_tokens(ThinkingEffort::Off),
        COMPACT_SUMMARY_MAX_TOKENS
    );
    assert_eq!(
        compact_summary_max_tokens(ThinkingEffort::High),
        COMPACT_SUMMARY_MAX_TOKENS_THINKING
    );
    assert!(
        compact_summary_chat_template_kwargs("local", ApiProvider::OpenAi, "http://127.0.0.1:8080")
            .is_some()
    );
    assert!(
        compact_summary_chat_template_kwargs(
            "openai",
            ApiProvider::OpenAi,
            "https://api.openai.com"
        )
        .is_none()
    );

    let reasoning =
        "draft\n**Task**\nFirst draft.\n\nmore\n## Task\nFinal draft.\nDecisions\n- keep it";
    let extracted = extract_summary_from_reasoning(reasoning);
    assert!(
        extracted.starts_with("## Task\nFinal draft."),
        "{extracted}"
    );
    assert!(!extracted.contains("First draft"), "{extracted}");

    let json = json!({
        "choices": [{
            "finish_reason": "length",
            "message": {
                "content": "",
                "reasoning_content": reasoning,
            }
        }],
        "usage": {}
    });
    let text = openai_summary_text_from_response(&json).expect("reasoning fallback");
    assert!(text.contains("Final draft"), "{text}");

    let empty = json!({"choices":[{"finish_reason":"stop","message":{"content":""}}]});
    let err = openai_summary_text_from_response(&empty).expect_err("empty summary should fail");
    assert!(
        err.to_string().contains("summary response had no text"),
        "{err:#}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn frugal_local_compact_summary_calls_local_llm_and_disables_thinking() {
    let root = temp_test_dir("compact-summary-local-thinking");
    let root = std::fs::canonicalize(root).expect("canonical temp dir");
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
    let addr = listener.local_addr().expect("local addr");
    let (tx, rx) = std::sync::mpsc::channel();
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
                line.to_ascii_lowercase()
                    .strip_prefix("content-length:")
                    .and_then(|v| v.trim().parse::<usize>().ok())
            })
            .unwrap_or(0);
        while request.len() < header_end + content_length {
            let n = stream.read(&mut buf).expect("read request body");
            assert!(n > 0, "client closed before sending body");
            request.extend_from_slice(&buf[..n]);
        }
        let body = request[header_end..header_end + content_length].to_vec();
        tx.send(body).expect("send request body");
        let response_body = r#"{"choices":[{"finish_reason":"stop","message":{"content":"Task\nSummarized."}}],"usage":{"prompt_tokens":4,"completion_tokens":2}}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\n\r\n{}",
            response_body.len(),
            response_body
        );
        stream
            .write_all(response.as_bytes())
            .expect("write response");
    });

    let mut agent = test_agent(&root);
    agent.api_provider = ApiProvider::OpenAi;
    agent.provider_id = "local".to_string();
    agent.provider_requires_api_key = false;
    agent.api_key.clear();
    agent.base_url = format!("http://{addr}");
    agent.model = DEFAULT_LOCAL_MODEL.to_string();
    agent.context_mode = ContextMode::Frugal;
    agent.thinking_effort = ThinkingEffort::High;
    let old = vec![Message {
        role: "user".to_string(),
        content: vec![Block::Text {
            text: "old context".to_string(),
        }],
    }];

    let (summary, usage) = agent
        .one_shot_summary(&old, "")
        .await
        .expect("summary request should complete");
    assert_eq!(summary, "Task\nSummarized.");
    assert_eq!(usage.output, 2);
    let body = rx.recv().expect("request body");
    let value: Value = serde_json::from_slice(&body).expect("json body");
    assert_eq!(value["max_tokens"], COMPACT_SUMMARY_MAX_TOKENS_THINKING);
    assert!(value.get("reasoning_effort").is_none(), "{value}");
    assert_eq!(value["chat_template_kwargs"]["enable_thinking"], false);
    assert_eq!(value["stream"], false);
    assert_eq!(value["messages"][0]["role"], "system");
    assert_eq!(value["messages"][0]["content"], COMPACT_SYSTEM);
    let summary_prompt = value["messages"][1]["content"]
        .as_str()
        .expect("summary user prompt");
    assert!(
        summary_prompt.contains("[user] old context"),
        "{summary_prompt}"
    );
    assert!(summary_prompt.contains("Recent state"), "{summary_prompt}");

    server.join().expect("server thread");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn prompt_scan_cache_revalidates_on_file_change() -> Result<()> {
    let root = temp_test_dir("prompt-scan-cache");
    let root = std::fs::canonicalize(&root)?;
    std::fs::write(root.join("recall.md"), "- first fact")?;
    let agent = test_agent(&root);

    let (_, recall, _) = agent.prompt_scans();
    assert!(
        recall.iter().any(|(_, _, c)| c.contains("first fact")),
        "{recall:?}"
    );

    // Same epoch, unchanged files: served from cache with identical content.
    let (_, recall_again, _) = agent.prompt_scans();
    assert_eq!(recall, recall_again);

    // A mid-turn write (the agent updating its own recall.md) must be picked
    // up through the stat signature without waiting for the next user turn.
    std::fs::write(root.join("recall.md"), "- second fact with longer text")?;
    let (_, recall_updated, _) = agent.prompt_scans();
    assert!(
        recall_updated
            .iter()
            .any(|(_, _, c)| c.contains("second fact")),
        "{recall_updated:?}"
    );

    // A newly created DEXT.md at the project root must also invalidate.
    std::fs::write(root.join("DEXT.md"), "## Rules\nuse rg")?;
    let (dext, _, _) = agent.prompt_scans();
    assert!(
        dext.iter().any(|(_, _, c)| c.contains("use rg")),
        "{dext:?}"
    );

    let _ = std::fs::remove_dir_all(&root);
    Ok(())
}

#[test]
fn wire_messages_drop_content_emptied_by_sanitization() {
    // An assistant message that carried only thinking blocks empties out once
    // sanitization strips them (prior-turn trim, or thinking disabled on
    // resume); it must not reach the wire as an empty content array.
    let history = vec![
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "first task".to_string(),
            }],
        },
        Message {
            role: "assistant".to_string(),
            content: vec![Block::Thinking {
                text: "only reasoning".to_string(),
                signature: Some("sig".to_string()),
            }],
        },
        Message {
            role: "user".to_string(),
            content: vec![Block::Text {
                text: "second task".to_string(),
            }],
        },
    ];
    let sanitized = sanitize_anthropic_messages(&history, true, false);
    assert!(
        sanitized[1].content.is_empty(),
        "prior-turn thinking-only message should sanitize to empty"
    );
    let wire = anthropic_wire_messages(&sanitized, true).expect("wire");
    assert_eq!(wire.len(), 2, "{wire:?}");
    assert!(
        wire.iter()
            .all(|m| !m["content"].as_array().map(Vec::is_empty).unwrap_or(true)),
        "{wire:?}"
    );
    // Breakpoint still lands on the last surviving message.
    let last_blocks = wire[1]["content"].as_array().expect("blocks");
    assert_eq!(
        last_blocks
            .last()
            .and_then(|b| b["cache_control"]["type"].as_str()),
        Some("ephemeral")
    );
}
