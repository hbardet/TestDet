//! The executor driver's environment switch (C2, §7.16).
//!
//! The switch itself needs no server: what is asserted here is that the driver
//! re-loads the config for the named environment, resolves it, and reports an
//! unknown name as diagnostics text instead of panicking.

use std::fs;
use std::sync::Arc;

use tempfile::TempDir;
use testdet_cli::tui_driver::ExecutorDriver;
use testdet_core::config::{LoadOptions, load_config, resolve};
use testdet_core::redact::redactor_for;
use testdet_tui::Driver;

fn fixture() -> (TempDir, LoadOptions) {
    let root = TempDir::new().expect("a temp directory");
    let manifest = root.path().join("testdet/testdet.yaml");
    fs::create_dir_all(manifest.parent().expect("has a parent")).expect("create the fixture dir");
    fs::write(
        &manifest,
        "version: 1\n\
         name: demo\n\
         defaultEnvironment: staging\n\
         collections: []\n\
         environments:\n\
         \x20 staging:\n\
         \x20   baseUrl: https://staging.example.com\n\
         \x20 production:\n\
         \x20   baseUrl: https://api.example.com\n",
    )
    .expect("write the manifest");
    let loader = LoadOptions {
        config: Some(manifest),
        cwd: root.path().to_path_buf(),
        env: None,
    };
    (root, loader)
}

fn driver(loader: &LoadOptions) -> ExecutorDriver {
    let config = load_config(loader).expect("the fixture config loads");
    let resolved = resolve(&config, None, &[]).expect("the fixture resolves");
    let redactor = Arc::new(redactor_for(&config, &resolved));
    ExecutorDriver::spawn(loader.clone(), Arc::new(resolved), redactor, None)
        .expect("the worker starts")
}

#[test]
fn switching_environment_re_resolves_the_config() {
    let (_root, loader) = fixture();
    let mut driver = driver(&loader);

    let production = driver
        .switch_environment("production")
        .expect("a declared environment resolves");
    assert_eq!(production.env_name, "production");
    assert_eq!(production.base_url.host_str(), Some("api.example.com"));

    // Back again: the loader kept the original options, so nothing is sticky.
    let staging = driver
        .switch_environment("staging")
        .expect("the default environment resolves too");
    assert_eq!(staging.base_url.host_str(), Some("staging.example.com"));

    let error = driver
        .switch_environment("nowhere")
        .expect_err("an undeclared environment is an error");
    assert!(
        error.contains("unknown environment") && error.contains("nowhere"),
        "the diagnostics name the problem: {error}"
    );
}

#[test]
fn cancelling_an_idle_driver_is_a_no_op() {
    let (_root, loader) = fixture();
    let mut driver = driver(&loader);

    // `Esc` when nothing runs must not poison the next request: the token the
    // driver replaces on switch is fresh, and the worker keeps running.
    driver.cancel();
    assert!(driver.events().is_empty());
    let resolved = driver
        .switch_environment("staging")
        .expect("the driver still works after an idle cancel");
    assert_eq!(resolved.env_name, "staging");
}

// --- C3: values stored in the UI must reach the wire (§7.16, §7.19) ---

use std::path::Path;
use std::time::{Duration, Instant};

use serde_json::json;
use testdet_core::Event;
use testdet_tui::{Action, App, dispatch};
use wiremock::matchers::{method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

/// A temp config whose one operation sends a stored variable in a header.
///
/// `{{var.token}}` only resolves if the store's overrides are the top variable
/// layer; without them the engine refuses to build the request, so a request
/// arriving at all proves the layering.
fn wire_fixture(server: &str) -> (TempDir, LoadOptions) {
    let root = TempDir::new().expect("a temp directory");
    let write = |relative: &str, content: &str| {
        let file = root.path().join(relative);
        fs::create_dir_all(file.parent().expect("has a parent")).expect("create the fixture dir");
        fs::write(&file, content).expect("write the fixture file");
    };
    write(
        "testdet/testdet.yaml",
        &format!(
            "version: 1\n\
             name: stored-vars\n\
             defaultEnvironment: local\n\
             collections: [ops]\n\
             environments:\n\
             \x20 local:\n\
             \x20   baseUrl: {server}\n"
        ),
    );
    write(
        "testdet/ops/whoami.yaml",
        "id: whoami\n\
         request:\n\
         \x20 kind: http\n\
         \x20 method: GET\n\
         \x20 path: /whoami\n\
         \x20 headers:\n\
         \x20   X-Token: \"{{var.token}}\"\n",
    );

    let manifest = root.path().join("testdet/testdet.yaml");
    let loader = LoadOptions {
        config: Some(manifest),
        cwd: root.path().to_path_buf(),
        env: None,
    };
    (root, loader)
}

/// Drive one run through the real driver until it finishes.
fn run_until_finished(app: &mut App, driver: &mut ExecutorDriver, operation: &str) {
    assert!(
        dispatch(app, Action::Run(operation.to_string()), driver).expect("dispatch succeeds"),
        "the UI keeps running"
    );

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let events = driver.events();
        let finished = events
            .iter()
            .any(|event| matches!(event, Event::Finished { .. }));
        for event in events {
            app.apply_event(event);
        }
        if finished {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the run did not finish within 10s"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Every file under `root`, so the test can prove nothing was written.
fn files_under(root: &Path) -> Vec<(std::path::PathBuf, String)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read the fixture directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let content = fs::read(&path)
                    .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                    .unwrap_or_default();
                files.push((path, content));
            }
        }
    }
    files
}

#[test]
fn a_stored_variable_reaches_the_wire_above_operation_vars() {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a test runtime");
    let server = runtime.block_on(MockServer::start());
    runtime.block_on(
        Mock::given(method("GET"))
            .and(path("/whoami"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "ok": true })))
            .mount(&server),
    );

    let (root, loader) = wire_fixture(&server.uri());
    let config = load_config(&loader).expect("the fixture config loads");
    let resolved = resolve(&config, None, &[]).expect("the fixture resolves");
    let redactor = Arc::new(redactor_for(&config, &resolved));

    let mut app = App::new(config, resolved.clone());
    // What `s` stores, without needing a response first.
    app.store_var("token", json!("S3CRET"), "test");

    let mut driver = ExecutorDriver::spawn(loader, Arc::new(resolved), redactor, None)
        .expect("the worker starts");
    run_until_finished(&mut app, &mut driver, "whoami");

    let requests = runtime
        .block_on(server.received_requests())
        .expect("the mock records requests");
    assert_eq!(requests.len(), 1, "exactly one request reached the server");
    let header = requests[0]
        .headers
        .get("x-token")
        .expect("the request carries X-Token");
    assert_eq!(
        header.to_str().expect("an ASCII header value"),
        "S3CRET",
        "the stored value wins and lands on the wire"
    );

    // Session-scoped and in memory only: nothing under the config root has it.
    for (file, content) in files_under(root.path()) {
        assert!(
            !content.contains("S3CRET"),
            "the stored value is never written to disk, but {} has it",
            file.display()
        );
    }
}
