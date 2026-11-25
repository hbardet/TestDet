//! Integration tests for per-user sessions (Task 17, spec §7.7).
//!
//! Every test drives the real `SessionStore`/`execute_for_user` path against a
//! wiremock server, so the cookies and the login requests asserted here are the
//! ones that reached the wire.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use serde_json::{Value, json};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, Respond, ResponseTemplate};

use testdet_core::config::discover::Config;
use testdet_core::config::model::Manifest;
use testdet_core::config::resolve::{Resolved, resolve};
use testdet_core::operation::model::Operation;
use testdet_core::redact::Redactor;
use testdet_core::session::{
    SessionStore, execute_for_user, is_auth_failure, resolve_capture_path,
};
use testdet_core::{Event, RunContext};

/// A resolved environment pointing at `base_url`, with `users`, `operations`
/// and an optional `defaultLogin`.
fn resolved_for(
    base_url: &str,
    users: Value,
    operations: Vec<Operation>,
    default_login: Option<&str>,
) -> Arc<Resolved> {
    let manifest: Manifest = serde_json::from_value(json!({
        "version": 1,
        "collections": [],
        "graphqlPath": "/graphql",
        "environments": { "test": { "baseUrl": base_url } },
        "defaultEnvironment": "test"
    }))
    .expect("fixture manifest is valid");

    let mut by_id = BTreeMap::new();
    for operation in operations {
        by_id.insert(operation.id.clone(), operation);
    }

    let config = Config {
        manifest,
        root_dir: PathBuf::from("/tmp/testdet"),
        operations: by_id,
        users: serde_json::from_value(users).expect("fixture users are valid"),
        secrets: None,
        secrets_env: "test".to_string(),
        default_login: default_login.map(str::to_string),
        operation_files: BTreeMap::new(),
        documents: BTreeMap::new(),
    };

    Arc::new(resolve(&config, None, &[]).expect("fixture config resolves"))
}

fn operation(value: Value) -> Operation {
    serde_json::from_value(value).expect("operation fixture is valid")
}

fn login_op(id: &str, endpoint: &str) -> Operation {
    operation(json!({
        "id": id,
        "request": { "kind": "http", "method": "POST", "path": endpoint }
    }))
}

fn login_op_capturing(id: &str, endpoint: &str) -> Operation {
    operation(json!({
        "id": id,
        "request": { "kind": "http", "method": "POST", "path": endpoint },
        "capture": { "token": "data.login.token" }
    }))
}

fn http_op(id: &str, endpoint: &str) -> Operation {
    operation(json!({
        "id": id,
        "request": { "kind": "http", "method": "POST", "path": endpoint }
    }))
}

fn graphql_op(id: &str) -> Operation {
    operation(json!({
        "id": id,
        "request": { "kind": "graphql", "query": "query { me { id } }" }
    }))
}

fn session_token_op(id: &str, endpoint: &str) -> Operation {
    operation(json!({
        "id": id,
        "request": {
            "kind": "http",
            "method": "GET",
            "path": endpoint,
            "headers": { "Authorization": "Bearer {{session.token}}" }
        }
    }))
}

fn two_users() -> Value {
    json!({
        "alice": { "credentials": { "email": "alice@example.test" }, "login": "loginA" },
        "admin": { "credentials": { "email": "admin@example.test" }, "login": "loginB" }
    })
}

fn store(resolved: Arc<Resolved>, redactor: Arc<Redactor>) -> SessionStore {
    SessionStore::new(RunContext::new(resolved, redactor))
}

async fn requests_to(server: &MockServer, endpoint: &str) -> Vec<Request> {
    server
        .received_requests()
        .await
        .expect("mock server records requests")
        .into_iter()
        .filter(|request| request.url.path() == endpoint)
        .collect()
}

async fn count_to(server: &MockServer, endpoint: &str) -> usize {
    requests_to(server, endpoint).await.len()
}

async fn last_header(server: &MockServer, endpoint: &str, name: &str) -> Option<String> {
    requests_to(server, endpoint)
        .await
        .last()
        .and_then(|request| request.headers.get(name))
        .and_then(|value| value.to_str().ok())
        .map(str::to_string)
}

/// A responder that answers 401 the first time and 200 afterwards, so a
/// re-login is observable without depending on mock ordering.
#[derive(Default)]
struct FirstUnauthorized {
    calls: AtomicUsize,
}

impl Respond for FirstUnauthorized {
    fn respond(&self, _request: &Request) -> ResponseTemplate {
        if self.calls.fetch_add(1, Ordering::SeqCst) == 0 {
            ResponseTemplate::new(401)
        } else {
            ResponseTemplate::new(200).set_body_json(json!({ "ok": true }))
        }
    }
}

#[tokio::test]
async fn two_users_hold_separate_cookie_jars() {
    let server = MockServer::start().await;
    for (endpoint, cookie) in [("/login/a", "sid=alice"), ("/login/b", "sid=admin")] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(
                ResponseTemplate::new(200).insert_header("Set-Cookie", format!("{cookie}; Path=/")),
            )
            .mount(&server)
            .await;
    }
    for endpoint in ["/echo/a", "/echo/b"] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
            .mount(&server)
            .await;
    }

    let resolved = resolved_for(
        &server.uri(),
        two_users(),
        vec![
            login_op("loginA", "/login/a"),
            login_op("loginB", "/login/b"),
            http_op("echoA", "/echo/a"),
            http_op("echoB", "/echo/b"),
        ],
        None,
    );
    let mut store = store(resolved, Arc::new(Redactor::new()));

    let echo_a = http_op("echoA", "/echo/a");
    let echo_b = http_op("echoB", "/echo/b");

    assert_eq!(
        execute_for_user(&mut store, Some("alice"), &echo_a)
            .await
            .status,
        200
    );
    assert_eq!(
        execute_for_user(&mut store, Some("admin"), &echo_b)
            .await
            .status,
        200
    );

    assert_eq!(
        last_header(&server, "/echo/a", "cookie").await.as_deref(),
        Some("sid=alice")
    );
    assert_eq!(
        last_header(&server, "/echo/b", "cookie").await.as_deref(),
        Some("sid=admin")
    );
}

#[tokio::test]
async fn switching_back_to_a_cached_user_logs_in_once() {
    let server = MockServer::start().await;
    for endpoint in ["/login/a", "/login/b", "/echo/a", "/echo/b"] {
        Mock::given(method("POST"))
            .and(path(endpoint))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
            .mount(&server)
            .await;
    }

    let resolved = resolved_for(
        &server.uri(),
        two_users(),
        vec![
            login_op("loginA", "/login/a"),
            login_op("loginB", "/login/b"),
            http_op("echoA", "/echo/a"),
            http_op("echoB", "/echo/b"),
        ],
        None,
    );
    let mut store = store(resolved, Arc::new(Redactor::new()));

    let echo_a = http_op("echoA", "/echo/a");
    let echo_b = http_op("echoB", "/echo/b");

    execute_for_user(&mut store, Some("alice"), &echo_a).await;
    execute_for_user(&mut store, Some("admin"), &echo_b).await;
    execute_for_user(&mut store, Some("alice"), &echo_a).await;

    assert_eq!(count_to(&server, "/login/a").await, 1);
    assert_eq!(count_to(&server, "/login/b").await, 1);
    assert_eq!(count_to(&server, "/echo/a").await, 2);
}

#[tokio::test]
async fn exactly_one_relogin_happens_after_a_401() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/a"))
        .respond_with(ResponseTemplate::new(200).insert_header("Set-Cookie", "sid=alice-1; Path=/"))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/protected"))
        .respond_with(FirstUnauthorized::default())
        .mount(&server)
        .await;

    let resolved = resolved_for(
        &server.uri(),
        two_users(),
        vec![
            login_op("loginA", "/login/a"),
            http_op("protected", "/protected"),
        ],
        None,
    );
    let mut store = store(resolved, Arc::new(Redactor::new()));

    let outcome = execute_for_user(
        &mut store,
        Some("alice"),
        &http_op("protected", "/protected"),
    )
    .await;

    assert_eq!(outcome.status, 200);
    assert_eq!(count_to(&server, "/login/a").await, 2);
    assert_eq!(count_to(&server, "/protected").await, 2);
}

#[tokio::test]
async fn a_failing_login_is_never_retried() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/a"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/protected"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let resolved = resolved_for(
        &server.uri(),
        two_users(),
        vec![
            login_op("loginA", "/login/a"),
            http_op("protected", "/protected"),
        ],
        None,
    );
    let mut store = store(resolved, Arc::new(Redactor::new()));

    let outcome = execute_for_user(
        &mut store,
        Some("alice"),
        &http_op("protected", "/protected"),
    )
    .await;

    assert_eq!(count_to(&server, "/login/a").await, 1);
    assert_eq!(count_to(&server, "/protected").await, 0);
    assert_eq!(outcome.status, 0);
    let error = outcome.transport_error.expect("login failure recorded");
    assert!(error.message.contains("alice"), "{}", error.message);
    assert!(error.message.contains("loginA"), "{}", error.message);
}

#[tokio::test]
async fn graphql_unauthenticated_is_detected_at_status_200() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": null,
            "errors": [
                { "message": "who are you?", "extensions": { "code": "UNAUTHENTICATED" } }
            ]
        })))
        .mount(&server)
        .await;

    let resolved = resolved_for(
        &server.uri(),
        two_users(),
        vec![login_op("loginA", "/login/a"), graphql_op("me")],
        None,
    );
    let mut store = store(resolved, Arc::new(Redactor::new()));

    let outcome = execute_for_user(&mut store, Some("alice"), &graphql_op("me")).await;

    assert_eq!(outcome.status, 200);
    assert!(is_auth_failure(&outcome));
    assert_eq!(count_to(&server, "/graphql").await, 2);
    assert_eq!(count_to(&server, "/login/a").await, 2);
}

#[tokio::test]
async fn a_captured_token_is_registered_with_the_redactor_and_never_echoed() {
    const TOKEN: &str = "tok-abc-123";

    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": { "login": { "token": TOKEN } }
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/protected"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;

    let resolved = resolved_for(
        &server.uri(),
        two_users(),
        vec![
            login_op_capturing("loginA", "/login/a"),
            session_token_op("protected", "/protected"),
        ],
        None,
    );
    let redactor = Arc::new(Redactor::new());
    let mut store = store(resolved, redactor.clone());

    let outcome = execute_for_user(
        &mut store,
        Some("alice"),
        &session_token_op("protected", "/protected"),
    )
    .await;

    // The token was captured, so the request carried it...
    assert_eq!(
        last_header(&server, "/protected", "authorization")
            .await
            .as_deref(),
        Some("Bearer tok-abc-123")
    );
    // ...the echo shows it scrubbed...
    assert_eq!(
        outcome
            .request
            .headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("authorization"))
            .map(|(_, value)| value.as_str()),
        Some("Bearer <CAPTURE_TOKEN>-CENSORED")
    );
    // ...and no representation of the outcome leaks it.
    assert!(!serde_json::to_string(&outcome).unwrap().contains(TOKEN));
    assert!(redactor.names().iter().any(|name| name == "CAPTURE_TOKEN"));
}

#[test]
fn resolve_capture_path_supports_array_indices_and_reports_misses() {
    let body = json!({ "data": { "users": [{ "id": "u1" }, { "id": "u2" }] } });

    assert_eq!(
        resolve_capture_path(&body, "data.users[0].id"),
        Some(json!("u1"))
    );
    assert_eq!(
        resolve_capture_path(&body, "data.users.1.id"),
        Some(json!("u2"))
    );
    assert_eq!(resolve_capture_path(&body, "data.users[2].id"), None);
    assert_eq!(resolve_capture_path(&body, "data.nope"), None);
    assert_eq!(resolve_capture_path(&body, "data.users[0"), None);
}

#[tokio::test]
async fn a_capture_miss_is_a_diagnostic_and_does_not_stop_other_captures() {
    let (sender, mut events) = tokio::sync::mpsc::unbounded_channel();
    let resolved = resolved_for(
        "http://127.0.0.1:1",
        two_users(),
        vec![login_op("loginA", "/login/a")],
        None,
    );
    let ctx = RunContext::new(resolved, Arc::new(Redactor::new())).with_events(Some(sender));
    let mut store = SessionStore::new(ctx);

    let mut capture = BTreeMap::new();
    capture.insert("userId".to_string(), "data.users[0].id".to_string());
    capture.insert("missing".to_string(), "data.nope".to_string());

    store.capture_into(
        Some("alice"),
        &capture,
        br#"{"data":{"users":[{"id":"u1"}]}}"#,
    );

    assert_eq!(store.vars(Some("alice")).get("userId"), Some(&json!("u1")));

    let mut codes = Vec::new();
    while let Ok(event) = events.try_recv() {
        if let Event::Failed { diagnostic } = event {
            codes.push((diagnostic.code, diagnostic.message));
        }
    }
    assert_eq!(codes.len(), 1);
    assert_eq!(codes[0].0, "E_UNRESOLVED_CAPTURE");
    assert!(codes[0].1.contains("data.nope"), "{}", codes[0].1);
}

#[tokio::test]
async fn an_operation_captures_its_own_response_for_later_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login/a"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": {} })))
        .mount(&server)
        .await;
    Mock::given(method("POST"))
        .and(path("/whoami"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "data": { "me": { "id": "u-42" } } })),
        )
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/later"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;

    let whoami = operation(json!({
        "id": "whoami",
        "request": { "kind": "http", "method": "POST", "path": "/whoami" },
        "capture": { "userId": "data.me.id" }
    }));
    let later = operation(json!({
        "id": "later",
        "request": {
            "kind": "http",
            "method": "GET",
            "path": "/later",
            "headers": { "X-User": "{{session.userId}}" }
        }
    }));

    let resolved = resolved_for(
        &server.uri(),
        two_users(),
        vec![
            login_op("loginA", "/login/a"),
            whoami.clone(),
            later.clone(),
        ],
        None,
    );
    let mut store = store(resolved, Arc::new(Redactor::new()));

    execute_for_user(&mut store, Some("alice"), &whoami).await;
    assert_eq!(
        store.vars(Some("alice")).get("userId"),
        Some(&json!("u-42"))
    );

    execute_for_user(&mut store, Some("alice"), &later).await;
    assert_eq!(
        last_header(&server, "/later", "x-user").await.as_deref(),
        Some("u-42")
    );
}

#[tokio::test]
async fn a_static_token_user_triggers_zero_auth_requests() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/protected"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
        .mount(&server)
        .await;

    let resolved = resolved_for(
        &server.uri(),
        json!({ "ci": { "token": "ci-token-123" } }),
        vec![http_op("protected", "/protected")],
        None,
    );
    let mut store = store(resolved, Arc::new(Redactor::new()));

    let outcome =
        execute_for_user(&mut store, Some("ci"), &http_op("protected", "/protected")).await;

    assert_eq!(outcome.status, 200);
    let requests = server
        .received_requests()
        .await
        .expect("mock server records requests");
    assert_eq!(requests.len(), 1);
    assert_eq!(
        requests[0]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some("Bearer ci-token-123")
    );
}
