//! `testdet validate` — check the config and report every diagnostic.

use std::path::PathBuf;

use testdet_core::config::LoadOptions;
use testdet_core::config::load_diagnostics;
use testdet_core::diagnostic::{render_json, render_text};

use crate::exit::Exit;
use crate::format::Format;

/// Arguments for `testdet validate`.
#[derive(Debug, Clone)]
pub struct ValidateArgs {
    /// Explicit `--config` path; `None` discovers by walking up from `cwd`.
    pub config: Option<PathBuf>,
    /// Output format (`--format`).
    pub format: Format,
    /// Explicit `--env`; accepted and ignored in v1 (validate is
    /// environment-independent).
    pub env: Option<String>,
}

/// Run `testdet validate`.
///
/// Builds every diagnostic with [`load_diagnostics`] (warnings included) so a
/// warnings-only config prints its warnings and exits 0. A config with at
/// least one error exits 1. The returned string is the complete rendered
/// output; `testdet-cli::run` routes it to stdout for [`Format::Json`] and to
/// stderr otherwise.
pub fn run(args: ValidateArgs) -> (Exit, String) {
    if args.format == Format::Ndjson {
        return (Exit::Usage, crate::commands::ndjson_usage_error());
    }

    // `env` is accepted but ignored in v1: validation is environment-independent.
    let _ = args.env.as_ref();

    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not read the current directory: {error}\n"),
            );
        }
    };

    let diagnostics = load_diagnostics(&LoadOptions {
        config: args.config,
        cwd,
    });

    let exit = if diagnostics.has_errors() {
        Exit::ConfigInvalid
    } else {
        Exit::Ok
    };

    let output = match args.format {
        Format::Json => {
            let mut json = render_json(&diagnostics);
            json.push('\n');
            json
        }
        Format::Text => render_text(&diagnostics),
        Format::Ndjson => unreachable!("handled above"),
    };

    (exit, output)
}
