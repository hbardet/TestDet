//! End-to-end tests for `testdet run` (Task 18, spec §7.15).
//!
//! Every test spawns the real binary (`CARGO_BIN_EXE_testdet`) against a
//! `wiremock` server and a temporary config tree, so the requests counted here
//! are the ones that reached the wire and the exit codes are the ones a shell
//! sees. `CARGO_BIN_EXE_*` is only defined for the crate that builds the
//! binary, which is why these live here and not in `testdet-cli/tests`.

use std::fs;
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// Write `content` at `root/<relative>`, creating parent directories.
fn write(root: &Path, relative: &str, content: &str) {
    let path = root.join(relative);
    fs::create_dir_all(path.parent().expect("fixture path has a parent")).unwrap();
    fs::write(path, content).unwrap();
}

/// A manifest with one `local` environment pointing at `base_url`.
fn manifest(base_url: &str) -> String {
    format!(
        "version: 1\nname: run-api\ncollections: [ops]\ndefaultEnvironment: local\n\
         environments:\n  local:\n    baseUrl: {base_url}\n"
    )
}

/// A minimal `kind: http` GET operation, with an optional `tags:` line.
fn http_op(id: &str, endpoint: &str, tags: &str) -> String {
    format!("id: {id}\nrequest:\n  kind: http\n  method: GET\n  path: {endpoint}\n{tags}")
}

/// A port nothing listens on: bind, read the port, drop the listener. Used to
/// provoke a real connect error without stopping the mock server.
fn closed_port() -> u16 {
    let listener = TcpListener::bind("127.0.0.1:0").expect("bind an ephemeral port");
    let port = listener.local_addr().expect("local addr").port();
    drop(listener);
    port
}

/// Spawn the built binary in `root`, with the ambient environment scrubbed of
/// the variables these tests assert on so they cannot pass or fail by accident.
async fn run_cli(root: &Path, args: &[&str], envs: &[(&str, &str)]) -> Output {
    let root: PathBuf = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();
    let envs: Vec<(String, String)> = envs
        .iter()
        .map(|(key, value)| (key.to_string(), value.to_string()))
        .collect();

    // `spawn_blocking`: the child talks to the mock server, whose tasks run on
    // this runtime; blocking an async worker would deadlock the test.
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"));
        command.current_dir(&root).args(&args);
        command.env_remove("TESTDET_RUN_TOKEN");
        command.env_remove("TESTDET_RUN_MISSING");
        command.env_remove("LEAKY_TOKEN");
        command.env_remove("USERS_ONLY_PASSWORD");
        for (key, value) in &envs {
            command.env(key, value);
        }
        command.output().expect("spawn testdet")
    })
    .await
    .expect("the spawned binary finished")
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

/// Parse ndjson stdout into one value per non-empty line.
fn lines(output: &Output) -> Vec<Value> {
    stdout(output)
        .lines()
        .filter(|line| !line.trim().is_empty())
        .map(|line| serde_json::from_str(line).expect("every ndjson line parses"))
        .collect()
}

async fn count(server: &MockServer, endpoint: &str) -> usize {
    server
        .received_requests()
        .await
        .expect("the mock server records requests")
        .iter()
        .filter(|request| request.url.path() == endpoint)
        .count()
}

/// A catch-all 200 with a JSON body, mounted once for every test that only
/// cares about the fan-out.
async fn respond_ok(server: &MockServer) {
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn no_user_flag_runs_anonymously_and_never_logs_in() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "data": {} })))
        .mount(&server)
        .await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/login.yaml",
        &http_op("login", "/login", "").replace("GET", "POST"),
    );
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo", ""),
    );
    // A user with a login: an accidental authenticated run would send it.
    write(
        root.path(),
        "testdet/users.yaml",
        "defaultLogin: login\nusers:\n  alice:\n    credentials:\n      email: alice@example.test\n",
    );

    let output = run_cli(root.path(), &["run", "echo"], &[]).await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert_eq!(
        count(&server, "/login").await,
        0,
        "anonymous must not log in"
    );
    assert_eq!(count(&server, "/echo").await, 1);
    assert!(stdout(&output).contains("user: anonymous"));
}

#[tokio::test]
async fn multiple_operations_emit_one_json_object_per_line() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    for id in ["alpha", "beta", "gamma"] {
        write(
            root.path(),
            &format!("testdet/ops/{id}.yaml"),
            &http_op(id, &format!("/{id}"), ""),
        );
    }

    let output = run_cli(root.path(), &["run", "--all", "--format", "json"], &[]).await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let objects = lines(&output);
    assert_eq!(objects.len(), 3, "three operations, three ndjson lines");
    let ids: Vec<&str> = objects
        .iter()
        .map(|object| object["operation"].as_str().expect("operation is a string"))
        .collect();
    assert_eq!(ids, ["alpha", "beta", "gamma"]);
    for object in &objects {
        assert_eq!(object["status"], 200);
    }
}

#[tokio::test]
async fn all_operations_run_even_when_one_fails() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/alpha.yaml",
        &http_op("alpha", "/alpha", ""),
    );
    write(
        root.path(),
        "testdet/ops/broken.yaml",
        &http_op(
            "broken",
            &format!("http://127.0.0.1:{}/broken", closed_port()),
            "",
        ),
    );
    write(
        root.path(),
        "testdet/ops/gamma.yaml",
        &http_op("gamma", "/gamma", ""),
    );

    let output = run_cli(root.path(), &["run", "--all", "--format", "ndjson"], &[]).await;

    let objects = lines(&output);
    assert_eq!(objects.len(), 3, "every selected operation ran");
    assert_eq!(objects[1]["operation"], "broken");
    assert!(
        !objects[1]["transport_error"].is_null(),
        "the middle operation failed at transport level: {}",
        objects[1]
    );
    assert_eq!(output.status.code(), Some(2));
}

#[tokio::test]
async fn exit_code_is_the_worst_observed() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/alpha.yaml",
        &http_op("alpha", "/alpha", ""),
    );
    write(
        root.path(),
        "testdet/ops/broken.yaml",
        &http_op(
            "broken",
            &format!("http://127.0.0.1:{}/broken", closed_port()),
            "",
        ),
    );

    let output = run_cli(root.path(), &["run", "--all", "--format", "json"], &[]).await;

    assert_eq!(lines(&output).len(), 2);
    assert_eq!(
        output.status.code(),
        Some(2),
        "one connect error outranks the successes"
    );
}

#[tokio::test]
async fn config_invalid_exits_one_before_any_request() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    // `collections: [missing]` names a directory that does not exist.
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()).replace("[ops]", "[missing]"),
    );

    let output = run_cli(root.path(), &["run", "--all"], &[]).await;

    assert_eq!(output.status.code(), Some(1));
    assert!(
        stderr(&output).contains("E_UNKNOWN_FILE"),
        "{}",
        stderr(&output)
    );
    assert!(
        output.stdout.is_empty(),
        "diagnostics are not machine output"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "an invalid config must issue no request"
    );
}

#[tokio::test]
async fn unknown_id_exits_three() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo", ""),
    );

    let output = run_cli(root.path(), &["run", "nope"], &[]).await;

    assert_eq!(output.status.code(), Some(3));
    let diagnostics = stderr(&output);
    assert!(diagnostics.contains("nope"), "{}", diagnostics);
    assert!(
        diagnostics.contains("`echo`"),
        "the hint lists the known ids: {diagnostics}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty()
    );
}

#[tokio::test]
async fn no_body_sets_body_to_null_and_keeps_body_size() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo", ""),
    );

    let output = run_cli(
        root.path(),
        &["run", "echo", "--no-body", "--format", "json"],
        &[],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    // One selected operation under `json` is a single pretty object, not ndjson.
    let object: Value = serde_json::from_str(stdout(&output).trim()).expect("one JSON object");
    assert!(object["body"].is_null(), "body is dropped: {}", object);
    assert_eq!(object["body_retained"], false);
    assert!(
        object["body_size"].as_u64().expect("body_size is a number") > 0,
        "body_size survives body dropping: {}",
        object
    );
}

#[tokio::test]
async fn tag_selection_matches_any_listed_tag() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/alpha.yaml",
        &http_op("alpha", "/alpha", "tags: [smoke, daily]\n"),
    );
    write(
        root.path(),
        "testdet/ops/beta.yaml",
        &http_op("beta", "/beta", "tags: [daily]\n"),
    );
    write(
        root.path(),
        "testdet/ops/gamma.yaml",
        &http_op("gamma", "/gamma", "tags: [weekly]\n"),
    );

    let output = run_cli(
        root.path(),
        &["run", "--tag", "daily", "--format", "json"],
        &[],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let ids: Vec<String> = lines(&output)
        .iter()
        .map(|object| {
            object["operation"]
                .as_str()
                .expect("operation is a string")
                .to_string()
        })
        .collect();
    assert_eq!(ids, ["alpha", "beta"]);
    assert_eq!(count(&server, "/gamma").await, 0);
}

#[tokio::test]
async fn an_unset_secret_fails_the_run_naming_the_variable() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &format!(
            "version: 1\nname: run-api\ncollections: [ops]\ndefaultEnvironment: local\n\
             environments:\n  local:\n    baseUrl: {}\n    secrets:\n      source: env\n",
            server.uri()
        ),
    );
    write(
        root.path(),
        "testdet/secrets/local.yaml",
        "TESTDET_RUN_MISSING:\n  description: never set\n",
    );
    write(
        root.path(),
        "testdet/ops/guarded.yaml",
        &http_op("guarded", "/guarded/{{env.TESTDET_RUN_MISSING}}", ""),
    );

    let output = run_cli(root.path(), &["run", "guarded"], &[]).await;

    assert_eq!(output.status.code(), Some(1));
    let diagnostics = stderr(&output);
    assert!(
        diagnostics.contains("TESTDET_RUN_MISSING"),
        "the diagnostic names the variable: {diagnostics}"
    );
    assert!(
        diagnostics.contains("secrets/local.yaml"),
        "the diagnostic names the declaration file: {diagnostics}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "an unset secret must not become an empty header or path segment"
    );
}

#[tokio::test]
async fn a_registered_secret_never_appears_on_stdout() {
    let server = MockServer::start().await;
    respond_ok(&server).await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &format!(
            "version: 1\nname: run-api\ncollections: [ops]\ndefaultEnvironment: local\n\
             environments:\n  local:\n    baseUrl: {}\n    secrets:\n      source: env\n",
            server.uri()
        ),
    );
    write(
        root.path(),
        "testdet/secrets/local.yaml",
        "TESTDET_RUN_TOKEN:\n  description: a real secret\n",
    );
    // The token ends up in the request URL, which `Outcome::redact` scrubs.
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo/{{env.TESTDET_RUN_TOKEN}}", ""),
    );

    let output = run_cli(
        root.path(),
        &["run", "echo", "--format", "json"],
        &[("TESTDET_RUN_TOKEN", "super-secret-value")],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    assert!(
        !stdout(&output).contains("super-secret-value"),
        "the registered secret leaked: {}",
        stdout(&output)
    );
    assert!(stdout(&output).contains("-CENSORED"), "{}", stdout(&output));
}

/// The secret from the first review: an `{{env.*}}` used by an operation with
/// no declaration file at all.
const LEAKY: &str = "zhah2Aeb0Ohpoo9qua6sha4eiph3Aes";

#[tokio::test]
async fn f1_an_env_secret_referenced_by_an_operation_never_leaks() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/leaky.yaml",
        "id: leaky\nrequest:\n  kind: http\n  method: GET\n  path: /leak/{{env.LEAKY_TOKEN}}\n  headers:\n    authorization: Bearer {{env.LEAKY_TOKEN}}\n",
    );
    // Deliberately no `testdet/secrets/local.yaml`: the reference alone must
    // seed the redactor (F1).

    let output = run_cli(
        root.path(),
        &["run", "leaky", "--format", "json"],
        &[("LEAKY_TOKEN", LEAKY)],
    )
    .await;

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let rendered = stdout(&output);
    assert!(
        !rendered.contains(LEAKY),
        "the JSON output leaked: {rendered}"
    );
    // The echo carries the censor tag in both the URL and the header.
    assert!(
        rendered.contains("<LEAKY_TOKEN>-CENSORED"),
        "no censor tag: {rendered}"
    );
    let parsed: Value = serde_json::from_str(rendered.trim()).expect("one JSON object");
    assert!(
        !parsed["request"]["url"]
            .as_str()
            .unwrap_or("")
            .contains(LEAKY),
        "{parsed}"
    );

    // It was the REAL value on the wire: the reference did resolve.
    let requests = server.received_requests().await.expect("recorded");
    let request = requests.first().expect("one request");
    assert_eq!(request.url.path(), format!("/leak/{LEAKY}"));
    assert_eq!(
        request
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok()),
        Some(format!("Bearer {LEAKY}").as_str())
    );
}

#[tokio::test]
async fn f3_a_500_login_exits_non_zero() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/users.yaml",
        "defaultLogin: login\nusers:\n  alice:\n    credentials:\n      email: alice@example.test\n",
    );
    write(
        root.path(),
        "testdet/ops/login.yaml",
        "id: login\nrequest:\n  kind: http\n  method: POST\n  path: /login\n",
    );
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo", ""),
    );

    let output = run_cli(root.path(), &["run", "echo", "--user", "alice"], &[]).await;

    assert_ne!(
        output.status.code(),
        Some(0),
        "a 500 login must not read as success"
    );
    let diagnostics = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        diagnostics.contains("could not log in") && diagnostics.contains("500"),
        "the diagnostic names the login failure: {diagnostics}"
    );
    assert_eq!(
        count(&server, "/echo").await,
        0,
        "no operation runs after a failed login"
    );
}

#[tokio::test]
async fn f3_a_graphql_errors_login_exits_non_zero() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "errors": [{ "message": "bad credentials" }]
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/users.yaml",
        "defaultLogin: login\nusers:\n  alice:\n    credentials:\n      email: alice@example.test\n",
    );
    write(
        root.path(),
        "testdet/ops/login.yaml",
        "id: login\nrequest:\n  kind: graphql\n  query: mutation { login }\n",
    );
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo", ""),
    );

    let output = run_cli(root.path(), &["run", "echo", "--user", "alice"], &[]).await;

    assert_ne!(output.status.code(), Some(0));
    let diagnostics = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        diagnostics.contains("bad credentials"),
        "the GraphQL errors reach the diagnostic: {diagnostics}"
    );
    assert_eq!(count(&server, "/echo").await, 0);
}

#[tokio::test]
async fn f4_a_users_only_secret_is_required_and_never_sent_as_a_template() {
    // Declared but unset: `run` refuses before the first request.
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/secrets/local.yaml",
        "USERS_ONLY_PASSWORD:\n  description: only used by users.yaml\n",
    );
    write(
        root.path(),
        "testdet/users.yaml",
        "defaultLogin: login\nusers:\n  alice:\n    credentials:\n      password: \"{{env.USERS_ONLY_PASSWORD}}\"\n",
    );
    write(
        root.path(),
        "testdet/ops/login.yaml",
        "id: login\nrequest:\n  kind: http\n  method: POST\n  path: /login\n  headers:\n    x-password: \"{{session.credentials.password}}\"\n",
    );
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo", ""),
    );

    let output = run_cli(root.path(), &["run", "echo", "--user", "alice"], &[]).await;
    assert_eq!(output.status.code(), Some(1));
    let diagnostics = stderr(&output);
    assert!(
        diagnostics.contains("USERS_ONLY_PASSWORD"),
        "the diagnostic names the secret: {diagnostics}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "no request when a required secret is unset"
    );

    // Declared and set: the resolved value is on the wire, never the template.
    let output = run_cli(
        root.path(),
        &["run", "echo", "--user", "alice"],
        &[("USERS_ONLY_PASSWORD", "user-secret-value")],
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));

    let requests = server.received_requests().await.expect("recorded");
    let login = requests
        .iter()
        .find(|request| request.url.path() == "/login")
        .expect("the login request ran");
    let header = login
        .headers
        .get("x-password")
        .and_then(|value| value.to_str().ok());
    assert_eq!(header, Some("user-secret-value"));
    assert!(
        !format!("{requests:?}").contains("{{env."),
        "no literal template on the wire"
    );
}

#[tokio::test]
async fn f8_an_unresolvable_template_is_a_config_error_with_no_request() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/broken.yaml",
        "id: broken\nrequest:\n  kind: http\n  method: GET\n  path: /broken\n  headers:\n    x-broken: \"{{var.missing}}\"\n",
    );

    let output = run_cli(root.path(), &["run", "broken"], &[]).await;

    assert_eq!(
        output.status.code(),
        Some(1),
        "a prepare failure is a config error, not an internal one"
    );
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        combined.contains("missing"),
        "one diagnostic names the variable: {combined}"
    );
    assert_eq!(
        combined.matches("does not resolve").count(),
        1,
        "the message is rendered once: {combined}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "no request when the request cannot be built"
    );
}

#[tokio::test]
async fn f9_capture_forces_body_retention_and_no_body_is_honoured_otherwise() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "captured-token-value"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/first"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "token": "retention-only-value"
        })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/second"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/plain"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({ "ok": true })))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/users.yaml",
        "defaultLogin: login\nusers:\n  alice:\n    credentials:\n      email: alice@example.test\n",
    );
    write(
        root.path(),
        "testdet/ops/login.yaml",
        "id: login\nrequest:\n  kind: http\n  method: POST\n  path: /login\ncapture:\n  token: token\n",
    );
    write(
        root.path(),
        "testdet/ops/first.yaml",
        "id: first\nrequest:\n  kind: http\n  method: GET\n  path: /first\ncapture:\n  copied: token\n",
    );
    write(
        root.path(),
        "testdet/ops/second.yaml",
        "id: second\nrequest:\n  kind: http\n  method: GET\n  path: /second\n  headers:\n    x-captured: \"{{session.token}}\"\n",
    );
    write(
        root.path(),
        "testdet/ops/plain.yaml",
        &http_op("plain", "/plain", ""),
    );

    // A capturing operation keeps its body even under `--no-body`, and the
    // captured value is the real one.
    let output = run_cli(
        root.path(),
        &[
            "run",
            "first",
            "second",
            "--user",
            "alice",
            "--no-body",
            "--format",
            "json",
        ],
        &[],
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let rendered = stdout(&output);
    // The echo is censored (F2), but the captured value stays usable.
    assert!(rendered.contains("<CAPTURE_TOKEN>-CENSORED"), "{rendered}");
    assert!(rendered.contains("\"body_retained\":true"), "{rendered}");

    let requests = server.received_requests().await.expect("recorded");
    let second = requests
        .iter()
        .find(|request| request.url.path() == "/second")
        .expect("the second request ran");
    assert_eq!(
        second
            .headers
            .get("x-captured")
            .and_then(|value| value.to_str().ok()),
        Some("captured-token-value")
    );

    // Without a capture, `--no-body` is honoured for an authenticated run too.
    let output = run_cli(
        root.path(),
        &[
            "run",
            "plain",
            "--user",
            "alice",
            "--no-body",
            "--format",
            "json",
        ],
        &[],
    )
    .await;
    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let parsed: Value = serde_json::from_str(stdout(&output).trim()).expect("one JSON object");
    assert!(parsed["body"].is_null(), "{parsed}");
    assert_eq!(parsed["body_retained"], false);
}

#[tokio::test]
async fn f10_text_data_goes_to_stdout_and_only_diagnostics_to_stderr() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200).set_body_string("ok"))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/echo.yaml",
        &http_op("echo", "/echo", ""),
    );

    // A declaration plus a reference gives `env` a row to print.
    write(
        root.path(),
        "testdet/secrets/local.yaml",
        "SOME_SECRET:\n  description: unused but listed\n",
    );
    write(
        root.path(),
        "testdet/ops/uses-secret.yaml",
        &http_op("uses-secret", "/s/{{env.SOME_SECRET}}", ""),
    );

    for args in [
        vec!["config"],
        vec!["show", "echo"],
        vec!["run", "echo"],
        vec!["env"],
    ] {
        let output = run_cli(root.path(), &args, &[]).await;
        assert_eq!(
            output.status.code(),
            Some(0),
            "{args:?} stderr: {}",
            stderr(&output)
        );
        assert!(
            !stdout(&output).is_empty(),
            "{args:?} text data belongs on stdout"
        );
        assert!(
            stderr(&output).is_empty(),
            "{args:?} printed diagnostics on success: {}",
            stderr(&output)
        );
    }
}

#[tokio::test]
async fn f14_an_unjoinable_path_exits_one_and_sends_nothing() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(200))
        .mount(&server)
        .await;

    let root = tempfile::tempdir().unwrap();
    write(
        root.path(),
        "testdet/testdet.yaml",
        &manifest(&server.uri()),
    );
    write(
        root.path(),
        "testdet/ops/bad-url.yaml",
        "id: bad-url\nrequest:\n  kind: http\n  method: GET\n  path: \"http://[::1\"\n",
    );

    let output = run_cli(root.path(), &["run", "bad-url"], &[]).await;

    assert_eq!(output.status.code(), Some(1));
    let combined = format!("{}{}", stdout(&output), stderr(&output));
    assert!(
        combined.contains("cannot build a request URL"),
        "the join failure is a diagnostic: {combined}"
    );
    assert!(
        server
            .received_requests()
            .await
            .expect("recorded")
            .is_empty(),
        "a failed join never falls back to the base URL"
    );
}
