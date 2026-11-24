//! `testdet env` — list every required secret and where it came from (§7.11).

use std::collections::BTreeMap;
use std::path::PathBuf;

use testdet_core::config::LoadOptions;
use testdet_core::config::load_with_diagnostics;
use testdet_core::config::resolve;
use testdet_core::diagnostic::render_text;
use testdet_core::secrets::{Origin, VarRequirement, required_vars_for};

use crate::exit::Exit;
use crate::format::Format;

/// Arguments for `testdet env`.
#[derive(Debug, Clone)]
pub struct EnvArgs {
    /// Explicit `--config` path; `None` discovers by walking up from `cwd`.
    pub config: Option<PathBuf>,
    /// The environment to resolve (`--env`); `None` uses the manifest default.
    pub env: Option<String>,
    /// Output format (`--format`).
    pub format: Format,
}

/// Run `testdet env`.
///
/// Returns the exit code and the complete rendered output. The body — the
/// variable list in either format — belongs on stdout (ruling 7), which
/// `testdet-cli::run` routes by exit code. A resolve error prints its text
/// diagnostics and exits 1; a usage error (`--format ndjson`) exits 3.
pub fn run(args: EnvArgs) -> (Exit, String) {
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

    // The config is loaded best-effort so an undeclared `{{env.X}}` reference
    // (a crosscheck `E_UNKNOWN_ENV`) still yields a requirement listed as
    // `(undeclared)` rather than aborting the command.
    let (config, diagnostics) = load_with_diagnostics(&LoadOptions {
        config: args.config,
        cwd,
        env: args.env.clone(),
    });
    let Some(config) = config else {
        return (Exit::ConfigInvalid, render_text(&diagnostics));
    };

    // Join the requirements with `resolve`'s environment selection: an unknown
    // environment or missing `baseUrl` is a config error, not an env list.
    if let Err(diagnostics) = resolve(&config, args.env.as_deref(), &[]) {
        return (Exit::ConfigInvalid, render_text(&diagnostics));
    }

    let requirements = required_vars_for(&config, args.env.as_deref());

    let output = match args.format {
        Format::Json => render_json(&requirements),
        Format::Text => render_text_vars(&requirements),
        Format::Ndjson => unreachable!("handled above"),
    };

    (Exit::Ok, output)
}

/// The JSON form (ruling 7): an array of
/// `{name, declared, set, origin, usedBy}` where `origin` is a tagged object
/// (`{"kind":"dotenv","path":…}` / `{"kind":"shell"}` / `{"kind":"default"}`)
/// or `null`.
fn render_json(requirements: &BTreeMap<String, VarRequirement>) -> String {
    let entries: Vec<serde_json::Value> = requirements
        .iter()
        .map(|(name, requirement)| {
            serde_json::json!({
                "name": name,
                "declared": requirement.declared,
                "set": requirement.set,
                "origin": serde_json::to_value(&requirement.origin).expect("origin serializes"),
                "usedBy": requirement.used_by,
            })
        })
        .collect();

    let mut out = serde_json::to_string(&entries).expect("requirements serialize");
    out.push('\n');
    out
}

/// The text form (ruling 7): names left-padded to the longest name, two
/// spaces, `required by N operation(s)`, two spaces, then
/// `set <- <origin>` (`environment`, the file path, or `declared default`) or
/// `NOT SET`, with ` (undeclared)` appended for an undeclared name.
fn render_text_vars(requirements: &BTreeMap<String, VarRequirement>) -> String {
    let width = requirements
        .keys()
        .map(|name| name.chars().count())
        .max()
        .unwrap_or(0);

    let mut out = String::new();
    for (name, requirement) in requirements {
        let status = match &requirement.origin {
            Some(Origin::Shell) => "set <- environment".to_string(),
            Some(Origin::Dotenv(path)) => format!("set <- {}", path.display()),
            Some(Origin::Default) => "set <- declared default".to_string(),
            None => "NOT SET".to_string(),
        };
        let mut line = format!(
            "{name:>width$}  required by {} operation(s)  {status}",
            requirement.used_by.len()
        );
        if !requirement.declared {
            line.push_str(" (undeclared)");
        }
        line.push('\n');
        out.push_str(&line);
    }
    out
}
