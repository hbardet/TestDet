//! Response-viewer tests: the body, the tabs, stored variables and cancelling.
//!
//! Every test builds a real `Config` from a throwaway directory, resolves it,
//! and asserts on the flattened `TestBackend` text — substrings, never exact
//! frames. Engine events are synthesised as `Outcome`s, because the TUI must
//! render one whatever produced it.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use testdet_core::config::{LoadOptions, Resolved, load_config, resolve};
use testdet_core::outcome::{
    CookieInfo, GraphQLError, Outcome, RequestEcho, Timing, TransportError, TransportKind,
};
use testdet_core::redact::Redactor;
use testdet_core::serde_json::{Value, json};
use testdet_core::{Bytes, EnvelopeShape, Event};
use testdet_tui::{Action, App, Driver, dispatch};

/// A throwaway config directory, removed when the test ends.
struct Sandbox {
    root: PathBuf,
}

impl Sandbox {
    fn new(tag: &str) -> Self {
        let unique = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| elapsed.as_nanos())
            .unwrap_or_default();
        let root =
            std::env::temp_dir().join(format!("testdet-tui-{tag}-{}-{unique}", std::process::id()));
        let sandbox = Self { root };
        sandbox.write(
            "testdet/testdet.yaml",
            "version: 1\n\
             name: demo\n\
             defaultEnvironment: staging\n\
             collections: [users, products]\n\
             environments:\n\
             \x20 staging:\n\
             \x20   baseUrl: https://staging.example.com\n\
             \x20 production:\n\
             \x20   baseUrl: https://api.example.com\n",
        );
        sandbox.write(
            "testdet/users/get-user.yaml",
            "id: getUser\n\
             description: Fetch a user\n\
             tags: [read]\n\
             request:\n\
             \x20 kind: graphql\n\
             \x20 query: |\n\
             \x20   query GetUser { user { id } }\n\
             vars:\n\
             \x20 token: default\n",
        );
        sandbox.write(
            "testdet/products/list-products.yaml",
            "id: listProducts\n\
             description: List products\n\
             tags: [read, catalog]\n\
             request:\n\
             \x20 kind: http\n\
             \x20 method: GET\n\
             \x20 path: /products\n",
        );
        sandbox
    }

    fn write(&self, rel: &str, content: &str) {
        let path = self.root.join(rel);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).expect("create the fixture directory");
        }
        fs::write(path, content).expect("write the fixture file");
    }

    fn manifest(&self) -> PathBuf {
        self.root.join("testdet/testdet.yaml")
    }
}

impl Drop for Sandbox {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.root);
    }
}

struct Fixture {
    sandbox: Sandbox,
    app: App,
    /// `getUser`'s own variable, before anything is stored.
    operation_token: Value,
    /// The same config resolved for the other environment, ready to be what a
    /// driver would hand back for an `e` press.
    production: Resolved,
}

fn fixture(tag: &str) -> Fixture {
    let sandbox = Sandbox::new(tag);
    let config = load_config(&LoadOptions {
        config: Some(sandbox.manifest()),
        cwd: sandbox.root.clone(),
        env: None,
    })
    .expect("the fixture config loads");
    let resolved = resolve(&config, None, &[]).expect("the fixture resolves");
    let operation_token = resolved
        .vars_for(&config.operations["getUser"], None)
        .remove("token")
        .expect("the operation declares `token`");
    let production = resolve(&config, Some("production"), &[]).expect("production resolves");
    Fixture {
        sandbox,
        app: App::new(config, resolved),
        operation_token,
        production,
    }
}

/// Flatten the rendered frame into one string, row by row.
fn render(app: &App) -> String {
    let backend = TestBackend::new(100, 40);
    let mut terminal = Terminal::new(backend).expect("a test terminal");
    terminal.draw(|frame| app.draw(frame)).expect("draw");
    let buffer = terminal.backend().buffer().clone();
    let mut text = String::new();
    for y in 0..buffer.area.height {
        for x in 0..buffer.area.width {
            text.push_str(buffer[(x, y)].symbol());
        }
        text.push('\n');
    }
    text
}

fn press(app: &mut App, code: KeyCode) -> Action {
    app.handle_key(KeyEvent::from(code))
}

fn type_text(app: &mut App, text: &str) {
    for character in text.chars() {
        press(app, KeyCode::Char(character));
    }
}

/// A finished outcome with a JSON content type and a 132 ms / 1.2 kB status.
fn outcome(operation: &str, status: u16, body: Option<&[u8]>) -> Outcome {
    let body = body.map(Bytes::copy_from_slice);
    let body_size = body.as_ref().map_or(0, |body| body.len() as u64);
    Outcome {
        operation: operation.to_string(),
        user: None,
        request: RequestEcho {
            method: "POST".to_string(),
            url: "https://staging.example.com/graphql".to_string(),
            headers: Vec::new(),
            body_size: 0,
        },
        status,
        headers: vec![("content-type".to_string(), "application/json".to_string())],
        cookies: Vec::new(),
        body,
        body_retained: true,
        body_size,
        graphql_errors: Vec::new(),
        timing: Timing {
            total: Duration::from_millis(132),
            connect: None,
            ttfb: None,
        },
        transport_error: None,
        shape: EnvelopeShape::Ok,
    }
}

fn apply(app: &mut App, outcome: Outcome) {
    app.apply_event(Event::Finished { outcome });
}

fn select(app: &mut App, operation: &str) {
    while app.selected_operation() != Some(operation) {
        press(app, KeyCode::Char('j'));
    }
}

#[test]
fn graphql_errors_are_pinned_above_the_body_even_on_200() {
    let mut fixture = fixture("graphql-errors");
    let mut finished = outcome("getUser", 200, Some(br#"{"data":{"user":{"name":"Ada"}}}"#));
    finished.body_size = 1234;
    finished.graphql_errors = vec![GraphQLError {
        message: "field exploded".to_string(),
        path: Some(vec!["user".to_string(), "name".to_string()]),
        extensions: None,
    }];
    apply(&mut fixture.app, finished);

    let text = render(&fixture.app);
    assert!(
        text.contains("200 OK · 132 ms · 1.2 kB"),
        "the status line reports status, total time and size:\n{text}"
    );
    let error = text.find("field exploded").expect("the error is shown");
    let body = text.find("Ada").expect("the body is shown");
    assert!(
        error < body,
        "the GraphQL error block sits above the body:\n{text}"
    );
    assert!(
        text.contains("at user.name"),
        "the error path is shown:\n{text}"
    );
}

#[test]
fn a_collapsed_body_renders_one_summary_line() {
    let mut fixture = fixture("collapsed");
    let mut finished = outcome("listProducts", 200, Some(b"alpha\nbeta\ngamma"));
    finished.headers = vec![("content-type".to_string(), "text/plain".to_string())];
    apply(&mut fixture.app, finished);

    assert!(
        render(&fixture.app).contains("alpha"),
        "expanded by default"
    );
    press(&mut fixture.app, KeyCode::Char('c'));
    let text = render(&fixture.app);
    assert!(
        !text.contains("alpha") && !text.contains("beta"),
        "a collapsed body draws no lines:\n{text}"
    );
    assert!(
        text.contains("3 lines"),
        "the summary names the line count:\n{text}"
    );

    press(&mut fixture.app, KeyCode::Char('c'));
    assert!(
        render(&fixture.app).contains("alpha"),
        "`c` expands again:\n{}",
        render(&fixture.app)
    );
}

#[test]
fn a_non_utf8_body_renders_as_a_size_not_a_panic() {
    let mut fixture = fixture("non-utf8");
    let mut finished = outcome("listProducts", 200, Some(&[0xff, 0x00, 0xfe]));
    finished.headers = vec![("content-type".to_string(), "text/plain".to_string())];
    apply(&mut fixture.app, finished);

    let text = render(&fixture.app);
    assert!(
        text.contains("binary (3 bytes)"),
        "a non-UTF-8 body is a size, never replacement characters:\n{text}"
    );
    assert!(
        !text.contains('\u{fffd}'),
        "nothing was lossily replaced:\n{text}"
    );
}

#[test]
fn the_body_filter_hides_non_matching_lines() {
    let mut fixture = fixture("body-filter");
    let mut finished = outcome("listProducts", 200, Some(b"alpha\nbeta\ngamma"));
    finished.headers = vec![("content-type".to_string(), "text/plain".to_string())];
    apply(&mut fixture.app, finished);

    press(&mut fixture.app, KeyCode::Char('/'));
    type_text(&mut fixture.app, "beta");

    let text = render(&fixture.app);
    assert!(text.contains("beta"), "the matching line stays:\n{text}");
    assert!(
        !text.contains("alpha") && !text.contains("gamma"),
        "non-matching body lines are hidden:\n{text}"
    );

    press(&mut fixture.app, KeyCode::Esc);
    let text = render(&fixture.app);
    assert!(
        text.contains("alpha") && text.contains("gamma"),
        "Esc clears the filter and brings the body back:\n{text}"
    );
}

#[test]
fn s_on_a_selected_node_stores_it_under_the_namespace_above_operation_vars() {
    let mut fixture = fixture("store");
    select(&mut fixture.app, "getUser");
    assert_eq!(
        fixture.operation_token,
        json!("default"),
        "the operation var is the baseline"
    );

    let mut finished = outcome(
        "getUser",
        200,
        Some(br#"{"data":{"token":"abc","userId":"u_1"}}"#),
    );
    finished.shape = EnvelopeShape::Ok;
    apply(&mut fixture.app, finished);

    assert_eq!(
        press(&mut fixture.app, KeyCode::Char('s')),
        Action::StoreVar
    );

    assert_eq!(
        fixture.app.vars().get("token"),
        Some(&json!("abc")),
        "the response field is stored under its own name"
    );
    assert_eq!(fixture.app.vars().get("userId"), Some(&json!("u_1")));
    assert_eq!(
        fixture.app.vars_for("getUser").get("token"),
        Some(&json!("abc")),
        "the stored value wins over the operation var"
    );
    assert_eq!(
        fixture.operation_token,
        json!("default"),
        "storing never mutates the operation's own vars"
    );
    assert!(
        fixture
            .app
            .status()
            .is_some_and(|status| status.contains("stored 2 variable(s)")),
        "the banner reports what was stored: {:?}",
        fixture.app.status()
    );
}

#[test]
fn stored_vars_are_session_scoped_and_never_written_to_disk() {
    let mut fixture = fixture("no-disk");
    let before = directory_state(&fixture.sandbox.root);
    let secret = "stored-token-do-not-persist";

    let body = format!(r#"{{"data":{{"token":"{secret}"}}}}"#);
    apply(
        &mut fixture.app,
        outcome("getUser", 200, Some(body.as_bytes())),
    );
    press(&mut fixture.app, KeyCode::Char('s'));

    assert_eq!(fixture.app.vars().get("token"), Some(&json!(secret)));
    let after = directory_state(&fixture.sandbox.root);
    assert_eq!(
        before, after,
        "storing a variable must not create or change any file"
    );
    let leaked: Vec<&PathBuf> = after
        .iter()
        .filter(|(_, contents)| String::from_utf8_lossy(contents).contains(secret))
        .map(|(path, _)| path)
        .collect();
    assert!(
        leaked.is_empty(),
        "the stored value never reaches disk: {leaked:?}"
    );
}

#[test]
fn esc_cancels_an_inflight_request_and_renders_cancelled() {
    let mut fixture = fixture("cancel");
    select(&mut fixture.app, "getUser");

    assert_eq!(
        press(&mut fixture.app, KeyCode::Enter),
        Action::Run("getUser".to_string())
    );
    assert!(fixture.app.is_running());
    assert_eq!(press(&mut fixture.app, KeyCode::Esc), Action::Cancel);

    let mut cancelled = outcome("getUser", 0, None);
    cancelled.body_retained = false;
    cancelled.headers.clear();
    cancelled.transport_error = Some(TransportError {
        kind: TransportKind::Cancelled,
        message: "cancelled".to_string(),
    });
    apply(&mut fixture.app, cancelled);

    assert!(!fixture.app.is_running(), "the run is over");
    let text = render(&fixture.app);
    assert!(
        text.contains("transport error (cancelled)"),
        "a cancelled request renders as a distinct error block:\n{text}"
    );
    assert!(
        text.contains("no response · 132 ms · 0 B"),
        "a cancelled request has no status:\n{text}"
    );
}

#[test]
fn the_cookies_tab_shows_flags_and_redacted_values() {
    // Built by hand rather than through `fixture`: the redactor has to be
    // installed before the first frame, and it is the host that owns it.
    let sandbox = Sandbox::new("cookies");
    let config = load_config(&LoadOptions {
        config: Some(sandbox.manifest()),
        cwd: sandbox.root.clone(),
        env: None,
    })
    .expect("the fixture config loads");
    let resolved = resolve(&config, None, &[]).expect("the fixture resolves");
    let redactor = Arc::new(Redactor::new());
    redactor.register("SESSION", "s3cret");
    let mut app = App::new(config, resolved).with_redactor(redactor);

    let mut finished = outcome("getUser", 200, Some(b"{}"));
    finished.cookies = vec![CookieInfo {
        name: "session".to_string(),
        value: "s3cret".to_string(),
        domain: "example.com".to_string(),
        path: "/".to_string(),
        http_only: true,
        secure: true,
        expires: None,
    }];
    apply(&mut app, finished);

    press(&mut app, KeyCode::Tab);
    let headers = render(&app);
    assert!(
        headers.contains("[Headers]"),
        "Tab moves to the headers tab:\n{headers}"
    );

    press(&mut app, KeyCode::Tab);
    let text = render(&app);
    assert!(
        text.contains("[Cookies]"),
        "a second Tab moves to the cookies tab:\n{text}"
    );
    assert!(text.contains("session"), "the cookie name is shown");
    assert!(text.contains("HttpOnly") && text.contains("Secure"));
    assert!(
        text.contains("<SESSION>-CENSORED"),
        "the value is censored:\n{text}"
    );
    assert!(
        !text.contains("s3cret"),
        "the raw cookie value never reaches the screen:\n{text}"
    );
}

#[test]
fn switching_environment_updates_the_banner_and_the_request_url() {
    let mut fixture = fixture("switch");
    select(&mut fixture.app, "listProducts");
    let before = render(&fixture.app);
    assert!(
        before.contains("https://staging.example.com/products"),
        "the request pane shows the staging URL:\n{before}"
    );

    assert_eq!(
        press(&mut fixture.app, KeyCode::Char('e')),
        Action::SwitchEnv("production".to_string())
    );
    let mut driver = SwitchDriver {
        next: Some(fixture.production.clone()),
    };
    dispatch(
        &mut fixture.app,
        Action::SwitchEnv("production".to_string()),
        &mut driver,
    )
    .expect("dispatch succeeds");

    assert_eq!(fixture.app.environment(), "production");
    let after = render(&fixture.app);
    assert!(
        after.contains("demo · production"),
        "the banner names the new environment:\n{after}"
    );
    assert!(
        after.contains("https://api.example.com/products"),
        "the request URL was re-derived:\n{after}"
    );
    assert!(
        !after.contains("https://staging.example.com"),
        "the old environment is gone:\n{after}"
    );

    // A name the host cannot resolve is a banner line, not a panic.
    dispatch(
        &mut fixture.app,
        Action::SwitchEnv("nowhere".to_string()),
        &mut driver,
    )
    .expect("dispatch succeeds");
    assert_eq!(fixture.app.environment(), "production");
    assert!(
        fixture
            .app
            .status()
            .is_some_and(|status| status.contains("unknown environment")),
        "the failure surfaces in the banner: {:?}",
        fixture.app.status()
    );
}

/// A driver that can hand back one prepared environment.
struct SwitchDriver {
    next: Option<Resolved>,
}

impl Driver for SwitchDriver {
    fn start(
        &mut self,
        _operation: &str,
        _user: Option<&str>,
        _vars: &std::collections::BTreeMap<String, testdet_core::serde_json::Value>,
    ) {
    }

    fn cancel(&mut self) {}

    fn switch_environment(&mut self, environment: &str) -> Result<Resolved, String> {
        self.next
            .take()
            .ok_or_else(|| format!("error[E_UNKNOWN_ENV]: unknown environment `{environment}`"))
    }

    fn events(&mut self) -> Vec<Event> {
        Vec::new()
    }
}

/// Every file under `root`, sorted by path, with its bytes.
fn directory_state(root: &Path) -> Vec<(PathBuf, Vec<u8>)> {
    let mut files = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        for entry in fs::read_dir(&dir).expect("read the fixture directory") {
            let path = entry.expect("a directory entry").path();
            if path.is_dir() {
                stack.push(path);
            } else {
                let contents = fs::read(&path).expect("read a fixture file");
                files.push((path, contents));
            }
        }
    }
    files.sort_by(|left, right| left.0.cmp(&right.0));
    files
}
