//! Integration tests for typed conversion and the per-file rules pass.

use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

use testdet_core::config::Document;
use testdet_core::config::parse::{parse_manifest, parse_operation, parse_users};
use testdet_core::diagnostic::{Severity, codes};
use testdet_core::operation::model::{Body, Request};

/// Resolve a committed fixture under `tests/fixtures/<dir>/`.
fn fixture(dir: &str, name: &str) -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures")
        .join(dir)
        .join(name)
}

/// Load a committed valid-operation fixture.
fn load_valid(name: &str) -> Document {
    Document::load(&fixture("valid", name)).expect("valid fixture is loadable YAML")
}

/// Load a committed invalid-operation fixture.
fn load_invalid(name: &str) -> Document {
    Document::load(&fixture("invalid", name)).expect("invalid fixture is loadable YAML")
}

/// Load an inline YAML string from a unique temporary file.
fn load_str(source: &str) -> Result<Document, testdet_core::diagnostic::Diagnostics> {
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock before unix epoch")
        .as_nanos();
    let path = std::env::temp_dir().join(format!("testdet-parse-{unique}.yaml"));
    std::fs::write(&path, source).expect("failed to write temporary fixture");
    let result = Document::load(&path);
    let _ = std::fs::remove_file(&path);
    result
}

#[test]
fn parses_a_graphql_operation_with_a_block_scalar_query() {
    let operation = parse_operation(&load_valid("operation-graphql-block-scalar.yaml"))
        .expect("valid operation");

    assert_eq!(operation.id, "getUser");
    assert_eq!(
        operation.tags,
        vec!["users".to_string(), "query".to_string()]
    );

    match operation.request {
        Request::Graphql(request) => {
            assert!(
                request.query.contains("\n  user(id: $id)"),
                "block scalar preserved the indented query, got: {:?}",
                request.query
            );
            assert_eq!(
                request.variables.get("id").and_then(|v| v.as_str()),
                Some("{{var.userId}}")
            );
            assert_eq!(request.operation_name.as_deref(), Some("GetUser"));
            assert_eq!(request.path.as_deref(), Some("/graphql"));
        }
        other => panic!("expected a graphql request, got {other:?}"),
    }
}

#[test]
fn parses_a_rest_operation_with_a_json_body() {
    let operation =
        parse_operation(&load_valid("operation-rest-patch-json.yaml")).expect("valid operation");

    assert_eq!(operation.id, "renameUser");

    match operation.request {
        Request::Http(request) => {
            assert_eq!(request.method, "PATCH");
            assert_eq!(request.path, "/v1/users/{{var.userId}}");
            match request.body {
                Some(Body::Json(value)) => {
                    assert_eq!(
                        value["displayName"],
                        serde_json::json!("{{var.newName}}"),
                        "json body payload is preserved as typed JSON"
                    );
                }
                other => panic!("expected a json body, got {other:?}"),
            }
        }
        other => panic!("expected an http request, got {other:?}"),
    }
}

#[test]
fn method_on_a_graphql_request_is_a_foreign_field() {
    let diagnostics = parse_operation(&load_invalid("operation-graphql-foreign-method.yaml"))
        .expect_err("`method` on `kind: graphql` is rejected");

    let foreign = diagnostics
        .iter()
        .find(|d| d.code == codes::E_FOREIGN_FIELD)
        .expect("the foreign field is reported");
    assert_eq!(foreign.severity, Severity::Error);
    assert_eq!(foreign.path.as_deref(), Some("request.method"));
    assert!(foreign.span.is_some(), "foreign field carries a span");
    assert!(
        foreign.message.contains("method") && foreign.message.contains("kind: http"),
        "message names the key and the kind it belongs to, got: {:?}",
        foreign.message
    );
    assert_eq!(
        foreign.hint.as_deref(),
        Some("remove it, or switch `request.kind`")
    );

    // Dedup: the schema's collapsed `request` oneOf error is reclassified into
    // the specific E_FOREIGN_FIELD, not reported alongside it.
    assert!(
        !diagnostics.iter().any(
            |d| d.code == codes::E_UNKNOWN_FIELD && d.path.as_deref() == Some("request.method")
        ),
        "no duplicate unknown-field diagnostic at the foreign key path"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.code == codes::E_WRONG_TYPE && d.path.as_deref() == Some("request")),
        "the schema's oneOf collapse must not survive reclassification"
    );
}

#[test]
fn nested_form_value_is_rejected() {
    let diagnostics = parse_operation(&load_invalid("operation-form-nested.yaml"))
        .expect_err("a nested form value is rejected");

    let flat = diagnostics
        .iter()
        .find(|d| d.code == codes::E_FLAT_BODY_ONLY)
        .expect("the nested form value is reported");
    assert_eq!(flat.severity, Severity::Error);
    assert_eq!(flat.path.as_deref(), Some("request.body.value.a"));
    assert!(flat.span.is_some(), "flat-body diagnostic carries a span");
    assert!(
        flat.message.contains("a") && flat.message.contains("mapping"),
        "message names the offending key and its shape, got: {:?}",
        flat.message
    );
    assert_eq!(
        flat.hint.as_deref(),
        Some("flatten it, or use `kind: json` / `kind: multipart`")
    );

    assert!(
        !diagnostics
            .iter()
            .any(|d| d.code == codes::E_WRONG_TYPE
                && d.path.as_deref() == Some("request.body.value.a")),
        "no duplicate wrong-type diagnostic at the offending path"
    );
    assert!(
        !diagnostics
            .iter()
            .any(|d| d.code == codes::E_WRONG_TYPE && d.path.as_deref() == Some("request")),
        "the schema's oneOf collapse must not survive reclassification"
    );
}

#[test]
fn multipart_with_a_hand_set_content_type_is_forbidden() {
    let diagnostics = parse_operation(&load_invalid("operation-multipart-content-type.yaml"))
        .expect_err("a hand-set multipart Content-Type is rejected");

    let forbidden = diagnostics
        .iter()
        .find(|d| d.code == codes::E_FORBIDDEN_HEADER)
        .expect("the forbidden header is reported");
    assert_eq!(forbidden.severity, Severity::Error);
    assert_eq!(
        forbidden.path.as_deref(),
        Some("request.headers.Content-Type")
    );
    assert!(forbidden.span.is_some(), "forbidden header carries a span");
    assert!(
        forbidden.message.contains("Content-Type"),
        "message names the header, got: {:?}",
        forbidden.message
    );
    assert!(
        forbidden
            .hint
            .as_deref()
            .expect("forbidden header carries a hint")
            .contains("multipart boundary"),
        "hint explains TestDet owns the multipart boundary"
    );
}

#[test]
fn expect_is_rejected_in_v1() {
    let diagnostics = parse_operation(&load_invalid("operation-expect.yaml"))
        .expect_err("`expect:` does not exist in v1");

    let unknown = diagnostics
        .iter()
        .find(|d| d.code == codes::E_UNKNOWN_FIELD)
        .expect("the unknown `expect` field is reported");
    assert_eq!(unknown.severity, Severity::Error);
    assert_eq!(unknown.path.as_deref(), Some("expect"));
    assert!(
        unknown.message.contains("expect"),
        "message names the unknown field, got: {:?}",
        unknown.message
    );
}

#[test]
fn parses_a_minimal_manifest() {
    let document = load_str("version: 1\ncollections: [operations]\n").expect("valid manifest");
    let manifest = parse_manifest(&document).expect("manifest parses");
    assert_eq!(*manifest.version, 1);
    assert_eq!(manifest.collections, vec!["operations".to_string()]);
    assert_eq!(manifest.graphql_path, "/graphql");
}

#[test]
fn parses_a_users_file() {
    let document = load_str(
        "defaultLogin: login\nusers:\n  admin:\n    credentials:\n      email: \"{{env.ADMIN_EMAIL}}\"\n",
    )
    .expect("valid users file");
    let users = parse_users(&document).expect("users file parses");
    assert_eq!(users.default_login.as_deref(), Some("login"));
    assert_eq!(
        users
            .users
            .get("admin")
            .and_then(|u| u.credentials.get("email"))
            .map(String::as_str),
        Some("{{env.ADMIN_EMAIL}}")
    );
}
