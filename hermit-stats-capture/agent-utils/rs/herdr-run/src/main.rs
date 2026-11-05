//! `herdr-run` executable entry point.

fn main() {
    herdr_run::cli::prepare_process();
    std::process::exit(herdr_run::cli::main(std::env::args_os().skip(1)));
}
