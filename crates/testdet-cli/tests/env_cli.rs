//! Direct `env::run` tests: the text layout and the JSON shape (ruling 8).
//!
//! The process environment is not touched — the source is a real `.env.staging`
//! file under the config directory — so these tests are deterministic across
//! any ambient environment.

use std::fs;
use std::path::{Path, PathBuf};

use testdet_cli::commands::env::{self, EnvArgs};
use testdet_cli::exit::Exit;
use testdet_cli::format::Format;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn manifest() -> &'static str {
    "version: 1\nname: my-api\ncollections: [ops]\ndefaultEnvironment: staging\nenvironments:\n  staging:\n    baseUrl: https://staging.example.com\n    secrets:\n      source: dotenv\n      file: .env.staging\n"
}

fn declaration() -> &'static str {
    "TESTDET_CLI_TOKEN:\n  description: token\nTESTDET_CLI_ADMIN:\n  description: admin\n"
}

fn login_operation() -> &'static str {
    "id: login\nrequest:\n  kind: http\n  method: GET\n  path: /login/{{env.TESTDET_CLI_TOKEN}}\n"
}

fn admin_operation() -> &'static str {
    "id: admin\nrequest:\n  kind: http\n  method: GET\n  path: /admin/{{env.TESTDET_CLI_ADMIN}}\n"
}

fn probe_operation() -> &'static str {
    "id: probe\nrequest:\n  kind: http\n  method: GET\n  path: /probe/{{env.TESTDET_CLI_TOKEN}}/{{env.TESTDET_CLI_MISSING}}\n"
}

fn dotenv_file() -> &'static str {
    "TESTDET_CLI_TOKEN=cli-secret-value\n"
}

/// Build the config tree and return the explicit `--config` path. The dotenv
/// file lives under `testdet/` because Task 9 resolves `secrets.file` against
/// `root_dir` (the directory containing the manifest).
fn build_config() -> (tempfile::TempDir, PathBuf, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "testdet/testdet.yaml", manifest());
    write(root.path(), "testdet/secrets/staging.yaml", declaration());
    write(root.path(), "testdet/ops/login.yaml", login_operation());
    write(root.path(), "testdet/ops/admin.yaml", admin_operation());
    write(root.path(), "testdet/ops/probe.yaml", probe_operation());
    write(root.path(), "testdet/.env.staging", dotenv_file());
    let config = root.path().join("testdet/testdet.yaml");
    let env_file = root
        .path()
        .canonicalize()
        .unwrap()
        .join("testdet")
        .join(".env.staging");
    (root, config, env_file)
}

fn env_run(config: Option<PathBuf>, format: Format) -> (Exit, String) {
    env::run(EnvArgs {
        config,
        env: None,
        format,
    })
}

#[test]
fn text_output_pads_names_and_reports_origin_without_the_value() {
    let (_root, config, env_file) = build_config();
    let (exit, output) = env_run(Some(config), Format::Text);
    assert_eq!(exit, Exit::Ok);

    let env_file_str = env_file.display().to_string();

    // Names are sorted; TESTDET_CLI_MISSING (19 chars) is the longest, so the
    // two 17-char names are left-padded by two spaces.
    assert!(output.contains(&format!(
        "  TESTDET_CLI_TOKEN  required by 2 operation(s)  set <- {env_file_str}\n"
    )));
    assert!(output.contains("  TESTDET_CLI_ADMIN  required by 1 operation(s)  NOT SET\n"));
    assert!(
        output.contains("TESTDET_CLI_MISSING  required by 1 operation(s)  NOT SET (undeclared)\n")
    );

    // The secret value must never appear (ruling 9).
    assert!(!output.contains("cli-secret-value"));
}

#[test]
fn json_output_has_tagged_origin_and_used_by() {
    let (_root, config, env_file) = build_config();
    let (exit, output) = env_run(Some(config), Format::Json);
    assert_eq!(exit, Exit::Ok);

    let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).expect("valid JSON array");
    assert_eq!(parsed.len(), 3);

    let by_name = |name: &str| parsed.iter().find(|entry| entry["name"] == name).unwrap();

    let token = by_name("TESTDET_CLI_TOKEN");
    assert_eq!(token["declared"], true);
    assert_eq!(token["set"], true);
    assert_eq!(token["origin"]["kind"], "dotenv");
    assert_eq!(
        token["origin"]["path"],
        env_file.display().to_string().as_str()
    );
    assert_eq!(token["usedBy"], serde_json::json!(["login", "probe"]));

    let admin = by_name("TESTDET_CLI_ADMIN");
    assert_eq!(admin["declared"], true);
    assert_eq!(admin["set"], false);
    assert!(admin["origin"].is_null());
    assert_eq!(admin["usedBy"], serde_json::json!(["admin"]));

    let missing = by_name("TESTDET_CLI_MISSING");
    assert_eq!(missing["declared"], false);
    assert_eq!(missing["set"], false);
    assert!(missing["origin"].is_null());
    assert_eq!(missing["usedBy"], serde_json::json!(["probe"]));

    // The secret value must never appear (ruling 9).
    assert!(!output.contains("cli-secret-value"));
}

#[test]
fn ndjson_is_a_usage_error_for_env() {
    let (_root, config, _env_file) = build_config();
    let (exit, output) = env_run(Some(config), Format::Ndjson);
    assert_eq!(exit, Exit::Usage);
    assert!(output.contains("`--format ndjson` only applies to `run`"));
}
