//! Headless CLI frontend for TestDet: the clap surface and command dispatch.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::commands::{config, env, show, validate};
use crate::exit::Exit;
use crate::format::Format;

pub mod commands;
pub mod exit;
pub mod format;

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
    Validate(ValidateCli),
    /// Run a single operation end to end.
    Run,
    /// Inspect environment variables.
    Env(EnvCli),
    /// Show the resolved configuration.
    Config(ConfigCli),
    /// Emit the JSON Schema for the config.
    Schema,
    /// Show a resolved operation.
    Show(ShowCli),
}

#[derive(clap::Args)]
struct ValidateCli {
    /// Explicit config path; otherwise discovered by walking up from the cwd.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    format: Format,
    /// Environment override; selects the secrets declaration file
    /// (`testdet/secrets/<env>.yaml`).
    #[arg(long)]
    env: Option<String>,
}

#[derive(clap::Args)]
struct ConfigCli {
    /// Explicit config path; otherwise discovered by walking up from the cwd.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Environment to resolve; defaults to the manifest `defaultEnvironment`.
    #[arg(long)]
    env: Option<String>,
    /// User whose headers/vars layers are merged.
    #[arg(long)]
    user: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    format: Format,
}

#[derive(clap::Args)]
struct EnvCli {
    /// Explicit config path; otherwise discovered by walking up from the cwd.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Environment to resolve; defaults to the manifest `defaultEnvironment`.
    #[arg(long)]
    env: Option<String>,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    format: Format,
}

#[derive(clap::Args)]
struct ShowCli {
    /// Explicit config path; otherwise discovered by walking up from the cwd.
    #[arg(long)]
    config: Option<PathBuf>,
    /// Output format.
    #[arg(long, value_enum, default_value = "text")]
    format: Format,
    /// The operation id to resolve.
    id: String,
}

/// Parse `argv` and dispatch to the matching subcommand.
///
/// Returns the process exit code: `0` ok, `1` config invalid, `2` transport
/// failure, `3` usage, `4` internal. All stdout/stderr writes happen here;
/// commands return their complete output as a string.
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
        Commands::Validate(args) => {
            let format = args.format;
            let (exit, output) = validate::run(validate::ValidateArgs {
                config: args.config,
                format,
                env: args.env,
            });
            // `validate` renders JSON for every outcome (clean, warnings,
            // errors), so JSON output stays machine-readable on stdout even
            // when the exit code is 1.
            emit(exit, &output, format == Format::Json)
        }
        Commands::Env(args) => {
            let (exit, output) = env::run(env::EnvArgs {
                config: args.config,
                env: args.env,
                format: args.format,
            });
            // The body (text or json) belongs on stdout; diagnostics and usage
            // errors go to stderr (ruling 7).
            emit(exit, &output, exit == Exit::Ok)
        }
        Commands::Show(args) => {
            let format = args.format;
            let (exit, output) = show::run(show::ShowArgs {
                config: args.config,
                id: args.id,
                format,
            });
            // Only a successfully resolved operation in JSON form belongs on
            // stdout; diagnostics and usage errors are text and go to stderr.
            emit(exit, &output, format == Format::Json && exit == Exit::Ok)
        }
        Commands::Config(args) => {
            let format = args.format;
            let (exit, output) = config::run(config::ConfigArgs {
                config: args.config,
                env: args.env,
                user: args.user,
                format,
            });
            // The JSON config view belongs on stdout on success; text output,
            // diagnostics and usage errors go to stderr.
            emit(exit, &output, format == Format::Json && exit == Exit::Ok)
        }
        Commands::Tui | Commands::Run | Commands::Schema => {
            eprintln!("not implemented");
            4
        }
    }
}

/// Write `output` to stdout or stderr and return the exit code.
fn emit(exit: Exit, output: &str, to_stdout: bool) -> u8 {
    if to_stdout {
        let mut stdout = std::io::stdout().lock();
        let _ = stdout.write_all(output.as_bytes());
        let _ = stdout.flush();
    } else {
        let mut stderr = std::io::stderr().lock();
        let _ = stderr.write_all(output.as_bytes());
        let _ = stderr.flush();
    }
    exit.code()
}
