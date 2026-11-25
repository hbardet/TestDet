use std::process::ExitCode;

fn main() -> ExitCode {
    ExitCode::from(testdet_cli::run(std::env::args_os().collect()))
}
