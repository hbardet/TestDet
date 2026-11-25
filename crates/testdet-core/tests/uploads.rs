//! Integration tests for files, uploads, path safety and MIME sniffing
//! (Task 19, spec §7.14).
//!
//! The GraphQL and REST tests drive the real `execute` entry point against a
//! wiremock server and parse the multipart body that actually reached the
//! wire, so the parts asserted here are the parts a backend would see.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde_json::{Value, json};
use tempfile::TempDir;
use wiremock::matchers::{method, path as path_matcher};
use wiremock::{Mock, MockServer, Request, ResponseTemplate};

use testdet_core::RunContext;
use testdet_core::config::Document;
use testdet_core::config::discover::Config;
use testdet_core::config::model::{HeaderMap, Manifest};
use testdet_core::config::parse::parse_operation;
use testdet_core::config::resolve::{Resolved, resolve};
use testdet_core::diagnostic::codes;
use testdet_core::engine::execute;
use testdet_core::files::{FileCache, check_path, sniff_mime};
use testdet_core::operation::model::{
    Body, FileEncoding, FileSpec, GraphqlRequest, HttpRequest, Operation,
    Request as OperationRequest,
};
use testdet_core::redact::Redactor;
use testdet_core::session::SessionStore;

/// The committed fixtures directory, used as a config root so that
/// `fixtures/avatar.png` resolves to the committed PNG.
fn fixtures_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures")
}

/// The committed 1×1 PNG.
fn avatar_bytes() -> Vec<u8> {
    std::fs::read(fixtures_root().join("fixtures/avatar.png"))
        .expect("the PNG fixture is committed")
}

/// A resolved environment pointing at `base_url` with `root` as the config
/// directory (the base every relative file path resolves against).
fn resolved_for(base_url: &str, root: PathBuf) -> Arc<Resolved> {
    let manifest: Manifest = serde_json::from_value(json!({
        "version": 1,
        "collections": [],
        "graphqlPath": "/graphql",
        "environments": { "test": { "baseUrl": base_url } },
        "defaultEnvironment": "test"
    }))
    .expect("fixture manifest is valid");

    let config = Config {
        manifest,
        root_dir: root,
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

fn file_spec(path: &str, encoding: FileEncoding) -> FileSpec {
    FileSpec {
        path: path.to_string(),
        mime_type: None,
        filename: None,
        encoding,
    }
}

/// A GraphQL mutation taking one `Upload!` variable wired to `{{file.avatar}}`.
fn graphql_upload(encoding: FileEncoding) -> Operation {
    Operation {
        id: "uploadAvatar".to_string(),
        description: None,
        tags: Vec::new(),
        request: OperationRequest::Graphql(GraphqlRequest {
            path: None,
            query: "mutation ($file: Upload!) { upload(file: $file) { id } }".to_string(),
            operation_name: None,
            variables: BTreeMap::from([("file".to_string(), json!("{{file.avatar}}"))]),
            headers: HeaderMap::default(),
        }),
        vars: BTreeMap::new(),
        files: BTreeMap::from([(
            "avatar".to_string(),
            file_spec("fixtures/avatar.png", encoding),
        )]),
        capture: BTreeMap::new(),
    }
}

/// A REST multipart upload with one text field and the same file.
fn rest_upload(headers: HeaderMap) -> Operation {
    Operation {
        id: "uploadRest".to_string(),
        description: None,
        tags: Vec::new(),
        request: OperationRequest::Http(HttpRequest {
            method: "POST".to_string(),
            path: "/upload".to_string(),
            body: Some(Body::Multipart(BTreeMap::from([(
                "a".to_string(),
                "x".to_string(),
            )]))),
            headers,
        }),
        vars: BTreeMap::new(),
        files: BTreeMap::from([(
            "avatar".to_string(),
            file_spec("fixtures/avatar.png", FileEncoding::Multipart),
        )]),
        capture: BTreeMap::new(),
    }
}

fn store(resolved: Arc<Resolved>) -> SessionStore {
    SessionStore::new(RunContext::new(resolved, Arc::new(Redactor::new())))
}

async fn serve(server: &MockServer, endpoint: &str) {
    Mock::given(method("POST"))
        .and(path_matcher(endpoint.to_string()))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": { "ok": true } })))
        .mount(server)
        .await;
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

/// One parsed multipart part.
#[derive(Debug)]
struct Part {
    name: String,
    filename: Option<String>,
    content_type: Option<String>,
    body: Vec<u8>,
}

/// The boundary parameter of a `multipart/form-data` content type.
fn boundary(content_type: &str) -> String {
    content_type
        .split("boundary=")
        .nth(1)
        .expect("a multipart content type carries a boundary")
        .trim()
        .trim_matches('"')
        .to_string()
}

/// Find `needle` in `haystack` at or after `from`.
fn find(haystack: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    haystack
        .get(from..)?
        .windows(needle.len())
        .position(|window| window == needle)
        .map(|offset| from + offset)
}

/// Parse a multipart body into its parts, preserving binary payloads.
fn parse_multipart(content_type: &str, body: &[u8]) -> Vec<Part> {
    let delimiter = format!("--{}", boundary(content_type));
    let delimiter = delimiter.as_bytes();

    let mut parts = Vec::new();
    let mut cursor = 0;
    while let Some(start) = find(body, delimiter, cursor) {
        let after = start + delimiter.len();
        // `--` closes the body; anything else starts a part after CRLF.
        if body.get(after..after + 2) == Some(b"--") {
            break;
        }
        let Some(headers_end) = find(body, b"\r\n\r\n", after) else {
            break;
        };
        let Some(next) = find(body, delimiter, headers_end) else {
            break;
        };

        let headers = String::from_utf8_lossy(&body[after..headers_end]).into_owned();
        let mut content = body[headers_end + 4..next].to_vec();
        if content.ends_with(b"\r\n") {
            content.truncate(content.len() - 2);
        }

        parts.push(Part {
            name: quoted(&headers, "name").unwrap_or_default(),
            filename: quoted(&headers, "filename"),
            content_type: header_value(&headers, "content-type"),
            body: content,
        });
        cursor = next;
    }
    parts
}

/// The value of a `key="value"` parameter in a part's headers.
fn quoted(headers: &str, key: &str) -> Option<String> {
    let start = headers.find(&format!("{key}=\""))? + key.len() + 2;
    let rest = &headers[start..];
    let end = rest.find('"')?;
    Some(rest[..end].to_string())
}

/// The value of a plain `Name: value` header line.
fn header_value(headers: &str, name: &str) -> Option<String> {
    headers.lines().find_map(|line| {
        let (key, value) = line.split_once(':')?;
        key.trim()
            .eq_ignore_ascii_case(name)
            .then(|| value.trim().to_string())
    })
}

fn part<'a>(parts: &'a [Part], name: &str) -> &'a Part {
    parts
        .iter()
        .find(|part| part.name == name)
        .unwrap_or_else(|| panic!("no part named `{name}` in {parts:?}"))
}

#[tokio::test]
async fn a_graphql_file_variable_becomes_the_three_part_spec() {
    let server = MockServer::start().await;
    serve(&server, "/graphql").await;

    let mut store = store(resolved_for(&server.uri(), fixtures_root()));
    let outcome = execute(&mut store, &graphql_upload(FileEncoding::Multipart)).await;
    assert!(
        outcome.transport_error.is_none(),
        "the upload reached the server: {:?}",
        outcome.transport_error
    );

    let requests = requests_to(&server, "/graphql").await;
    assert_eq!(requests.len(), 1, "one POST carries the whole upload");
    let request = &requests[0];
    let content_type = request
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .expect("a multipart content type");
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "got: {content_type}"
    );

    let parts = parse_multipart(content_type, &request.body);
    assert_eq!(
        parts
            .iter()
            .map(|part| part.name.as_str())
            .collect::<Vec<_>>(),
        vec!["operations", "map", "0"],
        "the spec's part order"
    );

    let operations: Value =
        serde_json::from_slice(&part(&parts, "operations").body).expect("operations is JSON");
    assert_eq!(
        operations["variables"]["file"],
        Value::Null,
        "the file variable is nulled in operations"
    );
    assert!(
        operations["query"]
            .as_str()
            .expect("the query is a string")
            .contains("Upload!"),
        "the query survives"
    );

    let map: Value = serde_json::from_slice(&part(&parts, "map").body).expect("map is JSON");
    assert_eq!(map, json!({ "0": ["variables.file"] }), "the map, exactly");

    let file = part(&parts, "0");
    assert_eq!(file.filename.as_deref(), Some("avatar.png"));
    assert_eq!(file.content_type.as_deref(), Some("image/png"));
    assert_eq!(file.body, avatar_bytes(), "part 0 carries the bytes");
}

#[tokio::test]
async fn base64_encoding_inlines_the_file_instead() {
    let server = MockServer::start().await;
    serve(&server, "/graphql").await;

    let mut store = store(resolved_for(&server.uri(), fixtures_root()));
    let outcome = execute(&mut store, &graphql_upload(FileEncoding::Base64)).await;
    assert!(outcome.transport_error.is_none());

    let requests = requests_to(&server, "/graphql").await;
    let request = &requests[0];
    let content_type = request
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .expect("a content type");
    assert_eq!(content_type, "application/json", "no multipart is built");

    let body: Value = serde_json::from_slice(&request.body).expect("the body is JSON");
    let inlined = body["variables"]["file"]
        .as_str()
        .expect("the file is a JSON string");
    assert_eq!(inlined, STANDARD.encode(avatar_bytes()));
    assert_eq!(
        STANDARD.decode(inlined).expect("valid base64"),
        avatar_bytes()
    );
}

#[tokio::test]
async fn rest_multipart_maps_files_by_field_name() {
    let server = MockServer::start().await;
    serve(&server, "/upload").await;

    let mut store = store(resolved_for(&server.uri(), fixtures_root()));
    let outcome = execute(&mut store, &rest_upload(HeaderMap::default())).await;
    assert!(outcome.transport_error.is_none());

    let requests = requests_to(&server, "/upload").await;
    let request = &requests[0];
    let content_type = request
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .expect("a multipart content type");
    assert!(content_type.starts_with("multipart/form-data; boundary="));

    let parts = parse_multipart(content_type, &request.body);
    let field = part(&parts, "a");
    assert_eq!(field.body, b"x", "the form field is a text part");
    let file = part(&parts, "avatar");
    assert_eq!(file.body, avatar_bytes(), "the file is keyed by field name");
    assert_eq!(file.filename.as_deref(), Some("avatar.png"));
    assert_eq!(file.content_type.as_deref(), Some("image/png"));
}

#[tokio::test]
async fn a_hand_set_content_type_on_a_multipart_request_is_forbidden() {
    // Validation rejects it at load time (Task 7, `E_FORBIDDEN_HEADER`)…
    let invalid = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests/fixtures/invalid/operation-multipart-content-type.yaml");
    let document = Document::load(&invalid).expect("the invalid fixture is loadable YAML");
    let diagnostics = parse_operation(&document).expect_err("a hand-set Content-Type is rejected");
    assert!(
        diagnostics
            .iter()
            .any(|diagnostic| diagnostic.code == codes::E_FORBIDDEN_HEADER),
        "got: {:?}",
        diagnostics.iter().map(|d| d.code).collect::<Vec<_>>()
    );

    // …and `prepare` never copies one either: the boundary is reqwest's.
    let server = MockServer::start().await;
    serve(&server, "/upload").await;
    let hand_set = HeaderMap(BTreeMap::from([(
        "Content-Type".to_string(),
        Some("application/json".to_string()),
    )]));

    let mut store = store(resolved_for(&server.uri(), fixtures_root()));
    execute(&mut store, &rest_upload(hand_set)).await;

    let requests = requests_to(&server, "/upload").await;
    let content_type = requests[0]
        .headers
        .get("content-type")
        .and_then(|value| value.to_str().ok())
        .expect("a content type");
    assert!(
        content_type.starts_with("multipart/form-data; boundary="),
        "the hand-set header is dropped, got: {content_type}"
    );
}

#[test]
fn dot_dot_escaping_the_config_directory_is_refused() {
    let root = fixtures_root();
    let spec = file_spec("../../etc/passwd", FileEncoding::Multipart);
    let diagnostic = check_path(&root, &spec).expect_err("`..` is refused");

    assert_eq!(diagnostic.code, codes::E_PATH_ESCAPE);
    assert_eq!(
        diagnostic.hint.as_deref(),
        Some("a file path must stay inside the config directory")
    );
    assert!(
        diagnostic.message.contains("../../etc/passwd"),
        "got: {}",
        diagnostic.message
    );
}

#[test]
fn an_absolute_path_is_refused() {
    let root = fixtures_root();
    let spec = file_spec("/etc/passwd", FileEncoding::Multipart);
    let diagnostic = check_path(&root, &spec).expect_err("an absolute path is refused");

    assert_eq!(diagnostic.code, codes::E_PATH_ESCAPE);
    assert_eq!(
        diagnostic.hint.as_deref(),
        Some("a file path must stay inside the config directory")
    );
}

#[test]
fn a_missing_fixture_names_the_resolved_path() {
    let root = fixtures_root();
    let spec = file_spec("fixtures/nope.png", FileEncoding::Multipart);
    let mut cache = FileCache::new(&root);
    let diagnostic = cache.load(&spec).expect_err("the fixture does not exist");

    let resolved = root.join("fixtures/nope.png");
    assert_eq!(diagnostic.code, codes::E_UNKNOWN_FILE);
    assert!(
        diagnostic.message.contains(&resolved.display().to_string()),
        "the message names the resolved path {}, got: {}",
        resolved.display(),
        diagnostic.message
    );
    assert_eq!(cache.reads(), 0, "a missing file is never read");
}

#[test]
fn the_size_ceiling_is_enforced_at_five_megabytes() {
    let dir = TempDir::new().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("fixtures")).expect("the fixtures dir");
    let big = dir.path().join("fixtures/big.bin");
    std::fs::write(&big, vec![0x41u8; 6 * 1024 * 1024]).expect("a 6 MB fixture");

    let mut cache = FileCache::new(dir.path());
    let spec = file_spec("fixtures/big.bin", FileEncoding::Multipart);
    let diagnostic = cache.load(&spec).expect_err("6 MB exceeds the ceiling");

    assert_eq!(diagnostic.code, codes::E_FILE_TOO_LARGE);
    assert!(
        diagnostic.message.contains("5242880"),
        "the message names the limit, got: {}",
        diagnostic.message
    );
    assert!(
        diagnostic.message.contains("big.bin"),
        "the message names the file, got: {}",
        diagnostic.message
    );

    // Exactly 5 MB is still allowed: the ceiling is a limit, not a margin.
    let exact = dir.path().join("fixtures/exact.bin");
    std::fs::write(&exact, vec![0x42u8; 5 * 1024 * 1024]).expect("a 5 MB fixture");
    let spec = file_spec("fixtures/exact.bin", FileEncoding::Multipart);
    let loaded = cache.load(&spec).expect("5 MB is under the ceiling");
    assert_eq!(loaded.bytes.len(), 5 * 1024 * 1024);
}

#[test]
fn mime_is_sniffed_when_not_declared_and_declared_wins_when_present() {
    let dir = TempDir::new().expect("a temp dir");
    std::fs::create_dir_all(dir.path().join("fixtures")).expect("the fixtures dir");
    std::fs::write(dir.path().join("fixtures/a.png"), avatar_bytes()).expect("png");
    std::fs::write(dir.path().join("fixtures/a.json"), br#"{"a":1}"#).expect("json");
    std::fs::write(dir.path().join("fixtures/a.txt"), b"hello\nworld").expect("text");
    std::fs::write(dir.path().join("fixtures/a.bin"), [0u8, 1, 2, 0]).expect("binary");

    let mut cache = FileCache::new(dir.path());
    let mime = |cache: &mut FileCache, path: &str| {
        cache
            .load(&file_spec(path, FileEncoding::Multipart))
            .expect("the fixture loads")
            .mime
    };

    assert_eq!(mime(&mut cache, "fixtures/a.png"), "image/png");
    assert_eq!(mime(&mut cache, "fixtures/a.json"), "application/json");
    assert_eq!(mime(&mut cache, "fixtures/a.txt"), "text/plain");
    assert_eq!(
        mime(&mut cache, "fixtures/a.bin"),
        "application/octet-stream"
    );

    let declared = FileSpec {
        mime_type: Some("application/x-custom".to_string()),
        ..file_spec("fixtures/a.png", FileEncoding::Multipart)
    };
    assert_eq!(
        cache.load(&declared).expect("the fixture loads").mime,
        "application/x-custom",
        "a declared MIME type wins verbatim"
    );

    assert_eq!(sniff_mime(b"%PDF-1.7\n", None), "application/pdf");
    assert_eq!(sniff_mime(b"PK\x03\x04rest", None), "application/zip");
    assert_eq!(sniff_mime(&[0xFF, 0xD8, 0xFF, 0xE0], None), "image/jpeg");
    assert_eq!(sniff_mime(b"GIF89a\x01\x00", None), "image/gif");
    assert_eq!(
        sniff_mime(b"anything", Some("image/webp")),
        "image/webp",
        "declared wins even for unknown bytes"
    );
}

#[tokio::test]
async fn a_file_is_read_once_per_run_not_once_per_request() {
    let server = MockServer::start().await;
    serve(&server, "/graphql").await;

    let resolved = resolved_for(&server.uri(), fixtures_root());
    let redactor = Arc::new(Redactor::new());
    let ctx = RunContext::new(resolved.clone(), Arc::clone(&redactor));
    let probe = ctx.clone();
    let mut store = SessionStore::new(ctx);

    let operation = graphql_upload(FileEncoding::Multipart);
    execute(&mut store, &operation).await;
    execute(&mut store, &operation).await;
    assert_eq!(
        requests_to(&server, "/graphql").await.len(),
        2,
        "two requests ran"
    );
    assert_eq!(
        probe.files.lock().expect("the cache lock").reads(),
        1,
        "one read serves every request in the run"
    );

    // A second run is a second context, so it reads again.
    let second = RunContext::new(resolved, redactor);
    let probe = second.clone();
    let mut store = SessionStore::new(second);
    execute(&mut store, &operation).await;
    assert_eq!(
        probe.files.lock().expect("the cache lock").reads(),
        1,
        "each run reads the file once"
    );
}
