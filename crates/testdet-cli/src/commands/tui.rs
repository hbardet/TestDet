//! `testdet tui` — the interactive frontend.
//!
//! Everything that needs a terminal lives in `testdet-tui`; everything that
//! needs the network lives in [`crate::tui_driver`]. This module is the seam:
//! it loads and resolves the config (so a broken config exits 1 with
//! diagnostics and never touches the terminal), builds the shared
//! `RunContext`, spawns the executor and hands both to the event loop.

use std::path::PathBuf;
use std::sync::Arc;

use testdet_core::config::{LoadOptions, load_config, resolve};
use testdet_core::diagnostic::render_text;
use testdet_core::redact::redactor_for;

use crate::commands::run::unknown_user;
use crate::exit::Exit;
use crate::tui_driver::ExecutorDriver;

/// Arguments for `testdet tui`.
#[derive(Debug, Clone)]
pub struct TuiArgs {
    /// `--config`: explicit config path; otherwise discovered from the cwd.
    pub config: Option<PathBuf>,
    /// `--user`: the identity to start as; `u` cycles it in the UI.
    pub user: Option<String>,
    /// `--env`: the environment to resolve; `e` cycles the banner in the UI.
    pub env: Option<String>,
}

/// Open the UI until the user quits.
///
/// Returns the exit code and whatever should be written outside the terminal:
/// diagnostics for a config/usage error, an empty string on a normal quit.
pub fn run(args: TuiArgs) -> (Exit, String) {
    let cwd = match std::env::current_dir() {
        Ok(cwd) => cwd,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not read the current directory: {error}\n"),
            );
        }
    };

    let config = match load_config(&LoadOptions {
        config: args.config,
        cwd,
        env: args.env.clone(),
    }) {
        Ok(config) => config,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_text(&diagnostics)),
    };

    if let Some(user) = &args.user
        && !config.users.contains_key(user)
    {
        return (Exit::Usage, render_text(&unknown_user(user, &config)));
    }

    let resolved = match resolve(&config, args.env.as_deref(), &[]) {
        Ok(resolved) => resolved,
        Err(diagnostics) => return (Exit::ConfigInvalid, render_text(&diagnostics)),
    };

    let redactor = Arc::new(redactor_for(&config, &resolved));
    let shared = Arc::new(resolved.clone());

    let mut driver = match ExecutorDriver::spawn(shared, redactor, args.user.clone()) {
        Ok(driver) => driver,
        Err(error) => {
            return (
                Exit::Internal,
                format!("internal error: could not start the executor: {error}\n"),
            );
        }
    };

    let app = testdet_tui::App::new(config, resolved).with_user(args.user);
    match testdet_tui::run(app, &mut driver) {
        Ok(()) => (Exit::Ok, String::new()),
        Err(error) => (Exit::Internal, format!("terminal error: {error}\n")),
    }
}
