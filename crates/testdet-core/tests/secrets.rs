//! Integration tests for secret sources, `required_vars` and
//! `secret_warnings` (Task 13).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;

use testdet_core::config::crosscheck::crosscheck;
use testdet_core::config::discover::Config;
use testdet_core::config::model::{
    HeaderMap, Manifest, SecretSource as SourceKind, SecretSpec, SecretsDeclaration,
};
use testdet_core::config::resolve::SecretsConfig;
use testdet_core::diagnostic::{Severity, codes};
use testdet_core::operation::model::{HttpRequest, Operation, Request};
use testdet_core::secrets::{Origin, build_source, required_vars, secret_warnings};

/// Write `content` at `dir/<relative>`, creating parent directories.
fn write(dir: &Path, relative: &str, content: &str) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    std::fs::write(path, content).unwrap();
}

/// Build a manifest from JSON, exercising the real serde deserialisation path.
fn manifest_from(value: Value) -> Manifest {
    serde_json::from_value(value).expect("valid manifest")
}

/// A minimal HTTP operation whose `path` is the only scannable template text.
fn http_op(id: &str, path: &str) -> Operation {
    Operation {
        id: id.into(),
        description: None,
        tags: vec![],
        request: Request::Http(HttpRequest {
            method: "GET".into(),
            path: path.into(),
            body: None,
            headers: HeaderMap::default(),
        }),
        vars: BTreeMap::new(),
        files: BTreeMap::new(),
        capture: BTreeMap::new(),
    }
}

/// Assemble a `Config` directly, for tests whose focus is the secret logic.
fn config_with(
    manifest: Manifest,
    root_dir: PathBuf,
    operations: BTreeMap<String, Operation>,
    secrets: Option<SecretsDeclaration>,
) -> Config {
    Config {
        manifest,
        root_dir,
        operations,
        users: BTreeMap::new(),
        secrets,
        secrets_env: "local".to_string(),
        default_login: None,
        operation_files: BTreeMap::new(),
        documents: BTreeMap::new(),
    }
}

/// A declared secret with no default.
fn no_default() -> SecretSpec {
    SecretSpec {
        description: None,
        default_value: None,
    }
}

#[test]
fn shell_env_beats_dotenv_which_beats_the_declared_default() {
    // A unique variable name so the single-process test binary never collides
    // with the ambient environment; removed before the test returns.
    let name = "TESTDET_TASK13_SHELL_BEATS_DOTENV_BEATS_DEFAULT";
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        ".env.staging",
        &format!("{name}=file-secret\n"),
    );

    let mut declaration = SecretsDeclaration::default();
    declaration.0.insert(
        name.to_string(),
        SecretSpec {
            description: None,
            default_value: Some("default-secret".to_string()),
        },
    );

    let dotenv_cfg = SecretsConfig {
        source: SourceKind::Dotenv,
        file: Some(root.path().join(".env.staging")),
        declaration: Some(declaration.clone()),
    };
    let source = build_source(&dotenv_cfg, root.path());

    // 1. shell beats dotenv.
    unsafe { std::env::set_var(name, "shell-secret") };
    let lookup = source.lookup(name);
    assert_eq!(lookup.value.as_deref(), Some("shell-secret"));
    assert_eq!(lookup.origin, Some(Origin::Shell));

    // 2. dotenv beats the declared default (shell removed).
    unsafe { std::env::remove_var(name) };
    let lookup = source.lookup(name);
    assert_eq!(lookup.value.as_deref(), Some("file-secret"));
    assert_eq!(
        lookup.origin,
        Some(Origin::Dotenv(root.path().join(".env.staging")))
    );

    // 3. the declared default beats unset (an env-only source has no file).
    let env_cfg = SecretsConfig {
        source: SourceKind::Env,
        file: None,
        declaration: Some(declaration),
    };
    let env_source = build_source(&env_cfg, root.path());
    let lookup = env_source.lookup(name);
    assert_eq!(lookup.value.as_deref(), Some("default-secret"));
    assert_eq!(lookup.origin, Some(Origin::Default));

    unsafe { std::env::remove_var(name) };
}

#[test]
fn an_undeclared_reference_is_an_error_and_an_unset_declared_name_is_a_warning() {
    let root = TempDir::new().unwrap();
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "defaultEnvironment": "local",
        "environments": {
            "local": { "baseUrl": "http://localhost:8080" }
        }
    }));

    let mut secrets = SecretsDeclaration::default();
    secrets.0.insert("DECLARED".into(), no_default());

    let config = config_with(
        manifest,
        root.path().to_path_buf(),
        BTreeMap::from([(
            "op".to_string(),
            http_op("op", "/x/{{env.DECLARED}}/{{env.MISSING}}"),
        )]),
        Some(secrets),
    );

    // The undeclared reference is an error, from crosscheck (not duplicated).
    let cross = crosscheck(&config);
    let err = cross
        .iter()
        .find(|d| d.code == codes::E_UNKNOWN_ENV && d.message.contains("MISSING"))
        .expect("E_UNKNOWN_ENV for MISSING");
    assert_eq!(err.severity, Severity::Error);

    // The declared-but-unset reference is a warning, from secret_warnings.
    let warnings = secret_warnings(&config);
    assert_eq!(warnings.len(), 1, "exactly one unset-secret warning");
    let warning = warnings.iter().next().expect("one warning");
    assert_eq!(warning.code, codes::W_UNSET_SECRET);
    assert_eq!(warning.severity, Severity::Warning);
    assert!(warning.message.contains("DECLARED"));
    assert!(
        !warnings.iter().any(|d| d.code == codes::E_UNKNOWN_ENV),
        "secret_warnings must not duplicate the undeclared-reference error"
    );
}

#[test]
fn a_config_with_no_declaration_file_treats_every_reference_as_required() {
    let root = TempDir::new().unwrap();
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "baseUrl": "http://localhost:8080"
    }));

    let config = config_with(
        manifest,
        root.path().to_path_buf(),
        BTreeMap::from([
            ("opA".to_string(), http_op("opA", "/a/{{env.A}}")),
            ("opB".to_string(), http_op("opB", "/b/{{env.B}}")),
        ]),
        None,
    );

    let requirements = required_vars(&config);

    assert_eq!(requirements.len(), 2, "two references -> two requirements");
    let a = &requirements["A"];
    assert!(!a.declared, "no declaration file: A is undeclared");
    assert!(!a.set, "A is not set in this process");
    assert_eq!(a.used_by, vec!["opA".to_string()]);

    let b = &requirements["B"];
    assert!(!b.declared);
    assert!(!b.set);
    assert_eq!(b.used_by, vec!["opB".to_string()]);
}

#[test]
fn required_vars_reports_the_origin_never_the_value() {
    let root = TempDir::new().unwrap();
    write(root.path(), ".env.staging", "API_TOKEN=s3cr3t-value\n");

    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "defaultEnvironment": "staging",
        "environments": {
            "staging": {
                "baseUrl": "https://staging.example.com",
                "secrets": { "source": "dotenv", "file": ".env.staging" }
            }
        }
    }));

    let mut secrets = SecretsDeclaration::default();
    secrets.0.insert("API_TOKEN".into(), no_default());

    let config = config_with(
        manifest,
        root.path().to_path_buf(),
        BTreeMap::from([(
            "login".to_string(),
            http_op("login", "/login/{{env.API_TOKEN}}"),
        )]),
        Some(secrets),
    );

    let requirements = required_vars(&config);
    let requirement = &requirements["API_TOKEN"];

    assert!(requirement.declared);
    assert!(requirement.set);
    assert_eq!(
        requirement.origin,
        Some(Origin::Dotenv(root.path().join(".env.staging")))
    );

    let rendered = serde_json::to_string(requirement).expect("serializes");
    assert!(
        rendered.contains(".env.staging"),
        "the origin path appears: {rendered}"
    );
    assert!(
        !rendered.contains("s3cr3t"),
        "the secret value must never appear: {rendered}"
    );
}
