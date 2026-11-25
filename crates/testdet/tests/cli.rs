use std::fs;
use std::path::Path;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

#[test]
fn help_lists_every_subcommand_and_exits_zero() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"))
        .arg("--help")
        .output()
        .expect("spawn");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    for cmd in ["tui", "validate", "run", "env", "config", "schema", "show"] {
        assert!(text.contains(cmd), "`--help` must mention `{cmd}`");
    }
}

/// Spawn the real binary and assert the stream contract: `--format json`
/// writes a parseable JSON array and nothing else to stdout, while text
/// diagnostics go to stderr. `CARGO_BIN_EXE_testdet` is only defined for the
/// crate that builds the binary, which is why this lives here and not in
/// `testdet-cli/tests`.
#[test]
fn validate_json_goes_to_stdout_and_text_to_stderr() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    // Three foreign fields on `kind: http` → three E_FOREIGN_FIELD errors.
    write(
        root.path(),
        "testdet/users/bad.yaml",
        "id: bad\nrequest:\n  kind: http\n  method: GET\n  path: /x\n  query: \"q\"\n  operationName: \"op\"\n  variables: {}\n",
    );
    let config = root.path().join("testdet/testdet.yaml");

    let json = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"))
        .arg("validate")
        .arg("--config")
        .arg(&config)
        .arg("--format")
        .arg("json")
        .output()
        .expect("spawn validate --format json");
    assert_eq!(json.status.code(), Some(1));
    let stdout = String::from_utf8(json.stdout).unwrap();
    let parsed: Vec<serde_json::Value> =
        serde_json::from_str(&stdout).expect("stdout must be a JSON array");
    assert_eq!(parsed.len(), 3);
    assert!(
        json.stderr.is_empty(),
        "JSON diagnostics must not also appear on stderr"
    );

    let text = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"))
        .arg("validate")
        .arg("--config")
        .arg(&config)
        .output()
        .expect("spawn validate (default text)");
    assert_eq!(text.status.code(), Some(1));
    assert!(text.stdout.is_empty(), "text diagnostics go to stderr only");
    let stderr = String::from_utf8(text.stderr).unwrap();
    assert_eq!(stderr.matches("E_FOREIGN_FIELD").count(), 3);
}

#[test]
fn tui_help_mentions_the_user_and_env_flags() {
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"))
        .args(["tui", "--help"])
        .output()
        .expect("spawn tui --help");
    assert_eq!(out.status.code(), Some(0));
    let text = String::from_utf8(out.stdout).unwrap();
    assert!(
        text.contains("--user"),
        "`tui --help` must mention `--user`"
    );
    assert!(text.contains("--env"), "`tui --help` must mention `--env`");
    assert!(
        text.contains("--config"),
        "`tui --help` must mention `--config`"
    );
}

/// A config error must be reported *before* the terminal is touched: exit 1,
/// diagnostics on stderr, and no escape sequence anywhere in the output.
#[test]
fn tui_with_a_broken_config_exits_one_without_touching_the_terminal() {
    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users/bad.yaml",
        "id: bad\nrequest:\n  kind: http\n  method: GET\n  path: /x\n  query: \"q\"\n",
    );
    let out = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"))
        .arg("tui")
        .arg("--config")
        .arg(root.path().join("testdet/testdet.yaml"))
        .output()
        .expect("spawn tui");
    assert_eq!(out.status.code(), Some(1), "a broken config exits 1");
    let stderr = String::from_utf8(out.stderr).unwrap();
    assert!(
        stderr.contains("E_FOREIGN_FIELD"),
        "the diagnostics are printed: {stderr}"
    );
    assert!(
        !stderr.contains('\u{1b}') && !String::from_utf8_lossy(&out.stdout).contains('\u{1b}'),
        "no ANSI escape may reach the terminal"
    );
}
