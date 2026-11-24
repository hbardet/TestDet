//! `testdet show` — resolve and print a single operation.

use std::path::PathBuf;

use testdet_core::config::LoadOptions;
use testdet_core::config::load_config;
use testdet_core::diagnostic::render_text;
use testdet_core::operation::model::{Operation, Request};

use crate::exit::Exit;
use crate::format::Format;

/// Arguments for `testdet show`.
#[derive(Debug, Clone)]
pub struct ShowArgs {
    /// Explicit `--config` path; `None` discovers by walking up from `cwd`.
    pub config: Option<PathBuf>,
    /// The operation id to resolve.
    pub id: String,
    /// Output format (`--format`).
    pub format: Format,
}

/// Run `testdet show`.
///
/// Returns the exit code and the complete rendered output. A successful
/// [`Format::Json`] render is the only output that belongs on stdout; the
/// human [`Format::Text`] render, unknown-id usage errors and config-failure
/// diagnostics all go to stderr, which `testdet-cli::run` handles.
pub fn run(args: ShowArgs) -> (Exit, String) {
    if args.format == Format::Ndjson {
        return (Exit::Usage, crate::commands::ndjson_usage_error());
    }

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not read the current directory: {error}\n"),
            );
        }
    };

    // A config that fails to load is reported as text on stderr (never JSON),
    // so `show` does not print diagnostics in the machine-readable stream.
    let config = match load_config(&LoadOptions {
        config: args.config,
        cwd,
    }) {
        Ok(config) => config,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_text(&diagnostics)),
    };

    let Some(operation) = config.operations.get(&args.id) else {
        return (Exit::Usage, unknown_operation(&args.id, &config));
    };

    let output = match args.format {
        Format::Json => {
            let mut json = serde_json::to_string_pretty(operation)
                .expect("serializing an operation cannot fail");
            json.push('\n');
            json
        }
        Format::Text => render_operation(operation),
        Format::Ndjson => unreachable!("handled above"),
    };

    (Exit::Ok, output)
}

/// The usage error for an id that names no operation, with a hint listing the
/// known ids (already sorted, since `Config.operations` is a `BTreeMap`).
fn unknown_operation(id: &str, config: &testdet_core::config::Config) -> String {
    let mut out = format!("unknown operation `{id}`\n");
    if !config.operations.is_empty() {
        let ids = config
            .operations
            .keys()
            .map(|known| format!("`{known}`"))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("known operations: {ids}\n"));
    }
    out
}

/// The short human rendering: id, kind, the request's method/path or the first
/// line of its query, then tags. Two-space indented, no ANSI.
fn render_operation(operation: &Operation) -> String {
    let mut out = String::new();
    out.push_str(&format!("  id: {}\n", operation.id));
    out.push_str(&format!("  kind: {}\n", request_kind(&operation.request)));
    match &operation.request {
        Request::Http(request) => {
            out.push_str(&format!("  {} {}\n", request.method, request.path));
        }
        Request::Graphql(request) => {
            let first = request.query.lines().next().unwrap_or("");
            out.push_str(&format!("  query: {first}\n"));
        }
    }
    if !operation.tags.is_empty() {
        out.push_str(&format!("  tags: {}\n", operation.tags.join(", ")));
    }
    out
}

/// The lowercase kind name of a request.
fn request_kind(request: &Request) -> &'static str {
    match request {
        Request::Http(_) => "http",
        Request::Graphql(_) => "graphql",
    }
}
