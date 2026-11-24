//! Direct `validate::run` tests: rendered content and exit codes only.
//!
//! Stream separation (JSON on stdout, text on stderr) is asserted by spawning
//! the binary in `crates/testdet/tests/cli.rs`; these tests drive
//! `commands::validate::run` directly so they can pin exact output and codes
//! without depending on the process working directory.

use std::fs;
use std::path::{Path, PathBuf};

use testdet_cli::commands::validate::{self, ValidateArgs};
use testdet_cli::exit::Exit;
use testdet_cli::format::Format;

fn write(root: &Path, rel: &str, content: &str) {
    let path = root.join(rel);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(path, content).unwrap();
}

fn manifest() -> &'static str {
    "version: 1\nname: my-api\ncollections: [users]\n"
}

fn clean_operation() -> &'static str {
    "id: upload\nrequest:\n  kind: http\n  method: POST\n  path: /upload\n"
}

/// An operation whose `files.extra` entry is never referenced: Task 11 emits
/// `W_UNUSED_VAR` for it. (Task 13 adds the secret-specific warning case.)
fn warning_operation() -> &'static str {
    "id: upload\nrequest:\n  kind: http\n  method: POST\n  path: /upload\n  body:\n    kind: multipart\n    value:\n      doc: \"{{file.used}}\"\nfiles:\n  used:\n    path: ./doc.txt\n  extra:\n    path: ./extra.txt\n"
}

/// Three foreign request fields on `kind: http` → three `E_FOREIGN_FIELD`s.
fn invalid_operation() -> &'static str {
    "id: bad\nrequest:\n  kind: http\n  method: GET\n  path: /x\n  query: \"q\"\n  operationName: \"op\"\n  variables: {}\n"
}

/// Build the config tree and return the explicit `--config` path.
fn build_config(operation: &str, filename: &str) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "testdet/testdet.yaml", manifest());
    write(root.path(), &format!("testdet/users/{filename}"), operation);
    let config = root.path().join("testdet/testdet.yaml");
    (root, config)
}

fn validate(config: Option<PathBuf>, format: Format) -> (Exit, String) {
    validate::run(ValidateArgs {
        config,
        format,
        env: None,
    })
}

#[test]
fn clean_config_text_is_empty_and_exits_zero() {
    let (_root, config) = build_config(clean_operation(), "upload.yaml");
    let (exit, output) = validate(Some(config), Format::Text);
    assert_eq!(exit, Exit::Ok);
    assert_eq!(output, "", "a clean config renders nothing in text format");
}

#[test]
fn invalid_config_exits_one_and_lists_three_diagnostic_codes() {
    let (_root, config) = build_config(invalid_operation(), "bad.yaml");
    let (exit, output) = validate(Some(config), Format::Text);
    assert_eq!(exit, Exit::ConfigInvalid);
    // Exactly three E_FOREIGN_FIELD diagnostics; the text renderer repeats the
    // code once per diagnostic.
    assert_eq!(output.matches("E_FOREIGN_FIELD").count(), 3);
}

#[test]
fn json_clean_config_prints_empty_array() {
    let (_root, config) = build_config(clean_operation(), "upload.yaml");
    let (exit, output) = validate(Some(config), Format::Json);
    assert_eq!(exit, Exit::Ok);
    assert_eq!(output, "[]\n", "a clean config renders an empty JSON array");
}

#[test]
fn json_invalid_config_is_a_parseable_array_of_three() {
    let (_root, config) = build_config(invalid_operation(), "bad.yaml");
    let (exit, output) = validate(Some(config), Format::Json);
    assert_eq!(exit, Exit::ConfigInvalid);
    let parsed: Vec<serde_json::Value> = serde_json::from_str(&output).expect("valid JSON array");
    assert_eq!(parsed.len(), 3);
    for diagnostic in &parsed {
        assert_eq!(diagnostic["code"], "E_FOREIGN_FIELD");
    }
}

#[test]
fn warnings_alone_exit_zero() {
    let (_root, config) = build_config(warning_operation(), "upload.yaml");
    let (exit, output) = validate(Some(config), Format::Text);
    assert_eq!(exit, Exit::Ok, "warnings-only config must exit 0");
    assert!(output.contains("W_UNUSED_VAR"), "warning is rendered");
    assert!(!output.contains("error"), "no error text for a warning");
}

#[test]
fn ndjson_is_a_usage_error_for_validate() {
    let (_root, config) = build_config(clean_operation(), "upload.yaml");
    let (exit, output) = validate(Some(config), Format::Ndjson);
    assert_eq!(exit, Exit::Usage);
    assert!(output.contains("`--format ndjson` only applies to `run`"));
}

#[test]
fn snapshot_valid_config_warning_as_text() {
    let (root, config) = build_config(warning_operation(), "upload.yaml");
    let (exit, output) = validate(Some(config), Format::Text);
    assert_eq!(exit, Exit::Ok);
    let canonical_root = root.path().canonicalize().unwrap();
    let root_str = canonical_root.to_str().unwrap();
    insta::with_settings!({ filters => vec![(root_str, "<root>")] }, {
        insta::assert_snapshot!(output);
    });
}

#[test]
fn snapshot_invalid_config_three_diagnostics_as_text() {
    let (root, config) = build_config(invalid_operation(), "bad.yaml");
    let (exit, output) = validate(Some(config), Format::Text);
    assert_eq!(exit, Exit::ConfigInvalid);
    let canonical_root = root.path().canonicalize().unwrap();
    let root_str = canonical_root.to_str().unwrap();
    insta::with_settings!({ filters => vec![(root_str, "<root>")] }, {
        insta::assert_snapshot!(output);
    });
}
