//! End-to-end smoke over the committed fixture tree (Task 23, spec §7.15).
//!
//! `tests/fixtures/e2e/testdet/` is the reference config the README shows: a
//! GraphQL login that captures a token, a GraphQL query that spends it and
//! captures a value, and a REST read that puts that value in its path. The tree
//! is committed with the reserved `.invalid` authority (it never resolves), so
//! it stays port-free; this test copies the tree into a temporary directory,
//! rewrites that authority to the `wiremock` server, and then spawns the real
//! binary once per command. Nothing here can reach a real network.
//!
//! `CARGO_BIN_EXE_*` is only defined for the crate that builds the binary,
//! which is why these live here and not in `testdet-cli/tests`.

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Output;

use serde_json::Value;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// The committed tree — the directory a backend repo would call `testdet/`.
const FIXTURE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/e2e/testdet");

/// The reserved authority the committed manifest carries. `.invalid` is
/// reserved by RFC 2606, so an accidental run outside this test cannot reach a
/// service; the test substitutes the mock's `host:port` for it.
const PLACEHOLDER: &str = "testdet.invalid";

const SESSION_TOKEN: &str = "e2e-session-token";
/// The id the login response reports (the user's default project).
const DEFAULT_PROJECT_ID: &str = "proj-default";
/// The id `projects` lists and captures; `projects_detail` must ask for this
/// one, not the login default, which proves the path used `projects`' capture.
const PROJECT_ID: &str = "proj-42";
const DETAIL_PATH: &str = "/projects/proj-42";
const PASSWORD_MARKER: &str = "e2e-password-marker-3f9c";
const UNUSED_MARKER: &str = "e2e-unused-marker-7b21";

/// Copy `from` into `to`, recursively, creating `to`.
fn copy_tree(from: &Path, to: &Path) {
    fs::create_dir_all(to).unwrap();
    for entry in fs::read_dir(from).unwrap() {
        let entry = entry.unwrap();
        let target = to.join(entry.file_name());
        if entry.file_type().unwrap().is_dir() {
            copy_tree(&entry.path(), &target);
        } else {
            fs::copy(entry.path(), &target).unwrap();
        }
    }
}

/// Copy the committed tree into `root/testdet/` and point its `baseUrl` at
/// `server`. Keeping the `testdet/` directory name is what lets the binary
/// discover the config by walking up from `root`, exactly as it does for a
/// real repo.
fn stage(server: &MockServer) -> tempfile::TempDir {
    let root = tempfile::tempdir().unwrap();
    let tree = root.path().join("testdet");
    copy_tree(Path::new(FIXTURE), &tree);

    let manifest = tree.join("testdet.yaml");
    let text = fs::read_to_string(&manifest).unwrap();
    let authority = server.uri().trim_start_matches("http://").to_string();
    fs::write(&manifest, text.replace(PLACEHOLDER, &authority)).unwrap();
    root
}

/// Spawn the built binary in `root` (the parent of the copied `testdet/`), with
/// the ambient environment scrubbed of the two fixture variables so the
/// declaration defaults are the origin the assertions see.
async fn run_cli(root: &Path, args: &[&str]) -> Output {
    let root: PathBuf = root.to_path_buf();
    let args: Vec<String> = args.iter().map(|arg| (*arg).to_string()).collect();

    // `spawn_blocking`: the child talks to the mock server, whose tasks run on
    // this runtime; blocking an async worker would deadlock the test.
    tokio::task::spawn_blocking(move || {
        let mut command = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"));
        command.current_dir(&root).args(&args);
        command.env_remove("E2E_PASSWORD");
        command.env_remove("E2E_UNUSED");
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

/// The three endpoints the fixture's operations reach.
async fn mount_fixture(server: &MockServer) {
    Mock::given(method("POST"))
        .and(path("/auth/login"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "login": { "token": SESSION_TOKEN, "project": { "id": DEFAULT_PROJECT_ID } } }
        })))
        .mount(server)
        .await;

    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "projects": [{ "id": PROJECT_ID, "name": "Demo" }] }
        })))
        .mount(server)
        .await;

    Mock::given(method("GET"))
        .and(path(DETAIL_PATH))
        .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
            "data": { "project": { "id": PROJECT_ID, "name": "Demo" } }
        })))
        .mount(server)
        .await;
}

#[tokio::test]
async fn the_fixture_tree_validates_runs_and_reports_from_a_single_binary() {
    let server = MockServer::start().await;
    mount_fixture(&server).await;
    let root = stage(&server);

    // validate: the fixture is genuinely valid, so text format renders nothing.
    let output = run_cli(root.path(), &["validate"]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "validate stderr: {}",
        stderr(&output)
    );
    assert!(
        stdout(&output).is_empty() && stderr(&output).is_empty(),
        "a clean config renders no diagnostics"
    );

    // run: one ndjson object per selected operation, in id order, and the
    // capture chain actually happened on the wire.
    let output = run_cli(
        root.path(),
        &["run", "--all", "--format", "json", "--no-body"],
    )
    .await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "run stderr: {}",
        stderr(&output)
    );
    let objects = lines(&output);
    assert_eq!(objects.len(), 3, "three operations, three ndjson lines");
    let ids: Vec<&str> = objects
        .iter()
        .map(|object| object["operation"].as_str().expect("operation is a string"))
        .collect();
    assert_eq!(ids, ["login", "projects", "projects_detail"]);
    for object in &objects {
        assert_eq!(object["status"], 200, "outcome: {object}");
    }
    assert!(
        !stdout(&output).contains(SESSION_TOKEN),
        "the captured token leaked into stdout: {}",
        stdout(&output)
    );

    let requests = server.received_requests().await.expect("recorded");
    assert_eq!(requests.len(), 3, "one request per operation");
    assert_eq!(requests[0].url.path(), "/auth/login");
    assert_eq!(requests[1].url.path(), "/graphql");
    assert_eq!(requests[2].url.path(), DETAIL_PATH);
    assert!(
        !requests
            .iter()
            .any(|request| request.url.path() == format!("/projects/{DEFAULT_PROJECT_ID}")),
        "the path must use `projects`' capture, not the login default"
    );
    let authorization = |index: usize| {
        requests[index]
            .headers
            .get("authorization")
            .and_then(|value| value.to_str().ok())
    };
    assert_eq!(
        authorization(1),
        Some(format!("Bearer {SESSION_TOKEN}").as_str()),
        "the login token reached the second request"
    );
    assert_eq!(
        authorization(2),
        Some(format!("Bearer {SESSION_TOKEN}").as_str()),
        "the login token reached the third request"
    );

    // env: both declared variables and their origins, never a value.
    let output = run_cli(root.path(), &["env"]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "env stderr: {}",
        stderr(&output)
    );
    let listed = stdout(&output);
    assert!(listed.contains("E2E_PASSWORD"), "{listed}");
    assert!(listed.contains("E2E_UNUSED"), "{listed}");
    assert_eq!(
        listed.matches("set <- declared default").count(),
        2,
        "{listed}"
    );
    assert!(!listed.contains(PASSWORD_MARKER), "{listed}");
    assert!(!listed.contains(UNUSED_MARKER), "{listed}");

    // config --env staging: the staging origin, and no secret value anywhere.
    let output = run_cli(root.path(), &["config", "--env", "staging"]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "config stderr: {}",
        stderr(&output)
    );
    let view = stdout(&output);
    assert!(view.contains("env: staging"), "{view}");
    assert!(
        view.contains("baseUrl: https://staging.example.test"),
        "{view}"
    );
    assert!(!view.contains(PASSWORD_MARKER), "{view}");
    assert!(!view.contains(UNUSED_MARKER), "{view}");

    // config (local) resolves the declaration defaults; it must still report
    // presence, never the value.
    let output = run_cli(root.path(), &["config"]).await;
    assert_eq!(
        output.status.code(),
        Some(0),
        "config stderr: {}",
        stderr(&output)
    );
    let local = stdout(&output);
    assert_eq!(local.matches("set: true").count(), 2, "{local}");
    assert!(!local.contains(PASSWORD_MARKER), "{local}");
    assert!(!local.contains(UNUSED_MARKER), "{local}");
}

/// The committed tree itself — placeholder authority and all — validates clean.
#[test]
fn the_committed_fixture_tree_validates_clean() {
    let config = Path::new(FIXTURE).join("testdet.yaml");
    let output = std::process::Command::new(env!("CARGO_BIN_EXE_testdet"))
        .args(["validate", "--format", "json", "--config"])
        .arg(&config)
        .output()
        .expect("spawn validate --format json");

    assert_eq!(output.status.code(), Some(0), "stderr: {}", stderr(&output));
    let parsed: Vec<Value> =
        serde_json::from_slice(&output.stdout).expect("stdout is a JSON array");
    assert!(
        parsed.is_empty(),
        "the fixture must validate clean: {parsed:?}"
    );
}
