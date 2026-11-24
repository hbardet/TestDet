//! Integration tests for environment resolution and the §7.19 precedence table.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;

use testdet_core::config::discover::{Config, LoadOptions, load_config};
use testdet_core::config::model::{HeaderMap, Manifest, User};
use testdet_core::config::resolve::resolve;
use testdet_core::diagnostic::{Severity, codes};
use testdet_core::operation::model::{GraphqlRequest, Operation, Request};

/// Write `content` at `dir/<relative>`, creating parent directories.
fn write(dir: &Path, relative: &str, content: &str) {
    let path = dir.join(relative);
    std::fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    std::fs::write(path, content).unwrap();
}

/// A `LoadOptions` rooted at `dir`, walking up from it with no `--config`.
fn load_options(dir: &Path) -> LoadOptions {
    LoadOptions {
        config: None,
        cwd: dir.to_path_buf(),
        env: None,
    }
}

/// Build a manifest from JSON, exercising the real serde deserialisation path
/// (camelCase keys, defaults, unknown-field rejection) without a filesystem tree.
fn manifest_from(value: Value) -> Manifest {
    serde_json::from_value(value).expect("valid manifest")
}

/// Build a user from JSON.
fn user_from(value: Value) -> User {
    serde_json::from_value(value).expect("valid user")
}

/// Assemble a `Config` directly, for tests whose focus is the merge logic.
fn config_with(
    manifest: Manifest,
    users: BTreeMap<String, User>,
    default_login: Option<String>,
) -> Config {
    Config {
        manifest,
        root_dir: PathBuf::from("/tmp/testdet"),
        operations: BTreeMap::new(),
        users,
        secrets: None,
        secrets_env: "local".to_string(),
        default_login,
        operation_files: BTreeMap::new(),
        documents: BTreeMap::new(),
    }
}

/// A header map from `(name, Option<value>)` pairs; `None` means unset.
fn headers(pairs: &[(&str, Option<&str>)]) -> HeaderMap {
    HeaderMap(
        pairs
            .iter()
            .map(|(name, value)| (name.to_string(), value.map(str::to_string)))
            .collect(),
    )
}

/// The resolved value of one header, or `None` when absent.
fn header<'a>(map: &'a HeaderMap, name: &str) -> Option<&'a str> {
    map.0.get(name).and_then(|value| value.as_deref())
}

/// A minimal GraphQL operation carrying `request_headers` and `vars`.
fn graphql_op(headers: HeaderMap, vars: BTreeMap<String, Value>) -> Operation {
    Operation {
        id: "getUser".into(),
        description: None,
        tags: vec![],
        request: Request::Graphql(GraphqlRequest {
            path: None,
            query: "query { user { id } }".into(),
            operation_name: None,
            variables: BTreeMap::new(),
            headers,
        }),
        vars,
        files: BTreeMap::new(),
        capture: BTreeMap::new(),
    }
}

#[test]
fn headers_merge_defaults_then_env_then_user_then_operation() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "defaultEnvironment": "local",
        "defaults": { "headers": { "X-Layer": "default", "X-Defaults-Only": "d" } },
        "environments": {
            "local": {
                "baseUrl": "http://localhost:8080",
                "headers": { "X-Layer": "env", "X-Env-Only": "e" }
            }
        }
    }));
    let users = BTreeMap::from([(
        "alice".to_string(),
        user_from(json!({ "headers": { "X-Layer": "user", "X-User-Only": "u" } })),
    )]);
    let config = config_with(manifest, users, None);
    let resolved = resolve(&config, None, &[]).unwrap();
    let operation = graphql_op(
        headers(&[("X-Layer", Some("operation")), ("X-Op-Only", Some("o"))]),
        BTreeMap::new(),
    );

    let merged = resolved.headers_for(&operation, Some("alice"));

    assert_eq!(header(&merged, "X-Layer"), Some("operation"));
    assert_eq!(header(&merged, "X-Defaults-Only"), Some("d"));
    assert_eq!(header(&merged, "X-Env-Only"), Some("e"));
    assert_eq!(header(&merged, "X-User-Only"), Some("u"));
    assert_eq!(header(&merged, "X-Op-Only"), Some("o"));
}

#[test]
fn a_user_named_login_is_not_implicitly_applied_when_no_user_is_selected() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "defaultEnvironment": "local",
        "defaults": { "headers": { "X-Base": "b" } },
        "environments": {
            "local": { "baseUrl": "http://localhost:8080" }
        }
    }));
    let users = BTreeMap::from([(
        "login".to_string(),
        user_from(json!({ "headers": { "X-Login-User": "sneaky" } })),
    )]);
    // `defaultLogin` names an operation, never a user; a user literally named
    // `login` must not leak into the headers of an anonymous request.
    let config = config_with(manifest, users, Some("login".to_string()));
    let resolved = resolve(&config, None, &[]).unwrap();
    let operation = graphql_op(HeaderMap::default(), BTreeMap::new());

    let merged = resolved.headers_for(&operation, None);

    assert_eq!(header(&merged, "X-Login-User"), None);
    assert_eq!(header(&merged, "X-Base"), Some("b"));
}

#[test]
fn a_null_header_removes_an_inherited_one() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "defaultEnvironment": "local",
        "defaults": { "headers": { "X-Trace": "trace", "X-Keep": "k" } },
        "environments": {
            "local": {
                "baseUrl": "http://localhost:8080",
                "headers": { "X-Trace": null }
            }
        }
    }));
    let config = config_with(manifest, BTreeMap::new(), None);
    let resolved = resolve(&config, None, &[]).unwrap();
    let operation = graphql_op(HeaderMap::default(), BTreeMap::new());

    let merged = resolved.headers_for(&operation, None);

    assert_eq!(header(&merged, "X-Trace"), None);
    assert_eq!(header(&merged, "X-Keep"), Some("k"));
}

#[test]
fn vars_precedence_is_operation_env_user_then_cli() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "defaultEnvironment": "local",
        "environments": {
            "local": {
                "baseUrl": "http://localhost:8080",
                "vars": { "userId": "env", "envOnly": "e" }
            }
        }
    }));
    let users = BTreeMap::from([(
        "alice".to_string(),
        user_from(json!({ "vars": { "userId": "user", "userOnly": "u" } })),
    )]);
    let config = config_with(manifest, users, None);
    let operation = graphql_op(
        HeaderMap::default(),
        BTreeMap::from([("userId".to_string(), Value::String("op".into()))]),
    );

    let resolved = resolve(&config, None, &[("userId".to_string(), "cli".to_string())]).unwrap();
    let vars = resolved.vars_for(&operation, Some("alice"));
    assert_eq!(vars.get("userId").and_then(Value::as_str), Some("cli"));
    assert_eq!(vars.get("envOnly").and_then(Value::as_str), Some("e"));
    assert_eq!(vars.get("userOnly").and_then(Value::as_str), Some("u"));

    let resolved_no_cli = resolve(&config, None, &[]).unwrap();
    let vars = resolved_no_cli.vars_for(&operation, Some("alice"));
    assert_eq!(vars.get("userId").and_then(Value::as_str), Some("user"));
}

#[test]
fn an_environment_may_not_introduce_a_user() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        r#"
version: 1
name: my-api
defaultEnvironment: local
collections: []
environments:
  local:
    baseUrl: http://localhost:8080
    users:
      ghost:
        token: "{{env.GHOST_TOKEN}}"
"#,
    );

    let config = load_config(&load_options(root.path())).expect("valid manifest");
    let diagnostics = resolve(&config, None, &[]).unwrap_err();

    let err = diagnostics
        .iter()
        .find(|d| d.code == codes::E_UNKNOWN_USER)
        .expect("E_UNKNOWN_USER");
    assert_eq!(err.severity, Severity::Error);
    assert_eq!(err.path.as_deref(), Some("environments.local.users.ghost"));
    assert!(
        err.message.contains("ghost"),
        "message names the user, got: {:?}",
        err.message
    );
    assert!(
        err.hint.as_deref().unwrap().contains("testdet/users.yaml"),
        "hint names users.yaml, got: {:?}",
        err.hint
    );
}

#[test]
fn an_environment_may_override_one_field_of_an_existing_user() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        r#"
version: 1
name: my-api
defaultEnvironment: staging
collections: []
environments:
  staging:
    baseUrl: https://staging.example.com
    users:
      admin:
        credentials:
          password: "{{env.STAGING_ADMIN_PASSWORD}}"
"#,
    );
    write(
        root.path(),
        "testdet/users.yaml",
        r#"
users:
  admin:
    credentials:
      email: "{{env.ADMIN_EMAIL}}"
      password: "{{env.ADMIN_PASSWORD}}"
"#,
    );

    let config = load_config(&load_options(root.path())).unwrap();
    let resolved = resolve(&config, None, &[]).unwrap();

    let admin = resolved.users.get("admin").expect("admin survives");
    assert_eq!(
        admin.credentials.get("email").map(String::as_str),
        Some("{{env.ADMIN_EMAIL}}")
    );
    assert_eq!(
        admin.credentials.get("password").map(String::as_str),
        Some("{{env.STAGING_ADMIN_PASSWORD}}")
    );
}

#[test]
fn an_environment_may_unset_a_users_file_token_with_null() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        r#"
version: 1
name: my-api
defaultEnvironment: staging
collections: []
environments:
  staging:
    baseUrl: https://staging.example.com
    users:
      admin:
        token: null
"#,
    );
    write(
        root.path(),
        "testdet/users.yaml",
        r#"
users:
  admin:
    credentials:
      email: "{{env.ADMIN_EMAIL}}"
    token: "{{env.ADMIN_TOKEN}}"
"#,
    );

    let config = load_config(&load_options(root.path())).unwrap();
    let resolved = resolve(&config, None, &[]).unwrap();

    let admin = resolved.users.get("admin").expect("admin survives");
    assert_eq!(
        admin.token,
        Some(None),
        "`token: null` unsets the users-file token"
    );
}

#[test]
fn default_environment_must_name_a_declared_environment() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "defaultEnvironment": "prod",
        "environments": {
            "local": { "baseUrl": "http://localhost:8080" }
        }
    }));
    let config = config_with(manifest, BTreeMap::new(), None);
    let diagnostics = resolve(&config, None, &[]).unwrap_err();

    let err = diagnostics
        .iter()
        .find(|d| d.code == codes::E_UNKNOWN_ENV)
        .expect("E_UNKNOWN_ENV");
    assert_eq!(err.path.as_deref(), Some("defaultEnvironment"));
    assert_eq!(err.message, "unknown environment `prod`");
    assert_eq!(err.hint.as_deref(), Some("declared environments: `local`"));
}

#[test]
fn no_environments_block_means_a_single_local_env_from_top_level_base_url() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "baseUrl": "http://localhost:8080"
    }));
    let config = config_with(manifest, BTreeMap::new(), None);
    let resolved = resolve(&config, None, &[]).unwrap();

    assert_eq!(resolved.env_name, "local");
    assert_eq!(resolved.base_url.as_str(), "http://localhost:8080/");
}

#[test]
fn an_explicit_local_env_is_accepted_on_a_shorthand_config() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "baseUrl": "http://localhost:8080"
    }));
    let config = config_with(manifest, BTreeMap::new(), None);
    let resolved = resolve(&config, Some("local"), &[]).unwrap();

    assert_eq!(resolved.env_name, "local");
    assert_eq!(resolved.base_url.as_str(), "http://localhost:8080/");
}

#[test]
fn url_joining_does_not_double_the_slash() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "baseUrl": "http://h:8080/"
    }));
    let config = config_with(manifest, BTreeMap::new(), None);
    let resolved = resolve(&config, None, &[]).unwrap();

    assert_eq!(
        resolved.url_for("/v1/users").as_str(),
        "http://h:8080/v1/users"
    );
}

#[test]
fn url_joining_preserves_a_base_path_prefix() {
    let manifest = manifest_from(json!({
        "version": 1,
        "collections": [],
        "baseUrl": "https://h/api/"
    }));
    let config = config_with(manifest, BTreeMap::new(), None);
    let resolved = resolve(&config, None, &[]).unwrap();

    assert_eq!(resolved.url_for("/v1/x").as_str(), "https://h/api/v1/x");
}

#[test]
fn collections_and_operation_queries_are_not_overridable() {
    let root = TempDir::new().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        r#"
version: 1
name: my-api
defaultEnvironment: local
collections: []
environments:
  local:
    baseUrl: http://localhost:8080
    collections:
      - operations
"#,
    );

    let diagnostics = load_config(&load_options(root.path())).unwrap_err();
    let err = diagnostics
        .iter()
        .find(|d| {
            d.code == codes::E_UNKNOWN_FIELD
                && d.path.as_deref() == Some("environments.local.collections")
        })
        .expect("E_UNKNOWN_FIELD at environments.local.collections");
    assert_eq!(err.severity, Severity::Error);
}
