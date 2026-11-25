//! Rendering and keymap tests for the TUI, driven through `TestBackend`.
//!
//! Every test builds a real `Config` from a throwaway directory (the config
//! format is the product, so the fixture is written as YAML), resolves it, and
//! asserts on the flattened buffer text — substrings, never exact frames.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use crossterm::event::{KeyCode, KeyEvent};
use ratatui::Terminal;
use ratatui::backend::TestBackend;
use testdet_core::config::{LoadOptions, load_config, resolve};
use testdet_tui::{Action, App};

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
             defaults:\n\
             \x20 headers:\n\
             \x20   Accept: application/json\n\
             environments:\n\
             \x20 staging:\n\
             \x20   baseUrl: https://staging.example.com\n\
             \x20   headers:\n\
             \x20     X-Api-Key: sekrit-value\n\
             \x20 production:\n\
             \x20   baseUrl: https://api.example.com\n",
        );
        sandbox.write(
            "testdet/users.yaml",
            "users:\n\
             \x20 alice:\n\
             \x20   token: alice-token\n\
             \x20 bob:\n\
             \x20   token: bob-token\n",
        );
        sandbox.write(
            "testdet/users/get-user.yaml",
            "id: getUser\n\
             description: Fetch a user\n\
             tags: [read]\n\
             request:\n\
             \x20 kind: graphql\n\
             \x20 query: |\n\
             \x20   query GetUser($id: ID!) {\n\
             \x20     user(id: $id) { id name }\n\
             \x20   }\n",
        );
        sandbox.write(
            "testdet/users/create-user.yaml",
            "id: createUser\n\
             description: Create a user\n\
             tags: [mutation, write]\n\
             request:\n\
             \x20 kind: graphql\n\
             \x20 query: |\n\
             \x20   mutation CreateUser { createUser { id } }\n",
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

fn fixture() -> (Sandbox, App) {
    let sandbox = Sandbox::new("render");
    let config = load_config(&LoadOptions {
        config: Some(sandbox.manifest()),
        cwd: sandbox.root.clone(),
        env: None,
    })
    .expect("the fixture config loads");
    let resolved = resolve(&config, None, &[]).expect("the fixture resolves");
    (sandbox, App::new(config, resolved))
}

/// Flatten the rendered frame into one string, row by row.
fn render(app: &App) -> String {
    let backend = TestBackend::new(100, 32);
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

#[test]
fn the_tree_lists_collections_and_operations_and_filters_by_text() {
    let (_sandbox, mut app) = fixture();

    let before = render(&app);
    assert!(
        before.contains("getUser"),
        "the tree lists the operation:\n{before}"
    );
    assert!(
        before.contains("listProducts"),
        "the tree groups by directory:\n{before}"
    );
    assert!(
        before.contains("users"),
        "the group header names the directory:\n{before}"
    );

    // `/` enters filter mode; typing narrows live.
    assert_eq!(
        press(&mut app, KeyCode::Char('/')),
        Action::Filter(String::new())
    );
    type_text(&mut app, "zz");
    let after = render(&app);
    assert!(
        !after.contains("getUser"),
        "a non-matching operation disappears:\n{after}"
    );
    assert!(
        !after.contains("listProducts"),
        "every non-match disappears:\n{after}"
    );

    // Esc leaves filter mode and clears the query.
    assert_eq!(press(&mut app, KeyCode::Esc), Action::Filter(String::new()));
    assert!(render(&app).contains("getUser"), "Esc restores the tree");
}

#[test]
fn filter_matches_tags_too() {
    let (_sandbox, mut app) = fixture();

    press(&mut app, KeyCode::Char('/'));
    type_text(&mut app, "mutation");

    let text = render(&app);
    assert!(
        text.contains("createUser"),
        "a tag match stays visible:\n{text}"
    );
    assert!(
        !text.contains("getUser"),
        "a non-matching operation is hidden:\n{text}"
    );
    assert!(
        !text.contains("listProducts"),
        "a non-matching operation is hidden:\n{text}"
    );
}

#[test]
fn enter_on_a_selected_operation_returns_run_with_its_id() {
    let (_sandbox, mut app) = fixture();

    // Groups sort by directory (`products` then `users`) and operations by id.
    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Action::Run("listProducts".into())
    );
    assert_eq!(press(&mut app, KeyCode::Esc), Action::Cancel);

    let (_second, mut fresh) = fixture();
    press(&mut fresh, KeyCode::Down);
    assert_eq!(
        press(&mut fresh, KeyCode::Enter),
        Action::Run("createUser".into())
    );
}

#[test]
fn the_request_pane_shows_kind_path_and_query_for_the_selection() {
    let (_sandbox, mut app) = fixture();

    // products → users → createUser → getUser.
    press(&mut app, KeyCode::Down);
    press(&mut app, KeyCode::Down);
    let text = render(&app);

    assert!(text.contains("kind: graphql"), "the kind is shown:\n{text}");
    assert!(
        text.contains("POST /graphql"),
        "the method and path are shown:\n{text}"
    );
    assert!(
        text.contains("query GetUser"),
        "the query is shown indented:\n{text}"
    );
    assert!(
        text.contains("X-Api-Key: X-Api-Key-CENSORED"),
        "inherited headers are masked by name:\n{text}"
    );
    assert!(
        !text.contains("sekrit-value"),
        "a header value never reaches the screen:\n{text}"
    );
    assert!(
        text.contains("no response yet"),
        "the response area is reserved:\n{text}"
    );
}

#[test]
fn q_quits_and_esc_returns_cancel_while_running() {
    let (_sandbox, mut app) = fixture();

    assert_eq!(
        press(&mut app, KeyCode::Esc),
        Action::None,
        "idle Esc does nothing"
    );

    assert_eq!(
        press(&mut app, KeyCode::Enter),
        Action::Run("listProducts".into())
    );
    assert_eq!(
        press(&mut app, KeyCode::Esc),
        Action::Cancel,
        "Esc cancels while a run is in flight"
    );

    assert_eq!(press(&mut app, KeyCode::Char('q')), Action::Quit);
}

#[test]
fn u_cycles_users_and_e_cycles_environments() {
    let (sandbox, mut app) = fixture();

    assert_eq!(
        press(&mut app, KeyCode::Char('u')),
        Action::SwitchUser("alice".into())
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('u')),
        Action::SwitchUser("bob".into())
    );
    assert_eq!(
        press(&mut app, KeyCode::Char('u')),
        Action::SwitchUser(String::new())
    );

    assert_eq!(
        press(&mut app, KeyCode::Char('e')),
        Action::SwitchEnv("production".into())
    );
    // `e` only proposes the next name: the switch is the host's job (it has to
    // re-resolve the config), so the banner does not move until the result is
    // applied — and an unknown environment therefore never moves it at all.
    assert_eq!(app.environment(), "staging");
    let production = load_config(&LoadOptions {
        config: Some(sandbox.manifest()),
        cwd: sandbox.root.clone(),
        env: Some("production".to_string()),
    })
    .and_then(|config| resolve(&config, Some("production"), &[]))
    .expect("production resolves");
    app.apply_resolved(production);
    assert_eq!(
        press(&mut app, KeyCode::Char('e')),
        Action::SwitchEnv("staging".into())
    );

    let text = render(&app);
    assert!(
        text.contains("demo · production"),
        "the banner names the applied environment:\n{text}"
    );
    assert!(
        text.contains("anonymous"),
        "the banner names the user:\n{text}"
    );
}

fn source_text(dir: &Path, into: &mut String) {
    for entry in fs::read_dir(dir)
        .expect("read the source directory")
        .flatten()
    {
        let path = entry.path();
        if path.is_dir() {
            source_text(&path, into);
        } else if path.extension().is_some_and(|ext| ext == "rs") {
            into.push_str(&fs::read_to_string(&path).expect("read a source file"));
        }
    }
}

#[test]
fn the_tui_does_no_io_of_its_own() {
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let manifest = fs::read_to_string(manifest_dir.join("Cargo.toml")).expect("read Cargo.toml");
    let dependencies = manifest
        .split("[dependencies]")
        .nth(1)
        .expect("a [dependencies] section")
        .split("\n[")
        .next()
        .unwrap_or_default();
    for forbidden in ["reqwest", "tokio", "clap"] {
        assert!(
            !dependencies.contains(forbidden),
            "testdet-tui must not depend on `{forbidden}`:\n{dependencies}"
        );
    }

    let mut sources = String::new();
    source_text(&manifest_dir.join("src"), &mut sources);
    for forbidden in ["reqwest::", "tokio::"] {
        assert!(
            !sources.contains(forbidden),
            "testdet-tui must not reach for `{forbidden}`"
        );
    }
}
