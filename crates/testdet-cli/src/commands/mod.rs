//! CLI subcommands.
//!
//! Each subcommand exposes a `run(args) -> (Exit, String)` function: it returns
//! the process exit code and the complete rendered output as a string, leaving
//! every write to `testdet-cli::run` so tests never have to spawn a process to
//! assert on content or exit codes.

pub mod config;
pub mod env;
pub mod run;
pub mod show;
pub mod validate;

/// The usage error returned when `--format ndjson` is passed to a command
/// other than `run` (Task 18).
pub(crate) fn ndjson_usage_error() -> String {
    "`--format ndjson` only applies to `run`\n".to_string()
}
