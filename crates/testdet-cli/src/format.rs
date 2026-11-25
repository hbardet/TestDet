//! Output formats shared by the CLI commands.
//!
//! Clap derives the `--format` flag from [`clap::ValueEnum`] here, but the
//! enum itself stays free of any terminal I/O: it is just the value the
//! command dispatch uses to pick a renderer. Text output is data too, so it
//! goes to stdout; only diagnostics go to stderr (§7.15, F10).

/// How a command renders its output.
#[derive(Debug, Clone, Copy, PartialEq, Eq, clap::ValueEnum)]
pub enum Format {
    /// Human-readable text, written to stdout (diagnostics go to stderr).
    Text,
    /// Machine-readable JSON, written to stdout.
    Json,
    /// Newline-delimited JSON, used only by `run`.
    Ndjson,
}
