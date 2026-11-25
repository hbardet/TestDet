//! `testdet run` — select operations, run them sequentially, render outcomes.
//!
//! Spec §7.15 keeps v1's fan-out deliberately narrow: selection by id, tag or
//! `--all`; exactly one user per invocation (anonymous when `--user` is
//! absent); strictly sequential requests; every selected operation runs even
//! when one fails; and the exit code is the worst observed (`4 > 1 > 2 > 3 >
//! 0`). Breadth here is what would force concurrency, and concurrency collides
//! with the single-re-login rule (§7.7.3).
//!
//! Output: `text` is a short block per outcome on stderr, `json` is one pretty
//! object for one selected operation and ndjson (one compact object per line)
//! otherwise, and `ndjson` is ndjson even for a single operation.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use testdet_core::RunContext;
use testdet_core::config::{Config, LoadOptions, load_config, resolve};
use testdet_core::diagnostic::{Diagnostic, Diagnostics, Severity, codes, render_text};
use testdet_core::execute;
use testdet_core::operation::model::Operation;
use testdet_core::outcome::Outcome;
use testdet_core::redact::redactor_for;
use testdet_core::secrets::required_vars_for;
use testdet_core::session::{SessionStore, execute_for_user};

use crate::exit::Exit;
use crate::format::Format;

/// The diagnostic code for a selection that cannot be satisfied.
///
/// The spec's registry (§7.12) has no usage code — usage errors are normally
/// plain text — so `run` uses this one for the diagnostics [`select`] returns;
/// the CLI renders them as text on stderr and exits 3.
const E_USAGE: &str = "E_USAGE";

/// Arguments for `testdet run`.
#[derive(Debug, Clone)]
pub struct RunArgs {
    /// Explicit operation ids, in the order given (`run <id>...`).
    pub ids: Vec<String>,
    /// `--tag`: select every operation listing this tag (any position).
    pub tag: Option<String>,
    /// `--all`: select every operation in the config.
    pub all: bool,
    /// `--user`: the single identity to run as; `None` runs anonymously.
    pub user: Option<String>,
    /// `--env`: the environment to resolve; `None` uses the manifest default.
    pub env: Option<String>,
    /// `--var k=v` overrides, the top template-variable layer.
    pub vars: Vec<(String, String)>,
    /// `--format`: text, json or ndjson.
    pub format: Format,
    /// `--no-body`: do not keep response bodies (`body: null`, size kept).
    pub no_body: bool,
    /// `--timeout SECONDS`: override the config's request timeout.
    pub timeout: Option<u64>,
}

/// Run `testdet run`.
///
/// Returns the exit code and the complete rendered output. Environment
/// discovery, selection, user validation, environment resolution and the
/// unset-secret preflight all happen before the first request, so a config or
/// usage problem never reaches the wire.
pub fn run(args: RunArgs) -> (Exit, String) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not read the current directory: {error}\n"),
            );
        }
    };

    // `run` has no `--config` in §7.15: the config is discovered by walking up
    // from the working directory.
    let config = match load_config(&LoadOptions {
        config: None,
        cwd,
        env: args.env.clone(),
    }) {
        Ok(config) => config,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_text(&diagnostics)),
    };

    let selected = match select(&config, &args) {
        Ok(selected) => selected,
        Err(diagnostics) => return (Exit::Usage, render_text(&diagnostics)),
    };

    if let Some(user) = &args.user
        && !config.users.contains_key(user)
    {
        return (Exit::Usage, render_text(&unknown_user(user, &config)));
    }

    let resolved = match resolve(&config, args.env.as_deref(), &args.vars) {
        Ok(resolved) => resolved,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_text(&diagnostics)),
    };

    // A declared-but-unset secret would otherwise be sent as an empty string.
    // Refuse before the first request (exit 1), naming variable and file.
    let unset = unset_secrets(&config, &selected, args.env.as_deref());
    if !unset.is_empty() {
        return (Exit::ConfigInvalid, render_text(&unset));
    }

    let redactor = redactor_for(&config, &resolved);
    let mut ctx = RunContext::new(Arc::new(resolved), Arc::new(redactor))
        .with_user(args.user.clone())
        .with_retain_body(!args.no_body)
        // No `--verbose` exists in §7.15, so `run` renders outcomes, not events.
        .with_events(None);
    if let Some(seconds) = args.timeout {
        ctx.timeout = Duration::from_secs(seconds);
    }

    // One store for the whole run: cookies and captured variables persist
    // across operations, which is what makes capture-between-operations work.
    let mut store = SessionStore::new(ctx);
    let operations: Vec<&Operation> = selected
        .iter()
        .filter_map(|id| config.operations.get(id))
        .collect();

    let runtime = match tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
    {
        Ok(runtime) => runtime,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not start the async runtime: {error}\n"),
            );
        }
    };

    let outcomes = runtime.block_on(fan_out(&mut store, args.user.as_deref(), &operations));

    let exit = worst_exit(&outcomes, false);
    (exit, render(&outcomes, args.format))
}

/// Resolve the selection to an ordered, deduplicated list of operation ids.
///
/// - explicit ids keep the order given; an unknown id is one
///   `E_UNRESOLVED_REF` diagnostic whose hint lists close ids;
/// - `--tag` matches any listed tag (`tags: [smoke, daily]` matches `--tag
///   daily`), in config order;
/// - ids and `--tag` combine (ids first, then tag matches), deduped;
/// - `--all` is mutually exclusive with ids and `--tag`;
/// - an empty selection is a usage error.
pub fn select(config: &Config, args: &RunArgs) -> Result<Vec<String>, Diagnostics> {
    let mut diagnostics = Diagnostics::new();

    if args.all && (!args.ids.is_empty() || args.tag.is_some()) {
        diagnostics.push(usage(
            "`--all` cannot be combined with operation ids or `--tag`",
            "drop `--all`, or drop the ids and `--tag`",
        ));
    }

    for id in &args.ids {
        if !config.operations.contains_key(id) {
            diagnostics.push(Diagnostic {
                code: codes::E_UNRESOLVED_REF,
                severity: Severity::Error,
                file: None,
                span: None,
                path: None,
                message: format!("unknown operation `{id}`"),
                hint: Some(known_ids(id, config)),
            });
        }
    }

    if !diagnostics.is_empty() {
        return Err(diagnostics);
    }

    let mut selected: Vec<String> = Vec::new();
    for id in &args.ids {
        if !selected.contains(id) {
            selected.push(id.clone());
        }
    }
    if let Some(tag) = &args.tag {
        for (id, operation) in &config.operations {
            if operation.tags.iter().any(|listed| listed == tag) && !selected.contains(id) {
                selected.push(id.clone());
            }
        }
    }
    if args.all {
        for id in config.operations.keys() {
            if !selected.contains(id) {
                selected.push(id.clone());
            }
        }
    }

    if selected.is_empty() {
        return Err(Diagnostics::from(vec![usage(
            "no operations selected",
            "select operations by id, `--tag` or `--all`",
        )]));
    }

    Ok(selected)
}

/// The worst exit code across `outcomes`, in the spec's order
/// `Internal(4) > ConfigInvalid(1) > TransportFailure(2) > Usage(3) > Ok(0)`.
///
/// A transport error in any outcome yields 2, unless a config error already
/// outranks it (`had_config_error`, exit 1). An outcome with neither a status
/// nor a transport error is an internal failure (4) — the engine could not
/// produce either (§7.9) — which should not happen but must not read as
/// success if it does.
pub fn worst_exit(outcomes: &[Outcome], had_config_error: bool) -> Exit {
    let mut worst = if had_config_error {
        Exit::ConfigInvalid
    } else {
        Exit::Ok
    };

    for outcome in outcomes {
        let exit = outcome_exit(outcome);
        if severity(exit) > severity(worst) {
            worst = exit;
        }
    }

    worst
}

/// The exit code one outcome implies.
fn outcome_exit(outcome: &Outcome) -> Exit {
    if outcome.transport_error.is_some() {
        Exit::TransportFailure
    } else if outcome.status == 0 {
        Exit::Internal
    } else {
        Exit::Ok
    }
}

/// The `4 > 1 > 2 > 3 > 0` order as a comparable number: higher is worse.
fn severity(exit: Exit) -> u8 {
    match exit {
        Exit::Internal => 5,
        Exit::ConfigInvalid => 4,
        Exit::TransportFailure => 3,
        Exit::Usage => 2,
        Exit::Ok => 1,
    }
}

/// Run every selected operation sequentially, in selection order.
///
/// All selected operations run even if one fails; each outcome is pushed as it
/// finishes, so the renderer sees them in order.
async fn fan_out(
    store: &mut SessionStore,
    user: Option<&str>,
    operations: &[&Operation],
) -> Vec<Outcome> {
    let mut outcomes = Vec::with_capacity(operations.len());
    for operation in operations {
        outcomes.push(run_one(store, user, operation).await);
    }
    outcomes
}

/// Run one operation as `user`.
///
/// `Some(user)` uses [`execute_for_user`], which re-authenticates once on an
/// auth failure (§7.7.3). An anonymous run (ruling 2) must never log in —
/// `ensure(None)` is already a no-op — and must also not take the re-login
/// path on a 401, which would be a wasted round trip: the anonymous branch
/// therefore calls the non-retrying engine primitive directly and captures the
/// operation's own `capture:` map afterwards, as `execute_for_user` would.
async fn run_one(store: &mut SessionStore, user: Option<&str>, operation: &Operation) -> Outcome {
    match user {
        Some(_) => execute_for_user(store, user, operation).await,
        None => {
            let _ = store.ensure(None).await;
            let outcome = execute(store, operation).await;
            if !operation.capture.is_empty()
                && let Some(body) = outcome.body.as_deref()
            {
                store.capture_into(None, &operation.capture, body);
            }
            outcome
        }
    }
}

/// Render the outcomes for the requested format.
fn render(outcomes: &[Outcome], format: Format) -> String {
    match format {
        Format::Text => outcomes
            .iter()
            .map(render_text_outcome)
            .collect::<Vec<_>>()
            .join("\n"),
        // One operation under `json` is a single pretty object; two or more
        // would mean buffering everything, so §7.15 makes them ndjson.
        Format::Json if outcomes.len() == 1 => match serde_json::to_string_pretty(&outcomes[0]) {
            Ok(json) => format!("{json}\n"),
            Err(error) => format!("internal error: could not render the outcome: {error}\n"),
        },
        Format::Json | Format::Ndjson => {
            let mut out = String::new();
            for outcome in outcomes {
                match serde_json::to_string(outcome) {
                    Ok(line) => {
                        out.push_str(&line);
                        out.push('\n');
                    }
                    Err(error) => {
                        out.push_str(&format!(
                            "internal error: could not render the outcome: {error}\n"
                        ));
                    }
                }
            }
            out
        }
    }
}

/// One outcome as a short text block: status, operation, user, timing, size,
/// GraphQL errors pinned above the body, then the JSON body pretty-printed.
fn render_text_outcome(outcome: &Outcome) -> String {
    let mut out = String::new();

    let status = if outcome.transport_error.is_some() {
        "transport failure".to_string()
    } else {
        outcome.status.to_string()
    };
    out.push_str(&format!("status: {status}\n"));
    out.push_str(&format!("operation: {}\n", outcome.operation));
    out.push_str(&format!(
        "user: {}\n",
        outcome.user.as_deref().unwrap_or("anonymous")
    ));
    out.push_str(&format!(
        "elapsed: {}ms\n",
        outcome.timing.total.as_millis()
    ));
    out.push_str(&format!(
        "bytes: {}{}\n",
        outcome.body_size,
        if outcome.body_retained {
            ""
        } else {
            " (body not retained)"
        }
    ));

    if let Some(error) = &outcome.transport_error {
        out.push_str(&format!("transport error: {}\n", error.message));
    }
    for error in &outcome.graphql_errors {
        out.push_str(&format!("graphql error: {}\n", error.message));
    }

    if outcome.body_retained
        && let Some(body) = outcome.body.as_deref()
    {
        push_body(&mut out, body, outcome.body_size);
    }

    out
}

/// Append the response body pretty-printed when it is JSON, verbatim when it
/// is some other UTF-8 text, and as a byte count when it is not UTF-8.
fn push_body(out: &mut String, body: &[u8], body_size: u64) {
    let Ok(text) = std::str::from_utf8(body) else {
        out.push_str(&format!("body: {body_size} bytes (not UTF-8)\n"));
        return;
    };

    out.push_str("body:\n");
    match serde_json::from_str::<serde_json::Value>(text) {
        Ok(value) => match serde_json::to_string_pretty(&value) {
            Ok(pretty) => out.push_str(&pretty),
            Err(_) => out.push_str(text.trim_end()),
        },
        Err(_) => out.push_str(text.trim_end_matches('\n')),
    }
    out.push('\n');
}

/// A usage diagnostic (`E_USAGE`) rendered as text on stderr with exit 3.
fn usage(message: &str, hint: &str) -> Diagnostic {
    Diagnostic {
        code: E_USAGE,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: message.to_string(),
        hint: Some(hint.to_string()),
    }
}

/// The hint for an unknown operation id: close ids when there are any,
/// otherwise every known id (`Config.operations` is a sorted `BTreeMap`).
fn known_ids(id: &str, config: &Config) -> String {
    if config.operations.is_empty() {
        return "the config declares no operations".to_string();
    }

    let lower = id.to_lowercase();
    let close: Vec<&String> = config
        .operations
        .keys()
        .filter(|candidate| {
            let candidate = candidate.to_lowercase();
            candidate.contains(&lower) || lower.contains(&candidate)
        })
        .collect();
    let listed: Vec<&String> = if close.is_empty() {
        config.operations.keys().collect()
    } else {
        close
    };

    format!(
        "known operations: {}",
        listed
            .iter()
            .map(|id| format!("`{id}`"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

/// The usage diagnostic for an unknown `--user`, listing the declared users.
fn unknown_user(user: &str, config: &Config) -> Diagnostics {
    let hint = if config.users.is_empty() {
        "the config declares no users; omit `--user` to run anonymously".to_string()
    } else {
        format!(
            "known users: {}",
            config
                .users
                .keys()
                .map(|name| format!("`{name}`"))
                .collect::<Vec<_>>()
                .join(", ")
        )
    };

    Diagnostics::from(vec![Diagnostic {
        code: codes::E_UNKNOWN_USER,
        severity: Severity::Error,
        file: None,
        span: None,
        path: None,
        message: format!("unknown user `{user}`"),
        hint: Some(hint),
    }])
}

/// One error per secret the selected operations reference that is declared
/// but has no value, naming the variable and its declaration file.
///
/// An undeclared `{{env.X}}` is already a config error from `crosscheck`
/// (`E_UNKNOWN_ENV`), so only the declared-but-unset case is handled here.
fn unset_secrets(config: &Config, selected: &[String], env: Option<&str>) -> Diagnostics {
    let requirements = required_vars_for(config, env);
    let declaration: PathBuf = config
        .root_dir
        .join("secrets")
        .join(format!("{}.yaml", config.secrets_env));
    let mut diagnostics = Diagnostics::new();

    for (name, requirement) in &requirements {
        if !requirement.declared || requirement.set {
            continue;
        }
        let used = requirement
            .used_by
            .iter()
            .any(|id| selected.iter().any(|selected| selected == id));
        if !used {
            continue;
        }

        diagnostics.push(Diagnostic {
            code: codes::W_UNSET_SECRET,
            // The `env` command reports this as a warning; `run` upgrades it
            // to an error because an unset secret would be sent as "".
            severity: Severity::Error,
            file: Some(declaration.clone()),
            span: None,
            path: None,
            message: format!("secret `{name}` is declared but not set"),
            hint: Some(format!(
                "set `{name}` in the environment (or the environment's dotenv file) before running; \
                 it is declared in `{}`",
                declaration.display()
            )),
        });
    }

    diagnostics
}

#[cfg(test)]
mod tests {
    use super::*;
    use testdet_core::outcome::{CookieInfo, RequestEcho, Timing, TransportError, TransportKind};

    /// A temp config tree with the given operations, loaded through the real
    /// loader so the tests exercise the same `Config` `run` sees.
    fn config(operations: &[(&str, &str)]) -> (tempfile::TempDir, Config) {
        let dir = tempfile::tempdir().expect("temp dir");
        let write = |relative: &str, content: &str| {
            let path = dir.path().join(relative);
            std::fs::create_dir_all(path.parent().expect("parent")).expect("mkdir");
            std::fs::write(path, content).expect("write");
        };
        write(
            "testdet/testdet.yaml",
            "version: 1\nname: run-unit\ncollections: [ops]\ndefaultEnvironment: local\n\
             environments:\n  local:\n    baseUrl: http://127.0.0.1:1\n",
        );
        for (id, tags) in operations {
            write(
                &format!("testdet/ops/{id}.yaml"),
                &format!("id: {id}\nrequest:\n  kind: http\n  method: GET\n  path: /{id}\n{tags}"),
            );
        }
        let config = load_config(&LoadOptions {
            config: None,
            cwd: dir.path().to_path_buf(),
            env: None,
        })
        .expect("the fixture config loads");
        (dir, config)
    }

    fn args() -> RunArgs {
        RunArgs {
            ids: Vec::new(),
            tag: None,
            all: false,
            user: None,
            env: None,
            vars: Vec::new(),
            format: Format::Text,
            no_body: false,
            timeout: None,
        }
    }

    fn outcome(status: u16, transport: Option<TransportKind>) -> Outcome {
        Outcome {
            operation: "op".to_string(),
            user: None,
            request: RequestEcho {
                method: "GET".to_string(),
                url: "http://127.0.0.1:1/op".to_string(),
                headers: Vec::new(),
                body_size: 0,
            },
            status,
            headers: Vec::new(),
            cookies: Vec::<CookieInfo>::new(),
            body: None,
            body_retained: true,
            body_size: 0,
            graphql_errors: Vec::new(),
            timing: Timing {
                total: Duration::ZERO,
                connect: None,
                ttfb: None,
            },
            transport_error: transport.map(|kind| TransportError {
                kind,
                message: "connect error".to_string(),
            }),
        }
    }

    #[test]
    fn explicit_ids_keep_the_given_order_without_duplicates() {
        let (_dir, config) = config(&[("alpha", ""), ("beta", ""), ("gamma", "")]);
        let args = RunArgs {
            ids: vec![
                "gamma".to_string(),
                "alpha".to_string(),
                "gamma".to_string(),
            ],
            ..args()
        };

        let selected = select(&config, &args).expect("a valid selection");
        assert_eq!(selected, ["gamma", "alpha"]);
    }

    #[test]
    fn ids_and_tag_combine_ids_first_then_tag_matches() {
        let (_dir, config) = config(&[
            ("alpha", "tags: [smoke, daily]\n"),
            ("beta", "tags: [smoke]\n"),
            ("gamma", "tags: [daily]\n"),
        ]);
        let args = RunArgs {
            ids: vec!["beta".to_string(), "alpha".to_string()],
            tag: Some("daily".to_string()),
            ..args()
        };

        // `alpha` is both an explicit id and a tag match: it is not repeated.
        let selected = select(&config, &args).expect("a valid selection");
        assert_eq!(selected, ["beta", "alpha", "gamma"]);
    }

    #[test]
    fn tag_matches_any_listed_tag() {
        let (_dir, config) = config(&[
            ("alpha", "tags: [smoke, daily]\n"),
            ("beta", "tags: [smoke]\n"),
        ]);
        let args = RunArgs {
            tag: Some("daily".to_string()),
            ..args()
        };

        let selected = select(&config, &args).expect("a valid selection");
        assert_eq!(selected, ["alpha"]);
    }

    #[test]
    fn all_selects_every_operation_and_rejects_combination() {
        let (_dir, config) = config(&[("alpha", ""), ("beta", "")]);
        let all = select(
            &config,
            &RunArgs {
                all: true,
                ..args()
            },
        )
        .expect("a valid selection");
        assert_eq!(all, ["alpha", "beta"]);

        let diagnostics = select(
            &config,
            &RunArgs {
                all: true,
                tag: Some("smoke".to_string()),
                ..args()
            },
        )
        .expect_err("`--all` with `--tag` is a usage error");
        assert!(
            diagnostics
                .iter()
                .any(|diagnostic| diagnostic.message.contains("`--all` cannot be combined"))
        );
    }

    #[test]
    fn nothing_selected_is_a_usage_error_naming_the_flags() {
        let (_dir, config) = config(&[("alpha", "")]);
        let diagnostics = select(&config, &args()).expect_err("an empty selection is invalid");
        let hint = diagnostics
            .iter()
            .find_map(|diagnostic| diagnostic.hint.clone())
            .expect("the usage error carries a hint");
        assert!(hint.contains("select operations by id, `--tag` or `--all`"));
    }

    #[test]
    fn unknown_id_is_one_unresolved_ref_with_close_ids_in_the_hint() {
        let (_dir, config) = config(&[("echo", ""), ("echo_invite", ""), ("other", "")]);
        let diagnostics = select(
            &config,
            &RunArgs {
                ids: vec!["ech".to_string(), "nope".to_string()],
                ..args()
            },
        )
        .expect_err("unknown ids are usage errors");

        // One diagnostic per unknown id, not one per known id.
        assert_eq!(diagnostics.len(), 2);
        assert!(
            diagnostics
                .iter()
                .all(|d| d.code == codes::E_UNRESOLVED_REF)
        );
        let hint = diagnostics
            .iter()
            .find(|d| d.message.contains("`ech`"))
            .and_then(|d| d.hint.clone())
            .expect("the unknown id carries a hint");
        assert!(hint.contains("`echo`"), "hint lists close ids: {hint}");
        assert!(!hint.contains("`other`"), "hint is narrowed: {hint}");
    }

    #[test]
    fn worst_exit_orders_internal_config_transport_usage_ok() {
        assert_eq!(worst_exit(&[], false), Exit::Ok);
        assert_eq!(worst_exit(&[outcome(200, None)], false), Exit::Ok);
        assert_eq!(
            worst_exit(&[outcome(200, Some(TransportKind::Connect))], false),
            Exit::TransportFailure
        );
        // A config error outranks a transport failure (1 > 2).
        assert_eq!(
            worst_exit(&[outcome(200, Some(TransportKind::Connect))], true),
            Exit::ConfigInvalid
        );
        // An outcome with neither a status nor a transport error is internal.
        assert_eq!(worst_exit(&[outcome(0, None)], true), Exit::Internal);
        assert_eq!(
            worst_exit(&[outcome(500, None), outcome(200, None)], false),
            Exit::Ok
        );
    }
}
