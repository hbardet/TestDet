//! Integration tests for cross-file validation (Task 11).
//!
//! Each test builds a temp-dir config tree and loads it through `load_config`
//! (or `load_diagnostics` for warnings-only cases) so the assertions prove the
//! wiring — cross-check runs after config load — not just the function.

use std::path::{Path, PathBuf};

use tempfile::TempDir;

use testdet_core::config::discover::{LoadOptions, load_config, load_diagnostics};
use testdet_core::diagnostic::{Severity, codes};

/// Write `content` at `dir/<relative>`, creating parent directories.
fn write(dir: &Path, relative: &str, content: &str) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A `LoadOptions` rooted at `dir`, walking up from it with no `--config`.
fn from_cwd(dir: &Path) -> LoadOptions {
    LoadOptions {
        config: None,
        cwd: dir.to_path_buf(),
        env: None,
    }
}

/// The canonical path of an operation file in a collection directory.
fn op_file(root: &Path, collection: &str, id: &str) -> PathBuf {
    std::fs::canonicalize(
        root.join("testdet")
            .join(collection)
            .join(format!("{id}.yaml")),
    )
    .unwrap()
}

/// The canonical path of the users file.
fn users_file(root: &Path) -> PathBuf {
    std::fs::canonicalize(root.join("testdet").join("users.yaml")).unwrap()
}

#[test]
fn default_login_must_name_an_existing_operation() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users/login.yaml",
        "id: login\nrequest:\n  kind: http\n  method: POST\n  path: /login\n",
    );
    write(root.path(), "testdet/users.yaml", "defaultLogin: nope\n");

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_UNRESOLVED_REF)
        .collect();

    assert_eq!(hits.len(), 1, "exactly one unresolved defaultLogin");
    let diagnostic = hits[0];
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.path.as_deref(), Some("defaultLogin"));
    assert_eq!(
        diagnostic.file.as_deref(),
        Some(users_file(root.path()).as_path())
    );
    assert!(
        diagnostic.message.contains("nope"),
        "message names the id, got: {:?}",
        diagnostic.message
    );
    assert!(
        diagnostic
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("`login`")),
        "hint lists the existing operation, got: {:?}",
        diagnostic.hint
    );
}

#[test]
fn a_per_user_login_override_must_exist() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users/login.yaml",
        "id: login\nrequest:\n  kind: http\n  method: POST\n  path: /login\n",
    );
    write(
        root.path(),
        "testdet/users.yaml",
        "defaultLogin: login\nusers:\n  alice:\n    credentials:\n      email: alice@example.com\n    login: loginWithInvite\n",
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_UNRESOLVED_REF)
        .collect();

    assert_eq!(hits.len(), 1, "exactly one unresolved per-user login");
    let diagnostic = hits[0];
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.path.as_deref(), Some("users.alice.login"));
    assert_eq!(
        diagnostic.file.as_deref(),
        Some(users_file(root.path()).as_path())
    );
    assert!(
        diagnostic.message.contains("loginWithInvite"),
        "message names the id, got: {:?}",
        diagnostic.message
    );
    assert!(
        diagnostic
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("`login`")),
        "hint lists the existing operation, got: {:?}",
        diagnostic.hint
    );
}

#[test]
fn every_file_token_has_a_matching_files_entry() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users/upload.yaml",
        "id: upload\nrequest:\n  kind: http\n  method: POST\n  path: /upload\n  body:\n    kind: json\n    value:\n      avatar: \"{{file.avatar}}\"\nfiles:\n  logo:\n    path: ./logo.png\n  banner:\n    path: ./banner.png\n",
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_UNKNOWN_FILE)
        .collect();

    assert_eq!(hits.len(), 1, "exactly one unknown file reference");
    let diagnostic = hits[0];
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.path.as_deref(), Some("body.value.avatar"));
    assert_eq!(
        diagnostic.file.as_deref(),
        Some(op_file(root.path(), "users", "upload").as_path())
    );
    assert!(
        diagnostic.message.contains("`avatar`"),
        "message names the file, got: {:?}",
        diagnostic.message
    );
    assert!(
        diagnostic
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("`banner`") && hint.contains("`logo`")),
        "hint lists the declared files sorted, got: {:?}",
        diagnostic.hint
    );
}

#[test]
fn every_env_token_is_declared_somewhere() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ndefaultEnvironment: dev\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/secrets/dev.yaml",
        "API_TOKEN:\n  description: api token\n",
    );
    write(
        root.path(),
        "testdet/users/call.yaml",
        "id: call\nrequest:\n  kind: http\n  method: GET\n  path: /call\n  headers:\n    Authorization: \"Bearer {{env.API_TOKEN}}\"\nvars:\n  apiKey: \"{{env.MISSING}}\"\n",
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_UNKNOWN_ENV)
        .collect();

    assert_eq!(hits.len(), 1, "exactly one undeclared env reference");
    let diagnostic = hits[0];
    assert_eq!(diagnostic.severity, Severity::Error);
    assert!(
        diagnostic.span.is_some(),
        "cross-file diagnostics carry a span into the operation file"
    );
    assert_eq!(diagnostic.path.as_deref(), Some("vars.apiKey"));
    assert_eq!(
        diagnostic.file.as_deref(),
        Some(op_file(root.path(), "users", "call").as_path())
    );
    assert!(
        diagnostic.message.contains("`MISSING`"),
        "message names the secret, got: {:?}",
        diagnostic.message
    );
    assert!(
        diagnostic
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("testdet/secrets/dev.yaml")),
        "hint names the secrets file, got: {:?}",
        diagnostic.hint
    );
}

#[test]
fn every_session_token_is_captured_by_the_login_operation() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users.yaml",
        "defaultLogin: login\nusers:\n  alice:\n    credentials:\n      email: alice@example.com\n      password: hunter2\n",
    );
    write(
        root.path(),
        "testdet/users/login.yaml",
        "id: login\nrequest:\n  kind: http\n  method: POST\n  path: /login\n  body:\n    kind: json\n    value:\n      email: \"{{session.email}}\"\n      password: \"{{session.password}}\"\ncapture:\n  token: data.login.token\n",
    );
    write(
        root.path(),
        "testdet/users/profile.yaml",
        "id: profile\nrequest:\n  kind: http\n  method: GET\n  path: /profile\n  headers:\n    Authorization: \"Bearer {{session.token}}\"\nvars:\n  refresh: \"{{session.refresh}}\"\n",
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_UNKNOWN_VAR)
        .collect();

    assert_eq!(hits.len(), 1, "exactly one uncaptured session reference");
    let diagnostic = hits[0];
    assert_eq!(diagnostic.severity, Severity::Error);
    assert_eq!(diagnostic.path.as_deref(), Some("vars.refresh"));
    assert_eq!(
        diagnostic.file.as_deref(),
        Some(op_file(root.path(), "users", "profile").as_path())
    );
    assert!(
        diagnostic.message.contains("`login`"),
        "message names the login operation, got: {:?}",
        diagnostic.message
    );
    assert!(
        diagnostic
            .hint
            .as_deref()
            .is_some_and(|hint| hint.contains("`token`")),
        "hint lists the login's capture names, got: {:?}",
        diagnostic.hint
    );
}

#[test]
fn an_unused_files_entry_is_a_warning() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users/upload.yaml",
        "id: upload\nrequest:\n  kind: http\n  method: POST\n  path: /upload\n  body:\n    kind: multipart\n    value:\n      doc: \"{{file.used}}\"\nfiles:\n  used:\n    path: ./doc.txt\n  extra:\n    path: ./extra.txt\n",
    );

    // Warnings only: the config still loads successfully.
    let config = load_config(&from_cwd(root.path())).unwrap();
    assert!(config.operations.contains_key("upload"));

    let diagnostics = load_diagnostics(&from_cwd(root.path()));
    assert!(
        !diagnostics.has_errors(),
        "an unused file is a warning, not an error"
    );
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::W_UNUSED_VAR)
        .collect();

    assert_eq!(hits.len(), 1, "exactly one unused file warning");
    let diagnostic = hits[0];
    assert_eq!(diagnostic.severity, Severity::Warning);
    assert_eq!(diagnostic.path.as_deref(), Some("files.extra"));
    assert_eq!(
        diagnostic.file.as_deref(),
        Some(op_file(root.path(), "users", "upload").as_path())
    );
    assert!(
        diagnostic.message.contains("`extra`"),
        "message names the file, got: {:?}",
        diagnostic.message
    );
}

#[test]
fn a_config_without_a_secrets_directory_treats_env_references_as_required() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ndefaultEnvironment: dev\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users/call.yaml",
        "id: call\nrequest:\n  kind: http\n  method: GET\n  path: /call\n  headers:\n    Authorization: \"Bearer {{env.API_TOKEN}}\"\n",
    );

    // No testdet/secrets/ directory: the config is valid; the env reference is
    // simply required/undeclared rather than an E_UNKNOWN_ENV error.
    let config = load_config(&from_cwd(root.path())).unwrap();
    assert!(config.operations.contains_key("call"));
}

#[test]
fn malformed_templates_are_reported_by_validate() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        "version: 1\nname: my-api\ncollections: [users]\n",
    );
    write(
        root.path(),
        "testdet/users/call.yaml",
        "id: call\nrequest:\n  kind: graphql\n  query: \"{{var.id\"\n  headers:\n    Authorization: \"{{ }}\"\n",
    );

    let diagnostics = load_config(&from_cwd(root.path())).unwrap_err();
    let hits: Vec<_> = diagnostics
        .iter()
        .filter(|d| d.code == codes::E_WRONG_TYPE)
        .collect();

    assert_eq!(hits.len(), 2, "one E_WRONG_TYPE per malformed template");
    assert!(
        hits.iter()
            .all(|d| d.message.contains("malformed template")),
        "each malformed diagnostic names the template, got: {:?}",
        hits.iter().map(|d| &d.message).collect::<Vec<_>>()
    );
}
