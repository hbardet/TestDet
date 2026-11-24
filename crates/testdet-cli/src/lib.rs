//! Headless CLI frontend for TestDet: the clap surface and command dispatch.

use std::ffi::OsString;

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(
    name = "testdet",
    version = testdet_core::VERSION,
    about = "A git-native GraphQL/REST API testing tool"
)]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
    /// Explore collections and operations in a terminal UI.
    Tui,
    /// Validate the testdet/ config directory.
    Validate,
    /// Run a single operation end to end.
    Run,
    /// Inspect environment variables.
    Env,
    /// Show the resolved configuration.
    Config,
    /// Emit the JSON Schema for the config.
    Schema,
    /// Show a resolved operation.
    Show,
}

/// Parse `argv` and dispatch to the matching subcommand.
///
/// Returns the process exit code: `0` ok, `1` config invalid, `2` transport
/// failure, `3` usage, `4` internal. Every subcommand is a stub until later
/// tasks wire it up, so each prints `not implemented` to stderr and returns 4.
pub fn run(argv: Vec<OsString>) -> u8 {
    let cli = match Cli::try_parse_from(argv) {
        Ok(cli) => cli,
        Err(err) => {
            let to_stderr = err.use_stderr();
            let _ = err.print();
            return if to_stderr { 3 } else { 0 };
        }
    };

    match cli.command {
        Commands::Tui
        | Commands::Validate
        | Commands::Run
        | Commands::Env
        | Commands::Config
        | Commands::Schema
        | Commands::Show => {
            eprintln!("not implemented");
            4
        }
    }
}
