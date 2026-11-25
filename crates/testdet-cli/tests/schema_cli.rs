//! `testdet schema` end to end against a mock introspection endpoint (§7.17).
//!
//! The command reuses the engine, so these tests assert on what actually
//! reached the wire: the query body (`--legacy`), and the JSON that lands on
//! stdout or in `--out`.

use std::fs;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};
use tempfile::TempDir;
use testdet_cli::commands::schema::{SchemaArgs, run_in};
use testdet_cli::exit::Exit;
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

/// The introspection result a healthy server returns.
fn introspection_result() -> Value {
    json!({
        "data": {
            "__schema": {
                "queryType": { "name": "Query" },
                "mutationType": null,
                "types": [
                    { "kind": "OBJECT", "name": "Query", "description": "The root." }
                ]
            }
        }
    })
}

fn runtime() -> tokio::runtime::Runtime {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a test runtime")
}

/// A config tree whose `local` environment points at `server`.
fn config_root(server: &str) -> TempDir {
    let root = tempfile::tempdir().expect("a temp directory");
    let manifest = root.path().join("testdet/testdet.yaml");
    fs::create_dir_all(manifest.parent().expect("has a parent")).expect("create the fixture dir");
    fs::write(
        &manifest,
        format!(
            "version: 1\n\
             name: schema-fixture\n\
             collections: []\n\
             graphqlPath: /graphql\n\
             defaultEnvironment: local\n\
             environments:\n\
             \x20 local:\n\
             \x20   baseUrl: {server}\n"
        ),
    )
    .expect("write the manifest");
    root
}

/// Mount a 200 response carrying `body` on `POST /graphql`.
async fn mount_schema(server: &MockServer, body: &Value) {
    Mock::given(method("POST"))
        .and(path("/graphql"))
        .respond_with(ResponseTemplate::new(200).set_body_json(body.clone()))
        .mount(server)
        .await;
}

fn args() -> SchemaArgs {
    SchemaArgs {
        env: None,
        user: None,
        out: None,
        legacy: false,
        force: false,
    }
}

#[test]
fn prints_the_introspection_result_as_json_to_stdout() {
    let runtime = runtime();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(mount_schema(&server, &introspection_result()));
    let root = config_root(&server.uri());

    let (exit, output) = run_in(args(), root.path());

    assert_eq!(exit, Exit::Ok, "a healthy introspection succeeds: {output}");
    assert!(output.ends_with('\n'), "the output ends with a newline");
    let printed: Value = serde_json::from_str(&output).expect("stdout is JSON");
    assert_eq!(
        printed["__schema"]["queryType"]["name"], "Query",
        "the result carries the schema, not the GraphQL envelope: {printed}"
    );
    assert!(
        printed.get("data").is_none(),
        "the `data` wrapper is unwrapped: {printed}"
    );
}

#[test]
fn out_writes_the_file_and_nothing_else() {
    let runtime = runtime();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(mount_schema(&server, &introspection_result()));
    let root = config_root(&server.uri());
    let out = root.path().join("schema.json");

    let (exit, output) = run_in(
        SchemaArgs {
            out: Some(out.clone()),
            ..args()
        },
        root.path(),
    );

    assert_eq!(exit, Exit::Ok);
    assert_eq!(output, "", "`--out` prints nothing on stdout");
    let written = fs::read_to_string(&out).expect("the file was written");
    let parsed: Value = serde_json::from_str(&written).expect("the file is JSON");
    assert_eq!(parsed["__schema"]["queryType"]["name"], "Query");
    assert!(written.ends_with('\n'), "the file ends with a newline");
}

#[test]
fn legacy_sends_the_older_query() {
    let runtime = runtime();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(mount_schema(&server, &introspection_result()));
    let root = config_root(&server.uri());

    let (modern, _) = run_in(args(), root.path());
    assert_eq!(modern, Exit::Ok);
    let (old, _) = run_in(
        SchemaArgs {
            legacy: true,
            ..args()
        },
        root.path(),
    );
    assert_eq!(old, Exit::Ok);

    let requests = runtime
        .block_on(server.received_requests())
        .expect("the mock records requests");
    assert_eq!(requests.len(), 2, "one request per run");
    let body = |request: &Request| String::from_utf8_lossy(&request.body).into_owned();
    let modern_body = body(&requests[0]);
    let legacy_body = body(&requests[1]);

    // The modern query asks for the newer fields; the legacy one cannot, which
    // is the whole point of the flag (§7.17).
    assert!(
        modern_body.contains("specifiedByURL") && modern_body.contains("isRepeatable"),
        "the modern query asks for the newer fields: {modern_body}"
    );
    assert!(
        !legacy_body.contains("specifiedByURL"),
        "the legacy query omits specifiedByURL: {legacy_body}"
    );
    assert!(
        !legacy_body.contains("isRepeatable"),
        "the legacy query omits isRepeatable: {legacy_body}"
    );
    assert!(
        legacy_body.contains("__schema"),
        "the legacy query is still an introspection: {legacy_body}"
    );
}

#[test]
fn a_disabled_introspection_is_a_clear_failure_not_a_silent_empty_file() {
    let runtime = runtime();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(async {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "errors": [{ "message": "introspection is disabled on this server" }]
            })))
            .mount(&server)
            .await;
    });
    let root = config_root(&server.uri());
    let out = root.path().join("schema.json");

    let (exit, output) = run_in(
        SchemaArgs {
            out: Some(out.clone()),
            ..args()
        },
        root.path(),
    );

    assert_ne!(exit, Exit::Ok, "a refused introspection is a failure");
    assert!(
        !out.exists(),
        "a failure never leaves an empty or partial file behind"
    );
    assert!(
        output.contains(&server.uri()) || output.contains("/graphql"),
        "the message names the endpoint: {output}"
    );
    assert!(
        output.contains("introspection"),
        "the server's reason is reported: {output}"
    );
}

#[test]
fn keys_are_sorted_so_committed_dumps_diff_cleanly() {
    let runtime = runtime();
    let server = runtime.block_on(MockServer::start());
    // Deliberately not in alphabetical order: `types` first, `queryType` last.
    runtime.block_on(async {
        Mock::given(method("POST"))
            .and(path("/graphql"))
            .respond_with(ResponseTemplate::new(200).set_body_raw(
                r#"{"data":{"__schema":{"types":[],"queryType":{"name":"Query"},"mutationType":null}}}"#,
                "application/json",
            ))
            .mount(&server)
            .await;
    });
    let root = config_root(&server.uri());

    let (first_exit, first) = run_in(args(), root.path());
    let (second_exit, second) = run_in(args(), root.path());
    assert_eq!(first_exit, Exit::Ok);
    assert_eq!(second_exit, Exit::Ok);

    assert_eq!(first, second, "two runs are byte-identical");
    let mutation = first.find("\"mutationType\"").expect("a mutationType key");
    let query = first.find("\"queryType\"").expect("a queryType key");
    let types = first.find("\"types\"").expect("a types key");
    assert!(
        mutation < query && query < types,
        "keys are sorted, not in response order:\n{first}"
    );
}

#[test]
fn out_refuses_to_overwrite_without_force() {
    let runtime = runtime();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(mount_schema(&server, &introspection_result()));
    let root = config_root(&server.uri());
    let out = root.path().join("schema.json");
    fs::write(&out, "sentinel\n").expect("pre-create the dump");

    let (exit, output) = run_in(
        SchemaArgs {
            out: Some(out.clone()),
            ..args()
        },
        root.path(),
    );

    assert_eq!(
        exit,
        Exit::Usage,
        "an existing dump without `--force` is usage"
    );
    assert!(
        output.contains("--force"),
        "the message names the flag that overwrites: {output}"
    );
    assert_eq!(
        fs::read_to_string(&out).expect("the file is still there"),
        "sentinel\n",
        "the existing file is untouched"
    );
    assert_eq!(
        runtime
            .block_on(server.received_requests())
            .expect("the mock records requests")
            .len(),
        0,
        "the refusal happens before any request"
    );

    // With `--force` the same invocation overwrites.
    let (forced, _) = run_in(
        SchemaArgs {
            out: Some(out.clone()),
            force: true,
            ..args()
        },
        root.path(),
    );
    assert_eq!(forced, Exit::Ok);
    let parsed: Value =
        serde_json::from_str(&fs::read_to_string(&out).expect("the file was rewritten"))
            .expect("the file is JSON");
    assert_eq!(parsed["__schema"]["queryType"]["name"], "Query");
}

/// `--out` may point outside the config tree; the parent is created.
#[test]
fn out_creates_missing_parent_directories() {
    let runtime = runtime();
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(mount_schema(&server, &introspection_result()));
    let root = config_root(&server.uri());
    let out: PathBuf = root.path().join("dumps/nested/schema.json");

    let (exit, _) = run_in(
        SchemaArgs {
            out: Some(out.clone()),
            ..args()
        },
        root.path(),
    );

    assert_eq!(exit, Exit::Ok);
    assert!(Path::new(&out).is_file(), "the dump exists");
}
