//! `gh-paced` executable entry point.

fn main() {
    std::process::exit(gh_paced::cli::main(std::env::args_os().skip(1)));
}
