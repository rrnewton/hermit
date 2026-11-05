//! Compatibility entry point for herdr-agent.
fn main() {
    std::process::exit(agentctl::legacy_cli::main(std::env::args_os().skip(1)));
}
