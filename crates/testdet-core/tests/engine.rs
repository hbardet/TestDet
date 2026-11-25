//! Integration tests for the HTTP transport and the defensive GraphQL
//! envelope (Task 16, spec §7.9/§7.10).
//!
//! Every test drives the real `execute` entry point against a wiremock server
//! (or a dead port, for the connect-refusal case), so the request that reaches
//! the wire is the request the engine actually built.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;

use bytes::Bytes;
use serde_json::{Value, json};
use wiremock::matchers::{body_json, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

use testdet_core::RunContext;
use testdet_core::TransportKind;
use testdet_core::config::discover::Config;
use testdet_core::config::model::Manifest;
use testdet_core::config::resolve::{Resolved, resolve};
use testdet_core::engine::{EnvelopeShape, execute, read_envelope};
use testdet_core::operation::model::{GraphqlRequest, Operation, Request};
use testdet_core::redact::Redactor;
use testdet_core::session::SessionStore;

/// A resolved environment pointing at `base_url`, with `retries` transport
/// retries and one manifest-default header.
fn resolved(base_url: &str, retries: u32) -> Arc<Resolved> {
    let manifest: Manifest = serde_json::from_value(json!({
        "version": 1,
        "collections": [],
        "graphqlPath": "/graphql",
        "defaults": { "headers": { "X-Default": "default-value" } },
        "environments": {
            "test": {
                "baseUrl": base_url,
                "transport": { "retries": retries }
            }
        },
        "defaultEnvironment": "test"
    }))
    .expect("fixture manifest is valid");

    let config = Config {
        manifest,
        root_dir: PathBuf::from("/tmp/testdet"),
        operations: BTreeMap::new(),
        users: BTreeMap::new(),
        secrets: None,
        secrets_env: "test".to_string(),
        default_login: None,
        operation_files: BTreeMap::new(),
        documents: BTreeMap::new(),
    };

    Arc::new(resolve(&config, None, &[]).expect("fixture config resolves"))
}

/// A context over `resolved` with an empty redaction registry.
fn context(resolved: Arc<Resolved>) -> RunContext {
    RunContext::new(resolved, Arc::new(Redactor::new()))
}

/// A GraphQL operation with `vars` (a JSON object).
fn graphql_op(id: &str, request: GraphqlRequest, vars: Value) -> Operation {
    Operation {
        id: id.to_string(),
        description: None,
        tags: Vec::new(),
        request: Request::Graphql(request),
        vars: serde_json::from_value(vars).expect("operation vars are an object"),
        files: BTreeMap::new(),
        capture: BTreeMap::new(),
    }
}

/// A bare GraphQL operation whose query echoes the fixtures, with no
/// templating.
fn bare_graphql_op(id: &str) -> Operation {
    graphql_op(
        id,
        GraphqlRequest {
            path: None,
            query: "query { viewer { id } }".to_string(),
            operation_name: None,
            variables: BTreeMap::new(),
            headers: Default::default(),
        },
        json!({}),
    )
}

/// The number of times `execute` hit `server`.
async fn request_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .expect("mock server records requests")
        .len()
}

#[tokio::test]
async fn a_500_is_not_retried_by_default() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;

    let mut store = SessionStore::new(context(resolved(&server.uri(), 0)));
    let outcome = execute(&mut store, &bare_graphql_op("broken")).await;

    assert_eq!(outcome.status, 500);
    assert!(outcome.transport_error.is_none());
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn posts_a_graphql_envelope_with_query_variables_and_operation_name() {
    const QUERY: &str = "query Q($n: Int) { user(id: $n) { id } }";
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .and(body_json(json!({
            "query": QUERY,
            "variables": { "n": 7 },
            "operationName": "Q"
        })))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": { "user": { "id": "1" } } })),
        )
        .mount(&server)
        .await;

    let mut variables = BTreeMap::new();
    variables.insert("n".to_string(), json!("{{var.n}}"));

    let operation = graphql_op(
        "getUser",
        GraphqlRequest {
            path: None,
            query: QUERY.to_string(),
            operation_name: Some("Q".to_string()),
            variables,
            headers: Default::default(),
        },
        json!({ "n": 7 }),
    );

    let mut store = SessionStore::new(context(resolved(&server.uri(), 0)));
    let outcome = execute(&mut store, &operation).await;

    assert_eq!(outcome.status, 200);
    assert!(outcome.transport_error.is_none());
    assert_eq!(request_count(&server).await, 1);
}

#[tokio::test]
async fn a_200_with_errors_is_a_successful_outcome_carrying_the_errors() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [
                { "message": "first problem" },
                {
                    "message": "second problem",
                    "path": ["user", "id"],
                    "extensions": { "code": "FORBIDDEN" }
                }
            ]
        })))
        .mount(&server)
        .await;

    let mut store = SessionStore::new(context(resolved(&server.uri(), 0)));
    let outcome = execute(&mut store, &bare_graphql_op("errors")).await;

    assert_eq!(outcome.status, 200);
    assert!(outcome.transport_error.is_none());
    assert_eq!(outcome.graphql_errors.len(), 2);
    assert_eq!(outcome.graphql_errors[0].message, "first problem");
    assert!(outcome.graphql_errors[0].path.is_none());
    assert_eq!(
        outcome.graphql_errors[1].path.as_deref(),
        Some(["user".to_string(), "id".to_string()].as_slice())
    );
    assert_eq!(
        outcome.graphql_errors[1]
            .extensions
            .as_ref()
            .expect("extensions kept")["code"],
        "FORBIDDEN"
    );
}

#[tokio::test]
async fn a_connection_refusal_is_a_transport_error_with_no_status() {
    let mut store = SessionStore::new(context(resolved("http://127.0.0.1:1", 0)));
    let outcome = execute(&mut store, &bare_graphql_op("refused")).await;

    assert_eq!(outcome.status, 0);
    let error = outcome.transport_error.expect("transport error recorded");
    assert_eq!(error.kind, TransportKind::Connect);
    assert!(outcome.graphql_errors.is_empty());
}

#[tokio::test]
async fn a_body_that_is_not_json_is_reported_not_panicked() {
    // A PNG header: valid bytes, never valid UTF-8/JSON.
    let png: Vec<u8> = vec![0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0xff];

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(png.clone()))
        .mount(&server)
        .await;

    let mut store = SessionStore::new(context(resolved(&server.uri(), 0)));
    let outcome = execute(&mut store, &bare_graphql_op("binary")).await;

    assert_eq!(outcome.status, 200);
    assert!(outcome.transport_error.is_none());
    assert!(outcome.graphql_errors.is_empty());
    assert!(outcome.body_retained);
    assert_eq!(outcome.body, Some(Bytes::from(png.clone())));
    assert_eq!(outcome.body_size, png.len() as u64);
    assert_eq!(read_envelope(&png).1, EnvelopeShape::NotJson);
}

#[tokio::test]
async fn a_graphql_body_that_is_null_or_an_array_degrades_gracefully() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(b"null".to_vec()))
        .mount(&server)
        .await;

    let mut store = SessionStore::new(context(resolved(&server.uri(), 0)));
    let outcome = execute(&mut store, &bare_graphql_op("nullBody")).await;

    assert_eq!(outcome.status, 200);
    assert!(outcome.graphql_errors.is_empty());
    assert_eq!(outcome.body, Some(Bytes::from_static(b"null")));

    assert_eq!(read_envelope(b"null").1, EnvelopeShape::NotAnObject);
    assert_eq!(read_envelope(b"[]").1, EnvelopeShape::NotAnObject);
}

#[tokio::test]
async fn errors_that_is_not_an_array_is_not_a_crash() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(br#"{"errors": "boom"}"#.to_vec()))
        .mount(&server)
        .await;

    let mut store = SessionStore::new(context(resolved(&server.uri(), 0)));
    let outcome = execute(&mut store, &bare_graphql_op("badErrors")).await;

    assert!(outcome.graphql_errors.is_empty());
    assert_eq!(
        read_envelope(br#"{"errors": "boom"}"#).1,
        EnvelopeShape::ErrorsNotAnArray
    );
}

#[tokio::test]
async fn the_request_echo_carries_the_final_headers_after_all_merges() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .mount(&server)
        .await;

    let mut headers = testdet_core::config::model::HeaderMap::default();
    headers
        .0
        .insert("X-Override".to_string(), Some("from-operation".to_string()));
    headers.0.insert(
        "Authorization".to_string(),
        Some("Bearer {{var.token}}".to_string()),
    );

    let operation = graphql_op(
        "withHeaders",
        GraphqlRequest {
            path: None,
            query: "query { viewer { id } }".to_string(),
            operation_name: None,
            variables: BTreeMap::new(),
            headers,
        },
        json!({ "token": "s3cr3t-token" }),
    );

    let mut store = SessionStore::new(context(resolved(&server.uri(), 0)));
    let outcome = execute(&mut store, &operation).await;

    // Default layer survives, operation layer wins over the default layer.
    assert_eq!(header(&outcome, "x-default"), Some("default-value"));
    assert_eq!(header(&outcome, "x-override"), Some("from-operation"));
    assert_eq!(
        header(&outcome, "authorization"),
        Some("Bearer s3cr3t-token")
    );
    // Exactly one value per header name: no duplicate from the merge.
    assert_eq!(
        outcome
            .request
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .count(),
        1
    );

    // The same headers reached the wire.
    let requests = server
        .received_requests()
        .await
        .expect("mock server records requests");
    let wire = &requests[0].headers;
    assert_eq!(
        wire.get("x-default").map(|v| v.to_str().unwrap()),
        Some("default-value")
    );
    assert_eq!(
        wire.get("x-override").map(|v| v.to_str().unwrap()),
        Some("from-operation")
    );
    assert_eq!(
        wire.get("authorization").map(|v| v.to_str().unwrap()),
        Some("Bearer s3cr3t-token")
    );
}

#[tokio::test]
async fn cancellation_before_send_yields_a_cancelled_outcome_without_touching_the_network() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .mount(&server)
        .await;

    let ctx = context(resolved(&server.uri(), 0));
    ctx.cancel.cancel();
    let mut store = SessionStore::new(ctx);
    let outcome = execute(&mut store, &bare_graphql_op("cancelled")).await;

    assert_eq!(outcome.status, 0);
    let error = outcome.transport_error.expect("transport error recorded");
    assert_eq!(error.kind, TransportKind::Cancelled);
    assert_eq!(error.message, "cancelled");
    assert_eq!(request_count(&server).await, 0);
}

/// The value of `name` in the request echo, case-insensitively.
fn header<'a>(outcome: &'a testdet_core::Outcome, name: &str) -> Option<&'a str> {
    outcome
        .request
        .headers
        .iter()
        .find(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str())
}
