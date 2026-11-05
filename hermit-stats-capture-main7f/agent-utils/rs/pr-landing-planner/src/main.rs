//! CLI entry point for `pr-landing-planner`.

#![forbid(unsafe_code)]

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    std::process::exit(pr_landing_planner::cli::main(&args));
}
