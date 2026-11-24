//! Direct `show::run` tests: rendered content and exit codes only.

use std::fs;
use std::path::{Path, PathBuf};

use testdet_cli::commands::show::{self, ShowArgs};
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
    "id: upload\nrequest:\n  kind: http\n  method: POST\n  path: /upload\ntags: [media, bulk]\n"
}

fn invalid_operation() -> &'static str {
    "id: bad\nrequest:\n  kind: http\n  method: GET\n  path: /x\n  query: \"q\"\n  operationName: \"op\"\n  variables: {}\n"
}

fn build_config(operation: &str, filename: &str) -> (tempfile::TempDir, PathBuf) {
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "testdet/testdet.yaml", manifest());
    write(root.path(), &format!("testdet/users/{filename}"), operation);
    let config = root.path().join("testdet/testdet.yaml");
    (root, config)
}

fn show(config: Option<PathBuf>, id: &str, format: Format) -> (Exit, String) {
    show::run(ShowArgs {
        config,
        id: id.to_string(),
        format,
    })
}

#[test]
fn show_known_operation_json_is_pretty_and_complete() {
    let (_root, config) = build_config(clean_operation(), "upload.yaml");
    let (exit, output) = show(Some(config), "upload", Format::Json);
    assert_eq!(exit, Exit::Ok);
    let parsed: serde_json::Value = serde_json::from_str(&output).expect("pretty JSON");
    assert_eq!(parsed["id"], "upload");
    assert_eq!(parsed["request"]["kind"], "http");
    assert_eq!(parsed["request"]["method"], "POST");
    assert_eq!(parsed["tags"][0], "media");
    assert_eq!(parsed["tags"][1], "bulk");
}

#[test]
fn show_known_operation_text_has_id_kind_and_first_line() {
    let (_root, config) = build_config(clean_operation(), "upload.yaml");
    let (exit, output) = show(Some(config), "upload", Format::Text);
    assert_eq!(exit, Exit::Ok);
    assert!(output.contains("  id: upload\n"));
    assert!(output.contains("  kind: http\n"));
    assert!(output.contains("  POST /upload\n"));
    assert!(output.contains("  tags: media, bulk\n"));
}

#[test]
fn show_unknown_id_is_a_usage_error_with_sorted_hint() {
    // Two operations so the sorted-id hint ordering is meaningful.
    let root = tempfile::tempdir().unwrap();
    write(root.path(), "testdet/testdet.yaml", manifest());
    write(
        root.path(),
        "testdet/users/zed.yaml",
        "id: zed\nrequest:\n  kind: http\n  method: GET\n  path: /zed\n",
    );
    write(
        root.path(),
        "testdet/users/alpha.yaml",
        "id: alpha\nrequest:\n  kind: http\n  method: GET\n  path: /alpha\n",
    );
    let config = root.path().join("testdet/testdet.yaml");

    let (exit, output) = show(Some(config), "nope", Format::Text);
    assert_eq!(exit, Exit::Usage);
    assert!(output.contains("unknown operation `nope`"));
    assert!(output.contains("known operations: `alpha`, `zed`"));
}

#[test]
fn show_failing_config_prints_text_diagnostics_and_exits_one() {
    let (_root, config) = build_config(invalid_operation(), "bad.yaml");
    let (exit, output) = show(Some(config), "bad", Format::Json);
    assert_eq!(exit, Exit::ConfigInvalid);
    // `show` never emits JSON for a config that fails to load: diagnostics are
    // text (stderr), so even in json mode the payload is not a JSON document.
    assert!(!output.trim_start().starts_with('{'));
    assert!(!output.trim_start().starts_with('['));
    assert!(output.contains("E_FOREIGN_FIELD"));
}

#[test]
fn ndjson_is_a_usage_error_for_show() {
    let (_root, config) = build_config(clean_operation(), "upload.yaml");
    let (exit, output) = show(Some(config), "upload", Format::Ndjson);
    assert_eq!(exit, Exit::Usage);
    assert!(output.contains("`--format ndjson` only applies to `run`"));
}
