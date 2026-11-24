//! Output formats shared by the CLI commands.
//!
//! Clap derives the `--format` flag from [`ValueEnum`] here, but the enum
//! itself stays free of any terminal I/O: it is just the value the command
//! dispatch uses to pick stdout (machine-readable) vs stderr (human).

/// How a command renders its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Human-readable text, written to stderr.
    Text,
    /// Machine-readable JSON, written to stdout.
    Json,
    /// Newline-delimited JSON, used only by `run`.
    Ndjson,
}
