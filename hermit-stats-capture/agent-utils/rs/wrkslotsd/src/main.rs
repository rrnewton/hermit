//! Executable entry point for the read-only observer slice.

fn main() -> std::process::ExitCode {
    std::process::ExitCode::from(wrkslotsd::cli::run())
}
