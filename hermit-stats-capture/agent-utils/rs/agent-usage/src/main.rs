//! `agent-usage` executable entry point.

fn main() {
    std::process::exit(agent_usage::cli::main(std::env::args_os().skip(1)));
}
