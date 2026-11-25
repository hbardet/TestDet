//! Headless CLI frontend for TestDet: the clap surface and command dispatch.

use std::ffi::OsString;
use std::io::Write;
use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::commands::{config, env, run, show, validate};
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
    /// Run operations end to end: one user, sequential, worst exit code.
    Run(RunCli),
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
struct RunCli {
    /// Operations to run, in the order given.
    ids: Vec<String>,
    /// Select every operation listing this tag.
    #[arg(long)]
    tag: Option<String>,
    /// Select every operation in the config (excludes ids and `--tag`).
    #[arg(long)]
    all: bool,
    /// The single user to run as; omit to run anonymously.
    #[arg(long)]
    user: Option<String>,
    /// Environment to resolve; defaults to the manifest `defaultEnvironment`.
    #[arg(long)]
    env: Option<String>,
    /// A template variable override (`--var key=value`), highest precedence.
    #[arg(long = "var", value_name = "KEY=VALUE", value_parser = parse_var)]
    vars: Vec<(String, String)>,
    /// Output format: `text`, `json` (ndjson for 2+ operations) or `ndjson`.
    #[arg(long, value_enum, default_value = "text")]
    format: Format,
    /// Do not retain response bodies (`body` is null, `body_size` is kept).
    #[arg(long = "no-body")]
    no_body: bool,
    /// Request timeout (`30`, `30s` or `500ms`); overrides the config's value.
    #[arg(long, value_name = "DURATION", value_parser = parse_timeout)]
    timeout: Option<u64>,
}

/// Parse a `--timeout` argument into whole seconds.
///
/// §7.15 writes the flag as `--timeout 30s`, so the unit suffix is accepted;
/// a bare number means seconds, and `ms` must land on a whole number of them.
fn parse_timeout(raw: &str) -> Result<u64, String> {
    let raw = raw.trim();
    if let Some(millis) = raw.strip_suffix("ms") {
        let millis = parse_number(millis)
            .ok_or_else(|| format!("expected a number of milliseconds, got `{raw}`"))?;
        let seconds = millis / 1000;
        return if seconds == 0 {
            Err(format!("`{raw}` is shorter than one second"))
        } else {
            Ok(seconds)
        };
    }
    let number = raw.strip_suffix('s').unwrap_or(raw);
    parse_number(number).ok_or_else(|| format!("expected a duration such as `30s`, got `{raw}`"))
}

/// Parse a non-negative integer, rejecting an empty string.
fn parse_number(raw: &str) -> Option<u64> {
    let raw = raw.trim();
    if raw.is_empty() {
        None
    } else {
        raw.parse::<u64>().ok()
    }
}

/// Parse a `--var key=value` argument.
fn parse_var(raw: &str) -> Result<(String, String), String> {
    match raw.split_once('=') {
        Some((key, value)) if !key.trim().is_empty() => {
            Ok((key.trim().to_string(), value.to_string()))
        }
        _ => Err(format!(
            "expected `key=value` with a non-empty key, got `{raw}`"
        )),
    }
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
            // Text data and machine output both belong on stdout; only
            // diagnostics and usage errors go to stderr (F10).
            emit(exit, &output, exit == Exit::Ok)
        }
        Commands::Show(args) => {
            let format = args.format;
            let (exit, output) = show::run(show::ShowArgs {
                config: args.config,
                id: args.id,
                format,
            });
            // The operation view is data, in either format, so it belongs on
            // stdout; diagnostics and usage errors go to stderr (F10).
            let _ = format;
            emit(exit, &output, exit == Exit::Ok)
        }
        Commands::Config(args) => {
            let format = args.format;
            let (exit, output) = config::run(config::ConfigArgs {
                config: args.config,
                env: args.env,
                user: args.user,
                format,
            });
            // The config view is data, in either format, so it belongs on
            // stdout; diagnostics and usage errors go to stderr (F10).
            let _ = format;
            emit(exit, &output, exit == Exit::Ok)
        }
        Commands::Run(args) => {
            let format = args.format;
            let (exit, output) = run::run(run::RunArgs {
                ids: args.ids,
                tag: args.tag,
                all: args.all,
                user: args.user,
                env: args.env,
                vars: args.vars,
                format,
                no_body: args.no_body,
                timeout: args.timeout,
            });
            // Outcomes are data — the text blocks as much as the ndjson/json
            // objects — and belong on stdout even when an operation failed
            // (exit 2) or the engine produced an internal outcome (exit 4).
            // A run that never produced a result (exit 1: selection, unset
            // secret, unresolvable template; exit 3: usage) prints
            // diagnostics, which go to stderr in either format (F10).
            let produced_outcomes =
                matches!(exit, Exit::Ok | Exit::TransportFailure | Exit::Internal);
            let _ = format;
            emit(exit, &output, produced_outcomes)
        }
        Commands::Tui | Commands::Schema => {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_timeout_flag_accepts_seconds_suffixes_and_bare_numbers() {
        assert_eq!(parse_timeout("30").expect("bare seconds"), 30);
        assert_eq!(parse_timeout("30s").expect("seconds suffix"), 30);
        assert_eq!(parse_timeout("1500ms").expect("milliseconds"), 1);
        assert_eq!(parse_timeout(" 5s ").expect("surrounding space"), 5);
        assert!(parse_timeout("0ms").is_err());
        assert!(parse_timeout("soon").is_err());
        assert!(parse_timeout("").is_err());
    }
}
