// Mirrors the packaged-binary smoke checks in `.github/workflows/release.yml`
// so drift in their expected output fails ordinary CI before a tag is pushed.
// Keep `EMPTY_PACK_HEADER` identical to the strings asserted by the release
// workflow on Unix and Windows.
use std::path::{Path, PathBuf};
use std::process::{Command, Output};

const EMPTY_PACK_HEADER: &str = "Packs  0 found";

fn isolated_root(name: &str) -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("dext-release-smoke-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(root.join("home")).unwrap();
    std::fs::create_dir_all(root.join("work")).unwrap();
    root
}

fn run(root: &Path, args: &[&str]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_dext"));
    command
        .args(args)
        .current_dir(root.join("work"))
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap_or_default())
        .env("HOME", root.join("home"))
        .env("USERPROFILE", root.join("home"))
        .env("DEXT_HOME", root.join("verify-home"));
    if let Some(system_root) = std::env::var_os("SystemRoot") {
        command.env("SystemRoot", system_root);
    }
    command.output().expect("run packaged dext candidate")
}

#[test]
fn packaged_binary_reports_package_version() {
    let root = isolated_root("version");
    let output = run(&root, &["--version"]);
    assert!(output.status.success(), "{output:?}");
    assert_eq!(
        String::from_utf8_lossy(&output.stdout).trim(),
        format!("dext {}", env!("CARGO_PKG_VERSION"))
    );
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn packaged_binary_ships_no_pack_content_with_empty_home() {
    let root = isolated_root("packs");
    let output = run(&root, &["pack", "list"]);
    assert!(output.status.success(), "{output:?}");
    let stdout = String::from_utf8_lossy(&output.stdout);
    assert!(
        stdout
            .lines()
            .any(|line| line.trim_end() == EMPTY_PACK_HEADER),
        "release workflow expects {EMPTY_PACK_HEADER:?}; got:\n{stdout}"
    );

    let json = run(&root, &["pack", "list", "--json"]);
    assert!(json.status.success(), "{json:?}");
    let parsed: serde_json::Value =
        serde_json::from_slice(&json.stdout).expect("pack list --json is JSON");
    assert_eq!(parsed, serde_json::json!([]));
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn release_workflow_asserts_the_same_empty_pack_header() {
    let workflow = std::fs::read_to_string(
        Path::new(env!("CARGO_MANIFEST_DIR")).join(".github/workflows/release.yml"),
    )
    .expect("read release workflow");
    let expected_unix = format!("grep -F \"{EMPTY_PACK_HEADER}\"");
    let expected_windows = format!("$pack.Contains(\"{EMPTY_PACK_HEADER}\")");
    assert!(
        workflow.contains(&expected_unix),
        "Unix release smoke drifted"
    );
    assert!(
        workflow.contains(&expected_windows),
        "Windows release smoke drifted"
    );
}
