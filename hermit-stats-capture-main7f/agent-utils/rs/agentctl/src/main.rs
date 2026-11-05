//! Canonical agentctl executable.
fn main() {
    std::process::exit(agentctl::cli::main(std::env::args_os().skip(1)));
}
